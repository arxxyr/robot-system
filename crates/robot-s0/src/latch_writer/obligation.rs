//! `EStop` 持久化义务：单字 `AtomicU64` 的 Pending / Active 两位与连续未清偿区间起点。
//!
//! 字布局：bit0 `PENDING`、bit1 `ACTIVE`、高 62 bit `unsettled_since`（相对 `base` 的
//! 纳秒）。`unsettled_since` 是**本轮连续非 settled 区间的起点**：整字从 0 变非 0 时记录，
//! 区间内的 request / claim / 完成 / 恢复一律保留该起点，整字 settled 才归零。它不是
//! 单笔请求时刻，也不是最早未清偿请求的精确时刻；相对新 Pending 允许保守提前告警。
//!
//! 责任不因编码失败丢失：`now < base` 或差值超 62 bit 时仍置 `PENDING`，时间位写 0
//! （= `base`，只会让告警保守提前）并返回 [`RequestOutcome::RegisteredUnstamped`]。
//!
//! 唯一领取：`ACTIVE` 已置即拒绝再次 `begin_write`；[`Claim`] 绑定实例 ID、非 `Clone`、
//! 按值消费；跨实例 `finish_*` 不改字、原样退回。[`ClaimGuard`] 借用同一实例，未消费
//! 即 `Drop` 时执行 `finish_err`（恢复 Pending、起点不变），panic 解栈同样如此。
//!
//! 实例 ID 由全局计数分配，不回绕、不复用，耗尽显式 [`IdExhausted`]。

use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};

use robot_core::time::MonoRawNs;

const PENDING: u64 = 0b01;
const ACTIVE: u64 = 0b10;
const FLAG_BITS: u32 = 2;
const STAMP_BITS: u32 = 64 - FLAG_BITS;
const STAMP_MAX: u64 = (1u64 << STAMP_BITS) - 1;

/// 全局单调实例计数；`fetch_update(checked_add)` 保证不回绕、不复用。
pub(super) struct InstanceCounter(AtomicU64);

impl InstanceCounter {
    pub(super) const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// 分配下一个非零 ID；到达 `u64::MAX` 后显式失败，绝不回绕。
    pub(super) fn next(&self) -> Result<NonZeroU64, IdExhausted> {
        let previous = self
            .0
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| IdExhausted)?;
        NonZeroU64::new(previous + 1).ok_or(IdExhausted)
    }

    /// 仅测试：从指定值开始的局部计数器（用 `u64::MAX` 核耗尽），绝不触碰生产静态计数。
    #[cfg(test)]
    pub(super) const fn starting_at(value: u64) -> Self {
        Self(AtomicU64::new(value))
    }
}

static OBLIGATION_IDS: InstanceCounter = InstanceCounter::new();

/// 生产全局义务 ID 计数器（供同一模块树的受检配对路径引用）。
pub(super) fn global_obligation_ids() -> &'static InstanceCounter {
    &OBLIGATION_IDS
}

/// 实例 ID 计数耗尽；不回绕、不复用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("实例 ID 计数已耗尽，不回绕")]
pub struct IdExhausted;

/// 义务实例的唯一 ID；字段私有、无公开构造。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObligationId(NonZeroU64);

impl ObligationId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// `request` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestOutcome {
    /// 字从 0 变为 Pending，并以 `now` 记录区间起点。
    Registered,
    /// 区间已存在，只置 Pending（或已是 Pending），起点保留。
    Merged,
    /// 字从 0 变为 Pending，但 `now` 无法编码（早于 base 或超界）：时间位为 0，
    /// 告警只会保守提前；责任已登记。
    RegisteredUnstamped,
}

/// `begin_write` 被拒绝的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimRefused {
    /// 没有待清偿的 Pending。
    NoPending,
    /// 已有一笔 Active 写入在途；唯一领取。
    AlreadyActive,
}

/// `finish_ok` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settle {
    /// 整字归零。
    Settled,
    /// 写入期间到达的新 Pending 保留，起点不变。
    PendingRemains,
}

/// 跨实例 `finish_*`：字未改，原 Claim 退回。
#[derive(Debug)]
pub struct ClaimMismatch(pub Claim);

