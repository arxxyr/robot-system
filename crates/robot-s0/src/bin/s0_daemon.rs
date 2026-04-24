//! `s0_daemon` —— Step 6 Part 2 完整 S0 守护进程。
//!
//! ### 启动顺序
//!
//! 1. 解析 CLI（`--robot-id` / `--hardware-id` / `--latch-path` /
//!    `--heartbeat-timeout-ms` / `--smoke-duration` 等）
//! 2. 读 `EStop` latch 文件 → 若持久化为 `true` 则 `initial_guards.estop_latched = true`
//! 3. 构造共享 channel / atomic / seqlock / latest-command slot
//! 4. 按顺序 spawn：
//!    - `ipc_thread`（iceoryx2 subscribers 在内部构造，factory closure 同步）
//!    - `watchdog`（读 `ipc_thread`.`last_hb_ns`，超时发 `HeartbeatTimeout` 到 `wd_tx`）
//!    - `rt_thread`（`SCHED_FIFO` 70 + `mlockall`；`strict_rt` 可选）
//!    - `pub_thread`（iceoryx2 publishers 在内部构造）
//!    - `io_thread`（no-op 占位）
//! 5. 注册 `SIGINT` / `SIGTERM` handler → 原子 shutdown flag
//! 6. 主 loop 等：`smoke_duration` 到期 / shutdown flag 触发 / state 进入 `STOPPING`
//! 7. 发 `Event::Stop` 给 `ipc_thread`（让 state machine 真正走进 `STOPPING`），
//!    `request_stop` 所有线程，`join`，保存 `EStop latch`（若 `estop_latched`=true）
//!
//! ### CLI
//!
//! ```text
//! s0_daemon --robot-id xlerobot \
//!           --hardware-id right_arm \
//!           --heartbeat-timeout-ms 200 \
//!           --latch-path /var/lib/robot/xlerobot/estop_latch \
//!           --smoke-duration 10s
//! ```
//!
//! `robot_id` 来源（架构决策 ②）：`--robot-id` > 环境变量 `ROBOT_ID` > `"default"`。
//!
//! ### 退出码
//!
//! - `0`：正常 shutdown（到 `STOPPING` 或 `smoke_duration` 到期）。
//! - `1`：线程启动失败 / 运行中 panic / RT 线程零 tick。
//! - `2`：CLI 参数解析失败。

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::unbounded;
use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::adapter::{JointCommandBuf, ManipulatorAdapter, RtSnapshot};
use robot_core::config::IdentityFingerprint;
use robot_core::factory::{AdapterBuildContext, find_factory, registered_factories};
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::markers;
use robot_s0::config_loader::{LoadedConfig, load_robot_config};
use robot_s0::estop_latch::{load_estop_latch, save_estop_latch};
use robot_s0::io_thread::{IoThreadConfig, spawn_io_thread};
use robot_s0::ipc_thread::{IpcSubscribers, IpcThreadConfig, IpcThreadHandle, spawn_ipc_thread};
use robot_s0::pub_thread::{IpcPublishers, PubThreadConfig, spawn_pub_thread};
use robot_s0::rt_thread::{RtThreadConfig, spawn_rt_thread};
use robot_s0::state_machine::{Event, Guards, S0State, S0StateSnapshot};
use robot_s0::watchdog::{WatchdogConfig, spawn_watchdog};
use robot_transport::iceoryx2::{TopicClass, publisher as open_pub, subscriber as open_sub};
use robot_transport::{LatestCommandSlot, RtSnapshotPublisher};

// ============================================================================
// 全局 shutdown 原子（signal handler 写，main 循环读）
// ============================================================================

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn shutdown_signal_handler(_sig: i32) {
    // signal-safe：只写 AtomicBool，不做堆分配、不做任何 async-signal-unsafe 操作。
    SHUTDOWN.store(true, Ordering::Release);
}

fn install_signal_handlers() {
    // SA_RESTART 留默认；SIGINT / SIGTERM 都走同一 handler，设置 SHUTDOWN flag。
    // SAFETY: libc signal FFI；handler 函数签名匹配 `extern "C" fn(i32)`；
    //         handler 内部只写 AtomicBool，满足 async-signal-safety。
    // 双 cast：function item → thin pointer → libc::sighandler_t（= usize）。
    // 直接 `as usize` 在新 rustc 下会 warn `function_casts_as_integer`。
    let h = shutdown_signal_handler as *const () as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, h);
        libc::signal(libc::SIGTERM, h);
    }
}

