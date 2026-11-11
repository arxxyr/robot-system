//! `iceoryx2_rt_baseline` —— 同进程两线程 IPC 延迟诊断。
//!
//! 采集 publish → subscribe 端到端延迟直方图。发布频率 ≥50 Hz 时，旧 p99
//! <100 µs / p999 <500 µs 判据已撤销；内置 FAIL 仅作 legacy 诊断，适用范围见
//! `docs/10_verification.md` §10.1。
//!
//! ### 线程布局
//!
//! - **`sub_thread`**：`SCHED_FIFO 70` + `mlockall` + 可选 `pin_to_cpu`；创建 iceoryx2
//!   Node + `open_or_create subscriber<Heartbeat>`；`receive()` 拿到 sample 后
//!   从 `stamp_ns` 计算延迟并记入 `hdrhistogram`。
//! - **`pub_thread`**：`SCHED_FIFO 70` + `mlockall` + 可选 `pin_to_cpu`（cpu+1，避开 sub）；
//!   以 `--hz` 节拍发送 Heartbeat body，`stamp_ns` 直接写 `clock.now().0`。
//!
//! 同进程不同 Node 的 iceoryx2 通信路径和跨进程等价（shm 路由一致），所以这个
//! baseline 也能近似跨进程成本；真正跨进程需要跑两个 binary，task 22 on Orin 时
//! 视情况决定。
//!
//! ### CLI
//!
//! ```text
//! iceoryx2_rt_baseline --duration 10m --hz 50 --priority 70 --cpu 4
//! iceoryx2_rt_baseline --duration 30s --hz 50          # 本地 smoke（无 rtprio 时降级）
//! ```
//!
//! ### 退出码
//!
//! - `0`：所有阈值通过。
//! - `1`：门槛 fail（p99 / p999 / `sample_count` 任一违反）。
//! - `2`：CLI 参数非法 / 启动失败。

#![forbid(unused_must_use)]
// Harness 大量 ns ↔ µs 展示 + 比例统计涉及 u64/f64 互转，在这里安全（latency 实际值
// 远小于 f64 mantissa 上限）。在一整个 bin 内 allow 比散打 `#[allow]` 更干净。
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::similar_names
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::build_helpers::build_heartbeat;
use robot_msg::markers;
use robot_transport::iceoryx2::{TopicClass, publisher as open_pub, subscriber as open_sub};
use robot_transport::traits::{LatestSubscriber, Publisher};

// ============================================================================
// CLI
// ============================================================================

struct Args {
    duration: Duration,
    hz: f64,
    priority: i32,
    sub_cpu: Option<usize>,
    pub_cpu: Option<usize>,
    strict_rt: bool,
    p99_threshold_ns: u64,
    p999_threshold_ns: u64,
    topic: String,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut args = Self {
            duration: Duration::from_secs(30),
            hz: 50.0,
            priority: 70,
            sub_cpu: None,
            pub_cpu: None,
            strict_rt: false,
            p99_threshold_ns: 100_000,  // 100 µs
            p999_threshold_ns: 500_000, // 500 µs
            topic: format!("/rt_baseline_{}", std::process::id()),
        };

        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            match a.as_str() {
                "--duration" => {
                    i += 1;
                    args.duration = parse_duration(raw.get(i).ok_or("--duration requires value")?)?;
                }
                "--hz" => {
                    i += 1;
                    args.hz = raw
                        .get(i)
                        .ok_or("--hz requires value")?
                        .parse()
                        .map_err(|e| format!("--hz: {e}"))?;
                }
                "--priority" => {
                    i += 1;
                    args.priority = raw
                        .get(i)
                        .ok_or("--priority requires value")?
                        .parse()
                        .map_err(|e| format!("--priority: {e}"))?;
                }
                "--cpu" => {
                    // --cpu N 同时绑 sub=N 和 pub=N+1（最常用）
                    i += 1;
                    let base: usize = raw
                        .get(i)
                        .ok_or("--cpu requires value")?
                        .parse()
                        .map_err(|e| format!("--cpu: {e}"))?;
                    args.sub_cpu = Some(base);
                    args.pub_cpu = Some(base + 1);
                }
                "--sub-cpu" => {
                    i += 1;
                    args.sub_cpu = Some(
                        raw.get(i)
                            .ok_or("--sub-cpu requires value")?
                            .parse()
                            .map_err(|e| format!("--sub-cpu: {e}"))?,
                    );
                }
                "--pub-cpu" => {
                    i += 1;
                    args.pub_cpu = Some(
                        raw.get(i)
                            .ok_or("--pub-cpu requires value")?
                            .parse()
                            .map_err(|e| format!("--pub-cpu: {e}"))?,
                    );
                }
                "--strict-rt" => args.strict_rt = true,
                "--p99-us" => {
                    i += 1;
                    let us: u64 = raw
                        .get(i)
                        .ok_or("--p99-us requires value")?
                        .parse()
                        .map_err(|e| format!("--p99-us: {e}"))?;
                    args.p99_threshold_ns = us.saturating_mul(1_000);
                }
                "--p999-us" => {
                    i += 1;
                    let us: u64 = raw
                        .get(i)
                        .ok_or("--p999-us requires value")?
                        .parse()
                        .map_err(|e| format!("--p999-us: {e}"))?;
                    args.p999_threshold_ns = us.saturating_mul(1_000);
                }
                "--topic" => {
                    i += 1;
                    args.topic
                        .clone_from(raw.get(i).ok_or("--topic requires value")?);
                }
                "--help" | "-h" => return Err("HELP".into()),
                other => return Err(format!("unknown arg: {other}")),
            }
            i += 1;
        }

        if args.hz <= 0.0 {
            return Err("--hz must be > 0".into());
        }
        Ok(args)
    }
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    if let Some(rest) = s.strip_suffix("ms") {
        Ok(Duration::from_millis(
            rest.parse().map_err(|e| format!("ms: {e}"))?,
        ))
    } else if let Some(rest) = s.strip_suffix("m") {
        Ok(Duration::from_secs(
            rest.parse::<u64>().map_err(|e| format!("m: {e}"))? * 60,
        ))
    } else if let Some(rest) = s.strip_suffix('s') {
        Ok(Duration::from_secs(
            rest.parse().map_err(|e| format!("s: {e}"))?,
        ))
    } else {
        Ok(Duration::from_secs(
            s.parse().map_err(|e| format!("secs: {e}"))?,
        ))
    }
}

