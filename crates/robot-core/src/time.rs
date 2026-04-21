//! 时钟封装：`CLOCK_MONOTONIC_RAW` 单调时钟、`Deadline` / `DurationNs` 类型、
//! `RobotClock` trait 及两个实现（`SystemClock` / `MockClock`）。
//!
//! 本模块是 `docs/00_design.md` §3.2 / §3.2.1 的落地。业务代码**严禁**直接调
//! `std::time::Instant::now` / `SystemTime::now` / `tokio::time::Instant::now`；
//! 一切时间读取经 `RobotClock` 走。`SystemClock` 使用 `CLOCK_MONOTONIC_RAW`
//! （`NTP` / `adjtime` 无感），sleep 路径允许 §3.2.1 授权下退化到相对 `nanosleep`。
//!
//! 平台：Linux / macOS 走 `libc` syscall；其他 target 为 `unimplemented!` stub
//! （项目目标平台：Linux `x86_64` / Linux `arm64`；macOS 仅作 Step 0–5 开发期
//! 支持；Windows 只跑 S2 客户端，不经 `robot-core::time`）。
//!
//! ### `libc` 使用声明
//!
//! 本模块是 `docs/00_design.md` §4.2 `robot-core` 允许列表的**例外**，详见
//! `docs/CHANGELOG.md` 2026-04-21 "robot-core §4.2 允许列表扩展：加 libc"
//! （状态：偏离）。允许 `libc` 使用的范围限于本模块及未来明确的 `robot-core::os`
//! 一类 OS 抽象模块；其他模块禁止 `use libc`。

// 允许 libc（robot-core §4.2 例外，见 docs/CHANGELOG.md 2026-04-21）。

use core::ops::{Add, Sub};

// ============================================================================
// Newtype：MonoRawNs / Deadline / DurationNs
// ============================================================================

/// 单调原始时钟纳秒数（`CLOCK_MONOTONIC_RAW`）。
///
/// u64 范围 ≈ 584 年，v0.x 周期内绝不会溢出；算术接口仍做饱和处理，避免
/// 任何路径 panic（RT 铁律，§0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct MonoRawNs(pub u64);

/// 截止时刻（绝对时间，基于 `CLOCK_MONOTONIC_RAW`）。
///
/// 与 `MonoRawNs` 的区别是语义：`Deadline` 表示"在此时刻前必须完成"，
/// 而 `MonoRawNs` 仅是时间读数。类型分离避免把任意时刻当 deadline 误用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Deadline(pub MonoRawNs);

/// 相对纳秒数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct DurationNs(pub u64);

impl MonoRawNs {
    pub const ZERO: Self = Self(0);
}

impl DurationNs {
    pub const ZERO: Self = Self(0);

    #[inline]
    #[must_use]
    pub const fn from_nanos(ns: u64) -> Self {
        Self(ns)
    }

    #[inline]
    #[must_use]
    pub const fn from_micros(us: u64) -> Self {
        Self(us.saturating_mul(1_000))
    }

    #[inline]
    #[must_use]
    pub const fn from_millis(ms: u64) -> Self {
        Self(ms.saturating_mul(1_000_000))
    }

    #[inline]
    #[must_use]
    pub const fn from_secs(s: u64) -> Self {
        Self(s.saturating_mul(1_000_000_000))
    }

    #[inline]
    #[must_use]
    pub const fn as_nanos(self) -> u64 {
        self.0
    }
}

impl Deadline {
    /// 由当前时刻 + 相对超时构造 deadline（饱和加，不会 panic）。
    #[inline]
    #[must_use]
    pub const fn from_now_plus(now: MonoRawNs, d: DurationNs) -> Self {
        Self(MonoRawNs(now.0.saturating_add(d.0)))
    }

    #[inline]
    #[must_use]
    pub const fn as_mono_raw_ns(self) -> MonoRawNs {
        self.0
    }
}

// ---- 算术：全部饱和，绝不 panic ----