// ============================================================================
// CLI
// ============================================================================

struct Args {
    /// CLI 显式指定时为 `Some`；否则从 `--config` TOML / env / "default" 回填。
    robot_id_cli: Option<String>,
    /// CLI 显式指定时为 `Some`；否则从 `--config` 第一段 hardware / `"default_arm"` 回填。
    hardware_id_cli: Option<String>,
    /// CLI 显式指定时为 `Some`；否则按 `robot_id` 回填 `/var/lib/robot/<id>/estop_latch`。
    latch_path_cli: Option<PathBuf>,
    config_path: Option<PathBuf>,
    heartbeat_timeout_ns: u64,
    smoke_duration: Option<Duration>,
    tick_period_ns: u64,
    strict_rt: bool,
    joint_count: u8,
    cpu_affinity: Option<usize>,
}

/// CLI 解析完 + `--config` 加载完之后的最终配置（CLI override 优先级已应用）。
struct ResolvedArgs {
    robot_id: String,
    hardware_id: String,
    latch_path: PathBuf,
    heartbeat_timeout_ns: u64,
    smoke_duration: Option<Duration>,
    tick_period_ns: u64,
    strict_rt: bool,
    joint_count: u8,
    cpu_affinity: Option<usize>,
    /// `--config` 加载的完整配置（含 ConfigIdentity），无 `--config` 时为 None。
    loaded_config: Option<LoadedConfig>,
    /// v0.2 partial fingerprint：`--config` 模式下来自 TOML，否则按 CLI 现场算
    /// 一个不带 hash 的 stub（仅 Display 用，跨进程 handshake 推 v0.3）。
    identity: IdentityFingerprint,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut args = Self {
            robot_id_cli: None,
            hardware_id_cli: None,
            latch_path_cli: None,
            config_path: None,
            heartbeat_timeout_ns: 200_000_000, // 200 ms
            smoke_duration: None,
            tick_period_ns: 20_000_000, // 50 Hz
            strict_rt: false,
            joint_count: 3,
            cpu_affinity: None,
        };

        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            match a.as_str() {
                "--robot-id" => {
                    i += 1;
                    args.robot_id_cli =
                        Some(raw.get(i).ok_or("--robot-id requires value")?.clone());
                }
                "--hardware-id" => {
                    i += 1;
                    args.hardware_id_cli =
                        Some(raw.get(i).ok_or("--hardware-id requires value")?.clone());
                }
                "--latch-path" => {
                    i += 1;
                    args.latch_path_cli = Some(PathBuf::from(
                        raw.get(i).ok_or("--latch-path requires value")?,
                    ));
                }
                "--config" => {
                    i += 1;
                    args.config_path =
                        Some(PathBuf::from(raw.get(i).ok_or("--config requires value")?));
                }
                "--heartbeat-timeout-ms" => {
                    i += 1;
                    let ms: u64 = raw
                        .get(i)
                        .ok_or("--heartbeat-timeout-ms requires value")?
                        .parse()
                        .map_err(|e| format!("--heartbeat-timeout-ms: {e}"))?;
                    args.heartbeat_timeout_ns = ms.saturating_mul(1_000_000);
                }
                "--smoke-duration" => {
                    i += 1;
                    args.smoke_duration = Some(parse_duration(
                        raw.get(i).ok_or("--smoke-duration requires value")?,
                    )?);
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
                "--help" | "-h" => return Err("HELP".into()),
                other => return Err(format!("unknown arg: {other}")),
            }
            i += 1;
        }

