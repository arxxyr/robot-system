//! S1 worker liveness 计数 + test-only freeze hook（v0.5 候选 E 段(8)，Spec
//! D12 + P.2.7）。
//!
//! ## 生产面（永远编译）
//!
//! [`TickLoopMetrics`]：`s1_daemon` 主线程 `run_action_tick_loop` 的进度计数
//! ——`loop_tick_count`（**每次 poll 迭代**推进，空闲亦推进；D12 r3 T4 第 1
//! 条 "request-driven worker 观测独立 loop_tick_count"）+ `heartbeat_published`
//! / `command_published`。watchdog observer 读前两者：tick loop 与 heartbeat
//! pub 在 B1 设计下同一线程（`run_action_tick_loop` 一并发 hb + cmd），两个
//! observer 共线程但语义分开——heartbeat 计数只在 hb 周期推进，是 S0 心跳
//! 链路的直接观测面。
//!
//! rr server ×4 的 `loop_tick_count` 住 [`crate::rr_server::RrServerMetrics`]，
//! state cache 的住 [`crate::state_cache::StateCacheLiveness`]。
//!
//! ## test-only 面（`cfg(any(test, feature = "test-freeze-hook"))`）
//!
//! [`freeze_check`]：沿 S0 `freeze_hook.rs` 语义——worker loop 顶端一次
//! `AtomicU64::load`；非 0 且未过期 → busy-spin 到 `clock.now() >= until` 或
//! stop。**不进 default features / 生产 release**（Cargo.toml feature 表守门）。
//! 命令路由住 [`crate::freeze_control`]（test-only iceoryx2 topic）。

#![allow(clippy::doc_markdown)]

use std::sync::atomic::AtomicU64;
#[cfg(any(test, feature = "test-freeze-hook"))]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(any(test, feature = "test-freeze-hook"))]
use robot_core::time::RobotClock;

/// `run_action_tick_loop` 进度计数（`Arc` 由 [`crate::coordinator::Coordinator`]
/// 持有并借给 tick loop / watchdog observer / freeze control）。
#[derive(Debug, Default)]
pub struct TickLoopMetrics {
    /// 每次 poll 迭代 +1（含空闲；liveness 源）。
    pub loop_tick_count: AtomicU64,
    /// 成功 publish `Heartbeat` 的帧数（liveness 源：hb 周期推进）。
    pub heartbeat_published: AtomicU64,
    /// 成功 publish `JointCommand` 的帧数（诊断，不作 observer）。
    pub command_published: AtomicU64,
    /// test-only：tick loop 顶端 freeze 截止（absolute ns，0 = 未冻结）。
    #[cfg(any(test, feature = "test-freeze-hook"))]
    pub freeze_until_ns_tick: AtomicU64,
    /// test-only：heartbeat publish 前 freeze 截止（同一线程，仅 park 点不同）。
    #[cfg(any(test, feature = "test-freeze-hook"))]
    pub freeze_until_ns_heartbeat: AtomicU64,
}

/// worker loop 顶端调一次：`freeze_until_ns == 0` 立即返回；否则 spin 到
/// `clock.now().0 >= until` 或 `stop`（沿 S0 `freeze_hook::freeze_check`）。
///
/// **spin 不 sleep**：模拟"线程被 hold 住但 OS 仍在调度"（cap-deadlock），
/// 让 `loop_tick_count` 真停滞而非让出 CPU 后被误判活跃。
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
    fn metrics_default_all_zero() {
        let m = TickLoopMetrics::default();
        assert_eq!(m.loop_tick_count.load(Ordering::Relaxed), 0);
        assert_eq!(m.heartbeat_published.load(Ordering::Relaxed), 0);
        assert_eq!(m.command_published.load(Ordering::Relaxed), 0);
        assert_eq!(m.freeze_until_ns_tick.load(Ordering::Relaxed), 0);
        assert_eq!(m.freeze_until_ns_heartbeat.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn freeze_check_returns_immediately_when_zero() {
        let clock = SystemClock::new();
        let freeze = AtomicU64::new(0);
        let stop = AtomicBool::new(false);
        let t0 = Instant::now();
        freeze_check(&freeze, &clock, &stop);
        assert!(t0.elapsed() < Duration::from_millis(5));
    }

    #[test]
    fn freeze_check_spins_until_expiry() {
        let clock = SystemClock::new();
        let freeze = AtomicU64::new(clock.now().0 + 30_000_000);
        let stop = AtomicBool::new(false);
        let t0 = Instant::now();
        freeze_check(&freeze, &clock, &stop);
        let el = t0.elapsed();
        assert!(el >= Duration::from_millis(25), "{el:?}");
        assert!(el < Duration::from_millis(500), "{el:?}");
    }

    #[test]
    fn freeze_check_exits_on_stop() {
        let clock = SystemClock::new();
        let freeze = AtomicU64::new(clock.now().0 + 10_000_000_000);
        let stop = AtomicBool::new(true);
        let t0 = Instant::now();
        freeze_check(&freeze, &clock, &stop);
        assert!(t0.elapsed() < Duration::from_millis(50));
    }
}
