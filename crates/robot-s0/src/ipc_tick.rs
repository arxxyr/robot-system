//! Task-30 v9 / M7 阶段 3：`ipc_tick` —— state_machine 与 executor ctrl 的
//! errata 三步式 tick（设计 v9 §4.4.3 errata 实施版）。
//!
//! ## 角色
//!
//! 旧 `ipc_thread::run_ipc_loop` 直接调 `try_transition` 写新 state，**不**驱动
//! `ExecutorCtrlClient`，state_machine ↔ adapter 执行链路是断的。本模块按
//! v9 §4.4 两阶段 transition 协议提供新的 tick 函数：
//!
//! ```text
//! Step 1: events.drain_emergency()
//!         → emergency event 命中即清 pending + commit Emergency + safety_stop
//!         → return（本 tick 不再处理后续）
//!
//! Step 2: 推进 in-flight effectful pending（仅 Effectful 才会 reach 这里）：
//!         if pending_started_at + 1s < now → commit_fault(AdapterErr) + return
//!         match seq:
//!           None        → request_normal → Submitted{seq} | Busy(下 tick) | Unsupported(commit_fault)
//!           Some(seq)   → poll_last_normal_resp(seq) → None(下 tick) | Ok(commit) | Err(commit_fault)
//!
//! Step 3: plan_transition（pending 为空时）
//!         while events.pop() → plan match kind:
//!           Pure       → commit + set_policy 立即
//!           Emergency  → safety_stop + commit + set_policy 立即
//!           Effectful  → retry.pending = Some((plan, None)) + 起计 1s + break
//! ```
//!
//! ## §0.5 errata 三条强制约束
//!
//! - **A1**：emergency 必须 preempt in-flight pending。Step 1 头部消化
//!   emergency 后即清 `retry.pending`；后续 ack 即使返回也丢弃。
//! - **A2**：`pending_started_at` 在 plan **入 pending 那一刻**置位（不是
//!   `Submitted{seq}` 才置）。`Busy` 重试期间不重置。
//! - **A3**：effectful ack 失败（`AdapterResp::Err(_)` / 1s 超时 /
//!   `NormalReqStatus::Unsupported`）**直接**调 `commit_fault` +
//!   `set_policy(Disabled)`，**不**走 `Event::AdapterErr` 入队再 plan_transition。
//!
//! 三条 finding #3 修订（v9 已收紧）：所有 `AdapterErr` 变体（含 `SafetyLatched`）
//! 一律映射到 `FaultCode::AdapterErr`；`SafetyStopFailed` 仅来自 emergency
//! 路径或 `confirm_torque_off` 失败。

#![allow(clippy::doc_markdown)]

use std::path::Path;

use robot_core::executor_ctrl::{
    AdapterResp, ExecutorCtrlClient, NormalReqStatus, OutputPolicy, OutputSafetyState,
};
use robot_core::time::{DurationNs, MonoRawNs, RobotClock};

use crate::clear_estop_action;
use crate::state_machine::{
    Event, EventQueue, FaultCode, PlannedTransition, S0State, TransitionKind, commit_fault,
    commit_transition, plan_transition,
};

/// `ipc_tick` 之间持久的"待执行 effectful transition"状态。
///
/// `pending` 三态：
/// - `None`：无 in-flight，可以 plan 新 transition
/// - `Some((plan, None))`：plan 已 reserved，下 tick 提交（首次 request_normal）
/// - `Some((plan, Some(seq)))`：已 Submitted，等 poll 返结果
///
/// `pending_started_at`：A2 约束 —— **plan 入 pending 那一刻**就置位（不是
/// Submitted 才置）。Busy 重试期间不重置；超 1s 升 Fault。
#[derive(Clone, Debug, Default)]
pub struct IpcRetryState {
    pub pending: Option<(PlannedTransition, Option<u64>)>,
    pub pending_started_at: Option<MonoRawNs>,
}

impl IpcRetryState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 清空 pending（emergency drain / ack 失败 / 超时 共用）。
    pub fn clear(&mut self) {
        self.pending = None;
        self.pending_started_at = None;
    }
}

/// effectful pending 的总超时（v9 §0.5 A2 约定 = 1 s）。
const PENDING_TOTAL_TIMEOUT: DurationNs = DurationNs::from_secs(1);

/// 单 tick 借用上下文；SerialRT 通过同一共享 ctrl 完成 mailbox 与退场协议。
pub(crate) struct IpcTickContext<'a> {
    pub retry: &'a mut IpcRetryState,
    pub state: &'a mut S0State,
    pub guards: &'a mut Guards,
    pub executor_ctrl: &'a dyn ExecutorCtrlClient,
    pub serial_ctrl: Option<&'a crate::hard_rt_ctrl::HardRtCtrlInner>,
    pub clock: &'a dyn RobotClock,
    pub latch_path: Option<&'a Path>,
}

/// 所有包含 EStop 的组合先持久化 latch，再合并故障；不重复发布 SerialRT 原语。
pub(crate) fn apply_emergency_batch(
    ctx: &mut IpcTickContext<'_>,
    batch: crate::event_ingress::EmergencyBatch,
) -> bool {
    apply_emergency_batch_observed(ctx, batch, |_| {})
}

pub(crate) fn apply_emergency_batch_observed(
    ctx: &mut IpcTickContext<'_>,
    batch: crate::event_ingress::EmergencyBatch,
    observe_latch: impl FnOnce(bool),
) -> bool {
    apply_emergency_batch_impl(ctx, batch, observe_latch, || {})
}

fn apply_emergency_batch_impl(
    ctx: &mut IpcTickContext<'_>,
    batch: crate::event_ingress::EmergencyBatch,
    observe_latch: impl FnOnce(bool),
    after_fault_ack: impl FnOnce(),
) -> bool {
    if !batch.estop && batch.ssf.is_none() {
        return false;
    }
    if batch.estop {
        ctx.guards.estop_latched = true;
        if let Some(path) = ctx.latch_path {
            let result = crate::estop_latch::save_estop_latch(path, true);
            observe_latch(result.is_ok());
            if let Err(error) = result {
                tracing::error!(?error, "EStop latch 持久化失败，内存急停继续保持");
            }
        }
        if ctx.serial_ctrl.is_none() {
            ctx.executor_ctrl.safety_stop();
        }
        if let Some(plan) = plan_transition(*ctx.state, Event::EStop, ctx.guards) {
            commit_transition(ctx.state, &plan);
            ctx.executor_ctrl.set_policy(plan.new_policy);
        }
        #[cfg(feature = "test-comm-fault-hook")]
        if let Some(ctrl) = ctx.serial_ctrl {
            ctrl.trace_now(crate::comm_fault_trace::TraceKind::IpcEStopCommit, 0, 0);
        }
    }
    if let Some(ssf) = batch.ssf {
        commit_fault(ctx.state, FaultCode::SafetyStopFailed);
        ctx.executor_ctrl.set_policy(OutputPolicy::Disabled);
        if let Some(ctrl) = ctx.serial_ctrl {
            ctrl.ack_fault(ssf.generation);
            after_fault_ack();
            #[cfg(feature = "test-comm-fault-hook")]
            trace_fault_commit(ctrl, ssf);
        }
    }
    ctx.retry.clear();
    true
}