        Ok(args)
    }

    /// 应用 *CLI > TOML > env > 默认* 优先级，加载 `--config`（如有），算
    /// ConfigIdentity，回填默认。
    ///
    /// 优先级链：
    /// - `robot_id`：CLI `--robot-id` > TOML `[robot] id` > env `ROBOT_ID` > `"default"`
    /// - `hardware_id`：CLI `--hardware-id` > TOML 第一段 `[[hardware]] id` > `"default_arm"`
    /// - `latch_path`：CLI `--latch-path` > `/var/lib/robot/<robot_id>/estop_latch`
    fn resolve(self) -> Result<ResolvedArgs, String> {
        let loaded_config = match &self.config_path {
            Some(path) => Some(load_robot_config(path).map_err(|e| e.to_string())?),
            None => None,
        };

        let env_robot_id = std::env::var("ROBOT_ID").ok();

        let robot_id = self
            .robot_id_cli
            .clone()
            .or_else(|| loaded_config.as_ref().map(|c| c.parsed.robot.id.clone()))
            .or(env_robot_id)
            .unwrap_or_else(|| "default".into());

        let hardware_id = self.hardware_id_cli.clone().unwrap_or_else(|| {
            loaded_config
                .as_ref()
                .and_then(|c| c.parsed.hardware.first().map(|h| h.id.clone()))
                .unwrap_or_else(|| "default_arm".into())
        });

        let latch_path = self
            .latch_path_cli
            .clone()
            .unwrap_or_else(|| PathBuf::from(format!("/var/lib/robot/{robot_id}/estop_latch")));

        // identity：有 TOML 用 TOML 的；无 TOML 时构造 stub（hash = blake3(空)），
        // robot_id / config_version 取 CLI 现场值。stub 仅供 Display；v0.3 跨进程
        // handshake 强制要求 --config，到时移除 stub 路径。
        let identity = loaded_config.as_ref().map_or_else(
            || {
                IdentityFingerprint::v02_partial(
                    robot_id.clone(),
                    "cli-only",
                    *blake3::hash(b"").as_bytes(),
                )
            },
            |c| c.identity.clone(),
        );

        Ok(ResolvedArgs {
            robot_id,
            hardware_id,
            latch_path,
            heartbeat_timeout_ns: self.heartbeat_timeout_ns,
            smoke_duration: self.smoke_duration,
            tick_period_ns: self.tick_period_ns,
            strict_rt: self.strict_rt,
            joint_count: self.joint_count,
            cpu_affinity: self.cpu_affinity,
            loaded_config,
            identity,
        })
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
        "s0_daemon (Step 7)

USAGE:
  s0_daemon [OPTIONS]

OPTIONS:
  --config <PATH>              configs/<robot>.toml。CLI > TOML > env > 默认（task 25）。
  --robot-id <ID>              Robot identifier。优先级：CLI > TOML > env ROBOT_ID > \"default\"。
  --hardware-id <ID>           本实例控制的硬件（如 \"left_arm\"）。优先级：CLI > TOML 首段 > \"default_arm\"。
  --latch-path <PATH>          EStop latch 文件路径。默认 /var/lib/robot/<robot_id>/estop_latch。
  --heartbeat-timeout-ms <MS>  S1 heartbeat 超时阈值（毫秒）。默认 200。
  --smoke-duration <DUR>       运行时长上限（3s / 500ms / 10 均可）。默认无限（直到 signal）。
  --tick-hz <HZ>               rt_thread tick 频率。默认 50。
  --joints <N>                 mock adapter 关节数。默认 3。
  --strict-rt                  RT syscall 失败立即 panic（生产推荐）。
  --cpu <IDX>                  rt_thread CPU 亲和性。默认不绑核。
  -h, --help                   显示帮助。

TOPICS（iceoryx2）:
  订阅：
    /<robot_id>/s1/<hardware_id>/command
    /<robot_id>/s1/heartbeat
    /<robot_id>/events/estop
  发布：
    /<robot_id>/s0/<hardware_id>/state
    /<robot_id>/s0/heartbeat
"
    );
}

fn topics(args: &ResolvedArgs) -> Topics {
    let r = &args.robot_id;
    let h = &args.hardware_id;
    Topics {
        sub_command: format!("/{r}/s1/{h}/command"),
        sub_heartbeat: format!("/{r}/s1/heartbeat"),
        sub_estop: format!("/{r}/events/estop"),
        pub_state: format!("/{r}/s0/{h}/state"),
        pub_heartbeat: format!("/{r}/s0/heartbeat"),
    }
}

struct Topics {
    sub_command: String,
    sub_heartbeat: String,
    sub_estop: String,
    pub_state: String,
    pub_heartbeat: String,
}

// ============================================================================
// main
// ============================================================================

