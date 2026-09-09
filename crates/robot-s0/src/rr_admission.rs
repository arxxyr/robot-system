//! 控制 RR 的有界准入账本；仅准备原语，生产接线留在后续阶段。
//!
//! `Slot` 是显式归还的线性凭证；丢弃凭证不会释放容量。每个账本独占自身
//! 计数和身份，不可克隆。初始化可在非 RT 边界执行，准入与归还不分配。

use std::fmt::Debug;
use std::hash::Hash;
use std::io;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};

use robot_transport::iceoryx2::{RrPeerId, RrStaticLimits};

use crate::event_ingress::RR_CHANNEL_CAPACITY;

const C_MAX: usize = 16;
static NEXT_LEDGER_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RrRequestClass {
    Normal,
    Restore,
    Lower,
}

impl RrRequestClass {
    #[must_use]
    pub const fn of_set_enabled(dry_run: bool, enabled: bool) -> Self {
        match (dry_run, enabled) {
            (true, _) => Self::Normal,
            (false, true) => Self::Restore,
            (false, false) => Self::Lower,
        }
    }

    #[must_use]
    pub const fn of_clear(dry_run: bool) -> Self {
        if dry_run { Self::Normal } else { Self::Restore }
    }
}

/// 容量派生入口只接受此受检值，不信任 `RrStaticLimits` 的公开字段。
///
/// ```compile_fail
/// use robot_s0::rr_admission::CheckedLimits;
/// let forged = CheckedLimits { l: 4, c: 8, product: 32 };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckedLimits {
    l: usize,
    c: usize,
    product: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitsError {
    LTooSmall { l: usize },
    CTooSmall { c: usize },
    CTooLarge { c: usize },
    Overflow { c: usize, l: usize },
    ProductTooLarge { c: usize, l: usize, product: usize },
}

