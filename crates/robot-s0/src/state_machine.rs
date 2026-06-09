//! S0 子态机：5 主态 + ENABLED 的 4 子态 + 12 事件 + 转换表。
//!
//! 对应 `docs/00_design.md` §1.4（冻结的转换表）+ §6.7 核心不变量。
//!
//! ### Part 1 实现范围
//!
//! 所有状态变体 + 所有 transition 行**全部写死**：单测用 `Event::*` 注入验证
//! 转换表；运行时（`rt_thread` / `ipc_thread`）这一步只会真正发 `Enable` / `Heartbeat` /
//! `EStop` 三种事件，其他事件的**触发器信号源**在 Part 2 才接：
//!
//! | 事件 | 触发源 | Part 1 可用？ |
//! |---|---|---|
//! | `Enable` / `Heartbeat` / `EStop` | 测试或 RPC 注入 | ✅ |
//! | `HeartbeatTimeout` | `watchdog`（Part 2） | ❌ |
//! | `AdapterErr` / `CmdDeadlineExpired` / `LimitViolation` / `WatchdogTimeout` / `RtOverrun` | `rt_thread` 内检测（Part 2） | ❌ |
//! | `ClearError` / `ClearEStop` | RPC（Part 2） | ❌ |
//! | `Stop` | `systemd SIGTERM` handler（Part 2） | ❌ |
//!
//! 这些事件**变体 + transition 单测**都存在；Part 2 只接信号源不改 shape。
//!
//! ### 守卫状态（`Guards`）
//!
//! Part 1 `Guards` 是扁平 POD struct，字段由调用方在 transition 前根据当前运行时
//! 现实填写（例如 `adapter_connected` 来自 `ManipulatorAdapter::connect()` 的结果）。
//! Part 2 的 watchdog / deadline 会增量写入对应字段；Guards struct 本身不需改。

#![allow(clippy::doc_markdown)]

use robot_core::adapter::ControlMode;

// ============================================================================
// 枚举定义
// ============================================================================

/// ENABLED 的 4 个子态。`CtrlLost{Hold,Zero,Stop}` 三者对应 `JointCommand.on_timeout`
/// 的三种策略（§1.4）。Part 1 `rt_thread` 只进 `Active`；其他三态需要 watchdog 触发。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnabledSub {
    Active,
    CtrlLostHold,
    CtrlLostZero,
    CtrlLostStop,
}

/// 控制链路状态。`Ok` = 收到新 heartbeat；`Lost` = watchdog 超时。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlLink {
    Ok,
    Lost,
}

/// 输出模式。与 `EnabledSub` 一一对应但独立命名，对外 state topic 暴露时用这个字段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputMode {
    Active,
    Hold,
    Zero,
    Stop,
}

impl OutputMode {
    /// 从 ENABLED 子态映射到 OutputMode（§1.4 约定）。
    #[must_use]
    pub const fn from_sub(sub: EnabledSub) -> Self {
        match sub {
            EnabledSub::Active => Self::Active,
            EnabledSub::CtrlLostHold => Self::Hold,
            EnabledSub::CtrlLostZero => Self::Zero,
            EnabledSub::CtrlLostStop => Self::Stop,
        }
    }
}

/// Fault 代码。对应 §1.4 ENABLED → FAULT 的 5 条触发边 + task-30 v9 加 1 条
/// (`SafetyStopFailed`：`safety_stop` / `confirm_torque_off` 失败)。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultCode {
    AdapterErr,
    CmdDeadlineExpired,
    LimitViolation,
    WatchdogTimeout,
    RtOverrun,
    /// task-30 v9 pre-2 / §0.5 errata + finding #3 修订：
    /// **真**硬件 fail-safe 失败时升 `Fault(SafetyStopFailed)`：
    /// 1. `RtSafeCtrl::safety_stop_rt` / `ManipulatorAdapter::safety_stop`
    ///    返回 `Err`；
    /// 2. `OutputSafetyProbe::read_torque_enable_mask` 持续显示 motor 仍
    ///    torque-on（`confirm_torque_off` 超阈值）。
    ///
    /// **不**来自 `AdapterResp::Err(AdapterErr::SafetyLatched)` —— v9 finding
    /// #3 收紧：latch 拒请求 = 正常保护，非"停机失败"，由 ipc_tick A3 升
    /// `Fault(AdapterErr)`（见 [`crate::ipc_tick`]）。
    ///
    /// 比 `AdapterErr` 更高优级，提示上游"硬件 fail-safe 没拿到证据，需要
    /// 人工现场确认"。
    SafetyStopFailed,
}

/// S0 主状态机。5 主态。`ENABLED` 带子态结构。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum S0State {
    Safe,
    Ready,
    Enabled {
        sub: EnabledSub,
        control_link: ControlLink,
        output_mode: OutputMode,
    },
    Fault(FaultCode),
    Stopping,
}

impl Default for S0State {
    /// 上电后的默认状态：`SAFE`（§6.7 "S0 启动 = 进 SAFE 状态 + 读 `EStop` latch"）。
    fn default() -> Self {
        Self::Safe
    }
}

/// 事件集合。§1.4 列出的 12 种；**部分事件在 Part 1 没有真实触发源**（见模块 doc 表格）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Enable,
    /// **v0.5 候选 C**：operator `SetEnabled(false)` —— Enabled/Ready → Safe
    /// （torque-off，**不**置 estop_latched，区别 EStop）；Safe/Fault no-op 幂等。
    Disable,
    Heartbeat,
    HeartbeatTimeout,
    AdapterErr(FaultCode),
    CmdDeadlineExpired,
    LimitViolation,
    WatchdogTimeout,
    RtOverrun,
    EStop,
    ClearError,
    ClearEStop,
    Stop,
}

// ============================================================================
// Guards
// ============================================================================

/// 转换守卫条件的扁平快照。调用方在 `try_transition` 前填入当前事实。
///
/// Part 1 `on_timeout_hint` 默认 `Hold`（保守），Part 2 从最近一条 `JointCommand`
/// 的 `on_timeout` 字段决定。
///
/// 字段 bool 多（对齐 §1.4 转换表的 9 条守卫条件），clippy
/// `struct_excessive_bools` 在此 allow —— 这些 bool 语义互相独立，聚合成 bitflags
/// 反而降低可读性（每条转换规则都引用独立字段）。
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Guards {
    pub estop_latched: bool,
    pub adapter_connected: bool,
    pub calibration_loaded: bool,
    pub joint_state_valid: bool,
    pub power_ok: bool,
    pub heartbeat_ok: bool,
    pub cmd_deadline_valid: bool,
    pub limits_valid: bool,
    pub hw_enable_ok: bool,
    pub adapter_fault_cleared: bool,
    pub command_mode: ControlMode,
    pub on_timeout_hint: EnabledSub,
}

