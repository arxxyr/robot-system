//! `mock_s1` —— **ballet test tool only, NOT production S1**（B1 (3/3) 起）。
//!
//! ### 定位（v0.3 起）
//!
//! v0.3 production S1 = `s1_daemon`（[`crates/robot-s1/src/bin/s1_daemon.rs`](
//! ../s1_daemon/index.html)）：启动期锚定 + 50 Hz `target == anchor` hold
//! command + 50 Hz heartbeat，无副作用。`mock_s1` 保留是因为它的多关节
//! 正弦摆动 + `--frozen-joints` mask + `--amplitude` / `--frequency` CLI 是
//! 真机 ballet 验证（task #15 Stage 1/2 在 Orin 跑 6/9-motor 摆动）的唯一
//! 入口，`s1_daemon` 的 hold 用例**无法替代**。
//!
//! - **production / 默认 smoke**：用 `s1_daemon`；`crash_restart_smoke`
//!   默认就走它。
//! - **ballet / bench / 真机摆动测试**：用 `mock_s1`；
//!   `crash_restart_smoke --use-mock-s1` 是兼容入口。
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
//!         --config configs/<robot>.toml \
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
//! `mock_s1` **不做** trajectory 规划、IK、`CommandMeta` 幂等 / hold mode
//! 安全收敛；这些是 `s1_daemon` (B1) + Phase B B2/B3/B4 后续工作项的事。
//! 本 bin 专注"以最低摩擦提供多关节 ballet 驱动 + 推 S0 state machine 进入
//! `ENABLED.Active`"，是测试工具而非生产组件。

#![forbid(unused_must_use)]

use std::f64::consts::TAU;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use std::path::PathBuf;

use robot_core::time::{RobotClock, SystemClock};
use robot_msg::build_helpers::{build_heartbeat, build_joint_command};
use robot_msg::joint_layout::compute_layout_id;
use robot_msg::markers;
use robot_msg::msg::{ControlMode, StaleAction};
use robot_s1::coordinator;
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
    /// 段(4) PC4：--config 必填——joints 数与 layout 均由 config 派生
    /// （PD-E5(a) 唯一权威）。
    config: PathBuf,
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
    /// 冻结 joint 集合（mask 模式）：这些 joint 在每帧 command 里始终发
    /// `bias[i]`（即 anchor 时读到的当前位置），**不**叠加正弦摆动。
    ///
    /// 用途：bus 上同时挂"要 ballet 的关节"+"不该被 ballet 撞坏的关节"时，
    /// 例如 task #15 Stage 2（bus2 9-motor：右臂 6 摆 + base 3 wheel 冻结）。
    /// 冻结关节仍然走 `sync_write` 的 `Goal_Position`（adapter 没有"按 joint
    /// 跳过 write"的接口），但 target 永远 = bias，电机原地不动。
    frozen_joints: Vec<usize>,
}

impl Args {
    #[allow(clippy::too_many_lines)] // 大段 if-else CLI 解析；分函数会更绕。
    fn parse() -> Result<Self, String> {
        let env_robot_id = std::env::var("ROBOT_ID").ok();
        let mut args = Self {
            robot_id: env_robot_id.unwrap_or_else(|| "default".into()),
            hardware_id: "default_arm".into(),
            config: PathBuf::new(),
            joints: 3,
            amplitude: 0.3,
            frequency: 0.5,
            state_hz: 50.0,
            heartbeat_hz: 50.0,
            duration: None,
            layout_id: 0xCAFE_BABE_DEAD_BEEF,
            source_id: 2, // S1 固定 2（§3.5）
            anchor_timeout_ms: 1000,
            frozen_joints: Vec::new(),
        };

        // 段(4)：--joints 显式传值仅与 config 派生数做一致性断言。
        let mut explicit_joints = false;
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
                "--config" => {
                    i += 1;
                    args.config = PathBuf::from(raw.get(i).ok_or("--config requires value")?);
                }
                "--joints" => {
                    i += 1;
                    args.joints = raw
                        .get(i)
                        .ok_or("--joints requires value")?
                        .parse()
                        .map_err(|e| format!("--joints: {e}"))?;
                    explicit_joints = true;
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
                "--frozen-joints" => {
                    i += 1;
                    args.frozen_joints =
                        parse_joint_indices(raw.get(i).ok_or("--frozen-joints requires value")?)?;
                }
                "--help" | "-h" => return Err("HELP".into()),
                other => return Err(format!("unknown arg: {other}")),
            }
            i += 1;
        }

