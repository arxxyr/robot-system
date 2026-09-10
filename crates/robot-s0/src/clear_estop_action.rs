//! Clear 只读预检。真实解除仅通过 `clear_coordination` 的拥有事务执行。
use crate::state_machine::{Guards, S0State};
use robot_core::executor_ctrl::{ExecutorCtrlClient, OutputSafetyState};
use robot_core::time::RobotClock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClearEStopDryRunOutcome {
    WouldClear { would_clear_at_ns: u64 },
    StateNotSafe { actual: S0State },
    NotTorqueOffConfirmed { actual: OutputSafetyState },
}
/// 不创建事务、修改权限或访问文件；guard 副本不能替代实际执行器断扭观测。
pub(crate) fn evaluate_clear_estop_dry_run(
    state: S0State,
    _guards: &Guards,
    ctrl: &dyn ExecutorCtrlClient,
    clock: &dyn RobotClock,
) -> ClearEStopDryRunOutcome {
    match (state, ctrl.safety_state()) {
        (S0State::Safe, OutputSafetyState::TorqueOffConfirmed) => {
            ClearEStopDryRunOutcome::WouldClear {
                would_clear_at_ns: clock.now().0,
            }
        }
        (S0State::Safe, actual) => ClearEStopDryRunOutcome::NotTorqueOffConfirmed { actual },
        (actual, _) => ClearEStopDryRunOutcome::StateNotSafe { actual },
    }
}
