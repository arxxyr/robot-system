//! Worker tick loop 顶端的 freeze hook（v0.3 A2 / `docs/00_design.md` §6.7
//! #2 systemd 是最外层看门狗）。
//!
//! ⚠️ **test-only**：整 module 由 `cfg(any(test, feature = "test-freeze-hook"))`
//! 守门，**不进生产 release**。reviewer 拍板硬边界（详
//! [`docs/handoffs/v0.3_entry.md`](../../../../docs/handoffs/v0.3_entry.md)
//! §4.2）：`test-freeze-hook` 不能进 default feature / 任何生产 release combo
//! （Cargo.toml feature 表已注释守门）。
//!
//! ## 为什么需要
//!
//! v0.2 收尾把 §6.7 #2 降级宣称为"代码层面到位 / 没端到端测过"
//! （详 [`docs/handoffs/2026-04-26_5.md`](../../../../docs/handoffs/2026-04-26_5.md)）：
//! [`crate::systemd_notify::LivenessObserver`] 真接 4 worker `tick_count`，watchdog
//! 主循环 liveness-gated 不喂 `sd_notify(WATCHDOG=1)`，但缺一条端到端测试证
//! 明 worker-stall → `WatchdogSec` → systemd `Restart=on-failure` 拉起新 PID 真
//! 跑通。A2 freeze hook + A2-3 `freeze_systemd_smoke` 集成测试就是补这条 e2e。
//!
//! ## 工作原理
//!
//! 每个 worker 在 tick loop **顶端**调一次 [`freeze_check`]：
//!
//! 1. `freeze_until_ns == 0` → 立刻返回（hot path 仅 1 次 atomic load + 比较）。
//! 2. `freeze_until_ns > clock.now()` → busy-spin 用 `std::hint::spin_loop()`
//!    直到过期或 `stop` 信号到来。
//!
//! **spin（不 sleep）的理由**：
//! - sleep 让出 CPU 让其它 worker 看似活跃；本测试要的是"线程被 hold 住但
//!   OS 调度仍在跑"语义（cap-deadlock 模拟）。
//! - 单调等待时长由 daemon 内 monotonic clock 决定，**不接受外部 absolute
//!   ns**。reviewer 拍板硬边界：A2-2 control thread 收到 `worker + duration_ms`
//!   → 用 daemon 内 [`robot_core::time::RobotClock`] 计算
//!   `freeze_until_ns = clock.now().0 + duration_ms * 1_000_000`，再写到对
//!   应 worker 的 `freeze_until_ns` 原子里。
//!
//! ## RT 安全性
//!
//! [`freeze_check`] 是 **alloc-free / syscall-free / no-Arc-clone**：
//! - 1 次 `AtomicU64::load`（命中 0 则提前 return，hot path 极轻）
//! - 否则循环：`clock.now()` 单调时钟读 + `AtomicBool::load` + `spin_loop()` hint
//!
//! 可在 [`crate::rt_thread`] `SCHED_FIFO` 路径调（reviewer 拍板：`rt_thread`
//! hook 加，但**不**在 Orin `SCHED_FIFO` busy-spin 冻 RT 系统级 smoke，避免调
//! 度饥饿；`rt_thread` 验证走单测 / 非 `SCHED_FIFO` 集成）。

#![cfg(any(test, feature = "test-freeze-hook"))]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use robot_core::time::RobotClock;

/// 在 worker tick loop 顶端调一次。`freeze_until_ns == 0` 时立即返回；非 0
/// 时 spin 到 `clock.now().0 >= freeze_until_ns` 或 `stop` 信号到来。
///
/// 调用方式（5 个 worker 都是这一行）：
///
/// ```ignore
/// while !stop.load(Ordering::Acquire) {
///     #[cfg(any(test, feature = "test-freeze-hook"))]
///     crate::freeze_hook::freeze_check(&metrics.freeze_until_ns, clock, stop);
///     // ... 正常 tick 逻辑
/// }
/// ```
///
/// 详 module doc。
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
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn returns_immediately_when_freeze_until_zero() {
        let clock = SystemClock::new();
        let freeze = AtomicU64::new(0);
        let stop = AtomicBool::new(false);
        let start = Instant::now();
        freeze_check(&freeze, &clock, &stop);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(5),
            "freeze_until_ns=0 应当立即返回，实际耗时 {elapsed:?}",
        );
    }

    #[test]
    fn spins_until_freeze_until_ns_passed() {
        let clock = SystemClock::new();
        let freeze = AtomicU64::new(clock.now().0 + 30_000_000); // 30ms
        let stop = AtomicBool::new(false);
        let start = Instant::now();
        freeze_check(&freeze, &clock, &stop);
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(25),
            "freeze 应当 spin ≥ 25ms，实际 {elapsed:?}",
        );
        assert!(
            elapsed < Duration::from_millis(150),
            "freeze 不应明显超过 30ms（防 spin 失控），实际 {elapsed:?}",
        );
    }

    #[test]
    fn stop_signal_breaks_spin_promptly() {
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        // 设 freeze 到很远的未来（10s），然后 50ms 后从其它线程 store stop=true。
        let freeze = Arc::new(AtomicU64::new(clock.now().0 + 10_000_000_000));
        let stop = Arc::new(AtomicBool::new(false));

        let stop_for_signal = Arc::clone(&stop);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            stop_for_signal.store(true, Ordering::Release);
        });

        let start = Instant::now();
        freeze_check(&freeze, clock.as_ref(), &stop);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "stop 信号应当让 spin 在 ~50ms 内退出，实际 {elapsed:?}",
        );
    }
}