/// 每 tick 恰一次，不能被 emergency、pending 或 lane 容量跳过。
pub(crate) fn service_fault_mailbox(ctx: &mut IpcTickContext<'_>) {
    service_fault_mailbox_impl(ctx, || {});
}

fn service_fault_mailbox_impl(ctx: &mut IpcTickContext<'_>, after_fault_ack: impl FnOnce()) {
    use std::sync::atomic::Ordering;

    let Some(ctrl) = ctx.serial_ctrl else { return };
    let Some(mailbox) = ctrl.mailbox_snapshot() else {
        return;
    };
    if mailbox.code == FaultCode::SafetyStopFailed
        || mailbox.generation == ctrl.fault_ack.load(Ordering::Acquire)
    {
        return;
    }
    commit_fault(ctx.state, mailbox.code);
    ctx.executor_ctrl.set_policy(OutputPolicy::Disabled);
    ctx.guards.adapter_connected = false;
    ctx.guards.adapter_fault_cleared = false;
    ctx.retry.clear();
    ctrl.ack_fault(mailbox.generation);
    after_fault_ack();
    #[cfg(feature = "test-comm-fault-hook")]
    trace_fault_commit(ctrl, mailbox);
    tracing::error!(generation = mailbox.generation, code = ?mailbox.code,
        detail = mailbox.detail, origin = ?mailbox.origin, "串口故障 mailbox 已对账");
}

#[cfg(feature = "test-comm-fault-hook")]
fn trace_fault_commit(
    ctrl: &crate::hard_rt_ctrl::HardRtCtrlInner,
    snapshot: crate::hard_rt_ctrl::MailboxSnapshot,
) {
    let mut context = ctrl.trace_context();
    // ACK 后 mailbox 可能已经发布下一代；归属只能来自本次实际提交的快照。
    context.generation = snapshot.generation;
    crate::comm_fault_trace::emit(
        crate::comm_fault_trace::TraceKind::FaultIpcCommit,
        crate::comm_fault_trace::now(),
        context,
        crate::comm_fault_trace::fault_code(snapshot.code),
        u64::from(snapshot.detail),
    );
}

fn commit_pending_failure(ctx: &mut IpcTickContext<'_>, fault: FaultCode) {
    commit_fault(ctx.state, fault);
    ctx.executor_ctrl.set_policy(OutputPolicy::Disabled);
    if ctx.serial_ctrl.is_some() {
        ctx.guards.adapter_connected = false;
        ctx.guards.adapter_fault_cleared = false;
    }
    ctx.retry.clear();
}

/// 正常动作退场的类型化结果，取消回执与真实 EStop notice 正交。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingAdvance {
    Ready,
    InFlight,
    CancelledByEmergency,
}

/// 完成或失败后本 tick 继续处理已排队的链路事件。
pub(crate) fn advance_pending(ctx: &mut IpcTickContext<'_>) -> PendingAdvance {
    use robot_core::executor_ctrl::AdapterErr;
    use robot_core::serial_services::NormalAbortTarget;

    let Some((plan, seq)) = ctx.retry.pending else {
        return PendingAdvance::Ready;
    };
    let elapsed = ctx
        .retry
        .pending_started_at
        .and_then(|start| ctx.clock.now() - start);
    if elapsed.is_some_and(|duration| duration > PENDING_TOTAL_TIMEOUT) {
        let fault = match ctx.serial_ctrl {
            Some(_) => FaultCode::CommFault,
            None => FaultCode::AdapterErr,
        };
        commit_pending_failure(ctx, fault);
        if ctx.serial_ctrl.is_some() {
            ctx.executor_ctrl.abort_pending_normal(match seq {
                Some(seq) => NormalAbortTarget::Submitted { seq },
                None => NormalAbortTarget::Unsubmitted,
            });
        }
        tracing::error!(?seq, action = ?plan.kind, elapsed_ns = elapsed.map_or(0, |duration| duration.0),
            "普通动作等待超时，已提交故障与 Disabled");
        return PendingAdvance::Ready;
    }
    match seq {
        None => {
            let TransitionKind::Effectful { action } = plan.kind else {
                commit_pending_failure(ctx, FaultCode::AdapterErr);
                return PendingAdvance::Ready;
            };
            match ctx.executor_ctrl.request_normal(action) {
                NormalReqStatus::Submitted { seq } => {
                    ctx.retry.pending = Some((plan, Some(seq)));
                    PendingAdvance::InFlight
                }
                NormalReqStatus::Busy => PendingAdvance::InFlight,
                NormalReqStatus::Unsupported(message) => {
                    tracing::error!(?action, message, "执行器拒绝普通动作能力");
                    commit_pending_failure(ctx, FaultCode::AdapterErr);
                    PendingAdvance::Ready
                }
            }
        }
        Some(seq) => match ctx.executor_ctrl.poll_last_normal_resp(seq) {
            None => PendingAdvance::InFlight,
            Some(AdapterResp::Err(AdapterErr::PreemptedByEmergency))
                if ctx.serial_ctrl.is_some() =>
            {
                // 取消 tag 可以先于权威急停 CAS 被消费；此返回自身不创造 Fault。
                ctx.retry.clear();
                PendingAdvance::CancelledByEmergency
            }
            Some(AdapterResp::Ok) => {
                commit_transition(ctx.state, &plan);
                ctx.executor_ctrl.set_policy(plan.new_policy);
                ctx.retry.clear();
                PendingAdvance::Ready
            }
            Some(AdapterResp::Err(error)) => {
                let fault = match (ctx.serial_ctrl, error) {
                    (Some(_), AdapterErr::Comm(_) | AdapterErr::AbortedByTimeout) => {
                        FaultCode::CommFault
                    }
                    _ => FaultCode::AdapterErr,
                };
                tracing::error!(seq, action = ?plan.kind, ?error,
                    elapsed_ns = elapsed.map_or(0, |duration| duration.0), "普通动作返回失败");
                commit_pending_failure(ctx, fault);
                PendingAdvance::Ready
            }
        },
    }
}

/// 应用一个已经出队且通过 deadline 的事件，不负责读取下一事件。
pub(crate) fn apply_control_event(ctx: &mut IpcTickContext<'_>, event: Event) -> TickOutcome {
    let mut outcome = TickOutcome::default();
    apply_event_guard_side_effect(event, ctx.guards);
    let Some(plan) = plan_transition(*ctx.state, event, ctx.guards) else {
        if event == Event::ClearEStop {
            outcome.clear_estop = Some(ClearEStopTickOutcome::StateNotSafe { actual: *ctx.state });
        }
        return outcome;
    };
    match plan.kind {
        TransitionKind::Pure if event == Event::ClearEStop => {
            let Some(path) = ctx.latch_path else {
                return outcome;
            };
            match clear_estop_action::try_apply_clear_estop(
                ctx.guards,
                ctx.executor_ctrl,
                path,
                ctx.clock,
            ) {
                Ok(result) => {
                    outcome.clear_estop = Some(ClearEStopTickOutcome::Cleared {
                        cleared_at_ns: result.cleared_at_ns,
                    });
                    commit_transition(ctx.state, &plan);
                    ctx.executor_ctrl.set_policy(plan.new_policy);
                }
                Err(clear_estop_action::ClearEStopError::FsyncFailed(_)) => {
                    outcome.clear_estop = Some(ClearEStopTickOutcome::FsyncFailed);
                }
                Err(clear_estop_action::ClearEStopError::NotTorqueOffConfirmed { actual }) => {
                    outcome.clear_estop =
                        Some(ClearEStopTickOutcome::NotTorqueOffConfirmed { actual });
                }
            }
        }
        TransitionKind::Pure => {
            commit_transition(ctx.state, &plan);
            ctx.executor_ctrl.set_policy(plan.new_policy);
        }
        TransitionKind::Emergency => {
            if ctx.serial_ctrl.is_none() && event == Event::EStop {
                ctx.executor_ctrl.safety_stop();
            }
            commit_transition(ctx.state, &plan);
            ctx.executor_ctrl.set_policy(plan.new_policy);
        }
        TransitionKind::Effectful { .. } => {
            ctx.retry.pending = Some((plan, None));
            ctx.retry.pending_started_at = Some(ctx.clock.now());
        }
    }
    outcome
}

