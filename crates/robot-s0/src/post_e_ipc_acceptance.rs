//! 独立 IPC 验收：只提供输入与执行器完成事实，调度始终调用生产 runtime。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender, bounded};
use robot_core::adapter::ControlMode;
use robot_core::executor_ctrl::{
    AdapterErr, AdapterNormalReq, AdapterResp, ExecutorCtrlClient, NormalReqStatus, OutputPolicy,
    OutputSafetyState,
};
use robot_core::serial_services::NormalAbortTarget;
use robot_core::time::{DurationNs, MockClock, RobotClock};

use crate::event_ingress::{AdmitError, ControlEvent, ExternalEvents, LinkEvent, RrEvent};
use crate::hard_rt_ctrl::{HardRtCtrlClient, HardRtCtrlInner, decode_serial_pending, inhibit};
use crate::ipc_runtime::{IpcLoopState, IpcRuntimeContext, IpcSources, TopicEvents};
use crate::ipc_thread::{
    ClearEStopRrPayload, ClearEStopRrRequest, ClearEStopRrResponse, IpcThreadMetrics, RrChannels,
    SetEnabledRrChannels, SetEnabledRrPayload, SetEnabledRrRequest, SetEnabledRrResponse,
};
use crate::state_machine::{
    ControlLink, EnabledSub, Event, FaultCode, Guards, OutputMode, S0State,
};

struct ObservedCtrl {
    client: HardRtCtrlClient,
    policies: Mutex<Vec<OutputPolicy>>,
    aborts: Mutex<Vec<NormalAbortTarget>>,
    requests: AtomicU64,
    clears: AtomicU64,
    probes: AtomicU64,
    stops: AtomicU64,
}

impl ObservedCtrl {
    fn new(ctrl: Arc<HardRtCtrlInner>) -> Self {
        Self {
            client: HardRtCtrlClient::new(ctrl),
            policies: Mutex::new(Vec::new()),
            aborts: Mutex::new(Vec::new()),
            requests: AtomicU64::new(0),
            clears: AtomicU64::new(0),
            probes: AtomicU64::new(0),
            stops: AtomicU64::new(0),
        }
    }
}

impl ExecutorCtrlClient for ObservedCtrl {
    fn abort_pending_normal(&self, target: NormalAbortTarget) {
        self.aborts.lock().unwrap().push(target);
        self.client.abort_pending_normal(target);
    }
    fn set_policy(&self, policy: OutputPolicy) {
        self.policies.lock().unwrap().push(policy);
        self.client.set_policy(policy);
    }
    fn safety_stop(&self) {
        self.stops.fetch_add(1, Ordering::Relaxed);
        self.client.safety_stop();
    }
    fn request_normal(&self, req: AdapterNormalReq) -> NormalReqStatus {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.client.request_normal(req)
    }
    fn poll_last_normal_resp(&self, seq: u64) -> Option<AdapterResp> {
        self.client.poll_last_normal_resp(seq)
    }
    fn clear_safety_latch(&self) {
        self.clears.fetch_add(1, Ordering::Relaxed);
        self.client.clear_safety_latch();
    }
    fn safety_state(&self) -> OutputSafetyState {
        self.probes.fetch_add(1, Ordering::Relaxed);
        self.client.safety_state()
    }
}

struct Fixture {
    runtime: IpcLoopState,
    ctrl: Arc<HardRtCtrlInner>,
    clock: Arc<MockClock>,
    observed: ObservedCtrl,
    metrics: IpcThreadMetrics,
    watchdog_tx: Sender<Event>,
    watchdog_rx: Receiver<Event>,
    external_tx: Sender<ControlEvent>,
    external_rx: Receiver<ControlEvent>,
    clear_tx: Sender<ClearEStopRrRequest>,
    clear_rx: Receiver<ClearEStopRrResponse>,
    clear: RrChannels,
    set_tx: Sender<SetEnabledRrRequest>,
    set_rx: Receiver<SetEnabledRrResponse>,
    set: SetEnabledRrChannels,
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new(state: S0State) -> Self {
        let (ctrl, clock) = super::post_e_convergence_acceptance::healthy_ipc_control();
        Self::from_control(state, ctrl, clock)
    }

    fn from_control(state: S0State, ctrl: Arc<HardRtCtrlInner>, clock: Arc<MockClock>) -> Self {
        let guards = Guards {
            rt_serial: true,
            estop_latched: false,
            adapter_connected: true,
            calibration_loaded: true,
            joint_state_valid: true,
            power_ok: true,
            heartbeat_ok: true,
            cmd_deadline_valid: true,
            limits_valid: true,
            hw_enable_ok: true,
            adapter_fault_cleared: true,
            command_mode: ControlMode::Position,
            on_timeout_hint: EnabledSub::CtrlLostHold,
        };
        let (watchdog_tx, watchdog_rx) = bounded(64);
        let (external_tx, external_rx) = bounded(64);
        let (clear_tx, request_rx) = bounded(64);
        let (response_tx, clear_rx) = bounded(64);
        let clear = RrChannels {
            request_rx,
            response_tx,
        };
        let (set_tx, request_rx) = bounded(64);
        let (response_tx, set_rx) = bounded(64);
        let set = SetEnabledRrChannels {
            request_rx,
            response_tx,
        };
        let directory = tempfile::tempdir().unwrap();
        crate::estop_latch::save_estop_latch(&directory.path().join("latch"), false).unwrap();
        Self {
            runtime: IpcLoopState::new(state, guards, Arc::new(AtomicBool::new(false))),
            observed: ObservedCtrl::new(ctrl.clone()),
            ctrl,
            clock,
            metrics: IpcThreadMetrics::default(),
            watchdog_tx,
            watchdog_rx,
            external_tx,
            external_rx,
            clear_tx,
            clear_rx,
            clear,
            set_tx,
            set_rx,
            set,
            directory,
        }
    }

    fn tick(&mut self) -> S0State {
        self.tick_with(TopicEvents::default(), || {})
    }

    fn tick_with(&mut self, topics: TopicEvents, after_ingress: impl FnOnce()) -> S0State {
        let path = self.directory.path().join("latch");
        let context = IpcRuntimeContext {
            executor_ctrl: &self.observed,
            serial_ctrl: Some(&self.ctrl),
            emergency_ctrl: Some(&self.observed.client),
            clock: self.clock.as_ref(),
            latch_path: Some(&path),
            metrics: &self.metrics,
        };
        let sources = IpcSources {
            watchdog: &self.watchdog_rx,
            external: &self.external_rx,
            clear_rr: Some(&self.clear),
            set_rr: Some(&self.set),
        };
        self.runtime
            .tick_after_ingress(&context, &sources, || topics, after_ingress)
            .state
    }

    fn clear_request(&self, id: &str, dry_run: bool) -> ClearEStopRrRequest {
        ClearEStopRrRequest {
            request_id: id.into(),
            dry_run,
            confirmation: "确认清急停".into(),
            audit_reason: "独立 IPC 验收".into(),
            deadline_ns: self.clock.now().0 + 5_000_000_000,
        }
    }

    fn set_request(&self, id: &str, enabled: bool, dry_run: bool) -> SetEnabledRrRequest {
        SetEnabledRrRequest {
            request_id: id.into(),
            enabled,
            dry_run,
            deadline_ns: self.clock.now().0 + 5_000_000_000,
            audit_reason: "独立 IPC 验收".into(),
        }
    }

