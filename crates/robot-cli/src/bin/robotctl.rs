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
use robot_msg::markers;
use robot_transport::iceoryx2::{Iceoryx2Subscriber, TopicClass, subscriber as open_sub};
use robot_transport::traits::LatestSubscriber;

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
}

struct Args {
    command: Command,
    robot_id: String,
    hardware_id: String,
    interval_ms: u64,
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
            "-h" | "--help" => return Err("HELP".into()),
            other => return Err(format!("unknown subcommand: {other}")),
        };

        let mut args = Self {
            command,
            robot_id: env_robot_id.unwrap_or_else(|| "default".into()),
            hardware_id: "default_arm".into(),
            interval_ms: 100,
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

OPTIONS:
  --robot-id <ID>       优先级：CLI > env ROBOT_ID > \"default\"。
  --hardware-id <ID>    默认 \"default_arm\"，匹配 S0 / mock_s1。
  --interval-ms <MS>    watch 刷新间隔（默认 100 ms）。

TOPICS:
  sub /<robot_id>/s0/<hardware_id>/state
  sub /<robot_id>/s0/heartbeat
"
    );
}

// ============================================================================
// main
// ============================================================================

fn main() {
    // task-32：统一 tracing subscriber 初始化。
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

fn run(args: &Args) -> Result<(), i32> {
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
    let mut hb_sub = match open_sub::<markers::Heartbeat>(&node, &topic_hb, TopicClass::S1CmdVel) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fatal: open heartbeat subscriber: {e}");
            return Err(1);
        }
    };

    install_signal_handlers();

    match args.command {
        Command::Status => {
            print_snapshot(&mut state_sub, &mut hb_sub);
            Ok(())
        }
        Command::Watch => {
            while !SHUTDOWN.load(Ordering::Acquire) {
                print_snapshot(&mut state_sub, &mut hb_sub);
                thread::sleep(Duration::from_millis(args.interval_ms));
            }
            eprintln!("signal received → exit");
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
                    .map(|v| v.iter().collect())
                    .unwrap_or_default();
                format!(
                    "state: stamp={}  layout={:#018x}  n={}  pos={:.3?}",
                    js.stamp_ns(),
                    js.layout_id(),
                    positions.len(),
                    positions,
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
