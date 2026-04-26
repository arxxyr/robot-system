//! systemd `sd_notify(3)` 协议轻量实现（**不**依赖 `sd-notify` 第三方 crate）。
//!
//! 协议：UNIX `SOCK_DGRAM`，messages 是 `KEY=VALUE\n` 多行文本，常用 key:
//! `READY=1` / `WATCHDOG=1` / `STOPPING=1` / `STATUS=<text>`。详
//! [`man:sd_notify(3)`](https://www.freedesktop.org/software/systemd/man/sd_notify.html)。
//!
//! ### 不在 systemd 下跑时的行为
//!
//! 当环境变量 `NOTIFY_SOCKET` **未设置**或为空字符串时，所有 notify 函数
//! 都 **no-op + 返回 Ok**。这样 `s0_daemon` 在裸命令行 / runbook 调试 /
//! crash_restart_smoke 等场景下都跑得通，跟 systemd 部署形态零差异。
//!
//! ### Abstract socket 限制
//!
//! 如果 `NOTIFY_SOCKET` 以 `@` 开头（Linux abstract namespace socket），本
//! 实现**当前不支持**，notify 会 silent skip。systemd 240+ 默认走 path-based
//! socket（`/run/systemd/notify`），实测 Ubuntu 22.04 / 24.04 / Tegra 全是
//! path-based，所以暂不阻塞。要支持 abstract 走 `libc::sendto` + 手撸
//! `sockaddr_un`（前置 `\0` 字节），可后续再加。
//!
//! ### Watchdog
//!
//! systemd 在 unit 用 `WatchdogSec=2s` 时把 `WATCHDOG_USEC=2000000` 注入
//! 环境。daemon 应每 `WATCHDOG_USEC / 2` 微秒调一次 `watchdog()`；不够及时
//! systemd 视为僵死，按 `Restart=` 策略重启。
//!
//! [`spawn_watchdog_thread`] 启动 dedicated 线程做这件事，传入 `stop` 原子
//! 让 daemon graceful shutdown 时停止 ping。

#![allow(clippy::doc_markdown)]

use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

// ============================================================================
// 一次性 notify
// ============================================================================

/// `READY=1` —— daemon 完成所有初始化（4 线程都 spawn + 进入主循环），systemd
/// 才认为 service "active"。`Type=notify` unit 必须收到 READY=1 才解锁
/// `After=` 链路上的下游 unit。
///
/// **必须**在所有 thread spawn + 初始 state 落地之后调一次。
pub fn ready() {
    let _ = send_message("READY=1\n");
}

/// `WATCHDOG=1` —— ping 一次。systemd 收到即重置 watchdog 计时器；
/// `WatchdogSec=2s` unit 不在 2s 内收到下次 ping 即按 `Restart=on-failure`
/// 处理。一般由 [`spawn_watchdog_thread`] 周期触发，不需要手调。
pub fn watchdog() {
    let _ = send_message("WATCHDOG=1\n");
}

/// `STOPPING=1` —— 通知 systemd 进入 graceful shutdown，给后续 SIGTERM /
/// timeout 流程一个明确"我在收尾"的语义。systemd 收到后切到 deactivating
/// 状态、停发 watchdog timeout 等。**只**在 daemon 主动退出时调一次。
pub fn stopping() {
    let _ = send_message("STOPPING=1\n");
}

/// `STATUS=<text>` —— 自由格式状态字符串，`systemctl status` 会显示。
/// 可用于"current state: Enabled.Active / Safe / Stopping" 这种摘要。
pub fn status(text: &str) {
    let payload = format!("STATUS={text}\n");
    let _ = send_message(&payload);
}

// ============================================================================
// watchdog 周期 ping 线程（liveness-gated）
// ============================================================================

