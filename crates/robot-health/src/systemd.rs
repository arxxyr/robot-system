//! systemd `sd_notify(3)` 协议轻量实现 + **liveness-gated** watchdog 线程
//! （**不**依赖 `sd-notify` 第三方 crate）。
//!
//! **v0.5 候选 E 段(8)（Spec PD-E6(c) + R3-B2）**：自 `robot-s0`
//! `systemd_notify.rs`（v0.2 task 24-29 / v0.3 A2 端到端锁定）迁入本 crate，
//! 作 S0 / S1 / robot-rpc gateway 三 daemon 的**单一实现**（robot-health 定位
//! "非 RT、daemon 可复用"，tracing 依赖合法；S0 已依赖无循环）。S0 迁移传
//! `thread_name = "s0_systemd_wd"` 原值，判停语义等价；**ping 节奏经段(8) round 2
//! 修正**（首个 enforce 间隔，见下 "启动 grace"），不再宣称 bit 等价。
//!
//! ## 协议
//!
//! UNIX `SOCK_DGRAM`，messages 是 `KEY=VALUE\n` 多行文本，常用 key:
//! `READY=1` / `WATCHDOG=1` / `STOPPING=1` / `STATUS=<text>`。详
//! [`man:sd_notify(3)`](https://www.freedesktop.org/software/systemd/man/sd_notify.html)。
//!
//! ### 不在 systemd 下跑时的行为
//!
//! 环境变量 `NOTIFY_SOCKET` **未设置**或为空 → 所有 notify 函数 **no-op +
//! 返回 Ok**；`WATCHDOG_USEC` 未设置或 0 → [`spawn_watchdog`] 返 `None`。
//! daemon 在裸命令行 / runbook 调试 / smoke 子进程场景下与 systemd 部署形态
//! 零差异。
//!
//! ### Abstract socket 限制
//!
//! `NOTIFY_SOCKET` 以 `@` 开头（Linux abstract namespace）**当前不支持**，
//! silent skip。systemd 240+ 默认 path-based（`/run/systemd/notify`），
//! Ubuntu 22.04 / 24.04 / Tegra 实测全是 path-based。
//!
//! ## Watchdog（liveness-gated，`docs/00_design.md` §6.7 #2）
//!
//! systemd 在 unit 用 `WatchdogSec=N` 时把 `WATCHDOG_USEC` 注入环境；
//! [`spawn_watchdog`] 起 dedicated 线程按 `usec / 2` 周期评估
//! [`LivenessObserver`] 集合：**每个** observer 的 `tick` 都比上周期增长才发
//! `WATCHDOG=1`；任一停滞 → 本周期**不**发（warn 日志 `withholding
//! sd_notify(WATCHDOG=1)`）→ systemd `WatchdogSec` 超时 → `Restart=on-failure`
//! 拉起新 daemon。
//!
//! - **启动 grace**（`WatchdogConfig::startup_grace_ping`）：cycle 0（spawn 即刻）
//!   固定喂一次（无 observer 比较）并**同时**取初始 snapshot；cycle 1（一个
//!   `usec / 2` 间隔后）起每周期比较 → enforce。首个 enforce ping 距 grace ping
//!   恰 `usec / 2`，systemd 侧始终保有 50% 余量。**继承缺陷修正（段(8) round 2，
//!   CHANGELOG Reverse Sync）**：v0.2 S0 原实现 grace 后先 sleep 一个间隔才取
//!   snapshot、再 sleep 一个间隔才首次 enforce——首个间隔 = `WATCHDOG_USEC`
//!   整、无余量，健康进程启动期可能被 systemd 误杀。
//! - **间隔下限**：[`ping_interval`] = `max(usec / 2, 1 µs)`（`WATCHDOG_USEC = 1`
//!   极端配置下 `usec / 2 == 0` 会退化成 busy-loop）。
//! - **判停内核** [`evaluate_cycle`] 是纯函数（与 IO / 时序解耦）；主循环经
//!   [`WatchdogIo`] 注入 ping / sleep——生产 = sd_notify + 分片 `thread::sleep`
//!   （≤ 50 ms 一片、片间查 stop，**ping 节奏不变**、`stop()` 响应 ≤ 50 ms），
//!   单测 = 脚本化 IO 逐周期驱动（**PX-4**：撤 v0.2 "50 ms 墙钟窗口数
//!   datagram" 的时序测试，判停 / grace / 恢复全部确定性锁定）。
//! - **不在 RT 线程内调**：`UnixDatagram::send_to` 涉及 syscall。

