//! `robotctl` —— 运维 CLI（v0.2 Part 2 最小版）。
//!
//! ### 子命令
//!
//! - `robotctl status`：订阅一次 `JointState` + `Heartbeat`，打印摘要后退出。
//!   找不到最新 sample 则打印 "no sample yet" 退出 0。
//! - `robotctl watch`：每 `--interval-ms`（默认 100 ms）重采样一次；Ctrl+C 退出。
//!
//! ### CLI
//!
//! ```text
//! robotctl status --robot-id xlerobot --hardware-id right_arm
//! robotctl watch  --robot-id xlerobot --hardware-id right_arm --interval-ms 200
//! ```

#![forbid(unused_must_use)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::build_helpers::build_estop_event;
use robot_msg::markers;
use robot_transport::iceoryx2::{
    Iceoryx2Subscriber, TopicClass, publisher as open_pub, subscriber as open_sub,
};
use robot_transport::traits::{LatestSubscriber, Publisher};

// ============================================================================
// Signal handling
// ============================================================================

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// SIGINT / SIGTERM 处理器 —— **直接 `_exit(0)`**。
///
/// 原设计是只 set `SHUTDOWN` flag + 主循环检测 + `process::exit(0)` 跳过 Drop。
/// 实测场景（handoff `2026-04-25_1` §7.6.3 待办 #1）：publisher（`s0_daemon`）先
/// 死 + 几秒后 robotctl 被 SIGTERM → SIGSEGV (exit=139)，crash 在**主循环内**
/// 而不是 Drop 路径 —— 大概率 `iceoryx2 0.8.1` subscriber 内部 critical section
/// 被 signal interrupt 后状态不一致。
///
/// `libc::_exit(0)` 是 **async-signal-safe**（POSIX `signal(7)` 白名单），立刻
/// 退出不走任何清理 —— iceoryx2 shared memory 由 OS 回收，无 leak 影响。
/// `SHUTDOWN` flag 保留以便 Cargo test / 非 Linux 上仍能正常 break 循环。
extern "C" fn shutdown_signal_handler(_sig: i32) {
    SHUTDOWN.store(true, Ordering::Release);
    // 限定 robotctl 这种短命 CLI：crash 场景下 graceful Drop 不可靠，直接
    // _exit 最稳。不要把这个 pattern 复制到 long-lived 进程或 library。
    #[cfg(target_os = "linux")]
    unsafe {
        libc::_exit(0);
    }
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
fn install_signal_handlers() {}

// ============================================================================
// CLI
// ============================================================================

enum Command {
    Status,
    Watch,
    /// **task-30 v9 / task #15**：发一帧 `EStopEvent` 到
    /// `/<robot_id>/events/estop`，让 `s0_daemon` `ipc_thread` `poll_estop` 拿到 →
    /// `ipc_tick` emergency drain → 真硬件 broadcast `Torque_Enable=0`。
    Estop,
}

struct Args {
    command: Command,
    robot_id: String,
    hardware_id: String,
    interval_ms: u64,
    /// `Watch` 子命令的总持续时间（秒）。`None` = 无限跑直到 SIGINT/SIGTERM。
    /// `Some(n)` = `n` 秒后正常退出，方便 runbook 不用 `timeout` 包装。
    watch_duration_secs: Option<u64>,
    /// `Estop` 子命令的 reason 字符串。仅日志 / 诊断用，不影响 transition。
    estop_reason: String,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let env_robot_id = std::env::var("ROBOT_ID").ok();
        let mut raw: Vec<String> = std::env::args().skip(1).collect();

        if raw.is_empty() {
            return Err("HELP".into());
        }
        let sub = raw.remove(0);
        let command = match sub.as_str() {
            "status" => Command::Status,
            "watch" => Command::Watch,
            "estop" => Command::Estop,
            "-h" | "--help" => return Err("HELP".into()),
            other => return Err(format!("unknown subcommand: {other}")),
        };

        let mut args = Self {
            command,
            robot_id: env_robot_id.unwrap_or_else(|| "default".into()),
            hardware_id: "default_arm".into(),
            interval_ms: 100,
            watch_duration_secs: None,
            estop_reason: "robotctl".into(),
        };

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
                "--interval-ms" => {
                    i += 1;
                    args.interval_ms = raw
                        .get(i)
                        .ok_or("--interval-ms requires value")?
                        .parse()
                        .map_err(|e| format!("--interval-ms: {e}"))?;
                }
                "--duration-secs" => {
                    i += 1;
                    let secs: u64 = raw
                        .get(i)
                        .ok_or("--duration-secs requires value")?
                        .parse()
                        .map_err(|e| format!("--duration-secs: {e}"))?;
                    args.watch_duration_secs = Some(secs);
                }
                "--reason" => {
                    i += 1;
                    args.estop_reason
                        .clone_from(raw.get(i).ok_or("--reason requires value")?);
                }
                "-h" | "--help" => return Err("HELP".into()),
                other => return Err(format!("unknown arg: {other}")),
            }
            i += 1;
        }
        Ok(args)
    }
}