fn print_help() {
    eprintln!(
        "iceoryx2_rt_baseline (Step 6 Part 2 hard gate)

USAGE:
  iceoryx2_rt_baseline [OPTIONS]

OPTIONS:
  --duration <DUR>     soak 时长（10m / 30s / 500ms 均可）。默认 30s。
  --hz <HZ>            publish 频率。默认 50。
  --priority <N>       SCHED_FIFO 优先级。默认 70。
  --cpu <N>            同时绑 sub_cpu=N pub_cpu=N+1。默认不绑。
  --sub-cpu <N>        仅绑 sub。
  --pub-cpu <N>        仅绑 pub。
  --strict-rt          RT syscall 失败立即 panic（Orin 真门槛模式）。
  --p99-us <US>        p99 阈值（微秒）。默认 100。
  --p999-us <US>       p999 阈值（微秒）。默认 500。
  --topic <NAME>       iceoryx2 service 名。默认 /rt_baseline_<pid>。
  -h, --help           显示此帮助。
"
    );
}

// ============================================================================
// main
// ============================================================================

fn main() {
    robot_logging::set_process_name("robot-rt-base");
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

    match run(&args) {
        Ok(report) => {
            println!("{report}");
            std::process::exit(i32::from(!report.pass));
        }
        Err(e) => {
            eprintln!("fatal: {e}");
            std::process::exit(2);
        }
    }
}

// ============================================================================
// Report
// ============================================================================

struct Report {
    samples: u64,
    p50_ns: u64,
    p99_ns: u64,
    p999_ns: u64,
    max_ns: u64,
    overruns: u64,
    drops: u64,
    pass: bool,
    p99_threshold_ns: u64,
    p999_threshold_ns: u64,
    duration: Duration,
    hz: f64,
}