/// 一次被领取的写入责任；按值消费，非 `Clone` / `Copy`。
///
/// ```compile_fail,E0599
/// use robot_s0::latch_writer::obligation::Claim;
/// fn duplicate(claim: Claim) -> Claim { claim.clone() }
/// ```
///
/// ```compile_fail,E0382
/// use robot_s0::latch_writer::obligation::{Claim, EStopObligation};
/// fn twice(obligation: &EStopObligation, claim: Claim) {
///     let _ = obligation.finish_err(claim);
///     let _ = obligation.finish_err(claim);
/// }
/// ```
#[must_use = "Claim 必须经 finish_ok / finish_err 消费，否则责任停留在 Active"]
#[derive(Debug)]
pub struct Claim {
    obligation: ObligationId,
    unsettled_since: u64,
}

impl Claim {
    #[must_use]
    pub const fn obligation(&self) -> ObligationId {
        self.obligation
    }

    /// 领取时字中的连续未清偿区间起点（相对 base 的纳秒；0 = base）。
    #[must_use]
    pub const fn unsettled_since_ns(&self) -> u64 {
        self.unsettled_since
    }
}

/// 只读视图。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObligationView {
    pub pending: bool,
    pub active: bool,
    /// 连续未清偿区间的起点（绝对时刻）；settled 时为 `None`。
    pub unsettled_since: Option<MonoRawNs>,
}

impl ObligationView {
    #[must_use]
    pub const fn is_settled(&self) -> bool {
        !self.pending && !self.active
    }
}

/// `EStop` 持久化义务字。
#[derive(Debug)]
pub struct EStopObligation {
    id: ObligationId,
    base: MonoRawNs,
    word: AtomicU64,
}

impl EStopObligation {
    /// 分配唯一实例 ID；所有时间位相对 `base`。
    ///
    /// # Errors
    ///
    /// 全局实例计数耗尽。
    pub fn new(base: MonoRawNs) -> Result<Self, IdExhausted> {
        Self::new_with_counter(base, &OBLIGATION_IDS)
    }

    /// 与 [`EStopObligation::new`] 同一受检分配路径，只是计数器可注入（测试用局部计数）。
    pub(super) fn new_with_counter(
        base: MonoRawNs,
        counter: &InstanceCounter,
    ) -> Result<Self, IdExhausted> {
        Ok(Self {
            id: ObligationId(counter.next()?),
            base,
            word: AtomicU64::new(0),
        })
    }

    #[must_use]
    pub const fn id(&self) -> ObligationId {
        self.id
    }

    #[must_use]
    pub const fn base(&self) -> MonoRawNs {
        self.base
    }

    /// `now` 相对 base 的可编码戳；早于 base 或超 62 bit 时为 `None`。
    const fn encode_stamp(&self, now: MonoRawNs) -> Option<u64> {
        let Some(delta) = now.0.checked_sub(self.base.0) else {
            return None;
        };
        if delta > STAMP_MAX {
            return None;
        }
        Some(delta << FLAG_BITS)
    }

    const fn decode_stamp(&self, word: u64) -> MonoRawNs {
        MonoRawNs(self.base.0.saturating_add(word >> FLAG_BITS))
    }