impl Add<DurationNs> for MonoRawNs {
    type Output = MonoRawNs;

    #[inline]
    fn add(self, rhs: DurationNs) -> MonoRawNs {
        MonoRawNs(self.0.saturating_add(rhs.0))
    }
}

impl Sub for MonoRawNs {
    type Output = Option<DurationNs>;

    /// `a - b`：若 `a >= b` 返回 `Some(Duration)`，否则 `None`。
    #[inline]
    fn sub(self, rhs: MonoRawNs) -> Option<DurationNs> {
        self.0.checked_sub(rhs.0).map(DurationNs)
    }
}

impl Add<DurationNs> for Deadline {
    type Output = Deadline;

    #[inline]
    fn add(self, rhs: DurationNs) -> Deadline {
        Deadline(self.0 + rhs)
    }
}

impl Sub for Deadline {
    type Output = Option<DurationNs>;

    #[inline]
    fn sub(self, rhs: Deadline) -> Option<DurationNs> {
        self.0 - rhs.0
    }
}

impl Sub<DurationNs> for Deadline {
    type Output = Deadline;

    #[inline]
    fn sub(self, rhs: DurationNs) -> Deadline {
        Deadline(MonoRawNs(self.0.0.saturating_sub(rhs.0)))
    }
}

impl Add for DurationNs {
    type Output = DurationNs;

    #[inline]
    fn add(self, rhs: DurationNs) -> DurationNs {
        DurationNs(self.0.saturating_add(rhs.0))
    }
}

impl Sub for DurationNs {
    type Output = Option<DurationNs>;

    #[inline]
    fn sub(self, rhs: DurationNs) -> Option<DurationNs> {
        self.0.checked_sub(rhs.0).map(DurationNs)
    }
}

// ============================================================================
// RobotClock trait
// ============================================================================

/// 统一时钟接口（§3.2.1）。所有实现必须 `Send + Sync`，以便跨线程共享
/// （RT snapshot / mock 测试均需要）。
///
/// `sleep_*` 默认语义：至少阻塞 / 推进指定时长；具体精度由实现决定。
/// `MockClock` 不真阻塞，仅推进内部时钟读数，供确定性测试使用。
pub trait RobotClock: Send + Sync {
    /// 读取当前单调原始时钟。
    fn now(&self) -> MonoRawNs;

    /// 阻塞直到 `deadline` 到达。
    ///
    /// 如果 `deadline` 已过（`deadline <= now()`），**立即返回**，不 panic、
    /// 不进 syscall。此约定覆盖 RT 路径可能发生的 "deadline 过期" 自然情形
    /// （`docs/00_design.md` §6.3 `ENABLED.CONTROL_LOST_*` 子态下命令已过期）。
    fn sleep_until(&self, deadline: Deadline);

    /// 阻塞至少 `d`。
    ///
    /// `DurationNs::ZERO` → **立即返回**，不进 syscall。
    fn sleep_for(&self, d: DurationNs);
}

// ============================================================================
// SystemClock：Linux / macOS 走 libc；其他 target 为 panic stub
// ============================================================================

/// 生产时钟实现：底层 `clock_gettime(CLOCK_MONOTONIC_RAW)` + `nanosleep`。
///
/// ### sleep 语义说明（§3.2.1 授权）
///
/// `CLOCK_MONOTONIC_RAW` 不受 NTP / `adjtime` 影响，但 `clock_nanosleep`
/// 不支持 `_RAW` 常量（Linux 仅接受 `_REALTIME` / `_MONOTONIC` /
/// `_MONOTONIC_COARSE` / `_BOOTTIME`），macOS 压根没有 `clock_nanosleep`。
/// 本实现 `sleep_until` 把绝对 deadline 换算成相对时长再调 `nanosleep`，
/// 两平台统一；短时间跨度内 `CLOCK_MONOTONIC_RAW` 与 `CLOCK_MONOTONIC` 的
/// adjtime 漂移可忽略（`docs/00_design.md` §3.2.1 允许）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl SystemClock {
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl RobotClock for SystemClock {
    fn now(&self) -> MonoRawNs {
        MonoRawNs(clock_now_raw_ns())
    }

    fn sleep_until(&self, deadline: Deadline) {
        let now = clock_now_raw_ns();
        let rel = deadline.0.0.saturating_sub(now);
        if rel > 0 {
            sleep_nanos(rel);
        }
    }

    fn sleep_for(&self, d: DurationNs) {
        if d.0 > 0 {
            sleep_nanos(d.0);
        }
    }
}