#![allow(clippy::doc_markdown)]

use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

// ============================================================================
// 一次性 notify
// ============================================================================

/// `READY=1` —— daemon 完成全部初始化（所有 worker spawn + 主循环就绪），
/// `Type=notify` unit 才转 active + 解锁 `After=` 下游。**必须**在全部
/// observer ready 之后调一次（D12 第 3 条）。
pub fn notify_ready() {
    let _ = send_message("READY=1\n");
}

/// `WATCHDOG=1` —— ping 一次（重置 systemd watchdog 计时器）。一般由
/// [`spawn_watchdog`] 线程周期触发，不需要手调。
pub fn notify_watchdog() {
    let _ = send_message("WATCHDOG=1\n");
}

/// `STOPPING=1` —— 通知 systemd 进入 graceful shutdown（切 deactivating、停
/// watchdog timeout）。**先于** [`WatchdogHandle::stop`] 调用（D12 第 4 条）。
pub fn notify_stopping() {
    let _ = send_message("STOPPING=1\n");
}

/// `STATUS=<text>` —— 自由格式状态字符串（`systemctl status` 显示）。
pub fn notify_status(text: &str) {
    let payload = format!("STATUS={text}\n");
    let _ = send_message(&payload);
}

// ============================================================================
// LivenessObserver / WatchdogConfig / WatchdogHandle
// ============================================================================

/// Worker 线程 liveness 观察器：`label` 用于日志识别失活线程；`tick` 返回该
/// worker 当前单调 tick 计数（典型 = `Arc<XxxMetrics>.loop_tick_count`），
/// watchdog 每周期比较 snapshot 看是否增长。
///
/// **request-driven worker 必须观测独立 `loop_tick_count`**（空闲 poll 轮询
/// 也推进），**不**用请求计数作 liveness 源——否则合法空闲被判停滞
/// （Spec D12 r3 T4 第 1 条）。
pub struct LivenessObserver {
    pub label: &'static str,
    pub tick: Box<dyn Fn() -> u64 + Send>,
}

impl LivenessObserver {
    /// 用 `Arc<AtomicU64>` 作 tick 源。
    #[must_use]
    pub fn from_atomic(label: &'static str, tick: Arc<AtomicU64>) -> Self {
        Self {
            label,
            tick: Box::new(move || tick.load(Ordering::Relaxed)),
        }
    }

    /// 用任意闭包作 tick 源（捕获 `Arc<Metrics>` 读其字段）。
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

/// [`spawn_watchdog`] 配置（R3-B2：线程名撤硬编码改参数）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogConfig {
    /// watchdog 线程名（S0 = `"s0_systemd_wd"` 原值；S1 = `"s1_systemd_wd"`；
    /// gateway = `"gw_systemd_wd"`）。
    pub thread_name: String,
    /// 首周期无条件喂一次（沿 S0 既有 grace 语义）。
    pub startup_grace_ping: bool,
}

impl WatchdogConfig {
    /// `startup_grace_ping = true`（三 daemon 默认口径）。
    #[must_use]
    pub fn new(thread_name: impl Into<String>) -> Self {
        Self {
            thread_name: thread_name.into(),
            startup_grace_ping: true,
        }
    }
}

