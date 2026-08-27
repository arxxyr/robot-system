//! gateway worker liveness 面 + test-only freeze hook（v0.5 候选 E 段(8)，
//! Spec D12 + P.2.7）。
//!
//! ## 生产面（永远编译）
//!
//! 各 worker 线程 metrics 均带 `loop_tick_count`（**每次 poll 迭代**推进，空闲
//! 亦推进；D12 r3 T4 第 1 条 "request-driven worker 观测独立 loop_tick_count，
//! 不用请求计数"）：
//!
//! | observer label | 计数源 |
//! |---|---|
//! | `rr_io_<topic>` ×6 | [`crate::rr_io_thread::RrIoMetrics::loop_tick_count`] |
//! | `estop_pub` | [`crate::estop_pub_thread::EStopPubMetrics::loop_tick_count`] |
//! | `state_pool:<hardware_id>` 每线程一条 | [`crate::state_pool::StatePoolThreadMetrics::loop_tick_count`]（R6-5：禁聚合求和） |
//! | `tonic_probe` | [`crate::systemd_probe::TonicProbeMetrics::success_count`]（loopback `GetState("")` → `InvalidArgument` 计成功） |
//!
//! ## test-only 面（`cfg(any(test, feature = "test-freeze-hook"))`）
//!
//! [`freeze_check`]：沿 S0 `freeze_hook.rs` / S1 `liveness.rs` 语义——loop 顶端
//! 一次 `AtomicU64::load`；非 0 且未过期 → busy-spin 到过期或 stop。命令路由
//! 住 [`crate::freeze_control`]（topic `/<robot_id>/gateway/test/freeze`）。
//! tonic probe 是 tokio task，其冻结走 `tokio::time::sleep` 分片等待（不 spin
//! runtime worker），效果同为 `success_count` 停滞。

#![allow(clippy::doc_markdown)]

#[cfg(any(test, feature = "test-freeze-hook"))]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(any(test, feature = "test-freeze-hook"))]
use robot_core::time::RobotClock;

/// std worker loop 顶端调一次（沿 S0 `freeze_hook::freeze_check`）。
#[cfg(any(test, feature = "test-freeze-hook"))]
#[inline]
pub fn freeze_check(freeze_until_ns: &AtomicU64, clock: &dyn RobotClock, stop: &AtomicBool) {
    let until = freeze_until_ns.load(Ordering::Acquire);
    if until == 0 {
        return;
    }
    while clock.now().0 < until && !stop.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use robot_core::time::SystemClock;
    use std::time::{Duration, Instant};

    #[test]
    fn freeze_check_returns_immediately_when_zero() {
        let clock = SystemClock::new();
        let t0 = Instant::now();
        freeze_check(&AtomicU64::new(0), &clock, &AtomicBool::new(false));
        assert!(t0.elapsed() < Duration::from_millis(5));
    }

    #[test]
    fn freeze_check_spins_until_expiry_or_stop() {
        let clock = SystemClock::new();
        let t0 = Instant::now();
        freeze_check(
            &AtomicU64::new(clock.now().0 + 30_000_000),
            &clock,
            &AtomicBool::new(false),
        );
        let el = t0.elapsed();
        assert!(
            el >= Duration::from_millis(25) && el < Duration::from_millis(500),
            "{el:?}"
        );
        let t0 = Instant::now();
        freeze_check(
            &AtomicU64::new(clock.now().0 + 10_000_000_000),
            &clock,
            &AtomicBool::new(true),
        );
        assert!(t0.elapsed() < Duration::from_millis(50));
    }
}
