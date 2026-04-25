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
}

/// `io_thread` 启动后的 handle。
pub struct IoThreadHandle {
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    metrics: Arc<IoThreadMetrics>,
}

impl IoThreadHandle {
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
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
/// # Errors
///
/// `std::io::Error` 当 `thread::Builder::spawn` 失败。
pub fn spawn_io_thread(cfg: IoThreadConfig) -> std::io::Result<IoThreadHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let metrics = Arc::new(IoThreadMetrics::default());

    let join_stop = Arc::clone(&stop);
    let join_metrics = Arc::clone(&metrics);

    let join = thread::Builder::new()
        .name("s0_io_thread".into())
        .spawn(move || {
            while !join_stop.load(Ordering::Acquire) {
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

    #[test]
    fn spawn_then_stop_cleanly() {
        let handle = spawn_io_thread(IoThreadConfig::default()).unwrap();
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
        let handle = spawn_io_thread(cfg).unwrap();
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
        let handle = spawn_io_thread(IoThreadConfig::default()).unwrap();
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
}