/// Worker thread liveness 观察器。`label` 用于日志识别失活线程；`tick` 闭包
/// 返回该 worker 当前 monotonic tick 计数（一般是 `Arc<XxxMetrics>` 里的
/// `tick_count.load(Relaxed)`），watchdog 周期间比较 snapshot 看是否增长。
///
/// **设计**（reviewer 在 commit 8e5a238 之后阻断 1 提出）：原 watchdog 线
/// 程无条件 `sd_notify(WATCHDOG=1)`，只要本线程还活就喂 systemd —— RT /
/// SerialRT / ipc / pub 任意 worker 卡死也照样喂，等价于禁用 watchdog。
/// `docs/00_design.md:1134` 要求"内部 RT watchdog 失效 → sd_notify 超时
/// → systemd 重启 S0"。改成"观测 worker tick 才喂"才能真把 worker 卡死
/// 暴露给 systemd。
pub struct LivenessObserver {
    pub label: &'static str,
    pub tick: Box<dyn Fn() -> u64 + Send>,
}

impl LivenessObserver {
    /// 用 `Arc<AtomicU64>` 作 tick 源（最常见）。捕获所有 `XxxMetrics` 里
    /// 的 `tick_count` 字段。
    #[must_use]
    pub fn from_atomic(label: &'static str, tick: Arc<std::sync::atomic::AtomicU64>) -> Self {
        Self {
            label,
            tick: Box::new(move || tick.load(Ordering::Relaxed)),
        }
    }

