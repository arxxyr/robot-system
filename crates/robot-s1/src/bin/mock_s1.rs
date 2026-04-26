//! `mock_s1` —— v0.2 Part 2 的最小闭环驱动方。
//!
//! ### 职责
//!
//! - 按 `state_hz`（默认 50 Hz）publish `JointCommand`：位置模式 + 多关节正弦轨迹。
//! - 按 `heartbeat_hz`（默认 50 Hz）publish `Heartbeat`（`source_id = 2`，§3.5）。
//!
//! 组合效果：S0 的 `ipc_thread` 收到 heartbeat 把 state 从 `READY` 推到
//! `ENABLED.Active`；收到 command 写 `LatestCommandSlot`；`rt_thread` 把位置 echo 到
//! `JointState`；`pub_thread` 再发出去给 `robotctl` / recorder。
//!
//! ### CLI
//!
//! ```text
//! mock_s1 --robot-id xlerobot \
//!         --hardware-id right_arm \
//!         --joints 3 \
//!         --amplitude 0.3 \
//!         --frequency 0.5 \
//!         --duration 10s
//! ```
//!
//! `--duration` 可省略（无限跑直到 Ctrl+C）。
//!
//! ### 退出码
//!
//! - `0`：正常退出（超时或 Ctrl+C）。
//! - `1`：iceoryx2 open publisher 失败。
//! - `2`：CLI 参数非法。
//!
//! ### 非目标
//!
//! `mock_s1` **不做** trajectory 规划、IK、`CommandMeta` 幂等；这些是 v0.3 真
//! coordinator 的事。本 bin 专注"以最低摩擦驱动 S0 state machine 进入 ENABLED.Active"。

#![forbid(unused_must_use)]

use std::f64::consts::TAU;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::build_helpers::{build_heartbeat, build_joint_command};
use robot_msg::markers;
use robot_msg::msg::{ControlMode, StaleAction};
use robot_transport::iceoryx2::{
    Iceoryx2Publisher, Iceoryx2Subscriber, TopicClass, publisher as open_pub,
    subscriber as open_sub,
};
use robot_transport::traits::{LatestSubscriber, Publisher};

// ============================================================================
// Signal handling
// ============================================================================

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn shutdown_signal_handler(_sig: i32) {
    SHUTDOWN.store(true, Ordering::Release);
}

#[cfg(target_os = "linux")]
fn install_signal_handlers() {
    let h = shutdown_signal_handler as *const () as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, h);
        libc::signal(libc::SIGTERM, h);
    }
}

#[cfg(not(target_os = "linux"))]
fn install_signal_handlers() {
    // macOS/dev 无信号处理；手动 Ctrl+C 通过 panic 触发 Drop，接受退出不干净。
}

// ============================================================================
// CLI
// ============================================================================