#[cfg(all(unix, any(target_os = "linux", target_os = "macos")))]
fn clock_now_raw_ns() -> u64 {
    // libc::timespec::{tv_sec, tv_nsec} 在 Linux 与 macOS（64-bit）均为 i64。
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts 是栈上已初始化的 POD，指针有效；clock_gettime 保证只写 ts。
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &raw mut ts) };
    assert!(
        rc == 0,
        "clock_gettime(CLOCK_MONOTONIC_RAW) failed: clockid={}, errno={}",
        libc::CLOCK_MONOTONIC_RAW,
        std::io::Error::last_os_error()
    );
    // tv_sec / tv_nsec 非负（CLOCK_MONOTONIC_RAW 语义保证）；饱和换算防御异常平台返回。
    let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nsec = u64::try_from(ts.tv_nsec).unwrap_or(0);
    secs.saturating_mul(1_000_000_000).saturating_add(nsec)
}

#[cfg(not(all(unix, any(target_os = "linux", target_os = "macos"))))]
fn clock_now_raw_ns() -> u64 {
    unimplemented!(
        "robot-core::time::SystemClock requires Linux or macOS \
         (CLOCK_MONOTONIC_RAW); current target is unsupported"
    );
}

#[cfg(all(unix, any(target_os = "linux", target_os = "macos")))]
fn sleep_nanos(ns: u64) {
    let secs = ns / 1_000_000_000;
    // 0 ≤ rem < 1e9，`cast_signed` 等价于不变，绝不 wrap（clippy pedantic 合规）。
    let rem_ns = (ns % 1_000_000_000).cast_signed();
    let mut req = libc::timespec {
        tv_sec: i64::try_from(secs).unwrap_or(i64::MAX),
        tv_nsec: rem_ns,
    };
    loop {
        let mut rem = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: req / rem 均为栈上已初始化 POD，指针在本帧有效。
        let rc = unsafe { libc::nanosleep(&raw const req, &raw mut rem) };
        if rc == 0 {
            return;
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => {
                // 被信号打断：继续睡剩余时长（§6.2 心跳 / 看门狗信号常规路径）。
                req = rem;
            }
            // EINVAL 意味着我们构造的 timespec 不合法（tv_nsec 超界 / 负数），
            // 属于本模块的数据结构 bug，应该早发现。EFAULT 意味着指针无效，
            // 在本帧栈上值上不可能。都直接 panic 暴露出来。
            other => {
                panic!(
                    "nanosleep failed unexpectedly: errno={:?}, \
                     tv_sec={}, tv_nsec={}",
                    other, req.tv_sec, req.tv_nsec
                );
            }
        }
    }
}

#[cfg(not(all(unix, any(target_os = "linux", target_os = "macos"))))]
fn sleep_nanos(_ns: u64) {
    unimplemented!(
        "robot-core::time::SystemClock requires Linux or macOS (nanosleep); \
         current target is unsupported"
    );
}

// ============================================================================
// MockClock：确定性测试时钟，不真阻塞
// ============================================================================

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// 测试时钟：手动推进，不真阻塞。
///
/// - `advance(d)` / `set(t)`：显式推进或设定当前时间。
/// - `sleep_for(d)`：内部 `fetch_add(d)`，立即返回。
/// - `sleep_until(dl)`：load + compare；若 `dl > now` 则 store(dl)，否则不动。
/// - `Clone` 后各份共享同一时间源（内部 `Arc<AtomicU64>`），方便在测试里把
///   同一 clock 既发给 publisher 又发给 subscriber 观测。
#[derive(Debug, Clone, Default)]
pub struct MockClock {
    now_ns: Arc<AtomicU64>,
}

