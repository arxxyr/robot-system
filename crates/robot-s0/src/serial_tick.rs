//! 新串口协议的唯一单 tick 调度入口；线程与独立验收共用此路径。

use std::sync::atomic::Ordering;

use robot_core::adapter::{
    ControlMode, JointCommandView, JointSelection, JointStateBuf, MAX_JOINTS, RtSnapshot,
    SerialRtAdapter,
};
use robot_core::diag::DiagSink;
use robot_core::error::RobotError;
use robot_core::executor_ctrl::{
    AdapterErr, AdapterResp, OutputPolicy, OutputSafetyState, TorqueEnableMask, normal_codes,
};
use robot_core::serial_services::{AbortReason, BusOpClass, CommErrorKind, PreemptProbe};
use robot_core::time::{MonoRawNs, RobotClock};

use crate::hard_rt_ctrl::{
    ActionRecord, EnableCompensation, EnableFinishPending, FaultOrigin, decode_serial_pending,
    finish_enable_owned, start_enable_execution,
};
use crate::serial_convergence::{
    ConvergeCounters, ConvergePhase, SerialConvergence, emit, fault_for, observe_device_error,
    publish_completion, seq_low,
};
use crate::serial_io_ctrl::SerialIoCtrlInner;
use crate::serial_io_thread::SerialIoThreadMetrics;
use crate::serial_preempt::{InstalledIntent, SerialPreemptProbe};
use crate::state_machine::FaultCode;
use robot_core::control_authority::{EnableResult, SafetyOperation};

const ACTION_RETIRE_BUDGET_NS: u64 = 1_105_000_000;
const PERIODIC_READ_BUDGET_NS: u64 = 200_000_000;
const PERIODIC_WRITE_BUDGET_NS: u64 = 50_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PeriodicOutput {
    Skip,
    Forward,
    VelocityZero {
        joint_count: usize,
        stamp_ns: u64,
        deadline_ns: u64,
    },
    MixedHold {
        joint_count: usize,
        stamp_ns: u64,
        deadline_ns: u64,
    },
}

fn velocity_joint_count(command: &JointCommandView<'_>) -> Result<usize, RobotError> {
    let count = command.velocities.len();
    if !(1..=MAX_JOINTS).contains(&count)
        || command.positions.len() != count
        || command.torques.len() != count
    {
        return Err(RobotError::InvalidParameter("速度目标维度无效"));
    }
    Ok(count)
}

/// 速度及内部部分位置目标受来源时间约束；全选其它模式保留原策略合同。
fn select_periodic_output(
    command: &JointCommandView<'_>,
    policy: OutputPolicy,
    now: MonoRawNs,
) -> Result<PeriodicOutput, RobotError> {
    if matches!(
        command.selection,
        JointSelection::MixedPositionRawVelocity { .. }
    ) {
        return match policy {
            OutputPolicy::StopTorque | OutputPolicy::Disabled => Ok(PeriodicOutput::Skip),
            OutputPolicy::ActiveForward => {
                command.validate_selection(command.positions.len())?;
                command.validate_selection_time(now.0)?;
                Ok(PeriodicOutput::Forward)
            }
            OutputPolicy::ReadyHold | OutputPolicy::HoldCurrent | OutputPolicy::ZeroCommand => {
                command.validate_selection(command.positions.len())?;
                Ok(PeriodicOutput::MixedHold {
                    joint_count: command.positions.len(),
                    stamp_ns: now.0,
                    deadline_ns: now
                        .0
                        .checked_add(PERIODIC_WRITE_BUDGET_NS)
                        .ok_or(RobotError::InvalidParameter("混合保持截止时间溢出"))?,
                })
            }
        };
    }
    if !matches!(policy, OutputPolicy::StopTorque | OutputPolicy::Disabled) {
        command.validate_selection(command.positions.len())?;
        command.validate_selection_time(now.0)?;
    }
    match (command.mode, policy) {
        (_, OutputPolicy::StopTorque | OutputPolicy::Disabled)
        | (ControlMode::Velocity, OutputPolicy::ReadyHold) => Ok(PeriodicOutput::Skip),
        (ControlMode::Velocity, OutputPolicy::ActiveForward) => {
            velocity_joint_count(command)?;
            if command.velocities.iter().any(|value| !value.is_finite()) {
                return Err(RobotError::InvalidParameter("速度目标必须有限"));
            }
            if command.stamp_ns > now.0 {
                return Err(RobotError::InvalidParameter("速度目标时间戳来自未来"));
            }
            if now.0 >= command.deadline_ns {
                return Err(RobotError::DeadlineExceeded);
            }
            Ok(PeriodicOutput::Forward)
        }
        (ControlMode::Velocity, OutputPolicy::HoldCurrent | OutputPolicy::ZeroCommand) => {
            let joint_count = velocity_joint_count(command)?;
            let deadline_ns = now
                .0
                .checked_add(PERIODIC_WRITE_BUDGET_NS)
                .ok_or(RobotError::InvalidParameter("速度归零截止时间溢出"))?;
            Ok(PeriodicOutput::VelocityZero {
                joint_count,
                stamp_ns: now.0,
                deadline_ns,
            })
        }
        _ => Ok(PeriodicOutput::Forward),
    }
}

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
    active_enable: Option<EnableFinishPending>,
}

impl SerialTick {
    /// 正常线程退出按值移交原 Pending；收敛机其余字段仅为复制观测事实。
    pub(crate) fn into_exit(self) -> crate::hard_rt_ctrl::ExecutorExit {
        crate::hard_rt_ctrl::ExecutorExit::from_pending(self.active_enable)
    }

