//! 带唯一取消票据的分类转发队列；锁内只移交记录，业务载荷在锁外析构。

use crate::class_cache::{Class, EntryId, IdExhausted};
use crate::wire_credit::{Credits, WirePermit};
use std::collections::VecDeque;
use std::num::NonZeroU64;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicU64, Ordering},
};

static NEXT_QUEUE_ID: AtomicU64 = AtomicU64::new(1);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueId(NonZeroU64);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueProfile {
    pub high_cap: usize,
    pub restore_active_cap: usize,
    pub normal_cap: usize,
}
impl QueueProfile {
    pub const S0_CONTROL: Self = Self {
        high_cap: 16,
        restore_active_cap: 8,
        normal_cap: 48,
    };
    pub const S1_TASK: Self = Self {
        high_cap: 0,
        restore_active_cap: 0,
        normal_cap: 64,
    };
}
pub trait QueueItem {
    fn entry_id(&self) -> EntryId;
    fn class(&self) -> Class;
    fn deadline_ns(&self) -> u64;
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueFull {
    pub class: Class,
}

/// 已排队及已发送未退休阶段共用的 Restore 占位，不按 Drop 归还。
/// ```compile_fail
/// use robot_rpc::forward_queue::RestoreLease;
/// fn duplicate(lease:RestoreLease) { let second=lease.clone(); }
/// ```
/// ```compile_fail
/// use robot_rpc::forward_queue::{QueueId,RestoreLease};
/// fn forge(issuer:QueueId) { let lease=RestoreLease { issuer,serial:std::num::NonZeroU64::MIN }; }
/// ```
#[derive(Debug)]
pub struct RestoreLease {
    issuer: QueueId,
    serial: NonZeroU64,
}
#[derive(Debug)]
pub struct LeaseMismatch(pub RestoreLease);

struct Record<T> {
    item: T,
    id: EntryId,
    serial: NonZeroU64,
    restore_lease: Option<RestoreLease>,
}
struct ForwardQueue<T> {
    id: QueueId,
    profile: QueueProfile,
    lower: VecDeque<Record<T>>,
    restore: VecDeque<Record<T>>,
    normal: VecDeque<Record<T>>,
    restore_active: usize,
    next_serial: NonZeroU64,
}
impl<T> ForwardQueue<T> {
    // 公共和队内取消共用唯一退还路径；凭证按值消费。
    #[allow(clippy::needless_pass_by_value)]
    fn retire_restore(&mut self, lease: RestoreLease) -> Result<(), LeaseMismatch> {
        if lease.issuer != self.id {
            return Err(LeaseMismatch(lease));
        }
        debug_assert!(lease.serial < self.next_serial);
        self.restore_active -= 1;
        Ok(())
    }
    fn remove_record(&mut self, id: EntryId, serial: NonZeroU64) -> Option<Record<T>> {
        let mut removed = None;
        for lane in [&mut self.lower, &mut self.restore, &mut self.normal] {
            if let Some(index) = lane
                .iter()
                .position(|record| record.id == id && record.serial == serial)
            {
                removed = lane.remove(index);
                break;
            }
        }
        let mut record = removed?;
        if let Some(lease) = record.restore_lease.take() {
            self.retire_restore(lease).expect("队内租约必属于此队列");
        }
        Some(record)
    }
}

/// 同一 Arc 的句柄克隆仍是同一个队列；新构造才发放唯一 issuer。
pub struct SharedForwardQueue<T: QueueItem>(Arc<Mutex<ForwardQueue<T>>>);
impl<T: QueueItem> Clone for SharedForwardQueue<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// 队内 Drop 取消，出队后 Drop 不改变任何在途责任。
/// ```compile_fail
/// use robot_rpc::forward_queue::{QueueItem,Ticket};
/// fn duplicate<T:QueueItem>(ticket:Ticket<T>) { let second=ticket.clone(); }
/// ```
pub struct Ticket<T: QueueItem> {
    queue: Weak<Mutex<ForwardQueue<T>>>,
    id: EntryId,
    enqueue_serial: NonZeroU64,
    armed: bool,
}
impl<T: QueueItem> std::fmt::Debug for Ticket<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ticket")
            .field("id", &self.id)
            .field("enqueue_serial", &self.enqueue_serial)
            .field("armed", &self.armed)
            .finish_non_exhaustive()
    }
}
impl<T: QueueItem> Drop for Ticket<T> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(queue) = self.queue.upgrade() {
            let removed = {
                queue
                    .lock()
                    .expect("转发队列锁未中毒")
                    .remove_record(self.id, self.enqueue_serial)
            };
            // removed 可能含会重入本队列的业务析构，必须已释放 guard。
            drop(removed);
        }
    }
}
#[derive(Debug)]
pub struct TicketMismatch<T: QueueItem>(pub Ticket<T>);
pub struct Popped<T> {
    pub item: T,
    pub permit: WirePermit,
    pub restore_lease: Option<RestoreLease>,
}

