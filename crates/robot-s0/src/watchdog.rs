//! S1 heartbeat watchdog —— 独立非 RT 线程，监视 `last_hb_ns` 原子是否
//! 超过阈值，超时发 `Event::HeartbeatTimeout` 到 `ipc_thread`。
//!
//! 对应 `docs/00_design.md` §6.2 "> 200 ms 无 S1 heartbeat → `CONTROL_LOST_*`"。
//!
//! ### 去抖（debounce）
//!
//! 超时只发一次 `HeartbeatTimeout`，然后 "armed = false" 静默；直到 `ipc_thread`
//! 收到新 heartbeat（写 `last_hb_ns`），watchdog 下次轮询看到 `last_hb_ns` 增长
//! → armed 回 true。这避免 `CONTROL_LOST_*` 状态下疯狂重发事件。
//!
//! ### 线程语义
//!
//! - 非 RT（普通优先级）。轮询周期 10 ms 默认，不够敏感的场景可调小。
//! - 用 `Arc<AtomicBool>` 作退出信号，`request_stop()` 后下个轮询周期退出。
//! - 用 `crossbeam_channel::Sender<Event>` 发事件给 `ipc_thread`；channel 满
//!   （不应发生，`ipc_thread` drain 很快）时丢弃事件 + 打印 warning 到 stderr。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Sender, TrySendError};
use robot_core::time::RobotClock;

use crate::state_machine::Event;

/// Watchdog 配置。默认 200 ms 超时、10 ms 轮询（`docs/00_design.md` §6.2）。
#[derive(Clone, Copy, Debug)]
pub struct WatchdogConfig {
    pub heartbeat_timeout_ns: u64,
    pub poll_interval_ns: u64,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            heartbeat_timeout_ns: 200_000_000, // 200 ms
            poll_interval_ns: 10_000_000,      // 10 ms
        }
    }
}

/// 启动 watchdog 线程。
///
/// 参数：
/// - `cfg`：阈值 + 轮询间隔。
/// - `clock`：读 `now()` 比 `last_hb_ns`。生产用 `SystemClock`，测试用 `MockClock`。
/// - `last_hb_ns`：`ipc_thread` 每收到一条 heartbeat 就 `store(now.0, Release)`；
///   watchdog `load(Acquire)` 比较。初始值建议 `clock.now().0`，否则 watchdog
///   启动瞬间就会触发一次超时（除非刚启动就应该 `CONTROL_LOST_*`？但 S0 启动
///   期是 `SAFE`，不进 ENABLED 所以没关系——`ipc_thread` 启动时会初始化）。
/// - `event_tx`：发送 `HeartbeatTimeout` 事件给 `ipc_thread`。
/// - `stop`：主进程退出前 `store(true)`，watchdog 下个轮询退出。
///
/// # Errors
///
/// 当 `std::thread::Builder::spawn` 失败（OS 资源耗尽）。
#[allow(clippy::needless_pass_by_value)]
pub fn spawn_watchdog(
    cfg: WatchdogConfig,
    clock: Arc<dyn RobotClock>,
    last_hb_ns: Arc<AtomicU64>,
    event_tx: Sender<Event>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    #[cfg(feature = "test-comm-fault-hook")]
    crate::comm_fault_hook::before_spawn("watchdog")?;
    std::thread::Builder::new()
        .name("s0_watchdog".into())
        .spawn(move || run_loop(cfg, clock, &last_hb_ns, &event_tx, &stop))
}

