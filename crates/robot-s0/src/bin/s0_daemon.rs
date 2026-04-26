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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crossbeam_channel::unbounded;
use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::adapter::{
    HardRtAdapter, JointCommandBuf, ManipulatorAdapter, RtClass, RtSnapshot, SerialRtAdapter,
};
use robot_core::config::IdentityFingerprint;
use robot_core::executor_ctrl::ExecutorCtrlClient;
use robot_core::factory::{AdapterBuildContext, find_factory, registered_factories};
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::markers;
use robot_s0::config_loader::{LoadedConfig, load_robot_config};
use robot_s0::estop_latch::{load_estop_latch, save_estop_latch};
use robot_s0::hard_rt_ctrl::{HardRtCtrlClient, HardRtCtrlInner};
use robot_s0::io_thread::{IoThreadConfig, spawn_io_thread};
use robot_s0::ipc_thread::{IpcSubscribers, IpcThreadConfig, IpcThreadHandle, spawn_ipc_thread};
use robot_s0::pub_thread::{IpcPublishers, PubThreadConfig, spawn_pub_thread};
use robot_s0::rt_thread::{RtThreadConfig, RtThreadHandle, spawn_rt_thread};
use robot_s0::serial_io_thread::{
    SerialIoThreadConfig, SerialIoThreadHandle, spawn_serial_io_thread,
};
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
    // task-32：所有 bin 的第一件事 —— 统一 tracing subscriber 初始化。
    // 幂等：多次调用只第一次生效（底层 `set_global_default` 限制）。
    let _ = robot_logging::init();

    let parsed = match Args::parse() {
        Ok(a) => a,
        Err(msg) => {
            if msg == "HELP" {
                print_help();
                std::process::exit(0);
            }
            tracing::error!(error = %msg, "CLI argument parse failed");
            print_help();
            std::process::exit(2);
        }
    };

    let resolved = match parsed.resolve() {
        Ok(r) => r,
        Err(msg) => {
            tracing::error!(error = %msg, "CLI argument resolve failed");
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

    tracing::info!(
        robot_id = %args.robot_id,
        hardware_id = %args.hardware_id,
        latch_path = %args.latch_path.display(),
        hb_timeout_ms = args.heartbeat_timeout_ns / 1_000_000,
        tick_hz = 1_000_000_000 / args.tick_period_ns,
        strict_rt = args.strict_rt,
        joints = args.joint_count,
        cpu = ?args.cpu_affinity,
        "s0_daemon starting"
    );
    if let Some(cfg) = &args.loaded_config {
        tracing::info!(
            config_path = %cfg.source_path.display(),
            hardware_sections = cfg.parsed.hardware.len(),
            sensor_sections = cfg.parsed.sensor.len(),
            "config loaded"
        );
    } else {
        tracing::info!("config: none (CLI-only mode; identity is stub)");
    }
    tracing::info!(identity = %args.identity, "identity fingerprint");
    tracing::info!(
        sub_cmd = %t.sub_command,
        sub_hb = %t.sub_heartbeat,
        sub_estop = %t.sub_estop,
        pub_state = %t.pub_state,
        pub_hb = %t.pub_heartbeat,
        "iceoryx2 topics"
    );

    // ---- 1) EStop latch ----
    if let Err(e) = ensure_latch_parent(&args.latch_path) {
        tracing::error!(
            latch_path = %args.latch_path.display(),
            ?e,
            "fatal: failed to prepare EStop latch directory"
        );
        return Err(1);
    }
    let initial_estop_latched = match load_estop_latch(&args.latch_path) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(
                latch_path = %args.latch_path.display(),
                ?e,
                "fatal: failed to load EStop latch"
            );
            return Err(1);
        }
    };
    tracing::info!(latched = initial_estop_latched, "initial estop_latched");

    // ---- 2) 共享 channel / atomic / slot ----
    let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());

    let (cmd_writer, cmd_reader) =
        LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());

    let (rt_snap_pub, rt_snap_rdr) = RtSnapshotPublisher::<RtSnapshot>::new(RtSnapshot::default());
    let (state_pub, state_rdr) =
        RtSnapshotPublisher::<S0StateSnapshot>::new(S0StateSnapshot::default());

    let (wd_tx, wd_rx) = unbounded::<Event>();

    // ---- 3) build adapter + 创建 rt_ctrl + ExecutorCtrlClient（必须在 spawn_ipc_thread 之前） ----
    //
    // **task-30 v9 / M7 阶段 4**：spawn_ipc_thread 必传 `Arc<dyn ExecutorCtrlClient>`
    // 让 ipc_tick 三步式驱动 set_policy / safety_stop / request_normal /
    // poll_last_normal_resp / clear_safety_latch。client 由对应路径 ctrl 包装：
    // - HardRT 路径：`HardRtCtrlClient::new(Arc::clone(&rt_ctrl))`
    // - SerialRT 路径：`SerialIoCtrlClient` 是 `HardRtCtrlClient` 的 type alias
    //   （v9 收敛后 atomic 协议层共用），同款构造。
    let built = build_adapter(args)?;
    // task-30 v9 finding #3 + M7 阶段 4 第二步守门：编译期 + 运行期双重 dispatch。
    //
    // - HardRt：trait object 已 RtSafeCtrl witness，进 spawn_rt_thread。
    // - SerialRt：trait object 已 OutputSafetyProbe witness，进
    //   spawn_serial_io_thread。
    // - SoftRt：v0.2 不接通；v0.4 起 SoftRT adapter 挂 s0_io_thread。
    //
    // executor_ctrl 在两条路径上类型一致（HardRtCtrlClient ==
    // SerialIoCtrlClient = HardRtCtrlClient 的 type alias，atomic 协议层共享
    // HardRtCtrlInner），ipc_thread 注入 `Arc<dyn ExecutorCtrlClient>` 不区分
    // 路径。reviewer 硬要求 #3：dispatch 后只把对应路径的 client 注入。
    let plan: SpawnPlan = match built {
        BuiltAdapter::HardRt(a) => {
            debug_assert_eq!(a.info().rt_class, RtClass::HardRT);
            SpawnPlan::HardRt(a)
        }
        BuiltAdapter::SerialRt(a) => {
            debug_assert_eq!(a.info().rt_class, RtClass::SerialRT);
            SpawnPlan::SerialRt(a)
        }
        BuiltAdapter::SoftRt(a) => {
            let class = a.info().rt_class;
            let name = a.info().name;
            match class {
                RtClass::SoftRT => tracing::error!(
                    adapter = name,
                    rt_class = "SoftRT",
                    "fatal: SoftRT adapter 应挂 s0_io_thread (v0.4)，不能进 ManipulatorAdapter spawn 路径"
                ),
                RtClass::HardRT => tracing::error!(
                    adapter = name,
                    rt_class = "HardRT",
                    "fatal: 报告 HardRT 但 factory 没走 build_hard_rt（factory 实现遗漏 RtSafeCtrl + override）"
                ),
                RtClass::SerialRT => tracing::error!(
                    adapter = name,
                    rt_class = "SerialRT",
                    "fatal: 报告 SerialRT 但 factory 没走 build_serial_rt（factory 实现遗漏 OutputSafetyProbe + override）"
                ),
            }
            return Err(1);
        }
    };

    // rt_ctrl + client：ipc_thread 拿 client；rt_thread / serial_io_thread 拿 ctrl。
    // HardRtCtrlInner 在 HardRT / SerialRT 两条路径上共享类型（v9 收敛后 atomic
    // 协议层统一），构造方式相同。
    let rt_ctrl = Arc::new(HardRtCtrlInner::new());
    let executor_ctrl: Arc<dyn ExecutorCtrlClient> =
        Arc::new(HardRtCtrlClient::new(Arc::clone(&rt_ctrl)));

    // ---- 4) spawn ipc_thread ----
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
        // **task #15 阻断 1 修复**：把 latch_path 注入 ipc_thread，让 EStop
        // 触发即 fsync 落盘（不等 graceful shutdown）。daemon 退出时仍会再
        // 写一次 final 值（§12 graceful save），但不再依赖那次写。
        latch_path: Some(args.latch_path.clone()),
    };

    let sub_cmd_topic = t.sub_command.clone();
    let sub_hb_topic = t.sub_heartbeat.clone();
    let sub_estop_topic = t.sub_estop.clone();
    let ipc_factory = move || {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let command =
            open_sub::<markers::JointCommand>(&node, &sub_cmd_topic, TopicClass::S0Command)
                .expect("open command subscriber");
        let heartbeat = open_sub::<markers::Heartbeat>(&node, &sub_hb_topic, TopicClass::Heartbeat)
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
        Arc::clone(&executor_ctrl),
    ) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(?e, "fatal: spawn_ipc_thread failed");
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

    // ---- 4.5) spawn system health monitor（task-31 B'a + B'b）----
    // 每 [monitor.interval_secs]（默认 5 s）采样系统级 CPU / mem / load +
    // per-process（`interested_processes` + 全系统 top N by CPU），
    // tracing::info! 结构化输出。非 RT，普通优先级线程，无 RT 影响。
    // handle 不显式 stop —— main 作用域内 local，Drop 自动 join（含 early return）。
    let monitor_cfg = args
        .loaded_config
        .as_ref()
        .map(|cfg| {
            let m = &cfg.parsed.monitor;
            robot_health::MonitorConfig {
                interval: std::time::Duration::from_secs(m.interval_secs),
                interested_processes: m.interested_processes.clone(),
                top_n: m.top_n,
            }
        })
        .unwrap_or_default();
    let _health_handle = robot_health::spawn_monitor(monitor_cfg);

    // ---- 6) spawn executor (rt_thread for HardRT, serial_io_thread for SerialRT) ----
    //
    // adapter / rt_ctrl / executor_ctrl 已经在 ---- 3) 创建好；这里按 SpawnPlan
    // 分发：
    //
    // - HardRT：spawn_rt_thread（SCHED_FIFO 70 + mlockall）。`HardRtCtrlInner`
    //   默认 policy = Disabled / safety_state = TorqueOffConfirmed —— 由
    //   state_machine `Safe → Ready → Enabled.Active` 走两阶段 transition 通过
    //   client.request_normal(Enable(true)) 走 ctrl 路径，rt_thread 主循环 Step
    //   2 真上扭后 ipc_tick Pure/Effectful 分支才 set_policy(ActiveForward)。
    // - SerialRT：spawn_serial_io_thread（普通优先级；SerialRT 不进 SCHED_FIFO，
    //   串口 RTT 决定 tick 周期 5-20 ms）。同样走两阶段 transition；
    //   serial_io_thread 在 Step 3 consume normal action 时调
    //   `adapter.enable(true)`，emergency 后下 tick `read_torque_enable_mask`
    //   把 OutputSafetyState 推进到 TorqueOffConfirmed / AdapterUncertain /
    //   StopFailed。
    let executor: ExecutorHandle = match plan {
        SpawnPlan::HardRt(adapter) => {
            let rt_cfg = RtThreadConfig {
                tick_period_ns: args.tick_period_ns,
                sched_fifo_priority: 70,
                cpu_affinity: args.cpu_affinity,
                strict_rt: args.strict_rt,
            };
            match spawn_rt_thread(
                rt_cfg,
                Arc::clone(&clock),
                adapter,
                cmd_reader,
                rt_snap_pub,
                Arc::clone(&rt_ctrl),
            ) {
                Ok(h) => ExecutorHandle::HardRt(h),
                Err(e) => {
                    tracing::error!(?e, "fatal: spawn_rt_thread failed");
                    graceful_stop(ipc, &wd_stop, wd_handle, None, None, None);
                    return Err(1);
                }
            }
        }
        SpawnPlan::SerialRt(adapter) => {
            let serial_cfg = SerialIoThreadConfig::default();
            match spawn_serial_io_thread(
                serial_cfg,
                Arc::clone(&clock),
                adapter,
                cmd_reader,
                rt_snap_pub,
                Arc::clone(&rt_ctrl),
            ) {
                Ok(h) => ExecutorHandle::SerialRt(h),
                Err(e) => {
                    tracing::error!(?e, "fatal: spawn_serial_io_thread failed");
                    graceful_stop(ipc, &wd_stop, wd_handle, None, None, None);
                    return Err(1);
                }
            }
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
        let heartbeat = open_pub::<markers::Heartbeat>(&node, &pub_hb_topic, TopicClass::Heartbeat)
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
        Arc::clone(&rt_ctrl),
    ) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(?e, "fatal: spawn_pub_thread failed");
            graceful_stop(ipc, &wd_stop, wd_handle, Some(executor), None, None);
            return Err(1);
        }
    };

    // ---- 7) spawn io_thread（占位）----
    let io_handle = match spawn_io_thread(IoThreadConfig::default()) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(?e, "fatal: spawn_io_thread failed");
            graceful_stop(
                ipc,
                &wd_stop,
                wd_handle,
                Some(executor),
                Some(pub_handle),
                None,
            );
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
        tracing::info!("estop_latched=true → not auto-sending Event::Enable");
    } else {
        let _ = event_tx.send(Event::Enable);
    }

    // ---- 8c) systemd `Type=notify` 握手 + watchdog ping 线程 ----
    //
    // 所有线程都已 spawn + initial state 落盘后，告诉 systemd "READY"，让
    // `Type=notify` unit 的 active 状态成立 + 解锁下游 unit 的 `After=` 链。
    //
    // `WATCHDOG_USEC` 由 systemd unit `WatchdogSec=` 注入；spawn 出独立非 RT
    // 线程按 `usec / 2` 间隔 ping，daemon 卡死 / 死锁就让 systemd 按
    // `Restart=on-failure` 自动拉起。**不在 systemd 下跑时**（NOTIFY_SOCKET /
    // WATCHDOG_USEC 都未设置）这两条都 silent no-op，跟 runbook / crash_restart_smoke
    // 等场景零差异。详 `crates/robot-s0/src/systemd_notify.rs`。
    robot_s0::systemd_notify::ready();
    let systemd_wd_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let systemd_wd_handle =
        robot_s0::systemd_notify::spawn_watchdog_thread(std::sync::Arc::clone(&systemd_wd_stop));

    // ---- 9) 主 loop：等 shutdown flag / smoke 超时 / state = STOPPING ----
    let start = std::time::Instant::now();
    let mut last_report = start;
    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            tracing::info!("signal received → Event::Stop");
            break;
        }
        if args.smoke_duration.is_some_and(|d| start.elapsed() >= d) {
            tracing::info!("smoke duration elapsed → Event::Stop");
            break;
        }
        let snap = state_rdr.load();
        if matches!(snap.state, S0State::Stopping) {
            tracing::info!("state machine entered STOPPING → exiting main loop");
            break;
        }
        if last_report.elapsed() >= Duration::from_secs(2) {
            tracing::info!(
                state = ?snap.state,
                "guards.estop" = snap.guards_summary.estop_latched,
                "guards.hb_ok" = snap.guards_summary.heartbeat_ok,
                t = ?start.elapsed(),
                "heartbeat"
            );
            last_report = std::time::Instant::now();
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // ---- 10) 发 Event::Stop，让 state machine 进 STOPPING ----
    let _ = event_tx.send(Event::Stop);

    // ---- 10a) 通知 systemd 进入 graceful shutdown，停 watchdog ping ----
    //
    // 顺序很关键：先 STOPPING= 让 systemd 切到 deactivating（停 watchdog
    // timeout），再 stop ping 线程。如果反过来，watchdog 线程已经停但 systemd
    // 还按 `WatchdogSec` 计时，graceful 期间稍长一点就被 systemd 误杀。
    robot_s0::systemd_notify::stopping();
    systemd_wd_stop.store(true, std::sync::atomic::Ordering::Release);
    if let Some(h) = systemd_wd_handle {
        let _ = h.join();
    }

    // ---- 11) graceful stop ----
    graceful_stop(
        ipc,
        &wd_stop,
        wd_handle,
        Some(executor),
        Some(pub_handle),
        Some(io_handle),
    );

    // ---- 12) 保存 EStop latch ----
    let final_state_snap = state_rdr.load();
    let final_latched = final_state_snap.guards_summary.estop_latched;
    if let Err(e) = save_estop_latch(&args.latch_path, final_latched) {
        tracing::warn!(
            latch_path = %args.latch_path.display(),
            ?e,
            "failed to save EStop latch"
        );
    } else {
        tracing::info!(
            latched = final_latched,
            latch_path = %args.latch_path.display(),
            "final estop_latched persisted"
        );
    }

    tracing::info!("s0_daemon: clean shutdown");
    Ok(())
}