    fn create_enable_pending(&mut self, submitted: bool) {
        assert_eq!(self.runtime.state, S0State::Ready);
        self.tick_with(
            TopicEvents {
                heartbeat: true,
                estop: false,
            },
            || {},
        );
        assert!(
            self.runtime
                .retry
                .pending
                .is_some_and(|(_, seq)| seq.is_none())
        );
        if submitted {
            self.tick();
            assert!(
                self.runtime
                    .retry
                    .pending
                    .is_some_and(|(_, seq)| seq.is_some())
            );
        }
    }

    fn complete_normal(&self, response: AdapterResp) {
        let raw = self.ctrl.pending_normal.swap(0, Ordering::AcqRel);
        let pending = decode_serial_pending(raw).unwrap().unwrap();
        crate::serial_convergence::publish_completion(&self.ctrl, pending.action, response);
    }

    fn fault(&self, code: FaultCode) -> u32 {
        publish_fault(&self.ctrl, code)
    }
}

fn publish_fault(ctrl: &HardRtCtrlInner, code: FaultCode) -> u32 {
    ctrl.publish_fault(
        OutputSafetyState::TorqueOffConfirmed,
        inhibit::COMM_FAULT,
        code,
        0,
        crate::hard_rt_ctrl::FaultOrigin::PendingTimeout,
    )
    .unwrap()
}

fn active() -> S0State {
    S0State::Enabled {
        sub: EnabledSub::Active,
        control_link: ControlLink::Ok,
        output_mode: OutputMode::Active,
    }
}

#[test]
fn both_timeout_identities_block_clear_error_in_same_and_later_ticks() {
    for submitted in [false, true] {
        let mut f = Fixture::new(S0State::Ready);
        f.create_enable_pending(submitted);
        let requests = f.observed.requests.load(Ordering::Relaxed);
        let seq = f.runtime.retry.pending.unwrap().1;
        f.clock.advance(DurationNs::from_millis(1001));
        f.observed.policies.lock().unwrap().clear();
        for _ in 0..4 {
            f.external_tx.send(ControlEvent::ClearError).unwrap();
            f.external_tx.send(ControlEvent::Enable).unwrap();
            assert_eq!(f.tick(), S0State::Fault(FaultCode::CommFault));
            assert!(!f.runtime.guards.adapter_connected);
            assert!(!f.runtime.guards.adapter_fault_cleared);
            assert_eq!(
                f.ctrl.policy.load(Ordering::Acquire),
                OutputPolicy::Disabled.to_u8()
            );
            assert_eq!(f.observed.requests.load(Ordering::Relaxed), requests);
        }
        assert_eq!(
            *f.observed.aborts.lock().unwrap(),
            vec![seq.map_or(NormalAbortTarget::Unsubmitted, |seq| {
                NormalAbortTarget::Submitted { seq }
            })]
        );
        assert_ne!(f.ctrl.abort_request.load(Ordering::Acquire), 0);
        assert!(
            f.observed
                .policies
                .lock()
                .unwrap()
                .iter()
                .all(|p| *p == OutputPolicy::Disabled)
        );
    }
}

#[test]
fn mailbox_after_initial_guard_refresh_cannot_be_cleared_in_same_tick() {
    let mut f = Fixture::new(S0State::Ready);
    f.external_tx.send(ControlEvent::ClearError).unwrap();
    f.external_tx.send(ControlEvent::Enable).unwrap();
    let ctrl = f.ctrl.clone();
    assert_eq!(
        f.tick_with(TopicEvents::default(), || {
            publish_fault(&ctrl, FaultCode::CommFault);
        }),
        S0State::Fault(FaultCode::CommFault)
    );
    assert_eq!(f.ctrl.fault_ack.load(Ordering::Acquire), 1);
    assert!(!f.runtime.guards.adapter_connected);
    assert!(!f.runtime.guards.adapter_fault_cleared);
    assert_eq!(f.observed.requests.load(Ordering::Relaxed), 0);
    assert!(
        f.observed
            .policies
            .lock()
            .unwrap()
            .iter()
            .all(|p| *p == OutputPolicy::Disabled)
    );
}

#[test]
fn ordinary_pending_blocks_lanes_but_mailbox_always_retires_it() {
    let mut f = Fixture::new(S0State::Ready);
    f.create_enable_pending(true);
    f.runtime.events.admit_link(LinkEvent::HeartbeatTimeout);
    f.runtime
        .events
        .admit_control(ControlEvent::Enable)
        .unwrap();
    f.runtime
        .events
        .admit_rr(RrEvent::SetEnabled(f.set_request("禁用", false, false)))
        .unwrap();
    assert_eq!(f.tick(), S0State::Ready);
    assert!(f.set_rx.is_empty());
    assert!(f.runtime.retry.pending.is_some());
    let generation = f.fault(FaultCode::CommFault);
    assert_eq!(f.tick(), S0State::Fault(FaultCode::CommFault));
    assert_eq!(f.ctrl.fault_ack.load(Ordering::Acquire), generation);
    assert!(f.runtime.retry.pending.is_none());
    assert!(matches!(
        f.set_rx.try_recv().unwrap().payload,
        SetEnabledRrPayload::Disabled { .. }
    ));
    assert!(!f.runtime.guards.heartbeat_ok);
}

#[test]
fn completed_enable_applies_queued_link_timeout_before_publishing_snapshot() {
    let mut f = Fixture::new(S0State::Ready);
    f.create_enable_pending(true);
    f.watchdog_tx.send(Event::HeartbeatTimeout).unwrap();
    assert_eq!(f.tick(), S0State::Ready);
    f.complete_normal(AdapterResp::Ok);
    assert_eq!(
        f.tick(),
        S0State::Enabled {
            sub: EnabledSub::CtrlLostHold,
            control_link: ControlLink::Lost,
            output_mode: OutputMode::Hold,
        }
    );
    assert!(f.runtime.retry.pending.is_none());
    assert_eq!(f.observed.aborts.lock().unwrap().len(), 0);
}

#[test]
fn admitted_live_requests_are_preempted_without_republishing_serial_estop() {
    let mut f = Fixture::new(S0State::Safe);
    f.clear_tx.send(f.clear_request("清急停", false)).unwrap();
    f.set_tx.send(f.set_request("禁用", false, false)).unwrap();
    f.tick_with(
        TopicEvents {
            estop: true,
            heartbeat: false,
        },
        || {},
    );
    assert!(f.clear_rx.is_empty());
    assert!(f.set_rx.is_empty());
    f.tick();
    assert_eq!(
        f.clear_rx.try_recv().unwrap().payload,
        ClearEStopRrPayload::PreemptedByEmergency
    );
    assert_eq!(
        f.set_rx.try_recv().unwrap().payload,
        SetEnabledRrPayload::PreemptedByEStop
    );
    assert_eq!(f.observed.clears.load(Ordering::Relaxed), 0);
    assert_eq!(f.observed.stops.load(Ordering::Relaxed), 0);
    assert_eq!(f.metrics.latch_writes.load(Ordering::Relaxed), 1);
    assert!(crate::estop_latch::load_estop_latch(&f.directory.path().join("latch")).unwrap());
    assert!(f.clear_rx.is_empty() && f.set_rx.is_empty());
}

