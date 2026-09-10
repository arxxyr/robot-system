//! 无生产调用方的 Clear 协调原语：唯一许可、单在飞命令与真实退休后移交回复。
//!
//! 所有文件工作经现有 writer mailbox；本模块不触文件、不运行 adapter、不发送 RR。
//! `advance` 是普通线程路径，每次最多消费一条完成并返回本 tick 的有界事件。

use crate::clear_estop_transaction::ClearTxnStage;
pub use crate::ipc_pending::RrIdentity;
use crate::latch_writer::{
    AnnounceError, ClearTxnIdentity, DispatchRefused, PersistClient, PersistCommand,
    PersistCompletion, RemoveTokenKind, WriterRefusal,
};
use robot_core::control_authority::{
    CleanupKind, CleanupPermit, ClearFinalizingOwner, ClearOwner, ClearPendingOwner,
    ClearPendingRetired, ClearRetirement, ControlAuthority, Phase, Refusal, RepairPermit,
    RepairTarget,
};
use robot_core::executor_ctrl::OutputSafetyState;
use robot_core::time::{MonoRawNs, RobotClock};
use std::num::NonZeroU128;

/// 按值保存真实 A2 返回，不能从摘要重建。
#[derive(Debug)]
pub enum HeldPermit {
    Pending(ClearPendingOwner),
    Finalizing(ClearFinalizingOwner),
    Owner(ClearOwner),
    Repair(RepairPermit),
    Cleanup(CleanupPermit),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchIntent {
    Initial,
    NextStage,
    Retry,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchFault {
    NoCredit,
    AnnounceInFlight,
    AnnounceOverflow,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StageKind {
    Preparing,
    Finalizing,
    FinalizeCleanup,
    Repairing,
    RepairClean,
    Resyncing,
    AwaitRetry,
    AwaitCapacity,
    Stuck,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolFault {
    IdentityMismatch,
    TokenStateMismatch,
    NoTransaction,
    UnexpectedCompletion {
        stage: StageKind,
        completion: PersistCompletion,
    },
    TargetMismatch,
    KindMismatch,
    AuthorityWrongPhase(Phase),
    CorruptedPhase,
    CrossDomain,
    Dispatch(DispatchFault),
    AuthorityRefused(Refusal),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StuckReason {
    WriterGone,
    MarkerLost,
    ProtocolBug(ProtocolFault),
    RepairExhausted,
    PairMismatch,
    RetireRefused(Refusal),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClearStage {
    Preparing,
    Finalizing,
    FinalizeCleanup,
    Repairing,
    RepairClean,
    Resyncing,
    AwaitRetry {
        command: PersistCommand,
        due_at_ns: u64,
    },
    AwaitCapacity {
        command: PersistCommand,
        intent: DispatchIntent,
    },
    Stuck(StuckReason),
}
impl ClearStage {
    #[must_use]
    pub const fn kind(self) -> StageKind {
        match self {
            Self::Preparing => StageKind::Preparing,
            Self::Finalizing => StageKind::Finalizing,
            Self::FinalizeCleanup => StageKind::FinalizeCleanup,
            Self::Repairing => StageKind::Repairing,
            Self::RepairClean => StageKind::RepairClean,
            Self::Resyncing => StageKind::Resyncing,
            Self::AwaitRetry { .. } => StageKind::AwaitRetry,
            Self::AwaitCapacity { .. } => StageKind::AwaitCapacity,
            Self::Stuck(_) => StageKind::Stuck,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupersedeReason {
    Shutdown,
    Authority(Refusal),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClearOutcome {
    Cleared {
        cleared_at_ns: u64,
        superseded: bool,
    },
    Superseded(SupersedeReason),
    PreemptedByEmergency,
    FsyncFailed {
        stage: ClearTxnStage,
    },
    PersistPending,
    WriterUnavailable,
    AlreadyInProgress,
    BusyLowering,
    AuthorityRefused(Refusal),
    WouldClear {
        at: u64,
    },
    StateNotSafe,
    NotTorqueOffConfirmed {
        safety: OutputSafetyState,
    },
    DeadlineExceeded,
}
#[derive(Debug)]
pub enum ReplyState {
    Pending {
        rr: RrIdentity,
        locked: Option<ClearOutcome>,
    },
    Replied,
    None,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetirementRecord {
    Cleanup(ClearRetirement),
    Unprepared(ClearPendingRetired),
}
/// 真实退休事件携带的 POD 事实，不含权限许可或请求责任。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetiredView {
    pub txn: ClearTxnIdentity,
    pub retirement: RetirementRecord,
    pub logical_committed_at: Option<u64>,
    pub physical_done_at: Option<u64>,
}
#[derive(Debug)]
pub enum ClearEvent {
    Reply {
        rr: RrIdentity,
        outcome: ClearOutcome,
        retired: bool,
    },
    Retired(RetiredView),
    StuckEntered(StuckReason),
    CommitLogically {
        at: u64,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Expected {
    pub target: Option<RepairTarget>,
    pub kind: Option<CleanupKind>,
}
impl Expected {
    const fn for_command(command: PersistCommand) -> Self {
        match command {
            PersistCommand::RepairClear { target, .. } => Self {
                target: Some(target),
                kind: None,
            },
            PersistCommand::RemoveMarker { kind, .. } => Self {
                target: None,
                kind: Some(kind),
            },
            PersistCommand::PrepareClear { .. } | PersistCommand::ResyncRemoval { .. } => Self {
                target: None,
                kind: None,
            },
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InFlight {
    pub command: PersistCommand,
    pub intent: DispatchIntent,
    pub expected: Expected,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryFlight {
    pub txn: ClearTxnIdentity,
    pub command: PersistCommand,
    pub stage: StageKind,
    pub expected: Expected,
    pub counted: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnsyncedBeforeRepair {
    pub target: RepairTarget,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingDispatch {
    pub command: PersistCommand,
    pub intent: DispatchIntent,
}

// 不含许可的内部业务上下文，允许权限转换时始终由局部变量唯一持有原令牌。
#[derive(Debug)]
struct TxnContext {
    txn: ClearTxnIdentity,
    stage: ClearStage,
    reply: ReplyState,
    started_at_ns: u64,
    logical_committed_at: Option<u64>,
    physical_done_at: Option<u64>,
    in_flight: Option<InFlight>,
    flight: Option<RetryFlight>,
    retry_accepted: u8,
    continuation: Option<UnsyncedBeforeRepair>,
    superseded: Option<SupersedeReason>,
    next: Option<PendingDispatch>,
    retry_action: RetryAction,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetryAction {
    Keep,
    RepairIfLatched,
    RepairAfterOutstanding,
    PairMismatch,
    CleanupIfLatched,
}
impl TxnContext {
    fn lock(&mut self, outcome: ClearOutcome) {
        if let ReplyState::Pending { locked, .. } = &mut self.reply
            && locked.is_none()
        {
            *locked = Some(outcome);
        }
    }
    fn stuck(&mut self, reason: StuckReason, events: &mut Vec<ClearEvent>) {
        if !matches!(self.stage, ClearStage::Stuck(_)) {
            self.stage = ClearStage::Stuck(reason);
            events.push(ClearEvent::StuckEntered(reason));
        }
    }
    fn plan(&mut self, command: PersistCommand, intent: DispatchIntent) {
        self.next = Some(PendingDispatch { command, intent });
    }
    fn count_claim(&mut self, claimed: bool) {
        if let Some(flight) = &mut self.flight
            && !flight.counted
            && claimed
        {
            self.retry_accepted += 1;
            flight.counted = true;
        }
    }
}

/// 活动事务只借用观察，许可与请求身份不允许克隆或抽出。
///
/// ```compile_fail
/// use robot_s0::clear_coordination::ClearTxnState;
/// fn duplicate(state: ClearTxnState) { let _ = state.clone(); }
/// ```
#[derive(Debug)]
pub struct ClearTxnState {
    held: HeldPermit,
    context: TxnContext,
}
impl ClearTxnState {
    #[must_use]
    pub const fn held(&self) -> &HeldPermit {
        &self.held
    }
    #[must_use]
    pub const fn txn(&self) -> ClearTxnIdentity {
        self.context.txn
    }
    #[must_use]
    pub const fn stage(&self) -> ClearStage {
        self.context.stage
    }
    #[must_use]
    pub const fn reply(&self) -> &ReplyState {
        &self.context.reply
    }
    #[must_use]
    pub const fn started_at_ns(&self) -> u64 {
        self.context.started_at_ns
    }
    #[must_use]
    pub const fn logical_committed_at(&self) -> Option<u64> {
        self.context.logical_committed_at
    }
    #[must_use]
    pub const fn physical_done_at(&self) -> Option<u64> {
        self.context.physical_done_at
    }
    #[must_use]
    pub const fn in_flight(&self) -> Option<InFlight> {
        self.context.in_flight
    }
    #[must_use]
    pub const fn flight(&self) -> Option<RetryFlight> {
        self.context.flight
    }
    #[must_use]
    pub const fn retry_accepted(&self) -> u8 {
        self.context.retry_accepted
    }
    #[must_use]
    pub const fn continuation(&self) -> Option<UnsyncedBeforeRepair> {
        self.context.continuation
    }
    #[must_use]
    pub const fn superseded(&self) -> Option<SupersedeReason> {
        self.context.superseded
    }
    #[must_use]
    pub const fn next(&self) -> Option<PendingDispatch> {
        self.context.next
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartInputs {
    pub state_safe: bool,
    pub bus_idle_no_lower_obligation: bool,
    pub obligation_settled: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdvanceInputs {
    pub now_ns: u64,
    pub draining: bool,
    pub estop_seen_since: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchState {
    Queued,
    AwaitCapacity,
    WriterGone,
    Fault(DispatchFault),
}
#[derive(Debug)]
pub enum StartOutcome {
    RefusedBeforeReserve {
        rr: RrIdentity,
        outcome: ClearOutcome,
    },
    Retained {
        dispatch: DispatchState,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    pub first_backoff_ns: u64,
    pub cap_ns: u64,
    pub max_accepted: u8,
}
impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            first_backoff_ns: 200_000_000,
            cap_ns: 2_000_000_000,
            max_accepted: 8,
        }
    }
}
impl RetryPolicy {
    fn due_at(self, now: u64, accepted: u8, draining: bool) -> u64 {
        let backoff = if draining {
            0
        } else {
            self.first_backoff_ns
                .saturating_mul(1_u64.checked_shl(u32::from(accepted)).unwrap_or(u64::MAX))
                .min(self.cap_ns)
        };
        now.saturating_add(backoff)
    }
}

#[derive(Debug)]
pub struct ClearCoordination {
    txn: Option<ClearTxnState>,
    retry: RetryPolicy,
}
impl ClearCoordination {
    #[must_use]
    pub const fn new(retry: RetryPolicy) -> Self {
        Self { txn: None, retry }
    }
    #[must_use]
    pub const fn view(&self) -> Option<&ClearTxnState> {
        self.txn.as_ref()
    }
    #[must_use]
    pub const fn is_retired_or_absent(&self) -> bool {
        self.txn.is_none()
    }
    #[must_use]
    pub const fn stuck(&self) -> Option<StuckReason> {
        match &self.txn {
            Some(state) => match state.context.stage {
                ClearStage::Stuck(reason) => Some(reason),
                _ => None,
            },
            None => None,
        }
    }

    #[must_use]
    pub fn start(
        &mut self,
        rr: RrIdentity,
        txn_id: NonZeroU128,
        inputs: &StartInputs,
        authority: &ControlAuthority,
        client: &mut PersistClient,
        now_ns: u64,
    ) -> StartOutcome {
        self.start_impl(rr, txn_id, *inputs, authority, client, now_ns, None)
    }

    /// 将 IPC 在本 tick 急停公告消费前观察到的代次绑定到真实预约 CAS。
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "保留已批准的原身份、预约代次和真实持久化端点逐项接线"
    )]
    pub fn start_with_epoch(
        &mut self,
        rr: RrIdentity,
        txn_id: NonZeroU128,
        inputs: &StartInputs,
        authority: &ControlAuthority,
        client: &mut PersistClient,
        now_ns: u64,
        expected_epoch: robot_core::control_authority::PermissionEpoch,
    ) -> StartOutcome {
        self.start_impl(
            rr,
            txn_id,
            *inputs,
            authority,
            client,
            now_ns,
            Some(expected_epoch),
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "保留已批准的原身份、预约代次和真实持久化端点逐项接线"
    )]
    fn start_impl(
        &mut self,
        rr: RrIdentity,
        txn_id: NonZeroU128,
        inputs: StartInputs,
        authority: &ControlAuthority,
        client: &mut PersistClient,
        now_ns: u64,
        expected_epoch: Option<robot_core::control_authority::PermissionEpoch>,
    ) -> StartOutcome {
        let refused = match () {
            () if now_ns > rr.deadline_ns => Some(ClearOutcome::DeadlineExceeded),
            () if !inputs.state_safe => Some(ClearOutcome::StateNotSafe),
            () if self.txn.is_some() => Some(ClearOutcome::AlreadyInProgress),
            () if !inputs.bus_idle_no_lower_obligation => Some(ClearOutcome::BusyLowering),
            () if !inputs.obligation_settled => Some(ClearOutcome::PersistPending),
            () => None,
        };
        if let Some(outcome) = refused {
            return StartOutcome::RefusedBeforeReserve { rr, outcome };
        }
        // 无活动事务时先排空至多一条旧完成，再观察断链；没有跨 tick 缓存。
        let _ = client.try_complete();
        let _ = client.try_complete();
        if !client.alive() || !client.has_credit() {
            return StartOutcome::RefusedBeforeReserve {
                rr,
                outcome: ClearOutcome::WriterUnavailable,
            };
        }
        let reserved = match expected_epoch {
            Some(epoch) => authority.reserve_clear_if_epoch(epoch),
            None => authority.reserve_clear(),
        };
        let owner = match reserved {
            Ok(owner) => owner,
            Err(reason) => {
                let outcome = match reason {
                    Refusal::Busy(
                        Phase::EnableQueued
                        | Phase::EnableExecuting
                        | Phase::EnableSucceeded
                        | Phase::EnableFailed,
                    ) => ClearOutcome::AlreadyInProgress,
                    Refusal::SafetyNotOffConfirmed(safety) => {
                        ClearOutcome::NotTorqueOffConfirmed { safety }
                    }
                    other => ClearOutcome::AuthorityRefused(other),
                };
                return StartOutcome::RefusedBeforeReserve { rr, outcome };
            }
        };
        let txn = ClearTxnIdentity {
            txn_id: txn_id.get(),
            expected_epoch: owner.epoch().get(),
            original_latched: owner.original_latched(),
        };
        let mut state = ClearTxnState {
            held: HeldPermit::Pending(owner),
            context: TxnContext {
                txn,
                stage: ClearStage::Preparing,
                reply: ReplyState::Pending { rr, locked: None },
                started_at_ns: now_ns,
                logical_committed_at: None,
                physical_done_at: None,
                in_flight: None,
                flight: None,
                retry_accepted: 0,
                continuation: None,
                superseded: None,
                next: None,
                retry_action: RetryAction::Keep,
            },
        };
        let command = PersistCommand::PrepareClear { txn };
        // StartOutcome 已携 Stuck 分类，开始阶段没有向外伪发业务 Reply。
        let mut events = Vec::new();
        let dispatch = dispatch(
            &mut state.context,
            client,
            command,
            DispatchIntent::Initial,
            now_ns,
            self.retry,
            &mut events,
        );
        self.txn = Some(state);
        StartOutcome::Retained { dispatch }
    }

    /// 普通线程调用；同域 clock 用于真实逻辑提交后及完整匹配 Resynced 后的观测。
    /// 输出不跨 tick 积累，每次最多两条：退休→回复，或逻辑提交→首次 Stuck。
    #[must_use]
    pub fn advance(
        &mut self,
        authority: &ControlAuthority,
        client: &mut PersistClient,
        inputs: AdvanceInputs,
        clock: &dyn RobotClock,
    ) -> Vec<ClearEvent> {
        self.advance_impl(
            authority,
            client,
            inputs,
            clock,
            #[cfg(test)]
            None,
        )
    }

    fn advance_impl(
        &mut self,
        authority: &ControlAuthority,
        client: &mut PersistClient,
        inputs: AdvanceInputs,
        clock: &dyn RobotClock,
        #[cfg(test)] injected_completion: Option<PersistCompletion>,
    ) -> Vec<ClearEvent> {
        let mut events = Vec::new();
        let completion = client.try_complete();
        #[cfg(test)]
        let completion = match injected_completion {
            Some(injected) => {
                assert!(completion.is_none(), "畸形输入夹具不能覆盖真实缓冲完成");
                Some(injected)
            }
            None => completion,
        };
        let second = completion.and_then(|_| client.try_complete());
        let Some(mut state) = self.txn.take() else {
            return events;
        };
        state
            .context
            .count_claim(client.shared().announce.offered_at().is_none());
        if let Some(unexpected) = second {
            state.context.stuck(
                StuckReason::ProtocolBug(ProtocolFault::UnexpectedCompletion {
                    stage: state.context.stage.kind(),
                    completion: unexpected,
                }),
                &mut events,
            );
        }
        if let Some(completion) = completion
            && !matches!(state.context.stage, ClearStage::Stuck(_))
        {
            if let Err(fault) = match_completion(&state, completion) {
                state
                    .context
                    .stuck(StuckReason::ProtocolBug(fault), &mut events);
            } else {
                state.context.count_claim(true);
                state.context.flight = None;
                let flight = state.context.in_flight.take().expect("匹配已确认在飞");
                let ClearTxnState { held, mut context } = state;
                match consume_completion(
                    held,
                    &mut context,
                    flight.command,
                    completion,
                    authority,
                    inputs,
                    clock,
                    self.retry,
                    &mut events,
                ) {
                    Ok(held) => state = ClearTxnState { held, context },
                    Err(retirement) => {
                        emit_retirement(context, retirement, &mut events);
                        return events;
                    }
                }
            }
        }
        if !client.alive() && needs_writer(&state.context) {
            state.context.next = None;
            state.context.stuck(StuckReason::WriterGone, &mut events);
        }
        if client.alive() && !matches!(state.context.stage, ClearStage::Stuck(_)) {
            state = ready_dispatch(state, authority, client, inputs, self.retry, &mut events);
        }
        self.txn = Some(state);
        events
    }
}

fn needs_writer(context: &TxnContext) -> bool {
    context.in_flight.is_some()
        || context.next.is_some()
        || matches!(
            context.stage,
            ClearStage::AwaitRetry { .. }
                | ClearStage::AwaitCapacity { .. }
                | ClearStage::Preparing
                | ClearStage::Finalizing
                | ClearStage::Repairing
                | ClearStage::RepairClean
                | ClearStage::FinalizeCleanup
                | ClearStage::Resyncing
        )
}

fn dispatch(
    context: &mut TxnContext,
    client: &mut PersistClient,
    command: PersistCommand,
    intent: DispatchIntent,
    now_ns: u64,
    retry: RetryPolicy,
    events: &mut Vec<ClearEvent>,
) -> DispatchState {
    context.next = Some(PendingDispatch { command, intent });
    if !client.alive() {
        context.next = None;
        context.stuck(StuckReason::WriterGone, events);
        return DispatchState::WriterGone;
    }
    if intent == DispatchIntent::Retry && context.retry_accepted >= retry.max_accepted {
        context.stuck(StuckReason::RepairExhausted, events);
        // 仅 Start 返回 DispatchState；Retry 的具名原因保留于 stage。
        return DispatchState::AwaitCapacity;
    }
    let result = if client.has_credit() {
        client.try_dispatch(command, MonoRawNs(now_ns))
    } else {
        Err(DispatchRefused::NoCredit)
    };
    accept_dispatch_result(context, command, intent, result, events)
}

fn accept_dispatch_result(
    context: &mut TxnContext,
    command: PersistCommand,
    intent: DispatchIntent,
    result: Result<(), DispatchRefused>,
    events: &mut Vec<ClearEvent>,
) -> DispatchState {
    match result {
        Ok(()) => {
            context.next = None;
            context.stage = stage_for(command);
            let expected = Expected::for_command(command);
            context.in_flight = Some(InFlight {
                command,
                intent,
                expected,
            });
            context.flight = match intent {
                DispatchIntent::Retry => Some(RetryFlight {
                    txn: context.txn,
                    command,
                    stage: context.stage.kind(),
                    expected,
                    counted: false,
                }),
                DispatchIntent::Initial | DispatchIntent::NextStage => None,
            };
            DispatchState::Queued
        }
        Err(DispatchRefused::Full) => {
            context.next = None;
            context.stage = ClearStage::AwaitCapacity { command, intent };
            DispatchState::AwaitCapacity
        }
        Err(DispatchRefused::Disconnected) => {
            context.next = None;
            context.stuck(StuckReason::WriterGone, events);
            DispatchState::WriterGone
        }
        Err(refused @ (DispatchRefused::NoCredit | DispatchRefused::Announce(_))) => {
            let fault = match refused {
                DispatchRefused::NoCredit => DispatchFault::NoCredit,
                DispatchRefused::Announce(AnnounceError::AlreadyOffered) => {
                    DispatchFault::AnnounceInFlight
                }
                DispatchRefused::Announce(AnnounceError::Overflow) => {
                    DispatchFault::AnnounceOverflow
                }
                DispatchRefused::Full | DispatchRefused::Disconnected => unreachable!(),
            };
            context.stuck(
                StuckReason::ProtocolBug(ProtocolFault::Dispatch(fault)),
                events,
            );
            DispatchState::Fault(fault)
        }
    }
}

const fn stage_for(command: PersistCommand) -> ClearStage {
    match command {
        PersistCommand::PrepareClear { .. } => ClearStage::Preparing,
        PersistCommand::RepairClear { .. } => ClearStage::Repairing,
        PersistCommand::RemoveMarker {
            kind: CleanupKind::Finalize,
            ..
        } => ClearStage::FinalizeCleanup,
        PersistCommand::RemoveMarker {
            kind: CleanupKind::Repaired(_),
            ..
        } => ClearStage::RepairClean,
        PersistCommand::ResyncRemoval { .. } => ClearStage::Resyncing,
    }
}

#[allow(
    clippy::match_same_arms,
    reason = "各命令阶段保留独立许可和完成矩阵，便于逐项审查"
)]
fn match_completion(
    state: &ClearTxnState,
    completion: PersistCompletion,
) -> Result<(), ProtocolFault> {
    let context = &state.context;
    let unexpected = || ProtocolFault::UnexpectedCompletion {
        stage: context.stage.kind(),
        completion,
    };
    let flight = context.in_flight.ok_or_else(unexpected)?;
    let txn = match completion {
        PersistCompletion::Prepared { txn, .. }
        | PersistCompletion::PrepareFailed { txn, .. }
        | PersistCompletion::Repaired { txn, .. }
        | PersistCompletion::RepairFailed { txn, .. }
        | PersistCompletion::MarkerRemoved { txn, .. }
        | PersistCompletion::MarkerRemoveFailed { txn, .. }
        | PersistCompletion::Resynced { txn } => Some(txn),
        PersistCompletion::Refused { txn, .. } => txn,
    };
    if txn != Some(context.txn) || flight.command.txn() != context.txn {
        return Err(ProtocolFault::IdentityMismatch);
    }
    if context.stage != stage_for(flight.command)
        || flight.expected != Expected::for_command(flight.command)
    {
        return Err(unexpected());
    }
    // 协议拒绝也必须完整匹配命令；非法原因不能证明 writer 已处理本次在飞。
    if let PersistCompletion::Refused { reason, .. } = completion {
        let allowed = match reason {
            WriterRefusal::IdentityMismatch => return Err(ProtocolFault::IdentityMismatch),
            WriterRefusal::TokenStateMismatch => return Err(ProtocolFault::TokenStateMismatch),
            WriterRefusal::NoTransaction => return Err(ProtocolFault::NoTransaction),
            WriterRefusal::ShuttingDown => true,
            WriterRefusal::EStopPersistOutstanding => matches!(
                flight.command,
                PersistCommand::PrepareClear { .. } | PersistCommand::RepairClear { .. }
            ),
            WriterRefusal::PairMismatch => {
                matches!(flight.command, PersistCommand::RepairClear { .. })
            }
        };
        if !allowed {
            return Err(unexpected());
        }
    }
    let permitted = match (&state.held, flight.command, completion) {
        (
            HeldPermit::Pending(_),
            PersistCommand::PrepareClear { .. },
            PersistCompletion::Prepared { .. }
            | PersistCompletion::PrepareFailed { .. }
            | PersistCompletion::Refused { .. },
        ) => true,
        (
            HeldPermit::Repair(permit),
            PersistCommand::RepairClear { target, .. },
            PersistCompletion::Repaired { target: actual, .. },
        ) => {
            if target != actual || permit.target() != actual {
                return Err(ProtocolFault::TargetMismatch);
            }
            true
        }
        (
            HeldPermit::Repair(permit),
            PersistCommand::RepairClear { target, .. },
            PersistCompletion::RepairFailed { .. } | PersistCompletion::Refused { .. },
        ) => permit.target() == target,
        (
            HeldPermit::Cleanup(permit),
            PersistCommand::RemoveMarker { kind, .. },
            PersistCompletion::MarkerRemoved { kind: actual, .. },
        ) => {
            if kind != actual || permit.kind() != actual {
                return Err(ProtocolFault::KindMismatch);
            }
            true
        }
        (
            HeldPermit::Cleanup(permit),
            PersistCommand::RemoveMarker { kind, .. },
            PersistCompletion::MarkerRemoveFailed { .. } | PersistCompletion::Refused { .. },
        ) => permit.kind() == kind,
        (
            HeldPermit::Cleanup(_),
            PersistCommand::ResyncRemoval { .. },
            PersistCompletion::Resynced { .. }
            | PersistCompletion::MarkerRemoveFailed {
                token: RemoveTokenKind::Unsynced,
                ..
            }
            | PersistCompletion::Refused { .. },
        ) => true,
        _ => false,
    };
    if permitted { Ok(()) } else { Err(unexpected()) }
}

fn authority_fault(reason: Refusal) -> ProtocolFault {
    match reason {
        Refusal::WrongPhase(phase) => ProtocolFault::AuthorityWrongPhase(phase),
        Refusal::CorruptedPhase => ProtocolFault::CorruptedPhase,
        Refusal::CrossDomain { .. } => ProtocolFault::CrossDomain,
        other => ProtocolFault::AuthorityRefused(other),
    }
}

fn begin_repair(
    owner: ClearOwner,
    context: &mut TxnContext,
    authority: &ControlAuthority,
    intent: DispatchIntent,
    events: &mut Vec<ClearEvent>,
) -> HeldPermit {
    match authority.begin_repair(owner) {
        Ok(permit) => {
            context.stage = ClearStage::Repairing;
            context.plan(
                PersistCommand::RepairClear {
                    txn: context.txn,
                    target: permit.target(),
                },
                intent,
            );
            HeldPermit::Repair(permit)
        }
        Err(refused) => {
            context.stuck(
                StuckReason::ProtocolBug(authority_fault(refused.reason)),
                events,
            );
            HeldPermit::Owner(refused.token)
        }
    }
}

fn retry_later(
    context: &mut TxnContext,
    command: PersistCommand,
    inputs: AdvanceInputs,
    policy: RetryPolicy,
) {
    context.stage = ClearStage::AwaitRetry {
        command,
        due_at_ns: policy.due_at(inputs.now_ns, context.retry_accepted, inputs.draining),
    };
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::single_match_else,
    reason = "单一按值消费表完整保留许可返还，枚举阶段按仓库约定使用 match"
)]
fn consume_completion(
    held: HeldPermit,
    context: &mut TxnContext,
    command: PersistCommand,
    completion: PersistCompletion,
    authority: &ControlAuthority,
    inputs: AdvanceInputs,
    clock: &dyn RobotClock,
    policy: RetryPolicy,
    events: &mut Vec<ClearEvent>,
) -> Result<HeldPermit, RetirementRecord> {
    let held = match (held, completion) {
        (HeldPermit::Pending(owner), PersistCompletion::Prepared { .. }) => match authority
            .commit_clear(owner)
        {
            Ok(finalizing) => {
                let committed_at = clock.now().0;
                context.logical_committed_at = Some(committed_at);
                context.stage = ClearStage::Finalizing;
                events.push(ClearEvent::CommitLogically { at: committed_at });
                match authority.authorize_finalize(finalizing) {
                    Ok(cleanup) => {
                        context.lock(ClearOutcome::Cleared {
                            cleared_at_ns: committed_at,
                            superseded: false,
                        });
                        context.stage = ClearStage::FinalizeCleanup;
                        context.plan(
                            PersistCommand::RemoveMarker {
                                txn: context.txn,
                                kind: CleanupKind::Finalize,
                            },
                            DispatchIntent::NextStage,
                        );
                        HeldPermit::Cleanup(cleanup)
                    }
                    Err(refused) => match refused.reason {
                        Refusal::Latched => {
                            context.lock(ClearOutcome::Cleared {
                                cleared_at_ns: committed_at,
                                superseded: true,
                            });
                            begin_repair(
                                ClearOwner::Finalizing(refused.token),
                                context,
                                authority,
                                DispatchIntent::NextStage,
                                events,
                            )
                        }
                        reason => {
                            context
                                .stuck(StuckReason::ProtocolBug(authority_fault(reason)), events);
                            HeldPermit::Finalizing(refused.token)
                        }
                    },
                }
            }
            Err(refused) => {
                let superseded = match refused.reason {
                    Refusal::Shutdown => SupersedeReason::Shutdown,
                    other => SupersedeReason::Authority(other),
                };
                context.superseded = Some(superseded);
                context.lock(if authority.snapshot().latch && inputs.estop_seen_since {
                    ClearOutcome::PreemptedByEmergency
                } else {
                    ClearOutcome::Superseded(superseded)
                });
                begin_repair(
                    ClearOwner::Pending(refused.token),
                    context,
                    authority,
                    DispatchIntent::NextStage,
                    events,
                )
            }
        },
        (HeldPermit::Pending(owner), PersistCompletion::PrepareFailed { stage, .. }) => {
            context.lock(ClearOutcome::FsyncFailed { stage });
            begin_repair(
                ClearOwner::Pending(owner),
                context,
                authority,
                DispatchIntent::NextStage,
                events,
            )
        }
        (
            HeldPermit::Pending(owner),
            PersistCompletion::Refused {
                reason:
                    reason @ (WriterRefusal::EStopPersistOutstanding | WriterRefusal::ShuttingDown),
                ..
            },
        ) => {
            context.lock(match reason {
                WriterRefusal::EStopPersistOutstanding => ClearOutcome::PreemptedByEmergency,
                WriterRefusal::ShuttingDown => ClearOutcome::Superseded(SupersedeReason::Shutdown),
                _ => unreachable!(),
            });
            match authority.retire_unprepared_clear(owner) {
                Ok(retirement) => return Err(RetirementRecord::Unprepared(retirement)),
                Err(refused) => {
                    context.stuck(StuckReason::RetireRefused(refused.reason), events);
                    HeldPermit::Pending(refused.token)
                }
            }
        }
        (HeldPermit::Repair(permit), PersistCompletion::Repaired { .. }) => {
            context.continuation = None;
            match authority.authorize_repair_cleanup(permit) {
                Ok(cleanup) => {
                    context.stage = ClearStage::RepairClean;
                    context.plan(
                        PersistCommand::RemoveMarker {
                            txn: context.txn,
                            kind: cleanup.kind(),
                        },
                        DispatchIntent::NextStage,
                    );
                    HeldPermit::Cleanup(cleanup)
                }
                Err(refused) => match refused.reason {
                    Refusal::StaleEpoch { .. } | Refusal::RepairTargetChanged { .. } => {
                        begin_repair(
                            refused.token,
                            context,
                            authority,
                            DispatchIntent::NextStage,
                            events,
                        )
                    }
                    reason => {
                        context.stuck(StuckReason::ProtocolBug(authority_fault(reason)), events);
                        HeldPermit::Owner(refused.token)
                    }
                },
            }
        }
        (
            HeldPermit::Repair(permit),
            PersistCompletion::RepairFailed {
                stage,
                credential_kept,
                ..
            },
        ) => {
            if credential_kept {
                context.continuation = match stage {
                    ClearTxnStage::SyncRemovalDirectory => Some(UnsyncedBeforeRepair {
                        target: permit.target(),
                    }),
                    _ => None,
                };
                context.retry_action = match context.continuation {
                    Some(_) => RetryAction::Keep,
                    None => RetryAction::RepairIfLatched,
                };
                retry_later(context, command, inputs, policy);
            } else {
                context.stuck(StuckReason::MarkerLost, events);
            }
            HeldPermit::Repair(permit)
        }
        (
            HeldPermit::Repair(permit),
            PersistCompletion::Refused {
                reason: WriterRefusal::PairMismatch,
                ..
            },
        ) => {
            context.continuation = None;
            context.retry_action = RetryAction::PairMismatch;
            retry_later(context, command, inputs, policy);
            HeldPermit::Repair(permit)
        }
        (
            HeldPermit::Repair(permit),
            PersistCompletion::Refused {
                reason:
                    reason @ (WriterRefusal::EStopPersistOutstanding | WriterRefusal::ShuttingDown),
                ..
            },
        ) => {
            context.retry_action = match reason {
                WriterRefusal::EStopPersistOutstanding => RetryAction::RepairAfterOutstanding,
                _ => RetryAction::Keep,
            };
            retry_later(context, command, inputs, policy);
            HeldPermit::Repair(permit)
        }
        (HeldPermit::Cleanup(permit), PersistCompletion::MarkerRemoved { at, .. }) => {
            context.physical_done_at.get_or_insert(at.0);
            return retire_cleanup(permit, context, authority, events);
        }
        (HeldPermit::Cleanup(permit), PersistCompletion::Resynced { .. }) => {
            // 必须位于完整匹配之后；不是 tick 预采时间，也不是精确文件完成戳。
            context
                .physical_done_at
                .get_or_insert_with(|| clock.now().0);
            return retire_cleanup(permit, context, authority, events);
        }
        (HeldPermit::Cleanup(permit), PersistCompletion::MarkerRemoveFailed { token, .. }) => {
            match token {
                RemoveTokenKind::Retained => {
                    context.retry_action = RetryAction::CleanupIfLatched;
                    retry_later(context, command, inputs, policy);
                }
                RemoveTokenKind::Unsynced => match command {
                    PersistCommand::ResyncRemoval { .. } => {
                        context.retry_action = RetryAction::Keep;
                        retry_later(context, command, inputs, policy);
                    }
                    _ => {
                        context.stage = ClearStage::Resyncing;
                        context.plan(
                            PersistCommand::ResyncRemoval { txn: context.txn },
                            DispatchIntent::NextStage,
                        );
                    }
                },
                RemoveTokenKind::Lost => context.stuck(StuckReason::MarkerLost, events),
            }
            HeldPermit::Cleanup(permit)
        }
        (
            HeldPermit::Cleanup(permit),
            PersistCompletion::Refused {
                reason: WriterRefusal::ShuttingDown,
                ..
            },
        ) => {
            context.retry_action = RetryAction::Keep;
            retry_later(context, command, inputs, policy);
            HeldPermit::Cleanup(permit)
        }
        (held, PersistCompletion::Refused { reason, .. }) => {
            let fault = match reason {
                WriterRefusal::IdentityMismatch => ProtocolFault::IdentityMismatch,
                WriterRefusal::TokenStateMismatch => ProtocolFault::TokenStateMismatch,
                WriterRefusal::NoTransaction => ProtocolFault::NoTransaction,
                WriterRefusal::EStopPersistOutstanding
                | WriterRefusal::ShuttingDown
                | WriterRefusal::PairMismatch => ProtocolFault::UnexpectedCompletion {
                    stage: context.stage.kind(),
                    completion,
                },
            };
            context.stuck(StuckReason::ProtocolBug(fault), events);
            held
        }
        (held, _) => {
            context.stuck(
                StuckReason::ProtocolBug(ProtocolFault::UnexpectedCompletion {
                    stage: context.stage.kind(),
                    completion,
                }),
                events,
            );
            held
        }
    };
    Ok(held)
}

fn retire_cleanup(
    permit: CleanupPermit,
    context: &mut TxnContext,
    authority: &ControlAuthority,
    events: &mut Vec<ClearEvent>,
) -> Result<HeldPermit, RetirementRecord> {
    match authority.retire_clear_cleanup(permit) {
        Ok(retirement) => Err(RetirementRecord::Cleanup(retirement)),
        Err(refused) => {
            context.stuck(StuckReason::RetireRefused(refused.reason), events);
            Ok(HeldPermit::Cleanup(refused.token))
        }
    }
}

fn emit_retirement(
    mut context: TxnContext,
    retirement: RetirementRecord,
    events: &mut Vec<ClearEvent>,
) {
    events.push(ClearEvent::Retired(RetiredView {
        txn: context.txn,
        retirement,
        logical_committed_at: context.logical_committed_at,
        physical_done_at: context.physical_done_at,
    }));
    let reply = std::mem::replace(&mut context.reply, ReplyState::Replied);
    if let ReplyState::Pending { rr, locked } = reply {
        let outcome = match retirement {
            RetirementRecord::Cleanup(retired) if retired.logical_committed => {
                ClearOutcome::Cleared {
                    cleared_at_ns: context
                        .logical_committed_at
                        .expect("已提交 Clear 应有逻辑提交事实"),
                    superseded: retired.superseded
                        || matches!(retired.kind, CleanupKind::Repaired(_)),
                }
            }
            RetirementRecord::Cleanup(_) | RetirementRecord::Unprepared(_) => {
                locked.expect("退休前应锁存业务结果")
            }
        };
        events.push(ClearEvent::Reply {
            rr,
            outcome,
            retired: true,
        });
    }
}

fn ready_dispatch(
    mut state: ClearTxnState,
    authority: &ControlAuthority,
    client: &mut PersistClient,
    inputs: AdvanceInputs,
    policy: RetryPolicy,
    events: &mut Vec<ClearEvent>,
) -> ClearTxnState {
    if state.context.in_flight.is_some() {
        return state;
    }
    let pending = match state.context.next.take() {
        Some(next) => Some(next),
        None => match state.context.stage {
            ClearStage::AwaitCapacity { command, intent } if client.has_credit() => {
                Some(PendingDispatch { command, intent })
            }
            ClearStage::AwaitRetry { command, due_at_ns }
                if inputs.now_ns >= due_at_ns && client.has_credit() =>
            {
                // 在改目标前也核同一共享预算，耗尽不得凭新目标重新获得第九次。
                if state.context.retry_accepted >= policy.max_accepted {
                    state.context.stuck(StuckReason::RepairExhausted, events);
                    return state;
                }
                state = retarget_retry(state, authority, command, events);
                if matches!(state.context.stage, ClearStage::Stuck(_)) {
                    return state;
                }
                Some(state.context.next.take().unwrap_or(PendingDispatch {
                    command,
                    intent: DispatchIntent::Retry,
                }))
            }
            _ => None,
        },
    };
    if let Some(next) = pending {
        let _ = dispatch(
            &mut state.context,
            client,
            next.command,
            next.intent,
            inputs.now_ns,
            policy,
            events,
        );
    }
    state
}

fn retarget_retry(
    state: ClearTxnState,
    authority: &ControlAuthority,
    command: PersistCommand,
    events: &mut Vec<ClearEvent>,
) -> ClearTxnState {
    let ClearTxnState { held, mut context } = state;
    let latched = authority.snapshot().latch;
    if context.retry_action == RetryAction::PairMismatch && !latched {
        context.stuck(StuckReason::PairMismatch, events);
        return ClearTxnState { held, context };
    }
    let held = match held {
        HeldPermit::Repair(permit)
            if context.continuation.is_none()
                && match context.retry_action {
                    RetryAction::RepairIfLatched | RetryAction::PairMismatch => {
                        latched && permit.target() == RepairTarget::OriginalUnlatched
                    }
                    RetryAction::RepairAfterOutstanding => true,
                    RetryAction::Keep | RetryAction::CleanupIfLatched => false,
                } =>
        {
            match authority.abandon_repair(permit) {
                Ok(owner) => begin_repair(
                    owner,
                    &mut context,
                    authority,
                    DispatchIntent::Retry,
                    events,
                ),
                Err(refused) => {
                    context.stuck(
                        StuckReason::ProtocolBug(authority_fault(refused.reason)),
                        events,
                    );
                    HeldPermit::Repair(refused.token)
                }
            }
        }
        HeldPermit::Cleanup(permit)
            if context.retry_action == RetryAction::CleanupIfLatched
                && latched
                && matches!(command, PersistCommand::RemoveMarker { .. })
                && matches!(
                    permit.kind(),
                    CleanupKind::Finalize | CleanupKind::Repaired(RepairTarget::OriginalUnlatched)
                ) =>
        {
            match authority.abandon_cleanup(permit) {
                Ok(owner) => begin_repair(
                    owner,
                    &mut context,
                    authority,
                    DispatchIntent::Retry,
                    events,
                ),
                Err(refused) => {
                    context.stuck(
                        StuckReason::ProtocolBug(authority_fault(refused.reason)),
                        events,
                    );
                    HeldPermit::Cleanup(refused.token)
                }
            }
        }
        held => held,
    };
    ClearTxnState { held, context }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::latch_writer::{PersistShared, mailbox};
    use robot_core::control_authority::{AuthorityInit, InhibitSet};
    use std::sync::Arc;

    // 同一公开验收源在 lib 过滤门与独立集成二进制各自真实执行，不复制状态机。
    mod public_acceptance {
        use crate as robot_s0;
        include!("../tests/clear_coordination_acceptance.rs");

        use super::super::{ProtocolFault, StageKind};
        use crate::latch_writer::ClearTxnIdentity;
        use crate::latch_writer::{PersistCompletion, WriterRefusal};
        use robot_core::control_authority::CleanupKind;

        #[test]
        fn repeated_full_preserves_retry_then_real_dispatch_claim_and_cleanup_complete() {
            let mut rig = Rig::new(false);
            let mut coordinator = coordinator();
            rig.fail(&[ClearTxnStage::RemoveMarker]);
            queued(rig.start(&mut coordinator));
            rig.spawn();
            rig.delivered(1);
            let _ = rig.tick(&mut coordinator);
            rig.delivered(2);
            assert!(rig.tick(&mut coordinator).is_empty());
            let ClearStage::AwaitRetry { command, .. } = coordinator.view().unwrap().stage() else {
                panic!("真实删除失败进入重试等待");
            };
            let word = rig.authority.snapshot();
            let mut events = Vec::new();
            // Full 对单信用公开 API 不自然可达；只注入真实派发结果的同一处理入口。
            for _ in 0..2 {
                let context = &mut coordinator.txn.as_mut().unwrap().context;
                assert_eq!(
                    super::super::accept_dispatch_result(
                        context,
                        command,
                        DispatchIntent::Retry,
                        Err(crate::latch_writer::DispatchRefused::Full),
                        &mut events
                    ),
                    DispatchState::AwaitCapacity
                );
                assert_eq!(
                    context.stage,
                    ClearStage::AwaitCapacity {
                        command,
                        intent: DispatchIntent::Retry
                    }
                );
                assert_eq!(context.retry_accepted, 0);
                assert!(context.in_flight.is_none());
            }
            assert!(events.is_empty());
            assert_eq!(rig.authority.snapshot(), word);
            assert!(rig.client.has_credit());
            assert!(rig.tick(&mut coordinator).is_empty());
            assert_eq!(
                coordinator.view().unwrap().in_flight().unwrap().intent,
                DispatchIntent::Retry
            );
            rig.delivered(3);
            let _ = retirement(&rig.tick(&mut coordinator));
            assert_eq!(rig.pair(), (b"0\n".to_vec(), b"0\n".to_vec()));
        }

        fn arm_protocol_barrier(rig: &Rig, stage: ClearTxnStage, claimed: bool) {
            if claimed {
                rig.parked(stage);
            } else {
                rig.clock.pause_before_claim.store(true, Ordering::Release);
            }
        }

        fn wait_protocol_barrier(rig: &Rig, claimed: bool) {
            wait_until(
                || {
                    if claimed {
                        rig.hook.reached.load(Ordering::Acquire)
                    } else {
                        rig.clock.claim_paused.load(Ordering::Acquire)
                    }
                },
                "真实领取边界",
            );
            assert_eq!(rig.shared.announce.offered_at().is_none(), claimed);
        }

        fn protocol_stage(stage: StageKind, claimed: bool) -> (Rig, ClearCoordination) {
            let mut rig = Rig::new(false);
            let mut coordinator = coordinator();
            match stage {
                StageKind::Repairing => rig.fail(&[ClearTxnStage::CreateMarker]),
                StageKind::RepairClean => rig.fail(&[ClearTxnStage::CreateBackup]),
                StageKind::Resyncing => rig.fail(&[ClearTxnStage::SyncRemovalDirectory]),
                _ => {}
            }
            queued(rig.start(&mut coordinator));
            if stage == StageKind::Preparing {
                arm_protocol_barrier(&rig, ClearTxnStage::CreateMarker, claimed);
            }
            rig.spawn();
            if stage != StageKind::Preparing {
                rig.delivered(1);
                match stage {
                    StageKind::Repairing => {
                        arm_protocol_barrier(&rig, ClearTxnStage::InspectPair, claimed);
                    }
                    StageKind::FinalizeCleanup => {
                        arm_protocol_barrier(&rig, ClearTxnStage::RemoveMarker, claimed);
                    }
                    _ => {}
                }
                let _ = rig.tick(&mut coordinator);
                if matches!(stage, StageKind::RepairClean | StageKind::Resyncing) {
                    rig.delivered(2);
                    arm_protocol_barrier(
                        &rig,
                        if stage == StageKind::Resyncing {
                            ClearTxnStage::SyncRemovalDirectory
                        } else {
                            ClearTxnStage::RemoveMarker
                        },
                        claimed,
                    );
                    let _ = rig.tick(&mut coordinator);
                }
            }
            wait_protocol_barrier(&rig, claimed);
            assert_eq!(coordinator.view().unwrap().stage().kind(), stage);
            (rig, coordinator)
        }

        fn reject_protocol(
            rig: &mut Rig,
            coordinator: &mut ClearCoordination,
            completion: PersistCompletion,
            claimed: bool,
            retry: bool,
        ) {
            let before = coordinator.view().unwrap();
            let original_flight = before.in_flight();
            let original_retry = before.flight();
            let logical = before.logical_committed_at();
            let physical = before.physical_done_at();
            let word = rig.authority.snapshot();
            let rr_pointer = match before.reply() {
                super::super::ReplyState::Pending { rr, .. } => rr.request_id.as_ptr(),
                _ => panic!("必须持有原请求"),
            };
            let readings = rig.clock.coordinator_reads.load(Ordering::Acquire);
            // 只替换同一完整匹配入口的畸形输入，不宣称它由真实 mailbox 送达。
            let events = coordinator.advance_impl(
                &rig.authority,
                &mut rig.client,
                AdvanceInputs {
                    now_ns: BASE + 1,
                    draining: false,
                    estop_seen_since: false,
                },
                rig.clock.as_ref(),
                Some(completion),
            );
            assert!(matches!(
                events.as_slice(),
                [ClearEvent::StuckEntered(StuckReason::ProtocolBug(_))]
            ));
            let after = coordinator.view().unwrap();
            assert_eq!(after.in_flight(), original_flight);
            assert_eq!(after.retry_accepted(), u8::from(retry && claimed));
            assert_eq!(
                after
                    .flight()
                    .map(|f| (f.txn, f.command, f.stage, f.expected)),
                original_retry.map(|f| (f.txn, f.command, f.stage, f.expected))
            );
            assert_eq!(after.logical_committed_at(), logical);
            assert_eq!(after.physical_done_at(), physical);
            assert_eq!(rig.authority.snapshot(), word);
            assert_eq!(
                rig.clock.coordinator_reads.load(Ordering::Acquire),
                readings
            );
            assert!(!rig.client.has_credit());
            assert_eq!(rig.shared.announce.offered_at().is_none(), claimed);
            assert!(
                matches!(after.reply(), super::super::ReplyState::Pending { rr, .. }
                if rr.request_id.as_ptr() == rr_pointer)
            );
            assert!(rig.tick(coordinator).is_empty());
            assert_eq!(
                coordinator.view().unwrap().retry_accepted(),
                u8::from(retry && claimed)
            );
        }

        #[test]
        fn malformed_completion_matrix_preserves_real_unclaimed_and_claimed_responsibility() {
            for claimed in [false, true] {
                for (stage, variant) in [
                    (StageKind::Preparing, 0),
                    (StageKind::Preparing, 1),
                    (StageKind::Preparing, 2),
                    (StageKind::Preparing, 3),
                    (StageKind::Preparing, 4),
                    (StageKind::Preparing, 5),
                    (StageKind::Repairing, 6),
                    (StageKind::FinalizeCleanup, 7),
                    (StageKind::FinalizeCleanup, 8),
                    (StageKind::FinalizeCleanup, 9),
                    (StageKind::RepairClean, 8),
                    (StageKind::Resyncing, 8),
                    (StageKind::Resyncing, 10),
                ] {
                    let (mut rig, mut coordinator) = protocol_stage(stage, claimed);
                    let txn = coordinator.view().unwrap().txn();
                    let completion = match variant {
                        0 => PersistCompletion::Refused {
                            txn: Some(txn),
                            reason: WriterRefusal::PairMismatch,
                        },
                        1 => PersistCompletion::Prepared {
                            txn: ClearTxnIdentity {
                                txn_id: txn.txn_id + 1,
                                ..txn
                            },
                            at: MonoRawNs(9),
                        },
                        2 => PersistCompletion::Refused {
                            txn: None,
                            reason: WriterRefusal::ShuttingDown,
                        },
                        3 => PersistCompletion::Refused {
                            txn: Some(txn),
                            reason: WriterRefusal::NoTransaction,
                        },
                        4 => PersistCompletion::Refused {
                            txn: Some(txn),
                            reason: WriterRefusal::TokenStateMismatch,
                        },
                        5 => PersistCompletion::Refused {
                            txn: Some(txn),
                            reason: WriterRefusal::IdentityMismatch,
                        },
                        6 => PersistCompletion::Repaired {
                            txn,
                            target: RepairTarget::Latched,
                            at: MonoRawNs(9),
                        },
                        7 => PersistCompletion::MarkerRemoved {
                            txn,
                            kind: CleanupKind::Repaired(RepairTarget::Latched),
                            at: MonoRawNs(9),
                        },
                        8 => PersistCompletion::Refused {
                            txn: Some(txn),
                            reason: WriterRefusal::EStopPersistOutstanding,
                        },
                        9 => PersistCompletion::Resynced { txn },
                        10 => PersistCompletion::MarkerRemoved {
                            txn,
                            kind: CleanupKind::Finalize,
                            at: MonoRawNs(9),
                        },
                        _ => unreachable!(),
                    };
                    reject_protocol(&mut rig, &mut coordinator, completion, claimed, false);
                    match variant {
                        1 | 2 | 5 => assert_eq!(
                            coordinator.stuck(),
                            Some(StuckReason::ProtocolBug(ProtocolFault::IdentityMismatch))
                        ),
                        6 => assert_eq!(
                            coordinator.stuck(),
                            Some(StuckReason::ProtocolBug(ProtocolFault::TargetMismatch))
                        ),
                        7 => assert_eq!(
                            coordinator.stuck(),
                            Some(StuckReason::ProtocolBug(ProtocolFault::KindMismatch))
                        ),
                        _ => {}
                    }
                }
            }
        }

        #[test]
        fn illegal_refused_does_not_invent_retry_claim_and_keeps_real_claim_exactly_once() {
            for claimed in [false, true] {
                let mut rig = Rig::new(false);
                let mut coordinator = coordinator();
                rig.fail(&[ClearTxnStage::RemoveMarker]);
                queued(rig.start(&mut coordinator));
                rig.spawn();
                rig.delivered(1);
                let _ = rig.tick(&mut coordinator);
                rig.delivered(2);
                let _ = rig.tick(&mut coordinator);
                rig.due(&coordinator);
                arm_protocol_barrier(&rig, ClearTxnStage::RemoveMarker, claimed);
                assert!(rig.tick(&mut coordinator).is_empty());
                wait_protocol_barrier(&rig, claimed);
                let txn = coordinator.view().unwrap().txn();
                assert_eq!(
                    coordinator.view().unwrap().in_flight().unwrap().intent,
                    DispatchIntent::Retry
                );
                reject_protocol(
                    &mut rig,
                    &mut coordinator,
                    PersistCompletion::Refused {
                        txn: Some(txn),
                        reason: WriterRefusal::EStopPersistOutstanding,
                    },
                    claimed,
                    true,
                );
            }
        }
    }

    fn authority() -> ControlAuthority {
        ControlAuthority::new(AuthorityInit::new(
            false,
            OutputSafetyState::TorqueOffConfirmed,
            InhibitSet::EMPTY,
        ))
        .unwrap()
    }
    fn inputs() -> StartInputs {
        StartInputs {
            state_safe: true,
            bus_idle_no_lower_obligation: true,
            obligation_settled: true,
        }
    }
    fn rr(routes: &mut Vec<crate::test_util::TestRrRoute>) -> RrIdentity {
        routes.push(crate::test_util::TestRrRoute::new());
        RrIdentity {
            route: routes.last().unwrap().identity(),
            request_id: "same-allocation".into(),
            deadline_ns: 100,
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "同一实例逐项验证守门顺序与原身份按值返还，保持完整断言链"
    )]
    fn start_refusals_return_original_owned_identity_in_guard_order() {
        for case in 0..10 {
            let mut routes = Vec::new();
            let authority = authority();
            let shared = Arc::new(PersistShared::new(MonoRawNs(1)).unwrap());
            let (mut client, endpoint) = mailbox(shared);
            let mut coordinator = ClearCoordination::new(RetryPolicy::default());
            let mut enable_owner = None;
            let mut guards = inputs();
            let mut now = 1;
            let expected = match case {
                0 => {
                    now = 101;
                    guards.state_safe = false;
                    ClearOutcome::DeadlineExceeded
                }
                1 => {
                    guards.state_safe = false;
                    ClearOutcome::StateNotSafe
                }
                2 => {
                    guards.bus_idle_no_lower_obligation = false;
                    ClearOutcome::BusyLowering
                }
                3 => {
                    guards.obligation_settled = false;
                    ClearOutcome::PersistPending
                }
                4 => {
                    drop(endpoint);
                    ClearOutcome::WriterUnavailable
                }
                5 => {
                    let _ = authority.shutdown();
                    ClearOutcome::AuthorityRefused(Refusal::Shutdown)
                }
                6 => {
                    let _ = authority.tighten_safety(
                        robot_core::control_authority::SafetyTightening::StopFailed,
                    );
                    ClearOutcome::NotTorqueOffConfirmed {
                        safety: OutputSafetyState::StopFailed,
                    }
                }
                7 => {
                    assert!(matches!(
                        coordinator.start(
                            rr(&mut routes),
                            NonZeroU128::new(1).unwrap(),
                            &inputs(),
                            &authority,
                            &mut client,
                            1
                        ),
                        StartOutcome::Retained { .. }
                    ));
                    ClearOutcome::AlreadyInProgress
                }
                8 => {
                    enable_owner = Some(authority.reserve_enable().unwrap().0);
                    ClearOutcome::AlreadyInProgress
                }
                9 => {
                    client
                        .try_dispatch(
                            PersistCommand::PrepareClear {
                                txn: ClearTxnIdentity {
                                    txn_id: 99,
                                    expected_epoch: 1,
                                    original_latched: false,
                                },
                            },
                            MonoRawNs(1),
                        )
                        .unwrap();
                    ClearOutcome::WriterUnavailable
                }
                _ => unreachable!(),
            };
            let identity = rr(&mut routes);
            let original_pointer = identity.request_id.as_ptr();
            match coordinator.start(
                identity,
                NonZeroU128::new(2).unwrap(),
                &guards,
                &authority,
                &mut client,
                now,
            ) {
                StartOutcome::RefusedBeforeReserve { rr, outcome } => {
                    assert_eq!(rr.request_id.as_ptr(), original_pointer);
                    assert_eq!(rr.request_id, "same-allocation");
                    assert_eq!(rr.deadline_ns, 100);
                    assert_eq!(outcome, expected);
                }
                StartOutcome::Retained { .. } => panic!("准入拒绝不得持有原请求"),
            }
            if let Some(owner) = enable_owner {
                let _ = authority.disable();
                authority.retire_enable(owner).unwrap();
            }
        }
    }

    #[test]
    fn unreachable_dispatch_refusals_use_same_handler_without_losing_owner_or_intent() {
        for intent in [
            DispatchIntent::Initial,
            DispatchIntent::NextStage,
            DispatchIntent::Retry,
        ] {
            for refused in [
                DispatchRefused::NoCredit,
                DispatchRefused::Announce(AnnounceError::AlreadyOffered),
                DispatchRefused::Announce(AnnounceError::Overflow),
                DispatchRefused::Full,
                DispatchRefused::Disconnected,
            ] {
                let mut routes = Vec::new();
                let authority = authority();
                let owner = authority.reserve_clear().unwrap();
                let txn = ClearTxnIdentity {
                    txn_id: 1,
                    expected_epoch: owner.epoch().get(),
                    original_latched: false,
                };
                let command = PersistCommand::PrepareClear { txn };
                let mut context = TxnContext {
                    txn,
                    stage: ClearStage::Preparing,
                    reply: ReplyState::Pending {
                        rr: rr(&mut routes),
                        locked: None,
                    },
                    started_at_ns: 1,
                    logical_committed_at: None,
                    physical_done_at: None,
                    in_flight: None,
                    flight: None,
                    retry_accepted: 0,
                    continuation: None,
                    superseded: None,
                    next: Some(PendingDispatch { command, intent }),
                    retry_action: RetryAction::Keep,
                };
                let before = authority.snapshot();
                let mut events = Vec::new();
                let result = accept_dispatch_result(
                    &mut context,
                    command,
                    intent,
                    Err(refused),
                    &mut events,
                );
                assert_eq!(authority.snapshot(), before);
                assert_eq!(context.retry_accepted, 0);
                assert!(context.in_flight.is_none());
                assert!(context.flight.is_none());
                assert!(matches!(context.reply, ReplyState::Pending { .. }));
                match refused {
                    DispatchRefused::Full => {
                        assert_eq!(result, DispatchState::AwaitCapacity);
                        assert_eq!(context.stage, ClearStage::AwaitCapacity { command, intent });
                        assert!(events.is_empty());
                        assert_eq!(
                            accept_dispatch_result(
                                &mut context,
                                command,
                                intent,
                                Err(DispatchRefused::Full),
                                &mut events
                            ),
                            DispatchState::AwaitCapacity
                        );
                        assert_eq!(context.stage, ClearStage::AwaitCapacity { command, intent });
                        assert_eq!(context.retry_accepted, 0);
                        assert!(events.is_empty());
                    }
                    DispatchRefused::Disconnected => assert_eq!(result, DispatchState::WriterGone),
                    _ => {
                        assert!(matches!(result, DispatchState::Fault(_)));
                        assert_eq!(context.next.unwrap().command, command);
                    }
                }
                // 仅拒绝分类夹具，没有文件工作；真实 core 取消用于收尾测试 owner。
                authority.retire_unprepared_clear(owner).unwrap();
            }
        }
    }
}
