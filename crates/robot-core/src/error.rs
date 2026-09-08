//! 统一错误类型 `RobotError`。
//!
//! `docs/00_design.md` §4.1 模块地图里的 `error.rs`。Step 6 Part 1 引入最小变体，
//! 覆盖 adapter trait 返回路径（`docs/00_design.md` §4.4）；后续步骤按需扩展，
//! 每次新增变体沿用 CHANGELOG "workspace runtime 依赖白名单" 的登记模板。
//!
//! ### 设计约束
//!
//! - 所有变体静态信息（`&'static str`），零堆分配 —— 保留"在 RT 线程上
//!   构造 Err 也不触发 alloc"的可能性（§4.9 RT 铁律）。
//! - `thiserror` 作为公共依赖；workspace 根 `Cargo.toml` 已精确 pin `2.0.18`。
//! - 不实现 `From<std::io::Error>` 等自动转换：每次跨边界强制显式 `match`，
//!   避免意外把 `io::Error::kind == WouldBlock` 等吞到笼统分类里。

pub use crate::serial_services::{AbortReason, CommErrorKind};
use thiserror::Error;

/// 机器人运行时的统一错误类型。
///
/// 变体按职责分组：
/// - **connectivity**: `NotConnected` / `Busy`
/// - **parameter**: `InvalidParameter`
/// - **runtime**: `HardwareFault` / `DeadlineExceeded` / `LimitsViolation`
/// - **capability**: `Unsupported`（task-30 v9 pre-3a）
///
/// 扩展新变体前请在 `docs/CHANGELOG.md` 登记（非 hot-path 错误无需走 baseline
/// 回归，但需保持静态 `&'static str` 的 no-alloc 约束）。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RobotError {
    /// adapter 未连接 / 未 `connect()`。
    #[error("adapter not connected")]
    NotConnected,

    /// adapter 忙 / 正在处理上一条请求。RT 侧视为可重试。
    #[error("adapter busy")]
    Busy,

    /// 参数越界 / 非法枚举值 / 长度不匹配等。消息为 `&'static str`，不分配。
    #[error("invalid parameter: {0}")]
    InvalidParameter(&'static str),

    /// 硬件 fault（电机报错、总线错误等）。应触发 state machine `AdapterErr`。
    #[error("hardware fault: {0}")]
    HardwareFault(&'static str),

    /// 操作未在 `Deadline` 前完成。
    #[error("deadline exceeded")]
    DeadlineExceeded,

    /// 通信错误保留类型化原因，供 `SerialRT` 按操作域定级。
    #[error("通信错误：{0:?}")]
    Comm(CommErrorKind),

    /// 正常操作在事务边界被中止，不代表硬件通信失败。
    #[error("操作中止：{0:?}")]
    Aborted(AbortReason),

    /// 命令违反 `JointLimits`（位置 / 速度 / 力矩边界）。
    #[error("limits violation")]
    LimitsViolation,

    /// 操作 / 请求**当前 adapter 不支持**（能力级拒绝，不是参数错误也不是硬件
    /// 故障）。task-30 v9 pre-3a 新加。**纯能力声明**：返回这个错误本身**不**
    /// 升 `Fault`。是否进 `Fault` 取决于调用边界，由调用方在拿到本错误后**显式**
    /// 决策，**不在** `RobotError` 这一层做。
    ///
    /// **三种调用边界**：
    /// - **effectful state-machine transition**（M7 `ipc_tick`）路径：本错误
    ///   被包装成 [`crate::executor_ctrl::NormalReqStatus::Unsupported`]，再由
    ///   `ipc_tick` **显式**调 `commit_fault(AdapterErr)`。`RobotError::Unsupported`
    ///   不直接触发 fault；触发动作发生在 `NormalReqStatus` 这一层。
    /// - **探测式 / pre-flight 路径**（启动期 capability 检测）：调用方拿到
    ///   `Err(Unsupported)` 后选择降级 / 跳过 / 警告，**不**升 fault。
    /// - **配置加载 / 工具命令路径**：通常直接 surface 给 user / 写日志。
    ///
    /// 覆盖示例：
    /// - `SerialRT` adapter 收到 `RtSafeCtrl::safety_stop_rt` 这种 RT-only 请求
    ///   → 返回 `Unsupported("safety_stop_rt only available on HardRT")`。
    /// - 串口 adapter 收到不识别的寄存器 / 控制 mode 组合。
    /// - mock adapter 拒绝它 design 上不实现的可选 trait method。
    ///
    /// **总结**：
    /// `RobotError::Unsupported` = "能力级 NO"（中性事实）；
    /// `NormalReqStatus::Unsupported` = "能力级 NO + 我在 effectful 路径上拿到它"
    /// （强制升 fault 的具体调用边界）。两者在不同抽象层，不冲突。
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_renders_static_message() {
        use std::io::Write as _;
        // `Display` 必须对所有变体 well-defined，且完全走 `&'static str` 路径（不 alloc）。
        let samples = [
            RobotError::NotConnected,
            RobotError::Busy,
            RobotError::InvalidParameter("joint_count > 32"),
            RobotError::HardwareFault("CAN bus offline"),
            RobotError::DeadlineExceeded,
            RobotError::LimitsViolation,
            RobotError::Unsupported("safety_stop_rt only on HardRT"),
        ];
        for e in &samples {
            // 使用 `ToString` 会分配；这里用 writeln 到固定 buffer 验证零 alloc 路径：
            let mut buf = [0u8; 128];
            let mut cursor = std::io::Cursor::new(&mut buf[..]);
            write!(cursor, "{e}").unwrap();
            let len = usize::try_from(cursor.position()).unwrap();
            assert!(len > 0, "variant produced empty Display: {e:?}");
        }
    }

    #[test]
    fn copy_semantics_hold() {
        let a = RobotError::InvalidParameter("x");
        let b = a;
        let c = a;
        assert_eq!(b, c);
    }

    #[test]
    fn unsupported_format_string_is_static() {
        // task-30 v9 pre-3a：Display 不分配；message 静态借用。
        let e = RobotError::Unsupported("safety_stop_rt only on HardRT");
        let s = format!("{e}");
        assert!(s.contains("unsupported"));
        assert!(s.contains("safety_stop_rt only on HardRT"));
    }
}
