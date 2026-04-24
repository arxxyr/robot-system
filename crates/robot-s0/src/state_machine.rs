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

/// Fault 代码。对应 §1.4 ENABLED → FAULT 的 5 条触发边。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultCode {
    AdapterErr,
    CmdDeadlineExpired,
    LimitViolation,
    WatchdogTimeout,
    RtOverrun,
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
#[allow(clippy::too_many_lines)]
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

        // ---- SAFE → READY ----
        (S::Safe, E::Enable) => {
            if g.ready_guards_pass() {
                Ok(S::Ready)
            } else {
                reject("SAFE→READY guards not satisfied")
            }
        }

        // ---- SAFE → SAFE（解除 EStop）----
        (S::Safe, E::ClearEStop) => {
            if g.estop_latched {
                // Part 2：ClearEStop RPC 在 estop_latch 层已经清理 latch；到这里应该是 false。
                reject("ClearEStop arrived but estop still latched")
            } else {
                // 已经没 latch，什么都不做
                Ok(S::Safe)
            }
        }

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
}