    /// 用任意闭包作 tick 源（`Arc<Metrics>` 里读 plain `AtomicU64` 字段时
    /// 用这个：捕获 `Arc<Metrics>` 然后 `metrics.tick_count.load(...)`）。
    #[must_use]
    pub fn from_fn<F>(label: &'static str, f: F) -> Self
    where
        F: Fn() -> u64 + Send + 'static,
    {
        Self {
            label,
            tick: Box::new(f),
        }
    }
}

/// 启动 dedicated watchdog ping 线程。读 `WATCHDOG_USEC` env 决定 ping
/// 间隔（= `usec / 2`），停在 `stop` 原子被 release 时。
///
/// **liveness-gated 语义**（reviewer round 4 阻断 1 修复，§6.7 #2 实施）：
/// 每 ping 周期对 `observers` 里所有 `tick` 闭包调一次 + 跟上次 snapshot
/// 比较；**任一**观察器停滞（值未增长）→ 本周期**不**发
/// `sd_notify(WATCHDOG=1)`。systemd 收不到 ping → `WatchdogSec` 超时 →
/// `Restart=on-failure` 拉起新 daemon。这把 RT/SerialRT/ipc/pub 任意 worker
/// 死锁 / busy-loop / panic-after-handler 等场景接到 systemd 重启路径。
///
/// **启动 grace**：第一个 ping 周期固定喂一次（无 observer 比较），让
/// daemon 4 个 worker 完成 spawn + first tick；之后才进 enforce 模式。
///
/// **不在 systemd 下跑时**（`WATCHDOG_USEC` 未设置或 = 0）返回 `None`。
/// observers 数量为 0 时退化成无条件 ping（兼容；非 systemd 路径用不到）。
///
/// **不在 RT 线程内调**：内部 `UnixDatagram::send_to` 涉及 syscall + 内核
/// 队列写，违反 §0 / §4.9 RT 铁律。本线程是普通优先级 non-RT 工作线程。
#[must_use = "返回 Option<JoinHandle> 调用方应保留以便 graceful shutdown 时 join"]
pub fn spawn_watchdog_thread(
    stop: Arc<AtomicBool>,
    observers: Vec<LivenessObserver>,
) -> Option<JoinHandle<()>> {
    let usec: u64 = std::env::var("WATCHDOG_USEC").ok()?.parse().ok()?;
    spawn_watchdog_thread_with_usec(stop, usec, observers)
}

/// 内部 helper：拿 `usec` 显式入参，不读 env。`spawn_watchdog_thread` 通过它
/// 实现，单测也通过它直接验证（避免 env mutation 并发竞争）。
fn spawn_watchdog_thread_with_usec(
    stop: Arc<AtomicBool>,
    usec: u64,
    observers: Vec<LivenessObserver>,
) -> Option<JoinHandle<()>> {
    if usec == 0 {
        return None;
    }
    // ping 间隔 = usec / 2，给 systemd 留 50% 余量吸收 jitter。
    let interval = Duration::from_micros(usec / 2);
    let handle = thread::Builder::new()
        .name("s0_systemd_wd".into())
        .spawn(move || run_watchdog_loop(&stop, interval, &observers))
        .expect("spawn s0_systemd_wd thread");
    Some(handle)
}

/// 主循环：第一周期固定喂（grace），之后每周期对所有 observer 比较。
fn run_watchdog_loop(stop: &AtomicBool, interval: Duration, observers: &[LivenessObserver]) {
    // 第一周期：grace ping，让 4 个 worker 完成 spawn + first tick 增量。
    if !stop.load(Ordering::Acquire) {
        watchdog();
    }
    // 初始 snapshot：第一个 grace 间隔之后再读，避免 daemon 启动期 worker
    // 还没跑第一个 tick 就被判停滞。
    thread::sleep(interval);
    let mut prev: Vec<u64> = observers.iter().map(|o| (o.tick)()).collect();

    // **关键**：每次 iteration 先 sleep 一个 interval **再** compare。如果
    // 反过来（snapshot prev 之后立刻 compare）第一次 iteration 必然
    // now == prev = stalled 误报，因为 0 时间过去。
    while !stop.load(Ordering::Acquire) {
        thread::sleep(interval);
        if stop.load(Ordering::Acquire) {
            break;
        }
        let mut all_alive = true;
        for (i, observer) in observers.iter().enumerate() {
            let now = (observer.tick)();
            if now == prev[i] {
                tracing::warn!(
                    thread = observer.label,
                    prev = prev[i],
                    now,
                    "watchdog: worker tick stalled, withholding sd_notify(WATCHDOG=1)"
                );
                all_alive = false;
                // 不更新 prev[i]：下个周期看是否恢复（值再次 == 还是停 vs 跳了一段值）
            } else {
                prev[i] = now;
            }
        }
        if all_alive {
            watchdog();
        }
    }
}

// ============================================================================
// 内部
// ============================================================================

/// 发送一条 notify 消息。读 `NOTIFY_SOCKET` env，未设置时 no-op。
///
/// 内部 dispatch 到 [`send_message_to`]：测试可以直接调 `send_message_to`
/// 验证 socket 行为，**不动 env**，避免单测之间并发竞争（详 reviewer
/// 复验：`--test-threads=8` 循环 20 轮第 7 轮 status_payload_formatting
/// 收不到 datagram；root cause = 多 test 同时改 NOTIFY_SOCKET）。
fn send_message(payload: &str) -> std::io::Result<()> {
    let socket_path = match std::env::var("NOTIFY_SOCKET") {
        Ok(p) if !p.is_empty() => p,
        _ => return Ok(()), // 不在 systemd 下：no-op
    };
    send_message_to(payload, &socket_path)
}

/// 内部 helper：拿 `socket_path` 显式入参，**不读 env**。abstract socket
/// (`@<name>`) silent skip 跟 [`send_message`] 一致。
fn send_message_to(payload: &str, socket_path: &str) -> std::io::Result<()> {
    if socket_path.starts_with('@') {
        // Abstract namespace socket（Linux 扩展）。本实现暂不支持；systemd
        // 240+ 默认 path-based，主流环境无影响。详 module-level 文档。
        return Ok(());
    }
    let sock = UnixDatagram::unbound()?;
    sock.send_to(payload.as_bytes(), socket_path).map(drop)
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;
    use std::sync::Mutex;

    // ========================================================================
    // 全局 env 串行化锁
    // ========================================================================
    //
    // **背景**（reviewer 复验，commit 8e5a238 后阻断）：本模块原始测试都直接
    // 改全局 `NOTIFY_SOCKET` / `WATCHDOG_USEC` env。Cargo 默认并行测试
    // (`--test-threads=N`)，多个测试同时改同一个 env → A 设的 NOTIFY_SOCKET
    // 被 B 覆盖 / 移除，A 的 send_message 落到 B 的 socket，A 收不到 datagram。
    // 用户实测 `cargo test -p robot-s0 systemd_notify -- --test-threads=8`
    // 循环 20 轮第 7 轮 `status_payload_formatting` 失败。
    //
    // **修复**（双管齐下）：
    // 1. 拆出 [`send_message_to`] / [`spawn_watchdog_thread_with_usec`] 内部
    //    helper（不读 env），socket 行为测试改走 helper，零 env 竞争。
    // 2. 仍要测 env 探测的少量 case（`no_notify_socket_silent_noop` /
    //    `abstract_socket_silent_skip` / `watchdog_thread_returns_none_without_usec`
    //    / `ready_via_env_round_trip`）通过 `ENV_LOCK` 全局 Mutex 串行化。
    //
    // PoisonError handle: 任一 test panic 时锁中毒，`.into_inner()` 直接拿
    // guard。每个 test 独立设/清 env，不依赖前一 test 的状态，可接受。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn bind_test_socket() -> (tempfile::TempDir, std::path::PathBuf, UnixDatagram) {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("notify.sock");
        let listener = UnixDatagram::bind(&sock_path).unwrap();
        listener
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        (dir, sock_path, listener)
    }

