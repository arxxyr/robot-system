//! 独立验收：脚本只模拟设备回答，由真实收敛机决定下一次调用。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use robot_core::adapter::{
    AdapterInfo, AdapterKind, ControlMode, JointCommandBuf, JointCommandView, JointLimits,
    JointStateBuf, ManipulatorAdapter, RtClass, RtSnapshot,
};
use robot_core::error::RobotError;
use robot_core::executor_ctrl::{
    AdapterErr, AdapterNormalReq, AdapterResp, ExecutorCtrlClient, NormalReqStatus, OutputPolicy,
    OutputSafetyProbe, OutputSafetyState, SerialEmergencyCtrl, TorqueEnableMask,
};
use robot_core::serial_services::{
    AbortReason, BusOpClass, CommErrorKind, LeasedPreemptProbe, NormalAbortTarget, PreemptProbe,
};
use robot_core::time::{DurationNs, MockClock, MonoRawNs, RobotClock};

use crate::diag::SerialDiagRing;
use crate::hard_rt_ctrl::{
    ActionRecord, CompletedEffect, FaultOrigin, HardRtCtrlClient, HardRtCtrlInner,
    PENDING_CANCELLED, decode_serial_pending, inhibit,
};
use crate::serial_convergence::{
    BootOutcome, ConvergeCompletion, ConvergeContext, ConvergePhase, SerialConvergence,
};
use crate::serial_io_thread::{SerialIoThreadMetrics, SerialTick, SerialTickContext};
use crate::serial_preempt::SerialPreemptProbe;
use crate::state_machine::FaultCode;

