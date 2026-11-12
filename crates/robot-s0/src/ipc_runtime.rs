//! 原 IPC 线程的有界双队列与唯一事务推进；文件操作仅交同一 `PersistClient`。
use crate::boot_supervisor::BootShared;
use crate::clear_coordination::{
    AdvanceInputs, ClearCoordination, ClearEvent, ClearOutcome, RetryPolicy, StartInputs,
    StartOutcome,
};
use crate::event_ingress::{
    ControlEvent, HighQueue, HighWork, LINK_QUOTA, LinkEvent, NormalQueue, NormalWork, RrEvent,
    SOURCE_QUOTA,
};
use crate::hard_rt_ctrl::{CtrlKind, HardRtCtrlClient};
use crate::ipc_pending::{
    self, BridgeStep, BusinessObservation, EnableEffect, EnableStartOutcome, IpcPending,
    LowerCause, LowerDiagnostic, LowerStartOutcome, PendingOutcome, RevokeCause, RrIdentity,
};
use crate::ipc_thread::{
    ClearEStopRrPayload, ClearEStopRrRequest, ClearEStopRrResponse, IpcThreadMetrics,
    RetiredOutbox, RrChannels, SetEnabledRrChannels, SetEnabledRrPayload, SetEnabledRrRequest,
    SetEnabledRrResponse,
};
use crate::latch_writer::PersistClient;
use crate::state_machine::{
    self, EnabledSub, Event, FaultCode, Guards, S0State, S0StateSnapshot, TransitionKind,
};
use crossbeam_channel::{Receiver, TryRecvError};
use robot_core::executor_ctrl::{
    AdapterNormalReq, AdapterResp, ExecutorCtrlClient, NormalReqStatus, OutputPolicy,
    OutputSafetyState, SerialEmergencyCtrl,
};
use robot_core::time::RobotClock;
use robot_msg::msg::SideEffect;
use std::num::{NonZeroU64, NonZeroU128};
use std::sync::atomic::Ordering;

/// 仅复制推进前的 POD 事实；原许可、RR身份与拥有者始终留在协调器。
#[cfg(feature = "test-comm-fault-hook")]
#[derive(Clone, Copy)]
struct ClearAdvanceFact {
    txn: crate::latch_writer::ClearTxnIdentity,
    stage: crate::clear_coordination::ClearStage,
    accepted: u8,
    in_flight: Option<crate::clear_coordination::InFlight>,
    flight: Option<crate::clear_coordination::RetryFlight>,
}
#[cfg(feature = "test-comm-fault-hook")]
impl ClearAdvanceFact {
    fn from_state(state: &crate::clear_coordination::ClearTxnState) -> Self {
        Self {
            txn: state.txn(),
            stage: state.stage(),
            accepted: state.retry_accepted(),
            in_flight: state.in_flight(),
            flight: state.flight(),
        }
    }
}
#[cfg(feature = "test-comm-fault-hook")]
fn clear_stage_fact(stage: Option<crate::clear_coordination::ClearStage>) -> (&'static str, u64) {
    use crate::clear_coordination::ClearStage as S;
    match stage {
        None => ("Absent", 0),
        Some(S::Preparing) => ("Preparing", 0),
        Some(S::Finalizing) => ("Finalizing", 0),
        Some(S::FinalizeCleanup) => ("FinalizeCleanup", 0),
        Some(S::Repairing) => ("Repairing", 0),
        Some(S::RepairClean) => ("RepairClean", 0),
        Some(S::Resyncing) => ("Resyncing", 0),
        Some(S::AwaitRetry { due_at_ns, .. }) => ("AwaitRetry", due_at_ns),
        Some(S::AwaitCapacity { .. }) => ("AwaitCapacity", 0),
        Some(S::Stuck(_)) => ("Stuck", 0),
    }
}

#[cfg(feature = "test-comm-fault-hook")]
fn clear_dispatch_fact(
    flight: Option<crate::clear_coordination::InFlight>,
) -> (&'static str, &'static str) {
    use crate::clear_coordination::DispatchIntent;
    use crate::latch_writer::PersistCommand;
    let Some(flight) = flight else {
        return ("None", "None");
    };
    let command = match flight.command {
        PersistCommand::PrepareClear { .. } => "Prepare",
        PersistCommand::RemoveMarker { .. } => "RemoveMarker",
        PersistCommand::ResyncRemoval { .. } => "Resync",
        PersistCommand::RepairClear { .. } => "Repair",
    };
    let intent = match flight.intent {
        DispatchIntent::Initial => "Initial",
        DispatchIntent::NextStage => "NextStage",
        DispatchIntent::Retry => "Retry",
    };
    (command, intent)
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct TopicEvents {
    pub heartbeat: bool,
    pub estop: bool,
}
pub(crate) struct IpcSources<'a> {
    pub watchdog: &'a Receiver<Event>,
    pub external: &'a Receiver<ControlEvent>,
    pub clear_rr: Option<&'a RrChannels>,
    pub set_rr: Option<&'a SetEnabledRrChannels>,
}
pub(crate) struct IpcRuntimeContext<'a> {
    pub ctrl: &'a HardRtCtrlClient,
    pub boot: &'a BootShared,
    pub clock: &'a dyn RobotClock,
    pub metrics: &'a IpcThreadMetrics,
}
/// 只限制非 RT 诊断输出；不是请求责任或退休状态。
#[derive(Clone, Copy, PartialEq, Eq)]
struct LowerLogKey {
    started_at: u64,
    diagnostic: LowerDiagnostic,
    waiter_count: usize,
    earliest_deadline: u64,
}
#[derive(Clone, Copy)]
struct LowerLogState {
    key: LowerLogKey,
    logged_at: u64,
    changes_logged: u8,
    near_deadline_logged: bool,
}

const LOWER_LOG_MIN_INTERVAL_NS: u64 = 100_000_000;
const LOWER_LOG_DEADLINE_WINDOW_NS: u64 = 250_000_000;
const LOWER_LOG_MAX_CHANGES: u8 = 16;

fn lower_log_decision(
    previous: Option<LowerLogState>,
    key: LowerLogKey,
    now: u64,
) -> (bool, bool, u8) {
    let near_deadline = key.waiter_count != 0
        && key.earliest_deadline.saturating_sub(now) <= LOWER_LOG_DEADLINE_WINDOW_NS;
    let Some(previous) = previous.filter(|previous| previous.key.started_at == key.started_at)
    else {
        return (true, near_deadline, 1);
    };
    let log_change = previous.key != key
        && previous.changes_logged < LOWER_LOG_MAX_CHANGES
        && now.saturating_sub(previous.logged_at) >= LOWER_LOG_MIN_INTERVAL_NS;
    let log_deadline = near_deadline && !previous.near_deadline_logged;
    (
        log_change || log_deadline,
        previous.near_deadline_logged || near_deadline,
        previous.changes_logged.saturating_add(u8::from(log_change)),
    )
}

