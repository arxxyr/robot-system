//! 网关唯一责任聚合：缓存决定状态，原许可决定额度；关闭和超时都不是退休凭证。

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use crate::class_cache::{
    AbandonReason, CachedTerminal, Class, ClassCache, ClassCacheConfigError, ClassDecision,
    EntryId, EntryIdAllocator, EntryState, Placeholder, Reserves, TerminalState, Transition,
    TypedOutcome,
};
use crate::forward_queue::{
    Popped, QueueFull, QueueProfile, RestoreLease, SharedForwardQueue, Ticket, TicketMismatch,
};
use crate::idempotency::{Fingerprint, Method};
use crate::reply_classify::{Expected, Verdict, classify, decode_reply};
use crate::rr_io_thread::RrForwardRequest;
use crate::wire_credit::{ClassShares, Credits, TopicKind, WirePermit};
use robot_msg::msg::{GetTaskStatusResponse, SideEffect};
use robot_transport::iceoryx2::RrStaticLimits;

pub(crate) type SharedLedger = Arc<Mutex<GatewayLedger>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RrRoute {
    Write(Method),
    ReadStatus,
}

/// 只存尚未清偿的原凭证；不复制缓存状态或信用计数。
#[derive(Default)]
struct HeldResponsibility {
    wire: Option<(Arc<Credits>, WirePermit)>,
    restore: Option<(SharedForwardQueue<RrForwardRequest>, RestoreLease)>,
    cancel_ticket: Option<Ticket<RrForwardRequest>>,
}
impl HeldResponsibility {
    fn settle(&mut self) -> bool {
        let mut accepted = true;
        if let Some((issuer, permit)) = self.wire.take()
            && let Err(rejected) = issuer.release(permit)
        {
            self.wire = Some((issuer, rejected.0));
            accepted = false;
        }
        if let Some((queue, lease)) = self.restore.take()
            && let Err(rejected) = queue.retire_restore(lease)
        {
            self.restore = Some((queue, rejected.0));
            accepted = false;
        }
        accepted && self.cancel_ticket.is_none()
    }
}

struct WriteOwner {
    placeholder: Option<Placeholder>,
    method: Method,
    key: String,
    expected: Expected,
    held: HeldResponsibility,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadState {
    Queued,
    Sending,
    Sent,
    Orphan,
    NoHandle,
}
struct ReadOwner {
    issuer: Arc<Credits>,
    request_id: String,
    state: ReadState,
    held: HeldResponsibility,
}

pub(crate) struct GatewayLedger {
    cache: ClassCache,
    writes: HashMap<EntryId, WriteOwner>,
    reads: HashMap<EntryId, ReadOwner>,
    faulted: bool,
    admission_closed: bool,
}

#[derive(Debug)]
pub(crate) enum BeginWrite {
    Fresh(EntryId),
    Replay(CachedTerminal),
    Refused(ClassDecision),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplyDisposition {
    Mismatch,
    Unretired,
    Settled,
    Faulted,
}

impl GatewayLedger {
    pub(crate) fn new(
        ttl_ns: u64,
        max_entries: usize,
    ) -> Result<SharedLedger, ClassCacheConfigError> {
        let cache = ClassCache::with_reserves(
            ttl_ns,
            max_entries,
            Reserves::DEFAULT,
            EntryIdAllocator::global(),
        )?;
        #[cfg(feature = "test-comm-fault-hook")]
        let cache = {
            let mut cache = cache;
            if std::env::var("ROBOT_TEST_PE_CACHE_OBSERVE").as_deref() == Ok("1") {
                cache.enable_admission_observation();
            }
            cache
        };
        Ok(Arc::new(Mutex::new(Self {
            cache,
            writes: HashMap::new(),
            reads: HashMap::new(),
            faulted: false,
            admission_closed: false,
        })))
    }

    pub(crate) fn begin_write(
        &mut self,
        method: Method,
        key: &str,
        fingerprint: Fingerprint,
        class: Class,
        request_id: &str,
        now_ns: u64,
    ) -> BeginWrite {
        // 即使已停止入口，重放/冲突仍先由同一缓存判断；新的占位由调用方 guard 清理。
        let decision = self.cache.decide(method, key, fingerprint, class, now_ns);
        #[cfg(feature = "test-comm-fault-hook")]
        self.observe_admission(method, key, class, request_id, now_ns, &decision);
        match decision {
            ClassDecision::Fresh(placeholder) => {
                let id = placeholder.id();
                self.writes.insert(
                    id,
                    WriteOwner {
                        placeholder: Some(placeholder),
                        method,
                        key: key.to_owned(),
                        expected: Expected {
                            method,
                            request_id: request_id.to_owned(),
                        },
                        held: HeldResponsibility::default(),
                    },
                );
                BeginWrite::Fresh(id)
            }
            ClassDecision::Replay(terminal) => BeginWrite::Replay(terminal),
            refused => {
                #[cfg(feature = "test-comm-fault-hook")]
                if matches!(
                    refused,
                    ClassDecision::InFlight {
                        state: EntryState::Orphan
                    }
                ) && let Some((id, _, _, _)) = self.cache.observe_entry(method, key)
                {
                    self.observe_r4c_write(id, "duplicate", now_ns);
                }
                BeginWrite::Refused(refused)
            }
        }
    }