/// 启动期确保 latch 父目录存在；失败直接拒启动，避免 `EStop` 后落盘失败却
/// 已经进入可使能状态。
fn ensure_latch_parent(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)
}

// ============================================================================
// adapter 构造（Step 7 task 27b：走 inventory factory）
// ============================================================================

/// task-30 v9 finding #3：build 出来的 adapter 按 `RtClass` 分两路。
///
/// `HardRt` 变体的 trait object **编译期保证**实现 `RtSafeCtrl`（综合
/// `HardRtAdapter`），可直接 move 进 `spawn_rt_thread`。`SerialRt` 变体的
/// trait object **编译期保证**实现 `OutputSafetyProbe`（综合
/// [`SerialRtAdapter`]），可直接 move 进 `spawn_serial_io_thread`。`SoftRt`
/// 仍走 fallback `build()`，目前 v0.2 拒启动（待 v0.4 `SoftRT s0_io_thread`
/// 落地）。
#[allow(clippy::enum_variant_names)] // 三变体共后缀 Rt 是与 RtClass 枚举的有意对齐
enum BuiltAdapter {
    HardRt(Box<dyn HardRtAdapter + Send>),
    SerialRt(Box<dyn SerialRtAdapter + Send>),
    SoftRt(Box<dyn ManipulatorAdapter + Send>),
}