pub(crate) struct IpcLoopState {
    pub(crate) state: S0State,
    pub(crate) guards: Guards,
    pending: IpcPending,
    high: HighQueue,
    normal: NormalQueue,
    links: crate::event_ingress::LinkQueue,
    coordination: ClearCoordination,
    health: robot_health::RuntimeHealthReporter,
    overdue: crate::persist_health::OverdueTracker,
    next_txn: Option<NonZeroU128>,
    estop_during_clear: bool,
    shutdown_lower_started: bool,
    lowered_generation: Option<NonZeroU64>,
    deferred_control: Option<ControlEvent>,
    deferred_restore: Option<(HighWork, bool)>,
    stranded_clear_reply: Option<(RrIdentity, ClearOutcome)>,
    control_invalidated: bool,
    lower_log: Option<LowerLogState>,
    #[cfg(feature = "test-comm-fault-hook")]
    clear_fact_sequence: u64,
    #[cfg(feature = "test-comm-fault-hook")]
    clear_shutdown_observed: u64,
}
impl IpcLoopState {
    pub(crate) fn new(state: S0State, guards: Guards, normal_capacity: usize) -> Self {
        Self {
            state,
            guards,
            pending: IpcPending::Idle,
            high: HighQueue::new(),
            normal: NormalQueue::with_capacity(normal_capacity),
            links: crate::event_ingress::LinkQueue::new(),
            coordination: ClearCoordination::new(RetryPolicy::default()),
            health: robot_health::RuntimeHealthReporter::new(),
            overdue: crate::persist_health::OverdueTracker::new(),
            next_txn: Some(NonZeroU128::MIN),
            estop_during_clear: false,
            shutdown_lower_started: false,
            lowered_generation: None,
            deferred_control: None,
            deferred_restore: None,
            stranded_clear_reply: None,
            control_invalidated: false,
            lower_log: None,
            #[cfg(feature = "test-comm-fault-hook")]
            clear_fact_sequence: 0,
            #[cfg(feature = "test-comm-fault-hook")]
            clear_shutdown_observed: 0,
        }
    }
    pub(crate) fn is_quiescent(&self) -> bool {
        matches!(self.pending, IpcPending::Idle)
            && self.high.is_empty()
            && self.normal.is_empty()
            && self.links.is_empty()
            && self.deferred_control.is_none()
            && self.deferred_restore.is_none()
            && self.stranded_clear_reply.is_none()
            && self.coordination.is_retired_or_absent()
    }
    fn take_pending(&mut self) -> IpcPending {
        std::mem::replace(&mut self.pending, IpcPending::Idle)
    }
    fn observation(&self, context: &IpcRuntimeContext<'_>) -> BusinessObservation {
        BusinessObservation {
            now: context.clock.now(),
            state: self.state,
            estop: self.guards.estop_latched,
        }
    }
    fn observe_lower_pending(&mut self, context: &IpcRuntimeContext<'_>) {
        let IpcPending::Lower(lower) = &self.pending else {
            self.lower_log = None;
            return;
        };
        if lower.waiters().is_empty() {
            self.lower_log = None;
            return;
        }
        let now = context.clock.now().0;
        let diagnostic = lower.diagnostic(context.ctrl.authority().snapshot());
        let waiter_count = lower.waiters().len();
        let earliest_deadline = lower
            .waiters()
            .iter()
            .map(|waiter| waiter.identity().deadline_ns)
            .min()
            .unwrap_or(u64::MAX);
        let key = LowerLogKey {
            started_at: lower.started_at().0,
            diagnostic,
            waiter_count,
            earliest_deadline,
        };
        let (should_log, near_deadline_logged, changes_logged) =
            lower_log_decision(self.lower_log, key, now);
        if should_log {
            for waiter in lower.waiters().iter() {
                let rr = waiter.identity();
                tracing::info!(
                    request_id = %rr.request_id,
                    route = ?rr.route,
                    cause = ?lower.cause(),
                    source_seq = ?diagnostic.source_seq,
                    lower_seq = ?diagnostic.lower_seq,
                    deadline_ns = rr.deadline_ns,
                    age_ns = now.saturating_sub(lower.started_at().0),
                    wait_mask = diagnostic.wait_mask,
                    phase = ?diagnostic.phase,
                    safety = ?diagnostic.safety,
                    inhibits = diagnostic.inhibits,
                    "Lower 待决快照（非退休证据）"
                );
            }
        }
        let logged_at = if should_log {
            now
        } else {
            self.lower_log.map_or(now, |previous| previous.logged_at)
        };
        self.lower_log = Some(LowerLogState {
            key: if should_log {
                key
            } else {
                self.lower_log.map_or(key, |previous| previous.key)
            },
            logged_at,
            changes_logged,
            near_deadline_logged,
        });
    }
    fn reply_clear(
        outbox: &mut RetiredOutbox,
        rr: RrIdentity,
        payload: ClearEStopRrPayload,
        effect: SideEffect,
        superseded: bool,
    ) {
        outbox.clear.push_back(ClearEStopRrResponse {
            route: rr.route,
            request_id: rr.request_id,
            payload,
            effect,
            superseded,
        });
    }
    fn reply_set(
        outbox: &mut RetiredOutbox,
        rr: RrIdentity,
        payload: SetEnabledRrPayload,
        effect: SideEffect,
    ) {
        outbox.set.push_back(SetEnabledRrResponse {
            route: rr.route,
            request_id: rr.request_id,
            payload,
            effect,
        });
    }
    fn accept_step(
        &mut self,
        step: BridgeStep,
        context: &IpcRuntimeContext<'_>,
        outbox: &mut RetiredOutbox,
    ) {
        self.pending = step.pending;
        if let Some(failure) = step.failure {
            let snapshot = context.ctrl.authority().snapshot();
            if !matches!(self.state, S0State::Fault(_) | S0State::Stopping)
                && !snapshot.latch
                && !snapshot.shutdown
            {
                // 只消费真实提交/执行失败事实。原执行器已负责物理补偿，不能重排 Stop。
                state_machine::commit_fault(&mut self.state, FaultCode::AdapterErr);
                context.ctrl.set_policy(OutputPolicy::Disabled);
                self.revoke(RevokeCause::Fault, context);
            }
            match failure.source() {
                ipc_pending::EnableFailureSource::SubmitUnsupported(reason) => {
                    tracing::error!(reason, "Enable 原提交被执行器拒绝");
                }
                ipc_pending::EnableFailureSource::ExecutionFailed(error) => {
                    tracing::error!(?error, "Enable 原执行返回错误");
                }
            }
        }
        if let Some(g) = step.lowered_generation {
            self.lowered_generation = Some(g);
        }
        if let Some(commit) = step.commit {
            // 权限已经由真实 commit_enable 提交；这里只提交对应业务状态/输出选择。
            self.guards.hw_enable_ok = true;
            if self.state == S0State::Safe {
                self.commit_pure(Event::Enable, context);
            }
            if let Some(plan) =
                state_machine::plan_transition(self.state, Event::Heartbeat, &self.guards)
            {
                state_machine::commit_transition(&mut self.state, &plan);
                context.ctrl.set_policy(plan.new_policy);
            }
            tracing::debug!(
                at_ns = commit.at.0,
                epoch = commit.epoch.get(),
                "Enable 原事务已提交"
            );
        }
        for reply in step.replies.into_entries() {
            let effect = match reply.effect {
                EnableEffect::None => SideEffect::None,
                EnableEffect::Started => SideEffect::Started,
                EnableEffect::Committed => SideEffect::Committed,
            };
            let payload = match reply.outcome {
                PendingOutcome::Existing(payload) => payload,
                PendingOutcome::SupersededByDisable => SetEnabledRrPayload::SupersededByDisable,
            };
            if let SetEnabledRrPayload::Disabled { completed_at_ns } = payload {
                tracing::info!(
                    request_id = %reply.identity.request_id,
                    route = ?reply.identity.route,
                    completed_at_ns,
                    "Lower 原物理门退休，准备发送 RR 回复"
                );
            }
            if matches!(payload, SetEnabledRrPayload::Disabled { .. })
                && !matches!(self.state, S0State::Fault(_) | S0State::Stopping)
            {
                self.state = S0State::Safe;
                context.ctrl.set_policy(OutputPolicy::Disabled);
            }
            Self::reply_set(outbox, reply.identity, payload, effect);
        }
    }
    fn apply_lower(
        &mut self,
        rr: Option<RrIdentity>,
        cause: LowerCause,
        context: &IpcRuntimeContext<'_>,
        outbox: &mut RetiredOutbox,
    ) -> bool {
        let fresh_safe_disable = matches!(self.pending, IpcPending::Idle)
            && self.state == S0State::Safe
            && cause == LowerCause::OperatorDisable
            && rr.is_some();
        let pending = self.take_pending();
        match ipc_pending::begin_lower_owned(
            pending,
            rr,
            cause,
            context.ctrl,
            context.boot,
            self.observation(context),
        ) {
            LowerStartOutcome::Advanced(step) => {
                self.accept_step(step, context, outbox);
                if fresh_safe_disable {
                    // 新的幂等 Disable 先经原物理退休门；旧 latch 不能先锁成抢占结果。
                    let pending = self.take_pending();
                    match ipc_pending::advance_owned(pending, context.ctrl, context.clock) {
                        Ok(step) => self.accept_step(step, context, outbox),
                        Err(rejected) => {
                            self.pending = rejected.pending;
                            tracing::error!(reason=?rejected.reason,"新 Disable 保原责任");
                        }
                    }
                }
                true
            }
            LowerStartOutcome::Retained(rejected) => {
                self.pending = rejected.pending;
                tracing::error!(reason=?rejected.reason,"Lower 保原责任");
                false
            }
            LowerStartOutcome::BeforeStart {
                pending,
                rr,
                reason,
            } => {
                self.pending = pending;
                if let Some(rr) = rr {
                    Self::reply_set(outbox, rr, SetEnabledRrPayload::QueueFull, SideEffect::None);
                }
                tracing::debug!(?reason, "Lower 尚未开始");
                false
            }
        }
    }
    fn commit_pure(&mut self, event: Event, context: &IpcRuntimeContext<'_>) {
        if let Some(plan) = state_machine::plan_transition(self.state, event, &self.guards) {
            match plan.kind {
                TransitionKind::Pure | TransitionKind::Emergency => {
                    state_machine::commit_transition(&mut self.state, &plan);
                    context.ctrl.set_policy(plan.new_policy);
                }
                TransitionKind::Effectful { .. } => {}
            }
        }
    }
    fn refresh_guards(&mut self, context: &IpcRuntimeContext<'_>) {
        let ctrl = context.ctrl.inner();
        self.guards.estop_latched = ctrl.authority().snapshot().latch;
        if ctrl.ctrl_kind == CtrlKind::SerialRt {
            let abort = ctrl.abort_request.load(Ordering::Acquire);
            let usable = abort == 0
                && ctrl.inhibit_bits() == 0
                && ctrl.boot_outcome.load(Ordering::Acquire) == 1
                && ctrl.rx_trust.load(Ordering::Acquire);
            self.guards.adapter_connected = usable;
            self.guards.adapter_fault_cleared = usable
                && ctrl.safety_state() == OutputSafetyState::TorqueOffConfirmed
                && ctrl.comm_level.load(Ordering::Acquire) == 0
                && context
                    .clock
                    .now()
                    .0
                    .saturating_sub(ctrl.l0_since_ns.load(Ordering::Acquire))
                    >= 500_000_000;
        }
    }
    fn revoke(&mut self, cause: RevokeCause, context: &IpcRuntimeContext<'_>) {
        let observation = self.observation(context);
        let pending = self.take_pending();
        self.pending = match ipc_pending::revoke_enable(pending, cause, observation) {
            Ok(pending) => pending,
            Err(rejected) => rejected.input.0,
        };
        self.control_invalidated = true;
    }
    fn emergency(&mut self, context: &IpcRuntimeContext<'_>, persist: &PersistClient) {
        #[cfg(test)]
        self.emergency_impl(context, persist, || {});
        #[cfg(not(test))]
        self.emergency_impl(context, persist);
    }
    fn emergency_impl(
        &mut self,
        context: &IpcRuntimeContext<'_>,
        persist: &PersistClient,
        #[cfg(test)] after_fault_ack: impl FnOnce(),
    ) {
        if context.ctrl.inner().claim_ipc_estop() {
            self.high.mark_restore_estop_prefix();
            if let Some((_, preempted)) = self.deferred_restore.as_mut() {
                *preempted = true;
            }
            let _ = persist.shared().obligation.request(context.clock.now());
            self.estop_during_clear = true;
            self.guards.estop_latched = true;
            self.revoke(RevokeCause::Emergency, context);
            self.commit_pure(Event::EStop, context);
            #[cfg(feature = "test-comm-fault-hook")]
            context.ctrl.inner().trace_now(
                crate::comm_fault_trace::TraceKind::IpcEStopCommit,
                0,
                0,
            );
        }
        let ctrl = context.ctrl.inner();
        if let Some(fault) = ctrl.mailbox_snapshot()
            && fault.generation != ctrl.fault_ack.load(Ordering::Acquire)
        {
            // mailbox 原发布路径已收紧权限；这里只提交业务故障并归还原公告。
            state_machine::commit_fault(&mut self.state, fault.code);
            context.ctrl.set_policy(OutputPolicy::Disabled);
            self.guards.adapter_fault_cleared = false;
            self.revoke(RevokeCause::Fault, context);
            ctrl.ack_fault(fault.generation);
            #[cfg(test)]
            after_fault_ack();
            #[cfg(feature = "test-comm-fault-hook")]
            {
                let mut trace_context = ctrl.trace_context();
                // ACK 后生产者可以立即换代，归属必须保持本次实际提交的原快照。
                trace_context.generation = fault.generation;
                crate::comm_fault_trace::emit(
                    crate::comm_fault_trace::TraceKind::FaultIpcCommit,
                    crate::comm_fault_trace::now(),
                    trace_context,
                    crate::comm_fault_trace::fault_code(fault.code),
                    u64::from(fault.detail),
                );
            }
        }
    }
    fn admit_rr(&mut self, event: RrEvent, high: bool, draining: bool, outbox: &mut RetiredOutbox) {
        if draining {
            Self::reject_rr(event, true, outbox);
            return;
        }
        if high {
            match HighWork::from_rr(event) {
                Ok(work) => {
                    if let Err(work) = self.high.admit(work) {
                        Self::reject_high(work, false, outbox);
                    }
                }
                Err(event) => Self::reject_rr(event, false, outbox),
            }
        } else {
            match NormalWork::from_rr(event) {
                Ok(work) => {
                    if let Err(work) = self.normal.admit(work) {
                        Self::reject_normal(work, false, outbox);
                    }
                }
                Err(event) => Self::reject_rr(event, false, outbox),
            }
        }
    }
    fn reject_rr(event: RrEvent, shutdown: bool, outbox: &mut RetiredOutbox) {
        match event {
            RrEvent::ClearEStop(request) => Self::reply_clear(
                outbox,
                clear_identity(request),
                if shutdown {
                    ClearEStopRrPayload::ShuttingDown
                } else {
                    ClearEStopRrPayload::QueueFull
                },
                SideEffect::None,
                false,
            ),
            RrEvent::SetEnabled(request) => Self::reply_set(
                outbox,
                set_identity(request),
                if shutdown {
                    SetEnabledRrPayload::PreemptedByShutdown
                } else {
                    SetEnabledRrPayload::QueueFull
                },
                SideEffect::None,
            ),
        }
    }
    fn reject_high(work: HighWork, shutdown: bool, outbox: &mut RetiredOutbox) {
        match work {
            HighWork::LiveDisable(r) | HighWork::LiveEnable(r) => {
                Self::reject_rr(RrEvent::SetEnabled(r), shutdown, outbox);
            }
            HighWork::LiveClear(r) => Self::reject_rr(RrEvent::ClearEStop(r), shutdown, outbox),
            HighWork::Control(_) => {}
        }
    }
    fn reject_normal(work: NormalWork, shutdown: bool, outbox: &mut RetiredOutbox) {
        match work {
            NormalWork::DryRunSet(r) => Self::reject_rr(RrEvent::SetEnabled(r), shutdown, outbox),
            NormalWork::DryRunClear(r) => Self::reject_rr(RrEvent::ClearEStop(r), shutdown, outbox),
        }
    }
    fn poll_sources(
        &mut self,
        context: &IpcRuntimeContext<'_>,
        sources: &IpcSources<'_>,
        sealed: &mut [bool; 4],
        outbox: &mut RetiredOutbox,
    ) {
        for _ in 0..SOURCE_QUOTA {
            match sources.watchdog.try_recv() {
                Ok(Event::Heartbeat) => self.admit_link(LinkEvent::Heartbeat),
                Ok(Event::HeartbeatTimeout) => self.admit_link(LinkEvent::HeartbeatTimeout),
                Ok(Event::EStop) => {
                    let _ = context.ctrl.estop_monotonic();
                }
                Ok(event) => {
                    if let Ok(event) = ControlEvent::try_from(event) {
                        self.admit_control(event, context);
                    }
                }
                Err(_) => break,
            }
        }
        for _ in 0..SOURCE_QUOTA {
            match sources.external.try_recv() {
                Ok(event) => {
                    context
                        .metrics
                        .external_events
                        .fetch_add(1, Ordering::Relaxed);
                    self.admit_control(event, context);
                }
                Err(_) => break,
            }
        }
        let draining = context.boot.runtime_words().generation() != 0;
        if let Some(ch) = sources.clear_rr {
            for (index, rx, high) in [(0, &ch.request_rx, true), (1, &ch.normal_request_rx, false)]
            {
                for _ in 0..SOURCE_QUOTA {
                    match rx.try_recv() {
                        Ok(r) => {
                            if context.clock.now().0 > r.deadline_ns {
                                Self::reply_clear(
                                    outbox,
                                    clear_identity(r),
                                    ClearEStopRrPayload::DeadlineExceeded,
                                    SideEffect::None,
                                    false,
                                );
                            } else {
                                self.admit_rr(RrEvent::ClearEStop(r), high, draining, outbox);
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            sealed[index] = true;
                            if !draining && context.boot.ready_reached() {
                                let _ = context.boot.arm_runtime_shutdown(context.clock.now());
                            }
                            break;
                        }
                    }
                }
            }
        }
        if let Some(ch) = sources.set_rr {
            for (index, rx, high) in [(2, &ch.request_rx, true), (3, &ch.normal_request_rx, false)]
            {
                for _ in 0..SOURCE_QUOTA {
                    match rx.try_recv() {
                        Ok(r) => {
                            if context.clock.now().0 > r.deadline_ns {
                                Self::reply_set(
                                    outbox,
                                    set_identity(r),
                                    SetEnabledRrPayload::DeadlineExceeded,
                                    SideEffect::None,
                                );
                            } else {
                                self.admit_rr(RrEvent::SetEnabled(r), high, draining, outbox);
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            sealed[index] = true;
                            if !draining && context.boot.ready_reached() {
                                let _ = context.boot.arm_runtime_shutdown(context.clock.now());
                            }
                            break;
                        }
                    }
                }
            }
        }
    }
    fn admit_link(&mut self, event: LinkEvent) {
        self.links.admit(event);
    }
    fn admit_control(&mut self, event: ControlEvent, context: &IpcRuntimeContext<'_>) {
        if let Err(HighWork::Control(event)) = self.high.admit(HighWork::Control(event)) {
            // 满队列不能丢掉安全收紧；原高优先事件留在唯一 deferred 槽。
            context.ctrl.request_stop_execution();
            let _ = context.ctrl.authority().disable();
            if self.deferred_control.is_none() {
                self.deferred_control = Some(event);
            }
        }
    }
    fn advance_pending(&mut self, context: &IpcRuntimeContext<'_>, outbox: &mut RetiredOutbox) {
        let observation = self.observation(context);
        let pending = self.take_pending();
        let mut pending = ipc_pending::observe_business(pending, observation);
        let deadline = match &pending {
            IpcPending::Enable(enable) if enable.revoked_by().is_none() => {
                enable
                    .rr()
                    .and_then(|rr| rr.locked())
                    .is_some_and(|locked| {
                        matches!(
                            locked.outcome(),
                            PendingOutcome::Existing(SetEnabledRrPayload::DeadlineExceeded)
                        )
                    })
                    || context.clock.now().0.saturating_sub(enable.started_at().0) > 1_000_000_000
            }
            _ => false,
        };
        if deadline {
            let target = match &pending {
                IpcPending::Enable(enable) => match enable.seq() {
                    Some(seq) => robot_core::serial_services::NormalAbortTarget::Submitted { seq },
                    None => robot_core::serial_services::NormalAbortTarget::Unsubmitted,
                },
                _ => unreachable!("只核 Enable 原责任"),
            };
            let _ = context.ctrl.authority().disable();
            context.ctrl.abort_pending_normal(target);
            context.ctrl.request_stop_execution();
            state_machine::commit_fault(&mut self.state, FaultCode::AdapterErr);
            context.ctrl.set_policy(OutputPolicy::Disabled);
            pending = match ipc_pending::revoke_enable(
                pending,
                RevokeCause::Fault,
                self.observation(context),
            ) {
                Ok(original) => original,
                Err(rejected) => rejected.input.0,
            };
        }
        match pending {
            IpcPending::Control(control) => self.advance_control(control, context),
            pending => match ipc_pending::advance_owned(pending, context.ctrl, context.clock) {
                Ok(step) => self.accept_step(step, context, outbox),
                Err(rejected) => {
                    self.pending = rejected.pending;
                    tracing::debug!(reason=?rejected.reason,"IPC 保原未退休责任");
                }
            },
        }
    }
    pub(crate) fn tick(
        &mut self,
        context: &IpcRuntimeContext<'_>,
        sources: &IpcSources<'_>,
        sealed: &mut [bool; 4],
        persist: &mut PersistClient,
        outbox: &mut RetiredOutbox,
        poll_topics: impl FnOnce() -> TopicEvents,
    ) -> S0StateSnapshot {
        let previous = self.state;
        self.refresh_guards(context);
        let topics = poll_topics();
        if topics.heartbeat {
            self.admit_link(LinkEvent::Heartbeat);
        }
        if topics.estop {
            let _ = context.ctrl.estop_monotonic();
        }
        self.poll_sources(context, sources, sealed, outbox);
        // 只能在真实 claim 之前采样；claim 之后的新收紧必须使本 tick 的 Clear 预约失效。
        let restore_epoch = context.ctrl.authority().snapshot().epoch;
        self.emergency(context, persist);
        if context.boot.runtime_words().generation() != 0 {
            while let Some(work) = self.high.pop() {
                Self::reject_high(work, true, outbox);
            }
            while let Some(work) = self.normal.pop() {
                Self::reject_normal(work, true, outbox);
            }
            self.links.clear();
            self.deferred_control = None;
            if let Some((work, _)) = self.deferred_restore.take() {
                Self::reject_high(work, true, outbox);
            }
            if !self.shutdown_lower_started {
                self.shutdown_lower_started =
                    self.apply_lower(None, LowerCause::Shutdown, context, outbox);
            }
            self.state = S0State::Stopping;
            context.ctrl.set_policy(OutputPolicy::Disabled);
        }
        // 高源的 Lower 先于 Restore，普通请求最后。当前 Clear 不被新 Enable 覆盖。
        let mut high_remaining = crate::event_ingress::RR_QUOTA;
        while high_remaining != 0 {
            let Some(work) = self.high.pop_lower() else {
                break;
            };
            high_remaining -= 1;
            self.process_high(work, restore_epoch, context, persist, outbox);
        }
        // 已准入的 Disable/Stop 已先收紧；原 Enable 真实完成只能在撤销状态中退责。
        self.advance_pending(context, outbox);
        // 本 tick 的超时/Lower 可新置 abort；Restore 不能复用推进前的恢复条件。
        self.refresh_guards(context);
        if matches!(self.pending, IpcPending::Idle) {
            if high_remaining != 0
                && let Some(event) = self.deferred_control.take()
            {
                high_remaining -= 1;
                self.process_control(event, context, outbox);
            }
            if high_remaining != 0
                && matches!(self.pending, IpcPending::Idle)
                && self.deferred_control.is_none()
                && let Some((work, preempted)) = self.deferred_restore.take()
            {
                high_remaining -= 1;
                self.process_restore(work, preempted, restore_epoch, context, persist, outbox);
            }
            for _ in 0..high_remaining {
                // 取队列之前检查唯一延后槽，不能拿第二封信覆盖原 RR。
                if !matches!(self.pending, IpcPending::Idle)
                    || self.deferred_restore.is_some()
                    || self.deferred_control.is_some()
                {
                    break;
                }
                let Some((work, preempted)) = self.high.pop_restore() else {
                    break;
                };
                self.process_restore(work, preempted, restore_epoch, context, persist, outbox);
            }
        }
        for _ in 0..LINK_QUOTA {
            let Some(event) = self.links.pop() else {
                break;
            };
            self.process_link(event, context, outbox);
        }
        for _ in 0..crate::event_ingress::RR_QUOTA {
            let Some(work) = self.normal.pop() else {
                break;
            };
            self.process_normal(work, context, outbox);
        }
        self.advance_clear(context, persist, outbox);
        self.try_ack_lowering(context);
        self.refresh_guards(context);
        self.observe_lower_pending(context);
        if self.state != previous {
            context
                .metrics
                .state_changes
                .fetch_add(1, Ordering::Relaxed);
        }
        context
            .metrics
            .ipc_tick_count
            .fetch_add(1, Ordering::Relaxed);
        context.metrics.tick_count.fetch_add(1, Ordering::Relaxed);
        S0StateSnapshot::new(self.state, self.guards, context.clock.now().0)
    }
}
fn clear_identity(r: ClearEStopRrRequest) -> RrIdentity {
    RrIdentity {
        request_id: r.request_id,
        deadline_ns: r.deadline_ns,
        route: r.route,
    }
}
fn set_identity(r: SetEnabledRrRequest) -> RrIdentity {
    RrIdentity {
        request_id: r.request_id,
        deadline_ns: r.deadline_ns,
        route: r.route,
    }
}

impl IpcLoopState {
    fn process_high(
        &mut self,
        work: HighWork,
        expected_epoch: robot_core::control_authority::PermissionEpoch,
        context: &IpcRuntimeContext<'_>,
        persist: &mut PersistClient,
        outbox: &mut RetiredOutbox,
    ) {
        match work {
            HighWork::Control(event) => self.process_control(event, context, outbox),
            HighWork::LiveDisable(request) => {
                if context.clock.now().0 > request.deadline_ns {
                    Self::reply_set(
                        outbox,
                        set_identity(request),
                        SetEnabledRrPayload::DeadlineExceeded,
                        SideEffect::None,
                    );
                    return;
                }
                let _ = self.apply_lower(
                    Some(set_identity(request)),
                    LowerCause::OperatorDisable,
                    context,
                    outbox,
                );
            }
            HighWork::LiveEnable(request) => self.start_enable(Some(request), context, outbox),
            HighWork::LiveClear(request) => {
                self.start_clear(request, expected_epoch, context, persist, outbox);
            }
        }
    }
    fn process_restore(
        &mut self,
        work: HighWork,
        preempted: bool,
        expected_epoch: robot_core::control_authority::PermissionEpoch,
        context: &IpcRuntimeContext<'_>,
        persist: &mut PersistClient,
        outbox: &mut RetiredOutbox,
    ) {
        match work {
            HighWork::LiveClear(request) if context.clock.now().0 > request.deadline_ns => {
                Self::reply_clear(
                    outbox,
                    clear_identity(request),
                    ClearEStopRrPayload::DeadlineExceeded,
                    SideEffect::None,
                    false,
                );
            }
            HighWork::LiveEnable(request) if context.clock.now().0 > request.deadline_ns => {
                Self::reply_set(
                    outbox,
                    set_identity(request),
                    SetEnabledRrPayload::DeadlineExceeded,
                    SideEffect::None,
                );
            }
            HighWork::LiveClear(request) if preempted => Self::reply_clear(
                outbox,
                clear_identity(request),
                ClearEStopRrPayload::PreemptedByEmergency,
                SideEffect::None,
                false,
            ),
            HighWork::LiveEnable(request) if preempted => Self::reply_set(
                outbox,
                set_identity(request),
                SetEnabledRrPayload::PreemptedByEStop,
                SideEffect::None,
            ),
            work => self.process_high(work, expected_epoch, context, persist, outbox),
        }
    }
    fn process_normal(
        &mut self,
        work: NormalWork,
        context: &IpcRuntimeContext<'_>,
        outbox: &mut RetiredOutbox,
    ) {
        match work {
            NormalWork::DryRunSet(request) => {
                let payload = if context.clock.now().0 > request.deadline_ns {
                    SetEnabledRrPayload::DeadlineExceeded
                } else {
                    match crate::set_enabled_action::evaluate_set_enabled_dry_run(
                        self.state,
                        &self.guards,
                        request.enabled,
                    ) {
                        crate::set_enabled_action::SetEnabledDryRunOutcome::WouldAccept => {
                            SetEnabledRrPayload::WouldAccept {
                                at_ns: context.clock.now().0,
                            }
                        }
                        crate::set_enabled_action::SetEnabledDryRunOutcome::LatchBlocked => {
                            SetEnabledRrPayload::LatchBlocked
                        }
                        crate::set_enabled_action::SetEnabledDryRunOutcome::GuardRejected {
                            actual,
                        } => SetEnabledRrPayload::GuardRejected { actual },
                    }
                };
                Self::reply_set(outbox, set_identity(request), payload, SideEffect::None);
            }
            NormalWork::DryRunClear(request) => {
                let outcome = if context.clock.now().0 > request.deadline_ns {
                    ClearOutcome::DeadlineExceeded
                } else {
                    match crate::clear_estop_action::evaluate_clear_estop_dry_run(self.state,&self.guards,context.ctrl,context.clock) {
                        crate::clear_estop_action::ClearEStopDryRunOutcome::WouldClear{would_clear_at_ns}=>ClearOutcome::WouldClear{at:would_clear_at_ns},
                        crate::clear_estop_action::ClearEStopDryRunOutcome::StateNotSafe{..}=>ClearOutcome::StateNotSafe,
                        crate::clear_estop_action::ClearEStopDryRunOutcome::NotTorqueOffConfirmed{actual}=>ClearOutcome::NotTorqueOffConfirmed{safety:actual},
                    }
                };
                Self::reply_clear(
                    outbox,
                    clear_identity(request),
                    ClearEStopRrPayload::Coordination(outcome),
                    SideEffect::None,
                    false,
                );
            }
        }
    }
    #[allow(
        clippy::too_many_lines,
        reason = "完整保留同一次预约的业务守门、原请求和失败返原顺序"
    )]
    fn start_enable(
        &mut self,
        request: Option<SetEnabledRrRequest>,
        context: &IpcRuntimeContext<'_>,
        outbox: &mut RetiredOutbox,
    ) {
        if request
            .as_ref()
            .is_some_and(|r| context.clock.now().0 > r.deadline_ns)
        {
            if let Some(request) = request {
                Self::reply_set(
                    outbox,
                    set_identity(request),
                    SetEnabledRrPayload::DeadlineExceeded,
                    SideEffect::None,
                );
            }
            return;
        }
        // 真实已运行态的新查询请求只返回当前业务事实，没有第二次执行。
        if matches!(
            self.state,
            S0State::Enabled {
                sub: EnabledSub::Active,
                ..
            }
        ) {
            if let Some(request) = request {
                Self::reply_set(
                    outbox,
                    set_identity(request),
                    SetEnabledRrPayload::Enabled {
                        completed_at_ns: context.clock.now().0,
                    },
                    SideEffect::None,
                );
            }
            return;
        }
        let evaluation =
            crate::set_enabled_action::evaluate_set_enabled_dry_run(self.state, &self.guards, true);
        let rejected = match evaluation {
            crate::set_enabled_action::SetEnabledDryRunOutcome::WouldAccept => None,
            crate::set_enabled_action::SetEnabledDryRunOutcome::LatchBlocked => {
                Some(SetEnabledRrPayload::LatchBlocked)
            }
            crate::set_enabled_action::SetEnabledDryRunOutcome::GuardRejected { actual } => {
                Some(SetEnabledRrPayload::GuardRejected { actual })
            }
        };
        if let Some(payload) = rejected {
            if let Some(request) = request {
                Self::reply_set(outbox, set_identity(request), payload, SideEffect::None);
            }
            return;
        }
        if !self.coordination.is_retired_or_absent() || !matches!(self.pending, IpcPending::Idle) {
            if let Some(request) = request {
                Self::reply_set(
                    outbox,
                    set_identity(request),
                    SetEnabledRrPayload::AlreadyInProgress,
                    SideEffect::None,
                );
            }
            return;
        }
        if self.state == S0State::Safe {
            self.commit_pure(Event::Enable, context);
        }
        // hw_enable_ok 是真实 commit 的结果，不能反过来作为预约前置而永久自锁。
        if !(self.guards.heartbeat_ok && self.guards.cmd_deadline_valid && self.guards.limits_valid)
        {
            match request {
                Some(request) => {
                    self.deferred_restore = Some((HighWork::LiveEnable(request), false));
                }
                None => self.deferred_control = Some(ControlEvent::Enable),
            }
            return;
        }
        let pending = self.take_pending();
        match ipc_pending::start_enable_owned(
            pending,
            request.map(set_identity),
            context.ctrl,
            context.clock.now(),
        ) {
            EnableStartOutcome::Advanced(step) => self.accept_step(step, context, outbox),
            EnableStartOutcome::Retained(rejected) => {
                self.pending = rejected.pending;
                tracing::error!(reason=?rejected.reason,"Enable 保原责任");
            }
            EnableStartOutcome::BeforeReserve {
                current,
                rr,
                reason,
            } => {
                self.pending = current;
                if let Some(rr) = rr {
                    Self::reply_set(
                        outbox,
                        rr,
                        SetEnabledRrPayload::Superseded,
                        SideEffect::None,
                    );
                }
                tracing::debug!(?reason, "Enable 权限未保留");
            }
        }
    }
    fn start_clear(
        &mut self,
        request: ClearEStopRrRequest,
        expected_epoch: robot_core::control_authority::PermissionEpoch,
        context: &IpcRuntimeContext<'_>,
        persist: &mut PersistClient,
        outbox: &mut RetiredOutbox,
    ) {
        let Some(txn) = self.next_txn else {
            Self::reply_clear(
                outbox,
                clear_identity(request),
                ClearEStopRrPayload::Coordination(ClearOutcome::AuthorityRefused(
                    robot_core::control_authority::Refusal::Exhausted,
                )),
                SideEffect::None,
                false,
            );
            return;
        };
        self.next_txn = txn.checked_add(1);
        let inputs = StartInputs {
            state_safe: self.state == S0State::Safe,
            bus_idle_no_lower_obligation: matches!(self.pending, IpcPending::Idle)
                && self.high.len_lower() == 0
                && self.deferred_control.is_none(),
            obligation_settled: persist.shared().obligation.is_settled(),
        };
        match self.coordination.start_with_epoch(
            clear_identity(request),
            txn,
            &inputs,
            context.ctrl.authority(),
            persist,
            context.clock.now().0,
            expected_epoch,
        ) {
            StartOutcome::RefusedBeforeReserve { rr, outcome } => Self::reply_clear(
                outbox,
                rr,
                ClearEStopRrPayload::Coordination(outcome),
                SideEffect::None,
                false,
            ),
            StartOutcome::Retained { .. } => self.estop_during_clear = false,
        }
    }
    fn advance_clear(
        &mut self,
        context: &IpcRuntimeContext<'_>,
        persist: &mut PersistClient,
        outbox: &mut RetiredOutbox,
    ) {
        let inputs = AdvanceInputs {
            now_ns: context.clock.now().0,
            draining: context.boot.runtime_words().generation() != 0,
            estop_seen_since: self.estop_during_clear,
        };
        #[cfg(feature = "test-comm-fault-hook")]
        let before = self.coordination.view().map(ClearAdvanceFact::from_state);
        let events =
            self.coordination
                .advance(context.ctrl.authority(), persist, inputs, context.clock);
        #[cfg(feature = "test-comm-fault-hook")]
        self.record_clear_advance(before.as_ref(), inputs, context);
        let mut retired = None;
        for event in events {
            match event {
                ClearEvent::CommitLogically { at } => {
                    self.guards.estop_latched = context.ctrl.authority().snapshot().latch;
                    tracing::debug!(at, "Clear 真实逻辑提交");
                }
                ClearEvent::Retired(view) => retired = Some(view),
                ClearEvent::Reply {
                    rr,
                    outcome,
                    retired: did_retire,
                } => {
                    let effect = match retired {
                        Some(view) if view.logical_committed_at.is_some() => SideEffect::Committed,
                        Some(_) => SideEffect::Started,
                        None => SideEffect::Unknown,
                    };
                    // Reply 由同协调器退休链产生；协议异常仍保唯一信封到退出，不冒作退休。
                    if !did_retire {
                        tracing::error!("协调器返回未退休 Reply，原信封保持");
                        self.stranded_clear_reply = Some((rr, outcome));
                        continue;
                    }
                    let superseded = matches!(
                        outcome,
                        ClearOutcome::Cleared {
                            superseded: true,
                            ..
                        }
                    );
                    Self::reply_clear(
                        outbox,
                        rr,
                        ClearEStopRrPayload::Coordination(outcome),
                        effect,
                        superseded,
                    );
                }
                ClearEvent::StuckEntered(reason) => {
                    tracing::error!(?reason, "Clear 责任保持，等待真实收尾");
                }
            }
        }
        self.update_health(context, persist);
    }
    #[cfg(feature = "test-comm-fault-hook")]
    fn record_clear_advance(
        &mut self,
        before: Option<&ClearAdvanceFact>,
        inputs: AdvanceInputs,
        context: &IpcRuntimeContext<'_>,
    ) {
        use crate::clear_coordination::{HeldPermit, ReplyState};
        let Some(state) = self.coordination.view() else {
            return;
        };
        let after = ClearAdvanceFact::from_state(state);
        let generation = context.boot.runtime_words().generation();
        let first_shutdown = generation != 0 && self.clear_shutdown_observed == 0;
        let changed =
            before.is_none_or(|old| old.stage != after.stage || old.accepted != after.accepted);
        if !changed && !first_shutdown {
            return;
        }
        if first_shutdown {
            self.clear_shutdown_observed = generation;
        }
        self.clear_fact_sequence = self.clear_fact_sequence.saturating_add(1);
        let ReplyState::Pending { rr, locked } = state.reply() else {
            tracing::info!(
                fact_sequence = self.clear_fact_sequence,
                pending = false,
                "R4重投原责任"
            );
            return;
        };
        let cleanup = match state.held() {
            HeldPermit::Cleanup(permit) => Some(permit),
            _ => None,
        };
        let owner_kind = cleanup.map_or("None", |permit| match permit.owner() {
            robot_core::control_authority::ClearOwner::Pending(_) => "Pending",
            robot_core::control_authority::ClearOwner::Finalizing(_) => "Finalizing",
        });
        let (before_stage, before_due) = clear_stage_fact(before.map(|fact| fact.stage));
        let (after_stage, after_due) = clear_stage_fact(Some(after.stage));
        let before_dispatch = before.and_then(|fact| fact.in_flight);
        let (before_command, before_intent) = clear_dispatch_fact(before_dispatch);
        let (after_command, after_intent) = clear_dispatch_fact(state.in_flight());
        tracing::info!(
            fact_sequence=self.clear_fact_sequence, inputs_now_ns=inputs.now_ns,
            observed_at_ns=context.clock.now().0, draining=inputs.draining,
            first_shutdown, shutdown_generation=generation,
            txn_id=%state.txn().txn_id, expected_epoch=state.txn().expected_epoch,
            original_latched=state.txn().original_latched, before_txn=?before.map(|fact|fact.txn),
            before_stage, before_due, after_stage, after_due, stage_detail=?state.stage(),
            before_count=before.map_or(0, |fact|fact.accepted), after_count=after.accepted,
            before_in_flight=?before_dispatch, before_command, before_intent,
            before_command_txn_id=%before_dispatch.map_or(0, |flight|flight.command.txn().txn_id),
            before_command_epoch=before_dispatch.map_or(0, |flight|flight.command.txn().expected_epoch),
            before_command_latched=?before_dispatch.map(|flight|flight.command.txn().original_latched),
            before_flight=?before.and_then(|fact|fact.flight),
            before_flight_counted=?before.and_then(|fact|fact.flight).map(|flight|flight.counted),
            after_command, after_intent,
            in_flight=?state.in_flight(), flight=?state.flight(), next=?state.next(),
            held_cleanup=cleanup.is_some(), cleanup_kind=?cleanup.map(robot_core::control_authority::CleanupPermit::kind),
            owner_kind, owner_domain=?cleanup.map(|permit|permit.owner().domain()),
            owner_epoch=?cleanup.map(|permit|permit.owner().epoch().get()),
            owner_original_latched=?cleanup.map(|permit|permit.owner().original_latched()),
            sampled_epoch=?cleanup.map(|permit|permit.sampled_epoch().get()),
            pending=true, request_id=%rr.request_id, route=?Some(rr.route), deadline_ns=rr.deadline_ns,
            locked=?locked, logical_committed_at=?state.logical_committed_at(),
            physical_done_at=?state.physical_done_at(),
            "R4重投原责任"
        );
    }