#[test]
fn prefix_counts_dry_and_expired_and_preserves_later_admission() {
    let mut f = Fixture::new(S0State::Safe);
    let mut expired = f.set_request("过期", false, false);
    expired.deadline_ns = f.clock.now().0;
    f.runtime
        .events
        .admit_rr(RrEvent::ClearEStop(f.clear_request("只读", true)))
        .unwrap();
    f.runtime
        .events
        .admit_rr(RrEvent::SetEnabled(expired))
        .unwrap();
    f.runtime
        .events
        .admit_rr(RrEvent::SetEnabled(f.set_request("旧请求", false, false)))
        .unwrap();
    f.tick_with(
        TopicEvents {
            estop: true,
            heartbeat: false,
        },
        || {},
    );
    f.clock.advance(DurationNs::from_millis(1));
    f.set_tx
        .send(f.set_request("新请求", false, false))
        .unwrap();
    f.tick();
    assert!(matches!(
        f.clear_rx.try_recv().unwrap().payload,
        ClearEStopRrPayload::NotTorqueOffConfirmed { .. }
    ));
    let replies: Vec<_> = f
        .set_rx
        .try_iter()
        .map(|r| (r.request_id, r.payload))
        .collect();
    assert_eq!(replies.len(), 3);
    assert_eq!(
        replies[0],
        ("过期".into(), SetEnabledRrPayload::DeadlineExceeded)
    );
    assert_eq!(
        replies[1],
        ("旧请求".into(), SetEnabledRrPayload::PreemptedByEStop)
    );
    assert_eq!(replies[2].0, "新请求");
    assert!(matches!(replies[2].1, SetEnabledRrPayload::Disabled { .. }));
    assert_eq!(f.observed.clears.load(Ordering::Relaxed), 0);
}

#[test]
fn source_not_yet_admitted_is_outside_previous_estop_prefix() {
    let mut f = Fixture::new(S0State::Safe);
    for index in 0..9 {
        f.set_tx
            .send(f.set_request(&index.to_string(), false, false))
            .unwrap();
    }
    f.tick_with(
        TopicEvents {
            estop: true,
            heartbeat: false,
        },
        || {},
    );
    assert_eq!(f.set.request_rx.len(), 1);
    f.tick();
    let replies: Vec<_> = f.set_rx.try_iter().collect();
    assert_eq!(replies.len(), 9);
    assert!(
        replies[..8]
            .iter()
            .all(|r| r.payload == SetEnabledRrPayload::PreemptedByEStop)
    );
    assert_eq!(replies[8].request_id, "8");
    assert!(matches!(
        replies[8].payload,
        SetEnabledRrPayload::Disabled { .. }
    ));
}

#[test]
fn repeated_estop_extends_prefix_to_newly_admitted_requests() {
    let mut f = Fixture::new(S0State::Safe);
    for id in ["第一轮", "第二轮"] {
        f.set_tx.send(f.set_request(id, false, false)).unwrap();
        f.tick_with(
            TopicEvents {
                estop: true,
                heartbeat: false,
            },
            || {},
        );
    }
    f.tick();
    let replies: Vec<_> = f.set_rx.try_iter().collect();
    assert_eq!(replies.len(), 2);
    assert!(
        replies
            .iter()
            .all(|r| r.payload == SetEnabledRrPayload::PreemptedByEStop)
    );
}

#[test]
fn ssf_keeps_queued_disable_idempotent_but_fails_inflight_disable() {
    for inflight in [false, true] {
        for estop in [false, true] {
            let mut f = Fixture::new(active());
            f.set_tx.send(f.set_request("禁用", false, false)).unwrap();
            if inflight {
                f.tick();
                f.tick();
                assert!(f.runtime.retry.pending.is_some());
            }
            let generation = f.fault(FaultCode::SafetyStopFailed);
            assert_eq!(
                f.tick_with(
                    TopicEvents {
                        estop,
                        heartbeat: false
                    },
                    || {}
                ),
                S0State::Fault(FaultCode::SafetyStopFailed)
            );
            assert_eq!(f.ctrl.fault_ack.load(Ordering::Acquire), generation);
            f.tick();
            let payload = f.set_rx.try_recv().unwrap().payload;
            match (inflight, estop) {
                (_, true) => assert_eq!(payload, SetEnabledRrPayload::PreemptedByEStop),
                (true, false) => assert_eq!(payload, SetEnabledRrPayload::AdapterError),
                (false, false) => assert!(matches!(payload, SetEnabledRrPayload::Disabled { .. })),
            }
            assert!(f.set_rx.is_empty());
        }
    }
}

#[test]
fn second_deadline_for_both_rpc_kinds_has_no_evaluator_or_latch_side_effect() {
    for dry_run in [false, true] {
        let mut f = Fixture::new(S0State::Safe);
        let mut clear = f.clear_request("过期清急停", dry_run);
        let mut set = f.set_request("过期启用", true, dry_run);
        clear.deadline_ns = f.clock.now().0;
        set.deadline_ns = f.clock.now().0;
        f.clear_tx.send(clear).unwrap();
        f.set_tx.send(set).unwrap();
        let clock = f.clock.clone();
        let before = f.ctrl.safety_word.load(Ordering::Acquire);
        f.tick_with(TopicEvents::default(), || {
            clock.advance(DurationNs::from_millis(1));
        });
        assert_eq!(
            f.clear_rx.try_recv().unwrap().payload,
            ClearEStopRrPayload::DeadlineExceeded
        );
        assert_eq!(
            f.set_rx.try_recv().unwrap().payload,
            SetEnabledRrPayload::DeadlineExceeded
        );
        assert_eq!(f.observed.probes.load(Ordering::Relaxed), 0);
        assert_eq!(f.metrics.clear_evaluations.load(Ordering::Relaxed), 0);
        assert_eq!(f.metrics.set_evaluations.load(Ordering::Relaxed), 0);
        assert_eq!(f.observed.clears.load(Ordering::Relaxed), 0);
        assert_eq!(f.metrics.latch_writes.load(Ordering::Relaxed), 0);
        assert_eq!(f.ctrl.safety_word.load(Ordering::Acquire), before);
        assert!(f.runtime.retry.pending.is_none());
        assert!(!crate::estop_latch::load_estop_latch(&f.directory.path().join("latch")).unwrap());
        f.set_tx
            .send(f.set_request("后续正常", false, false))
            .unwrap();
        f.tick();
        assert!(matches!(
            f.set_rx.try_recv().unwrap().payload,
            SetEnabledRrPayload::Disabled { .. }
        ));
    }
}

#[test]
fn deadline_equal_now_is_valid_for_both_read_only_evaluators() {
    let mut f = Fixture::new(S0State::Safe);
    let mut clear = f.clear_request("边界清急停", true);
    let mut set = f.set_request("边界启用", true, true);
    clear.deadline_ns = f.clock.now().0;
    set.deadline_ns = f.clock.now().0;
    f.clear_tx.send(clear).unwrap();
    f.set_tx.send(set).unwrap();
    f.tick();
    assert!(matches!(
        f.clear_rx.try_recv().unwrap().payload,
        ClearEStopRrPayload::WouldClear { .. }
    ));
    assert!(matches!(
        f.set_rx.try_recv().unwrap().payload,
        SetEnabledRrPayload::WouldAccept { .. }
    ));
    assert_eq!(f.observed.clears.load(Ordering::Relaxed), 0);
}