    /// 仍在原 ledger 锁内读取本次判定；日志不授予许可或执行额外 GC。
    #[cfg(feature = "test-comm-fault-hook")]
    fn observe_admission(
        &self,
        method: Method,
        key: &str,
        class: Class,
        request_id: &str,
        now_ns: u64,
        decision: &ClassDecision,
    ) {
        let Some(snapshot) = self.cache.admission_observation() else {
            return;
        };
        let (kind, capacity_class, capacity_len, capacity_threshold, entry_id) = match decision {
            ClassDecision::Fresh(placeholder) => {
                ("Fresh", None, None, None, Some(placeholder.id()))
            }
            ClassDecision::Replay(_) => ("Replay", None, None, None, None),
            ClassDecision::Conflict { .. } => ("Conflict", None, None, None, None),
            ClassDecision::InFlight { .. } => ("InFlight", None, None, None, None),
            ClassDecision::Capacity {
                class,
                len,
                threshold,
            } => ("Capacity", Some(*class), Some(*len), Some(*threshold), None),
            ClassDecision::IdExhausted => ("IdExhausted", None, None, None, None),
        };
        tracing::info!(
            method = method.as_str(), request_id, key, now_ns,
            incoming_class = ?class, decision = kind, entry_id = ?entry_id,
            capacity_class = ?capacity_class, capacity_len = ?capacity_len,
            capacity_threshold = ?capacity_threshold,
            before_gc_len = snapshot.before_gc_len, gc_removed = snapshot.removed,
            post_gc_len = snapshot.post_gc_len, threshold = snapshot.threshold,
            max_entries = snapshot.max_entries, ttl_ns = snapshot.ttl_ns,
            class_stage = ?snapshot.class_stage,
            classes = "Emergency,Lower,Restore,Normal",
            stages = "Queued,Sending,Publishing,Sent,Orphan,RepliedUnretired,NoHandle,Committed,StartedRetired,UnknownRetired",
            point = "post_gc_pre_admission", "S48原缓存准入"
        );
    }

    pub(crate) fn begin_read(
        &mut self,
        request_id: &str,
        issuer: &Arc<Credits>,
    ) -> Result<EntryId, tonic::Status> {
        // 原 S1_TASK 队内容量 + 同一真实 topic 的 L 是完整只读责任上限；观察有信用
        // 不等于准入。所有尚未 push 的 owner 也在同一个 ledger 临界区内计入。
        let capacity = QueueProfile::S1_TASK
            .normal_cap
            .checked_add(issuer.share(Class::Normal))
            .ok_or_else(|| tonic::Status::resource_exhausted("read RR capacity overflow"))?;
        let owned = self
            .reads
            .values()
            .filter(|owner| Arc::ptr_eq(&owner.issuer, issuer))
            .count();
        if owned >= capacity {
            return Err(tonic::Status::resource_exhausted(
                "gateway read RR responsibility full",
            ));
        }
        let id = EntryIdAllocator::global()
            .next()
            .map_err(|_| tonic::Status::resource_exhausted("gateway entry identity exhausted"))?;
        self.reads.insert(
            id,
            ReadOwner {
                issuer: Arc::clone(issuer),
                request_id: request_id.to_owned(),
                state: ReadState::Queued,
                held: HeldResponsibility::default(),
            },
        );
        Ok(id)
    }

    #[cfg(any(test, feature = "test-freeze-hook"))]
    pub(crate) fn cached_terminal(&self, method: Method, key: &str) -> Option<CachedTerminal> {
        self.cache.cached_terminal(method, key)
    }
    pub(crate) fn stop_admission(&mut self) {
        self.admission_closed = true;
    }
    pub(crate) fn admission_closed(&self) -> bool {
        self.admission_closed || self.faulted
    }

    /// 终态缓存可以正常析构；活责任及任何归还失败都使退出聚合不能释放。
    pub(crate) fn responsibilities_empty(&self) -> bool {
        !self.faulted && self.writes.is_empty() && self.reads.is_empty()
    }

    /// 仅故障验收读取原只读责任状态；不返回可写字段或额外许可。
    #[cfg(all(test, feature = "test-rr-fixtures"))]
    pub(crate) fn fixture_read_state(&self, id: EntryId) -> Option<EntryState> {
        self.reads.get(&id).map(|owner| match owner.state {
            ReadState::Queued => EntryState::Queued,
            ReadState::Sending => EntryState::Sending,
            ReadState::Sent => EntryState::Sent,
            ReadState::Orphan => EntryState::Orphan,
            ReadState::NoHandle => EntryState::NoHandle,
        })
    }