    fn process_control(
        &mut self,
        event: ControlEvent,
        context: &IpcRuntimeContext<'_>,
        outbox: &mut RetiredOutbox,
    ) {
        match event {
            ControlEvent::Enable => self.start_enable(None, context, outbox),
            ControlEvent::Disable => {
                let _ = self.apply_lower(None, LowerCause::OperatorDisable, context, outbox);
            }
            ControlEvent::Stop => {
                if context.boot.ready_reached() {
                    let _ = context.boot.arm_runtime_shutdown(context.clock.now());
                }
                context.ctrl.request_stop_execution();
                self.state = S0State::Stopping;
                context.ctrl.set_policy(OutputPolicy::Disabled);
            }
            // 无 RR 身份的旧 bare Clear 入口不再具有事务授权，不能清任何 latch。
            ControlEvent::ClearEStop => {
                tracing::warn!("ClearEStop 必须通过拥有原 RR 身份的事务入口");
            }
            ControlEvent::ClearError => {
                if matches!(self.pending, IpcPending::Idle) {
                    self.commit_pure(Event::ClearError, context);
                } else {
                    self.deferred_control = Some(event);
                }
            }
            ControlEvent::AdapterErr(code) => self.enter_fault(code, context),
            ControlEvent::CmdDeadlineExpired
            | ControlEvent::LimitViolation
            | ControlEvent::WatchdogTimeout
            | ControlEvent::RtOverrun => {
                self.commit_pure(event.into(), context);
                let _ = context.ctrl.authority().disable();
                context.ctrl.request_stop_execution();
                self.revoke(RevokeCause::Fault, context);
            }
        }
    }
    fn enter_fault(&mut self, code: FaultCode, context: &IpcRuntimeContext<'_>) {
        let _ = context.ctrl.authority().disable();
        context.ctrl.request_stop_execution();
        state_machine::commit_fault(&mut self.state, code);
        context.ctrl.set_policy(OutputPolicy::Disabled);
        self.revoke(RevokeCause::Fault, context);
    }
    fn process_link(
        &mut self,
        event: LinkEvent,
        context: &IpcRuntimeContext<'_>,
        outbox: &mut RetiredOutbox,
    ) {
        self.guards.heartbeat_ok = match event {
            LinkEvent::Heartbeat => true,
            LinkEvent::HeartbeatTimeout => false,
        };
        let Some(plan) = state_machine::plan_transition(self.state, event.into(), &self.guards)
        else {
            return;
        };
        match plan.kind {
            TransitionKind::Effectful {
                action: AdapterNormalReq::Enable(true),
            } => {
                if self.deferred_restore.is_none() && matches!(self.pending, IpcPending::Idle) {
                    self.start_enable(None, context, outbox);
                }
            }
            TransitionKind::Effectful { .. } => {
                if matches!(self.pending, IpcPending::Idle) {
                    self.pending =
                        IpcPending::Control(ipc_pending::new_control(plan, context.clock.now()));
                    self.control_invalidated = false;
                } else {
                    self.admit_link(event);
                }
            }
            TransitionKind::Pure | TransitionKind::Emergency => {
                state_machine::commit_transition(&mut self.state, &plan);
                context.ctrl.set_policy(plan.new_policy);
            }
        }
    }
    fn advance_control(
        &mut self,
        control: ipc_pending::ControlPending,
        context: &IpcRuntimeContext<'_>,
    ) {
        let expired = context.clock.now().0.saturating_sub(control.started_at().0) > 1_000_000_000;
        if expired && !self.control_invalidated {
            self.enter_fault(FaultCode::AdapterErr, context);
            let target = match control.seq() {
                Some(seq) => robot_core::serial_services::NormalAbortTarget::Submitted { seq },
                None => robot_core::serial_services::NormalAbortTarget::Unsubmitted,
            };
            context.ctrl.abort_pending_normal(target);
            self.control_invalidated = true;
        }
        if control.seq().is_some() {
            match ipc_pending::finish_control_owned(control, context.ctrl) {
                Ok((plan, AdapterResp::Ok)) => {
                    if !self.control_invalidated {
                        state_machine::commit_transition(&mut self.state, &plan);
                        context.ctrl.set_policy(plan.new_policy);
                    }
                    self.control_invalidated = false;
                }
                Ok((_, AdapterResp::Err(_))) => {
                    self.enter_fault(FaultCode::AdapterErr, context);
                    self.control_invalidated = false;
                }
                Err(original) => self.pending = IpcPending::Control(original),
            }
            return;
        }
        if self.control_invalidated {
            match ipc_pending::cancel_unsubmitted_control(control) {
                Ok(_) => self.control_invalidated = false,
                Err(original) => self.pending = IpcPending::Control(original),
            }
            return;
        }
        let action = match control.plan().kind {
            TransitionKind::Effectful { action } => action,
            TransitionKind::Pure | TransitionKind::Emergency => {
                self.pending = IpcPending::Control(control);
                return;
            }
        };
        match context.ctrl.request_normal(action) {
            NormalReqStatus::Submitted { seq } => {
                self.pending =
                    match ipc_pending::record_submitted(IpcPending::Control(control), seq) {
                        Ok(pending) => pending,
                        Err(rejected) => rejected.input.0,
                    };
            }
            NormalReqStatus::Busy => self.pending = IpcPending::Control(control),
            NormalReqStatus::Unsupported(_) => {
                match ipc_pending::cancel_unsubmitted_control(control) {
                    Ok(_) => self.enter_fault(FaultCode::AdapterErr, context),
                    Err(original) => self.pending = IpcPending::Control(original),
                }
            }
        }
    }
    fn try_ack_lowering(&self, context: &IpcRuntimeContext<'_>) {
        let words = context.boot.runtime_words();
        let g = words.generation();
        if g == 0
            || self.lowered_generation.map(NonZeroU64::get) != Some(g)
            || !matches!(self.pending, IpcPending::Idle)
            || !self.high.is_empty()
            || self.deferred_control.is_some()
            || self.deferred_restore.is_some()
            || !context.ctrl.start_slot().is_empty()
            || context.ctrl.execution_slot().load().is_some()
        {
            return;
        }
        let Some(binding) = context.boot.binding() else {
            return;
        };
        if !std::sync::Arc::ptr_eq(binding.authority(), context.ctrl.authority()) {
            return;
        }
        let snapshot = context.ctrl.authority().snapshot();
        if snapshot.phase != Some(robot_core::control_authority::Phase::Idle)
            || !snapshot.shutdown
            || !snapshot.disabled
            || snapshot.exhausted
            || snapshot.normal_io_active
            || snapshot.safety != OutputSafetyState::TorqueOffConfirmed
        {
            return;
        }
        // 原 g 只发布一次；不把当前权限 epoch 当关机代次。
        let _ =
            words
                .lowering_ack_epoch
                .compare_exchange(0, g, Ordering::AcqRel, Ordering::Acquire);
    }
    fn update_health(&mut self, context: &IpcRuntimeContext<'_>, persist: &PersistClient) {
        use crate::persist_health::CurrentDegradation as C;
        use robot_health::RuntimeDegradation as R;
        let now = context.clock.now();
        let report = crate::persist_health::classify(crate::persist_health::HealthInputs {
            obligation: persist.shared().obligation.view(),
            now_ns: now.0,
            alarm_ns: 2_000_000_000,
            writer_alive: persist.alive(),
            clear_stuck: self.coordination.stuck(),
        });
        let mut current = robot_health::RuntimeDegradations::empty();
        for kind in &report.current {
            current.insert(match kind {
                C::PersistPending => R::PersistPending,
                C::WriterGone => R::WriterGone,
                C::RepairExhausted => R::RepairExhausted,
                C::MarkerLost => R::MarkerLost,
                C::ProtocolBug => R::ProtocolBug,
                C::PairMismatch => R::PairMismatch,
                C::RetireRefused => R::RetireRefused,
            });
        }
        let unsettled = report.unsettled.map(|v| robot_health::RuntimeUnsettled {
            since_ns: v.since_ns,
            overdue: v.overdue,
        });
        self.health.report(
            now.0,
            robot_health::RuntimeHealthInput { current, unsettled },
        );
        #[cfg(feature = "test-comm-fault-hook")]
        if let Some(snapshot) = self.health.current() {
            crate::comm_fault_hook::record_current_health(snapshot);
        }
        if self.overdue.observe(&report).is_some() {
            #[cfg(feature = "test-comm-fault-hook")]
            if let Some(page) = context.boot.observation() {
                page.store(
                    crate::comm_fault_observation::ObservationField::ObligationOverdue,
                    1,
                );
            }
        }
    }
}

