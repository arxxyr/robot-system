//! 权限字的位布局与纯函数编解码；本文件不含任何原子操作。
//!
//! | 位 | 字段 |
//! |---|---|
//! | 0–7 | `OutputSafetyState` 既有 wire 编码 |
//! | 8 | serial EStop pending |
//! | 9 | IPC EStop notice |
//! | 10 | `NORMAL_IO_ACTIVE` |
//! | 11 | Latch |
//! | 12–14 | Phase（7 非法） |
//! | 15 | Disabled |
//! | 16 | Shutdown |
//! | 17 | Exhausted |
//! | 18–23 | 六类 inhibit（与 `hard_rt_ctrl::inhibit` 同值左移 18 位） |
//! | 24–63 | 40 位 permission epoch |

use robot_core::executor_ctrl::OutputSafetyState;

use super::{InhibitSet, Phase};

pub(super) const SAFETY_MASK: u64 = 0xff;
pub(super) const SERIAL_ESTOP_PENDING: u64 = 1 << 8;
pub(super) const IPC_ESTOP_NOTICE: u64 = 1 << 9;
pub(super) const NORMAL_IO_ACTIVE: u64 = 1 << 10;
pub(super) const LATCH: u64 = 1 << 11;
pub(super) const PHASE_SHIFT: u32 = 12;
pub(super) const PHASE_MASK: u64 = 0b111 << PHASE_SHIFT;
pub(super) const DISABLED: u64 = 1 << 15;
pub(super) const SHUTDOWN: u64 = 1 << 16;
pub(super) const EXHAUSTED: u64 = 1 << 17;
pub(super) const INHIBIT_SHIFT: u32 = 18;
pub(super) const INHIBIT_MASK: u64 = 0x3f << INHIBIT_SHIFT;
pub(super) const EPOCH_SHIFT: u32 = 24;
pub(super) const EPOCH_MAX: u64 = (1 << 40) - 1;

/// 推进 epoch 的结果：是否真正推进，以及推进后是否处于 Exhausted。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Advance {
    pub advanced: bool,
    pub exhausted: bool,
}

/// 权限字的 `Copy` 快照；所有变更都返回新值，由调用方 CAS 提交。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Word(u64);

impl Word {
    pub(super) const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub(super) const fn raw(self) -> u64 {
        self.0
    }

    pub(super) const fn has(self, bits: u64) -> bool {
        self.0 & bits != 0
    }

    #[must_use]
    pub(super) const fn set(self, bits: u64) -> Self {
        Self(self.0 | bits)
    }

    #[must_use]
    pub(super) const fn clear(self, bits: u64) -> Self {
        Self(self.0 & !bits)
    }

    pub(super) fn safety(self) -> OutputSafetyState {
        OutputSafetyState::from_u8(u8::try_from(self.0 & SAFETY_MASK).unwrap_or(u8::MAX))
    }

    #[must_use]
    pub(super) fn with_safety(self, state: OutputSafetyState) -> Self {
        Self((self.0 & !SAFETY_MASK) | u64::from(state.to_u8()))
    }

    pub(super) fn phase_bits(self) -> u8 {
        u8::try_from((self.0 & PHASE_MASK) >> PHASE_SHIFT).unwrap_or(u8::MAX)
    }

    pub(super) fn phase(self) -> Option<Phase> {
        Phase::from_bits(self.phase_bits())
    }

    #[must_use]
    pub(super) fn with_phase(self, phase: Phase) -> Self {
        Self((self.0 & !PHASE_MASK) | (u64::from(phase.bits()) << PHASE_SHIFT))
    }

    pub(super) fn inhibits(self) -> InhibitSet {
        InhibitSet::from_bits_truncate(
            u8::try_from((self.0 & INHIBIT_MASK) >> INHIBIT_SHIFT).unwrap_or(u8::MAX),
        )
    }

    #[must_use]
    pub(super) fn with_inhibits(self, set: InhibitSet) -> Self {
        Self((self.0 & !INHIBIT_MASK) | (u64::from(set.bits()) << INHIBIT_SHIFT))
    }

    pub(super) const fn epoch(self) -> u64 {
        self.0 >> EPOCH_SHIFT
    }

    /// 调用方保证 `epoch <= EPOCH_MAX`；超界位被截断而不是溢出到其它字段。
    #[must_use]
    pub(super) const fn with_epoch(self, epoch: u64) -> Self {
        Self((self.0 & ((1 << EPOCH_SHIFT) - 1)) | ((epoch & EPOCH_MAX) << EPOCH_SHIFT))
    }

    /// 下一权限 epoch；已到 `EPOCH_MAX` 时没有下一代次，永不回绕。
    pub(super) const fn next_epoch(self) -> Option<u64> {
        let epoch = self.epoch();
        if epoch < EPOCH_MAX {
            Some(epoch + 1)
        } else {
            None
        }
    }

    /// 推进权限 epoch；没有下一代次时置 Exhausted + Disabled，保留其它所有位。
    #[must_use]
    pub(super) const fn advance_or_exhaust(self) -> (Self, Advance) {
        match self.next_epoch() {
            Some(next) => (
                self.with_epoch(next),
                Advance {
                    advanced: true,
                    exhausted: self.has(EXHAUSTED),
                },
            ),
            None => (
                self.set(EXHAUSTED | DISABLED),
                Advance {
                    advanced: false,
                    exhausted: true,
                },
            ),
        }
    }
}