struct Args {
    robot_id: String,
    hardware_id: String,
    joints: usize,
    amplitude: f64,
    frequency: f64,
    state_hz: f64,
    heartbeat_hz: f64,
    duration: Option<Duration>,
    layout_id: u64,
    source_id: u32,
    /// 启动时从 S0 state topic 读一次当前 `positions[0..joints]` 作正弦 bias，
    /// 让第一帧 Goal 等于当前位置（motor 不跳）。task 29 commit 5 加。
    /// 0 = 禁用（旧行为，bias 全 0，对未在 center 附近的 motor 会瞬间跳）。
    anchor_timeout_ms: u64,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let env_robot_id = std::env::var("ROBOT_ID").ok();
        let mut args = Self {
            robot_id: env_robot_id.unwrap_or_else(|| "default".into()),
            hardware_id: "default_arm".into(),
            joints: 3,
            amplitude: 0.3,
            frequency: 0.5,
            state_hz: 50.0,
            heartbeat_hz: 50.0,
            duration: None,
            layout_id: 0xCAFE_BABE_DEAD_BEEF,
            source_id: 2, // S1 固定 2（§3.5）
            anchor_timeout_ms: 1000,
        };

        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            match a.as_str() {
                "--robot-id" => {
                    i += 1;
                    args.robot_id
                        .clone_from(raw.get(i).ok_or("--robot-id requires value")?);
                }
                "--hardware-id" => {
                    i += 1;
                    args.hardware_id
                        .clone_from(raw.get(i).ok_or("--hardware-id requires value")?);
                }
                "--joints" => {
                    i += 1;
                    args.joints = raw
                        .get(i)
                        .ok_or("--joints requires value")?
                        .parse()
                        .map_err(|e| format!("--joints: {e}"))?;
                }
                "--amplitude" => {
                    i += 1;
                    args.amplitude = raw
                        .get(i)
                        .ok_or("--amplitude requires value")?
                        .parse()
                        .map_err(|e| format!("--amplitude: {e}"))?;
                }
                "--frequency" => {
                    i += 1;
                    args.frequency = raw
                        .get(i)
                        .ok_or("--frequency requires value")?
                        .parse()
                        .map_err(|e| format!("--frequency: {e}"))?;
                }
                "--state-hz" => {
                    i += 1;
                    args.state_hz = raw
                        .get(i)
                        .ok_or("--state-hz requires value")?
                        .parse()
                        .map_err(|e| format!("--state-hz: {e}"))?;
                }
                "--heartbeat-hz" => {
                    i += 1;
                    args.heartbeat_hz = raw
                        .get(i)
                        .ok_or("--heartbeat-hz requires value")?
                        .parse()
                        .map_err(|e| format!("--heartbeat-hz: {e}"))?;
                }
                "--duration" => {
                    i += 1;
                    args.duration = Some(parse_duration(
                        raw.get(i).ok_or("--duration requires value")?,
                    )?);
                }
                "--anchor-timeout-ms" => {
                    i += 1;
                    args.anchor_timeout_ms = raw
                        .get(i)
                        .ok_or("--anchor-timeout-ms requires value")?
                        .parse()
                        .map_err(|e| format!("--anchor-timeout-ms: {e}"))?;
                }
                "--help" | "-h" => return Err("HELP".into()),
                other => return Err(format!("unknown arg: {other}")),
            }
            i += 1;
        }

        Ok(args)
    }
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    if let Some(rest) = s.strip_suffix("ms") {
        let ms: u64 = rest.parse().map_err(|e| format!("ms: {e}"))?;
        Ok(Duration::from_millis(ms))
    } else if let Some(rest) = s.strip_suffix('s') {
        let secs: u64 = rest.parse().map_err(|e| format!("s: {e}"))?;
        Ok(Duration::from_secs(secs))
    } else {
        let secs: u64 = s.parse().map_err(|e| format!("secs: {e}"))?;
        Ok(Duration::from_secs(secs))
    }
}

fn print_help() {
    eprintln!(
        "mock_s1 (Step 6 Part 2)

USAGE:
  mock_s1 [OPTIONS]

OPTIONS:
  --robot-id <ID>          Robot identifier。优先级：CLI > env ROBOT_ID > \"default\"。
  --hardware-id <ID>       匹配 S0 的 --hardware-id。默认 \"default_arm\"。
  --joints <N>             关节数（默认 3）。
  --amplitude <RAD>        正弦幅值（弧度，默认 0.3）。
  --frequency <HZ>         正弦频率（Hz，默认 0.5 = 2 秒一周期）。
  --state-hz <HZ>          JointCommand 发布频率（默认 50）。
  --heartbeat-hz <HZ>      Heartbeat 发布频率（默认 50）。
  --duration <DUR>         运行时长（3s / 500ms / 10）。省略 = 无限。
  --anchor-timeout-ms <MS> 启动时从 S0 state 读当前位置作正弦 bias 的超时（默认
                           1000 ms）。0 = 禁用（正弦从 rad=0 开始，motor 可能瞬
                           间跳到 center）。task 29 commit 5 加。
  -h, --help               显示此帮助。

TOPICS:
  sub /<robot_id>/s0/<hardware_id>/state  (一次性读当前 positions 作 bias)
  pub /<robot_id>/s1/<hardware_id>/command
  pub /<robot_id>/s1/heartbeat
"
    );
}

// ============================================================================
// main
// ============================================================================

fn main() {
    // task-32：统一 tracing subscriber 初始化（详 robot-logging::init）。
    let _ = robot_logging::init();

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

    if let Err(code) = run(&args) {
        std::process::exit(code);
    }
}