#[test]
fn all_four_sources_admit_only_eight_each_tick() {
    let mut f = Fixture::new(S0State::Fault(FaultCode::CommFault));
    for index in 0..16 {
        f.watchdog_tx
            .send(if index % 2 == 0 {
                Event::Heartbeat
            } else {
                Event::HeartbeatTimeout
            })
            .unwrap();
        f.external_tx
            .send(if index % 2 == 0 {
                ControlEvent::Enable
            } else {
                ControlEvent::Disable
            })
            .unwrap();
        f.clear_tx
            .send(f.clear_request(&format!("清{index}"), true))
            .unwrap();
        f.set_tx
            .send(f.set_request(&format!("设{index}"), false, true))
            .unwrap();
    }
    f.tick();
    assert_eq!(
        (
            f.watchdog_rx.len(),
            f.external_rx.len(),
            f.clear.request_rx.len(),
            f.set.request_rx.len()
        ),
        (8, 8, 8, 8)
    );
    assert_eq!((f.clear_rx.len(), f.set_rx.len()), (8, 8));
    assert_eq!(f.metrics.watchdog_events.load(Ordering::Relaxed), 8);
    assert_eq!(f.metrics.external_events.load(Ordering::Relaxed), 8);
}

#[test]
fn external_source_is_bounded_at_64_and_stop_disables_even_when_full() {
    let (ctrl, _) = super::post_e_convergence_acceptance::healthy_ipc_control();
    let observed = Arc::new(ObservedCtrl::new(ctrl.clone()));
    let (external, receiver) =
        ExternalEvents::channel(Arc::new(AtomicBool::new(false)), observed.clone(), None);
    for index in 0..64 {
        external
            .control(if index % 2 == 0 {
                ControlEvent::Enable
            } else {
                ControlEvent::Disable
            })
            .unwrap();
    }
    assert_eq!(receiver.len(), 64);
    assert_eq!(
        external.control(ControlEvent::Enable),
        Err(AdmitError::Full)
    );
    observed.set_policy(OutputPolicy::ActiveForward);
    assert_eq!(external.control(ControlEvent::Stop), Err(AdmitError::Full));
    assert_eq!(
        ctrl.policy.load(Ordering::Acquire),
        OutputPolicy::Disabled.to_u8()
    );
    drop(receiver);
    assert_eq!(
        external.control(ControlEvent::Stop),
        Err(AdmitError::Disconnected)
    );
}

#[test]
fn serial_typed_cancellation_finishes_rr_without_fabricating_fault_or_estop() {
    let mut f = Fixture::new(active());
    f.set_tx
        .send(f.set_request("在途禁用", false, false))
        .unwrap();
    f.tick();
    f.tick();
    assert!(f.runtime.retry.pending.is_some());
    let before = f.ctrl.safety_word.load(Ordering::Acquire);
    // 模拟真实 serial 已生产的完成事实；不发布权威 EStop，不伪造 emergency batch。
    f.complete_normal(AdapterResp::Err(AdapterErr::PreemptedByEmergency));
    assert_eq!(f.tick(), active());
    assert_eq!(
        f.set_rx.try_recv().unwrap().payload,
        SetEnabledRrPayload::PreemptedByEStop
    );
    assert!(f.runtime.retry.pending.is_none());
    assert_eq!(f.ctrl.safety_word.load(Ordering::Acquire), before);
    assert!(!f.runtime.guards.estop_latched);
    assert_eq!(f.metrics.latch_writes.load(Ordering::Relaxed), 0);
    assert!(f.ctrl.mailbox_snapshot().is_none());
    assert_eq!(f.observed.stops.load(Ordering::Relaxed), 0);
}

#[test]
fn mailbox_is_acknowledged_during_four_consecutive_estop_ticks() {
    let mut f = Fixture::new(S0State::Ready);
    for _ in 0..4 {
        let generation = f.fault(FaultCode::CommFault);
        assert_eq!(
            f.tick_with(
                TopicEvents {
                    estop: true,
                    heartbeat: false
                },
                || {}
            ),
            S0State::Fault(FaultCode::CommFault)
        );
        assert_eq!(f.ctrl.fault_ack.load(Ordering::Acquire), generation);
    }
    assert_eq!(f.metrics.latch_writes.load(Ordering::Relaxed), 4);
    assert_eq!(f.observed.stops.load(Ordering::Relaxed), 0);
}

#[test]
fn cancelled_control_action_does_not_cancel_new_rr_owner() {
    let mut f = Fixture::new(active());
    f.external_tx.send(ControlEvent::Disable).unwrap();
    f.tick();
    f.tick();
    f.complete_normal(AdapterResp::Err(AdapterErr::PreemptedByEmergency));
    f.set_tx
        .send(f.set_request("新禁用请求", false, false))
        .unwrap();
    assert_eq!(f.tick(), active());
    assert!(f.set_rx.is_empty(), "旧 control 取消不能替新 RR 生成响应");
    assert!(
        f.runtime
            .retry
            .pending
            .is_some_and(|(_, seq)| seq.is_none())
    );
    f.tick();
    f.complete_normal(AdapterResp::Ok);
    assert_eq!(f.tick(), S0State::Safe);
    let response = f.set_rx.try_recv().unwrap();
    assert_eq!(response.request_id, "新禁用请求");
    assert!(matches!(
        response.payload,
        SetEnabledRrPayload::Disabled { .. }
    ));
    assert!(f.set_rx.is_empty());
}

#[test]
fn abort_handoff_both_interleavings_keep_readiness_false() {
    for complete_before_refresh in [false, true] {
        let mut f = Fixture::new(S0State::Fault(FaultCode::CommFault));
        f.observed
            .abort_pending_normal(NormalAbortTarget::Unsubmitted);
        let abort = f.ctrl.abort_request.load(Ordering::Acquire);
        let retire = || {
            publish_fault(&f.ctrl, FaultCode::CommFault);
            assert!(f.ctrl.clear_abort(abort));
        };
        if complete_before_refresh {
            retire();
        }
        let context = IpcRuntimeContext {
            executor_ctrl: &f.observed,
            serial_ctrl: Some(&f.ctrl),
            emergency_ctrl: None,
            clock: f.clock.as_ref(),
            latch_path: None,
            metrics: &f.metrics,
        };
        f.runtime.refresh_serial_guards_after_abort(&context, || {
            if !complete_before_refresh {
                retire();
            }
        });
        assert_eq!(f.ctrl.abort_request.load(Ordering::Acquire), 0);
        assert!(!f.runtime.guards.adapter_connected);
        assert!(!f.runtime.guards.adapter_fault_cleared);
    }
}

#[test]
fn each_lane_uses_its_own_quota_and_overflow_timeout_runs_after_fifo() {
    let mut f = Fixture::new(S0State::Fault(FaultCode::CommFault));
    for index in 0..64 {
        f.runtime.events.admit_link(if index % 2 == 0 {
            LinkEvent::HeartbeatTimeout
        } else {
            LinkEvent::Heartbeat
        });
    }
    // 第65个异类事件触发 overflow sticky；不改变已有64个FIFO内容。
    f.runtime.events.admit_link(LinkEvent::HeartbeatTimeout);
    for index in 0..64 {
        f.runtime
            .events
            .admit_rr(RrEvent::SetEnabled(f.set_request(
                &index.to_string(),
                false,
                true,
            )))
            .unwrap();
        f.runtime
            .events
            .admit_control(if index % 2 == 0 {
                ControlEvent::Enable
            } else {
                ControlEvent::Disable
            })
            .unwrap();
    }
    for (round, expected) in [
        (1, (48, 40, 40)),
        (2, (32, 16, 16)),
        (3, (16, 0, 0)),
        (4, (0, 0, 0)),
    ] {
        f.tick();
        assert_eq!(f.runtime.events.lane_lengths(), expected, "配额轮 {round}");
        // 恰好16的偶数批均以Heartbeat结束，overflow不得插队。
        assert!(f.runtime.guards.heartbeat_ok);
        let expected_responses = if round <= 2 {
            24
        } else if round == 3 {
            16
        } else {
            0
        };
        assert_eq!(f.set_rx.try_iter().count(), expected_responses);
    }
    f.tick();
    assert!(!f.runtime.guards.heartbeat_ok);
    assert_eq!(f.runtime.events.pop_link(), None);
}

