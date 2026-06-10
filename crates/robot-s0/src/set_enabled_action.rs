//! `set_enabled_action` —— v0.5 候选 C `SetEnabled` dry-run precondition 评估。
//!
//! ## 角色
//!
//! `ipc_thread` 的 RR drain 收到 `dry_run=true` 的 `SetEnabled` 请求时，调本
//! 模块 [`evaluate_set_enabled_dry_run`] 做 **read-only** 评估：当前 `state` /
//! `guards` 下，`Event::Enable` / `Event::Disable` 的 **precondition 是否被接受**
//! → 返 [`SetEnabledDryRunOutcome`]。**0 state change / 0 adapter request /
//! 0 latch change**（沿 `clear_estop_action::evaluate_clear_estop_dry_run` 范式）。
//!
//! ## D7 / B4 语义边界（避免候选 A "假 dry-run" 洞）
//!
//! 返回的是 **precondition would accept**，**不**代表 adapter 会 ack、**不**代表
//! enable/disable 真完成（dry-run **不**执行 `AdapterNormalReq::Enable(*)`，无法
//! 预知硬件 ack；enable 的 `Ready→Enabled.Active` 还依赖 heartbeat，dry-run 也
//! 无法预知）。response 文案须写 "precondition would accept" 不写 "would succeed"。
//!
//! ## live 路径复用
//!
//! live（`dry_run=false`）请求的**即时 precondition gate** 也走本评估：`LatchBlocked`
//! / `GuardRejected` 即时拒（不入 pending）；`WouldAccept` 才 push event + 起多
//! tick 观测（D8）。dry-run 与 live 共用同一 gate，杜绝"dry-run 说能、live 却走
//! 不同判断"的漂移。

use crate::state_machine::{EnabledSub, Guards, S0State};

/// `SetEnabled` dry-run / live-precondition 评估结果（D7 precondition-only）。
///
/// `Copy` 友好（仅 `S0State` POD 字段）；与 `ipc_thread::SetEnabledRrPayload`
/// 的 `WouldAccept` / `LatchBlocked` / `GuardRejected` 一一对应。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SetEnabledDryRunOutcome {
    /// precondition 会接受。**不**代表 adapter 会 ack / enable·disable 真完成
    /// （D7/B4）。
    WouldAccept,
    /// `estop_latched` → enable 被拒（latch 下不允许上扭；§6.7 #3 / AC-4）。
    /// 区别于通用 `GuardRejected`：客户端应先 `ClearEStop` 再 enable。
    LatchBlocked,
    /// 其它 guard 不满足（`ready_guards_pass` 失败）或 state 不允许 enable
    /// （`Fault` 需先 `ClearError`，AC-9；`Stopping` / `Enabled` 非 `Active`
    /// 子态）。`actual` 携当前 state 供 gateway 诊断文案。
    GuardRejected { actual: S0State },
}