fn allocate_queue_id(counter: &AtomicU64) -> Result<QueueId, IdExhausted> {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .ok()
        .and_then(NonZeroU64::new)
        .map(QueueId)
        .ok_or(IdExhausted)
}
impl<T: QueueItem> SharedForwardQueue<T> {
    pub fn new(profile: QueueProfile) -> Result<Self, IdExhausted> {
        Ok(Self::from_id(profile, allocate_queue_id(&NEXT_QUEUE_ID)?))
    }
    fn from_id(profile: QueueProfile, id: QueueId) -> Self {
        Self(Arc::new(Mutex::new(ForwardQueue {
            id,
            profile,
            lower: VecDeque::new(),
            restore: VecDeque::new(),
            normal: VecDeque::new(),
            restore_active: 0,
            next_serial: NonZeroU64::MIN,
        })))
    }
    #[cfg(test)]
    fn with_counter(profile: QueueProfile, counter: &AtomicU64) -> Result<Self, IdExhausted> {
        Ok(Self::from_id(profile, allocate_queue_id(counter)?))
    }

    pub fn push(&self, item: T) -> Result<Ticket<T>, QueueFull> {
        let class = item.class();
        let id = item.entry_id();
        let mut pending = Some(item);
        let admitted = {
            let mut queue = self.0.lock().expect("转发队列锁未中毒");
            let capacity = match class {
                Class::Lower => queue.lower.len() + queue.restore.len() < queue.profile.high_cap,
                Class::Restore => {
                    queue.lower.len() + queue.restore.len() < queue.profile.high_cap
                        && queue.restore_active < queue.profile.restore_active_cap
                }
                Class::Normal => queue.normal.len() < queue.profile.normal_cap,
                Class::Emergency => false,
            };
            if capacity {
                match queue.next_serial.checked_add(1) {
                    Some(next) => {
                        let serial = queue.next_serial;
                        let restore_lease = match class {
                            Class::Restore => Some(RestoreLease {
                                issuer: queue.id,
                                serial,
                            }),
                            _ => None,
                        };
                        let record = Record {
                            item: pending.take().expect("载荷尚未移交"),
                            id,
                            serial,
                            restore_lease,
                        };
                        queue.next_serial = next;
                        match class {
                            Class::Lower => queue.lower.push_back(record),
                            Class::Restore => {
                                queue.restore_active += 1;
                                queue.restore.push_back(record);
                            }
                            Class::Normal => queue.normal.push_back(record),
                            Class::Emergency => unreachable!("Emergency 已由容量分支拒绝"),
                        }
                        Ok(serial)
                    }
                    None => Err(QueueFull { class }),
                }
            } else {
                Err(QueueFull { class })
            }
        };
        // Full / Emergency / serial 耗尽的原载荷也在解锁后析构。
        drop(pending);
        admitted.map(|enqueue_serial| Ticket {
            queue: Arc::downgrade(&self.0),
            id,
            enqueue_serial,
            armed: true,
        })
    }

