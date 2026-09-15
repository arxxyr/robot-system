//! 原健康辅助线程的一次性故障接缝，仅由 `test-comm-fault-hook` 启用。
//!
//! 控制对象不持有 stop 或线程句柄，不改变原拥有与回收路径。调用者必须先成功
//! 安装原 handle，再武装；只有原 helper 的循环边界可以发布事实并触发 panic。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// 原 handle 的固定线程种类；不能在武装时改选其它线程。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperKind {
    HealthMonitor,
    SystemdWatchdog,
}

/// 原 helper 确实消费触发后的只读事实；时戳不充当线程退出或 join 凭证。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelperEntered {
    pub kind: HelperKind,
    pub at_ns: u64,
}

struct State {
    kind: HelperKind,
    clock: OnceLock<fn() -> u64>,
    consumed: AtomicBool,
    at_ns: AtomicU64,
    entered: AtomicBool,
}

/// 每个原 handle 独有的有限控制对象；克隆只共享一次触发与只读事实。
#[derive(Clone)]
pub struct HelperPanicControl(Arc<State>);

impl HelperPanicControl {
    pub(crate) fn new(kind: HelperKind) -> Self {
        Self(Arc::new(State {
            kind,
            clock: OnceLock::new(),
            consumed: AtomicBool::new(false),
            at_ns: AtomicU64::new(0),
            entered: AtomicBool::new(false),
        }))
    }

    /// 仅首调用成功；原 handle 安装失败时不得武装。
    /// `clock` 应只读取调用者的原单调时钟域，由真实 helper 调用且不得 panic。
    #[must_use]
    pub fn arm(&self, clock: fn() -> u64) -> bool {
        self.0.clock.set(clock).is_ok()
    }

    /// 只返回真实 helper 已发布的一帧；零时戳也合法，不作未发布哨兵。
    #[must_use]
    pub fn entered(&self) -> Option<HelperEntered> {
        self.0
            .entered
            .load(Ordering::Acquire)
            .then(|| HelperEntered {
                kind: self.0.kind,
                at_ns: self.0.at_ns.load(Ordering::Relaxed),
            })
    }