#[cfg(test)]
mod owned_tick_tests {
    use super::*;
    use crate::latch_writer::{PersistEndpoint, StandbyStore, WriterPairing, WriterSlot};
    use crate::rr_admission::{CheckedLimits, PeerLedger, RrRequestClass, Slot};
    use robot_core::control_authority::{
        AuthorityInit, ControlAuthority, InhibitSet, NormalIoIntent, Phase,
    };
    use robot_core::time::{DurationNs, MockClock, MonoRawNs};
    use std::sync::Arc;

    #[test]
    fn lower_log_changes_are_bounded_and_deadline_is_reported_once() {
        let diagnostic = LowerDiagnostic {
            wait_mask: LowerDiagnostic::LOWER_RESPONSE,
            source_seq: None,
            lower_seq: Some(7),
            phase: Some(Phase::Idle),
            safety: OutputSafetyState::TorqueOffConfirmed,
            inhibits: 0,
        };
        let key = LowerLogKey {
            started_at: 1,
            diagnostic,
            waiter_count: 1,
            earliest_deadline: 1_000_000_000,
        };
        let (log, near, count) = lower_log_decision(None, key, 0);
        assert_eq!((log, near, count), (true, false, 1));
        let previous = LowerLogState {
            key,
            logged_at: 0,
            changes_logged: count,
            near_deadline_logged: near,
        };
        let mut changed = key;
        changed.diagnostic.wait_mask |= LowerDiagnostic::NORMAL_IO_ACTIVE;
        assert!(!lower_log_decision(Some(previous), changed, 1).0);
        assert!(lower_log_decision(Some(previous), changed, 100_000_000).0);
        let capped = LowerLogState {
            changes_logged: LOWER_LOG_MAX_CHANGES,
            ..previous
        };
        assert!(!lower_log_decision(Some(capped), changed, 500_000_000).0);
        let (log, near, _) = lower_log_decision(Some(capped), changed, 750_000_000);
        assert!(log);
        assert!(near);
        let once = LowerLogState {
            near_deadline_logged: true,
            ..capped
        };
        assert!(!lower_log_decision(Some(once), changed, 800_000_000).0);
    }

