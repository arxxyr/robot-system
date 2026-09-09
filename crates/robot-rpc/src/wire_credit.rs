//! 实际未退休请求持有的分类 wire 信用；丢弃许可不自动放行。

use crate::class_cache::{Class, IdExhausted};
use robot_transport::TransportError;
use robot_transport::iceoryx2::{RrProfile, RrStaticLimits};
use std::num::NonZeroU64;
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

static NEXT_CREDITS_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicKind {
    S0Control,
    S1Task,
}

/// 含唯一 topic 来源的受检份额，无独立 kind 参数可以错配。
/// ```compile_fail
/// use robot_rpc::wire_credit::{ClassShares,TopicKind};
/// let forged = ClassShares { kind:TopicKind::S1Task,normal:4,restore:1,lower:1 };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassShares {
    kind: TopicKind,
    normal: usize,
    restore: usize,
    lower: usize,
}
#[derive(Debug)]
pub enum ShareError {
    InvalidLimits(TransportError),
}
impl ClassShares {
    pub fn for_topic(kind: TopicKind, limits: &RrStaticLimits) -> Result<Self, ShareError> {
        let limits = RrProfile {
            l: limits.l,
            c: limits.c,
            ..RrProfile::S0_CONTROL
        }
        .validate()
        .map_err(ShareError::InvalidLimits)?;
        Ok(match kind {
            TopicKind::S0Control => Self {
                kind,
                normal: limits.l - 2,
                restore: 1,
                lower: 1,
            },
            TopicKind::S1Task => Self {
                kind,
                normal: limits.l,
                restore: 0,
                lower: 0,
            },
        })
    }
    #[must_use]
    pub const fn kind(&self) -> TopicKind {
        self.kind
    }
    #[must_use]
    pub const fn normal(&self) -> usize {
        self.normal
    }
    #[must_use]
    pub const fn restore(&self) -> usize {
        self.restore
    }
    #[must_use]
    pub const fn lower(&self) -> usize {
        self.lower
    }
    const fn for_class(self, class: Class) -> usize {
        match class {
            Class::Normal => self.normal,
            Class::Restore => self.restore,
            Class::Lower => self.lower,
            Class::Emergency => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreditsId(NonZeroU64);

/// 唯一 wire 许可；必须携真实退休依据由持有者显式归还。
/// ```compile_fail
/// use robot_rpc::wire_credit::WirePermit;
/// fn duplicate(permit:WirePermit) { let second=permit.clone(); }
/// ```
/// ```compile_fail
/// use robot_rpc::wire_credit::{WirePermit,CreditsId};
/// use robot_rpc::class_cache::Class;
/// fn forge(issuer:CreditsId) { let permit=WirePermit { issuer,serial:std::num::NonZeroU64::MIN,class:Class::Lower }; }
/// ```
#[derive(Debug)]
pub struct WirePermit {
    issuer: CreditsId,
    serial: NonZeroU64,
    class: Class,
}
impl WirePermit {
    #[must_use]
    pub const fn class(&self) -> Class {
        self.class
    }
}
#[derive(Debug)]
pub struct PermitMismatch(pub WirePermit);
#[derive(Debug)]
struct CreditState {
    used: [usize; 4],
    next_serial: NonZeroU64,
}
#[derive(Debug)]
pub struct Credits {
    id: CreditsId,
    shares: ClassShares,
    state: Mutex<CreditState>,
}
impl Credits {
    pub fn new(shares: ClassShares) -> Result<Self, IdExhausted> {
        let id = NEXT_CREDITS_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or(IdExhausted)?;
        Ok(Self {
            id: CreditsId(id),
            shares,
            state: Mutex::new(CreditState {
                used: [0; 4],
                next_serial: NonZeroU64::MIN,
            }),
        })
    }
    pub fn try_acquire(&self, class: Class) -> Option<WirePermit> {
        let mut state = self.state.lock().expect("信用账本锁未中毒");
        let index = index(class);
        if state.used[index] >= self.shares.for_class(class) {
            return None;
        }
        let next = state.next_serial.checked_add(1)?;
        let permit = WirePermit {
            issuer: self.id,
            serial: state.next_serial,
            class,
        };
        state.next_serial = next;
        state.used[index] += 1;
        Some(permit)
    }
    // 按值消费确保许可只能归还一次；不可改成借用。
    #[allow(clippy::needless_pass_by_value)]
    pub fn release(&self, permit: WirePermit) -> Result<(), PermitMismatch> {
        if permit.issuer != self.id {
            return Err(PermitMismatch(permit));
        }
        let mut state = self.state.lock().expect("信用账本锁未中毒");
        debug_assert!(permit.serial < state.next_serial);
        state.used[index(permit.class)] -= 1;
        Ok(())
    }
    #[must_use]
    pub fn used(&self, class: Class) -> usize {
        self.state.lock().expect("信用账本锁未中毒").used[index(class)]
    }
    #[must_use]
    pub const fn share(&self, class: Class) -> usize {
        self.shares.for_class(class)
    }
    #[must_use]
    pub const fn kind(&self) -> TopicKind {
        self.shares.kind
    }
}
const fn index(class: Class) -> usize {
    match class {
        Class::Normal => 0,
        Class::Restore => 1,
        Class::Lower => 2,
        Class::Emergency => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn credits() -> Credits {
        Credits::new(
            ClassShares::for_topic(TopicKind::S0Control, &RrStaticLimits { l: 4, c: 8 }).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn permit_from_other_credits_is_refused_and_returned_used_unchanged() {
        let first = credits();
        let second = credits();
        let permit = first.try_acquire(Class::Lower).unwrap();
        let identity = (permit.issuer, permit.serial, permit.class);
        let PermitMismatch(returned) = second.release(permit).unwrap_err();
        assert_eq!((returned.issuer, returned.serial, returned.class), identity);
        assert_eq!(
            (first.used(Class::Lower), second.used(Class::Lower)),
            (1, 0)
        );
        first.release(returned).unwrap();
        assert_eq!(first.used(Class::Lower), 0);
    }
    #[test]
    fn dropped_permit_keeps_used() {
        let credits = credits();
        {
            let _unretired = credits.try_acquire(Class::Restore).unwrap();
        }
        assert_eq!(credits.used(Class::Restore), 1);
        assert!(credits.try_acquire(Class::Restore).is_none());
    }
    #[test]
    fn shares_from_limits_s0control_l4_is_2_1_1_and_l2_rejected() {
        let shares =
            ClassShares::for_topic(TopicKind::S0Control, &RrStaticLimits { c: 8, l: 4 }).unwrap();
        assert_eq!(
            (shares.normal(), shares.restore(), shares.lower()),
            (2, 1, 1)
        );
        assert!(
            ClassShares::for_topic(TopicKind::S0Control, &RrStaticLimits { c: 8, l: 2 }).is_err()
        );
    }
    #[test]
    fn shares_s1task_is_l_0_0() {
        let shares =
            ClassShares::for_topic(TopicKind::S1Task, &RrStaticLimits { c: 1, l: 64 }).unwrap();
        assert_eq!(
            (shares.normal(), shares.restore(), shares.lower()),
            (64, 0, 0)
        );
        let credits = Credits::new(shares).unwrap();
        assert_eq!(credits.kind(), TopicKind::S1Task);
        for class in [Class::Lower, Class::Restore, Class::Emergency] {
            assert!(credits.try_acquire(class).is_none());
        }
        let permits: Vec<_> = (0..64)
            .map(|_| credits.try_acquire(Class::Normal).unwrap())
            .collect();
        assert!(credits.try_acquire(Class::Normal).is_none());
        for permit in permits {
            credits.release(permit).unwrap();
        }
    }
    #[test]
    fn shares_validate_both_topics_with_real_transport_error() {
        for kind in [TopicKind::S0Control, TopicKind::S1Task] {
            for (c, l) in [
                (0, 4),
                (0, usize::MAX),
                (17, 4),
                (16, 5),
                (16, usize::MAX),
                (usize::MAX, 4),
            ] {
                let expected = RrProfile {
                    c,
                    l,
                    ..RrProfile::S0_CONTROL
                }
                .validate()
                .unwrap_err();
                let ShareError::InvalidLimits(actual) =
                    ClassShares::for_topic(kind, &RrStaticLimits { c, l }).unwrap_err();
                assert_eq!(actual.to_string(), expected.to_string());
            }
            for (c, l) in [(8, 4), (16, 4), (1, 64)] {
                assert!(ClassShares::for_topic(kind, &RrStaticLimits { c, l }).is_ok());
            }
        }
    }
    #[test]
    fn serials_unique_monotonic() {
        let credits = credits();
        let mut last = 0;
        for class in [Class::Normal, Class::Lower, Class::Restore, Class::Normal] {
            let permit = credits.try_acquire(class).unwrap();
            assert!(permit.serial.get() > last);
            last = permit.serial.get();
            credits.release(permit).unwrap();
        }
    }
    #[test]
    fn serial_exhaustion_keeps_counts_and_release_does_not_reset() {
        let credits = credits();
        credits.state.lock().unwrap().next_serial = NonZeroU64::new(u64::MAX - 1).unwrap();
        let permit = credits.try_acquire(Class::Lower).unwrap();
        assert_eq!(permit.serial.get(), u64::MAX - 1);
        assert!(credits.try_acquire(Class::Normal).is_none());
        assert_eq!(credits.used(Class::Normal), 0);
        credits.release(permit).unwrap();
        assert_eq!(credits.used(Class::Lower), 0);
        assert!(credits.try_acquire(Class::Lower).is_none());
        assert_eq!(credits.state.lock().unwrap().next_serial.get(), u64::MAX);
    }
    #[test]
    fn capacity_rejection_does_not_consume_serial() {
        let credits = credits();
        let permit = credits.try_acquire(Class::Lower).unwrap();
        let next = credits.state.lock().unwrap().next_serial;
        assert!(credits.try_acquire(Class::Lower).is_none());
        assert!(credits.try_acquire(Class::Emergency).is_none());
        assert_eq!(credits.state.lock().unwrap().next_serial, next);
        credits.release(permit).unwrap();
    }
}