#[allow(clippy::needless_pass_by_value)]
fn run_loop(
    cfg: WatchdogConfig,
    clock: Arc<dyn RobotClock>,
    last_hb_ns: &Arc<AtomicU64>,
    event_tx: &Sender<Event>,
    stop: &Arc<AtomicBool>,
) {
    // armed = true 表示 "下次超时允许发事件"
    // armed = false 表示 "已发过事件，等新 heartbeat 重新武装"
    let mut armed = true;
    let mut last_seen_hb = last_hb_ns.load(Ordering::Acquire);
    let mut pending_timeout = false;
    let mut full_merged = 0_u64;

    while crate::worker_stop::worker_running(
        crate::worker_stop::WorkerKind::Watchdog,
        clock.as_ref(),
        stop,
    ) {
        let cur_hb = last_hb_ns.load(Ordering::Acquire);
        let now = clock.now().0;

        // rearm 条件：`last_hb_ns` 增长（ipc_thread 收到新 heartbeat）
        if cur_hb > last_seen_hb {
            armed = true;
            last_seen_hb = cur_hb;
        }

        // 超时判断：now - cur_hb > threshold
        // 注意：若 cur_hb > now（MockClock 下可能发生），skip（防 underflow）
        let overdue = now.saturating_sub(cur_hb) > cfg.heartbeat_timeout_ns;
        if overdue && armed {
            pending_timeout = true;
            armed = false; // 去抖：直到下次看到新 hb 才重新 armed
        }
        if pending_timeout {
            match event_tx.try_send(Event::HeartbeatTimeout) {
                Ok(()) => {
                    pending_timeout = false;
                    if full_merged != 0 {
                        tracing::warn!(full_merged, "watchdog 满队列合并的 Timeout 已送达");
                        full_merged = 0;
                    }
                }
                Err(TrySendError::Full(_)) => full_merged = full_merged.saturating_add(1),
                Err(TrySendError::Disconnected(_)) => {
                    tracing::error!(reason = "Disconnected", "watchdog 的 IPC 接收端已退出");
                    break;
                }
            }
        }

        // 真 sleep；非 RT 线程，普通 std::thread::sleep 够用。
        std::thread::sleep(Duration::from_nanos(cfg.poll_interval_ns));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use robot_core::time::{MockClock, MonoRawNs};

    fn fast_cfg() -> WatchdogConfig {
        WatchdogConfig {
            heartbeat_timeout_ns: 50_000_000, // 50 ms
            poll_interval_ns: 1_000_000,      // 1 ms
        }
    }

    /// 永远没有 heartbeat → watchdog 第一次轮询就应该发一次 `HeartbeatTimeout`。
    #[test]
    fn fires_once_when_never_heartbeated() {
        // last_hb_ns = 0; MockClock start = 0；advance 200ms → overdue。
        let clock: Arc<dyn RobotClock> = Arc::new(MockClock::new(MonoRawNs(200_000_000)));
        let last_hb = Arc::new(AtomicU64::new(0));
        let (tx, rx) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_watchdog(fast_cfg(), clock, last_hb, tx, Arc::clone(&stop)).unwrap();

        // 等 watchdog 至少跑 1 次轮询（1 ms interval + 宽松）
        let ev = rx
            .recv_timeout(Duration::from_millis(200))
            .expect("should get HeartbeatTimeout");
        assert_eq!(ev, Event::HeartbeatTimeout);

        // 去抖：后续不应再发（即使 clock 永远没更新 hb）
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());

        stop.store(true, Ordering::Release);
        handle.join().unwrap();
    }

    /// 收到新 heartbeat → rearm → 再次超时 → 再发一次。
    #[test]
    fn rearms_after_new_heartbeat() {
        let clock = Arc::new(MockClock::new(MonoRawNs(200_000_000)));
        let clock_dyn: Arc<dyn RobotClock> = Arc::clone(&clock) as Arc<dyn RobotClock>;
        let last_hb = Arc::new(AtomicU64::new(0));
        let (tx, rx) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_watchdog(
            fast_cfg(),
            clock_dyn,
            Arc::clone(&last_hb),
            tx,
            Arc::clone(&stop),
        )
        .unwrap();

        // 第一次超时
        let ev1 = rx.recv_timeout(Duration::from_millis(200)).unwrap();
        assert_eq!(ev1, Event::HeartbeatTimeout);

        // 模拟新 heartbeat：last_hb 更新到接近当前时间
        last_hb.store(clock.now().0, Ordering::Release);
        // 但时间继续推进超过阈值
        // MockClock 的 now 不会自动推进，只有 advance / sleep_until 才推。
        // 直接用 advance 让下次 poll 时 now > last_hb + threshold。
        // 稍等一下让 watchdog 看到新的 last_hb（一轮 1ms poll）
        std::thread::sleep(Duration::from_millis(10));
        clock.advance(robot_core::time::DurationNs(100_000_000)); // +100 ms > 50 ms threshold

        // 应该再次超时
        let ev2 = rx.recv_timeout(Duration::from_millis(200)).unwrap();
        assert_eq!(ev2, Event::HeartbeatTimeout);

        stop.store(true, Ordering::Release);
        handle.join().unwrap();
    }

    /// 心跳持续新鲜 → watchdog 不应发事件。
    #[test]
    fn no_event_when_heartbeat_fresh() {
        let clock = Arc::new(MockClock::new(MonoRawNs(0)));
        let clock_dyn: Arc<dyn RobotClock> = Arc::clone(&clock) as Arc<dyn RobotClock>;
        let last_hb = Arc::new(AtomicU64::new(0));
        let (tx, rx) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_watchdog(
            fast_cfg(),
            clock_dyn,
            Arc::clone(&last_hb),
            tx,
            Arc::clone(&stop),
        )
        .unwrap();

        // 每 10 ms 更新一次 last_hb = now，共 6 次（总 60 ms），阈值 50 ms。
        // 每次 now - last_hb ≤ 10 ms < 50 ms，绝不会触发。
        for _ in 0..6 {
            std::thread::sleep(Duration::from_millis(10));
            clock.advance(robot_core::time::DurationNs(10_000_000));
            last_hb.store(clock.now().0, Ordering::Release);
        }

        assert!(rx.try_recv().is_err(), "unexpected event while hb fresh");

        stop.store(true, Ordering::Release);
        handle.join().unwrap();
    }

    /// `request_stop` 后线程应尽快退出。
    #[test]
    fn stops_promptly_on_stop_signal() {
        let clock: Arc<dyn RobotClock> = Arc::new(MockClock::new(MonoRawNs(0)));
        let last_hb = Arc::new(AtomicU64::new(0));
        let (tx, _rx) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_watchdog(
            WatchdogConfig::default(),
            clock,
            last_hb,
            tx,
            Arc::clone(&stop),
        )
        .unwrap();

        std::thread::sleep(Duration::from_millis(20));
        stop.store(true, Ordering::Release);
        let start = std::time::Instant::now();
        handle.join().unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "watchdog join too slow: {:?}",
            start.elapsed()
        );
    }
}

