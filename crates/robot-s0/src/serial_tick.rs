//! 新串口协议的唯一单 tick 调度入口；线程与独立验收共用此路径。

use std::sync::atomic::Ordering;

use robot_core::adapter::{
    ControlMode, JointCommandView, JointStateBuf, RtSnapshot, SerialRtAdapter,
};
use robot_core::diag::DiagSink;
use robot_core::error::RobotError;
use robot_core::executor_ctrl::{
    AdapterErr, AdapterResp, OutputPolicy, OutputSafetyState, TorqueEnableMask, normal_codes,
};
use robot_core::serial_services::{AbortReason, BusOpClass, CommErrorKind, PreemptProbe};
use robot_core::time::{MonoRawNs, RobotClock};

use crate::hard_rt_ctrl::{ActionRecord, FaultOrigin, decode_serial_pending};
use crate::serial_convergence::{
    ConvergeCounters, ConvergePhase, SerialConvergence, emit, fault_for, observe_device_error,
    publish_completion, seq_low,
};
use crate::serial_io_ctrl::SerialIoCtrlInner;
use crate::serial_io_thread::SerialIoThreadMetrics;
use crate::serial_preempt::SerialPreemptProbe;
use crate::state_machine::FaultCode;

const ACTION_RETIRE_BUDGET_NS: u64 = 1_105_000_000;
const PERIODIC_READ_BUDGET_NS: u64 = 200_000_000;
const PERIODIC_WRITE_BUDGET_NS: u64 = 50_000_000;

pub(crate) struct SerialTickContext<'a> {
    pub ctrl: &'a SerialIoCtrlInner,
    pub probe: &'a SerialPreemptProbe,
    pub diag: &'a dyn DiagSink,
    pub clock: &'a dyn RobotClock,
    pub metrics: &'a SerialIoThreadMetrics,
}

pub(crate) struct SerialTick {
    convergence: SerialConvergence,
    torque_mask: TorqueEnableMask,
    state: JointStateBuf,
    sequence: u64,
    reported: ConvergeCounters,
}

impl SerialTick {
    pub(crate) fn new(authorized: bool, joint_count: usize, spawned_at: MonoRawNs) -> Self {
        Self {
            convergence: SerialConvergence::new(authorized, joint_count, spawned_at),
            torque_mask: TorqueEnableMask::default(),
            state: JointStateBuf::default(),
            sequence: 0,
            reported: ConvergeCounters::default(),
        }
    }

    #[cfg(test)]
    pub(crate) const fn convergence(&self) -> &SerialConvergence {
        &self.convergence
    }

    #[cfg(test)]
    pub(crate) fn convergence_mut(&mut self) -> &mut SerialConvergence {
        &mut self.convergence
    }

    #[cfg(test)]
    pub(crate) const fn state(&self) -> &JointStateBuf {
        &self.state
    }

    pub(crate) fn tick<A: SerialRtAdapter>(
        &mut self,
        adapter: &mut A,
        command: &JointCommandView<'_>,
        context: &SerialTickContext<'_>,
        deadline: MonoRawNs,
    ) -> RtSnapshot {
        let SerialTickContext {
            ctrl,
            probe,
            diag,
            clock,
            ..
        } = *context;
        probe.begin_tick();
        self.convergence.claim_estop(ctrl, diag, clock.now());
        self.convergence.retire_timeout(ctrl, diag, clock.now());
        if self.convergence.phase() != ConvergePhase::Idle {
            self.reject_inhibited_pending(context);
            self.convergence
                .step(adapter, ctrl, probe, diag, clock, &mut self.torque_mask);
            return self.snapshot(context, deadline);
        }
        if !self.convergence.normal_io_allowed(ctrl) {
            self.reject_inhibited_pending(context);
            return self.snapshot(context, deadline);
        }
        // 取消词只取消原动作，本身不创建急停轮次或故障；本 tick 不再接其它普通 IO。
        if self.consume_normal(adapter, context) {
            self.claim_followup(context);
            return self.snapshot(context, deadline);
        }
        self.periodic(adapter, command, context);
        self.claim_followup(context);
        self.snapshot(context, deadline)
    }