fn main() {
    let parsed = match Args::parse() {
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

    let resolved = match parsed.resolve() {
        Ok(r) => r,
        Err(msg) => {
            eprintln!("error: {msg}");
            std::process::exit(2);
        }
    };

    if let Err(code) = run(&resolved) {
        std::process::exit(code);
    }
}

#[allow(clippy::too_many_lines)]
fn run(args: &ResolvedArgs) -> Result<(), i32> {
    let t = topics(args);

    eprintln!(
        "s0_daemon starting:\n  \
         robot_id={}  hardware_id={}\n  \
         latch={}\n  \
         hb_timeout_ms={}  tick_hz={}  strict_rt={}  joints={}  cpu={:?}",
        args.robot_id,
        args.hardware_id,
        args.latch_path.display(),
        args.heartbeat_timeout_ns / 1_000_000,
        1_000_000_000 / args.tick_period_ns,
        args.strict_rt,
        args.joint_count,
        args.cpu_affinity,
    );
    if let Some(cfg) = &args.loaded_config {
        eprintln!(
            "config       : {} ({} hardware, {} sensor sections)",
            cfg.source_path.display(),
            cfg.parsed.hardware.len(),
            cfg.parsed.sensor.len(),
        );
    } else {
        eprintln!("config       : <none> (CLI-only mode; identity is stub)");
    }
    eprintln!("identity     : {}", args.identity);
    eprintln!("topics:");
    eprintln!("  sub cmd   : {}", t.sub_command);
    eprintln!("  sub hb    : {}", t.sub_heartbeat);
    eprintln!("  sub estop : {}", t.sub_estop);
    eprintln!("  pub state : {}", t.pub_state);
    eprintln!("  pub hb    : {}", t.pub_heartbeat);

    // ---- 1) EStop latch ----
    let initial_estop_latched = match load_estop_latch(&args.latch_path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "fatal: failed to load EStop latch {}: {e}",
                args.latch_path.display()
            );
            return Err(1);
        }
    };
    eprintln!("initial estop_latched = {initial_estop_latched}");

    // ---- 2) 共享 channel / atomic / slot ----
    let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());

    let (cmd_writer, cmd_reader) =
        LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());

    let (rt_snap_pub, rt_snap_rdr) = RtSnapshotPublisher::<RtSnapshot>::new(RtSnapshot::default());
    let (state_pub, state_rdr) =
        RtSnapshotPublisher::<S0StateSnapshot>::new(S0StateSnapshot::default());

    let (wd_tx, wd_rx) = unbounded::<Event>();

    // ---- 3) spawn ipc_thread ----
    let mut initial_guards = Guards {
        estop_latched: initial_estop_latched,
        adapter_connected: true,
        calibration_loaded: true,
        joint_state_valid: true,
        power_ok: true,
        heartbeat_ok: false,
        cmd_deadline_valid: true,
        limits_valid: true,
        hw_enable_ok: true,
        adapter_fault_cleared: true,
        command_mode: robot_core::adapter::ControlMode::Position,
        on_timeout_hint: robot_s0::state_machine::EnabledSub::CtrlLostHold,
    };
    // latch 为 true 时开工就 SAFE；ipc_thread 里只在未 latch 时才进 Ready/Enabled。
    if initial_estop_latched {
        initial_guards.adapter_connected = true;
    }

    let ipc_cfg = IpcThreadConfig {
        poll_interval_ns: 500_000,
        initial_guards,
        initial_state: S0State::Safe,
    };

    let sub_cmd_topic = t.sub_command.clone();
    let sub_hb_topic = t.sub_heartbeat.clone();
    let sub_estop_topic = t.sub_estop.clone();
    let ipc_factory = move || {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let command =
            open_sub::<markers::JointCommand>(&node, &sub_cmd_topic, TopicClass::S0Command)
                .expect("open command subscriber");
        let heartbeat = open_sub::<markers::Heartbeat>(&node, &sub_hb_topic, TopicClass::S1CmdVel)
            .expect("open heartbeat subscriber");
        let estop =
            open_sub::<markers::EStopEvent>(&node, &sub_estop_topic, TopicClass::SafetyEvent)
                .expect("open estop subscriber");
        // Node 保持活到 subscribers 消亡 = daemon 生命周期。
        Box::leak(Box::new(node));
        IpcSubscribers {
            command,
            heartbeat,
            estop,
        }
    };

    let ipc = match spawn_ipc_thread(
        ipc_cfg,
        Arc::clone(&clock),
        ipc_factory,
        cmd_writer,
        state_pub,
        wd_rx,
    ) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("fatal: spawn_ipc_thread: {e}");
            return Err(1);
        }
    };
    let event_tx = ipc.event_sender();
    let last_hb_ns = ipc.last_hb_ns();

    // ---- 4) spawn watchdog ----
    let wd_stop = Arc::new(AtomicBool::new(false));
    let wd_handle = spawn_watchdog(
        WatchdogConfig {
            heartbeat_timeout_ns: args.heartbeat_timeout_ns,
            poll_interval_ns: 10_000_000, // 10 ms
        },
        Arc::clone(&clock),
        last_hb_ns,
        wd_tx,
        Arc::clone(&wd_stop),
    );

    // ---- 5) spawn rt_thread ----
    //
    // adapter 来源（Step 7 task 27b）：
    // - 有 `--config`：取 TOML 第一段 `[[hardware]]` 的 `adapter` + `joints` 字段，
    //   走 `inventory::find_factory` 查 factory + build。
    // - 无 `--config`：fallback 到 `mock_hard_rt`，joint 名按 j0..jN 自动生成
    //   （兼容旧 CLI smoke 路径）。
    let adapter: Box<dyn ManipulatorAdapter + Send> = match build_adapter(args) {
        Ok(a) => a,
        Err(code) => {
            graceful_stop(ipc, &wd_stop, wd_handle, None, None, None);
            return Err(code);
        }
    };
    let rt_cfg = RtThreadConfig {
        tick_period_ns: args.tick_period_ns,
        sched_fifo_priority: 70,
        cpu_affinity: args.cpu_affinity,
        strict_rt: args.strict_rt,
    };
    let rt = match spawn_rt_thread(rt_cfg, Arc::clone(&clock), adapter, cmd_reader, rt_snap_pub) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("fatal: spawn_rt_thread: {e}");
            graceful_stop(ipc, &wd_stop, wd_handle, None, None, None);
            return Err(1);
        }
    };

    // ---- 6) spawn pub_thread ----
    let pub_state_topic = t.pub_state.clone();
    let pub_hb_topic = t.pub_heartbeat.clone();
    let pub_factory = move || {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let joint_state =
            open_pub::<markers::JointState>(&node, &pub_state_topic, TopicClass::S0State)
                .expect("open state publisher");
        let heartbeat = open_pub::<markers::Heartbeat>(&node, &pub_hb_topic, TopicClass::S1CmdVel)
            .expect("open heartbeat publisher");
        Box::leak(Box::new(node));
        IpcPublishers {
            joint_state,
            heartbeat,
        }
    };

    let pub_cfg = PubThreadConfig {
        state_hz: 50.0,
        heartbeat_hz: 50.0,
        s0_source_id: 1,
        joint_layout_id: 0xCAFE_BABE_DEAD_BEEF,
        poll_interval_ns: 1_000_000,
    };
    let pub_handle = match spawn_pub_thread(
        pub_cfg,
        Arc::clone(&clock),
        pub_factory,
        rt_snap_rdr,
        state_rdr.clone(),
    ) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("fatal: spawn_pub_thread: {e}");
            graceful_stop(ipc, &wd_stop, wd_handle, Some(rt), None, None);
            return Err(1);
        }
    };

    // ---- 7) spawn io_thread（占位）----
    let io_handle = match spawn_io_thread(IoThreadConfig::default()) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("fatal: spawn_io_thread: {e}");
            graceful_stop(ipc, &wd_stop, wd_handle, Some(rt), Some(pub_handle), None);
            return Err(1);
        }
    };

    // ---- 8) 注册 signal handlers ----
    install_signal_handlers();

    // ---- 8b) 自动 Event::Enable（Part 2 无 RPC，由 daemon 启动时替代 RPC 源）----
    //
    // state machine 的 SAFE → READY 转换需要 Event::Enable。v0.3 起由 RPC 网关
    // 发；Part 2 简化：daemon 启动时立即发一次，配合 initial_guards 的 all-true
    // 推进到 READY。之后 ipc_thread 收到第一个 heartbeat 会把 READY 推到
    // ENABLED.Active。跳过 EStop latch 时 estop_latched=true → try_transition 拒绝
    // 进 READY，state 留在 SAFE —— 期望行为。
    if initial_estop_latched {
        eprintln!("estop_latched = true → not auto-sending Event::Enable");
    } else {
        let _ = event_tx.send(Event::Enable);
    }

    // ---- 9) 主 loop：等 shutdown flag / smoke 超时 / state = STOPPING ----
    let start = std::time::Instant::now();
    let mut last_report = start;
    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            eprintln!("signal received → Event::Stop");
            break;
        }
        if args.smoke_duration.is_some_and(|d| start.elapsed() >= d) {
            eprintln!("smoke duration elapsed → Event::Stop");
            break;
        }
        let snap = state_rdr.load();
        if matches!(snap.state, S0State::Stopping) {
            eprintln!("state machine entered STOPPING → exiting main loop");
            break;
        }
        if last_report.elapsed() >= Duration::from_secs(2) {
            eprintln!(
                "heartbeat: state={:?}  guards.estop={}  guards.hb_ok={}  t={:?}",
                snap.state,
                snap.guards_summary.estop_latched,
                snap.guards_summary.heartbeat_ok,
                start.elapsed(),
            );
            last_report = std::time::Instant::now();
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // ---- 10) 发 Event::Stop，让 state machine 进 STOPPING ----
    let _ = event_tx.send(Event::Stop);

    // ---- 11) graceful stop ----
    graceful_stop(
        ipc,
        &wd_stop,
        wd_handle,
        Some(rt),
        Some(pub_handle),
        Some(io_handle),
    );

    // ---- 12) 保存 EStop latch ----
    let final_state_snap = state_rdr.load();
    let final_latched = final_state_snap.guards_summary.estop_latched;
    if let Err(e) = save_estop_latch(&args.latch_path, final_latched) {
        eprintln!(
            "warn: failed to save EStop latch to {}: {e}",
            args.latch_path.display()
        );
    } else {
        eprintln!(
            "final estop_latched = {final_latched} → persisted to {}",
            args.latch_path.display()
        );
    }

    eprintln!("s0_daemon: clean shutdown");
    Ok(())
}