/// 旧状态机测试入口委托同一分步实现；完整线程调度还包含 RR 与 source 配额。
pub fn ipc_tick(
    retry: &mut IpcRetryState,
    state: &mut S0State,
    guards: &mut Guards,
    executor_ctrl: &dyn ExecutorCtrlClient,
    clock: &dyn RobotClock,
    events: &mut EventQueue,
    latch_path: Option<&Path>,
) -> TickOutcome {
    use crate::event_ingress::{CONTROL_QUOTA, EmergencyBatch, LINK_QUOTA};
    use crate::hard_rt_ctrl::{FaultOrigin, MailboxSnapshot};

    let mut ctx = IpcTickContext {
        retry,
        state,
        guards,
        executor_ctrl,
        serial_ctrl: None,
        clock,
        latch_path,
    };
    let batch = EmergencyBatch {
        estop: events.claim_estop(),
        ssf: events.claim_legacy_ssf().then_some(MailboxSnapshot {
            generation: 0,
            code: FaultCode::SafetyStopFailed,
            detail: 0,
            origin: FaultOrigin::InvariantViolation,
        }),
    };
    let handled = apply_emergency_batch(&mut ctx, batch);
    service_fault_mailbox(&mut ctx);
    if handled || advance_pending(&mut ctx) == PendingAdvance::InFlight {
        return TickOutcome::default();
    }
    let mut outcome = TickOutcome::default();
    for _ in 0..LINK_QUOTA {
        let Some(event) = events.pop_link() else {
            break;
        };
        let _ = apply_control_event(&mut ctx, event.into());
        if ctx.retry.pending.is_some() {
            return outcome;
        }
    }
    for _ in 0..CONTROL_QUOTA {
        let Some(event) = events.pop_control() else {
            break;
        };
        let current = apply_control_event(&mut ctx, event.into());
        if current.clear_estop.is_some() {
            outcome = current;
        }
        if ctx.retry.pending.is_some() {
            break;
        }
    }
    outcome
}

/// `ipc_tick` 单次执行的副作用 outcome。
///
/// **A1-3 (1/3a)**：撤掉 A1-2 期间的两个 bool（`clear_estop_latch_persisted`
/// / `clear_estop_latch_write_failed`），换成一个 status enum
/// [`ClearEStopTickOutcome`]，让 `ipc_thread` 能区分 `Cleared` /
/// `StateNotSafe` / `NotTorqueOffConfirmed` / `FsyncFailed` 四个 variant
/// 生成完整的 RR response payload。reviewer 拍板：S0 返回的 domain status
/// **不**直接 cast 成 tonic code，由 gateway 显式 match。
///
/// `clear_estop` 字段：
///
/// - `None`：本 tick 没消费 `Event::ClearEStop`（典型的 EStop / Heartbeat /
///   Effectful pending tick）。
/// - `Some(ClearEStopTickOutcome::Cleared { cleared_at_ns })`：三步硬顺序
///   全过；`ipc_thread` 应计 `latch_writes++` + 同步 `prev_estop_latched=
///   false` + **跳过**通用 `prev != now` hook 的二次 `save_estop_latch`。
/// - `Some(ClearEStopTickOutcome::FsyncFailed)`：gate 通过但 fsync 失败；
///   guard / executor / latch 全不动；`ipc_thread` 应计 `latch_write_errors++`。
/// - `Some(ClearEStopTickOutcome::StateNotSafe { actual })`：state 不在
///   `Safe`，`plan_transition` 返 `None`；guard / executor / latch 全不动；
///   `ipc_thread` 不计数（`prev_estop_latched` 也不动）。
/// - `Some(ClearEStopTickOutcome::NotTorqueOffConfirmed { actual })`：
///   gate 拒；guard / executor / latch 全不动；`ipc_thread` 不计数。
///
/// **dry_run 不进 `ipc_tick`**，因此 [`ClearEStopTickOutcome`] **不**含
/// `WouldClear` variant；dry_run 由 `ipc_thread` 在 RR drain 阶段直接调
/// [`clear_estop_action::evaluate_clear_estop_dry_run`] 同步评估，read-only
/// 检查不触动 state machine（详 reviewer A1-3 follow-up 2 拍板）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TickOutcome {
    /// 本 tick 是否消费了 `Event::ClearEStop`，及消费结果。详上方 doc。
    pub clear_estop: Option<ClearEStopTickOutcome>,
}

/// 本 tick 消费 `Event::ClearEStop` 时的 domain status。
///
/// 设计原则（reviewer A1-3 follow-up 1 拍板）：
///
/// - **不**承担 RPC 语义。`ipc_thread` / gateway 显式 match 把它转成 RR
///   response / gRPC `tonic::Status`。
/// - 必须 `Copy`（给 `TickOutcome::default()` 提供 trivial 构造）。`io::Error`
///   不进 enum；fsync 失败的具体 IO error 走 `tracing` 落 journal，本枚举
///   只标 `FsyncFailed` 让上层知道是哪类。
/// - 不含 `WouldClear` —— dry_run 不进 `ipc_tick`，由 ipc_thread 调
///   [`clear_estop_action::evaluate_clear_estop_dry_run`] 评估。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClearEStopTickOutcome {
    /// 三步硬顺序全过：fsync(primary+bak) → clear_safety_latch() → 翻 guard。
    /// `cleared_at_ns` = action handler 完成瞬间的 `clock.now()` mono raw 时间戳。
    Cleared { cleared_at_ns: u64 },

    /// `*state != S0State::Safe`，`plan_transition` 返 `None`，本 tick 直接
    /// continue；guard / executor / latch 全不动。
    StateNotSafe { actual: S0State },

    /// state == Safe，`plan_transition` 通过，但 `executor_ctrl.safety_state()
    /// != TorqueOffConfirmed`，`try_apply_clear_estop` 在 gate 拒；
    /// guard / executor / latch 全不动。
    NotTorqueOffConfirmed { actual: OutputSafetyState },

    /// gate 通过，`save_estop_latch_clear` fsync 失败；`executor_ctrl.
    /// clear_safety_latch()` 未调、guard 仍 true、磁盘 latch 仍 latched=true
    /// （`save_estop_latch_clear` backup-first 保证）。具体 `io::Error`
    /// 走 tracing 落 journal，本 variant 不携带 IO error 细节。
    FsyncFailed,
}