impl Default for Guards {
    fn default() -> Self {
        Self {
            estop_latched: false,
            adapter_connected: false,
            calibration_loaded: false,
            joint_state_valid: false,
            power_ok: false,
            heartbeat_ok: false,
            cmd_deadline_valid: false,
            limits_valid: false,
            hw_enable_ok: false,
            adapter_fault_cleared: false,
            command_mode: ControlMode::Stop,
            on_timeout_hint: EnabledSub::CtrlLostHold,
        }
    }
}

impl Guards {
    /// SAFE → READY 的 guard 集合（§1.4）。
    #[must_use]
    pub const fn ready_guards_pass(&self) -> bool {
        !self.estop_latched
            && self.adapter_connected
            && self.calibration_loaded
            && self.joint_state_valid
            && self.power_ok
    }

    /// READY → ENABLED.ACTIVE 的 guard 集合（§1.4）。
    #[must_use]
    pub const fn enabled_guards_pass(&self) -> bool {
        self.heartbeat_ok && self.cmd_deadline_valid && self.limits_valid && self.hw_enable_ok
    }
}

// ============================================================================
// 拒绝原因
// ============================================================================

/// 转换被拒绝的诊断信息（供日志 / metrics / RPC 回执）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rejected {
    pub from: S0State,
    pub event: Event,
    pub reason: &'static str,
}

// ============================================================================
// S0StateSnapshot（给 seqlock 用的 Copy/POD 快照）
// ============================================================================

/// `ipc_thread` → `pub_thread` 方向的 state machine 快照。
///
/// ### 为什么要这个类型
///
/// Part 2 架构决策：`ipc_thread` 是 state machine 的执行者，每 tick 消费事件调
/// [`try_transition`] 后必须把结果发布给 `pub_thread`（它负责对外 iceoryx2 publish
/// `JointState` + S0 自身 Heartbeat）。同步原语复用现有 `RtSnapshotPublisher<T>`
/// seqlock —— 要求 `T: Copy`。
///
/// 把当前 [`S0State`] + [`Guards`] 快照 + 时间戳打包成单一 POD struct 承载，方便
/// `pub_thread` 侧一次 `load()` 拿到完整视图（避免两次 load 之间的撕裂语义）。
///
/// ### 字段
///
/// - `state`：主状态机当前态（5 主态之一）。
/// - `guards_summary`：`ipc_thread` 本 tick 喂给 [`try_transition`] 的 [`Guards`]；
///   `pub_thread` 可以从中读出 `estop_latched` / `heartbeat_ok` 等细粒度信号写进
///   对外 state body。
/// - `stamp_ns`：`RobotClock::now()` 纳秒，**单调**；`pub_thread` 侧可据此判断
///   "本 tick 是否更新"（两次 load 比 stamp 即可，无需比对整个结构）。
///
/// ### 不变量
///
/// - **Copy/POD**：编译期 `size_of` 断言 ≤ 64 B（当前实测 ≈ 32 B）；无带 Drop 字段。
/// - **Default = Safe**：上电初值；`ipc_thread` spawn 时 seqlock 用 `Default` 初始化，
///   `pub_thread` 在 `ipc_thread` 还没发第一次 publish 前读到的也是 Safe 态（对应
///   §6.7 "S0 启动 = 进 SAFE 状态"的观测一致性）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct S0StateSnapshot {
    pub state: S0State,
    pub guards_summary: Guards,
    pub stamp_ns: u64,
}

impl S0StateSnapshot {
    /// 构造一份快照。`ipc_thread` 每 tick 在完成 `try_transition` 后调用。
    #[must_use]
    pub const fn new(state: S0State, guards_summary: Guards, stamp_ns: u64) -> Self {
        Self {
            state,
            guards_summary,
            stamp_ns,
        }
    }
}

// 编译期尺寸守卫：seqlock memcpy 成本随 T 大小线性上升，S0StateSnapshot 必须保持精简。
// 实测大小受 Guards（10 bool + 2 enum = ~12 B）+ S0State（~4 B）+ stamp_ns (8 B) + padding。
// 留 64 B 余量，未来若要扩字段（fault code 明细、last event）仍有空间。
const _: () = assert!(
    core::mem::size_of::<S0StateSnapshot>() <= 64,
    "S0StateSnapshot must stay ≤ 64 B to keep seqlock memcpy cheap",
);

// ============================================================================
// task-30 v9 / M7：plan_transition / commit_transition / commit_fault API
// + EventQueue + drain_emergency
// ============================================================================

use robot_core::executor_ctrl::{AdapterNormalReq, OutputPolicy};

/// transition 副作用类别（v9 §4.4.1）。
///
/// `ipc_tick` 据此决定如何 commit：
/// - [`Pure`] / [`Emergency`]：立即 commit state + set_policy；
/// - [`Effectful`]：先把 plan 入 `IpcRetryState.pending`，等 adapter ack 才 commit。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionKind {
    /// 无副作用 transition（如 SAFE → READY 守卫满足 / FAULT 子态切换 / 子态间
    /// 切换）。立即 `commit_transition` + `set_policy`，无需等 adapter ack。
    Pure,
    /// 紧急 transition（任意态 + `Event::EStop` → `Safe`）。立即 commit +
    /// `safety_stop()` fire-and-forget；硬件实际状态由 `OutputSafetyState` 异步反映。
    Emergency,
    /// 副作用 transition：必须先调 adapter 动作，等硬件 ack 才 commit。
    /// 例：`READY → ENABLED.Active` 需 `Enable(true)`；`ENABLED → Safe`
    /// 非紧急路径走 `Enable(false)`。M7 `ipc_tick` 把 plan 入 pending。
    Effectful { action: AdapterNormalReq },
}

/// `plan_transition` 的输出：未提交的 transition 计划。`ipc_tick` 决定何时
/// `commit_transition`（立即 / ack 后）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlannedTransition {
    pub new_state: S0State,
    pub new_policy: OutputPolicy,
    pub kind: TransitionKind,
}

