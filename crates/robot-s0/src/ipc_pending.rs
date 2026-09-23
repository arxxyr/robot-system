//! IPC 待办的唯一请求责任、首次业务锁存与受检 Enable 退休桥。
//!
//! seq 与业务观察只记录来源；本模块不提交 adapter、不清 seq、不回包。
//! owner 退休也不代表原请求或总线动作已完成，生产接线另行验收。

// 完整返还固定 16 个 waiter 的责任，不能用 Box 降低错误大小或额外分配。
#![allow(clippy::result_large_err, reason = "拒绝必须按值返还全部固定请求责任")]

use crate::hard_rt_ctrl::HardRtCtrlClient;
use robot_core::control_authority::{
    AuthoritySnapshot, ControlAuthority, DomainId, EnableOwner, PermissionEpoch, Phase, Refusal,
    StartViewSlot,
};
use robot_core::executor_ctrl::{AdapterResp, ExecutorCtrlClient};
use robot_core::time::MonoRawNs;

use crate::event_ingress::RR_CHANNEL_CAPACITY;
use crate::ipc_thread::{PendingTarget, SetEnabledRrPayload};
use crate::state_machine::{EnabledSub, FaultCode, PlannedTransition, S0State};

pub const MAX_CLIENTS_SUPPORTED: usize = RR_CHANNEL_CAPACITY / 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevokeCause {
    Disable,
    Shutdown,
    Emergency,
    Fault,
}

/// 请求身份只能随责任 move，不能复制成第二个可退休请求。
///
/// ```compile_fail
/// use robot_s0::ipc_pending::RrIdentity;
/// fn duplicate(rr: RrIdentity) { let _ = rr.clone(); }
/// ```
#[derive(Debug)]
pub struct RrIdentity {
    pub request_id: String,
    pub deadline_ns: u64,
    pub route: crate::rr_admission::SlotIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingOutcome {
    Existing(SetEnabledRrPayload),
    SupersededByDisable,
}

#[derive(Debug)]
pub struct LockedOutcome {
    outcome: PendingOutcome,
    at: MonoRawNs,
}
impl LockedOutcome {
    #[must_use]
    pub const fn outcome(&self) -> &PendingOutcome {
        &self.outcome
    }
    #[must_use]
    pub const fn at(&self) -> MonoRawNs {
        self.at
    }
}

/// 只为新接纳的 Safe Disable 保存历史 latch 边界；不是执行或退休许可。
#[derive(Debug)]
struct FreshDisableBoundary {
    domain: DomainId,
    epoch: PermissionEpoch,
    observation: BusinessObservation,
}

#[derive(Debug)]
pub struct PendingReply {
    identity: RrIdentity,
    locked: Option<LockedOutcome>,
    fresh_disable: Option<FreshDisableBoundary>,
}
impl PendingReply {
    #[must_use]
    pub const fn identity(&self) -> &RrIdentity {
        &self.identity
    }
    #[must_use]
    pub const fn locked(&self) -> Option<&LockedOutcome> {
        self.locked.as_ref()
    }

    fn observe(&mut self, target: PendingTarget, observation: BusinessObservation) {
        if self.locked.is_some() {
            return;
        }
        if let Some(boundary) = &mut self.fresh_disable {
            // 实际推进同快照判失败；不能在原 IO/seq 未结束时先锁 Safe 成功。
            boundary.observation = observation;
        } else {
            self.locked =
                decide_business(target, self.identity.deadline_ns, observation).map(|payload| {
                    LockedOutcome {
                        outcome: PendingOutcome::Existing(payload),
                        at: observation.now,
                    }
                });
        }
    }
}

#[derive(Debug)]
pub struct LowerWaiters {
    entries: [Option<PendingReply>; MAX_CLIENTS_SUPPORTED],
}
impl LowerWaiters {
    const fn empty() -> Self {
        Self {
            entries: [const { None }; MAX_CLIENTS_SUPPORTED],
        }
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.iter().count()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }
    pub fn iter(&self) -> impl Iterator<Item = &PendingReply> {
        self.entries.iter().flatten()
    }

    fn observe(&mut self, observation: BusinessObservation) {
        for reply in self.entries.iter_mut().flatten() {
            reply.observe(PendingTarget::Safe, observation);
        }
    }

