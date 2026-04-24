//! S0 daemon 的 Part 1 smoke main：
//!
//! - 构造 `SystemClock` + `MockHardRTAdapter`（3 关节）+ `LatestCommandSlot` +
//!   `RtSnapshotPublisher/Reader`；
//! - 拉起 `rt_thread`（默认 `SCHED_FIFO 70 + mlockall + 50 Hz`，权限不足时降级）；
//! - 跑 `--smoke-duration`（默认 10 s），读 snapshot 计数 + 报摘要；
//! - `Ctrl+C` / 超时 / 主线程 panic 时 `request_stop` + `join`。
//!
//! 没有 `ipc_thread` / `pub_thread` / iceoryx2 / state machine 驱动——那些是 Part 2
//! 的交付。本 main 是 "`rt_thread` 在真正内核下能跑稳" 的最小证据。
//!
//! ### 退出码
//!
//! - `0`：smoke duration 到期，snapshot 观察到 ≥ 1 个 tick，无 panic。
//! - `1`：RT 线程完全没跑（`tick_count` == 0），可能 `SCHED_FIFO` + `MockClock` 配错。
//! - `2`：参数解析失败。
//!
//! ### 用法
//!
//! ```bash
//! # 开发机（无 rtprio），走降级路径
//! cargo run -p robot-s0 --bin s0_daemon -- --smoke-duration 3s
//!
//! # Orin（已配 limits.d），严格 RT
//! cargo run -p robot-s0 --release --bin s0_daemon -- --smoke-duration 10s --strict-rt
//! ```

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use robot_adapter_mock::MockHardRTAdapter;
use robot_core::adapter::{JointCommandBuf, RtSnapshot};
use robot_core::time::{RobotClock, SystemClock};
use robot_s0::rt_thread::{RtThreadConfig, spawn_rt_thread};
use robot_transport::{LatestCommandSlot, RtSnapshotPublisher};

// ============================================================================
// CLI 参数（手写 parse，不引 clap）
// ============================================================================

struct Args {
    smoke_duration: Duration,
    tick_period_ns: u64,
    strict_rt: bool,
    joint_count: u8,
    cpu_affinity: Option<usize>,
}

impl Args {
    fn defaults() -> Self {
        Self {
            smoke_duration: Duration::from_secs(10),
            tick_period_ns: 20_000_000, // 50 Hz
            strict_rt: false,
            joint_count: 3,
            cpu_affinity: None,
        }
    }

    fn parse() -> Result<Self, String> {
        let mut args = Self::defaults();
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            match a.as_str() {
                "--smoke-duration" => {
                    i += 1;
                    args.smoke_duration =
                        parse_duration(raw.get(i).ok_or("--smoke-duration requires value")?)?;
                }
                "--tick-hz" => {
                    i += 1;
                    let hz: u64 = raw
                        .get(i)
                        .ok_or("--tick-hz requires value")?
                        .parse()
                        .map_err(|e| format!("--tick-hz: {e}"))?;
                    if hz == 0 {
                        return Err("--tick-hz must be > 0".into());
                    }
                    args.tick_period_ns = 1_000_000_000 / hz;
                }
                "--strict-rt" => args.strict_rt = true,
                "--joints" => {
                    i += 1;
                    args.joint_count = raw
                        .get(i)
                        .ok_or("--joints requires value")?
                        .parse()
                        .map_err(|e| format!("--joints: {e}"))?;
                }
                "--cpu" => {
                    i += 1;
                    let cpu: usize = raw
                        .get(i)
                        .ok_or("--cpu requires value")?
                        .parse()
                        .map_err(|e| format!("--cpu: {e}"))?;
                    args.cpu_affinity = Some(cpu);
                }
                "--help" | "-h" => {
                    return Err("HELP".into());
                }
                other => return Err(format!("unknown arg: {other}")),
            }
            i += 1;
        }
        Ok(args)
    }
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    // 支持 `3s` / `500ms` / `100` 纯数字（默认秒）
    if let Some(rest) = s.strip_suffix("ms") {
        let ms: u64 = rest
            .parse()
            .map_err(|e| format!("--smoke-duration ms: {e}"))?;
        Ok(Duration::from_millis(ms))
    } else if let Some(rest) = s.strip_suffix('s') {
        let secs: u64 = rest
            .parse()
            .map_err(|e| format!("--smoke-duration s: {e}"))?;
        Ok(Duration::from_secs(secs))
    } else {
        let secs: u64 = s.parse().map_err(|e| format!("--smoke-duration: {e}"))?;
        Ok(Duration::from_secs(secs))
    }
}