        // ---- 段(4) PC4：--config 必填 + config 派生（joints / layout）----
        //
        // mock_s1 是测试工具：本块所有失败统一走 parse Err → exit 2（不细分
        // config 错误码族；生产语义在 s1_daemon）。
        if args.config.as_os_str().is_empty() {
            return Err("--config is required（段(4) PC4：joints/layout 由 config 派生）".into());
        }
        let toml_text = std::fs::read_to_string(&args.config)
            .map_err(|e| format!("read --config {}: {e}", args.config.display()))?;
        let cfg_robot_id = coordinator::parse_robot_id_from_toml(&toml_text)
            .map_err(|e| format!("parse [robot].id: {e}"))?;
        if cfg_robot_id != args.robot_id {
            return Err(format!(
                "[robot].id '{cfg_robot_id}' != --robot-id '{}' (config/flag mismatch; 会订阅错 topic)",
                args.robot_id
            ));
        }
        let hardware = coordinator::parse_hardware_section_from_toml(&toml_text, &args.hardware_id)
            .map_err(|e| format!("parse [[hardware]]: {e}"))?;
        if explicit_joints && args.joints != hardware.joints.len() {
            return Err(format!(
                "--joints {} != config 派生关节数 {}（显式传值仅一致性断言）",
                args.joints,
                hardware.joints.len()
            ));
        }
        args.joints = hardware.joints.len();
        let pairs: Vec<(&str, u32)> = hardware
            .joints
            .iter()
            .map(String::as_str)
            .zip(hardware.motor_ids.iter().map(|&m| u32::from(m)))
            .collect();
        args.layout_id = compute_layout_id(&args.robot_id, &args.hardware_id, &pairs);

        // --frozen-joints 越界 / 重复检查（必须在 --joints 之后做；CLI 解析顺
        // 序由用户决定，这里在 parse 完后统一校验）。
        for &idx in &args.frozen_joints {
            if idx >= args.joints {
                return Err(format!(
                    "--frozen-joints: index {idx} out of range (joints={})",
                    args.joints
                ));
            }
        }

        Ok(args)
    }
}

/// 解析 `--frozen-joints "0,3,5"` 格式：逗号分隔的 joint 下标（**0-based**）。
///
/// - 空字符串 → 空 Vec（**等价**于不传 `--frozen-joints`，所有关节都摆动）。
/// - 允许重复（去重后再用）。
/// - 越界检查在 `Args::parse` 末尾统一做（这里不知道 `joints`）。
fn parse_joint_indices(s: &str) -> Result<Vec<usize>, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<usize> = Vec::new();
    for part in trimmed.split(',') {
        let p = part.trim();
        if p.is_empty() {
            return Err(format!("--frozen-joints: empty entry in '{s}'"));
        }
        let idx: usize = p
            .parse()
            .map_err(|e| format!("--frozen-joints: '{p}' not a usize: {e}"))?;
        if !out.contains(&idx) {
            out.push(idx);
        }
    }
    Ok(out)
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
  --config <PATH>          必填（段(4) PC4）：与 daemon 同款 TOML；joints 数
                           与 layout_id 由 config 派生（PD-E5(a) 唯一权威）。
  --joints <N>             显式传值仅与 config 派生数一致性断言（不等 = 报错）。
  --amplitude <RAD>        正弦幅值（弧度，默认 0.3）。
  --frequency <HZ>         正弦频率（Hz，默认 0.5 = 2 秒一周期）。
  --state-hz <HZ>          JointCommand 发布频率（默认 50）。
  --heartbeat-hz <HZ>      Heartbeat 发布频率（默认 50）。
  --duration <DUR>         运行时长（3s / 500ms / 10）。省略 = 无限。
  --anchor-timeout-ms <MS> 启动时从 S0 state 读当前位置作正弦 bias 的超时（默认
                           1000 ms）。0 = 禁用（正弦从 rad=0 开始，motor 可能瞬
                           间跳到 center）。task 29 commit 5 加。
  --frozen-joints <LIST>   逗号分隔的 0-based joint 下标，这些 joint 每帧 target
                           = bias[i]（不叠加正弦）。其它 joint 仍按 sin 摆动。
                           例：'--joints 9 --frozen-joints 6,7,8' = 前 6 摆 +
                           后 3 冻结。task #15 Stage 2 加（bus2 9-motor 同栈）。
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
        frozen_joints = ?args.frozen_joints,
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
                .map(|i| {
                    if args.frozen_joints.contains(&i) {
                        bias[i]
                    } else {
                        bias[i] + args.amplitude * (phase + (i as f64) * (TAU / 8.0)).sin()
                    }
                })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_joint_indices_empty_returns_empty_vec() {
        assert_eq!(parse_joint_indices("").unwrap(), Vec::<usize>::new());
        assert_eq!(parse_joint_indices("   ").unwrap(), Vec::<usize>::new());
    }

    #[test]
    fn parse_joint_indices_single_index() {
        assert_eq!(parse_joint_indices("3").unwrap(), vec![3]);
    }

    #[test]
    fn parse_joint_indices_multiple_with_whitespace() {
        assert_eq!(parse_joint_indices("6, 7, 8").unwrap(), vec![6, 7, 8]);
    }

    #[test]
    fn parse_joint_indices_dedup_preserves_first_occurrence_order() {
        // 重复入参允许，去重后保留首次出现顺序（不稳定排序避免，按用户输入直观）。
        assert_eq!(parse_joint_indices("0,3,0,1,3").unwrap(), vec![0, 3, 1]);
    }

    #[test]
    fn parse_joint_indices_empty_entry_is_error() {
        let err = parse_joint_indices("0,,3").unwrap_err();
        assert!(err.contains("empty entry"), "unexpected: {err}");
    }

    #[test]
    fn parse_joint_indices_non_numeric_is_error() {
        let err = parse_joint_indices("0,foo,2").unwrap_err();
        assert!(err.contains("not a usize"), "unexpected: {err}");
    }
}
