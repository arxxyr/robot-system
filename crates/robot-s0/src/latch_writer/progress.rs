//! writer 真实阶段戳：单字 `AtomicU64`，低 3 bit 阶段、高 61 bit 相对 `base` 的进入时刻。
//!
//! 每次 `store` 都是实际进入该阶段的时刻，永不清零；`FileStep` 戳是
//! `WriterGuard::before` 的第一条实际语句，测试屏障在其后。监督线程读 → 算 → 退出前
//! 重读。`load` 遇到非法阶段位（6 / 7）显式返回错误，不猜测。
//!
//! [`ExitStamp`] 在 `run_writer` 入口构造、最后析构：`Drop` 发布 `Exited`，晚于
//! `ClaimGuard` 的责任恢复（先恢复责任，后宣告退出）。

use std::sync::atomic::{AtomicU64, Ordering};

use robot_core::time::{MonoRawNs, RobotClock};

const PHASE_BITS: u32 = 3;
const PHASE_MASK: u64 = (1 << PHASE_BITS) - 1;
const STAMP_MAX: u64 = (1u64 << (64 - PHASE_BITS)) - 1;

/// writer 阶段；`6` / `7` 非法，本模块从不产生。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle = 0,
    /// 已取到命令 / 义务，尚未进入第一个文件步骤。
    Claiming = 1,
    /// 一个文件步骤的 `Before` 相已开始（守门第一条语句）。
    FileStep = 2,
    /// 命令已执行完，正在把完成送回 IPC。
    Delivering = 3,
    /// 义务写入失败后的退避。
    RetryBackoff = 4,
    /// writer 已退出（`ExitStamp` Drop 发布）。
    Exited = 5,
}

impl Phase {
    #[must_use]
    pub const fn from_bits(bits: u64) -> Option<Self> {
        match bits {
            0 => Some(Self::Idle),
            1 => Some(Self::Claiming),
            2 => Some(Self::FileStep),
            3 => Some(Self::Delivering),
            4 => Some(Self::RetryBackoff),
            5 => Some(Self::Exited),
            _ => None,
        }
    }
}

/// 进入时刻无法编码（早于 base 或超 61 bit）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("进度戳超出 61 bit 相对时间编码范围")]
pub struct ProgressOverflow;

/// 字中的阶段位非法（6 / 7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("进度字阶段位非法: {0}")]
pub struct ProgressDecodeError(pub u64);

/// 只读视图：阶段与实际进入时刻。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressView {
    pub phase: Phase,
    pub entered_at: MonoRawNs,
}

/// 纯解码：供监督与测试直接核字。
///
/// # Errors
///
/// 阶段位为 6 / 7。
pub const fn decode(raw: u64, base: MonoRawNs) -> Result<ProgressView, ProgressDecodeError> {
    let bits = raw & PHASE_MASK;
    let Some(phase) = Phase::from_bits(bits) else {
        return Err(ProgressDecodeError(bits));
    };
    Ok(ProgressView {
        phase,
        entered_at: MonoRawNs(base.0.saturating_add(raw >> PHASE_BITS)),
    })
}

/// writer 进度字。
#[derive(Debug)]
pub struct WriterProgress {
    base: MonoRawNs,
    word: AtomicU64,
}