#[test]
fn rr_waiting_across_two_pending_ticks_expires_without_effects() {
    for dry_run in [false, true] {
        let mut f = Fixture::new(S0State::Ready);
        f.create_enable_pending(true);
        let mut clear = f.clear_request("等候清急停", dry_run);
        let mut set = f.set_request("等候禁用", false, dry_run);
        clear.deadline_ns = f.clock.now().0 + 1_000_000;
        set.deadline_ns = clear.deadline_ns;
        f.clear_tx.send(clear).unwrap();
        f.set_tx.send(set).unwrap();
        f.tick();
        f.clock.advance(DurationNs::from_millis(5));
        f.tick();
        assert_eq!(f.runtime.events.lane_lengths().1, 2);
        assert!(f.clear_rx.is_empty() && f.set_rx.is_empty());
        let probes = f.observed.probes.load(Ordering::Relaxed);
        f.complete_normal(AdapterResp::Ok);
        f.tick();
        assert_eq!(
            f.clear_rx.try_recv().unwrap().payload,
            ClearEStopRrPayload::DeadlineExceeded
        );
        assert_eq!(
            f.set_rx.try_recv().unwrap().payload,
            SetEnabledRrPayload::DeadlineExceeded
        );
        assert_eq!(f.observed.probes.load(Ordering::Relaxed), probes);
        assert_eq!(f.observed.clears.load(Ordering::Relaxed), 0);
        assert_eq!(f.metrics.latch_writes.load(Ordering::Relaxed), 0);
        assert!(f.runtime.retry.pending.is_none());
    }
}

#[test]
fn late_ssf_is_not_acknowledged_until_next_step_zero_capture() {
    let mut f = Fixture::new(S0State::Safe);
    let ctrl = f.ctrl.clone();
    f.tick_with(TopicEvents::default(), || {
        publish_fault(&ctrl, FaultCode::SafetyStopFailed);
    });
    assert_eq!(f.ctrl.fault_ack.load(Ordering::Acquire), 0);
    assert_eq!(f.tick(), S0State::Fault(FaultCode::SafetyStopFailed));
    assert_eq!(f.ctrl.fault_ack.load(Ordering::Acquire), 1);
}

#[test]
fn rr_observes_link_change_before_control_stop() {
    let mut f = Fixture::new(active());
    f.runtime.events.admit_link(LinkEvent::HeartbeatTimeout);
    f.runtime
        .events
        .admit_rr(RrEvent::SetEnabled(f.set_request("顺序只读", true, true)))
        .unwrap();
    f.runtime.events.admit_control(ControlEvent::Stop).unwrap();
    assert_eq!(f.tick(), S0State::Stopping);
    assert_eq!(
        f.set_rx.try_recv().unwrap().payload,
        SetEnabledRrPayload::GuardRejected {
            actual: S0State::Enabled {
                sub: EnabledSub::CtrlLostHold,
                control_link: ControlLink::Lost,
                output_mode: OutputMode::Hold,
            },
        }
    );
}

#[test]
fn inhibit_read_seam_rejects_old_zero_even_if_serial_clears_abort() {
    let mut f = Fixture::new(S0State::Fault(FaultCode::CommFault));
    f.observed
        .abort_pending_normal(NormalAbortTarget::Unsubmitted);
    let abort = f.ctrl.abort_request.load(Ordering::Acquire);
    let context = IpcRuntimeContext {
        executor_ctrl: &f.observed,
        serial_ctrl: Some(&f.ctrl),
        emergency_ctrl: None,
        clock: f.clock.as_ref(),
        latch_path: None,
        metrics: &f.metrics,
    };
    f.runtime.refresh_serial_guards_after_inhibit(&context, || {
        publish_fault(&f.ctrl, FaultCode::CommFault);
        assert!(f.ctrl.clear_abort(abort));
    });
    assert!(!f.runtime.guards.adapter_connected);
    assert!(!f.runtime.guards.adapter_fault_cleared);
}

#[test]
fn actual_normal_error_publication_cannot_expose_a_recoverable_fault() {
    for (error, expected, trusted) in [
        (
            robot_core::error::RobotError::Comm(
                robot_core::serial_services::CommErrorKind::Timeout,
            ),
            FaultCode::CommFault,
            false,
        ),
        (
            robot_core::error::RobotError::HardwareFault("独立设备错误"),
            FaultCode::AdapterErr,
            true,
        ),
    ] {
        super::post_e_convergence_acceptance::normal_failure_publication(error, |ctrl, clock| {
            assert!(ctrl.mailbox_snapshot().is_some());
            assert_ne!(ctrl.inhibit.load(Ordering::Acquire), 0);
            assert_eq!(ctrl.rx_trust.load(Ordering::Acquire), trusted);
            let mut f = Fixture::from_control(S0State::Ready, ctrl, clock);
            for _ in 0..2 {
                f.external_tx.send(ControlEvent::ClearError).unwrap();
                f.external_tx.send(ControlEvent::Enable).unwrap();
                assert_eq!(f.tick(), S0State::Fault(expected));
                assert!(!f.runtime.guards.adapter_connected);
                assert!(!f.runtime.guards.adapter_fault_cleared);
                assert_eq!(
                    f.ctrl.policy.load(Ordering::Acquire),
                    OutputPolicy::Disabled.to_u8()
                );
                assert_eq!(f.observed.requests.load(Ordering::Relaxed), 0);
            }
        });
    }
}

#[test]
fn source_deadline_replies_immediately_even_when_estop_skips_rr_lane() {
    let mut f = Fixture::new(S0State::Safe);
    let mut clear = f.clear_request("源过期清", false);
    let mut set = f.set_request("源过期设", true, false);
    clear.deadline_ns = f.clock.now().0 - 1;
    set.deadline_ns = clear.deadline_ns;
    f.clear_tx.send(clear).unwrap();
    f.set_tx.send(set).unwrap();
    f.tick_with(
        TopicEvents {
            estop: true,
            heartbeat: false,
        },
        || {},
    );
    assert_eq!(
        f.clear_rx.try_recv().unwrap().payload,
        ClearEStopRrPayload::DeadlineExceeded
    );
    assert_eq!(
        f.set_rx.try_recv().unwrap().payload,
        SetEnabledRrPayload::DeadlineExceeded
    );
    assert_eq!(f.runtime.events.lane_lengths().1, 0);
    assert!(f.runtime.retry.pending.is_none());
}

#[test]
fn source_full_rejects_immediately_without_dropping_old_rr_prefix() {
    let mut f = Fixture::new(S0State::Safe);
    for index in 0..128 {
        f.runtime
            .events
            .admit_rr(RrEvent::SetEnabled(f.set_request(
                &format!("旧{index}"),
                true,
                true,
            )))
            .unwrap();
    }
    f.set_tx
        .send(f.set_request("满队列新请求", true, false))
        .unwrap();
    f.tick_with(
        TopicEvents {
            estop: true,
            heartbeat: false,
        },
        || {},
    );
    let response = f.set_rx.try_recv().unwrap();
    assert_eq!(response.request_id, "满队列新请求");
    assert_eq!(response.payload, SetEnabledRrPayload::AlreadyInProgress);
    assert_eq!(f.runtime.events.lane_lengths().1, 128);
    assert!(f.runtime.retry.pending.is_none());
}

