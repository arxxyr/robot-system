//! W3 IPC 的确定性调度验收。真实队列、权限、原 seq、文件 writer；不是 wire/RR线程 E2E。
//! 子进程边界持有全部未退责任；父进程有界监督，不让断言失败析构残余句柄。

use super::*;
use crate::hard_rt_ctrl::{HardRtCtrlInner, inhibit};
use crate::latch_writer::{
    NoHook, StandbyStore, WriterConfig, WriterHandle, WriterPairing, WriterStop,
};
use crate::rr_admission::{CheckedLimits, PeerLedger, RrRequestClass, Slot, SlotIdentity};
use crate::state_machine::ControlLink;
use crossbeam_channel::{Sender, bounded};
use robot_core::control_authority::{AuthorityInit, ControlAuthority, InhibitSet, NormalIoIntent};
use robot_core::time::{DurationNs, MockClock, MonoRawNs, SystemClock};
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

struct Running(AtomicBool);
impl WriterStop for Running {
    fn running(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
struct LocalRoutes {
    ledger: PeerLedger<u8>,
    slots: HashMap<SlotIdentity, Slot<u8>>,
    clients: u8,
}
impl LocalRoutes {
    fn new() -> Self {
        Self::with_clients(8)
    }
    fn with_clients(clients: u8) -> Self {
        Self {
            ledger: PeerLedger::new(
                &CheckedLimits::try_from_limits(&robot_transport::iceoryx2::RrStaticLimits {
                    l: 4,
                    c: usize::from(clients),
                })
                .unwrap(),
            )
            .unwrap(),
            slots: HashMap::with_capacity(usize::from(clients) * 4),
            clients,
        }
    }
    fn admit(&mut self, class: RrRequestClass) -> Option<SlotIdentity> {
        for peer in 0..self.clients {
            if let Ok(slot) = self.ledger.admit(peer, class) {
                let route = slot.identity();
                assert!(self.slots.insert(route, slot).is_none());
                return Some(route);
            }
        }
        None
    }
    fn finish_local(&mut self, route: SlotIdentity) {
        // 这里只归还本地模拟消费者的真实槽；没有 ActiveRequest，不声称 send_response。
        self.ledger
            .release(self.slots.remove(&route).expect("每一原路由恰好消费一次"))
            .unwrap();
    }
}
struct Fixture {
    runtime: IpcLoopState,
    ctrl: Arc<HardRtCtrlInner>,
    client: HardRtCtrlClient,
    clock: Arc<MockClock>,
    boot: BootShared,
    persist: PersistClient,
    // 原 writer/控制源/备用 store 持到验收子进程退出，不以未读字段代替 join。
    _writer: Option<WriterHandle>,
    _writer_stop: Arc<Running>,
    _standby: StandbyStore,
    _lock: crate::latch_lock::LatchTargetLock,
    directory: tempfile::TempDir,
    metrics: IpcThreadMetrics,
    outbox: RetiredOutbox,
    sealed: [bool; 4],
    watchdog_tx: Sender<Event>,
    watchdog_rx: Receiver<Event>,
    external_tx: Sender<ControlEvent>,
    external_rx: Receiver<ControlEvent>,
    clear_high: Sender<ClearEStopRrRequest>,
    clear_normal: Sender<ClearEStopRrRequest>,
    clear: RrChannels,
    set_high: Sender<SetEnabledRrRequest>,
    set_normal: Sender<SetEnabledRrRequest>,
    set: SetEnabledRrChannels,
    _clear_response: Receiver<ClearEStopRrResponse>,
    _set_response: Receiver<SetEnabledRrResponse>,
    clear_routes: LocalRoutes,
    set_routes: LocalRoutes,
    adapter: robot_adapter_mock::MockHardRTAdapter,
}
impl Fixture {
    fn new(state: S0State) -> ManuallyDrop<Self> {
        let authority = Arc::new(
            ControlAuthority::new(AuthorityInit::new(
                false,
                OutputSafetyState::TorqueOffConfirmed,
                InhibitSet::EMPTY,
            ))
            .unwrap(),
        );
        Self::from_control(
            state,
            Arc::new(HardRtCtrlInner::new(authority)),
            Arc::new(MockClock::new(MonoRawNs(1_000_000_000))),
        )
    }
    fn serial(state: S0State) -> ManuallyDrop<Self> {
        let (ctrl, clock) = crate::post_e_convergence_acceptance::healthy_ipc_control();
        Self::from_control(state, ctrl, clock)
    }
    fn from_control(
        state: S0State,
        ctrl: Arc<HardRtCtrlInner>,
        clock: Arc<MockClock>,
    ) -> ManuallyDrop<Self> {
        let directory = tempfile::tempdir().unwrap();
        let latch = directory.path().join("latch");
        crate::estop_latch::save_estop_latch(&latch, ctrl.safety_latched()).unwrap();
        let lock = crate::latch_lock::acquire_latch_lock(&latch).unwrap();
        let store = || {
            crate::clear_estop_transaction::ClearTxnStore::open(&latch, "ipc-acceptance", "arm")
                .unwrap()
        };
        let (slot, standby) = WriterPairing::new(store(), store(), &lock, clock.now())
            .unwrap()
            .into_parts();
        let boot = BootShared::new(clock.now());
        let permit = boot
            .install_runtime_for_test(ctrl.authority().clone(), slot.shared().clone())
            .unwrap_or_else(|_| panic!("同对真实绑定"));
        drop(permit);
        let (persist, endpoint) = crate::latch_writer::mailbox(slot.shared().clone());
        let writer_stop = Arc::new(Running(AtomicBool::new(true)));
        let writer = crate::latch_writer::spawn_writer(
            slot,
            endpoint,
            WriterConfig::default(),
            writer_stop.clone(),
            clock.clone(),
            Box::new(NoHook),
        )
        .unwrap();
        let (watchdog_tx, watchdog_rx) = bounded(64);
        let (external_tx, external_rx) = bounded(64);
        let (clear_high, clear_high_rx) = bounded(16);
        let (clear_normal, clear_normal_rx) = bounded(16);
        let (clear_tx, clear_rx) = bounded(32);
        let (set_high, set_high_rx) = bounded(16);
        let (set_normal, set_normal_rx) = bounded(16);
        let (set_tx, set_rx) = bounded(32);
        let guards = Guards {
            rt_serial: ctrl.ctrl_kind == CtrlKind::SerialRt,
            adapter_connected: true,
            calibration_loaded: true,
            joint_state_valid: true,
            power_ok: true,
            heartbeat_ok: true,
            cmd_deadline_valid: true,
            limits_valid: true,
            hw_enable_ok: false,
            adapter_fault_cleared: true,
            ..Guards::default()
        };
        let mut adapter = robot_adapter_mock::MockHardRTAdapter::with_unlimited_limits(2);
        robot_core::adapter::ManipulatorAdapter::connect(&mut adapter).unwrap();
        ManuallyDrop::new(Self {
            runtime: IpcLoopState::new(state, guards, 32),
            client: HardRtCtrlClient::new(ctrl.clone()),
            ctrl,
            clock,
            boot,
            persist,
            _writer: Some(writer),
            _writer_stop: writer_stop,
            _standby: standby,
            _lock: lock,
            directory,
            metrics: IpcThreadMetrics::default(),
            outbox: RetiredOutbox::default(),
            sealed: [false; 4],
            watchdog_tx,
            watchdog_rx,
            external_tx,
            external_rx,
            clear_high,
            clear_normal,
            clear: RrChannels {
                request_rx: clear_high_rx,
                normal_request_rx: clear_normal_rx,
                response_tx: clear_tx,
            },
            set_high,
            set_normal,
            set: SetEnabledRrChannels {
                request_rx: set_high_rx,
                normal_request_rx: set_normal_rx,
                response_tx: set_tx,
            },
            _clear_response: clear_rx,
            _set_response: set_rx,
            clear_routes: LocalRoutes::new(),
            set_routes: LocalRoutes::new(),
            adapter,
        })
    }
    fn route_clients(&mut self, clients: u8) {
        // 仅空夹具选择受检 C<=16/L=4；原 RR 槽未准入前设置，不扩大实际有界源队列。
        assert!(self.clear_routes.slots.is_empty() && self.set_routes.slots.is_empty());
        self.clear_routes = LocalRoutes::with_clients(clients);
        self.set_routes = LocalRoutes::with_clients(clients);
    }
    fn tick(&mut self) -> S0StateSnapshot {
        self.tick_topics(TopicEvents::default(), || {})
    }
    fn tick_topics(
        &mut self,
        topics: TopicEvents,
        before_topics: impl FnOnce(),
    ) -> S0StateSnapshot {
        let context = IpcRuntimeContext {
            ctrl: &self.client,
            boot: &self.boot,
            clock: self.clock.as_ref(),
            metrics: &self.metrics,
        };
        let sources = IpcSources {
            watchdog: &self.watchdog_rx,
            external: &self.external_rx,
            clear_rr: Some(&self.clear),
            set_rr: Some(&self.set),
        };
        self.runtime.tick(
            &context,
            &sources,
            &mut self.sealed,
            &mut self.persist,
            &mut self.outbox,
            || {
                before_topics();
                topics
            },
        )
    }
    fn set_request(
        &mut self,
        id: &str,
        enabled: bool,
        dry_run: bool,
        deadline: u64,
    ) -> Option<SetEnabledRrRequest> {
        Some(SetEnabledRrRequest {
            route: self
                .set_routes
                .admit(RrRequestClass::of_set_enabled(dry_run, enabled))?,
            request_id: id.to_owned(),
            enabled,
            dry_run,
            deadline_ns: deadline,
            audit_reason: "真实本地准入".into(),
        })
    }
    fn clear_request(
        &mut self,
        id: &str,
        dry_run: bool,
        deadline: u64,
    ) -> Option<ClearEStopRrRequest> {
        Some(ClearEStopRrRequest {
            route: self.clear_routes.admit(RrRequestClass::of_clear(dry_run))?,
            request_id: id.to_owned(),
            dry_run,
            deadline_ns: deadline,
            confirmation: "确认清急停".into(),
            audit_reason: "真实本地准入".into(),
        })
    }
    fn send_set(&mut self, id: &str, enabled: bool, dry: bool, deadline: u64) {
        let request = self.set_request(id, enabled, dry, deadline).unwrap();
        if dry {
            self.set_normal.send(request).unwrap();
        } else {
            self.set_high.send(request).unwrap();
        }
    }
    fn send_clear(&mut self, id: &str, dry: bool, deadline: u64) {
        let request = self.clear_request(id, dry, deadline).unwrap();
        if dry {
            self.clear_normal.send(request).unwrap();
        } else {
            self.clear_high.send(request).unwrap();
        }
    }
    fn take_set(&mut self) -> SetEnabledRrResponse {
        let reply = self.outbox.set.pop_front().expect("原Set回复");
        self.set_routes.finish_local(reply.route);
        reply
    }
    fn take_clear(&mut self) -> ClearEStopRrResponse {
        let reply = self.outbox.clear.pop_front().expect("原Clear回复");
        self.clear_routes.finish_local(reply.route);
        reply
    }
    fn execute_one(&mut self) -> bool {
        let raw = self.ctrl.pending_normal.swap(0, Ordering::AcqRel);
        let Some(pending) = self.ctrl.decode_pending(raw).unwrap() else {
            return false;
        };
        assert!(
            crate::rt_thread::execute_owned_normal(&mut self.adapter, pending, &self.ctrl)
                .is_none()
        );
        true
    }
    fn enable(&mut self, id: &str) {
        self.send_set(id, true, false, u64::MAX);
        self.tick();
        assert!(matches!(self.runtime.pending, IpcPending::Enable(_)));
    }
    fn fault(&self, code: FaultCode) -> u32 {
        self.ctrl
            .publish_fault(
                OutputSafetyState::TorqueOffConfirmed,
                inhibit::COMM_FAULT,
                code,
                0,
                crate::hard_rt_ctrl::FaultOrigin::PendingTimeout,
            )
            .unwrap()
    }
    fn assert_no_motion(&self) {
        assert!(
            self.ctrl
                .authority()
                .acquire_normal_io(NormalIoIntent::Motion)
                .is_err()
        );
    }
    fn assert_disk(&self, latched: bool) {
        assert_eq!(
            crate::estop_latch::load_estop_latch(&self.directory.path().join("latch")).unwrap(),
            latched
        );
    }
}

fn supervised(name: &str, expected_failure: Option<&str>) -> bool {
    const KEY: &str = "ROBOT_W3_IPC_ACCEPTANCE_CHILD";
    if std::env::var(KEY).as_deref() == Ok(name) {
        let main = std::thread::current().id();
        std::panic::set_hook(Box::new(move |info| {
            eprintln!("IPC验收断言：{info}");
            if std::thread::current().id() == main {
                std::process::exit(101);
            }
        }));
        return false;
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("child.log");
    let log = std::fs::File::create(&path).unwrap();
    let exact = format!("ipc_runtime::post_e_ipc_acceptance::{name}");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture", "--test-threads=1"])
        .env(KEY, name)
        .env("TMPDIR", directory.path())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let deadline = SystemClock.now().0 + 20_000_000_000;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if SystemClock.now().0 < deadline => {
                std::thread::sleep(Duration::from_millis(2));
            }
            other => {
                let _ = child.kill();
                let reap = SystemClock.now().0 + 2_000_000_000;
                while !matches!(child.try_wait(), Ok(Some(_))) {
                    if SystemClock.now().0 >= reap {
                        std::process::abort();
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                panic!("已kill/reap：{other:?}");
            }
        }
    };
    let raw = std::fs::read_to_string(&path).unwrap();
    if let Some(expected) = expected_failure {
        assert_eq!(status.code(), Some(101), "{raw}");
        assert!(raw.contains(expected), "{raw}");
    } else {
        assert!(status.success(), "{raw}");
        assert_eq!(
            raw.lines()
                .filter(|line| *line == format!("ipc_owned_verified={name}"))
                .count(),
            1,
            "{raw}"
        );
    }
    if raw.contains("rss_flood_measured_ticks=") {
        for line in raw
            .lines()
            .filter(|line| line.starts_with("rss_flood_measured_ticks="))
        {
            println!("{line}");
        }
    }
    true
}
macro_rules! case {($name:ident,$body:block)=>{#[test]fn $name(){let name=stringify!($name);if supervised(name,None){return;}$body eprintln!("\nipc_owned_verified={name}");std::process::exit(0);}};}

case!(
    both_timeout_identities_block_clear_error_in_same_and_later_ticks,
    {
        for submitted in [false, true] {
            let mut f = Fixture::serial(S0State::Ready);
            // 原合法 Disable 占住普通槽，使第二个原 Enable 真实得到 Busy，而非手造 None seq。
            let occupied = if submitted {
                None
            } else {
                match f.client.request_normal(AdapterNormalReq::Enable(false)) {
                    NormalReqStatus::Submitted { seq } => Some(seq),
                    other => panic!("原占槽动作未提交: {other:?}"),
                }
            };
            f.enable("old");
            let IpcPending::Enable(enable) = &f.runtime.pending else {
                panic!("原 Enable owner");
            };
            assert_eq!(enable.seq().is_some(), submitted);
            let target = match enable.seq() {
                Some(seq) => robot_core::serial_services::NormalAbortTarget::Submitted { seq },
                None => robot_core::serial_services::NormalAbortTarget::Unsubmitted,
            };
            f.clock.advance(DurationNs::from_millis(1001));
            for _ in 0..4 {
                f.external_tx.send(ControlEvent::ClearError).unwrap();
                f.external_tx.send(ControlEvent::Enable).unwrap();
                f.tick();
                // IPC 的真实 1s 超时先锁 AdapterErr/原 abort；Serial 通信失败公告不是本例伪造。
                assert_eq!(f.runtime.state, S0State::Fault(FaultCode::AdapterErr));
                assert!(
                    !f.runtime.guards.adapter_connected && !f.runtime.guards.adapter_fault_cleared
                );
                assert_eq!(
                    f.ctrl.abort_request.load(Ordering::Acquire),
                    target.encode().unwrap()
                );
                assert_eq!(
                    f.ctrl.policy.load(Ordering::Acquire),
                    OutputPolicy::Disabled.to_u8()
                );
                f.assert_no_motion();
            }
            // 未运行设备的占槽原 seq 与整个 fixture 均保到本验收子进程结束。
            if let Some(seq) = occupied {
                assert_eq!(
                    f.ctrl
                        .decode_pending(f.ctrl.pending_normal.load(Ordering::Acquire))
                        .unwrap()
                        .unwrap()
                        .action
                        .seq,
                    seq
                );
            }
        }
    }
);
case!(
    mailbox_after_initial_guard_refresh_cannot_be_cleared_in_same_tick,
    {
        let mut f = Fixture::serial(S0State::Fault(FaultCode::AdapterErr));
        f.external_tx.send(ControlEvent::ClearError).unwrap();
        let ctrl = f.ctrl.clone();
        f.tick_topics(TopicEvents::default(), move || {
            ctrl.publish_fault(
                OutputSafetyState::TorqueOffConfirmed,
                inhibit::COMM_FAULT,
                FaultCode::CommFault,
                0,
                crate::hard_rt_ctrl::FaultOrigin::PendingTimeout,
            )
            .unwrap();
        });
        assert_eq!(f.runtime.state, S0State::Fault(FaultCode::CommFault));
        assert!(!f.runtime.guards.adapter_fault_cleared);
        f.assert_no_motion();
    }
);
case!(
    ordinary_pending_blocks_lanes_but_mailbox_always_retires_it,
    {
        // W3：未执行的 Enable 被真实安全收紧撤销，旧 owner/seq 必须真实退责；Lower 不再堵在普通队列。
        let mut f = Fixture::new(S0State::Safe);
        f.enable("old");
        f.send_set("disable", false, false, u64::MAX);
        f.watchdog_tx.send(Event::HeartbeatTimeout).unwrap();
        let generation = f.fault(FaultCode::CommFault);
        f.tick();
        assert_eq!(f.runtime.state, S0State::Fault(FaultCode::CommFault));
        assert_eq!(f.ctrl.fault_ack.load(Ordering::Acquire), generation);
        assert!(!f.runtime.guards.heartbeat_ok);
        f.assert_no_motion();
        assert!(
            !f.outbox
                .set
                .iter()
                .any(|reply| reply.effect == SideEffect::Committed)
        );
        assert!(matches!(f.runtime.pending, IpcPending::Lower(_)));
    }
);
case!(
    completed_enable_applies_queued_link_timeout_before_publishing_snapshot,
    {
        let mut f = Fixture::new(S0State::Ready);
        f.enable("enable");
        assert!(f.execute_one());
        f.watchdog_tx.send(Event::HeartbeatTimeout).unwrap();
        let snapshot = f.tick();
        assert!(matches!(
            snapshot.state,
            S0State::Enabled {
                control_link: ControlLink::Lost,
                ..
            }
        ));
        assert_eq!(f.take_set().effect, SideEffect::Committed);
        assert!(matches!(f.runtime.pending, IpcPending::Idle));
    }
);
case!(
    admitted_live_requests_are_preempted_without_republishing_serial_estop,
    {
        let mut f = Fixture::serial(S0State::Safe);
        f.send_clear("clear", false, u64::MAX);
        f.send_set("enable", true, false, u64::MAX);
        let epoch = f.ctrl.authority().snapshot().epoch;
        f.tick_topics(
            TopicEvents {
                estop: true,
                heartbeat: false,
            },
            || {},
        );
        assert!(f.ctrl.authority().snapshot().epoch > epoch);
        assert_eq!(
            f.take_clear().payload,
            ClearEStopRrPayload::PreemptedByEmergency
        );
        assert_eq!(f.take_set().payload, SetEnabledRrPayload::PreemptedByEStop);
        let after = f.ctrl.authority().snapshot().epoch;
        f.tick();
        assert_eq!(f.ctrl.authority().snapshot().epoch, after);
        assert!(f.runtime.coordination.is_retired_or_absent());
        assert_eq!(f.ctrl.pending_normal.load(Ordering::Acquire), 0);
        assert!(f.outbox.clear.is_empty() && f.outbox.set.is_empty());
    }
);
case!(
    prefix_counts_dry_and_expired_and_preserves_later_admission,
    {
        let mut f = Fixture::new(S0State::Safe);
        let now = f.clock.now().0;
        let expired = f.set_request("expired", true, false, now).unwrap();
        f.runtime.high.admit(HighWork::LiveEnable(expired)).unwrap();
        f.runtime
            .high
            .admit(HighWork::Control(ControlEvent::ClearEStop))
            .unwrap();
        let old = f.clear_request("old", false, u64::MAX).unwrap();
        f.runtime.high.admit(HighWork::LiveClear(old)).unwrap();
        f.send_clear("dry", true, u64::MAX);
        f.clock.advance(DurationNs(1));
        f.tick_topics(
            TopicEvents {
                estop: true,
                heartbeat: false,
            },
            || {},
        );
        assert_eq!(f.take_set().payload, SetEnabledRrPayload::DeadlineExceeded);
        assert_eq!(
            f.take_clear().payload,
            ClearEStopRrPayload::PreemptedByEmergency
        );
        assert!(matches!(
            f.take_clear().payload,
            ClearEStopRrPayload::Coordination(ClearOutcome::NotTorqueOffConfirmed { .. })
        ));
        f.send_set("new", true, false, u64::MAX);
        f.tick();
        assert_eq!(f.take_set().payload, SetEnabledRrPayload::LatchBlocked);
        assert!(f.runtime.coordination.is_retired_or_absent());
    }
);
case!(source_not_yet_admitted_is_outside_previous_estop_prefix, {
    let mut f = Fixture::new(S0State::Safe);
    f.route_clients(16);
    for index in 0..9 {
        f.send_set(&index.to_string(), true, false, u64::MAX);
    }
    f.tick_topics(
        TopicEvents {
            estop: true,
            heartbeat: false,
        },
        || {},
    );
    assert_eq!(f.set.request_rx.len(), 1);
    for index in 0..8 {
        let reply = f.take_set();
        assert_eq!(reply.request_id, index.to_string());
        assert_eq!(reply.payload, SetEnabledRrPayload::PreemptedByEStop);
    }
    f.tick();
    let last = f.take_set();
    assert_eq!(last.request_id, "8");
    assert_eq!(last.payload, SetEnabledRrPayload::LatchBlocked);
});
case!(repeated_estop_extends_prefix_to_newly_admitted_requests, {
    let mut f = Fixture::new(S0State::Safe);
    f.runtime.guards.heartbeat_ok = false;
    f.send_set("deferred", true, false, u64::MAX);
    f.tick();
    assert!(f.runtime.deferred_restore.is_some());
    f.send_set("first", true, false, u64::MAX);
    f.tick_topics(
        TopicEvents {
            estop: true,
            heartbeat: false,
        },
        || {},
    );
    for name in ["deferred", "first"] {
        let reply = f.take_set();
        assert_eq!(reply.request_id, name);
        assert_eq!(reply.payload, SetEnabledRrPayload::PreemptedByEStop);
    }
    f.send_clear("second", false, u64::MAX);
    f.tick_topics(
        TopicEvents {
            estop: true,
            heartbeat: false,
        },
        || {},
    );
    assert_eq!(
        f.take_clear().payload,
        ClearEStopRrPayload::PreemptedByEmergency
    );
    assert!(f.runtime.high.is_empty());
});
case!(
    ssf_keeps_queued_disable_idempotent_but_fails_inflight_disable,
    {
        // W3 的 Disable 必须经同 owned Lower + 实际 OffConfirmed 收尾；故障本身不是成功 ACK。
        let mut f = Fixture::serial(S0State::Safe);
        f.send_set("disable", false, false, u64::MAX);
        let generation = f.fault(FaultCode::CommFault);
        f.tick();
        assert_eq!(f.ctrl.fault_ack.load(Ordering::Acquire), generation);
        assert!(matches!(f.runtime.pending, IpcPending::Lower(_)));
        assert!(f.outbox.set.is_empty());
        f.assert_no_motion();
        let _ = f
            .ctrl
            .authority()
            .tighten_safety(robot_core::control_authority::SafetyTightening::StopFailed);
        f.tick();
        assert!(
            !f.outbox
                .set
                .iter()
                .any(|r| matches!(r.payload, SetEnabledRrPayload::Disabled { .. }))
        );
    }
);
case!(
    second_deadline_for_both_rpc_kinds_has_no_evaluator_or_latch_side_effect,
    {
        let mut f = Fixture::new(S0State::Safe);
        let now = f.clock.now().0;
        let clear = f.clear_request("clear", false, now).unwrap();
        let set = f.set_request("set", true, false, now).unwrap();
        f.runtime.high.admit(HighWork::LiveClear(clear)).unwrap();
        f.runtime.high.admit(HighWork::LiveEnable(set)).unwrap();
        let before = f.ctrl.authority().snapshot();
        f.clock.advance(DurationNs(1));
        f.tick();
        assert_eq!(
            f.take_clear().payload,
            ClearEStopRrPayload::DeadlineExceeded
        );
        assert_eq!(f.take_set().payload, SetEnabledRrPayload::DeadlineExceeded);
        assert_eq!(f.ctrl.authority().snapshot(), before);
        assert!(f.runtime.coordination.is_retired_or_absent());
        f.assert_disk(false);
    }
);
case!(deadline_equal_now_is_valid_for_both_read_only_evaluators, {
    let mut f = Fixture::new(S0State::Safe);
    let now = f.clock.now().0;
    f.send_clear("clear", true, now);
    f.send_set("set", false, true, now);
    let before = f.ctrl.authority().snapshot();
    f.tick();
    assert_eq!(
        f.take_clear().payload,
        ClearEStopRrPayload::Coordination(ClearOutcome::WouldClear { at: now })
    );
    assert_eq!(
        f.take_set().payload,
        SetEnabledRrPayload::WouldAccept { at_ns: now }
    );
    assert_eq!(f.ctrl.authority().snapshot(), before);
    f.assert_disk(false);
});
case!(all_four_sources_admit_only_eight_each_tick, {
    let mut f = Fixture::new(S0State::Safe);
    f.route_clients(16);
    for index in 0..9 {
        f.send_clear(&format!("high-clear-{index}"), false, 0);
        f.send_clear(&format!("normal-clear-{index}"), true, 0);
        f.send_set(&format!("high-set-{index}"), true, false, 0);
        f.send_set(&format!("normal-set-{index}"), false, true, 0);
    }
    f.tick();
    assert_eq!(
        [
            f.clear.request_rx.len(),
            f.clear.normal_request_rx.len(),
            f.set.request_rx.len(),
            f.set.normal_request_rx.len()
        ],
        [1; 4]
    );
    assert_eq!(f.outbox.clear.len(), 16);
    assert_eq!(f.outbox.set.len(), 16);
    for _ in 0..16 {
        assert_eq!(
            f.take_clear().payload,
            ClearEStopRrPayload::DeadlineExceeded
        );
        assert_eq!(f.take_set().payload, SetEnabledRrPayload::DeadlineExceeded);
    }
    assert!(f.runtime.high.is_empty() && f.runtime.normal.is_empty());
});
case!(
    external_source_is_bounded_at_64_and_stop_disables_even_when_full,
    {
        let f = Fixture::new(S0State::Safe);
        let (external, receiver) = crate::event_ingress::ExternalEvents::channel(
            Arc::new(AtomicBool::new(false)),
            Arc::new(f.client.clone()),
            None,
        );
        for _ in 0..64 {
            external.control(ControlEvent::ClearError).unwrap();
        }
        assert_eq!(receiver.len(), 64);
        let before = f.ctrl.authority().snapshot().epoch;
        assert_eq!(
            external.control(ControlEvent::Stop),
            Err(crate::event_ingress::AdmitError::Full)
        );
        assert!(f.ctrl.authority().snapshot().disabled);
        assert_eq!(
            f.ctrl.policy.load(Ordering::Acquire),
            OutputPolicy::Disabled.to_u8()
        );
        assert!(f.ctrl.authority().snapshot().epoch >= before);
        f.assert_no_motion();
        drop(receiver);
        assert_eq!(
            external.control(ControlEvent::Stop),
            Err(crate::event_ingress::AdmitError::Disconnected)
        );
    }
);
case!(
    serial_typed_cancellation_finishes_rr_without_fabricating_fault_or_estop,
    {
        // 原手写 AdapterResp 被撤销；从真 IPC post 取原 packed，再由生产消费函数产生取消完成。
        let mut f = Fixture::new(S0State::Safe);
        f.enable("enable");
        let raw = f.ctrl.pending_normal.swap(0, Ordering::AcqRel);
        let original = f.ctrl.decode_pending(raw).unwrap().unwrap();
        f.send_set("disable", false, false, u64::MAX);
        f.tick();
        let ctrl = f.ctrl.clone();
        assert!(crate::rt_thread::execute_owned_normal(&mut f.adapter, original, &ctrl).is_none());
        f.tick();
        let response = f.take_set();
        assert_eq!(response.request_id, "enable");
        assert_eq!(response.payload, SetEnabledRrPayload::SupersededByDisable);
        assert!(!f.ctrl.safety_latched());
        assert!(f.ctrl.mailbox_snapshot().is_none());
        f.assert_disk(false);
    }
);
case!(
    mailbox_is_acknowledged_during_four_consecutive_estop_ticks,
    {
        let mut f = Fixture::serial(S0State::Ready);
        for _ in 0..4 {
            let generation = f.fault(FaultCode::CommFault);
            f.tick_topics(
                TopicEvents {
                    estop: true,
                    heartbeat: false,
                },
                || {},
            );
            assert_eq!(f.runtime.state, S0State::Fault(FaultCode::CommFault));
            assert_eq!(f.ctrl.fault_ack.load(Ordering::Acquire), generation);
            f.assert_no_motion();
        }
    }
);
case!(cancelled_control_action_does_not_cancel_new_rr_owner, {
    let mut f = Fixture::new(S0State::Safe);
    f.external_tx.send(ControlEvent::Enable).unwrap();
    f.tick();
    let raw = f.ctrl.pending_normal.swap(0, Ordering::AcqRel);
    let original = f.ctrl.decode_pending(raw).unwrap().unwrap();
    f.send_set("new-disable", false, false, u64::MAX);
    f.tick();
    assert!(f.outbox.set.is_empty());
    let ctrl = f.ctrl.clone();
    assert!(crate::rt_thread::execute_owned_normal(&mut f.adapter, original, &ctrl).is_none());
    f.tick();
    assert!(
        !f.outbox
            .set
            .iter()
            .any(|reply| reply.request_id != "new-disable")
    );
    // 原 control 已真实消费拒绝，未启动设备且原 Off 仍成立；只有新 Lower 原路由可退休。
    let reply = f.take_set();
    assert_eq!(reply.request_id, "new-disable");
    assert!(matches!(
        reply.payload,
        SetEnabledRrPayload::Disabled { .. }
    ));
    assert_eq!(reply.effect, SideEffect::Committed);
    assert!(matches!(f.runtime.pending, IpcPending::Idle));
    assert!(f.outbox.set.is_empty());
    assert_eq!(
        f.ctrl.authority().snapshot().safety,
        OutputSafetyState::TorqueOffConfirmed
    );
    f.assert_no_motion();
});
case!(abort_handoff_both_interleavings_keep_readiness_false, {
    for before_refresh in [false, true] {
        let mut f = Fixture::serial(S0State::Fault(FaultCode::CommFault));
        f.client
            .abort_pending_normal(robot_core::serial_services::NormalAbortTarget::Unsubmitted);
        let abort = f.ctrl.abort_request.load(Ordering::Acquire);
        let publish = |ctrl: &HardRtCtrlInner| {
            ctrl.publish_fault(
                OutputSafetyState::TorqueOffConfirmed,
                inhibit::COMM_FAULT,
                FaultCode::CommFault,
                0,
                crate::hard_rt_ctrl::FaultOrigin::PendingTimeout,
            )
            .unwrap();
            assert!(ctrl.clear_abort(abort));
        };
        if before_refresh {
            publish(&f.ctrl);
            f.tick();
        } else {
            let ctrl = f.ctrl.clone();
            f.tick_topics(TopicEvents::default(), move || publish(&ctrl));
        }
        assert_eq!(f.ctrl.abort_request.load(Ordering::Acquire), 0);
        assert!(!f.runtime.guards.adapter_connected);
        assert!(!f.runtime.guards.adapter_fault_cleared);
        f.assert_no_motion();
    }
});
case!(
    each_lane_uses_its_own_quota_and_overflow_timeout_runs_after_fifo,
    {
        let mut f = Fixture::new(S0State::Fault(FaultCode::CommFault));
        for index in 0..64 {
            f.runtime.admit_link(if index % 2 == 0 {
                LinkEvent::HeartbeatTimeout
            } else {
                LinkEvent::Heartbeat
            });
        }
        f.runtime.admit_link(LinkEvent::HeartbeatTimeout);
        for _ in 0..32 {
            f.runtime
                .high
                .admit(HighWork::Control(ControlEvent::ClearError))
                .unwrap();
        }
        for index in 0..16 {
            let r = f
                .set_request(&index.to_string(), false, true, u64::MAX)
                .unwrap();
            f.runtime.normal.admit(NormalWork::DryRunSet(r)).unwrap();
        }
        f.tick();
        assert_eq!(f.runtime.high.len(), 8);
        assert!(f.runtime.normal.is_empty());
        assert_eq!(f.runtime.links.len(), 49);
        for _ in 0..16 {
            assert!(matches!(
                f.take_set().payload,
                SetEnabledRrPayload::WouldAccept { .. }
            ));
        }
        for _ in 0..3 {
            f.tick();
        }
        assert!(f.runtime.guards.heartbeat_ok);
        assert_eq!(f.runtime.links.len(), 1);
        f.tick();
        assert!(!f.runtime.guards.heartbeat_ok);
        assert!(f.runtime.links.is_empty());
    }
);
case!(
    rr_waiting_across_two_pending_ticks_expires_without_effects,
    {
        let mut f = Fixture::new(S0State::Safe);
        f.enable("old");
        let now = f.clock.now().0;
        f.send_clear("waiting", false, now + 1);
        f.tick();
        f.tick();
        assert!(f.outbox.clear.is_empty());
        assert!(f.execute_one());
        f.clock.advance(DurationNs(2));
        f.tick();
        assert_eq!(f.take_set().effect, SideEffect::Committed);
        assert_eq!(
            f.take_clear().payload,
            ClearEStopRrPayload::DeadlineExceeded
        );
        assert!(f.runtime.coordination.is_retired_or_absent());
        f.assert_disk(false);
    }
);
case!(late_ssf_is_not_acknowledged_until_next_step_zero_capture, {
    let mut f = Fixture::serial(S0State::Safe);
    f.tick();
    let generation = f.fault(FaultCode::CommFault);
    assert_ne!(f.ctrl.fault_ack.load(Ordering::Acquire), generation);
    f.tick();
    assert_eq!(f.ctrl.fault_ack.load(Ordering::Acquire), generation);
    assert_eq!(f.runtime.state, S0State::Fault(FaultCode::CommFault));
});
case!(rr_observes_link_change_before_control_stop, {
    // W3 高 Stop 先执行；只读 RR 随后观察最终 Stopping，不沿旧“RR先于Stop”顺序。
    let mut f = Fixture::new(S0State::Safe);
    f.send_set("dry", true, true, u64::MAX);
    f.watchdog_tx.send(Event::HeartbeatTimeout).unwrap();
    f.external_tx.send(ControlEvent::Stop).unwrap();
    let snapshot = f.tick();
    assert_eq!(snapshot.state, S0State::Stopping);
    assert!(!f.runtime.guards.heartbeat_ok);
    assert_eq!(
        f.take_set().payload,
        SetEnabledRrPayload::GuardRejected {
            actual: S0State::Stopping
        }
    );
    f.assert_no_motion();
});
case!(
    inhibit_read_seam_rejects_old_zero_even_if_serial_clears_abort,
    {
        // 同原子权限已经替代旧两个可写字；旧 guard 快照不能给新的执行授权。
        let mut f = Fixture::serial(S0State::Safe);
        assert_eq!(f.ctrl.inhibit_bits(), 0);
        f.client
            .abort_pending_normal(robot_core::serial_services::NormalAbortTarget::Unsubmitted);
        let abort = f.ctrl.abort_request.load(Ordering::Acquire);
        let epoch = f.ctrl.authority().snapshot().epoch;
        f.fault(FaultCode::CommFault);
        assert!(f.ctrl.clear_abort(abort));
        assert_ne!(f.ctrl.authority().snapshot().epoch, epoch);
        f.send_set("old-guard", true, false, u64::MAX);
        f.tick();
        assert_eq!(f.take_set().effect, SideEffect::None);
        assert!(!f.runtime.guards.adapter_fault_cleared);
        f.assert_no_motion();
    }
);
case!(
    actual_normal_error_publication_cannot_expose_a_recoverable_fault,
    {
        for (error, expected, trusted) in [
            (
                robot_core::error::RobotError::Comm(
                    robot_core::serial_services::CommErrorKind::Timeout,
                ),
                FaultCode::CommFault,
                false,
            ),
            (
                robot_core::error::RobotError::HardwareFault("真实脚本设备错误"),
                FaultCode::AdapterErr,
                true,
            ),
        ] {
            crate::post_e_convergence_acceptance::normal_failure_publication(
                error,
                |ctrl, clock| {
                    assert!(ctrl.mailbox_snapshot().is_some());
                    assert_ne!(ctrl.inhibit_bits(), 0);
                    assert_eq!(ctrl.rx_trust.load(Ordering::Acquire), trusted);
                    let mut f = Fixture::from_control(S0State::Ready, ctrl, clock);
                    for _ in 0..2 {
                        f.external_tx.send(ControlEvent::ClearError).unwrap();
                        f.external_tx.send(ControlEvent::Enable).unwrap();
                        f.tick();
                        assert_eq!(f.runtime.state, S0State::Fault(expected));
                        assert!(!f.runtime.guards.adapter_fault_cleared);
                        f.assert_no_motion();
                    }
                },
            );
        }
    }
);
case!(
    source_deadline_replies_immediately_even_when_estop_skips_rr_lane,
    {
        let mut f = Fixture::new(S0State::Safe);
        for _ in 0..32 {
            f.runtime
                .high
                .admit(HighWork::Control(ControlEvent::ClearEStop))
                .unwrap();
        }
        f.send_clear("expired-clear", false, 0);
        f.send_set("expired-set", true, false, 0);
        f.tick_topics(
            TopicEvents {
                estop: true,
                heartbeat: false,
            },
            || {},
        );
        assert_eq!(
            f.take_clear().payload,
            ClearEStopRrPayload::DeadlineExceeded
        );
        assert_eq!(f.take_set().payload, SetEnabledRrPayload::DeadlineExceeded);
        assert_eq!(f.runtime.high.len(), 8);
        assert!(f.runtime.coordination.is_retired_or_absent());
    }
);
case!(
    source_full_rejects_immediately_without_dropping_old_rr_prefix,
    {
        let mut f = Fixture::new(S0State::Safe);
        let old = f.clear_request("old", false, u64::MAX).unwrap();
        let route = old.route;
        f.runtime.high.admit(HighWork::LiveClear(old)).unwrap();
        for _ in 1..32 {
            f.runtime
                .high
                .admit(HighWork::Control(ControlEvent::ClearEStop))
                .unwrap();
        }
        f.send_set("full", true, false, u64::MAX);
        f.tick_topics(
            TopicEvents {
                estop: true,
                heartbeat: false,
            },
            || {},
        );
        assert_eq!(f.take_set().payload, SetEnabledRrPayload::QueueFull);
        let original = f.take_clear();
        assert_eq!(original.route, route);
        assert_eq!(original.payload, ClearEStopRrPayload::PreemptedByEmergency);
        assert_eq!(f.runtime.high.len(), 8);
    }
);
case!(
    fault_registration_keeps_original_action_until_completion_and_late_timeout,
    {
        crate::post_e_convergence_acceptance::normal_failure_before_publication(
            robot_core::error::RobotError::HardwareFault("真实故障完成前"),
            |ctrl, clock, seq| {
                assert_eq!(
                    crate::hard_rt_ctrl::ActionRecord::unpack(
                        ctrl.active_action.load(Ordering::Acquire)
                    )
                    .seq,
                    seq
                );
                assert!(
                    HardRtCtrlClient::new(ctrl.clone())
                        .poll_last_normal_resp(seq)
                        .is_none()
                );
                let mut f = Fixture::from_control(S0State::Ready, ctrl, clock);
                for _ in 0..2 {
                    f.external_tx.send(ControlEvent::ClearError).unwrap();
                    f.external_tx.send(ControlEvent::Enable).unwrap();
                    f.tick();
                    assert_eq!(f.runtime.state, S0State::Fault(FaultCode::AdapterErr));
                    f.assert_no_motion();
                }
                f.client.abort_pending_normal(
                    robot_core::serial_services::NormalAbortTarget::Submitted { seq },
                );
            },
        );
    }
);

struct FloodProbe {
    fixture: ManuallyDrop<Fixture>,
    next: u64,
    ticks: u64,
    completed: u64,
    pending: HashMap<String, (u64, Option<u64>)>,
    clear_source: std::collections::VecDeque<String>,
    set_source: std::collections::VecDeque<String>,
    lost_reply: Option<ClearEStopRrResponse>,
    safe_links: bool,
}
impl FloodProbe {
    fn new() -> Self {
        Self::with_fixture(Fixture::new(S0State::Fault(FaultCode::CommFault)), false)
    }
    fn with_fixture(fixture: ManuallyDrop<Fixture>, safe_links: bool) -> Self {
        Self {
            fixture,
            next: 0,
            ticks: 0,
            completed: 0,
            pending: HashMap::with_capacity(128),
            clear_source: std::collections::VecDeque::with_capacity(16),
            set_source: std::collections::VecDeque::with_capacity(16),
            lost_reply: None,
            safe_links,
        }
    }
    fn inject(&mut self) {
        for index in 0..64 {
            let heartbeat = if self.safe_links {
                index < 16
            } else {
                index % 2 == 0
            };
            let event = if heartbeat {
                Event::Heartbeat
            } else {
                Event::HeartbeatTimeout
            };
            match self.fixture.watchdog_tx.try_send(event) {
                Ok(()) | Err(crossbeam_channel::TrySendError::Full(_)) => {}
                Err(error) => panic!("真实源断链：{error}"),
            }
            let event = if self.safe_links {
                ControlEvent::ClearEStop
            } else {
                ControlEvent::AdapterErr(FaultCode::CommFault)
            };
            match self.fixture.external_tx.try_send(event) {
                Ok(()) | Err(crossbeam_channel::TrySendError::Full(_)) => {}
                Err(error) => panic!("真实源断链：{error}"),
            }
        }
        for _ in 0..64 {
            let clear_id = format!("flood-clear-{}", self.next);
            self.next += 1;
            if let Some(request) = self.fixture.clear_request(&clear_id, true, u64::MAX) {
                match self.fixture.clear_normal.try_send(request) {
                    Ok(()) => {
                        assert!(
                            self.pending
                                .insert(clear_id.clone(), (self.ticks, None))
                                .is_none()
                        );
                        self.clear_source.push_back(clear_id);
                    }
                    Err(error) => {
                        let request = error.into_inner();
                        self.fixture.clear_routes.finish_local(request.route);
                    }
                }
            }
            let set_id = format!("flood-set-{}", self.next);
            self.next += 1;
            if let Some(request) = self.fixture.set_request(&set_id, false, true, u64::MAX) {
                match self.fixture.set_normal.try_send(request) {
                    Ok(()) => {
                        assert!(
                            self.pending
                                .insert(set_id.clone(), (self.ticks, None))
                                .is_none()
                        );
                        self.set_source.push_back(set_id);
                    }
                    Err(error) => {
                        let request = error.into_inner();
                        self.fixture.set_routes.finish_local(request.route);
                    }
                }
            }
        }
    }
    fn step(&mut self, lose_one: bool) {
        self.ticks += 1;
        self.inject();
        assert_eq!(self.fixture.watchdog_rx.len(), 64);
        assert_eq!(self.fixture.external_rx.len(), 64);
        let clear_before = self.fixture.clear.normal_request_rx.len();
        let set_before = self.fixture.set.normal_request_rx.len();
        let controls_before = self.fixture.runtime.high.len();
        let external_before = self.fixture.metrics.external_events.load(Ordering::Acquire);
        self.fixture.tick();
        assert_eq!(self.fixture.watchdog_rx.len(), 56);
        assert_eq!(self.fixture.external_rx.len(), 56);
        assert_eq!(
            self.fixture.metrics.external_events.load(Ordering::Acquire) - external_before,
            8
        );
        assert_eq!(
            self.fixture.clear.normal_request_rx.len(),
            clear_before.saturating_sub(8)
        );
        assert_eq!(
            self.fixture.set.normal_request_rx.len(),
            set_before.saturating_sub(8)
        );
        let admitted_controls = (controls_before + 8).min(32);
        if self.safe_links {
            // 真 Enable 仍在设备调用栈内，Restore/普通 Control 保原队列；Lower 与 link/normal 继续。
            assert!(matches!(
                self.fixture.runtime.pending,
                IpcPending::Enable(_)
            ));
            assert_eq!(self.fixture.runtime.high.len(), admitted_controls);
        } else {
            assert_eq!(
                self.fixture.runtime.high.len(),
                admitted_controls.saturating_sub(24)
            );
        }
        for source in [&mut self.clear_source, &mut self.set_source] {
            for _ in 0..source.len().min(8) {
                let id = source.pop_front().unwrap();
                let (start, at) = self.pending.get_mut(&id).unwrap();
                assert!(self.ticks - *start < 8);
                assert!(at.replace(self.ticks).is_none());
            }
        }
        let mut count = 0;
        while let Some(reply) = self.fixture.outbox.clear.pop_front() {
            if lose_one && self.lost_reply.is_none() {
                self.lost_reply = Some(reply);
                continue;
            }
            self.fixture.clear_routes.finish_local(reply.route);
            self.finish(&reply.request_id);
            count += 1;
        }
        while let Some(reply) = self.fixture.outbox.set.pop_front() {
            // Enable 的额外真实回复交整链断言者持有，不能混作灌流已知请求。
            if !reply.request_id.starts_with("flood-") {
                self.fixture.outbox.set.push_front(reply);
                break;
            }
            self.fixture.set_routes.finish_local(reply.route);
            self.finish(&reply.request_id);
            count += 1;
        }
        assert!(count >= 15, "持续灌流必须逐tick前进");
        self.completed += count;
        assert!(self.pending.len() <= 33);
        self.assert_ages();
        self.fixture.clock.advance(DurationNs::from_millis(5));
    }
    fn finish(&mut self, id: &str) {
        let (start, admitted) = self.pending.remove(id).expect("回复唯一对应已接受请求");
        assert!(self.ticks - start <= 14);
        assert!(self.ticks - admitted.expect("必须实际从来源准入") <= 6);
    }
    fn assert_ages(&self) {
        for (id, (start, admitted)) in &self.pending {
            assert!(self.ticks - *start <= 14, "未回复请求总等待超14tick：{id}");
            match admitted {
                Some(at) => assert!(self.ticks - *at <= 6, "未回复请求lane等待超6tick：{id}"),
                None => assert!(self.ticks - *start < 8, "未准入源等待超8tick：{id}"),
            }
        }
    }
    fn drain(&mut self) {
        for _ in 0..14 {
            self.ticks += 1;
            self.fixture.tick();
            for source in [&mut self.clear_source, &mut self.set_source] {
                for _ in 0..source.len().min(8) {
                    let id = source.pop_front().unwrap();
                    self.pending.get_mut(&id).unwrap().1 = Some(self.ticks);
                }
            }
            while !self.fixture.outbox.clear.is_empty() {
                let reply = self.fixture.take_clear();
                self.finish(&reply.request_id);
            }
            while !self.fixture.outbox.set.is_empty() {
                let reply = self.fixture.take_set();
                self.finish(&reply.request_id);
            }
            self.assert_ages();
            if self.pending.is_empty() {
                break;
            }
        }
        assert!(self.pending.is_empty());
        assert!(self.clear_source.is_empty() && self.set_source.is_empty());
        assert!(self.fixture.runtime.normal.is_empty());
    }
}
case!(
    sustained_four_source_flood_obeys_every_tick_quotas_and_rr_wait_bounds,
    {
        let mut probe = FloodProbe::new();
        let start = SystemClock.now().0;
        while SystemClock.now().0 - start < 2_000_000_000 {
            probe.step(false);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(probe.ticks >= 100 && probe.completed >= 1600);
        probe.drain();
    }
);
#[cfg(target_os = "linux")]
fn process_rss_high_water_kib() -> i64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY：只在内核成功初始化完整rusage后读取；Linux ru_maxrss单位KiB。
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    unsafe { usage.assume_init() }.ru_maxrss
}
#[cfg(target_os = "linux")]
case!(isolated_ten_second_flood_rss_growth_is_at_most_one_mib, {
    let mut probe = FloodProbe::new();
    let start = SystemClock.now().0;
    while SystemClock.now().0 - start < 1_000_000_000 {
        probe.step(false);
        std::thread::sleep(Duration::from_millis(5));
    }
    let before = process_rss_high_water_kib();
    let ticks = probe.ticks;
    let start = SystemClock.now().0;
    while SystemClock.now().0 - start < 10_000_000_000 {
        probe.step(false);
        assert!(
            process_rss_high_water_kib() <= before + 1024,
            "预热后RSS高水位增长超1MiB"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let measured = probe.ticks - ticks;
    assert!(measured >= 1000);
    println!(
        "rss_flood_measured_ticks={measured} growth_kib={}",
        process_rss_high_water_kib() - before
    );
    probe.drain();
});
#[test]
fn flood_oracle_rejects_one_lost_reply_despite_other_traffic_progress() {
    let name = "flood_oracle_rejects_one_lost_reply_despite_other_traffic_progress";
    if supervised(name, Some("未回复请求lane等待超6tick")) {
        return;
    }
    let mut probe = FloodProbe::new();
    probe.step(true);
    assert!(probe.lost_reply.is_some());
    for _ in 0..14 {
        probe.step(false);
    }
    panic!("丢回复反证必须被实际oracle拒绝");
}
case!(
    real_enable_retirement_applies_lost_before_snapshot_under_four_source_flood,
    {
        let mut probe = crate::post_e_convergence_acceptance::complete_enable_while(
            false,
            |ctrl, clock| {
                let mut f = Fixture::from_control(S0State::Ready, ctrl, clock);
                f.enable("enable");
                FloodProbe::with_fixture(f, true)
            },
            |probe| {
                for _ in 0..2 {
                    probe.step(false);
                }
            },
        );
        probe
            .fixture
            .runtime
            .admit_link(LinkEvent::HeartbeatTimeout);
        probe.fixture.tick();
        assert!(matches!(
            probe.fixture.runtime.state,
            S0State::Enabled {
                control_link: ControlLink::Lost,
                ..
            }
        ));
        assert_eq!(probe.fixture.take_set().request_id, "enable");
        assert!(matches!(probe.fixture.runtime.pending, IpcPending::Idle));
    }
);
case!(
    real_enable_under_four_source_flood_keeps_stop_disabled_and_retires_to_stopping,
    {
        let mut probe = crate::post_e_convergence_acceptance::complete_enable_while(
            false,
            |ctrl, clock| {
                let mut f = Fixture::from_control(S0State::Ready, ctrl, clock);
                f.enable("enable");
                FloodProbe::with_fixture(f, true)
            },
            |probe| {
                probe.step(false);
                probe
                    .fixture
                    .runtime
                    .high
                    .admit(HighWork::Control(ControlEvent::Stop))
                    .unwrap();
                probe.fixture.tick();
            },
        );
        probe.fixture.tick();
        assert_eq!(probe.fixture.runtime.state, S0State::Stopping);
        probe.fixture.assert_no_motion();
        assert!(
            !probe
                .fixture
                .outbox
                .set
                .iter()
                .any(|reply| reply.effect == SideEffect::Committed)
        );
    }
);

case!(
    live_clear_keeps_sampled_epoch_across_real_estop_claim_without_writes,
    {
        let mut f = Fixture::new(S0State::Safe);
        let sampled = f.ctrl.authority().snapshot().epoch;
        let request = f.clear_request("old-epoch-clear", false, u64::MAX).unwrap();
        let route = request.route;
        let _ = f.client.estop_monotonic();
        {
            let fixture: &mut Fixture = &mut f;
            let context = IpcRuntimeContext {
                ctrl: &fixture.client,
                boot: &fixture.boot,
                clock: fixture.clock.as_ref(),
                metrics: &fixture.metrics,
            };
            fixture.runtime.emergency(&context, &fixture.persist);
        }
        assert!(!f.ctrl.authority().snapshot().ipc_estop_notice);
        assert!(f.ctrl.claim_serial_estop());
        let observation = f
            .ctrl
            .authority()
            .begin_safety_observation(robot_core::control_authority::SafetyOperation::ConfirmOff);
        robot_core::adapter::RtSafeCtrl::safety_stop_rt(&mut f.adapter).unwrap();
        f.ctrl
            .authority()
            .complete_safety_observation(observation)
            .unwrap();
        let end = SystemClock.now().0 + 3_000_000_000;
        while !f.persist.shared().obligation.is_settled()
            || f.persist.shared().progress.load().unwrap().phase != crate::latch_writer::Phase::Idle
        {
            assert!(
                SystemClock.now().0 < end,
                "原 EStop 义务必须由真实 writer 清偿"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let before = f.ctrl.authority().snapshot();
        assert_ne!(before.epoch, sampled);
        let progress = f.persist.shared().progress.load().unwrap();
        let main = std::fs::read(f.directory.path().join("latch")).unwrap();
        let backup = std::fs::read(f.directory.path().join("latch.bak")).unwrap();
        assert_eq!(main, b"1\n");
        assert_eq!(backup, b"1\n");
        {
            // 定向复用生产消费函数传旧 epoch，隔离采样→新 EStop→claim→预约的窗口。
            // 这不是完整 tick 的并发调度或 wire RR 测试，也未复制拒绝分支。
            let fixture: &mut Fixture = &mut f;
            let context = IpcRuntimeContext {
                ctrl: &fixture.client,
                boot: &fixture.boot,
                clock: fixture.clock.as_ref(),
                metrics: &fixture.metrics,
            };
            fixture.runtime.process_restore(
                HighWork::LiveClear(request),
                false,
                sampled,
                &context,
                &mut fixture.persist,
                &mut fixture.outbox,
            );
        }
        let reply = f.take_clear();
        assert_eq!(reply.route, route);
        assert_eq!(reply.request_id, "old-epoch-clear");
        assert!(matches!(
            reply.payload,
            ClearEStopRrPayload::Coordination(ClearOutcome::AuthorityRefused(
                robot_core::control_authority::Refusal::StaleEpoch { .. }
            ))
        ));
        assert_eq!(reply.effect, SideEffect::None);
        assert!(f.outbox.clear.is_empty());
        assert_eq!(f.ctrl.authority().snapshot(), before);
        assert!(f.runtime.coordination.is_retired_or_absent());
        assert!(f.persist.has_credit());
        assert!(f.persist.shared().announce.offered_at().is_none());
        assert_eq!(f.persist.shared().progress.load().unwrap(), progress);
        assert_eq!(
            std::fs::read(f.directory.path().join("latch")).unwrap(),
            main
        );
        assert_eq!(
            std::fs::read(f.directory.path().join("latch.bak")).unwrap(),
            backup
        );
        assert!(!f.directory.path().join("latch.clear_pending").exists());
    }
);
