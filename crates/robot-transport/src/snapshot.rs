//! 单写多读 seqlock —— **RT 侧 publish，非 RT 侧 load**。
//!
//! 对应 `docs/00_design.md` §4.9 snapshot 同步规范的 **`rt_thread` → `pub_thread`**
//! 方向；Step 6 Part 1 首次落地。
//!
//! ### 为什么用 seqlock 而非 `arc-swap`
//!
//! arc-swap 的 `store()` 内部 `Arc::new(T)` 要分配堆，破 RT 铁律（§4.9）。本模块
//! 用手写 seqlock 把"写 snapshot"做成 **2×atomic add + 1×memcpy**（A78AE 上测
//! `sizeof::<RtSnapshot>() ≈ 400 B` 约 60-80 ns 稳态），零 alloc、零锁。
//!
//! 相反方向（非 RT 写 / RT 读）用 `super::command_slot::LatestCommandSlot`
//! —— arc-swap Guard 在 RT 侧 `load()` 不 bump refcount，方向是天然契合的。
//!
//! ### 约束
//!
//! - `T: Copy` 强制 —— seqlock 靠 memcpy 搬运，不跑析构。编译期阻止 `Vec` /
//!   `String` / `Box<T>` 等带 Drop 的字段进入载荷。
//! - **单写**：`RtSnapshotPublisher` 是 `!Clone`（结构体不 derive `Clone` 也不
//!   手写 `Clone`），确保同一 channel 同时只有一个 writer 持有 `publish` 能力。
//! - 多读：`RtSnapshotReader` 可 `Clone`（`pub_thread` 可能多消费者，如 recorder 侧录）。
//!
//! ### reader 行为
//!
//! - 稳态（writer 不在写）：1 次 atomic load + 1 次 memcpy + 1 次 atomic load
//!   ≈ 60 ns（无 retry）。
//! - writer 夹在中间（version 奇）：`spin_loop()` 等到偶 version 再读；最坏情况
//!   多一次 tick 的等待（`rt_thread` tick 1 ms）。Part 1 接受；Part 2 `pub_thread`
//!   要报 "snapshot retry count" 作为观测信号。

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// 单写多读 seqlock 核心。内部使用，不对外导出。
///
/// ### 算法
///
/// - `version` 偶数 = 稳态；奇数 = writer 正在写。
/// - writer：`version += 1`（奇）→ memcpy 新值 → `version += 1`（偶）。
/// - reader：loop：读 `v1`（偶？）→ memcpy 到栈 → 读 `v2`，`v1 == v2` 则一致。
struct Seqlock<T: Copy> {
    version: AtomicU64,
    data: UnsafeCell<T>,
}

// SAFETY: 单写约束由 `RtSnapshotPublisher` `!Clone` 在类型层保证；reader 只读
//        经 version fence 验证一致性；T: Copy + Send 保证值语义可跨线程 memcpy。
unsafe impl<T: Copy + Send> Sync for Seqlock<T> {}

/// 写端（RT 线程持有，**不可 Clone**）。
///
/// `publish(&T)` 只做 2 次原子加 + 1 次 memcpy，不分配。
pub struct RtSnapshotPublisher<T: Copy + Send + 'static> {
    inner: Arc<Seqlock<T>>,
}

/// 读端（非 RT 线程持有，**可 Clone**）。
pub struct RtSnapshotReader<T: Copy + Send + 'static> {
    inner: Arc<Seqlock<T>>,
}

impl<T: Copy + Send + 'static> RtSnapshotPublisher<T> {
    /// 创建一对 publisher / reader，初始值为 `initial`。
    ///
    /// 内部 `Arc::new` 在 caller（非 RT）执行；Publisher / Reader 持 `Arc` 后
    /// RT 侧不再触发 Arc ops。
    #[must_use]
    pub fn new(initial: T) -> (Self, RtSnapshotReader<T>) {
        let inner = Arc::new(Seqlock {
            version: AtomicU64::new(0),
            data: UnsafeCell::new(initial),
        });
        (
            Self {
                inner: Arc::clone(&inner),
            },
            RtSnapshotReader { inner },
        )
    }

    /// 派生一个新的 reader（在 non-RT 场景调用，可 `Arc::clone`）。
    #[must_use]
    pub fn reader(&self) -> RtSnapshotReader<T> {
        RtSnapshotReader {
            inner: Arc::clone(&self.inner),
        }
    }

    /// 发布新值。
    ///
    /// RT 侧调用：2 × atomic add + 1 × memcpy(`sizeof::<T>()`)，不分配、不阻塞。
    #[inline]
    pub fn publish(&self, value: &T) {
        let v = self.inner.version.fetch_add(1, Ordering::AcqRel);
        debug_assert!(
            v & 1 == 0,
            "RtSnapshotPublisher: concurrent writer detected (version became odd)"
        );
        // SAFETY: writer 单写约束保证 `data` 当前无其他写；reader 以 version fence
        //        保证读到的是稳态值。`T: Copy` 允许直接位模式覆盖而无需 Drop。
        unsafe {
            std::ptr::write_volatile(self.inner.data.get(), *value);
        }
        self.inner.version.fetch_add(1, Ordering::Release);
    }
}