/// watchdog 线程句柄：[`Self::stop`] = 停喂 + join（消费）；`Drop` = 同款
/// best-effort（分片 sleep 保证 ≤ 50 ms 内响应）。
pub struct WatchdogHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl WatchdogHandle {
    /// 停止喂 ping 并 join 线程。调用方应**先** [`notify_stopping`] 再调本
    /// 方法（否则 systemd 仍按 `WatchdogSec` 计时，graceful 期间稍长会被误杀）。
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for WatchdogHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

// ============================================================================
// spawn
// ============================================================================

/// 起 liveness-gated watchdog 线程。读 `WATCHDOG_USEC` env 决定 ping 间隔
/// （= `usec / 2`）；未设置或 0 → `None`（非 systemd）。
///
/// observers 为空 → 退化成无条件 ping（兼容；生产三 daemon 均传全 observer）。
#[must_use = "返回 Option<WatchdogHandle> 调用方应保留以便 graceful shutdown 时 stop"]
pub fn spawn_watchdog(
    observers: Vec<LivenessObserver>,
    cfg: WatchdogConfig,
) -> Option<WatchdogHandle> {
    let usec: u64 = std::env::var("WATCHDOG_USEC").ok()?.parse().ok()?;
    spawn_watchdog_with_usec(observers, cfg, usec)
}

/// ping 间隔 = `usec / 2`（systemd 留 50% 余量吸收 jitter），下限 1 µs（极端
/// 配置 `WATCHDOG_USEC = 1` 时 `usec / 2 == 0` 会退化成 busy-loop）。
#[must_use]
pub fn ping_interval(usec: u64) -> Duration {
    Duration::from_micros((usec / 2).max(1))
}

/// 显式 `usec` 入参版（不读 env）：`spawn_watchdog` 经它实现；测试 / 嵌入
/// 场景直接用。`usec == 0` → `None`。
///
/// # Panics
///
/// `thread::Builder::spawn` 失败（OS 线程资源耗尽）——启动期致命，沿 S0 原
/// `expect` 口径。
#[must_use]
pub fn spawn_watchdog_with_usec(
    observers: Vec<LivenessObserver>,
    cfg: WatchdogConfig,
    usec: u64,
) -> Option<WatchdogHandle> {
    if usec == 0 {
        return None;
    }
    let interval = ping_interval(usec);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = Arc::clone(&stop);
    let WatchdogConfig {
        thread_name,
        startup_grace_ping: grace,
    } = cfg;
    let join = thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let mut io = SdNotifyIo;
            run_watchdog_loop(&stop_for_thread, interval, &observers, grace, &mut io);
        })
        .expect("spawn systemd watchdog thread");
    Some(WatchdogHandle {
        stop,
        join: Some(join),
    })
}

// ============================================================================
// 内核：evaluate_cycle（纯函数）+ WatchdogIo（IO 注入）+ 主循环
// ============================================================================

/// 单周期判停（纯函数）：对每个 observer 取当前 tick 与 `prev[i]` 比较——
/// 增长 → 更新 `prev[i]`；停滞 → warn 且**不**更新（下周期再看是否恢复）。
/// 返 `all_alive`（本周期是否应发 `WATCHDOG=1`）。
///
/// `prev.len() == observers.len()` 由调用方保证。
pub fn evaluate_cycle(observers: &[LivenessObserver], prev: &mut [u64]) -> bool {
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
        } else {
            prev[i] = now;
        }
    }
    all_alive
}

/// 主循环的 IO 面（ping / 周期 sleep），生产与单测各一实现。
pub trait WatchdogIo {
    /// 发一次 `WATCHDOG=1`（或记录）。
    fn ping(&mut self);
    /// 等待一个 ping 周期；期间观测到 `stop` → 返 `false`（调用方立即退出）。
    fn sleep_interval(&mut self, interval: Duration, stop: &AtomicBool) -> bool;
}

/// 生产 IO：`sd_notify(WATCHDOG=1)` + 分片 `thread::sleep`（每片 ≤
/// [`SdNotifyIo::SLICE`]，片间查 stop → `WatchdogHandle::stop` 响应 ≤ 50 ms；
/// 累计睡满 `interval` 才返，ping 节奏与整段 sleep 等价）。
pub struct SdNotifyIo;

impl SdNotifyIo {
    /// sleep 分片上限。
    pub const SLICE: Duration = Duration::from_millis(50);
}

impl WatchdogIo for SdNotifyIo {
    fn ping(&mut self) {
        notify_watchdog();
    }

    fn sleep_interval(&mut self, interval: Duration, stop: &AtomicBool) -> bool {
        let mut remaining = interval;
        while !remaining.is_zero() {
            if stop.load(Ordering::Acquire) {
                return false;
            }
            let slice_len = remaining.min(Self::SLICE);
            thread::sleep(slice_len);
            remaining -= slice_len;
        }
        !stop.load(Ordering::Acquire)
    }
}