#[test]
fn fault_registration_keeps_original_action_until_completion_and_late_timeout() {
    super::post_e_convergence_acceptance::normal_failure_before_publication(
        robot_core::error::RobotError::HardwareFault("独立硬件错误"),
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
                assert_eq!(f.tick(), S0State::Fault(FaultCode::AdapterErr));
                assert_eq!(
                    f.ctrl.policy.load(Ordering::Acquire),
                    OutputPolicy::Disabled.to_u8()
                );
                assert!(!f.runtime.guards.adapter_fault_cleared);
            }
            f.observed
                .abort_pending_normal(NormalAbortTarget::Submitted { seq });
        },
    );
}

/// 四源持续灌流只调用真实IPC runtime；仅观测器本身清空历史，生产队列不清空。
struct FloodProbe {
    fixture: Fixture,
    next_request: u64,
    tick: u64,
    pending: std::collections::HashMap<String, (u64, Option<u64>)>,
    clear_source: std::collections::VecDeque<String>,
    set_source: std::collections::VecDeque<String>,
    completed: u64,
}

impl FloodProbe {
    fn new() -> Self {
        let mut probe = Self {
            fixture: Fixture::new(S0State::Fault(FaultCode::CommFault)),
            next_request: 0,
            tick: 0,
            pending: std::collections::HashMap::with_capacity(512),
            clear_source: std::collections::VecDeque::with_capacity(64),
            set_source: std::collections::VecDeque::with_capacity(64),
            completed: 0,
        };
        for index in 0..64 {
            probe.fixture.runtime.events.admit_link(if index % 2 == 0 {
                LinkEvent::HeartbeatTimeout
            } else {
                LinkEvent::Heartbeat
            });
        }
        for index in 0..128 {
            probe
                .fixture
                .runtime
                .events
                .admit_control(if index % 2 == 0 {
                    ControlEvent::Disable
                } else {
                    ControlEvent::AdapterErr(FaultCode::CommFault)
                })
                .unwrap();
            let id = probe.id();
            probe
                .fixture
                .runtime
                .events
                .admit_rr(RrEvent::SetEnabled(
                    probe.fixture.set_request(&id, false, true),
                ))
                .unwrap();
            assert!(probe.pending.insert(id, (0, Some(0))).is_none());
        }
        probe
    }

    fn id(&mut self) -> String {
        self.next_request += 1;
        self.next_request.to_string()
    }

    fn step(&mut self) {
        self.step_with_lost_reply(false);
    }

    fn inject_sources(&mut self) {
        let f = &mut self.fixture;
        // 不是把64当成功发送数：每源恰64次try_send，Full明确计入背压。
        for index in 0..64 {
            match f.watchdog_tx.try_send(if index % 2 == 0 {
                Event::HeartbeatTimeout
            } else {
                Event::Heartbeat
            }) {
                Ok(()) | Err(crossbeam_channel::TrySendError::Full(_)) => {}
                Err(error) => panic!("watchdog源退出：{error}"),
            }
            match f.external_tx.try_send(if index % 2 == 0 {
                ControlEvent::Disable
            } else {
                ControlEvent::AdapterErr(FaultCode::CommFault)
            }) {
                Ok(()) | Err(crossbeam_channel::TrySendError::Full(_)) => {}
                Err(error) => panic!("external源退出：{error}"),
            }
        }
        for _ in 0..64 {
            let id = self.id();
            match self
                .fixture
                .clear_tx
                .try_send(self.fixture.clear_request(&id, true))
            {
                Ok(()) => {
                    assert!(self.pending.insert(id.clone(), (self.tick, None)).is_none());
                    self.clear_source.push_back(id);
                }
                Err(crossbeam_channel::TrySendError::Full(_)) => {}
                Err(error) => panic!("clear源退出：{error}"),
            }
            let id = self.id();
            match self
                .fixture
                .set_tx
                .try_send(self.fixture.set_request(&id, false, true))
            {
                Ok(()) => {
                    assert!(self.pending.insert(id.clone(), (self.tick, None)).is_none());
                    self.set_source.push_back(id);
                }
                Err(crossbeam_channel::TrySendError::Full(_)) => {}
                Err(error) => panic!("set源退出：{error}"),
            }
        }
    }

    fn step_with_lost_reply(&mut self, mut discard_first_reply: bool) {
        self.tick += 1;
        self.inject_sources();
        let f = &mut self.fixture;
        assert_eq!(
            (
                f.watchdog_rx.len(),
                f.external_rx.len(),
                f.clear.request_rx.len(),
                f.set.request_rx.len()
            ),
            (64, 64, 64, 64)
        );
        let (link, rr, control) = f.runtime.events.lane_lengths();
        let admitted = (
            (link + 8).min(64),
            (rr + 16).min(128),
            (control + 8).min(128),
        );
        assert!(f.runtime.retry.pending.is_none());
        let before_wd = f.metrics.watchdog_events.load(Ordering::Relaxed);
        let before_external = f.metrics.external_events.load(Ordering::Relaxed);
        assert_eq!(f.tick(), S0State::Fault(FaultCode::CommFault));
        assert_eq!(
            f.metrics.watchdog_events.load(Ordering::Relaxed) - before_wd,
            8
        );
        assert_eq!(
            f.metrics.external_events.load(Ordering::Relaxed) - before_external,
            8
        );
        assert_eq!(
            (
                f.watchdog_rx.len(),
                f.external_rx.len(),
                f.clear.request_rx.len(),
                f.set.request_rx.len()
            ),
            (56, 56, 56, 56)
        );
        // 每tick独立检查三个lane的实际消耗，不只量总吞吐；非空lane不得饥饿。
        assert_eq!(
            f.runtime.events.lane_lengths(),
            (
                admitted.0.saturating_sub(16),
                admitted.1.saturating_sub(24),
                admitted.2.saturating_sub(24)
            )
        );
        for source in [&mut self.clear_source, &mut self.set_source] {
            for _ in 0..8 {
                let id = source.pop_front().unwrap();
                let (started, admission) = self.pending.get_mut(&id).unwrap();
                assert!(self.tick - *started < 8, "source等待超8tick");
                assert!(admission.replace(self.tick).is_none());
            }
        }
        let mut completed_this_tick = 0;
        for id in f
            .clear_rx
            .try_iter()
            .map(|reply| reply.request_id)
            .chain(f.set_rx.try_iter().map(|reply| reply.request_id))
        {
            if discard_first_reply {
                discard_first_reply = false;
                continue;
            }
            let (started, admitted) = self
                .pending
                .remove(&id)
                .expect("回复必须且只能归属一个已接受请求");
            assert!(self.tick - started <= 14, "总等待超14tick");
            assert!(
                self.tick - admitted.expect("不能在source drain前回复") <= 6,
                "lane等待超6tick"
            );
            completed_this_tick += 1;
        }
        assert!(completed_this_tick >= 16, "持续灌流RR不得连续停顿");
        self.completed += completed_this_tick;
        assert!(self.pending.len() <= 256);
        assert!(self.clear_source.len() <= 64 && self.set_source.len() <= 64);
        f.observed.policies.lock().unwrap().clear();
        f.observed.aborts.lock().unwrap().clear();
        f.clock.advance(DurationNs::from_millis(5));
        self.assert_pending_ages();
    }