    /// 登记一笔 `EStop` 持久化责任。字为 0 → Pending + 起点；非 0 → 置 Pending、起点保留。
    /// 编码失败仍置 Pending（时间位 0），返回 `RegisteredUnstamped`。
    pub fn request(&self, now: MonoRawNs) -> RequestOutcome {
        let mut current = self.word.load(Ordering::Acquire);
        loop {
            let (next, outcome) = if current == 0 {
                match self.encode_stamp(now) {
                    Some(stamp) => (PENDING | stamp, RequestOutcome::Registered),
                    None => (PENDING, RequestOutcome::RegisteredUnstamped),
                }
            } else {
                (current | PENDING, RequestOutcome::Merged)
            };
            if next == current {
                return outcome;
            }
            match self.word.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return outcome,
                Err(observed) => current = observed,
            }
        }
    }

    /// 领取写入：CAS Pending → Active，起点保留；Active 已置即拒绝（唯一领取）。
    ///
    /// # Errors
    ///
    /// 见 [`ClaimRefused`]。
    pub fn begin_write(&self) -> Result<Claim, ClaimRefused> {
        let mut current = self.word.load(Ordering::Acquire);
        loop {
            if current & ACTIVE != 0 {
                return Err(ClaimRefused::AlreadyActive);
            }
            if current & PENDING == 0 {
                return Err(ClaimRefused::NoPending);
            }
            let next = (current & !PENDING) | ACTIVE;
            match self.word.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(Claim {
                        obligation: self.id,
                        unsettled_since: current >> FLAG_BITS,
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// 写入成功：清 Active；无新 Pending 则整字归零，有则保留 Pending 与原起点。
    /// 与并发 `request` 由同一 CAS 线性化，任一方先赢都不清除并发 Pending。
    ///
    /// # Errors
    ///
    /// Claim 属于别的实例：字不改，原样退回。
    pub fn finish_ok(&self, claim: Claim) -> Result<Settle, ClaimMismatch> {
        if claim.obligation != self.id {
            return Err(ClaimMismatch(claim));
        }
        let mut current = self.word.load(Ordering::Acquire);
        loop {
            let (next, settle) = if current & PENDING != 0 {
                (current & !ACTIVE, Settle::PendingRemains)
            } else {
                (0, Settle::Settled)
            };
            match self.word.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(settle),
                Err(observed) => current = observed,
            }
        }
    }

    /// 写入失败：清 Active、重新置 Pending，起点保留。
    ///
    /// # Errors
    ///
    /// Claim 属于别的实例：字不改，原样退回。
    pub fn finish_err(&self, claim: Claim) -> Result<(), ClaimMismatch> {
        if claim.obligation != self.id {
            return Err(ClaimMismatch(claim));
        }
        let mut current = self.word.load(Ordering::Acquire);
        loop {
            let next = (current & !ACTIVE) | PENDING;
            match self.word.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => current = observed,
            }
        }
    }

    /// 借用绑定的领取守卫：`commit` / `fail` 消费，未消费即 `Drop` 时恢复 Pending。
    ///
    /// # Errors
    ///
    /// 见 [`ClaimRefused`]。
    pub fn claim_guard(&self) -> Result<ClaimGuard<'_>, ClaimRefused> {
        let claim = self.begin_write()?;
        Ok(ClaimGuard {
            obligation: self,
            claim: Some(claim),
        })
    }

    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.word.load(Ordering::Acquire) == 0
    }

    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.word.load(Ordering::Acquire) & PENDING != 0
    }

    #[must_use]
    pub fn view(&self) -> ObligationView {
        let word = self.word.load(Ordering::Acquire);
        ObligationView {
            pending: word & PENDING != 0,
            active: word & ACTIVE != 0,
            unsettled_since: (word != 0).then(|| self.decode_stamp(word)),
        }
    }

    /// 仅测试：原始字。
    #[cfg(test)]
    pub(super) fn raw(&self) -> u64 {
        self.word.load(Ordering::Acquire)
    }
}

/// 借用同一实例的领取守卫；不可跨实例（借用绑定），`Drop` 未消费即 `finish_err`。
#[must_use = "守卫未 commit / fail 即 Drop 时会恢复 Pending"]
#[derive(Debug)]
pub struct ClaimGuard<'a> {
    obligation: &'a EStopObligation,
    claim: Option<Claim>,
}