fn print_help() {
    eprintln!(
        "robotctl (Step 6 Part 2)

USAGE:
  robotctl <SUBCOMMAND> [OPTIONS]

SUBCOMMANDS:
  status    订阅一次 S0 state + heartbeat，打印摘要退出。
  watch     每 --interval-ms（默认 100）刷一次；Ctrl+C 退出。
  estop     发一帧 EStopEvent 到 /<robot_id>/events/estop，触发 daemon
            emergency drain（软件层 EStop 入口）。

OPTIONS:
  --robot-id <ID>       优先级：CLI > env ROBOT_ID > \"default\"。
  --hardware-id <ID>    默认 \"default_arm\"，匹配 S0 / mock_s1。
  --interval-ms <MS>    watch 刷新间隔（默认 100 ms）。
  --duration-secs <N>   watch 总持续时间（秒）。不传 = 无限跑直到 Ctrl+C。
  --reason <STR>        estop 子命令的 reason 字符串（默认 \"robotctl\"，仅日志）。

TOPICS:
  sub /<robot_id>/s0/<hardware_id>/state
  sub /<robot_id>/s0/heartbeat
  pub /<robot_id>/events/estop      （estop 子命令）
"
    );
}

// ============================================================================
// main
// ============================================================================

fn main() {
    // task-32：统一 tracing subscriber 初始化。
    let _ = robot_logging::init();

    // v0.5 候选 A：入口分流（Q-CLI2(b) / AC-2）。已落地的 gateway 子命令
    // （async lane，见 robot_cli::dispatch::is_dispatchable_async）走 tokio
    // runtime；其余（既有 sync 3 子命令 status/watch/estop + 未落地 async 名
    // + 未知名 + 无参）落到下面既有手写 parser，行为 0 改动。
    let cmdline: Vec<String> = std::env::args().skip(1).collect();
    if let Some(name) = cmdline.first()
        && robot_cli::dispatch::is_dispatchable_async(name)
    {
        let code = robot_cli::block_on_async(robot_cli::dispatch::run_async(name, &cmdline[1..]));
        std::process::exit(i32::from(code));
    }

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

fn run(args: &Args) -> Result<(), i32> {
    // estop 子命令完全独立路径：只 open publisher，不 open state / hb subscribers。
    if matches!(args.command, Command::Estop) {
        return run_estop(args);
    }

    let topic_state = format!("/{}/s0/{}/state", args.robot_id, args.hardware_id);
    let topic_hb = format!("/{}/s0/heartbeat", args.robot_id);

    eprintln!(
        "robotctl: robot_id={}  hardware_id={}\n  sub state : {}\n  sub hb    : {}",
        args.robot_id, args.hardware_id, topic_state, topic_hb,
    );

    let node = match NodeBuilder::new().create::<IxService>() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("fatal: iceoryx2 node create: {e:?}");
            return Err(1);
        }
    };

    let mut state_sub =
        match open_sub::<markers::JointState>(&node, &topic_state, TopicClass::S0State) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("fatal: open state subscriber: {e}");
                return Err(1);
            }
        };
    let mut hb_sub = match open_sub::<markers::Heartbeat>(&node, &topic_hb, TopicClass::Heartbeat) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fatal: open heartbeat subscriber: {e}");
            return Err(1);
        }
    };

    install_signal_handlers();

    match args.command {
        Command::Estop => unreachable!("handled above"),
        Command::Status => {
            print_snapshot(&mut state_sub, &mut hb_sub);
            Ok(())
        }
        Command::Watch => {
            // **task #15 阻断 2 修复**：可选 `--duration-secs` 让 runbook /
            // CI 跑固定窗口的 watch 不用外面套 `timeout`；不传 = 老语义
            // (Ctrl+C 退出)。
            let started = std::time::Instant::now();
            let limit = args.watch_duration_secs.map(Duration::from_secs);
            while !SHUTDOWN.load(Ordering::Acquire) {
                print_snapshot(&mut state_sub, &mut hb_sub);
                if let Some(d) = limit
                    && started.elapsed() >= d
                {
                    eprintln!("watch duration {} s elapsed → exit", d.as_secs());
                    break;
                }
                thread::sleep(Duration::from_millis(args.interval_ms));
            }
            if SHUTDOWN.load(Ordering::Acquire) {
                eprintln!("signal received → exit");
            }
            // task-30 follow-up（handoff 2026-04-25_1 §3.6）：iceoryx2 0.8.1
            // subscriber Drop 路径在 publisher（如 s0_daemon）已先退出时访问
            // 已释放的 shared memory 会 SIGSEGV（exit=139）。这是 iceoryx2 内部
            // race，修框架超 scope —— 短寿命 CLI 工具接受 **跳过 Drop** 直接
            // `process::exit(0)` 的妥协（iceoryx2 shared-memory 资源由 OS 回收，
            // 无 leak 影响）。
            //
            // **不要**把这个 pattern 复制到 long-lived 进程或 library；只限
            // 单次运行、立刻退出的 CLI 子命令。
            std::process::exit(0);
        }
    }
}