    fn snapshot(&mut self, context: &SerialTickContext<'_>, deadline: MonoRawNs) -> RtSnapshot {
        let counts = self.convergence.counters();
        for (counter, delta) in [
            (
                &context.metrics.emergency_safety_stop_count,
                counts.stop_writes - self.reported.stop_writes,
            ),
            (
                &context.metrics.confirm_off_count,
                counts.confirmed - self.reported.confirmed,
            ),
            (
                &context.metrics.confirm_uncertain_count,
                counts.uncertain - self.reported.uncertain,
            ),
            (
                &context.metrics.confirm_failed_count,
                counts.failed - self.reported.failed,
            ),
        ] {
            counter.fetch_add(delta, Ordering::Relaxed);
        }
        self.reported = counts;
        self.sequence = self.sequence.wrapping_add(1);
        if context.probe.tick_progress_allowed() {
            context
                .ctrl
                .liveness_counter
                .fetch_add(1, Ordering::Relaxed);
        }
        context.metrics.tick_count.fetch_add(1, Ordering::Relaxed);
        RtSnapshot {
            state: self.state,
            tick_seq: self.sequence,
            tick_deadline_ns: deadline.0,
        }
    }

    fn claim_followup(&mut self, context: &SerialTickContext<'_>) {
        let SerialTickContext {
            ctrl, diag, clock, ..
        } = *context;
        self.convergence.claim_estop(ctrl, diag, clock.now());
        self.convergence.retire_timeout(ctrl, diag, clock.now());
    }