// ============================================================================
// adapter 构造（Step 7 task 27b：走 inventory factory）
// ============================================================================

/// 按 `--config` TOML 第一段 hardware（或 fallback `mock_hard_rt`）走
/// `robot_core::factory::find_factory` 拿 factory + build。
///
/// 错误返回 i32 退出码（与 `run` 顶层一致），调用方据此 `graceful_stop`。
fn build_adapter(args: &ResolvedArgs) -> Result<Box<dyn ManipulatorAdapter + Send>, i32> {
    // 1) 决定 adapter_kind + joint_names 来源
    let (adapter_kind, joint_names): (String, Vec<String>) = if let Some(cfg) = &args.loaded_config
    {
        if let Some(hw) = cfg.parsed.hardware.first() {
            (hw.adapter.clone(), hw.joints.clone())
        } else {
            eprintln!(
                "fatal: --config {} 内无 [[hardware]] 段，无 adapter 可启动",
                cfg.source_path.display()
            );
            return Err(2);
        }
    } else {
        // CLI-only fallback：mock + 自动生成 j0..jN-1 关节名
        let names = (0..args.joint_count).map(|i| format!("j{i}")).collect();
        ("mock_hard_rt".to_string(), names)
    };

    // 2) 查 factory
    let Some(factory) = find_factory(&adapter_kind) else {
        eprintln!(
            "fatal: 没有名为 '{adapter_kind}' 的 adapter factory。\n\
             已注册：{:?}\n\
             检查 robot-s0 features（cargo build --features <combo>）或 \
             configs/<robot>.toml 里 [[hardware]] adapter 字段拼写。",
            registered_factories()
                .map(robot_core::factory::AdapterFactory::name)
                .collect::<Vec<_>>()
        );
        return Err(2);
    };

    // 3) build adapter
    let ctx = AdapterBuildContext {
        instance_name: &args.hardware_id,
        adapter_kind: &adapter_kind,
        joint_names: &joint_names,
    };
    factory.build(&ctx).map_err(|e| {
        eprintln!("fatal: adapter '{adapter_kind}' build failed: {e}");
        1
    })
}

/// 有顺序地停线程 + `join`，容忍任一 handle 为 None（早期启动失败场景）。
fn graceful_stop(
    ipc: IpcThreadHandle,
    wd_stop: &Arc<AtomicBool>,
    wd_handle: std::thread::JoinHandle<()>,
    rt: Option<robot_s0::rt_thread::RtThreadHandle>,
    pub_handle: Option<robot_s0::pub_thread::PubThreadHandle>,
    io_handle: Option<robot_s0::io_thread::IoThreadHandle>,
) {
    // 顺序：pub/io 先停（不再往外发），rt 停（不再动 adapter），ipc 停，watchdog 最后。
    if let Some(h) = pub_handle {
        h.request_stop();
        let _ = h.join();
    }
    if let Some(h) = io_handle {
        h.request_stop();
        let _ = h.join();
    }
    if let Some(h) = rt {
        h.request_stop();
        let _ = h.join();
    }
    ipc.request_stop();
    let _ = ipc.join();
    wd_stop.store(true, Ordering::Release);
    let _ = wd_handle.join();
}
