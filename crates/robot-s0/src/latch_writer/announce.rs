//! 命令公告字：IPC 在 `try_send` **之前**发布 `Offered{accepted_at}`，writer 取到命令后
//! CAS 认领，IPC 只在 `try_send` 失败（Full / Disconnected）时撤回。
//!
//! 单字 `AtomicU64`：0 = `None`；非 0 = `Offered`，低位 1、高 63 bit 相对 `base` 的
//! 纳秒。`claim` 与 `withdraw` 都是从同一 `Offered` 值到 0 的 CAS，恰有一方成功。
//! 公告先于发送、成功不再写：监督据此推导"已接纳未开始"的 pickup 截止。

use std::sync::atomic::{AtomicU64, Ordering};

use robot_core::time::MonoRawNs;

const OFFERED: u64 = 1;
const STAMP_MAX: u64 = (1u64 << 63) - 1;

/// `offer` 失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AnnounceError {
    /// 已有公告在途（唯一在途命令）。
    #[error("已有命令公告在途")]
    AlreadyOffered,
    /// 时刻早于 base 或超 63 bit。
    #[error("公告时刻超出编码范围")]
    Overflow,
}

/// 命令公告字。
#[derive(Debug)]
pub struct CommandAnnounce {
    base: MonoRawNs,
    word: AtomicU64,
}

impl CommandAnnounce {
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

    /// 发布公告（在 `try_send` 之前）。
    ///
    /// # Errors
    ///
    /// 见 [`AnnounceError`]；失败不改字。
    pub fn offer(&self, now: MonoRawNs) -> Result<(), AnnounceError> {
        let delta = now
            .0
            .checked_sub(self.base.0)
            .ok_or(AnnounceError::Overflow)?;
        if delta > STAMP_MAX {
            return Err(AnnounceError::Overflow);
        }
        let word = (delta << 1) | OFFERED;
        self.word
            .compare_exchange(0, word, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| AnnounceError::AlreadyOffered)
    }

    /// IPC 撤回（仅 `try_send` 得到 Full / Disconnected 时）；返回是否撤回了在途公告。
    pub fn withdraw(&self) -> bool {
        self.take().is_some()
    }

    /// writer 认领：返回公告的接纳时刻；无公告返回 `None`。
    pub fn claim(&self) -> Option<MonoRawNs> {
        self.take()
    }

    /// 只读：当前在途公告的接纳时刻。
    #[must_use]
    pub fn offered_at(&self) -> Option<MonoRawNs> {
        self.decode(self.word.load(Ordering::Acquire))
    }

    fn take(&self) -> Option<MonoRawNs> {
        let mut current = self.word.load(Ordering::Acquire);
        loop {
            if current == 0 {
                return None;
            }
            match self
                .word
                .compare_exchange_weak(current, 0, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return self.decode(current),
                Err(observed) => current = observed,
            }
        }
    }

    const fn decode(&self, word: u64) -> Option<MonoRawNs> {
        if word == 0 {
            None
        } else {
            Some(MonoRawNs(self.base.0.saturating_add(word >> 1)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn offer_then_claim_returns_accepted_at_and_clears() {
        let announce = CommandAnnounce::new(MonoRawNs(1_000));
        assert_eq!(announce.claim(), None);
        announce.offer(MonoRawNs(1_300)).unwrap();
        assert_eq!(announce.offered_at(), Some(MonoRawNs(1_300)));
        assert_eq!(
            announce.offer(MonoRawNs(1_400)),
            Err(AnnounceError::AlreadyOffered)
        );
        assert_eq!(announce.claim(), Some(MonoRawNs(1_300)));
        assert_eq!(announce.offered_at(), None);
        assert!(!announce.withdraw(), "已认领后撤回无公告可撤");
    }

    #[test]
    fn overflow_does_not_change_word() {
        let announce = CommandAnnounce::new(MonoRawNs(1_000));
        assert_eq!(announce.offer(MonoRawNs(10)), Err(AnnounceError::Overflow));
        assert_eq!(
            announce.offer(MonoRawNs(u64::MAX)),
            Err(AnnounceError::Overflow)
        );
        assert_eq!(announce.offered_at(), None);
    }

    #[test]
    fn withdraw_removes_offer_before_credit_is_returned() {
        let announce = CommandAnnounce::new(MonoRawNs(1_000));
        announce.offer(MonoRawNs(2_000)).unwrap();
        assert!(announce.withdraw());
        assert_eq!(announce.claim(), None);
        announce.offer(MonoRawNs(3_000)).unwrap();
        assert_eq!(announce.offered_at(), Some(MonoRawNs(3_000)));
    }

    /// `claim` 与 `withdraw` 竞争：恰一方成功。
    #[test]
    fn claim_and_withdraw_race_exactly_one_wins() {
        for _ in 0..200 {
            let announce = Arc::new(CommandAnnounce::new(MonoRawNs(1_000)));
            announce.offer(MonoRawNs(2_000)).unwrap();
            let barrier = Arc::new(Barrier::new(2));
            let withdrawer = {
                let announce = Arc::clone(&announce);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    announce.withdraw()
                })
            };
            barrier.wait();
            let claimed = announce.claim();
            let withdrawn = withdrawer.join().unwrap();
            assert_ne!(claimed.is_some(), withdrawn, "恰一方成功");
            assert_eq!(announce.offered_at(), None);
        }
    }
}