    fn assert_pending_ages(&self) {
        for (id, (started, admitted)) in &self.pending {
            assert!(self.tick - started <= 14, "未回复请求总等待超14tick：{id}");
            match admitted {
                Some(admitted) => {
                    assert!(self.tick - admitted <= 6, "未回复请求lane等待超6tick：{id}");
                }
                None => assert!(
                    self.tick - started < 8,
                    "未admit请求source等待超8tick：{id}"
                ),
            }
        }
    }

    fn drain_accepted(&mut self) {
        for _ in 0..14 {
            self.tick += 1;
            self.fixture.tick();
            for source in [&mut self.clear_source, &mut self.set_source] {
                for _ in 0..source.len().min(8) {
                    let id = source.pop_front().unwrap();
                    let (_, admission) = self.pending.get_mut(&id).unwrap();
                    assert!(admission.replace(self.tick).is_none());
                }
            }
            for id in self
                .fixture
                .clear_rx
                .try_iter()
                .map(|reply| reply.request_id)
                .chain(self.fixture.set_rx.try_iter().map(|reply| reply.request_id))
            {
                let (started, admitted) = self.pending.remove(&id).expect("排空时不能重复回复");
                assert!(self.tick - started <= 14);
                assert!(self.tick - admitted.unwrap() <= 6);
            }
            self.assert_pending_ages();
            self.fixture.clock.advance(DurationNs::from_millis(5));
            if self.pending.is_empty() {
                break;
            }
        }
        assert!(self.pending.is_empty(), "停止注入后仍有已接受请求未回复");
        assert!(self.clear_source.is_empty() && self.set_source.is_empty());
        assert_eq!(self.fixture.runtime.events.lane_lengths(), (0, 0, 0));
    }
}

#[test]
fn sustained_four_source_flood_obeys_every_tick_quotas_and_rr_wait_bounds() {
    let mut probe = FloodProbe::new();
    let clock = robot_core::time::SystemClock::new();
    let start = clock.now().0;
    while clock.now().0 - start < 2_000_000_000 {
        probe.step();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(probe.tick >= 100 && probe.completed >= 1600);
    probe.drain_accepted();
}

#[cfg(target_os = "linux")]
fn process_rss_high_water_kib() -> i64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY：内核成功填满rusage后才读取；Linux ru_maxrss单位是KiB。
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    unsafe { usage.assume_init() }.ru_maxrss
}

#[cfg(target_os = "linux")]
#[test]
fn isolated_ten_second_flood_rss_growth_is_at_most_one_mib() {
    const CHILD: &str = "ROBOT_TEST_RSS_FLOOD_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "post_e_ipc_acceptance::isolated_ten_second_flood_rss_growth_is_at_most_one_mib",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("rss_flood_measured_ticks="),
            "不能以零测枚举冒充隔离RSS运行"
        );
        return;
    }
    let clock = robot_core::time::SystemClock::new();
    let mut probe = FloodProbe::new();
    let warmup = clock.now().0;
    while clock.now().0 - warmup < 1_000_000_000 {
        probe.step();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let baseline = process_rss_high_water_kib();
    let start = clock.now().0;
    let before_ticks = probe.tick;
    while clock.now().0 - start < 10_000_000_000 {
        probe.step();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(
            process_rss_high_water_kib() <= baseline + 1024,
            "预热后RSS高水位增长超1MiB"
        );
    }
    let elapsed = clock.now().0 - start;
    assert!(probe.tick - before_ticks >= 1000);
    println!(
        "rss_flood_measured_ticks={} elapsed_ns={elapsed} baseline_kib={baseline} final_kib={}",
        probe.tick - before_ticks,
        process_rss_high_water_kib()
    );
    probe.drain_accepted();
}

#[test]
#[should_panic(expected = "未回复请求")]
fn flood_oracle_rejects_one_lost_reply_despite_other_traffic_progress() {
    let mut probe = FloodProbe::new();
    // 丢弃真实runtime产生的首条回复；首轮其它39条仍能满足吞吐检查。
    probe.step_with_lost_reply(true);
    for _ in 0..15 {
        probe.step();
    }
}

/// Enable 的实际设备调用在栈上暂停；IPC 时间推进由 `MockClock` 控制，完成回执仍由 `SerialTick` 发布。
struct InFlightFlood {
    fixture: Fixture,
    external: ExternalEvents,
    with_stop: bool,
    next_id: u64,
    pending: std::collections::HashMap<String, u64>,
    full_replies: usize,
    deadline_replies: usize,
    original_reply: Option<SetEnabledRrPayload>,
}

impl InFlightFlood {
    fn prepare(ctrl: Arc<HardRtCtrlInner>, clock: Arc<MockClock>, with_stop: bool) -> Self {
        let mut fixture = Fixture::from_control(S0State::Safe, ctrl.clone(), clock);
        let client = Arc::new(HardRtCtrlClient::new(ctrl));
        let (external, receiver) = ExternalEvents::channel(
            Arc::new(AtomicBool::new(false)),
            client.clone(),
            Some(client),
        );
        fixture.external_rx = receiver;
        let mut original = fixture.set_request("真实在途Enable", true, false);
        original.deadline_ns = fixture.clock.now().0 + 1_050_000_000;
        fixture.set_tx.send(original).unwrap();
        assert_eq!(fixture.tick(), S0State::Ready);
        fixture.create_enable_pending(true);
        for clear in [true, false] {
            let id = if clear {
                "lane过期Clear"
            } else {
                "lane过期Set"
            };
            let deadline = fixture.clock.now().0 + 500_000_000;
            if clear {
                let mut request = fixture.clear_request(id, false);
                request.deadline_ns = deadline;
                fixture.clear_tx.send(request).unwrap();
            } else {
                let mut request = fixture.set_request(id, true, false);
                request.deadline_ns = deadline;
                fixture.set_tx.send(request).unwrap();
            }
        }
        let pending = ["lane过期Clear", "lane过期Set"]
            .map(|id| (id.to_owned(), fixture.clock.now().0))
            .into();
        Self {
            fixture,
            external,
            with_stop,
            next_id: 0,
            pending,
            full_replies: 0,
            deadline_replies: 0,
            original_reply: None,
        }
    }

    fn fill_sources(&mut self) {
        for _ in 0..64 {
            assert!(matches!(
                self.fixture.watchdog_tx.try_send(Event::HeartbeatTimeout),
                Ok(()) | Err(crossbeam_channel::TrySendError::Full(_))
            ));
            assert!(matches!(
                self.external.control(ControlEvent::ClearError),
                Ok(()) | Err(AdmitError::Full)
            ));
            self.next_id += 1;
            let id = format!("clear-{}", self.next_id);
            match self
                .fixture
                .clear_tx
                .try_send(self.fixture.clear_request(&id, true))
            {
                Ok(()) => {
                    assert!(
                        self.pending
                            .insert(id, self.fixture.clock.now().0)
                            .is_none()
                    );
                }
                Err(crossbeam_channel::TrySendError::Full(_)) => {}
                Err(error) => panic!("Clear源退出：{error}"),
            }
            let id = format!("set-{}", self.next_id);
            match self
                .fixture
                .set_tx
                .try_send(self.fixture.set_request(&id, true, true))
            {
                Ok(()) => {
                    assert!(
                        self.pending
                            .insert(id, self.fixture.clock.now().0)
                            .is_none()
                    );
                }
                Err(crossbeam_channel::TrySendError::Full(_)) => {}
                Err(error) => panic!("Set源退出：{error}"),
            }
        }
        assert_eq!(self.source_lengths(), (64, 64, 64, 64));
    }

