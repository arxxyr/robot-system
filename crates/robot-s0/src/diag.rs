//! `SerialRT` 固定诊断环：有效容量 256，满时丢弃最新事件，不等待消费者。
//!
//! 正常路径为单生产者、单消费者。公开 safe API 仍允许被误并发调用，故每侧
//! 用一次 CAS 准入排除同侧竞争；失败立即返回，绝不靠调用约定维持内存安全。

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use robot_core::diag::{DiagEvent, DiagSink};

const CAPACITY: usize = 256;

/// 固定容量的非等待诊断通道；生产者竞争也按 drop-newest 计入 dropped。
pub struct SerialDiagRing {
    slots: [UnsafeCell<MaybeUninit<DiagEvent>>; CAPACITY],
    head: AtomicUsize,
    tail: AtomicUsize,
    dropped: AtomicU64,
    producer_active: AtomicBool,
    consumer_active: AtomicBool,
}

// SAFETY: producer_active 保证最多一个写者，consumer_active 保证最多一个读者。
// 写者仅写 head 指向的空槽，读者仅读 tail 指向的已发布槽。head Release/Acquire
// 发布完整内容，tail Release/Acquire 保证读完才可复用；两侧绝不同时访问同一槽。
unsafe impl Sync for SerialDiagRing {}

/// 准入卫兵保证即使未来控制流新增提前退出，也会归还本侧访问权。
struct Admission<'a>(&'a AtomicBool);

impl<'a> Admission<'a> {
    fn try_enter(flag: &'a AtomicBool) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Self(flag))
    }
}

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl SerialDiagRing {
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| UnsafeCell::new(MaybeUninit::uninit())),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            producer_active: AtomicBool::new(false),
            consumer_active: AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn pop(&self) -> Option<DiagEvent> {
        let _admission = Admission::try_enter(&self.consumer_active)?;
        let tail = self.tail.load(Ordering::Relaxed);
        if tail == self.head.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: Acquire 已观察初始化写，准入排除同侧读者；tail 发布前槽不复用。
        let event = unsafe { (*self.slots[tail % CAPACITY].get()).assume_init_read() };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(event)
    }

    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn with_empty_counter(counter: usize) -> Self {
        let ring = Self::new();
        ring.head.store(counter, Ordering::Relaxed);
        ring.tail.store(counter, Ordering::Relaxed);
        ring
    }
}

impl Default for SerialDiagRing {
    fn default() -> Self {
        Self::new()
    }
}

impl DiagSink for SerialDiagRing {
    fn record(&self, event: DiagEvent) {
        let Some(_admission) = Admission::try_enter(&self.producer_active) else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let head = self.head.load(Ordering::Relaxed);
        if head.wrapping_sub(self.tail.load(Ordering::Acquire)) >= CAPACITY {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // SAFETY: 本侧访问权唯一，Acquire 证明槽已空闲；head 发布前消费者不可读。
        unsafe {
            (*self.slots[head % CAPACITY].get()).write(event);
        }
        self.head.store(head.wrapping_add(1), Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn event(seq: u32) -> DiagEvent {
        DiagEvent {
            stamp_ns: u64::from(seq),
            site: 1,
            kind: 2,
            a: 3,
            b: 4,
            seq_lo: seq,
        }
    }

    #[test]
    fn capacity_is_256_and_wrapping_keeps_unread_events() {
        let ring = SerialDiagRing::with_empty_counter(usize::MAX - 127);
        for seq in 0..256 {
            ring.record(event(seq));
        }
        ring.record(event(999));
        assert_eq!(ring.dropped(), 1);
        for seq in 0..256 {
            assert_eq!(ring.pop(), Some(event(seq)));
        }
        assert_eq!(ring.pop(), None);
        ring.record(event(256));
        assert_eq!(ring.pop(), Some(event(256)));
    }

    #[test]
    fn concurrent_safe_callers_do_not_duplicate_or_tear_events() {
        let ring = Arc::new(SerialDiagRing::new());
        let mut handles = Vec::new();
        for producer in 0..4 {
            let ring = Arc::clone(&ring);
            handles.push(std::thread::spawn(move || {
                for seq in 0..4_000 {
                    ring.record(event(producer * 4_000 + seq));
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let mut readers = Vec::new();
        for _ in 0..4 {
            let ring = Arc::clone(&ring);
            readers.push(std::thread::spawn(move || {
                let mut seen = Vec::new();
                while let Some(got) = ring.pop() {
                    assert_eq!(got, event(got.seq_lo));
                    seen.push(got.seq_lo);
                }
                seen
            }));
        }
        let mut seen: Vec<_> = readers
            .into_iter()
            .flat_map(|reader| reader.join().unwrap())
            .collect();
        seen.sort_unstable();
        assert!(seen.windows(2).all(|pair| pair[0] != pair[1]));
        assert_eq!(seen.len() as u64 + ring.dropped(), 16_000);
    }
}