/// **task-30 v9 / task #15**：发一帧 `EStopEvent` 到
/// `/<robot_id>/events/estop`。
///
/// `s0_daemon` `ipc_thread` `poll_estop` 收到 → `ipc_tick` emergency drain →
/// `executor_ctrl.safety_stop()`（client 同步 `policy=StopTorque`,
/// `safety_latched=true`, `safety_state=StopCommanded`）→ rt/serial 端 swap
/// `emergency_action` 拿到 `SAFETY_STOP` → adapter `safety_stop_fast`
/// （Feetech 真硬件 = broadcast `Torque_Enable=0`，< 10 ms）→ 下 tick
/// `confirm_torque_off` → wire `S0SafetyState=TorqueOffConfirmed`。
///
/// daemon 退出时 latch 文件被写为 `1`，下次启动 `estop_latched=true`。
fn run_estop(args: &Args) -> Result<(), i32> {
    let topic_estop = format!("/{}/events/estop", args.robot_id);
    eprintln!(
        "robotctl estop: robot_id={}  reason={:?}\n  pub estop : {}",
        args.robot_id, args.estop_reason, topic_estop,
    );

    let node = match NodeBuilder::new().create::<IxService>() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("fatal: iceoryx2 node create: {e:?}");
            return Err(1);
        }
    };

    let mut estop_pub =
        match open_pub::<markers::EStopEvent>(&node, &topic_estop, TopicClass::SafetyEvent) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("fatal: open estop publisher: {e}");
                return Err(1);
            }
        };

    let clock = SystemClock::new();
    let stamp_ns = clock.now().0;
    // build_estop_event(reason, stamp_ns, source, latched)
    let body = build_estop_event(&args.estop_reason, stamp_ns, "robotctl", true);
    let upper = 1024 + body.len();

    let mut writer = match estop_pub.loan(upper) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("fatal: loan estop sample: {e}");
            return Err(1);
        }
    };
    writer.set_seq(1);
    writer.set_source_id(99); // robotctl 约定 source_id=99
    writer.set_stamp_ns(stamp_ns);
    writer.set_body_bytes(&body);
    if let Err(e) = writer.publish() {
        eprintln!("fatal: publish estop: {e}");
        return Err(1);
    }

    eprintln!(
        "robotctl estop: published 1 EStopEvent (stamp_ns={stamp_ns}); daemon should \
         emergency-drain within next ipc tick + serial/rt tick (≈ 10–50 ms)."
    );

    // **handoff 2026-04-25_1 §3.6**：iceoryx2 0.8.1 publisher Drop 路径在
    // service 中其它端先退出时可能 SIGSEGV。短命 CLI 跳 Drop 直 _exit。
    #[cfg(target_os = "linux")]
    unsafe {
        libc::_exit(0);
    }
    #[cfg(not(target_os = "linux"))]
    Ok(())
}

fn print_snapshot(
    state_sub: &mut Iceoryx2Subscriber<markers::JointState>,
    hb_sub: &mut Iceoryx2Subscriber<markers::Heartbeat>,
) {
    // state
    let state_line = match state_sub.latest() {
        Ok(Some(lease)) => match lease.view() {
            Ok(view) => {
                let js = view.body.get();
                let positions: Vec<f64> = js
                    .position_rad()
                    .map_or_else(Vec::new, |v| v.iter().collect());
                // task-30 v9 pre-2：附 S0 mode + safety_state + fault_code 摘要
                format!(
                    "state: stamp={}  layout={:#018x}  n={}  pos={:.3?}\n\
                     s0:    mode={:?}  link={:?}  out={:?}  safety={:?}  fault={:?}",
                    js.stamp_ns(),
                    js.layout_id(),
                    positions.len(),
                    positions,
                    js.s0_mode(),
                    js.s0_control_link(),
                    js.s0_output_mode(),
                    js.s0_safety_state(),
                    js.s0_fault_code(),
                )
            }
            Err(e) => format!("state: decode error: {e}"),
        },
        Ok(None) => "state: (no sample yet)".into(),
        Err(e) => format!("state: receive error: {e}"),
    };

    // heartbeat
    let hb_line = match hb_sub.latest() {
        Ok(Some(lease)) => match lease.view() {
            Ok(view) => {
                let hb = view.body.get();
                format!(
                    "hb:    src={}  seq={}  rate={:.1} Hz  stamp={}",
                    hb.source_id(),
                    hb.seq(),
                    hb.tick_rate_hz(),
                    hb.stamp_ns(),
                )
            }
            Err(e) => format!("hb: decode error: {e}"),
        },
        Ok(None) => "hb:    (no sample yet)".into(),
        Err(e) => format!("hb: receive error: {e}"),
    };

    println!("{state_line}\n{hb_line}\n");
}
