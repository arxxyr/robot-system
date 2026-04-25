#![allow(clippy::doc_markdown)]
//! 单测共享工具。**仅 `#[cfg(test)]` 编译**，不进 production binary。
//!
//! ## 背景
//!
//! workspace `cargo test` 并行跑 io_thread / rt_thread / serial_io_thread
//! 的多个 worker 测试时，主测试线程用 `thread::sleep(N ms)` + 断言"worker
//! 已 tick K 次"是**时序敏感**的：cargo 的多线程并行调度让 mock/system
//! clock 下 worker 不一定能在固定 sleep 内完成 N tick，导致测试**间歇性
//! 失败**（reviewer 在 ClearEStop 阶段指出 9 个测试在并行下 flaky）。
//!
//! ## 修复
//!
//! [`wait_until`]：用**断言式轮询**替代固定 sleep。给定超时 + predicate，
//! 1 ms 间隔检查直到 `pred() == true` 或超时。**测试通过当且仅当 predicate
//! 在超时内为真**，与外部调度无关。
//!
//! 用法：
//! ```ignore
//! use crate::test_util::wait_until;
//! use std::time::Duration;
//!
//! let metrics = handle.metrics();
//! assert!(
//!     wait_until(Duration::from_millis(500), || {
//!         metrics.tick_count.load(Ordering::Relaxed) > 0
//!     }),
//!     "worker 500ms 内未跑出第一 tick"
//! );
//! ```

#![cfg(test)]

use std::time::{Duration, Instant};

/// 轮询 `pred` 直到返 `true` 或超 `timeout`。返回 `pred()` 的最终值（`true`
/// 表示成功，`false` 表示超时）。轮询间隔 1 ms（足够细让 worker 有 tick
/// 机会，又不过度占 CPU）。
///
/// **不**用 `MockClock` 推进：本 helper 是 wall-clock-based，专门解决"主
/// 测试线程等 worker 完成动作"的工程问题。worker 内部走什么时钟无关。
pub(crate) fn wait_until<F: FnMut() -> bool>(timeout: Duration, mut pred: F) -> bool {
    let start = Instant::now();
    let poll_interval = Duration::from_millis(1);
    loop {
        if pred() {
            return true;
        }
        if start.elapsed() >= timeout {
            return pred(); // 最后再判一次，避免 race
        }
        std::thread::sleep(poll_interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn wait_until_returns_true_when_pred_satisfied() {
        let flag = Arc::new(AtomicBool::new(false));
        let f = Arc::clone(&flag);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            f.store(true, Ordering::Release);
        });
        assert!(wait_until(Duration::from_millis(500), || flag.load(Ordering::Acquire)));
    }

    #[test]
    fn wait_until_returns_false_on_timeout() {
        let flag = AtomicBool::new(false);
        let started = Instant::now();
        let result = wait_until(Duration::from_millis(20), || flag.load(Ordering::Acquire));
        assert!(!result);
        // 超时 ≈ 20ms（不会卡更久）
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[test]
    #[allow(clippy::unnecessary_min_or_max)]
    fn wait_until_returns_immediately_when_already_true() {
        let started = Instant::now();
        // 故意用大超时验证立即返回不会卡到超时
        let big_timeout = Duration::from_secs(10);
        let result = wait_until(big_timeout, || true);
        assert!(result);
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "应立即返；耗时 {:?}",
            started.elapsed()
        );
    }
}