    fn source_lengths(&self) -> (usize, usize, usize, usize) {
        (
            self.fixture.watchdog_rx.len(),
            self.fixture.external_rx.len(),
            self.fixture.clear.request_rx.len(),
            self.fixture.set.request_rx.len(),
        )
    }

    fn run_inflight(&mut self) {
        use crate::hard_rt_ctrl::ActionRecord;
        let action = ActionRecord::unpack(self.fixture.ctrl.active_action.load(Ordering::Acquire));
        assert!(action.valid, "回调必须发生在真实 serial active 发布之后");
        assert_eq!(
            self.fixture.observed.poll_last_normal_resp(action.seq),
            None
        );
        if self.with_stop {
            self.fixture
                .observed
                .set_policy(OutputPolicy::ActiveForward);
            assert_eq!(self.external.control(ControlEvent::Stop), Ok(()));
            assert_eq!(
                self.fixture.ctrl.policy.load(Ordering::Acquire),
                OutputPolicy::Disabled.to_u8()
            );
        }
        let started = self.fixture.clock.now().0;
        let evaluations = (
            self.fixture
                .metrics
                .clear_evaluations
                .load(Ordering::Relaxed),
            self.fixture.metrics.set_evaluations.load(Ordering::Relaxed),
        );
        for tick in 1..=200 {
            self.fill_sources();
            assert_eq!(self.fixture.tick(), S0State::Ready);
            assert_eq!(self.source_lengths(), (56, 56, 56, 56));
            assert_eq!(
                self.fixture.runtime.events.lane_lengths(),
                (1, (tick * 16).min(128), if self.with_stop { 2 } else { 1 })
            );
            assert_eq!(
                (
                    self.fixture
                        .metrics
                        .clear_evaluations
                        .load(Ordering::Relaxed),
                    self.fixture.metrics.set_evaluations.load(Ordering::Relaxed)
                ),
                evaluations
            );
            assert!(self.fixture.runtime.retry.pending.is_some());
            assert_eq!(
                self.fixture.observed.poll_last_normal_resp(action.seq),
                None
            );
            assert!(self.fixture.observed.aborts.lock().unwrap().is_empty());
            if self.with_stop {
                assert_eq!(
                    self.fixture.ctrl.policy.load(Ordering::Acquire),
                    OutputPolicy::Disabled.to_u8()
                );
            }
            self.collect_replies(true);
            self.fixture.clock.advance(DurationNs::from_millis(5));
        }
        assert_eq!(self.fixture.clock.now().0 - started, 1_000_000_000);
        assert!(self.full_replies > 1000, "必须实际达到 RR lane 背压");
        assert!(self.original_reply.is_none());
        assert!(
            self.pending.contains_key("lane过期Clear") && self.pending.contains_key("lane过期Set")
        );
    }

    fn record_reply(&mut self, id: &str, full: bool, expired: bool, in_flight: bool) {
        let started = self
            .pending
            .remove(id)
            .expect("每条回复只能对应一个实际接受的请求");
        assert!(self.fixture.clock.now().0 - started <= 1_175_000_000);
        if in_flight {
            assert!(
                full && !expired,
                "在途期间不能 dequeue 到 evaluator 或第二deadline"
            );
        }
        if id.starts_with("lane过期") {
            assert!(expired && !full);
        }
        self.full_replies += usize::from(full);
        self.deadline_replies += usize::from(expired);
    }

    fn collect_replies(&mut self, in_flight: bool) {
        for reply in self.fixture.clear_rx.try_iter().collect::<Vec<_>>() {
            self.record_reply(
                &reply.request_id,
                reply.payload == ClearEStopRrPayload::AlreadyInProgress,
                reply.payload == ClearEStopRrPayload::DeadlineExceeded,
                in_flight,
            );
        }
        for reply in self.fixture.set_rx.try_iter().collect::<Vec<_>>() {
            if reply.request_id == "真实在途Enable" {
                assert!(self.original_reply.replace(reply.payload).is_none());
            } else {
                self.record_reply(
                    &reply.request_id,
                    reply.payload == SetEnabledRrPayload::AlreadyInProgress,
                    reply.payload == SetEnabledRrPayload::DeadlineExceeded,
                    in_flight,
                );
            }
        }
    }

    fn verify_retirement(mut self) {
        let before = self
            .fixture
            .metrics
            .clear_evaluations
            .load(Ordering::Relaxed)
            + self.fixture.metrics.set_evaluations.load(Ordering::Relaxed);
        let expected = if self.with_stop {
            S0State::Stopping
        } else {
            S0State::Enabled {
                sub: EnabledSub::CtrlLostHold,
                control_link: ControlLink::Lost,
                output_mode: OutputMode::Hold,
            }
        };
        assert_eq!(
            self.fixture.tick(),
            expected,
            "真实 Enable 完成后的首份快照不得出现 Active"
        );
        assert!(self.fixture.runtime.retry.pending.is_none());
        assert_eq!(
            self.fixture
                .metrics
                .clear_evaluations
                .load(Ordering::Relaxed)
                + self.fixture.metrics.set_evaluations.load(Ordering::Relaxed)
                - before,
            22,
            "首24个出队中的两条过期请求不能进入 evaluator"
        );
        self.collect_replies(false);
        assert_eq!(self.deadline_replies, 2);
        assert!(self.fixture.observed.aborts.lock().unwrap().is_empty());
        assert!(self.fixture.ctrl.mailbox_snapshot().is_none());
        for _ in 0..14 {
            self.fixture.clock.advance(DurationNs::from_millis(5));
            assert_eq!(self.fixture.tick(), expected);
            self.collect_replies(false);
        }
        assert!(
            self.pending.is_empty(),
            "停止注入后14tick内必须处理所有已接受的新RR"
        );
        assert_eq!(self.fixture.runtime.events.lane_lengths(), (0, 0, 0));
        assert_eq!(self.source_lengths(), (0, 0, 0, 0));
        assert_eq!(
            self.original_reply,
            Some(if self.with_stop {
                SetEnabledRrPayload::PreemptedByShutdown
            } else {
                SetEnabledRrPayload::DeadlineExceeded
            })
        );
        if self.with_stop {
            assert_eq!(
                self.fixture.ctrl.policy.load(Ordering::Acquire),
                OutputPolicy::Disabled.to_u8()
            );
        }
    }
}

#[test]
fn real_enable_retirement_applies_lost_before_snapshot_under_four_source_flood() {
    let probe = super::post_e_convergence_acceptance::complete_enable_while(
        true,
        |ctrl, clock| InFlightFlood::prepare(ctrl, clock, false),
        InFlightFlood::run_inflight,
    );
    probe.verify_retirement();
}

#[test]
fn real_enable_under_four_source_flood_keeps_stop_disabled_and_retires_to_stopping() {
    let probe = super::post_e_convergence_acceptance::complete_enable_while(
        false,
        |ctrl, clock| InFlightFlood::prepare(ctrl, clock, true),
        InFlightFlood::run_inflight,
    );
    probe.verify_retirement();
}