    // ========================================================================
    // socket 行为测试：不动 env，零 race
    // ========================================================================

    /// `READY=1\n` payload 通过 `send_message_to` 真送到 UNIX datagram socket。
    #[test]
    fn send_message_to_writes_ready_payload() {
        let (_dir, sock_path, listener) = bind_test_socket();
        send_message_to("READY=1\n", sock_path.to_str().unwrap()).unwrap();

        let mut buf = [0u8; 64];
        let (n, _) = listener.recv_from(&mut buf).expect("recv READY");
        assert_eq!(&buf[..n], b"READY=1\n");
    }

    /// `STATUS=<text>\n` payload 拼接 + 通过 `send_message_to` 送出。
    #[test]
    fn send_message_to_status_payload_formatting() {
        let (_dir, sock_path, listener) = bind_test_socket();
        let payload = format!("STATUS={}\n", "Enabled.Active (sub: Active, link: Ok)");
        send_message_to(&payload, sock_path.to_str().unwrap()).unwrap();

        let mut buf = [0u8; 256];
        let (n, _) = listener.recv_from(&mut buf).unwrap();
        assert_eq!(
            std::str::from_utf8(&buf[..n]).unwrap(),
            "STATUS=Enabled.Active (sub: Active, link: Ok)\n"
        );
    }

    /// `send_message_to` 收到 abstract socket 路径（`@` 开头）silent skip。
    #[test]
    fn send_message_to_abstract_silent_skip() {
        // 给一个不存在的 abstract socket 路径，应当 Ok 而不是 NotFound err。
        send_message_to("READY=1\n", "@/tmp/nonexistent.sock").unwrap();
    }

    /// `WATCHDOG_USEC=0` 等价 → 返 None（避免 0 间隔 busy-loop）。
    /// 走内部 helper 不动 env。
    #[test]
    fn spawn_watchdog_with_zero_usec_returns_none() {
        let stop = Arc::new(AtomicBool::new(false));
        assert!(spawn_watchdog_thread_with_usec(stop, 0, vec![]).is_none());
    }

    /// `usec > 0` 真 spawn 出线程；`stop` 原子设置后线程立刻退出。
    #[test]
    fn spawn_watchdog_with_positive_usec_spawns_and_exits_on_stop() {
        let stop = Arc::new(AtomicBool::new(false));
        let h = spawn_watchdog_thread_with_usec(Arc::clone(&stop), 10_000_000, vec![])
            .expect("usec > 0 应当 spawn 出线程");
        // 不真 ping（NOTIFY_SOCKET 未设置 silent no-op），只验线程能停。
        std::thread::sleep(Duration::from_millis(10));
        stop.store(true, Ordering::Release);
        h.join().expect("watchdog thread should join cleanly");
    }