impl core::fmt::Display for Report {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let expected = (self.duration.as_secs_f64() * self.hz) as u64;
        let sample_ratio = if expected > 0 {
            self.samples as f64 / expected as f64
        } else {
            0.0
        };
        let p99_status = if self.p99_ns <= self.p99_threshold_ns {
            "pass"
        } else {
            "FAIL"
        };
        let p999_status = if self.p999_ns <= self.p999_threshold_ns {
            "pass"
        } else {
            "FAIL"
        };
        writeln!(f, "=== iceoryx2_rt_baseline report ===")?;
        writeln!(f, "  duration     : {:?}", self.duration)?;
        writeln!(
            f,
            "  samples      : {} / expected {} (ratio {:.3})",
            self.samples, expected, sample_ratio
        )?;
        writeln!(
            f,
            "  p50          : {:>7} ns ({:>6.1} µs)",
            self.p50_ns,
            self.p50_ns as f64 / 1000.0
        )?;
        writeln!(
            f,
            "  p99          : {:>7} ns ({:>6.1} µs)   threshold {} µs → {}",
            self.p99_ns,
            self.p99_ns as f64 / 1000.0,
            self.p99_threshold_ns / 1000,
            p99_status,
        )?;
        writeln!(
            f,
            "  p999         : {:>7} ns ({:>6.1} µs)   threshold {} µs → {}",
            self.p999_ns,
            self.p999_ns as f64 / 1000.0,
            self.p999_threshold_ns / 1000,
            p999_status,
        )?;
        writeln!(
            f,
            "  max          : {:>7} ns ({:>6.1} µs)",
            self.max_ns,
            self.max_ns as f64 / 1000.0
        )?;
        writeln!(f, "  pub_overruns : {}", self.overruns)?;
        writeln!(f, "  sub_drops    : {}", self.drops)?;
        writeln!(
            f,
            "  overall      : {}",
            if self.pass { "PASS" } else { "FAIL" }
        )
    }
}

// ============================================================================
// run
// ============================================================================