impl<T: Copy + Send + 'static> RtSnapshotReader<T> {
    /// 读取当前值。
    ///
    /// 稳态路径：2 × atomic load + 1 × memcpy；writer 夹在中间时 `spin_loop` 重试。
    #[inline]
    #[must_use]
    pub fn load(&self) -> T {
        loop {
            let v1 = self.inner.version.load(Ordering::Acquire);
            if v1 & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            // SAFETY: version 偶 → writer 不在写；memcpy 读一份；再验 v2 == v1 保证
            //        这次读区间内 writer 没进来。`T: Copy` 允许 `read_volatile`。
            let value = unsafe { std::ptr::read_volatile(self.inner.data.get()) };
            let v2 = self.inner.version.load(Ordering::Acquire);
            if v1 == v2 {
                return value;
            }
            std::hint::spin_loop();
        }
    }

    /// 返回当前 version（偶数 = 稳态；奇数 = writer 正在写）。
    ///
    /// 用于诊断 / 计数"snapshot 未变化"场景（两次 `load` 之间 version 未变 = 未更新）。
    #[inline]
    #[must_use]
    pub fn version(&self) -> u64 {
        self.inner.version.load(Ordering::Acquire)
    }
}

impl<T: Copy + Send + 'static> Clone for RtSnapshotReader<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::thread;

    #[test]
    fn publish_load_roundtrip_single_thread() {
        let (pubr, rdr) = RtSnapshotPublisher::new(0u64);
        assert_eq!(rdr.load(), 0);
        pubr.publish(&42);
        assert_eq!(rdr.load(), 42);
        pubr.publish(&100);
        assert_eq!(rdr.load(), 100);
    }

    #[test]
    fn version_advances_two_per_publish() {
        let (pubr, rdr) = RtSnapshotPublisher::new(0u32);
        assert_eq!(rdr.version(), 0);
        pubr.publish(&1);
        assert_eq!(rdr.version(), 2);
        pubr.publish(&2);
        assert_eq!(rdr.version(), 4);
    }

    #[test]
    fn reader_clone_sees_same_data() {
        let (pubr, rdr1) = RtSnapshotPublisher::new(0u64);
        let rdr2 = rdr1.clone();
        pubr.publish(&777);
        assert_eq!(rdr1.load(), 777);
        assert_eq!(rdr2.load(), 777);
    }

    #[test]
    fn reader_from_publisher_builder() {
        let (pubr, _rdr1) = RtSnapshotPublisher::new(0u64);
        let rdr_extra = pubr.reader();
        pubr.publish(&555);
        assert_eq!(rdr_extra.load(), 555);
    }

    /// 压测：writer 高频 publish + 多个 reader 并发 load 30 万次，验证所有读到的值
    /// 都是 publisher 发过的；seqlock 不应产生"一半新一半旧"的撕裂值。
    #[test]
    fn stress_concurrent_readers_see_consistent_values() {
        #[derive(Clone, Copy)]
        struct Payload {
            a: u64,
            b: u64,
        }
        let (pubr, rdr) = RtSnapshotPublisher::new(Payload { a: 0, b: 0 });
        let stop = Arc::new(AtomicBool::new(false));

        let pub_handle = {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut i: u64 = 1;
                while !stop.load(Ordering::Relaxed) {
                    // 不变量：a == b（writer 每次同时改两个字段）。
                    pubr.publish(&Payload { a: i, b: i });
                    i = i.wrapping_add(1);
                }
            })
        };

        let mut rdr_handles = Vec::new();
        for _ in 0..3 {
            let rdr = rdr.clone();
            rdr_handles.push(thread::spawn(move || {
                let mut observed = 0u64;
                for _ in 0..100_000 {
                    let p = rdr.load();
                    // 如果 seqlock 撕裂，a/b 会不等 → fail
                    assert_eq!(p.a, p.b, "seqlock tear detected: {}, {}", p.a, p.b);
                    observed = observed.max(p.a);
                }
                observed
            }));
        }

        let max_observed: u64 = rdr_handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .max()
            .unwrap();
        stop.store(true, Ordering::Relaxed);
        pub_handle.join().unwrap();
        // sanity：至少看到过一次 publish 之后的值
        assert!(max_observed >= 1, "reader never observed any publish");
    }
}
