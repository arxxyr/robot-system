//! IPC 的完整单 tick：输入配额、急停、mailbox、pending 与三个普通 lane。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, Sender};
use robot_core::executor_ctrl::{ExecutorCtrlClient, OutputSafetyState, SerialEmergencyCtrl};
use robot_core::time::RobotClock;

use crate::clear_estop_action::{self, ClearEStopDryRunOutcome};
use crate::event_ingress::{
    CONTROL_QUOTA, ControlEvent, EmergencyBatch, EventQueue, LINK_QUOTA, LinkEvent, RR_QUOTA,
    RrEvent, SOURCE_QUOTA,
};
use crate::hard_rt_ctrl::{FaultOrigin, HardRtCtrlInner, MailboxSnapshot};
use crate::ipc_thread::{
    ClearEStopRrPayload, ClearEStopRrRequest, ClearEStopRrResponse, IpcThreadMetrics,
    PendingSetEnabled, PendingTarget, RrChannels, SetEnabledRrChannels, SetEnabledRrPayload,
    SetEnabledRrRequest, SetEnabledRrResponse,
};
use crate::ipc_tick::{
    ClearEStopTickOutcome, IpcRetryState, IpcTickContext, PendingAdvance, TickOutcome,
    advance_pending, apply_control_event, apply_emergency_batch_observed, service_fault_mailbox,
};
use crate::set_enabled_action::{self, SetEnabledDryRunOutcome};
use crate::state_machine::{EnabledSub, Event, FaultCode, Guards, S0State, S0StateSnapshot};

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
    pub executor_ctrl: &'a dyn ExecutorCtrlClient,
    pub serial_ctrl: Option<&'a HardRtCtrlInner>,
    pub emergency_ctrl: Option<&'a dyn SerialEmergencyCtrl>,
    pub clock: &'a dyn RobotClock,
    pub latch_path: Option<&'a Path>,
    pub metrics: &'a IpcThreadMetrics,
}

struct RrReplySinks<'a> {
    clear: Option<&'a Sender<ClearEStopRrResponse>>,
    set: Option<&'a Sender<SetEnabledRrResponse>>,
}

#[derive(Default)]
struct PendingReplies {
    in_flight_live: Option<ClearEStopRrRequest>,
    clear_result: Option<ClearEStopRrPayload>,
    set_enabled: Option<PendingSetEnabled>,
}

pub(crate) struct IpcLoopState {
    pub(crate) state: S0State,
    pub(crate) guards: Guards,
    pub(crate) retry: IpcRetryState,
    pub(crate) events: EventQueue,
    pending: PendingReplies,
}

impl IpcLoopState {
    pub(crate) fn new(state: S0State, guards: Guards, legacy_notice: Arc<AtomicBool>) -> Self {
        Self {
            state,
            guards,
            retry: IpcRetryState::new(),
            events: EventQueue::with_estop_notice(legacy_notice),
            pending: PendingReplies::default(),
        }
    }

    pub(crate) fn tick(
        &mut self,
        context: &IpcRuntimeContext<'_>,
        sources: &IpcSources<'_>,
        poll_topics: impl FnOnce() -> TopicEvents,
    ) -> S0StateSnapshot {
        self.tick_impl(context, sources, poll_topics, || {})
    }

    #[cfg(test)]
    pub(crate) fn tick_after_ingress(
        &mut self,
        context: &IpcRuntimeContext<'_>,
        sources: &IpcSources<'_>,
        poll_topics: impl FnOnce() -> TopicEvents,
        after_ingress: impl FnOnce(),
    ) -> S0StateSnapshot {
        self.tick_impl(context, sources, poll_topics, after_ingress)
    }

