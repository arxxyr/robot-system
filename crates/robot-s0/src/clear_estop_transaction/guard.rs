//! 文件步骤的三相错误记录与步骤守门。
//!
//! 每个文件步骤按 `Before`（守门）→ `Operation`（真实系统调用）→ `After`
//! （仅测试的后置检查点）三相执行。凭证只随 `Operation` 成功推进；`Before`
//! 相的中断不改任何资源，`After` 相的失败发生在资源已经改变之后。守门中断
//! 用独立类型表达，不复用 `io::ErrorKind::Interrupted`，避免被系统调用的
//! `EINTR` 重试吞掉。

use std::io;

use super::ClearTxnStage;

/// 一个文件步骤内失败发生的相位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepPhase {
    /// 步骤守门拒绝，尚未执行任何系统调用。
    Before,
    /// 真实系统调用失败。
    Operation,
    /// 系统调用已成功，之后的后置检查点失败（仅测试注入）。
    After,
}

/// 记录失败的文件步骤、相位与原始 IO 错误；不把部分成功改写成"没有副作用"。
#[derive(Debug, thiserror::Error)]
#[error("Clear 事务文件步骤 {stage:?}（{phase:?} 相）失败: {source}")]
pub struct StageError {
    pub stage: ClearTxnStage,
    pub phase: StepPhase,
    #[source]
    pub source: io::Error,
}

impl StageError {
    pub(super) fn new(stage: ClearTxnStage, phase: StepPhase, source: io::Error) -> Self {
        Self {
            stage,
            phase,
            source,
        }
    }

    pub(super) fn operation(stage: ClearTxnStage, source: io::Error) -> Self {
        Self::new(stage, StepPhase::Operation, source)
    }

    /// 若本错误来自步骤守门中断，返回其原因。
    #[must_use]
    pub fn guard_interrupt(&self) -> Option<InterruptReason> {
        self.source
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<GuardInterrupt>())
            .map(|interrupt| interrupt.reason)
    }
}

/// 步骤守门中断的原因；由调用方（唯一 writer）决定，文件层不解释。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptReason {
    /// 更高优先级的急停持久化义务待写，清零步骤必须让路。
    EStopObligationPending,
    /// 测试屏障要求在此步骤前停止。
    TestBarrier,
    /// 进程正在关闭，不再开始新的文件步骤。
    ShuttingDown,
}

/// 守门中断；作为 `StageError.source` 的内部错误保存，可通过
/// [`StageError::guard_interrupt`] 取回。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("文件步骤被守门中断: {reason:?}")]
pub struct GuardInterrupt {
    pub reason: InterruptReason,
}

impl GuardInterrupt {
    #[must_use]
    pub const fn new(reason: InterruptReason) -> Self {
        Self { reason }
    }

    pub(super) fn into_io_error(self) -> io::Error {
        io::Error::other(self)
    }
}

/// 每个文件步骤开始前调用；返回 `Err` 即以该步骤为失败阶段、相位为 `Before` 中断。
pub trait StepGuard {
    fn before(&mut self, stage: ClearTxnStage) -> Result<(), GuardInterrupt>;

    #[cfg(feature = "test-comm-fault-hook")]
    fn after_success(&mut self, _stage: ClearTxnStage) {}

    #[cfg(feature = "test-comm-fault-hook")]
    fn directory_sync_input(&mut self, _stage: ClearTxnStage) -> DirectorySyncInput {
        DirectorySyncInput::Original
    }

    #[cfg(feature = "test-comm-fault-hook")]
    fn directory_sync_event(&mut self, _stage: ClearTxnStage, _event: DirectorySyncEvent) {}
}

/// 测试仅选择本次同步的输入 fd；不替换 store 的原目录所有权。
#[cfg(feature = "test-comm-fault-hook")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectorySyncInput {
    Original,
    OPathAlias,
}

#[cfg(feature = "test-comm-fault-hook")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasPreparation {
    Open,
    Metadata,
    Identity,
}

/// 来自别名准备和真实 fsync 调用点的只读事实；准备失败不代表执行过 fsync。
#[cfg(feature = "test-comm-fault-hook")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectorySyncEvent {
    Expected(super::FileIdentity),
    Verified(super::FileIdentity),
    PreparationFailed {
        phase: AliasPreparation,
        errno: Option<i32>,
    },
    Call,
    Returned {
        success: bool,
        errno: Option<i32>,
    },
}

/// 永不中断的守门；启动恢复与不需要让路的调用方使用。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoGuard;

impl StepGuard for NoGuard {
    fn before(&mut self, _stage: ClearTxnStage) -> Result<(), GuardInterrupt> {
        Ok(())
    }

    #[cfg(feature = "test-comm-fault-hook")]
    fn after_success(&mut self, stage: ClearTxnStage) {
        crate::comm_fault_hook::w4_recovery_kill::after_success(stage);
    }
}