impl WriterProgress {
    #[must_use]
    pub const fn new(base: MonoRawNs) -> Self {
        Self {
            base,
            word: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub const fn base(&self) -> MonoRawNs {
        self.base
    }

    /// 记录实际进入 `phase` 的时刻。
    ///
    /// # Errors
    ///
    /// `now` 早于 base 或超 61 bit：不写入、返回 [`ProgressOverflow`]。
    pub fn store(&self, phase: Phase, now: MonoRawNs) -> Result<(), ProgressOverflow> {
        let delta = now.0.checked_sub(self.base.0).ok_or(ProgressOverflow)?;
        if delta > STAMP_MAX {
            return Err(ProgressOverflow);
        }
        self.word
            .store((delta << PHASE_BITS) | phase as u64, Ordering::Release);
        Ok(())
    }

    /// 时刻无法编码时仍发布阶段，时间位写 0（= base，只会让截止保守提前）。
    pub fn store_unstamped(&self, phase: Phase) {
        self.word.store(phase as u64, Ordering::Release);
    }

    /// 读取阶段与进入时刻。
    ///
    /// # Errors
    ///
    /// 阶段位非法（6 / 7）。
    pub fn load(&self) -> Result<ProgressView, ProgressDecodeError> {
        decode(self.word.load(Ordering::Acquire), self.base)
    }

    #[must_use]
    pub fn raw(&self) -> u64 {
        self.word.load(Ordering::Acquire)
    }

    /// 仅测试：写入原始字（用于制造非法阶段位）。
    #[cfg(test)]
    pub(super) fn store_raw(&self, raw: u64) {
        self.word.store(raw, Ordering::Release);
    }
}

/// `Drop` 时发布 `Exited`；在 `run_writer` 入口构造，最后析构。
#[must_use = "ExitStamp 只有存活到函数末尾才能保证 Exited 最后发布"]
pub struct ExitStamp<'a> {
    progress: &'a WriterProgress,
    clock: &'a dyn RobotClock,
}

impl<'a> ExitStamp<'a> {
    pub const fn new(progress: &'a WriterProgress, clock: &'a dyn RobotClock) -> Self {
        Self { progress, clock }
    }
}

impl Drop for ExitStamp<'_> {
    fn drop(&mut self) {
        let now = self.clock.now();
        if self.progress.store(Phase::Exited, now).is_err() {
            self.progress.store_unstamped(Phase::Exited);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64 as Counter;

    struct FixedClock(Counter);

    impl RobotClock for FixedClock {
        fn now(&self) -> MonoRawNs {
            MonoRawNs(self.0.load(Ordering::Relaxed))
        }

        fn sleep_until(&self, _deadline: robot_core::time::Deadline) {}

        fn sleep_for(&self, _d: robot_core::time::DurationNs) {}
    }

    #[test]
    fn store_and_load_keep_real_entry_time() {
        let progress = WriterProgress::new(MonoRawNs(1_000));
        assert_eq!(
            progress.load().unwrap(),
            ProgressView {
                phase: Phase::Idle,
                entered_at: MonoRawNs(1_000)
            }
        );
        progress.store(Phase::FileStep, MonoRawNs(1_250)).unwrap();
        assert_eq!(
            progress.load().unwrap(),
            ProgressView {
                phase: Phase::FileStep,
                entered_at: MonoRawNs(1_250)
            }
        );
        progress.store(Phase::Delivering, MonoRawNs(2_000)).unwrap();
        assert_eq!(progress.load().unwrap().phase, Phase::Delivering);
    }

    #[test]
    fn overflow_and_pre_base_are_explicit_errors_without_writing() {
        let progress = WriterProgress::new(MonoRawNs(1_000));
        progress.store(Phase::Claiming, MonoRawNs(1_100)).unwrap();
        let before = progress.raw();
        assert_eq!(
            progress.store(Phase::FileStep, MonoRawNs(999)),
            Err(ProgressOverflow)
        );
        assert_eq!(
            progress.store(Phase::FileStep, MonoRawNs(u64::MAX)),
            Err(ProgressOverflow)
        );
        assert_eq!(progress.raw(), before, "失败不写入");
        progress.store_unstamped(Phase::Exited);
        assert_eq!(
            progress.load().unwrap(),
            ProgressView {
                phase: Phase::Exited,
                entered_at: MonoRawNs(1_000)
            }
        );
    }

    #[test]
    fn illegal_phase_bits_decode_to_error() {
        let progress = WriterProgress::new(MonoRawNs(0));
        for bits in [6u64, 7] {
            progress.store_raw((42 << PHASE_BITS) | bits);
            assert_eq!(progress.load(), Err(ProgressDecodeError(bits)));
            assert_eq!(decode(bits, MonoRawNs(0)), Err(ProgressDecodeError(bits)));
        }
        assert_eq!(Phase::from_bits(5), Some(Phase::Exited));
        assert_eq!(Phase::from_bits(8), None);
    }

    #[test]
    fn exit_stamp_publishes_exited_on_drop_with_current_time() {
        let progress = WriterProgress::new(MonoRawNs(1_000));
        let clock = FixedClock(Counter::new(5_000));
        {
            let _stamp = ExitStamp::new(&progress, &clock);
            progress.store(Phase::FileStep, MonoRawNs(1_500)).unwrap();
            clock.0.store(6_000, Ordering::Relaxed);
        }
        assert_eq!(
            progress.load().unwrap(),
            ProgressView {
                phase: Phase::Exited,
                entered_at: MonoRawNs(6_000)
            }
        );
        // 时刻早于 base：仍发布 Exited，时间位为 base。
        let early = FixedClock(Counter::new(10));
        {
            let _stamp = ExitStamp::new(&progress, &early);
        }
        assert_eq!(
            progress.load().unwrap(),
            ProgressView {
                phase: Phase::Exited,
                entered_at: MonoRawNs(1_000)
            }
        );
    }
}