    pub(crate) fn new(authorized: bool, joint_count: usize, spawned_at: MonoRawNs) -> Self {
        Self {
            convergence: SerialConvergence::new(authorized, joint_count, spawned_at),
            torque_mask: TorqueEnableMask::default(),
            state: JointStateBuf::default(),
            sequence: 0,
            reported: ConvergeCounters::default(),
            active_enable: None,
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
        self.tick_impl(adapter, command, context, deadline, || {}, || {})
    }

    #[cfg(test)]
    pub(crate) fn tick_after_normal_completion<A: SerialRtAdapter>(
        &mut self,
        adapter: &mut A,
        command: &JointCommandView<'_>,
        context: &SerialTickContext<'_>,
        deadline: MonoRawNs,
        after_completion: impl FnOnce(),
    ) -> RtSnapshot {
        self.tick_impl(adapter, command, context, deadline, || {}, after_completion)
    }

    #[cfg(test)]
    pub(crate) fn tick_before_normal_completion<A: SerialRtAdapter>(
        &mut self,
        adapter: &mut A,
        command: &JointCommandView<'_>,
        context: &SerialTickContext<'_>,
        deadline: MonoRawNs,
        before_completion: impl FnOnce(),
    ) -> RtSnapshot {
        self.tick_impl(
            adapter,
            command,
            context,
            deadline,
            before_completion,
            || {},
        )
    }

    fn tick_impl<A: SerialRtAdapter>(
        &mut self,
        adapter: &mut A,
        command: &JointCommandView<'_>,
        context: &SerialTickContext<'_>,
        deadline: MonoRawNs,
        before_completion: impl FnOnce(),
        after_completion: impl FnOnce(),
    ) -> RtSnapshot {
        let SerialTickContext {
            ctrl,
            probe,
            diag,
            clock,
            ..
        } = *context;
        probe.begin_tick();
        // 先在真实无事务边界办理采样请求；不得被 Fault/SSF 的早退饿死。
        #[cfg(feature = "test-comm-fault-hook")]
        let sample_gate = crate::comm_fault_trace::serial_service_boundary(ctrl, clock.now().0);
        self.claim_followup(context);
        if let Some(active) = self.active_enable.take() {
            self.active_enable = self.finish_enable(active, context);
            if self.active_enable.is_none() {
                context
                    .metrics
                    .normal_completed_count
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
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
        if self.active_enable.is_some() {
            return self.snapshot(context, deadline);
        }
        // 取消词只取消原动作，本身不创建急停轮次或故障；本 tick 不再接其它普通 IO。
        if self.consume_normal(adapter, context, before_completion, after_completion) {
            self.claim_followup(context);
            return self.snapshot(context, deadline);
        }
        #[cfg(feature = "test-comm-fault-hook")]
        if sample_gate {
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
        let estop = self
            .convergence
            .claim_estop_arranged(ctrl, diag, clock.now());
        let timeout = self
            .convergence
            .retire_timeout_arranged(ctrl, diag, clock.now());
        if let Some(active) = &mut self.active_enable {
            for arranged in [estop, timeout].into_iter().flatten() {
                active.record_serial_recovery(arranged);
            }
        }
    }

    fn finish_enable(
        &mut self,
        active: EnableFinishPending,
        context: &SerialTickContext<'_>,
    ) -> Option<EnableFinishPending> {
        match finish_enable_owned(active, context.ctrl) {
            Ok(finished) => {
                finished.publish_after_compensation(context.ctrl, |_, compensation| {
                    if compensation == EnableCompensation::Required {
                        self.convergence.ensure_enable_compensation(
                            context.ctrl,
                            context.diag,
                            context.clock.now(),
                        );
                    }
                });
                None
            }
            Err(active) => Some(active),
        }
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
    #[allow(
        clippy::too_many_lines,
        reason = "真实取请求、调用 adapter、finish 重试与原 seq 发布保持同一拥有链"
    )]
    fn consume_normal<A: SerialRtAdapter>(
        &mut self,
        adapter: &mut A,
        context: &SerialTickContext<'_>,
        before_completion: impl FnOnce(),
        after_completion: impl FnOnce(),
    ) -> bool {
        let SerialTickContext {
            ctrl,
            probe,
            diag,
            clock,
            metrics,
        } = *context;
        #[cfg(feature = "test-comm-fault-hook")]
        let raw = {
            let gated = crate::comm_fault_trace::sample_gate_active();
            if gated {
                let observed = ctrl.pending_normal.load(Ordering::Acquire);
                if matches!(decode_serial_pending(observed), Ok(Some(pending)) if !pending.cancelled)
                    && ctrl.abort_request.load(Ordering::Acquire) == 0
                {
                    return false;
                }
                // 判断后仅取得观察到的退休项；并发新 normal 留到下一 tick。
                match ctrl.pending_normal.compare_exchange(
                    observed,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(raw) => raw,
                    Err(_) => return false,
                }
            } else {
                ctrl.pending_normal.swap(0, Ordering::AcqRel)
            }
        };
        #[cfg(not(feature = "test-comm-fault-hook"))]
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
        let mut execution = None;
        let start_error = if record.action == normal_codes::ENABLE && record.arg == 1 {
            match start_enable_execution(ctrl) {
                Ok(token) => {
                    let installed = ctrl.execution_slot().install(token.view());
                    execution = Some(token);
                    installed.err().map(|_| AdapterErr::AuthorityRevoked)
                }
                Err(error) => Some(error),
            }
        } else {
            None
        };
        let intent = match (record.action, record.arg) {
            (normal_codes::ENABLE, 1) => InstalledIntent::EnableStep,
            (normal_codes::ENABLE, 0) => InstalledIntent::Disable,
            (normal_codes::CONNECT, _) => InstalledIntent::Connect,
            (normal_codes::DISCONNECT, _) => InstalledIntent::Disconnect,
            (normal_codes::SET_MODE, _) => InstalledIntent::Motion,
            _ => InstalledIntent::None,
        };
        probe.set_context(
            BusOpClass::NormalEffectful,
            intent,
            record.seq,
            MonoRawNs(clock.now().0.saturating_add(ACTION_RETIRE_BUDGET_NS)),
            self.convergence.evidence_trusted(ctrl),
        );
        let mut adapter_called = false;
        let response = match start_error {
            Some(error) => AdapterResp::Err(error),
            None => match probe.should_abort(BusOpClass::NormalEffectful) {
                Some(AbortReason::Emergency) => AdapterResp::Err(AdapterErr::PreemptedByEmergency),
                Some(AbortReason::Timeout) => AdapterResp::Err(AdapterErr::AbortedByTimeout),
                Some(AbortReason::Revoked) => AdapterResp::Err(AdapterErr::AuthorityRevoked),
                None => {
                    adapter_called = true;
                    execute_serial_normal(adapter, record, ctrl)
                }
            },
        };
        if execution.is_some() {
            ctrl.execution_slot().clear();
        }
        // 错误责任先登记：IPC Acquire 看见完成时，不得再从旧健康 guard 清回 Ready。
        let recovery = self.route_normal(response, record.seq, context);
        before_completion();
        match execution {
            Some(execution) => {
                let mut active = EnableFinishPending::new(
                    execution,
                    record,
                    match response {
                        AdapterResp::Ok => EnableResult::Succeeded,
                        AdapterResp::Err(_) => EnableResult::Failed,
                    },
                    response,
                    adapter_called,
                );
                if let Some(recovery) = recovery {
                    active.record_serial_recovery(recovery);
                }
                self.active_enable = self.finish_enable(active, context);
            }
            None => publish_completion(ctrl, record, response),
        }
        if self.active_enable.is_none() {
            after_completion();
            metrics
                .normal_completed_count
                .fetch_add(1, Ordering::Relaxed);
        }
        self.claim_followup(context);
        self.active_enable.is_some() || !self.convergence.normal_io_allowed(ctrl)
    }

    fn route_normal(
        &mut self,
        response: AdapterResp,
        seq: u64,
        context: &SerialTickContext<'_>,
    ) -> Option<crate::serial_convergence::EnableRecoveryArranged> {
        if response == AdapterResp::Err(AdapterErr::HardwareFault) {
            observe_device_error(RobotError::HardwareFault("普通设备错误"), context.ctrl);
        }
        let (code, detail, origin) = match response {
            AdapterResp::Ok
            | AdapterResp::Err(AdapterErr::PreemptedByEmergency | AdapterErr::AuthorityRevoked) => {
                return None;
            }
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
        self.convergence.enter_fault_arranged(
            code,
            detail,
            origin,
            context.ctrl,
            context.diag,
            context.clock.now(),
            seq,
        )
    }

    // 保留单一目标分派、预算安装和失败收敛路径，不为行数阈值拆成间接调度。
    #[allow(clippy::too_many_lines)]
    fn periodic_write<A: SerialRtAdapter>(
        &mut self,
        adapter: &mut A,
        command: &JointCommandView<'_>,
        context: &SerialTickContext<'_>,
    ) -> bool {
        let SerialTickContext {
            ctrl,
            probe,
            diag,
            clock,
            metrics,
        } = *context;
        let policy = OutputPolicy::from_u8(ctrl.policy.load(Ordering::Acquire));
        let zeros = [0.0_f32; MAX_JOINTS];
        let raw_zeros = [0_i16; MAX_JOINTS];
        // 同一分派时刻同时约束来源有效性与本次事务预算，不用第二次取时延长窗口。
        let dispatched_at = clock.now();
        let selection = if command.selection == JointSelection::All
            || matches!(policy, OutputPolicy::StopTorque | OutputPolicy::Disabled)
        {
            Ok(())
        } else {
            command.validate_selection(adapter.joint_count())
        };
        let output =
            match selection.and_then(|()| select_periodic_output(command, policy, dispatched_at)) {
                Ok(PeriodicOutput::Skip) => return true,
                Ok(PeriodicOutput::Forward) => *command,
                Ok(PeriodicOutput::VelocityZero {
                    joint_count,
                    stamp_ns,
                    deadline_ns,
                }) => JointCommandView {
                    positions: &zeros[..joint_count],
                    velocities: &zeros[..joint_count],
                    torques: &zeros[..joint_count],
                    raw_velocities: &raw_zeros[..joint_count],
                    mode: ControlMode::Velocity,
                    selection: JointSelection::All,
                    stamp_ns,
                    deadline_ns,
                },
                Ok(PeriodicOutput::MixedHold {
                    joint_count,
                    stamp_ns,
                    deadline_ns,
                }) => {
                    let state = &self.state;
                    let bounded = usize::from(state.joint_count) == joint_count
                        && state.stamp_ns <= dispatched_at.0
                        && dispatched_at.0 - state.stamp_ns <= PERIODIC_READ_BUDGET_NS
                        && match command.selection {
                            JointSelection::MixedPositionRawVelocity { position_mask, .. } => {
                                state.positions[..joint_count].iter().enumerate().all(
                                    |(slot, value)| {
                                        !JointSelection::Mask(position_mask).contains(slot)
                                            || value.is_finite()
                                    },
                                )
                            }
                            _ => false,
                        };
                    if !bounded {
                        metrics.write_errors.fetch_add(1, Ordering::Relaxed);
                        self.route_periodic(
                            RobotError::InvalidParameter("混合保持缺少有界成功反馈"),
                            context,
                        );
                        return false;
                    }
                    JointCommandView {
                        positions: &state.positions[..joint_count],
                        velocities: &zeros[..joint_count],
                        torques: &zeros[..joint_count],
                        raw_velocities: &raw_zeros[..joint_count],
                        mode: ControlMode::Position,
                        selection: command.selection,
                        stamp_ns,
                        deadline_ns,
                    }
                }
                Err(error) => {
                    metrics.write_errors.fetch_add(1, Ordering::Relaxed);
                    self.route_periodic(error, context);
                    return false;
                }
            };
        // 零目标只借用本地栈数组，不改来源命令或 latest slot；仍申请原 Motion 租约。
        let transaction_deadline = dispatched_at.0.saturating_add(PERIODIC_WRITE_BUDGET_NS);
        let write_deadline = match output.mode {
            ControlMode::Velocity => output.deadline_ns.min(transaction_deadline),
            ControlMode::Position if output.selection != JointSelection::All => {
                output.deadline_ns.min(transaction_deadline)
            }
            _ => transaction_deadline,
        };
        // probe 截止时间约束 checkpoint/liveness；此处不宣称串口 wire 已有硬拒写期限。
        probe.set_context(
            BusOpClass::PeriodicWrite,
            InstalledIntent::Motion,
            0,
            MonoRawNs(write_deadline),
            self.convergence.evidence_trusted(ctrl),
        );
        if let Err(error) = adapter.write_command(&output) {
            metrics.write_errors.fetch_add(1, Ordering::Relaxed);
            self.route_periodic(error, context);
            if error != RobotError::Aborted(AbortReason::Revoked) {
                return false;
            }
            // Motion 被撤权不等于禁止采样。先重检并发急停、超时与收敛门，
            // 再由后续 PeriodicRead 独立申请真实租约；不复用被拒的写权限。
        }
        self.convergence.claim_estop(ctrl, diag, clock.now());
        self.convergence.retire_timeout(ctrl, diag, clock.now());
        self.convergence.normal_io_allowed(ctrl)
    }

    fn periodic<A: SerialRtAdapter>(
        &mut self,
        adapter: &mut A,
        command: &JointCommandView<'_>,
        context: &SerialTickContext<'_>,
    ) {
        if !self.periodic_write(adapter, command, context) {
            return;
        }
        let SerialTickContext {
            ctrl,
            probe,
            clock,
            metrics,
            ..
        } = *context;
        let started = clock.now();
        let mut candidate = self.state;
        probe.set_context(
            BusOpClass::PeriodicRead,
            InstalledIntent::PeriodicRead,
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
            RobotError::Aborted(AbortReason::Revoked) => {}
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
    let observation = match (record.action, record.arg) {
        (normal_codes::ENABLE, 0) => Some(
            ctrl.authority()
                .begin_safety_observation(SafetyOperation::Disable),
        ),
        (normal_codes::DISCONNECT, _) => Some(
            ctrl.authority()
                .begin_safety_observation(SafetyOperation::Disconnect),
        ),
        _ => None,
    };
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
            if let Some(observation) = observation {
                if ctrl.rx_trust.load(Ordering::Acquire) {
                    let _ = ctrl.authority().complete_safety_observation(observation);
                } else {
                    ctrl.tighten_safety(OutputSafetyState::AdapterUncertain);
                }
            }
            AdapterResp::Ok
        }
        Err(error) => AdapterResp::Err(match error {
            RobotError::Aborted(AbortReason::Emergency) => AdapterErr::PreemptedByEmergency,
            RobotError::Aborted(AbortReason::Timeout) => AdapterErr::AbortedByTimeout,
            RobotError::Aborted(AbortReason::Revoked) => AdapterErr::AuthorityRevoked,
            RobotError::Comm(kind) => AdapterErr::Comm(kind),
            RobotError::DeadlineExceeded => AdapterErr::Comm(CommErrorKind::Timeout),
            RobotError::NotConnected => AdapterErr::Comm(CommErrorKind::Disconnected),
            RobotError::Unsupported(_) | RobotError::InvalidParameter(_) => AdapterErr::Unsupported,
            _ => AdapterErr::HardwareFault,
        }),
    }
}

#[cfg(all(test, feature = "feetech"))]
#[path = "serial_tick_feetech_tests.rs"]
mod feetech_tests;

#[cfg(all(test, feature = "mock"))]
mod tests {
    use super::*;
    use std::sync::Arc;

    use robot_adapter_mock::MockSerialRTAdapter;
    use robot_core::adapter::{AdapterInfo, JointCommandBuf, JointLimits, ManipulatorAdapter};
    use robot_core::executor_ctrl::OutputSafetyProbe;
    use robot_core::serial_services::LeasedPreemptProbe;
    use robot_core::time::MockClock;

    use crate::diag::SerialDiagRing;
    use crate::hard_rt_ctrl::test_authority;

    const POLICIES: [OutputPolicy; 6] = [
        OutputPolicy::Disabled,
        OutputPolicy::ReadyHold,
        OutputPolicy::ActiveForward,
        OutputPolicy::HoldCurrent,
        OutputPolicy::ZeroCommand,
        OutputPolicy::StopTorque,
    ];
    const NON_VELOCITY_MODES: [ControlMode; 4] = [
        ControlMode::Position,
        ControlMode::Torque,
        ControlMode::ServoPosition,
        ControlMode::Stop,
    ];

    fn command(mode: ControlMode) -> JointCommandBuf {
        JointCommandBuf {
            positions: [1.0; MAX_JOINTS],
            velocities: [2.0; MAX_JOINTS],
            torques: [3.0; MAX_JOINTS],
            mode: mode.into(),
            raw_velocities: [0; MAX_JOINTS],
            selection: JointSelection::All,
            stamp_ns: 100,
            deadline_ns: 200,
            joint_count: 3,
            seq: 71,
        }
    }

    fn assert_same_command(actual: &JointCommandBuf, expected: &JointCommandBuf) {
        assert_eq!(
            actual.positions.map(f32::to_bits),
            expected.positions.map(f32::to_bits)
        );
        assert_eq!(
            actual.velocities.map(f32::to_bits),
            expected.velocities.map(f32::to_bits)
        );
        assert_eq!(
            actual.torques.map(f32::to_bits),
            expected.torques.map(f32::to_bits)
        );
        assert_eq!(actual.mode, expected.mode);
        assert_eq!(actual.selection, expected.selection);
        assert_eq!(actual.raw_velocities, expected.raw_velocities);
        assert_eq!(actual.joint_count, expected.joint_count);
        assert_eq!(actual.stamp_ns, expected.stamp_ns);
        assert_eq!(actual.deadline_ns, expected.deadline_ns);
        assert_eq!(actual.seq, expected.seq);
    }

    /// 记录真正交给 adapter 的目标；每次普通 IO 均取得并归还同一 authority 的租约。
    struct RecordingAdapter {
        inner: MockSerialRTAdapter,
        probe: Arc<SerialPreemptProbe>,
        last_command: Option<JointCommandBuf>,
        write_attempts: usize,
        writes: usize,
        reads: usize,
        stops: usize,
        confirms: usize,
        last_confirm_all_off: bool,
        last_confirm_count: usize,
        read_error: Option<RobotError>,
        write_error: Option<RobotError>,
    }

    impl ManipulatorAdapter for RecordingAdapter {
        fn info(&self) -> AdapterInfo {
            self.inner.info()
        }
        fn connect(&mut self) -> Result<(), RobotError> {
            self.inner.connect()
        }
        fn disconnect(&mut self) -> Result<(), RobotError> {
            self.inner.disconnect()
        }

        fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError> {
            let lease = self
                .probe
                .begin_leased(BusOpClass::PeriodicRead)
                .map_err(RobotError::Aborted)?;
            self.reads += 1;
            let result = match self.read_error.take() {
                Some(error) => Err(error),
                None => self.inner.read_state(out),
            };
            self.probe.finish_leased(lease);
            result
        }

        fn write_command(&mut self, target: &JointCommandView<'_>) -> Result<(), RobotError> {
            self.write_attempts += 1;
            if let Some(error) = self.write_error.take() {
                return Err(error);
            }
            let lease = self
                .probe
                .begin_leased(BusOpClass::PeriodicWrite)
                .map_err(RobotError::Aborted)?;
            let mut recorded = JointCommandBuf {
                mode: target.mode.into(),
                selection: target.selection,
                stamp_ns: target.stamp_ns,
                deadline_ns: target.deadline_ns,
                joint_count: u8::try_from(target.velocities.len()).unwrap(),
                ..JointCommandBuf::default()
            };
            recorded.positions[..target.positions.len()].copy_from_slice(target.positions);
            recorded.velocities[..target.velocities.len()].copy_from_slice(target.velocities);
            recorded.torques[..target.torques.len()].copy_from_slice(target.torques);
            recorded.raw_velocities[..target.raw_velocities.len()]
                .copy_from_slice(target.raw_velocities);
            self.last_command = Some(recorded);
            self.writes += 1;
            let result = self.inner.write_command(target);
            self.probe.finish_leased(lease);
            result
        }

        fn set_mode(&mut self, mode: ControlMode) -> Result<(), RobotError> {
            self.inner.set_mode(mode)
        }
        fn enable(&mut self, on: bool) -> Result<(), RobotError> {
            self.inner.enable(on)
        }

        fn safety_stop(&mut self) -> Result<(), RobotError> {
            let lease = self
                .probe
                .begin_leased(BusOpClass::Safety)
                .map_err(RobotError::Aborted)?;
            self.stops += 1;
            let result = self.inner.safety_stop();
            self.probe.finish_leased(lease);
            result
        }

        fn limits(&self) -> &JointLimits {
            self.inner.limits()
        }
    }

    impl OutputSafetyProbe for RecordingAdapter {
        fn read_torque_enable_mask(&mut self, out: &mut [bool]) -> Result<(), RobotError> {
            let lease = self
                .probe
                .begin_leased(BusOpClass::Confirm)
                .map_err(RobotError::Aborted)?;
            self.confirms += 1;
            self.last_confirm_count = out.len();
            let result = self.inner.read_torque_enable_mask(out);
            self.last_confirm_all_off = result.is_ok() && out.iter().all(|enabled| !enabled);
            self.probe.finish_leased(lease);
            result
        }

        fn joint_count(&self) -> usize {
            self.inner.joint_count()
        }
    }

    struct Fixture {
        clock: Arc<MockClock>,
        ctrl: Arc<SerialIoCtrlInner>,
        probe: Arc<SerialPreemptProbe>,
        diag: SerialDiagRing,
        metrics: SerialIoThreadMetrics,
        tick: SerialTick,
        adapter: RecordingAdapter,
    }

    impl Fixture {
        fn new(enabled: bool) -> Self {
            Self::with_joint_count(enabled, 3)
        }

        fn with_joint_count(enabled: bool, joint_count: u8) -> Self {
            let clock = Arc::new(MockClock::new(MonoRawNs(100)));
            let ctrl = Arc::new(SerialIoCtrlInner::new_serial(test_authority(true)));
            let probe = Arc::new(SerialPreemptProbe::new(clock.clone(), ctrl.clone()));
            let mut fixture = Self {
                clock,
                ctrl,
                probe: probe.clone(),
                diag: SerialDiagRing::new(),
                metrics: SerialIoThreadMetrics::default(),
                tick: SerialTick::new(true, usize::from(joint_count), MonoRawNs(100)),
                adapter: RecordingAdapter {
                    inner: MockSerialRTAdapter::with_unlimited_limits(joint_count),
                    probe,
                    last_command: None,
                    write_attempts: 0,
                    writes: 0,
                    reads: 0,
                    stops: 0,
                    confirms: 0,
                    last_confirm_all_off: false,
                    last_confirm_count: 0,
                    read_error: None,
                    write_error: None,
                },
            };
            let mut boot = command(ControlMode::Velocity);
            boot.joint_count = joint_count;
            for _ in 0..3 {
                fixture.step(&boot.as_view());
            }
            assert_eq!(fixture.tick.convergence.phase(), ConvergePhase::Idle);
            assert!(fixture.tick.convergence.normal_io_allowed(&fixture.ctrl));
            assert_eq!(fixture.adapter.stops, 1);
            assert_eq!(fixture.adapter.confirms, 1);
            if enabled {
                let authority = fixture.ctrl.authority();
                let (owner, view) = authority.reserve_enable().unwrap();
                let execution = authority.start_enable(view).unwrap();
                fixture.adapter.enable(true).unwrap();
                authority
                    .finish_enable(execution, EnableResult::Succeeded)
                    .unwrap();
                authority.commit_enable(owner).unwrap();
            }
            fixture
        }

        fn step(&mut self, target: &JointCommandView<'_>) {
            let context = SerialTickContext {
                ctrl: &self.ctrl,
                probe: &self.probe,
                diag: &self.diag,
                clock: self.clock.as_ref(),
                metrics: &self.metrics,
            };
            self.tick
                .tick(&mut self.adapter, target, &context, MonoRawNs(1_000));
        }

        fn set_policy(&self, policy: OutputPolicy) {
            self.ctrl.policy.store(policy.to_u8(), Ordering::Release);
        }

        fn assert_fault_then_stop(&mut self, target: &JointCommandView<'_>) {
            assert_eq!(self.adapter.write_attempts, 0);
            assert_eq!(self.adapter.reads, 0);
            assert_eq!(self.metrics.write_errors.load(Ordering::Relaxed), 1);
            assert_ne!(self.ctrl.fault_mailbox.load(Ordering::Acquire), 0);
            assert_eq!(self.tick.convergence.phase(), ConvergePhase::StopIssue);
            self.step(target);
            assert_eq!(self.adapter.stops, 2);
            assert_eq!(self.tick.convergence.phase(), ConvergePhase::Confirm);
            self.step(target);
            assert_eq!(self.adapter.confirms, 2);
            assert!(self.adapter.last_confirm_all_off);
            assert_eq!(self.adapter.write_attempts, 0);
            assert_eq!(self.adapter.reads, 0);
            assert_ne!(self.ctrl.inhibit_bits(), 0);
            // 原故障路由可保留不可信确认；这里只证明真实停止与读回，不能补成可信 PASS。
            assert_ne!(self.ctrl.safety_state(), OutputSafetyState::TorqueOn);
        }
    }

    fn bus1_layout() -> crate::grouped_position::XleRobotBus1Layout {
        let config: crate::config_loader::RobotConfigToml =
            toml::from_str(include_str!("../../../configs/xlerobot_bus1_full.toml")).unwrap();
        crate::grouped_position::XleRobotBus1Layout::from_hardware(&config.hardware[0]).unwrap()
    }

    fn mixed_command() -> JointCommandBuf {
        let config: crate::config_loader::RobotConfigToml =
            toml::from_str(include_str!("../../../configs/xlerobot_bus2_full.toml")).unwrap();
        crate::grouped_position::XleRobotBus2Layout::from_hardware(&config.hardware[0])
            .unwrap()
            .command([0.4; 6], [100, -100, 50], 77, 100, 200)
            .unwrap()
    }

    #[test]
    fn mixed_complete_target_reads_back_and_estop_confirms_all_nine() {
        let mut fixture = Fixture::with_joint_count(true, 9);
        fixture.set_policy(OutputPolicy::ActiveForward);
        let source = mixed_command();
        fixture.step(&source.as_view());
        assert_eq!(&fixture.tick.state.positions[..6], &[0.4; 6]);
        // 本测试验证配置槽位目标的跨层透传，不复制轮位表；角色映射由独立黄金与排列测试验证。
        let expected_velocities = source.raw_velocities.map(f32::from);
        assert_eq!(
            &fixture.tick.state.velocities[6..9],
            &expected_velocities[6..9]
        );
        let mut expected = source;
        expected.seq = 0;
        assert_same_command(fixture.adapter.last_command.as_ref().unwrap(), &expected);
        assert_eq!(fixture.tick.state.joint_count, 9);
        let writes = fixture.adapter.write_attempts;
        fixture.set_policy(OutputPolicy::StopTorque);
        let _ = fixture.ctrl.authority().estop();
        fixture.step(&source.as_view());
        fixture.step(&source.as_view());
        assert_eq!(fixture.adapter.write_attempts, writes);
        assert_eq!(fixture.adapter.last_confirm_count, 9);
        assert!(fixture.adapter.last_confirm_all_off);
        assert_eq!(fixture.adapter.stops, 2);
    }

    #[test]
    fn mixed_hold_uses_bounded_feedback_and_zero_raw_without_refreshing_old_goal() {
        for policy in [
            OutputPolicy::ReadyHold,
            OutputPolicy::HoldCurrent,
            OutputPolicy::ZeroCommand,
        ] {
            for age_ns in [100, PERIODIC_READ_BUDGET_NS] {
                let mut fixture = Fixture::with_joint_count(true, 9);
                fixture.set_policy(OutputPolicy::ActiveForward);
                let mut seed = mixed_command();
                seed.positions[..6].fill(0.25);
                seed.raw_velocities.fill(0);
                fixture.step(&seed.as_view());
                let source = mixed_command();
                let original = source;
                fixture.clock.set(MonoRawNs(100 + age_ns));
                fixture.set_policy(policy);
                fixture.step(&source.as_view());
                let held = fixture.adapter.last_command.unwrap();
                assert_eq!(&held.positions[..6], &[0.25; 6]);
                assert_eq!(held.raw_velocities, [0; MAX_JOINTS]);
                assert_eq!(held.selection, source.selection);
                assert_eq!(
                    (held.stamp_ns, held.deadline_ns),
                    (100 + age_ns, 100 + age_ns + PERIODIC_WRITE_BUDGET_NS)
                );
                assert_same_command(&source, &original);
                assert_eq!(&fixture.tick.state.velocities[6..9], &[0.; 3]);
                assert_eq!(fixture.adapter.writes, 2);
                assert_eq!(fixture.tick.convergence.phase(), ConvergePhase::Idle);
            }
        }
    }

    #[test]
    fn mixed_hold_missing_stale_future_or_invalid_feedback_faults_before_write() {
        for policy in [
            OutputPolicy::ReadyHold,
            OutputPolicy::HoldCurrent,
            OutputPolicy::ZeroCommand,
        ] {
            for invalid in 0..5 {
                let mut fixture = Fixture::with_joint_count(true, 9);
                let source = mixed_command();
                if invalid != 0 {
                    fixture.set_policy(OutputPolicy::ActiveForward);
                    fixture.step(&source.as_view());
                }
                match invalid {
                    0 => assert_eq!(fixture.tick.state.joint_count, 0),
                    1 => fixture.clock.set(MonoRawNs(101 + PERIODIC_READ_BUDGET_NS)),
                    2 => fixture.clock.set(MonoRawNs(99)),
                    3 => fixture.tick.state.positions[0] = f32::NAN,
                    _ => fixture.tick.state.joint_count = 8,
                }
                let writes = fixture.adapter.write_attempts;
                let reads = fixture.adapter.reads;
                fixture.set_policy(policy);
                fixture.step(&source.as_view());
                assert_eq!(fixture.adapter.write_attempts, writes);
                assert_eq!(fixture.adapter.reads, reads);
                assert_eq!(fixture.tick.convergence.phase(), ConvergePhase::StopIssue);
                fixture.step(&source.as_view());
                fixture.step(&source.as_view());
                assert_eq!(fixture.adapter.last_confirm_count, 9);
                assert!(fixture.adapter.last_confirm_all_off);
                assert_eq!(fixture.adapter.stops, 2);
            }
        }
    }

    #[test]
    fn mixed_active_invalid_or_partial_transaction_failure_uses_original_full_bus_stop() {
        let valid = mixed_command();
        for invalid in 0..4 {
            let mut source = valid;
            match invalid {
                0 => source.deadline_ns = 100,
                1 => source.stamp_ns = 101,
                2 => source.raw_velocities[7] = i16::MIN,
                _ => {
                    source.selection = JointSelection::MixedPositionRawVelocity {
                        position_mask: 0x7f,
                        velocity_mask: 0x1c0,
                    }
                }
            }
            let mut fixture = Fixture::with_joint_count(true, 9);
            fixture.set_policy(OutputPolicy::ActiveForward);
            fixture.step(&source.as_view());
            fixture.assert_fault_then_stop(&source.as_view());
            assert_eq!(fixture.adapter.last_confirm_count, 9);
        }
        // 与真实Feetech两帧之间过期/第二帧写错返回同类错误，验证原S0收敛路由。
        for kind in [CommErrorKind::PendingDeadline, CommErrorKind::IoWrite] {
            let mut fixture = Fixture::with_joint_count(true, 9);
            fixture.set_policy(OutputPolicy::ActiveForward);
            fixture.adapter.write_error = Some(RobotError::Comm(kind));
            fixture.step(&valid.as_view());
            let fault = fixture.ctrl.mailbox_snapshot().unwrap();
            assert_eq!(
                (fault.code, fault.detail),
                (FaultCode::CommFault, kind.to_u8())
            );
            assert_eq!(fixture.tick.convergence.phase(), ConvergePhase::StopIssue);
            fixture.step(&valid.as_view());
            fixture.step(&valid.as_view());
            assert_eq!(fixture.adapter.write_attempts, 1);
            assert_eq!(fixture.adapter.reads, 0);
            assert_eq!(fixture.adapter.last_confirm_count, 9);
            assert!(fixture.adapter.last_confirm_all_off);
            assert_eq!(
                fixture.ctrl.mailbox_snapshot().unwrap().generation,
                fault.generation
            );
        }
    }

    #[test]
    fn grouped_position_runs_original_tick_feedback_and_full_bus_fault_stop() {
        use crate::grouped_position::Bus1PositionGroup;
        let layout = bus1_layout();
        let mut fixture = Fixture::with_joint_count(true, 8);
        fixture.set_policy(OutputPolicy::ActiveForward);
        let mut all = command(ControlMode::Position);
        all.joint_count = 8;
        fixture.step(&all.as_view());
        let head = layout
            .command(Bus1PositionGroup::Head, &[0.2, 0.3], 72, 100, 200)
            .unwrap();
        fixture.step(&head.as_view());
        assert_eq!(
            &fixture.tick.state.positions[..8],
            &[1., 1., 1., 1., 1., 1., 0.2, 0.3]
        );
        assert_eq!(
            &fixture.tick.state.velocities[..8],
            &[2., 2., 2., 2., 2., 2., 0., 0.]
        );
        assert_eq!(
            &fixture.tick.state.torques[..8],
            &[3., 3., 3., 3., 3., 3., 0., 0.]
        );
        let mut expected_head = head;
        expected_head.seq = 0; // 借用视图不携带 slot 的 seq，记录器不伪造来源元数据。
        assert_same_command(
            fixture.adapter.last_command.as_ref().unwrap(),
            &expected_head,
        );
        let arm = layout
            .command(Bus1PositionGroup::LeftArm, &[0.4; 6], 73, 100, 200)
            .unwrap();
        fixture.step(&arm.as_view());
        assert_eq!(
            &fixture.tick.state.positions[..8],
            &[0.4, 0.4, 0.4, 0.4, 0.4, 0.4, 0.2, 0.3]
        );
        assert_eq!(fixture.tick.state.joint_count, 8);
        assert_eq!(fixture.tick.state.stamp_ns, 100);
        let before_failure = fixture.tick.state;
        fixture.adapter.read_error = Some(RobotError::HardwareFault("确定性分组反馈故障"));
        fixture.step(&head.as_view());
        assert_eq!(fixture.tick.state, before_failure);
        assert_eq!(fixture.tick.convergence.phase(), ConvergePhase::StopIssue);
        fixture.step(&head.as_view());
        fixture.step(&head.as_view());
        assert_eq!(fixture.adapter.stops, 2);
        assert_eq!(fixture.adapter.confirms, 2);
        assert_eq!(fixture.adapter.last_confirm_count, 8);
        assert!(fixture.adapter.last_confirm_all_off);
        assert_ne!(fixture.ctrl.inhibit_bits(), 0);
    }

    #[test]
    fn invalid_grouped_position_faults_before_adapter_write_and_stops_whole_bus() {
        use crate::grouped_position::Bus1PositionGroup;
        let valid = bus1_layout()
            .command(Bus1PositionGroup::Head, &[0.2, 0.3], 1, 100, 200)
            .unwrap();
        let mut nonfinite = valid;
        nonfinite.positions[7] = f32::NAN;
        for invalid in [
            JointCommandBuf {
                selection: JointSelection::Mask(0),
                ..valid
            },
            JointCommandBuf {
                selection: JointSelection::Mask(1 << 8),
                ..valid
            },
            JointCommandBuf {
                joint_count: 7,
                ..valid
            },
            JointCommandBuf {
                mode: robot_core::adapter::ControlModeRepr::Velocity,
                ..valid
            },
            JointCommandBuf {
                stamp_ns: 101,
                ..valid
            },
            JointCommandBuf {
                deadline_ns: 100,
                ..valid
            },
            nonfinite,
        ] {
            let mut fixture = Fixture::with_joint_count(true, 8);
            fixture.set_policy(OutputPolicy::ActiveForward);
            fixture.step(&invalid.as_view());
            fixture.assert_fault_then_stop(&invalid.as_view());
            assert_eq!(fixture.adapter.last_confirm_count, 8);
        }
    }

    #[test]
    fn grouped_position_source_deadline_bounds_probe_progress() {
        use crate::grouped_position::Bus1PositionGroup;
        let target = bus1_layout()
            .command(Bus1PositionGroup::Head, &[0.2, 0.3], 1, 100, 200)
            .unwrap();
        let mut fixture = Fixture::with_joint_count(true, 8);
        fixture.set_policy(OutputPolicy::ActiveForward);
        let context = SerialTickContext {
            ctrl: &fixture.ctrl,
            probe: &fixture.probe,
            diag: &fixture.diag,
            clock: fixture.clock.as_ref(),
            metrics: &fixture.metrics,
        };
        fixture.probe.begin_tick();
        assert!(
            fixture
                .tick
                .periodic_write(&mut fixture.adapter, &target.as_view(), &context)
        );
        assert_eq!(fixture.adapter.write_attempts, 1);
        assert_eq!(fixture.adapter.writes, 1);
        assert_eq!(fixture.adapter.reads, 0);
        let progress = fixture.ctrl.liveness_counter.load(Ordering::Relaxed);
        fixture.clock.set(MonoRawNs(200));
        fixture.probe.checkpoint();
        assert_eq!(
            fixture.ctrl.liveness_counter.load(Ordering::Relaxed),
            progress
        );
        assert!(!fixture.probe.tick_progress_allowed());
    }

    #[test]
    fn grouped_position_estop_stops_and_confirms_all_eight_joints() {
        use crate::grouped_position::Bus1PositionGroup;
        let target = bus1_layout()
            .command(Bus1PositionGroup::Head, &[0.2, 0.3], 1, 100, 200)
            .unwrap();
        let mut fixture = Fixture::with_joint_count(true, 8);
        fixture.set_policy(OutputPolicy::ActiveForward);
        fixture.step(&target.as_view());
        let writes = fixture.adapter.write_attempts;
        fixture.set_policy(OutputPolicy::StopTorque);
        let _ = fixture.ctrl.authority().estop();
        fixture.step(&target.as_view());
        fixture.step(&target.as_view());
        assert_eq!(fixture.adapter.write_attempts, writes);
        assert_eq!(fixture.adapter.stops, 2);
        assert_eq!(fixture.adapter.confirms, 2);
        assert_eq!(fixture.adapter.last_confirm_count, 8);
        assert!(fixture.adapter.last_confirm_all_off);
    }

    #[test]
    fn non_velocity_selector_preserves_every_legacy_policy() {
        for mode in NON_VELOCITY_MODES {
            let mut source = command(mode);
            source.stamp_ns = u64::MAX;
            source.deadline_ns = 0;
            for policy in POLICIES {
                let expected = match policy {
                    OutputPolicy::Disabled | OutputPolicy::StopTorque => PeriodicOutput::Skip,
                    _ => PeriodicOutput::Forward,
                };
                assert_eq!(
                    select_periodic_output(&source.as_view(), policy, MonoRawNs(100)),
                    Ok(expected)
                );
            }
        }
    }

    #[test]
    fn nonfinite_full_position_never_reaches_adapter_write() {
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut target = command(ControlMode::Position);
            target.positions[1] = value;
            let mut fixture = Fixture::new(true);
            fixture.set_policy(OutputPolicy::ActiveForward);
            fixture.step(&target.as_view());
            assert_eq!(fixture.adapter.write_attempts, 0);
            // 停止策略仍直接跳过目标检查，不受非法旧目标阻碍。
            for policy in [OutputPolicy::StopTorque, OutputPolicy::Disabled] {
                assert_eq!(
                    select_periodic_output(&target.as_view(), policy, MonoRawNs(100)),
                    Ok(PeriodicOutput::Skip)
                );
            }
        }
    }

    #[test]
    fn active_velocity_accepts_stamp_and_rejects_deadline_boundaries() {
        let source = command(ControlMode::Velocity);
        for now in [100, 199] {
            assert_eq!(
                select_periodic_output(
                    &source.as_view(),
                    OutputPolicy::ActiveForward,
                    MonoRawNs(now)
                ),
                Ok(PeriodicOutput::Forward)
            );
        }
        for now in [200, 201] {
            assert_eq!(
                select_periodic_output(
                    &source.as_view(),
                    OutputPolicy::ActiveForward,
                    MonoRawNs(now)
                ),
                Err(RobotError::DeadlineExceeded)
            );
        }
        assert_eq!(
            select_periodic_output(
                &source.as_view(),
                OutputPolicy::ActiveForward,
                MonoRawNs(99)
            ),
            Err(RobotError::InvalidParameter("速度目标时间戳来自未来"))
        );
    }

    #[test]
    fn active_velocity_rejects_invalid_shape_and_nonfinite_targets() {
        let source = command(ControlMode::Velocity);
        let oversized = [0.0_f32; MAX_JOINTS + 1];
        for (positions, velocities, torques) in [
            (&[][..], &[][..], &[][..]),
            (&oversized[..], &oversized[..], &oversized[..]),
            (
                &source.positions[..2],
                &source.velocities[..3],
                &source.torques[..3],
            ),
            (
                &source.positions[..3],
                &source.velocities[..3],
                &source.torques[..2],
            ),
        ] {
            let target = JointCommandView {
                positions,
                velocities,
                torques,
                ..source.as_view()
            };
            assert_eq!(
                select_periodic_output(&target, OutputPolicy::ActiveForward, MonoRawNs(100)),
                Err(RobotError::InvalidParameter("速度目标维度无效"))
            );
        }
        for count in [1, MAX_JOINTS] {
            let mut valid = source;
            valid.joint_count = u8::try_from(count).unwrap();
            assert_eq!(
                select_periodic_output(
                    &valid.as_view(),
                    OutputPolicy::ActiveForward,
                    MonoRawNs(100)
                ),
                Ok(PeriodicOutput::Forward)
            );
        }
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut invalid = source;
            invalid.velocities[2] = value;
            assert_eq!(
                select_periodic_output(
                    &invalid.as_view(),
                    OutputPolicy::ActiveForward,
                    MonoRawNs(100)
                ),
                Err(RobotError::InvalidParameter("速度目标必须有限"))
            );
        }
    }

    #[test]
    fn velocity_zero_selector_uses_only_shape_and_bounded_local_time() {
        let mut source = command(ControlMode::Velocity);
        source.stamp_ns = u64::MAX;
        source.deadline_ns = 0;
        source.velocities[0] = f32::NAN;
        for policy in [OutputPolicy::HoldCurrent, OutputPolicy::ZeroCommand] {
            assert_eq!(
                select_periodic_output(&source.as_view(), policy, MonoRawNs(100)),
                Ok(PeriodicOutput::VelocityZero {
                    joint_count: 3,
                    stamp_ns: 100,
                    deadline_ns: 100 + PERIODIC_WRITE_BUDGET_NS
                })
            );
            assert_eq!(
                select_periodic_output(
                    &source.as_view(),
                    policy,
                    MonoRawNs(u64::MAX - PERIODIC_WRITE_BUDGET_NS)
                ),
                Ok(PeriodicOutput::VelocityZero {
                    joint_count: 3,
                    stamp_ns: u64::MAX - PERIODIC_WRITE_BUDGET_NS,
                    deadline_ns: u64::MAX
                })
            );
            assert_eq!(
                select_periodic_output(
                    &source.as_view(),
                    policy,
                    MonoRawNs(u64::MAX - PERIODIC_WRITE_BUDGET_NS + 1)
                ),
                Err(RobotError::InvalidParameter("速度归零截止时间溢出"))
            );
            let empty = JointCommandView {
                positions: &[],
                velocities: &[],
                torques: &[],
                ..source.as_view()
            };
            assert_eq!(
                select_periodic_output(&empty, policy, MonoRawNs(100)),
                Err(RobotError::InvalidParameter("速度目标维度无效"))
            );
        }
    }

    #[test]
    fn velocity_skip_policies_do_not_validate_unused_goals() {
        let source = command(ControlMode::Velocity);
        let unused = JointCommandView {
            positions: &[],
            velocities: &[f32::NAN],
            torques: &[],
            stamp_ns: u64::MAX,
            deadline_ns: 0,
            ..source.as_view()
        };
        for policy in [
            OutputPolicy::ReadyHold,
            OutputPolicy::Disabled,
            OutputPolicy::StopTorque,
        ] {
            assert_eq!(
                select_periodic_output(&unused, policy, MonoRawNs(u64::MAX)),
                Ok(PeriodicOutput::Skip)
            );
        }
    }

    #[test]
    fn tick_active_velocity_forwards_exact_goal_and_reads_with_independent_lease() {
        let mut fixture = Fixture::new(true);
        fixture.set_policy(OutputPolicy::ActiveForward);
        let source = command(ControlMode::Velocity);
        fixture.step(&source.as_view());
        let mut expected = source;
        expected.seq = 0; // 借用视图不带来源 seq；记录器不伪造 slot 元数据。
        expected.positions[3..].fill(0.0);
        expected.velocities[3..].fill(0.0);
        expected.torques[3..].fill(0.0);
        assert_same_command(&fixture.adapter.last_command.unwrap(), &expected);
        assert_eq!(fixture.adapter.writes, 1);
        assert_eq!(fixture.adapter.reads, 1);
        assert_eq!(fixture.tick.state.stamp_ns, 100);
        assert!(!fixture.ctrl.authority().snapshot().normal_io_active);
    }

    #[test]
    fn tick_ready_velocity_skips_goal_but_keeps_periodic_read() {
        let mut fixture = Fixture::new(true);
        fixture.set_policy(OutputPolicy::ReadyHold);
        let source = command(ControlMode::Velocity);
        fixture.step(&source.as_view());
        assert_eq!(fixture.adapter.write_attempts, 0);
        assert_eq!(fixture.adapter.reads, 1);
        assert_eq!(fixture.tick.state.stamp_ns, 100);
        assert_eq!(fixture.metrics.write_errors.load(Ordering::Relaxed), 0);
        assert_eq!(fixture.ctrl.fault_mailbox.load(Ordering::Acquire), 0);
    }

    #[test]
    fn velocity_deadline_bounds_probe_progress_without_claiming_wire_rejection() {
        for deadline_ns in [101, 1_000_000_000] {
            let mut fixture = Fixture::new(true);
            fixture.set_policy(OutputPolicy::ActiveForward);
            let mut source = command(ControlMode::Velocity);
            source.deadline_ns = deadline_ns;
            let context = SerialTickContext {
                ctrl: &fixture.ctrl,
                probe: &fixture.probe,
                diag: &fixture.diag,
                clock: fixture.clock.as_ref(),
                metrics: &fixture.metrics,
            };
            fixture.probe.begin_tick();
            assert!(
                fixture
                    .tick
                    .periodic_write(&mut fixture.adapter, &source.as_view(), &context)
            );
            assert_eq!(fixture.adapter.writes, 1);
            let progress = fixture.ctrl.liveness_counter.load(Ordering::Relaxed);
            fixture
                .clock
                .set(MonoRawNs(deadline_ns.min(100 + PERIODIC_WRITE_BUDGET_NS)));
            fixture.probe.checkpoint();
            assert_eq!(
                fixture.ctrl.liveness_counter.load(Ordering::Relaxed),
                progress
            );
            assert!(!fixture.probe.tick_progress_allowed());
        }
        let mut fixture = Fixture::new(true);
        fixture.set_policy(OutputPolicy::ActiveForward);
        let mut source = command(ControlMode::Position);
        source.deadline_ns = 101;
        let context = SerialTickContext {
            ctrl: &fixture.ctrl,
            probe: &fixture.probe,
            diag: &fixture.diag,
            clock: fixture.clock.as_ref(),
            metrics: &fixture.metrics,
        };
        fixture.probe.begin_tick();
        assert!(
            fixture
                .tick
                .periodic_write(&mut fixture.adapter, &source.as_view(), &context)
        );
        fixture.clock.set(MonoRawNs(101));
        assert!(fixture.probe.tick_progress_allowed());
    }

    #[test]
    fn tick_hold_and_zero_clear_all_goal_vectors_without_mutating_source() {
        for policy in [OutputPolicy::HoldCurrent, OutputPolicy::ZeroCommand] {
            let mut fixture = Fixture::new(true);
            fixture.set_policy(policy);
            let mut source = command(ControlMode::Velocity);
            source.deadline_ns = 0;
            source.velocities[0] = f32::NAN;
            let original = source;
            fixture.step(&source.as_view());
            let zero = fixture.adapter.last_command.unwrap();
            assert_eq!(ControlMode::from(zero.mode), ControlMode::Velocity);
            assert_eq!(zero.joint_count, source.joint_count);
            assert_eq!(zero.positions.map(f32::to_bits), [0; MAX_JOINTS]);
            assert_eq!(zero.velocities.map(f32::to_bits), [0; MAX_JOINTS]);
            assert_eq!(zero.torques.map(f32::to_bits), [0; MAX_JOINTS]);
            assert_eq!(zero.stamp_ns, 100);
            assert_eq!(zero.deadline_ns, 100 + PERIODIC_WRITE_BUDGET_NS);
            assert_same_command(&source, &original);
            assert_eq!(fixture.adapter.writes, 1);
            assert_eq!(fixture.adapter.reads, 1);
        }
    }

    #[test]
    fn tick_non_velocity_policies_keep_original_adapter_calls() {
        for mode in NON_VELOCITY_MODES {
            for policy in POLICIES {
                let mut fixture = Fixture::new(true);
                fixture.set_policy(policy);
                let source = command(mode);
                fixture.step(&source.as_view());
                let expected_writes = usize::from(!matches!(
                    policy,
                    OutputPolicy::Disabled | OutputPolicy::StopTorque
                ));
                assert_eq!(fixture.adapter.writes, expected_writes);
                assert_eq!(fixture.adapter.reads, 1);
                if let Some(actual) = fixture.adapter.last_command {
                    assert_eq!(ControlMode::from(actual.mode), mode);
                    assert_eq!(actual.positions[0].to_bits(), source.positions[0].to_bits());
                    assert_eq!(
                        actual.velocities[0].to_bits(),
                        source.velocities[0].to_bits()
                    );
                    assert_eq!(actual.torques[0].to_bits(), source.torques[0].to_bits());
                    assert_eq!(actual.stamp_ns, source.stamp_ns);
                    assert_eq!(actual.deadline_ns, source.deadline_ns);
                }
            }
        }
    }

    #[test]
    fn tick_invalid_active_velocity_faults_before_write_then_executes_safety_stop() {
        let mut expired = command(ControlMode::Velocity);
        expired.deadline_ns = 100;
        let mut future = command(ControlMode::Velocity);
        future.stamp_ns = 101;
        let mut nonfinite = command(ControlMode::Velocity);
        nonfinite.velocities[1] = f32::INFINITY;
        let empty = JointCommandView {
            positions: &[],
            velocities: &[],
            torques: &[],
            ..expired.as_view()
        };
        for target in [
            expired.as_view(),
            future.as_view(),
            nonfinite.as_view(),
            empty,
        ] {
            let mut fixture = Fixture::new(true);
            fixture.set_policy(OutputPolicy::ActiveForward);
            fixture.step(&target);
            fixture.assert_fault_then_stop(&target);
        }
    }

    #[test]
    fn tick_unsupported_periodic_write_faults_then_stops_and_confirms_without_normal_io() {
        let mut fixture = Fixture::new(true);
        fixture.set_policy(OutputPolicy::ActiveForward);
        fixture.adapter.write_error = Some(RobotError::Unsupported("夹具拒绝未实现的控制模式"));
        let source = command(ControlMode::Velocity);
        fixture.step(&source.as_view());
        let fault = fixture.ctrl.mailbox_snapshot().unwrap();
        assert_eq!(fault.code, FaultCode::AdapterErr);
        assert_eq!(fault.detail, 0);
        assert_eq!(fault.origin, FaultOrigin::Periodic);
        assert_eq!(fixture.metrics.write_errors.load(Ordering::Relaxed), 1);
        assert_eq!(fixture.tick.convergence.phase(), ConvergePhase::StopIssue);
        assert!(!fixture.ctrl.authority().snapshot().normal_io_active);

        // 错误只记录一次；随后的 tick 真正执行断扭与确认，不能继续普通 IO。
        fixture.step(&source.as_view());
        assert_eq!(fixture.adapter.stops, 2);
        assert_eq!(fixture.tick.convergence.phase(), ConvergePhase::Confirm);
        fixture.step(&source.as_view());
        assert_eq!(fixture.adapter.confirms, 2);
        assert!(fixture.adapter.last_confirm_all_off);
        fixture.step(&source.as_view());
        assert_eq!(fixture.adapter.write_attempts, 1);
        assert_eq!(fixture.adapter.writes, 0);
        assert_eq!(fixture.adapter.reads, 0);
        assert_eq!(fixture.metrics.write_errors.load(Ordering::Relaxed), 1);
        assert_ne!(fixture.ctrl.inhibit_bits(), 0);
        assert_ne!(fixture.ctrl.safety_state(), OutputSafetyState::TorqueOn);
        assert_eq!(
            fixture.ctrl.mailbox_snapshot().unwrap().generation,
            fault.generation
        );
    }

    #[test]
    fn tick_zero_deadline_overflow_faults_without_fabricating_fresh_goal() {
        for policy in [OutputPolicy::HoldCurrent, OutputPolicy::ZeroCommand] {
            let mut fixture = Fixture::new(true);
            fixture.clock.set(MonoRawNs(u64::MAX));
            fixture.set_policy(policy);
            let source = command(ControlMode::Velocity);
            fixture.step(&source.as_view());
            fixture.assert_fault_then_stop(&source.as_view());
        }
    }

    #[test]
    fn revoked_velocity_motion_keeps_independently_authorized_read() {
        let mut fixture = Fixture::new(false);
        fixture.set_policy(OutputPolicy::ZeroCommand);
        let source = command(ControlMode::Velocity);
        fixture.step(&source.as_view());
        assert_eq!(fixture.adapter.write_attempts, 1);
        assert_eq!(fixture.adapter.writes, 0);
        assert_eq!(fixture.adapter.reads, 1);
        assert_eq!(fixture.metrics.write_errors.load(Ordering::Relaxed), 1);
        assert_eq!(fixture.ctrl.fault_mailbox.load(Ordering::Acquire), 0);
        assert_eq!(fixture.tick.convergence.phase(), ConvergePhase::Idle);
        assert!(!fixture.ctrl.authority().snapshot().normal_io_active);
    }

    #[test]
    fn ready_velocity_read_failure_uses_original_fault_and_safety_route() {
        let mut fixture = Fixture::new(true);
        fixture.set_policy(OutputPolicy::ReadyHold);
        fixture.adapter.read_error = Some(RobotError::HardwareFault("确定性读失败"));
        let source = command(ControlMode::Velocity);
        fixture.step(&source.as_view());
        assert_eq!(fixture.adapter.write_attempts, 0);
        assert_eq!(fixture.adapter.reads, 1);
        assert_eq!(fixture.metrics.read_errors.load(Ordering::Relaxed), 1);
        assert_eq!(fixture.tick.state.stamp_ns, 0);
        assert_eq!(fixture.tick.convergence.phase(), ConvergePhase::StopIssue);
        fixture.step(&source.as_view());
        assert_eq!(fixture.adapter.stops, 2);
        assert_eq!(fixture.adapter.write_attempts, 0);
    }

    #[test]
    fn disabled_and_stop_velocity_do_not_write_ordinary_goal() {
        for policy in [OutputPolicy::Disabled, OutputPolicy::StopTorque] {
            let mut fixture = Fixture::new(false);
            fixture.set_policy(policy);
            let source = command(ControlMode::Velocity);
            fixture.step(&source.as_view());
            assert_eq!(fixture.adapter.write_attempts, 0);
            assert_eq!(fixture.adapter.reads, 1);
            assert_eq!(fixture.adapter.stops, 1);
        }
    }

    #[test]
    fn velocity_estop_uses_fast_safety_chain_without_periodic_write() {
        let mut fixture = Fixture::new(true);
        fixture.set_policy(OutputPolicy::StopTorque);
        let _ = fixture.ctrl.authority().estop();
        let source = command(ControlMode::Velocity);
        fixture.step(&source.as_view());
        assert_eq!(fixture.adapter.stops, 2);
        assert_eq!(fixture.adapter.write_attempts, 0);
        assert_eq!(fixture.adapter.reads, 0);
        fixture.step(&source.as_view());
        assert_eq!(fixture.adapter.confirms, 2);
        assert!(fixture.adapter.last_confirm_all_off);
    }
}
