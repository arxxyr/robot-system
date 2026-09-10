//! 分类预留的幂等缓存；Pending 不按 TTL 遗忘，终态从实际退休时刻开始计时。

use crate::idempotency::{Fingerprint, Method};
use robot_msg::msg::{ActionRpcStatus, ClearEStopStatus, RpcStatus, SetEnabledStatus, SideEffect};
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ENTRY_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_CACHE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Normal,
    Restore,
    Lower,
    Emergency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reserves {
    pub emergency: usize,
    pub lower: usize,
    pub restore: usize,
}
impl Reserves {
    pub const DEFAULT: Self = Self {
        emergency: 64,
        lower: 64,
        restore: 128,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdExhausted;

/// 全局单调身份，不能由外部构造或重复使用局部计数器伪造。
/// ```compile_fail
/// use robot_rpc::class_cache::EntryId;
/// let id = EntryId(std::num::NonZeroU64::MIN);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryId(NonZeroU64);

#[derive(Debug)]
pub struct EntryIdAllocator(&'static AtomicU64);
impl EntryIdAllocator {
    #[must_use]
    pub fn global() -> Self {
        Self(&NEXT_ENTRY_ID)
    }
    #[cfg(test)]
    fn from_counter(counter: &'static AtomicU64) -> Self {
        Self(counter)
    }
    pub fn next(&self) -> Result<EntryId, IdExhausted> {
        let value = self
            .0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| IdExhausted)?;
        NonZeroU64::new(value).map(EntryId).ok_or(IdExhausted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CacheId(NonZeroU64);

/// 唯一占位责任；丢弃本值不自动删除 Pending。
/// ```compile_fail
/// use robot_rpc::class_cache::Placeholder;
/// fn duplicate(value: Placeholder) { let copy = value.clone(); }
/// ```
#[derive(Debug)]
#[must_use]
pub struct Placeholder {
    cache: CacheId,
    id: EntryId,
    method: Method,
    key: String,
    class: Class,
}
impl Placeholder {
    #[must_use]
    pub const fn id(&self) -> EntryId {
        self.id
    }
    #[must_use]
    pub const fn class(&self) -> Class {
        self.class
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    Queued,
    Sending,
    Publishing,
    Sent,
    Orphan,
    RepliedUnretired,
    NoHandle,
    Committed,
    StartedRetired,
    UnknownRetired,
}
impl EntryState {
    const fn index(self) -> usize {
        match self {
            Self::Queued => 0,
            Self::Sending => 1,
            Self::Publishing => 2,
            Self::Sent => 3,
            Self::Orphan => 4,
            Self::RepliedUnretired => 5,
            Self::NoHandle => 6,
            Self::Committed => 7,
            Self::StartedRetired => 8,
            Self::UnknownRetired => 9,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalState {
    Committed,
    StartedRetired,
    UnknownRetired,
}
impl From<TerminalState> for EntryState {
    fn from(state: TerminalState) -> Self {
        match state {
            TerminalState::Committed => Self::Committed,
            TerminalState::StartedRetired => Self::StartedRetired,
            TerminalState::UnknownRetired => Self::UnknownRetired,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainOutcome {
    NotSubmitted,
    Unknown { raw: u8 },
    Registered,
    DrainFailedCompensated { cancel_ok: bool },
}

/// 重放按原方法逐字段保存；生成枚举 newtype 的未知原始值也保留。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedOutcome {
    ClearEStop {
        status: RpcStatus,
        domain: ClearEStopStatus,
        superseded: bool,
        cleared_at_ns: u64,
        dry_run: bool,
        error_msg: String,
    },
    SetEnabled {
        status: RpcStatus,
        domain: SetEnabledStatus,
        completed_at_ns: u64,
        error_msg: String,
    },
    InvokeTask {
        status: ActionRpcStatus,
        action_id: String,
        accepted_at_ns: u64,
        error_msg: String,
        drain: DrainOutcome,
    },
    CancelTask {
        status: ActionRpcStatus,
        action_id: String,
        cancel_at_ns: u64,
        error_msg: String,
    },
    ClearError {
        status: ActionRpcStatus,
        cleared_at_ns: u64,
        error_msg: String,
    },
    EStop {
        published_at_ns: u64,
        recipients: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedTerminal {
    pub state: TerminalState,
    pub effect: SideEffect,
    pub outcome: TypedOutcome,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ClassCacheConfigError {
    ReservesSumOverflow,
    ReservesExceedCapacity {
        max_entries: usize,
        reserves_sum: usize,
    },
    IdExhausted,
}

#[derive(Debug)]
pub enum ClassDecision {
    Fresh(Placeholder),
    Replay(CachedTerminal),
    Conflict {
        cached_fp: Fingerprint,
        incoming_fp: Fingerprint,
    },
    InFlight {
        state: EntryState,
    },
    Capacity {
        class: Class,
        len: usize,
        threshold: usize,
    },
    IdExhausted,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    Applied,
    Stale { current: Option<EntryState> },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbandonReason {
    NotDelivered,
    ExpiredBeforeSend,
    PublishFailedPreHistory,
    RetiredNoneFailure,
}

#[derive(Debug)]
struct Entry {
    id: EntryId,
    fp: Fingerprint,
    state: EntryState,
    terminal: Option<CachedTerminal>,
    terminal_at: Option<u64>,
}

#[derive(Debug)]
pub struct ClassCache {
    id: CacheId,
    ttl_ns: u64,
    max_entries: usize,
    reserves: Reserves,
    alloc: EntryIdAllocator,
    entries: HashMap<String, Entry>,
}
impl ClassCache {
    pub fn with_reserves(
        ttl_ns: u64,
        max_entries: usize,
        reserves: Reserves,
        alloc: EntryIdAllocator,
    ) -> Result<Self, ClassCacheConfigError> {
        let sum = reserves
            .emergency
            .checked_add(reserves.lower)
            .and_then(|n| n.checked_add(reserves.restore))
            .ok_or(ClassCacheConfigError::ReservesSumOverflow)?;
        if max_entries <= sum {
            return Err(ClassCacheConfigError::ReservesExceedCapacity {
                max_entries,
                reserves_sum: sum,
            });
        }
        let id = NEXT_CACHE_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or(ClassCacheConfigError::IdExhausted)?;
        Ok(Self {
            id: CacheId(id),
            ttl_ns,
            max_entries,
            reserves,
            alloc,
            entries: HashMap::new(),
        })
    }

    pub fn decide(
        &mut self,
        method: Method,
        key: &str,
        fp: Fingerprint,
        class: Class,
        now_ns: u64,
    ) -> ClassDecision {
        self.gc(now_ns);
        let qualified = namespace(method, key);
        if let Some(entry) = self.entries.get(&qualified) {
            if entry.fp != fp {
                return ClassDecision::Conflict {
                    cached_fp: entry.fp,
                    incoming_fp: fp,
                };
            }
            return match &entry.terminal {
                Some(terminal) => ClassDecision::Replay(terminal.clone()),
                None => ClassDecision::InFlight { state: entry.state },
            };
        }
        let threshold = self.threshold(class);
        if self.entries.len() >= threshold {
            return ClassDecision::Capacity {
                class,
                len: self.entries.len(),
                threshold,
            };
        }
        let Ok(id) = self.alloc.next() else {
            return ClassDecision::IdExhausted;
        };
        self.entries.insert(
            qualified,
            Entry {
                id,
                fp,
                state: EntryState::Queued,
                terminal: None,
                terminal_at: None,
            },
        );
        ClassDecision::Fresh(Placeholder {
            cache: self.id,
            id,
            method,
            key: key.to_owned(),
            class,
        })
    }

    const fn threshold(&self, class: Class) -> usize {
        match class {
            Class::Normal => {
                self.max_entries
                    - self.reserves.emergency
                    - self.reserves.lower
                    - self.reserves.restore
            }
            Class::Restore => self.max_entries - self.reserves.emergency - self.reserves.lower,
            Class::Lower => self.max_entries - self.reserves.emergency,
            Class::Emergency => self.max_entries,
        }
    }

    pub fn transition(
        &mut self,
        method: Method,
        key: &str,
        id: EntryId,
        to: EntryState,
        _now_ns: u64,
    ) -> Transition {
        let Some(entry) = self.entries.get_mut(&namespace(method, key)) else {
            return Transition::Stale { current: None };
        };
        let allowed = matches!(
            (entry.state, to),
            (
                EntryState::Queued,
                EntryState::Sending | EntryState::Publishing
            ) | (EntryState::Sending, EntryState::Sent | EntryState::NoHandle)
                | (
                    EntryState::Sent,
                    EntryState::Orphan | EntryState::RepliedUnretired | EntryState::NoHandle
                )
                | (
                    EntryState::Orphan,
                    EntryState::RepliedUnretired | EntryState::NoHandle
                )
                | (
                    EntryState::RepliedUnretired | EntryState::Publishing,
                    EntryState::NoHandle
                )
        );
        if entry.id != id || !allowed {
            return Transition::Stale {
                current: Some(entry.state),
            };
        }
        entry.state = to;
        Transition::Applied
    }

    pub fn finish(
        &mut self,
        method: Method,
        key: &str,
        id: EntryId,
        terminal: CachedTerminal,
        now_ns: u64,
    ) -> Transition {
        let Some(entry) = self.entries.get_mut(&namespace(method, key)) else {
            return Transition::Stale { current: None };
        };
        let allowed = matches!(entry.state, EntryState::Sent | EntryState::Orphan)
            || (entry.state == EntryState::Publishing
                && terminal.state == TerminalState::Committed);
        if entry.id != id || !allowed {
            return Transition::Stale {
                current: Some(entry.state),
            };
        }
        entry.state = terminal.state.into();
        entry.terminal = Some(terminal);
        entry.terminal_at = Some(now_ns);
        Transition::Applied
    }

    pub fn abandon(&mut self, p: Placeholder) -> Result<(), Placeholder> {
        if p.cache != self.id {
            return Err(p);
        }
        let key = namespace(p.method, &p.key);
        if self
            .entries
            .get(&key)
            .is_some_and(|entry| entry.id == p.id && entry.state == EntryState::Queued)
        {
            self.entries.remove(&key);
            Ok(())
        } else {
            Err(p)
        }
    }

    pub fn abandon_by_id(
        &mut self,
        method: Method,
        key: &str,
        id: EntryId,
        reason: AbandonReason,
        _now_ns: u64,
    ) -> Transition {
        let key = namespace(method, key);
        let Some(entry) = self.entries.get(&key) else {
            return Transition::Stale { current: None };
        };
        let allowed = matches!(
            (entry.state, reason),
            (
                EntryState::Sending,
                AbandonReason::NotDelivered | AbandonReason::ExpiredBeforeSend
            ) | (
                EntryState::Publishing,
                AbandonReason::PublishFailedPreHistory
            ) | (
                EntryState::Sent | EntryState::Orphan,
                AbandonReason::RetiredNoneFailure
            )
        );
        if entry.id != id || !allowed {
            return Transition::Stale {
                current: Some(entry.state),
            };
        }
        self.entries.remove(&key);
        Transition::Applied
    }

    #[must_use]
    /// 只读复制已有终态；测试观测不能改变 GC 或准入状态。
    #[cfg(any(test, feature = "test-freeze-hook"))]
    pub(crate) fn cached_terminal(&self, method: Method, key: &str) -> Option<CachedTerminal> {
        self.entries.get(&namespace(method, key))?.terminal.clone()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    #[must_use]
    pub fn state_counts(&self) -> [usize; 10] {
        let mut counts = [0; 10];
        for entry in self.entries.values() {
            counts[entry.state.index()] += 1;
        }
        counts
    }
    pub fn gc(&mut self, now_ns: u64) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, entry| {
            entry
                .terminal_at
                .and_then(|at| at.checked_add(self.ttl_ns))
                .is_none_or(|expires| expires > now_ns)
        });
        before - self.entries.len()
    }
}
fn namespace(method: Method, key: &str) -> String {
    format!("{}:{key}", method.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cache() -> ClassCache {
        ClassCache::with_reserves(100, 4096, Reserves::DEFAULT, EntryIdAllocator::global()).unwrap()
    }
    fn fresh(
        cache: &mut ClassCache,
        method: Method,
        key: &str,
        class: Class,
        now: u64,
    ) -> Placeholder {
        let ClassDecision::Fresh(p) = cache.decide(method, key, [1; 32], class, now) else {
            panic!("必须准入新条目")
        };
        p
    }
    fn terminal() -> CachedTerminal {
        CachedTerminal {
            state: TerminalState::Committed,
            effect: SideEffect::Committed,
            outcome: TypedOutcome::SetEnabled {
                status: RpcStatus::DeadlineExceeded,
                domain: SetEnabledStatus(12),
                completed_at_ns: 71,
                error_msg: "锁存的超时结果".into(),
            },
        }
    }
    fn sent(cache: &mut ClassCache, method: Method, key: &str, p: &Placeholder) {
        assert_eq!(
            cache.transition(method, key, p.id(), EntryState::Sending, 1),
            Transition::Applied
        );
        assert_eq!(
            cache.transition(method, key, p.id(), EntryState::Sent, 2),
            Transition::Applied
        );
    }
    fn capacity(cache: &mut ClassCache, class: Class, len: usize, threshold: usize) {
        assert!(
            matches!(cache.decide(Method::SetEnabled,"refused",[1;32],class,0),ClassDecision::Capacity { class:c,len:l,threshold:t } if c==class && l==len && t==threshold)
        );
    }

    #[test]
    fn normal_3740_plus_restore_100_hits_3840_threshold_exactly() {
        let mut cache = cache();
        for n in 0..3740 {
            let _p = fresh(
                &mut cache,
                Method::SetEnabled,
                &format!("n{n}"),
                Class::Normal,
                0,
            );
        }
        for n in 0..100 {
            let _p = fresh(
                &mut cache,
                Method::SetEnabled,
                &format!("r{n}"),
                Class::Restore,
                0,
            );
        }
        capacity(&mut cache, Class::Normal, 3840, 3840);
        for (class, target, prefix) in [
            (Class::Restore, 3968, "restore"),
            (Class::Lower, 4032, "lower"),
            (Class::Emergency, 4096, "estop"),
        ] {
            while cache.len() < target {
                let key = format!("{prefix}{}", cache.len());
                let _p = fresh(&mut cache, Method::SetEnabled, &key, class, 0);
            }
            capacity(&mut cache, class, target, target);
        }
    }
    #[test]
    fn pinned_nohandle_reduces_admissible_count_not_threshold() {
        let mut cache = cache();
        let p = fresh(&mut cache, Method::SetEnabled, "pinned", Class::Lower, 0);
        assert_eq!(
            cache.transition(Method::SetEnabled, "pinned", p.id(), EntryState::Sending, 1),
            Transition::Applied
        );
        assert_eq!(
            cache.transition(
                Method::SetEnabled,
                "pinned",
                p.id(),
                EntryState::NoHandle,
                2
            ),
            Transition::Applied
        );
        assert_eq!(cache.gc(u64::MAX), 0);
        for n in 0..3839 {
            let _p = fresh(
                &mut cache,
                Method::SetEnabled,
                &format!("n{n}"),
                Class::Normal,
                0,
            );
        }
        capacity(&mut cache, Class::Normal, 3840, 3840);
        assert_eq!(cache.state_counts()[6], 1);
    }
    #[test]
    fn sixty_fifth_distinct_estop_admitted_when_len_below_max() {
        let mut cache = cache();
        for n in 0..65 {
            let _p = fresh(
                &mut cache,
                Method::EStop,
                &n.to_string(),
                Class::Emergency,
                0,
            );
        }
        assert_eq!(cache.len(), 65);
    }
    #[test]
    fn estop_lifecycle_queued_publishing_committed_zero_recipients_weak_ack() {
        let mut cache = cache();
        let p = fresh(&mut cache, Method::EStop, "key", Class::Emergency, 0);
        assert_eq!(
            cache.transition(Method::EStop, "key", p.id(), EntryState::Publishing, 2),
            Transition::Applied
        );
        let terminal = CachedTerminal {
            state: TerminalState::Committed,
            effect: SideEffect::Committed,
            outcome: TypedOutcome::EStop {
                published_at_ns: 3,
                recipients: 0,
            },
        };
        assert_eq!(
            cache.finish(Method::EStop, "key", p.id(), terminal.clone(), 3),
            Transition::Applied
        );
        let ClassDecision::Replay(actual) =
            cache.decide(Method::EStop, "key", [1; 32], Class::Emergency, 4)
        else {
            panic!("零 recipients 弱 Ack 仍重放")
        };
        assert_eq!(actual, terminal);
    }
    #[test]
    fn estop_publish_pre_history_failure_abandons_post_history_error_is_nohandle() {
        let mut cache = cache();
        let p = fresh(&mut cache, Method::EStop, "pre", Class::Emergency, 0);
        assert_eq!(
            cache.transition(Method::EStop, "pre", p.id(), EntryState::Publishing, 1),
            Transition::Applied
        );
        assert_eq!(
            cache.abandon_by_id(
                Method::EStop,
                "pre",
                p.id(),
                AbandonReason::PublishFailedPreHistory,
                2
            ),
            Transition::Applied
        );
        let p = fresh(&mut cache, Method::EStop, "post", Class::Emergency, 0);
        assert_eq!(
            cache.transition(Method::EStop, "post", p.id(), EntryState::Publishing, 1),
            Transition::Applied
        );
        assert_eq!(
            cache.transition(Method::EStop, "post", p.id(), EntryState::NoHandle, 2),
            Transition::Applied
        );
        assert_eq!(
            cache.abandon_by_id(
                Method::EStop,
                "post",
                p.id(),
                AbandonReason::PublishFailedPreHistory,
                3
            ),
            Transition::Stale {
                current: Some(EntryState::NoHandle)
            }
        );
        assert_eq!(cache.gc(u64::MAX), 0);
        assert_eq!(cache.len(), 1);
    }
    #[test]
    fn reserves_exceed_capacity_rejected() {
        for max_entries in [0, 255, 256] {
            assert!(
                matches!(ClassCache::with_reserves(100,max_entries,Reserves::DEFAULT,EntryIdAllocator::global()),Err(ClassCacheConfigError::ReservesExceedCapacity { max_entries:m,reserves_sum:256 }) if m==max_entries)
            );
        }
        assert!(
            ClassCache::with_reserves(100, 257, Reserves::DEFAULT, EntryIdAllocator::global())
                .is_ok()
        );
    }
    #[test]
    fn reserves_sum_overflow_is_rejected_before_identity_or_entries() {
        for reserves in [
            Reserves {
                emergency: usize::MAX,
                lower: 1,
                restore: 0,
            },
            Reserves {
                emergency: usize::MAX - 1,
                lower: 1,
                restore: 1,
            },
        ] {
            let counter = Box::leak(Box::new(AtomicU64::new(1)));
            assert!(matches!(
                ClassCache::with_reserves(
                    100,
                    usize::MAX,
                    reserves,
                    EntryIdAllocator::from_counter(counter)
                ),
                Err(ClassCacheConfigError::ReservesSumOverflow)
            ));
            assert_eq!(counter.load(Ordering::Relaxed), 1);
        }
    }
    #[test]
    fn same_key_pending_inflight_terminal_replay_other_fp_conflict() {
        let mut cache = cache();
        let p = fresh(&mut cache, Method::SetEnabled, "key", Class::Restore, 0);
        assert!(matches!(
            cache.decide(Method::SetEnabled, "key", [1; 32], Class::Restore, 1),
            ClassDecision::InFlight {
                state: EntryState::Queued
            }
        ));
        assert!(
            matches!(cache.decide(Method::SetEnabled,"key",[2;32],Class::Restore,1),ClassDecision::Conflict { cached_fp,incoming_fp } if cached_fp==[1;32] && incoming_fp==[2;32])
        );
        sent(&mut cache, Method::SetEnabled, "key", &p);
        let expected = terminal();
        assert_eq!(
            cache.finish(Method::SetEnabled, "key", p.id(), expected.clone(), 5),
            Transition::Applied
        );
        let ClassDecision::Replay(actual) =
            cache.decide(Method::SetEnabled, "key", [1; 32], Class::Restore, 6)
        else {
            panic!("终态必须重放")
        };
        assert_eq!(actual, expected);
        let other = fresh(&mut cache, Method::ClearError, "key", Class::Normal, 6);
        assert_ne!(other.id(), p.id());
    }
    fn at_state(state: EntryState) -> (ClassCache, Placeholder, Method) {
        let mut cache = cache();
        let method = if state == EntryState::Publishing {
            Method::EStop
        } else {
            Method::SetEnabled
        };
        let p = fresh(&mut cache, method, "key", Class::Restore, 0);
        match state {
            EntryState::Queued => {}
            EntryState::Publishing => {
                assert_eq!(
                    cache.transition(method, "key", p.id(), state, 1),
                    Transition::Applied
                );
            }
            EntryState::Sending | EntryState::NoHandle => {
                assert_eq!(
                    cache.transition(method, "key", p.id(), EntryState::Sending, 1),
                    Transition::Applied
                );
                if state == EntryState::NoHandle {
                    assert_eq!(
                        cache.transition(method, "key", p.id(), state, 2),
                        Transition::Applied
                    );
                }
            }
            EntryState::Sent | EntryState::Orphan | EntryState::RepliedUnretired => {
                sent(&mut cache, method, "key", &p);
                if state != EntryState::Sent {
                    assert_eq!(
                        cache.transition(method, "key", p.id(), state, 3),
                        Transition::Applied
                    );
                }
            }
            EntryState::Committed | EntryState::StartedRetired | EntryState::UnknownRetired => {
                sent(&mut cache, method, "key", &p);
                let mut value = terminal();
                value.state = match state {
                    EntryState::Committed => TerminalState::Committed,
                    EntryState::StartedRetired => TerminalState::StartedRetired,
                    _ => TerminalState::UnknownRetired,
                };
                assert_eq!(
                    cache.finish(method, "key", p.id(), value, 7),
                    Transition::Applied
                );
            }
        }
        (cache, p, method)
    }
    const STATES: [EntryState; 10] = [
        EntryState::Queued,
        EntryState::Sending,
        EntryState::Publishing,
        EntryState::Sent,
        EntryState::Orphan,
        EntryState::RepliedUnretired,
        EntryState::NoHandle,
        EntryState::Committed,
        EntryState::StartedRetired,
        EntryState::UnknownRetired,
    ];
    #[test]
    fn transition_rejects_edges_outside_allowed_graph() {
        for from in STATES {
            for to in STATES {
                let (mut cache, p, method) = at_state(from);
                let before = cache.state_counts();
                let allowed = match from {
                    EntryState::Queued => {
                        matches!(to, EntryState::Sending | EntryState::Publishing)
                    }
                    EntryState::Sending => matches!(to, EntryState::Sent | EntryState::NoHandle),
                    EntryState::Sent => matches!(
                        to,
                        EntryState::Orphan | EntryState::RepliedUnretired | EntryState::NoHandle
                    ),
                    EntryState::Orphan => {
                        matches!(to, EntryState::RepliedUnretired | EntryState::NoHandle)
                    }
                    EntryState::Publishing | EntryState::RepliedUnretired => {
                        to == EntryState::NoHandle
                    }
                    _ => false,
                };
                assert_eq!(
                    cache.transition(method, "key", p.id(), to, 99),
                    if allowed {
                        Transition::Applied
                    } else {
                        Transition::Stale {
                            current: Some(from),
                        }
                    }
                );
                if !allowed {
                    assert_eq!(cache.state_counts(), before);
                }
            }
        }
        for state in [
            EntryState::Queued,
            EntryState::Sending,
            EntryState::RepliedUnretired,
            EntryState::NoHandle,
            EntryState::Committed,
            EntryState::StartedRetired,
            EntryState::UnknownRetired,
        ] {
            let (mut cache, p, method) = at_state(state);
            let at = cache.entries[&namespace(method, "key")].terminal_at;
            assert_eq!(
                cache.finish(method, "key", p.id(), terminal(), 90),
                Transition::Stale {
                    current: Some(state)
                }
            );
            for reason in [
                AbandonReason::NotDelivered,
                AbandonReason::ExpiredBeforeSend,
                AbandonReason::PublishFailedPreHistory,
                AbandonReason::RetiredNoneFailure,
            ] {
                if state != EntryState::Sending {
                    assert_eq!(
                        cache.abandon_by_id(method, "key", p.id(), reason, 99),
                        Transition::Stale {
                            current: Some(state)
                        }
                    );
                }
            }
            assert_eq!(cache.entries[&namespace(method, "key")].terminal_at, at);
        }
    }
    #[test]
    fn abandon_wrong_cache_returns_placeholder_both_unchanged() {
        let mut first = cache();
        let mut second = cache();
        let p = fresh(&mut first, Method::SetEnabled, "key", Class::Restore, 0);
        let q = fresh(&mut second, Method::SetEnabled, "key", Class::Restore, 0);
        let id = p.id();
        let returned = second.abandon(p).unwrap_err();
        assert_eq!(returned.id(), id);
        assert_eq!((first.len(), second.len()), (1, 1));
        first.abandon(returned).unwrap();
        assert_eq!(first.len(), 0);
        second.abandon(q).unwrap();
        let p = fresh(&mut first, Method::SetEnabled, "sending", Class::Restore, 0);
        assert_eq!(
            first.transition(
                Method::SetEnabled,
                "sending",
                p.id(),
                EntryState::Sending,
                1
            ),
            Transition::Applied
        );
        let returned = first.abandon(p).unwrap_err();
        assert_eq!(returned.class(), Class::Restore);
        assert_eq!(first.len(), 1);
    }
    #[test]
    fn foreign_entry_id_is_stale_on_transition_and_finish() {
        let mut first = cache();
        let mut second = cache();
        let p = fresh(&mut first, Method::SetEnabled, "key", Class::Restore, 0);
        let q = fresh(&mut second, Method::SetEnabled, "key", Class::Restore, 0);
        assert_ne!(p.id(), q.id());
        assert_eq!(
            first.transition(Method::SetEnabled, "key", q.id(), EntryState::Sending, 1),
            Transition::Stale {
                current: Some(EntryState::Queued)
            }
        );
        sent(&mut first, Method::SetEnabled, "key", &p);
        assert_eq!(
            first.finish(Method::SetEnabled, "key", q.id(), terminal(), 3),
            Transition::Stale {
                current: Some(EntryState::Sent)
            }
        );
        assert_eq!(second.state_counts()[0], 1);
    }
    #[test]
    fn id_exhaustion_returns_decision_without_insert_or_wrap() {
        let counter = Box::leak(Box::new(AtomicU64::new(u64::MAX - 1)));
        let mut cache = ClassCache::with_reserves(
            100,
            4096,
            Reserves::DEFAULT,
            EntryIdAllocator::from_counter(counter),
        )
        .unwrap();
        let p = fresh(&mut cache, Method::SetEnabled, "last", Class::Normal, 0);
        assert_eq!(p.id().0.get(), u64::MAX - 1);
        assert!(matches!(
            cache.decide(Method::SetEnabled, "next", [1; 32], Class::Normal, 0),
            ClassDecision::IdExhausted
        ));
        assert_eq!(cache.len(), 1);
        cache.abandon(p).unwrap();
        assert!(matches!(
            cache.decide(Method::SetEnabled, "last", [1; 32], Class::Normal, 0),
            ClassDecision::IdExhausted
        ));
        assert_eq!(cache.len(), 0);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }
    #[test]
    fn gc_only_terminals_from_terminal_at() {
        let mut cache = cache();
        let p = fresh(&mut cache, Method::SetEnabled, "late", Class::Restore, 0);
        sent(&mut cache, Method::SetEnabled, "late", &p);
        assert_eq!(cache.gc(1000), 0);
        assert_eq!(
            cache.finish(Method::SetEnabled, "late", p.id(), terminal(), 1000),
            Transition::Applied
        );
        let _queued = fresh(
            &mut cache,
            Method::SetEnabled,
            "pending",
            Class::Normal,
            1000,
        );
        assert_eq!(cache.gc(1099), 0);
        assert_eq!(
            cache.finish(Method::SetEnabled, "late", p.id(), terminal(), 1099),
            Transition::Stale {
                current: Some(EntryState::Committed)
            }
        );
        assert_eq!(cache.gc(1100), 1);
        assert_eq!(cache.gc(u64::MAX), 0);
        assert_eq!(cache.len(), 1);
        let p = fresh(
            &mut cache,
            Method::SetEnabled,
            "overflow",
            Class::Restore,
            0,
        );
        sent(&mut cache, Method::SetEnabled, "overflow", &p);
        assert_eq!(
            cache.finish(
                Method::SetEnabled,
                "overflow",
                p.id(),
                terminal(),
                u64::MAX - 50
            ),
            Transition::Applied
        );
        assert_eq!(cache.gc(u64::MAX), 0);
    }
    #[test]
    fn typed_outcome_replays_losslessly_per_method() {
        let mut cases = vec![
            (
                Method::SetEnabled,
                TypedOutcome::SetEnabled {
                    status: RpcStatus(255),
                    domain: SetEnabledStatus(254),
                    completed_at_ns: 11,
                    error_msg: "未丢失的失败".into(),
                },
            ),
            (
                Method::CancelTask,
                TypedOutcome::CancelTask {
                    status: ActionRpcStatus(253),
                    action_id: "cancel-action".into(),
                    cancel_at_ns: 12,
                    error_msg: "取消失败".into(),
                },
            ),
            (
                Method::ClearError,
                TypedOutcome::ClearError {
                    status: ActionRpcStatus(252),
                    cleared_at_ns: 13,
                    error_msg: "清错失败".into(),
                },
            ),
            (
                Method::EStop,
                TypedOutcome::EStop {
                    published_at_ns: 14,
                    recipients: 0,
                },
            ),
        ];
        for superseded in [false, true] {
            cases.push((
                Method::ClearEStop,
                TypedOutcome::ClearEStop {
                    status: RpcStatus(251),
                    domain: ClearEStopStatus(250),
                    superseded,
                    cleared_at_ns: 15,
                    dry_run: true,
                    error_msg: "清锁存结果".into(),
                },
            ));
        }
        for drain in [
            DrainOutcome::Registered,
            DrainOutcome::DrainFailedCompensated { cancel_ok: false },
            DrainOutcome::DrainFailedCompensated { cancel_ok: true },
        ] {
            cases.push((
                Method::InvokeTask,
                TypedOutcome::InvokeTask {
                    status: ActionRpcStatus(249),
                    action_id: "invoke-action".into(),
                    accepted_at_ns: 16,
                    error_msg: "drain 结果".into(),
                    drain,
                },
            ));
        }
        let mut cache = cache();
        for (n, (method, outcome)) in cases.into_iter().enumerate() {
            let key = n.to_string();
            let p = fresh(&mut cache, method, &key, Class::Restore, 0);
            if method == Method::EStop {
                assert_eq!(
                    cache.transition(method, &key, p.id(), EntryState::Publishing, 1),
                    Transition::Applied
                );
            } else {
                sent(&mut cache, method, &key, &p);
            }
            let wanted = CachedTerminal {
                state: TerminalState::Committed,
                effect: SideEffect::Committed,
                outcome,
            };
            assert_eq!(
                cache.finish(method, &key, p.id(), wanted.clone(), 17),
                Transition::Applied
            );
            let ClassDecision::Replay(actual) =
                cache.decide(method, &key, [1; 32], Class::Restore, 18)
            else {
                panic!("必须重放完整 typed 终态")
            };
            assert_eq!(actual, wanted);
        }
    }
}