/// 计算 transition 的**计划**，**不**提交。返回 `None` 表示 `(cur, ev)` 不在
/// 转换表 / guard 不通过；返回 `Some(plan)` 表示有合法 transition，由调用方
/// 决定何时 [`commit_transition`]。
///
/// 与 [`try_transition`] 的关系：
/// - 旧 `try_transition` 直接返新 state（v0.2 用），不区分 Pure/Emergency/Effectful；
/// - 新 `plan_transition` 返综合计划（**含**副作用类别 + new_policy），是 M7
///   `ipc_tick` 走 errata 三步式的入口。
/// - 两者**必须语义等价**：同 `(cur, ev, guards)` 下 `plan_transition.is_some()`
///   ↔ `try_transition.is_ok()`，且 `plan.new_state == try_transition.new_state`。
///   单测 `plan_and_try_transition_agree_on_state` 强制对齐。
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn plan_transition(cur: S0State, ev: Event, g: &Guards) -> Option<PlannedTransition> {
    // 先按 try_transition 的逻辑算 new_state
    let new_state = try_transition(cur, ev, g).ok()?;

    // 决定副作用类别 + new_policy
    let (kind, new_policy) = classify_transition(cur, ev, new_state);

    Some(PlannedTransition {
        new_state,
        new_policy,
        kind,
    })
}

/// 内部 helper：根据 (cur, event, new_state) 决定 kind + policy。
///
/// **EStop 路径** → Emergency + StopTorque。
/// **`AdapterErr(SafetyStopFailed)`** → Emergency + Disabled（任意态 → Fault
/// 都视为 emergency，让 ipc_tick 立即下 safety_stop 兜底）。
/// **READY → ENABLED.Active**（`Heartbeat`）→ Effectful + ActiveForward
/// （需要 `Enable(true)` 上扭）。
/// **ENABLED → ENABLED 子态切换**（`HeartbeatTimeout` / `Heartbeat` 恢复）→
/// Pure + 对应 policy。
/// **其它**（SAFE → READY / Stop / ClearError 等）→ Pure。
#[allow(clippy::too_many_lines)]
fn classify_transition(
    cur: S0State,
    ev: Event,
    new_state: S0State,
) -> (TransitionKind, OutputPolicy) {
    use Event as E;
    use S0State as S;

    // EStop = Emergency
    if matches!(ev, E::EStop) {
        return (TransitionKind::Emergency, OutputPolicy::StopTorque);
    }

    // SafetyStopFailed = Emergency（任意态 → Fault(SafetyStopFailed)）
    if matches!(ev, E::AdapterErr(FaultCode::SafetyStopFailed)) {
        return (TransitionKind::Emergency, OutputPolicy::Disabled);
    }

    // READY → ENABLED.Active：需要先 Enable(true)（Effectful）
    if matches!(cur, S::Ready) && matches!(ev, E::Heartbeat) {
        return (
            TransitionKind::Effectful {
                action: AdapterNormalReq::Enable(true),
            },
            OutputPolicy::ActiveForward,
        );
    }

    // ENABLED → SAFE（v0.5 候选 C `SetEnabled(false)` = Disable）：Effectful
    // `Enable(false)`（真 torque-off，沿 enable 上扭对称）。Ready/Safe/Fault 的
    // Disable 落默认 Pure + new_state policy（Ready 未上扭；Safe/Fault no-op，无需
    // adapter action）。
    if matches!(cur, S::Enabled { .. }) && matches!(ev, E::Disable) {
        return (
            TransitionKind::Effectful {
                action: AdapterNormalReq::Enable(false),
            },
            OutputPolicy::Disabled,
        );
    }

    // 决定 new_policy：按 new_state 选典型 policy
    let policy = match new_state {
        S::Safe | S::Fault(_) | S::Stopping => OutputPolicy::Disabled,
        S::Ready => OutputPolicy::ReadyHold,
        S::Enabled {
            output_mode: OutputMode::Active,
            ..
        } => OutputPolicy::ActiveForward,
        S::Enabled {
            output_mode: OutputMode::Hold,
            ..
        } => OutputPolicy::HoldCurrent,
        S::Enabled {
            output_mode: OutputMode::Zero,
            ..
        } => OutputPolicy::ZeroCommand,
        S::Enabled {
            output_mode: OutputMode::Stop,
            ..
        } => OutputPolicy::StopTorque,
    };

    (TransitionKind::Pure, policy)
}

/// 提交计划好的 transition。**仅在** `Pure` / `Emergency` 立即调；`Effectful`
/// 路径必须等 adapter ack 成功后调。
pub fn commit_transition(state: &mut S0State, plan: &PlannedTransition) {
    *state = plan.new_state;
}

/// 直接把 state 拍到 `Fault(code)`，不走 `plan_transition`。
///
/// **task-30 v9 §0.5 A3 实施版**：M7 `ipc_tick` 在 effectful ack 失败时调本 API
/// **直接**升 fault，不走 `Event::AdapterErr` 入队 + `plan_transition` —— 防止
/// guard 挡住造成 silent stall。
pub fn commit_fault(state: &mut S0State, fault: FaultCode) {
    *state = S0State::Fault(fault);
}

// ============================================================================
// EventQueue（task-30 v9 §4.4.3 errata Step 1 用）
// ============================================================================

/// 简单 FIFO 队列 + `drain_emergency` 优先抽取语义。
///
/// **设计 v9 errata §0.5 A1**：`ipc_tick` 第一步 `drain_emergency` 必须**先于**
/// 推进 in-flight pending 处理；emergency 不能被 effectful pending 阻塞 1s。
/// 本类型提供线性扫一次队列、移除并返回**第一个** `Event::EStop` /
/// `Event::FaultLatched` / `Event::SafetyStopFailed` 的方法，其它事件原序保留。
///
/// 普通入队 / 出队走 `push` / `pop`；emergency 走 `drain_emergency`。
#[derive(Debug, Default)]
pub struct EventQueue {
    inner: std::collections::VecDeque<Event>,
}

