//! 串口停止、启动确认与故障对账共用一个收敛轮次；仅串口 owner 驱动。

use std::sync::atomic::Ordering;

use robot_core::adapter::{MAX_JOINTS, SerialRtAdapter};
use robot_core::diag::{DiagEvent, DiagSink};
use robot_core::error::RobotError;
use robot_core::executor_ctrl::{
    AdapterErr, AdapterResp, OutputSafetyState, SafetyStopFailDetail, TorqueEnableMask,
    normal_codes, pack_completed,
};
use robot_core::serial_services::{BusOpClass, CommErrorKind, NormalAbortTarget};
use robot_core::time::{MonoRawNs, RobotClock};

use crate::hard_rt_ctrl::{
    ActionRecord, CompletedEffect, FaultOrigin, HardRtCtrlInner, SerialPending,
    decode_serial_pending, inhibit, tighten_estop_policy,
};
use crate::state_machine::FaultCode;

use crate::serial_preempt::{InstalledIntent, SerialPreemptProbe};
use robot_core::control_authority::{
    InhibitReason, PermissionEpoch, SafetyObservation, SafetyOperation,
};

const CALL_BUDGET_NS: u64 = 50_000_000;
const CONFIRM_BUDGET_NS: u64 = 250_000_000;
const MAX_STOP_ATTEMPTS: u8 = 3;
const MAX_CONFIRM_ATTEMPTS: u8 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConvergeContext {
    Fault,
    Emergency,
    Preflight,
    Compensation,
}

/// 只表示本次控制路径已经安排物理停止；不能证明 Off、ACK 或事务退休。
/// 构造封闭在实际 claim / timeout / fault 处理内，None 作用域是设备级急停。
#[derive(Debug)]
pub(crate) struct EnableRecoveryArranged {
    seq: Option<u64>,
}

