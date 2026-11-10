//! S0 状态与转换判定：5 主态、ENABLED 的 4 子态及控制事件。
//!
//! 对应 `docs/00_design.md` §1.4（冻结的转换表）+ §6.7 核心不变量。
//!
//! IPC 运行时负责消费控制请求、心跳、执行器故障与停止事件，填写守卫并
//! 发布状态快照。本模块判定状态转换，不直接执行设备 I/O 或持久化。
//!
//! `HardRT` 执行器与 `SerialRT` 收敛路径分别处理物理操作；状态转换不能
//! 代替启停确认、持久化结果或原控制权协议。
//!
//! [`Guards`] 是调用方在转换前填写的事实快照，例如连接、校准、心跳及
//! 命令截止状态；`on_timeout_hint` 来自最近命令的超时策略。

#![allow(clippy::doc_markdown)]

use robot_core::adapter::ControlMode;

// ============================================================================
// 枚举定义
// ============================================================================

/// ENABLED 的 4 个子态。`CtrlLost{Hold,Zero,Stop}` 三者对应 `JointCommand.on_timeout`
/// 的三种策略（§1.4），由控制链路失活事件与当前超时策略选择。
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

/// Fault 代码，涵盖执行器故障、超时、限位、通信不可信及停止失败。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultCode {
    /// SerialRT 通信不可信，必须由执行器完成本地收敛。
    CommFault,
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
    /// `Fault(AdapterErr)`（见 [`crate::ipc_thread`]）。
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

/// 控制事件集合；由 IPC 运行时、执行器与 watchdog 路径提供事实。
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
/// `on_timeout_hint` 默认 `Hold`，运行时按最近一条 `JointCommand.on_timeout` 更新。
///
/// 字段 bool 多（对齐 §1.4 转换表的 9 条守卫条件），clippy
/// `struct_excessive_bools` 在此 allow —— 这些 bool 语义互相独立，聚合成 bitflags
/// 反而降低可读性（每条转换规则都引用独立字段）。
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Guards {
    /// SerialRT 使用独立通信故障与急停收敛协议。
    pub rt_serial: bool,
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
            rt_serial: false,
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
/// `ipc_thread` 是 state machine 的执行者，每 tick 消费事件调
/// [`try_transition`] 后必须把结果发布给 `pub_thread`（它负责对外 iceoryx2 publish
/// `JointState` + S0 自身 Heartbeat）。同步原语复用现有 `RtSnapshotPublisher<T>`
/// seqlock —— S48 A3b 起要求 `T: SnapshotValue`（显式字段 codec，见
/// [`crate::state_snapshot_codec`]），`Copy` 本身不再被当作跨线程搬运的安全依据。
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
// + 原持责执行路径
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
        return (
            TransitionKind::Emergency,
            match new_state {
                S::Fault(_) => OutputPolicy::Disabled,
                _ => OutputPolicy::StopTorque,
            },
        );
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
    if let S0State::Fault(previous) = state
        && fault_severity(*previous) >= fault_severity(fault)
    {
        return;
    }
    *state = S0State::Fault(fault);
}

const fn fault_severity(fault: FaultCode) -> u8 {
    match fault {
        FaultCode::SafetyStopFailed => 3,
        FaultCode::CommFault => 2,
        _ => 1,
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
        (S::Fault(_), E::EStop) if g.rt_serial => Ok(cur),
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

        (S::Safe | S::Ready | S::Enabled { .. }, E::AdapterErr(FaultCode::CommFault)) => {
            Ok(S::Fault(FaultCode::CommFault))
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
        // (Safe, ClearEStop) 只表达业务允许，不能授予清锁权限。
        // 实际 live RR 由 clear_coordination 预约原 authority、等待唯一
        // writer 的文件完成并真实退休。裸控制事件不能绕过该拥有事务。
        //
        // 这条 transition 故意不检查 `g.estop_latched`——真正的硬件断扭
        // 确认由真实 authority 的断扭观测守门，不由 guard 副本代替。
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
    /// 本测试同时在**编译期**证明 `S0StateSnapshot: SnapshotValue`
    /// —— `RtSnapshotPublisher<T>` 泛型 bound 为 `T: SnapshotValue`（S48 A3b，
    /// codec 见 `state_snapshot_codec`），实例化失败即编译失败。这是 Part 2
    /// `ipc_thread.state_pub` / `pub_thread.state_rdr` 接线的前置条件。
    #[test]
    fn snapshot_roundtrips_through_seqlock() {
        use robot_transport::RtSnapshotPublisher;

        let initial = S0StateSnapshot::default();
        let (mut pubr, rdr) = RtSnapshotPublisher::new(initial);
        assert_eq!(rdr.load(), initial);

        let g = healthy_guards();
        let snap = S0StateSnapshot::new(enabled_active(), g, 12_345_000);
        pubr.publish(&snap).unwrap();

        let loaded = rdr.load();
        assert_eq!(loaded, snap);
        assert_eq!(loaded.state, enabled_active());
        assert_eq!(loaded.stamp_ns, 12_345_000);
        assert!(loaded.guards_summary.heartbeat_ok);
    }

    // ============================================================================
    // task-30 v9 / M7：plan_transition / commit_transition / commit_fault /
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