    fn abandon_queued(&mut self, id: EntryId) {
        if let Some(owner) = self.writes.get_mut(&id) {
            let Some(placeholder) = owner.placeholder.take() else {
                self.faulted = true;
                return;
            };
            match self.cache.abandon(placeholder) {
                Ok(()) => {
                    self.writes.remove(&id);
                }
                Err(original) => {
                    owner.placeholder = Some(original);
                    self.faulted = true;
                }
            }
        } else if self
            .reads
            .get(&id)
            .is_some_and(|owner| owner.state == ReadState::Queued)
        {
            self.reads.remove(&id);
        } else {
            self.faulted = true;
        }
    }

    fn attachable(
        &self,
        id: EntryId,
        route: RrRoute,
        request_id: &str,
        credits: &Arc<Credits>,
    ) -> bool {
        match route {
            RrRoute::Write(method) => self.writes.get(&id).is_some_and(|owner| {
                owner.method == method
                    && owner.expected.request_id == request_id
                    && owner.held.wire.is_none()
                    && owner.held.restore.is_none()
                    && owner.placeholder.is_some()
            }),
            RrRoute::ReadStatus => self.reads.get(&id).is_some_and(|owner| {
                Arc::ptr_eq(&owner.issuer, credits)
                    && owner.request_id == request_id
                    && owner.state == ReadState::Queued
                    && owner.held.wire.is_none()
                    && owner.held.restore.is_none()
            }),
        }
    }

    pub(crate) fn transition(&mut self, id: EntryId, to: EntryState, now_ns: u64) -> bool {
        if let Some(owner) = self.writes.get(&id) {
            let applied = self
                .cache
                .transition(owner.method, &owner.key, id, to, now_ns)
                == Transition::Applied;
            #[cfg(feature = "test-comm-fault-hook")]
            if applied && to == EntryState::Orphan {
                self.observe_r4c_write(id, "orphan", now_ns);
            }
            return applied;
        }
        let Some(owner) = self.reads.get_mut(&id) else {
            return false;
        };
        let next = match (owner.state, to) {
            (ReadState::Queued, EntryState::Sending) => ReadState::Sending,
            (ReadState::Sending, EntryState::Sent) => ReadState::Sent,
            (ReadState::Sent, EntryState::Orphan) => ReadState::Orphan,
            (ReadState::Sending | ReadState::Sent | ReadState::Orphan, EntryState::NoHandle) => {
                ReadState::NoHandle
            }
            _ => return false,
        };
        owner.state = next;
        true
    }

    /// 调用者仍持原ledger锁；只绑定已存在的同EntryId缓存和责任原件。
    #[cfg(feature = "test-comm-fault-hook")]
    fn observe_r4c_write(&self, id: EntryId, point: &str, at_ns: u64) {
        let Some(owner) = self.writes.get(&id) else {
            return;
        };
        if owner.method != Method::ClearEStop
            || std::env::var("ROBOT_TEST_R4C_LEDGER_KEY").as_deref() != Ok(owner.key.as_str())
        {
            return;
        }
        let Some((cache_id, state, terminal, fingerprint)) =
            self.cache.observe_entry(owner.method, &owner.key)
        else {
            return;
        };
        tracing::info!(
            point, at_ns, method = owner.method.as_str(), key = owner.key,
            request_id = owner.expected.request_id, entry_id = ?id, cache_entry_id = ?cache_id,
            state = ?state, terminal, fingerprint = ?fingerprint,
            placeholder = owner.placeholder.is_some(), wire = owner.held.wire.is_some(),
            restore = owner.held.restore.is_some(), cancel_ticket = owner.held.cancel_ticket.is_some(),
            admission_closed = self.admission_closed, faulted = self.faulted,
            "R4C原网关责任"
        );
    }

    fn hold_pop(
        &mut self,
        id: EntryId,
        credits: Arc<Credits>,
        permit: WirePermit,
        restore: Option<(SharedForwardQueue<RrForwardRequest>, RestoreLease)>,
    ) {
        let held = match self.writes.get_mut(&id) {
            Some(owner) => &mut owner.held,
            None => {
                &mut self
                    .reads
                    .get_mut(&id)
                    .expect("attach 已核原只读 owner")
                    .held
            }
        };
        held.wire = Some((credits, permit));
        held.restore = restore;
    }

    fn settle_applied(&mut self, id: EntryId) -> bool {
        if let Some(owner) = self.writes.get_mut(&id) {
            if owner.held.settle() {
                self.writes.remove(&id);
                return true;
            }
        } else if let Some(owner) = self.reads.get_mut(&id)
            && owner.held.settle()
        {
            self.reads.remove(&id);
            return true;
        }
        self.faulted = true;
        false
    }

    pub(crate) fn abandon_before_delivery(
        &mut self,
        id: EntryId,
        reason: AbandonReason,
        now_ns: u64,
    ) -> bool {
        let applied = match self.writes.get(&id) {
            Some(owner) => {
                self.cache
                    .abandon_by_id(owner.method, &owner.key, id, reason, now_ns)
                    == Transition::Applied
            }
            None => {
                self.reads
                    .get(&id)
                    .is_some_and(|owner| owner.state == ReadState::Sending)
                    && matches!(
                        reason,
                        AbandonReason::ExpiredBeforeSend | AbandonReason::NotDelivered
                    )
            }
        };
        if applied {
            self.settle_applied(id)
        } else {
            self.faulted = true;
            false
        }
    }