/// `BuiltAdapter` `SoftRT` 早退 + 标 `RtClass` 后的"已分发"形态。仅
/// `HardRt` / `SerialRt` 两路对应 `spawn_rt_thread` / `spawn_serial_io_thread`。
#[allow(clippy::enum_variant_names)] // 同 BuiltAdapter，对齐 RtClass 命名
enum SpawnPlan {
    HardRt(Box<dyn HardRtAdapter + Send>),
    SerialRt(Box<dyn SerialRtAdapter + Send>),
}

/// 按 `--config` TOML 第一段 hardware（或 fallback `mock_hard_rt`）走
/// `robot_core::factory::find_factory` 拿 factory + build。
///
/// **task-30 v9 finding #3**：先试 `build_hard_rt`（`HardRT` factory 必返综合
/// 类型）；若返 `Unsupported` → 走 `build` 普通路径。两条路径各自封装在
/// [`BuiltAdapter`] 变体内。
///
/// 错误返回 i32 退出码（与 `run` 顶层一致），调用方据此 `graceful_stop`。
fn build_adapter(args: &ResolvedArgs) -> Result<BuiltAdapter, i32> {
    // 1) 决定 adapter_kind + joint_names + address + motor_ids + calibration_paths 来源
    let (adapter_kind, joint_names, address, motor_ids, calibration_paths): (
        String,
        Vec<String>,
        Option<String>,
        Vec<u8>,
        Vec<String>,
    ) = if let Some(cfg) = &args.loaded_config {
        if let Some(hw) = cfg.parsed.hardware.first() {
            // motor_ids 从 [[hardware]] extras["motor_ids"] 数组读（task 28 扩展）
            let ids = hw
                .extras
                .get("motor_ids")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_integer().and_then(|i| u8::try_from(i).ok()))
                        .collect::<Vec<u8>>()
                })
                .unwrap_or_default();
            // calibration_paths 从 [[hardware]] extras["calibration_paths"] 数组
            // 读（task 29 commit 3 扩展）：dimos bus1 = left_arm.json + head.json
            // 双文件，bus2 = right_arm.json + base.json。空数组 / 未设置 =
            // "无标定"，factory 不做 silent clamp。
            let cal_paths = hw
                .extras
                .get("calibration_paths")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default();
            (
                hw.adapter.clone(),
                hw.joints.clone(),
                hw.address.clone(),
                ids,
                cal_paths,
            )
        } else {
            tracing::error!(
                config_path = %cfg.source_path.display(),
                "fatal: config 内无 [[hardware]] 段，无 adapter 可启动"
            );
            return Err(2);
        }
    } else {
        // CLI-only fallback：mock + 自动生成 j0..jN-1 关节名（无 address / motor_ids / calibration）
        let names = (0..args.joint_count).map(|i| format!("j{i}")).collect();
        (
            "mock_hard_rt".to_string(),
            names,
            None,
            Vec::new(),
            Vec::new(),
        )
    };

    // 2) 查 factory
    let Some(factory) = find_factory(&adapter_kind) else {
        tracing::error!(
            adapter_kind = %adapter_kind,
            registered = ?registered_factories()
                .map(robot_core::factory::AdapterFactory::name)
                .collect::<Vec<_>>(),
            "fatal: 没有名为 '{adapter_kind}' 的 adapter factory。检查 robot-s0 \
             features (cargo build --features <combo>) 或 configs/<robot>.toml 里 \
             [[hardware]] adapter 字段拼写"
        );
        return Err(2);
    };

    // 3) build adapter
    let ctx = AdapterBuildContext {
        instance_name: &args.hardware_id,
        adapter_kind: &adapter_kind,
        joint_names: &joint_names,
        address: address.as_deref(),
        motor_ids: &motor_ids,
        calibration_paths: &calibration_paths,
    };
    // **task-30 v9 finding #3 + M7 阶段 4 第二步**：按 RtClass 三路 dispatch。
    // 1) 先 `build_hard_rt`：HardRT factory 必返综合 trait object（编译期 RtSafeCtrl）。
    // 2) HardRT factory `Unsupported` → 试 `build_serial_rt`：SerialRT factory
    //    必返综合 trait object（编译期 OutputSafetyProbe）。
    // 3) 仍 `Unsupported` → 走 `build`（SoftRT 占位；v0.2 不接通）。
    //
    // **顺序极其关键**：HardRT 优先 = 编译期最强守门；SerialRT 次之 = 串口路径
    // confirm_torque_off 必须；SoftRT 兜底 = 仅做日志/策略 adapter（无 motor torque）。
    match factory.build_hard_rt(&ctx) {
        Ok(hard_rt) => Ok(BuiltAdapter::HardRt(hard_rt)),
        Err(robot_core::error::RobotError::Unsupported(_)) => match factory.build_serial_rt(&ctx) {
            Ok(serial_rt) => Ok(BuiltAdapter::SerialRt(serial_rt)),
            Err(robot_core::error::RobotError::Unsupported(_)) => {
                factory.build(&ctx).map(BuiltAdapter::SoftRt).map_err(|e| {
                    tracing::error!(adapter = %adapter_kind, ?e, "fatal: adapter build failed");
                    1
                })
            }
            Err(e) => {
                tracing::error!(adapter = %adapter_kind, ?e, "fatal: adapter build_serial_rt failed");
                Err(1)
            }
        },
        Err(e) => {
            tracing::error!(adapter = %adapter_kind, ?e, "fatal: adapter build_hard_rt failed");
            Err(1)
        }
    }
}