/// 按事件类型对 `guards` 做已知副作用。**ipc_tick 内部**逐 event 调用
/// （第 5 条门槛修订）。
///
/// 历史包袱：原本由 `ipc_thread::run_loop` 在 channel drain 阶段批量调用，
/// 那个语义在多事件批量场景下会让"后到事件的 guard 更新被前到事件的
/// plan_transition 看到"。本函数被移到 ipc_tick 之后，每个 event 的
/// plan_transition 只看到自己的 guard 副作用。
///
/// 本函数**仅**处理"事件自身明确指定"的 guard 字段；其它 guard
/// （`adapter_connected` / `power_ok` 等）由 caller 在启动 / 错误恢复路径
/// 单独维护。
pub(crate) fn apply_event_guard_side_effect(ev: Event, guards: &mut Guards) {
    match ev {
        Event::EStop => guards.estop_latched = true,
        // **A1-2**：`Event::ClearEStop` 移出本批量 apply；它的 guard 副作用
        // 由 [`clear_estop_action::try_apply_clear_estop`] 在 `ipc_tick` 内
        // plan_transition 之后通过 fsync + `clear_safety_latch()` + 翻 guard
        // 的三步硬顺序完成（详 `docs/00_design.md` §6.6.1 errata）。
        Event::Heartbeat => guards.heartbeat_ok = true,
        Event::HeartbeatTimeout => guards.heartbeat_ok = false,
        _ => {}
    }
}

// ============================================================================
// Guards 重新导出（避免 ipc_tick 模块外部调用方 import 两次）
// ============================================================================

pub use crate::state_machine::Guards;