    pub fn pop_eligible(&self, credits: &Credits) -> Option<Popped<T>> {
        let popped = {
            let mut queue = self.0.lock().expect("转发队列锁未中毒");
            let ForwardQueue {
                lower,
                restore,
                normal,
                ..
            } = &mut *queue;
            let mut found = None;
            for (class, lane) in [
                (Class::Lower, lower),
                (Class::Restore, restore),
                (Class::Normal, normal),
            ] {
                if !lane.is_empty()
                    && let Some(permit) = credits.try_acquire(class)
                {
                    found = Some((lane.pop_front().expect("已检查非空"), permit));
                    break;
                }
            }
            found
        };
        popped.map(|(record, permit)| Popped {
            item: record.item,
            permit,
            restore_lease: record.restore_lease,
        })
    }

    pub fn remove(&self, mut ticket: Ticket<T>) -> Result<Option<T>, TicketMismatch<T>> {
        if !Weak::ptr_eq(&ticket.queue, &Arc::downgrade(&self.0)) {
            return Err(TicketMismatch(ticket));
        }
        ticket.armed = false;
        let removed = {
            self.0
                .lock()
                .expect("转发队列锁未中毒")
                .remove_record(ticket.id, ticket.enqueue_serial)
        };
        Ok(removed.map(|record| record.item))
    }