    pub(crate) fn finish_reply(
        &mut self,
        id: EntryId,
        bytes: &[u8],
        now_ns: u64,
    ) -> ReplyDisposition {
        if let Some(owner) = self.reads.get(&id) {
            let Ok(response) = flatbuffers::root::<GetTaskStatusResponse>(bytes) else {
                return ReplyDisposition::Mismatch;
            };
            if response.request_id() != Some(owner.request_id.as_str())
                || !matches!(owner.state, ReadState::Sent | ReadState::Orphan)
            {
                return ReplyDisposition::Mismatch;
            }
            return if self.settle_applied(id) {
                ReplyDisposition::Settled
            } else {
                ReplyDisposition::Faulted
            };
        }
        let Some(owner) = self.writes.get(&id) else {
            return ReplyDisposition::Mismatch;
        };
        let Ok(view) = decode_reply(&owner.expected, bytes) else {
            return ReplyDisposition::Mismatch;
        };
        let verdict = classify(&owner.expected, &view);
        let applied = match verdict {
            Verdict::Mismatch => return ReplyDisposition::Mismatch,
            Verdict::Pending => {
                return match self.cache.transition(
                    owner.method,
                    &owner.key,
                    id,
                    EntryState::RepliedUnretired,
                    now_ns,
                ) {
                    Transition::Applied => ReplyDisposition::Unretired,
                    Transition::Stale { .. } => ReplyDisposition::Mismatch,
                };
            }
            Verdict::Terminal(terminal) => {
                self.cache
                    .finish(owner.method, &owner.key, id, terminal, now_ns)
            }
            Verdict::Abandon => self.cache.abandon_by_id(
                owner.method,
                &owner.key,
                id,
                AbandonReason::RetiredNoneFailure,
                now_ns,
            ),
        };
        match applied {
            Transition::Applied => {
                if self.settle_applied(id) {
                    ReplyDisposition::Settled
                } else {
                    ReplyDisposition::Faulted
                }
            }
            Transition::Stale { .. } => ReplyDisposition::Mismatch,
        }
    }

    pub(crate) fn finish_estop(
        &mut self,
        id: EntryId,
        published_at_ns: u64,
        recipients: usize,
    ) -> bool {
        let Some(owner) = self.writes.get(&id) else {
            self.faulted = true;
            return false;
        };
        if owner.method != Method::EStop {
            self.faulted = true;
            return false;
        }
        let terminal = CachedTerminal {
            state: TerminalState::Committed,
            effect: SideEffect::Committed,
            outcome: TypedOutcome::EStop {
                published_at_ns,
                recipients,
            },
        };
        match self
            .cache
            .finish(owner.method, &owner.key, id, terminal, published_at_ns)
        {
            Transition::Applied => self.settle_applied(id),
            Transition::Stale { .. } => {
                self.faulted = true;
                false
            }
        }
    }

