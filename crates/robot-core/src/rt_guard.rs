//! RT 线程入口的双层守卫：`RtArc<T>` 禁止 `Arc::clone/drop`，`rt_entry!` 宏
//! 可选叠加 `assert_no_alloc` 堆分配检测。
//!
//! 对应 `docs/00_design.md` §4.9 RT 铁律 + §10.2.3 assert-no-alloc CI gate。
//!
//! ### 两层含义
//!
//! 1. **`RtArcGuard` / `RtArc<T>`**：**始终活跃**（无 feature gate）。进入 RT scope
//!    后任何 `RtArc::clone` 或 `Arc::drop`（`strong_count` == 1 的最后一次）触发
//!    thread-local flag 检查并 `panic!`。开销 = 一次 thread-local 读，纳秒级。
//!
//! 2. **`rt_entry!` 宏 + `assert_no_alloc`**：**feature gate `rt_guard`**。默认 off
//!    避免依赖方被强行替换 global allocator。打开后：
//!    - BINARY crate 需自己声明 `#[global_allocator] static _: AllocDisabler = ...`
//!    - `rt_entry!` 用 `assert_no_alloc(|| body)` 包裹 RT 主循环；scope 内任何
//!      alloc 触发进程 abort（不能通过 `catch_unwind` 恢复）
//!
//! ### 典型用法
//!
//! ```rust,ignore
//! // binary crate:
//! # #[cfg(feature = "rt_guard")]
//! # mod demo {
//! #[global_allocator]
//! static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
//! # }
//!
//! // rt_thread 主循环：
//! use robot_core::rt_entry;
//! loop {
//!     rt_entry!({
//!         // 这里 clone RtArc 会 panic；alloc 会 abort（rt_guard on 且 binary 设了 AllocDisabler 时）
//!     });
//! }
//! ```
//!
//! ### 为什么不完全依赖 `assert_no_alloc`
//!
//! `assert_no_alloc` 只抓堆分配，抓不到 `Arc::clone` 导致的 **原子操作 + hidden retired
//! list push**（arc-swap 的 Guard Deref 不 bump refcount，但用户代码里手写的
//! `Arc::clone` 会）。`RtArc` 作为类型安全补充：把 Arc clone 在 RT scope 变成
//! 编译 / 运行时可观测事件。

use std::cell::Cell;
use std::sync::Arc;

thread_local! {
    /// 当前线程是否在 RT scope 内。`RtArcGuard::enter` 设 true，drop 恢复 false。
    static RT_IN_SCOPE: Cell<bool> = const { Cell::new(false) };
}

/// `Drop`-based guard：构造时标记当前线程进入 RT scope，析构时恢复。
///
/// 不可嵌套（重入）——同线程连续两次 `enter` 会 panic，避免 scope 范围被错误加长。
#[must_use = "guard must stay alive for the duration of the RT scope"]
pub struct RtArcGuard {
    _not_send: std::marker::PhantomData<*const ()>,
}

