//! 最新命令槽（`ipc_thread` 写 → `rt_thread` 读）。基于 `arc-swap`。
//!
//! 对应 `docs/00_design.md` §1.2 latest-command slot 的实现。与
//! [`super::snapshot::RtSnapshotPublisher`] 镜像对称：两者方向相反，所以用不同
//! 原语：
//!
//! | 方向 | 原语 | 位置 |
//! |---|---|---|
//! | `rt_thread` 写 → `pub_thread` 读 | 手写 seqlock | `snapshot.rs` |
//! | `ipc_thread` 写 → `rt_thread` 读 | arc-swap | 本文件 |
//!
//! ### arc-swap 在 RT 侧安全的理由
//!
//! `ArcSwap<T>::load()` 返回 `Guard<Arc<T>>`，底层用 hazard pointer，**不 bump
//! refcount**。RT 线程在 scope 内调用 `load()` → deref 读数据 → Guard drop，全程
//! 不触发 `Arc::clone` / `Arc::drop`（§4.9 RT 铁律豁免）。
//!
//! writer 侧 `store(T)` 内部 `Arc::new(T)`，会分配堆 —— 但 `ipc_thread` 是非 RT 线程，
//! 可以分配。
//!
//! ### 单写多读
//!
//! - `LatestCommandWriter`：**`!Clone`**（单一 `ipc_thread` 写）
//! - `LatestCommandReader`：`Clone`（如果 `rt_thread` + watchdog 都想读）
//!
//! Part 2 watchdog 可能要判断 "latest command 过期"，需要自己的 reader handle。

use arc_swap::{ArcSwap, Guard};
use std::sync::Arc;

/// 构造器 —— 一次返回 writer + reader。
///
/// 语义和 `RtSnapshotPublisher::new` 对称，但底层是 arc-swap。
pub struct LatestCommandSlot<T: Send + Sync + 'static>(std::marker::PhantomData<T>);

impl<T: Send + Sync + 'static> LatestCommandSlot<T> {
    /// 创建带初始值的一对 writer / reader。
    ///
    /// 这里 `new` 不返回 `Self`（`LatestCommandSlot<T>` 本身只是命名空间，没实例）；
    /// 命名优先对齐使用侧习惯 `LatestCommandSlot::new(init)`。
    #[allow(clippy::new_ret_no_self)]
    #[must_use]
    pub fn new(initial: T) -> (LatestCommandWriter<T>, LatestCommandReader<T>) {
        let inner = Arc::new(ArcSwap::from_pointee(initial));
        (
            LatestCommandWriter {
                inner: Arc::clone(&inner),
            },
            LatestCommandReader { inner },
        )
    }
}

/// 写端（`ipc_thread` 持有，**不可 Clone**）。
pub struct LatestCommandWriter<T: Send + Sync + 'static> {
    inner: Arc<ArcSwap<T>>,
}

/// 读端（`rt_thread` + 其他非 RT 观察者持有，可 Clone）。
pub struct LatestCommandReader<T: Send + Sync + 'static> {
    inner: Arc<ArcSwap<T>>,
}

impl<T: Send + Sync + 'static> LatestCommandWriter<T> {
    /// 存入新值 —— `ipc_thread` 非 RT 路径调用，允许 `Arc::new` 分配堆。
    #[inline]
    pub fn store(&self, value: T) {
        self.inner.store(Arc::new(value));
    }

    /// 从 Writer 派生一个额外 reader（启动期一次性）。
    #[must_use]
    pub fn reader(&self) -> LatestCommandReader<T> {
        LatestCommandReader {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: Send + Sync + 'static> LatestCommandReader<T> {
    /// RT 侧调用：`Guard<Arc<T>>` 基于 hazard pointer，**不 bump refcount**。
    ///
    /// 用法：
    /// ```ignore
    /// let guard = reader.load();
    /// let cmd: &T = &*guard;   // Deref，不触发 Arc ops
    /// // guard drop：也不触发 Arc ops（hazard pointer 释放）
    /// ```
    #[inline]
    #[must_use]
    pub fn load(&self) -> Guard<Arc<T>> {
        self.inner.load()
    }
}

impl<T: Send + Sync + 'static> Clone for LatestCommandReader<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_then_load_returns_latest() {
        let (w, r) = LatestCommandSlot::new(0u64);
        assert_eq!(**r.load(), 0);
        w.store(42);
        assert_eq!(**r.load(), 42);
        w.store(100);
        assert_eq!(**r.load(), 100);
    }

    #[test]
    fn reader_clone_sees_same_value() {
        let (w, r1) = LatestCommandSlot::new(0u64);
        let r2 = r1.clone();
        w.store(777);
        assert_eq!(**r1.load(), 777);
        assert_eq!(**r2.load(), 777);
    }

    #[test]
    fn writer_reader_helper_parallels_new() {
        let (w, _r1) = LatestCommandSlot::new(0u64);
        let r_extra = w.reader();
        w.store(999);
        assert_eq!(**r_extra.load(), 999);
    }

    /// 回归守卫：arc-swap Guard 的 Deref 不 bump refcount，RT scope 里用是安全的。
    /// 如果未来 arc-swap 升级改了语义，这个测试会捕到 —— 到时候要重新评估。
    #[test]
    fn load_guard_does_not_bump_strong_count() {
        let (w, r) = LatestCommandSlot::new(5u64);
        // 初始：Arc 由 w 和 r 各持一份（Arc<ArcSwap<T>>），strong_count = 2
        let initial = Arc::strong_count(&r.inner);
        {
            let _g = r.load();
            let during = Arc::strong_count(&r.inner);
            // load() 返回 Guard<Arc<T>>，外层 Arc<ArcSwap<T>> 的引用计数不动
            assert_eq!(
                during, initial,
                "arc-swap Guard unexpectedly bumped Arc count"
            );
        }
        let after = Arc::strong_count(&r.inner);
        assert_eq!(after, initial);
        // 再写一次也不影响外层 Arc<ArcSwap<T>> 的 count
        w.store(10);
        assert_eq!(Arc::strong_count(&r.inner), initial);
    }

    #[test]
    fn sync_and_send_reader() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LatestCommandReader<u64>>();
        assert_send_sync::<LatestCommandWriter<u64>>();
    }
}