    fn retain_wrong_ticket(&mut self, id: EntryId, ticket: Ticket<RrForwardRequest>) {
        // 每个 HandlerWait 只有一个原票据，故不存在可覆盖的第二张票。
        let held = match self.writes.get_mut(&id) {
            Some(owner) => &mut owner.held,
            None => &mut self.reads.get_mut(&id).expect("错误票据仍有原 owner").held,
        };
        assert!(held.cancel_ticket.is_none(), "原票据只能移交一次");
        held.cancel_ticket = Some(ticket);
        self.faulted = true;
    }
}

/// 载荷在锁外析构时，仅撤销尚未发送的同一占位；不拥有在途退休权。
pub(crate) struct QueuedGuard {
    ledger: SharedLedger,
    id: EntryId,
    armed: bool,
}
impl QueuedGuard {
    pub(crate) fn new(ledger: &SharedLedger, id: EntryId) -> Self {
        Self {
            ledger: Arc::clone(ledger),
            id,
            armed: true,
        }
    }
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
    pub(crate) fn belongs_to(&self, ledger: &SharedLedger, id: EntryId) -> bool {
        self.armed && self.id == id && Arc::ptr_eq(&self.ledger, ledger)
    }
}
impl Drop for QueuedGuard {
    fn drop(&mut self) {
        if self.armed {
            self.ledger
                .lock()
                .expect("网关责任锁未中毒")
                .abandon_queued(self.id);
        }
    }
}

/// 同一真实 topic 的逻辑端口；clone 只共享原队列、原信用与唯一 ledger。
#[derive(Clone)]
pub struct RrPort {
    pub(crate) queue: SharedForwardQueue<RrForwardRequest>,
    pub(crate) credits: Arc<Credits>,
    pub(crate) ledger: SharedLedger,
    pub(crate) route: RrRoute,
    stopped: Arc<AtomicBool>,
}
impl RrPort {
    pub(crate) fn new(
        route: RrRoute,
        kind: TopicKind,
        limits: &RrStaticLimits,
        ledger: &SharedLedger,
    ) -> Result<Self, String> {
        let shares = ClassShares::for_topic(kind, limits)
            .map_err(|error| format!("RR 实际份额非法: {error:?}"))?;
        let profile = match kind {
            TopicKind::S0Control => QueueProfile::S0_CONTROL,
            TopicKind::S1Task => QueueProfile::S1_TASK,
        };
        Ok(Self {
            queue: SharedForwardQueue::new(profile)
                .map_err(|error| format!("RR 队列身份耗尽: {error:?}"))?,
            credits: Arc::new(
                Credits::new(shares).map_err(|error| format!("RR 信用身份耗尽: {error:?}"))?,
            ),
            ledger: Arc::clone(ledger),
            route,
            stopped: Arc::new(AtomicBool::new(false)),
        })
    }
    pub(crate) fn stop_admission(&self) {
        self.stopped.store(true, Ordering::Release);
    }
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
            || self
                .ledger
                .lock()
                .expect("网关责任锁未中毒")
                .admission_closed()
    }
    pub(crate) fn has_credit(&self, class: Class) -> bool {
        self.credits.used(class) < self.credits.share(class)
    }
    pub(crate) fn push(
        &self,
        item: RrForwardRequest,
    ) -> Result<Ticket<RrForwardRequest>, QueueFull> {
        if self.is_stopped() {
            return Err(QueueFull { class: item.class });
        }
        self.queue.push(item)
    }
    #[allow(
        clippy::result_large_err,
        reason = "校验失败必须返原出队项及其许可，不能拆散原取消与信用责任"
    )]
    pub(crate) fn attach_pop(
        &self,
        mut popped: Popped<RrForwardRequest>,
        now_ns: u64,
    ) -> Result<RrForwardRequest, Popped<RrForwardRequest>> {
        let id = popped.item.entry_id;
        let mut ledger = self.ledger.lock().expect("网关责任锁未中毒");
        let valid = popped.item.guard.belongs_to(&self.ledger, id)
            && popped.item.route == self.route
            && popped.item.class == popped.permit.class()
            && ledger.attachable(id, self.route, &popped.item.request_id, &self.credits);
        if !valid || !ledger.transition(id, EntryState::Sending, now_ns) {
            self.stop_admission();
            return Err(popped);
        }
        popped.item.guard.disarm();
        ledger.hold_pop(
            id,
            Arc::clone(&self.credits),
            popped.permit,
            popped
                .restore_lease
                .map(|lease| (self.queue.clone(), lease)),
        );
        Ok(popped.item)
    }
    pub(crate) fn cancel(&self, id: EntryId, ticket: Ticket<RrForwardRequest>) {
        match self.queue.remove(ticket) {
            Ok(item) => drop(item),
            Err(TicketMismatch(original)) => {
                self.stop_admission();
                self.ledger
                    .lock()
                    .expect("网关责任锁未中毒")
                    .retain_wrong_ticket(id, original);
            }
        }
    }
    pub(crate) fn responsibility_empty(&self) -> bool {
        [Class::Lower, Class::Restore, Class::Normal]
            .into_iter()
            .all(|class| self.queue.len(class) == 0 && self.credits.used(class) == 0)
            && self.queue.restore_active() == 0
    }
}

pub(crate) struct HandlerWait {
    pub(crate) receiver: tokio::sync::oneshot::Receiver<Vec<u8>>,
    port: RrPort,
    id: EntryId,
    ticket: Option<Ticket<RrForwardRequest>>,
}
impl HandlerWait {
    pub(crate) fn new(
        port: &RrPort,
        id: EntryId,
        ticket: Ticket<RrForwardRequest>,
        receiver: tokio::sync::oneshot::Receiver<Vec<u8>>,
    ) -> Self {
        Self {
            receiver,
            port: port.clone(),
            id,
            ticket: Some(ticket),
        }
    }
}
impl Drop for HandlerWait {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            self.port.cancel(self.id, ticket);
        }
    }
}

pub(crate) const fn class_for_write(method: Method, dry_run: bool, enabled: bool) -> Class {
    match (method, dry_run, enabled) {
        (Method::EStop, _, _) => Class::Emergency,
        (Method::ClearEStop | Method::SetEnabled, true, _)
        | (Method::InvokeTask | Method::CancelTask | Method::ClearError, _, _) => Class::Normal,
        (Method::ClearEStop, false, _) | (Method::SetEnabled, false, true) => Class::Restore,
        (Method::SetEnabled, false, false) => Class::Lower,
    }
}