/// 主循环：cycle 0 = `grace` 固定喂 + **即刻**取初始 snapshot；之后每周期
/// **先 sleep 一个间隔再比较**（cycle 1 起 enforce）。首个 enforce ping 距
/// grace ping 恰一个间隔。
///
/// **round 2 修正**：原先 grace 后 sleep 一个间隔才取 snapshot、再 sleep 一个
/// 间隔才首次比较——首个 ping 间隔 = 2 × `usec / 2` = `WATCHDOG_USEC` 整，
/// 注释宣称的 50% 余量不存在。即刻 snapshot 的前提 = `READY=1` 只在全部
/// observer 就绪后发（D12 第 3 条），此刻 worker 已在跑，cycle 1 前推进 ≥ 1
/// 次即视为存活。
pub fn run_watchdog_loop<IO: WatchdogIo>(
    stop: &AtomicBool,
    interval: Duration,
    observers: &[LivenessObserver],
    grace: bool,
    io: &mut IO,
) {
    if stop.load(Ordering::Acquire) {
        return;
    }
    if grace {
        io.ping();
    }
    let mut prev: Vec<u64> = observers.iter().map(|o| (o.tick)()).collect();

    while !stop.load(Ordering::Acquire) {
        if !io.sleep_interval(interval, stop) {
            break;
        }
        if evaluate_cycle(observers, &mut prev) {
            io.ping();
        }
    }
}

// ============================================================================
// 内部：socket 发送
// ============================================================================

/// 发送一条 notify 消息。读 `NOTIFY_SOCKET` env，未设置时 no-op。
fn send_message(payload: &str) -> std::io::Result<()> {
    let socket_path = match std::env::var("NOTIFY_SOCKET") {
        Ok(p) if !p.is_empty() => p,
        _ => return Ok(()), // 不在 systemd 下：no-op
    };
    send_message_to(payload, &socket_path)
}