// ============================================================================
// Tests —— v9 §0.5 三个红灯门槛
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use robot_core::executor_ctrl::{
        AdapterErr, AdapterNormalReq, ExecutorCtrlClient, NormalReqStatus, OutputSafetyState,
        emergency_codes, normal_codes, pack_completed,
    };
    use robot_core::time::MockClock;
    use tempfile::tempdir;

    use crate::estop_latch::save_estop_latch;
    use crate::hard_rt_ctrl::{HardRtCtrlClient, HardRtCtrlInner};
    use crate::state_machine::{ControlLink, EnabledSub, Event, OutputMode};

    /// 构造一组：
    /// - `ctrl` 共享 inner
    /// - `client` 实现 `ExecutorCtrlClient`
    /// - `clock` MockClock 起点 0
    /// - `events` 空队列
    /// - `retry` 空状态
    /// - `guards` 全绿（healthy）
    ///
    /// **state 由测试自己初始化**（避免 mut shadow + reassign 的 dead-code 警告）。
    #[allow(clippy::type_complexity)]
    fn fixtures() -> (
        Arc<HardRtCtrlInner>,
        HardRtCtrlClient,
        Arc<MockClock>,
        EventQueue,
        IpcRetryState,
        Guards,
    ) {
        let inner = Arc::new(HardRtCtrlInner::new());
        let client = HardRtCtrlClient::new(Arc::clone(&inner));
        let clock = Arc::new(MockClock::new(MonoRawNs::ZERO));
        let events = EventQueue::new();
        let retry = IpcRetryState::new();
        let guards = healthy_guards();
        (inner, client, clock, events, retry, guards)
    }

    fn healthy_guards() -> Guards {
        Guards {
            rt_serial: false,
            estop_latched: false,
            adapter_connected: true,
            calibration_loaded: true,
            joint_state_valid: true,
            power_ok: true,
            heartbeat_ok: true,
            cmd_deadline_valid: true,
            limits_valid: true,
            hw_enable_ok: true,
            adapter_fault_cleared: true,
            command_mode: robot_core::adapter::ControlMode::Position,
            on_timeout_hint: EnabledSub::CtrlLostHold,
        }
    }

    #[cfg(feature = "test-comm-fault-hook")]
    #[test]
    fn fault_trace_keeps_acked_generation_when_next_fault_is_published() {
        verify_fault_trace_generation(false);
    }

    #[cfg(feature = "test-comm-fault-hook")]
    #[test]
    fn ssf_commit_emits_its_actual_generation_before_next_mailbox() {
        verify_fault_trace_generation(true);
    }

    #[cfg(feature = "test-comm-fault-hook")]
    fn verify_fault_trace_generation(ssf: bool) {
        use crate::comm_fault_trace::{TraceKind, capture_emitted};
        use crate::event_ingress::EmergencyBatch;
        use crate::hard_rt_ctrl::{FaultOrigin, inhibit};

        let inner = Arc::new(HardRtCtrlInner::new_serial());
        let client = HardRtCtrlClient::new(inner.clone());
        let clock = MockClock::new(MonoRawNs::ZERO);
        let mut retry = IpcRetryState::new();
        let mut state = S0State::Ready;
        let mut guards = healthy_guards();
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
        let snapshot = inner.mailbox_snapshot().unwrap();
        let mut ctx = IpcTickContext {
            retry: &mut retry,
            state: &mut state,
            guards: &mut guards,
            executor_ctrl: &client,
            serial_ctrl: Some(&inner),
            clock: &clock,
            latch_path: None,
        };
        let publish_after_ack = || {
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
        };
        let events = capture_emitted(|| {
            if ssf {
                assert!(apply_emergency_batch_impl(
                    &mut ctx,
                    EmergencyBatch {
                        estop: false,
                        ssf: Some(snapshot)
                    },
                    |_| {},
                    publish_after_ack
                ));
            } else {
                service_fault_mailbox_impl(&mut ctx, publish_after_ack);
            }
        });
        let commits: Vec<_> = events
            .iter()
            .filter(|event| event.kind == TraceKind::FaultIpcCommit)
            .collect();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].generation, generation);
        assert_eq!(commits[0].a, if ssf { 6 } else { 7 });
        assert_eq!(commits[0].b, 17);
        assert_eq!(state, S0State::Fault(code));
        assert_eq!(inner.mailbox_snapshot().unwrap().generation, generation + 1);
    }

    fn enabled_active() -> S0State {
        S0State::Enabled {
            sub: EnabledSub::Active,
            control_link: ControlLink::Ok,
            output_mode: OutputMode::Active,
        }
    }

    // ========================================================================
    // §0.5 A1 红灯门槛 #1：test_estop_preempts_in_flight_pending
    // ========================================================================

    /// emergency event 在 in-flight pending 卡住时注入 → **同 tick** 内
    /// commit Emergency（state=Safe，policy=StopTorque，latch=true），pending
    /// 被强制清空；后续即使 rt_thread ack 也不影响 state。
    #[test]
    fn test_estop_preempts_in_flight_pending() {
        let (inner, client, clock, mut events, mut retry, mut guards) = fixtures();

        // 1) 让 state 处于 Ready，模拟"准备 effectful Enable(true)"
        let mut state = S0State::Ready;
        // 2) 排 effectful transition（Ready + Heartbeat → Enabled.Active 是 Effectful+Enable(true)）
        events.push(Event::Heartbeat);
        // tick 1：Step 3 plan_transition → 入 pending(None) + 起 timer + break
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            None,
        );
        assert!(
            matches!(retry.pending, Some((_, None))),
            "tick 1 应入 pending(None)；retry={:?}",
            retry.pending
        );
        // tick 2：Step 2 None 分支 → request_normal → Submitted{seq}
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            None,
        );
        assert!(
            matches!(retry.pending, Some((_, Some(_)))),
            "tick 2 应已 Submitted；retry={:?}",
            retry.pending
        );
        // state 仍是 Ready（effectful 不 commit 直到 ack）
        assert_eq!(state, S0State::Ready);

        // 3) 注入 EStop
        events.push(Event::EStop);
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            None,
        );

        // 4) 同 tick 断言：
        // - state 进 Safe
        assert_eq!(state, S0State::Safe);
        // - policy = StopTorque
        assert_eq!(
            OutputPolicy::from_u8(inner.policy.load(Ordering::Acquire)),
            OutputPolicy::StopTorque
        );
        // - safety_latched = true（client.safety_stop() 同步置）
        assert!(inner.safety_latched.load(Ordering::Acquire));
        // - emergency_action = SAFETY_STOP（client.safety_stop() 设；rt 端会消费）
        assert_eq!(
            inner.emergency_action.load(Ordering::Acquire),
            emergency_codes::SAFETY_STOP
        );
        // - pending 被清空
        assert!(retry.pending.is_none(), "pending should be cleared");
        assert!(retry.pending_started_at.is_none());

        // 5) 模拟 rt_thread 后续 ack（即使 ack Ok 也不应影响 state）
        // 我们这里直接把 last_completed 写成 (seq=99, Ok)，模拟 rt_thread
        // ack 来晚的场景；ipc_tick 后续 tick 不应把 state 拉回 Enabled。
        inner
            .last_completed
            .store(pack_completed(99, AdapterResp::Ok), Ordering::Release);
        // 再跑一 tick
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            None,
        );
        assert_eq!(state, S0State::Safe, "晚到的 ack 不应让 state 离开 Safe");
    }

    // ========================================================================
    // §0.5 A2 红灯门槛 #2：test_busy_total_timeout_1s
    // ========================================================================

    /// rt_thread 始终返回 Busy（slot 占用从未空出），1s 后必须 commit Fault。
    /// 入 pending 后 ≤ 1.05s 内 state == Fault(AdapterErr)，policy == Disabled。
    /// **不**允许永久卡 Busy。
    #[test]
    fn test_busy_total_timeout_1s() {
        let (inner, client, clock, mut events, mut retry, mut guards) = fixtures();

        // 1) 准备一个永远 Busy 的状态：手动占住 packed slot（非零）
        // pack_pending_normal(action, arg, seq) 设一个 dummy 值
        let dummy_packed = robot_core::executor_ctrl::pack_pending_normal(
            normal_codes::ENABLE,
            1,
            7777, // dummy seq，rt 永远不消费
        );
        inner.pending_normal.store(dummy_packed, Ordering::Release);

        // 2) state = Ready，排 Heartbeat → effectful Enable(true)
        let mut state = S0State::Ready;
        events.push(Event::Heartbeat);

        // 3) 第一个 tick：Step 3 plan_transition 入 pending（None seq），
        //    第二个 tick：Step 2 走 None 分支，request_normal 返 Busy（slot 已占）
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            None,
        );
        assert!(matches!(retry.pending, Some((_, None))));
        let started = retry.pending_started_at.expect("A2: 入队即置 started_at");

        // 4) 反复 tick + 推进 MockClock 直到 1.1s
        let mut elapsed_ms: u64 = 0;
        loop {
            ipc_tick(
                &mut retry,
                &mut state,
                &mut guards,
                &client,
                clock.as_ref(),
                &mut events,
                None,
            );
            // 检查 fault 升级
            if matches!(state, S0State::Fault(FaultCode::AdapterErr)) {
                break;
            }
            // 推进 50ms
            clock.advance(DurationNs::from_millis(50));
            elapsed_ms += 50;
            // **断言**：1.05s 内必须升 Fault（v9 §0.5 A2 约定）
            assert!(
                elapsed_ms <= 1100,
                "1s 总超时未触发，已过 {elapsed_ms} ms 仍卡 Busy"
            );
        }

        // 5) 断言 state == Fault(AdapterErr) + policy == Disabled
        assert_eq!(
            state,
            S0State::Fault(FaultCode::AdapterErr),
            "Busy 1s 超时应升 Fault(AdapterErr)，实际 {state:?}"
        );
        assert_eq!(
            OutputPolicy::from_u8(inner.policy.load(Ordering::Acquire)),
            OutputPolicy::Disabled
        );
        // pending 清空
        assert!(retry.pending.is_none());
        assert!(retry.pending_started_at.is_none());
        // 实际超时时刻（>1s）
        let final_now = clock.now();
        let delta = (final_now - started).expect("now >= started");
        assert!(
            delta > DurationNs::from_secs(1),
            "delta should exceed 1s, was {delta:?}"
        );
    }

    // ========================================================================
    // §0.5 A3 红灯门槛 #3：test_effectful_failure_direct_fault
    // ========================================================================

    /// effectful ack 返 `AdapterResp::Err(SafetyLatched)` → 当前 tick 内
    /// commit_fault 直接升 Fault(AdapterErr)（**finding #3 修订**：
    /// SafetyLatched 不映射 SafetyStopFailed），policy = Disabled。
    /// **不**经过任何 `Event::AdapterErr` 入队（events queue 仍空）。
    #[test]
    fn test_effectful_failure_direct_fault() {
        let (inner, client, clock, mut events, mut retry, mut guards) = fixtures();

        // 1) state = Ready，Heartbeat → effectful Enable(true)
        let mut state = S0State::Ready;
        events.push(Event::Heartbeat);

        // tick 1：Step 3 入 pending (None)
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            None,
        );
        // tick 2：Step 2 None 分支 → request_normal Submitted{seq}
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            None,
        );
        let Some((_, Some(pending_seq))) = retry.pending else {
            panic!(
                "expected Some((_, Some(seq))) after submit, got {:?}",
                retry.pending
            )
        };
        // events 应已被消费空（Heartbeat 进 plan_transition 后被入 pending）
        assert!(events.is_empty());

        // 2) 模拟 rt_thread ack 返 Err(SafetyLatched)
        inner.last_completed.store(
            pack_completed(pending_seq, AdapterResp::Err(AdapterErr::SafetyLatched)),
            Ordering::Release,
        );

        // 3) tick 3：Step 2 Some(seq) 分支 → poll 拿到 Err → commit_fault 直接升 Fault
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            None,
        );

        // 4) 断言
        assert_eq!(
            state,
            S0State::Fault(FaultCode::AdapterErr),
            "SafetyLatched 必须映射 FaultCode::AdapterErr（finding #3），实际 {state:?}"
        );
        assert_eq!(
            OutputPolicy::from_u8(inner.policy.load(Ordering::Acquire)),
            OutputPolicy::Disabled
        );
        // **A3 关键**：events queue 仍空（**没**走 Event::AdapterErr 入队 + 下 tick plan）
        assert!(
            events.is_empty(),
            "A3: ack 失败不应入 Event::AdapterErr 队，events={events:?}"
        );
        // pending 清空
        assert!(retry.pending.is_none());
    }

    // ========================================================================
    // 辅助 lock 测试：Pure / Emergency / Effectful 各路径正确性
    // ========================================================================

    /// Pure transition 立即 commit + set_policy，不入 pending。
    #[test]
    fn pure_transition_commits_immediately() {
        let (inner, client, clock, mut events, mut retry, mut guards) = fixtures();
        let mut state = S0State::Safe;
        // SAFE → READY 是 Pure（守卫满足）
        events.push(Event::Enable);
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            None,
        );
        assert_eq!(state, S0State::Ready);
        assert_eq!(
            OutputPolicy::from_u8(inner.policy.load(Ordering::Acquire)),
            OutputPolicy::ReadyHold
        );
        assert!(retry.pending.is_none());
    }

    /// Effectful transition：Submitted → 下 tick poll Ok → commit。
    #[test]
    fn effectful_transition_commits_after_ack() {
        let (inner, client, clock, mut events, mut retry, mut guards) = fixtures();
        let mut state = S0State::Ready;
        events.push(Event::Heartbeat);
        // tick 1: Step 3 入 pending (None)
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            None,
        );
        assert_eq!(state, S0State::Ready, "Ready 在 ack 前不应离开");
        // tick 2: Step 2 None 分支 → Submitted
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            None,
        );
        let Some((_, Some(seq))) = retry.pending else {
            panic!("pending should be Submitted")
        };
        // ack Ok
        inner
            .last_completed
            .store(pack_completed(seq, AdapterResp::Ok), Ordering::Release);
        // tick 3: Step 2 Some(seq) → Ok → commit
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            None,
        );
        assert_eq!(state, enabled_active(), "ack Ok 后 state 进 Enabled.Active");
        assert_eq!(
            OutputPolicy::from_u8(inner.policy.load(Ordering::Acquire)),
            OutputPolicy::ActiveForward
        );
        assert!(retry.pending.is_none());
    }

    /// `Unsupported` 在 effectful 路径必须直接 commit_fault（A3）。
    #[test]
    fn effectful_unsupported_direct_fault() {
        // 用一个总返 Unsupported 的 client mock
        struct UnsupportedClient(Arc<HardRtCtrlInner>);
        impl ExecutorCtrlClient for UnsupportedClient {
            fn set_policy(&self, p: OutputPolicy) {
                self.0.policy.store(p.to_u8(), Ordering::Release);
            }
            fn safety_stop(&self) {
                self.0
                    .emergency_action
                    .store(emergency_codes::SAFETY_STOP, Ordering::Release);
            }
            fn request_normal(&self, _req: AdapterNormalReq) -> NormalReqStatus {
                NormalReqStatus::Unsupported("test: forced unsupported")
            }
            fn poll_last_normal_resp(&self, _seq: u64) -> Option<AdapterResp> {
                None
            }
            fn clear_safety_latch(&self) {
                self.0.safety_latched.store(false, Ordering::Release);
            }
            fn safety_state(&self) -> robot_core::executor_ctrl::OutputSafetyState {
                robot_core::executor_ctrl::OutputSafetyState::from_u8(
                    self.0.safety_state.load(Ordering::Acquire),
                )
            }
        }

        let inner = Arc::new(HardRtCtrlInner::new());
        let client = UnsupportedClient(Arc::clone(&inner));
        let clock = Arc::new(MockClock::new(MonoRawNs::ZERO));
        let mut events = EventQueue::new();
        let mut retry = IpcRetryState::new();
        let mut state = S0State::Ready;
        let mut guards = healthy_guards();

        events.push(Event::Heartbeat);
        // tick 1: 入 pending
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            None,
        );
        // tick 2: request_normal Unsupported → commit_fault
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            None,
        );
        assert_eq!(state, S0State::Fault(FaultCode::AdapterErr));
        assert_eq!(
            OutputPolicy::from_u8(inner.policy.load(Ordering::Acquire)),
            OutputPolicy::Disabled
        );
        assert!(retry.pending.is_none());
    }

    /// **finding（阶段 3 收尾）lock + A1-2 升级**：EStop 后端到端恢复链路。
    ///
    /// 1) 触发 EStop → safety_latched=true，state=Safe，guards.estop_latched=true
    /// 2) 模拟 ipc_thread 把 latch=true 落盘 + rt/serial 端 confirm torque off →
    ///    safety_state=TorqueOffConfirmed
    /// 3) 排 `Event::ClearEStop` → ipc_tick 调
    ///    `clear_estop_action::try_apply_clear_estop` 走三步硬顺序：
    ///    fsync(false) → executor_ctrl.clear_safety_latch() → guards.estop_latched=false
    /// 4) 排 `Event::Enable` → SAFE → READY（Pure transition）
    /// 5) 排 `Event::Heartbeat` → READY → ENABLED.Active 是 Effectful Enable(true)
    ///    —— 后续 tick 走 Step 2 None 分支，client.request_normal **不**被
    ///    latch 拒（client 端 latch 已清），返 Submitted
    ///
    /// 如果 ipc_tick **不**调 clear_safety_latch，第 5 步 client 端就会拒
    /// （NormalReqStatus::Unsupported），ipc_tick 再走 A3 升 Fault(AdapterErr)，
    /// 测试 fail。
    #[test]
    #[allow(clippy::too_many_lines)] // A1-3 (1/3a) 加 outcome 断言后跨过 100 行门槛
    fn clear_estop_unblocks_subsequent_enable_via_latch() {
        let (inner, client, clock, mut events, mut retry, mut guards) = fixtures();
        let mut state = enabled_active();

        // **A1-2**：ClearEStop 走 try_apply_clear_estop，必须有真实 latch_path。
        let tmp = tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");

        // 1) EStop
        events.push(Event::EStop);
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            Some(&latch_path),
        );
        assert_eq!(state, S0State::Safe);
        assert!(
            inner.safety_latched.load(Ordering::Acquire),
            "EStop 后 safety_latched 必须为 true"
        );
        assert!(
            guards.estop_latched,
            "EStop 走 apply_event_guard_side_effect 翻 guard"
        );

        // 2) 模拟 ipc_thread 通用 hook 把 latch=true 落盘 + rt/serial 端
        //    confirm torque off → safety_state=TorqueOffConfirmed。
        //
        // **A1-2**：guards.estop_latched 由 try_apply_clear_estop 内部翻转，
        // 不再由外部测试方手动清；这里只 provision latch=true。
        save_estop_latch(&latch_path, true).unwrap();
        inner.safety_state.store(
            OutputSafetyState::TorqueOffConfirmed.to_u8(),
            Ordering::Release,
        );

        // 3) ClearEStop → A1-2 走 try_apply_clear_estop 三步硬顺序
        events.push(Event::ClearEStop);
        let outcome = ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            Some(&latch_path),
        );
        assert!(
            matches!(
                outcome.clear_estop,
                Some(ClearEStopTickOutcome::Cleared { .. })
            ),
            "outcome 应抛 ClearEStopTickOutcome::Cleared 让 ipc_thread 防二次写；\
             实际 outcome.clear_estop = {:?}",
            outcome.clear_estop
        );
        assert!(
            !inner.safety_latched.load(Ordering::Acquire),
            "ClearEStop transition 后 executor safety_latched 必须为 false"
        );
        assert!(!guards.estop_latched, "guard 必须翻为 false");
        assert_eq!(state, S0State::Safe);
        assert_eq!(
            std::fs::read_to_string(&latch_path).unwrap(),
            "0\n",
            "fsync 必须把 latch 改成 0\\n"
        );

        // 4) Enable → SAFE → READY（Pure transition）
        events.push(Event::Enable);
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            Some(&latch_path),
        );
        assert_eq!(state, S0State::Ready);

        // 5) Heartbeat → READY → ENABLED.Active 是 Effectful Enable(true)
        events.push(Event::Heartbeat);
        // tick 1: 入 pending(None)
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            Some(&latch_path),
        );
        // tick 2: Step 2 None 分支调 request_normal —— 应**不**被 latch 拒
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            Some(&latch_path),
        );
        // **关键断言**：pending 应进 Submitted，**不**被 latch 拒升 Fault
        assert!(
            matches!(retry.pending, Some((_, Some(_)))),
            "ClearEStop 后 Enable(true) 应能 Submitted（client latch 已清），\
             实际 retry.pending = {:?}, state = {:?}",
            retry.pending,
            state
        );
        assert_ne!(
            state,
            S0State::Fault(FaultCode::AdapterErr),
            "Enable(true) 不应被 latch 拒升 Fault；ClearEStop -> clear_safety_latch \
             链路必须接通"
        );
    }

    /// **第 5 条门槛锁测（SerialRT E2E）+ A1-2 加固**：同 tick 内
    /// `[Enable, ClearEStop]` 顺序进入 EventQueue（先 Enable 后 ClearEStop），
    /// Enable 必须**看不到** ClearEStop 提前清的 `estop_latched=false` ——
    /// 也就是 Enable 的 `plan_transition` 看到 `estop_latched=true` 被拒绝。
    ///
    /// 旧实现（apply_event_guard_side_effect 在 ipc_thread::run_loop
    /// channel drain 阶段批量调用）会让 ClearEStop 提前改 guards.estop_latched
    /// =false，Enable 紧接其后 plan 时看到 false 就误通过 → state=Ready。
    /// 修订后：apply 移到 ipc_tick 内逐 event 处理时再调，Enable 看到的是
    /// 旧 guards（estop_latched=true），plan_transition 拒绝。
    ///
    /// **A1-2** 进一步加固：`Event::ClearEStop` 完全移出
    /// `apply_event_guard_side_effect`，guard 副作用只在
    /// `clear_estop_action::try_apply_clear_estop` 末尾（fsync +
    /// `clear_safety_latch` 都成功后）才翻。同 tick 内 Enable 永远看不到
    /// 提前清的 `estop_latched=false`。
    ///
    /// 这条不变量未来如果有人改回 batch update guards 模式 / 或者把 guard
    /// 翻转挪回 `plan_transition` 之前，会被本测试钉死。
    #[test]
    fn events_in_same_tick_use_per_event_guard_view() {
        let (inner, client, clock, mut events, mut retry, mut guards) = fixtures();
        // 初始 state=Safe，guards.estop_latched=true（模拟 EStop 已落 latch）。
        // safety_state 模拟 SerialRT 已 confirm torque off（让 ClearEStop 守门
        // 通过，能真正调 clear_safety_latch；本测试核心是验"per-event guard
        // view"，端到端走通 latch 清也算 bonus）。
        let mut state = S0State::Safe;
        guards.estop_latched = true;
        inner.safety_state.store(
            OutputSafetyState::TorqueOffConfirmed.to_u8(),
            Ordering::Release,
        );

        // **A1-2**：ClearEStop 走 try_apply_clear_estop，必须有真实 latch_path
        // + 预 provision latch=true。
        let tmp = tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");
        save_estop_latch(&latch_path, true).unwrap();

        // 同 tick 把 Enable 与 ClearEStop 顺序入队。注意发送顺序：先 Enable
        // 后 ClearEStop —— 这是测试的关键场景。
        events.push(Event::Enable);
        events.push(Event::ClearEStop);

        let outcome = ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            Some(&latch_path),
        );

        // **关键断言**：state 必须留在 Safe（Enable 因 estop_latched=true 被拒，
        // ClearEStop 处理后只是 SAFE→SAFE）。**不**得变 Ready。
        assert_eq!(
            state,
            S0State::Safe,
            "Enable 不应因 ClearEStop 提前清 estop_latched 而误通过；\
             实际 state={state:?}"
        );
        // ClearEStop 走 try_apply_clear_estop 在末尾翻 guard。
        assert!(
            !guards.estop_latched,
            "ClearEStop 处理后 guards.estop_latched 应清"
        );
        // outcome 抛 Cleared（fsync 三步全过）。
        assert!(
            matches!(
                outcome.clear_estop,
                Some(ClearEStopTickOutcome::Cleared { .. })
            ),
            "ClearEStop 成功路径必须抛 ClearEStopTickOutcome::Cleared；\
             实际 outcome.clear_estop = {:?}",
            outcome.clear_estop
        );
        // ClearEStop 走 try_apply_clear_estop 调 clear_safety_latch（即使初始
        // latch=false 也是无副作用 idempotent store）。这里 inner 默认初值
        // latch=false，本条只确认 store 被调到。
        assert!(!inner.safety_latched.load(Ordering::Acquire));
    }

    /// **A1-2 旁路防护**：state != Safe 时收到 `Event::ClearEStop`，
    /// `plan_transition` 在 (Ready/Enabled/Fault, ClearEStop) 上没有表项，
    /// 直接返 `None`（continue）。`clear_estop_action::try_apply_clear_estop`
    /// **绝不**得被调用：latch 文件不得被写（inode/mtime 不动）、executor
    /// `clear_safety_latch` 仍为 false（未被翻动）、guard / state 全不动。
    ///
    /// 这条单测挡的是"action-before-transition"旁路：未来若有人不小心把
    /// `try_apply_clear_estop` 提前到 `plan_transition` 之前调（错误地"先做
    /// 副作用再问合法性"），会被本测试钉死。
    ///
    /// reviewer A1-2 round 2 收紧补的第 4 条单测：走真实 ipc_tick 生产变体
    /// （带 latch writer / outcome）+ 真实 latch_path（tempdir，不预 provision），
    /// 比单纯锁 `plan_transition` 返 `None` 更精准——能挡住"误调用 action
    /// handler"这类 bug，因为 spy 一旦被调就会写文件。
    #[test]
    fn ipc_tick_clear_estop_blocked_when_state_not_safe() {
        let (inner, client, clock, mut events, mut retry, mut guards) = fixtures();

        // **关键设置**：state = Ready（非 Safe）；guards.estop_latched=true
        // 模拟"EStop 落 latch 但状态机没退回 Safe"的不一致中间态（生产中
        // 不会出现，但这是测试要锁的旁路场景）。
        let mut state = S0State::Ready;
        guards.estop_latched = true;
        // safety_state 设成 TorqueOffConfirmed —— 即使 gate 通过，action handler
        // 也不应该被调；本测试要锁的是"plan_transition 拒绝后整段 continue"，
        // 不依赖 gate 拒绝。
        inner.safety_state.store(
            OutputSafetyState::TorqueOffConfirmed.to_u8(),
            Ordering::Release,
        );

        // 真实 latch_path，但**不**预 provision —— 期望测试结束后文件仍不存在
        // （writer 根本没被调）。
        let tmp = tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");
        let bak_path = tmp.path().join("estop_latch.bak");
        assert!(!latch_path.exists(), "tempdir 起点 latch primary 不存在");
        assert!(!bak_path.exists(), "tempdir 起点 latch bak 不存在");

        events.push(Event::ClearEStop);

        let outcome = ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            Some(&latch_path),
        );

        // 1) action writer 完全没被调用（plan_transition reject 后 continue）
        assert!(!latch_path.exists(), "latch primary 文件不得被写");
        assert!(!bak_path.exists(), "latch bak 文件不得被写");

        // 2) executor 不动（clear_safety_latch 没被调；safety_latched 保持初值）
        assert!(
            !inner.safety_latched.load(Ordering::Acquire),
            "executor.safety_latched 保持初值（false），未被翻动"
        );

        // 3) state 不动（Ready 仍是 Ready）
        assert_eq!(state, S0State::Ready);

        // 4) guard 不动
        assert!(guards.estop_latched, "guards.estop_latched 仍为 true");

        // 5) outcome 抛 StateNotSafe（reviewer A1-3 (1/3a) 拍板：
        //    plan_transition 拒后必须把 domain status 抛给 ipc_thread / gateway，
        //    生成 RR FailedPrecondition response）
        assert_eq!(
            outcome.clear_estop,
            Some(ClearEStopTickOutcome::StateNotSafe {
                actual: S0State::Ready,
            }),
            "non-Safe state 必须抛 StateNotSafe domain status；\
             实际 outcome.clear_estop = {:?}",
            outcome.clear_estop
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // A1-3 (1/3a) ClearEStopTickOutcome 4 个 variant 锁测
    //
    // - Cleared: 已被 clear_estop_unblocks_subsequent_enable_via_latch +
    //   events_in_same_tick_use_per_event_guard_view 覆盖（live 成功）。
    // - StateNotSafe: 已被 ipc_tick_clear_estop_blocked_when_state_not_safe 覆盖。
    // - 本节补 NotTorqueOffConfirmed + FsyncFailed 两条独立锁测。
    // ────────────────────────────────────────────────────────────────────────

    /// **A1-3 (1/3a)**：state == Safe + safety_state != TorqueOffConfirmed →
    /// `try_apply_clear_estop` gate 拒；ipc_tick 抛
    /// `ClearEStopTickOutcome::NotTorqueOffConfirmed { actual }`，
    /// guard / executor / latch 全不动。
    #[test]
    fn ipc_tick_clear_estop_outcome_not_torque_off_confirmed_when_torque_on() {
        let (inner, client, clock, mut events, mut retry, mut guards) = fixtures();

        // state = Safe，但 safety_state = TorqueOn（电机仍 enabled）
        let mut state = S0State::Safe;
        guards.estop_latched = true;
        inner
            .safety_state
            .store(OutputSafetyState::TorqueOn.to_u8(), Ordering::Release);
        inner.safety_latched.store(true, Ordering::Release);

        // 真实 latch_path + 预 provision latched=true
        let tmp = tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");
        save_estop_latch(&latch_path, true).unwrap();

        events.push(Event::ClearEStop);

        let outcome = ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            Some(&latch_path),
        );

        assert_eq!(
            outcome.clear_estop,
            Some(ClearEStopTickOutcome::NotTorqueOffConfirmed {
                actual: OutputSafetyState::TorqueOn,
            }),
            "outcome 必须抛 NotTorqueOffConfirmed {{ actual: TorqueOn }}；\
             实际 outcome.clear_estop = {:?}",
            outcome.clear_estop
        );

        // 副作用全不动
        assert!(guards.estop_latched, "guard 不得翻");
        assert!(
            inner.safety_latched.load(Ordering::Acquire),
            "executor.safety_latched 不得清"
        );
        assert_eq!(
            std::fs::read_to_string(&latch_path).unwrap(),
            "1\n",
            "latch primary 文件不得被写"
        );
        assert_eq!(state, S0State::Safe);
    }

    /// **A1-3 (1/3a)**：gate 通过但 fsync 失败 → ipc_tick 抛
    /// `ClearEStopTickOutcome::FsyncFailed`；guard / executor 全不动；
    /// 磁盘 latch 仍 latched=true（backup-first 保证）。
    #[test]
    fn ipc_tick_clear_estop_outcome_fsync_failed_on_bad_backup_path() {
        let (inner, client, clock, mut events, mut retry, mut guards) = fixtures();

        // state = Safe + safety_state = TorqueOffConfirmed（gate 全过）
        let mut state = S0State::Safe;
        guards.estop_latched = true;
        inner.safety_state.store(
            OutputSafetyState::TorqueOffConfirmed.to_u8(),
            Ordering::Release,
        );
        inner.safety_latched.store(true, Ordering::Release);

        // 真实 latch_path + 预 provision latched=true，再把 bak 替换成
        // 非空目录让 backup-first save_estop_latch_clear 第一步失败
        let tmp = tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");
        let bak_path = tmp.path().join("estop_latch.bak");
        save_estop_latch(&latch_path, true).unwrap();
        std::fs::remove_file(&bak_path).unwrap();
        std::fs::create_dir(&bak_path).unwrap();
        std::fs::write(bak_path.join("nonempty"), b"x").unwrap();

        events.push(Event::ClearEStop);

        let outcome = ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            Some(&latch_path),
        );

        assert_eq!(
            outcome.clear_estop,
            Some(ClearEStopTickOutcome::FsyncFailed),
            "outcome 必须抛 FsyncFailed；实际 outcome.clear_estop = {:?}",
            outcome.clear_estop
        );

        // 副作用全不动（A1-2 round 2 backup-first 保证）
        assert!(guards.estop_latched, "guard 不得翻");
        assert!(
            inner.safety_latched.load(Ordering::Acquire),
            "executor.safety_latched 不得清（fsync 失败时绝不能调）"
        );
        assert_eq!(
            std::fs::read(&latch_path).unwrap(),
            b"1\n",
            "primary 必须保持 latched=true（backup-first 保证 partial failure 不动 primary）"
        );
        assert_eq!(state, S0State::Safe);
    }

    /// emergency event drain 时**没有** in-flight pending，state 仍正确进 Safe。
    #[test]
    fn emergency_without_pending_still_commits() {
        let (inner, client, clock, mut events, mut retry, mut guards) = fixtures();
        let mut state = enabled_active();
        events.push(Event::EStop);
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
            None,
        );
        assert_eq!(state, S0State::Safe);
        assert_eq!(
            OutputPolicy::from_u8(inner.policy.load(Ordering::Acquire)),
            OutputPolicy::StopTorque
        );
        assert!(inner.safety_latched.load(Ordering::Acquire));
        assert!(retry.pending.is_none());
    }
}