    // ========================================================================
    // liveness-gated watchdog 锁测（reviewer round 4 阻断 1）
    // ========================================================================

    /// 端到端：所有 observer tick 都增长 → watchdog 每周期都 ping。
    /// 用 NOTIFY_SOCKET 临时 socket + ENV_LOCK 串行化（小心 env race）。
    #[test]
    fn watchdog_pings_when_all_observers_alive() {
        use std::sync::atomic::AtomicU64;
        let _guard = lock_env();
        let (_dir, sock_path, listener) = bind_test_socket();

        let tick_a = Arc::new(AtomicU64::new(0));
        let tick_b = Arc::new(AtomicU64::new(0));
        let observers = vec![
            LivenessObserver::from_atomic("worker_a", Arc::clone(&tick_a)),
            LivenessObserver::from_atomic("worker_b", Arc::clone(&tick_b)),
        ];

        // SAFETY: env mutation 在 ENV_LOCK 锁定下串行。
        unsafe {
            std::env::set_var("NOTIFY_SOCKET", &sock_path);
        }
        let stop = Arc::new(AtomicBool::new(false));
        // usec=20ms → interval=10ms：grace ping 后 sleep 10ms snapshot；接下来
        // 每 10ms 一周期。我们让 worker 持续 increment，验证 ping 到达。
        let h = spawn_watchdog_thread_with_usec(Arc::clone(&stop), 20_000, observers)
            .expect("usec > 0");

        // 起一个"心跳"线程持续 increment（模拟 worker 活着）。
        let bumper_stop = Arc::new(AtomicBool::new(false));
        let bumper_stop_clone = Arc::clone(&bumper_stop);
        let bumper_a = Arc::clone(&tick_a);
        let bumper_b = Arc::clone(&tick_b);
        let bumper_handle = std::thread::spawn(move || {
            while !bumper_stop_clone.load(Ordering::Acquire) {
                bumper_a.fetch_add(1, Ordering::Relaxed);
                bumper_b.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(2));
            }
        });

        // 收集 ping：grace 1 + 至少 enforce 模式下 2 周期（共 ≥ 3 ping）
        let mut count = 0;
        let mut buf = [0u8; 32];
        for _ in 0..5 {
            if listener.recv_from(&mut buf).is_ok() {
                count += 1;
            }
        }

        bumper_stop.store(true, Ordering::Release);
        let _ = bumper_handle.join();
        stop.store(true, Ordering::Release);
        let _ = h.join();
        // SAFETY: 同上，cleanup env。
        unsafe {
            std::env::remove_var("NOTIFY_SOCKET");
        }