/// 显式 `socket_path` 入参（**不读 env**，单测零 env 竞争）；abstract socket
/// (`@<name>`) silent skip 与 [`send_message`] 一致。
fn send_message_to(payload: &str, socket_path: &str) -> std::io::Result<()> {
    if socket_path.starts_with('@') {
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
    use std::time::Instant;

    // ------------------------------------------------------------------
    // 全局 env 串行化锁（沿 S0 原口径：多 test 并发改 NOTIFY_SOCKET 会互踩）
    // ------------------------------------------------------------------
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

    // ------------------------------------------------------------------
    // 确定性内核测试（PX-4：脚本化 IO 逐周期驱动，无墙钟窗口 / 无 socket）
    // ------------------------------------------------------------------

    /// 脚本化 IO：每次 `sleep_interval` = 一个周期；`on_cycle(n)` 让测试在
    /// 第 n 周期推进 / 冻结 observer；超过 `max_cycles` 返 false（= stop）。
    /// **每次 ping 记录所在 cycle**（round 2：锁 grace 在 cycle 0、首次 enforce
    /// 在 cycle 1，而非只数总次数）。
    struct ScriptedIo<F: FnMut(usize)> {
        pings: Vec<usize>,
        cycles: usize,
        max_cycles: usize,
        on_cycle: F,
    }

    impl<F: FnMut(usize)> WatchdogIo for ScriptedIo<F> {
        fn ping(&mut self) {
            self.pings.push(self.cycles);
        }
        fn sleep_interval(&mut self, _interval: Duration, _stop: &AtomicBool) -> bool {
            self.cycles += 1;
            if self.cycles > self.max_cycles {
                return false;
            }
            (self.on_cycle)(self.cycles);
            true
        }
    }

    /// 返回各 ping 所在 cycle（cycle 0 = spawn 即刻，cycle n = 第 n 个间隔后）。
    fn run_scripted<F: FnMut(usize)>(
        observers: &[LivenessObserver],
        grace: bool,
        max_cycles: usize,
        on_cycle: F,
    ) -> Vec<usize> {
        let stop = AtomicBool::new(false);
        let mut io = ScriptedIo {
            pings: Vec::new(),
            cycles: 0,
            max_cycles,
            on_cycle,
        };
        run_watchdog_loop(&stop, Duration::from_millis(1), observers, grace, &mut io);
        io.pings
    }

    #[test]
    fn evaluate_cycle_updates_prev_only_for_alive_observers() {
        let a = Arc::new(AtomicU64::new(10));
        let b = Arc::new(AtomicU64::new(20));
        let observers = vec![
            LivenessObserver::from_atomic("a", Arc::clone(&a)),
            LivenessObserver::from_atomic("b", Arc::clone(&b)),
        ];
        let mut prev = vec![10, 20];
        // 都停滞 → false，prev 不动
        assert!(!evaluate_cycle(&observers, &mut prev));
        assert_eq!(prev, vec![10, 20]);
        // a 推进、b 停滞 → false，仅 prev[0] 更新
        a.store(11, Ordering::Relaxed);
        assert!(!evaluate_cycle(&observers, &mut prev));
        assert_eq!(prev, vec![11, 20]);
        // 都推进 → true
        a.store(12, Ordering::Relaxed);
        b.store(21, Ordering::Relaxed);
        assert!(evaluate_cycle(&observers, &mut prev));
        assert_eq!(prev, vec![12, 21]);
    }

    #[test]
    fn all_alive_pings_grace_plus_every_enforce_cycle() {
        let t = Arc::new(AtomicU64::new(0));
        let observers = vec![LivenessObserver::from_atomic("w", Arc::clone(&t))];
        let bump = Arc::clone(&t);
        // cycle 0 grace + 即刻 snapshot；cycle 1..=5 各 evaluate 一次 → 5 enforce。
        let pings = run_scripted(&observers, true, 5, move |_| {
            bump.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(pings, vec![0, 1, 2, 3, 4, 5], "grace@0 + enforce@1..=5");
    }

    #[test]
    fn first_enforce_ping_lands_on_cycle_one_after_grace_on_cycle_zero() {
        // round 2 阻断 1 回归锁：observer 仅在 cycle 1 的间隔内推进一次，之后停滞。
        // 即刻 snapshot → cycle 1 看到推进 → ping@1；cycle 2+ 停滞 → withholding。
        // 旧实现（cycle 1 才取 snapshot）会把这次推进吞进 snapshot，永远不 enforce
        // ping（结果 [0]）——本测试即区分新旧时序。
        let t = Arc::new(AtomicU64::new(5));
        let observers = vec![LivenessObserver::from_atomic("w", Arc::clone(&t))];
        let bump = Arc::clone(&t);
        let pings = run_scripted(&observers, true, 4, move |cycle| {
            if cycle == 1 {
                bump.fetch_add(1, Ordering::Relaxed);
            }
        });
        assert_eq!(pings, vec![0, 1], "grace@0；首次 enforce 恰在 cycle 1");
    }

    #[test]
    fn ping_interval_is_half_usec_with_one_micro_floor() {
        assert_eq!(ping_interval(2_000_000), Duration::from_secs(1));
        assert_eq!(ping_interval(4_000_000), Duration::from_secs(2));
        assert_eq!(ping_interval(3), Duration::from_micros(1));
        assert_eq!(
            ping_interval(1),
            Duration::from_micros(1),
            "usec/2 == 0 → 下限 1 µs"
        );
    }

    #[test]
    fn stalled_observer_withholds_all_enforce_pings() {
        let t = Arc::new(AtomicU64::new(42));
        let observers = vec![LivenessObserver::from_atomic("dead", Arc::clone(&t))];
        // tick 永不推进（模拟 worker 死锁）→ 只有 grace ping。
        let pings = run_scripted(&observers, true, 6, |_| {});
        assert_eq!(pings, vec![0], "仅 grace@0；enforce 周期全部 withholding");
    }

    #[test]
    fn one_stalled_among_many_withholds_and_recovery_resumes() {
        let a = Arc::new(AtomicU64::new(0));
        let b = Arc::new(AtomicU64::new(0));
        let observers = vec![
            LivenessObserver::from_atomic("a", Arc::clone(&a)),
            LivenessObserver::from_atomic("b", Arc::clone(&b)),
        ];
        let (ba, bb) = (Arc::clone(&a), Arc::clone(&b));
        // a 每周期推进；b 在 cycle 3、4 停滞（其余推进）。
        // cycle 0 grace + snapshot；1 → ping；2 → ping；3 → b 停 → 无；4 → b 停
        // → 无；5 → b 恢复 → ping；6 → ping。
        let pings = run_scripted(&observers, true, 6, move |cycle| {
            ba.fetch_add(1, Ordering::Relaxed);
            if !(3..=4).contains(&cycle) {
                bb.fetch_add(1, Ordering::Relaxed);
            }
        });
        assert_eq!(pings, vec![0, 1, 2, 5, 6]);
    }

    #[test]
    fn grace_disabled_skips_first_unconditional_ping() {
        let t = Arc::new(AtomicU64::new(0));
        let observers = vec![LivenessObserver::from_atomic("w", Arc::clone(&t))];
        let bump = Arc::clone(&t);
        let pings = run_scripted(&observers, false, 5, move |_| {
            bump.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(pings, vec![1, 2, 3, 4, 5], "无 grace：仅 enforce@1..=5");
    }

    #[test]
    fn no_observers_degrades_to_unconditional_ping() {
        let pings = run_scripted(&[], true, 3, |_| {});
        assert_eq!(
            pings,
            vec![0, 1, 2, 3],
            "grace@0 + enforce@1..=3（空 observer 集恒 alive）"
        );
    }

    #[test]
    fn stop_during_initial_sleep_exits_without_evaluation() {
        struct StopIo(usize);
        impl WatchdogIo for StopIo {
            fn ping(&mut self) {
                self.0 += 1;
            }
            fn sleep_interval(&mut self, _: Duration, _: &AtomicBool) -> bool {
                false
            }
        }
        let stop = AtomicBool::new(false);
        let mut io = StopIo(0);
        run_watchdog_loop(&stop, Duration::from_millis(1), &[], true, &mut io);
        assert_eq!(
            io.0, 1,
            "grace ping + 即刻 snapshot 后首个 sleep 即 stop → 退出"
        );
    }

    #[test]
    fn stop_set_before_start_pings_nothing() {
        struct CountIo(usize);
        impl WatchdogIo for CountIo {
            fn ping(&mut self) {
                self.0 += 1;
            }
            fn sleep_interval(&mut self, _: Duration, _: &AtomicBool) -> bool {
                true
            }
        }
        let stop = AtomicBool::new(true);
        let mut io = CountIo(0);
        run_watchdog_loop(&stop, Duration::from_millis(1), &[], true, &mut io);
        assert_eq!(io.0, 0, "spawn 前已 stop → 连 grace 都不喂");
    }

    // ------------------------------------------------------------------
    // 真线程生命周期（无时序断言，仅锁 stop 响应上界）
    // ------------------------------------------------------------------

    #[test]
    fn spawn_with_zero_usec_returns_none() {
        assert!(spawn_watchdog_with_usec(vec![], WatchdogConfig::new("t"), 0).is_none());
    }

    #[test]
    fn stop_responds_within_slice_even_with_long_interval() {
        // usec = 20 s → interval 10 s；分片 sleep 让 stop 在 ≤ 50 ms 级响应。
        let h = spawn_watchdog_with_usec(vec![], WatchdogConfig::new("t_long"), 20_000_000)
            .expect("usec > 0");
        thread::sleep(Duration::from_millis(20));
        let t0 = Instant::now();
        h.stop();
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "stop() 应在分片粒度内返回，实际 {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn drop_stops_thread_best_effort() {
        let h = spawn_watchdog_with_usec(vec![], WatchdogConfig::new("t_drop"), 20_000_000)
            .expect("usec > 0");
        let t0 = Instant::now();
        drop(h);
        assert!(t0.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn config_new_defaults_grace_on() {
        let c = WatchdogConfig::new("s0_systemd_wd");
        assert_eq!(c.thread_name, "s0_systemd_wd");
        assert!(c.startup_grace_ping);
    }

    // ------------------------------------------------------------------
    // socket 行为（不动 env，零 race）
    // ------------------------------------------------------------------

    #[test]
    fn send_message_to_writes_ready_payload() {
        let (_dir, sock_path, listener) = bind_test_socket();
        send_message_to("READY=1\n", sock_path.to_str().unwrap()).unwrap();
        let mut buf = [0u8; 64];
        let (n, _) = listener.recv_from(&mut buf).expect("recv READY");
        assert_eq!(&buf[..n], b"READY=1\n");
    }

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

    #[test]
    fn send_message_to_abstract_silent_skip() {
        send_message_to("READY=1\n", "@/tmp/nonexistent.sock").unwrap();
    }

    #[test]
    fn sd_notify_io_sleep_interval_honors_stop_mid_way() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_setter = Arc::clone(&stop);
        let setter = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            stop_setter.store(true, Ordering::Release);
        });
        let mut io = SdNotifyIo;
        let t0 = Instant::now();
        let alive = io.sleep_interval(Duration::from_secs(10), &stop);
        assert!(!alive, "stop 置位后 sleep_interval 必须返 false");
        assert!(t0.elapsed() < Duration::from_secs(1));
        setter.join().unwrap();
    }

    // ------------------------------------------------------------------
    // env 探测（ENV_LOCK 串行化 + 显式清理）
    // ------------------------------------------------------------------

    #[test]
    fn no_notify_socket_silent_noop() {
        let _guard = lock_env();
        // SAFETY: env mutation 在 ENV_LOCK 锁定下串行。
        unsafe {
            std::env::remove_var("NOTIFY_SOCKET");
        }
        notify_ready();
        notify_watchdog();
        notify_stopping();
        notify_status("test");
    }

    #[test]
    fn abstract_socket_silent_skip_via_public_api() {
        let _guard = lock_env();
        // SAFETY: env mutation 在 ENV_LOCK 锁定下串行。
        unsafe {
            std::env::set_var("NOTIFY_SOCKET", "@/tmp/abstract.sock");
        }
        notify_ready();
        notify_watchdog();
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("NOTIFY_SOCKET");
        }
    }

    #[test]
    fn ready_via_env_round_trip() {
        let _guard = lock_env();
        let (_dir, sock_path, listener) = bind_test_socket();
        // SAFETY: env mutation 在 ENV_LOCK 锁定下串行。
        unsafe {
            std::env::set_var("NOTIFY_SOCKET", &sock_path);
        }
        notify_ready();
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("NOTIFY_SOCKET");
        }
        let mut buf = [0u8; 64];
        let (n, _) = listener.recv_from(&mut buf).expect("recv READY");
        assert_eq!(&buf[..n], b"READY=1\n");
    }

    #[test]
    fn grace_ping_reaches_socket_via_real_thread() {
        // 真线程 + 真 socket，但只断言 grace ping **到达**（有界 recv 超时），
        // 不数窗口内 datagram 数（PX-4 撤时序窗口）。
        let _guard = lock_env();
        let (_dir, sock_path, listener) = bind_test_socket();
        // SAFETY: env mutation 在 ENV_LOCK 锁定下串行。
        unsafe {
            std::env::set_var("NOTIFY_SOCKET", &sock_path);
        }
        let stalled = Arc::new(AtomicU64::new(7));
        let h = spawn_watchdog_with_usec(
            vec![LivenessObserver::from_atomic("dead", stalled)],
            WatchdogConfig::new("t_grace"),
            20_000_000,
        )
        .expect("usec > 0");
        let mut buf = [0u8; 32];
        let (n, _) = listener.recv_from(&mut buf).expect("grace ping 必须到达");
        assert_eq!(&buf[..n], b"WATCHDOG=1\n");
        h.stop();
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("NOTIFY_SOCKET");
        }
    }

    #[test]
    fn spawn_watchdog_returns_none_without_or_zero_usec() {
        let _guard = lock_env();
        // SAFETY: env mutation 在 ENV_LOCK 锁定下串行。
        unsafe {
            std::env::remove_var("WATCHDOG_USEC");
        }
        assert!(spawn_watchdog(vec![], WatchdogConfig::new("t")).is_none());
        // SAFETY: 同上。
        unsafe {
            std::env::set_var("WATCHDOG_USEC", "0");
        }
        assert!(spawn_watchdog(vec![], WatchdogConfig::new("t")).is_none());
        // SAFETY: 同上。
        unsafe {
            std::env::remove_var("WATCHDOG_USEC");
        }
    }
}