/// Timeout 的被判断心跳必须仍是当前心跳；较旧或较新都已过期。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Valid,
    Stale {
        latest_hb_ns: u64,
        judged_hb_ns: u64,
    },
}

#[must_use]
pub const fn judge_heartbeat_timeout(latest_hb_ns: u64, judged_hb_ns: u64) -> Freshness {
    if latest_hb_ns == judged_hb_ns {
        Freshness::Valid
    } else {
        Freshness::Stale {
            latest_hb_ns,
            judged_hb_ns,
        }
    }
}

/// 与既有 watchdog 循环保持同式，阈值相等及未来心跳都不超时。
#[must_use]
pub const fn is_overdue(now_ns: u64, last_hb_ns: u64, timeout_ns: u64) -> bool {
    now_ns.saturating_sub(last_hb_ns) > timeout_ns
}

#[cfg(test)]
mod freshness_tests {
    use super::*;

    #[test]
    fn equal_stamp_is_valid() {
        assert_eq!(judge_heartbeat_timeout(100, 100), Freshness::Valid);
    }
    #[test]
    fn newer_heartbeat_is_stale() {
        assert_eq!(
            judge_heartbeat_timeout(101, 100),
            Freshness::Stale {
                latest_hb_ns: 101,
                judged_hb_ns: 100
            }
        );
    }
    #[test]
    fn older_stamp_99_vs_100_is_stale() {
        assert_eq!(
            judge_heartbeat_timeout(99, 100),
            Freshness::Stale {
                latest_hb_ns: 99,
                judged_hb_ns: 100
            }
        );
    }
    #[test]
    fn is_overdue_matches_run_loop_formula_and_saturates() {
        for (now, last, timeout, expected) in [
            (100, 90, 10, false),
            (101, 90, 10, true),
            (90, 100, 0, false),
            (u64::MAX, 0, u64::MAX, false),
            (u64::MAX, 0, u64::MAX - 1, true),
        ] {
            assert_eq!(is_overdue(now, last, timeout), expected);
            assert_eq!(
                is_overdue(now, last, timeout),
                now.saturating_sub(last) > timeout
            );
        }
    }
}
