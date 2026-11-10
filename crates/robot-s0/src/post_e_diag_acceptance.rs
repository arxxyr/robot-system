//! AC-10 独立验收：真实串口 tick / Feetech 重试产生固定诊断，不在串口线程发 tracing。
//!
//! 脚本只规定设备回包和 transport 错误；调度、重试、恢复和故障收敛全部调用生产实现。

#![cfg(feature = "feetech")]

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::Duration;

use robot_adapter_feetech::adapter::FeetechAdapterConfig;
use robot_adapter_feetech::transport::DrainReport;
use robot_adapter_feetech::{FeetechAdapter, FeetechBus, FeetechError, Motor, SerialTransport};
use robot_core::adapter::{ControlModeRepr, JointCommandBuf};
use robot_core::control_authority::{EnableOwner, InhibitReason, Phase};
use robot_core::diag::{DiagEvent, DiagSink};
use robot_core::executor_ctrl::{
    AdapterErr, AdapterNormalReq, AdapterResp, ExecutorCtrlClient, NormalReqStatus, OutputPolicy,
    OutputSafetyState,
};
use robot_core::serial_services::{CommErrorKind, NormalAbortTarget};
use robot_core::time::{DurationNs, MockClock, MonoRawNs, RobotClock};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

use crate::diag::SerialDiagRing;
use crate::hard_rt_ctrl::{FaultOrigin, HardRtCtrlClient, HardRtCtrlInner};
use crate::serial_convergence::{BootOutcome, ConvergePhase};
use crate::serial_io_thread::{SerialIoThreadMetrics, SerialTick, SerialTickContext};
use crate::serial_preempt::SerialPreemptProbe;
use crate::state_machine::FaultCode;

const SERIAL_THREAD: &str = "s0_serial_io_thread";
const CANARY_TARGET: &str = "post_e_diag_acceptance_canary";
const MOTOR: u8 = 42;
const INITIAL_NS: u64 = 1_000_000_000;

#[derive(Default)]
struct TraceCounts {
    canary: AtomicUsize,
    business_events: AtomicUsize,
    business_spans: AtomicUsize,
    wrong_thread: AtomicUsize,
}

struct LocalSubscriber(Arc<TraceCounts>);