impl CheckedLimits {
    pub fn try_from_limits(limits: &RrStaticLimits) -> Result<Self, LimitsError> {
        let RrStaticLimits { l, c } = *limits;
        if l < 3 {
            return Err(LimitsError::LTooSmall { l });
        }
        if c == 0 {
            return Err(LimitsError::CTooSmall { c });
        }
        if c > C_MAX {
            return Err(LimitsError::CTooLarge { c });
        }
        let product = c.checked_mul(l).ok_or(LimitsError::Overflow { c, l })?;
        if product > RR_CHANNEL_CAPACITY {
            return Err(LimitsError::ProductTooLarge { c, l, product });
        }
        Ok(Self { l, c, product })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassCaps {
    pub restore: usize,
    pub lower: usize,
    pub normal: usize,
    pub total: usize,
}

impl ClassCaps {
    #[must_use]
    pub const fn from_checked(limits: &CheckedLimits) -> Self {
        Self {
            restore: limits.c,
            lower: limits.c,
            normal: limits.c * (limits.l - 2),
            total: limits.product,
        }
    }

    const fn for_class(self, class: RrRequestClass) -> usize {
        match class {
            RrRequestClass::Normal => self.normal,
            RrRequestClass::Restore => self.restore,
            RrRequestClass::Lower => self.lower,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerShares {
    pub normal: usize,
    pub restore: usize,
    pub lower: usize,
}

impl PeerShares {
    #[must_use]
    pub const fn from_checked(limits: &CheckedLimits) -> Self {
        Self {
            normal: limits.l - 2,
            restore: 1,
            lower: 1,
        }
    }

    const fn for_class(self, class: RrRequestClass) -> usize {
        match class {
            RrRequestClass::Normal => self.normal,
            RrRequestClass::Restore => self.restore,
            RrRequestClass::Lower => self.lower,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassCounts {
    pub normal: usize,
    pub restore: usize,
    pub lower: usize,
    pub total: usize,
}

impl ClassCounts {
    const fn for_class(self, class: RrRequestClass) -> usize {
        match class {
            RrRequestClass::Normal => self.normal,
            RrRequestClass::Restore => self.restore,
            RrRequestClass::Lower => self.lower,
        }
    }

    fn class_mut(&mut self, class: RrRequestClass) -> &mut usize {
        match class {
            RrRequestClass::Normal => &mut self.normal,
            RrRequestClass::Restore => &mut self.restore,
            RrRequestClass::Lower => &mut self.lower,
        }
    }

    fn acquire(&mut self, class: RrRequestClass) {
        *self.class_mut(class) += 1;
        self.total += 1;
    }
    fn release(&mut self, class: RrRequestClass) {
        *self.class_mut(class) -= 1;
        self.total -= 1;
    }
}

/// 仅账本初始化可发放的进程内唯一身份。
///
/// ```compile_fail
/// use robot_s0::rr_admission::LedgerId;
/// let forged = LedgerId(std::num::NonZeroU64::new(1).unwrap());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerId(NonZeroU64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerError {
    IdExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    TotalCap,
    ClassCap,
    PeerTableFull,
    PeerShare,
    SerialExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    QueueFull {
        class: RrRequestClass,
        reason: RefusalReason,
    },
}

impl Refusal {
    #[must_use]
    pub const fn reason(self) -> RefusalReason {
        match self {
            Self::QueueFull { reason, .. } => reason,
        }
    }
}

/// 必须显式归还到来源账本；丢弃会保留占用，不隐式恢复准入容量。
///
/// ```compile_fail
/// use robot_s0::rr_admission::Slot;
/// fn duplicate(slot: Slot<u32>) { let copy = slot.clone(); }
/// ```
///
/// ```compile_fail
/// use robot_s0::rr_admission::{LedgerId, Slot, RrRequestClass};
/// fn forge(ledger: LedgerId) {
///     let slot = Slot { ledger, serial: std::num::NonZeroU64::new(1).unwrap(), peer: 1u32, class: RrRequestClass::Lower };
/// }
/// ```
#[derive(Debug)]
pub struct Slot<P> {
    ledger: LedgerId,
    serial: NonZeroU64,
    peer: P,
    class: RrRequestClass,
}

impl<P: Copy> Slot<P> {
    #[must_use]
    pub const fn class(&self) -> RrRequestClass {
        self.class
    }
    #[must_use]
    pub const fn peer(&self) -> P {
        self.peer
    }
}

#[derive(Debug)]
pub struct SlotMismatch<P>(pub Slot<P>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeerEntry<P> {
    peer: P,
    counts: ClassCounts,
}

#[derive(Debug)]
pub struct PeerLedger<P: Copy + Eq + Hash + Debug> {
    id: LedgerId,
    caps: ClassCaps,
    shares: PeerShares,
    peers: [Option<PeerEntry<P>>; C_MAX],
    global: ClassCounts,
    next_serial: NonZeroU64,
}

pub type ControlPeerLedger = PeerLedger<RrPeerId>;

impl<P: Copy + Eq + Hash + Debug> PeerLedger<P> {
    pub fn new(limits: &CheckedLimits) -> Result<Self, LedgerError> {
        Self::with_counter(&NEXT_LEDGER_ID, limits)
    }

    fn with_counter(
        counter: &'static AtomicU64,
        limits: &CheckedLimits,
    ) -> Result<Self, LedgerError> {
        let id = counter
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or(LedgerError::IdExhausted)?;
        Ok(Self {
            id: LedgerId(id),
            caps: ClassCaps::from_checked(limits),
            shares: PeerShares::from_checked(limits),
            peers: [None; C_MAX],
            global: ClassCounts::default(),
            next_serial: NonZeroU64::MIN,
        })
    }

    pub fn admit(&mut self, peer: P, class: RrRequestClass) -> Result<Slot<P>, Refusal> {
        let refuse = |reason| Refusal::QueueFull { class, reason };
        if self.global.total == self.caps.total {
            return Err(refuse(RefusalReason::TotalCap));
        }
        if self.global.for_class(class) == self.caps.for_class(class) {
            return Err(refuse(RefusalReason::ClassCap));
        }
        let index = self
            .peers
            .iter()
            .position(|entry| entry.as_ref().is_some_and(|entry| entry.peer == peer))
            .or_else(|| self.peers.iter().position(Option::is_none))
            .ok_or_else(|| refuse(RefusalReason::PeerTableFull))?;
        let counts = self.peers[index].map_or_else(ClassCounts::default, |entry| entry.counts);
        if counts.for_class(class) == self.shares.for_class(class) {
            return Err(refuse(RefusalReason::PeerShare));
        }
        let next = self
            .next_serial
            .checked_add(1)
            .ok_or_else(|| refuse(RefusalReason::SerialExhausted))?;
        // 上述拒绝全部只读；序号也可推进后，才占槽并修改计数。
        let slot = Slot {
            ledger: self.id,
            serial: self.next_serial,
            peer,
            class,
        };
        self.next_serial = next;
        let entry = self.peers[index].get_or_insert(PeerEntry { peer, counts });
        entry.counts.acquire(class);
        self.global.acquire(class);
        Ok(slot)
    }

    // 凭证按值消费是一次性归还协议，不得改成借用。
    #[allow(clippy::needless_pass_by_value)]
    pub fn release(&mut self, slot: Slot<P>) -> Result<(), SlotMismatch<P>> {
        if slot.ledger != self.id {
            return Err(SlotMismatch(slot));
        }
        debug_assert!(slot.serial < self.next_serial);
        // 私有凭证只由本实例成功准入产生，非 Clone/Copy，必有尚未归还的 peer。
        let entry = self
            .peers
            .iter_mut()
            .find(|entry| entry.as_ref().is_some_and(|entry| entry.peer == slot.peer))
            .expect("本账本尚未归还的凭证必须持有 peer 槽");
        let counts = &mut entry.as_mut().expect("已找到非空 peer 槽").counts;
        counts.release(slot.class);
        if counts.total == 0 {
            *entry = None;
        }
        self.global.release(slot.class);
        Ok(())
    }

    #[must_use]
    pub const fn counts(&self) -> ClassCounts {
        self.global
    }
    #[must_use]
    pub fn peer_counts(&self, peer: P) -> Option<ClassCounts> {
        self.peers
            .iter()
            .flatten()
            .find(|entry| entry.peer == peer)
            .map(|entry| entry.counts)
    }
    #[must_use]
    pub const fn id(&self) -> LedgerId {
        self.id
    }
}

/// 非 RT 初始化检查：单个 RR 来源的两类通道及回复存储必须覆盖其份额。
pub fn validate_lane_capacity(
    limits: &CheckedLimits,
    high_cap: usize,
    normal_cap: usize,
    responses: usize,
) -> io::Result<()> {
    for (field, expected, actual) in [
        ("high_cap", 2 * limits.c, high_cap),
        ("normal_cap", limits.c * (limits.l - 2), normal_cap),
    ] {
        if actual != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{field} expected={expected} actual={actual}"),
            ));
        }
    }
    if responses < limits.product {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "responses expected_min={} actual={responses}",
                limits.product
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use RrRequestClass::{Lower, Normal, Restore};

    fn limits(c: usize) -> CheckedLimits {
        CheckedLimits::try_from_limits(&RrStaticLimits { c, l: 4 }).unwrap()
    }
    fn ledger(c: usize) -> PeerLedger<u32> {
        PeerLedger::new(&limits(c)).unwrap()
    }

    fn refused_unchanged(
        ledger: &mut PeerLedger<u32>,
        peer: u32,
        class: RrRequestClass,
        reason: RefusalReason,
    ) {
        let before = (ledger.global, ledger.peers, ledger.next_serial);
        let refusal = ledger.admit(peer, class).unwrap_err();
        assert_eq!(refusal, Refusal::QueueFull { class, reason });
        assert_eq!(refusal.reason(), reason);
        assert_eq!((ledger.global, ledger.peers, ledger.next_serial), before);
    }

    #[test]
    fn class_of_set_enabled_and_clear_table() {
        for (dry, enabled, expected) in [
            (true, true, Normal),
            (true, false, Normal),
            (false, true, Restore),
            (false, false, Lower),
        ] {
            assert_eq!(RrRequestClass::of_set_enabled(dry, enabled), expected);
        }
        assert_eq!(RrRequestClass::of_clear(true), Normal);
        assert_eq!(RrRequestClass::of_clear(false), Restore);
    }

    #[test]
    fn same_peer_third_normal_is_queue_full() {
        let mut ledger = ledger(8);
        let first = ledger.admit(1, Normal).unwrap();
        let second = ledger.admit(1, Normal).unwrap();
        refused_unchanged(&mut ledger, 1, Normal, RefusalReason::PeerShare);
        ledger.release(first).unwrap();
        ledger.release(second).unwrap();
    }

    #[test]
    fn same_peer_second_lower_and_second_restore_are_queue_full_not_already_in_progress() {
        for class in [Lower, Restore] {
            let mut ledger = ledger(8);
            let slot = ledger.admit(1, class).unwrap();
            refused_unchanged(&mut ledger, 1, class, RefusalReason::PeerShare);
            ledger.release(slot).unwrap();
        }
    }

    #[test]
    fn peers_are_independent_within_global_caps() {
        let mut ledger = ledger(8);
        let slots = [
            ledger.admit(1, Normal).unwrap(),
            ledger.admit(1, Normal).unwrap(),
            ledger.admit(1, Lower).unwrap(),
            ledger.admit(1, Restore).unwrap(),
        ];
        for class in [Normal, Lower, Restore] {
            let slot = ledger.admit(2, class).unwrap();
            assert_eq!(ledger.peer_counts(1).unwrap().total, 4);
            ledger.release(slot).unwrap();
        }
        for slot in slots {
            ledger.release(slot).unwrap();
        }
    }

    #[test]
    fn global_class_cap_c_lower_and_restore() {
        for class in [Lower, Restore] {
            let mut ledger = ledger(8);
            let slots: Vec<_> = (0..8)
                .map(|peer| ledger.admit(peer, class).unwrap())
                .collect();
            refused_unchanged(&mut ledger, 8, class, RefusalReason::ClassCap);
            for slot in slots {
                ledger.release(slot).unwrap();
            }
        }
    }

    #[test]
    fn global_total_cap_c_times_l() {
        let mut ledger = ledger(8);
        let mut slots = Vec::new();
        for peer in 0..8 {
            for class in [Normal, Normal, Lower, Restore] {
                slots.push(ledger.admit(peer, class).unwrap());
            }
        }
        assert_eq!(ledger.counts().total, 32);
        for class in [Normal, Lower, Restore] {
            refused_unchanged(&mut ledger, 0, class, RefusalReason::TotalCap);
        }
        for slot in slots {
            ledger.release(slot).unwrap();
        }
        assert_eq!(ledger.counts(), ClassCounts::default());
    }

    #[test]
    fn seventeenth_peer_is_queue_full_reason_peer_table_full_zero_change() {
        let mut ledger = ledger(16);
        let slots: Vec<_> = (0..16)
            .map(|peer| ledger.admit(peer, Normal).unwrap())
            .collect();
        refused_unchanged(&mut ledger, 16, Normal, RefusalReason::PeerTableFull);
        // 已有 peer 仍可使用剩余份额；peer 表满不应影响查找顺序。
        let extra = ledger.admit(0, Normal).unwrap();
        ledger.release(extra).unwrap();
        for slot in slots {
            ledger.release(slot).unwrap();
        }
    }

    #[test]
    fn slot_from_other_ledger_refused_and_returned_counts_unchanged() {
        let mut first = ledger(8);
        let mut second = ledger(8);
        let slot = first.admit(1, Lower).unwrap();
        let original = (slot.ledger, slot.serial, slot.peer, slot.class);
        let before = (first.global, first.peers, second.global, second.peers);
        let SlotMismatch(returned) = second.release(slot).unwrap_err();
        assert_eq!(
            (
                returned.ledger,
                returned.serial,
                returned.peer,
                returned.class
            ),
            original
        );
        assert_eq!(
            (first.global, first.peers, second.global, second.peers),
            before
        );
        first.release(returned).unwrap();
    }

    #[test]
    fn dropped_slot_keeps_counts() {
        let mut ledger = ledger(8);
        // 凭证自然离开作用域，无 Drop 归还；不用 forget 或制造副本。
        {
            let _unreturned = ledger.admit(1, Lower).unwrap();
        }
        assert_eq!(ledger.counts().lower, 1);
        refused_unchanged(&mut ledger, 1, Lower, RefusalReason::PeerShare);
    }

    #[test]
    fn release_frees_peer_entry_and_readmits() {
        let mut ledger = ledger(16);
        let slots: Vec<_> = (0..16)
            .map(|peer| ledger.admit(peer, Normal).unwrap())
            .collect();
        let mut slots = slots.into_iter();
        ledger.release(slots.next().unwrap()).unwrap();
        assert_eq!(ledger.peer_counts(0), None);
        let next = ledger.admit(16, Normal).unwrap();
        assert!(ledger.peer_counts(16).is_some());
        ledger.release(next).unwrap();
        for slot in slots {
            ledger.release(slot).unwrap();
        }
        assert!(ledger.peers.iter().all(Option::is_none));
    }

    #[test]
    fn ledger_id_exhaustion_explicit_no_wrap() {
        static COUNTER: AtomicU64 = AtomicU64::new(u64::MAX - 1);
        let first = PeerLedger::<u32>::with_counter(&COUNTER, &limits(8)).unwrap();
        assert_eq!(first.id.0.get(), u64::MAX - 1);
        assert_eq!(
            PeerLedger::<u32>::with_counter(&COUNTER, &limits(8)).unwrap_err(),
            LedgerError::IdExhausted
        );
        assert_eq!(COUNTER.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn serial_exhaustion_keeps_state_and_release_does_not_reopen_admission() {
        let mut ledger = ledger(8);
        ledger.next_serial = NonZeroU64::new(u64::MAX - 1).unwrap();
        let slot = ledger.admit(1, Lower).unwrap();
        assert_eq!(slot.serial.get(), u64::MAX - 1);
        refused_unchanged(&mut ledger, 2, Normal, RefusalReason::SerialExhausted);
        // 四步既有拒绝优先级仍在序号检查之前。
        refused_unchanged(&mut ledger, 1, Lower, RefusalReason::PeerShare);
        ledger.release(slot).unwrap();
        assert_eq!(ledger.counts(), ClassCounts::default());
        assert!(ledger.peers.iter().all(Option::is_none));
        refused_unchanged(&mut ledger, 1, Lower, RefusalReason::SerialExhausted);
        assert_eq!(ledger.next_serial.get(), u64::MAX);
    }

    #[test]
    fn checked_limits_matrix() {
        for (c, l) in [(1, 3), (1, 64), (8, 4), (16, 4)] {
            let checked = CheckedLimits::try_from_limits(&RrStaticLimits { c, l }).unwrap();
            assert_eq!(
                ClassCaps::from_checked(&checked),
                ClassCaps {
                    restore: c,
                    lower: c,
                    normal: c * (l - 2),
                    total: c * l
                }
            );
            assert_eq!(
                PeerShares::from_checked(&checked),
                PeerShares {
                    normal: l - 2,
                    restore: 1,
                    lower: 1
                }
            );
        }
        for (c, l, expected) in [
            (
                16,
                5,
                LimitsError::ProductTooLarge {
                    c: 16,
                    l: 5,
                    product: 80,
                },
            ),
            (
                16,
                usize::MAX,
                LimitsError::Overflow {
                    c: 16,
                    l: usize::MAX,
                },
            ),
            (usize::MAX, 4, LimitsError::CTooLarge { c: usize::MAX }),
            (8, 2, LimitsError::LTooSmall { l: 2 }),
            (17, 4, LimitsError::CTooLarge { c: 17 }),
            (0, 4, LimitsError::CTooSmall { c: 0 }),
            (0, usize::MAX, LimitsError::CTooSmall { c: 0 }),
        ] {
            assert_eq!(
                CheckedLimits::try_from_limits(&RrStaticLimits { c, l }),
                Err(expected)
            );
        }
    }

    #[test]
    fn validate_lane_capacity_matrix() {
        for c in [8, 16] {
            let limits = limits(c);
            assert!(validate_lane_capacity(&limits, 2 * c, 2 * c, 4 * c).is_ok());
            assert!(validate_lane_capacity(&limits, 2 * c, 2 * c, 4 * c + 1).is_ok());
            for (high, normal, responses, field, expected, actual) in [
                (2 * c - 1, 2 * c, 4 * c, "high_cap", 2 * c, 2 * c - 1),
                (2 * c, 2 * c + 1, 4 * c, "normal_cap", 2 * c, 2 * c + 1),
                (2 * c, 2 * c, 4 * c - 1, "responses", 4 * c, 4 * c - 1),
            ] {
                let error = validate_lane_capacity(&limits, high, normal, responses).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
                let message = error.to_string();
                assert!(
                    message.contains(field)
                        && message.contains(&expected.to_string())
                        && message.contains(&actual.to_string())
                );
            }
        }
    }
}