fn print_help() {
    eprintln!(
        "s0_daemon (Step 6 Part 1 smoke)

USAGE:
  s0_daemon [OPTIONS]

OPTIONS:
  --smoke-duration <DUR>   总运行时长（例：3s / 500ms / 10）。默认 10s。
  --tick-hz <HZ>           tick 频率（默认 50 Hz = 20 ms period）。
  --joints <N>             mock adapter 关节数（默认 3；上限 MAX_JOINTS=32）。
  --strict-rt              SCHED_FIFO / mlockall / pin_to_cpu 失败立即 panic。默认 false（降级）。
  --cpu <IDX>              CPU 亲和性。默认不绑核。
  -h, --help               显示此帮助。
"
    );
}

// ============================================================================
// main
// ============================================================================

fn main() {
    let args = match Args::parse() {
        Ok(a) => a,
        Err(msg) => {
            if msg == "HELP" {
                print_help();
                std::process::exit(0);
            }
            eprintln!("error: {msg}");
            print_help();
            std::process::exit(2);
        }
    };

    eprintln!(
        "s0_daemon: starting rt_thread (hz={}, joints={}, strict_rt={}, cpu={:?}, duration={:?})",
        1_000_000_000 / args.tick_period_ns,
        args.joint_count,
        args.strict_rt,
        args.cpu_affinity,
        args.smoke_duration,
    );

    // ---- 构造 ----
    let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
    let adapter = MockHardRTAdapter::with_unlimited_limits(args.joint_count);
    let (_cmd_writer, cmd_reader) =
        LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
    let (snap_pub, snap_rdr) = RtSnapshotPublisher::new(RtSnapshot::default());

    let cfg = RtThreadConfig {
        tick_period_ns: args.tick_period_ns,
        sched_fifo_priority: 70,
        cpu_affinity: args.cpu_affinity,
        strict_rt: args.strict_rt,
    };

    // ---- 启动 RT 线程 ----
    let handle = match spawn_rt_thread(cfg, clock, adapter, cmd_reader, snap_pub) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("fatal: failed to spawn rt_thread: {e}");
            std::process::exit(1);
        }
    };

    // ---- smoke 期间周期性采样 ----
    let start = std::time::Instant::now();
    let sample_interval = args.smoke_duration / 4;
    let mut samples: Vec<u64> = Vec::with_capacity(5);
    while start.elapsed() < args.smoke_duration {
        std::thread::sleep(sample_interval);
        let snap = snap_rdr.load();
        samples.push(snap.tick_seq);
        eprintln!(
            "sample t={:>6}ms tick_seq={} state.joint_count={}",
            start.elapsed().as_millis(),
            snap.tick_seq,
            snap.state.joint_count,
        );
    }

    // ---- 停止 + 摘要 ----
    handle.request_stop();
    let metrics = Arc::clone(handle.metrics());
    handle.join().expect("rt_thread panicked");

    let final_tick = metrics.tick_count.load(Ordering::Relaxed);
    let overruns = metrics.overruns.load(Ordering::Relaxed);
    let write_errs = metrics.write_errors.load(Ordering::Relaxed);
    let read_errs = metrics.read_errors.load(Ordering::Relaxed);

    eprintln!(
        "\n=== summary ===\n\
         total_ticks : {final_tick}\n\
         overruns    : {overruns}\n\
         read_errors : {read_errs}\n\
         write_errors: {write_errs}\n\
         samples     : {samples:?}\n"
    );

    if final_tick == 0 {
        eprintln!("error: rt_thread produced 0 ticks — RT loop likely never entered");
        std::process::exit(1);
    }
    std::process::exit(0);
}