impl EnableRecoveryArranged {
    pub(crate) fn covers(&self, seq: u64) -> bool {
        self.seq.is_none_or(|arranged| arranged == seq)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConvergeCompletion {
    AllOffTrusted,
    AdapterUncertain { kind: CommErrorKind },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ConvergeRound {
    pub ctx: ConvergeContext,
    pub generation: Option<u32>,
    pub estop_bound: bool,
    pub attempt: u8,
    pub some_on_seen: bool,
    pub completed_safety: Option<ConvergeCompletion>,
}

impl ConvergeRound {
    const fn new(ctx: ConvergeContext) -> Self {
        Self {
            ctx,
            generation: None,
            estop_bound: matches!(ctx, ConvergeContext::Emergency),
            attempt: 0,
            some_on_seen: false,
            completed_safety: None,
        }
    }

    fn restart_physical(&mut self) {
        self.attempt = 0;
        self.some_on_seen = false;
        self.completed_safety = None;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConvergePhase {
    Idle,
    StopIssue,
    StopRetry,
    Connect,
    Confirm,
    AwaitAck,
    SafetyStopFailedAwaitAck {
        generation: u32,
        detail: SafetyStopFailDetail,
    },
    SafetyStopFailed,
}

impl ConvergePhase {
    const fn code(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::StopIssue => 1,
            Self::StopRetry => 2,
            Self::Connect => 3,
            Self::Confirm => 4,
            Self::AwaitAck => 5,
            Self::SafetyStopFailedAwaitAck { .. } => 6,
            Self::SafetyStopFailed => 7,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum BootOutcome {
    Pending,
    Passed,
    Faulted,
}

#[derive(Clone, Copy)]
struct ActiveFault {
    code: FaultCode,
    detail: u8,
    origin: FaultOrigin,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Initialization {
    Fresh,
    Started,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ConvergeCounters {
    pub stop_writes: u64,
    pub confirmed: u64,
    pub uncertain: u64,
    pub failed: u64,
}

/// 不保存 adapter 或线程句柄。每次 step 只调用一个阶段的设备入口。
pub(crate) struct SerialConvergence {
    round: Option<ConvergeRound>,
    phase: ConvergePhase,
    fault: Option<ActiveFault>,
    authorized: bool,
    joint_count: usize,
    initialization: Initialization,
    connect_pending: bool,
    boot_outcome: BootOutcome,
    spawned_at: MonoRawNs,
    first_boot_exit: Option<MonoRawNs>,
    stop_attempt: u8,
    still_on: u8,
    last_kind: CommErrorKind,
    generation_saturated: bool,
    // 仅本轮真实观测/失败提交及其后续单次解除产生的 epoch；陈旧时不重采。
    release_epoch: Option<PermissionEpoch>,
    counters: ConvergeCounters,
}

impl SerialConvergence {
    pub(crate) fn new(authorized: bool, joint_count: usize, spawned_at: MonoRawNs) -> Self {
        Self {
            round: Some(ConvergeRound::new(if authorized {
                ConvergeContext::Preflight
            } else {
                ConvergeContext::Fault
            })),
            phase: ConvergePhase::StopIssue,
            fault: None,
            authorized,
            joint_count,
            initialization: Initialization::Fresh,
            connect_pending: true,
            boot_outcome: BootOutcome::Pending,
            spawned_at,
            first_boot_exit: None,
            stop_attempt: 0,
            still_on: 0,
            last_kind: CommErrorKind::Unknown,
            generation_saturated: false,
            release_epoch: None,
            counters: ConvergeCounters::default(),
        }
    }

    pub(crate) const fn phase(&self) -> ConvergePhase {
        self.phase
    }

    pub(crate) const fn counters(&self) -> ConvergeCounters {
        self.counters
    }

    #[cfg(test)]
    pub(crate) const fn round(&self) -> Option<&ConvergeRound> {
        self.round.as_ref()
    }

    #[cfg(test)]
    pub(crate) const fn boot_outcome(&self) -> BootOutcome {
        self.boot_outcome
    }

    #[cfg(test)]
    pub(crate) const fn first_boot_exit(&self) -> Option<MonoRawNs> {
        self.first_boot_exit
    }

    #[cfg(test)]
    pub(crate) fn clear_completed_safety_for_test(&mut self) {
        if let Some(round) = &mut self.round {
            round.completed_safety = None;
        }
    }

    pub(crate) fn evidence_trusted(&self, ctrl: &HardRtCtrlInner) -> bool {
        match self.round.as_ref().map(|round| round.ctx) {
            Some(ConvergeContext::Preflight) => {
                self.authorized && ctrl.session_clean.load(Ordering::Acquire)
            }
            _ => ctrl.rx_trust.load(Ordering::Acquire),
        }
    }

    pub(crate) fn normal_io_allowed(&self, ctrl: &HardRtCtrlInner) -> bool {
        self.phase == ConvergePhase::Idle && ctrl.inhibit_bits() == 0
    }

    /// 仅在本次实际操作已经新建或合并停止责任后调用，不能用旧完成轮构造。
    fn physical_recovery(&self, seq: Option<u64>) -> Option<EnableRecoveryArranged> {
        match self.phase {
            ConvergePhase::StopIssue
            | ConvergePhase::StopRetry
            | ConvergePhase::Connect
            | ConvergePhase::Confirm
                if self
                    .round
                    .as_ref()
                    .is_some_and(|round| round.completed_safety.is_none()) =>
            {
                Some(EnableRecoveryArranged { seq })
            }
            _ => None,
        }
    }

    /// 当前 Enable 的补偿复用唯一物理收敛机，不创造故障、急停或连接操作。
    pub(crate) fn ensure_enable_compensation(
        &mut self,
        ctrl: &HardRtCtrlInner,
        diag: &dyn DiagSink,
        now: MonoRawNs,
    ) {
        if matches!(
            ctrl.safety_state(),
            OutputSafetyState::StopFailed | OutputSafetyState::AdapterUncertain
        ) {
            return;
        }
        if self.phase == ConvergePhase::Idle {
            self.round = Some(ConvergeRound::new(ConvergeContext::Compensation));
            self.stop_attempt = 0;
            self.still_on = 0;
            self.last_kind = CommErrorKind::Unknown;
            self.connect_pending = false;
            self.release_epoch = None;
            ctrl.tighten_inhibits(inhibit::CONVERGING);
            self.transition(ConvergePhase::StopIssue, diag, now);
        }
        // 活动轮已持有停止责任；不得重置任何尝试数、generation、截止或原观测。
        // AwaitAck / sticky 终态也不能重开物理预算。
    }

    fn transition(&mut self, phase: ConvergePhase, diag: &dyn DiagSink, now: MonoRawNs) {
        if self.phase != phase {
            emit(diag, now, 0, self.phase.code(), phase.code(), 0);
            self.phase = phase;
        }
    }

    /// 信任收紧必须先于故障同级去重，否则后到的真实 RX 错误会被本地超时遮住。
    fn observe_comm(
        &mut self,
        kind: CommErrorKind,
        ctrl: &HardRtCtrlInner,
        diag: &dyn DiagSink,
        now: MonoRawNs,
    ) {
        self.last_kind = kind;
        if kind.clears_trust() {
            ctrl.session_clean.store(false, Ordering::Release);
            ctrl.rx_trust.store(false, Ordering::Release);
            emit(diag, now, 1, kind.to_u8(), 0, 0);
        }
    }

    /// 发布顺序统一经过 ctrl：安全状态 → 原因位/待对账位 → 单词 mailbox。
    pub(crate) fn enter_fault(
        &mut self,
        code: FaultCode,
        detail: u8,
        origin: FaultOrigin,
        ctrl: &HardRtCtrlInner,
        diag: &dyn DiagSink,
        now: MonoRawNs,
    ) {
        let _ = self.enter_fault_impl(code, detail, origin, ctrl, diag, now);
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "沿原故障输入追加原 seq 关联，同次安排才返回恢复事实"
    )]
    pub(crate) fn enter_fault_arranged(
        &mut self,
        code: FaultCode,
        detail: u8,
        origin: FaultOrigin,
        ctrl: &HardRtCtrlInner,
        diag: &dyn DiagSink,
        now: MonoRawNs,
        seq: u64,
    ) -> Option<EnableRecoveryArranged> {
        if self.enter_fault_impl(code, detail, origin, ctrl, diag, now) {
            self.physical_recovery(Some(seq))
        } else {
            None
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "故障去重、原代次发布与停止轮接管在同一路径判定，不凭旧轮生成事实"
    )]
    fn enter_fault_impl(
        &mut self,
        code: FaultCode,
        detail: u8,
        origin: FaultOrigin,
        ctrl: &HardRtCtrlInner,
        diag: &dyn DiagSink,
        now: MonoRawNs,
    ) -> bool {
        if code == FaultCode::CommFault {
            self.observe_comm(CommErrorKind::from_u8(detail), ctrl, diag, now);
            ctrl.comm_level.store(2, Ordering::Release);
        }
        emit(
            diag,
            now,
            2,
            severity(code),
            origin as u8,
            u32::from(detail),
        );
        let previous = self.fault;
        let first_emergency_fault = self.round.as_ref().is_some_and(|round| {
            round.ctx == ConvergeContext::Emergency && round.generation.is_none()
        });
        if !first_emergency_fault
            && previous.is_some_and(|fault| severity(code) <= severity(fault.code))
        {
            if let Some(fault) = &mut self.fault {
                // 保留当前 code/origin，仅更新同级诊断的细节，不重发 generation。
                if fault.code == code {
                    fault.detail = detail;
                }
            }
            return false;
        }

        // 新 Emergency 轮的首个故障有独立 ACK 义务；旧轮次已 ACK 不可吞掉它。
        let (code, detail, origin) = match previous {
            Some(fault) if first_emergency_fault && severity(fault.code) > severity(code) => {
                (fault.code, fault.detail, fault.origin)
            }
            _ => (code, detail, origin),
        };

        let ssf =
            (code == FaultCode::SafetyStopFailed).then(|| SafetyStopFailDetail::from_u8(detail));
        let state = match ssf {
            Some(SafetyStopFailDetail::AdapterUncertain { .. }) => {
                OutputSafetyState::AdapterUncertain
            }
            Some(_) => OutputSafetyState::StopFailed,
            None if self.phase == ConvergePhase::Idle => OutputSafetyState::StopCommanded,
            None => ctrl.safety_state(),
        };
        let reasons = match code {
            FaultCode::CommFault => inhibit::COMM_FAULT,
            FaultCode::SafetyStopFailed => inhibit::STOP_FAILED,
            _ => inhibit::ADAPTER_FAULT,
        };
        if ssf.is_none() {
            // 本轮收紧必须先于故障发布，后者返回最后一次自有 CAS 的释放代次。
            ctrl.tighten_inhibits(inhibit::CONVERGING);
        }
        let (generation, publication_epoch) =
            ctrl.publish_fault_with_epoch(state, reasons, code, detail, origin);
        self.release_epoch = Some(publication_epoch);
        self.fault = Some(ActiveFault {
            code,
            detail,
            origin,
        });
        if let Some(previous) = previous {
            emit(
                diag,
                now,
                14,
                severity(previous.code),
                severity(code),
                generation.unwrap_or(0),
            );
        }
        if generation.is_none() {
            // 不得把饱和当作无 gen 的 Emergency 完成；保留抑制并永久拒绝放行。
            self.generation_saturated = true;
            emit(diag, now, 3, 0, 0, u32::MAX);
        }
        let round = self
            .round
            .get_or_insert_with(|| ConvergeRound::new(ConvergeContext::Fault));
        round.generation = generation.or(round.generation);
        if round.ctx == ConvergeContext::Preflight {
            round.ctx = ConvergeContext::Fault;
        }
        if let Some(detail) = ssf {
            match detail {
                SafetyStopFailDetail::AdapterUncertain { .. } => self.counters.uncertain += 1,
                _ => self.counters.failed += 1,
            }
            self.release_epoch =
                release_inhibit_at(ctrl, InhibitReason::Converging, publication_epoch);
            self.mark_boot_exit(ctrl, now);
            self.transition(
                ConvergePhase::SafetyStopFailedAwaitAck {
                    generation: generation.unwrap_or(0),
                    detail,
                },
                diag,
                now,
            );
            return false;
        }
        let restart =
            (previous.is_some() && !first_emergency_fault) || self.phase == ConvergePhase::Idle;
        if restart {
            self.restart_fault_convergence(diag, now);
        }
        true
    }

    /// 升级故障只重置物理收敛进度，保留本轮上下文与 generation 对账义务。
    fn restart_fault_convergence(&mut self, diag: &dyn DiagSink, now: MonoRawNs) {
        if let Some(round) = &mut self.round {
            round.restart_physical();
        }
        self.stop_attempt = 0;
        self.still_on = 0;
        self.connect_pending = false;
        self.transition(ConvergePhase::StopIssue, diag, now);
    }

    /// 调用者在 tick 头及正常调用的抢占返回后立即调用；只有成功 claim 才合并。
    pub(crate) fn claim_estop(
        &mut self,
        ctrl: &HardRtCtrlInner,
        diag: &dyn DiagSink,
        now: MonoRawNs,
    ) {
        let _ = self.claim_estop_arranged(ctrl, diag, now);
    }

    pub(crate) fn claim_estop_arranged(
        &mut self,
        ctrl: &HardRtCtrlInner,
        diag: &dyn DiagSink,
        now: MonoRawNs,
    ) -> Option<EnableRecoveryArranged> {
        if !ctrl.claim_serial_estop() {
            return None;
        }
        emit(diag, now, 4, self.phase.code(), 0, 0);
        if !matches!(
            self.phase,
            ConvergePhase::SafetyStopFailedAwaitAck { .. } | ConvergePhase::SafetyStopFailed
        ) {
            emit(diag, now, 11, self.phase.code(), 0, 0);
        }
        match self.phase {
            ConvergePhase::SafetyStopFailedAwaitAck { .. } | ConvergePhase::SafetyStopFailed => {}
            ConvergePhase::Idle => {
                self.round = Some(ConvergeRound::new(ConvergeContext::Emergency));
                self.stop_attempt = 0;
                self.still_on = 0;
                self.last_kind = CommErrorKind::Unknown;
                self.connect_pending = false;
                ctrl.tighten_inhibits(inhibit::CONVERGING);
                self.transition(ConvergePhase::StopIssue, diag, now);
            }
            ConvergePhase::AwaitAck => {
                let completed = self.round.as_ref().and_then(|round| round.completed_safety);
                if let Some(round) = &mut self.round {
                    round.estop_bound = true;
                }
                match completed {
                    Some(ConvergeCompletion::AllOffTrusted) => {
                        if let Some(round) = &mut self.round {
                            round.restart_physical();
                        }
                        self.stop_attempt = 0;
                        self.still_on = 0;
                        self.last_kind = CommErrorKind::Unknown;
                        self.connect_pending = false;
                        ctrl.tighten_inhibits(inhibit::CONVERGING);
                        self.transition(ConvergePhase::StopIssue, diag, now);
                    }
                    Some(ConvergeCompletion::AdapterUncertain { kind }) => {
                        self.fail_stop(
                            SafetyStopFailDetail::AdapterUncertain { kind },
                            FaultOrigin::EStopEscalation,
                            ctrl,
                            diag,
                            now,
                        );
                    }
                    None => {
                        emit(diag, now, 3, 1, 0, 0);
                        self.fail_stop(
                            SafetyStopFailDetail::AdapterUncertain {
                                kind: CommErrorKind::Unknown,
                            },
                            FaultOrigin::InvariantViolation,
                            ctrl,
                            diag,
                            now,
                        );
                    }
                }
            }
            _ => {
                if let Some(round) = &mut self.round {
                    round.estop_bound = true;
                }
                emit(diag, now, 5, self.phase.code(), 0, 0);
            }
        }
        self.physical_recovery(None)
    }

    /// 只登记退場与收敛责任；设备操作统一留给下一次 step。
    pub(crate) fn retire_timeout(
        &mut self,
        ctrl: &HardRtCtrlInner,
        diag: &dyn DiagSink,
        now: MonoRawNs,
    ) -> bool {
        self.retire_timeout_impl(ctrl, diag, now, || {}).0
    }

    pub(crate) fn retire_timeout_arranged(
        &mut self,
        ctrl: &HardRtCtrlInner,
        diag: &dyn DiagSink,
        now: MonoRawNs,
    ) -> Option<EnableRecoveryArranged> {
        self.retire_timeout_impl(ctrl, diag, now, || {}).1
    }

    #[cfg(test)]
    pub(crate) fn retire_timeout_before_clear(
        &mut self,
        ctrl: &HardRtCtrlInner,
        diag: &dyn DiagSink,
        now: MonoRawNs,
        hook: impl FnOnce(),
    ) -> bool {
        self.retire_timeout_impl(ctrl, diag, now, hook).0
    }

    fn retire_timeout_impl(
        &mut self,
        ctrl: &HardRtCtrlInner,
        diag: &dyn DiagSink,
        now: MonoRawNs,
        before_clear: impl FnOnce(),
    ) -> (bool, Option<EnableRecoveryArranged>) {
        let observed = ctrl.abort_request.load(Ordering::Acquire);
        let request = match NormalAbortTarget::decode(observed) {
            Ok(None) => return (false, None),
            decoded => decoded,
        };
        emit(diag, now, 16, 0, 0, seq_low(observed));
        let active = ActionRecord::unpack(ctrl.active_action.load(Ordering::Relaxed));
        let completed = CompletedEffect::unpack(ctrl.completed_effect.load(Ordering::Relaxed));
        let pending = ctrl.pending_normal.load(Ordering::Acquire);
        let mut exempt = false;
        let valid = match request {
            Ok(Some(NormalAbortTarget::Unsubmitted)) => pending == 0 && !active.valid,
            Ok(Some(NormalAbortTarget::Submitted { seq: target })) => match () {
                () if active.valid && active.seq == target => {
                    // 已 start 的 Enable 只能由持 Execution 的执行器完成；本处只收紧。
                    if ctrl.authority().snapshot().phase
                        != Some(robot_core::control_authority::Phase::EnableExecuting)
                    {
                        publish_aborted(ctrl, active);
                    }
                    true
                }
                () if completed.action.valid && completed.action.seq == target => {
                    exempt = completed.success
                        && completed.action.action == normal_codes::ENABLE
                        && completed.action.arg == 0;
                    true
                }
                () => match take_pending_for_seq(ctrl, target) {
                    Some(pending) => {
                        // 匹配的 Submitted 先完成 timeout 对账；取消 tag 不吞独立 EStop。
                        publish_aborted(ctrl, pending.action);
                        true
                    }
                    None => false,
                },
            },
            _ => false,
        };
        let (kind, origin) = if valid {
            (CommErrorKind::PendingDeadline, FaultOrigin::PendingTimeout)
        } else {
            emit(
                diag,
                now,
                3,
                u8::from(active.valid),
                u8::from(completed.action.valid),
                seq_low(observed),
            );
            (CommErrorKind::Unknown, FaultOrigin::InvariantViolation)
        };
        let registered =
            self.enter_fault_impl(FaultCode::CommFault, kind.to_u8(), origin, ctrl, diag, now);
        // 关联必须来自上面实际处理的同一 observed，不能另读可能已经替换的 abort。
        let arranged = match request {
            Ok(Some(NormalAbortTarget::Submitted { seq }))
                if registered && active.valid && active.seq == seq =>
            {
                self.physical_recovery(Some(seq))
            }
            _ => None,
        };
        if exempt
            && self.round.as_ref().is_some_and(|round| !round.estop_bound)
            && !matches!(
                self.phase,
                ConvergePhase::SafetyStopFailedAwaitAck { .. } | ConvergePhase::SafetyStopFailed
            )
        {
            emit(diag, now, 6, 0, 0, seq_low(completed.action.seq));
            // 旧成功只免去额外 Stop，不给新 epoch 的放行许可；原轮预算不重置。
            self.transition(ConvergePhase::Confirm, diag, now);
        }
        // 即便 CAS 失败，本次义务已登记；新请求仍保留给下一 tick。
        before_clear();
        let _ = ctrl.clear_abort(observed);
        (true, arranged)
    }

    pub(crate) fn step<A: SerialRtAdapter>(
        &mut self,
        adapter: &mut A,
        ctrl: &HardRtCtrlInner,
        probe: &SerialPreemptProbe,
        diag: &dyn DiagSink,
        clock: &dyn RobotClock,
        torque_mask: &mut TorqueEnableMask,
    ) {
        let now = clock.now();
        if self.initialization == Initialization::Fresh {
            self.initialization = Initialization::Started;
            emit(diag, now, 17, u8::from(self.authorized), 0, 0);
            ctrl.tighten_inhibits(inhibit::BOOT | inhibit::CONVERGING);
            if !self.authorized {
                self.enter_fault(
                    FaultCode::CommFault,
                    CommErrorKind::Unknown.to_u8(),
                    FaultOrigin::BootUntrusted,
                    ctrl,
                    diag,
                    now,
                );
            }
            if self.joint_count == 0
                || self.joint_count > MAX_JOINTS
                || adapter.joint_count() != self.joint_count
            {
                emit(
                    diag,
                    now,
                    3,
                    2,
                    0,
                    u32::try_from(self.joint_count).unwrap_or(u32::MAX),
                );
                self.fail_stop(
                    SafetyStopFailDetail::StopNotDelivered {
                        kind: CommErrorKind::Unknown,
                    },
                    FaultOrigin::InvariantViolation,
                    ctrl,
                    diag,
                    now,
                );
            }
        }
        if self.boot_timed_out(ctrl, now)
            && self.stop_attempt > 0
            && matches!(
                self.phase,
                ConvergePhase::StopRetry | ConvergePhase::Connect | ConvergePhase::Confirm
            )
        {
            self.finish_boot_timeout(ctrl, diag, now);
            return;
        }
        match self.phase {
            ConvergePhase::StopIssue | ConvergePhase::StopRetry => {
                self.step_stop(adapter, ctrl, probe, diag, clock);
            }
            ConvergePhase::Connect => self.step_connect(adapter, ctrl, probe, diag, clock),
            ConvergePhase::Confirm => {
                self.step_confirm(adapter, ctrl, probe, diag, clock, torque_mask);
            }
            ConvergePhase::AwaitAck => {
                let complete = self
                    .round
                    .as_ref()
                    .is_some_and(|round| round.completed_safety.is_some());
                if complete
                    && !self.generation_saturated
                    && let Some(epoch) = self.release_epoch
                    && let Some(released) = ctrl.clear_acked_fault_pending_at(epoch)
                {
                    self.release_epoch = Some(released);
                    if self.finish_boot(BootOutcome::Faulted, ctrl, now) {
                        self.transition(ConvergePhase::Idle, diag, now);
                    }
                }
            }
            ConvergePhase::SafetyStopFailedAwaitAck { generation, .. } => {
                if generation != 0
                    && !self.generation_saturated
                    && let Some(epoch) = self.release_epoch
                    && let Some(released) = ctrl.clear_acked_fault_pending_at(epoch)
                {
                    self.release_epoch = Some(released);
                    if self.finish_boot(BootOutcome::Faulted, ctrl, now) {
                        self.transition(ConvergePhase::SafetyStopFailed, diag, now);
                    }
                }
            }
            ConvergePhase::Idle | ConvergePhase::SafetyStopFailed => {}
        }
    }

    fn step_stop<A: SerialRtAdapter>(
        &mut self,
        adapter: &mut A,
        ctrl: &HardRtCtrlInner,
        probe: &SerialPreemptProbe,
        diag: &dyn DiagSink,
        clock: &dyn RobotClock,
    ) {
        let now = clock.now();
        ctrl.tighten_safety(OutputSafetyState::StopCommanded);
        ctrl.tighten_inhibits(inhibit::CONVERGING);
        if self.phase == ConvergePhase::StopIssue
            && self.round.as_ref().is_some_and(|round| round.estop_bound)
        {
            tighten_estop_policy(&ctrl.policy);
        }
        self.stop_attempt += 1;
        self.counters.stop_writes += 1;
        probe.set_context(
            BusOpClass::Safety,
            InstalledIntent::None,
            0,
            MonoRawNs(now.0.saturating_add(CALL_BUDGET_NS)),
            false,
        );
        #[cfg(feature = "test-comm-fault-hook")]
        ctrl.trace_now(
            crate::comm_fault_trace::TraceKind::SafetyStart,
            u64::from(self.stop_attempt),
            0,
        );
        let result = adapter.safety_stop();
        let now = clock.now();
        #[cfg(feature = "test-comm-fault-hook")]
        crate::comm_fault_trace::emit(
            crate::comm_fault_trace::TraceKind::SafetyReturn,
            now.0,
            ctrl.trace_context(),
            u64::from(self.stop_attempt),
            u64::from(result.is_ok()),
        );
        match result {
            Ok(()) if self.boot_timed_out(ctrl, now) => self.finish_boot_timeout(ctrl, diag, now),
            Ok(()) => self.transition(
                if self.connect_pending {
                    ConvergePhase::Connect
                } else {
                    ConvergePhase::Confirm
                },
                diag,
                now,
            ),
            Err(error) => {
                let kind = comm_kind(error).unwrap_or(CommErrorKind::Unknown);
                if comm_kind(error).is_some() {
                    self.observe_comm(kind, ctrl, diag, now);
                }
                self.last_kind = kind;
                emit(diag, now, 7, self.stop_attempt, kind.to_u8(), 0);
                if self.stop_attempt >= MAX_STOP_ATTEMPTS {
                    self.fail_stop(
                        SafetyStopFailDetail::StopNotDelivered { kind },
                        FaultOrigin::StopNotDelivered,
                        ctrl,
                        diag,
                        now,
                    );
                } else {
                    self.transition(ConvergePhase::StopRetry, diag, now);
                }
            }
        }
    }

    fn step_connect<A: SerialRtAdapter>(
        &mut self,
        adapter: &mut A,
        ctrl: &HardRtCtrlInner,
        probe: &SerialPreemptProbe,
        diag: &dyn DiagSink,
        clock: &dyn RobotClock,
    ) {
        let now = clock.now();
        let budget = (self.joint_count as u64).saturating_mul(4 * CALL_BUDGET_NS);
        probe.set_context(
            BusOpClass::Connect,
            InstalledIntent::None,
            0,
            MonoRawNs(now.0.saturating_add(budget)),
            false,
        );
        self.connect_pending = false;
        let result = adapter.connect();
        let now = clock.now();
        if let Err(error) = result {
            observe_device_error(error, ctrl);
            let (code, detail) = fault_for(error);
            self.enter_fault(code, detail, FaultOrigin::ConnectFailed, ctrl, diag, now);
        }
        emit(
            diag,
            now,
            8,
            u8::from(result.is_ok()),
            self.last_kind.to_u8(),
            0,
        );
        if self.boot_timed_out(ctrl, now) {
            self.finish_boot_timeout(ctrl, diag, now);
        } else {
            self.transition(ConvergePhase::Confirm, diag, now);
        }
    }

    fn step_confirm<A: SerialRtAdapter>(
        &mut self,
        adapter: &mut A,
        ctrl: &HardRtCtrlInner,
        probe: &SerialPreemptProbe,
        diag: &dyn DiagSink,
        clock: &dyn RobotClock,
        torque_mask: &mut TorqueEnableMask,
    ) {
        let now = clock.now();
        if self
            .round
            .as_ref()
            .is_some_and(|round| round.attempt >= MAX_CONFIRM_ATTEMPTS)
        {
            self.finish_exhausted(ctrl, diag, now);
            return;
        }
        probe.set_context(
            BusOpClass::Confirm,
            InstalledIntent::None,
            0,
            MonoRawNs(now.0.saturating_add(CONFIRM_BUDGET_NS)),
            self.evidence_trusted(ctrl),
        );
        let observation = ctrl
            .authority()
            .begin_safety_observation(SafetyOperation::ConfirmOff);
        let result = adapter.read_torque_enable_mask(&mut torque_mask[..self.joint_count]);
        let now = clock.now();
        #[cfg(feature = "test-comm-fault-hook")]
        crate::comm_fault_trace::emit(
            crate::comm_fault_trace::TraceKind::ConfirmReturn,
            now.0,
            ctrl.trace_context(),
            torque_mask[..self.joint_count]
                .iter()
                .filter(|enabled| **enabled)
                .count() as u64,
            u64::from(result.is_ok()),
        );
        // hygiene 与 probe 是同一次确认。先消费事实、收紧信任，再解释 all-off。
        let hygiene_error = probe.take_comm_error();
        if let Some(round) = &mut self.round {
            round.attempt += 1;
        }
        if let Some(kind) = hygiene_error {
            self.confirm_comm_error(kind, ctrl, diag, now);
        }
        if let Err(error) = result {
            observe_device_error(error, ctrl);
            if matches!(
                error,
                RobotError::InvalidParameter(_) | RobotError::Unsupported(_)
            ) {
                emit(diag, now, 3, 4, 0, 0);
            }
            self.last_kind = comm_kind(error).unwrap_or(CommErrorKind::Unknown);
            if let Some(kind) = comm_kind(error) {
                self.confirm_comm_error(kind, ctrl, diag, now);
            }
        }
        // AdapterErr → CommFault 已重开下一轮，旧 probe 不得完成新轮次。
        if self.phase != ConvergePhase::Confirm {
            return;
        }
        if self.boot_timed_out(ctrl, now) {
            self.finish_boot_timeout(ctrl, diag, now);
            return;
        }
        match result {
            Ok(()) => {
                let on = u8::try_from(
                    torque_mask[..self.joint_count]
                        .iter()
                        .filter(|enabled| **enabled)
                        .count(),
                )
                .expect("已校验关节数不超过32");
                if on == 0 {
                    #[cfg(feature = "test-comm-fault-hook")]
                    if self.evidence_trusted(ctrl) {
                        crate::comm_fault_trace::emit(
                            crate::comm_fault_trace::TraceKind::AllOffTrusted,
                            now.0,
                            ctrl.trace_context(),
                            0,
                            0,
                        );
                    }
                    self.finish_observation(
                        self.evidence_trusted(ctrl),
                        Some(observation),
                        ctrl,
                        diag,
                        now,
                    );
                } else {
                    self.still_on = on;
                    if let Some(round) = &mut self.round {
                        round.some_on_seen = true;
                    }
                    self.finish_exhausted(ctrl, diag, now);
                }
            }
            Err(_) => self.finish_exhausted(ctrl, diag, now),
        }
    }

    fn confirm_comm_error(
        &mut self,
        kind: CommErrorKind,
        ctrl: &HardRtCtrlInner,
        diag: &dyn DiagSink,
        now: MonoRawNs,
    ) {
        self.observe_comm(kind, ctrl, diag, now);
        if self
            .round
            .as_ref()
            .is_some_and(|round| round.ctx != ConvergeContext::Preflight)
        {
            self.enter_fault(
                FaultCode::CommFault,
                kind.to_u8(),
                FaultOrigin::Confirm,
                ctrl,
                diag,
                now,
            );
        }
    }

    fn finish_exhausted(&mut self, ctrl: &HardRtCtrlInner, diag: &dyn DiagSink, now: MonoRawNs) {
        let Some(round) = self.round else {
            return;
        };
        if round.attempt < MAX_CONFIRM_ATTEMPTS {
            return;
        }
        if round.some_on_seen {
            self.fail_stop(
                SafetyStopFailDetail::StopFailed {
                    still_on: self.still_on,
                },
                FaultOrigin::Confirm,
                ctrl,
                diag,
                now,
            );
        } else {
            self.finish_observation(false, None, ctrl, diag, now);
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "消费原物理观测后沿同一代次释放抑制并决定 ACK 或启动出口"
    )]
    fn finish_observation(
        &mut self,
        trusted: bool,
        observation: Option<SafetyObservation>,
        ctrl: &HardRtCtrlInner,
        diag: &dyn DiagSink,
        now: MonoRawNs,
    ) {
        let Some(round) = self.round else {
            return;
        };
        let confirmed_epoch = if trusted {
            match observation {
                Some(observation) => {
                    let epoch = observation.epoch();
                    if ctrl
                        .authority()
                        .complete_safety_observation(observation)
                        .is_err()
                    {
                        emit(diag, now, 3, 5, 0, 0);
                        return;
                    }
                    Some(epoch)
                }
                None => return,
            }
        } else {
            None
        };
        if trusted && round.ctx == ConvergeContext::Preflight {
            ctrl.rx_trust.store(true, Ordering::Release);
            emit(diag, now, 9, 1, 0, 0);
        }
        if !trusted {
            emit(diag, now, 12, 0, self.last_kind.to_u8(), 0);
        }
        let completion = if trusted {
            ConvergeCompletion::AllOffTrusted
        } else {
            ConvergeCompletion::AdapterUncertain {
                kind: self.last_kind,
            }
        };
        self.release_epoch = if trusted {
            confirmed_epoch
        } else {
            Some(
                ctrl.authority()
                    .tighten_safety(
                        robot_core::control_authority::SafetyTightening::AdapterUncertain,
                    )
                    .tightened
                    .epoch,
            )
        };
        if !trusted && round.estop_bound {
            self.fail_stop(
                SafetyStopFailDetail::AdapterUncertain {
                    kind: self.last_kind,
                },
                FaultOrigin::Confirm,
                ctrl,
                diag,
                now,
            );
            return;
        }
        if trusted {
            self.counters.confirmed += 1;
            if self.fault.is_none() {
                ctrl.comm_level.store(0, Ordering::Release);
                ctrl.l0_since_ns.store(now.0, Ordering::Release);
            }
        } else {
            self.counters.uncertain += 1;
        }
        if !trusted && round.ctx == ConvergeContext::Preflight {
            let code = if ctrl.session_clean.load(Ordering::Acquire) {
                FaultCode::AdapterErr
            } else {
                FaultCode::CommFault
            };
            let detail = match code {
                FaultCode::CommFault => self.last_kind.to_u8(),
                _ => 0,
            };
            self.enter_fault(code, detail, FaultOrigin::PreflightFailed, ctrl, diag, now);
        }
        if let Some(round) = &mut self.round {
            round.completed_safety = Some(completion);
        }
        emit(diag, now, 15, ctrl.safety_state().to_u8(), 0, 0);
        if self.boot_outcome == BootOutcome::Pending {
            emit(diag, now, 13, u8::from(trusted), 0, 0);
        }
        let Some(epoch) = self.release_epoch else {
            return;
        };
        let Some(released) = release_inhibit_at(ctrl, InhibitReason::Converging, epoch) else {
            return;
        };
        self.release_epoch = Some(released);
        let has_gen = self
            .round
            .as_ref()
            .is_some_and(|round| round.generation.is_some());
        if has_gen || self.generation_saturated {
            self.mark_boot_exit(ctrl, now);
            self.transition(ConvergePhase::AwaitAck, diag, now);
        } else if self.finish_boot(BootOutcome::Passed, ctrl, now) {
            self.transition(ConvergePhase::Idle, diag, now);
        }
    }

    fn fail_stop(
        &mut self,
        detail: SafetyStopFailDetail,
        origin: FaultOrigin,
        ctrl: &HardRtCtrlInner,
        diag: &dyn DiagSink,
        now: MonoRawNs,
    ) {
        self.enter_fault(
            FaultCode::SafetyStopFailed,
            detail.to_u8(),
            origin,
            ctrl,
            diag,
            now,
        );
    }

    fn boot_timed_out(&self, ctrl: &HardRtCtrlInner, now: MonoRawNs) -> bool {
        self.boot_outcome == BootOutcome::Pending
            && (ctrl.boot_deadline_expired.load(Ordering::Acquire)
                || now.0
                    >= self
                        .spawned_at
                        .0
                        .saturating_add(
                            (self.joint_count as u64).saturating_mul(4 * CALL_BUDGET_NS),
                        )
                        .saturating_add(2_500_000_000))
    }

    fn finish_boot_timeout(&mut self, ctrl: &HardRtCtrlInner, diag: &dyn DiagSink, now: MonoRawNs) {
        self.last_kind = CommErrorKind::Unknown;
        emit(diag, now, 10, 0, 0, 0);
        // 未获得完整确认只能记录不确定，不把跳过的 probe 伪装成 all-off。
        if let Some(round) = &mut self.round {
            round.completed_safety = Some(ConvergeCompletion::AdapterUncertain {
                kind: CommErrorKind::Unknown,
            });
        }
        if self.round.as_ref().is_some_and(|round| round.estop_bound) {
            self.fail_stop(
                SafetyStopFailDetail::AdapterUncertain {
                    kind: CommErrorKind::Unknown,
                },
                FaultOrigin::PreflightTimeout,
                ctrl,
                diag,
                now,
            );
        } else {
            self.enter_fault(
                FaultCode::CommFault,
                CommErrorKind::Unknown.to_u8(),
                FaultOrigin::PreflightTimeout,
                ctrl,
                diag,
                now,
            );
            self.finish_observation(false, None, ctrl, diag, now);
        }
    }

    fn mark_boot_exit(&mut self, ctrl: &HardRtCtrlInner, now: MonoRawNs) {
        if self.boot_outcome == BootOutcome::Pending && self.first_boot_exit.is_none() {
            self.first_boot_exit = Some(now);
            // 两个原子支持时间零；重开物理轮不得刷新首次出口或 ACK 截止。
            ctrl.boot_first_exit_ns.store(now.0, Ordering::Relaxed);
            ctrl.boot_exit_reached.store(true, Ordering::Release);
        }
    }

    fn finish_boot(
        &mut self,
        outcome: BootOutcome,
        ctrl: &HardRtCtrlInner,
        now: MonoRawNs,
    ) -> bool {
        if self.boot_outcome == BootOutcome::Pending {
            let Some(epoch) = self.release_epoch else {
                return false;
            };
            let Some(released) = release_inhibit_at(ctrl, InhibitReason::Boot, epoch) else {
                return false;
            };
            self.release_epoch = Some(released);
            self.boot_outcome = outcome;
            // 观测值先发布，随后真实 outcome 的 Release 完成启动提交；READY 的 Acquire
            // 观察到 outcome 后，父端不会读到缺失的完成时间。首次带 gen 出口保持独立。
            #[cfg(feature = "test-comm-fault-hook")]
            crate::comm_fault_hook::record_preflight_finished(now.0);
            #[cfg(not(feature = "test-comm-fault-hook"))]
            let _ = now;
            ctrl.boot_outcome.store(outcome as u8, Ordering::Release);
        }
        true
    }
}

/// 本模块已核真实步骤/ACK 后，仍拒绝其它 epoch 的解除许可。
fn release_inhibit_at(
    ctrl: &HardRtCtrlInner,
    reason: InhibitReason,
    epoch: PermissionEpoch,
) -> Option<PermissionEpoch> {
    let snapshot = ctrl.authority().snapshot();
    if snapshot.epoch != epoch || snapshot.exhausted {
        return None;
    }
    if !snapshot.inhibits.contains(reason) {
        return Some(epoch);
    }
    let permit = match ctrl.authority().capture_inhibit_release(reason) {
        Ok(permit) if permit.epoch() == epoch => permit,
        _ => return None,
    };
    ctrl.authority()
        .try_release_inhibit(permit)
        .ok()
        .map(|released| released.epoch)
}

fn publish_aborted(ctrl: &HardRtCtrlInner, action: ActionRecord) {
    publish_completion(ctrl, action, AdapterResp::Err(AdapterErr::AbortedByTimeout));
}

pub(crate) fn publish_completion(
    ctrl: &HardRtCtrlInner,
    action: ActionRecord,
    response: AdapterResp,
) {
    ctrl.completed_effect.store(
        CompletedEffect {
            action,
            success: response == AdapterResp::Ok,
        }
        .pack(),
        Ordering::Relaxed,
    );
    ctrl.active_action.store(0, Ordering::Relaxed);
    ctrl.last_completed
        .store(pack_completed(action.seq, response), Ordering::Release);
}

fn take_pending_for_seq(ctrl: &HardRtCtrlInner, seq: u64) -> Option<SerialPending> {
    let mut raw = ctrl.pending_normal.load(Ordering::Acquire);
    loop {
        let pending = decode_serial_pending(raw).ok().flatten()?;
        if pending.action.seq != seq {
            return None;
        }
        match ctrl
            .pending_normal
            .compare_exchange(raw, 0, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return Some(pending),
            Err(actual) => raw = actual,
        }
    }
}

pub(crate) fn comm_kind(error: RobotError) -> Option<CommErrorKind> {
    match error {
        RobotError::Comm(kind) => Some(kind),
        RobotError::DeadlineExceeded => Some(CommErrorKind::Timeout),
        RobotError::NotConnected => Some(CommErrorKind::Disconnected),
        _ => None,
    }
}

pub(crate) fn fault_for(error: RobotError) -> (FaultCode, u8) {
    match comm_kind(error) {
        Some(kind) => (FaultCode::CommFault, kind.to_u8()),
        None => (FaultCode::AdapterErr, 0),
    }
}

/// L1 仅用于设备诊断；后到的设备状态不能把已定级的 L2 通信故障降级。
pub(crate) fn observe_device_error(error: RobotError, ctrl: &HardRtCtrlInner) {
    if matches!(error, RobotError::HardwareFault(_)) {
        let _ = ctrl
            .comm_level
            .compare_exchange(0, 1, Ordering::Release, Ordering::Relaxed);
    }
}

const fn severity(code: FaultCode) -> u8 {
    match code {
        FaultCode::SafetyStopFailed => 3,
        FaultCode::CommFault => 2,
        _ => 1,
    }
}

/// site 20 归收敛机；kind：0相位、1信任丢失、2故障、3不变量、4急停claim、
/// 5急停合并、6退场豁免、7停止重试、8连接结果、9可信确认、10启动超时、
/// 11急停义务、12不可信确认、13启动出口、14升级、15收敛结果、16中止、17启动上下文。
/// 所有内容固定宽度，不在串口执行线程格式化。
pub(crate) fn emit(diag: &dyn DiagSink, now: MonoRawNs, kind: u8, a: u8, b: u8, seq_lo: u32) {
    diag.record(DiagEvent {
        stamp_ns: now.0,
        site: 20,
        kind,
        a,
        b,
        seq_lo,
    });
}

/// 诊断协议只保留序号低 32 位，不把截断后的数值用于请求匹配。
pub(crate) fn seq_low(sequence: u64) -> u32 {
    let bytes = sequence.to_le_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::SerialDiagRing;
    use crate::hard_rt_ctrl::HardRtCtrlClient;
    use robot_adapter_mock::MockHardRTAdapter;
    use robot_core::executor_ctrl::SerialEmergencyCtrl;
    use robot_core::time::MockClock;
    use std::sync::Arc;

    #[test]
    fn bound_boot_timeout_records_uncertainty_before_or_after_the_first_safety() {
        for after_first_safety in [false, true] {
            let clock = Arc::new(MockClock::default());
            let ctrl = Arc::new(HardRtCtrlInner::new_serial(
                crate::hard_rt_ctrl::test_authority(true),
            ));
            let probe = SerialPreemptProbe::new(clock.clone(), ctrl.clone());
            let diag = SerialDiagRing::new();
            let mut convergence = SerialConvergence::new(true, 2, clock.now());
            let mut adapter = MockHardRTAdapter::with_unlimited_limits(2);
            let mut torque = TorqueEnableMask::default();
            HardRtCtrlClient::new(ctrl.clone()).estop_monotonic();
            convergence.claim_estop(&ctrl, &diag, clock.now());
            if after_first_safety {
                convergence.step(
                    &mut adapter,
                    &ctrl,
                    &probe,
                    &diag,
                    clock.as_ref(),
                    &mut torque,
                );
            }
            ctrl.boot_deadline_expired.store(true, Ordering::Release);
            convergence.step(
                &mut adapter,
                &ctrl,
                &probe,
                &diag,
                clock.as_ref(),
                &mut torque,
            );
            assert!(matches!(
                convergence.phase(),
                ConvergePhase::SafetyStopFailedAwaitAck { .. }
            ));
            assert_eq!(
                convergence.round().unwrap().completed_safety,
                Some(ConvergeCompletion::AdapterUncertain {
                    kind: CommErrorKind::Unknown
                })
            );
            assert_eq!(convergence.counters().stop_writes, 1);
            assert!(ctrl.boot_exit_reached.load(Ordering::Acquire));
            assert_eq!(ctrl.boot_first_exit_ns.load(Ordering::Relaxed), 0);
            assert_eq!(convergence.first_boot_exit(), Some(MonoRawNs(0)));
        }
    }
}