pub(crate) struct WriteRequest<'a> {
    pub(crate) method: Method,
    pub(crate) request_id: &'a str,
    pub(crate) key: &'a str,
    pub(crate) fingerprint: Fingerprint,
    pub(crate) class: Class,
    pub(crate) timeout_ms: u64,
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "拒绝转换按值消费原分类结果，保持原占位或缓存终态的拥有期"
)]
pub(crate) fn refusal_status(refused: ClassDecision) -> tonic::Status {
    match refused {
        ClassDecision::Conflict { .. } => tonic::Status::already_exists(
            "idempotency_key already used with a different request fingerprint",
        ),
        ClassDecision::InFlight { state } => {
            tonic::Status::aborted(format!("request remains in flight: {state:?}"))
        }
        ClassDecision::Capacity { len, threshold, .. } => {
            tonic::Status::resource_exhausted(format!("idempotency cache full ({len}/{threshold})"))
        }
        ClassDecision::IdExhausted => {
            tonic::Status::resource_exhausted("gateway entry identity exhausted")
        }
        ClassDecision::Fresh(_) | ClassDecision::Replay(_) => {
            tonic::Status::internal("invalid cache decision routing")
        }
    }
}

/// 重放仍走实际 typed builder 和原 gRPC 解码映射，不由 handler 丢弃失败事实。
#[allow(
    clippy::too_many_lines,
    reason = "各终态在同一穷尽匹配中重建完整协议字段，保留原失败与副作用事实"
)]
fn replay_wire(request_id: &str, terminal: &CachedTerminal) -> Result<Vec<u8>, tonic::Status> {
    use crate::class_cache::DrainOutcome;
    use robot_msg::build_helpers::{
        ClearResponseMeta, build_cancel_task_response_typed, build_clear_error_response_typed,
        build_clear_estop_response_typed, build_invoke_task_response_typed,
        build_set_enabled_response_typed,
    };
    use robot_msg::msg::InvokeOutcome;
    let effect = terminal.effect;
    Ok(match &terminal.outcome {
        TypedOutcome::ClearEStop {
            status,
            domain,
            superseded,
            cleared_at_ns,
            dry_run,
            error_msg,
        } => build_clear_estop_response_typed(
            request_id,
            *status,
            error_msg,
            *cleared_at_ns,
            *dry_run,
            ClearResponseMeta {
                domain: *domain,
                superseded: *superseded,
                effect,
                retired: true,
            },
        ),
        TypedOutcome::SetEnabled {
            status,
            domain,
            completed_at_ns,
            error_msg,
        } => build_set_enabled_response_typed(
            request_id,
            *status,
            *domain,
            *completed_at_ns,
            error_msg,
            effect,
            true,
        ),
        TypedOutcome::InvokeTask {
            status,
            action_id,
            accepted_at_ns,
            error_msg,
            drain,
        } => {
            let outcome = match drain {
                DrainOutcome::NotSubmitted => InvokeOutcome::NotSubmitted,
                DrainOutcome::Registered => InvokeOutcome::Registered,
                DrainOutcome::DrainFailedCompensated { cancel_ok: true } => {
                    InvokeOutcome::DrainFailedCancelAccepted
                }
                DrainOutcome::DrainFailedCompensated { cancel_ok: false } => {
                    InvokeOutcome::DrainFailedCancelRejected
                }
                DrainOutcome::Unknown { raw } => InvokeOutcome(*raw),
            };
            build_invoke_task_response_typed(
                request_id,
                *status,
                error_msg,
                action_id,
                *accepted_at_ns,
                effect,
                true,
                outcome,
            )
        }
        TypedOutcome::CancelTask {
            status,
            action_id,
            cancel_at_ns,
            error_msg,
        } => build_cancel_task_response_typed(
            request_id,
            *status,
            error_msg,
            action_id,
            *cancel_at_ns,
            effect,
            true,
        ),
        TypedOutcome::ClearError {
            status,
            cleared_at_ns,
            error_msg,
        } => build_clear_error_response_typed(
            request_id,
            *status,
            error_msg,
            *cleared_at_ns,
            effect,
            true,
        ),
        TypedOutcome::EStop { .. } => {
            return Err(tonic::Status::internal("EStop cannot use RR replay"));
        }
    })
}