impl EventQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 普通入队（FIFO 尾部）。
    pub fn push(&mut self, ev: Event) {
        self.inner.push_back(ev);
    }

    /// 普通出队（FIFO 头部）。
    pub fn pop(&mut self) -> Option<Event> {
        self.inner.pop_front()
    }

    /// 队列是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// 队列长度（含 emergency events）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// **task-30 v9 §0.5 A1**：从队列摘出第一个 emergency event（按 push
    /// 顺序），其它事件**原序保留**。返回 `None` 表示队列里没有 emergency。
    ///
    /// emergency events：
    /// - `Event::EStop`
    /// - `Event::AdapterErr(FaultCode::SafetyStopFailed)` —— v9 视作 emergency
    ///   级别 fault（safety_stop 失败 = 任意态直进 Fault + 立即下硬件
    ///   safety_stop 兜底）。
    pub fn drain_emergency(&mut self) -> Option<Event> {
        // 找到第一个 emergency event 的位置，移除并返回。
        let pos = self.inner.iter().position(|ev| {
            matches!(
                ev,
                Event::EStop | Event::AdapterErr(FaultCode::SafetyStopFailed)
            )
        })?;
        self.inner.remove(pos)
    }

    /// **A1-3 (1/3b) round 2**：peek 队列里是否有 emergency event（不移除）。
    ///
    /// `ipc_thread` RR drain 完后用本方法判断"in-flight live ClearEStop
    /// 是否需要被 emergency preempt 取消"——避免 RR live 进 EventQueue 后
    /// 被 [`drain_emergency`] 跳过（drain_emergency 只抽 emergency 自己），
    /// 等下一 tick 仍误执行 ClearEStop 清掉新触发的 EStop。
    ///
    /// emergency events 跟 [`drain_emergency`] 同集：
    /// - `Event::EStop`
    /// - `Event::AdapterErr(FaultCode::SafetyStopFailed)`
    #[must_use]
    pub fn has_emergency(&self) -> bool {
        self.inner.iter().any(|ev| {
            matches!(
                ev,
                Event::EStop | Event::AdapterErr(FaultCode::SafetyStopFailed)
            )
        })
    }

    /// **A1-3 (1/3b) round 2**：移除队列里**所有** `Event::ClearEStop`，
    /// 返回移除数量。
    ///
    /// 用于 `ipc_thread` 在 emergency preempt / deadline 过期时取消
    /// in-flight live ClearEStop —— 把对应 event 从 queue 摘掉，避免
    /// `ipc_tick` Step 3 在下一 tick 误处理已被取消的 RR 请求。
    pub fn remove_clear_estop_events(&mut self) -> usize {
        let before = self.inner.len();
        self.inner.retain(|ev| !matches!(ev, Event::ClearEStop));
        before - self.inner.len()
    }
}

// ============================================================================
// try_transition
// ============================================================================