#[allow(clippy::too_many_lines)]
fn run(args: &Args) -> Result<Report, String> {
    eprintln!(
        "iceoryx2_rt_baseline:\n  \
         duration={:?}  hz={}  priority={}  sub_cpu={:?}  pub_cpu={:?}  strict_rt={}\n  \
         topic={}\n  \
         thresholds: p99 ≤ {} µs, p999 ≤ {} µs",
        args.duration,
        args.hz,
        args.priority,
        args.sub_cpu,
        args.pub_cpu,
        args.strict_rt,
        args.topic,
        args.p99_threshold_ns / 1000,
        args.p999_threshold_ns / 1000,
    );

    // 共享状态：运行标志 + drop 计数 + overrun 计数。
    let stop = Arc::new(AtomicBool::new(false));
    let drops = Arc::new(AtomicU64::new(0));
    let overruns = Arc::new(AtomicU64::new(0));

    // sub_thread 同步信号：subscribe 成功后主线程再启动 pub_thread，避免 publisher
    // 发得太早而 subscriber 还没 subscribe 上。
    let (sub_ready_tx, sub_ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

    // 延迟直方图：sub 线程写，主线程 join 后读。用 Mutex 最简单，worst case 只有
    // 主线程 join 后访问，竞争不存在。
    let hist: Arc<std::sync::Mutex<Histogram<u64>>> = Arc::new(std::sync::Mutex::new(
        Histogram::<u64>::new_with_bounds(1_000, 10_000_000_000, 3)
            .map_err(|e| format!("hdrhistogram init: {e}"))?,
    ));

    // ---- sub_thread ----
    let sub_topic = args.topic.clone();
    let sub_priority = args.priority;
    let sub_cpu = args.sub_cpu;
    let sub_strict = args.strict_rt;
    let sub_stop = Arc::clone(&stop);
    let sub_drops = Arc::clone(&drops);
    let sub_hist = Arc::clone(&hist);
    let sub_handle = thread::Builder::new()
        .name("rt_baseline_sub".into())
        .spawn(move || {
            apply_rt(sub_priority, sub_cpu, sub_strict, "sub");

            let node = NodeBuilder::new()
                .create::<IxService>()
                .expect("sub node create");
            let mut sub = open_sub::<markers::Heartbeat>(&node, &sub_topic, TopicClass::Heartbeat)
                .expect("open subscriber");

            let _ = sub_ready_tx.send(());
            drop(sub_ready_tx);

            let clock = SystemClock::new();
            while !sub_stop.load(Ordering::Acquire) {
                match sub.latest() {
                    Ok(Some(lease)) => match lease.view() {
                        Ok(view) => {
                            let hb = view.body.get();
                            let pub_stamp = hb.stamp_ns();
                            let now = clock.now().0;
                            let latency = now.saturating_sub(pub_stamp);
                            let mut h = sub_hist.lock().expect("hist poisoned");
                            let _ = h.record(latency.max(1));
                        }
                        Err(_) => {
                            sub_drops.fetch_add(1, Ordering::Relaxed);
                        }
                    },
                    Ok(None) => {}
                    Err(_) => {
                        sub_drops.fetch_add(1, Ordering::Relaxed);
                    }
                }
                // 轮询密集：iceoryx2 是 polling 模式，这里用短 busy-spin + yield
                // 模拟 RT 订阅者的典型实现。
                std::hint::spin_loop();
                thread::yield_now();
            }
        })
        .map_err(|e| format!("spawn sub: {e}"))?;

    // 等 subscriber 准备好
    sub_ready_rx
        .recv()
        .map_err(|_| "sub thread panicked before ready")?;

    // ---- pub_thread ----
    let pub_topic = args.topic.clone();
    let pub_priority = args.priority;
    let pub_cpu = args.pub_cpu;
    let pub_strict = args.strict_rt;
    let pub_stop = Arc::clone(&stop);
    let pub_overruns = Arc::clone(&overruns);
    let pub_hz = args.hz;
    let pub_duration = args.duration;
    let pub_handle = thread::Builder::new()
        .name("rt_baseline_pub".into())
        .spawn(move || {
            apply_rt(pub_priority, pub_cpu, pub_strict, "pub");

            let node = NodeBuilder::new()
                .create::<IxService>()
                .expect("pub node create");
            let mut pub_ = open_pub::<markers::Heartbeat>(&node, &pub_topic, TopicClass::Heartbeat)
                .expect("open publisher");

            let clock = SystemClock::new();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let period_ns = (1e9_f64 / pub_hz) as u64;
            let start = Instant::now();
            let mut seq: u64 = 0;
            let mut next_deadline_ns = clock.now().0 + period_ns;

            while !pub_stop.load(Ordering::Acquire) && start.elapsed() < pub_duration {
                let now = clock.now().0;
                let stamp = now;
                let body = build_heartbeat(0xBA5E, seq, pub_hz, stamp);
                match pub_.loan(1024 + body.len()) {
                    Ok(mut w) => {
                        w.set_seq(seq);
                        w.set_source_id(0xBA5E);
                        w.set_stamp_ns(stamp);
                        w.set_body_bytes(&body);
                        let _ = w.publish();
                    }
                    Err(_) => {
                        pub_overruns.fetch_add(1, Ordering::Relaxed);
                    }
                }
                seq = seq.wrapping_add(1);

                // 自校准下一次 deadline：不累积漂移。
                next_deadline_ns += period_ns;
                let now2 = clock.now().0;
                if next_deadline_ns > now2 {
                    thread::sleep(Duration::from_nanos(next_deadline_ns - now2));
                } else {
                    pub_overruns.fetch_add(1, Ordering::Relaxed);
                    next_deadline_ns = now2 + period_ns;
                }
            }
        })
        .map_err(|e| format!("spawn pub: {e}"))?;

    // ---- 等 pub 完成 ----
    pub_handle.join().map_err(|_| "pub thread panicked")?;

    // 给 sub 多抓一会儿 buffer 内残留样本。
    thread::sleep(Duration::from_millis(500));
    stop.store(true, Ordering::Release);
    sub_handle.join().map_err(|_| "sub thread panicked")?;

    // ---- 汇总 ----
    let h = hist.lock().expect("hist lock");
    let samples = h.len();
    let p50_ns = h.value_at_quantile(0.50);
    let p99_ns = h.value_at_quantile(0.99);
    let p999_ns = h.value_at_quantile(0.999);
    let max_ns = h.max();
    let pass = samples > 0 && p99_ns <= args.p99_threshold_ns && p999_ns <= args.p999_threshold_ns;

    Ok(Report {
        samples,
        p50_ns,
        p99_ns,
        p999_ns,
        max_ns,
        overruns: overruns.load(Ordering::Relaxed),
        drops: drops.load(Ordering::Relaxed),
        pass,
        p99_threshold_ns: args.p99_threshold_ns,
        p999_threshold_ns: args.p999_threshold_ns,
        duration: args.duration,
        hz: args.hz,
    })
}

fn apply_rt(priority: i32, cpu: Option<usize>, strict: bool, tag: &str) {
    #[cfg(target_os = "linux")]
    {
        use robot_core::thread as rt;
        if let Err(e) = rt::set_sched_fifo(priority) {
            assert!(
                !strict,
                "{tag}: set_sched_fifo({priority}) failed strict: {e}"
            );
            eprintln!("warn: {tag}: set_sched_fifo({priority}) failed: {e}");
        }
        if let Err(e) = rt::mlockall() {
            assert!(!strict, "{tag}: mlockall failed strict: {e}");
            eprintln!("warn: {tag}: mlockall failed: {e}");
        }
        if let Some(c) = cpu
            && let Err(e) = rt::pin_to_cpu(c)
        {
            assert!(!strict, "{tag}: pin_to_cpu({c}) failed strict: {e}");
            eprintln!("warn: {tag}: pin_to_cpu({c}) failed: {e}");
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (priority, cpu, strict, tag);
    }
}