pub(crate) async fn exchange_write(
    port: &RrPort,
    request: WriteRequest<'_>,
    clock: &dyn robot_core::time::RobotClock,
    shutdown: &tokio::sync::watch::Receiver<bool>,
    grace_ms: u64,
    build: impl FnOnce(u64, u64) -> Result<Vec<u8>, tonic::Status>,
) -> Result<Vec<u8>, tonic::Status> {
    if port.route != RrRoute::Write(request.method) {
        return Err(tonic::Status::internal("RR method route mismatch"));
    }
    let decision = port.ledger.lock().expect("网关责任锁未中毒").begin_write(
        request.method,
        request.key,
        request.fingerprint,
        request.class,
        request.request_id,
        clock.now().0,
    );
    let id = match decision {
        BeginWrite::Fresh(id) => id,
        BeginWrite::Replay(terminal) => return replay_wire(request.request_id, &terminal),
        BeginWrite::Refused(refused) => return Err(refusal_status(refused)),
    };
    let guard = QueuedGuard::new(&port.ledger, id);
    if *shutdown.borrow() || port.is_stopped() {
        return Err(tonic::Status::unavailable(
            "gateway is shutting down; new requests rejected",
        ));
    }
    if !port.has_credit(request.class) {
        return Err(tonic::Status::resource_exhausted(
            "gateway RR wire credit full; retry shortly",
        ));
    }
    let now_ns = clock.now().0;
    let deadline_ns = now_ns.saturating_add(request.timeout_ms.saturating_mul(1_000_000));
    let wire_bytes = build(deadline_ns, now_ns)?;
    if *shutdown.borrow() {
        return Err(tonic::Status::unavailable(
            "gateway is shutting down; new requests rejected",
        ));
    }
    let (oneshot_tx, receiver) = tokio::sync::oneshot::channel();
    let item = RrForwardRequest {
        wire_bytes,
        oneshot_tx,
        request_id: request.request_id.to_owned(),
        entry_id: id,
        class: request.class,
        deadline_ns,
        route: port.route,
        guard,
    };
    let ticket = port.push(item).map_err(|_| {
        tonic::Status::resource_exhausted("gateway RR I/O forward queue full; retry shortly")
    })?;
    await_handler_reply(
        HandlerWait::new(port, id, ticket, receiver),
        request.timeout_ms,
        grace_ms,
    )
    .await
}

pub(crate) async fn exchange_read(
    port: &RrPort,
    request_id: &str,
    deadline_ns: u64,
    wire_bytes: Vec<u8>,
    timeout_ms: u64,
    grace_ms: u64,
) -> Result<Vec<u8>, tonic::Status> {
    if port.route != RrRoute::ReadStatus {
        return Err(tonic::Status::internal("read RR method route mismatch"));
    }
    if port.is_stopped() {
        return Err(tonic::Status::unavailable("gateway RR I/O thread stopped"));
    }
    if !port.has_credit(Class::Normal) {
        return Err(tonic::Status::resource_exhausted(
            "gateway read RR credit full",
        ));
    }
    let id = port
        .ledger
        .lock()
        .expect("网关责任锁未中毒")
        .begin_read(request_id, &port.credits)?;
    let guard = QueuedGuard::new(&port.ledger, id);
    let (oneshot_tx, receiver) = tokio::sync::oneshot::channel();
    let item = RrForwardRequest {
        wire_bytes,
        oneshot_tx,
        request_id: request_id.to_owned(),
        entry_id: id,
        class: Class::Normal,
        deadline_ns,
        route: port.route,
        guard,
    };
    let ticket = port
        .push(item)
        .map_err(|_| tonic::Status::resource_exhausted("gateway read RR forward queue full"))?;
    await_handler_reply(
        HandlerWait::new(port, id, ticket, receiver),
        timeout_ms,
        grace_ms,
    )
    .await
}