/// 尝试从 `cur` 在 `ev` 触发下转换，`g` 是当前守卫快照。返回新状态或拒绝。
///
/// 转换表完全对应 `docs/00_design.md` §1.4 的 7 行条目 + ENABLED 子态内部回归
/// （`CONTROL_LOST_*` → ACTIVE）。
///
/// # Errors
///
/// 返回 `Rejected` 当 (cur, ev) 不在表中或 guard 不通过。调用方可选择记录到
/// metrics 或直接忽略（没有匹配的转换 = 没动作）。
#[allow(clippy::too_many_lines, clippy::match_same_arms)]
pub fn try_transition(cur: S0State, ev: Event, g: &Guards) -> Result<S0State, Rejected> {
    use Event as E;
    use S0State as S;

    let reject = |reason: &'static str| -> Result<S0State, Rejected> {
        Err(Rejected {
            from: cur,
            event: ev,
            reason,
        })
    };

    match (cur, ev) {
        // ---- * → SAFE（EStop 压倒一切，包括 FAULT 和 STOPPING）----
        (_, E::EStop) => Ok(S::Safe),

        // ---- * → STOPPING（systemd SIGTERM 等关停信号）----
        (_, E::Stop) => Ok(S::Stopping),

        // ---- * → FAULT(SafetyStopFailed)（task-30 v9 pre-2）----
        // safety_stop / confirm_torque_off 失败 → 任意态直进 Fault(SafetyStopFailed)。
        // 比 AdapterErr 更高优：硬件 fail-safe 没拿到证据，硬件状态视为不可信。
        // 通过 `Event::AdapterErr(FaultCode::SafetyStopFailed)` 注入，**所有**主态
        // （含 Safe / Ready / Stopping / 其它 Fault）都允许进 Fault(SafetyStopFailed)。
        (_, E::AdapterErr(FaultCode::SafetyStopFailed)) => {
            Ok(S::Fault(FaultCode::SafetyStopFailed))
        }

        // ---- SAFE → READY ----
        (S::Safe, E::Enable) => {
            if g.ready_guards_pass() {
                Ok(S::Ready)
            } else {
                reject("SAFE→READY guards not satisfied")
            }
        }

        // ---- SAFE → SAFE（ClearEStop control action）----
        //
        // **A1-2**：state_machine 不再假设 latch 已被外层清掉。
        // (Safe, ClearEStop) 静态允许；真正的清 latch（fsync +
        // `clear_safety_latch()` + `guards.estop_latched` 翻转）由 ipc_tick
        // 拿到本 plan 之后调 `clear_estop_action::try_apply_clear_estop`
        // 完成。action handler 失败时 ipc_tick 不 commit_transition，state
        // 留 Safe。
        //
        // 这条 transition 故意不检查 `g.estop_latched`——真正的硬件断扭
        // 确认由 action handler 内部通过 `executor_ctrl.safety_state() ==
        // TorqueOffConfirmed` 完成（看硬件实际，不看软件 guard 副本）。
        (S::Safe, E::ClearEStop) => Ok(S::Safe),

        // ---- READY → ENABLED.ACTIVE ----
        (S::Ready, E::Heartbeat) => {
            if g.enabled_guards_pass() {
                Ok(S::Enabled {
                    sub: EnabledSub::Active,
                    control_link: ControlLink::Ok,
                    output_mode: OutputMode::Active,
                })
            } else {
                reject("READY→ENABLED guards not satisfied")
            }
        }

        // ---- ENABLED.ACTIVE → ENABLED.CONTROL_LOST_* ----
        (
            S::Enabled {
                sub: EnabledSub::Active,
                ..
            },
            E::HeartbeatTimeout,
        ) => {
            let sub = g.on_timeout_hint;
            Ok(S::Enabled {
                sub,
                control_link: ControlLink::Lost,
                output_mode: OutputMode::from_sub(sub),
            })
        }

        // ---- ENABLED.CONTROL_LOST_* → ENABLED.ACTIVE ----
        (
            S::Enabled {
                control_link: ControlLink::Lost,
                ..
            },
            E::Heartbeat,
        ) => {
            if g.heartbeat_ok {
                Ok(S::Enabled {
                    sub: EnabledSub::Active,
                    control_link: ControlLink::Ok,
                    output_mode: OutputMode::Active,
                })
            } else {
                reject("CONTROL_LOST→ACTIVE needs heartbeat_ok")
            }
        }

        // ---- ENABLED.* → FAULT ----
        (S::Enabled { .. }, E::AdapterErr(fc)) => Ok(S::Fault(fc)),
        (S::Enabled { .. }, E::CmdDeadlineExpired) => Ok(S::Fault(FaultCode::CmdDeadlineExpired)),
        (S::Enabled { .. }, E::LimitViolation) => Ok(S::Fault(FaultCode::LimitViolation)),
        (S::Enabled { .. }, E::WatchdogTimeout) => Ok(S::Fault(FaultCode::WatchdogTimeout)),
        (S::Enabled { .. }, E::RtOverrun) => Ok(S::Fault(FaultCode::RtOverrun)),

        // ---- FAULT → READY ----
        (S::Fault(_), E::ClearError) => {
            if !g.estop_latched && g.adapter_fault_cleared {
                Ok(S::Ready)
            } else {
                reject("FAULT→READY needs !estop_latched && adapter_fault_cleared")
            }
        }

        // ---- Enabled/Ready → SAFE（v0.5 候选 C `SetEnabled(false)` = Disable）----
        //
        // torque-off path（Enabled 走 effectful `Enable(false)`，见
        // `classify_transition`）；**不**置 `estop_latched`（区别 EStop）、**不**
        // 清 latch。Disable **不**检查 `estop_latched`：disable 一个 latched
        // 机器人是安全 + 显式的（hard_rt_ctrl `Enable(false)` latch 下仍 Submitted）。
        (S::Enabled { .. } | S::Ready, E::Disable) => Ok(S::Safe),

        // ---- Safe / Fault → no-op（已非 enabled，幂等）----
        (S::Safe, E::Disable) => Ok(S::Safe),
        (S::Fault(fc), E::Disable) => Ok(S::Fault(fc)),

        // ---- 默认拒绝 ----
        _ => reject("no transition for (state, event)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use robot_core::adapter::ControlMode;

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
            command_mode: ControlMode::Position,
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

    // ---- SAFE → READY ----

    #[test]
    fn safe_to_ready_pass() {
        let g = healthy_guards();
        assert_eq!(
            try_transition(S0State::Safe, Event::Enable, &g),
            Ok(S0State::Ready)
        );
    }

    #[test]
    fn safe_to_ready_blocked_by_estop() {
        let mut g = healthy_guards();
        g.estop_latched = true;
        let r = try_transition(S0State::Safe, Event::Enable, &g).unwrap_err();
        assert_eq!(r.from, S0State::Safe);
        assert_eq!(r.event, Event::Enable);
    }

    #[test]
    fn safe_to_ready_blocked_by_power() {
        let mut g = healthy_guards();
        g.power_ok = false;
        assert!(try_transition(S0State::Safe, Event::Enable, &g).is_err());
    }

    #[test]
    fn safe_to_ready_blocked_by_adapter_disconnected() {
        let mut g = healthy_guards();
        g.adapter_connected = false;
        assert!(try_transition(S0State::Safe, Event::Enable, &g).is_err());
    }

    // ---- READY → ENABLED.ACTIVE ----

    #[test]
    fn ready_to_enabled_active() {
        let g = healthy_guards();
        let next = try_transition(S0State::Ready, Event::Heartbeat, &g).unwrap();
        assert_eq!(next, enabled_active());
    }

    #[test]
    fn ready_to_enabled_blocked_by_heartbeat_ok() {
        let mut g = healthy_guards();
        g.heartbeat_ok = false;
        assert!(try_transition(S0State::Ready, Event::Heartbeat, &g).is_err());
    }

    // ---- ENABLED.ACTIVE → CONTROL_LOST_* ----

    #[test]
    fn active_to_control_lost_hold() {
        let mut g = healthy_guards();
        g.on_timeout_hint = EnabledSub::CtrlLostHold;
        let next = try_transition(enabled_active(), Event::HeartbeatTimeout, &g).unwrap();
        assert_eq!(
            next,
            S0State::Enabled {
                sub: EnabledSub::CtrlLostHold,
                control_link: ControlLink::Lost,
                output_mode: OutputMode::Hold,
            }
        );
    }

    #[test]
    fn active_to_control_lost_zero() {
        let mut g = healthy_guards();
        g.on_timeout_hint = EnabledSub::CtrlLostZero;
        let next = try_transition(enabled_active(), Event::HeartbeatTimeout, &g).unwrap();
        assert_eq!(
            next,
            S0State::Enabled {
                sub: EnabledSub::CtrlLostZero,
                control_link: ControlLink::Lost,
                output_mode: OutputMode::Zero,
            }
        );
    }

    #[test]
    fn active_to_control_lost_stop() {
        let mut g = healthy_guards();
        g.on_timeout_hint = EnabledSub::CtrlLostStop;
        let next = try_transition(enabled_active(), Event::HeartbeatTimeout, &g).unwrap();
        assert_eq!(
            next,
            S0State::Enabled {
                sub: EnabledSub::CtrlLostStop,
                control_link: ControlLink::Lost,
                output_mode: OutputMode::Stop,
            }
        );
    }

    // ---- CONTROL_LOST_* → ACTIVE ----

    #[test]
    fn control_lost_to_active_on_heartbeat() {
        let g = healthy_guards();
        let from = S0State::Enabled {
            sub: EnabledSub::CtrlLostHold,
            control_link: ControlLink::Lost,
            output_mode: OutputMode::Hold,
        };
        let next = try_transition(from, Event::Heartbeat, &g).unwrap();
        assert_eq!(next, enabled_active());
    }

    #[test]
    fn control_lost_to_active_blocked_without_heartbeat_ok() {
        let mut g = healthy_guards();
        g.heartbeat_ok = false;
        let from = S0State::Enabled {
            sub: EnabledSub::CtrlLostHold,
            control_link: ControlLink::Lost,
            output_mode: OutputMode::Hold,
        };
        assert!(try_transition(from, Event::Heartbeat, &g).is_err());
    }

    // ---- ENABLED.* → FAULT ----

    #[test]
    fn enabled_to_fault_adapter_err() {
        let g = healthy_guards();
        let next = try_transition(
            enabled_active(),
            Event::AdapterErr(FaultCode::AdapterErr),
            &g,
        )
        .unwrap();
        assert_eq!(next, S0State::Fault(FaultCode::AdapterErr));
    }

    #[test]
    fn enabled_to_fault_cmd_deadline() {
        let g = healthy_guards();
        let next = try_transition(enabled_active(), Event::CmdDeadlineExpired, &g).unwrap();
        assert_eq!(next, S0State::Fault(FaultCode::CmdDeadlineExpired));
    }

    #[test]
    fn enabled_to_fault_limit_violation() {
        let g = healthy_guards();
        let next = try_transition(enabled_active(), Event::LimitViolation, &g).unwrap();
        assert_eq!(next, S0State::Fault(FaultCode::LimitViolation));
    }

    #[test]
    fn enabled_to_fault_watchdog() {
        let g = healthy_guards();
        let next = try_transition(enabled_active(), Event::WatchdogTimeout, &g).unwrap();
        assert_eq!(next, S0State::Fault(FaultCode::WatchdogTimeout));
    }

    #[test]
    fn enabled_to_fault_rt_overrun() {
        let g = healthy_guards();
        let next = try_transition(enabled_active(), Event::RtOverrun, &g).unwrap();
        assert_eq!(next, S0State::Fault(FaultCode::RtOverrun));
    }

    // ---- * → SAFE（EStop）----

    #[test]
    fn estop_from_any_state() {
        let g = healthy_guards();
        for from in [
            S0State::Safe,
            S0State::Ready,
            enabled_active(),
            S0State::Fault(FaultCode::AdapterErr),
            S0State::Stopping,
        ] {
            let next = try_transition(from, Event::EStop, &g).unwrap();
            assert_eq!(next, S0State::Safe, "EStop should drop {from:?} into Safe");
        }
    }

    // ---- FAULT → READY ----

    #[test]
    fn fault_to_ready_on_clear_error() {
        let g = healthy_guards();
        let next =
            try_transition(S0State::Fault(FaultCode::RtOverrun), Event::ClearError, &g).unwrap();
        assert_eq!(next, S0State::Ready);
    }

    #[test]
    fn fault_to_ready_blocked_by_estop_latched() {
        let mut g = healthy_guards();
        g.estop_latched = true;
        assert!(
            try_transition(S0State::Fault(FaultCode::AdapterErr), Event::ClearError, &g).is_err()
        );
    }

    #[test]
    fn fault_to_ready_blocked_by_unclear_fault() {
        let mut g = healthy_guards();
        g.adapter_fault_cleared = false;
        assert!(
            try_transition(S0State::Fault(FaultCode::AdapterErr), Event::ClearError, &g).is_err()
        );
    }

    // ---- * → FAULT(SafetyStopFailed)（task-30 v9 pre-2）----

    #[test]
    fn safety_stop_failed_from_any_state() {
        // 任意态 + Event::AdapterErr(SafetyStopFailed) → Fault(SafetyStopFailed)
        let g = healthy_guards();
        for from in [
            S0State::Safe,
            S0State::Ready,
            enabled_active(),
            S0State::Fault(FaultCode::AdapterErr),
            S0State::Fault(FaultCode::CmdDeadlineExpired),
            S0State::Stopping,
        ] {
            let next =
                try_transition(from, Event::AdapterErr(FaultCode::SafetyStopFailed), &g).unwrap();
            assert_eq!(
                next,
                S0State::Fault(FaultCode::SafetyStopFailed),
                "SafetyStopFailed should drop {from:?} into Fault(SafetyStopFailed)"
            );
        }
    }

    #[test]
    fn safety_stop_failed_does_not_break_normal_adapter_err_branch() {
        // 确认普通 AdapterErr(AdapterErr) 仍走 Enabled.* → Fault(AdapterErr) 旧路径
        let g = healthy_guards();
        let next = try_transition(
            enabled_active(),
            Event::AdapterErr(FaultCode::AdapterErr),
            &g,
        )
        .unwrap();
        assert_eq!(next, S0State::Fault(FaultCode::AdapterErr));
    }

    // ---- * → STOPPING ----

    #[test]
    fn stop_from_any_state() {
        let g = healthy_guards();
        for from in [
            S0State::Safe,
            S0State::Ready,
            enabled_active(),
            S0State::Fault(FaultCode::AdapterErr),
        ] {
            let next = try_transition(from, Event::Stop, &g).unwrap();
            assert_eq!(next, S0State::Stopping);
        }
    }

    // ---- 拒绝路径 ----

    #[test]
    fn ready_heartbeat_timeout_is_rejected() {
        let g = healthy_guards();
        let r = try_transition(S0State::Ready, Event::HeartbeatTimeout, &g).unwrap_err();
        assert_eq!(r.from, S0State::Ready);
    }

    #[test]
    fn safe_heartbeat_is_rejected() {
        let g = healthy_guards();
        assert!(try_transition(S0State::Safe, Event::Heartbeat, &g).is_err());
    }

    #[test]
    fn default_state_is_safe() {
        assert_eq!(S0State::default(), S0State::Safe);
    }

    #[test]
    fn output_mode_from_sub() {
        assert_eq!(OutputMode::from_sub(EnabledSub::Active), OutputMode::Active);
        assert_eq!(
            OutputMode::from_sub(EnabledSub::CtrlLostHold),
            OutputMode::Hold
        );
        assert_eq!(
            OutputMode::from_sub(EnabledSub::CtrlLostZero),
            OutputMode::Zero
        );
        assert_eq!(
            OutputMode::from_sub(EnabledSub::CtrlLostStop),
            OutputMode::Stop
        );
    }

    // ---- S0StateSnapshot ----

    #[test]
    fn snapshot_default_is_safe_zero_stamp() {
        let snap = S0StateSnapshot::default();
        assert_eq!(snap.state, S0State::Safe);
        assert_eq!(snap.stamp_ns, 0);
        assert!(!snap.guards_summary.estop_latched);
    }

    #[test]
    fn snapshot_new_roundtrip_fields() {
        let g = healthy_guards();
        let snap = S0StateSnapshot::new(enabled_active(), g, 42);
        assert_eq!(snap.state, enabled_active());
        assert_eq!(snap.stamp_ns, 42);
        assert!(snap.guards_summary.heartbeat_ok);
    }

    #[test]
    fn snapshot_is_copy_and_eq() {
        // Compile-time check via assignment + direct PartialEq
        let g = healthy_guards();
        let snap = S0StateSnapshot::new(S0State::Ready, g, 7);
        let copy = snap; // move is actually copy since S0StateSnapshot: Copy
        assert_eq!(snap, copy);
        // snap still usable after "move"
        assert_eq!(snap.stamp_ns, 7);
    }

    #[test]
    fn snapshot_size_within_budget() {
        // 运行时 sanity check（const _ 断言已在 compile time 验证；这里留一份显式日志用）。
        let sz = core::mem::size_of::<S0StateSnapshot>();
        assert!(sz <= 64, "S0StateSnapshot grew to {sz} B, must stay ≤ 64 B");
    }

    /// 集成测试：`S0StateSnapshot` 经 seqlock publish/load 语义正确。
    ///
    /// 本测试同时在**编译期**证明 `S0StateSnapshot: Copy + Send + 'static`
    /// —— `RtSnapshotPublisher<T>` 泛型 bound 为 `T: Copy + Send + 'static`，
    /// 实例化失败即编译失败。这是 Part 2 `ipc_thread.state_pub` / `pub_thread.state_rdr`
    /// 接线的前置条件。
    #[test]
    fn snapshot_roundtrips_through_seqlock() {
        use robot_transport::RtSnapshotPublisher;

        let initial = S0StateSnapshot::default();
        let (pubr, rdr) = RtSnapshotPublisher::new(initial);
        assert_eq!(rdr.load(), initial);

        let g = healthy_guards();
        let snap = S0StateSnapshot::new(enabled_active(), g, 12_345_000);
        pubr.publish(&snap);

        let loaded = rdr.load();
        assert_eq!(loaded, snap);
        assert_eq!(loaded.state, enabled_active());
        assert_eq!(loaded.stamp_ns, 12_345_000);
        assert!(loaded.guards_summary.heartbeat_ok);
    }

    // ============================================================================
    // task-30 v9 / M7：plan_transition / commit_transition / commit_fault /
    // EventQueue 单测
    // ============================================================================

    /// **plan_transition vs try_transition 一致性**：同 `(cur, ev, guards)`
    /// 下两者对 new_state 必须语义等价。任何 transition 表演化两边一起改。
    #[test]
    fn plan_and_try_transition_agree_on_state() {
        let g = healthy_guards();
        // 覆盖关键 state × event 组合
        let cases = [
            (S0State::Safe, Event::Enable),
            (S0State::Ready, Event::Heartbeat),
            (enabled_active(), Event::EStop),
            (enabled_active(), Event::AdapterErr(FaultCode::AdapterErr)),
            (enabled_active(), Event::HeartbeatTimeout),
            (S0State::Safe, Event::EStop),
            (S0State::Stopping, Event::EStop),
            (
                S0State::Safe,
                Event::AdapterErr(FaultCode::SafetyStopFailed),
            ),
            (S0State::Fault(FaultCode::AdapterErr), Event::ClearError),
        ];
        for (cur, ev) in cases {
            let plan = plan_transition(cur, ev, &g);
            let try_res = try_transition(cur, ev, &g);
            match (plan, try_res) {
                (Some(p), Ok(s)) => {
                    assert_eq!(p.new_state, s, "plan vs try mismatch for ({cur:?}, {ev:?})");
                }
                (None, Err(_)) => {} // 双方都拒，OK
                (Some(p), Err(e)) => {
                    panic!("plan returned Some({p:?}) but try rejected: {e:?}")
                }
                (None, Ok(s)) => {
                    panic!("plan returned None but try returned Ok({s:?}) for ({cur:?}, {ev:?})")
                }
            }
        }
    }

    /// EStop 任意态都是 Emergency + StopTorque。
    #[test]
    fn plan_estop_is_emergency() {
        let g = healthy_guards();
        for cur in [
            S0State::Safe,
            S0State::Ready,
            enabled_active(),
            S0State::Fault(FaultCode::AdapterErr),
            S0State::Stopping,
        ] {
            let plan = plan_transition(cur, Event::EStop, &g).unwrap();
            assert_eq!(
                plan.kind,
                TransitionKind::Emergency,
                "EStop from {cur:?} must be Emergency"
            );
            assert_eq!(plan.new_state, S0State::Safe);
            assert_eq!(plan.new_policy, OutputPolicy::StopTorque);
        }
    }

    /// `AdapterErr(SafetyStopFailed)` 任意态都是 Emergency + Disabled
    /// （v9 §0.5 A3 + §pre-2 `* → Fault(SafetyStopFailed)`）。
    #[test]
    fn plan_safety_stop_failed_is_emergency() {
        let g = healthy_guards();
        for cur in [
            S0State::Safe,
            S0State::Ready,
            enabled_active(),
            S0State::Fault(FaultCode::AdapterErr),
        ] {
            let plan =
                plan_transition(cur, Event::AdapterErr(FaultCode::SafetyStopFailed), &g).unwrap();
            assert_eq!(plan.kind, TransitionKind::Emergency);
            assert_eq!(plan.new_state, S0State::Fault(FaultCode::SafetyStopFailed));
            assert_eq!(plan.new_policy, OutputPolicy::Disabled);
        }
    }

    /// `READY → ENABLED.Active`（`Heartbeat`）必须是 Effectful + Enable(true)。
    /// **finding M7 关键**：state machine 决定上扭后必须先排 adapter
    /// `Enable(true)` action 等 ack 才能 commit Enabled，避免对外汇报 Enabled
    /// 但硬件未真上扭的窗口。
    #[test]
    fn plan_ready_to_enabled_is_effectful_enable_true() {
        let g = healthy_guards();
        let plan = plan_transition(S0State::Ready, Event::Heartbeat, &g).unwrap();
        match plan.kind {
            TransitionKind::Effectful {
                action: AdapterNormalReq::Enable(true),
            } => {}
            other => panic!("expected Effectful{{Enable(true)}}, got {other:?}"),
        }
        assert_eq!(plan.new_policy, OutputPolicy::ActiveForward);
    }

    /// `Pure` transition：SAFE → READY 守卫满足 → 立即可 commit。
    #[test]
    fn plan_safe_to_ready_is_pure() {
        let g = healthy_guards();
        let plan = plan_transition(S0State::Safe, Event::Enable, &g).unwrap();
        assert_eq!(plan.kind, TransitionKind::Pure);
        assert_eq!(plan.new_state, S0State::Ready);
        assert_eq!(plan.new_policy, OutputPolicy::ReadyHold);
    }

    /// commit_transition 写入 state；commit_fault 直接拍 Fault。
    #[test]
    fn commit_transition_and_fault() {
        let g = healthy_guards();
        let mut state = S0State::Safe;
        let plan = plan_transition(S0State::Safe, Event::Enable, &g).unwrap();
        commit_transition(&mut state, &plan);
        assert_eq!(state, S0State::Ready);

        commit_fault(&mut state, FaultCode::AdapterErr);
        assert_eq!(state, S0State::Fault(FaultCode::AdapterErr));

        commit_fault(&mut state, FaultCode::SafetyStopFailed);
        assert_eq!(state, S0State::Fault(FaultCode::SafetyStopFailed));
    }

    // ---- EventQueue + drain_emergency ----

    #[test]
    fn event_queue_fifo_push_pop() {
        let mut q = EventQueue::new();
        assert!(q.is_empty());
        q.push(Event::Heartbeat);
        q.push(Event::Enable);
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop(), Some(Event::Heartbeat));
        assert_eq!(q.pop(), Some(Event::Enable));
        assert_eq!(q.pop(), None);
    }

    /// **v9 §0.5 A1 关键**：drain_emergency 摘 EStop / SafetyStopFailed，
    /// 其它事件原序保留。
    #[test]
    fn event_queue_drain_emergency_estop_first() {
        let mut q = EventQueue::new();
        q.push(Event::Heartbeat);
        q.push(Event::EStop);
        q.push(Event::Enable);
        q.push(Event::AdapterErr(FaultCode::AdapterErr));

        // drain 出 EStop（第一个 emergency）
        assert_eq!(q.drain_emergency(), Some(Event::EStop));
        // 其它事件原序保留
        assert_eq!(q.pop(), Some(Event::Heartbeat));
        assert_eq!(q.pop(), Some(Event::Enable));
        assert_eq!(q.pop(), Some(Event::AdapterErr(FaultCode::AdapterErr)));
        assert_eq!(q.pop(), None);
    }

    /// `AdapterErr(SafetyStopFailed)` 也算 emergency；`AdapterErr(其它)` 不算。
    #[test]
    fn event_queue_drain_emergency_safety_stop_failed() {
        let mut q = EventQueue::new();
        q.push(Event::Heartbeat);
        q.push(Event::AdapterErr(FaultCode::AdapterErr)); // **不**是 emergency
        q.push(Event::AdapterErr(FaultCode::SafetyStopFailed)); // 是 emergency
        q.push(Event::Stop);

        assert_eq!(
            q.drain_emergency(),
            Some(Event::AdapterErr(FaultCode::SafetyStopFailed))
        );
        // AdapterErr(AdapterErr) 留下
        assert_eq!(q.pop(), Some(Event::Heartbeat));
        assert_eq!(q.pop(), Some(Event::AdapterErr(FaultCode::AdapterErr)));
        assert_eq!(q.pop(), Some(Event::Stop));
    }

    #[test]
    fn event_queue_drain_emergency_returns_none_when_no_emergency() {
        let mut q = EventQueue::new();
        q.push(Event::Heartbeat);
        q.push(Event::Stop);
        assert_eq!(q.drain_emergency(), None);
        // 队列不变
        assert_eq!(q.len(), 2);
    }

    /// drain_emergency 多次调用：先 EStop 后 SafetyStopFailed（按 push 顺序）。
    #[test]
    fn event_queue_drain_emergency_multiple_returns_in_push_order() {
        let mut q = EventQueue::new();
        q.push(Event::EStop);
        q.push(Event::AdapterErr(FaultCode::SafetyStopFailed));
        q.push(Event::Heartbeat);
        assert_eq!(q.drain_emergency(), Some(Event::EStop));
        assert_eq!(
            q.drain_emergency(),
            Some(Event::AdapterErr(FaultCode::SafetyStopFailed))
        );
        assert_eq!(q.drain_emergency(), None);
        assert_eq!(q.pop(), Some(Event::Heartbeat));
    }

    // ---- v0.5 候选 C：`SetEnabled(false)` = Disable ----

    #[test]
    fn disable_from_enabled_to_safe() {
        let g = healthy_guards();
        assert_eq!(
            try_transition(enabled_active(), Event::Disable, &g),
            Ok(S0State::Safe)
        );
        // plan：Effectful `Enable(false)`（真 torque-off）+ Disabled policy。
        let plan = plan_transition(enabled_active(), Event::Disable, &g).unwrap();
        assert_eq!(plan.new_state, S0State::Safe);
        assert!(matches!(
            plan.kind,
            TransitionKind::Effectful {
                action: AdapterNormalReq::Enable(false)
            }
        ));
        assert_eq!(plan.new_policy, OutputPolicy::Disabled);
    }

    #[test]
    fn disable_from_ready_to_safe_is_pure() {
        let g = healthy_guards();
        assert_eq!(
            try_transition(S0State::Ready, Event::Disable, &g),
            Ok(S0State::Safe)
        );
        // Ready 未上扭 → Pure（无 adapter action）。
        let plan = plan_transition(S0State::Ready, Event::Disable, &g).unwrap();
        assert!(matches!(plan.kind, TransitionKind::Pure));
    }

    #[test]
    fn disable_from_safe_and_fault_is_noop() {
        let g = healthy_guards();
        assert_eq!(
            try_transition(S0State::Safe, Event::Disable, &g),
            Ok(S0State::Safe)
        );
        let fault = S0State::Fault(FaultCode::AdapterErr);
        assert_eq!(try_transition(fault, Event::Disable, &g), Ok(fault));
    }

    #[test]
    fn disable_allowed_when_latched_does_not_check_estop() {
        // Disable 一个 latched 机器人是安全 + 显式的；transition **不**检查
        // estop_latched、**不**清 latch（state machine 不碰 latch 文件）。
        let mut g = healthy_guards();
        g.estop_latched = true;
        assert_eq!(
            try_transition(enabled_active(), Event::Disable, &g),
            Ok(S0State::Safe)
        );
    }
}
