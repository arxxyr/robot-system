//! IPC 待办的唯一请求责任、首次业务锁存与受检 Enable 退休桥。
//!
//! seq 与业务观察只记录来源；本模块不提交 adapter、不清 seq、不回包。
//! owner 退休也不代表原请求或总线动作已完成，生产接线另行验收。

// 完整返还固定 16 个 waiter 的责任，不能用 Box 降低错误大小或额外分配。
#![allow(clippy::result_large_err, reason = "拒绝必须按值返还全部固定请求责任")]

use robot_core::control_authority::{
    ControlAuthority, DomainId, EnableOwner, PermissionEpoch, Phase, Refusal, StartViewSlot,
};
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

#[derive(Debug)]
pub struct PendingReply {
    identity: RrIdentity,
    locked: Option<LockedOutcome>,
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
        if self.locked.is_none() {
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LowerCause {
    OperatorDisable,
    PostEnableCleanup,
    AwaitConvergence,
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
}
impl LowerPending {
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
        }),
        lower_waiters: LowerWaiters::empty(),
        revoked_by: None,
        started_at,
    }
}

pub fn new_lower(
    cause: LowerCause,
    started_at: MonoRawNs,
) -> Result<LowerPending, PendingRejected<(LowerCause, MonoRawNs)>> {
    match cause {
        LowerCause::PostEnableCleanup => Err(PendingRejected {
            input: (cause, started_at),
            reason: PendingRefusal::CleanupRequiresRetiredEnable,
        }),
        LowerCause::OperatorDisable | LowerCause::AwaitConvergence => Ok(LowerPending {
            seq: None,
            cause,
            carried_enable: None,
            waiters: LowerWaiters::empty(),
            started_at,
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
    mut pending: IpcPending,
    rr: RrIdentity,
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
                locked: None,
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
            LowerCause::OperatorDisable | LowerCause::PostEnableCleanup => {
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
    LowerPending {
        seq: None,
        cause: LowerCause::PostEnableCleanup,
        carried_enable: Some(retired),
        waiters,
        started_at,
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
        #[cfg(test)]
        || {},
    )
}

fn retire_enable_pending_impl(
    pending: EnablePending,
    authority: &ControlAuthority,
    start_slot: &StartViewSlot,
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
        Some(Phase::EnableQueued | Phase::EnableSucceeded) if pending.revoked_by.is_none() => {
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
            },
            reason: EnableRetireRefusal::Authority(refused.reason),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_machine::{ControlLink, OutputMode, TransitionKind};
    use robot_core::control_authority::{AuthorityInit, EnableResult, InhibitSet};
    use robot_core::executor_ctrl::OutputSafetyState;

    fn authority() -> ControlAuthority {
        ControlAuthority::new(AuthorityInit::new(
            false,
            OutputSafetyState::TorqueOffConfirmed,
            InhibitSet::EMPTY,
        ))
        .unwrap()
    }
    fn rr(id: &str, deadline_ns: u64) -> RrIdentity {
        RrIdentity {
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
        let rejected = retire_enable_pending_impl(pending, &authority, &slot, || {
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
}