async fn await_handler_reply(
    mut wait: HandlerWait,
    timeout_ms: u64,
    grace_ms: u64,
) -> Result<Vec<u8>, tonic::Status> {
    let total_ms = timeout_ms.saturating_add(grace_ms);
    match tokio::time::timeout(
        std::time::Duration::from_millis(total_ms),
        &mut wait.receiver,
    )
    .await
    {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(_)) => Err(tonic::Status::unavailable("gateway RR I/O unavailable")),
        Err(_) => Err(tonic::Status::deadline_exceeded(format!(
            "no RR response within {total_ms} ms (timeout_ms + grace_ms)"
        ))),
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn concurrent_read_registration_is_bounded_before_any_queue_push_and_guard_returns_it() {
        let ledger = GatewayLedger::new(300_000_000_000, 4096).unwrap();
        let limits = robot_transport::iceoryx2::RrProfile::S1_TASK
            .validate()
            .unwrap();
        let credits = Arc::new(
            Credits::new(ClassShares::for_topic(TopicKind::S1Task, &limits).unwrap()).unwrap(),
        );
        let capacity = QueueProfile::S1_TASK.normal_cap + limits.l;
        let attempts = capacity + 16;
        let barrier = Barrier::new(attempts);
        let ids = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..attempts)
                .map(|index| {
                    let ledger = &ledger;
                    let credits = &credits;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        ledger
                            .lock()
                            .unwrap()
                            .begin_read(&format!("read-{index}"), credits)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(ids.iter().filter(|result| result.is_ok()).count(), capacity);
        assert_eq!(ledger.lock().unwrap().reads.len(), capacity);
        assert_eq!(credits.used(Class::Normal), 0, "登记不等于获取 wire 许可");
        let guards: Vec<_> = ids
            .into_iter()
            .filter_map(Result::ok)
            .map(|id| QueuedGuard::new(&ledger, id))
            .collect();
        drop(guards);
        assert!(ledger.lock().unwrap().responsibilities_empty());
        let id = ledger
            .lock()
            .unwrap()
            .begin_read("after-cancel", &credits)
            .unwrap();
        drop(QueuedGuard::new(&ledger, id));
        assert!(ledger.lock().unwrap().responsibilities_empty());
    }
}

/// 旧 handler 单元测试的显式传输夹具；只模拟发送/响应边界，不宣称真实 iceoryx。
/// 准入、队列、原许可、结果分类与清偿都调用上面的同一生产实现。
#[cfg(test)]
pub(crate) mod handler_fixture {
    use super::*;
    use robot_core::time::{RobotClock, SystemClock};
    use std::time::{Duration, Instant};

    pub(crate) fn ledger(max_entries: usize) -> SharedLedger {
        // 旧 handler 容量边界使用零预留的小容量夹具；生产唯一构造仍固定 DEFAULT。
        Arc::new(Mutex::new(GatewayLedger {
            cache: ClassCache::with_reserves(
                60_000_000_000,
                max_entries,
                Reserves {
                    restore: 0,
                    lower: 0,
                    emergency: 0,
                },
                EntryIdAllocator::global(),
            )
            .unwrap(),
            writes: HashMap::new(),
            reads: HashMap::new(),
            faulted: false,
            admission_closed: false,
        }))
    }
    pub(crate) fn port(ledger: &SharedLedger, route: RrRoute) -> RrPort {
        let kind = match route {
            RrRoute::Write(Method::ClearEStop | Method::SetEnabled) => TopicKind::S0Control,
            _ => TopicKind::S1Task,
        };
        RrPort::new(route, kind, &RrStaticLimits { l: 4, c: 8 }, ledger).unwrap()
    }
    pub(crate) fn receiver(port: &RrPort) -> TestRrReceiver {
        TestRrReceiver(Arc::new(ReceiverOwner(port.clone())))
    }
    struct ReceiverOwner(RrPort);
    impl Drop for ReceiverOwner {
        fn drop(&mut self) {
            self.0.stop_admission();
            if !self.0.ledger.lock().unwrap().responsibilities_empty() {
                // 未退休责任仍由原 Arc 持到本测试进程退出；停止不退款。
                let _held = std::mem::ManuallyDrop::new(self.0.clone());
            }
        }
    }
    #[derive(Clone)]
    pub(crate) struct TestRrReceiver(Arc<ReceiverOwner>);
    pub(crate) struct TestRequest {
        pub(crate) wire_bytes: Vec<u8>,
        pub(crate) request_id: String,
        pub(crate) oneshot_tx: TestReply,
    }
    pub(crate) struct TestReply {
        sender: Option<tokio::sync::oneshot::Sender<Vec<u8>>>,
        port: RrPort,
        id: EntryId,
        settled: bool,
    }
    impl TestReply {
        pub(crate) fn send(mut self, bytes: Vec<u8>) -> Result<(), Vec<u8>> {
            let disposition =
                self.port
                    .ledger
                    .lock()
                    .unwrap()
                    .finish_reply(self.id, &bytes, SystemClock.now().0);
            match disposition {
                ReplyDisposition::Settled | ReplyDisposition::Unretired => {
                    self.settled = disposition == ReplyDisposition::Settled;
                    self.sender.take().unwrap().send(bytes)
                }
                // 模拟 transport 接收到错帧；生产分类器拒绝它，绝不替 handler 确认退休。
                ReplyDisposition::Mismatch | ReplyDisposition::Faulted => Ok(()),
            }
        }
    }
    impl Drop for TestReply {
        fn drop(&mut self) {
            if !self.settled {
                self.port.ledger.lock().unwrap().transition(
                    self.id,
                    EntryState::NoHandle,
                    SystemClock.now().0,
                );
            }
        }
    }
    impl TestRrReceiver {
        pub(crate) fn try_recv(&self) -> Result<TestRequest, crossbeam_channel::TryRecvError> {
            let port = &self.0.0;
            let popped = port
                .queue
                .pop_eligible(&port.credits)
                .ok_or(crossbeam_channel::TryRecvError::Empty)?;
            let item = match port.attach_pop(popped, SystemClock.now().0) {
                Ok(item) => item,
                Err(original) => {
                    let _held = std::mem::ManuallyDrop::new(original);
                    panic!("真实队列弹出必须能挂接原 owner");
                }
            };
            assert!(port.ledger.lock().unwrap().transition(
                item.entry_id,
                EntryState::Sent,
                SystemClock.now().0
            ));
            Ok(TestRequest {
                wire_bytes: item.wire_bytes,
                request_id: item.request_id,
                oneshot_tx: TestReply {
                    sender: Some(item.oneshot_tx),
                    port: port.clone(),
                    id: item.entry_id,
                    settled: false,
                },
            })
        }
        pub(crate) fn recv_timeout(
            &self,
            timeout: Duration,
        ) -> Result<TestRequest, crossbeam_channel::RecvTimeoutError> {
            let deadline = Instant::now() + timeout;
            loop {
                if let Ok(item) = self.try_recv() {
                    return Ok(item);
                }
                if Instant::now() >= deadline {
                    return Err(crossbeam_channel::RecvTimeoutError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    impl GatewayLedger {
        pub(crate) fn len(&self) -> usize {
            self.cache.len()
        }
    }
}
