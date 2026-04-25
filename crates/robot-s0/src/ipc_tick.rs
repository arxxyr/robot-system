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

use robot_core::executor_ctrl::{
    AdapterResp, ExecutorCtrlClient, NormalReqStatus, OutputPolicy, OutputSafetyState,
};
use robot_core::time::{DurationNs, MonoRawNs, RobotClock};

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

/// **task-30 v9 §4.4.3 errata 实施版**：单次 ipc tick 的三步式。
///
/// 三步顺序见模块 doc 表。返回 `()`；副作用全部通过 `state` / `retry` /
/// `executor_ctrl` / `guards` 各 store 落地。M7 阶段 4 把本函数嵌进
/// `run_ipc_loop` 主循环；当前阶段独立可测。
///
/// **task-30 v9 / SerialRT E2E 第 5 条门槛 修订**：`guards` 改 `&mut`。
/// `apply_event_guard_side_effect` 不再由 `run_ipc_loop` 在 channel drain
/// 阶段批量调用 —— 改为本函数内**每个 event 处理前**逐条 apply。这样
/// 同 tick 多事件场景下，**每个 event 的 plan_transition 看到的 guards
/// 是"该事件被处理那一刻的状态"**，不会被同 batch 后续事件提前污染（典型
/// 反例：`[Enable, ClearEStop]` 同 tick 入队，旧实现会让 Enable 因 ClearEStop
/// 提前清 estop_latched 而误通过；本修订让 Enable 看到 estop_latched=true →
/// 拒绝；ClearEStop 自己处理时再清 latch）。
///
/// 阶段 4 若需要"本 tick 是否做过动作 → 决定 idle sleep"，再加返回 bool（当前
/// 的 §0.5 三测红灯门槛不依赖此返回值）。
pub fn ipc_tick(
    retry: &mut IpcRetryState,
    state: &mut S0State,
    guards: &mut Guards,
    executor_ctrl: &dyn ExecutorCtrlClient,
    clock: &dyn RobotClock,
    events: &mut EventQueue,
) {
    // ─── Step 1: emergency drain（A1）───
    //
    // 任何 emergency event（EStop / AdapterErr(SafetyStopFailed)）必须**先于**
    // 推进 pending 处理。命中即清 pending 并 commit Emergency；本 tick 直接
    // return，后续的 normal action 留给下 tick。
    if let Some(em) = events.drain_emergency() {
        // 第 5 条门槛修订：emergency event 自身的 guard 副作用（EStop →
        // estop_latched=true）必须先于 plan_transition 应用，否则
        // plan_transition 看到的还是旧 guards 会拒绝 transition。
        apply_event_guard_side_effect(em, guards);
        if let Some(plan) = plan_transition(*state, em, guards) {
            debug_assert!(matches!(plan.kind, TransitionKind::Emergency));
            executor_ctrl.safety_stop();
            commit_transition(state, &plan);
            executor_ctrl.set_policy(plan.new_policy);
        }
        // **A1**：强制清 in-flight pending，让后续 ack 即使到达也无副作用
        retry.clear();
        return;
    }

    // ─── Step 2: 推进 in-flight effectful pending ───
    if let Some((plan, seq_opt)) = retry.pending {
        // **A2 头部**：1s 总超时检查（覆盖 None / Some(seq) 两个分支）。
        // `MonoRawNs - MonoRawNs` 返 `Option<DurationNs>`：None 表示
        // start > now（不该发生）；保守视为未超时。
        if let Some(start) = retry.pending_started_at
            && let Some(elapsed) = clock.now() - start
            && elapsed > PENDING_TOTAL_TIMEOUT
        {
            // **A3**：直接 commit_fault，不走 Event::AdapterErr。
            // **不**兜底 `safety_stop()`：AdapterErr 不是 emergency 路径
            // （safety_stop client 端会把 policy 切到 StopTorque 覆盖 Disabled）；
            // 下游若需 emergency 由 state_machine `* → Fault(SafetyStopFailed)`
            // 走 emergency drain。见设计 v9 §0.5 A3 注释。
            commit_fault(state, FaultCode::AdapterErr);
            executor_ctrl.set_policy(OutputPolicy::Disabled);
            retry.clear();
            return;
        }

        match seq_opt {
            None => {
                // 第一次提交（或 Busy 重试）
                let TransitionKind::Effectful { action: req } = plan.kind else {
                    unreachable!("non-Effectful in pending: {:?}", plan.kind)
                };
                match executor_ctrl.request_normal(req) {
                    NormalReqStatus::Submitted { seq } => {
                        retry.pending = Some((plan, Some(seq)));
                        // pending_started_at 不动（A2：保持 plan 入队时刻）
                    }
                    NormalReqStatus::Busy => {
                        // 等下 tick；超时由 Step 2 头部检查兜底
                    }
                    NormalReqStatus::Unsupported(_msg) => {
                        // **A3**：能力级拒绝在 effectful 路径必须升 Fault。
                        // 不兜底 safety_stop（同 1s 超时分支）。
                        commit_fault(state, FaultCode::AdapterErr);
                        executor_ctrl.set_policy(OutputPolicy::Disabled);
                        retry.clear();
                    }
                }
                return;
            }
            Some(seq) => {
                match executor_ctrl.poll_last_normal_resp(seq) {
                    None => {
                        // 还没完成；超时由 Step 2 头部检查兜底
                    }
                    Some(AdapterResp::Ok) => {
                        // ack 成功 —— 现在才 commit state + set_policy
                        commit_transition(state, &plan);
                        executor_ctrl.set_policy(plan.new_policy);
                        retry.clear();
                    }
                    Some(AdapterResp::Err(_e)) => {
                        // **A3 + finding #3**：所有 AdapterErr 变体都映射到
                        // FaultCode::AdapterErr（含 SafetyLatched）；
                        // SafetyStopFailed 仅来自 emergency 路径 + confirm 失败。
                        // 不兜底 safety_stop（同 1s 超时分支）。
                        commit_fault(state, FaultCode::AdapterErr);
                        executor_ctrl.set_policy(OutputPolicy::Disabled);
                        retry.clear();
                    }
                }
                return;
            }
        }
    }

    // ─── Step 3: plan_transition（pending 为空时）───
    while let Some(event) = events.pop() {
        // 第 5 条门槛修订：每个 event 的 guard 副作用必须**在它自己 plan
        // 之前**应用，**不**与同 batch 其它 event 共用一份"已被批量更新"的
        // guards。否则 [Enable, ClearEStop] 这种顺序会让 Enable 看到 ClearEStop
        // 提前清的 estop_latched=false 而误通过。
        apply_event_guard_side_effect(event, guards);
        let Some(plan) = plan_transition(*state, event, guards) else {
            continue;
        };
        match plan.kind {
            TransitionKind::Pure => {
                commit_transition(state, &plan);
                executor_ctrl.set_policy(plan.new_policy);
                // **finding（M7 阶段 3 收尾）**：`Event::ClearEStop` 的 trait
                // 契约（`ExecutorCtrlClient::clear_safety_latch` doc）就是
                // 由 ClearEStop transition 调用，否则 EStop 后 latch 文件 /
                // guard 即便清了，executor 的 `safety_latched` 仍是 true，
                // 下一次 `Enable(true)` 会被 client 端 / rt 端二次 latch
                // check 拒，再被 ipc_tick 升 `Fault(AdapterErr)`，破坏端到端
                // ClearEStop 恢复链路。
                if matches!(event, Event::ClearEStop) {
                    // **task-30 v9 / SerialRT E2E #4 守门**：清 latch 之前
                    // 必须确认 SerialRT/HardRT 端已 confirm torque off
                    // （emergency_action 已被消费完 + safety_state 已升
                    // TorqueOffConfirmed），否则下一 rt/serial tick 兜底
                    // store(latch, true) 会**覆盖**本次 clear（race window）。
                    //
                    // emergency 触发瞬间，client.safety_stop() 同步把
                    // safety_state 重置为 StopCommanded（不再相信 spawn 初值
                    // TorqueOffConfirmed），rt/serial 端真断扭后才升回 Confirmed。
                    //
                    // 若 safety_state != TorqueOffConfirmed：state 已 commit
                    // 进 Safe（保持 ClearEStop 的"主态"语义），但**不**调
                    // clear_safety_latch —— 操作员/RPC 应在看到 wire
                    // S0SafetyState=TorqueOffConfirmed 后**重新发送** ClearEStop
                    // 才能真正解 latch。
                    if executor_ctrl.safety_state() == OutputSafetyState::TorqueOffConfirmed {
                        executor_ctrl.clear_safety_latch();
                    }
                }
            }
            TransitionKind::Emergency => {
                // 兜底（理论 Step 1 drain_emergency 应该已经拿走；保留以防万一）
                executor_ctrl.safety_stop();
                commit_transition(state, &plan);
                executor_ctrl.set_policy(plan.new_policy);
            }
            TransitionKind::Effectful { .. } => {
                // **A2**：plan 入 pending 时**立即**置 pending_started_at
                retry.pending = Some((plan, None));
                retry.pending_started_at = Some(clock.now());
                break; // 一个 tick 只排一个 pending
            }
        }
    }
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
        Event::ClearEStop => guards.estop_latched = false,
        Event::ClearError => guards.adapter_fault_cleared = true,
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
        AdapterErr, AdapterNormalReq, ExecutorCtrlClient, NormalReqStatus, emergency_codes,
        normal_codes, pack_completed,
    };
    use robot_core::time::MockClock;

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
        );
        // tick 2：Step 2 None 分支 → request_normal Submitted{seq}
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
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
        );
        // tick 2: request_normal Unsupported → commit_fault
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
        );
        assert_eq!(state, S0State::Fault(FaultCode::AdapterErr));
        assert_eq!(
            OutputPolicy::from_u8(inner.policy.load(Ordering::Acquire)),
            OutputPolicy::Disabled
        );
        assert!(retry.pending.is_none());
    }

    /// **finding（阶段 3 收尾）lock**：EStop 后端到端恢复链路。
    ///
    /// 1) 触发 EStop → safety_latched=true，state=Safe
    /// 2) 测试方清 estop_latched guard（模拟 latch 文件被清）+ 排
    ///    `Event::ClearEStop` → ipc_tick Pure 分支调
    ///    `executor_ctrl.clear_safety_latch()` → safety_latched=false
    /// 3) 排 `Event::Enable` → SAFE → READY（Pure transition）
    /// 4) 排 `Event::Heartbeat` → READY → ENABLED.Active 是 Effectful
    ///    Enable(true) —— 后续 tick 走 Step 2 None 分支，client.request_normal
    ///    **不**被 latch 拒（client 端 latch 已清），返 Submitted
    ///
    /// 如果 ipc_tick **不**调 clear_safety_latch，第 4 步 client 端就会拒
    /// （NormalReqStatus::Unsupported），ipc_tick 再走 A3 升 Fault(AdapterErr)，
    /// 测试 fail。
    #[test]
    fn clear_estop_unblocks_subsequent_enable_via_latch() {
        let (inner, client, clock, mut events, mut retry, mut guards) = fixtures();
        let mut state = enabled_active();

        // 1) EStop
        events.push(Event::EStop);
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
        );
        assert_eq!(state, S0State::Safe);
        assert!(
            inner.safety_latched.load(Ordering::Acquire),
            "EStop 后 safety_latched 必须为 true"
        );

        // 2) 测试方模拟 latch 文件已清 + rt/serial 端 confirm torque off +
        //    排 ClearEStop。
        //
        // **task-30 v9 / SerialRT E2E #4 守门**：ipc_tick 在 ClearEStop Pure
        // 路径调 clear_safety_latch 之前会 check
        // executor_ctrl.safety_state() == TorqueOffConfirmed。
        // 测试里 inner 没有 rt/serial thread 推 safety_state，必须手动
        // store —— 模拟 SerialRT confirm_torque_off 已完成的场景。
        guards.estop_latched = false; // estop_latched guard 由外部 latch 文件清
        inner.safety_state.store(
            OutputSafetyState::TorqueOffConfirmed.to_u8(),
            Ordering::Release,
        );
        events.push(Event::ClearEStop);
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
        );
        // **关键断言**：ipc_tick Pure 分支应调 clear_safety_latch()
        assert!(
            !inner.safety_latched.load(Ordering::Acquire),
            "ClearEStop transition 后 executor safety_latched 必须为 false"
        );
        // state 仍是 Safe（ClearEStop 不改 state，只清 latch）
        assert_eq!(state, S0State::Safe);

        // 3) Enable → SAFE → READY（Pure transition）
        events.push(Event::Enable);
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
        );
        assert_eq!(state, S0State::Ready);

        // 4) Heartbeat → READY → ENABLED.Active 是 Effectful Enable(true)
        events.push(Event::Heartbeat);
        // tick 1: 入 pending(None)
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
        );
        // tick 2: Step 2 None 分支调 request_normal —— 应**不**被 latch 拒
        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
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

    /// **第 5 条门槛锁测（SerialRT E2E）**：同 tick 内 `[Enable, ClearEStop]`
    /// 顺序进入 EventQueue（先 Enable 后 ClearEStop），Enable 必须**看不到**
    /// ClearEStop 提前清的 `estop_latched=false` —— 也就是 Enable 的
    /// `plan_transition` 看到 `estop_latched=true` 被拒绝；ClearEStop 自己
    /// 处理时再清 latch。
    ///
    /// 旧实现（apply_event_guard_side_effect 在 ipc_thread::run_loop
    /// channel drain 阶段批量调用）会让 ClearEStop 提前改 guards.estop_latched
    /// =false，Enable 紧接其后 plan 时看到 false 就误通过 → state=Ready。
    /// 修订后：apply 移到 ipc_tick 内逐 event 处理时再调，Enable 看到的是
    /// 旧 guards（estop_latched=true），plan_transition 拒绝；只有 ClearEStop
    /// 自己处理时才把 estop_latched 改成 false（state 仍留 Safe，因为
    /// ClearEStop 的 transition 只是 SAFE→SAFE）。
    ///
    /// 这条不变量未来如果有人改回 batch update guards 模式，会被本测试钉死。
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
        // 同 tick 把 Enable 与 ClearEStop 顺序入队。注意发送顺序：先 Enable
        // 后 ClearEStop —— 这是测试的关键场景。
        events.push(Event::Enable);
        events.push(Event::ClearEStop);

        ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            &client,
            clock.as_ref(),
            &mut events,
        );

        // **关键断言**：state 必须留在 Safe（Enable 因 estop_latched=true 被拒，
        // ClearEStop 处理后只是 SAFE→SAFE）。**不**得变 Ready。
        assert_eq!(
            state,
            S0State::Safe,
            "Enable 不应因 ClearEStop 提前清 estop_latched 而误通过；\
             实际 state={state:?}"
        );
        // ClearEStop 自己被处理时已把 guards.estop_latched 改成 false。
        assert!(
            !guards.estop_latched,
            "ClearEStop 处理后 guards.estop_latched 应清"
        );
        // ClearEStop 走 Pure 分支调 clear_safety_latch（即使初始 latch=false
        // 也是无副作用 idempotent store）。这里 inner 默认初值 latch=false，
        // 所以这条只确认 store 被调到（false→false 无变化）；锁测核心不靠
        // 这条，但保留以确认 Pure 分支真的走通了。
        assert!(!inner.safety_latched.load(Ordering::Acquire));
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
