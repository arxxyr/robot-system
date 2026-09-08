//! `s0_io_thread` —— S0 四线程模型的"`SoftRT` adapter 轮询线程"。
//!
//! 对应 `docs/00_design.md` §1.2：poll `SoftRT` adapter → 写 `softrt_snapshot`。
//!
//! ### Part 2 范围
//!
//! Part 2 只做**占位骨架**：100 Hz 空 loop，能 spawn / stop / join，tick 计数增长。
//! 不接任何 adapter。
//!
//! Part 3+（Step 7 起）再接真 `SoftRT` adapter（RealSense / webcam / wrist IMU 等）：
//! - 100 Hz 调 `adapter.poll_state()` 或类似接口
//! - 写 `softrt_snapshot` seqlock（新增，类型 TBD）
//! - 错误记 metrics，不影响 `rt_thread` 延迟
//!
//! ### 为什么现在要占位
//!
//! `s0_daemon` 在 Part 2 拉起全部 4 线程才能对称（ipc + rt + pub + io）—— 即使
//! `io_thread` 什么都不做，它的 spawn/stop 生命周期也要存在，方便：
//! - 测试 `s0_daemon` 的 graceful shutdown 行为
//! - 未来接真 adapter 时只改内部 `run_loop`，不改外部接线

#![forbid(unused_must_use)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use robot_core::time::RobotClock;

/// `io_thread` 配置。
#[derive(Clone, Copy, Debug)]
pub struct IoThreadConfig {
    /// poll 周期（纳秒）。默认 10 ms（100 Hz）。
    pub poll_interval_ns: u64,
}

impl Default for IoThreadConfig {
    fn default() -> Self {
        Self {
            poll_interval_ns: 10_000_000, // 10 ms
        }
    }
}

/// 占位计数器。Part 3 接 adapter 后还会加 `read_errors` / `polls_applied` 等。
#[derive(Debug, Default)]
pub struct IoThreadMetrics {
    pub tick_count: AtomicU64,
    /// **A2 freeze hook**（v0.3 / test-only，cfg-gated）：tick loop 顶端 spin
    /// 到该单调时钟 ns 为止；0 = 不冻。详 [`crate::freeze_hook`] 模块 doc。
    /// 仅 `cfg(any(test, feature = "test-freeze-hook"))` 编译时存在，**不进
    /// 生产 release**。
    #[cfg(any(test, feature = "test-freeze-hook"))]
    pub freeze_until_ns: AtomicU64,
}

/// `io_thread` 启动后的 handle。
pub struct IoThreadHandle {
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    metrics: Arc<IoThreadMetrics>,
}

impl IoThreadHandle {
    pub fn request_stop(&self) {
        self.stop.swap(true, Ordering::AcqRel);
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }

    #[must_use]
    pub fn metrics(&self) -> &Arc<IoThreadMetrics> {
        &self.metrics
    }

    /// # Errors
    ///
    /// `JoinHandle::join` 的 `Err`（线程 panic）。
    pub fn join(self) -> thread::Result<()> {
        self.join.join()
    }
}