impl LocalSubscriber {
    fn check_thread(&self) {
        if std::thread::current().name() != Some(SERIAL_THREAD) {
            self.0.wrong_thread.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Subscriber for LocalSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        self.check_thread();
        let count = self.0.business_spans.fetch_add(1, Ordering::Relaxed);
        Id::from_u64(u64::try_from(count).unwrap() + 1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}
    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
    fn enter(&self, _span: &Id) {}
    fn exit(&self, _span: &Id) {}

    fn event(&self, event: &Event<'_>) {
        self.check_thread();
        let counter = match event.metadata().target() {
            CANARY_TARGET => &self.0.canary,
            _ => &self.0.business_events,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

fn in_serial_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> (T, Arc<TraceCounts>) {
    let counts = Arc::new(TraceCounts::default());
    let captured = counts.clone();
    let result = std::thread::Builder::new()
        .name(SERIAL_THREAD.to_owned())
        .spawn(move || {
            tracing::subscriber::with_default(LocalSubscriber(captured.clone()), || {
                assert_eq!(std::thread::current().name(), Some(SERIAL_THREAD));
                tracing::warn!(target: "post_e_diag_acceptance_canary", phase = "before");
                assert_eq!(
                    captured.canary.load(Ordering::Relaxed),
                    1,
                    "前置 canary 必须真实可见"
                );
                let result = work();
                tracing::warn!(target: "post_e_diag_acceptance_canary", phase = "after");
                assert_eq!(
                    captured.canary.load(Ordering::Relaxed),
                    2,
                    "业务区后订阅器仍须生效"
                );
                result
            })
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(counts.wrong_thread.load(Ordering::Relaxed), 0);
    (result, counts)
}

/// 返回包在对应请求发出时才进入 RX；hygiene 不会丢掉尚未发送请求的未来回包。
enum Reply {
    Bytes(Vec<u8>),
    NoResponseRequired,
    ReadTimeout,
    WriteFailure,
}

struct WireStep {
    tx: Vec<u8>,
    reply: Reply,
}

struct ScriptedTransport {
    steps: VecDeque<WireStep>,
    pending_rx: Option<Reply>,
    clock: Arc<MockClock>,
    writes: Arc<AtomicUsize>,
}

impl SerialTransport for ScriptedTransport {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), FeetechError> {
        assert!(self.pending_rx.is_none(), "上一事务的回包尚未消费");
        let step = self
            .steps
            .pop_front()
            .expect("真实实现发出了脚本外的额外 TX");
        assert_eq!(bytes, step.tx, "实际请求必须匹配独立设备脚本");
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.clock.advance(DurationNs::from_millis(1));
        match step.reply {
            Reply::WriteFailure => Err(FeetechError::TransportWrite(std::io::Error::other(
                "脚本 TX 失败",
            ))),
            Reply::NoResponseRequired => Ok(()),
            reply => {
                self.pending_rx = Some(reply);
                Ok(())
            }
        }
    }

    fn read_exact(&mut self, bytes: &mut [u8]) -> Result<(), FeetechError> {
        self.clock.advance(DurationNs::from_millis(1));
        match self.pending_rx.take().expect("没有对应 TX 的 RX 调用") {
            Reply::Bytes(reply) => {
                assert_eq!(bytes.len(), reply.len(), "真实读取长度必须匹配完整设备响应");
                bytes.copy_from_slice(&reply);
                Ok(())
            }
            Reply::ReadTimeout => Err(FeetechError::TransportTimeout {
                timeout_ms: 50,
                wanted: bytes.len(),
                got: 0,
            }),
            Reply::NoResponseRequired | Reply::WriteFailure => {
                panic!("无回包事务不能进入 read_exact")
            }
        }
    }

    fn set_timeout(&mut self, timeout: Duration) -> Result<(), FeetechError> {
        assert_eq!(timeout, Duration::from_millis(50));
        Ok(())
    }

    fn is_connected(&self) -> bool {
        true
    }
    fn discard_input(&mut self) -> Result<(), FeetechError> {
        self.pending_rx = None;
        Ok(())
    }
    fn drain_until_quiet(
        &mut self,
        quiet: Duration,
        budget: Duration,
    ) -> Result<DrainReport, FeetechError> {
        assert_eq!(quiet, Duration::from_millis(50));
        assert_eq!(budget, Duration::from_millis(150));
        self.clock.advance(DurationNs::from_millis(50));
        Ok(DrainReport {
            bytes: 0,
            deadline_hit: false,
        })
    }
}

/// 测试侧直接编码协议字面，不借生产编码器生成 expected TX。
fn packet(id: u8, instruction_or_status: u8, parameters: &[u8]) -> Vec<u8> {
    let mut frame = vec![
        0xff,
        0xff,
        id,
        u8::try_from(parameters.len() + 2).unwrap(),
        instruction_or_status,
    ];
    frame.extend_from_slice(parameters);
    let checksum = !frame[2..]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    frame.push(checksum);
    frame
}

fn status(status: u8, data: &[u8]) -> Reply {
    Reply::Bytes(packet(MOTOR, status, data))
}

fn sync_read(address: u8, width: u8, reply: Reply) -> WireStep {
    WireStep {
        tx: packet(0xfe, 0x82, &[address, width, MOTOR]),
        reply,
    }
}

fn safety(reply: Reply) -> WireStep {
    WireStep {
        tx: packet(0xfe, 0x83, &[40, 1, MOTOR, 0]),
        reply,
    }
}

fn goal() -> WireStep {
    WireStep {
        tx: packet(0xfe, 0x83, &[42, 2, MOTOR, 0xdc, 0x05]),
        reply: Reply::NoResponseRequired,
    }
}

fn torque(value: u8, reply: Reply) -> WireStep {
    WireStep {
        tx: packet(MOTOR, 3, &[40, value]),
        reply,
    }
}

fn boot_script() -> Vec<WireStep> {
    vec![
        // 真实 preflight：Safety、Connect 两子事务、Confirm。
        safety(Reply::NoResponseRequired),
        WireStep {
            tx: packet(MOTOR, 1, &[]),
            reply: status(0, &[]),
        },
        WireStep {
            tx: packet(MOTOR, 2, &[3, 2]),
            reply: status(0, &[0x09, 0x03]),
        },
        sync_read(40, 1, status(0, &[0])),
    ]
}

fn device_script() -> Vec<WireStep> {
    let mut script = boot_script();
    script.extend([
        // Enable(true)：目标回读先不匹配，再恢复；扭矩写先设备 NACK，再恢复。
        sync_read(56, 2, status(0, &[0xdc, 0x05])),
        goal(),
        sync_read(42, 2, status(0, &[0xff, 0x07])),
        goal(),
        sync_read(42, 2, status(0, &[0xdc, 0x05])),
        torque(1, status(0x24, &[])),
        torque(1, status(0, &[])),
        // 成功 normal 后同一真实 tick 继续周期采样。
        sync_read(56, 2, status(0, &[0xdc, 0x05])),
        sync_read(58, 2, status(0, &[0, 0])),
        // Disable 超时进入真实 Fault，Safety 第一次写失败，随后写和确认成功。
        torque(0, Reply::ReadTimeout),
        safety(Reply::WriteFailure),
        safety(Reply::NoResponseRequired),
        sync_read(40, 1, status(0, &[0])),
    ]);
    script
}

/// 仅在真实权限拒绝已经产生诊断后插入并发保护；不伪造 adapter 返回值。
struct RevokedBoundaryDiag {
    diag: Arc<SerialDiagRing>,
    ctrl: Arc<HardRtCtrlInner>,
    action: Arc<AtomicU8>,
}

impl DiagSink for RevokedBoundaryDiag {
    fn record(&self, event: DiagEvent) {
        self.diag.record(event);
        if (event.site, event.kind, event.a) == (5, 7, 3) {
            match self.action.swap(0, Ordering::Relaxed) {
                0 => {}
                1 => {
                    let _ = self.ctrl.authority().estop();
                }
                2 => HardRtCtrlClient::new(self.ctrl.clone())
                    .abort_pending_normal(NormalAbortTarget::Unsubmitted),
                3 => {
                    let _ = self
                        .ctrl
                        .authority()
                        .tighten_inhibit(InhibitReason::AdapterFault);
                }
                _ => panic!("无效的测试保护操作"),
            }
        }
    }
}

struct TickFixture {
    clock: Arc<MockClock>,
    ctrl: Arc<HardRtCtrlInner>,
    probe: Arc<SerialPreemptProbe>,
    diag: Arc<SerialDiagRing>,
    adapter: FeetechAdapter<ScriptedTransport>,
    tick: SerialTick,
    metrics: SerialIoThreadMetrics,
    writes: Arc<AtomicUsize>,
    expected_writes: usize,
    after_revoked: Arc<AtomicU8>,
}

impl TickFixture {
    fn new() -> Self {
        Self::with_script(device_script())
    }

    fn with_script(script: Vec<WireStep>) -> Self {
        let clock = Arc::new(MockClock::new(MonoRawNs(INITIAL_NS)));
        let ctrl = Arc::new(HardRtCtrlInner::new_serial(
            crate::hard_rt_ctrl::test_authority(true),
        ));
        let probe = Arc::new(SerialPreemptProbe::new(clock.clone(), ctrl.clone()));
        let diag = Arc::new(SerialDiagRing::new());
        let writes = Arc::new(AtomicUsize::new(0));
        let expected_writes = script.len();
        let after_revoked = Arc::new(AtomicU8::new(0));
        let transport = ScriptedTransport {
            steps: script.into(),
            pending_rx: None,
            clock: clock.clone(),
            writes: writes.clone(),
        };
        let bus = FeetechBus::new(
            transport,
            HashMap::from([("diagnostic_joint".to_owned(), Motor { id: MOTOR })]),
        )
        .unwrap();
        let adapter = FeetechAdapter::new(
            FeetechAdapterConfig {
                clock: clock.clone(),
                preempt: Some(probe.clone()),
                diag: Some(Arc::new(RevokedBoundaryDiag {
                    diag: diag.clone(),
                    ctrl: ctrl.clone(),
                    action: after_revoked.clone(),
                })),
                call_deadline: Duration::from_millis(50),
                adapter_name: "独立诊断验收设备",
                joint_names: vec!["diagnostic_joint".to_owned()],
                calibrations: Vec::new(),
                calibration_range_tolerances_raw: Vec::new(),
            },
            bus,
        )
        .unwrap();
        Self {
            tick: SerialTick::new(true, 1, clock.now()),
            clock,
            ctrl,
            probe,
            diag,
            adapter,
            metrics: SerialIoThreadMetrics::default(),
            writes,
            expected_writes,
            after_revoked,
        }
    }

    fn step(&mut self) {
        self.step_command(&JointCommandBuf::default());
    }

    fn step_command(&mut self, command: &JointCommandBuf) {
        let deadline = MonoRawNs(self.clock.now().0 + 5_000_000);
        let context = SerialTickContext {
            ctrl: &self.ctrl,
            probe: &self.probe,
            diag: self.diag.as_ref(),
            clock: self.clock.as_ref(),
            metrics: &self.metrics,
        };
        self.tick
            .tick(&mut self.adapter, &command.as_view(), &context, deadline);
        self.clock.advance(DurationNs::from_millis(5));
    }

    fn normal(&mut self, enabled: bool, expected_response: AdapterResp) {
        if let Some(owner) = self.normal_before_commit(enabled, expected_response) {
            self.commit_enable(owner);
        }
    }

    fn normal_before_commit(
        &mut self,
        enabled: bool,
        expected_response: AdapterResp,
    ) -> Option<std::mem::ManuallyDrop<EnableOwner>> {
        let client = HardRtCtrlClient::new(self.ctrl.clone());
        let enable_owner = if enabled {
            let (owner, view) = self.ctrl.authority().reserve_enable().unwrap();
            // 断言失败不能丢失原 owner；仅原 seq 真完成后才按值移交 commit。
            let owner = std::mem::ManuallyDrop::new(owner);
            self.ctrl.start_slot().post(view).unwrap();
            Some(owner)
        } else {
            None
        };
        let NormalReqStatus::Submitted { seq } =
            client.request_normal(AdapterNormalReq::Enable(enabled))
        else {
            panic!("健康启动后必须接受 normal 请求")
        };
        self.step();
        assert_eq!(client.poll_last_normal_resp(seq), Some(expected_response));
        enable_owner
    }

    fn commit_enable(&self, owner: std::mem::ManuallyDrop<EnableOwner>) {
        match self
            .ctrl
            .authority()
            .commit_enable(std::mem::ManuallyDrop::into_inner(owner))
        {
            Ok(_) => {}
            Err(original) => {
                let reason = original.reason;
                let _held = std::mem::ManuallyDrop::new((original, self.ctrl.clone()));
                panic!("真实 Enable 成功后必须能提交原 owner：{reason:?}");
            }
        }
    }

    fn boot(&mut self) {
        for phase in [
            ConvergePhase::Connect,
            ConvergePhase::Confirm,
            ConvergePhase::Idle,
        ] {
            self.step();
            assert_eq!(self.tick.convergence().phase(), phase);
        }
        assert_eq!(self.tick.convergence().boot_outcome(), BootOutcome::Passed);
        assert!(self.ctrl.rx_trust.load(Ordering::Acquire));
    }

    fn periodic_position(&mut self) {
        // 默认 Stop 会绕过 Motion 租约；这里明确发一个非空 Position 命令。
        self.step_command(&JointCommandBuf {
            mode: ControlModeRepr::Position,
            joint_count: 1,
            ..JointCommandBuf::default()
        });
    }

    fn assert_periodic_sample(&mut self, motion_write: bool, raw: u16, velocity: u16) {
        let writes = self.writes.load(Ordering::Relaxed);
        let stamp = self.tick.state().stamp_ns;
        self.periodic_position();
        assert_eq!(
            self.writes.load(Ordering::Relaxed) - writes,
            2 + usize::from(motion_write),
            "拒绝 Motion 时只能发送两条采样请求；获准时才增加目标写"
        );
        let state = self.tick.state();
        assert!(state.stamp_ns > stamp);
        assert_eq!(state.joint_count, 1);
        let expected = (f32::from(raw) - 2047.0) * std::f32::consts::TAU / 4096.0;
        assert!((state.positions[0] - expected).abs() < 1e-6);
        assert_eq!(state.velocities[0].to_bits(), f32::from(velocity).to_bits());
        assert_eq!(self.tick.convergence().phase(), ConvergePhase::Idle);
        assert_eq!(self.ctrl.mailbox_snapshot(), None);
        assert!(self.ctrl.rx_trust.load(Ordering::Acquire));
        assert_eq!(self.ctrl.comm_level.load(Ordering::Acquire), 0);
        assert!(!self.ctrl.authority().snapshot().normal_io_active);
    }

    fn periodic_write_diagnostics(&self) -> Vec<(u8, u8)> {
        let mut events = Vec::new();
        while let Some(event) = self.diag.pop() {
            if event.site == 5 {
                events.push((event.kind, event.a));
            }
        }
        assert_eq!(self.diag.dropped(), 0);
        events
    }
}

fn periodic_reads(raw: u16, velocity: u16) -> [WireStep; 2] {
    [
        sync_read(56, 2, status(0, &raw.to_le_bytes())),
        sync_read(58, 2, status(0, &velocity.to_le_bytes())),
    ]
}

fn enable_script() -> [WireStep; 4] {
    [
        sync_read(56, 2, status(0, &[0xdc, 0x05])),
        goal(),
        sync_read(42, 2, status(0, &[0xdc, 0x05])),
        torque(1, status(0, &[])),
    ]
}

#[test]
fn periodic_revoked_motion_preserves_reads_before_commit_and_after_disable() {
    let mut script = boot_script();
    script.extend(periodic_reads(2048, 1));
    script.extend(enable_script());
    // normal 成功的同一 tick 已执行一次采样，然后观察尚未 commit 的窗口。
    script.extend(periodic_reads(2049, 2));
    script.extend(periodic_reads(2050, 3));
    script.push(WireStep {
        tx: packet(0xfe, 0x83, &[42, 2, MOTOR, 0xff, 0x07]),
        reply: Reply::NoResponseRequired,
    });
    script.extend(periodic_reads(2051, 4));
    script.extend(periodic_reads(2052, 5));
    let mut f = TickFixture::with_script(script);
    f.boot();
    f.ctrl
        .policy
        .store(OutputPolicy::ReadyHold.to_u8(), Ordering::Release);
    assert!(f.ctrl.authority().snapshot().disabled);
    f.assert_periodic_sample(false, 2048, 1);

    let owner = f.normal_before_commit(true, AdapterResp::Ok).unwrap();
    assert_eq!(
        f.ctrl.authority().snapshot().phase,
        Some(Phase::EnableSucceeded)
    );
    assert_eq!(f.ctrl.safety_state(), OutputSafetyState::TorqueOn);
    assert!(f.ctrl.authority().snapshot().disabled);
    f.assert_periodic_sample(false, 2050, 3);
    f.commit_enable(owner);
    assert!(!f.ctrl.authority().snapshot().disabled);
    f.assert_periodic_sample(true, 2051, 4);

    let _ = f.ctrl.authority().disable();
    f.assert_periodic_sample(false, 2052, 5);
    assert!(f.ctrl.authority().snapshot().disabled);
    // 此处只撤权，未执行 Disable 设备请求；不得凭读取成功伪造断扭事实。
    assert_eq!(f.ctrl.safety_state(), OutputSafetyState::TorqueOn);
    assert_eq!(f.metrics.normal_completed_count.load(Ordering::Relaxed), 1);
    assert_eq!(f.metrics.write_errors.load(Ordering::Relaxed), 3);
    assert_eq!(f.metrics.read_errors.load(Ordering::Relaxed), 0);
    assert_eq!(f.periodic_write_diagnostics(), [(7, 3); 3]);
    assert_eq!(f.writes.load(Ordering::Relaxed), f.expected_writes);
}

#[test]
fn periodic_revoked_motion_rechecks_estop_timeout_and_inhibit_before_read() {
    for action in 1..=3 {
        let mut f = TickFixture::with_script(boot_script());
        f.boot();
        f.ctrl
            .policy
            .store(OutputPolicy::ReadyHold.to_u8(), Ordering::Release);
        let previous = *f.tick.state();
        f.after_revoked.store(action, Ordering::Relaxed);
        f.periodic_position();
        assert_eq!(f.after_revoked.load(Ordering::Relaxed), 0);
        assert_eq!(f.writes.load(Ordering::Relaxed), f.expected_writes);
        assert_eq!(f.tick.state().stamp_ns, previous.stamp_ns);
        assert_eq!(f.metrics.write_errors.load(Ordering::Relaxed), 1);
        assert_eq!(f.metrics.read_errors.load(Ordering::Relaxed), 0);
        assert!(!f.ctrl.authority().snapshot().normal_io_active);
        assert!(f.ctrl.rx_trust.load(Ordering::Acquire));
        assert_ne!(f.ctrl.inhibit_bits(), 0);
        assert_eq!(f.periodic_write_diagnostics(), [(7, 3)]);
        match action {
            1 => {
                assert_eq!(f.tick.convergence().phase(), ConvergePhase::StopIssue);
                assert!(f.ctrl.authority().snapshot().latch);
                assert_eq!(f.ctrl.mailbox_snapshot(), None);
            }
            2 => {
                assert_eq!(f.tick.convergence().phase(), ConvergePhase::StopIssue);
                let fault = f.ctrl.mailbox_snapshot().unwrap();
                assert_eq!(fault.code, FaultCode::CommFault);
                assert_eq!(fault.detail, CommErrorKind::PendingDeadline.to_u8());
                assert_eq!(fault.origin, FaultOrigin::PendingTimeout);
            }
            3 => {
                assert_eq!(f.tick.convergence().phase(), ConvergePhase::Idle);
                assert_eq!(f.ctrl.mailbox_snapshot(), None);
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn periodic_transport_write_failure_still_faults_without_reading() {
    let mut script = boot_script();
    script.extend(enable_script());
    script.extend(periodic_reads(2048, 1));
    script.push(WireStep {
        tx: packet(0xfe, 0x83, &[42, 2, MOTOR, 0xff, 0x07]),
        reply: Reply::WriteFailure,
    });
    let mut f = TickFixture::with_script(script);
    f.boot();
    f.normal(true, AdapterResp::Ok);
    f.ctrl
        .policy
        .store(OutputPolicy::ActiveForward.to_u8(), Ordering::Release);
    let previous = *f.tick.state();
    f.periodic_position();
    assert_eq!(f.writes.load(Ordering::Relaxed), f.expected_writes);
    assert_eq!(f.tick.state().stamp_ns, previous.stamp_ns);
    assert_eq!(
        f.tick.state().positions.map(f32::to_bits),
        previous.positions.map(f32::to_bits)
    );
    assert_eq!(
        f.tick.state().velocities.map(f32::to_bits),
        previous.velocities.map(f32::to_bits)
    );
    assert_eq!(f.metrics.write_errors.load(Ordering::Relaxed), 1);
    assert_eq!(f.metrics.read_errors.load(Ordering::Relaxed), 0);
    assert_eq!(f.tick.convergence().phase(), ConvergePhase::StopIssue);
    let fault = f.ctrl.mailbox_snapshot().unwrap();
    assert_eq!(fault.code, FaultCode::CommFault);
    assert_eq!(fault.detail, CommErrorKind::IoWrite.to_u8());
    assert_eq!(fault.origin, FaultOrigin::Periodic);
    assert!(!f.ctrl.rx_trust.load(Ordering::Acquire));
    assert!(!f.ctrl.authority().snapshot().normal_io_active);
    assert_eq!(f.periodic_write_diagnostics(), [(0, 3)]);
}

#[test]
fn periodic_read_failure_after_revoked_motion_never_commits_partial_state() {
    let mut script = boot_script();
    script.extend(periodic_reads(2048, 1));
    script.push(sync_read(56, 2, status(0, &[0x00, 0x09])));
    script.push(sync_read(58, 2, Reply::ReadTimeout));
    let mut f = TickFixture::with_script(script);
    f.boot();
    f.ctrl
        .policy
        .store(OutputPolicy::ReadyHold.to_u8(), Ordering::Release);
    f.assert_periodic_sample(false, 2048, 1);
    let previous = *f.tick.state();
    f.periodic_position();
    assert_eq!(f.writes.load(Ordering::Relaxed), f.expected_writes);
    assert_eq!(f.tick.state().stamp_ns, previous.stamp_ns);
    assert_eq!(
        f.tick.state().positions.map(f32::to_bits),
        previous.positions.map(f32::to_bits)
    );
    assert_eq!(
        f.tick.state().velocities.map(f32::to_bits),
        previous.velocities.map(f32::to_bits)
    );
    assert_eq!(f.metrics.write_errors.load(Ordering::Relaxed), 2);
    assert_eq!(f.metrics.read_errors.load(Ordering::Relaxed), 1);
    assert_eq!(f.tick.convergence().phase(), ConvergePhase::StopIssue);
    assert!(!f.ctrl.rx_trust.load(Ordering::Acquire));
    assert!(!f.ctrl.authority().snapshot().normal_io_active);
    let fault = f.ctrl.mailbox_snapshot().unwrap();
    assert_eq!(fault.code, FaultCode::CommFault);
    assert_eq!(fault.detail, CommErrorKind::Timeout.to_u8());
    assert_eq!(fault.origin, FaultOrigin::Periodic);
    assert_eq!(f.periodic_write_diagnostics(), [(7, 3); 2]);
}

fn exercise_real_serial_path() -> Vec<DiagEvent> {
    let mut fixture = TickFixture::new();
    for phase in [
        ConvergePhase::Connect,
        ConvergePhase::Confirm,
        ConvergePhase::Idle,
    ] {
        fixture.step();
        assert_eq!(fixture.tick.convergence().phase(), phase);
    }
    assert_eq!(
        fixture.tick.convergence().boot_outcome(),
        BootOutcome::Passed
    );
    assert!(fixture.ctrl.rx_trust.load(Ordering::Acquire));
    fixture.normal(true, AdapterResp::Ok);
    assert_eq!(fixture.ctrl.safety_state(), OutputSafetyState::TorqueOn);
    fixture.normal(
        false,
        AdapterResp::Err(AdapterErr::Comm(CommErrorKind::Timeout)),
    );
    assert_eq!(
        fixture.ctrl.mailbox_snapshot().unwrap().code,
        FaultCode::CommFault
    );
    for phase in [
        ConvergePhase::StopRetry,
        ConvergePhase::Confirm,
        ConvergePhase::AwaitAck,
    ] {
        fixture.step();
        assert_eq!(fixture.tick.convergence().phase(), phase);
    }
    assert_eq!(
        fixture.ctrl.safety_state(),
        OutputSafetyState::AdapterUncertain
    );
    assert_eq!(
        fixture.writes.load(Ordering::Relaxed),
        fixture.expected_writes,
        "设备脚本必须全部实际执行"
    );
    assert_eq!(
        fixture
            .metrics
            .normal_completed_count
            .load(Ordering::Relaxed),
        2
    );
    assert_eq!(fixture.metrics.tick_count.load(Ordering::Relaxed), 8);
    assert_eq!(fixture.diag.dropped(), 0);
    let mut events = Vec::new();
    while let Some(event) = fixture.diag.pop() {
        events.push(event);
    }
    assert!(!events.is_empty(), "真实设备诊断不能退化为空样本");
    assert!(
        events
            .iter()
            .all(|event| (INITIAL_NS..=fixture.clock.now().0).contains(&event.stamp_ns))
    );
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].stamp_ns <= pair[1].stamp_ns)
    );
    events
}

#[test]
fn serial_thread_emits_no_tracing() {
    let (events, counts) = in_serial_thread(exercise_real_serial_path);
    assert!(
        events
            .iter()
            .any(|event| event.site == 20 && event.kind == 7),
        "必须实际经过停止重试"
    );
    assert_eq!(
        counts.business_events.load(Ordering::Relaxed),
        0,
        "真实串口业务区不得发 tracing event"
    );
    assert_eq!(
        counts.business_spans.load(Ordering::Relaxed),
        0,
        "真实串口业务区不得创建 tracing span"
    );
}

#[test]
fn diag_retry_recovered_fields() {
    let (events, counts) = in_serial_thread(exercise_real_serial_path);
    assert_eq!(counts.business_events.load(Ordering::Relaxed), 0);
    let fields = |site| {
        events
            .iter()
            .filter(|event| event.site == site)
            .map(|event| (event.kind, event.a, event.b, event.seq_lo))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        fields(2),
        [(1, MOTOR, 1, 547), (2, 0, 2, 0)],
        "Goal Retry / Recovered 必须保留真实尝试次数与差值"
    );
    assert_eq!(
        fields(3),
        [
            (6, MOTOR, 63, 0), // Torque NACK 不冒充通信错误；设备状态另存。
            (5, MOTOR, 0x24, 0),
            (1, MOTOR, 1, 0),
            (2, MOTOR, 2, 0),
            (6, MOTOR, 0, 0), // 下一次 disable 的真实 Timeout。
            (0, 0, 0, 0),
        ]
    );
    assert_eq!(
        fields(7),
        [(0, 3, 0, 0)],
        "Safety 实际写失败必须保留 IoWrite 分类"
    );
    let stop_retries: Vec<_> = events
        .iter()
        .filter(|event| event.site == 20 && event.kind == 7)
        .map(|event| (event.a, event.b, event.seq_lo))
        .collect();
    assert_eq!(
        stop_retries,
        [(1, 3, 0)],
        "StopRetry 必须来自第一轮真实 Safety 写失败"
    );
}

#[test]
fn serial_subscriber_canary_detects_an_actual_business_event() {
    let ((), counts) = in_serial_thread(|| {
        tracing::warn!(target: "post_e_diag_business_negative", "反例：业务 tracing 必须被捕获");
        let span = tracing::info_span!("反例：业务 span 必须被捕获");
        let _entered = span.enter();
    });
    assert_eq!(counts.business_events.load(Ordering::Relaxed), 1);
    assert_eq!(counts.business_spans.load(Ordering::Relaxed), 1);
}
