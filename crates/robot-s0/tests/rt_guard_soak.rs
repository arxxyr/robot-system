//! `rt_guard_soak` —— Step 7 task 26 RT 铁律 1 分钟回归测试。
//!
//! ### 入口
//!
//! ```bash
//! cargo test -p robot-s0 --features rt_guard --test rt_guard_soak -- --ignored
//! # 自定义时长（默认 60s）：
//! RT_GUARD_SOAK_SECS=10 cargo test -p robot-s0 --features rt_guard \
//!     --test rt_guard_soak -- --ignored
//! ```
//!
//! ### 验证什么
//!
//! 1. **0 heap allocation in RT scope**：整个 test binary 用
//!    [`assert_no_alloc::AllocDisabler`] 作 `#[global_allocator]`；RT 线程主循环每
//!    tick 进 `rt_entry!` scope（内部走 `assert_no_alloc::assert_no_alloc(...)`），
//!    任何堆分配立即 panic（thread panic propagate 到 `JoinHandle::join`）。
//! 2. **0 `Arc::clone` / `Arc::drop` in RT scope**：`rt_entry!` 同时挂
//!    `RtArcGuard`，`RtArc::clone` / `RtArc::drop` 在 scope 内 panic。
//! 3. **稳态吞吐**：`tick_count` ≥ `duration * tick_hz * 0.95`，证明 RT 循环
//!    没被周期性 stall。
//! 4. **stderr/journal 无 `assert_no_alloc` 警告**：本文件代码没 import 任何
//!    会在 RT 闭环里调用 alloc 的 helper（数据全 stack-allocated POD）。
//!
//! ### 为什么默认 60s
//!
//! `docs/handoffs/2026-04-24_5.md` §8 task 26 写 "1min ignored 回归测试"。60s 是
//! 合理的"短期 soak"窗口：
//! - 快到能在 dev 机上跑（但 ignored 默认不跑）；
//! - 长到能涵盖 50 Hz × 60s = 3000 ticks，覆盖 metrics overflow / cache thrash
//!   一类时序敏感问题；
//! - 真正长 soak（1h / 24h）走后续 Step 9 `crash_restart` + nightly schedule。
//!
//! ### 不做的事
//!
//! - 不接 iceoryx2（避免 transport 层 alloc 干扰；纯 RT loop alloc 守卫）。
//! - 不接 `SCHED_FIFO`（dev 机 / CI 都拿不到 rtprio；本测试是 *alloc 守卫*，
//!   不是 *latency 守卫*）。
//! - 不接 mlockall（同上）。

#![cfg(all(target_os = "linux", feature = "rt_guard"))]
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use robot_adapter_mock::MockHardRTAdapter;
use robot_core::adapter::{JointCommandBuf, RtSnapshot};
use robot_core::executor_ctrl::OutputPolicy;
use robot_core::time::{RobotClock, SystemClock};
use robot_s0::hard_rt_ctrl::HardRtCtrlInner;
use robot_s0::rt_thread::{RtThreadConfig, spawn_rt_thread};
use robot_transport::{LatestCommandSlot, RtSnapshotPublisher};

#[global_allocator]
static ALLOC_GUARD: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

const DEFAULT_SOAK_SECS: u64 = 60;
const TICK_HZ: f64 = 50.0;
const PASS_THRESHOLD_RATIO: f64 = 0.95;

#[test]
#[ignore = "1min RT soak; run with `cargo test -p robot-s0 --features rt_guard -- --ignored`"]
fn rt_guard_soak_no_alloc_no_arc_for_one_minute() {
    let secs = std::env::var("RT_GUARD_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SOAK_SECS);
    let duration = Duration::from_secs(secs);

    eprintln!(
        "[rt_guard_soak] starting {secs}s soak ({} expected ticks @ {TICK_HZ} Hz, threshold {:.0}%)",
        (secs as f64 * TICK_HZ) as u64,
        PASS_THRESHOLD_RATIO * 100.0,
    );

    // ---- 准备阶段（不在 RT scope，alloc 允许）----
    let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
    let (_cmd_writer, cmd_reader) =
        LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
    let (snap_pub, _snap_rdr) = RtSnapshotPublisher::<RtSnapshot>::new(RtSnapshot::default());
    let adapter = MockHardRTAdapter::with_unlimited_limits(3);

    let cfg = RtThreadConfig {
        tick_period_ns: (1_000_000_000.0 / TICK_HZ) as u64,
        sched_fifo_priority: 70,
        cpu_affinity: None,
        strict_rt: false, // CI / dev 拿不到 rtprio 时降级
    };

    // ---- spawn RT 线程并跑 soak ----
    //
    // task-30 v9 / M5：spawn_rt_thread 新增必传 `Arc<HardRtCtrlInner>` 参数。
    // soak 测试只验 alloc / Arc 守卫，**不**驱动 emergency / normal action；
    // 默认 OutputPolicy=Disabled 跳 write_command（仍 read_state + publish）。
    // 如果想覆盖 write_command 路径，把 policy 切到 ActiveForward。
    let rt_ctrl = Arc::new(HardRtCtrlInner::new());
    rt_ctrl.policy.store(
        OutputPolicy::ActiveForward.to_u8(),
        std::sync::atomic::Ordering::Release,
    );
    let handle = spawn_rt_thread(cfg, clock, adapter, cmd_reader, snap_pub, rt_ctrl)
        .expect("spawn_rt_thread failed");
    let metrics = Arc::clone(handle.metrics());

    let started = Instant::now();
    while started.elapsed() < duration {
        std::thread::sleep(Duration::from_millis(500));
    }

    handle.request_stop();
    handle.join().expect(
        "RT thread panicked (likely from rt_guard: heap alloc / RtArc::clone / RtArc::drop \
         inside rt_entry! scope)",
    );

    // ---- 验证 metrics ----
    let tick_count = metrics
        .tick_count
        .load(std::sync::atomic::Ordering::Relaxed);
    let read_errors = metrics
        .read_errors
        .load(std::sync::atomic::Ordering::Relaxed);
    let write_errors = metrics
        .write_errors
        .load(std::sync::atomic::Ordering::Relaxed);
    let overruns = metrics.overruns.load(std::sync::atomic::Ordering::Relaxed);

    eprintln!(
        "[rt_guard_soak] done: tick_count={tick_count}  read_err={read_errors}  \
         write_err={write_errors}  overruns={overruns}"
    );

    let expected = (secs as f64 * TICK_HZ) as u64;
    let min_pass = (expected as f64 * PASS_THRESHOLD_RATIO) as u64;
    assert!(
        tick_count >= min_pass,
        "tick_count {tick_count} below threshold {min_pass} (expected ~{expected}); \
         RT loop stall or alloc-related abort"
    );
    assert_eq!(
        read_errors, 0,
        "MockHardRTAdapter.read_state should never error in soak"
    );
    assert_eq!(
        write_errors, 0,
        "MockHardRTAdapter.write_command should never error in soak"
    );
    // overruns 不强约束——dev 机 / CI 抖动正常；记录到 stderr 即可。
}