impl RtArcGuard {
    /// 进入 RT scope。调用两次不 drop 中间那次会 panic（禁止嵌套）。
    ///
    /// # Panics
    ///
    /// 当同一线程的前一个 `RtArcGuard` 尚未 drop 时。
    pub fn enter() -> Self {
        RT_IN_SCOPE.with(|f| {
            assert!(
                !f.get(),
                "RtArcGuard::enter called while already in RT scope"
            );
            f.set(true);
        });
        Self {
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Drop for RtArcGuard {
    fn drop(&mut self) {
        RT_IN_SCOPE.with(|f| f.set(false));
    }
}

/// 当前线程是否处于 RT scope。供 `RtArc` / 其他 guard 类型查询。
#[inline]
#[must_use]
pub fn in_rt_scope() -> bool {
    RT_IN_SCOPE.with(Cell::get)
}

/// `Arc<T>` 的 wrapper，禁止在 RT scope 内 `clone()` 或触发最后一次析构。
///
/// RT scope 外行为等同于 `Arc<T>`（clone = refcount 原子加）；RT scope 内任何
/// 涉及堆 / 原子 refcount 的操作都 `panic`，提前暴露违规。
///
/// `Deref` 到 `&T`，只读借用不触发 refcount，因此 RT 闭环读数据完全合法：
///
/// ```rust,ignore
/// # use std::sync::Arc;
/// # use robot_core::rt_guard::{RtArc, RtArcGuard};
/// let limits: RtArc<Vec<f32>> = RtArc::new(vec![1.0, 2.0]);
/// let _g = RtArcGuard::enter();
/// let slice = &limits[..];   // Deref OK
/// // let _ = limits.clone();  // 会 panic
/// drop(_g);
/// ```
pub struct RtArc<T: ?Sized>(Arc<T>);

impl<T> RtArc<T> {
    #[must_use]
    pub fn new(inner: T) -> Self {
        Self(Arc::new(inner))
    }
}

impl<T: ?Sized> RtArc<T> {
    /// 从现成的 `Arc<T>` 包装；不分配。
    #[must_use]
    pub fn from_arc(a: Arc<T>) -> Self {
        Self(a)
    }

    /// 主动解包回 `Arc<T>`，返还所有权。在 RT scope 内调用也 OK（只搬指针）。
    #[must_use]
    pub fn into_arc(self) -> Arc<T> {
        // `RtArc` 有 Drop impl（会检查 RT scope），不能直接 move out `self.0`；
        // 用 `ManuallyDrop` 跳过析构，显式把内层 Arc 搬出去（所有权无变化）。
        let this = std::mem::ManuallyDrop::new(self);
        // SAFETY: `self` 被 wrap 进 ManuallyDrop 后其 Drop 不会跑；读一次 `self.0`
        //        把 Arc 的所有权转出，源副本不再被访问，等价 move。
        unsafe { std::ptr::read(&raw const this.0) }
    }

    /// 当前 strong count（调试用）。
    #[must_use]
    pub fn strong_count(this: &Self) -> usize {
        Arc::strong_count(&this.0)
    }
}

impl<T: ?Sized> std::ops::Deref for RtArc<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: ?Sized> Clone for RtArc<T> {
    fn clone(&self) -> Self {
        assert!(
            !in_rt_scope(),
            "RtArc::clone called inside RT scope (violates §4.9 RT 铁律)"
        );
        Self(Arc::clone(&self.0))
    }
}

impl<T: ?Sized> Drop for RtArc<T> {
    fn drop(&mut self) {
        // 只有最后一次 drop 会触发 T 的析构（释放堆）；中间 drop 仅原子减。
        // 两种都要禁——原子减同样是 Arc::drop 的 hidden work（cache line thrash），
        // 违反 "RT 不触 Arc" 的精神。
        assert!(
            !in_rt_scope(),
            "RtArc::drop called inside RT scope (violates §4.9 RT 铁律)"
        );
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for RtArc<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RtArc").field(&*self.0).finish()
    }
}

// ============================================================================
// assert_no_alloc bridge（feature = "rt_guard"）
// ============================================================================

/// 运行 `f`，在 `rt_guard` feature on 时启用 `assert_no_alloc` 检测。
///
/// - `rt_guard` ON：等价 `assert_no_alloc::assert_no_alloc(f)`——scope 内任何 alloc
///   触发进程 abort。需要 binary 侧配套 `#[global_allocator] static _: AllocDisabler = ...`
///   否则检测不生效（默认降级为普通调用）。
/// - `rt_guard` OFF：等价 `f()`，零开销。
#[inline]
pub fn assert_alloc_scope<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(feature = "rt_guard")]
    {
        assert_no_alloc::assert_no_alloc(f)
    }
    #[cfg(not(feature = "rt_guard"))]
    {
        f()
    }
}

/// RT 线程入口宏：同时挂 `RtArcGuard` 和 `assert_alloc_scope`。
///
/// 典型用法见模块头示例。
#[macro_export]
macro_rules! rt_entry {
    ($body:block) => {{
        let __rt_arc_guard = $crate::rt_guard::RtArcGuard::enter();
        $crate::rt_guard::assert_alloc_scope(|| $body)
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_enter_leaves_scope_on_drop() {
        assert!(!in_rt_scope());
        {
            let _g = RtArcGuard::enter();
            assert!(in_rt_scope());
        }
        assert!(!in_rt_scope());
    }

    #[test]
    #[should_panic(expected = "already in RT scope")]
    fn guard_cannot_nest() {
        let _g = RtArcGuard::enter();
        let _g2 = RtArcGuard::enter();
    }

    #[test]
    fn rt_arc_deref_in_scope_is_ok() {
        let a = RtArc::new(vec![1, 2, 3]);
        let _g = RtArcGuard::enter();
        // Deref 不触发 clone/drop：只读借用 safe
        let sum: i32 = a.iter().sum();
        assert_eq!(sum, 6);
    }

    #[test]
    #[should_panic(expected = "RtArc::clone called inside RT scope")]
    fn rt_arc_clone_in_scope_panics() {
        let a = RtArc::new(42u32);
        let _g = RtArcGuard::enter();
        let _b = a.clone();
    }

    #[test]
    fn rt_arc_clone_outside_scope_is_ok() {
        let a = RtArc::new(42u32);
        let b = a.clone();
        assert_eq!(*a, 42);
        assert_eq!(*b, 42);
        assert_eq!(RtArc::strong_count(&a), 2);
    }

    #[test]
    #[should_panic(expected = "RtArc::drop called inside RT scope")]
    fn rt_arc_drop_in_scope_panics() {
        let a = RtArc::new(123u32);
        let _g = RtArcGuard::enter();
        drop(a);
    }

    #[test]
    fn rt_entry_macro_runs_body() {
        let result = rt_entry!({ 1 + 1 });
        assert_eq!(result, 2);
    }

    #[test]
    fn rt_entry_macro_sets_scope() {
        rt_entry!({
            assert!(in_rt_scope());
        });
        assert!(!in_rt_scope());
    }
}