/// 用于 `graceful_stop` 的 executor 线程 handle 包装，按 `RtClass` 分发后只持
/// 其中一种（task-30 v9 / M7 阶段 4）。
enum ExecutorHandle {
    HardRt(RtThreadHandle),
    SerialRt(SerialIoThreadHandle),
}

impl ExecutorHandle {
    fn request_stop(&self) {
        match self {
            Self::HardRt(h) => h.request_stop(),
            Self::SerialRt(h) => h.request_stop(),
        }
    }

    fn join(self) -> thread::Result<()> {
        match self {
            Self::HardRt(h) => h.join(),
            Self::SerialRt(h) => h.join(),
        }
    }
}

/// 有顺序地停线程 + `join`，容忍任一 handle 为 None（早期启动失败场景）。
fn graceful_stop(
    ipc: IpcThreadHandle,
    wd_stop: &Arc<AtomicBool>,
    wd_handle: std::thread::JoinHandle<()>,
    executor: Option<ExecutorHandle>,
    pub_handle: Option<robot_s0::pub_thread::PubThreadHandle>,
    io_handle: Option<robot_s0::io_thread::IoThreadHandle>,
) {
    // 顺序：pub/io 先停（不再往外发），executor（rt 或 serial_io）停（不再动
    // adapter），ipc 停，watchdog 最后。
    if let Some(h) = pub_handle {
        h.request_stop();
        let _ = h.join();
    }
    if let Some(h) = io_handle {
        h.request_stop();
        let _ = h.join();
    }
    if let Some(h) = executor {
        h.request_stop();
        let _ = h.join();
    }
    ipc.request_stop();
    let _ = ipc.join();
    wd_stop.store(true, Ordering::Release);
    let _ = wd_handle.join();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_latch_parent_creates_missing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let latch = tmp
            .path()
            .join("robot")
            .join("xlerobot")
            .join("estop_latch");

        ensure_latch_parent(&latch).unwrap();

        assert!(latch.parent().unwrap().is_dir());
        assert!(!latch.exists());
    }
}