    fn reject_inhibited_pending(&mut self, context: &SerialTickContext<'_>) {
        let raw = context.ctrl.pending_normal.swap(0, Ordering::AcqRel);
        match decode_serial_pending(raw) {
            Ok(Some(pending)) => {
                let error = if pending.cancelled {
                    AdapterErr::PreemptedByEmergency
                } else {
                    match context.ctrl.mailbox_snapshot() {
                        Some(mail) if mail.code == FaultCode::CommFault => {
                            AdapterErr::Comm(CommErrorKind::from_u8(mail.detail))
                        }
                        _ => AdapterErr::HardwareFault,
                    }
                };
                publish_completion(context.ctrl, pending.action, AdapterResp::Err(error));
                context
                    .metrics
                    .normal_completed_count
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => self.invalid_pending(raw, context),
            Ok(None) => {}
        }
    }

    fn invalid_pending(&mut self, raw: u64, context: &SerialTickContext<'_>) {
        emit(context.diag, context.clock.now(), 3, 3, 0, seq_low(raw));
        self.convergence.enter_fault(
            FaultCode::CommFault,
            CommErrorKind::Unknown.to_u8(),
            FaultOrigin::InvariantViolation,
            context.ctrl,
            context.diag,
            context.clock.now(),
        );
    }

    /// true 表示本 tick 不能再开始普通周期调用；正常成功可继续同一 tick 的采样。
    fn consume_normal<A: SerialRtAdapter>(
        &mut self,
        adapter: &mut A,
        context: &SerialTickContext<'_>,
    ) -> bool {
        let SerialTickContext {
            ctrl,
            probe,
            diag,
            clock,
            metrics,
        } = *context;
        let raw = ctrl.pending_normal.swap(0, Ordering::AcqRel);
        let pending = match decode_serial_pending(raw) {
            Ok(Some(pending)) => pending,
            Ok(None) => return false,
            Err(_) => {
                self.invalid_pending(raw, context);
                return true;
            }
        };
        let record = pending.action;
        if pending.cancelled {
            publish_completion(
                ctrl,
                record,
                AdapterResp::Err(AdapterErr::PreemptedByEmergency),
            );
            metrics
                .emergency_preempted_count
                .fetch_add(1, Ordering::Relaxed);
            self.convergence.retire_timeout(ctrl, diag, clock.now());
            return true;
        }
        // 读取取消请求后才发布 active；在途请求由 adapter 的真实事务边界检查。
        let abort = ctrl.abort_request.load(Ordering::Acquire);
        ctrl.active_action.store(record.pack(), Ordering::Relaxed);
        if abort != 0 {
            self.convergence.retire_timeout(ctrl, diag, clock.now());
            if ctrl.active_action.load(Ordering::Relaxed) != 0 {
                publish_completion(ctrl, record, AdapterResp::Err(AdapterErr::AbortedByTimeout));
            }
            return true;
        }
        probe.set_context(
            BusOpClass::NormalEffectful,
            record.seq,
            MonoRawNs(clock.now().0.saturating_add(ACTION_RETIRE_BUDGET_NS)),
            self.convergence.evidence_trusted(ctrl),
        );
        let response = match probe.should_abort(BusOpClass::NormalEffectful) {
            Some(AbortReason::Emergency) => AdapterResp::Err(AdapterErr::PreemptedByEmergency),
            Some(AbortReason::Timeout) => AdapterResp::Err(AdapterErr::AbortedByTimeout),
            None if ctrl.safety_latched.load(Ordering::Acquire)
                && record.action == normal_codes::ENABLE
                && record.arg != 0 =>
            {
                metrics.latch_rejected_count.fetch_add(1, Ordering::Relaxed);
                AdapterResp::Err(AdapterErr::SafetyLatched)
            }
            None => execute_serial_normal(adapter, record, ctrl),
        };
        publish_completion(ctrl, record, response);
        metrics
            .normal_completed_count
            .fetch_add(1, Ordering::Relaxed);
        self.route_normal(response, context);
        self.convergence.claim_estop(ctrl, diag, clock.now());
        self.convergence.retire_timeout(ctrl, diag, clock.now());
        !self.convergence.normal_io_allowed(ctrl)
    }

    fn route_normal(&mut self, response: AdapterResp, context: &SerialTickContext<'_>) {
        if response == AdapterResp::Err(AdapterErr::HardwareFault) {
            observe_device_error(RobotError::HardwareFault("普通设备错误"), context.ctrl);
        }
        let (code, detail, origin) = match response {
            AdapterResp::Ok | AdapterResp::Err(AdapterErr::PreemptedByEmergency) => return,
            AdapterResp::Err(AdapterErr::AbortedByTimeout) => (
                FaultCode::CommFault,
                CommErrorKind::PendingDeadline.to_u8(),
                FaultOrigin::PendingTimeout,
            ),
            AdapterResp::Err(AdapterErr::Comm(kind)) => (
                FaultCode::CommFault,
                kind.to_u8(),
                FaultOrigin::NormalAction,
            ),
            AdapterResp::Err(_) => (FaultCode::AdapterErr, 0, FaultOrigin::NormalAction),
        };
        self.convergence.enter_fault(
            code,
            detail,
            origin,
            context.ctrl,
            context.diag,
            context.clock.now(),
        );
    }

    fn periodic<A: SerialRtAdapter>(
        &mut self,
        adapter: &mut A,
        command: &JointCommandView<'_>,
        context: &SerialTickContext<'_>,
    ) {
        let SerialTickContext {
            ctrl,
            probe,
            diag,
            clock,
            metrics,
        } = *context;
        let policy = OutputPolicy::from_u8(ctrl.policy.load(Ordering::Acquire));
        if matches!(
            policy,
            OutputPolicy::ActiveForward
                | OutputPolicy::ReadyHold
                | OutputPolicy::HoldCurrent
                | OutputPolicy::ZeroCommand
        ) {
            probe.set_context(
                BusOpClass::PeriodicWrite,
                0,
                MonoRawNs(clock.now().0.saturating_add(PERIODIC_WRITE_BUDGET_NS)),
                self.convergence.evidence_trusted(ctrl),
            );
            if let Err(error) = adapter.write_command(command) {
                metrics.write_errors.fetch_add(1, Ordering::Relaxed);
                self.route_periodic(error, context);
                return;
            }
            self.convergence.claim_estop(ctrl, diag, clock.now());
            self.convergence.retire_timeout(ctrl, diag, clock.now());
            if !self.convergence.normal_io_allowed(ctrl) {
                return;
            }
        }
        let started = clock.now();
        let mut candidate = self.state;
        probe.set_context(
            BusOpClass::PeriodicRead,
            0,
            MonoRawNs(started.0.saturating_add(PERIODIC_READ_BUDGET_NS)),
            self.convergence.evidence_trusted(ctrl),
        );
        match adapter.read_state(&mut candidate) {
            Ok(()) => {
                candidate.stamp_ns = started.0;
                self.state = candidate;
                if ctrl.comm_level.swap(0, Ordering::AcqRel) != 0
                    || ctrl.l0_since_ns.load(Ordering::Relaxed) == 0
                {
                    ctrl.l0_since_ns.store(clock.now().0, Ordering::Release);
                }
            }
            Err(error) => {
                metrics.read_errors.fetch_add(1, Ordering::Relaxed);
                self.route_periodic(error, context);
            }
        }
    }

    fn route_periodic(&mut self, error: RobotError, context: &SerialTickContext<'_>) {
        observe_device_error(error, context.ctrl);
        match error {
            RobotError::Aborted(AbortReason::Emergency) => {
                self.convergence
                    .claim_estop(context.ctrl, context.diag, context.clock.now());
            }
            RobotError::Aborted(AbortReason::Timeout) => {
                // 周期事务不匹配 timeout 请求；损坏实现仍保守收敛，不以 unreachable panic。
                self.invalid_pending(0, context);
            }
            _ => {
                let (code, detail) = fault_for(error);
                self.convergence.enter_fault(
                    code,
                    detail,
                    FaultOrigin::Periodic,
                    context.ctrl,
                    context.diag,
                    context.clock.now(),
                );
            }
        }
    }
}

fn execute_serial_normal<A: SerialRtAdapter>(
    adapter: &mut A,
    record: ActionRecord,
    ctrl: &SerialIoCtrlInner,
) -> AdapterResp {
    let result = match record.action {
        normal_codes::ENABLE => adapter.enable(record.arg != 0),
        normal_codes::SET_MODE => {
            let mode = match record.arg {
                0 => ControlMode::Stop,
                1 => ControlMode::Position,
                2 => ControlMode::Velocity,
                3 => ControlMode::Torque,
                4 => ControlMode::ServoPosition,
                _ => return AdapterResp::Err(AdapterErr::Unsupported),
            };
            adapter.set_mode(mode)
        }
        normal_codes::CONNECT => adapter.connect(),
        normal_codes::DISCONNECT => adapter.disconnect(),
        // trait 保持冻结；既有不支持的 HoldCurrent 仍按 effectful 失败完整收敛。
        _ => return AdapterResp::Err(AdapterErr::Unsupported),
    };
    match result {
        Ok(()) => {
            if record.action == normal_codes::ENABLE {
                let state = match (record.arg != 0, ctrl.rx_trust.load(Ordering::Acquire)) {
                    (true, _) => OutputSafetyState::TorqueOn,
                    (false, true) => OutputSafetyState::TorqueOffConfirmed,
                    (false, false) => OutputSafetyState::AdapterUncertain,
                };
                ctrl.set_serial_normal_safety(state);
            }
            AdapterResp::Ok
        }
        Err(error) => AdapterResp::Err(match error {
            RobotError::Aborted(AbortReason::Emergency) => AdapterErr::PreemptedByEmergency,
            RobotError::Aborted(AbortReason::Timeout) => AdapterErr::AbortedByTimeout,
            RobotError::Comm(kind) => AdapterErr::Comm(kind),
            RobotError::DeadlineExceeded => AdapterErr::Comm(CommErrorKind::Timeout),
            RobotError::NotConnected => AdapterErr::Comm(CommErrorKind::Disconnected),
            RobotError::Unsupported(_) | RobotError::InvalidParameter(_) => AdapterErr::Unsupported,
            _ => AdapterErr::HardwareFault,
        }),
    }
}