    pub(crate) fn panic_if_armed(&self) {
        let Some(clock) = self.0.clock.get() else {
            return;
        };
        if self.0.consumed.swap(true, Ordering::AcqRel) {
            return;
        }
        let at_ns = clock();
        self.0.at_ns.store(at_ns, Ordering::Relaxed);
        self.0.entered.store(true, Ordering::Release);
        match self.0.kind {
            HelperKind::HealthMonitor => panic!("R4c 原 HealthMonitor 循环测试 panic"),
            HelperKind::SystemdWatchdog => panic!("R4c 原 SystemdWatchdog 循环测试 panic"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{HelperEntered, HelperKind};
    use crate::systemd::{
        LivenessObserver, WatchdogConfig, WatchdogHandle, spawn_watchdog_with_usec,
    };
    use crate::{JoinState, MonitorConfig, MonitorHandle, spawn_monitor};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::{Duration, Instant};

    fn monitor() -> MonitorHandle {
        spawn_monitor(MonitorConfig {
            interval: Duration::from_secs(10),
            interested_processes: Vec::new(),
            top_n: 0,
        })
    }

    /// 原 observer 首次读取证明已进入真实主循环；关闭 grace 避免依赖 manager。
    fn watchdog() -> (WatchdogHandle, Receiver<()>) {
        let (sender, receiver) = mpsc::channel();
        let sent = AtomicBool::new(false);
        let observer = LivenessObserver::from_fn("真实循环首帧", move || {
            if !sent.swap(true, Ordering::AcqRel) {
                sender.send(()).expect("父测试等待原 observer 首次读取");
            }
            1
        });
        let mut config = WatchdogConfig::new("r4c-health-watchdog");
        config.startup_grace_ping = false;
        let handle = spawn_watchdog_with_usec(vec![observer], config, 20_000_000)
            .expect("非零间隔须创建原 watchdog");
        (handle, receiver)
    }

    fn wait_finished(finished: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !finished() {
            assert!(Instant::now() < deadline, "原 helper 必须在有界时间内退出");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(Instant::now() < deadline, "成功观察也必须在同一原5s窗内");
    }

    fn monitor_clock() -> u64 {
        assert_eq!(thread::current().name(), Some("robot-health"));
        0
    }

    fn watchdog_clock() -> u64 {
        assert_eq!(thread::current().name(), Some("r4c-health-watchdog"));
        42
    }

    fn forbidden_clock() -> u64 {
        panic!("未武装、重复武装或已退出时不得调用此时钟");
    }

    fn assert_panic(result: JoinState, expected: &str) {
        let JoinState::Joined(Err(payload)) = result else {
            panic!("必须保留原 JoinHandle 的真实 panic 结果");
        };
        assert_eq!(payload.downcast_ref::<&str>().copied(), Some(expected));
    }

    #[test]
    fn real_monitor_consumes_once_inside_loop_and_preserves_original_join_error() {
        let mut handle = monitor();
        let control = handle.test_panic_control();
        let shared = control.clone();
        assert!(control.entered().is_none());
        assert!(control.arm(monitor_clock));
        assert!(!shared.arm(forbidden_clock));
        wait_finished(|| handle.is_finished());
        assert_eq!(
            control.entered(),
            Some(HelperEntered {
                kind: HelperKind::HealthMonitor,
                at_ns: 0,
            })
        );
        assert_eq!(control.entered(), shared.entered());
        assert_panic(handle.try_join(), "R4c 原 HealthMonitor 循环测试 panic");
        assert!(!handle.is_finished());
        assert!(matches!(handle.try_join(), JoinState::Pending));
        drop(handle);
        assert!(!shared.arm(forbidden_clock));
        assert_eq!(shared.entered().unwrap().at_ns, 0);
    }

    #[test]
    fn real_watchdog_panics_at_original_sleep_slice_and_preserves_join_error() {
        let (mut handle, entered_loop) = watchdog();
        let control = handle.test_panic_control();
        entered_loop
            .recv_timeout(Duration::from_secs(5))
            .expect("真实 run_watchdog_loop 先读取原 observer");
        assert!(control.entered().is_none());
        assert!(control.arm(watchdog_clock));
        wait_finished(|| handle.is_finished());
        assert_eq!(
            control.entered(),
            Some(HelperEntered {
                kind: HelperKind::SystemdWatchdog,
                at_ns: 42,
            })
        );
        assert_panic(handle.try_join(), "R4c 原 SystemdWatchdog 循环测试 panic");
        assert!(!handle.is_finished());
        assert!(matches!(handle.try_join(), JoinState::Pending));
        assert!(!control.arm(forbidden_clock));
    }

    #[test]
    fn unarmed_real_helpers_stop_normally_without_entered_fact() {
        let mut monitor = monitor();
        let monitor_control = monitor.test_panic_control();
        let (mut watchdog, entered_loop) = watchdog();
        let watchdog_control = watchdog.test_panic_control();
        entered_loop
            .recv_timeout(Duration::from_secs(5))
            .expect("真实 watchdog 主循环已开始");
        assert!(monitor_control.entered().is_none());
        assert!(watchdog_control.entered().is_none());
        monitor.request_stop();
        watchdog.request_stop();
        wait_finished(|| monitor.is_finished() && watchdog.is_finished());
        assert!(matches!(monitor.try_join(), JoinState::Joined(Ok(()))));
        assert!(matches!(watchdog.try_join(), JoinState::Joined(Ok(()))));
        assert!(monitor_control.entered().is_none());
        assert!(watchdog_control.entered().is_none());
    }

    #[test]
    fn same_kind_handle_controls_are_independent() {
        let mut selected = monitor();
        let mut peer = monitor();
        let selected_control = selected.test_panic_control();
        let peer_control = peer.test_panic_control();
        assert!(selected_control.arm(monitor_clock));
        wait_finished(|| selected.is_finished());
        assert_panic(selected.try_join(), "R4c 原 HealthMonitor 循环测试 panic");
        assert!(!peer.is_finished());
        assert!(peer_control.entered().is_none());
        peer.request_stop();
        wait_finished(|| peer.is_finished());
        assert!(matches!(peer.try_join(), JoinState::Joined(Ok(()))));
        assert!(peer_control.entered().is_none());
    }

    #[test]
    fn retained_controls_cannot_publish_or_extend_already_joined_helpers() {
        let mut monitor = monitor();
        let monitor_control = monitor.test_panic_control();
        let (mut watchdog, entered_loop) = watchdog();
        let watchdog_control = watchdog.test_panic_control();
        entered_loop
            .recv_timeout(Duration::from_secs(5))
            .expect("真实 watchdog 主循环已开始");
        monitor.request_stop();
        watchdog.request_stop();
        wait_finished(|| monitor.is_finished() && watchdog.is_finished());
        assert!(matches!(monitor.try_join(), JoinState::Joined(Ok(()))));
        assert!(matches!(watchdog.try_join(), JoinState::Joined(Ok(()))));
        drop(monitor);
        drop(watchdog);
        assert!(monitor_control.arm(forbidden_clock));
        assert!(watchdog_control.arm(forbidden_clock));
        assert!(monitor_control.entered().is_none());
        assert!(watchdog_control.entered().is_none());
    }
}