/// 评估 `SetEnabled(enabled)` 在 `state` / `guards` 下的 precondition。
///
/// **read-only**：不改 `state`（按值收）、不碰 `guards`（只读）、不调 adapter、
/// 不动 latch。
///
/// - `enabled=false`（disable）：**永远** `WouldAccept` —— torque-off 任何时候
///   安全（含 `estop_latched`，D3e/AC-5）；`Safe`/`Fault` 幂等 no-op。
/// - `enabled=true`（enable）：
///   - `estop_latched` → `LatchBlocked`（最具体拒因，先判）。
///   - 已在 `Enabled.Active` → `WouldAccept`（幂等）。
///   - `Safe` / `Ready` → `ready_guards_pass()` 通过则 `WouldAccept`，否则
///     `GuardRejected`。
///   - `Fault` / `Stopping` / `Enabled` 非 `Active` 子态 → `GuardRejected`。
pub(crate) fn evaluate_set_enabled_dry_run(
    state: S0State,
    guards: &Guards,
    enabled: bool,
) -> SetEnabledDryRunOutcome {
    if !enabled {
        return SetEnabledDryRunOutcome::WouldAccept;
    }
    // enable：latch gate 最先（最具体拒因，区别通用 guard 拒）。
    if guards.estop_latched {
        return SetEnabledDryRunOutcome::LatchBlocked;
    }
    match state {
        // 已在 Enabled.Active → 幂等接受。
        S0State::Enabled {
            sub: EnabledSub::Active,
            ..
        } => SetEnabledDryRunOutcome::WouldAccept,
        // Safe：precondition = Safe→Ready 的 ready_guards_pass。
        // Ready：已过 Safe→Ready 正在路上；若 ready guard 中途掉了（如 power）
        //        则同样 GuardRejected。
        S0State::Safe | S0State::Ready => {
            if guards.ready_guards_pass() {
                SetEnabledDryRunOutcome::WouldAccept
            } else {
                SetEnabledDryRunOutcome::GuardRejected { actual: state }
            }
        }
        // Fault（AC-9 需先 ClearError）/ Stopping / Enabled 非 Active 子态 → 拒。
        _ => SetEnabledDryRunOutcome::GuardRejected { actual: state },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_machine::{ControlLink, EnabledSub, OutputMode, S0State};

    /// `ready_guards_pass` 通过的 guard（estop 未 latch + 4 条 ready 守卫 true）。
    fn ready_pass_guards() -> Guards {
        Guards {
            adapter_connected: true,
            calibration_loaded: true,
            joint_state_valid: true,
            power_ok: true,
            ..Guards::default()
        }
    }

    fn enabled_active() -> S0State {
        S0State::Enabled {
            sub: EnabledSub::Active,
            control_link: ControlLink::Ok,
            output_mode: OutputMode::Active,
        }
    }

    #[test]
    fn set_enabled_dry_run_precondition_only() {
        let g = ready_pass_guards();

        // enable happy：Safe + ready guard 过 → WouldAccept。
        assert_eq!(
            evaluate_set_enabled_dry_run(S0State::Safe, &g, true),
            SetEnabledDryRunOutcome::WouldAccept
        );
        // enable 幂等：已 Enabled.Active → WouldAccept。
        assert_eq!(
            evaluate_set_enabled_dry_run(enabled_active(), &g, true),
            SetEnabledDryRunOutcome::WouldAccept
        );

        // latch → LatchBlocked（先于通用 guard 判）。
        let mut latched = g;
        latched.estop_latched = true;
        assert_eq!(
            evaluate_set_enabled_dry_run(S0State::Safe, &latched, true),
            SetEnabledDryRunOutcome::LatchBlocked
        );

        // ready guard 不满足（power 掉）→ GuardRejected。
        let mut no_power = g;
        no_power.power_ok = false;
        assert_eq!(
            evaluate_set_enabled_dry_run(S0State::Safe, &no_power, true),
            SetEnabledDryRunOutcome::GuardRejected {
                actual: S0State::Safe
            }
        );

        // Fault → 拒 enable（AC-9，需先 ClearError）。
        let fault = S0State::Fault(crate::state_machine::FaultCode::AdapterErr);
        assert_eq!(
            evaluate_set_enabled_dry_run(fault, &g, true),
            SetEnabledDryRunOutcome::GuardRejected { actual: fault }
        );

        // disable 永远 WouldAccept —— 含 latched（D3e/AC-5）+ Safe 幂等。
        assert_eq!(
            evaluate_set_enabled_dry_run(enabled_active(), &g, false),
            SetEnabledDryRunOutcome::WouldAccept
        );
        assert_eq!(
            evaluate_set_enabled_dry_run(S0State::Safe, &latched, false),
            SetEnabledDryRunOutcome::WouldAccept
        );
        assert_eq!(
            evaluate_set_enabled_dry_run(fault, &g, false),
            SetEnabledDryRunOutcome::WouldAccept
        );
    }
}