    pub fn retire_restore(&self, lease: RestoreLease) -> Result<(), LeaseMismatch> {
        self.0
            .lock()
            .expect("转发队列锁未中毒")
            .retire_restore(lease)
    }
    #[must_use]
    pub fn len(&self, class: Class) -> usize {
        let queue = self.0.lock().expect("转发队列锁未中毒");
        match class {
            Class::Normal => queue.normal.len(),
            Class::Restore => queue.restore.len(),
            Class::Lower => queue.lower.len(),
            Class::Emergency => 0,
        }
    }
    #[must_use]
    pub fn restore_active(&self) -> usize {
        self.0.lock().expect("转发队列锁未中毒").restore_active
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::class_cache::EntryIdAllocator;
    use crate::wire_credit::{ClassShares, TopicKind};
    use robot_transport::iceoryx2::RrStaticLimits;
    use std::sync::atomic::AtomicUsize;

    struct Item {
        id: EntryId,
        class: Class,
        deadline: u64,
        probe: Option<DropProbe>,
    }
    struct DropProbe {
        queue: Weak<Mutex<ForwardQueue<Item>>>,
        count: Arc<AtomicUsize>,
    }
    impl std::fmt::Debug for Item {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Item")
                .field("id", &self.id)
                .field("class", &self.class)
                .finish_non_exhaustive()
        }
    }
    impl QueueItem for Item {
        fn entry_id(&self) -> EntryId {
            self.id
        }
        fn class(&self) -> Class {
            self.class
        }
        fn deadline_ns(&self) -> u64 {
            self.deadline
        }
    }
    impl Drop for Item {
        fn drop(&mut self) {
            if let Some(probe) = &self.probe {
                if let Some(queue) = probe.queue.upgrade() {
                    let queue = SharedForwardQueue(queue);
                    let _ = queue.len(self.class);
                }
                probe.count.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
    fn item(class: Class) -> Item {
        Item {
            id: EntryIdAllocator::global().next().unwrap(),
            class,
            deadline: 700,
            probe: None,
        }
    }
    fn queue() -> SharedForwardQueue<Item> {
        SharedForwardQueue::new(QueueProfile::S0_CONTROL).unwrap()
    }
    fn credits(kind: TopicKind) -> Credits {
        Credits::new(ClassShares::for_topic(kind, &RrStaticLimits { c: 8, l: 4 }).unwrap()).unwrap()
    }
    fn retire(queue: &SharedForwardQueue<Item>, credits: &Credits, popped: Popped<Item>) {
        if let Some(lease) = popped.restore_lease {
            queue.retire_restore(lease).unwrap();
        }
        credits.release(popped.permit).unwrap();
        drop(popped.item);
    }

    #[test]
    fn eight_restore_queued_then_ninth_is_queue_full() {
        let queue = queue();
        let tickets: Vec<_> = (0..8)
            .map(|_| queue.push(item(Class::Restore)).unwrap())
            .collect();
        assert_eq!(
            queue.push(item(Class::Restore)).unwrap_err(),
            QueueFull {
                class: Class::Restore
            }
        );
        assert_eq!((queue.len(Class::Restore), queue.restore_active()), (8, 8));
        drop(tickets);
    }
    #[test]
    fn pop_does_not_decrement_restore_active_retire_once_readmits() {
        let queue = SharedForwardQueue::new(QueueProfile {
            high_cap: 1,
            restore_active_cap: 1,
            normal_cap: 1,
        })
        .unwrap();
        let credits = credits(TopicKind::S0Control);
        let ticket = queue.push(item(Class::Restore)).unwrap();
        let popped = queue.pop_eligible(&credits).unwrap();
        assert_eq!((queue.len(Class::Restore), queue.restore_active()), (0, 1));
        assert!(queue.push(item(Class::Restore)).is_err());
        drop(ticket);
        assert_eq!(queue.restore_active(), 1);
        retire(&queue, &credits, popped);
        let next = queue.push(item(Class::Restore)).unwrap();
        assert_eq!(queue.restore_active(), 1);
        drop(next);
        assert_eq!(queue.restore_active(), 0);
    }
    #[test]
    fn queued_ticket_drop_removes_and_returns_restore_lease() {
        let queue = queue();
        let tickets: Vec<_> = (0..8)
            .map(|_| queue.push(item(Class::Restore)).unwrap())
            .collect();
        drop(tickets);
        assert_eq!((queue.len(Class::Restore), queue.restore_active()), (0, 0));
    }
    #[test]
    fn popped_ticket_drop_is_noop() {
        let queue = queue();
        let credits = credits(TopicKind::S0Control);
        let ticket = queue.push(item(Class::Restore)).unwrap();
        let popped = queue.pop_eligible(&credits).unwrap();
        drop(ticket);
        assert_eq!(queue.restore_active(), 1);
        assert_eq!(credits.used(Class::Restore), 1);
        retire(&queue, &credits, popped);
    }
    #[test]
    fn remove_then_drop_does_not_double_retire() {
        let queue = queue();
        let ticket = queue.push(item(Class::Restore)).unwrap();
        let returned = queue.remove(ticket).unwrap().unwrap();
        assert_eq!((queue.len(Class::Restore), queue.restore_active()), (0, 0));
        assert_eq!(returned.deadline_ns(), 700);
        drop(returned);
        assert_eq!(queue.restore_active(), 0);
    }
    #[test]
    fn ticket_drop_after_queue_dropped_is_noop() {
        let queue = queue();
        let ticket = queue.push(item(Class::Restore)).unwrap();
        drop(queue);
        assert!(ticket.queue.upgrade().is_none());
        drop(ticket);
    }
    #[test]
    fn push_returns_ticket_after_guard_release_no_deadlock() {
        let queue = queue();
        let ticket = queue.push(item(Class::Restore)).unwrap();
        assert!(queue.0.try_lock().is_ok());
        drop(ticket);
        assert_eq!(queue.restore_active(), 0);
    }
    #[test]
    fn pop_order_lower_restore_normal_gated_by_credits() {
        let queue = queue();
        let credits = credits(TopicKind::S0Control);
        let normal = queue.push(item(Class::Normal)).unwrap();
        let restore_ticket = queue.push(item(Class::Restore)).unwrap();
        let lower = queue.push(item(Class::Lower)).unwrap();
        let blocked = credits.try_acquire(Class::Lower).unwrap();
        let restore = queue.pop_eligible(&credits).unwrap();
        assert_eq!(restore.item.class(), Class::Restore);
        let ordinary = queue.pop_eligible(&credits).unwrap();
        assert_eq!(ordinary.item.class(), Class::Normal);
        assert!(queue.pop_eligible(&credits).is_none());
        credits.release(blocked).unwrap();
        let lower_work = queue.pop_eligible(&credits).unwrap();
        assert_eq!(lower_work.item.class(), Class::Lower);
        for popped in [restore, ordinary, lower_work] {
            retire(&queue, &credits, popped);
        }
        drop((normal, restore_ticket, lower));
        let first = queue.push(item(Class::Normal)).unwrap();
        let second = queue.push(item(Class::Normal)).unwrap();
        let first_id = first.id;
        let second_id = second.id;
        let a = queue.pop_eligible(&credits).unwrap();
        let b = queue.pop_eligible(&credits).unwrap();
        assert_eq!((a.item.id, b.item.id), (first_id, second_id));
        retire(&queue, &credits, a);
        retire(&queue, &credits, b);
        drop((first, second));
    }
    #[test]
    fn lease_from_other_queue_is_refused_and_returned() {
        let first = queue();
        let second = queue();
        assert_ne!(first.0.lock().unwrap().id, second.0.lock().unwrap().id);
        let shared = first.clone();
        let shared_id = shared.0.lock().unwrap().id;
        let first_id = first.0.lock().unwrap().id;
        assert_eq!(shared_id, first_id);
        let credits = credits(TopicKind::S0Control);
        let ticket = first.push(item(Class::Restore)).unwrap();
        let popped = first.pop_eligible(&credits).unwrap();
        let lease = popped.restore_lease.unwrap();
        let identity = (lease.issuer, lease.serial);
        let LeaseMismatch(returned) = second.retire_restore(lease).unwrap_err();
        assert_eq!((returned.issuer, returned.serial), identity);
        assert_eq!((first.restore_active(), second.restore_active()), (1, 0));
        first.retire_restore(returned).unwrap();
        credits.release(popped.permit).unwrap();
        drop((popped.item, ticket));
    }
    #[test]
    fn s1_profile_normal_64_then_full() {
        let queue = SharedForwardQueue::new(QueueProfile::S1_TASK).unwrap();
        let tickets: Vec<_> = (0..64)
            .map(|_| queue.push(item(Class::Normal)).unwrap())
            .collect();
        assert!(queue.push(item(Class::Normal)).is_err());
        for class in [Class::Lower, Class::Restore, Class::Emergency] {
            assert_eq!(queue.push(item(class)).unwrap_err(), QueueFull { class });
        }
        assert_eq!(queue.len(Class::Normal), 64);
        assert_eq!(queue.restore_active(), 0);
        drop(tickets);
    }
    #[test]
    fn wrong_queue_returns_armed_ticket_even_when_entry_id_matches() {
        let first = queue();
        let second = queue();
        let a = item(Class::Restore);
        let mut b = item(Class::Restore);
        b.id = a.id;
        let ticket = first.push(a).unwrap();
        let other = second.push(b).unwrap();
        let serial = ticket.enqueue_serial;
        let TicketMismatch(returned) = second.remove(ticket).unwrap_err();
        assert!(returned.armed);
        assert_eq!(returned.enqueue_serial, serial);
        assert_eq!(
            (first.len(Class::Restore), second.len(Class::Restore)),
            (1, 1)
        );
        assert_eq!((first.restore_active(), second.restore_active()), (1, 1));
        drop(returned);
        assert_eq!(
            (first.len(Class::Restore), second.len(Class::Restore)),
            (0, 1)
        );
        drop(other);
    }
    #[test]
    fn old_ticket_cannot_cancel_reenqueued_same_entry_id() {
        let queue = queue();
        let credits = credits(TopicKind::S0Control);
        let a = item(Class::Lower);
        let mut b = item(Class::Lower);
        b.id = a.id;
        let old = queue.push(a).unwrap();
        let popped = queue.pop_eligible(&credits).unwrap();
        retire(&queue, &credits, popped);
        let new = queue.push(b).unwrap();
        assert_ne!(old.enqueue_serial, new.enqueue_serial);
        drop(old);
        assert_eq!(queue.len(Class::Lower), 1);
        drop(new);
        assert_eq!(queue.len(Class::Lower), 0);
    }
    #[test]
    fn queue_id_exhaustion_does_not_construct_or_wrap() {
        let counter = AtomicU64::new(u64::MAX - 1);
        let queue =
            SharedForwardQueue::<Item>::with_counter(QueueProfile::S0_CONTROL, &counter).unwrap();
        assert_eq!(queue.0.lock().unwrap().id.0.get(), u64::MAX - 1);
        assert!(matches!(
            SharedForwardQueue::<Item>::with_counter(QueueProfile::S0_CONTROL, &counter),
            Err(IdExhausted)
        ));
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }
    #[test]
    fn enqueue_serial_exhaustion_keeps_state_and_retirement_does_not_reset() {
        let queue = queue();
        let credits = credits(TopicKind::S0Control);
        queue.0.lock().unwrap().next_serial = NonZeroU64::new(u64::MAX - 1).unwrap();
        let ticket = queue.push(item(Class::Restore)).unwrap();
        assert_eq!(ticket.enqueue_serial.get(), u64::MAX - 1);
        assert!(queue.push(item(Class::Normal)).is_err());
        assert_eq!(queue.len(Class::Normal), 0);
        let popped = queue.pop_eligible(&credits).unwrap();
        assert_eq!(
            popped.restore_lease.as_ref().unwrap().serial,
            ticket.enqueue_serial
        );
        retire(&queue, &credits, popped);
        drop(ticket);
        assert_eq!(queue.restore_active(), 0);
        assert!(queue.push(item(Class::Restore)).is_err());
        assert_eq!(queue.0.lock().unwrap().next_serial.get(), u64::MAX);
    }
    #[test]
    fn capacity_rejection_does_not_consume_enqueue_serial() {
        let queue = SharedForwardQueue::new(QueueProfile {
            high_cap: 1,
            restore_active_cap: 1,
            normal_cap: 1,
        })
        .unwrap();
        let first = queue.push(item(Class::Restore)).unwrap();
        let normal = queue.push(item(Class::Normal)).unwrap();
        let next = queue.0.lock().unwrap().next_serial;
        for class in [
            Class::Normal,
            Class::Restore,
            Class::Lower,
            Class::Emergency,
        ] {
            assert!(queue.push(item(class)).is_err());
            assert_eq!(queue.0.lock().unwrap().next_serial, next);
        }
        drop((first, normal));
    }
}

#[cfg(test)]
mod reentrant_drop_tests {
    use super::*;
    use crate::class_cache::EntryIdAllocator;
    use robot_core::time::{RobotClock, SystemClock};
    use std::io::Write;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    const CHILD_ENV: &str = "ROBOT_FORWARD_QUEUE_REENTRANT_DROP_CASE";
    const CASES: [&str; 5] = ["cancel", "remove", "full", "emergency", "serial"];
    struct ReentrantItem {
        id: EntryId,
        class: Class,
        queue: Weak<Mutex<ForwardQueue<Self>>>,
        drops: Arc<AtomicUsize>,
        reentered: Arc<AtomicUsize>,
    }
    impl QueueItem for ReentrantItem {
        fn entry_id(&self) -> EntryId {
            self.id
        }
        fn class(&self) -> Class {
            self.class
        }
        fn deadline_ns(&self) -> u64 {
            700
        }
    }
    impl Drop for ReentrantItem {
        fn drop(&mut self) {
            if let Some(queue) = self.queue.upgrade() {
                let _ = SharedForwardQueue(queue).len(self.class);
                self.reentered.fetch_add(1, Ordering::SeqCst);
            }
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct SupervisedChild(Option<Child>);
    impl SupervisedChild {
        fn wait_until(&mut self, deadline: u64) -> Option<ExitStatus> {
            loop {
                if let Some(status) = self.0.as_mut().unwrap().try_wait().expect("检查监督子进程")
                {
                    self.0.take();
                    return Some(status);
                }
                if SystemClock.now().0 >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        fn kill_and_reap(&mut self) -> ExitStatus {
            let _ = self.0.as_mut().unwrap().kill();
            self.wait_until(SystemClock.now().0 + 5_000_000_000)
                .expect("kill 后有界回收监督子进程")
        }
    }
    impl Drop for SupervisedChild {
        fn drop(&mut self) {
            if self.0.is_some() {
                let _ = self.kill_and_reap();
            }
        }
    }

    fn run_case(case: &str) {
        let profile = QueueProfile {
            high_cap: 1,
            restore_active_cap: 1,
            normal_cap: 1,
        };
        let queue = SharedForwardQueue::<ReentrantItem>::new(profile).unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let reentered = Arc::new(AtomicUsize::new(0));
        let make = |class| ReentrantItem {
            id: EntryIdAllocator::global().next().unwrap(),
            class,
            queue: Arc::downgrade(&queue.0),
            drops: Arc::clone(&drops),
            reentered: Arc::clone(&reentered),
        };
        match case {
            "cancel" => {
                let ticket = queue.push(make(Class::Restore)).unwrap();
                assert_eq!(queue.restore_active(), 1);
                drop(ticket);
                assert_eq!(queue.restore_active(), 0);
            }
            "remove" => {
                let ticket = queue.push(make(Class::Restore)).unwrap();
                let returned = queue
                    .remove(ticket)
                    .unwrap_or_else(|_| panic!("同队列必须接受票据"))
                    .unwrap();
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                assert_eq!(queue.restore_active(), 0);
                drop(returned);
            }
            "full" => {
                let ticket = queue.push(make(Class::Normal)).unwrap();
                assert!(queue.push(make(Class::Normal)).is_err());
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                assert_eq!(queue.len(Class::Normal), 1);
                drop(ticket);
            }
            "emergency" => {
                assert!(queue.push(make(Class::Emergency)).is_err());
            }
            "serial" => {
                queue.0.lock().unwrap().next_serial = NonZeroU64::new(u64::MAX).unwrap();
                assert!(queue.push(make(Class::Normal)).is_err());
                assert_eq!(queue.0.lock().unwrap().next_serial.get(), u64::MAX);
            }
            _ => panic!("未知子进程场景"),
        }
        let expected = if case == "full" { 2 } else { 1 };
        assert_eq!(drops.load(Ordering::SeqCst), expected);
        assert_eq!(reentered.load(Ordering::SeqCst), expected);
        assert_eq!(
            queue.len(Class::Normal) + queue.len(Class::Restore) + queue.len(Class::Lower),
            0
        );
        assert_eq!(queue.restore_active(), 0);
        writeln!(
            std::io::stdout().lock(),
            "reentrant_drop_completed case={case} drops={expected} reentered={expected}"
        )
        .unwrap();
    }

    #[test]
    fn payload_drop_reenters_len_after_cancel_remove_full_emergency_and_serial_exhaustion() {
        if let Ok(case) = std::env::var(CHILD_ENV) {
            run_case(&case);
            return;
        }
        for case in CASES {
            let output = tempfile::NamedTempFile::new().unwrap();
            let child_output = output.reopen().unwrap();
            let child_error = child_output.try_clone().unwrap();
            let child=Command::new(std::env::current_exe().unwrap()).args(["--exact","forward_queue::reentrant_drop_tests::payload_drop_reenters_len_after_cancel_remove_full_emergency_and_serial_exhaustion","--nocapture","--test-threads=1"])
                .env(CHILD_ENV,case).stdin(Stdio::null()).stdout(child_output).stderr(child_error).spawn().expect("启动真实重入析构子进程");
            let mut child = SupervisedChild(Some(child));
            let Some(status) = child.wait_until(SystemClock.now().0 + 20_000_000_000) else {
                let status = child.kill_and_reap();
                panic!("{case} 子进程超时，已真实回收：{status}");
            };
            let raw = std::fs::read_to_string(output.path()).unwrap();
            assert!(status.success(), "{case} status={status} raw={raw}");
            let expected = if case == "full" { 2 } else { 1 };
            let marker = format!(
                "reentrant_drop_completed case={case} drops={expected} reentered={expected}"
            );
            assert_eq!(raw.matches(&marker).count(), 1, "{raw}");
            writeln!(std::io::stdout().lock(),"reentrant_drop_verified case={case} exit=0 drops={expected} reentered={expected} child_reaped=true").unwrap();
        }
    }
}