    fn tick_impl(
        &mut self,
        context: &IpcRuntimeContext<'_>,
        sources: &IpcSources<'_>,
        poll_topics: impl FnOnce() -> TopicEvents,
        after_ingress: impl FnOnce(),
    ) -> S0StateSnapshot {
        let previous = self.state;
        let replies = RrReplySinks {
            clear: sources.clear_rr.map(|channels| &channels.response_tx),
            set: sources.set_rr.map(|channels| &channels.response_tx),
        };
        self.refresh_serial_guards(context);
        let topics = poll_topics();
        if topics.heartbeat {
            self.events.admit_link(LinkEvent::Heartbeat);
        }
        if topics.estop {
            match context.emergency_ctrl {
                Some(emergency) => {
                    let outcome = emergency.estop_monotonic();
                    tracing::info!(?outcome, "topic 急停原语已发布");
                }
                None => self.events.set_estop(),
            }
        }
        let batch = self.capture_emergency(context);
        self.admit_sources(context, sources, &replies);
        after_ingress();
        if batch.estop {
            // 仅标当前已 admission 的前缀；紧急路径 O(1)，回复仍占后续 RR 配额。
            self.events.mark_rr_estop_prefix();
        }
        let mut tick = IpcTickContext {
            retry: &mut self.retry,
            state: &mut self.state,
            guards: &mut self.guards,
            executor_ctrl: context.executor_ctrl,
            serial_ctrl: context.serial_ctrl,
            clock: context.clock,
            latch_path: context.latch_path,
        };
        let emergency = apply_emergency_batch_observed(&mut tick, batch, |persisted| {
            let counter = if persisted {
                &context.metrics.latch_writes
            } else {
                &context.metrics.latch_write_errors
            };
            counter.fetch_add(1, Ordering::Relaxed);
        });
        service_fault_mailbox(&mut tick);
        let pending_advance = if emergency {
            PendingAdvance::Ready
        } else {
            advance_pending(&mut tick)
        };
        if pending_advance == PendingAdvance::CancelledByEmergency {
            // 取消属于刚退场的请求；先取走原 owner，不能影响本 tick 后续 admission。
            cancel_pending_set(&mut self.pending, &replies);
        }
        if !emergency && pending_advance != PendingAdvance::InFlight {
            process_lanes(
                &mut self.events,
                &mut tick,
                &replies,
                &mut self.pending,
                context.metrics,
            );
        }
        resolve_pending(
            &mut self.pending,
            &replies,
            self.state,
            batch,
            context.clock.now().0,
        );
        self.refresh_serial_guards(context);
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

    fn refresh_serial_guards(&mut self, context: &IpcRuntimeContext<'_>) {
        self.refresh_serial_guards_impl(context, || {}, || {});
    }

    #[cfg(test)]
    pub(crate) fn refresh_serial_guards_after_abort(
        &mut self,
        context: &IpcRuntimeContext<'_>,
        after_abort: impl FnOnce(),
    ) {
        self.refresh_serial_guards_impl(context, after_abort, || {});
    }

    #[cfg(test)]
    pub(crate) fn refresh_serial_guards_after_inhibit(
        &mut self,
        context: &IpcRuntimeContext<'_>,
        after_inhibit: impl FnOnce(),
    ) {
        self.refresh_serial_guards_impl(context, || {}, after_inhibit);
    }

    fn refresh_serial_guards_impl(
        &mut self,
        context: &IpcRuntimeContext<'_>,
        after_abort: impl FnOnce(),
        after_inhibit: impl FnOnce(),
    ) {
        let Some(ctrl) = context.serial_ctrl else {
            return;
        };
        // 先读 retire 请求，再读 inhibit；观察到 retire 清零后也能看到先发布的故障位。
        let abort_request = ctrl.abort_request.load(Ordering::Acquire);
        after_abort();
        let inhibited = ctrl.inhibit.load(Ordering::Acquire) != 0;
        after_inhibit();
        let usable = abort_request == 0
            && !inhibited
            && ctrl.boot_outcome.load(Ordering::Acquire) == 1
            && ctrl.rx_trust.load(Ordering::Acquire);
        self.guards.adapter_connected = usable;
        let healthy_duration = context
            .clock
            .now()
            .0
            .saturating_sub(ctrl.l0_since_ns.load(Ordering::Acquire));
        self.guards.adapter_fault_cleared = usable
            && ctrl.safety_state() == OutputSafetyState::TorqueOffConfirmed
            && ctrl.comm_level.load(Ordering::Acquire) == 0
            && healthy_duration >= 500_000_000;
    }

    fn capture_emergency(&mut self, context: &IpcRuntimeContext<'_>) -> EmergencyBatch {
        match context.serial_ctrl {
            Some(ctrl) => EmergencyBatch {
                estop: ctrl.claim_ipc_estop(),
                ssf: ctrl.mailbox_snapshot().filter(|mailbox| {
                    mailbox.code == FaultCode::SafetyStopFailed
                        && mailbox.generation != ctrl.fault_ack.load(Ordering::Acquire)
                }),
            },
            None => EmergencyBatch {
                estop: self.events.claim_estop(),
                ssf: self.events.claim_legacy_ssf().then_some(MailboxSnapshot {
                    generation: 0,
                    code: FaultCode::SafetyStopFailed,
                    detail: 0,
                    origin: FaultOrigin::InvariantViolation,
                }),
            },
        }
    }

    fn admit_sources(
        &mut self,
        context: &IpcRuntimeContext<'_>,
        sources: &IpcSources<'_>,
        replies: &RrReplySinks<'_>,
    ) {
        for _ in 0..SOURCE_QUOTA {
            let Ok(event) = sources.watchdog.try_recv() else {
                break;
            };
            context
                .metrics
                .watchdog_events
                .fetch_add(1, Ordering::Relaxed);
            match event {
                Event::Heartbeat => self.events.admit_link(LinkEvent::Heartbeat),
                Event::HeartbeatTimeout => self.events.admit_link(LinkEvent::HeartbeatTimeout),
                _ => tracing::error!(?event, "watchdog channel 收到非链路事件"),
            }
        }
        for _ in 0..SOURCE_QUOTA {
            let Ok(event) = sources.external.try_recv() else {
                break;
            };
            context
                .metrics
                .external_events
                .fetch_add(1, Ordering::Relaxed);
            if let Err(error) = self.events.admit_control(event) {
                tracing::warn!(?error, ?event, "IPC control lane 拒绝 admission");
            }
        }
        if let Some(channels) = sources.clear_rr {
            for _ in 0..SOURCE_QUOTA {
                let Ok(request) = channels.request_rx.try_recv() else {
                    break;
                };
                admit_rr(
                    &mut self.events,
                    RrEvent::ClearEStop(request),
                    replies,
                    context.clock,
                );
            }
        }
        if let Some(channels) = sources.set_rr {
            for _ in 0..SOURCE_QUOTA {
                let Ok(request) = channels.request_rx.try_recv() else {
                    break;
                };
                admit_rr(
                    &mut self.events,
                    RrEvent::SetEnabled(request),
                    replies,
                    context.clock,
                );
            }
        }
    }
}

fn reply_clear(
    replies: &RrReplySinks<'_>,
    request: ClearEStopRrRequest,
    payload: ClearEStopRrPayload,
) {
    if let Some(sender) = replies.clear {
        let _ = sender.send(ClearEStopRrResponse {
            request_id: request.request_id,
            payload,
        });
    }
}

fn reply_set(
    replies: &RrReplySinks<'_>,
    request: SetEnabledRrRequest,
    payload: SetEnabledRrPayload,
) {
    if let Some(sender) = replies.set {
        let _ = sender.send(SetEnabledRrResponse {
            request_id: request.request_id,
            payload,
        });
    }
}

fn reject_rr(request: RrEvent, replies: &RrReplySinks<'_>, expired: bool) {
    match request {
        RrEvent::ClearEStop(request) => reply_clear(
            replies,
            request,
            if expired {
                ClearEStopRrPayload::DeadlineExceeded
            } else {
                ClearEStopRrPayload::AlreadyInProgress
            },
        ),
        RrEvent::SetEnabled(request) => reply_set(
            replies,
            request,
            if expired {
                SetEnabledRrPayload::DeadlineExceeded
            } else {
                SetEnabledRrPayload::AlreadyInProgress
            },
        ),
    }
}

fn admit_rr(
    events: &mut EventQueue,
    request: RrEvent,
    replies: &RrReplySinks<'_>,
    clock: &dyn RobotClock,
) {
    if clock.now().0 > request.deadline_ns() {
        reject_rr(request, replies, true);
    } else if let Err(request) = events.admit_rr(request) {
        reject_rr(request, replies, false);
    }
}

fn observe_outcome(outcome: TickOutcome, metrics: &IpcThreadMetrics) {
    match outcome.clear_estop {
        Some(ClearEStopTickOutcome::Cleared { .. }) => {
            metrics.latch_writes.fetch_add(1, Ordering::Relaxed);
        }
        Some(ClearEStopTickOutcome::FsyncFailed) => {
            metrics.latch_write_errors.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

fn process_lanes(
    events: &mut EventQueue,
    ctx: &mut IpcTickContext<'_>,
    replies: &RrReplySinks<'_>,
    pending: &mut PendingReplies,
    metrics: &IpcThreadMetrics,
) {
    for _ in 0..LINK_QUOTA {
        let Some(event) = events.pop_link() else {
            break;
        };
        observe_outcome(apply_control_event(ctx, event.into()), metrics);
        if ctx.retry.pending.is_some() {
            return;
        }
    }
    for _ in 0..RR_QUOTA {
        let Some((request, preempted)) = events.pop_rr_with_preemption() else {
            break;
        };
        // 第二道 deadline 位于 dequeue 之后、任何 evaluator/副作用/pending 之前。
        if ctx.clock.now().0 > request.deadline_ns() {
            reject_rr(request, replies, true);
        } else if preempted {
            process_preempted_rr(ctx, request, replies, pending, metrics);
        } else {
            process_rr_event(ctx, request, replies, pending, metrics);
        }
        if ctx.retry.pending.is_some() {
            return;
        }
    }
    for _ in 0..CONTROL_QUOTA {
        let Some(event) = events.pop_control() else {
            break;
        };
        observe_outcome(apply_control_event(ctx, event.into()), metrics);
        if ctx.retry.pending.is_some() {
            return;
        }
    }
}

fn process_preempted_rr(
    ctx: &mut IpcTickContext<'_>,
    request: RrEvent,
    replies: &RrReplySinks<'_>,
    pending: &mut PendingReplies,
    metrics: &IpcThreadMetrics,
) {
    match request {
        RrEvent::ClearEStop(request) if !request.dry_run => {
            reply_clear(replies, request, ClearEStopRrPayload::PreemptedByEmergency);
        }
        RrEvent::SetEnabled(request) if !request.dry_run => {
            reply_set(replies, request, SetEnabledRrPayload::PreemptedByEStop);
        }
        request => process_rr_event(ctx, request, replies, pending, metrics),
    }
}

fn process_rr_event(
    ctx: &mut IpcTickContext<'_>,
    request: RrEvent,
    replies: &RrReplySinks<'_>,
    pending: &mut PendingReplies,
    metrics: &IpcThreadMetrics,
) {
    match request {
        RrEvent::ClearEStop(request) => process_clear(ctx, request, replies, pending, metrics),
        RrEvent::SetEnabled(request) => process_set(ctx, request, replies, pending, metrics),
    }
}

fn process_clear(
    ctx: &mut IpcTickContext<'_>,
    request: ClearEStopRrRequest,
    replies: &RrReplySinks<'_>,
    pending: &mut PendingReplies,
    metrics: &IpcThreadMetrics,
) {
    if request.dry_run {
        #[cfg(test)]
        metrics.clear_evaluations.fetch_add(1, Ordering::Relaxed);
        let payload = match clear_estop_action::evaluate_clear_estop_dry_run(
            *ctx.state,
            ctx.guards,
            ctx.executor_ctrl,
            ctx.clock,
        ) {
            ClearEStopDryRunOutcome::WouldClear { would_clear_at_ns } => {
                ClearEStopRrPayload::WouldClear { would_clear_at_ns }
            }
            ClearEStopDryRunOutcome::StateNotSafe { actual } => {
                ClearEStopRrPayload::StateNotSafe { actual }
            }
            ClearEStopDryRunOutcome::NotTorqueOffConfirmed { actual } => {
                ClearEStopRrPayload::NotTorqueOffConfirmed { actual }
            }
        };
        reply_clear(replies, request, payload);
    } else if pending.in_flight_live.is_some() {
        reply_clear(replies, request, ClearEStopRrPayload::AlreadyInProgress);
    } else {
        pending.in_flight_live = Some(request);
        #[cfg(test)]
        metrics.clear_evaluations.fetch_add(1, Ordering::Relaxed);
        let outcome = apply_control_event(ctx, Event::ClearEStop);
        observe_outcome(outcome, metrics);
        let payload = match outcome.clear_estop {
            Some(ClearEStopTickOutcome::Cleared { cleared_at_ns }) => {
                ClearEStopRrPayload::Cleared { cleared_at_ns }
            }
            Some(ClearEStopTickOutcome::StateNotSafe { actual }) => {
                ClearEStopRrPayload::StateNotSafe { actual }
            }
            Some(ClearEStopTickOutcome::NotTorqueOffConfirmed { actual }) => {
                ClearEStopRrPayload::NotTorqueOffConfirmed { actual }
            }
            Some(ClearEStopTickOutcome::FsyncFailed) | None => ClearEStopRrPayload::FsyncFailed,
        };
        // 单飞持有至本 tick 派发真实结果，后续 live 不能覆盖对应请求。
        pending.clear_result = Some(payload);
    }
}

fn process_set(
    ctx: &mut IpcTickContext<'_>,
    request: SetEnabledRrRequest,
    replies: &RrReplySinks<'_>,
    pending: &mut PendingReplies,
    metrics: &IpcThreadMetrics,
) {
    #[cfg(test)]
    metrics.set_evaluations.fetch_add(1, Ordering::Relaxed);
    #[cfg(not(test))]
    let _ = metrics;
    let evaluation =
        set_enabled_action::evaluate_set_enabled_dry_run(*ctx.state, ctx.guards, request.enabled);
    let immediate = match () {
        () if !request.dry_run && pending.set_enabled.is_some() => {
            Some(SetEnabledRrPayload::AlreadyInProgress)
        }
        () => match evaluation {
            SetEnabledDryRunOutcome::LatchBlocked => Some(SetEnabledRrPayload::LatchBlocked),
            SetEnabledDryRunOutcome::GuardRejected { actual } => {
                Some(SetEnabledRrPayload::GuardRejected { actual })
            }
            SetEnabledDryRunOutcome::WouldAccept if request.dry_run => {
                Some(SetEnabledRrPayload::WouldAccept {
                    at_ns: ctx.clock.now().0,
                })
            }
            SetEnabledDryRunOutcome::WouldAccept
                if !request.enabled && matches!(ctx.state, S0State::Safe | S0State::Fault(_)) =>
            {
                Some(SetEnabledRrPayload::Disabled {
                    completed_at_ns: ctx.clock.now().0,
                })
            }
            SetEnabledDryRunOutcome::WouldAccept => None,
        },
    };
    if let Some(payload) = immediate {
        reply_set(replies, request, payload);
        return;
    }
    let (event, target) = if request.enabled {
        (Event::Enable, PendingTarget::EnabledActive)
    } else {
        (Event::Disable, PendingTarget::Safe)
    };
    let _ = apply_control_event(ctx, event);
    pending.set_enabled = Some(PendingSetEnabled {
        request_id: request.request_id,
        deadline_ns: request.deadline_ns,
        target,
        requested_enabled: request.enabled,
    });
}

fn resolve_pending(
    pending: &mut PendingReplies,
    replies: &RrReplySinks<'_>,
    state: S0State,
    emergency: EmergencyBatch,
    now: u64,
) {
    if let Some(payload) = pending.clear_result.take()
        && let Some(request) = pending.in_flight_live.take()
    {
        reply_clear(replies, request, payload);
    }
    let Some(request) = pending.set_enabled.as_ref() else {
        return;
    };
    let reached = match request.target {
        PendingTarget::EnabledActive => matches!(
            state,
            S0State::Enabled {
                sub: EnabledSub::Active,
                ..
            }
        ),
        PendingTarget::Safe => state == S0State::Safe,
    };
    let payload = match () {
        () if emergency.estop => Some(SetEnabledRrPayload::PreemptedByEStop),
        () if state == S0State::Stopping => Some(SetEnabledRrPayload::PreemptedByShutdown),
        () if matches!(
            state,
            S0State::Fault(
                FaultCode::AdapterErr | FaultCode::CommFault | FaultCode::SafetyStopFailed
            )
        ) =>
        {
            Some(SetEnabledRrPayload::AdapterError)
        }
        () if reached => Some(match request.target {
            PendingTarget::EnabledActive => SetEnabledRrPayload::Enabled {
                completed_at_ns: now,
            },
            PendingTarget::Safe => SetEnabledRrPayload::Disabled {
                completed_at_ns: now,
            },
        }),
        () if now > request.deadline_ns => Some(SetEnabledRrPayload::DeadlineExceeded),
        () => None,
    };
    if let Some(payload) = payload
        && let Some(request) = pending.set_enabled.take()
        && let Some(sender) = replies.set
    {
        let _ = sender.send(SetEnabledRrResponse {
            request_id: request.request_id,
            payload,
        });
    }
}

fn cancel_pending_set(pending: &mut PendingReplies, replies: &RrReplySinks<'_>) {
    if let Some(request) = pending.set_enabled.take()
        && let Some(sender) = replies.set
    {
        let _ = sender.send(SetEnabledRrResponse {
            request_id: request.request_id,
            payload: SetEnabledRrPayload::PreemptedByEStop,
        });
    }
}