// 本模块仅在 cfg(test) 编入 S0 lib test；生产分配器不变。
#[cfg(feature = "rt_guard")]
#[global_allocator]
static LIB_TEST_ALLOCATOR: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Io {
    Safety,
    Connect,
    Disconnect,
    Confirm,
    Enable(bool),
    Mode(ControlMode),
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnableBoundary {
    Ordinary,
    RevokeBetweenMotors,
    RevokeAfterLastMotor,
    FailAfterFirstMotor,
}

#[derive(Clone, Copy)]
struct Reply {
    io: Io,
    result: Result<(), RobotError>,
    torque_on: u32,
    hygiene_error: Option<CommErrorKind>,
    elapsed_ms: u64,
    interrupt: Option<AbortReason>,
}

impl Reply {
    fn ok(io: Io) -> Self {
        Self {
            io,
            result: Ok(()),
            torque_on: 0,
            hygiene_error: None,
            elapsed_ms: 1,
            interrupt: None,
        }
    }

    fn error(io: Io, error: RobotError) -> Self {
        Self {
            result: Err(error),
            ..Self::ok(io)
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Observation {
    io: Io,
    class: BusOpClass,
    started_at: MonoRawNs,
    trusted: bool,
    safety: OutputSafetyState,
    inhibit: u8,
}

struct ScriptedAdapter {
    name: &'static str,
    script: VecDeque<Reply>,
    observed: Vec<Observation>,
    clock: Arc<MockClock>,
    ctrl: Arc<HardRtCtrlInner>,
    probe: Arc<SerialPreemptProbe>,
    limits: JointLimits,
    during_enable: Option<Box<dyn FnOnce() + Send>>,
    during_confirm: Option<Box<dyn FnOnce() + Send>>,
    enable_boundary: EnableBoundary,
    actual_torque: [bool; 2],
}

impl ScriptedAdapter {
    fn new(
        clock: Arc<MockClock>,
        ctrl: Arc<HardRtCtrlInner>,
        probe: Arc<SerialPreemptProbe>,
        replies: impl IntoIterator<Item = Reply>,
    ) -> Self {
        Self {
            name: "独立收敛验收设备",
            script: replies.into_iter().collect(),
            observed: Vec::new(),
            clock,
            ctrl,
            probe,
            limits: JointLimits::default(),
            during_enable: None,
            during_confirm: None,
            enable_boundary: EnableBoundary::Ordinary,
            actual_torque: [false; 2],
        }
    }

    fn invoke(&mut self, io: Io) -> Reply {
        self.observed.push(Observation {
            io,
            class: self.probe.operation_class(),
            started_at: self.clock.now(),
            trusted: self.probe.confirmation_trusted(),
            safety: self.ctrl.safety_state(),
            inhibit: self.ctrl.inhibit_bits(),
        });
        let reply = self.script.pop_front().expect("出现脚本以外的额外 IO");
        assert_eq!(io, reply.io, "实际设备调用顺序偏离独立脚本");
        if io == Io::Enable(true)
            && let Some(during) = self.during_enable.take()
        {
            during();
        }
        if let Some(reason) = reply.interrupt {
            let client = HardRtCtrlClient::new(self.ctrl.clone());
            match reason {
                AbortReason::Timeout => {
                    let active =
                        ActionRecord::unpack(self.ctrl.active_action.load(Ordering::Acquire));
                    assert!(active.valid, "实际 normal 调用必须已发布 active 身份");
                    client.abort_pending_normal(NormalAbortTarget::Submitted { seq: active.seq });
                }
                AbortReason::Emergency => {
                    client.estop_monotonic();
                }
                AbortReason::Revoked => panic!("Revoked 必须来自真实 A2 租约拒绝，不能脚本注入"),
            }
            assert_eq!(
                self.probe.should_abort(self.probe.operation_class()),
                Some(reason)
            );
        }
        if let Some(kind) = reply.hygiene_error {
            self.probe.report_comm_error(kind);
        }
        self.clock
            .advance(DurationNs::from_millis(reply.elapsed_ms));
        self.probe.checkpoint();
        reply
    }

    fn assert_drained(&self) {
        assert_eq!(self.script.len(), 0, "有期望的设备调用未执行");
    }
}

impl ManipulatorAdapter for ScriptedAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: self.name,
            kind: AdapterKind::Manipulator,
            rt_class: RtClass::SerialRT,
            vendor_sdk: false,
        }
    }

    fn connect(&mut self) -> Result<(), RobotError> {
        self.invoke(Io::Connect).result
    }

    fn disconnect(&mut self) -> Result<(), RobotError> {
        // 仅显式 normal Disconnect 脚本允许；收敛中额外断开会因无脚本而失败。
        self.invoke(Io::Disconnect).result
    }

    fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError> {
        // 错误回答也污染临时输出，验证真实循环不提交失败采样。
        out.positions[0] = 937.0;
        out.velocities[0] = 938.0;
        out.stamp_ns = 939;
        self.invoke(Io::Read).result
    }

    fn write_command(&mut self, _cmd: &JointCommandView) -> Result<(), RobotError> {
        self.invoke(Io::Write).result
    }

    fn set_mode(&mut self, mode: ControlMode) -> Result<(), RobotError> {
        self.invoke(Io::Mode(mode)).result
    }

    fn enable(&mut self, on: bool) -> Result<(), RobotError> {
        let reply = self.invoke(Io::Enable(on));
        if self.enable_boundary == EnableBoundary::Ordinary {
            return reply.result;
        }
        reply.result?;
        for motor in 0..2 {
            let lease = self
                .probe
                .begin_leased(BusOpClass::NormalEffectful)
                .map_err(RobotError::Aborted)?;
            self.actual_torque[motor] = on;
            self.probe.finish_leased(lease);
            match (self.enable_boundary, motor) {
                (EnableBoundary::RevokeBetweenMotors, 0)
                | (EnableBoundary::RevokeAfterLastMotor, 1) => {
                    let _ = self.ctrl.authority().disable();
                }
                (EnableBoundary::FailAfterFirstMotor, 0) => {
                    return Err(RobotError::HardwareFault("首电机已经上扭后设备失败"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn safety_stop(&mut self) -> Result<(), RobotError> {
        let result = self.invoke(Io::Safety).result;
        if result.is_ok() {
            self.actual_torque.fill(false);
        }
        result
    }

    fn limits(&self) -> &JointLimits {
        &self.limits
    }
}

impl OutputSafetyProbe for ScriptedAdapter {
    fn read_torque_enable_mask(&mut self, out: &mut [bool]) -> Result<(), RobotError> {
        let reply = self.invoke(Io::Confirm);
        if let Some(during) = self.during_confirm.take() {
            during();
        }
        for (index, enabled) in out.iter_mut().enumerate() {
            *enabled = match self.enable_boundary {
                EnableBoundary::Ordinary => reply.torque_on & (1_u32 << index) != 0,
                _ => self.actual_torque[index],
            };
        }
        reply.result
    }

    fn joint_count(&self) -> usize {
        2
    }
}

struct Fixture {
    clock: Arc<MockClock>,
    ctrl: Arc<HardRtCtrlInner>,
    probe: Arc<SerialPreemptProbe>,
    diag: SerialDiagRing,
    adapter: ScriptedAdapter,
    machine: SerialConvergence,
    torque: TorqueEnableMask,
}

impl Fixture {
    fn new(authorized: bool, replies: impl IntoIterator<Item = Reply>) -> Self {
        let clock = Arc::new(MockClock::default());
        let ctrl = Arc::new(HardRtCtrlInner::new_serial(
            crate::hard_rt_ctrl::test_authority(true),
        ));
        let probe = Arc::new(SerialPreemptProbe::new(clock.clone(), ctrl.clone()));
        Self {
            adapter: ScriptedAdapter::new(clock.clone(), ctrl.clone(), probe.clone(), replies),
            machine: SerialConvergence::new(authorized, 2, clock.now()),
            clock,
            ctrl,
            probe,
            diag: SerialDiagRing::new(),
            torque: [false; 32],
        }
    }

    fn tick(&mut self) {
        self.machine.step(
            &mut self.adapter,
            &self.ctrl,
            &self.probe,
            &self.diag,
            self.clock.as_ref(),
            &mut self.torque,
        );
        self.clock.advance(DurationNs::from_millis(5));
    }

    /// 信任必须来自真实 Preflight 成功，不能在正常可达场景中手写 `rx_trust`。
    fn healthy() -> Self {
        let mut fixture = Self::new(true, [Io::Safety, Io::Connect, Io::Confirm].map(Reply::ok));
        for expected in [
            ConvergePhase::Connect,
            ConvergePhase::Confirm,
            ConvergePhase::Idle,
        ] {
            fixture.tick();
            assert_eq!(fixture.machine.phase(), expected);
        }
        assert_eq!(fixture.machine.boot_outcome(), BootOutcome::Passed);
        assert!(fixture.ctrl.rx_trust.load(Ordering::Acquire));
        assert_eq!(
            fixture.ctrl.safety_state(),
            OutputSafetyState::TorqueOffConfirmed
        );
        assert_eq!(fixture.ctrl.inhibit_bits(), 0);
        fixture.adapter.assert_drained();
        assert_eq!(
            fixture
                .adapter
                .observed
                .iter()
                .map(|item| (item.io, item.class))
                .collect::<Vec<_>>(),
            [
                (Io::Safety, BusOpClass::Safety),
                (Io::Connect, BusOpClass::Connect),
                (Io::Confirm, BusOpClass::Confirm)
            ]
        );
        assert!(fixture.adapter.observed[2].trusted);
        for item in &fixture.adapter.observed {
            assert_eq!(item.safety, OutputSafetyState::StopCommanded);
            assert_ne!(item.inhibit & inhibit::BOOT, 0);
            assert_ne!(item.inhibit & inhibit::CONVERGING, 0);
        }
        fixture.adapter.observed.clear();
        fixture
    }

    fn fault(&mut self, code: FaultCode, kind: CommErrorKind) {
        let origin = match (code, kind) {
            (FaultCode::CommFault, CommErrorKind::PendingDeadline) => FaultOrigin::PendingTimeout,
            (FaultCode::CommFault, _) => FaultOrigin::Periodic,
            _ => FaultOrigin::NormalAction,
        };
        self.machine.enter_fault(
            code,
            kind.to_u8(),
            origin,
            &self.ctrl,
            &self.diag,
            self.clock.now(),
        );
    }

    fn estop(&mut self) {
        HardRtCtrlClient::new(self.ctrl.clone()).estop_monotonic();
        self.machine
            .claim_estop(&self.ctrl, &self.diag, self.clock.now());
    }

    fn await_trusted_fault() -> Self {
        let mut fixture = Self::healthy();
        fixture.fault(FaultCode::CommFault, CommErrorKind::PendingDeadline);
        fixture
            .adapter
            .script
            .extend([Io::Safety, Io::Confirm].map(Reply::ok));
        fixture.tick();
        fixture.tick();
        assert_eq!(fixture.machine.phase(), ConvergePhase::AwaitAck);
        assert_eq!(
            fixture.machine.round().unwrap().completed_safety,
            Some(ConvergeCompletion::AllOffTrusted)
        );
        fixture.adapter.assert_drained();
        fixture
    }
}

#[test]
fn preflight_is_safety_connect_confirm_and_trust_is_published_only_at_exit() {
    let fixture = Fixture::healthy();
    assert!(fixture.ctrl.session_clean.load(Ordering::Acquire));
    assert!(fixture.machine.normal_io_allowed(&fixture.ctrl));
}

#[test]
fn same_severity_error_still_revokes_trust_without_new_generation_or_round() {
    let mut f = Fixture::healthy();
    f.fault(FaultCode::CommFault, CommErrorKind::PendingDeadline);
    f.adapter
        .script
        .extend([Io::Safety, Io::Confirm].map(Reply::ok));
    f.tick();
    let generation = f.ctrl.mailbox_snapshot().unwrap().generation;
    assert_eq!(f.machine.phase(), ConvergePhase::Confirm);
    f.fault(FaultCode::CommFault, CommErrorKind::Timeout);
    assert_eq!(f.machine.phase(), ConvergePhase::Confirm);
    assert_eq!(f.ctrl.mailbox_snapshot().unwrap().generation, generation);
    assert!(!f.ctrl.session_clean.load(Ordering::Acquire));
    assert!(!f.ctrl.rx_trust.load(Ordering::Acquire));
    f.tick();
    assert_eq!(f.ctrl.safety_state(), OutputSafetyState::AdapterUncertain);
    assert_eq!(f.machine.phase(), ConvergePhase::AwaitAck);
    f.adapter.assert_drained();
}

#[test]
fn hygiene_failure_before_successful_probe_cannot_create_trusted_boot() {
    let mut f = Fixture::new(
        true,
        [
            Reply::ok(Io::Safety),
            Reply::ok(Io::Connect),
            Reply {
                hygiene_error: Some(CommErrorKind::DiscardFailed),
                ..Reply::ok(Io::Confirm)
            },
        ],
    );
    f.tick();
    f.tick();
    f.tick();
    assert!(!f.ctrl.session_clean.load(Ordering::Acquire));
    assert!(!f.ctrl.rx_trust.load(Ordering::Acquire));
    assert_eq!(f.ctrl.safety_state(), OutputSafetyState::AdapterUncertain);
    assert_eq!(f.machine.phase(), ConvergePhase::AwaitAck);
    assert_eq!(f.machine.round().unwrap().attempt, 1);
    assert_eq!(
        f.ctrl.mailbox_snapshot().unwrap().code,
        FaultCode::CommFault
    );
    f.adapter.assert_drained();
}

#[test]
fn trusted_await_ack_reopens_physics_without_losing_generation_or_accepting_early_ack() {
    for ack_at in 0..3 {
        let mut f = Fixture::await_trusted_fault();
        let old_round = *f.machine.round().unwrap();
        let generation = f.ctrl.mailbox_snapshot().unwrap().generation;
        let old_reasons = f.ctrl.inhibit_bits();
        if ack_at == 0 {
            f.ctrl.ack_fault(generation);
        }
        HardRtCtrlClient::new(f.ctrl.clone()).estop_monotonic();
        assert_eq!(f.ctrl.safety_state(), OutputSafetyState::StopCommanded);
        f.machine.claim_estop(&f.ctrl, &f.diag, f.clock.now());
        assert_eq!(f.machine.phase(), ConvergePhase::StopIssue);
        let reopened = f.machine.round().unwrap();
        assert_eq!(
            (reopened.ctx, reopened.generation),
            (old_round.ctx, old_round.generation)
        );
        assert_eq!(reopened.completed_safety, None);
        assert_eq!(f.ctrl.inhibit_bits() & old_reasons, old_reasons);
        f.adapter
            .script
            .extend([Io::Safety, Io::Confirm].map(Reply::ok));
        if ack_at == 1 {
            f.ctrl.ack_fault(generation);
        }
        f.tick();
        assert_eq!(f.machine.phase(), ConvergePhase::Confirm);
        assert_ne!(f.ctrl.inhibit_bits() & inhibit::FAULT_PENDING, 0);
        if ack_at == 2 {
            f.ctrl.ack_fault(generation);
        }
        f.tick();
        assert_eq!(f.machine.phase(), ConvergePhase::AwaitAck);
        assert_eq!(f.ctrl.mailbox_snapshot().unwrap().generation, generation);
        f.tick();
        assert_eq!(f.machine.phase(), ConvergePhase::Idle);
        assert_eq!(f.ctrl.inhibit_bits() & inhibit::FAULT_PENDING, 0);
        assert_ne!(f.ctrl.inhibit_bits() & inhibit::COMM_FAULT, 0);
        f.adapter.assert_drained();
    }
}

#[test]
fn untrusted_await_ack_escalates_without_any_extra_io_and_ignores_old_ack() {
    let mut f = Fixture::new(false, [Io::Safety, Io::Connect, Io::Confirm].map(Reply::ok));
    f.tick();
    f.tick();
    f.tick();
    assert_eq!(f.machine.phase(), ConvergePhase::AwaitAck);
    let old = f.ctrl.mailbox_snapshot().unwrap();
    let io_count = f.adapter.observed.len();
    f.ctrl.ack_fault(old.generation);
    f.estop();
    assert!(matches!(
        f.machine.phase(),
        ConvergePhase::SafetyStopFailedAwaitAck { .. }
    ));
    let new = f.ctrl.mailbox_snapshot().unwrap();
    assert_eq!(new.generation, old.generation + 1);
    assert_eq!(new.code, FaultCode::SafetyStopFailed);
    for _ in 0..3 {
        f.tick();
    }
    assert_eq!(f.adapter.observed.len(), io_count);
    assert_ne!(f.ctrl.inhibit_bits() & inhibit::FAULT_PENDING, 0);
    f.ctrl.ack_fault(new.generation);
    f.tick();
    assert_eq!(f.machine.phase(), ConvergePhase::SafetyStopFailed);
    f.adapter.assert_drained();
}

#[test]
fn one_motor_seen_on_then_errors_exhausted_must_remain_stop_failed() {
    let mut f = Fixture::healthy();
    f.fault(FaultCode::CommFault, CommErrorKind::PendingDeadline);
    f.adapter.script.push_back(Reply::ok(Io::Safety));
    f.adapter.script.push_back(Reply {
        torque_on: 2,
        ..Reply::ok(Io::Confirm)
    });
    f.adapter.script.extend(std::iter::repeat_n(
        Reply::error(Io::Confirm, RobotError::Comm(CommErrorKind::Timeout)),
        4,
    ));
    f.tick();
    for attempt in 1..=4 {
        f.tick();
        assert_eq!(f.machine.phase(), ConvergePhase::Confirm);
        assert_eq!(f.machine.round().unwrap().attempt, attempt);
    }
    f.tick();
    assert_eq!(f.ctrl.safety_state(), OutputSafetyState::StopFailed);
    assert_eq!(
        f.ctrl.mailbox_snapshot().unwrap().code,
        FaultCode::SafetyStopFailed
    );
    assert!(matches!(
        f.machine.phase(),
        ConvergePhase::SafetyStopFailedAwaitAck { .. }
    ));
    f.adapter.assert_drained();
}

#[test]
fn three_failed_safety_writes_stop_without_connect_confirm_or_fourth_write() {
    let mut f = Fixture::new(
        true,
        std::iter::repeat_n(
            Reply::error(Io::Safety, RobotError::Comm(CommErrorKind::IoWrite)),
            3,
        ),
    );
    for expected_count in 1..=3 {
        f.tick();
        assert_eq!(f.adapter.observed.len(), expected_count);
    }
    assert_eq!(f.ctrl.safety_state(), OutputSafetyState::StopFailed);
    assert!(matches!(
        f.machine.phase(),
        ConvergePhase::SafetyStopFailedAwaitAck { .. }
    ));
    let generation = f.ctrl.mailbox_snapshot().unwrap().generation;
    f.ctrl.ack_fault(generation);
    f.tick();
    assert_eq!(f.machine.phase(), ConvergePhase::SafetyStopFailed);
    for _ in 0..4 {
        f.estop();
        f.tick();
    }
    assert_eq!(f.adapter.observed.len(), 3);
    for pair in f.adapter.observed.windows(2) {
        assert!(pair[1].started_at.0 - pair[0].started_at.0 >= 5_000_000);
    }
    f.adapter.assert_drained();
}

#[test]
fn emergency_confirm_error_adds_generation_without_replacing_context_or_reissuing_stop() {
    let mut f = Fixture::healthy();
    f.estop();
    f.adapter.script.extend([
        Reply::ok(Io::Safety),
        Reply::error(Io::Confirm, RobotError::Comm(CommErrorKind::Timeout)),
        Reply::ok(Io::Confirm),
    ]);
    f.tick();
    f.tick();
    let round = f.machine.round().unwrap();
    assert_eq!(round.ctx, ConvergeContext::Emergency);
    assert!(round.estop_bound);
    assert_eq!(round.attempt, 1);
    assert!(round.generation.is_some());
    assert_eq!(f.machine.phase(), ConvergePhase::Confirm);
    assert_eq!(
        f.ctrl.mailbox_snapshot().unwrap().code,
        FaultCode::CommFault
    );
    f.tick();
    assert_eq!(
        f.ctrl.mailbox_snapshot().unwrap().code,
        FaultCode::SafetyStopFailed
    );
    assert_eq!(f.ctrl.safety_state(), OutputSafetyState::AdapterUncertain);
    assert_eq!(
        f.adapter
            .observed
            .iter()
            .filter(|item| item.io == Io::Safety)
            .count(),
        1
    );
    f.adapter.assert_drained();
}

#[test]
fn pure_fault_never_changes_any_policy_and_bound_stop_issue_only_tightens_outputs() {
    let policies = [
        OutputPolicy::Disabled,
        OutputPolicy::StopTorque,
        OutputPolicy::ActiveForward,
        OutputPolicy::HoldCurrent,
        OutputPolicy::ZeroCommand,
        OutputPolicy::ReadyHold,
    ];
    for bound in [false, true] {
        for policy in policies {
            let mut f = Fixture::healthy();
            f.ctrl.policy.store(policy.to_u8(), Ordering::Release);
            f.fault(FaultCode::CommFault, CommErrorKind::PendingDeadline);
            if bound {
                f.estop();
            }
            assert_eq!(f.ctrl.policy.load(Ordering::Acquire), policy.to_u8());
            f.adapter
                .script
                .extend([Io::Safety, Io::Confirm].map(Reply::ok));
            f.tick();
            let expected = match (bound, policy) {
                (
                    true,
                    OutputPolicy::ActiveForward
                    | OutputPolicy::HoldCurrent
                    | OutputPolicy::ZeroCommand
                    | OutputPolicy::ReadyHold,
                ) => OutputPolicy::StopTorque,
                _ => policy,
            };
            assert_eq!(f.ctrl.policy.load(Ordering::Acquire), expected.to_u8());
            // 控制面的禁用发生在物理发停之后，后续 Confirm/AwaitAck 不得覆写。
            f.ctrl
                .policy
                .store(OutputPolicy::Disabled.to_u8(), Ordering::Release);
            f.tick();
            f.tick();
            assert_eq!(
                f.ctrl.policy.load(Ordering::Acquire),
                OutputPolicy::Disabled.to_u8()
            );
            f.adapter.assert_drained();
        }
    }
}

#[test]
fn startup_deadline_records_uncertainty_after_at_least_one_safety() {
    for deadline_before_first_io in [false, true] {
        let mut f = Fixture::new(true, [Reply::ok(Io::Safety)]);
        if !deadline_before_first_io {
            f.tick();
        }
        f.ctrl.boot_deadline_expired.store(true, Ordering::Release);
        // 允许处理 deadline 的无 IO tick，但不允许 Connect/Confirm 或第二次 Safety。
        for _ in 0..3 {
            if f.machine.phase() == ConvergePhase::AwaitAck {
                break;
            }
            f.tick();
        }
        assert_eq!(f.machine.phase(), ConvergePhase::AwaitAck);
        assert_eq!(
            f.machine.round().unwrap().completed_safety,
            Some(ConvergeCompletion::AdapterUncertain {
                kind: CommErrorKind::Unknown
            })
        );
        let old_exit = f.machine.first_boot_exit();
        assert!(old_exit.is_some());
        assert!(f.ctrl.boot_exit_reached.load(Ordering::Acquire));
        assert_eq!(
            f.ctrl.boot_first_exit_ns.load(Ordering::Relaxed),
            old_exit.unwrap().0
        );
        f.estop();
        assert!(matches!(
            f.machine.phase(),
            ConvergePhase::SafetyStopFailedAwaitAck { .. }
        ));
        assert_eq!(f.machine.first_boot_exit(), old_exit);
        assert_eq!(
            f.ctrl.boot_first_exit_ns.load(Ordering::Relaxed),
            old_exit.unwrap().0
        );
        assert_eq!(f.adapter.observed.len(), 1);
        f.adapter.assert_drained();
    }
}

#[test]
fn emergency_with_non_rx_fault_keeps_identity_and_must_wait_for_ack() {
    for code in [FaultCode::AdapterErr, FaultCode::CommFault] {
        let mut f = Fixture::healthy();
        f.estop();
        f.adapter
            .script
            .extend([Io::Safety, Io::Confirm].map(Reply::ok));
        f.tick();
        // 本地 deadline 属于通信分类但不是 RX 失步，设备 fault 也不撤销 RX 信任。
        f.fault(code, CommErrorKind::PendingDeadline);
        assert!(f.ctrl.rx_trust.load(Ordering::Acquire));
        let generation = f.ctrl.mailbox_snapshot().unwrap().generation;
        let round = f.machine.round().unwrap();
        assert_eq!(round.ctx, ConvergeContext::Emergency);
        assert!(round.estop_bound);
        assert_eq!(f.machine.phase(), ConvergePhase::Confirm);
        f.tick();
        assert_eq!(f.ctrl.safety_state(), OutputSafetyState::TorqueOffConfirmed);
        assert_eq!(f.machine.phase(), ConvergePhase::AwaitAck);
        f.tick();
        assert_eq!(f.machine.phase(), ConvergePhase::AwaitAck);
        f.ctrl.ack_fault(generation);
        f.tick();
        assert_eq!(f.machine.phase(), ConvergePhase::Idle);
        f.adapter.assert_drained();
    }
}

#[test]
fn hygiene_and_probe_errors_count_one_attempt_and_never_report_trusted_confirmation() {
    let mut f = Fixture::healthy();
    f.fault(FaultCode::CommFault, CommErrorKind::PendingDeadline);
    f.adapter.script.push_back(Reply::ok(Io::Safety));
    f.adapter.script.extend(std::iter::repeat_n(
        Reply {
            hygiene_error: Some(CommErrorKind::DiscardFailed),
            ..Reply::error(Io::Confirm, RobotError::Comm(CommErrorKind::Timeout))
        },
        5,
    ));
    f.tick();
    for attempt in 1..=4 {
        f.tick();
        assert_eq!(f.machine.phase(), ConvergePhase::Confirm);
        assert_eq!(f.machine.round().unwrap().attempt, attempt);
    }
    f.tick();
    assert_eq!(f.ctrl.safety_state(), OutputSafetyState::AdapterUncertain);
    assert_eq!(f.machine.phase(), ConvergePhase::AwaitAck);
    assert_eq!(f.machine.round().unwrap().attempt, 5);
    assert!(!f.ctrl.rx_trust.load(Ordering::Acquire));
    f.adapter.assert_drained();
}

#[test]
fn old_ack_never_clears_upgraded_fault_and_only_first_comm_upgrade_restarts_stop() {
    let mut f = Fixture::healthy();
    f.fault(FaultCode::AdapterErr, CommErrorKind::PendingDeadline);
    let first = f.ctrl.mailbox_snapshot().unwrap().generation;
    f.adapter.script.push_back(Reply::ok(Io::Safety));
    f.tick();
    f.fault(FaultCode::CommFault, CommErrorKind::PendingDeadline);
    let second = f.ctrl.mailbox_snapshot().unwrap().generation;
    assert_eq!(second, first + 1);
    assert_eq!(f.machine.phase(), ConvergePhase::StopIssue);
    f.ctrl.ack_fault(first);
    f.adapter.script.push_back(Reply::ok(Io::Safety));
    f.tick();
    f.fault(FaultCode::CommFault, CommErrorKind::Timeout);
    f.fault(FaultCode::AdapterErr, CommErrorKind::PendingDeadline);
    assert_eq!(f.ctrl.mailbox_snapshot().unwrap().generation, second);
    assert_eq!(f.machine.phase(), ConvergePhase::Confirm);
    f.adapter.script.extend(std::iter::repeat_n(
        Reply {
            torque_on: 2,
            ..Reply::ok(Io::Confirm)
        },
        5,
    ));
    for _ in 0..5 {
        f.tick();
    }
    let third = f.ctrl.mailbox_snapshot().unwrap().generation;
    assert_eq!(third, second + 1);
    assert_eq!(
        f.ctrl.mailbox_snapshot().unwrap().code,
        FaultCode::SafetyStopFailed
    );
    for old in [first, second] {
        f.ctrl.ack_fault(old);
        f.tick();
        assert_ne!(f.ctrl.inhibit_bits() & inhibit::FAULT_PENDING, 0);
    }
    assert_eq!(
        f.adapter
            .observed
            .iter()
            .filter(|item| item.io == Io::Safety)
            .count(),
        2
    );
    f.ctrl.ack_fault(third);
    f.tick();
    assert_eq!(f.machine.phase(), ConvergePhase::SafetyStopFailed);
    f.adapter.assert_drained();
}

#[test]
fn a_new_emergency_round_publishes_its_first_fault_even_after_an_older_fault_was_acked() {
    let mut f = Fixture::await_trusted_fault();
    let previous = f.ctrl.mailbox_snapshot().unwrap().generation;
    f.ctrl.ack_fault(previous);
    f.tick();
    assert_eq!(f.machine.phase(), ConvergePhase::Idle);
    f.estop();
    assert_eq!(f.machine.round().unwrap().generation, None);
    f.adapter.script.extend([
        Reply::ok(Io::Safety),
        Reply::error(Io::Confirm, RobotError::Comm(CommErrorKind::Timeout)),
    ]);
    f.tick();
    f.tick();
    let round = f.machine.round().unwrap();
    assert_eq!(round.ctx, ConvergeContext::Emergency);
    assert!(round.estop_bound);
    assert_eq!(round.attempt, 1);
    assert_eq!(round.generation, Some(previous + 1));
    assert_eq!(f.ctrl.mailbox_snapshot().unwrap().generation, previous + 1);
    assert_ne!(f.ctrl.inhibit_bits() & inhibit::FAULT_PENDING, 0);
    f.adapter.assert_drained();
}

#[test]
fn stop_retry_has_no_policy_write_permission_even_for_a_bound_round() {
    let mut f = Fixture::healthy();
    f.estop();
    f.adapter.script.extend([
        Reply::error(Io::Safety, RobotError::Comm(CommErrorKind::IoWrite)),
        Reply::ok(Io::Safety),
    ]);
    f.tick();
    assert_eq!(f.machine.phase(), ConvergePhase::StopRetry);
    // 人工构造观察哨兵，仅检验 phase 写权限，不声称控制面会在急停中恢复输出。
    f.ctrl
        .policy
        .store(OutputPolicy::ActiveForward.to_u8(), Ordering::Release);
    f.tick();
    assert_eq!(
        f.ctrl.policy.load(Ordering::Acquire),
        OutputPolicy::ActiveForward.to_u8()
    );
    f.adapter.assert_drained();
}

/// 主循环验收直接调用生产单 tick；不在夹具中模拟 retire/normal/periodic 调度。
struct TickFixture {
    driver: SerialTick,
    adapter: ScriptedAdapter,
    clock: Arc<MockClock>,
    ctrl: Arc<HardRtCtrlInner>,
    probe: Arc<SerialPreemptProbe>,
    diag: SerialDiagRing,
    metrics: SerialIoThreadMetrics,
    command: JointCommandBuf,
    pending: crate::ipc_pending::IpcPending,
    failure: Option<crate::ipc_pending::EnableFailureFact>,
}

impl TickFixture {
    fn healthy() -> Self {
        let clock = Arc::new(MockClock::default());
        let ctrl = Arc::new(HardRtCtrlInner::new_serial(
            crate::hard_rt_ctrl::test_authority(true),
        ));
        let probe = Arc::new(SerialPreemptProbe::new(clock.clone(), ctrl.clone()));
        let mut f = Self {
            driver: SerialTick::new(true, 2, clock.now()),
            adapter: ScriptedAdapter::new(
                clock.clone(),
                ctrl.clone(),
                probe.clone(),
                [Io::Safety, Io::Connect, Io::Confirm].map(Reply::ok),
            ),
            clock,
            ctrl,
            probe,
            diag: SerialDiagRing::new(),
            metrics: SerialIoThreadMetrics::default(),
            pending: crate::ipc_pending::IpcPending::Idle,
            failure: None,
            command: JointCommandBuf {
                joint_count: 2,
                ..JointCommandBuf::default()
            },
        };
        for _ in 0..3 {
            let _ = f.tick();
        }
        assert_eq!(f.driver.convergence().phase(), ConvergePhase::Idle);
        assert_eq!(f.driver.convergence().boot_outcome(), BootOutcome::Passed);
        assert!(f.ctrl.rx_trust.load(Ordering::Acquire));
        f.adapter.assert_drained();
        f.adapter.observed.clear();
        f
    }

    fn tick(&mut self) -> RtSnapshot {
        let result = self.driver.tick(
            &mut self.adapter,
            &self.command.as_view(),
            &SerialTickContext {
                ctrl: &self.ctrl,
                probe: &self.probe,
                diag: &self.diag,
                clock: self.clock.as_ref(),
                metrics: &self.metrics,
            },
            MonoRawNs(self.clock.now().0 + 5_000_000),
        );
        let pending = std::mem::replace(&mut self.pending, crate::ipc_pending::IpcPending::Idle);
        let client = HardRtCtrlClient::new(self.ctrl.clone());
        self.failure = None;
        self.pending =
            match crate::ipc_pending::advance_owned(pending, &client, self.clock.as_ref()) {
                Ok(step) => {
                    assert!(step.replies.is_empty());
                    self.failure = step.failure;
                    step.pending
                }
                Err(rejected) => rejected.pending,
            };
        self.clock.advance(DurationNs::from_millis(5));
        result
    }

    fn submit(&mut self, request: AdapterNormalReq) -> u64 {
        let client = HardRtCtrlClient::new(self.ctrl.clone());
        if request == AdapterNormalReq::Enable(true) {
            let original =
                std::mem::replace(&mut self.pending, crate::ipc_pending::IpcPending::Idle);
            self.pending = match crate::ipc_pending::start_enable_owned(
                original,
                None,
                &client,
                self.clock.now(),
            ) {
                crate::ipc_pending::EnableStartOutcome::Advanced(step) => step.pending,
                other => panic!("真实 Enable 预约/公告失败：{other:?}"),
            };
            return match &self.pending {
                crate::ipc_pending::IpcPending::Enable(enable) => enable.seq().unwrap(),
                other => panic!("{other:?}"),
            };
        }
        if request == AdapterNormalReq::Enable(false) {
            let _ = self.ctrl.authority().disable();
        }
        match client.request_normal(request) {
            NormalReqStatus::Submitted { seq } => seq,
            other => panic!("健康夹具提交失败：{other:?}"),
        }
    }

    fn timeout(&self, target: NormalAbortTarget) {
        HardRtCtrlClient::new(self.ctrl.clone()).abort_pending_normal(target);
    }

    fn run_to_await_ack(&mut self) {
        for _ in 0..8 {
            if matches!(
                self.driver.convergence().phase(),
                ConvergePhase::AwaitAck | ConvergePhase::SafetyStopFailedAwaitAck { .. }
            ) {
                self.adapter.assert_drained();
                return;
            }
            let _ = self.tick();
        }
        panic!("预排的有界脚本执行后仍未到 ACK 等待相位");
    }
}

/// IPC 独立验收复用真实 preflight 和一次周期采样产生的健康事实。
pub(crate) fn healthy_ipc_control() -> (Arc<HardRtCtrlInner>, Arc<MockClock>) {
    let mut fixture = TickFixture::healthy();
    fixture.adapter.script.push_back(Reply::ok(Io::Read));
    let _ = fixture.tick();
    fixture.adapter.assert_drained();
    fixture.clock.advance(DurationNs::from_millis(500));
    assert_eq!(
        fixture.ctrl.safety_state(),
        OutputSafetyState::TorqueOffConfirmed
    );
    assert_eq!(fixture.ctrl.inhibit_bits(), 0);
    (fixture.ctrl, fixture.clock)
}

/// 让 IPC 在真实 adapter Enable 调用尚未返回时运行；完成由生产 `SerialTick` 发布。
pub(crate) fn complete_enable_while<T: Send + 'static>(
    periodic_write: bool,
    prepare: impl FnOnce(Arc<HardRtCtrlInner>, Arc<MockClock>) -> T,
    during: impl FnOnce(&mut T) + Send + 'static,
) -> T {
    let mut fixture = TickFixture::healthy();
    fixture.adapter.script.push_back(Reply::ok(Io::Read));
    let _ = fixture.tick();
    fixture.clock.advance(DurationNs::from_millis(500));
    let state = Arc::new(std::sync::Mutex::new(prepare(
        fixture.ctrl.clone(),
        fixture.clock.clone(),
    )));
    let running = state.clone();
    fixture.adapter.during_enable = Some(Box::new(move || during(&mut running.lock().unwrap())));
    fixture.adapter.script.push_back(Reply {
        elapsed_ms: 0,
        ..Reply::ok(Io::Enable(true))
    });
    if periodic_write {
        fixture.adapter.script.push_back(Reply {
            elapsed_ms: 0,
            ..Reply::ok(Io::Write)
        });
    }
    fixture.adapter.script.push_back(Reply {
        elapsed_ms: 0,
        ..Reply::ok(Io::Read)
    });
    let pending = decode_serial_pending(fixture.ctrl.pending_normal.load(Ordering::Acquire))
        .unwrap()
        .expect("IPC 必须已经真正提交 enable");
    assert_eq!(
        pending.action.action,
        robot_core::executor_ctrl::normal_codes::ENABLE
    );
    assert_eq!(pending.action.arg, 1);
    let _ = fixture.driver.tick(
        &mut fixture.adapter,
        &fixture.command.as_view(),
        &SerialTickContext {
            ctrl: &fixture.ctrl,
            probe: &fixture.probe,
            diag: &fixture.diag,
            clock: fixture.clock.as_ref(),
            metrics: &fixture.metrics,
        },
        MonoRawNs(fixture.clock.now().0 + 5_000_000),
    );
    fixture.adapter.assert_drained();
    assert_eq!(
        fixture
            .adapter
            .observed
            .iter()
            .filter(|event| event.io == Io::Enable(true))
            .count(),
        1
    );
    assert_eq!(
        HardRtCtrlClient::new(fixture.ctrl.clone()).poll_last_normal_resp(pending.action.seq),
        Some(AdapterResp::Ok)
    );
    assert_eq!(fixture.ctrl.active_action.load(Ordering::Acquire), 0);
    Arc::into_inner(state)
        .expect("adapter 回调已归还测试状态")
        .into_inner()
        .unwrap()
}

/// 在真实设备错误完成发布点交给 IPC 反例运行，夹具不复制错误分流。
pub(crate) fn normal_failure_publication(
    error: RobotError,
    observe: impl FnOnce(Arc<HardRtCtrlInner>, Arc<MockClock>),
) {
    let mut f = TickFixture::healthy();
    f.adapter.script.push_back(Reply::ok(Io::Read));
    let _ = f.tick();
    f.clock.advance(DurationNs::from_millis(500));
    let seq = f.submit(AdapterNormalReq::SetMode(ControlMode::Position));
    f.adapter
        .script
        .push_back(Reply::error(Io::Mode(ControlMode::Position), error));
    let ctrl = f.ctrl.clone();
    let clock = f.clock.clone();
    let mut observed = false;
    let _ = f.driver.tick_after_normal_completion(
        &mut f.adapter,
        &f.command.as_view(),
        &SerialTickContext {
            ctrl: &f.ctrl,
            probe: &f.probe,
            diag: &f.diag,
            clock: f.clock.as_ref(),
            metrics: &f.metrics,
        },
        MonoRawNs(f.clock.now().0 + 5_000_000),
        || {
            observed = true;
            assert!(
                HardRtCtrlClient::new(ctrl.clone())
                    .poll_last_normal_resp(seq)
                    .is_some()
            );
            observe(ctrl, clock);
        },
    );
    assert!(observed, "必须到达实际normal完成发布点");
    f.adapter.assert_drained();
}

/// 故障登记已经发布但动作完成尚未发布：让独立 IPC 验收注入迟到 timeout。
pub(crate) fn normal_failure_before_publication(
    error: RobotError,
    observe: impl FnOnce(Arc<HardRtCtrlInner>, Arc<MockClock>, u64),
) {
    let mut f = TickFixture::healthy();
    f.adapter.script.push_back(Reply::ok(Io::Read));
    let _ = f.tick();
    f.clock.advance(DurationNs::from_millis(500));
    let seq = f.submit(AdapterNormalReq::SetMode(ControlMode::Position));
    f.adapter
        .script
        .push_back(Reply::error(Io::Mode(ControlMode::Position), error));
    let ctrl = f.ctrl.clone();
    let clock = f.clock.clone();
    let mut observed = false;
    let _ = f.driver.tick_before_normal_completion(
        &mut f.adapter,
        &f.command.as_view(),
        &SerialTickContext {
            ctrl: &f.ctrl,
            probe: &f.probe,
            diag: &f.diag,
            clock: f.clock.as_ref(),
            metrics: &f.metrics,
        },
        MonoRawNs(f.clock.now().0 + 5_000_000),
        || {
            observed = true;
            observe(ctrl, clock, seq);
        },
    );
    assert!(observed);
    assert_eq!(f.ctrl.active_action.load(Ordering::Acquire), 0);
    assert_eq!(
        HardRtCtrlClient::new(f.ctrl.clone()).poll_last_normal_resp(seq),
        Some(AdapterResp::Err(AdapterErr::HardwareFault))
    );
    assert_eq!(f.ctrl.abort_request.load(Ordering::Acquire), 0);
    assert!(f.ctrl.rx_trust.load(Ordering::Acquire));
    assert_eq!(
        f.ctrl.mailbox_snapshot().unwrap().detail,
        CommErrorKind::PendingDeadline.to_u8()
    );
    f.adapter.assert_drained();
}

#[test]
fn unsubmitted_timeout_drives_real_tick_without_original_action_or_new_sample() {
    let mut f = TickFixture::healthy();
    let old = *f.driver.state();
    f.ctrl
        .policy
        .store(OutputPolicy::ActiveForward.to_u8(), Ordering::Release);
    f.timeout(NormalAbortTarget::Unsubmitted);
    f.adapter
        .script
        .extend([Io::Safety, Io::Confirm].map(Reply::ok));
    f.run_to_await_ack();
    let message = f.ctrl.mailbox_snapshot().unwrap();
    assert_eq!(message.code, FaultCode::CommFault);
    assert_eq!(message.detail, CommErrorKind::PendingDeadline.to_u8());
    assert_eq!(message.origin, FaultOrigin::PendingTimeout);
    assert!(f.ctrl.rx_trust.load(Ordering::Acquire));
    assert!(f.ctrl.session_clean.load(Ordering::Acquire));
    assert!(!f.ctrl.safety_latched());
    assert_eq!(f.driver.state().stamp_ns, old.stamp_ns);
    assert_eq!(
        f.driver.state().positions.map(f32::to_bits),
        old.positions.map(f32::to_bits)
    );
    assert_eq!(f.ctrl.abort_request.load(Ordering::Acquire), 0);
}

#[test]
fn pending_submitted_timeout_cancels_before_normal_action_and_preserves_exact_sequence() {
    let mut f = TickFixture::healthy();
    let seq = f.submit(AdapterNormalReq::Enable(true));
    f.timeout(NormalAbortTarget::Submitted { seq });
    f.adapter
        .script
        .extend([Io::Safety, Io::Confirm].map(Reply::ok));
    f.run_to_await_ack();
    assert_eq!(f.ctrl.pending_normal.load(Ordering::Acquire), 0);
    assert_eq!(
        HardRtCtrlClient::new(f.ctrl.clone()).poll_last_normal_resp(seq),
        Some(AdapterResp::Err(AdapterErr::AbortedByTimeout))
    );
    assert!(f.ctrl.rx_trust.load(Ordering::Acquire));
    assert!(!f.ctrl.safety_latched());
    assert_ne!(f.ctrl.inhibit_bits() & inhibit::FAULT_PENDING, 0);
}

#[test]
fn completed_disable_skips_repeat_stop_but_requires_current_confirm_and_original_ack() {
    for next_read_fails in [false, true] {
        let mut f = TickFixture::healthy();
        let seq = f.submit(AdapterNormalReq::Enable(false));
        f.adapter.script.push_back(Reply::ok(Io::Enable(false)));
        f.adapter.script.push_back(if next_read_fails {
            Reply::error(Io::Read, RobotError::Comm(CommErrorKind::Timeout))
        } else {
            Reply::ok(Io::Read)
        });
        let _ = f.tick();
        f.adapter.assert_drained();
        assert_eq!(
            HardRtCtrlClient::new(f.ctrl.clone()).poll_last_normal_resp(seq),
            Some(AdapterResp::Ok)
        );
        assert_eq!(f.ctrl.rx_trust.load(Ordering::Acquire), !next_read_fails);
        let old_io_count = f.adapter.observed.len();
        f.timeout(NormalAbortTarget::Submitted { seq });
        f.adapter.script.push_back(Reply::ok(Io::Confirm));
        f.run_to_await_ack();
        assert_eq!(
            f.adapter.observed.len(),
            old_io_count + 1,
            "旧 Disable 只免去重复 Stop，必须实际确认当前代次"
        );
        let message = f.ctrl.mailbox_snapshot().unwrap();
        assert_eq!(message.code, FaultCode::CommFault);
        assert_ne!(f.ctrl.inhibit_bits() & inhibit::FAULT_PENDING, 0);
        let expected = if next_read_fails {
            OutputSafetyState::AdapterUncertain
        } else {
            OutputSafetyState::TorqueOffConfirmed
        };
        assert_eq!(f.ctrl.safety_state(), expected);
        let _ = f.tick();
        assert_eq!(f.driver.convergence().phase(), ConvergePhase::AwaitAck);
        f.ctrl.ack_fault(message.generation);
        let _ = f.tick();
        assert_eq!(f.driver.convergence().phase(), ConvergePhase::Idle);
        assert_ne!(f.ctrl.inhibit_bits() & inhibit::COMM_FAULT, 0);
    }
}

/// 旧 Disable 的 `CompletedEffect` 只免重复 Stop；新撤销不能借旧完成绕过真实 Confirm。
#[test]
fn late_disable_confirmation_preserves_revocation_and_consumed_attempt_budget() {
    for rejected_confirmations in [1_u8, 5] {
        let mut f = TickFixture::healthy();
        let sequence = f.submit(AdapterNormalReq::Enable(false));
        f.adapter
            .script
            .extend([Io::Enable(false), Io::Read].map(Reply::ok));
        let _ = f.tick();
        assert_eq!(
            HardRtCtrlClient::new(f.ctrl.clone()).poll_last_normal_resp(sequence),
            Some(AdapterResp::Ok)
        );
        f.timeout(NormalAbortTarget::Submitted { seq: sequence });
        let mut original_round = None;
        for attempt in 1..=rejected_confirmations {
            let authority = f.ctrl.authority().clone();
            f.adapter.during_confirm = Some(Box::new(move || {
                let _ = authority.disable();
            }));
            f.adapter.script.push_back(Reply::ok(Io::Confirm));
            let _ = f.tick();
            assert_eq!(f.driver.convergence().phase(), ConvergePhase::Confirm);
            let round = *f.driver.convergence().round().unwrap();
            let identity = (round.ctx, round.generation, round.estop_bound);
            assert_eq!(*original_round.get_or_insert(identity), identity);
            assert_eq!(round.attempt, attempt, "真实撤销不刷新本轮确认次数");
            assert_eq!(round.completed_safety, None);
            assert_ne!(f.ctrl.inhibit_bits() & inhibit::CONVERGING, 0);
        }
        let calls = f.adapter.observed.len();
        if rejected_confirmations == 1 {
            f.adapter.script.push_back(Reply::ok(Io::Confirm));
        }
        let _ = f.tick();
        f.adapter.assert_drained();
        let round = f.driver.convergence().round().unwrap();
        assert_eq!(
            (round.ctx, round.generation, round.estop_bound),
            original_round.unwrap()
        );
        assert_eq!(f.driver.convergence().phase(), ConvergePhase::AwaitAck);
        match rejected_confirmations {
            1 => {
                assert_eq!(f.adapter.observed.len(), calls + 1);
                assert_eq!(round.attempt, 2);
                assert_eq!(
                    round.completed_safety,
                    Some(ConvergeCompletion::AllOffTrusted)
                );
            }
            5 => {
                assert_eq!(
                    f.adapter.observed.len(),
                    calls,
                    "五次已用完，不准补第六次 IO"
                );
                assert_eq!(round.attempt, 5);
                assert!(matches!(
                    round.completed_safety,
                    Some(ConvergeCompletion::AdapterUncertain { .. })
                ));
                assert_eq!(f.ctrl.safety_state(), OutputSafetyState::AdapterUncertain);
            }
            _ => unreachable!(),
        }
        assert_ne!(
            f.ctrl.inhibit_bits() & inhibit::FAULT_PENDING,
            0,
            "原 ACK 责任保持"
        );
    }
}

#[test]
fn normal_noncommunication_errors_publish_adapter_fault_with_zero_default_detail() {
    for error in [
        RobotError::InvalidParameter("固定错误"),
        RobotError::HardwareFault("固定设备错误"),
    ] {
        let mut f = TickFixture::healthy();
        let _ = f.submit(AdapterNormalReq::Enable(true));
        f.adapter.script.extend([
            Reply::error(Io::Enable(true), error),
            Reply::ok(Io::Safety),
            Reply::ok(Io::Confirm),
        ]);
        // 先执行真正的 normal 入口，再由后续 tick 执行收敛。
        let _ = f.tick();
        f.run_to_await_ack();
        let message = f.ctrl.mailbox_snapshot().unwrap();
        assert_eq!(message.code, FaultCode::AdapterErr);
        assert_eq!(message.detail, 0);
        assert_eq!(message.origin, FaultOrigin::NormalAction);
        assert!(f.ctrl.rx_trust.load(Ordering::Acquire));
    }
}

#[test]
fn preflight_noncommunication_error_exhaustion_defaults_adapter_detail_to_zero() {
    let mut f = Fixture::new(true, [Io::Safety, Io::Connect].map(Reply::ok));
    f.adapter.script.extend(std::iter::repeat_n(
        Reply::error(Io::Confirm, RobotError::HardwareFault("设备拒绝")),
        5,
    ));
    for _ in 0..7 {
        f.tick();
    }
    assert_eq!(f.machine.phase(), ConvergePhase::AwaitAck);
    let message = f.ctrl.mailbox_snapshot().unwrap();
    assert_eq!(message.code, FaultCode::AdapterErr);
    assert_eq!(message.detail, 0);
    assert!(f.ctrl.session_clean.load(Ordering::Acquire));
    assert!(!f.ctrl.rx_trust.load(Ordering::Acquire));
    assert_eq!(f.ctrl.safety_state(), OutputSafetyState::AdapterUncertain);
    f.adapter.assert_drained();
}

#[test]
fn pending_cancel_decoder_only_strips_its_single_tag_and_rejects_every_reserved_bit() {
    assert_eq!(decode_serial_pending(0), Ok(None));
    for seq in [1, 17, (1_u64 << 40) - 1] {
        let payload = (seq << 16) | (1 << 8) | 1;
        for cancelled in [false, true] {
            let raw = payload | if cancelled { 1_u64 << 63 } else { 0 };
            let decoded = decode_serial_pending(raw).unwrap().unwrap();
            assert_eq!(decoded.action.seq, seq);
            assert_eq!((decoded.action.action, decoded.action.arg), (1, 1));
            assert_eq!(decoded.cancelled, cancelled);
        }
        for bit in 56..63 {
            assert!(decode_serial_pending(payload | (1_u64 << bit)).is_err());
            assert!(decode_serial_pending(payload | (1_u64 << bit) | PENDING_CANCELLED).is_err());
        }
    }
    assert!(decode_serial_pending(PENDING_CANCELLED).is_err());
    assert!(decode_serial_pending(PENDING_CANCELLED | 1).is_err());
}

#[test]
fn cancel_mark_cas_losing_to_real_serial_consumption_never_reinserts_the_old_request() {
    let mut f = TickFixture::healthy();
    let seq = f.submit(AdapterNormalReq::Enable(false));
    f.adapter
        .script
        .extend([Io::Enable(false), Io::Read].map(Reply::ok));
    let ctrl = f.ctrl.clone();
    let mut calls = 0;
    ctrl.cancel_pending_before_cas(|| {
        calls += 1;
        assert_eq!(calls, 1);
        let _ = f.tick();
    });
    assert_eq!(f.ctrl.pending_normal.load(Ordering::Acquire), 0);
    assert_eq!(
        HardRtCtrlClient::new(f.ctrl.clone()).poll_last_normal_resp(seq),
        Some(AdapterResp::Ok)
    );
    f.adapter.assert_drained();
}

#[test]
fn cancel_tag_visible_before_authority_cas_only_cancels_and_does_not_start_an_emergency() {
    let mut f = TickFixture::healthy();
    let seq = f.submit(AdapterNormalReq::Enable(true));
    let payload = f.ctrl.pending_normal.load(Ordering::Acquire);
    let client = HardRtCtrlClient::new(f.ctrl.clone());
    let mut injected = false;
    client.estop_monotonic_after_cancel(|| {
        if injected {
            return;
        }
        injected = true;
        assert_eq!(
            f.ctrl.pending_normal.load(Ordering::Acquire),
            payload | PENDING_CANCELLED
        );
        let _ = f.tick();
        assert_eq!(f.driver.convergence().phase(), ConvergePhase::Idle);
        assert_eq!(f.ctrl.mailbox_snapshot(), None);
        assert_eq!(
            f.ctrl.policy.load(Ordering::Acquire),
            OutputPolicy::Disabled.to_u8()
        );
        assert_eq!(f.adapter.observed.len(), 0);
        assert_eq!(
            client.poll_last_normal_resp(seq),
            Some(AdapterResp::Err(AdapterErr::PreemptedByEmergency))
        );
    });
    f.adapter
        .script
        .extend([Io::Safety, Io::Confirm].map(Reply::ok));
    let _ = f.tick();
    let _ = f.tick();
    assert_eq!(f.driver.convergence().phase(), ConvergePhase::Idle);
    assert_eq!(f.ctrl.estop_requests.load(Ordering::Acquire), 1);
    assert_eq!(f.adapter.observed.len(), 2);
    f.adapter.assert_drained();
}

#[test]
fn completed_cancellation_is_still_matched_by_a_late_timeout_and_keeps_estop_obligation() {
    let mut f = TickFixture::healthy();
    let seq = f.submit(AdapterNormalReq::Enable(true));
    let client = HardRtCtrlClient::new(f.ctrl.clone());
    let mut injected = false;
    client.estop_monotonic_after_cancel(|| {
        if !injected {
            injected = true;
            let _ = f.tick();
        }
    });
    assert_eq!(f.ctrl.pending_normal.load(Ordering::Acquire), 0);
    f.timeout(NormalAbortTarget::Submitted { seq });
    f.adapter
        .script
        .extend([Io::Safety, Io::Confirm].map(Reply::ok));
    f.run_to_await_ack();
    let mail = f.ctrl.mailbox_snapshot().unwrap();
    assert_eq!(mail.code, FaultCode::CommFault);
    assert_eq!(mail.detail, CommErrorKind::PendingDeadline.to_u8());
    assert!(f.ctrl.rx_trust.load(Ordering::Acquire));
    assert!(f.driver.convergence().round().unwrap().estop_bound);
    assert_eq!(f.ctrl.safety_state(), OutputSafetyState::TorqueOffConfirmed);
}

#[test]
fn duplicate_estop_publishers_preserve_cancelled_payload_and_timeout_identity() {
    let mut f = TickFixture::healthy();
    let seq = f.submit(AdapterNormalReq::Enable(true));
    let payload = f.ctrl.pending_normal.load(Ordering::Acquire);
    f.timeout(NormalAbortTarget::Submitted { seq });
    let first = HardRtCtrlClient::new(f.ctrl.clone());
    let second = HardRtCtrlClient::new(f.ctrl.clone());
    first.estop_monotonic();
    second.estop_monotonic();
    assert_eq!(
        f.ctrl.pending_normal.load(Ordering::Acquire),
        payload | PENDING_CANCELLED
    );
    assert!(matches!(
        first.request_normal(AdapterNormalReq::Enable(false)),
        NormalReqStatus::Busy
    ));
    f.adapter
        .script
        .extend([Io::Safety, Io::Confirm].map(Reply::ok));
    f.run_to_await_ack();
    assert_eq!(f.ctrl.estop_requests.load(Ordering::Acquire), 2);
    assert_eq!(
        f.ctrl.mailbox_snapshot().unwrap().detail,
        CommErrorKind::PendingDeadline.to_u8()
    );
    assert_eq!(
        first.poll_last_normal_resp(seq),
        Some(AdapterResp::Err(AdapterErr::AbortedByTimeout))
    );
    assert!(f.ctrl.rx_trust.load(Ordering::Acquire));
    assert_eq!(f.adapter.observed.len(), 2);
}

#[test]
fn all_completed_effects_except_successful_disable_keep_full_timeout_convergence() {
    let cases = [
        (AdapterNormalReq::Enable(true), Some(Io::Enable(true))),
        (AdapterNormalReq::Enable(false), Some(Io::Enable(false))),
        (
            AdapterNormalReq::SetMode(ControlMode::Position),
            Some(Io::Mode(ControlMode::Position)),
        ),
        (AdapterNormalReq::Connect, Some(Io::Connect)),
        (AdapterNormalReq::Disconnect, Some(Io::Disconnect)),
        (AdapterNormalReq::HoldCurrent, None),
    ];
    for (request, io) in cases {
        for success in [false, true] {
            if success && (io == Some(Io::Enable(false)) || io.is_none()) {
                continue;
            }
            let mut f = TickFixture::healthy();
            let seq = f.submit(request);
            if let Some(io) = io {
                f.adapter.script.push_back(if success {
                    Reply::ok(io)
                } else {
                    Reply::error(io, RobotError::HardwareFault("动作失败"))
                });
            }
            if success {
                f.adapter.script.push_back(Reply::ok(Io::Read));
            }
            let _ = f.tick();
            let completed =
                CompletedEffect::unpack(f.ctrl.completed_effect.load(Ordering::Acquire));
            assert_eq!(completed.action.seq, seq);
            assert_eq!(completed.success, success);
            assert!(!ActionRecord::unpack(f.ctrl.active_action.load(Ordering::Acquire)).valid);
            f.timeout(NormalAbortTarget::Submitted { seq });
            f.adapter
                .script
                .extend([Io::Safety, Io::Confirm].map(Reply::ok));
            f.run_to_await_ack();
            assert_eq!(
                f.ctrl.mailbox_snapshot().unwrap().code,
                FaultCode::CommFault
            );
            assert_eq!(
                f.ctrl.mailbox_snapshot().unwrap().detail,
                CommErrorKind::PendingDeadline.to_u8()
            );
            assert_eq!(
                f.adapter
                    .observed
                    .iter()
                    .filter(|entry| entry.io == Io::Safety)
                    .count(),
                1
            );
            assert_eq!(
                f.adapter
                    .observed
                    .iter()
                    .filter(|entry| entry.io == Io::Confirm)
                    .count(),
                1
            );
        }
    }
}

#[test]
fn active_action_is_retired_at_real_adapter_boundary_and_preserves_typed_result() {
    for reason in [AbortReason::Timeout, AbortReason::Emergency] {
        let mut f = TickFixture::healthy();
        let seq = f.submit(AdapterNormalReq::Enable(true));
        f.adapter.script.push_back(Reply {
            interrupt: Some(reason),
            ..Reply::error(Io::Enable(true), RobotError::Aborted(reason))
        });
        let _ = f.tick();
        let expected = match reason {
            AbortReason::Timeout => AdapterErr::AbortedByTimeout,
            AbortReason::Emergency => AdapterErr::PreemptedByEmergency,
            AbortReason::Revoked => unreachable!("此用例只遍历 Timeout/Emergency"),
        };
        assert_eq!(
            HardRtCtrlClient::new(f.ctrl.clone()).poll_last_normal_resp(seq),
            Some(AdapterResp::Err(expected))
        );
        let completed = CompletedEffect::unpack(f.ctrl.completed_effect.load(Ordering::Acquire));
        assert_eq!(completed.action.seq, seq);
        assert!(!completed.success);
        assert!(!ActionRecord::unpack(f.ctrl.active_action.load(Ordering::Acquire)).valid);
        assert!(f.ctrl.rx_trust.load(Ordering::Acquire));
        f.adapter
            .script
            .extend([Io::Safety, Io::Confirm].map(Reply::ok));
        let _ = f.tick();
        let _ = f.tick();
        match reason {
            AbortReason::Timeout => {
                assert_eq!(f.driver.convergence().phase(), ConvergePhase::AwaitAck);
                assert_eq!(
                    f.ctrl.mailbox_snapshot().unwrap().detail,
                    CommErrorKind::PendingDeadline.to_u8()
                );
                assert!(!f.ctrl.safety_latched());
            }
            AbortReason::Emergency => {
                assert_eq!(f.driver.convergence().phase(), ConvergePhase::Idle);
                assert_eq!(f.ctrl.mailbox_snapshot(), None);
                assert!(f.ctrl.safety_latched());
            }
            AbortReason::Revoked => unreachable!("此用例只遍历 Timeout/Emergency"),
        }
        f.adapter.assert_drained();
    }
}

#[test]
fn unsubmitted_with_unrelated_residue_fails_closed_without_executing_it() {
    for residue in 0..3 {
        let mut f = TickFixture::healthy();
        let seq = f.submit(AdapterNormalReq::Enable(true));
        match residue {
            0 => {}
            1 => {
                f.ctrl
                    .pending_normal
                    .fetch_or(PENDING_CANCELLED, Ordering::AcqRel);
            }
            _ => {
                let action = decode_serial_pending(f.ctrl.pending_normal.swap(0, Ordering::AcqRel))
                    .unwrap()
                    .unwrap()
                    .action;
                f.ctrl.active_action.store(action.pack(), Ordering::Release);
            }
        }
        f.timeout(NormalAbortTarget::Unsubmitted);
        f.adapter.script.push_back(Reply::ok(Io::Safety));
        f.adapter.script.push_back(Reply::ok(Io::Confirm));
        f.run_to_await_ack();
        let mail = f.ctrl.mailbox_snapshot().unwrap();
        assert_eq!(
            mail.detail,
            CommErrorKind::Unknown.to_u8(),
            "residue={residue}, seq={seq}"
        );
        assert_eq!(mail.origin, FaultOrigin::InvariantViolation);
        assert!(!f.ctrl.rx_trust.load(Ordering::Acquire));
        assert_eq!(f.ctrl.pending_normal.load(Ordering::Acquire), 0);
        assert_eq!(f.ctrl.abort_request.load(Ordering::Acquire), 0);
        assert_eq!(f.ctrl.safety_state(), OutputSafetyState::AdapterUncertain);
    }
}

#[test]
fn invalid_or_unmatched_timeout_and_invalid_pending_are_unknown_faults() {
    for (abort, pending) in [
        (1_u64 << 62, 0),
        (73, 0),
        (0, (1_u64 << 56) | (7 << 16) | 1),
        (0, PENDING_CANCELLED),
    ] {
        let mut f = TickFixture::healthy();
        f.ctrl.abort_request.store(abort, Ordering::Release);
        f.ctrl.pending_normal.store(pending, Ordering::Release);
        f.adapter.script.push_back(Reply::ok(Io::Safety));
        f.adapter.script.push_back(Reply::ok(Io::Confirm));
        f.run_to_await_ack();
        let mail = f.ctrl.mailbox_snapshot().unwrap();
        assert_eq!(mail.code, FaultCode::CommFault);
        assert_eq!(mail.detail, CommErrorKind::Unknown.to_u8());
        assert_eq!(mail.origin, FaultOrigin::InvariantViolation);
        assert!(!f.ctrl.rx_trust.load(Ordering::Acquire));
        assert_eq!(f.ctrl.abort_request.load(Ordering::Acquire), 0);
    }
}

#[test]
fn a_new_timeout_survives_the_old_retirement_compare_exchange_and_is_processed_next_tick() {
    let mut f = TickFixture::healthy();
    let seq = f.submit(AdapterNormalReq::Enable(false));
    f.adapter
        .script
        .extend([Io::Enable(false), Io::Read].map(Reply::ok));
    let _ = f.tick();
    f.timeout(NormalAbortTarget::Submitted { seq });
    let client = HardRtCtrlClient::new(f.ctrl.clone());
    assert!(f.driver.convergence_mut().retire_timeout_before_clear(
        &f.ctrl,
        &f.diag,
        f.clock.now(),
        || {
            assert!(f.ctrl.mailbox_snapshot().is_some(), "旧请求先登记责任");
            // 不合法的新身份也必须保留，下一 tick 应独立记不变量错误并撤销信任。
            client.abort_pending_normal(NormalAbortTarget::Submitted { seq: seq + 77 });
        }
    ));
    assert_eq!(f.ctrl.abort_request.load(Ordering::Acquire), seq + 77);
    assert!(f.ctrl.rx_trust.load(Ordering::Acquire));
    let generation = f.ctrl.mailbox_snapshot().unwrap().generation;
    assert_eq!(f.driver.convergence().phase(), ConvergePhase::Confirm);
    // 旧 Disable 完成只免重复 Stop，仍须本轮真实 Confirm；新 Unknown 不刷新此轮。
    f.adapter.script.push_back(Reply::ok(Io::Confirm));
    let _ = f.tick();
    assert_eq!(f.ctrl.abort_request.load(Ordering::Acquire), 0);
    assert!(
        !f.ctrl.rx_trust.load(Ordering::Acquire),
        "新请求必须实际经过 Unknown 退场"
    );
    assert_eq!(f.ctrl.mailbox_snapshot().unwrap().generation, generation);
    assert_eq!(f.driver.convergence().counters().stop_writes, 1);
    assert_eq!(
        f.adapter.observed.last().map(|observation| observation.io),
        Some(Io::Confirm)
    );
    f.adapter.assert_drained();
}

#[test]
fn every_inhibit_bit_blocks_periodic_io_even_with_active_forward_policy() {
    for bit in [
        inhibit::COMM_FAULT,
        inhibit::ADAPTER_FAULT,
        inhibit::STOP_FAILED,
        inhibit::CONVERGING,
        inhibit::FAULT_PENDING,
        inhibit::BOOT,
    ] {
        let mut f = TickFixture::healthy();
        let old = *f.driver.state();
        f.ctrl.tighten_inhibits(bit);
        f.ctrl
            .policy
            .store(OutputPolicy::ActiveForward.to_u8(), Ordering::Release);
        for _ in 0..3 {
            let snapshot = f.tick();
            assert_eq!(snapshot.state.stamp_ns, old.stamp_ns);
            assert_eq!(
                snapshot.state.positions.map(f32::to_bits),
                old.positions.map(f32::to_bits)
            );
        }
        assert!(f.adapter.observed.is_empty());
    }
}

#[test]
fn failed_and_preempted_periodic_reads_never_publish_partial_samples() {
    for error in [
        RobotError::Comm(CommErrorKind::Timeout),
        RobotError::Aborted(AbortReason::Emergency),
    ] {
        let mut f = TickFixture::healthy();
        f.adapter.script.push_back(Reply::ok(Io::Read));
        let _ = f.tick();
        let old = *f.driver.state();
        f.adapter.script.push_back(Reply {
            interrupt: match error {
                RobotError::Aborted(reason) => Some(reason),
                _ => None,
            },
            ..Reply::error(Io::Read, error)
        });
        let snapshot = f.tick();
        assert_eq!(snapshot.state.stamp_ns, old.stamp_ns);
        assert_eq!(
            snapshot.state.positions.map(f32::to_bits),
            old.positions.map(f32::to_bits)
        );
        assert_eq!(
            snapshot.state.velocities.map(f32::to_bits),
            old.velocities.map(f32::to_bits)
        );
        match error {
            RobotError::Aborted(_) => {
                assert_eq!(f.ctrl.mailbox_snapshot(), None);
                assert!(f.ctrl.rx_trust.load(Ordering::Acquire));
            }
            _ => assert_eq!(
                f.ctrl.mailbox_snapshot().unwrap().origin,
                FaultOrigin::Periodic
            ),
        }
        f.adapter.assert_drained();
    }
}

#[test]
fn repeated_overdue_reads_cannot_feed_liveness_but_io_free_waits_can() {
    let mut f = TickFixture::healthy();
    let before = f.ctrl.liveness_counter.load(Ordering::Acquire);
    f.adapter.script.extend(std::iter::repeat_n(
        Reply {
            elapsed_ms: 600,
            ..Reply::ok(Io::Read)
        },
        3,
    ));
    for _ in 0..3 {
        let _ = f.tick();
    }
    assert_eq!(
        f.ctrl.liveness_counter.load(Ordering::Acquire),
        before,
        "过期调用的 tick 尾不得掩盖 lease 失败"
    );
    f.ctrl.tighten_inhibits(inhibit::ADAPTER_FAULT);
    let _ = f.tick();
    assert!(
        f.ctrl.liveness_counter.load(Ordering::Acquire) > before,
        "无 IO 合法等待不能继承旧 deadline 永久停喂"
    );
    f.adapter.assert_drained();
}

#[test]
fn missing_completion_record_in_await_ack_escalates_without_more_io() {
    let mut f = Fixture::await_trusted_fault();
    let previous = f.ctrl.mailbox_snapshot().unwrap().generation;
    f.machine.clear_completed_safety_for_test();
    let io_count = f.adapter.observed.len();
    f.estop();
    assert!(matches!(
        f.machine.phase(),
        ConvergePhase::SafetyStopFailedAwaitAck { .. }
    ));
    let mail = f.ctrl.mailbox_snapshot().unwrap();
    assert_eq!(mail.generation, previous + 1);
    assert_eq!(mail.code, FaultCode::SafetyStopFailed);
    assert_eq!(mail.origin, FaultOrigin::InvariantViolation);
    assert_eq!(f.ctrl.safety_state(), OutputSafetyState::AdapterUncertain);
    for _ in 0..3 {
        f.tick();
    }
    assert_eq!(f.adapter.observed.len(), io_count);
    f.adapter.assert_drained();
}

#[cfg(feature = "test-comm-fault-hook")]
mod sampling_boundary {
    use super::*;
    use crate::comm_fault_trace::{self as trace, SampleMode, SampleStatus, TracePage};

    fn isolated(name: &str, check: impl FnOnce(&TracePage)) {
        if std::env::var_os("ROBOT_TEST_TICK_BOUNDARY_CHILD").is_some() {
            trace::install_from_env().unwrap();
            let path = std::env::var_os("ROBOT_TEST_TRACE_PATH").unwrap();
            let page = TracePage::open(std::path::Path::new(&path)).unwrap();
            check(&page);
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("trace");
        let _page = TracePage::create(&path).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("post_e_convergence_acceptance::sampling_boundary::{name}"),
                "--nocapture",
            ])
            .env("ROBOT_TEST_TICK_BOUNDARY_CHILD", "1")
            .env("ROBOT_TEST_TRACE_PATH", path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "真实 tick 子进程失败：{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    fn arm_idle(f: &mut TickFixture, page: &TracePage) {
        trace::request_boundary(101, SampleMode::ArmIdle, f.clock.now().0).unwrap();
        assert_eq!(page.armed_seq(), 0, "control 接收请求不等于 serial Ready");
        let _ = f.tick();
        let ready = page.boundary_snapshot().unwrap();
        assert_eq!(ready.receipt_seq, 101);
        assert_eq!(ready.mode, Some(SampleMode::ArmIdle));
        assert_eq!(ready.status, SampleStatus::Ready);
        assert!(ready.gate_active);
        assert!(f.adapter.observed.is_empty(), "Ready tick 不得开始普通 IO");
    }

    #[test]
    fn idle_gate_preserves_pending_then_real_estop_runs_safety_and_confirm() {
        isolated(
            "idle_gate_preserves_pending_then_real_estop_runs_safety_and_confirm",
            |page| {
                let mut f = TickFixture::healthy();
                arm_idle(&mut f, page);
                let seq = f.submit(AdapterNormalReq::Enable(true));
                let pending = f.ctrl.pending_normal.load(Ordering::Acquire);
                let _ = f.tick();
                assert_eq!(f.ctrl.pending_normal.load(Ordering::Acquire), pending);
                assert!(f.adapter.observed.is_empty());
                let client = HardRtCtrlClient::new(f.ctrl.clone());
                let mut before_cas_calls = 0;
                client.estop_monotonic_after_cancel(|| {
                    before_cas_calls += 1;
                    // 原语已进入但 CAS 尚未建立保护：此时另一个 serial tick 仍不能放行 normal。
                    let _ = f.tick();
                    assert!(page.boundary_snapshot().unwrap().gate_active);
                    assert!(f.adapter.observed.is_empty());
                    assert_eq!(
                        f.ctrl.pending_normal.load(Ordering::Acquire),
                        0,
                        "原语前置取消仍须正常退休"
                    );
                });
                assert_eq!(before_cas_calls, 1);
                // 不由原语调用者另写测试状态；serial 必须观察真实保护并自行通行。
                f.adapter
                    .script
                    .extend([Io::Safety, Io::Confirm].map(Reply::ok));
                let _ = f.tick();
                assert!(!page.boundary_snapshot().unwrap().gate_active);
                let _ = f.tick();
                f.adapter.assert_drained();
                assert_eq!(f.ctrl.safety_state(), OutputSafetyState::TorqueOffConfirmed);
                assert_eq!(f.ctrl.pending_normal.load(Ordering::Acquire), 0);
                assert_eq!(
                    HardRtCtrlClient::new(f.ctrl.clone()).poll_last_normal_resp(seq),
                    Some(AdapterResp::Err(AdapterErr::PreemptedByEmergency))
                );
                assert_eq!(page.armed_seq(), 101, "安全收敛不能提前关闭采样 epoch");
                trace::request_boundary(102, SampleMode::Disarm, f.clock.now().0).unwrap();
                f.adapter.script.push_back(Reply::ok(Io::Read));
                let _ = f.tick();
                assert_eq!(page.boundary_snapshot().unwrap().receipt_seq, 102);
                assert_eq!(page.armed_seq(), 0);
            },
        );
    }

    #[test]
    fn cancelled_request_retires_without_waiting_for_idle_expiry() {
        isolated(
            "cancelled_request_retires_without_waiting_for_idle_expiry",
            |page| {
                let mut f = TickFixture::healthy();
                arm_idle(&mut f, page);
                let seq = f.submit(AdapterNormalReq::Enable(true));
                // 模拟既有取消槽事实，不发新 EStop，不替测试门撤除保护。
                f.ctrl
                    .pending_normal
                    .fetch_or(PENDING_CANCELLED, Ordering::AcqRel);
                let _ = f.tick();
                assert!(page.boundary_snapshot().unwrap().gate_active);
                assert_eq!(f.ctrl.pending_normal.load(Ordering::Acquire), 0);
                assert_eq!(
                    HardRtCtrlClient::new(f.ctrl.clone()).poll_last_normal_resp(seq),
                    Some(AdapterResp::Err(AdapterErr::PreemptedByEmergency))
                );
                assert!(f.adapter.observed.is_empty());
            },
        );
    }

    #[test]
    fn submitted_timeout_bypasses_idle_gate_and_fault_can_disarm() {
        isolated(
            "submitted_timeout_bypasses_idle_gate_and_fault_can_disarm",
            |page| {
                let mut f = TickFixture::healthy();
                arm_idle(&mut f, page);
                let seq = f.submit(AdapterNormalReq::Enable(true));
                f.timeout(NormalAbortTarget::Submitted { seq });
                f.adapter
                    .script
                    .extend([Io::Safety, Io::Confirm].map(Reply::ok));
                f.run_to_await_ack();
                let mail = f.ctrl.mailbox_snapshot().unwrap();
                assert_eq!(mail.code, FaultCode::CommFault);
                assert_eq!(mail.detail, CommErrorKind::PendingDeadline.to_u8());
                assert_eq!(
                    HardRtCtrlClient::new(f.ctrl.clone()).poll_last_normal_resp(seq),
                    Some(AdapterResp::Err(AdapterErr::AbortedByTimeout))
                );
                trace::request_boundary(102, SampleMode::Disarm, f.clock.now().0).unwrap();
                let _ = f.tick();
                let result = page.boundary_snapshot().unwrap();
                assert_eq!(result.receipt_seq, 102);
                assert_eq!(result.status, SampleStatus::Disarmed);
                assert_eq!(result.armed_seq, 0);
                assert!(matches!(
                    f.driver.convergence().phase(),
                    ConvergePhase::AwaitAck
                ));
            },
        );
    }

    #[test]
    fn invalid_pending_under_idle_gate_still_becomes_invariant_fault() {
        isolated(
            "invalid_pending_under_idle_gate_still_becomes_invariant_fault",
            |page| {
                let mut f = TickFixture::healthy();
                arm_idle(&mut f, page);
                f.ctrl
                    .pending_normal
                    .store(PENDING_CANCELLED, Ordering::Release);
                f.adapter
                    .script
                    .extend([Io::Safety, Io::Confirm].map(Reply::ok));
                f.run_to_await_ack();
                let mail = f.ctrl.mailbox_snapshot().unwrap();
                assert_eq!(mail.origin, FaultOrigin::InvariantViolation);
                assert_eq!(mail.detail, CommErrorKind::Unknown.to_u8());
                assert_eq!(f.ctrl.pending_normal.load(Ordering::Acquire), 0);
                assert!(!f.ctrl.rx_trust.load(Ordering::Acquire));
            },
        );
    }
}

#[cfg(feature = "test-comm-fault-hook")]
#[test]
fn mock_call_lease_covers_estop_errors_and_excludes_other_adapters() {
    use crate::comm_fault_adapter::CommFaultAdapter;
    use crate::comm_fault_trace::{TraceKind, capture_emitted};
    use crate::hard_rt_ctrl::NORMAL_IO_ACTIVE;

    for case in ["estop_in_call", "inner_error", "refused", "non_mock"] {
        let mut f = TickFixture::healthy();
        let mocked = case != "non_mock";
        if mocked {
            f.adapter.name = "mock_serial_rt";
        }
        let (owner, view) = f.ctrl.authority().reserve_enable().unwrap();
        let execution = f.ctrl.authority().start_enable(view).unwrap();
        f.ctrl.execution_slot().install(execution.view()).unwrap();
        f.probe.set_context(
            BusOpClass::NormalEffectful,
            crate::serial_preempt::InstalledIntent::EnableStep,
            47,
            MonoRawNs(f.clock.now().0 + 100_000_000),
            true,
        );
        let ctrl = f.ctrl.clone();
        if case == "refused" {
            HardRtCtrlClient::new(ctrl.clone()).estop_monotonic();
        } else {
            f.adapter.script.push_back(match case {
                "inner_error" => {
                    Reply::error(Io::Enable(true), RobotError::HardwareFault("设备返回失败"))
                }
                _ => Reply::ok(Io::Enable(true)),
            });
            f.adapter.during_enable = Some(Box::new(move || {
                assert_eq!(ctrl.safety_word() & NORMAL_IO_ACTIVE != 0, mocked);
                if case == "estop_in_call" {
                    HardRtCtrlClient::new(ctrl.clone()).estop_monotonic();
                }
            }));
        }
        let mut wrapper = CommFaultAdapter::new(f.adapter, f.clock, f.probe, None, f.ctrl.clone());
        let mut response = None;
        let events = capture_emitted(|| {
            response = Some(wrapper.enable(true));
        });
        match case {
            "refused" => assert_eq!(
                response,
                Some(Err(RobotError::Aborted(AbortReason::Emergency)))
            ),
            "inner_error" => assert_eq!(
                response,
                Some(Err(RobotError::HardwareFault("设备返回失败")))
            ),
            _ => assert_eq!(response, Some(Ok(()))),
        }
        assert_eq!(
            f.ctrl.safety_word() & NORMAL_IO_ACTIVE,
            0,
            "{case}: 方法返回必须归还执行权"
        );
        f.ctrl.execution_slot().clear();
        let result = match response {
            Some(Ok(())) => robot_core::control_authority::EnableResult::Succeeded,
            _ => robot_core::control_authority::EnableResult::Failed,
        };
        f.ctrl.authority().finish_enable(execution, result).unwrap();
        match case {
            "non_mock" => {
                f.ctrl.authority().commit_enable(owner).unwrap();
            }
            _ => {
                f.ctrl.authority().retire_enable(owner).unwrap();
            }
        }
        let acquired = events
            .iter()
            .filter(|e| e.kind == TraceKind::NormalAcquire && e.b == 1)
            .count();
        let released = events
            .iter()
            .filter(|e| e.kind == TraceKind::NormalRelease)
            .count();
        let expected = usize::from(matches!(case, "estop_in_call" | "inner_error"));
        assert_eq!((acquired, released), (expected, expected), "{case}");
        if case == "estop_in_call" {
            let committed = events
                .iter()
                .find(|e| e.kind == TraceKind::PrimitiveCommitted)
                .unwrap();
            assert_eq!(committed.b & 1, 1, "真实 EStop CAS 发生在 wrapper lease 内");
        }
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "同一真实部分执行的失败事实、原 owner 退休和 Stop/Confirm 顺序须完整验收"
)]
fn actual_partial_enable_and_late_success_cannot_retire_lower_using_old_off() {
    for boundary in [
        EnableBoundary::RevokeBetweenMotors,
        EnableBoundary::RevokeAfterLastMotor,
        EnableBoundary::FailAfterFirstMotor,
    ] {
        let mut f = TickFixture::healthy();
        f.adapter.enable_boundary = boundary;
        let seq = f.submit(AdapterNormalReq::Enable(true));
        let (domain, epoch) = match &f.pending {
            crate::ipc_pending::IpcPending::Enable(enable) => {
                (enable.owner().domain(), enable.view_epoch())
            }
            _ => unreachable!(),
        };
        f.adapter
            .script
            .extend([Io::Enable(true), Io::Safety, Io::Confirm].map(Reply::ok));
        let _ = f.tick();
        assert!(
            f.adapter.actual_torque[0],
            "首个受真实租约保护的电机调用确已上扭"
        );
        let expected = match boundary {
            EnableBoundary::RevokeBetweenMotors => AdapterResp::Err(AdapterErr::AuthorityRevoked),
            EnableBoundary::RevokeAfterLastMotor => AdapterResp::Ok,
            EnableBoundary::FailAfterFirstMotor => AdapterResp::Err(AdapterErr::HardwareFault),
            EnableBoundary::Ordinary => unreachable!(),
        };
        assert_eq!(
            HardRtCtrlClient::new(f.ctrl.clone()).poll_last_normal_resp(seq),
            Some(expected)
        );
        assert_eq!(f.ctrl.safety_state(), OutputSafetyState::StopCommanded);
        match boundary {
            EnableBoundary::FailAfterFirstMotor => {
                let failure = f.failure.take().expect("原 seq 的首次真实失败事实");
                assert!(matches!(
                    failure.source(),
                    crate::ipc_pending::EnableFailureSource::ExecutionFailed(
                        AdapterErr::HardwareFault
                    )
                ));
                let crate::ipc_pending::IpcPending::Enable(enable) = &f.pending else {
                    panic!("首次交接失败事实时必须保留原 Enable owner");
                };
                assert_eq!(
                    (enable.owner().domain(), enable.view_epoch()),
                    (domain, epoch)
                );
                assert_eq!(enable.seq(), None, "原 seq 的真实完成已消费");
            }
            EnableBoundary::RevokeBetweenMotors
            | EnableBoundary::RevokeAfterLastMotor
            | EnableBoundary::Ordinary => {
                assert!(f.failure.is_none());
                assert!(matches!(
                    f.pending,
                    crate::ipc_pending::IpcPending::Lower(_)
                ));
            }
        }
        assert_eq!(f.driver.convergence().phase(), ConvergePhase::StopIssue);
        assert!(
            f.ctrl
                .authority()
                .acquire_normal_io(robot_core::control_authority::NormalIoIntent::Motion)
                .is_err()
        );
        let _ = f.tick();
        assert!(f.failure.is_none(), "下一轮原 route 退休不得重报失败事实");
        assert_eq!(f.adapter.actual_torque, [false; 2]);
        assert_eq!(
            f.ctrl.safety_state(),
            OutputSafetyState::StopCommanded,
            "实际 Stop 返回还不是 Confirm 证明"
        );
        assert!(matches!(
            f.pending,
            crate::ipc_pending::IpcPending::Lower(_)
        ));
        let _ = f.tick();
        assert!(f.failure.is_none());
        assert_eq!(f.ctrl.safety_state(), OutputSafetyState::TorqueOffConfirmed);
        assert_eq!(
            f.driver.convergence().counters().stop_writes,
            2,
            "含启动一次，本次只能安排一次 Stop"
        );
        match boundary {
            EnableBoundary::FailAfterFirstMotor => {
                assert_eq!(
                    f.ctrl.mailbox_snapshot().unwrap().code,
                    FaultCode::AdapterErr
                );
                assert!(
                    matches!(f.pending, crate::ipc_pending::IpcPending::Lower(_)),
                    "原故障 ACK 责任不能由确认清掉"
                );
            }
            _ => assert!(matches!(f.pending, crate::ipc_pending::IpcPending::Idle)),
        }
        f.adapter.assert_drained();
    }
}

#[test]
fn finish_retry_keeps_original_sequence_and_does_not_rearm_a_completed_stop() {
    for timeout in [false, true] {
        let mut f = TickFixture::healthy();
        let lease_holder = Arc::new(std::sync::Mutex::new(None));
        let during_holder = lease_holder.clone();
        let ctrl = f.ctrl.clone();
        // 仅制造真实 finish 的 NormalIoActive 拒绝；不伪造 A2 结果或原 Execution。
        f.adapter.during_enable = Some(Box::new(move || {
            let view = ctrl.execution_slot().load().unwrap();
            let lease = ctrl
                .authority()
                .acquire_normal_io(robot_core::control_authority::NormalIoIntent::EnableStep(
                    view,
                ))
                .unwrap();
            *during_holder.lock().unwrap() = Some(lease);
        }));
        let seq = f.submit(AdapterNormalReq::Enable(true));
        f.adapter.script.push_back(Reply::ok(Io::Enable(true)));
        let _ = f.tick();
        let client = HardRtCtrlClient::new(f.ctrl.clone());
        assert_eq!(client.poll_last_normal_resp(seq), None);
        if timeout {
            f.timeout(NormalAbortTarget::Submitted { seq });
        } else {
            client.estop_monotonic();
        }
        f.adapter
            .script
            .extend([Io::Safety, Io::Confirm].map(Reply::ok));
        let _ = f.tick();
        assert_eq!(
            client.poll_last_normal_resp(seq),
            None,
            "高优先级停止不冒作原 Enable finish"
        );
        assert_eq!(f.driver.convergence().phase(), ConvergePhase::Confirm);
        let _ = f.tick();
        assert_eq!(client.poll_last_normal_resp(seq), None);
        assert_eq!(f.ctrl.safety_state(), OutputSafetyState::TorqueOffConfirmed);
        let original_round = *f.driver.convergence().round().unwrap();
        let stops = f.driver.convergence().counters().stop_writes;
        f.ctrl
            .authority()
            .release_normal_io(lease_holder.lock().unwrap().take().unwrap())
            .unwrap();
        if !timeout {
            f.adapter.script.push_back(Reply::ok(Io::Read));
        }
        let _ = f.tick();
        assert_eq!(client.poll_last_normal_resp(seq), Some(AdapterResp::Ok));
        assert_eq!(
            f.ctrl.safety_state(),
            OutputSafetyState::TorqueOffConfirmed,
            "finish 续接不得撤掉本调用后实际确认的 Off"
        );
        assert_eq!(*f.driver.convergence().round().unwrap(), original_round);
        assert_eq!(f.driver.convergence().counters().stop_writes, stops);
        assert_eq!(f.metrics.normal_completed_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            f.adapter
                .observed
                .iter()
                .filter(|entry| entry.io == Io::Enable(true))
                .count(),
            1
        );
        f.adapter.assert_drained();
    }
}

#[test]
fn enable_compensation_merges_active_attempts_and_never_reopens_sticky_failure() {
    let mut f = Fixture::healthy();
    f.fault(FaultCode::CommFault, CommErrorKind::PendingDeadline);
    f.adapter.script.push_back(Reply::ok(Io::Safety));
    f.adapter.script.extend(std::iter::repeat_n(
        Reply::error(Io::Confirm, RobotError::Comm(CommErrorKind::Timeout)),
        5,
    ));
    f.tick();
    let initial = *f.machine.round().unwrap();
    let stop_writes = f.machine.counters().stop_writes;
    for attempt in 0..5 {
        let before = *f.machine.round().unwrap();
        f.machine
            .ensure_enable_compensation(&f.ctrl, &f.diag, f.clock.now());
        assert_eq!(*f.machine.round().unwrap(), before);
        assert_eq!(f.machine.counters().stop_writes, stop_writes);
        f.tick();
        assert_eq!(f.machine.round().unwrap().attempt, attempt + 1);
        assert_eq!(f.machine.round().unwrap().generation, initial.generation);
    }
    assert_eq!(f.ctrl.safety_state(), OutputSafetyState::AdapterUncertain);
    let terminal = *f.machine.round().unwrap();
    let terminal_phase = f.machine.phase();
    f.machine
        .ensure_enable_compensation(&f.ctrl, &f.diag, f.clock.now());
    assert_eq!(*f.machine.round().unwrap(), terminal);
    assert_eq!(f.machine.phase(), terminal_phase);
    assert_eq!(f.machine.counters().stop_writes, stop_writes);
    f.adapter.assert_drained();
}

#[test]
fn old_completed_or_same_fault_dedup_cannot_manufacture_enable_recovery_coverage() {
    let mut f = Fixture::await_trusted_fault();
    let original = *f.machine.round().unwrap();
    let mail = f.ctrl.mailbox_snapshot().unwrap();
    let receipt = f.machine.enter_fault_arranged(
        mail.code,
        mail.detail,
        FaultOrigin::NormalAction,
        &f.ctrl,
        &f.diag,
        f.clock.now(),
        31,
    );
    assert!(receipt.is_none());
    assert_eq!(*f.machine.round().unwrap(), original);
    assert_eq!(f.machine.phase(), ConvergePhase::AwaitAck);
    assert!(
        f.machine
            .claim_estop_arranged(&f.ctrl, &f.diag, f.clock.now())
            .is_none(),
        "无真实 pending 的重复 claim 不得复用旧 Off 生成接管事实"
    );
    f.adapter.assert_drained();
}

#[cfg(feature = "rt_guard")]
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "真实适配器与同一 driver 的完整补偿调用都在本场景受 allocator 验证"
)]
fn serial_enable_compensation_uses_real_allocator_without_allocating() {
    struct NoAllocAdapter {
        probe: Arc<SerialPreemptProbe>,
        ctrl: Arc<HardRtCtrlInner>,
        limits: JointLimits,
        on: bool,
        enable_calls: u64,
        stops: u64,
        confirms: u64,
    }
    impl ManipulatorAdapter for NoAllocAdapter {
        fn info(&self) -> AdapterInfo {
            AdapterInfo {
                name: "补偿零分配真调用",
                kind: AdapterKind::Manipulator,
                rt_class: RtClass::SerialRT,
                vendor_sdk: false,
            }
        }
        fn connect(&mut self) -> Result<(), RobotError> {
            Ok(())
        }
        fn disconnect(&mut self) -> Result<(), RobotError> {
            self.on = false;
            Ok(())
        }
        fn read_state(&mut self, _out: &mut JointStateBuf) -> Result<(), RobotError> {
            Ok(())
        }
        fn write_command(&mut self, _cmd: &JointCommandView<'_>) -> Result<(), RobotError> {
            Ok(())
        }
        fn set_mode(&mut self, _mode: ControlMode) -> Result<(), RobotError> {
            Ok(())
        }
        fn enable(&mut self, on: bool) -> Result<(), RobotError> {
            let lease = self
                .probe
                .begin_leased(BusOpClass::NormalEffectful)
                .map_err(RobotError::Aborted)?;
            self.on = on;
            self.enable_calls += 1;
            self.probe.finish_leased(lease);
            let _ = self.ctrl.authority().disable();
            // 真实撤销后的第二笔申请拒绝；第一笔模拟设备已经上扭。
            let lease = self
                .probe
                .begin_leased(BusOpClass::NormalEffectful)
                .map_err(RobotError::Aborted)?;
            self.probe.finish_leased(lease);
            Ok(())
        }
        fn safety_stop(&mut self) -> Result<(), RobotError> {
            self.stops += 1;
            self.on = false;
            Ok(())
        }
        fn limits(&self) -> &JointLimits {
            &self.limits
        }
    }
    impl OutputSafetyProbe for NoAllocAdapter {
        fn joint_count(&self) -> usize {
            2
        }
        fn read_torque_enable_mask(&mut self, out: &mut [bool]) -> Result<(), RobotError> {
            self.confirms += 1;
            out.fill(self.on);
            Ok(())
        }
    }
    let mut f = TickFixture::healthy();
    let seq = f.submit(AdapterNormalReq::Enable(true));
    let mut adapter = NoAllocAdapter {
        probe: f.probe.clone(),
        ctrl: f.ctrl.clone(),
        limits: JointLimits::default(),
        on: false,
        enable_calls: 0,
        stops: 0,
        confirms: 0,
    };
    let context = SerialTickContext {
        ctrl: &f.ctrl,
        probe: &f.probe,
        diag: &f.diag,
        clock: f.clock.as_ref(),
        metrics: &f.metrics,
    };
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..3 {
            let _ = f.driver.tick(
                &mut adapter,
                &f.command.as_view(),
                &context,
                MonoRawNs(f.clock.now().0 + 5_000_000),
            );
            f.clock.advance(DurationNs::from_millis(5));
        }
    });
    assert_eq!(
        (adapter.enable_calls, adapter.stops, adapter.confirms),
        (1, 1, 1)
    );
    assert!(!adapter.on);
    assert_eq!(f.ctrl.safety_state(), OutputSafetyState::TorqueOffConfirmed);
    assert_eq!(
        HardRtCtrlClient::new(f.ctrl.clone()).poll_last_normal_resp(seq),
        Some(AdapterResp::Err(AdapterErr::AuthorityRevoked))
    );
}

// SIGABRT 的实际判据只在 Unix 验收；不把其它平台的任意失败当分配器证明。
#[cfg(all(feature = "rt_guard", unix))]
#[test]
fn serial_compensation_allocator_negative_control_really_rejects_alloc() {
    use std::os::unix::process::ExitStatusExt;
    const CHILD: &str = "ROBOT_W3_SERIAL_ALLOC_NEGATIVE";
    if std::env::var_os(CHILD).is_some() {
        eprintln!("W3_SERIAL_ALLOC_NEGATIVE_ENTERED");
        assert_no_alloc::assert_no_alloc(|| {
            let value = std::hint::black_box(Vec::<u8>::with_capacity(std::hint::black_box(1024)));
            std::hint::black_box(value);
        });
        return;
    }
    let logs = tempfile::tempdir().unwrap();
    let path = logs.path().join("negative.log");
    let log = std::fs::File::create(&path).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "post_e_convergence_acceptance::serial_compensation_allocator_negative_control_really_rejects_alloc", "--nocapture"])
        .env(CHILD, "1").stdout(log.try_clone().unwrap()).stderr(log).spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            result => {
                let _ = child.kill();
                let reap = std::time::Instant::now() + std::time::Duration::from_secs(2);
                loop {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        break;
                    }
                    if std::time::Instant::now() >= reap {
                        std::process::abort();
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                panic!("负对照子进程异常，已真实 kill/reap：{result:?}");
            }
        }
    };
    let raw = std::fs::read_to_string(path).unwrap();
    assert!(
        raw.contains("W3_SERIAL_ALLOC_NEGATIVE_ENTERED"),
        "未进入实际反证：{raw}"
    );
    assert_eq!(
        status.signal(),
        Some(libc::SIGABRT),
        "必须是实际 allocator 触发 SIGABRT：{raw}"
    );
    eprintln!("W3_SERIAL_ALLOC_NEGATIVE_REJECTED");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "同一原 RR 的实际执行、故障锁存、物理退休及原 Slot 归还必须贯穿三个真实设备结果分支"
)]
fn serial_fault_retains_original_rr_and_slot_until_trusted_physical_retirement() {
    use crate::ipc_pending::{
        BusinessObservation, EnableStartOutcome, IpcPending, PendingOutcome, RevokeCause,
        RrIdentity, advance_owned, observe_business, revoke_enable, start_enable_owned,
    };
    use crate::ipc_thread::SetEnabledRrPayload;
    use crate::rr_admission::{CheckedLimits, PeerLedger, RrRequestClass};
    use crate::state_machine::S0State;
    use robot_core::control_authority::{Phase, Refusal, SafetyObservationError, SafetyOperation};

    #[derive(Clone, Copy)]
    enum Case {
        TrustedAdapterError,
        UntrustedCommFault,
        StopFailed,
    }
    fn drive_serial(f: &mut TickFixture) {
        let _ = f.driver.tick(
            &mut f.adapter,
            &f.command.as_view(),
            &SerialTickContext {
                ctrl: &f.ctrl,
                probe: &f.probe,
                diag: &f.diag,
                clock: f.clock.as_ref(),
                metrics: &f.metrics,
            },
            MonoRawNs(f.clock.now().0 + 5_000_000),
        );
        f.clock.advance(DurationNs::from_millis(5));
    }
    fn observe_actual_fault(f: &mut TickFixture) {
        // 来自本次真实 Serial 路由发布的 mailbox；不在夹具中制造 Fault 快照。
        let fault = f.ctrl.mailbox_snapshot().expect("真实设备故障必须有原公告");
        let observed = BusinessObservation {
            now: f.clock.now(),
            state: S0State::Fault(fault.code),
            estop: f.ctrl.authority().snapshot().latch,
        };
        let pending = std::mem::replace(&mut f.pending, IpcPending::Idle);
        let pending = observe_business(pending, observed);
        f.pending = match pending {
            IpcPending::Enable(_) => revoke_enable(pending, RevokeCause::Fault, observed)
                .expect("仅同原 Enable 记录已实际发布的故障撤销"),
            original => original,
        };
        f.ctrl.ack_fault(fault.generation);
    }

    for case in [
        Case::TrustedAdapterError,
        Case::UntrustedCommFault,
        Case::StopFailed,
    ] {
        let mut f = TickFixture::healthy();
        let limits = CheckedLimits::try_from_limits(&robot_transport::iceoryx2::RrStaticLimits {
            c: 1,
            l: 4,
        })
        .unwrap();
        let mut ledger = PeerLedger::new(&limits).unwrap();
        let mut slot = Some(ledger.admit(1_u8, RrRequestClass::Restore).unwrap());
        let route = slot.as_ref().unwrap().identity();
        let request_id = "原串口故障请求";
        let client = HardRtCtrlClient::new(f.ctrl.clone());
        let start = start_enable_owned(
            IpcPending::Idle,
            Some(RrIdentity {
                request_id: request_id.into(),
                deadline_ns: f.clock.now().0 + 1_000_000_000,
                route,
            }),
            &client,
            f.clock.now(),
        );
        f.pending = match start {
            EnableStartOutcome::Advanced(step) => step.pending,
            other => panic!("真实 owner/公告/原请求应提交：{other:?}"),
        };
        let (domain, epoch, seq) = match &f.pending {
            IpcPending::Enable(enable) => (
                enable.owner().domain(),
                enable.view_epoch(),
                enable.seq().unwrap(),
            ),
            _ => unreachable!(),
        };
        let error = match case {
            Case::UntrustedCommFault => RobotError::Comm(CommErrorKind::Timeout),
            Case::TrustedAdapterError | Case::StopFailed => {
                RobotError::HardwareFault("真实设备调用失败")
            }
        };
        f.adapter
            .script
            .push_back(Reply::error(Io::Enable(true), error));
        match case {
            Case::StopFailed => f.adapter.script.extend(std::iter::repeat_n(
                Reply::error(Io::Safety, RobotError::HardwareFault("真实 Stop 失败")),
                3,
            )),
            Case::TrustedAdapterError | Case::UntrustedCommFault => {
                f.adapter
                    .script
                    .extend([Io::Safety, Io::Confirm].map(Reply::ok));
            }
        }
        drive_serial(&mut f);
        assert!(matches!(
            client.poll_last_normal_resp(seq),
            Some(AdapterResp::Err(_))
        ));
        assert_eq!(f.metrics.normal_completed_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            f.ctrl.authority().snapshot().phase,
            Some(Phase::EnableFailed)
        );
        observe_actual_fault(&mut f);
        let first = match &f.pending {
            IpcPending::Enable(enable) => {
                assert_eq!(enable.seq(), Some(seq));
                let rr = enable.rr().unwrap();
                assert_eq!(rr.identity().route, route);
                assert_eq!(rr.identity().request_id, request_id);
                assert!(matches!(
                    rr.locked().unwrap().outcome(),
                    PendingOutcome::Existing(SetEnabledRrPayload::AdapterError)
                ));
                rr.locked().unwrap().at()
            }
            _ => unreachable!(),
        };
        let pending = std::mem::replace(&mut f.pending, IpcPending::Idle);
        let step = advance_owned(pending, &client, f.clock.as_ref()).unwrap();
        assert!(step.replies.is_empty());
        assert!(
            step.failure.is_none(),
            "原 mailbox 已接管，不重报第二次失败事实"
        );
        f.pending = step.pending;
        assert_eq!(f.ctrl.authority().snapshot().phase, Some(Phase::Idle));
        let mut replies = Vec::new();
        for _ in 0..4 {
            if let IpcPending::Lower(lower) = &f.pending {
                let carried = lower.carried_enable().expect("原 owner 退后仍持原 RR");
                assert_eq!((carried.domain(), carried.view_epoch()), (domain, epoch));
                assert_eq!(carried.source_seq(), None, "原 seq 已从真实完成槽消费");
                let rr = carried.rr().unwrap();
                assert_eq!(
                    (rr.identity().request_id.as_str(), rr.identity().route),
                    (request_id, route)
                );
                assert_eq!(rr.locked().unwrap().at(), first);
                assert!(matches!(
                    rr.locked().unwrap().outcome(),
                    PendingOutcome::Existing(SetEnabledRrPayload::AdapterError)
                ));
                assert!(slot.is_some());
                assert!(
                    ledger.admit(1_u8, RrRequestClass::Restore).is_err(),
                    "持原槽时不能再次借信用"
                );
            }
            drive_serial(&mut f);
            observe_actual_fault(&mut f);
            let pending = std::mem::replace(&mut f.pending, IpcPending::Idle);
            let step = advance_owned(pending, &client, f.clock.as_ref()).unwrap();
            f.pending = step.pending;
            for reply in step.replies.into_entries() {
                assert_eq!(
                    (reply.identity.request_id.as_str(), reply.identity.route),
                    (request_id, route)
                );
                assert!(matches!(
                    reply.outcome,
                    PendingOutcome::Existing(SetEnabledRrPayload::AdapterError)
                ));
                ledger
                    .release(slot.take().expect("同原 Slot 只能归还一次"))
                    .unwrap();
                replies.push(reply);
            }
        }
        match case {
            Case::TrustedAdapterError => {
                assert_eq!(f.ctrl.safety_state(), OutputSafetyState::TorqueOffConfirmed);
                assert!(matches!(f.pending, IpcPending::Idle));
                assert_eq!(replies.len(), 1);
                assert!(slot.is_none());
                let next = ledger.admit(1_u8, RrRequestClass::Restore).unwrap();
                ledger.release(next).unwrap();
            }
            Case::UntrustedCommFault | Case::StopFailed => {
                let expected = match case {
                    Case::UntrustedCommFault => OutputSafetyState::AdapterUncertain,
                    Case::StopFailed => OutputSafetyState::StopFailed,
                    Case::TrustedAdapterError => unreachable!(),
                };
                assert_eq!(f.ctrl.safety_state(), expected);
                assert!(replies.is_empty());
                assert!(matches!(f.pending, IpcPending::Lower(_)));
                assert!(slot.is_some());
                let round = *f.driver.convergence().round().unwrap();
                let stop_writes = f.driver.convergence().counters().stop_writes;
                let expected_calls = match case {
                    Case::StopFailed => {
                        assert_eq!(stop_writes, 4, "启动一次，故障后仅三次真实 Stop");
                        vec![Io::Enable(true), Io::Safety, Io::Safety, Io::Safety]
                    }
                    Case::UntrustedCommFault => {
                        assert_eq!(stop_writes, 2);
                        vec![Io::Enable(true), Io::Safety, Io::Confirm]
                    }
                    Case::TrustedAdapterError => unreachable!(),
                };
                assert_eq!(
                    f.adapter
                        .observed
                        .iter()
                        .map(|call| call.io)
                        .collect::<Vec<_>>(),
                    expected_calls,
                    "SSF 不得 Connect/Confirm/第四次 Stop，通信故障不重开轮次"
                );
                let actual_calls = f.adapter.observed.len();
                // 这是独立 core API 拒绝对照，不是 Serial 实际发出或成功完成了新 Confirm。
                // 原负分支没有额外设备调用，不清粘滞终态、不重开原轮或任何物理预算。
                let observed = f
                    .ctrl
                    .authority()
                    .begin_safety_observation(SafetyOperation::ConfirmOff);
                assert!(
                    matches!(f.ctrl.authority().complete_safety_observation(observed),
                    Err(SafetyObservationError::Rejected(Refusal::SafetyNotRelaxable(state))) if state == expected)
                );
                assert_eq!(*f.driver.convergence().round().unwrap(), round);
                assert_eq!(f.driver.convergence().counters().stop_writes, stop_writes);
                assert_eq!(f.adapter.observed.len(), actual_calls);
                let pending = std::mem::replace(&mut f.pending, IpcPending::Idle);
                let step = advance_owned(pending, &client, f.clock.as_ref()).unwrap();
                assert!(step.replies.is_empty());
                f.pending = step.pending;
                assert!(ledger.admit(1_u8, RrRequestClass::Restore).is_err());
            }
        }
        assert_eq!(f.metrics.normal_completed_count.load(Ordering::Relaxed), 1);
        f.adapter.assert_drained();
        // 负分支的原 pending/Slot 留在本地拥有聚合到夹具末尾，未发退休回复或退款。
        let _retained_fixture = (f.pending, slot, ledger);
    }
}