impl ClaimGuard<'_> {
    /// 写入成功。
    #[must_use]
    pub fn commit(mut self) -> Settle {
        let claim = self.claim.take().expect("守卫只消费一次");
        self.obligation
            .finish_ok(claim)
            .unwrap_or_else(|_| unreachable!("守卫借用绑定同一实例"))
    }

    /// 写入失败：恢复 Pending，起点不变。
    pub fn fail(mut self) {
        let claim = self.claim.take().expect("守卫只消费一次");
        self.obligation
            .finish_err(claim)
            .unwrap_or_else(|_| unreachable!("守卫借用绑定同一实例"));
    }

    #[must_use]
    pub fn claim(&self) -> &Claim {
        self.claim.as_ref().expect("守卫尚未消费")
    }
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        if let Some(claim) = self.claim.take() {
            // 借用绑定同一实例，不可能错配；即使错配也不能在 Drop 里 panic。
            let _ = self.obligation.finish_err(claim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Barrier;

    fn obligation() -> EStopObligation {
        EStopObligation::new(MonoRawNs(1_000)).unwrap()
    }

    #[test]
    fn ids_are_unique_and_monotonic() {
        let first = obligation();
        let second = obligation();
        assert!(second.id().get() > first.id().get());
        assert_ne!(first.id(), second.id());
    }

    #[test]
    fn request_registers_stamp_then_merges_keeping_start() {
        let o = obligation();
        assert!(o.is_settled());
        assert_eq!(o.request(MonoRawNs(1_500)), RequestOutcome::Registered);
        let view = o.view();
        assert!(view.pending && !view.active);
        assert_eq!(view.unsettled_since, Some(MonoRawNs(1_500)));
        assert_eq!(o.request(MonoRawNs(9_999)), RequestOutcome::Merged);
        assert_eq!(o.view().unsettled_since, Some(MonoRawNs(1_500)), "起点保留");
    }

    #[test]
    fn out_of_range_request_still_registers_pending_unstamped() {
        let o = obligation();
        // 早于 base。
        assert_eq!(
            o.request(MonoRawNs(10)),
            RequestOutcome::RegisteredUnstamped
        );
        let view = o.view();
        assert!(view.pending, "责任不得因编码失败丢失");
        assert_eq!(
            view.unsettled_since,
            Some(MonoRawNs(1_000)),
            "时间位 0 = base，保守提前"
        );
        let claim = o.begin_write().unwrap();
        assert_eq!(o.finish_ok(claim).unwrap(), Settle::Settled);
        // 超 62 bit。
        assert_eq!(
            o.request(MonoRawNs(u64::MAX)),
            RequestOutcome::RegisteredUnstamped
        );
        assert!(o.has_pending());
        // 非 0 字上的超界请求：Merged，不改起点。
        assert_eq!(o.request(MonoRawNs(0)), RequestOutcome::Merged);
    }

    #[test]
    fn begin_write_is_exclusive_and_keeps_start() {
        let o = obligation();
        assert_eq!(o.begin_write().unwrap_err(), ClaimRefused::NoPending);
        o.request(MonoRawNs(2_000));
        let claim = o.begin_write().unwrap();
        assert_eq!(claim.unsettled_since_ns(), 1_000, "领取携带相对起点");
        let view = o.view();
        assert!(view.active && !view.pending);
        assert_eq!(view.unsettled_since, Some(MonoRawNs(2_000)));
        assert_eq!(o.begin_write().unwrap_err(), ClaimRefused::AlreadyActive);
        // Active 期间新 Pending：Merged，起点保留。
        assert_eq!(o.request(MonoRawNs(3_000)), RequestOutcome::Merged);
        assert_eq!(o.begin_write().unwrap_err(), ClaimRefused::AlreadyActive);
        assert_eq!(o.finish_ok(claim).unwrap(), Settle::PendingRemains);
        let view = o.view();
        assert!(view.pending && !view.active);
        assert_eq!(
            view.unsettled_since,
            Some(MonoRawNs(2_000)),
            "旧起点 t1 保留"
        );
        let claim = o.begin_write().unwrap();
        assert_eq!(o.finish_ok(claim).unwrap(), Settle::Settled);
        assert!(o.is_settled());
        assert_eq!(o.request(MonoRawNs(4_000)), RequestOutcome::Registered);
        assert_eq!(
            o.view().unsettled_since,
            Some(MonoRawNs(4_000)),
            "settled 后新起点"
        );
    }

    #[test]
    fn finish_err_restores_pending_with_original_start() {
        let o = obligation();
        o.request(MonoRawNs(2_000));
        let claim = o.begin_write().unwrap();
        o.finish_err(claim).unwrap();
        let view = o.view();
        assert!(view.pending && !view.active);
        assert_eq!(view.unsettled_since, Some(MonoRawNs(2_000)));
    }

    #[test]
    fn cross_instance_claim_is_returned_without_touching_either_word() {
        let a = obligation();
        let b = obligation();
        a.request(MonoRawNs(2_000));
        b.request(MonoRawNs(2_500));
        let claim_a = a.begin_write().unwrap();
        let raw_a = a.raw();
        let raw_b = b.raw();
        let ClaimMismatch(returned) = b.finish_ok(claim_a).unwrap_err();
        assert_eq!(returned.obligation(), a.id());
        assert_eq!((a.raw(), b.raw()), (raw_a, raw_b), "两实例字不变");
        let ClaimMismatch(returned) = b.finish_err(returned).unwrap_err();
        assert_eq!((a.raw(), b.raw()), (raw_a, raw_b));
        assert_eq!(a.finish_ok(returned).unwrap(), Settle::Settled);
        assert!(!b.is_settled(), "B 的责任不受影响");
    }

    #[test]
    fn guard_drop_and_panic_restore_pending() {
        let o = obligation();
        o.request(MonoRawNs(2_000));
        {
            let guard = o.claim_guard().unwrap();
            assert_eq!(guard.claim().obligation(), o.id());
            assert!(o.view().active);
        }
        let view = o.view();
        assert!(view.pending && !view.active, "Drop 未消费即恢复 Pending");
        assert_eq!(view.unsettled_since, Some(MonoRawNs(2_000)));

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = o.claim_guard().unwrap();
            assert!(o.view().active);
            panic!("writer panics mid-write");
        }));
        assert!(result.is_err());
        let view = o.view();
        assert!(view.pending && !view.active, "panic 解栈后恢复 Pending");
        assert_eq!(view.unsettled_since, Some(MonoRawNs(2_000)), "起点不变");
        assert_eq!(o.claim_guard().unwrap().commit(), Settle::Settled);
        assert!(o.is_settled());
    }

    /// 单字 CAS 双胜负线性化：`finish_ok` 与并发 `request` 无论谁先赢，新责任都不丢。
    #[test]
    fn concurrent_request_and_finish_never_lose_responsibility() {
        for _ in 0..200 {
            let o = Arc::new(obligation());
            o.request(MonoRawNs(2_000));
            let claim = o.begin_write().unwrap();
            let barrier = Arc::new(Barrier::new(2));
            let requester = {
                let o = Arc::clone(&o);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    o.request(MonoRawNs(5_000))
                })
            };
            barrier.wait();
            let settle = o.finish_ok(claim).unwrap();
            let outcome = requester.join().unwrap();
            let view = o.view();
            assert!(view.pending, "并发 Pending 不得被清除");
            match (settle, outcome) {
                (Settle::Settled, RequestOutcome::Registered) => {
                    assert_eq!(
                        view.unsettled_since,
                        Some(MonoRawNs(5_000)),
                        "完成先赢：新起点"
                    );
                }
                (Settle::PendingRemains, RequestOutcome::Merged) => {
                    assert_eq!(
                        view.unsettled_since,
                        Some(MonoRawNs(2_000)),
                        "请求先赢：保留 t1"
                    );
                }
                other => panic!("非法交错 {other:?}"),
            }
        }
    }

    #[test]
    fn id_counter_exhaustion_is_explicit_and_does_not_wrap() {
        let counter = InstanceCounter::starting_at(u64::MAX - 1);
        assert_eq!(counter.next().unwrap().get(), u64::MAX, "最后一个可分配 ID");
        assert_eq!(counter.next().unwrap_err(), IdExhausted);
        assert_eq!(
            counter.next().unwrap_err(),
            IdExhausted,
            "耗尽后仍失败，不回绕"
        );
        assert_eq!(counter.0.load(Ordering::Acquire), u64::MAX);
        // 与生产同一路径的义务分配在局部耗尽计数上显式失败，生产静态计数不受影响。
        assert_eq!(
            EStopObligation::new_with_counter(MonoRawNs(0), &counter).unwrap_err(),
            IdExhausted
        );
        assert!(EStopObligation::new(MonoRawNs(0)).is_ok(), "全局分配仍可用");
    }
}