        assert!(
            count >= 3,
            "expected >= 3 pings (1 grace + ≥ 2 enforce), got {count}"
        );
    }

    /// 端到端：observer tick **停滞** → watchdog 跳过 ping（grace 1 之后就
    /// 停止 ping）。模拟 worker thread 死锁场景。
    #[test]
    fn watchdog_withholds_ping_when_observer_stalled() {
        use std::sync::atomic::AtomicU64;
        let _guard = lock_env();
        let (_dir, sock_path, listener) = bind_test_socket();

        let tick = Arc::new(AtomicU64::new(42));
        // tick 永远不更新 → 模拟死锁的 worker thread。
        let observers = vec![LivenessObserver::from_atomic(
            "dead_worker",
            Arc::clone(&tick),
        )];

        // SAFETY: env mutation 在 ENV_LOCK 锁定下串行。
        unsafe {
            std::env::set_var("NOTIFY_SOCKET", &sock_path);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let h = spawn_watchdog_thread_with_usec(Arc::clone(&stop), 20_000, observers)
            .expect("usec > 0");

        // 应该只收到 grace ping（第一个），之后 enforce 模式下 observer 停滞
        // 应一直跳过 ping。等 5 个 interval (~50 ms) 看共收到几次。
        let mut count = 0;
        let mut buf = [0u8; 32];
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_millis(50) {
            // 短超时，免得拖慢 test
            if listener.recv_from(&mut buf).is_ok() {
                count += 1;
            }
        }

        stop.store(true, Ordering::Release);
        let _ = h.join();
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("NOTIFY_SOCKET");
        }

        // grace 1 ping 是预期的；enforce 模式不该再有。**不能** = 0 因为 grace
        // 始终发；**不能** > 1 因为之后 observer 停滞跳过。
        assert_eq!(
            count, 1,
            "expected exactly 1 ping (grace only, then withhold on stall), got {count}"
        );
    }

    // ========================================================================
    // env 探测测试：必须串行化（`ENV_LOCK`）+ 显式清理 env
    // ========================================================================

    /// `NOTIFY_SOCKET` 未设置 → ready/watchdog/stopping/status 都 no-op + Ok。
    #[test]
    fn no_notify_socket_silent_noop() {
        let _guard = lock_env();
        // SAFETY: env mutation 在 `ENV_LOCK` 锁定下与其它 test 串行。
        unsafe {
            std::env::remove_var("NOTIFY_SOCKET");
        }
        // 不应 panic 也不应 Err（dev runbook 不在 systemd 下跑同款）。
        ready();
        watchdog();
        stopping();
        status("test");
    }

    /// `NOTIFY_SOCKET="@..."` → 公开 API 走 abstract silent skip 路径。
    #[test]
    fn abstract_socket_silent_skip_via_public_api() {
        let _guard = lock_env();
        // SAFETY: env mutation 在 `ENV_LOCK` 锁定下串行。
        unsafe {
            std::env::set_var("NOTIFY_SOCKET", "@/tmp/abstract.sock");
        }
        ready();
        watchdog();
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("NOTIFY_SOCKET");
        }
    }

    /// 端到端：公开 `ready()` 真把 `READY=1` 写到 NOTIFY_SOCKET 指向的
    /// socket（验证 env 探测路径完整）。
    #[test]
    fn ready_via_env_round_trip() {
        let _guard = lock_env();
        let (_dir, sock_path, listener) = bind_test_socket();
        // SAFETY: env mutation 在 `ENV_LOCK` 锁定下串行。
        unsafe {
            std::env::set_var("NOTIFY_SOCKET", &sock_path);
        }
        ready();
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("NOTIFY_SOCKET");
        }

        let mut buf = [0u8; 64];
        let (n, _) = listener.recv_from(&mut buf).expect("recv READY");
        assert_eq!(&buf[..n], b"READY=1\n");
    }

    /// `WATCHDOG_USEC` 未设置 → `spawn_watchdog_thread`（**走 env**）返 None。
    #[test]
    fn watchdog_thread_returns_none_without_usec() {
        let _guard = lock_env();
        // SAFETY: env mutation 在 `ENV_LOCK` 锁定下串行。
        unsafe {
            std::env::remove_var("WATCHDOG_USEC");
        }
        let stop = Arc::new(AtomicBool::new(false));
        assert!(spawn_watchdog_thread(stop, vec![]).is_none());
    }

    /// `WATCHDOG_USEC=0` → `spawn_watchdog_thread` 走 env 路径也返 None。
    #[test]
    fn watchdog_thread_returns_none_with_zero_usec_via_env() {
        let _guard = lock_env();
        // SAFETY: env mutation 在 `ENV_LOCK` 锁定下串行。
        unsafe {
            std::env::set_var("WATCHDOG_USEC", "0");
        }
        let stop = Arc::new(AtomicBool::new(false));
        assert!(spawn_watchdog_thread(stop, vec![]).is_none());
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("WATCHDOG_USEC");
        }
    }
}