#[allow(clippy::too_many_lines)]
fn run(args: &Args) -> Result<(), i32> {
    let topic_cmd = format!("/{}/s1/{}/command", args.robot_id, args.hardware_id);
    let topic_hb = format!("/{}/s1/heartbeat", args.robot_id);

    // task-32 示范：启动 banner 走 tracing 结构化字段（原 eprintln 见 git
    // blame；其余 eprintln 逐步迁移）。
    tracing::info!(
        robot_id = %args.robot_id,
        hardware_id = %args.hardware_id,
        joints = args.joints,
        amplitude = args.amplitude,
        frequency = args.frequency,
        state_hz = args.state_hz,
        heartbeat_hz = args.heartbeat_hz,
        duration = ?args.duration,
        topic_cmd = %topic_cmd,
        topic_hb = %topic_hb,
        "mock_s1 starting"
    );

    // ---- 构造 iceoryx2 publishers ----
    let node = match NodeBuilder::new().create::<IxService>() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("fatal: iceoryx2 node create: {e:?}");
            return Err(1);
        }
    };
    let mut cmd_pub =
        match open_pub::<markers::JointCommand>(&node, &topic_cmd, TopicClass::S0Command) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("fatal: open command publisher: {e}");
                return Err(1);
            }
        };
    let mut hb_pub = match open_pub::<markers::Heartbeat>(&node, &topic_hb, TopicClass::Heartbeat) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fatal: open heartbeat publisher: {e}");
            return Err(1);
        }
    };

    // ---- 锚定当前位置做正弦 bias（task 29 commit 5）----
    //
    // 防跳 root cause：mock_s1 原先正弦公式是 `amp * sin(phase)`，第一帧 sin(0) = 0
    // rad → Goal = center (raw 2047)。若 motor 当前在闭合位（如夹爪 raw 1400），
    // mock_s1 接管瞬间电机从 1400 跳到 2047 ≈ 36°（"张开到一半"）。
    //
    // 解法：启动时读一次 S0 state topic 的 `positions[0..joints]`，把正弦 bias 设成
    // 当前位置。第一帧 Goal = current_pos + 0 = current_pos → motor 不动。
    let topic_state = format!("/{}/s0/{}/state", args.robot_id, args.hardware_id);
    let bias = if args.anchor_timeout_ms > 0 {
        read_anchor_bias(
            &node,
            &topic_state,
            args.joints,
            Duration::from_millis(args.anchor_timeout_ms),
        )
    } else {
        eprintln!("anchor disabled (--anchor-timeout-ms 0), bias = 0 for all joints");
        vec![0.0; args.joints]
    };

    install_signal_handlers();

    let clock = SystemClock::new();
    let t0_ns = clock.now().0;

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let state_period_ns = (1e9_f64 / args.state_hz) as u64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let hb_period_ns = (1e9_f64 / args.heartbeat_hz) as u64;
    let poll_ns: u64 = 1_000_000; // 1 ms

    let mut last_state_ns = t0_ns;
    let mut last_hb_ns = t0_ns;
    let mut cmd_seq: u64 = 0;
    let mut hb_seq: u64 = 0;
    let mut last_report = std::time::Instant::now();
    let run_start = std::time::Instant::now();

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            eprintln!("signal received → exit");
            break;
        }
        if args.duration.is_some_and(|d| run_start.elapsed() >= d) {
            eprintln!("duration elapsed → exit");
            break;
        }

        let now_ns = clock.now().0;

        // 1) JointCommand 正弦
        if now_ns.saturating_sub(last_state_ns) >= state_period_ns {
            // 两处 precision-loss cast 可接受：纳秒差 → 秒（f64 52-bit mantissa 可表达
            // 2^52 ns ≈ 52 天，我们发布周期内远小于这个上限）；关节索引 usize → f64
            // 仅做相位偏移，N ≤ MAX_JOINTS=32。
            #[allow(clippy::cast_precision_loss)]
            let elapsed_s = (now_ns - t0_ns) as f64 / 1e9;
            let phase = TAU * args.frequency * elapsed_s;
            #[allow(clippy::cast_precision_loss)]
            let target: Vec<f64> = (0..args.joints)
                .map(|i| bias[i] + args.amplitude * (phase + (i as f64) * (TAU / 8.0)).sin())
                .collect();
            let body = build_joint_command(
                args.layout_id,
                ControlMode::Position,
                &target,
                now_ns + 500_000_000, // deadline now + 500 ms
                10,
                StaleAction::Hold,
                cmd_seq,
            );
            publish_body(&mut cmd_pub, &body, cmd_seq, args.source_id, now_ns);
            cmd_seq = cmd_seq.wrapping_add(1);
            last_state_ns = now_ns;
        }

        // 2) Heartbeat
        if now_ns.saturating_sub(last_hb_ns) >= hb_period_ns {
            let body = build_heartbeat(args.source_id, hb_seq, args.heartbeat_hz, now_ns);
            publish_body(&mut hb_pub, &body, hb_seq, args.source_id, now_ns);
            hb_seq = hb_seq.wrapping_add(1);
            last_hb_ns = now_ns;
        }

        if last_report.elapsed() >= Duration::from_secs(2) {
            eprintln!(
                "  cmd_seq={cmd_seq}  hb_seq={hb_seq}  elapsed={:?}",
                run_start.elapsed()
            );
            last_report = std::time::Instant::now();
        }

        thread::sleep(Duration::from_nanos(poll_ns));
    }

    eprintln!(
        "mock_s1 clean shutdown: cmd_seq={cmd_seq} hb_seq={hb_seq} total={:?}",
        run_start.elapsed()
    );
    Ok(())
}