    fn observe_fresh_failures(&mut self, snapshot: AuthoritySnapshot, now: MonoRawNs) {
        for reply in self.entries.iter_mut().flatten() {
            if reply.locked.is_some() {
                continue;
            }
            let Some(boundary) = &reply.fresh_disable else {
                continue;
            };
            let history_only = snapshot.epoch == boundary.epoch && !snapshot.exhausted;
            let observation = BusinessObservation {
                now,
                state: if snapshot.shutdown {
                    S0State::Stopping
                } else {
                    boundary.observation.state
                },
                estop: !history_only && (snapshot.latch || boundary.observation.estop),
            };
            let payload =
                decide_business(PendingTarget::Safe, reply.identity.deadline_ns, observation);
            // Safe 是业务状态，不是物理退休。成功只由 advance_lower 原完整门给出。
            let failure = match payload {
                Some(SetEnabledRrPayload::Disabled { .. }) | None => {
                    if now.0 > reply.identity.deadline_ns {
                        Some(SetEnabledRrPayload::DeadlineExceeded)
                    } else {
                        None
                    }
                }
                other => other,
            };
            if let Some(payload) = failure {
                reply.locked = Some(LockedOutcome {
                    outcome: PendingOutcome::Existing(payload),
                    at: now,
                });
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LowerCause {
    OperatorDisable,
    PostEnableCleanup,
    AwaitConvergence,
    Shutdown,
}

/// 同时持有唯一 A2 owner、原 Enable RR 与独立 Lower waiters。
///
/// ```compile_fail
/// use robot_s0::ipc_pending::EnablePending;
/// fn duplicate(pending: EnablePending) { let _ = pending.clone(); }
/// ```
#[derive(Debug)]
pub struct EnablePending {
    owner: EnableOwner,
    view_epoch: PermissionEpoch,
    seq: Option<u64>,
    rr: Option<PendingReply>,
    lower_waiters: LowerWaiters,
    revoked_by: Option<RevokeCause>,
    started_at: MonoRawNs,
    bridge: EnableBridgeState,
}
impl EnablePending {
    #[must_use]
    pub const fn owner(&self) -> &EnableOwner {
        &self.owner
    }
    #[must_use]
    pub const fn view_epoch(&self) -> PermissionEpoch {
        self.view_epoch
    }
    #[must_use]
    pub const fn seq(&self) -> Option<u64> {
        self.seq
    }
    #[must_use]
    pub const fn rr(&self) -> Option<&PendingReply> {
        self.rr.as_ref()
    }
    #[must_use]
    pub const fn lower_waiters(&self) -> &LowerWaiters {
        &self.lower_waiters
    }
    #[must_use]
    pub const fn revoked_by(&self) -> Option<RevokeCause> {
        self.revoked_by
    }
    #[must_use]
    pub const fn started_at(&self) -> MonoRawNs {
        self.started_at
    }

    fn observe(&mut self, observation: BusinessObservation) {
        if let Some(rr) = &mut self.rr {
            rr.observe(PendingTarget::EnabledActive, observation);
        }
        self.lower_waiters.observe(observation);
    }
}

#[derive(Debug)]
pub struct ControlPending {
    plan: PlannedTransition,
    seq: Option<u64>,
    started_at: MonoRawNs,
}
impl ControlPending {
    #[must_use]
    pub const fn plan(&self) -> &PlannedTransition {
        &self.plan
    }
    #[must_use]
    pub const fn seq(&self) -> Option<u64> {
        self.seq
    }
    #[must_use]
    pub const fn started_at(&self) -> MonoRawNs {
        self.started_at
    }
}

/// 仅消费同一控制通道实际公布的原序号结果；查询未完成完整返原。
pub(crate) fn finish_control_owned(
    pending: ControlPending,
    ctrl: &HardRtCtrlClient,
) -> Result<(PlannedTransition, AdapterResp), ControlPending> {
    match pending.seq.and_then(|seq| ctrl.poll_last_normal_resp(seq)) {
        Some(response) => Ok((pending.plan, response)),
        None => Err(pending),
    }
}

/// 未投递的旧控制计划可撤销；已有原序号必须继续持有到真实完成。
pub(crate) fn cancel_unsubmitted_control(
    pending: ControlPending,
) -> Result<PlannedTransition, ControlPending> {
    match pending.seq {
        None => Ok(pending.plan),
        Some(_) => Err(pending),
    }
}

/// 只由真实 A2 退休成功产生，仍完整持有旧 seq 与请求责任。
///
/// ```compile_fail
/// use robot_s0::ipc_pending::RetiredEnablePending;
/// fn duplicate(pending: RetiredEnablePending) { let _ = pending.clone(); }
/// ```
///
/// ```compile_fail
/// use robot_core::control_authority::{DomainId, PermissionEpoch, Phase};
/// use robot_core::time::MonoRawNs;
/// use robot_s0::ipc_pending::{LowerWaiters, RetiredEnablePending};
/// fn forge(domain: DomainId, waiters: LowerWaiters) -> RetiredEnablePending {
///     RetiredEnablePending { domain, view_epoch: PermissionEpoch::INITIAL,
///         source_seq: None, rr: None, lower_waiters: waiters, revoked_by: None,
///         started_at: MonoRawNs(1), retired_from: Phase::EnableQueued, withdrew_start: true }
/// }
/// ```
#[derive(Debug)]
pub struct RetiredEnablePending {
    domain: DomainId,
    view_epoch: PermissionEpoch,
    source_seq: Option<u64>,
    rr: Option<PendingReply>,
    lower_waiters: LowerWaiters,
    revoked_by: Option<RevokeCause>,
    started_at: MonoRawNs,
    retired_from: Phase,
    withdrew_start: bool,
    bridge: EnableBridgeState,
}
impl RetiredEnablePending {
    #[must_use]
    pub const fn domain(&self) -> DomainId {
        self.domain
    }
    #[must_use]
    pub const fn view_epoch(&self) -> PermissionEpoch {
        self.view_epoch
    }
    #[must_use]
    pub const fn source_seq(&self) -> Option<u64> {
        self.source_seq
    }
    #[must_use]
    pub const fn rr(&self) -> Option<&PendingReply> {
        self.rr.as_ref()
    }
    #[must_use]
    pub const fn lower_waiters(&self) -> &LowerWaiters {
        &self.lower_waiters
    }
    #[must_use]
    pub const fn revoked_by(&self) -> Option<RevokeCause> {
        self.revoked_by
    }
    #[must_use]
    pub const fn started_at(&self) -> MonoRawNs {
        self.started_at
    }
    #[must_use]
    pub const fn retired_from(&self) -> Phase {
        self.retired_from
    }
    #[must_use]
    pub const fn withdrew_start(&self) -> bool {
        self.withdrew_start
    }
}

#[derive(Debug)]
pub struct LowerPending {
    seq: Option<u64>,
    cause: LowerCause,
    carried_enable: Option<RetiredEnablePending>,
    waiters: LowerWaiters,
    started_at: MonoRawNs,
    shutdown_generation: Option<std::num::NonZeroU64>,
    completion: Option<robot_core::executor_ctrl::AdapterResp>,
}
impl LowerPending {
    /// 仅供非 RT IPC 诊断读取；不复制 RR 身份，也不改变退休判据。
    pub(crate) fn diagnostic(&self, snapshot: AuthoritySnapshot) -> LowerDiagnostic {
        use robot_core::control_authority::InhibitReason;
        use robot_core::executor_ctrl::OutputSafetyState;

        let mut wait_mask = 0;
        if self
            .carried_enable
            .as_ref()
            .is_some_and(|carried| carried.source_seq.is_some())
        {
            wait_mask |= LowerDiagnostic::SOURCE_RESPONSE;
        }
        if self.seq.is_some() {
            wait_mask |= LowerDiagnostic::LOWER_RESPONSE;
        }
        if self.seq.is_none() && self.completion.is_none() {
            wait_mask |= LowerDiagnostic::LOWER_NOT_SUBMITTED;
        }
        if snapshot.phase != Some(Phase::Idle) {
            wait_mask |= LowerDiagnostic::PHASE_NOT_IDLE;
        }
        if snapshot.safety != OutputSafetyState::TorqueOffConfirmed {
            wait_mask |= LowerDiagnostic::TORQUE_OFF_UNCONFIRMED;
        }
        if !snapshot.disabled {
            wait_mask |= LowerDiagnostic::NOT_DISABLED;
        }
        if snapshot.exhausted {
            wait_mask |= LowerDiagnostic::EXHAUSTED;
        }
        if snapshot.normal_io_active {
            wait_mask |= LowerDiagnostic::NORMAL_IO_ACTIVE;
        }
        if snapshot.serial_estop_pending {
            wait_mask |= LowerDiagnostic::SERIAL_ESTOP_PENDING;
        }
        if snapshot.inhibits.contains(InhibitReason::Converging) {
            wait_mask |= LowerDiagnostic::INHIBIT_CONVERGING;
        }
        if snapshot.inhibits.contains(InhibitReason::FaultPending) {
            wait_mask |= LowerDiagnostic::INHIBIT_FAULT_PENDING;
        }
        if snapshot.inhibits.contains(InhibitReason::Boot) {
            wait_mask |= LowerDiagnostic::INHIBIT_BOOT;
        }
        if !snapshot.inhibits.is_empty() {
            wait_mask |= LowerDiagnostic::SUBMISSION_INHIBITED;
        }
        LowerDiagnostic {
            wait_mask,
            source_seq: self.carried_enable.as_ref().and_then(|c| c.source_seq),
            lower_seq: self.seq,
            phase: snapshot.phase,
            safety: snapshot.safety,
            inhibits: snapshot.inhibits.bits(),
        }
    }
    #[must_use]
    #[cfg(test)]
    pub(crate) const fn shutdown_generation(&self) -> Option<std::num::NonZeroU64> {
        self.shutdown_generation
    }
    #[must_use]
    pub const fn seq(&self) -> Option<u64> {
        self.seq
    }
    #[must_use]
    pub const fn cause(&self) -> LowerCause {
        self.cause
    }
    #[must_use]
    pub const fn carried_enable(&self) -> Option<&RetiredEnablePending> {
        self.carried_enable.as_ref()
    }
    #[must_use]
    pub const fn waiters(&self) -> &LowerWaiters {
        &self.waiters
    }
    #[must_use]
    pub const fn started_at(&self) -> MonoRawNs {
        self.started_at
    }
}

/// 位 0–2 为原响应/提交等待，3–11 为原物理退休门，12 为提交前全量 inhibit。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LowerDiagnostic {
    pub wait_mask: u16,
    pub source_seq: Option<u64>,
    pub lower_seq: Option<u64>,
    pub phase: Option<Phase>,
    pub safety: robot_core::executor_ctrl::OutputSafetyState,
    pub inhibits: u8,
}
impl LowerDiagnostic {
    pub(crate) const SOURCE_RESPONSE: u16 = 1 << 0;
    pub(crate) const LOWER_RESPONSE: u16 = 1 << 1;
    pub(crate) const LOWER_NOT_SUBMITTED: u16 = 1 << 2;
    pub(crate) const PHASE_NOT_IDLE: u16 = 1 << 3;
    pub(crate) const TORQUE_OFF_UNCONFIRMED: u16 = 1 << 4;
    pub(crate) const NOT_DISABLED: u16 = 1 << 5;
    pub(crate) const EXHAUSTED: u16 = 1 << 6;
    pub(crate) const NORMAL_IO_ACTIVE: u16 = 1 << 7;
    pub(crate) const SERIAL_ESTOP_PENDING: u16 = 1 << 8;
    pub(crate) const INHIBIT_CONVERGING: u16 = 1 << 9;
    pub(crate) const INHIBIT_FAULT_PENDING: u16 = 1 << 10;
    pub(crate) const INHIBIT_BOOT: u16 = 1 << 11;
    pub(crate) const SUBMISSION_INHIBITED: u16 = 1 << 12;
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant, reason = "固定容量责任记录不额外装箱分配")]
pub enum IpcPending {
    Idle,
    Enable(EnablePending),
    Lower(LowerPending),
    Control(ControlPending),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingClass {
    Lower,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingRefusal {
    WrongState,
    AlreadyInProgress,
    QueueFull { class: PendingClass },
    SeqAlreadyPresent,
    NoNormalRequest,
    EnableSequenceStillHeld,
    CleanupRequiresRetiredEnable,
}
#[derive(Debug)]
pub struct PendingRejected<T> {
    pub input: T,
    pub reason: PendingRefusal,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BusinessObservation {
    pub now: MonoRawNs,
    pub state: S0State,
    pub estop: bool,
}

#[must_use]
pub fn new_enable(
    owner: EnableOwner,
    rr: Option<RrIdentity>,
    started_at: MonoRawNs,
) -> EnablePending {
    EnablePending {
        view_epoch: owner.epoch(),
        owner,
        seq: None,
        rr: rr.map(|identity| PendingReply {
            identity,
            locked: None,
            fresh_disable: None,
        }),
        lower_waiters: LowerWaiters::empty(),
        revoked_by: None,
        started_at,
        bridge: EnableBridgeState::default(),
    }
}

pub fn new_lower(
    cause: LowerCause,
    started_at: MonoRawNs,
) -> Result<LowerPending, PendingRejected<(LowerCause, MonoRawNs)>> {
    match cause {
        LowerCause::PostEnableCleanup | LowerCause::Shutdown => Err(PendingRejected {
            input: (cause, started_at),
            reason: PendingRefusal::CleanupRequiresRetiredEnable,
        }),
        LowerCause::OperatorDisable | LowerCause::AwaitConvergence => Ok(LowerPending {
            seq: None,
            cause,
            carried_enable: None,
            waiters: LowerWaiters::empty(),
            started_at,
            shutdown_generation: None,
            completion: None,
        }),
    }
}

#[must_use]
pub const fn new_control(plan: PlannedTransition, started_at: MonoRawNs) -> ControlPending {
    ControlPending {
        plan,
        seq: None,
        started_at,
    }
}

pub fn install_pending(
    current: IpcPending,
    incoming: IpcPending,
) -> Result<IpcPending, PendingRejected<(IpcPending, IpcPending)>> {
    match (&current, &incoming) {
        (IpcPending::Idle, IpcPending::Idle) => Err(PendingRejected {
            input: (current, incoming),
            reason: PendingRefusal::WrongState,
        }),
        (IpcPending::Idle, _) => Ok(incoming),
        _ => Err(PendingRejected {
            input: (current, incoming),
            reason: PendingRefusal::AlreadyInProgress,
        }),
    }
}

pub fn append_lower_waiter(
    pending: IpcPending,
    rr: RrIdentity,
) -> Result<IpcPending, PendingRejected<(IpcPending, RrIdentity)>> {
    append_lower_waiter_impl(pending, rr, None, None)
}

fn append_lower_waiter_impl(
    mut pending: IpcPending,
    rr: RrIdentity,
    fresh_disable: Option<FreshDisableBoundary>,
    locked: Option<LockedOutcome>,
) -> Result<IpcPending, PendingRejected<(IpcPending, RrIdentity)>> {
    let waiters = match &mut pending {
        IpcPending::Enable(enable) => &mut enable.lower_waiters,
        IpcPending::Lower(lower) => &mut lower.waiters,
        IpcPending::Idle | IpcPending::Control(_) => {
            return Err(PendingRejected {
                input: (pending, rr),
                reason: PendingRefusal::WrongState,
            });
        }
    };
    match waiters.entries.iter_mut().find(|entry| entry.is_none()) {
        Some(entry) => {
            *entry = Some(PendingReply {
                identity: rr,
                locked,
                fresh_disable,
            });
            Ok(pending)
        }
        None => Err(PendingRejected {
            input: (pending, rr),
            reason: PendingRefusal::QueueFull {
                class: PendingClass::Lower,
            },
        }),
    }
}

pub fn record_submitted(
    mut pending: IpcPending,
    seq: u64,
) -> Result<IpcPending, PendingRejected<(IpcPending, u64)>> {
    let slot = match &mut pending {
        IpcPending::Enable(enable) => Ok(&mut enable.seq),
        IpcPending::Control(control) => Ok(&mut control.seq),
        IpcPending::Lower(lower) => match lower.cause {
            LowerCause::AwaitConvergence => Err(PendingRefusal::NoNormalRequest),
            LowerCause::OperatorDisable | LowerCause::PostEnableCleanup | LowerCause::Shutdown => {
                match &lower.carried_enable {
                    Some(carried) if carried.source_seq.is_some() => {
                        Err(PendingRefusal::EnableSequenceStillHeld)
                    }
                    _ => Ok(&mut lower.seq),
                }
            }
        },
        IpcPending::Idle => Err(PendingRefusal::WrongState),
    };
    let reason = match slot {
        Ok(slot) if slot.is_none() => {
            *slot = Some(seq);
            return Ok(pending);
        }
        Ok(_) => PendingRefusal::SeqAlreadyPresent,
        Err(reason) => reason,
    };
    Err(PendingRejected {
        input: (pending, seq),
        reason,
    })
}

/// 固定优先级：EStop、Stopping、指定 Fault、目标态、严格超过 deadline。
#[must_use]
pub fn decide_business(
    target: PendingTarget,
    deadline_ns: u64,
    observation: BusinessObservation,
) -> Option<SetEnabledRrPayload> {
    match (observation.estop, observation.state, target) {
        (true, _, _) => Some(SetEnabledRrPayload::PreemptedByEStop),
        (_, S0State::Stopping, _) => Some(SetEnabledRrPayload::PreemptedByShutdown),
        (
            _,
            S0State::Fault(
                FaultCode::AdapterErr | FaultCode::CommFault | FaultCode::SafetyStopFailed,
            ),
            _,
        ) => Some(SetEnabledRrPayload::AdapterError),
        (
            _,
            S0State::Enabled {
                sub: EnabledSub::Active,
                ..
            },
            PendingTarget::EnabledActive,
        ) => Some(SetEnabledRrPayload::Enabled {
            completed_at_ns: observation.now.0,
        }),
        (_, S0State::Safe, PendingTarget::Safe) => Some(SetEnabledRrPayload::Disabled {
            completed_at_ns: observation.now.0,
        }),
        _ if observation.now.0 > deadline_ns => Some(SetEnabledRrPayload::DeadlineExceeded),
        _ => None,
    }
}

#[must_use]
pub fn observe_business(mut pending: IpcPending, observation: BusinessObservation) -> IpcPending {
    match &mut pending {
        IpcPending::Enable(enable) => enable.observe(observation),
        IpcPending::Lower(lower) => {
            if let Some(carried) = &mut lower.carried_enable
                && let Some(rr) = &mut carried.rr
            {
                rr.observe(PendingTarget::EnabledActive, observation);
            }
            lower.waiters.observe(observation);
        }
        IpcPending::Idle | IpcPending::Control(_) => {}
    }
    pending
}

/// 仅记录撤销业务信息；调用者仍须按生产协议先实际收紧权限。
pub fn revoke_enable(
    mut pending: IpcPending,
    cause: RevokeCause,
    observation: BusinessObservation,
) -> Result<IpcPending, PendingRejected<(IpcPending, RevokeCause, BusinessObservation)>> {
    match &mut pending {
        IpcPending::Enable(enable) => {
            enable.observe(observation);
            enable.revoked_by = Some(cause);
            if let Some(rr) = &mut enable.rr
                && rr.locked.is_none()
            {
                let outcome = match cause {
                    RevokeCause::Disable => PendingOutcome::SupersededByDisable,
                    RevokeCause::Shutdown => {
                        PendingOutcome::Existing(SetEnabledRrPayload::PreemptedByShutdown)
                    }
                    RevokeCause::Emergency => {
                        PendingOutcome::Existing(SetEnabledRrPayload::PreemptedByEStop)
                    }
                    RevokeCause::Fault => {
                        PendingOutcome::Existing(SetEnabledRrPayload::AdapterError)
                    }
                };
                rr.locked = Some(LockedOutcome {
                    outcome,
                    at: observation.now,
                });
            }
            Ok(pending)
        }
        IpcPending::Idle | IpcPending::Lower(_) | IpcPending::Control(_) => Err(PendingRejected {
            input: (pending, cause, observation),
            reason: PendingRefusal::WrongState,
        }),
    }
}

#[must_use]
pub fn into_post_enable_cleanup(
    mut retired: RetiredEnablePending,
    started_at: MonoRawNs,
) -> LowerPending {
    let waiters = std::mem::replace(&mut retired.lower_waiters, LowerWaiters::empty());
    let shutdown_generation = retired.bridge.shutdown_generation;
    LowerPending {
        seq: None,
        cause: LowerCause::PostEnableCleanup,
        carried_enable: Some(retired),
        waiters,
        started_at,
        shutdown_generation,
        completion: None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnableRetireRefusal {
    CrossDomain {
        owner: DomainId,
        authority: DomainId,
        slot: DomainId,
    },
    ExecutionStillLive,
    WrongPhase(Option<Phase>),
    RetirementRouteRequired,
    Authority(Refusal),
}
#[derive(Debug)]
pub struct EnableRetireRejected {
    pub pending: EnablePending,
    pub reason: EnableRetireRefusal,
}

/// 先验三方 domain 后撤公告，再真实退休；A2 拒绝仍返还完整责任。
/// 已发生的 withdrawal 不会回滚，Err 也不证明旧 seq 已取消。
/// 未撤销 Queued 起步失败和 Succeeded commit 拒绝的总装入口留后续 W3。
pub fn retire_enable_pending(
    pending: EnablePending,
    authority: &ControlAuthority,
    start_slot: &StartViewSlot,
) -> Result<RetiredEnablePending, EnableRetireRejected> {
    retire_enable_pending_impl(
        pending,
        authority,
        start_slot,
        RetirementEntry::Public,
        #[cfg(test)]
        || {},
    )
}

fn retire_enable_pending_impl(
    pending: EnablePending,
    authority: &ControlAuthority,
    start_slot: &StartViewSlot,
    entry: RetirementEntry,
    #[cfg(test)] after_withdraw: impl FnOnce(),
) -> Result<RetiredEnablePending, EnableRetireRejected> {
    let domain = pending.owner.domain();
    if domain != authority.domain() || domain != start_slot.domain() {
        return Err(EnableRetireRejected {
            pending,
            reason: EnableRetireRefusal::CrossDomain {
                owner: domain,
                authority: authority.domain(),
                slot: start_slot.domain(),
            },
        });
    }
    let reason = match authority.snapshot().phase {
        Some(Phase::EnableExecuting) => Some(EnableRetireRefusal::ExecutionStillLive),
        Some(Phase::EnableQueued | Phase::EnableSucceeded)
            if pending.revoked_by.is_none()
                && (!matches!(entry, RetirementEntry::Bridge)
                    || pending.bridge.route.is_none()) =>
        {
            Some(EnableRetireRefusal::RetirementRouteRequired)
        }
        Some(Phase::EnableQueued | Phase::EnableSucceeded | Phase::EnableFailed) => None,
        phase => Some(EnableRetireRefusal::WrongPhase(phase)),
    };
    if let Some(reason) = reason {
        return Err(EnableRetireRejected { pending, reason });
    }
    let withdrew_start = start_slot.withdraw(pending.view_epoch);
    #[cfg(test)]
    after_withdraw();
    let EnablePending {
        owner,
        view_epoch,
        seq,
        rr,
        lower_waiters,
        revoked_by,
        started_at,
        bridge,
    } = pending;
    match authority.retire_enable(owner) {
        Ok(retired) => Ok(RetiredEnablePending {
            domain,
            view_epoch,
            source_seq: seq,
            rr,
            lower_waiters,
            revoked_by,
            started_at,
            retired_from: retired.retired_from,
            withdrew_start,
            bridge,
        }),
        Err(refused) => Err(EnableRetireRejected {
            pending: EnablePending {
                owner: refused.token,
                view_epoch,
                seq,
                rr,
                lower_waiters,
                revoked_by,
                started_at,
                bridge,
            },
            reason: EnableRetireRefusal::Authority(refused.reason),
        }),
    }
}

#[derive(Debug, Default)]
struct EnableBridgeState {
    start_view: Option<robot_core::control_authority::EnableStartView>,
    completion: Option<robot_core::executor_ctrl::AdapterResp>,
    route: Option<EnableRetirementRoute>,
    shutdown_generation: Option<std::num::NonZeroU64>,
    effect: EnableEffect,
}

#[derive(Debug)]
enum EnableRetirementRoute {
    PostRefused,
    SubmitUnsupported,
    CompletedWithoutStart,
    CommitRefused,
    ExecutionFailed,
}

// 私有调用入口判别，不携 owner、请求或退休凭证。
#[derive(Clone, Copy)]
enum RetirementEntry {
    Public,
    Bridge,
}

/// 真实退休后的固定回复批次；字段只在本模块移动原请求时创建。
#[derive(Debug)]
pub(crate) struct RetiredEnableReply {
    pub identity: RrIdentity,
    pub outcome: PendingOutcome,
    pub effect: EnableEffect,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum EnableEffect {
    #[default]
    None,
    Started,
    Committed,
}

#[derive(Debug)]
pub(crate) struct RetiredReplyBatch {
    entries: [Option<RetiredEnableReply>; MAX_CLIENTS_SUPPORTED + 1],
}
impl RetiredReplyBatch {
    fn empty() -> Self {
        Self {
            entries: [const { None }; MAX_CLIENTS_SUPPORTED + 1],
        }
    }
    pub(crate) fn into_entries(self) -> impl Iterator<Item = RetiredEnableReply> {
        self.entries.into_iter().flatten()
    }
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }
}

#[derive(Debug)]
pub(crate) struct EnableCommitFact {
    pub at: MonoRawNs,
    pub epoch: PermissionEpoch,
}

/// 真实提交或原序号完成返回的失败来源，不由已锁业务回复反推。
#[derive(Debug)]
pub(crate) enum EnableFailureSource {
    SubmitUnsupported(&'static str),
    ExecutionFailed(robot_core::executor_ctrl::AdapterErr),
}

/// 首次真实失败的单次交接事实；不授予执行、停止或退休权限。
#[derive(Debug)]
pub(crate) struct EnableFailureFact {
    source: EnableFailureSource,
}

impl EnableFailureFact {
    #[must_use]
    pub(crate) const fn source(&self) -> &EnableFailureSource {
        &self.source
    }
}

#[derive(Debug)]
pub(crate) struct BridgeStep {
    pub pending: IpcPending,
    pub replies: RetiredReplyBatch,
    pub commit: Option<EnableCommitFact>,
    /// 先接回原 pending 再消费；不能据此重写已锁首因或重复安排物理停止。
    pub failure: Option<EnableFailureFact>,
    /// 仅真实 Lower 责任完成时产生，供 IPC 记住先前原 g 的收尾事实。
    pub lowered_generation: Option<std::num::NonZeroU64>,
}
impl BridgeStep {
    fn retained(pending: IpcPending) -> Self {
        Self {
            pending,
            replies: RetiredReplyBatch::empty(),
            commit: None,
            failure: None,
            lowered_generation: None,
        }
    }

    fn failed(pending: EnablePending, source: EnableFailureSource) -> Self {
        Self {
            failure: Some(EnableFailureFact { source }),
            ..Self::retained(IpcPending::Enable(pending))
        }
    }
}

#[derive(Debug)]
pub(crate) enum BridgeRefusal {
    Pending(PendingRefusal),
    Authority(Refusal),
    Retirement(EnableRetireRefusal),
    CrossDomain,
    InvalidSequence(u64),
    WaitingForShutdownPublication,
}
impl std::fmt::Display for BridgeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending(reason) => write!(f, "待办拒绝：{reason:?}"),
            Self::Authority(reason) => write!(f, "权限拒绝：{reason:?}"),
            Self::Retirement(reason) => write!(f, "退休拒绝：{reason:?}"),
            Self::CrossDomain => f.write_str("拥有域不一致"),
            Self::InvalidSequence(raw) => write!(f, "原请求编码非法：{raw:#x}"),
            Self::WaitingForShutdownPublication => f.write_str("等待原关机代次发布"),
        }
    }
}
#[derive(Debug)]
pub(crate) struct BridgeRejected {
    pub pending: IpcPending,
    pub reason: BridgeRefusal,
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "每个分支按值保留原 pending、固定 waiter 和回复批次，不额外装箱"
)]
pub(crate) enum EnableStartOutcome {
    BeforeReserve {
        current: IpcPending,
        rr: Option<RrIdentity>,
        reason: BridgeRefusal,
    },
    Advanced(BridgeStep),
    Retained(BridgeRejected),
}

pub(crate) fn start_enable_owned(
    current: IpcPending,
    rr: Option<RrIdentity>,
    ctrl: &crate::hard_rt_ctrl::HardRtCtrlClient,
    now: MonoRawNs,
) -> EnableStartOutcome {
    if !matches!(current, IpcPending::Idle) {
        return EnableStartOutcome::BeforeReserve {
            current,
            rr,
            reason: BridgeRefusal::Pending(PendingRefusal::AlreadyInProgress),
        };
    }
    let (owner, view) = match ctrl.authority().reserve_enable() {
        Ok(pair) => pair,
        Err(reason) => {
            return EnableStartOutcome::BeforeReserve {
                current,
                rr,
                reason: BridgeRefusal::Authority(reason),
            };
        }
    };
    let mut pending = new_enable(owner, rr, now);
    pending.bridge.start_view = Some(view);
    match submit_enable(pending, ctrl) {
        Ok(step) => EnableStartOutcome::Advanced(step),
        Err(rejected) => EnableStartOutcome::Retained(rejected),
    }
}

fn submit_enable(
    mut pending: EnablePending,
    ctrl: &crate::hard_rt_ctrl::HardRtCtrlClient,
) -> Result<BridgeStep, BridgeRejected> {
    use robot_core::control_authority::StartViewRefusal;
    use robot_core::executor_ctrl::{AdapterNormalReq, ExecutorCtrlClient, NormalReqStatus};
    if pending.owner.domain() != ctrl.authority().domain()
        || pending.owner.domain() != ctrl.start_slot().domain()
    {
        return Err(BridgeRejected {
            pending: IpcPending::Enable(pending),
            reason: BridgeRefusal::CrossDomain,
        });
    }
    if let Some(view) = pending.bridge.start_view.take() {
        let posted = match ctrl.start_slot().post(view) {
            Ok(()) => Ok(()),
            Err(refused) => match refused.reason {
                StartViewRefusal::Occupied { stale_epoch } => {
                    let _ = ctrl.start_slot().withdraw(stale_epoch);
                    ctrl.start_slot().post(refused.view)
                }
                StartViewRefusal::CrossDomain => Err(refused),
            },
        };
        if let Err(refused) = posted {
            pending.bridge.start_view = Some(refused.view);
            pending.bridge.route = Some(EnableRetirementRoute::PostRefused);
            lock_pending(
                &mut pending.rr,
                PendingOutcome::Existing(SetEnabledRrPayload::AdapterError),
                pending.started_at,
            );
            return Ok(BridgeStep::retained(IpcPending::Enable(pending)));
        }
    }
    if pending.seq.is_none()
        && pending.bridge.completion.is_none()
        && pending.bridge.route.is_none()
    {
        match ctrl.request_normal(AdapterNormalReq::Enable(true)) {
            NormalReqStatus::Submitted { seq } => {
                pending.seq = Some(seq);
                pending.bridge.effect = EnableEffect::Started;
            }
            NormalReqStatus::Busy => {}
            NormalReqStatus::Unsupported(message) => {
                pending.bridge.route = Some(EnableRetirementRoute::SubmitUnsupported);
                lock_pending(
                    &mut pending.rr,
                    PendingOutcome::Existing(SetEnabledRrPayload::AdapterError),
                    pending.started_at,
                );
                return Ok(BridgeStep::failed(
                    pending,
                    EnableFailureSource::SubmitUnsupported(message),
                ));
            }
        }
    }
    Ok(BridgeStep::retained(IpcPending::Enable(pending)))
}

fn lock_pending(reply: &mut Option<PendingReply>, outcome: PendingOutcome, at: MonoRawNs) {
    if let Some(reply) = reply
        && reply.locked.is_none()
    {
        reply.locked = Some(LockedOutcome { outcome, at });
    }
}

fn take_reply(
    reply: PendingReply,
    fallback: PendingOutcome,
    effect: EnableEffect,
) -> RetiredEnableReply {
    RetiredEnableReply {
        identity: reply.identity,
        outcome: reply.locked.map_or(fallback, |locked| locked.outcome),
        effect,
    }
}

fn retire_owned(
    pending: EnablePending,
    ctrl: &crate::hard_rt_ctrl::HardRtCtrlClient,
) -> Result<RetiredEnablePending, BridgeRejected> {
    retire_enable_pending_impl(
        pending,
        ctrl.authority(),
        ctrl.start_slot(),
        RetirementEntry::Bridge,
        #[cfg(test)]
        || {},
    )
    .map_err(|rejected| BridgeRejected {
        pending: IpcPending::Enable(rejected.pending),
        reason: BridgeRefusal::Retirement(rejected.reason),
    })
}

pub(crate) fn advance_owned(
    pending: IpcPending,
    ctrl: &crate::hard_rt_ctrl::HardRtCtrlClient,
    clock: &dyn robot_core::time::RobotClock,
) -> Result<BridgeStep, BridgeRejected> {
    match pending {
        IpcPending::Enable(enable) => advance_enable(enable, ctrl, clock),
        IpcPending::Lower(lower) => advance_lower(lower, ctrl, clock),
        pending => Ok(BridgeStep::retained(pending)),
    }
}

fn withdraw_cancel_enable(
    mut pending: EnablePending,
    ctrl: &HardRtCtrlClient,
) -> Result<EnablePending, BridgeRejected> {
    let _ = ctrl.start_slot().withdraw(pending.view_epoch);
    if let Some(seq) = pending.seq {
        match ctrl.try_cancel_unclaimed(seq) {
            crate::hard_rt_ctrl::CancelPendingResult::Taken(action) => {
                debug_assert_eq!(action.seq, seq);
                pending.seq = None;
            }
            crate::hard_rt_ctrl::CancelPendingResult::NotTaken => {}
            crate::hard_rt_ctrl::CancelPendingResult::Invalid(invalid) => {
                return Err(BridgeRejected {
                    pending: IpcPending::Enable(pending),
                    reason: BridgeRefusal::InvalidSequence(invalid.0),
                });
            }
        }
    }
    Ok(pending)
}

#[allow(
    clippy::too_many_lines,
    reason = "同一原 seq 的业务锁存、一次故障事实及退休 Err 返原在一处闭合"
)]
fn advance_enable(
    mut pending: EnablePending,
    ctrl: &crate::hard_rt_ctrl::HardRtCtrlClient,
    clock: &dyn robot_core::time::RobotClock,
) -> Result<BridgeStep, BridgeRejected> {
    use robot_core::executor_ctrl::{AdapterErr, AdapterResp, ExecutorCtrlClient};
    if pending.owner.domain() != ctrl.authority().domain()
        || pending.owner.domain() != ctrl.start_slot().domain()
    {
        return Err(BridgeRejected {
            pending: IpcPending::Enable(pending),
            reason: BridgeRefusal::CrossDomain,
        });
    }
    if pending.revoked_by.is_some() {
        pending = withdraw_cancel_enable(pending, ctrl)?;
    }
    if let Some(seq) = pending.seq
        && let Some(response) = ctrl.poll_last_normal_resp(seq)
    {
        pending.seq = None;
        pending.bridge.completion = Some(response);
    }
    if pending.revoked_by.is_some() || pending.bridge.route.is_some() {
        let retired = retire_owned(pending, ctrl)?;
        return advance_lower(into_post_enable_cleanup(retired, clock.now()), ctrl, clock);
    }
    match pending.bridge.completion {
        None => submit_enable(pending, ctrl),
        Some(AdapterResp::Err(error)) => {
            pending.bridge.route = Some(match error {
                AdapterErr::AuthorityRevoked | AdapterErr::PreemptedByEmergency => {
                    EnableRetirementRoute::CompletedWithoutStart
                }
                _ => EnableRetirementRoute::ExecutionFailed,
            });
            lock_pending(
                &mut pending.rr,
                PendingOutcome::Existing(SetEnabledRrPayload::AdapterError),
                clock.now(),
            );
            if matches!(
                pending.bridge.route,
                Some(EnableRetirementRoute::ExecutionFailed)
            ) {
                // 原 seq 的实际失败先交 IPC 锁业务故障；下次沿 route 退休。
                // 若立即尝试退休，Err 会把本次新事实丢在错误返回之外。
                return Ok(BridgeStep::failed(
                    pending,
                    EnableFailureSource::ExecutionFailed(error),
                ));
            }
            let retired = retire_owned(pending, ctrl)?;
            advance_lower(into_post_enable_cleanup(retired, clock.now()), ctrl, clock)
        }
        Some(AdapterResp::Ok) => {
            // 已锁失败或已有 Lower waiter 时不重新恢复执行权限；原首次业务结果保留。
            let failed_business = pending
                .rr
                .as_ref()
                .and_then(|rr| rr.locked.as_ref())
                .is_some_and(|locked| {
                    !matches!(
                        locked.outcome,
                        PendingOutcome::Existing(SetEnabledRrPayload::Enabled { .. })
                    )
                });
            if failed_business || !pending.lower_waiters.is_empty() {
                let _ = ctrl.authority().disable();
                pending.revoked_by = Some(RevokeCause::Disable);
                let retired = retire_owned(pending, ctrl)?;
                return advance_lower(into_post_enable_cleanup(retired, clock.now()), ctrl, clock);
            }
            let EnablePending {
                owner,
                view_epoch,
                seq,
                rr,
                lower_waiters,
                revoked_by,
                started_at,
                bridge,
            } = pending;
            match ctrl.authority().commit_enable(owner) {
                Ok(committed) => {
                    let at = clock.now();
                    let mut replies = RetiredReplyBatch::empty();
                    replies.entries[0] = rr.map(|rr| {
                        take_reply(
                            rr,
                            PendingOutcome::Existing(SetEnabledRrPayload::Enabled {
                                completed_at_ns: at.0,
                            }),
                            EnableEffect::Committed,
                        )
                    });
                    Ok(BridgeStep {
                        pending: IpcPending::Idle,
                        replies,
                        commit: Some(EnableCommitFact {
                            at,
                            epoch: committed.epoch,
                        }),
                        failure: None,
                        lowered_generation: None,
                    })
                }
                Err(refused) => {
                    let mut pending = EnablePending {
                        owner: refused.token,
                        view_epoch,
                        seq,
                        rr,
                        lower_waiters,
                        revoked_by,
                        started_at,
                        bridge,
                    };
                    pending.bridge.route = Some(EnableRetirementRoute::CommitRefused);
                    lock_pending(
                        &mut pending.rr,
                        PendingOutcome::Existing(SetEnabledRrPayload::AdapterError),
                        clock.now(),
                    );
                    let retired = retire_owned(pending, ctrl)?;
                    advance_lower(into_post_enable_cleanup(retired, clock.now()), ctrl, clock)
                }
            }
        }
    }
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "拒绝与推进均按值保留完整原请求和固定 waiter，不额外装箱"
)]
pub(crate) enum LowerStartOutcome {
    BeforeStart {
        pending: IpcPending,
        rr: Option<RrIdentity>,
        reason: BridgeRefusal,
    },
    Advanced(BridgeStep),
    Retained(BridgeRejected),
}

pub(crate) fn begin_lower_owned(
    pending: IpcPending,
    rr: Option<RrIdentity>,
    cause: LowerCause,
    ctrl: &crate::hard_rt_ctrl::HardRtCtrlClient,
    boot: &crate::boot_supervisor::BootShared,
    observation: BusinessObservation,
) -> LowerStartOutcome {
    begin_lower_owned_impl(
        pending,
        rr,
        cause,
        ctrl,
        boot,
        observation,
        #[cfg(test)]
        || {},
    )
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "同一拥有函数保全部失败返原；私有测试只在原前采与同次disable间插入一次真实并发更新"
)]
fn begin_lower_owned_impl(
    mut pending: IpcPending,
    rr: Option<RrIdentity>,
    cause: LowerCause,
    ctrl: &crate::hard_rt_ctrl::HardRtCtrlClient,
    boot: &crate::boot_supervisor::BootShared,
    observation: BusinessObservation,
    #[cfg(test)] before_disable: impl FnOnce(),
) -> LowerStartOutcome {
    let now = observation.now;
    let owner_domain = match &pending {
        IpcPending::Enable(enable) => Some(enable.owner.domain()),
        IpcPending::Lower(lower) => lower.carried_enable.as_ref().map(|carried| carried.domain),
        _ => None,
    };
    if owner_domain.is_some_and(|domain| {
        domain != ctrl.authority().domain() || domain != ctrl.start_slot().domain()
    }) {
        return LowerStartOutcome::BeforeStart {
            pending,
            rr,
            reason: BridgeRefusal::CrossDomain,
        };
    }
    let capacity_available = match &pending {
        IpcPending::Idle => true,
        IpcPending::Enable(enable) => enable.lower_waiters.len() < MAX_CLIENTS_SUPPORTED,
        IpcPending::Lower(lower) => lower.waiters.len() < MAX_CLIENTS_SUPPORTED,
        IpcPending::Control(_) => false,
    };
    if rr.is_some() && !capacity_available || matches!(pending, IpcPending::Control(_)) {
        return LowerStartOutcome::BeforeStart {
            pending,
            rr,
            reason: BridgeRefusal::Pending(PendingRefusal::QueueFull {
                class: PendingClass::Lower,
            }),
        };
    }
    let mut fresh_disable = None;
    let mut admission_failure = None;
    let generation = match cause {
        LowerCause::Shutdown => {
            let same = boot.binding().is_some_and(|binding| {
                std::sync::Arc::ptr_eq(binding.authority(), ctrl.authority())
            });
            let snapshot = ctrl.authority().snapshot();
            if !same {
                return LowerStartOutcome::BeforeStart {
                    pending,
                    rr,
                    reason: BridgeRefusal::CrossDomain,
                };
            }
            let Some(g) = std::num::NonZeroU64::new(boot.runtime_words().generation()) else {
                return LowerStartOutcome::BeforeStart {
                    pending,
                    rr,
                    reason: BridgeRefusal::WaitingForShutdownPublication,
                };
            };
            if !snapshot.shutdown || !snapshot.disabled {
                return LowerStartOutcome::BeforeStart {
                    pending,
                    rr,
                    reason: BridgeRefusal::WaitingForShutdownPublication,
                };
            }
            Some(g)
        }
        LowerCause::OperatorDisable => {
            let before = ctrl.authority().snapshot();
            let historical_candidate = matches!(pending, IpcPending::Idle)
                && observation.state == S0State::Safe
                && rr.is_some()
                && before.latch;
            let candidate = historical_candidate
                && !before.ipc_estop_notice
                && !before.shutdown
                && !before.exhausted;
            #[cfg(test)]
            before_disable();
            let tightened = ctrl.authority().disable();
            if candidate
                && tightened.advanced
                && !tightened.exhausted
                && before.epoch.get().checked_add(1) == Some(tightened.epoch.get())
            {
                fresh_disable = Some(FreshDisableBoundary {
                    domain: before.domain,
                    epoch: tightened.epoch,
                    observation,
                });
            } else if historical_candidate {
                // 未获豁免即回原历史 latch 拒绝，不能让首次 observe 前的快退默认成功。
                // before 是原拒绝事实；epoch 不连续本身不证明发生过新 EStop。
                let rejected = BusinessObservation {
                    state: if before.shutdown {
                        S0State::Stopping
                    } else {
                        observation.state
                    },
                    estop: before.latch || observation.estop,
                    ..observation
                };
                admission_failure = rr.as_ref().and_then(|identity| {
                    decide_business(PendingTarget::Safe, identity.deadline_ns, rejected)
                        .filter(|payload| !matches!(payload, SetEnabledRrPayload::Disabled { .. }))
                        .map(|payload| LockedOutcome {
                            outcome: PendingOutcome::Existing(payload),
                            at: now,
                        })
                });
            }
            None
        }
        LowerCause::PostEnableCleanup | LowerCause::AwaitConvergence => None,
    };
    if let IpcPending::Enable(enable) = &mut pending {
        if enable.owner.domain() != ctrl.authority().domain() {
            return LowerStartOutcome::BeforeStart {
                pending,
                rr,
                reason: BridgeRefusal::CrossDomain,
            };
        }
        let revoke = match cause {
            LowerCause::Shutdown => RevokeCause::Shutdown,
            _ => RevokeCause::Disable,
        };
        enable.revoked_by = Some(revoke);
        if let Some(g) = generation {
            enable.bridge.shutdown_generation = Some(g);
        }
        lock_pending(
            &mut enable.rr,
            match revoke {
                RevokeCause::Shutdown => {
                    PendingOutcome::Existing(SetEnabledRrPayload::PreemptedByShutdown)
                }
                _ => PendingOutcome::SupersededByDisable,
            },
            now,
        );
        let _ = ctrl.start_slot().withdraw(enable.view_epoch);
    }
    if matches!(pending, IpcPending::Idle) {
        pending = IpcPending::Lower(LowerPending {
            seq: None,
            cause,
            carried_enable: None,
            waiters: LowerWaiters::empty(),
            started_at: now,
            shutdown_generation: generation,
            completion: None,
        });
    }
    if let IpcPending::Lower(lower) = &mut pending
        && let Some(g) = generation
    {
        lower.shutdown_generation = Some(g);
    }
    if let Some(rr) = rr {
        pending = match append_lower_waiter_impl(pending, rr, fresh_disable, admission_failure) {
            Ok(pending) => pending,
            Err(rejected) => {
                return LowerStartOutcome::BeforeStart {
                    pending: rejected.input.0,
                    rr: Some(rejected.input.1),
                    reason: BridgeRefusal::Pending(rejected.reason),
                };
            }
        };
    }
    if let IpcPending::Enable(enable) = pending {
        let retired =
            withdraw_cancel_enable(enable, ctrl).and_then(|enable| retire_owned(enable, ctrl));
        pending = match retired {
            Ok(retired) => IpcPending::Lower(into_post_enable_cleanup(retired, now)),
            Err(rejected) => return LowerStartOutcome::Retained(rejected),
        };
    }
    LowerStartOutcome::Advanced(BridgeStep::retained(pending))
}

#[allow(
    clippy::too_many_lines,
    reason = "原 Lower 的首失败锁存、seq 清偿及同快照物理退休需集中保持所有权和判定顺序"
)]
fn advance_lower(
    mut lower: LowerPending,
    ctrl: &crate::hard_rt_ctrl::HardRtCtrlClient,
    clock: &dyn robot_core::time::RobotClock,
) -> Result<BridgeStep, BridgeRejected> {
    use robot_core::control_authority::InhibitReason;
    use robot_core::executor_ctrl::{
        AdapterNormalReq, AdapterResp, ExecutorCtrlClient, NormalReqStatus, OutputSafetyState,
    };
    if lower.waiters.iter().any(|reply| {
        reply
            .fresh_disable
            .as_ref()
            .is_some_and(|boundary| boundary.domain != ctrl.authority().domain())
    }) {
        return Err(BridgeRejected {
            pending: IpcPending::Lower(lower),
            reason: BridgeRefusal::CrossDomain,
        });
    }
    // 首失败必须先于所有 seq 早退锁存，不能被下一 tick 的 Safe 观测覆盖。
    lower
        .waiters
        .observe_fresh_failures(ctrl.authority().snapshot(), clock.now());
    if let Some(carried) = &mut lower.carried_enable {
        if carried.domain != ctrl.authority().domain() {
            return Err(BridgeRejected {
                pending: IpcPending::Lower(lower),
                reason: BridgeRefusal::CrossDomain,
            });
        }
        if let Some(seq) = carried.source_seq {
            let Some(response) = ctrl.poll_last_normal_resp(seq) else {
                return Ok(BridgeStep::retained(IpcPending::Lower(lower)));
            };
            carried.source_seq = None;
            carried.bridge.completion = Some(response);
        }
    }
    if let Some(seq) = lower.seq {
        let Some(response) = ctrl.poll_last_normal_resp(seq) else {
            return Ok(BridgeStep::retained(IpcPending::Lower(lower)));
        };
        lower.seq = None;
        lower.completion = Some(response);
        if matches!(response, AdapterResp::Err(_)) {
            for rr in lower.waiters.entries.iter_mut().flatten() {
                if rr.locked.is_none() {
                    rr.locked = Some(LockedOutcome {
                        outcome: PendingOutcome::Existing(SetEnabledRrPayload::AdapterError),
                        at: clock.now(),
                    });
                }
            }
        }
    }
    let snapshot = ctrl.authority().snapshot();
    // 退休资格与新撤销必须来自同一个原 snapshot；不存在后采 epoch 补许可。
    lower.waiters.observe_fresh_failures(snapshot, clock.now());
    let physically_done = snapshot.phase == Some(Phase::Idle)
        && snapshot.safety == OutputSafetyState::TorqueOffConfirmed
        && snapshot.disabled
        && !snapshot.exhausted
        && !snapshot.normal_io_active
        && !snapshot.serial_estop_pending
        && !snapshot.inhibits.contains(InhibitReason::Converging)
        && !snapshot.inhibits.contains(InhibitReason::FaultPending)
        && !snapshot.inhibits.contains(InhibitReason::Boot);
    if physically_done {
        let mut replies = RetiredReplyBatch::empty();
        if let Some(carried) = lower.carried_enable {
            let effect = carried.bridge.effect;
            replies.entries[0] = carried.rr.map(|rr| {
                take_reply(
                    rr,
                    PendingOutcome::Existing(SetEnabledRrPayload::AdapterError),
                    effect,
                )
            });
        }
        let at = clock.now();
        for (target, rr) in replies.entries[1..].iter_mut().zip(lower.waiters.entries) {
            *target = rr.map(|rr| {
                take_reply(
                    rr,
                    PendingOutcome::Existing(SetEnabledRrPayload::Disabled {
                        completed_at_ns: at.0,
                    }),
                    EnableEffect::Committed,
                )
            });
        }
        return Ok(BridgeStep {
            pending: IpcPending::Idle,
            replies,
            commit: None,
            failure: None,
            lowered_generation: lower.shutdown_generation,
        });
    }
    if lower.seq.is_none()
        && lower.completion.is_none()
        && lower.cause != LowerCause::AwaitConvergence
        && snapshot.inhibits.is_empty()
        && !snapshot.serial_estop_pending
        && !snapshot.normal_io_active
    {
        match ctrl.request_normal(AdapterNormalReq::Enable(false)) {
            NormalReqStatus::Submitted { seq } => lower.seq = Some(seq),
            NormalReqStatus::Busy => {}
            NormalReqStatus::Unsupported(_) => {
                return Err(BridgeRejected {
                    pending: IpcPending::Lower(lower),
                    reason: BridgeRefusal::Pending(PendingRefusal::NoNormalRequest),
                });
            }
        }
    }
    Ok(BridgeStep::retained(IpcPending::Lower(lower)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_machine::{ControlLink, OutputMode, TransitionKind};
    use robot_core::control_authority::{AuthorityInit, EnableResult, InhibitSet};
    use robot_core::executor_ctrl::OutputSafetyState;

    #[test]
    fn lower_diagnostic_reports_response_and_physical_wait_bits_without_mutation() {
        let mut snapshot = authority().snapshot();
        snapshot.phase = Some(Phase::EnableExecuting);
        snapshot.safety = OutputSafetyState::StopCommanded;
        snapshot.disabled = false;
        snapshot.normal_io_active = true;
        let lower = LowerPending {
            seq: Some(17),
            cause: LowerCause::OperatorDisable,
            carried_enable: None,
            waiters: LowerWaiters::empty(),
            started_at: MonoRawNs(10),
            shutdown_generation: None,
            completion: None,
        };
        let diagnostic = lower.diagnostic(snapshot);
        assert_eq!(diagnostic.lower_seq, Some(17));
        assert_eq!(lower.seq(), Some(17));
        assert_ne!(diagnostic.wait_mask & LowerDiagnostic::LOWER_RESPONSE, 0);
        assert_ne!(diagnostic.wait_mask & LowerDiagnostic::PHASE_NOT_IDLE, 0);
        assert_ne!(
            diagnostic.wait_mask & LowerDiagnostic::TORQUE_OFF_UNCONFIRMED,
            0
        );
        assert_ne!(diagnostic.wait_mask & LowerDiagnostic::NOT_DISABLED, 0);
        assert_ne!(diagnostic.wait_mask & LowerDiagnostic::NORMAL_IO_ACTIVE, 0);
        assert_eq!(
            diagnostic.wait_mask & LowerDiagnostic::LOWER_NOT_SUBMITTED,
            0
        );
    }

    fn authority() -> ControlAuthority {
        ControlAuthority::new(AuthorityInit::new(
            false,
            OutputSafetyState::TorqueOffConfirmed,
            InhibitSet::EMPTY,
        ))
        .unwrap()
    }
    fn rr(id: &str, deadline_ns: u64) -> RrIdentity {
        use crate::rr_admission::{CheckedLimits, PeerLedger, RrRequestClass};
        let limits = CheckedLimits::try_from_limits(&robot_transport::iceoryx2::RrStaticLimits {
            c: 1,
            l: 4,
        })
        .unwrap();
        let mut ledger = PeerLedger::<u8>::new(&limits).unwrap();
        let slot = ledger.admit(1, RrRequestClass::Restore).unwrap();
        let route = slot.identity();
        // 此夹具只验证 pending 的关联字段；真实 RR 线程持活 Slot 的链另行验收。
        ledger.release(slot).unwrap();
        RrIdentity {
            route,
            request_id: id.into(),
            deadline_ns,
        }
    }
    fn observation(now: u64, state: S0State) -> BusinessObservation {
        BusinessObservation {
            now: MonoRawNs(now),
            state,
            estop: false,
        }
    }
    fn active() -> S0State {
        S0State::Enabled {
            sub: EnabledSub::Active,
            control_link: ControlLink::Ok,
            output_mode: OutputMode::Active,
        }
    }
    fn enable(pending: IpcPending) -> EnablePending {
        match pending {
            IpcPending::Enable(enable) => enable,
            _ => panic!("应保留 Enable 责任"),
        }
    }
    fn lower(pending: &IpcPending) -> &LowerPending {
        match pending {
            IpcPending::Lower(lower) => lower,
            _ => panic!("应保留 Lower 责任"),
        }
    }
    fn assert_locked(reply: &PendingReply, outcome: PendingOutcome, at: u64) {
        let locked = reply.locked().unwrap();
        assert_eq!(*locked.outcome(), outcome);
        assert_eq!(locked.at(), MonoRawNs(at));
    }
    fn assert_existing(reply: &PendingReply, payload: SetEnabledRrPayload, at: u64) {
        assert_locked(reply, PendingOutcome::Existing(payload), at);
    }
    // 仅测试读取快照，复制身份字符串不创建可退休的请求记录。
    type ReplySnapshot = (String, u64, Option<(PendingOutcome, MonoRawNs)>);
    fn reply_snapshot(reply: &PendingReply) -> ReplySnapshot {
        (
            reply.identity().request_id.clone(),
            reply.identity().deadline_ns,
            reply
                .locked()
                .map(|locked| (*locked.outcome(), locked.at())),
        )
    }
    #[derive(Debug, Eq, PartialEq)]
    struct EnableSnapshot {
        domain: DomainId,
        owner_epoch: PermissionEpoch,
        view_epoch: PermissionEpoch,
        seq: Option<u64>,
        rr: Option<ReplySnapshot>,
        waiters: Vec<ReplySnapshot>,
        revoked: Option<RevokeCause>,
        started_at: MonoRawNs,
    }
    fn snapshot(pending: &EnablePending) -> EnableSnapshot {
        EnableSnapshot {
            domain: pending.owner().domain(),
            owner_epoch: pending.owner().epoch(),
            view_epoch: pending.view_epoch(),
            seq: pending.seq(),
            rr: pending.rr().map(reply_snapshot),
            waiters: pending.lower_waiters().iter().map(reply_snapshot).collect(),
            revoked: pending.revoked_by(),
            started_at: pending.started_at(),
        }
    }
    fn retire_queued_for_test(
        pending: EnablePending,
        authority: &ControlAuthority,
    ) -> RetiredEnablePending {
        let _ = authority.disable();
        let pending = enable(
            revoke_enable(
                IpcPending::Enable(pending),
                RevokeCause::Disable,
                observation(0, S0State::Ready),
            )
            .unwrap(),
        );
        retire_enable_pending(pending, authority, &StartViewSlot::new(authority)).unwrap()
    }

    #[test]
    fn first_deadline_result_survives_late_target_with_owner_seq_and_rr() {
        let authority = authority();
        let (owner, _) = authority.reserve_enable().unwrap();
        let pending = IpcPending::Enable(new_enable(owner, Some(rr("enable", 10)), MonoRawNs(2)));
        // 这里只登记脚本序号，不宣称 executor 提交或完成。
        let pending = record_submitted(pending, 77).unwrap();
        let pending = observe_business(pending, observation(10, S0State::Ready));
        assert!(enable_ref(&pending).rr().unwrap().locked().is_none());
        let pending = observe_business(pending, observation(11, S0State::Ready));
        let before = snapshot(enable_ref(&pending));
        let pending = observe_business(pending, observation(12, active()));
        assert_eq!(snapshot(enable_ref(&pending)), before);
        let pending = enable(pending);
        assert_existing(
            pending.rr().unwrap(),
            SetEnabledRrPayload::DeadlineExceeded,
            11,
        );
        assert_eq!(pending.seq(), Some(77));
        assert_eq!(pending.started_at(), MonoRawNs(2));
        assert_eq!(pending.rr().unwrap().identity().request_id, "enable");
        let retired = retire_queued_for_test(pending, &authority);
        assert_eq!(retired.source_seq(), Some(77));
    }

    fn enable_ref(pending: &IpcPending) -> &EnablePending {
        match pending {
            IpcPending::Enable(enable) => enable,
            _ => panic!("应保留 Enable 责任"),
        }
    }

    #[test]
    fn business_priority_and_strict_deadline_match_existing_resolver() {
        let decide = |state, target, estop, now| {
            decide_business(
                target,
                10,
                BusinessObservation {
                    now: MonoRawNs(now),
                    state,
                    estop,
                },
            )
        };
        for state in [
            S0State::Stopping,
            S0State::Fault(FaultCode::AdapterErr),
            active(),
            S0State::Safe,
        ] {
            assert_eq!(
                decide(state, PendingTarget::EnabledActive, true, 11),
                Some(SetEnabledRrPayload::PreemptedByEStop)
            );
        }
        assert_eq!(
            decide(S0State::Stopping, PendingTarget::Safe, false, 11),
            Some(SetEnabledRrPayload::PreemptedByShutdown)
        );
        for fault in [
            FaultCode::AdapterErr,
            FaultCode::CommFault,
            FaultCode::SafetyStopFailed,
        ] {
            assert_eq!(
                decide(S0State::Fault(fault), PendingTarget::Safe, false, 11),
                Some(SetEnabledRrPayload::AdapterError)
            );
        }
        assert_eq!(
            decide(active(), PendingTarget::EnabledActive, false, 11),
            Some(SetEnabledRrPayload::Enabled {
                completed_at_ns: 11
            })
        );
        assert_eq!(
            decide(S0State::Safe, PendingTarget::Safe, false, 11),
            Some(SetEnabledRrPayload::Disabled {
                completed_at_ns: 11
            })
        );
        for state in [
            S0State::Ready,
            S0State::Fault(FaultCode::RtOverrun),
            S0State::Fault(FaultCode::CmdDeadlineExpired),
            S0State::Fault(FaultCode::LimitViolation),
            S0State::Fault(FaultCode::WatchdogTimeout),
            S0State::Enabled {
                sub: EnabledSub::CtrlLostHold,
                control_link: ControlLink::Lost,
                output_mode: OutputMode::Hold,
            },
        ] {
            assert_eq!(decide(state, PendingTarget::EnabledActive, false, 10), None);
            assert_eq!(
                decide(state, PendingTarget::EnabledActive, false, 11),
                Some(SetEnabledRrPayload::DeadlineExceeded)
            );
        }
    }

    #[test]
    fn lower_waiters_lock_independent_deadlines_without_releasing_slots() {
        let lower_pending = new_lower(LowerCause::OperatorDisable, MonoRawNs(1)).unwrap();
        let pending = append_lower_waiter(IpcPending::Lower(lower_pending), rr("a", 10)).unwrap();
        let pending = append_lower_waiter(pending, rr("b", 20)).unwrap();
        let pending = observe_business(pending, observation(11, S0State::Ready));
        assert_existing(
            lower(&pending).waiters().iter().next().unwrap(),
            SetEnabledRrPayload::DeadlineExceeded,
            11,
        );
        assert!(
            lower(&pending)
                .waiters()
                .iter()
                .nth(1)
                .unwrap()
                .locked()
                .is_none()
        );
        let pending = observe_business(pending, observation(12, S0State::Safe));
        assert_eq!(lower(&pending).waiters().len(), 2);
        assert_existing(
            lower(&pending).waiters().iter().next().unwrap(),
            SetEnabledRrPayload::DeadlineExceeded,
            11,
        );
        assert_existing(
            lower(&pending).waiters().iter().nth(1).unwrap(),
            SetEnabledRrPayload::Disabled {
                completed_at_ns: 12,
            },
            12,
        );
    }

    #[test]
    fn full_waiters_return_seventeenth_rr_even_after_all_deadlines_lock() {
        for in_enable in [false, true] {
            let authority = authority();
            let mut pending = if in_enable {
                let (owner, _) = authority.reserve_enable().unwrap();
                IpcPending::Enable(new_enable(owner, Some(rr("original", 99)), MonoRawNs(1)))
            } else {
                IpcPending::Lower(new_lower(LowerCause::OperatorDisable, MonoRawNs(1)).unwrap())
            };
            for index in 0..MAX_CLIENTS_SUPPORTED {
                pending = append_lower_waiter(pending, rr(&format!("waiter-{index}"), 10)).unwrap();
            }
            pending = observe_business(pending, observation(11, S0State::Ready));
            let rejected = append_lower_waiter(pending, rr("seventeenth", 30)).unwrap_err();
            assert_eq!(
                rejected.reason,
                PendingRefusal::QueueFull {
                    class: PendingClass::Lower
                }
            );
            let (pending, rr) = rejected.input;
            assert_eq!(
                (rr.request_id.as_str(), rr.deadline_ns),
                ("seventeenth", 30)
            );
            let waiters = match &pending {
                IpcPending::Enable(enable) => enable.lower_waiters(),
                IpcPending::Lower(lower) => lower.waiters(),
                _ => unreachable!(),
            };
            assert_eq!(waiters.len(), 16);
            for (index, reply) in waiters.iter().enumerate() {
                assert_eq!(reply.identity().request_id, format!("waiter-{index}"));
                assert_eq!(reply.identity().deadline_ns, 10);
                assert_existing(reply, SetEnabledRrPayload::DeadlineExceeded, 11);
            }
            if let IpcPending::Enable(pending) = pending {
                assert_eq!(pending.rr().unwrap().identity().request_id, "original");
                let _retired = retire_queued_for_test(pending, &authority);
            }
        }
    }

    #[test]
    fn owned_rejections_preserve_install_waiter_and_sequence_inputs() {
        let authority = authority();
        let (owner, _) = authority.reserve_enable().unwrap();
        let pending = IpcPending::Enable(new_enable(owner, Some(rr("enable", 99)), MonoRawNs(1)));
        let incoming =
            IpcPending::Lower(new_lower(LowerCause::OperatorDisable, MonoRawNs(2)).unwrap());
        let rejected = install_pending(pending, incoming).unwrap_err();
        assert_eq!(rejected.reason, PendingRefusal::AlreadyInProgress);
        let (pending, incoming) = rejected.input;
        assert_eq!(lower(&incoming).started_at(), MonoRawNs(2));
        let pending = install_pending(IpcPending::Idle, pending).unwrap();
        let pending = record_submitted(pending, 10).unwrap();
        let rejected = record_submitted(pending, 11).unwrap_err();
        assert_eq!(rejected.reason, PendingRefusal::SeqAlreadyPresent);
        assert_eq!(rejected.input.1, 11);
        assert_eq!(enable_ref(&rejected.input.0).seq(), Some(10));
        let _retired = retire_queued_for_test(enable(rejected.input.0), &authority);
        let rejected = install_pending(IpcPending::Idle, IpcPending::Idle).unwrap_err();
        assert_eq!(rejected.reason, PendingRefusal::WrongState);
        assert!(matches!(
            rejected.input,
            (IpcPending::Idle, IpcPending::Idle)
        ));
        let rejected = append_lower_waiter(IpcPending::Idle, rr("original", 30)).unwrap_err();
        assert_eq!(rejected.reason, PendingRefusal::WrongState);
        assert_eq!(rejected.input.1.request_id, "original");
        let incoming = append_lower_waiter(incoming, rejected.input.1).unwrap();
        assert_eq!(lower(&incoming).waiters().len(), 1);
        let rejected = record_submitted(IpcPending::Idle, 12).unwrap_err();
        assert_eq!(rejected.reason, PendingRefusal::WrongState);
        assert_eq!(rejected.input.1, 12);
    }

    #[test]
    fn control_and_lower_constructors_keep_plans_and_forbid_fake_cleanup() {
        let plan = PlannedTransition {
            new_state: S0State::Safe,
            new_policy: robot_core::executor_ctrl::OutputPolicy::Disabled,
            kind: TransitionKind::Pure,
        };
        let control = new_control(plan, MonoRawNs(3));
        assert_eq!(control.plan(), &plan);
        assert_eq!(control.started_at(), MonoRawNs(3));
        assert_eq!(control.seq(), None);
        let control = record_submitted(IpcPending::Control(control), 14).unwrap();
        let rejected = append_lower_waiter(control, rr("control-waiter", 40)).unwrap_err();
        assert_eq!(rejected.reason, PendingRefusal::WrongState);
        assert_eq!(rejected.input.1.request_id, "control-waiter");
        let control = observe_business(rejected.input.0, observation(100, S0State::Safe));
        let rejected = revoke_enable(
            control,
            RevokeCause::Shutdown,
            observation(100, S0State::Stopping),
        )
        .unwrap_err();
        assert_eq!(rejected.reason, PendingRefusal::WrongState);
        assert_eq!(rejected.input.1, RevokeCause::Shutdown);
        assert_eq!(rejected.input.2, observation(100, S0State::Stopping));
        match rejected.input.0 {
            IpcPending::Control(control) => {
                assert_eq!(control.plan(), &plan);
                assert_eq!(control.seq(), Some(14));
            }
            _ => panic!("Control 不可被撤销入口取代"),
        }
        let rejected = new_lower(LowerCause::PostEnableCleanup, MonoRawNs(5)).unwrap_err();
        assert_eq!(
            rejected.reason,
            PendingRefusal::CleanupRequiresRetiredEnable
        );
        assert_eq!(
            rejected.input,
            (LowerCause::PostEnableCleanup, MonoRawNs(5))
        );
        let pending =
            IpcPending::Lower(new_lower(LowerCause::AwaitConvergence, MonoRawNs(6)).unwrap());
        let rejected = record_submitted(pending, 15).unwrap_err();
        assert_eq!(rejected.reason, PendingRefusal::NoNormalRequest);
        assert_eq!(rejected.input.1, 15);
        assert_eq!(lower(&rejected.input.0).seq(), None);
        assert_eq!(
            lower(&rejected.input.0).cause(),
            LowerCause::AwaitConvergence
        );
    }

    #[test]
    fn revoke_uses_first_observation_and_never_overwrites_locked_outcome() {
        for (cause, expected) in [
            (RevokeCause::Disable, PendingOutcome::SupersededByDisable),
            (
                RevokeCause::Shutdown,
                PendingOutcome::Existing(SetEnabledRrPayload::PreemptedByShutdown),
            ),
            (
                RevokeCause::Emergency,
                PendingOutcome::Existing(SetEnabledRrPayload::PreemptedByEStop),
            ),
            (
                RevokeCause::Fault,
                PendingOutcome::Existing(SetEnabledRrPayload::AdapterError),
            ),
        ] {
            let authority = authority();
            let (owner, _) = authority.reserve_enable().unwrap();
            let pending =
                IpcPending::Enable(new_enable(owner, Some(rr("original", 30)), MonoRawNs(1)));
            let pending = append_lower_waiter(pending, rr("waiter", 40)).unwrap();
            // 本用例只验证纯业务函数；随后以真实 disable 收尾 owner。
            let pending = revoke_enable(pending, cause, observation(2, S0State::Ready)).unwrap();
            assert_locked(enable_ref(&pending).rr().unwrap(), expected, 2);
            assert!(
                enable_ref(&pending)
                    .lower_waiters()
                    .iter()
                    .next()
                    .unwrap()
                    .locked()
                    .is_none()
            );
            let pending = revoke_enable(
                pending,
                RevokeCause::Emergency,
                observation(50, S0State::Stopping),
            )
            .unwrap();
            assert_locked(enable_ref(&pending).rr().unwrap(), expected, 2);
            assert_eq!(
                enable_ref(&pending).revoked_by(),
                Some(RevokeCause::Emergency)
            );
            let _retired = retire_queued_for_test(enable(pending), &authority);
        }
        let authority = authority();
        let (owner, _) = authority.reserve_enable().unwrap();
        let pending = IpcPending::Enable(new_enable(
            owner,
            Some(rr("target-first", 30)),
            MonoRawNs(1),
        ));
        let pending =
            revoke_enable(pending, RevokeCause::Disable, observation(2, active())).unwrap();
        assert_existing(
            enable_ref(&pending).rr().unwrap(),
            SetEnabledRrPayload::Enabled { completed_at_ns: 2 },
            2,
        );
        let _retired = retire_queued_for_test(enable(pending), &authority);
    }

    #[test]
    fn real_executing_owner_cannot_retire_until_finish_then_cleanup_keeps_old_seq() {
        let authority = authority();
        let slot = StartViewSlot::new(&authority);
        let (owner, view) = authority.reserve_enable().unwrap();
        slot.post(view).unwrap();
        let execution = authority.start_enable(slot.take().unwrap()).unwrap();
        let pending = IpcPending::Enable(new_enable(owner, Some(rr("enable", 50)), MonoRawNs(1)));
        let pending = record_submitted(pending, 71).unwrap();
        let pending = append_lower_waiter(pending, rr("a", 10)).unwrap();
        let pending = append_lower_waiter(pending, rr("b", 30)).unwrap();
        let _ = authority.disable();
        let pending = enable(
            revoke_enable(
                pending,
                RevokeCause::Disable,
                observation(11, S0State::Ready),
            )
            .unwrap(),
        );
        let before = snapshot(&pending);
        let rejected = retire_enable_pending(pending, &authority, &slot).unwrap_err();
        assert_eq!(rejected.reason, EnableRetireRefusal::ExecutionStillLive);
        assert_eq!(snapshot(&rejected.pending), before);
        assert_eq!(authority.snapshot().phase, Some(Phase::EnableExecuting));
        authority
            .finish_enable(execution, EnableResult::Succeeded)
            .unwrap();
        let retired = retire_enable_pending(rejected.pending, &authority, &slot).unwrap();
        assert_eq!(authority.snapshot().phase, Some(Phase::Idle));
        assert_eq!(retired.domain(), authority.domain());
        assert_eq!(retired.view_epoch(), view.epoch());
        assert_eq!(retired.retired_from(), Phase::EnableSucceeded);
        assert!(!retired.withdrew_start());
        assert_eq!(retired.source_seq(), Some(71));
        assert_eq!(retired.started_at(), MonoRawNs(1));
        assert_eq!(retired.revoked_by(), Some(RevokeCause::Disable));
        assert_eq!(
            retired
                .lower_waiters()
                .iter()
                .map(reply_snapshot)
                .collect::<Vec<_>>(),
            before.waiters
        );
        let cleanup = into_post_enable_cleanup(retired, MonoRawNs(12));
        assert_eq!(cleanup.cause(), LowerCause::PostEnableCleanup);
        assert_eq!(cleanup.started_at(), MonoRawNs(12));
        assert_eq!(cleanup.seq(), None);
        assert_eq!(cleanup.waiters().len(), 2);
        let carried = cleanup.carried_enable().unwrap();
        assert!(carried.lower_waiters().is_empty());
        assert_eq!(carried.rr().map(reply_snapshot), before.rr);
        assert_eq!(carried.source_seq(), Some(71));
        let pending = observe_business(IpcPending::Lower(cleanup), observation(12, S0State::Safe));
        assert_existing(
            lower(&pending).waiters().iter().next().unwrap(),
            SetEnabledRrPayload::DeadlineExceeded,
            11,
        );
        assert_existing(
            lower(&pending).waiters().iter().nth(1).unwrap(),
            SetEnabledRrPayload::Disabled {
                completed_at_ns: 12,
            },
            12,
        );
        let rejected = record_submitted(pending, 72).unwrap_err();
        assert_eq!(rejected.reason, PendingRefusal::EnableSequenceStillHeld);
        assert_eq!(rejected.input.1, 72);
        assert_eq!(
            lower(&rejected.input.0)
                .carried_enable()
                .unwrap()
                .source_seq(),
            Some(71)
        );
        assert_eq!(lower(&rejected.input.0).seq(), None);
    }

    #[test]
    fn cross_domain_checks_precede_withdrawal_and_return_owner_for_correct_retry() {
        let a = authority();
        let b = authority();
        let slot_a = StartViewSlot::new(&a);
        let slot_b = StartViewSlot::new(&b);
        let (owner_a, view_a) = a.reserve_enable().unwrap();
        let (owner_b, view_b) = b.reserve_enable().unwrap();
        assert_eq!(view_a.epoch(), view_b.epoch());
        slot_a.post(view_a).unwrap();
        slot_b.post(view_b).unwrap();
        let pending = IpcPending::Enable(new_enable(owner_a, Some(rr("enable", 90)), MonoRawNs(1)));
        let _ = a.disable();
        let after_disable = a.snapshot();
        assert_eq!(after_disable.phase, Some(Phase::EnableQueued));
        assert!(matches!(
            a.start_enable(view_a),
            Err(Refusal::StaleEpoch { .. })
        ));
        assert_eq!(a.snapshot(), after_disable);
        // start 拒绝不动公告，随后错域核验仍须保持同一已发布视图。
        assert!(slot_a.post(view_a).is_err());
        let pending = enable(
            revoke_enable(
                pending,
                RevokeCause::Disable,
                observation(2, S0State::Ready),
            )
            .unwrap(),
        );
        let before = snapshot(&pending);
        let before_a = a.snapshot();
        let before_b = b.snapshot();
        let rejected = retire_enable_pending(pending, &b, &slot_a).unwrap_err();
        assert_eq!(
            rejected.reason,
            EnableRetireRefusal::CrossDomain {
                owner: a.domain(),
                authority: b.domain(),
                slot: a.domain()
            }
        );
        assert_eq!(snapshot(&rejected.pending), before);
        let rejected = retire_enable_pending(rejected.pending, &a, &slot_b).unwrap_err();
        assert_eq!(
            rejected.reason,
            EnableRetireRefusal::CrossDomain {
                owner: a.domain(),
                authority: a.domain(),
                slot: b.domain()
            }
        );
        assert_eq!(a.snapshot(), before_a);
        assert_eq!(b.snapshot(), before_b);
        assert_eq!(
            slot_a.post(view_a).unwrap_err().reason,
            robot_core::control_authority::StartViewRefusal::Occupied {
                stale_epoch: view_a.epoch()
            }
        );
        assert_eq!(slot_b.take(), Some(view_b));
        let retired = retire_enable_pending(rejected.pending, &a, &slot_a).unwrap();
        assert!(retired.withdrew_start());
        assert_eq!(retired.retired_from(), Phase::EnableQueued);
        assert_eq!(slot_a.take(), None);
        b.retire_enable(owner_b).unwrap();
    }

    #[test]
    fn only_revoked_or_real_failed_routes_enter_retirement_bridge() {
        let authority = authority();
        let slot = StartViewSlot::new(&authority);
        let (owner, view) = authority.reserve_enable().unwrap();
        slot.post(view).unwrap();
        let pending = new_enable(owner, None, MonoRawNs(1));
        let rejected = retire_enable_pending(pending, &authority, &slot).unwrap_err();
        assert_eq!(
            rejected.reason,
            EnableRetireRefusal::RetirementRouteRequired
        );
        assert_eq!(slot.take(), Some(view));
        let execution = authority.start_enable(view).unwrap();
        authority
            .finish_enable(execution, EnableResult::Succeeded)
            .unwrap();
        let rejected = retire_enable_pending(rejected.pending, &authority, &slot).unwrap_err();
        assert_eq!(
            rejected.reason,
            EnableRetireRefusal::RetirementRouteRequired
        );
        // 正常成功仅由真正 commit 处理，不能伪造撤销来拓宽桥。
        authority.commit_enable(rejected.pending.owner).unwrap();
        // 真实 Failed 路由使用独立、已 Disabled 的权限实例。
        let authority = self::authority();
        let slot = StartViewSlot::new(&authority);
        let (owner, view) = authority.reserve_enable().unwrap();
        let execution = authority.start_enable(view).unwrap();
        authority
            .finish_enable(execution, EnableResult::Failed)
            .unwrap();
        let retired =
            retire_enable_pending(new_enable(owner, None, MonoRawNs(2)), &authority, &slot)
                .unwrap();
        assert_eq!(retired.retired_from(), Phase::EnableFailed);
        assert_eq!(retired.revoked_by(), None);
        assert_eq!(retired.source_seq(), None);
        let cleanup = into_post_enable_cleanup(retired, MonoRawNs(3));
        let cleanup = record_submitted(IpcPending::Lower(cleanup), 80).unwrap();
        assert_eq!(lower(&cleanup).seq(), Some(80));
        assert_eq!(lower(&cleanup).carried_enable().unwrap().source_seq(), None);
    }

    #[test]
    fn real_retire_race_returns_all_context_after_successful_withdrawal() {
        let authority = authority();
        let slot = StartViewSlot::new(&authority);
        let (owner, view) = authority.reserve_enable().unwrap();
        slot.post(view).unwrap();
        let pending = IpcPending::Enable(new_enable(owner, Some(rr("enable", 10)), MonoRawNs(1)));
        let pending = record_submitted(pending, 101).unwrap();
        let pending = append_lower_waiter(pending, rr("early", 10)).unwrap();
        let pending = append_lower_waiter(pending, rr("later", 30)).unwrap();
        // 纯 revoke 元数据不证明权限已收紧。这是公开原语竞争夹具，
        // 明确不是 W3 先实际收紧再撤销的合法生产序列。
        let pending = enable(
            revoke_enable(
                pending,
                RevokeCause::Disable,
                observation(11, S0State::Ready),
            )
            .unwrap(),
        );
        let before = snapshot(&pending);
        let mut execution = None;
        let rejected =
            retire_enable_pending_impl(pending, &authority, &slot, RetirementEntry::Public, || {
                assert_eq!(slot.take(), None);
                execution = Some(authority.start_enable(view).unwrap());
            })
            .unwrap_err();
        assert_eq!(
            rejected.reason,
            EnableRetireRefusal::Authority(Refusal::WrongPhase(Phase::EnableExecuting))
        );
        assert_eq!(snapshot(&rejected.pending), before);
        assert_eq!(slot.take(), None);
        assert_eq!(authority.snapshot().phase, Some(Phase::EnableExecuting));
        authority
            .finish_enable(execution.take().unwrap(), EnableResult::Failed)
            .unwrap();
        let retired = retire_enable_pending(rejected.pending, &authority, &slot).unwrap();
        assert_eq!(authority.snapshot().phase, Some(Phase::Idle));
        assert!(!retired.withdrew_start());
        assert_eq!(retired.source_seq(), Some(101));
        assert_eq!(retired.rr().map(reply_snapshot), before.rr);
        assert_eq!(
            retired
                .lower_waiters()
                .iter()
                .map(reply_snapshot)
                .collect::<Vec<_>>(),
            before.waiters
        );
        assert_eq!(retired.revoked_by(), before.revoked);
        assert_eq!(retired.retired_from(), Phase::EnableFailed);
    }

    fn bridge_started(client: &HardRtCtrlClient, request: RrIdentity) -> EnablePending {
        match start_enable_owned(IpcPending::Idle, Some(request), client, MonoRawNs(1)) {
            EnableStartOutcome::Advanced(step) => enable(step.pending),
            other => panic!("真实 reserve/post/submit 必须成功：{other:?}"),
        }
    }

    fn mock_hard_adapter() -> robot_adapter_mock::MockHardRTAdapter {
        use robot_core::adapter::ManipulatorAdapter;
        let mut adapter = robot_adapter_mock::MockHardRTAdapter::with_unlimited_limits(2);
        adapter.connect().unwrap();
        adapter
    }

    #[test]
    fn bridge_commits_only_after_real_executor_finish_and_returns_original_route_once() {
        use std::sync::{Arc, atomic::Ordering};
        let inner = Arc::new(crate::hard_rt_ctrl::HardRtCtrlInner::new(Arc::new(
            authority(),
        )));
        let client = HardRtCtrlClient::new(inner.clone());
        let request = rr("真实成功", 100);
        let route = request.route;
        let pending = bridge_started(&client, request);
        let sequence = pending.seq().unwrap();
        let clock = robot_core::time::MockClock::new(MonoRawNs(20));
        let pending = advance_owned(IpcPending::Enable(pending), &client, &clock).unwrap();
        assert!(pending.commit.is_none() && pending.replies.is_empty());
        let raw = inner.pending_normal.swap(0, Ordering::AcqRel);
        let packet = inner.decode_pending(raw).unwrap().unwrap();
        let mut adapter = mock_hard_adapter();
        assert!(crate::rt_thread::execute_owned_normal(&mut adapter, packet, &inner).is_none());
        assert_eq!(
            client.poll_last_normal_resp(sequence),
            Some(AdapterResp::Ok)
        );
        let completed = advance_owned(pending.pending, &client, &clock).unwrap();
        assert_eq!(completed.commit.unwrap().at, MonoRawNs(20));
        assert!(matches!(completed.pending, IpcPending::Idle));
        let replies: Vec<_> = completed.replies.into_entries().collect();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].identity.route, route);
        assert_eq!(replies[0].identity.request_id, "真实成功");
        assert_eq!(replies[0].effect, EnableEffect::Committed);
        assert!(matches!(
            replies[0].outcome,
            PendingOutcome::Existing(SetEnabledRrPayload::Enabled {
                completed_at_ns: 20
            })
        ));
        assert!(
            advance_owned(IpcPending::Idle, &client, &clock)
                .unwrap()
                .replies
                .is_empty()
        );
    }

    /// 确定性 owned 桥夹具：真权限原语、原 RR Slot 与真实 Mock 调用；不冒线程/磁盘链。
    struct FreshLowerRig {
        pending: IpcPending,
        slots: Vec<crate::rr_admission::Slot<u8>>,
        ledger: crate::rr_admission::PeerLedger<u8>,
        ctrl: HardRtCtrlClient,
        boot: crate::boot_supervisor::BootShared,
        clock: robot_core::time::MockClock,
    }
    impl FreshLowerRig {
        fn new() -> Self {
            Self::with_authority(
                ControlAuthority::new(AuthorityInit::new(
                    true,
                    OutputSafetyState::TorqueOffConfirmed,
                    InhibitSet::EMPTY,
                ))
                .unwrap(),
            )
        }
        fn with_authority(authority: ControlAuthority) -> Self {
            let limits = crate::rr_admission::CheckedLimits::try_from_limits(
                &robot_transport::iceoryx2::RrStaticLimits { c: 2, l: 4 },
            )
            .unwrap();
            Self {
                pending: IpcPending::Idle,
                slots: Vec::new(),
                ledger: crate::rr_admission::PeerLedger::new(&limits).unwrap(),
                ctrl: HardRtCtrlClient::new(std::sync::Arc::new(
                    crate::hard_rt_ctrl::HardRtCtrlInner::new(std::sync::Arc::new(authority)),
                )),
                boot: crate::boot_supervisor::BootShared::new(MonoRawNs(0)),
                clock: robot_core::time::MockClock::new(MonoRawNs(10)),
            }
        }
        fn admit(&mut self, id: &str, deadline: u64, before_disable: impl FnOnce()) {
            self.admit_from(1, id, deadline, before_disable);
        }
        fn admit_from(&mut self, peer: u8, id: &str, deadline: u64, before_disable: impl FnOnce()) {
            let slot = self
                .ledger
                .admit(peer, crate::rr_admission::RrRequestClass::Lower)
                .unwrap();
            let identity = RrIdentity {
                request_id: id.into(),
                deadline_ns: deadline,
                route: slot.identity(),
            };
            self.slots.push(slot);
            let pending = std::mem::replace(&mut self.pending, IpcPending::Idle);
            let input = BusinessObservation {
                now: robot_core::time::RobotClock::now(&self.clock),
                state: S0State::Safe,
                estop: true,
            };
            self.pending = match begin_lower_owned_impl(
                pending,
                Some(identity),
                LowerCause::OperatorDisable,
                &self.ctrl,
                &self.boot,
                input,
                before_disable,
            ) {
                LowerStartOutcome::Advanced(step) => {
                    assert!(step.replies.is_empty());
                    step.pending
                }
                other => panic!("真原请求应进入 Lower：{other:?}"),
            };
        }
        fn observe(&mut self, state: S0State, estop: bool) {
            let pending = std::mem::replace(&mut self.pending, IpcPending::Idle);
            self.pending = observe_business(
                pending,
                BusinessObservation {
                    now: robot_core::time::RobotClock::now(&self.clock),
                    state,
                    estop,
                },
            );
        }
        fn advance(&mut self) -> Vec<RetiredEnableReply> {
            let pending = std::mem::replace(&mut self.pending, IpcPending::Idle);
            let step = advance_owned(pending, &self.ctrl, &self.clock).unwrap();
            self.pending = step.pending;
            let replies: Vec<_> = step.replies.into_entries().collect();
            for reply in &replies {
                let index = self
                    .slots
                    .iter()
                    .position(|slot| slot.identity() == reply.identity.route)
                    .unwrap();
                self.ledger.release(self.slots.remove(index)).unwrap();
            }
            replies
        }
        fn lease(&self) -> robot_core::control_authority::NormalIoLease {
            self.ctrl
                .authority()
                .acquire_normal_io(robot_core::control_authority::NormalIoIntent::PeriodicRead)
                .unwrap()
        }
        fn execute_disable(&mut self) {
            use std::sync::atomic::Ordering;
            let inner = self.ctrl.inner();
            let raw = inner.pending_normal.swap(0, Ordering::AcqRel);
            let action = inner.decode_pending(raw).unwrap().expect("真实原提交序号");
            assert!(
                crate::rt_thread::execute_owned_normal(&mut mock_hard_adapter(), action, inner)
                    .is_none()
            );
        }
        fn finish_estop(&self) {
            use robot_core::adapter::RtSafeCtrl;
            assert!(self.ctrl.inner().claim_serial_estop());
            let observation = self.ctrl.authority().begin_safety_observation(
                robot_core::control_authority::SafetyOperation::ConfirmOff,
            );
            mock_hard_adapter().safety_stop_rt().unwrap();
            self.ctrl
                .authority()
                .complete_safety_observation(observation)
                .unwrap();
            assert!(self.ctrl.inner().claim_ipc_estop());
        }
    }

    #[test]
    fn fresh_lower_same_epoch_foreign_domain_returns_original_waiter_untouched() {
        let mut rig = FreshLowerRig::new();
        rig.admit("same-epoch-foreign", 100, || {});
        let foreign = FreshLowerRig::new();
        let _ = foreign.ctrl.authority().disable();
        assert_eq!(
            rig.ctrl.authority().snapshot().epoch,
            foreign.ctrl.authority().snapshot().epoch
        );
        assert_ne!(
            rig.ctrl.authority().domain(),
            foreign.ctrl.authority().domain()
        );
        let before = reply_snapshot(lower(&rig.pending).waiters.iter().next().unwrap());
        let original = std::mem::replace(&mut rig.pending, IpcPending::Idle);
        let Err(rejected) = advance_owned(original, &foreign.ctrl, &rig.clock) else {
            panic!("同数值epoch不能当同authority");
        };
        assert!(matches!(rejected.reason, BridgeRefusal::CrossDomain));
        rig.pending = rejected.pending;
        assert_eq!(
            reply_snapshot(lower(&rig.pending).waiters.iter().next().unwrap()),
            before
        );
        assert_eq!(rig.advance().len(), 1);
    }

    #[test]
    fn fresh_lower_real_lease_release_retires_once_without_false_preemption() {
        let mut rig = FreshLowerRig::new();
        let lease = rig.lease();
        rig.admit("historical", 20, || {});
        let boundary_epoch = lower(&rig.pending)
            .waiters
            .iter()
            .next()
            .unwrap()
            .fresh_disable
            .as_ref()
            .unwrap()
            .epoch;
        assert_eq!(boundary_epoch, rig.ctrl.authority().snapshot().epoch);
        rig.observe(S0State::Safe, true);
        assert!(rig.advance().is_empty());
        assert!(
            lower(&rig.pending)
                .waiters
                .iter()
                .next()
                .unwrap()
                .locked()
                .is_none()
        );
        rig.ctrl.authority().release_normal_io(lease).unwrap();
        let replies = rig.advance();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].identity.request_id, "historical");
        assert!(matches!(
            replies[0].outcome,
            PendingOutcome::Existing(SetEnabledRrPayload::Disabled { .. })
        ));
        assert_eq!(replies[0].effect, EnableEffect::Committed);
        assert!(rig.slots.is_empty());
        assert!(rig.advance().is_empty());
    }

    #[test]
    fn fresh_lower_new_estop_at_admission_or_between_observe_and_advance_stays_preempted() {
        for timing in 0..3 {
            let mut rig = FreshLowerRig::new();
            let lease = rig.lease();
            let authority = rig.ctrl.authority().clone();
            rig.admit("new-estop", 100, || {
                if timing == 0 {
                    let _ = authority.estop();
                }
            });
            if timing != 0 {
                rig.observe(S0State::Safe, true);
                assert!(rig.advance().is_empty());
                assert!(
                    lower(&rig.pending)
                        .waiters
                        .iter()
                        .next()
                        .unwrap()
                        .locked()
                        .is_none()
                );
                // timing=2 特意给过时的 estop=false；真实 snapshot 必须仍识别新撤销。
                rig.observe(S0State::Safe, timing != 2);
                let _ = authority.estop();
            } else {
                assert!(
                    lower(&rig.pending)
                        .waiters
                        .iter()
                        .next()
                        .unwrap()
                        .fresh_disable
                        .is_none()
                );
                rig.observe(S0State::Safe, true);
            }
            assert!(rig.advance().is_empty());
            assert!(matches!(
                lower(&rig.pending)
                    .waiters
                    .iter()
                    .next()
                    .unwrap()
                    .locked()
                    .unwrap()
                    .outcome(),
                PendingOutcome::Existing(SetEnabledRrPayload::PreemptedByEStop)
            ));
            authority.release_normal_io(lease).unwrap();
            rig.finish_estop();
            let replies = rig.advance();
            assert_eq!(replies.len(), 1);
            assert!(matches!(
                replies[0].outcome,
                PendingOutcome::Existing(SetEnabledRrPayload::PreemptedByEStop)
            ));
            assert_eq!(
                authority.snapshot().safety,
                OutputSafetyState::TorqueOffConfirmed
            );
        }
    }

    #[test]
    fn fresh_lower_ineligible_admission_keeps_original_failure_before_fast_advance() {
        use robot_core::adapter::RtSafeCtrl;
        use robot_core::control_authority::SafetyOperation;
        for timing in 0..3 {
            let mut rig = FreshLowerRig::new();
            let inner = rig.ctrl.inner().clone();
            let stop = || {
                assert!(inner.claim_serial_estop());
                let observed = inner
                    .authority()
                    .begin_safety_observation(SafetyOperation::ConfirmOff);
                mock_hard_adapter().safety_stop_rt().unwrap();
                inner
                    .authority()
                    .complete_safety_observation(observed)
                    .unwrap();
            };
            match timing {
                0 => {}
                1 => {
                    let _ = inner.authority().estop();
                    stop();
                    assert!(inner.authority().snapshot().ipc_estop_notice);
                }
                2 => {
                    let _ = inner.authority().shutdown();
                }
                _ => unreachable!(),
            }
            rig.admit("ineligible-fast", 100, || {
                if timing == 0 {
                    let _ = inner.authority().estop();
                    stop();
                    assert!(inner.claim_ipc_estop());
                }
            });
            let waiter = lower(&rig.pending).waiters.iter().next().unwrap();
            assert!(waiter.fresh_disable.is_none());
            assert_existing(waiter, SetEnabledRrPayload::PreemptedByEStop, 10);
            assert_eq!(
                inner.authority().snapshot().safety,
                OutputSafetyState::TorqueOffConfirmed
            );
            // 不先 observe：同 tick 快退仍由原锁存失败回包，不能被默认 Disabled 覆盖。
            let replies = rig.advance();
            assert_eq!(replies.len(), 1);
            assert!(matches!(
                replies[0].outcome,
                PendingOutcome::Existing(SetEnabledRrPayload::PreemptedByEStop)
            ));
            assert_eq!(replies[0].effect, EnableEffect::Committed);
            assert!(rig.slots.is_empty());
            assert!(rig.advance().is_empty());
        }
    }

    #[test]
    fn fresh_lower_deadline_equal_waits_but_current_time_over_locks_before_real_seq() {
        use robot_core::executor_ctrl::{AdapterNormalReq, NormalReqStatus};
        for late in [false, true] {
            let mut rig = FreshLowerRig::new();
            let lease = rig.lease();
            rig.admit("deadline", 20, || {});
            // 原 Lower 真实请求按值关联相同 seq；尚未由执行器取走，不能伪造完成。
            let seq = match rig.ctrl.request_normal(AdapterNormalReq::Enable(false)) {
                NormalReqStatus::Submitted { seq } => seq,
                other => panic!("{other:?}"),
            };
            rig.pending = record_submitted(rig.pending, seq).unwrap();
            rig.clock.set(MonoRawNs(20));
            rig.observe(S0State::Safe, true);
            if late {
                rig.clock.set(MonoRawNs(21));
            }
            assert!(rig.advance().is_empty());
            let reply = lower(&rig.pending).waiters.iter().next().unwrap();
            if late {
                assert_existing(reply, SetEnabledRrPayload::DeadlineExceeded, 21);
            } else {
                assert!(reply.locked().is_none());
            }
            rig.ctrl.authority().release_normal_io(lease).unwrap();
            rig.execute_disable();
            let replies = rig.advance();
            assert_eq!(replies.len(), 1);
            if late {
                assert!(matches!(
                    replies[0].outcome,
                    PendingOutcome::Existing(SetEnabledRrPayload::DeadlineExceeded)
                ));
            } else {
                assert!(matches!(
                    replies[0].outcome,
                    PendingOutcome::Existing(SetEnabledRrPayload::Disabled { .. })
                ));
            }
        }
    }

    #[test]
    fn fresh_lower_fault_stopping_and_actual_shutdown_do_not_get_safe_success() {
        use robot_core::executor_ctrl::{AdapterNormalReq, NormalReqStatus};
        for cause in 0..3 {
            let mut rig = FreshLowerRig::new();
            let lease = rig.lease();
            rig.admit("failure", 100, || {});
            let seq = match rig.ctrl.request_normal(AdapterNormalReq::Enable(false)) {
                NormalReqStatus::Submitted { seq } => seq,
                other => panic!("{other:?}"),
            };
            rig.pending = record_submitted(rig.pending, seq).unwrap();
            match cause {
                0 => rig.observe(S0State::Fault(FaultCode::CommFault), true),
                1 => rig.observe(S0State::Stopping, true),
                2 => {
                    rig.observe(S0State::Safe, false);
                    let _ = rig.ctrl.authority().shutdown();
                }
                _ => unreachable!(),
            }
            assert!(rig.advance().is_empty(), "原seq未完成只能保留");
            let first = reply_snapshot(lower(&rig.pending).waiters.iter().next().unwrap());
            assert!(first.2.is_some());
            rig.observe(S0State::Safe, true);
            assert!(rig.advance().is_empty());
            assert_eq!(
                reply_snapshot(lower(&rig.pending).waiters.iter().next().unwrap()),
                first
            );
            rig.ctrl.authority().release_normal_io(lease).unwrap();
            rig.execute_disable();
            let replies = rig.advance();
            assert_eq!(replies.len(), 1);
            assert!(!matches!(
                replies[0].outcome,
                PendingOutcome::Existing(SetEnabledRrPayload::Disabled { .. })
            ));
            assert_eq!(replies[0].outcome, first.2.unwrap().0);
        }
    }

    #[test]
    fn fresh_lower_appended_waiter_and_old_lower_do_not_inherit_or_refresh_boundary() {
        let mut rig = FreshLowerRig::new();
        let lease = rig.lease();
        rig.admit("first", 100, || {});
        let epoch = lower(&rig.pending)
            .waiters
            .iter()
            .next()
            .unwrap()
            .fresh_disable
            .as_ref()
            .unwrap()
            .epoch;
        // 每 peer 的 Lower share 固定为 1；第二原请求来自另一个真实 peer。
        rig.admit_from(2, "appended", 100, || {});
        {
            let mut waiters = lower(&rig.pending).waiters.iter();
            assert_eq!(
                waiters
                    .next()
                    .unwrap()
                    .fresh_disable
                    .as_ref()
                    .unwrap()
                    .epoch,
                epoch
            );
            assert!(waiters.next().unwrap().fresh_disable.is_none());
        }
        rig.observe(S0State::Safe, true);
        assert!(rig.advance().is_empty());
        for reply in lower(&rig.pending).waiters.iter() {
            assert!(matches!(
                reply.locked().unwrap().outcome(),
                PendingOutcome::Existing(SetEnabledRrPayload::PreemptedByEStop)
            ));
        }
        rig.ctrl.authority().release_normal_io(lease).unwrap();
        assert_eq!(rig.advance().len(), 2);
        assert!(rig.slots.is_empty());
        rig.pending =
            IpcPending::Lower(new_lower(LowerCause::OperatorDisable, MonoRawNs(10)).unwrap());
        rig.admit("old-lower", 100, || {});
        assert!(
            lower(&rig.pending)
                .waiters
                .iter()
                .all(|reply| reply.fresh_disable.is_none())
        );
        rig.observe(S0State::Safe, true);
        assert!(matches!(
            rig.advance()[0].outcome,
            PendingOutcome::Existing(SetEnabledRrPayload::PreemptedByEStop)
        ));
    }

    #[cfg(feature = "test-comm-fault-hook")]
    #[test]
    fn fresh_lower_permission_max_cannot_create_boundary_or_retire() {
        let authority = ControlAuthority::new_with_permission_epoch(
            AuthorityInit::new(
                true,
                OutputSafetyState::TorqueOffConfirmed,
                InhibitSet::EMPTY,
            ),
            PermissionEpoch::MAX,
        )
        .unwrap();
        let mut rig = FreshLowerRig::with_authority(authority);
        rig.admit("exhausted", 100, || {});
        assert_existing(
            lower(&rig.pending).waiters.iter().next().unwrap(),
            SetEnabledRrPayload::PreemptedByEStop,
            10,
        );
        assert!(rig.ctrl.authority().snapshot().exhausted);
        assert!(
            lower(&rig.pending)
                .waiters
                .iter()
                .next()
                .unwrap()
                .fresh_disable
                .is_none()
        );
        rig.observe(S0State::Safe, true);
        assert!(rig.advance().is_empty());
        assert!(matches!(rig.pending, IpcPending::Lower(_)));
        assert_eq!(rig.slots.len(), 1);
    }

    #[test]
    fn bridge_retains_taken_request_until_real_start_rejection_then_lower_cleanup() {
        use robot_core::executor_ctrl::AdapterErr;
        use std::sync::{Arc, atomic::Ordering};
        let inner = Arc::new(crate::hard_rt_ctrl::HardRtCtrlInner::new(Arc::new(
            authority(),
        )));
        let client = HardRtCtrlClient::new(inner.clone());
        let pending = bridge_started(&client, rr("先取后撤", 100));
        let seq = pending.seq().unwrap();
        // 执行器已取出 packed 请求，但尚未 take 真实 StartView。
        let packet = inner
            .decode_pending(inner.pending_normal.swap(0, Ordering::AcqRel))
            .unwrap()
            .unwrap();
        let boot = crate::boot_supervisor::BootShared::new(MonoRawNs(0));
        let clock = robot_core::time::MockClock::new(MonoRawNs(3));
        let lowered = match begin_lower_owned(
            IpcPending::Enable(pending),
            None,
            LowerCause::OperatorDisable,
            &client,
            &boot,
            observation(2, S0State::Safe),
        ) {
            LowerStartOutcome::Advanced(step) => step,
            other => panic!("同域 Lower 必须保留原责任：{other:?}"),
        };
        assert!(lowered.replies.is_empty());
        let lower = match lowered.pending {
            IpcPending::Lower(lower) => lower,
            other => panic!("{other:?}"),
        };
        assert_eq!(lower.carried_enable().unwrap().source_seq(), Some(seq));
        assert!(inner.start_slot().is_empty());
        assert_eq!(client.poll_last_normal_resp(seq), None);
        let mut adapter = mock_hard_adapter();
        assert!(crate::rt_thread::execute_owned_normal(&mut adapter, packet, &inner).is_none());
        assert_eq!(
            client.poll_last_normal_resp(seq),
            Some(AdapterResp::Err(AdapterErr::AuthorityRevoked))
        );
        let completed = advance_owned(IpcPending::Lower(lower), &client, &clock).unwrap();
        assert!(matches!(completed.pending, IpcPending::Idle));
        let replies: Vec<_> = completed.replies.into_entries().collect();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].identity.request_id, "先取后撤");
        assert_eq!(replies[0].effect, EnableEffect::Started);
        assert!(matches!(
            replies[0].outcome,
            PendingOutcome::SupersededByDisable
        ));
    }

    #[cfg(feature = "test-comm-fault-hook")]
    #[test]
    fn exhausted_off_observation_cannot_retire_lower_or_publish_generation() {
        use robot_core::control_authority::PermissionEpoch;
        let authority = std::sync::Arc::new(
            ControlAuthority::new_with_permission_epoch(
                AuthorityInit::new(
                    false,
                    OutputSafetyState::TorqueOffConfirmed,
                    InhibitSet::EMPTY,
                ),
                PermissionEpoch::MAX,
            )
            .unwrap(),
        );
        let _ = authority.disable();
        assert!(authority.snapshot().exhausted);
        assert_eq!(
            authority.snapshot().safety,
            OutputSafetyState::TorqueOffConfirmed
        );
        let ctrl = HardRtCtrlClient::new(std::sync::Arc::new(
            crate::hard_rt_ctrl::HardRtCtrlInner::new(authority),
        ));
        let lower = LowerPending {
            seq: None,
            cause: LowerCause::AwaitConvergence,
            carried_enable: None,
            waiters: LowerWaiters::empty(),
            started_at: MonoRawNs(1),
            shutdown_generation: std::num::NonZeroU64::new(9),
            completion: None,
        };
        assert_eq!(lower.shutdown_generation(), std::num::NonZeroU64::new(9));
        // 此处只核桥的负面拒绝，不把私有测试中的 g 作为生产 ACK 证明。
        let step = advance_lower(lower, &ctrl, &robot_core::time::MockClock::default()).unwrap();
        assert!(
            matches!(&step.pending, IpcPending::Lower(lower) if lower.shutdown_generation() == std::num::NonZeroU64::new(9))
        );
        assert_eq!(step.lowered_generation, None);
        assert!(step.replies.is_empty());
    }

    #[test]
    fn hard_rt_monotonic_estop_consumes_only_executor_notice_and_retires_real_lower() {
        use std::time::{Duration, Instant};
        const CHILD: &str = "ROBOT_W3_HARD_RT_ESTOP_CHILD";
        const NAME: &str = "ipc_pending::tests::hard_rt_monotonic_estop_consumes_only_executor_notice_and_retires_real_lower";
        if std::env::var_os(CHILD).is_some() {
            run_hard_rt_estop_child();
            std::process::exit(0);
        }
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("hard_rt_estop.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env(CHILD, "1")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                result => {
                    let _ = child.kill();
                    let reap_deadline = Instant::now() + Duration::from_secs(2);
                    loop {
                        if matches!(child.try_wait(), Ok(Some(_))) {
                            break;
                        }
                        if Instant::now() >= reap_deadline {
                            std::process::abort();
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    panic!("真实 HardRT 子进程超时/等待失败，已 kill/reap: {result:?}");
                }
            }
        };
        let output = std::fs::read_to_string(log_path).unwrap();
        assert!(
            status.success(),
            "真实 HardRT 急停链失败: {status} {output}"
        );
        assert_eq!(
            output.matches("W3_HARD_RT_ESTOP_LOWER_VERIFIED").count(),
            1,
            "{output}"
        );
        eprintln!("W3_HARD_RT_ESTOP_LOWER_CHILD_OK reaped=true");
    }

    #[allow(
        clippy::too_many_lines,
        reason = "同一受监督子进程持原 worker、owner 与 Slot，连续核实际急停到退休全链"
    )]
    fn run_hard_rt_estop_child() {
        use crate::rr_admission::{CheckedLimits, PeerLedger, RrRequestClass};
        use robot_core::adapter::{JointCommandBuf, RtSnapshot};
        use robot_core::executor_ctrl::{AdapterNormalReq, NormalReqStatus, SerialEmergencyCtrl};
        use robot_core::time::{RobotClock, SystemClock};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::time::Duration;

        // 失败时保留原 worker、owner 与 Slot 到真实子进程退出；worker panic 仍真 join Err。
        let main_thread = std::thread::current().id();
        std::panic::set_hook(Box::new(move |info| {
            eprintln!("HardRT 急停验收失败: {info}");
            if std::thread::current().id() == main_thread {
                std::process::exit(101);
            }
        }));
        {
            let authority = Arc::new(
                ControlAuthority::new(AuthorityInit::new(
                    false,
                    OutputSafetyState::TorqueUnknown,
                    InhibitSet::EMPTY,
                ))
                .unwrap(),
            );
            let inner = Arc::new(crate::hard_rt_ctrl::HardRtCtrlInner::new(authority.clone()));
            let client = HardRtCtrlClient::new(inner.clone());
            let clock = Arc::new(SystemClock::new());
            let (_commands, reader) =
                robot_transport::LatestCommandSlot::new(JointCommandBuf::default());
            let (publisher, _snapshot) =
                robot_transport::RtSnapshotPublisher::new(RtSnapshot::default());
            let handle = crate::rt_thread::spawn_rt_thread(
                crate::rt_thread::RtThreadConfig {
                    strict_rt: false,
                    cpu_affinity: None,
                    ..crate::rt_thread::RtThreadConfig::default()
                },
                clock.clone(),
                robot_adapter_mock::MockHardRTAdapter::with_unlimited_limits(2),
                reader,
                publisher,
                inner.clone(),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
            assert!(crate::test_util::wait_until(
                Duration::from_secs(2),
                || authority.snapshot().safety == OutputSafetyState::TorqueOffConfirmed
                    && handle.metrics().tick_count.load(Ordering::Acquire) > 0
            ));
            let (owner, view) = authority.reserve_enable().unwrap();
            inner.start_slot().post(view).unwrap();
            let sequence = match client.request_normal(AdapterNormalReq::Enable(true)) {
                NormalReqStatus::Submitted { seq } => seq,
                other => panic!("真实 Enable 未接纳: {other:?}"),
            };
            assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
                client.poll_last_normal_resp(sequence).is_some()
            }));
            assert_eq!(
                client.poll_last_normal_resp(sequence),
                Some(AdapterResp::Ok)
            );
            authority.commit_enable(owner).unwrap();
            assert_eq!(authority.snapshot().safety, OutputSafetyState::TorqueOn);
            let stops_before = handle
                .metrics()
                .emergency_safety_stop_count
                .load(Ordering::Acquire);
            let _ = client.estop_monotonic();
            let limits =
                CheckedLimits::try_from_limits(&robot_transport::iceoryx2::RrStaticLimits {
                    c: 1,
                    l: 4,
                })
                .unwrap();
            let mut ledger = PeerLedger::<u8>::new(&limits).unwrap();
            let slot = ledger.admit(1, RrRequestClass::Lower).unwrap();
            let route = slot.identity();
            let now = clock.now();
            let request = RrIdentity {
                request_id: "真实急停后的 Lower".into(),
                deadline_ns: now.0 + 2_000_000_000,
                route,
            };
            let boot = crate::boot_supervisor::BootShared::new(now);
            let mut pending = match begin_lower_owned(
                IpcPending::Idle,
                Some(request),
                LowerCause::AwaitConvergence,
                &client,
                &boot,
                BusinessObservation {
                    now,
                    state: S0State::Safe,
                    estop: true,
                },
            ) {
                LowerStartOutcome::Advanced(step) => step.pending,
                other => panic!("原 Lower 身份必须进入桥: {other:?}"),
            };
            assert!(
                matches!(&pending, IpcPending::Lower(lower) if lower.shutdown_generation().is_none()),
                "AwaitConvergence 不得伪造 Shutdown generation"
            );
            assert!(crate::test_util::wait_until(
                Duration::from_secs(2),
                || !authority.snapshot().serial_estop_pending
                    && authority.snapshot().safety == OutputSafetyState::TorqueOffConfirmed
                    && handle
                        .metrics()
                        .emergency_safety_stop_count
                        .load(Ordering::Acquire)
                        == stops_before + 1
            ));
            assert!(
                authority.snapshot().ipc_estop_notice,
                "执行器只消费自己的义务"
            );
            assert!(inner.claim_ipc_estop(), "同一 IPC 原 claim 必须仍可取通知");
            assert!(!inner.claim_ipc_estop());
            let completed = advance_owned(pending, &client, clock.as_ref()).unwrap();
            pending = completed.pending;
            assert!(matches!(pending, IpcPending::Idle));
            assert!(completed.commit.is_none() && completed.lowered_generation.is_none());
            let mut replies = completed.replies.into_entries();
            let reply = replies.next().unwrap();
            assert!(replies.next().is_none());
            assert_eq!(reply.identity.route, route);
            assert_eq!(reply.identity.request_id, "真实急停后的 Lower");
            assert!(matches!(
                reply.outcome,
                PendingOutcome::Existing(SetEnabledRrPayload::Disabled { .. })
            ));
            ledger.release(slot).unwrap();
            assert!(
                advance_owned(pending, &client, clock.as_ref())
                    .unwrap()
                    .replies
                    .is_empty()
            );
            let tick_before = handle.metrics().tick_count.load(Ordering::Acquire);
            assert!(crate::test_util::wait_until(
                Duration::from_secs(2),
                || handle.metrics().tick_count.load(Ordering::Acquire) >= tick_before + 2
            ));
            assert_eq!(
                handle
                    .metrics()
                    .emergency_safety_stop_count
                    .load(Ordering::Acquire),
                stops_before + 1,
                "本次实际 claim 只执行一次 Stop，后 tick 不得重复停止"
            );
            assert!(authority.snapshot().latch && !authority.snapshot().ipc_estop_notice);
            handle.request_stop();
            assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
                handle.is_finished()
            }));
            let exit = handle.join().unwrap();
            assert!(!exit.has_pending_enable());
        }
        println!("W3_HARD_RT_ESTOP_LOWER_VERIFIED");
    }
}