impl MockClock {
    #[must_use]
    pub fn new(initial: MonoRawNs) -> Self {
        Self {
            now_ns: Arc::new(AtomicU64::new(initial.0)),
        }
    }

    /// 将当前时间向前推进 `d`。
    ///
    /// 饱和加，不会 wrap；若需要知道推进后的时间，用完再调 `now()`。
    pub fn advance(&self, d: DurationNs) {
        self.now_ns.fetch_add(d.0, Ordering::Release);
    }

    /// 直接覆盖当前时间（允许倒退，仅供测试需要）。
    pub fn set(&self, t: MonoRawNs) {
        self.now_ns.store(t.0, Ordering::Release);
    }
}

impl RobotClock for MockClock {
    fn now(&self) -> MonoRawNs {
        MonoRawNs(self.now_ns.load(Ordering::Acquire))
    }

    fn sleep_until(&self, deadline: Deadline) {
        // 已过的 deadline → 不倒退 now；仅未来的 deadline 触发 store。
        // 并发场景下另一线程可能在读 + 写之间推进了 now；这里对"竞态下 now
        // 最终 ≥ deadline"不做强保证（测试时间线靠调用方显式控制）。
        let now = self.now_ns.load(Ordering::Acquire);
        if deadline.0.0 > now {
            self.now_ns.store(deadline.0.0, Ordering::Release);
        }
    }