/// 启动时订阅 S0 state topic，轮询直到拿到一帧 `JointState`（或 timeout），
/// 返回 `positions[0..joints]` 作为正弦 bias。
///
/// - 订阅 open 失败 → log + 返回 `vec![0.0; joints]`（fallback，不退出）
/// - 在 `timeout` 内取到 sample → 用 `positions[0..min(joints, state.n)]`；
///   joints 超出 state.n 的部分 bias=0（warning：S0 adapter 不管那些 joint）
/// - timeout → log warning + 返回 `vec![0.0; joints]`
///
/// 该函数**只在启动一次性调用**，不在热路径上，alloc + warning log 都 OK。
fn read_anchor_bias(
    node: &iceoryx2::node::Node<IxService>,
    topic_state: &str,
    joints: usize,
    timeout: Duration,
) -> Vec<f64> {
    let mut state_sub: Iceoryx2Subscriber<markers::JointState> =
        match open_sub(node, topic_state, TopicClass::S0State) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("warn: open state sub for anchor failed: {e} -- bias=0");
                return vec![0.0; joints];
            }
        };

    let poll_interval = Duration::from_millis(10);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match state_sub.latest() {
            Ok(Some(lease)) => match lease.view() {
                Ok(view) => {
                    let js = view.body.get();
                    let Some(pos_vec) = js.position_rad() else {
                        eprintln!("warn: state sample no position_rad field -- bias=0");
                        return vec![0.0; joints];
                    };
                    let mut bias = vec![0.0; joints];
                    let n = usize::min(joints, pos_vec.len());
                    for (i, slot) in bias.iter_mut().take(n).enumerate() {
                        *slot = pos_vec.get(i);
                    }
                    if n < joints {
                        eprintln!(
                            "warn: state has {n} joints, mock_s1 expects {joints}; extra joints bias=0"
                        );
                    }
                    eprintln!("anchored to current S0 state: bias={bias:?}  (first {n} joints)");
                    return bias;
                }
                Err(e) => {
                    eprintln!("warn: state view decode error: {e} -- retry");
                }
            },
            Ok(None) => {
                // 还没 sample，睡会儿再试
            }
            Err(e) => {
                eprintln!("warn: state sub recv error: {e} -- retry");
            }
        }
        if std::time::Instant::now() >= deadline {
            eprintln!(
                "warn: anchor timeout ({timeout:?}) no S0 state sample; bias=0 (motor may jump from current position to rad=0 on first command)"
            );
            return vec![0.0; joints];
        }
        thread::sleep(poll_interval);
    }
}

fn publish_body<T: robot_msg::markers::BodyMarker>(
    pub_: &mut Iceoryx2Publisher<T>,
    body: &[u8],
    seq: u64,
    source_id: u32,
    stamp_ns: u64,
) {
    let upper = 1024 + body.len();
    match pub_.loan(upper) {
        Ok(mut w) => {
            w.set_seq(seq);
            w.set_source_id(source_id);
            w.set_stamp_ns(stamp_ns);
            w.set_body_bytes(body);
            if let Err(e) = w.publish() {
                eprintln!("warn: publish failed: {e}");
            }
        }
        Err(e) => {
            eprintln!("warn: loan failed: {e}");
        }
    }
}