/// 启动 `io_thread`。Part 2 no-op 占位。
///
/// `clock` 仅给 [`crate::freeze_hook`] 用；Part 3 接真 adapter 后也会用它做
/// poll deadline。Part 2 占位 loop 仍用 `thread::sleep` 走真实时间，避免
/// 跟 [`robot_core::time::MockClock`] 测试的语义不同。
///
/// # Errors
///
/// `std::io::Error` 当 `thread::Builder::spawn` 失败。
#[allow(clippy::needless_pass_by_value)]
pub fn spawn_io_thread(
    cfg: IoThreadConfig,
    clock: Arc<dyn RobotClock>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<IoThreadHandle> {
    #[cfg(feature = "test-comm-fault-hook")]
    crate::comm_fault_hook::before_spawn("io")?;
    let metrics = Arc::new(IoThreadMetrics::default());

    let join_stop = Arc::clone(&stop);
    let join_metrics = Arc::clone(&metrics);
    // cfg(test) / feature 关时 clock 未被 closure 用；显式 rename 保证 Part 3
    // 接真 adapter 后无需改 spawn 签名（clippy::needless_pass_by_value 已
    // allow 于函数 attribute）。
    let join_clock = clock;

    let join = thread::Builder::new()
        .name("s0_io_thread".into())
        .spawn(move || {
            // cfg 关时 join_clock 进闭包后未使用；显式 forget 让 closure
            // 拿走 ownership 但不触发 unused_variables（cfg 开时 freeze_check
            // 真正读它）。
            #[cfg(not(any(test, feature = "test-freeze-hook")))]
            let _ = &join_clock;

            while crate::worker_stop::worker_running(
                crate::worker_stop::WorkerKind::Io,
                join_clock.as_ref(),
                &join_stop,
            ) {
                // ─── A2 freeze hook（test-only，0 cost when freeze_until_ns == 0）───
                #[cfg(any(test, feature = "test-freeze-hook"))]
                crate::freeze_hook::freeze_check(
                    &join_metrics.freeze_until_ns,
                    join_clock.as_ref(),
                    &join_stop,
                );

                // Part 3：这里会调 adapter.poll_state() → 写 softrt_snapshot。
                join_metrics.tick_count.fetch_add(1, Ordering::Relaxed);
                thread::sleep(Duration::from_nanos(cfg.poll_interval_ns));
            }
        })?;

    Ok(IoThreadHandle {
        join,
        stop,
        metrics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use robot_core::time::SystemClock;

    fn test_clock() -> Arc<dyn RobotClock> {
        Arc::new(SystemClock::new())
    }

    #[test]
    fn spawn_then_stop_cleanly() {
        let handle = spawn_io_thread(
            IoThreadConfig::default(),
            test_clock(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(30));
        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn tick_count_increments() {
        use crate::test_util::wait_until;
        // 加速 poll 让测试短时间就看到多次 tick。
        let cfg = IoThreadConfig {
            poll_interval_ns: 1_000_000, // 1 ms
        };
        let handle = spawn_io_thread(
            cfg,
            test_clock(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        let metrics = Arc::clone(handle.metrics());
        // 确定性轮询：等 tick_count >= 5（每 ms 一次，100ms 内绰绰有余）
        assert!(
            wait_until(Duration::from_secs(2), || {
                metrics.tick_count.load(Ordering::Relaxed) >= 5
            }),
            "io_thread 2s 内未 tick ≥ 5 次，实际 {}",
            metrics.tick_count.load(Ordering::Relaxed)
        );
        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn stops_promptly() {
        use crate::test_util::wait_until;
        let handle = spawn_io_thread(
            IoThreadConfig::default(),
            test_clock(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        let metrics = Arc::clone(handle.metrics());
        // 等线程跑起来（至少 1 tick）再 request_stop —— 比固定 5ms sleep 稳
        assert!(
            wait_until(Duration::from_secs(2), || {
                metrics.tick_count.load(Ordering::Relaxed) > 0
            }),
            "io_thread 2s 内未跑出第一 tick"
        );
        handle.request_stop();
        let start = std::time::Instant::now();
        handle.join().unwrap();
        // 退出延迟仍按 50ms 上限（poll_interval default 是 1ms 量级）
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "io_thread join too slow: {:?}",
            start.elapsed()
        );
    }

    /// **A2 freeze hook 单测**：设 `freeze_until_ns` 到未来 80ms，期间
    /// `tick_count` 不增长（spin 期间 worker 卡住）；过期后恢复增长。
    #[test]
    fn freeze_hook_holds_tick_count() {
        let cfg = IoThreadConfig {
            poll_interval_ns: 1_000_000, // 1 ms
        };
        let clock = test_clock();
        let handle = spawn_io_thread(
            cfg,
            Arc::clone(&clock),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        let metrics = Arc::clone(handle.metrics());

        // 等 worker 跑起来
        thread::sleep(Duration::from_millis(20));
        let pre_freeze = metrics.tick_count.load(Ordering::Relaxed);
        assert!(pre_freeze > 0, "worker 未启动，tick_count={pre_freeze}");

        // 设 freeze 80ms
        let until = clock.now().0 + 80_000_000;
        metrics.freeze_until_ns.store(until, Ordering::Release);

        // freeze 中段（40ms 后）确认 tick_count 没增多少
        thread::sleep(Duration::from_millis(40));
        let mid_freeze = metrics.tick_count.load(Ordering::Relaxed);
        let mid_grew = mid_freeze.saturating_sub(pre_freeze);
        assert!(
            mid_grew <= 1,
            "freeze 期间 tick_count 不应继续增长：pre={pre_freeze} mid={mid_freeze} 增 {mid_grew}",
        );

        // 等 freeze 过期 + 一些 tick 时间
        thread::sleep(Duration::from_millis(80));
        let post_freeze = metrics.tick_count.load(Ordering::Relaxed);
        assert!(
            post_freeze > mid_freeze + 5,
            "freeze 过期后 tick_count 应当恢复增长：mid={mid_freeze} post={post_freeze}",
        );

        handle.request_stop();
        handle.join().unwrap();
    }
}