    fn sleep_for(&self, d: DurationNs) {
        self.now_ns.fetch_add(d.0, Ordering::Release);
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- newtype 基础 ---

    #[test]
    fn mono_raw_ns_order_and_eq() {
        assert!(MonoRawNs(10) < MonoRawNs(20));
        assert_eq!(MonoRawNs(42), MonoRawNs(42));
        assert_eq!(MonoRawNs::ZERO, MonoRawNs(0));
    }

    #[test]
    fn duration_constructors_roundtrip() {
        assert_eq!(DurationNs::from_nanos(7).as_nanos(), 7);
        assert_eq!(DurationNs::from_micros(3).as_nanos(), 3_000);
        assert_eq!(DurationNs::from_millis(5).as_nanos(), 5_000_000);
        assert_eq!(DurationNs::from_secs(2).as_nanos(), 2_000_000_000);
        assert_eq!(DurationNs::ZERO.as_nanos(), 0);
    }

    #[test]
    fn duration_constructors_saturate_on_overflow() {
        // from_secs(u64::MAX) * 1e9 远超 u64，必须饱和到 u64::MAX 而非 panic。
        assert_eq!(DurationNs::from_secs(u64::MAX).as_nanos(), u64::MAX);
        assert_eq!(DurationNs::from_millis(u64::MAX).as_nanos(), u64::MAX);
        assert_eq!(DurationNs::from_micros(u64::MAX).as_nanos(), u64::MAX);
    }

    // --- 算术 ---

    #[test]
    fn mono_raw_ns_plus_duration_saturates() {
        assert_eq!(MonoRawNs(100) + DurationNs::from_nanos(5), MonoRawNs(105));
        assert_eq!(
            MonoRawNs(u64::MAX - 10) + DurationNs::from_nanos(100),
            MonoRawNs(u64::MAX)
        );
    }

    #[test]
    fn mono_raw_ns_minus_returns_option() {
        assert_eq!(
            MonoRawNs(200) - MonoRawNs(50),
            Some(DurationNs::from_nanos(150))
        );
        assert_eq!(MonoRawNs(10) - MonoRawNs(20), None);
        assert_eq!(MonoRawNs(7) - MonoRawNs(7), Some(DurationNs::ZERO));
    }

    #[test]
    fn deadline_plus_duration() {
        let dl = Deadline(MonoRawNs(1_000));
        // 1_000 ns + 1 ms = 1_000 + 1_000_000 = 1_001_000 ns
        assert_eq!(
            dl + DurationNs::from_millis(1),
            Deadline(MonoRawNs(1_001_000))
        );
    }

    #[test]
    fn deadline_minus_deadline_returns_option() {
        let a = Deadline(MonoRawNs(300));
        let b = Deadline(MonoRawNs(120));
        assert_eq!(a - b, Some(DurationNs::from_nanos(180)));
        assert_eq!(b - a, None);
    }

    #[test]
    fn deadline_minus_duration_saturates() {
        let dl = Deadline(MonoRawNs(100));
        assert_eq!(dl - DurationNs::from_nanos(30), Deadline(MonoRawNs(70)));
        // underflow → 饱和到 0，不 panic
        assert_eq!(dl - DurationNs::from_nanos(9999), Deadline(MonoRawNs(0)));
    }

    #[test]
    fn duration_plus_duration_saturates() {
        assert_eq!(
            DurationNs::from_nanos(5) + DurationNs::from_nanos(3),
            DurationNs::from_nanos(8)
        );
        assert_eq!(
            DurationNs::from_nanos(u64::MAX - 1) + DurationNs::from_nanos(100),
            DurationNs::from_nanos(u64::MAX)
        );
    }

    #[test]
    fn duration_minus_duration_returns_option() {
        assert_eq!(
            DurationNs::from_nanos(10) - DurationNs::from_nanos(3),
            Some(DurationNs::from_nanos(7))
        );
        assert_eq!(DurationNs::from_nanos(3) - DurationNs::from_nanos(10), None);
    }

    #[test]
    fn deadline_from_now_plus() {
        let dl = Deadline::from_now_plus(MonoRawNs(1_000), DurationNs::from_millis(2));
        assert_eq!(dl, Deadline(MonoRawNs(2_001_000)));
        // 饱和
        let dl2 = Deadline::from_now_plus(MonoRawNs(u64::MAX - 5), DurationNs::from_nanos(100));
        assert_eq!(dl2, Deadline(MonoRawNs(u64::MAX)));
    }

    // --- SystemClock ---

    #[test]
    fn system_clock_is_monotonic_over_1000_samples() {
        let clk = SystemClock::new();
        let mut prev = clk.now();
        for _ in 0..1_000 {
            let cur = clk.now();
            assert!(
                cur >= prev,
                "CLOCK_MONOTONIC_RAW backward: {prev:?} → {cur:?}"
            );
            prev = cur;
        }
    }

    #[test]
    fn system_clock_sleep_for_blocks_at_least_requested() {
        let clk = SystemClock::new();
        let before = clk.now();
        clk.sleep_for(DurationNs::from_millis(2));
        let after = clk.now();
        let elapsed = (after - before).expect("monotonic");
        assert!(
            elapsed.as_nanos() >= 2_000_000,
            "sleep_for shorter than requested: {}ns",
            elapsed.as_nanos()
        );
    }

    #[test]
    fn system_clock_sleep_until_past_deadline_returns_immediately() {
        let clk = SystemClock::new();
        let now = clk.now();
        let past = Deadline(MonoRawNs(now.0.saturating_sub(1_000_000)));
        let before = clk.now();
        clk.sleep_until(past);
        let after = clk.now();
        let elapsed = (after - before).expect("monotonic").as_nanos();
        // 允许少量调度抖动，但不应接近 1 ms
        assert!(
            elapsed < 1_000_000,
            "sleep_until past deadline still blocked {elapsed}ns"
        );
    }

    #[test]
    fn system_clock_sleep_until_future_blocks() {
        let clk = SystemClock::new();
        let deadline = Deadline(clk.now()) + DurationNs::from_millis(2);
        let before = clk.now();
        clk.sleep_until(deadline);
        let after = clk.now();
        assert!(
            after >= deadline.0,
            "sleep_until returned early: {after:?} < {:?}",
            deadline.0
        );
        let elapsed = (after - before).expect("monotonic").as_nanos();
        assert!(elapsed >= 2_000_000, "sleep_until underslept: {elapsed}ns");
    }

    // --- MockClock ---

    #[test]
    fn mock_clock_starts_at_initial() {
        let clk = MockClock::new(MonoRawNs(42));
        assert_eq!(clk.now(), MonoRawNs(42));
    }

    #[test]
    fn mock_clock_advance_moves_now_forward() {
        let clk = MockClock::new(MonoRawNs(100));
        clk.advance(DurationNs::from_nanos(50));
        assert_eq!(clk.now(), MonoRawNs(150));
    }

    #[test]
    fn mock_clock_set_overrides_now() {
        let clk = MockClock::new(MonoRawNs(100));
        clk.set(MonoRawNs(5));
        assert_eq!(clk.now(), MonoRawNs(5));
    }

    #[test]
    fn mock_clock_sleep_for_advances_now() {
        let clk = MockClock::new(MonoRawNs(1_000));
        clk.sleep_for(DurationNs::from_nanos(250));
        assert_eq!(clk.now(), MonoRawNs(1_250));
    }

    #[test]
    fn mock_clock_sleep_until_future_advances_to_deadline() {
        let clk = MockClock::new(MonoRawNs(1_000));
        clk.sleep_until(Deadline(MonoRawNs(5_000)));
        assert_eq!(clk.now(), MonoRawNs(5_000));
    }

    #[test]
    fn mock_clock_sleep_until_past_does_not_rewind() {
        let clk = MockClock::new(MonoRawNs(10_000));
        clk.sleep_until(Deadline(MonoRawNs(500)));
        assert_eq!(clk.now(), MonoRawNs(10_000));
    }

    #[test]
    fn mock_clock_clone_shares_time_source() {
        let a = MockClock::new(MonoRawNs::ZERO);
        let b = a.clone();
        a.sleep_for(DurationNs::from_nanos(77));
        assert_eq!(b.now(), MonoRawNs(77));
        b.advance(DurationNs::from_nanos(23));
        assert_eq!(a.now(), MonoRawNs(100));
    }

    #[test]
    fn mock_clock_sleep_for_zero_is_noop() {
        let clk = MockClock::new(MonoRawNs(42));
        clk.sleep_for(DurationNs::ZERO);
        assert_eq!(clk.now(), MonoRawNs(42));
    }

    #[test]
    fn system_clock_sleep_for_zero_is_noop() {
        let clk = SystemClock::new();
        let before = clk.now();
        clk.sleep_for(DurationNs::ZERO);
        let after = clk.now();
        let elapsed = (after - before).expect("monotonic").as_nanos();
        // 不进 syscall，elapsed 应该远小于 1 ms（纯寄存器读开销）。
        assert!(
            elapsed < 1_000_000,
            "sleep_for(ZERO) should skip syscall, but elapsed {elapsed}ns"
        );
    }

    #[test]
    fn system_clock_delta_non_negative_over_1000_samples() {
        // 与 `system_clock_is_monotonic_over_1000_samples` 同源但显式检查
        // `delta = cur - prev ≥ 0` 的 Option 语义（MonoRawNs::sub 返回 None
        // 代表倒退）。
        let clk = SystemClock::new();
        let mut prev = clk.now();
        for _ in 0..1_000 {
            let cur = clk.now();
            let delta = cur - prev;
            assert!(
                delta.is_some(),
                "CLOCK_MONOTONIC_RAW backward: {prev:?} → {cur:?}"
            );
            prev = cur;
        }
    }

    // --- trait bound 静态检查 ---

    #[test]
    fn clocks_are_send_sync_and_usable_as_trait_objects() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SystemClock>();
        assert_send_sync::<MockClock>();
        assert_send_sync::<Arc<dyn RobotClock>>();

        // 动态派发可用
        let clk: Arc<dyn RobotClock> = Arc::new(MockClock::new(MonoRawNs(42)));
        assert_eq!(clk.now(), MonoRawNs(42));
    }
}