    /// 确定性 tick 调度夹具使用真队列、真原语及同一 RT 消费函数；不称真实线程 E2E。
    struct Rig {
        state: IpcLoopState,
        clock: Arc<MockClock>,
        ctrl: HardRtCtrlClient,
        boot: BootShared,
        persist: PersistClient,
        _endpoint: PersistEndpoint,
        _writer: WriterSlot,
        _standby: StandbyStore,
        _lock: crate::latch_lock::LatchTargetLock,
        directory: tempfile::TempDir,
        outbox: RetiredOutbox,
        metrics: IpcThreadMetrics,
        ledger: PeerLedger<u32>,
        slots: Vec<Slot<u32>>,
        adapter: robot_adapter_mock::MockHardRTAdapter,
    }
    impl Rig {
        fn new(heartbeat: bool) -> Self {
            let authority = Arc::new(
                ControlAuthority::new(AuthorityInit::new(
                    false,
                    OutputSafetyState::TorqueOffConfirmed,
                    InhibitSet::EMPTY,
                ))
                .unwrap(),
            );
            Self::with_authority(heartbeat, authority)
        }
        fn with_authority(heartbeat: bool, authority: Arc<ControlAuthority>) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("estop_latch");
            crate::estop_latch::save_estop_latch(&path, false).unwrap();
            let lock = crate::latch_lock::acquire_latch_lock(&path).unwrap();
            let worker =
                crate::clear_estop_transaction::ClearTxnStore::open(&path, "tick", "arm").unwrap();
            let standby =
                crate::clear_estop_transaction::ClearTxnStore::open(&path, "tick", "arm").unwrap();
            let (writer, standby) = WriterPairing::new(worker, standby, &lock, MonoRawNs(10))
                .unwrap()
                .into_parts();
            let boot = BootShared::new(MonoRawNs(10));
            let permit = boot
                .install_runtime_for_test(authority.clone(), writer.shared().clone())
                .unwrap_or_else(|_| panic!("真实同域绑定"));
            drop(permit);
            let (persist, endpoint) = crate::latch_writer::mailbox(writer.shared().clone());
            let guards = Guards {
                adapter_connected: true,
                calibration_loaded: true,
                joint_state_valid: true,
                power_ok: true,
                heartbeat_ok: heartbeat,
                cmd_deadline_valid: true,
                limits_valid: true,
                hw_enable_ok: false,
                adapter_fault_cleared: true,
                ..Guards::default()
            };
            let ledger = PeerLedger::new(
                &CheckedLimits::try_from_limits(&robot_transport::iceoryx2::RrStaticLimits {
                    l: 4,
                    c: 8,
                })
                .unwrap(),
            )
            .unwrap();
            let mut adapter = robot_adapter_mock::MockHardRTAdapter::with_unlimited_limits(2);
            robot_core::adapter::ManipulatorAdapter::connect(&mut adapter).unwrap();
            Self {
                state: IpcLoopState::new(S0State::Safe, guards, 32),
                clock: Arc::new(MockClock::new(MonoRawNs(10))),
                ctrl: HardRtCtrlClient::new(Arc::new(crate::hard_rt_ctrl::HardRtCtrlInner::new(
                    authority,
                ))),
                boot,
                persist,
                _endpoint: endpoint,
                _writer: writer,
                _standby: standby,
                _lock: lock,
                directory,
                outbox: RetiredOutbox::default(),
                metrics: IpcThreadMetrics::default(),
                ledger,
                slots: Vec::new(),
                adapter,
            }
        }
        fn request(&mut self, id: &str, peer: u32, enabled: bool, deadline: u64) {
            let slot = self
                .ledger
                .admit(peer, RrRequestClass::of_set_enabled(false, enabled))
                .unwrap();
            let request = SetEnabledRrRequest {
                route: slot.identity(),
                request_id: id.to_owned(),
                enabled,
                dry_run: false,
                deadline_ns: deadline,
                audit_reason: "owned tick".to_owned(),
            };
            self.slots.push(slot);
            self.state
                .high
                .admit(if enabled {
                    HighWork::LiveEnable(request)
                } else {
                    HighWork::LiveDisable(request)
                })
                .unwrap();
        }
        fn tick(&mut self) -> S0StateSnapshot {
            let (_watchdog, watchdog) = crossbeam_channel::bounded(1);
            let (_external, external) = crossbeam_channel::bounded(1);
            let context = IpcRuntimeContext {
                ctrl: &self.ctrl,
                boot: &self.boot,
                clock: self.clock.as_ref(),
                metrics: &self.metrics,
            };
            self.state.tick(
                &context,
                &IpcSources {
                    watchdog: &watchdog,
                    external: &external,
                    clear_rr: None,
                    set_rr: None,
                },
                &mut [true; 4],
                &mut self.persist,
                &mut self.outbox,
                TopicEvents::default,
            )
        }
        fn execute_submitted(&mut self) {
            let inner = self.ctrl.inner();
            let raw = inner.pending_normal.swap(0, Ordering::AcqRel);
            let original = inner
                .decode_pending(raw)
                .unwrap()
                .expect("由原 IPC 真 post 产生");
            assert!(
                crate::rt_thread::execute_owned_normal(&mut self.adapter, original, inner)
                    .is_none()
            );
        }
    }
    fn supervised(name: &str) -> bool {
        const CHILD: &str = "ROBOT_W3_OWNED_TICK_CHILD";
        if std::env::var(CHILD).as_deref() == Ok(name) {
            let owner = std::thread::current().id();
            std::panic::set_hook(Box::new(move |info| {
                eprintln!("owned tick failure: {info}");
                if std::thread::current().id() == owner {
                    std::process::exit(101);
                }
            }));
            return false;
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("child.log");
        let log = std::fs::File::create(&path).unwrap();
        let exact = format!("ipc_runtime::owned_tick_tests::{name}");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &exact, "--nocapture", "--test-threads=1"])
            .env(CHILD, name)
            .env("TMPDIR", directory.path())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                result => {
                    let _ = child.kill();
                    let limit = std::time::Instant::now() + std::time::Duration::from_secs(2);
                    while !matches!(child.try_wait(), Ok(Some(_))) {
                        if std::time::Instant::now() >= limit {
                            std::process::abort();
                        }
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    panic!("子进程已 kill/reap: {result:?}");
                }
            }
        };
        let raw = std::fs::read_to_string(path).unwrap();
        assert!(status.success(), "{raw}");
        assert_eq!(
            raw.lines()
                .filter(|line| *line == format!("owned_tick_verified={name}"))
                .count(),
            1,
            "{raw}"
        );
        true
    }
    // stdout/stderr 合并时，先换行隔开单线程 libtest 的测试名前缀。
    macro_rules! case {($name:ident,$body:block)=>{
        #[test] fn $name(){let name=stringify!($name);if supervised(name){return;} $body
            eprintln!("\nowned_tick_verified={name}");std::process::exit(0);
        }
    };}

    case!(
        fresh_latched_disable_waits_for_real_periodic_read_release,
        {
            let authority = Arc::new(
                ControlAuthority::new(AuthorityInit::new(
                    true,
                    OutputSafetyState::TorqueOffConfirmed,
                    InhibitSet::EMPTY,
                ))
                .unwrap(),
            );
            let mut rig = Rig::with_authority(true, authority);
            // Off 与已有 latch 允许真实周期读；该原租约必须先阻止 Lower 物理退休。
            let lease = rig
                .ctrl
                .authority()
                .acquire_normal_io(NormalIoIntent::PeriodicRead)
                .unwrap();
            rig.request("latched-read-held", 1, false, 1_000_000);
            let route = rig.slots[0].identity();
            rig.tick();
            assert!(rig.ctrl.authority().snapshot().normal_io_active);
            assert!(matches!(rig.state.pending, IpcPending::Lower(_)));
            assert!(rig.outbox.set.is_empty(), "原读租约未归还不能伪造退休回复");
            rig.ctrl.authority().release_normal_io(lease).unwrap();
            rig.tick();
            assert!(matches!(rig.state.pending, IpcPending::Idle));
            assert_eq!(rig.outbox.set.len(), 1);
            let reply = rig.outbox.set.front().unwrap();
            assert_eq!(reply.route, route);
            assert_eq!(reply.request_id, "latched-read-held");
            assert!(
                matches!(reply.payload, SetEnabledRrPayload::Disabled { .. }),
                "真实读归还后须按原 Lower 退休，既有 latch 不能冒称后来的抢占：{:?}",
                reply.payload
            );
            assert_eq!(reply.effect, SideEffect::Committed);
            let after = rig.ctrl.authority().snapshot();
            assert!(after.latch);
            assert!(after.disabled);
            assert!(!after.normal_io_active);
            assert_eq!(after.safety, OutputSafetyState::TorqueOffConfirmed);
            assert_eq!(rig.ctrl.inner().pending_normal.load(Ordering::Acquire), 0);
            rig.tick();
            assert_eq!(rig.outbox.set.len(), 1, "同一 Slot 原回复只产生一次");
            std::mem::forget(rig);
        }
    );

    case!(fresh_latched_safe_disable_uses_real_lower_retirement, {
        let authority = Arc::new(
            ControlAuthority::new(AuthorityInit::new(
                true,
                OutputSafetyState::TorqueOffConfirmed,
                InhibitSet::EMPTY,
            ))
            .unwrap(),
        );
        let mut rig = Rig::with_authority(true, authority);
        let epoch = rig.ctrl.authority().snapshot().epoch;
        rig.request("latched-fresh", 1, false, 1_000_000);
        let route = rig.slots[0].identity();
        rig.tick();
        assert!(matches!(rig.state.pending, IpcPending::Idle));
        assert_eq!(rig.state.state, S0State::Safe);
        assert_eq!(rig.outbox.set.len(), 1);
        let reply = rig.outbox.set.front().unwrap();
        assert_eq!(reply.route, route);
        assert_eq!(reply.request_id, "latched-fresh");
        assert!(matches!(
            reply.payload,
            SetEnabledRrPayload::Disabled { .. }
        ));
        assert_eq!(reply.effect, SideEffect::Committed);
        let after = rig.ctrl.authority().snapshot();
        assert!(after.latch);
        assert!(after.disabled);
        assert_eq!(after.safety, OutputSafetyState::TorqueOffConfirmed);
        assert_eq!(after.epoch.get(), epoch.get() + 1);
        assert_eq!(rig.ctrl.inner().pending_normal.load(Ordering::Acquire), 0);
        rig.tick();
        assert_eq!(rig.outbox.set.len(), 1, "原响应只能产生一次");
        std::mem::forget(rig);
    });
    case!(
        fresh_disable_keeps_unfinished_off_and_existing_lower_first_result,
        {
            let authority = Arc::new(
                ControlAuthority::new(AuthorityInit::new(
                    true,
                    OutputSafetyState::TorqueOn,
                    InhibitSet::EMPTY,
                ))
                .unwrap(),
            );
            let mut rig = Rig::with_authority(true, authority);
            rig.request("needs-off", 1, false, 1_000_000);
            rig.tick();
            assert!(matches!(rig.state.pending, IpcPending::Lower(_)));
            assert!(rig.outbox.set.is_empty(), "Safe 业务状态不能替代真实 Off");
            assert_ne!(rig.ctrl.inner().pending_normal.load(Ordering::Acquire), 0);
            rig.execute_submitted();
            rig.request("already-lower", 2, false, 1_000_000);
            rig.tick();
            assert!(matches!(rig.state.pending, IpcPending::Idle));
            assert_eq!(rig.outbox.set.len(), 2);
            for reply in &rig.outbox.set {
                assert!(matches!(
                    reply.payload,
                    SetEnabledRrPayload::PreemptedByEStop
                ));
            }
            assert_eq!(
                rig.ctrl.authority().snapshot().safety,
                OutputSafetyState::TorqueOffConfirmed
            );
            std::mem::forget(rig);
        }
    );
    case!(fresh_disable_cannot_retire_unclaimed_executor_estop, {
        let mut rig = Rig::new(true);
        let _ = rig.ctrl.estop_monotonic();
        rig.request("executor-notice", 1, false, 1_000_000);
        rig.tick();
        assert!(rig.ctrl.authority().snapshot().serial_estop_pending);
        assert!(matches!(rig.state.pending, IpcPending::Lower(_)));
        assert!(rig.outbox.set.is_empty());
        std::mem::forget(rig);
    });

    #[cfg(feature = "test-comm-fault-hook")]
    fn verify_fault_trace_generation(ssf: bool) {
        use crate::comm_fault_trace::{TraceKind, capture_emitted};
        use crate::hard_rt_ctrl::{FaultOrigin, inhibit};
        let mut rig = Rig::new(true);
        let inner = rig.ctrl.inner().clone();
        let code = if ssf {
            FaultCode::SafetyStopFailed
        } else {
            FaultCode::CommFault
        };
        let generation = inner
            .publish_fault(
                OutputSafetyState::AdapterUncertain,
                inhibit::FAULT_PENDING,
                code,
                17,
                FaultOrigin::InvariantViolation,
            )
            .unwrap();
        let context = IpcRuntimeContext {
            ctrl: &rig.ctrl,
            boot: &rig.boot,
            clock: rig.clock.as_ref(),
            metrics: &rig.metrics,
        };
        let events = capture_emitted(|| {
            rig.state.emergency_impl(&context, &rig.persist, || {
                assert_eq!(inner.fault_ack.load(Ordering::Acquire), generation);
                assert_eq!(
                    inner.publish_fault(
                        OutputSafetyState::AdapterUncertain,
                        inhibit::FAULT_PENDING,
                        FaultCode::AdapterErr,
                        23,
                        FaultOrigin::InvariantViolation
                    ),
                    Some(generation + 1)
                );
            });
        });
        let commits: Vec<_> = events
            .iter()
            .filter(|event| event.kind == TraceKind::FaultIpcCommit)
            .collect();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].generation, generation);
        assert_eq!(commits[0].a, crate::comm_fault_trace::fault_code(code));
        assert_eq!(commits[0].b, 17);
        assert_eq!(rig.state.state, S0State::Fault(code));
        assert_eq!(inner.mailbox_snapshot().unwrap().generation, generation + 1);
        std::mem::forget(rig);
    }
    #[cfg(feature = "test-comm-fault-hook")]
    case!(
        fault_trace_keeps_acked_generation_when_next_fault_is_published,
        {
            verify_fault_trace_generation(false);
        }
    );
    #[cfg(feature = "test-comm-fault-hook")]
    case!(
        ssf_commit_emits_its_actual_generation_before_next_mailbox,
        {
            verify_fault_trace_generation(true);
        }
    );
    #[cfg(feature = "test-comm-fault-hook")]
    case!(
        real_estop_claim_emits_one_ipc_commit_after_business_transition,
        {
            use crate::comm_fault_trace::{TraceKind, capture_emitted};
            let mut rig = Rig::new(true);
            let _ = rig.ctrl.estop_monotonic();
            let events = capture_emitted(|| {
                rig.tick();
            });
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.kind == TraceKind::IpcEStopCommit)
                    .count(),
                1
            );
            assert_eq!(rig.state.state, S0State::Safe);
            assert!(rig.state.guards.estop_latched);
            assert!(rig.persist.shared().obligation.has_pending());
            let next = capture_emitted(|| {
                rig.tick();
            });
            assert!(
                !next
                    .iter()
                    .any(|event| event.kind == TraceKind::IpcEStopCommit)
            );
            std::mem::forget(rig);
        }
    );
    case!(effectful_unsupported_direct_fault, {
        let mut rig = Rig::new(true);
        rig.ctrl = HardRtCtrlClient::new(Arc::new(
            crate::hard_rt_ctrl::HardRtCtrlInner::new_serial(rig.ctrl.authority().clone()),
        ));
        let inner = rig.ctrl.inner();
        inner.boot_outcome.store(1, Ordering::Release);
        inner.rx_trust.store(true, Ordering::Release);
        inner.seq_counter.store(u64::MAX, Ordering::Release);
        rig.request("unsupported", 1, true, u64::MAX);
        let route = rig.slots[0].identity();
        rig.tick();
        assert!(matches!(rig.state.pending, IpcPending::Enable(_)));
        for _ in 0..3 {
            rig.tick();
        }
        assert!(matches!(rig.state.pending, IpcPending::Idle));
        assert!(rig.ctrl.start_slot().is_empty());
        assert_eq!(rig.ctrl.authority().snapshot().phase, Some(Phase::Idle));
        assert_eq!(rig.ctrl.inner().pending_normal.load(Ordering::Acquire), 0);
        let reply = rig.outbox.set.front().unwrap();
        assert_eq!(reply.route, route);
        assert_eq!(reply.request_id, "unsupported");
        assert_eq!(reply.payload, SetEnabledRrPayload::AdapterError);
        assert_eq!(reply.effect, SideEffect::None);
        assert_eq!(rig.outbox.set.len(), 1);
        assert_eq!(rig.state.state, S0State::Fault(FaultCode::AdapterErr));
        std::mem::forget(rig);
    });
    case!(
        real_hard_rt_enable_error_commits_fault_without_reissuing_stop,
        {
            let mut rig = Rig::new(true);
            robot_core::adapter::ManipulatorAdapter::disconnect(&mut rig.adapter).unwrap();
            rig.request("device-error", 1, true, u64::MAX);
            rig.tick();
            let sequence = match &rig.state.pending {
                IpcPending::Enable(enable) => enable.seq().unwrap(),
                _ => panic!("真实Enable预约"),
            };
            rig.execute_submitted();
            assert_eq!(
                rig.ctrl.poll_last_normal_resp(sequence),
                Some(robot_core::executor_ctrl::AdapterResp::Err(
                    robot_core::executor_ctrl::AdapterErr::HardwareFault
                ))
            );
            // 原执行器已经安装一次物理补偿。IPC业务Fault不能重排第二次Stop。
            let original_stop = rig.ctrl.inner().emergency_action.swap(0, Ordering::AcqRel);
            assert_ne!(original_stop, 0);
            rig.tick();
            assert_eq!(rig.state.state, S0State::Fault(FaultCode::AdapterErr));
            assert_eq!(rig.ctrl.inner().emergency_action.load(Ordering::Acquire), 0);
            assert!(rig.ctrl.authority().snapshot().disabled);
            std::mem::forget(rig);
        }
    );
    case!(
        real_enable_failure_fact_preserves_prior_fault_and_safety_boundaries,
        {
            // 同一真实提交边界的确定性交错；不把业务state夹具当A2权限。
            for (prior, latch, shutdown) in [
                (S0State::Fault(FaultCode::SafetyStopFailed), false, false),
                (S0State::Stopping, false, false),
                (S0State::Ready, true, false),
                (S0State::Ready, false, true),
            ] {
                let mut rig = Rig::new(true);
                rig.ctrl = HardRtCtrlClient::new(Arc::new(
                    crate::hard_rt_ctrl::HardRtCtrlInner::new_serial(rig.ctrl.authority().clone()),
                ));
                rig.ctrl
                    .inner()
                    .seq_counter
                    .store(u64::MAX, Ordering::Release);
                let slot = rig
                    .ledger
                    .admit(1, RrRequestClass::of_set_enabled(false, true))
                    .unwrap();
                let rr = RrIdentity {
                    route: slot.identity(),
                    request_id: "old-cause".to_owned(),
                    deadline_ns: u64::MAX,
                };
                rig.slots.push(slot);
                let EnableStartOutcome::Advanced(step) = ipc_pending::start_enable_owned(
                    IpcPending::Idle,
                    Some(rr),
                    &rig.ctrl,
                    rig.clock.now(),
                ) else {
                    panic!("真实Unsupported首次交回原owner")
                };
                assert!(matches!(
                    step.failure.as_ref().unwrap().source(),
                    ipc_pending::EnableFailureSource::SubmitUnsupported(_)
                ));
                rig.state.state = prior;
                if latch {
                    let _ = rig.ctrl.estop_monotonic();
                }
                if shutdown {
                    let _ = rig.boot.arm_runtime_shutdown(rig.clock.now()).unwrap();
                }
                let original = rig.ctrl.authority().snapshot();
                let context = IpcRuntimeContext {
                    ctrl: &rig.ctrl,
                    boot: &rig.boot,
                    clock: rig.clock.as_ref(),
                    metrics: &rig.metrics,
                };
                rig.state.accept_step(step, &context, &mut rig.outbox);
                assert_eq!(rig.state.state, prior);
                assert_eq!(rig.ctrl.authority().snapshot(), original);
                assert_eq!(rig.ctrl.inner().emergency_action.load(Ordering::Acquire), 0);
                let next = ipc_pending::advance_owned(
                    rig.state.take_pending(),
                    &rig.ctrl,
                    rig.clock.as_ref(),
                )
                .unwrap();
                assert!(next.failure.is_none(), "原route不重复产生失败事实");
                rig.state.accept_step(next, &context, &mut rig.outbox);
                std::mem::forget(rig);
            }
        }
    );
    case!(
        dry_run_torque_on_preserves_original_authority_and_both_files,
        {
            for (state, safety, expected) in [
                (
                    S0State::Safe,
                    OutputSafetyState::TorqueOn,
                    ClearOutcome::NotTorqueOffConfirmed {
                        safety: OutputSafetyState::TorqueOn,
                    },
                ),
                (
                    S0State::Ready,
                    OutputSafetyState::TorqueOffConfirmed,
                    ClearOutcome::StateNotSafe,
                ),
                (
                    S0State::Safe,
                    OutputSafetyState::TorqueOffConfirmed,
                    ClearOutcome::WouldClear { at: 10 },
                ),
            ] {
                let authority = Arc::new(
                    ControlAuthority::new(AuthorityInit::new(false, safety, InhibitSet::EMPTY))
                        .unwrap(),
                );
                let mut rig = Rig::with_authority(true, authority);
                assert!(!rig.state.guards.estop_latched);
                rig.state.state = state;
                let slot = rig.ledger.admit(1, RrRequestClass::of_clear(true)).unwrap();
                let route = slot.identity();
                rig.slots.push(slot);
                let request = ClearEStopRrRequest {
                    route,
                    request_id: "dry-on".to_owned(),
                    dry_run: true,
                    confirmation: "确认".to_owned(),
                    deadline_ns: u64::MAX,
                    audit_reason: "只读断扭守门".to_owned(),
                };
                let before = rig.ctrl.authority().snapshot();
                let guards = rig.state.guards;
                let path = rig.directory.path().join("estop_latch");
                let pair = (
                    std::fs::read(&path).unwrap(),
                    std::fs::read(path.with_extension("bak")).unwrap(),
                );
                rig.state
                    .normal
                    .admit(NormalWork::DryRunClear(request))
                    .unwrap();
                rig.tick();
                let reply = rig.outbox.clear.front().unwrap();
                assert_eq!(reply.route, route);
                assert_eq!(reply.payload, ClearEStopRrPayload::Coordination(expected));
                assert_eq!(reply.effect, SideEffect::None);
                assert!(!reply.superseded);
                assert_eq!(rig.ctrl.authority().snapshot(), before);
                assert_eq!(rig.state.guards, guards);
                assert_eq!(rig.state.state, state);
                assert_eq!(
                    (
                        std::fs::read(&path).unwrap(),
                        std::fs::read(path.with_extension("bak")).unwrap()
                    ),
                    pair
                );
                assert!(!rig.persist.shared().obligation.has_pending());
                assert!(rig.state.coordination.is_retired_or_absent());
                std::mem::forget(rig);
            }
        }
    );

    case!(
        lowering_ack_waits_for_original_sequence_and_publishes_only_original_shutdown_generation,
        {
            let mut rig = Rig::new(true);
            rig.request("held", 1, true, u64::MAX);
            rig.tick();
            let inner = rig.ctrl.inner();
            let raw = inner.pending_normal.swap(0, Ordering::AcqRel);
            let original = inner.decode_pending(raw).unwrap().unwrap();
            let _ = rig.boot.arm_runtime_shutdown(rig.clock.now()).unwrap();
            let g = rig.boot.runtime_words().generation();
            rig.tick();
            assert_eq!(
                rig.boot
                    .runtime_words()
                    .lowering_ack_epoch
                    .load(Ordering::Acquire),
                0
            );
            assert!(!matches!(rig.state.pending, IpcPending::Idle));
            // 原 packed seq 先被真实执行器取走，迟到消费仍经同一真实完成函数退责。
            assert!(
                crate::rt_thread::execute_owned_normal(
                    &mut rig.adapter,
                    original,
                    rig.ctrl.inner()
                )
                .is_none()
            );
            let _ = rig.ctrl.authority().disable();
            let later = rig.ctrl.authority().snapshot().epoch.get();
            assert_ne!(later, g);
            rig.tick();
            assert_eq!(
                rig.boot
                    .runtime_words()
                    .lowering_ack_epoch
                    .load(Ordering::Acquire),
                g
            );
            assert_ne!(
                rig.boot
                    .runtime_words()
                    .lowering_ack_epoch
                    .load(Ordering::Acquire),
                later
            );
            std::mem::forget(rig);
        }
    );
    case!(
        lowering_ack_keeps_real_clear_owner_and_start_announcement_until_retirement,
        {
            let mut clear = Rig::new(true);
            let original = clear.ctrl.authority().reserve_clear().unwrap();
            let _ = clear.boot.arm_runtime_shutdown(clear.clock.now()).unwrap();
            clear.tick();
            assert_eq!(
                clear.ctrl.authority().snapshot().phase,
                Some(Phase::ClearPreparing)
            );
            assert_eq!(
                clear
                    .boot
                    .runtime_words()
                    .lowering_ack_epoch
                    .load(Ordering::Acquire),
                0
            );
            let _ = clear
                .ctrl
                .authority()
                .retire_unprepared_clear(original)
                .unwrap();
            // Lower 已在等待原 owner 时投递真实 Disable；退休 owner 不能替代该 seq 的完成。
            clear.execute_submitted();
            clear.tick();
            assert_eq!(
                clear
                    .boot
                    .runtime_words()
                    .lowering_ack_epoch
                    .load(Ordering::Acquire),
                clear.boot.runtime_words().generation()
            );
            std::mem::forget(clear);
            // 原公开槽与owner的单独持有是守门边界夹具，不宣称正常IPC会分裂自身owner。
            let mut queued = Rig::new(true);
            let (owner, view) = queued.ctrl.authority().reserve_enable().unwrap();
            let epoch = owner.epoch();
            queued.ctrl.start_slot().post(view).unwrap();
            let _ = queued
                .boot
                .arm_runtime_shutdown(queued.clock.now())
                .unwrap();
            queued.tick();
            assert!(!queued.ctrl.start_slot().is_empty());
            assert_eq!(
                queued
                    .boot
                    .runtime_words()
                    .lowering_ack_epoch
                    .load(Ordering::Acquire),
                0
            );
            assert!(queued.ctrl.start_slot().withdraw(epoch));
            let _ = queued.ctrl.authority().retire_enable(owner).unwrap();
            queued.execute_submitted();
            queued.tick();
            assert_eq!(
                queued
                    .boot
                    .runtime_words()
                    .lowering_ack_epoch
                    .load(Ordering::Acquire),
                queued.boot.runtime_words().generation()
            );
            std::mem::forget(queued);
        }
    );
    #[cfg(feature = "test-comm-fault-hook")]
    case!(lowering_ack_never_publishes_for_real_exhausted_authority, {
        let authority = Arc::new(
            ControlAuthority::new_with_permission_epoch(
                AuthorityInit::new(
                    false,
                    OutputSafetyState::TorqueOffConfirmed,
                    InhibitSet::EMPTY,
                ),
                robot_core::control_authority::PermissionEpoch::MAX,
            )
            .unwrap(),
        );
        let mut rig = Rig::with_authority(true, authority);
        let _ = rig.boot.arm_runtime_shutdown(rig.clock.now()).unwrap();
        rig.tick();
        assert!(rig.ctrl.authority().snapshot().exhausted);
        assert_eq!(
            rig.ctrl.authority().snapshot().safety,
            OutputSafetyState::TorqueOffConfirmed
        );
        assert_eq!(
            rig.boot
                .runtime_words()
                .lowering_ack_epoch
                .load(Ordering::Acquire),
            0
        );
        std::mem::forget(rig);
    });
    case!(first_hw_false_enable_uses_real_start_finish_commit, {
        let mut rig = Rig::new(true);
        rig.request("enable", 1, true, u64::MAX);
        rig.tick();
        assert!(!rig.state.guards.hw_enable_ok);
        assert!(rig.ctrl.authority().snapshot().disabled);
        rig.execute_submitted();
        assert_eq!(
            rig.ctrl.authority().snapshot().phase,
            Some(Phase::EnableSucceeded)
        );
        assert!(rig.outbox.set.is_empty());
        let snapshot = rig.tick();
        assert!(matches!(
            snapshot.state,
            S0State::Enabled {
                sub: EnabledSub::Active,
                ..
            }
        ));
        assert!(rig.state.guards.hw_enable_ok);
        let response = rig.outbox.set.front().unwrap();
        assert_eq!(response.request_id, "enable");
        assert_eq!(response.effect, SideEffect::Committed);
        assert!(!rig.ctrl.authority().snapshot().disabled);
        std::mem::forget(rig);
    });
    case!(same_tick_disable_precedes_completed_enable_commit, {
        let mut rig = Rig::new(true);
        rig.request("enable", 1, true, u64::MAX);
        rig.tick();
        rig.execute_submitted();
        let epoch = rig.ctrl.authority().snapshot().epoch;
        rig.request("disable", 1, false, u64::MAX);
        rig.tick();
        assert_ne!(rig.ctrl.authority().snapshot().epoch, epoch);
        assert!(rig.ctrl.authority().snapshot().disabled);
        assert!(
            rig.ctrl
                .authority()
                .acquire_normal_io(NormalIoIntent::Motion)
                .is_err()
        );
        assert!(
            !rig.outbox
                .set
                .iter()
                .any(|reply| reply.effect == SideEffect::Committed)
        );
        // 原待清理 owner/seq/两 RR 保留到受监督子进程出口，绝不以 drop 生成退休。
        std::mem::forget(rig);
    });
    case!(enable_retire_applies_link_before_publish, {
        let mut rig = Rig::new(true);
        rig.request("enable", 1, true, u64::MAX);
        rig.tick();
        rig.execute_submitted();
        rig.state.admit_link(LinkEvent::HeartbeatTimeout);
        let final_snapshot = rig.tick();
        assert!(matches!(
            final_snapshot.state,
            S0State::Enabled {
                control_link: crate::state_machine::ControlLink::Lost,
                ..
            }
        ));
        assert_eq!(
            rig.outbox.set.front().unwrap().effect,
            SideEffect::Committed,
            "真实 commit 事实不因随后 Timeout 改写"
        );
        assert_eq!(rig.metrics.ipc_tick_count.load(Ordering::Acquire), 2);
        std::mem::forget(rig);
    });
    case!(deferred_restore_keeps_both_original_rr_identities, {
        let mut rig = Rig::new(false);
        rig.request("first", 1, true, 11);
        rig.request("second", 2, true, 11);
        rig.tick();
        rig.tick();
        assert!(rig.outbox.set.is_empty());
        assert!(
            matches!(rig.state.deferred_restore.as_ref(),Some((HighWork::LiveEnable(request),false)) if request.request_id=="first")
        );
        assert_eq!(rig.state.high.len_restore(), 1);
        assert_eq!(rig.ctrl.authority().snapshot().phase, Some(Phase::Idle));
        rig.clock.advance(DurationNs(2));
        rig.tick();
        let ids: Vec<_> = rig
            .outbox
            .set
            .iter()
            .map(|reply| reply.request_id.as_str())
            .collect();
        assert_eq!(ids, ["first", "second"]);
        for reply in &rig.outbox.set {
            assert_eq!(reply.payload, SetEnabledRrPayload::DeadlineExceeded);
            assert_eq!(reply.effect, SideEffect::None);
        }
        std::mem::forget(rig);
    });
    case!(link_overflow_replays_timeout_after_ordered_prefix, {
        let mut rig = Rig::new(false);
        for index in 0..64 {
            rig.state.admit_link(if index % 2 == 0 {
                LinkEvent::HeartbeatTimeout
            } else {
                LinkEvent::Heartbeat
            });
        }
        rig.state.admit_link(LinkEvent::HeartbeatTimeout);
        for _ in 0..4 {
            rig.tick();
        }
        assert!(
            rig.state.guards.heartbeat_ok,
            "满载前缀最后一个 Heartbeat 尚先于 overflow Timeout"
        );
        assert!(!rig.state.links.is_empty());
        rig.tick();
        assert!(!rig.state.guards.heartbeat_ok);
        assert!(rig.state.links.is_empty());
        std::mem::forget(rig);
    });
}

#[cfg(test)]
#[path = "post_e_ipc_acceptance.rs"]
mod post_e_ipc_acceptance;
