//! S0 守护进程：八 worker（含 writer）、SerialRT 启动收敛与独立启动监督。
//!
//! main 首句启动监督器，再执行日志、解析、实例锁与 adapter 构造。共享控制器和
//! bus 服务在 factory 前建立；SerialRT 在八 worker（含 writer） 全部成功后才允许首个总线 IO。
//! 物理预检与故障 ACK 完成后处理自动使能，再发送 READY；运行中 IPC 退出视为错误。
//! 启动失败先 Disabled、仅停止已启动 worker，并在同一绝对截止内回收已结束线程；
//! READY 后保留 RR → pub/io → executor → IPC → watchdog 的顺序关闭。
//!
//! `robot_id` 来源：CLI > TOML > 环境 `ROBOT_ID` > default。具体参数见 `--help`。
//! 退出码：0 正常；1 启动/运行/存储失败；2 参数错误；3 marker 第一阶段已提交但
//! 第二阶段未完成；4 marker 未授权；87 同实例锁竞争；88 独立监督器执法；
//! 89 latch 稳定锁被另一进程持有；90 运行期监督或未完成的 owned 关闭。

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crossbeam_channel::bounded;
use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::adapter::{
    HardRtAdapter, JointCommandBuf, ManipulatorAdapter, RtClass, RtSnapshot, SerialRtAdapter,
};
use robot_core::config::IdentityFingerprint;
use robot_core::control_authority::{AuthorityInit, ControlAuthority, InhibitReason, InhibitSet};
use robot_core::executor_ctrl::OutputSafetyState;
use robot_core::factory::{
    AdapterBuildContext, AdapterBuildServices, find_factory, registered_factories,
};
use robot_core::time::{DurationNs, MonoRawNs, RobotClock, SystemClock};
use robot_msg::joint_layout::compute_layout_id;
use robot_msg::markers;
use robot_s0::boot_supervisor::{
    BootSupervisor, STARTUP_ACK_BUDGET, absolute_boot_budget, boot_budget,
};
use robot_s0::clear_estop_transaction::{ClearTxnStore, StageError};
use robot_s0::config_loader::{
    LoadedConfig, load_robot_config, resolve_hardware_section, validate_hardware_section,
};
use robot_s0::hard_rt_ctrl::{HardRtCtrlClient, HardRtCtrlInner};
use robot_s0::io_thread::{IoThreadConfig, spawn_io_thread};
use robot_s0::ipc_thread::{
    ClearEStopRrRequest, ClearEStopRrResponse, IpcSubscribers, IpcThreadConfig, RrChannels,
    SetEnabledRrChannels, SetEnabledRrRequest, SetEnabledRrResponse, spawn_ipc_thread,
};
use robot_s0::latch_lock::{
    LATCH_LOCK_HELD_EXIT_CODE, LatchLockError, LatchTargetLock, acquire_latch_lock,
    validate_latch_path,
};
use robot_s0::latch_writer::{
    PersistClient, PersistEndpoint, PersistShared, StandbyStore, WriterConfig, WriterPairing,
    WriterSlot,
};
use robot_s0::pub_thread::{IpcPublishers, PubThreadConfig, spawn_pub_thread};
use robot_s0::rr_server_thread::{RrServerThreadConfig, spawn_rr_server_thread};
use robot_s0::rt_thread::{RtThreadConfig, RtThreadHandle, spawn_rt_thread};
use robot_s0::rx_trust::{MarkerCommand, RealMarkerFs, TrustLockError};
use robot_s0::serial_io_thread::{
    SerialIoThreadConfig, SerialIoThreadHandle, spawn_serial_io_thread,
};
use robot_s0::set_enabled_rr_server::spawn_set_enabled_rr_server_thread;
use robot_s0::shutdown_mechanism::{
    HandleKind, HandleKindSet, HandleSlots, InsertError, JoinedOutcome, ResidualHandles, RunExit,
    WorkerSlot, spawn_managed_writer,
};
use robot_s0::state_machine::{Event, Guards, S0State, S0StateSnapshot};
use robot_s0::watchdog::{WatchdogConfig, spawn_watchdog};
use robot_s0::worker_stop::{StopFlags, WorkerKind};
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
    marker_command: Option<MarkerCommand>,
    rx_trust_dir_cli: Option<PathBuf>,
    trust_policy: Option<SerialTrustPolicy>,
    notify_mode: NotifyMode,
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
    /// `--serial-fifo <prio>`：**opt-in** 给 `SerialRT` `s0_serial_io_thread`
    /// 上 `SCHED_FIFO`（含 mlockall）。缺省 None = 维持既有普通优先级
    /// （"`SerialRT` 不进 `SCHED_FIFO`"仍是缺省语义；偏离登记 CHANGELOG
    /// 2026-09-01）。`--cpu` / `--strict-rt` 同时作用于 `SerialRT` 路径。
    serial_fifo_priority: Option<u8>,
    joint_count: u8,
    cpu_affinity: Option<usize>,
}

/// CLI 解析完 + `--config` 加载完之后的最终配置（CLI override 优先级已应用）。
struct ResolvedArgs {
    marker_command: Option<MarkerCommand>,
    rx_trust_dir: PathBuf,
    trust_policy: Option<SerialTrustPolicy>,
    notify_mode: NotifyMode,
    robot_id: String,
    hardware_id: String,
    latch_path: PathBuf,
    heartbeat_timeout_ns: u64,
    smoke_duration: Option<Duration>,
    tick_period_ns: u64,
    strict_rt: bool,
    joint_count: u8,
    cpu_affinity: Option<usize>,
    serial_fifo_priority: Option<u8>,
    /// `--config` 加载的完整配置（含 ConfigIdentity），无 `--config` 时为 None。
    loaded_config: Option<LoadedConfig>,
    /// **v0.5 候选 E 段(3)**：本实例权威 `layout_id`。config 模式 =
    /// `compute_layout_id(robot_id, hardware_id, zip(joints, motor_ids))`
    /// 派生（`resolve()` 内与段校验同点算出）；CLI-only 保留既有占位
    /// `0xCAFE_BABE_DEAD_BEEF`（§3.5.1 部分落地边界——无 name/id 表可派生，
    /// 走 `CHANGELOG` 登记）。`pub_thread`（`JointState.layout_id`）与
    /// `ipc_thread`（command gate）共用此单源。
    authoritative_layout: u64,
    /// v0.2 partial fingerprint：`--config` 模式下来自 TOML，否则按 CLI 现场算
    /// 一个不带 hash 的 stub（仅 Display 用，跨进程 handshake 推 v0.3）。
    identity: IdentityFingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SerialTrustPolicy {
    Marker,
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotifyMode {
    Required,
    Standalone,
    Auto,
}

impl Args {
    #[allow(clippy::too_many_lines)]
    fn parse() -> Result<Self, String> {
        let mut args = Self {
            marker_command: None,
            rx_trust_dir_cli: None,
            trust_policy: None,
            notify_mode: NotifyMode::Auto,
            robot_id_cli: None,
            hardware_id_cli: None,
            latch_path_cli: None,
            config_path: None,
            heartbeat_timeout_ns: 200_000_000, // 200 ms
            smoke_duration: None,
            tick_period_ns: 20_000_000, // 50 Hz
            strict_rt: false,
            serial_fifo_priority: None,
            joint_count: 3,
            cpu_affinity: None,
        };

        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            match a.as_str() {
                "rx-trust" if i == 0 => {
                    i += 1;
                    args.marker_command = Some(match raw.get(i).map(String::as_str) {
                        Some("authorize") => MarkerCommand::Authorize,
                        Some("revoke") => MarkerCommand::Revoke,
                        Some("status") => MarkerCommand::Status,
                        _ => return Err("rx-trust 需要 authorize|revoke|status".into()),
                    });
                }
                "--rx-trust-dir" => {
                    i += 1;
                    if args.rx_trust_dir_cli.is_some() {
                        return Err("--rx-trust-dir 重复".into());
                    }
                    args.rx_trust_dir_cli =
                        Some(PathBuf::from(raw.get(i).ok_or("--rx-trust-dir 缺少值")?));
                }
                "--rx-trust-policy" => {
                    i += 1;
                    if args.trust_policy.is_some() {
                        return Err("--rx-trust-policy 重复".into());
                    }
                    args.trust_policy = Some(match raw.get(i).map(String::as_str) {
                        Some("marker") => SerialTrustPolicy::Marker,
                        Some("static") => SerialTrustPolicy::Static,
                        _ => return Err("--rx-trust-policy 需要 marker|static".into()),
                    });
                }
                "--notify-mode" => {
                    i += 1;
                    args.notify_mode = match raw.get(i).map(String::as_str) {
                        Some("required") => NotifyMode::Required,
                        Some("standalone") => NotifyMode::Standalone,
                        Some("auto") => NotifyMode::Auto,
                        _ => return Err("--notify-mode 需要 required|standalone|auto".into()),
                    };
                }
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
                "--serial-fifo" => {
                    i += 1;
                    let prio: u8 = raw
                        .get(i)
                        .ok_or("--serial-fifo requires value")?
                        .parse()
                        .map_err(|e| format!("--serial-fifo: {e}"))?;
                    if !(1..=99).contains(&prio) {
                        return Err("--serial-fifo: priority must be 1..=99".into());
                    }
                    args.serial_fifo_priority = Some(prio);
                }
                "--joints" => {
                    i += 1;
                    args.joint_count = raw
                        .get(i)
                        .ok_or("--joints requires value")?
                        .parse()
                        .map_err(|e| format!("--joints: {e}"))?;
                    if !(1..=32).contains(&args.joint_count) {
                        return Err("--joints 必须为 1..=32".into());
                    }
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

        if args.marker_command.is_some()
            && (args.config_path.is_some()
                || args.hardware_id_cli.is_some()
                || args.trust_policy.is_some())
        {
            return Err("rx-trust 子命令不接受 hardware/config/trust-policy".into());
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

        // v0.5 候选 E 段(2) 身份锁（P.2.2；沿 robot_rpc_gateway.rs:133 先例
        // 文案）：`--robot-id` 与 TOML `[robot].id` **同给且不等** → fail-fast
        // （main 的 resolve 错误路径，exit 2）。CLI 静默 override TOML 身份会
        // 让 topic / layout / latch 路径与 config 派生面分裂。
        if let (Some(cli), Some(cfg)) = (&self.robot_id_cli, &loaded_config)
            && cli != &cfg.parsed.robot.id
        {
            return Err(format!(
                "[robot].id '{}' != --robot-id '{}' (config/flag mismatch; 会订阅错 topic)",
                cfg.parsed.robot.id, cli
            ));
        }

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

        // v0.5 候选 E 段(2) 回修 P0-2：resolved 段查找 + 内容校验**前移到
        // resolve()**——必须先于 run() 的 latch 加载（exit 1 族）完成 config
        // fail-fast（AC-E3 exit 2 族）；"非法 motor_ids + latch 缺失"不得
        // 先报 latch。unknown hardware 同罪同码。build_adapter 保留同校验
        // 作防御断言，但不再是首次校验点。
        let authoritative_layout = if let Some(cfg) = &loaded_config {
            let Some(hw) = resolve_hardware_section(&cfg.parsed, &hardware_id) else {
                return Err(format!(
                    "--hardware-id '{hardware_id}' 在 --config 中无匹配 [[hardware]] 段"
                ));
            };
            validate_hardware_section(hw).map_err(|e| e.to_string())?;
            // v0.5 候选 E 段(3)：与校验同点派生权威 layout（joints 与
            // motor_ids 等长/唯一已由上行校验保证）。
            let pairs: Vec<(&str, u32)> = hw
                .joints
                .iter()
                .map(String::as_str)
                .zip(hw.motor_ids.iter().map(|&m| u32::from(m)))
                .collect();
            compute_layout_id(&robot_id, &hw.id, &pairs)
        } else {
            // §3.5.1 部分落地边界：CLI-only 无 name/id 表，保留既有占位。
            0xCAFE_BABE_DEAD_BEEF
        };

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

        let joint_count = loaded_config
            .as_ref()
            .and_then(|cfg| resolve_hardware_section(&cfg.parsed, &hardware_id))
            .map_or(self.joint_count, |hardware| {
                u8::try_from(hardware.motor_ids.len()).expect("已校验 <=32")
            });
        let rx_trust_dir = self.rx_trust_dir_cli.clone().unwrap_or_else(|| {
            latch_path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        });
        Ok(ResolvedArgs {
            marker_command: self.marker_command,
            rx_trust_dir,
            trust_policy: self.trust_policy,
            notify_mode: self.notify_mode,
            robot_id,
            hardware_id,
            latch_path,
            authoritative_layout,
            heartbeat_timeout_ns: self.heartbeat_timeout_ns,
            smoke_duration: self.smoke_duration,
            tick_period_ns: self.tick_period_ns,
            strict_rt: self.strict_rt,
            serial_fifo_priority: self.serial_fifo_priority,
            joint_count,
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
        "s0_daemon（post-E 通信故障硬化）

USAGE:
  s0_daemon [OPTIONS]
  s0_daemon rx-trust authorize|revoke|status [--robot-id ID] [--rx-trust-dir PATH]

OPTIONS:
  --config <PATH>              configs/<robot>.toml。CLI > TOML > env > 默认（task 25）。
  --robot-id <ID>              Robot identifier。优先级：CLI > TOML > env ROBOT_ID > \"default\"。
  --hardware-id <ID>           本实例控制的硬件（如 \"left_arm\"）。优先级：CLI > TOML 首段 > \"default_arm\"。
  --latch-path <PATH>          EStop latch 文件路径。默认 /var/lib/robot/<robot_id>/estop_latch。
  --rx-trust-dir <PATH>        信任文件与实例锁目录。默认 latch 的父目录。
  --rx-trust-policy <POLICY>   mock SerialRT 可选 marker|static；真实 Feetech 固定 marker。
  --notify-mode <MODE>         required|standalone|auto，默认 auto（看 INVOCATION_ID）。
  --serial-fifo <PRIORITY>     SerialRT 可选 FIFO 优先级，1..99。
  --heartbeat-timeout-ms <MS>  S1 heartbeat 超时阈值（毫秒）。默认 200。
  --smoke-duration <DUR>       运行时长上限（3s / 500ms / 10 均可）。默认无限（直到 signal）。
  --tick-hz <HZ>               rt_thread tick 频率。默认 50。
  --joints <N>                 mock adapter 关节数，1..32。默认 3。
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
  RR server (A1-3 (1/3c-2)):
    /<robot_id>/control/clear_estop
    /<robot_id>/control/set_enabled
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
        // **A1-3 (1/3c-2)**：ClearEStop RR control plane topic（reviewer
        // A1-3 (1/3b) 拍板 namespace）。
        rr_clear_estop: format!("/{r}/control/clear_estop"),
        // **v0.5 候选 C（D5）**：SetEnabled RR control plane topic（第 2 个专用
        // rr_server；与 clear_estop 同 namespace）。
        rr_set_enabled: format!("/{r}/control/set_enabled"),
    }
}

struct Topics {
    sub_command: String,
    sub_heartbeat: String,
    sub_estop: String,
    pub_state: String,
    pub_heartbeat: String,
    rr_clear_estop: String,
    rr_set_enabled: String,
}

// ============================================================================
// main
// ============================================================================

fn main() {
    let supervisor = BootSupervisor::start().unwrap_or_else(|_| std::process::exit(1));
    #[cfg(feature = "test-comm-fault-hook")]
    robot_s0::comm_fault_hook::boot_pause("before_logging");
    let _ = robot_logging::init();
    #[cfg(feature = "test-comm-fault-hook")]
    robot_s0::comm_fault_hook::boot_pause("before_parse");
    let parsed = match Args::parse() {
        Ok(args) => args,
        Err(message) if message == "HELP" => {
            print_help();
            std::process::exit(0);
        }
        Err(message) => {
            tracing::error!(%message, "CLI 解析失败");
            std::process::exit(2);
        }
    };
    let stops = Arc::new(StopFlags::new());
    #[cfg(feature = "test-comm-fault-hook")]
    if let Some(page) = supervisor.shared().observation_arc() {
        stops.set_observation(page);
    }
    #[cfg(feature = "test-comm-fault-hook")]
    robot_s0::comm_fault_hook::boot_pause("before_resolve");
    let args = parsed.resolve().unwrap_or_else(|error| {
        tracing::error!(%error, "配置解析失败");
        std::process::exit(2)
    });
    supervisor.update_joint_count(usize::from(args.joint_count));
    ensure_latch_parent(&args.latch_path).unwrap_or_else(|error| {
        tracing::error!(?error, "latch 目录不可用");
        std::process::exit(1)
    });
    // S48 A3d / W1：纯名称校验必须先于任何 latch / 锁文件写入（含下面信任锁写 PID），
    // 不合法名字是配置错误，退 1，不是被占用。
    if let Err(error) = validate_latch_path(&args.latch_path) {
        tracing::error!(latch = %args.latch_path.display(), %error, "latch 名字不合法");
        std::process::exit(1);
    }
    std::fs::create_dir_all(&args.rx_trust_dir).unwrap_or_else(|error| {
        tracing::error!(?error, "信任目录不可用");
        std::process::exit(1)
    });
    // 锁在 main 持有至 process::exit；任何早退均不提前释放给另一个 daemon。
    let _trust_lock =
        robot_s0::rx_trust::acquire_trust_lock(&args.rx_trust_dir).unwrap_or_else(|error| {
            match error {
                TrustLockError::Held { pid } => {
                    tracing::error!(?pid, "信任锁被占用");
                    std::process::exit(87)
                }
                TrustLockError::Io(error) => {
                    tracing::error!(?error, "信任锁系统错误");
                    std::process::exit(1)
                }
            }
        });
    if let Some(command) = args.marker_command {
        let result =
            robot_s0::rx_trust::run_marker_command(&RealMarkerFs, &args.rx_trust_dir, command);
        let code = match result {
            Ok(outcome) => {
                match robot_s0::rx_trust::marker_status(&RealMarkerFs, &args.rx_trust_dir) {
                    Ok(status) => println!(
                        "primary={:?} backup={:?} canonical_dirty={} authorized_candidate={} outcome={outcome:?}",
                        status.primary,
                        status.backup,
                        status.canonical_dirty(),
                        status.authorized_candidate()
                    ),
                    Err(error) => tracing::error!(?error, "命令后状态读取失败"),
                }
                outcome.exit_code()
            }
            Err(error) => {
                tracing::error!(?error, "信任命令失败");
                1
            }
        };
        std::process::exit(code);
    }
    // S48 A3d / W1：rx-trust 维护子命令不取 latch 锁；正常 daemon 在 run 之前取得
    // 目标稳定锁并持有至 process::exit，退出持久化与旧同步 IPC 路径同受此锁。
    let latch_lock = acquire_latch_lock(&args.latch_path).unwrap_or_else(|error| match error {
        LatchLockError::Held { pid } => {
            tracing::error!(?pid, latch = %args.latch_path.display(), "latch 锁被占用");
            std::process::exit(i32::from(LATCH_LOCK_HELD_EXIT_CODE))
        }
        other => {
            tracing::error!(%other, latch = %args.latch_path.display(), "latch 锁失败");
            std::process::exit(i32::from(other.exit_code()))
        }
    });
    run(&args, &supervisor, &stops, &latch_lock).exit();
}

struct ExecutionResources {
    clock: Arc<dyn RobotClock>,
    ctrl: Arc<HardRtCtrlInner>,
    client: Arc<HardRtCtrlClient>,
    writer_shared: Arc<PersistShared>,
    probe: Arc<robot_s0::serial_preempt::SerialPreemptProbe>,
    diag: Arc<robot_s0::diag::SerialDiagRing>,
    serial: bool,
    authorized: bool,
    latched: bool,
}

fn selected_kind(args: &ResolvedArgs) -> &str {
    args.loaded_config
        .as_ref()
        .and_then(|config| resolve_hardware_section(&config.parsed, &args.hardware_id))
        .map_or("mock_hard_rt", |hardware| hardware.adapter.as_str())
}

fn select_trust(args: &ResolvedArgs) -> Result<Option<SerialTrustPolicy>, i32> {
    match (selected_kind(args), args.trust_policy) {
        ("mock_hard_rt", None) => Ok(None),
        ("mock_serial_rt", override_policy) => {
            Ok(Some(override_policy.unwrap_or(SerialTrustPolicy::Static)))
        }
        ("feetech_sts3215", None) => Ok(Some(SerialTrustPolicy::Marker)),
        (kind, policy) => {
            tracing::error!(kind, ?policy, "不支持的 adapter 信任策略组合");
            Err(2)
        }
    }
}

struct WorkerStart {
    built: BuiltAdapter,
    slot: WriterSlot,
    endpoint: PersistEndpoint,
    client: PersistClient,
}

fn prepare_execution(
    args: &ResolvedArgs,
    latch_lock: &LatchTargetLock,
    base: MonoRawNs,
) -> Result<(ExecutionResources, WorkerStart, StandbyStore), i32> {
    tracing::info!(identity = %args.identity, "S0 实例配置身份");
    let trust = select_trust(args)?;
    // S48 A3d / W1：恢复失败在此返回，绝不进入下面的 adapter_open。
    let (latched, worker_store, standby_store) = recover_latch(args, latch_lock)?;
    let pairing =
        WriterPairing::new(worker_store, standby_store, latch_lock, base).map_err(|error| {
            tracing::error!(?error, "writer/standby 配对拒绝");
            1
        })?;
    let (slot, standby) = pairing.into_parts();
    let writer_shared = Arc::clone(slot.shared());
    let (persist, endpoint) = robot_s0::latch_writer::mailbox(Arc::clone(&writer_shared));
    tracing::info!(latched, "initial estop_latched");
    let serial = trust.is_some();
    let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
    // 服务必须先于 factory 创建，factory、adapter、serial 与 IPC 共享这一组对象。
    let inhibits = if serial {
        InhibitSet::EMPTY.with(InhibitReason::Boot)
    } else {
        InhibitSet::EMPTY
    };
    let authority = Arc::new(
        ControlAuthority::new(AuthorityInit::new(
            latched,
            OutputSafetyState::TorqueUnknown,
            inhibits,
        ))
        .map_err(|error| {
            tracing::error!(?error, "权限实例初始化拒绝");
            1
        })?,
    );
    let ctrl = Arc::new(if serial {
        HardRtCtrlInner::new_serial(authority)
    } else {
        HardRtCtrlInner::new(authority)
    });
    let client = Arc::new(HardRtCtrlClient::new(ctrl.clone()));
    let probe = Arc::new(robot_s0::serial_preempt::SerialPreemptProbe::new(
        clock.clone(),
        ctrl.clone(),
    ));
    let diag = Arc::new(robot_s0::diag::SerialDiagRing::new());
    let services = AdapterBuildServices {
        clock: clock.clone(),
        preempt: Some(probe.clone()),
        diag: Some(diag.clone()),
        call_deadline: DurationNs(50_000_000),
    };
    #[cfg(feature = "test-comm-fault-hook")]
    robot_s0::comm_fault_hook::boot_pause("adapter_open");
    let built = build_adapter(args, serial.then_some(&services))?;
    match (&built, serial) {
        (BuiltAdapter::HardRt(adapter), false) if adapter.info().rt_class == RtClass::HardRT => {}
        (BuiltAdapter::SerialRt(adapter), true)
            if adapter.info().rt_class == RtClass::SerialRT
                && adapter.joint_count() == usize::from(args.joint_count) => {}
        _ => {
            tracing::error!("factory 与已选择执行协议不一致");
            return Err(2);
        }
    }
    let authorized = match trust {
        Some(SerialTrustPolicy::Marker) => {
            let consumed =
                robot_s0::rx_trust::consume_authorization(&RealMarkerFs, &args.rx_trust_dir)
                    .map_err(|error| startup_io_error(&error))?;
            tracing::info!(?consumed, "一次性信任授权已消费");
            consumed.authorized
        }
        Some(SerialTrustPolicy::Static) => true,
        None => false,
    };
    Ok((
        ExecutionResources {
            clock,
            ctrl,
            client,
            writer_shared,
            probe,
            diag,
            serial,
            authorized,
            latched,
        },
        WorkerStart {
            built,
            slot,
            endpoint,
            client: persist,
        },
        standby,
    ))
}

fn startup_io_error(error: &std::io::Error) -> i32 {
    tracing::error!(?error, "S0 启动或存储失败");
    1
}

/// S48 A3d / W1 启动恢复：持锁检查数据对象 → 打开 W0 store → 核目录身份与锁一致
/// → `recover_and_load`。任一步失败退 1（不冒充被占用），且不产生 adapter 副作用。
fn recover_latch(
    args: &ResolvedArgs,
    latch_lock: &LatchTargetLock,
) -> Result<(bool, ClearTxnStore, ClearTxnStore), i32> {
    let objects = latch_lock.check_data_objects().map_err(|error| {
        tracing::error!(%error, latch = %args.latch_path.display(), "latch 数据对象检查失败");
        i32::from(error.exit_code())
    })?;
    let mut store = ClearTxnStore::open(&args.latch_path, &args.robot_id, &args.hardware_id)
        .map_err(|error| recovery_exit(&error))?;
    let lock_identity = latch_lock.directory_identity();
    let store_identity = store.directory_identity();
    if !lock_identity.matches(store_identity) {
        tracing::error!(
            %lock_identity,
            ?store_identity,
            latch = %args.latch_path.display(),
            "latch 目录身份与锁不一致（父目录已被替换），拒绝启动"
        );
        return Err(1);
    }
    let latched = store
        .recover_and_load()
        .map_err(|error| recovery_exit(&error))?;
    tracing::info!(
        primary_present = objects.primary.is_some(),
        backup_present = objects.backup.is_some(),
        latched,
        "latch 启动恢复完成"
    );
    let standby = ClearTxnStore::open(&args.latch_path, &args.robot_id, &args.hardware_id)
        .map_err(|error| recovery_exit(&error))?;
    if !lock_identity.matches(standby.directory_identity()) {
        tracing::error!("standby 目录身份与原锁不同");
        return Err(1);
    }
    Ok((latched, store, standby))
}

/// W0 事务阶段错误 → 退出码 1，并记录真实阶段与相位。
fn recovery_exit(error: &StageError) -> i32 {
    tracing::error!(
        stage = ?error.stage,
        phase = ?error.phase,
        %error,
        "latch 启动恢复失败"
    );
    1
}

fn run(
    args: &ResolvedArgs,
    supervisor: &BootSupervisor,
    stops: &Arc<StopFlags>,
    latch_lock: &LatchTargetLock,
) -> MainExit {
    #[cfg(feature = "test-comm-fault-hook")]
    let file_fault = match robot_s0::comm_fault_hook::file_fault_from_env() {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(%error, "W4 文件故障配置拒绝，尚未准备执行资源");
            return MainExit::empty(1);
        }
    };
    let (resources, start, standby) =
        match prepare_execution(args, latch_lock, supervisor.started_at()) {
            Ok(value) => value,
            Err(code) => return MainExit::empty(code),
        };
    let permit = match supervisor.install_runtime(
        resources.ctrl.authority().clone(),
        resources.writer_shared.clone(),
    ) {
        Ok(permit) => permit,
        Err(refused) => {
            tracing::error!(reason = ?refused.reason, "运行期原实例绑定失败");
            return MainExit::empty(1);
        }
    };
    let mut boot = BootHandles::new();
    let started = run_workers(
        args,
        supervisor,
        stops,
        &resources,
        start,
        &mut boot,
        #[cfg(feature = "test-comm-fault-hook")]
        file_fault,
    );
    match started {
        Err(code) => stop_startup_owned(boot, stops, &resources, standby, supervisor, args, code),
        Ok(state_reader) => {
            #[cfg(feature = "test-comm-fault-hook")]
            robot_s0::comm_fault_hook::boot_pause("before_ready");
            if let Err(code) = send_ready(args.notify_mode) {
                return stop_startup_owned(
                    boot, stops, &resources, standby, supervisor, args, code,
                );
            }
            permit.publish_ready(resources.clock.now());
            let mut runtime = boot.into_runtime();
            // READY 的唯一凭证发布后，第一步只能是不可失败的拥有移动。
            let result = run_runtime(args, supervisor, &resources, &mut runtime, &state_reader);
            finish_runtime_owned(runtime, &resources, standby, supervisor, result)
        }
    }
}

struct BootHandles {
    slots: HandleSlots,
    rejected: Option<WorkerSlot>,
    insert_error: Option<InsertError>,
    meta: WorkerMetadata,
}
struct RuntimeHandles(BootHandles);
#[derive(Default)]
struct WorkerMetadata {
    ipc: Option<Arc<robot_s0::ipc_thread::IpcThreadMetrics>>,
    events: Option<robot_s0::event_ingress::ExternalEvents>,
    executor: Option<ExecutorMetricsRef>,
    publisher: Option<Arc<robot_s0::pub_thread::PubThreadMetrics>>,
    io: Option<Arc<robot_s0::io_thread::IoThreadMetrics>>,
    #[cfg(feature = "test-comm-fault-hook")]
    publish_failure_probe: PublishFailureProbe,
}
impl BootHandles {
    fn new() -> Self {
        let required = [
            HandleKind::Ipc,
            HandleKind::ClearRr,
            HandleKind::SetRr,
            HandleKind::Watchdog,
            HandleKind::Executor,
            HandleKind::Publisher,
            HandleKind::Io,
            HandleKind::LatchWriter,
        ]
        .into_iter()
        .fold(HandleKindSet::EMPTY, HandleKindSet::with);
        Self {
            slots: HandleSlots::new(required),
            rejected: None,
            insert_error: None,
            meta: WorkerMetadata::default(),
        }
    }
    fn install(&mut self, handle: WorkerSlot) -> Result<(), i32> {
        match self.slots.spawned(handle) {
            Ok(()) => Ok(()),
            Err((error, handle)) => {
                // 调用者必须立即返回到同一 owned 收尾，不再继续 spawn。
                self.rejected = Some(handle);
                self.insert_error = Some(error);
                Err(1)
            }
        }
    }
    fn any_finished(&self) -> bool {
        HandleKind::ALL
            .into_iter()
            .any(|kind| self.slots.live_is_finished(kind) == Some(true))
    }
    fn into_runtime(self) -> RuntimeHandles {
        RuntimeHandles(self)
    }
}

/// 此处只保留真实线程结果、凭证和诊断，不将 join 改写成业务成功。
struct ExitSettled {
    _startup: Option<Cp2Settled>,
    _runtime: Option<robot_s0::shutdown_mechanism::TeardownReport>,
    _ipc: Option<thread::Result<robot_s0::ipc_thread::IpcExit>>,
    _executor: Option<thread::Result<robot_s0::hard_rt_ctrl::ExecutorExit>>,
    _writer: WriterSettlement,
    _standby: Option<StandbyStore>,
    _meta: WorkerMetadata,
}
struct Cp2Settled {
    _required: HandleKindSet,
    _joined: [Option<JoinedOutcome>; HandleKind::COUNT],
    _rejected_joined: Option<(HandleKind, JoinedOutcome)>,
    _insert_error: Option<InsertError>,
}
#[derive(Default)]
struct WriterSettlement {
    result: Option<thread::Result<robot_s0::latch_writer::WriterExit>>,
    unused_joined: Option<robot_s0::latch_writer::WriterJoined>,
    fallback: Option<FallbackResult>,
}
#[derive(Debug)]
enum FallbackResult {
    Token(robot_s0::latch_writer::TokenRefused),
    Claim(robot_s0::latch_writer::ClaimRefused),
    Persist(robot_s0::latch_writer::LastResortError),
    Written(robot_s0::latch_writer::Settle),
    Deadline,
}
struct MainExit {
    run_exit: RunExit,
    settled: ExitSettled,
}
impl MainExit {
    fn empty(code: i32) -> Self {
        Self {
            run_exit: RunExit::new(
                code,
                ResidualHandles::from_startup(Ok(std::array::from_fn(|_| None)), None),
            ),
            settled: ExitSettled {
                _startup: None,
                _runtime: None,
                _ipc: None,
                _executor: None,
                _writer: WriterSettlement::default(),
                _standby: None,
                _meta: WorkerMetadata::default(),
            },
        }
    }
    fn exit(self) -> ! {
        let Self { run_exit, settled } = self;
        let _settled = std::mem::ManuallyDrop::new(settled);
        run_exit.exit()
    }
}

fn settle_writer_once(
    settlement: &mut WriterSettlement,
    standby: &mut StandbyStore,
    res: &ExecutionResources,
    deadline: u64,
) {
    if settlement.unused_joined.is_none() || res.writer_shared.obligation.is_settled() {
        return;
    }
    if res.clock.now().0 >= deadline {
        settlement.fallback = Some(FallbackResult::Deadline);
        return;
    }
    let joined = settlement
        .unused_joined
        .take()
        .expect("前述只读检查持有唯一凭证");
    let outcome = match standby.after_join(joined) {
        Err(error) => FallbackResult::Token(error),
        Ok(token) => match res.writer_shared.obligation.claim_guard() {
            Err(error) => FallbackResult::Claim(error),
            Ok(guard) => {
                match token.persist_once(&mut robot_s0::clear_estop_transaction::NoGuard) {
                    Ok(()) => FallbackResult::Written(guard.commit()),
                    Err(error) => {
                        guard.fail();
                        FallbackResult::Persist(error)
                    }
                }
            }
        },
    };
    // 各结果分别记录；无论补写结果如何，此出口都不能重新成为 clean。
    match &outcome {
        FallbackResult::Token(error) => tracing::error!(?error, "备用写凭证拒绝"),
        FallbackResult::Claim(error) => tracing::error!(?error, "备用写领取拒绝"),
        FallbackResult::Persist(error) => tracing::error!(?error, "备用文件写失败"),
        FallbackResult::Written(settle) => {
            tracing::warn!(?settle, "备用文件写完成，退出仍非 clean");
        }
        FallbackResult::Deadline => {}
    }
    settlement.fallback = Some(outcome);
}

#[allow(
    clippy::too_many_lines,
    reason = "原 CP2 截止内逐项停止回收并完整保存所有结果，保留单一拥有出口"
)]
fn stop_startup_owned(
    boot: BootHandles,
    stops: &StopFlags,
    res: &ExecutionResources,
    mut standby: StandbyStore,
    supervisor: &BootSupervisor,
    args: &ResolvedArgs,
    code: i32,
) -> MainExit {
    let shared = supervisor.shared();
    let cp2 = supervisor
        .started_at()
        .0
        .saturating_add(absolute_boot_budget(usize::from(args.joint_count)).0)
        .saturating_add(1_000_000_000);
    let deadline = if shared.spawned_reached.load(Ordering::Acquire) {
        cp2
    } else {
        cp2.min(
            supervisor
                .started_at()
                .0
                .saturating_add(robot_s0::boot_supervisor::PRE_BOOT_BUDGET.0),
        )
    };
    let BootHandles {
        slots,
        mut rejected,
        insert_error,
        meta,
    } = boot;
    #[cfg(feature = "test-comm-fault-hook")]
    let mut meta = meta;
    let required = slots.required();
    let mut joined = std::array::from_fn(|_| None);
    let mut rejected_joined = None;
    stops.request_spawned_stop();
    if let Some(handle) = rejected.as_ref() {
        handle.request_stop();
    }
    let accepted = match slots.try_into_unjoined() {
        Err(original) => Err(original),
        Ok((_, mut accepted)) => {
            for handle in accepted.iter().flatten() {
                handle.request_stop();
            }
            let stop_deadline = deadline.saturating_sub(50_000_000);
            loop {
                if res.clock.now().0 >= stop_deadline {
                    break;
                }
                for (slot, result) in accepted.iter_mut().zip(&mut joined) {
                    join_startup_finished(slot, result);
                }
                if rejected.as_ref().is_some_and(WorkerSlot::is_finished) {
                    let handle = rejected.take().expect("原拒绝句柄仍在槽内");
                    let kind = handle.kind();
                    match handle.join() {
                        Ok(result) => rejected_joined = Some((kind, result)),
                        Err(handle) => rejected = Some(handle),
                    }
                }
                if accepted.iter().all(Option::is_none) && rejected.is_none() {
                    break;
                }
                let remaining = stop_deadline.saturating_sub(res.clock.now().0);
                if remaining == 0 {
                    break;
                }
                thread::sleep(Duration::from_nanos(remaining.min(5_000_000)));
            }
            Ok(accepted)
        }
    };
    #[cfg(feature = "test-comm-fault-hook")]
    {
        meta.publish_failure_probe.ipc_joined = joined[HandleKind::Ipc as usize].is_some();
        meta.publish_failure_probe.executor_joined =
            joined[HandleKind::Executor as usize].is_some();
        export_publish_failure_attempts(supervisor, &meta);
    }
    let mut writer = WriterSettlement::default();
    // 只取 accepted 原 writer；同 kind 的 rejected 结果不能冒充它。
    if let Some(JoinedOutcome::Writer {
        result,
        joined: credential,
    }) = joined[HandleKind::LatchWriter as usize].take()
    {
        writer.result = Some(result);
        writer.unused_joined = Some(credential);
    }
    settle_writer_once(&mut writer, &mut standby, res, deadline);
    let residual = ResidualHandles::from_startup(accepted, rejected);
    #[cfg(feature = "test-comm-fault-hook")]
    if let Some(page) = shared.observation() {
        page.record_residual_bitmap(residual.bitmap());
    }
    MainExit {
        run_exit: RunExit::new(code, residual),
        settled: ExitSettled {
            _startup: Some(Cp2Settled {
                _required: required,
                _joined: joined,
                _rejected_joined: rejected_joined,
                _insert_error: insert_error,
            }),
            _runtime: None,
            _ipc: None,
            _executor: None,
            _writer: writer,
            _standby: Some(standby),
            _meta: meta,
        },
    }
}
fn join_startup_finished(slot: &mut Option<WorkerSlot>, result: &mut Option<JoinedOutcome>) {
    if slot.as_ref().is_some_and(WorkerSlot::is_finished) {
        match slot.take().expect("刚读取的原句柄").join() {
            Ok(joined) => *result = Some(joined),
            Err(handle) => *slot = Some(handle),
        }
    }
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "各字段分别锁存原 ACK、退休、flush 请求和拒绝句柄停止事实，彼此并非互斥状态"
)]
struct RuntimeGates<'a> {
    boot: &'a robot_s0::boot_supervisor::BootShared,
    res: &'a ExecutionResources,
    events: Option<&'a robot_s0::event_ingress::ExternalEvents>,
    generation: u64,
    ack: bool,
    clear: bool,
    flush_requested: bool,
    flushed: u8,
    rejected: Option<&'a WorkerSlot>,
    rejected_stop_requested: bool,
}
impl robot_s0::shutdown_mechanism::TeardownGates for RuntimeGates<'_> {
    fn control_stop_and_notify(&mut self) {
        // 原 arm 已收紧权限，此处只投递同一物理 Stop，不能第二次递增 epoch。
        self.res.client.request_stop_execution();
        if let Some(events) = self.events {
            let _ = events.control(robot_s0::event_ingress::ControlEvent::Stop);
        }
        robot_health::systemd::notify_stopping();
        if let Some(handle) = self.rejected
            && matches!(
                handle.kind(),
                HandleKind::ClearRr
                    | HandleKind::SetRr
                    | HandleKind::Publisher
                    | HandleKind::Io
                    | HandleKind::FreezeControl
                    | HandleKind::HookControl
                    | HandleKind::HealthMonitor
                    | HandleKind::SystemdWatchdog
            )
        {
            handle.request_stop();
            self.rejected_stop_requested = true;
        }
    }
    fn ack_reached(&mut self, _now: u64) -> bool {
        self.ack = self.generation != 0
            && self.boot.runtime_words().generation() == self.generation
            && self
                .boot
                .runtime_words()
                .lowering_ack_epoch
                .load(Ordering::Acquire)
                == self.generation;
        if self.ack
            && !self.rejected_stop_requested
            && let Some(handle) = self.rejected
            && handle.kind() == HandleKind::Executor
        {
            handle.request_stop();
            self.rejected_stop_requested = true;
        }
        self.ack
    }
    fn clear_retired_and_flushed(&mut self, _now: u64) -> bool {
        self.clear = self.boot.clear_retired_for(self.generation);
        if !self.clear {
            return false;
        }
        if !self.flush_requested {
            self.res.writer_shared.request_flush();
            self.flush_requested = true;
        }
        self.flushed = self
            .res
            .writer_shared
            .flush_outcome()
            .map_or(0, |outcome| outcome as u8);
        if self.flushed != 0
            && !self.rejected_stop_requested
            && let Some(handle) = self.rejected
            && matches!(
                handle.kind(),
                HandleKind::Ipc | HandleKind::LatchWriter | HandleKind::Watchdog
            )
        {
            handle.request_stop();
            self.rejected_stop_requested = true;
        }
        self.flushed != 0
    }
}
fn executor_exit_clean(
    exit: Option<&thread::Result<robot_s0::hard_rt_ctrl::ExecutorExit>>,
    metrics: Option<&ExecutorMetricsRef>,
) -> bool {
    matches!(exit,Some(Ok(exit)) if !exit.has_pending_enable())
        && metrics.is_some_and(|metrics| metrics.snapshot_publish_errors() == 0)
}
fn writer_exit_clean(writer: &WriterSettlement, shared: &PersistShared) -> bool {
    matches!(writer.result.as_ref(), Some(Ok(_)))
        && writer.fallback.is_none()
        && shared.obligation.is_settled()
}
fn finish_runtime_owned(
    runtime: RuntimeHandles,
    res: &ExecutionResources,
    mut standby: StandbyStore,
    supervisor: &BootSupervisor,
    result: Result<(), i32>,
) -> MainExit {
    let RuntimeHandles(BootHandles {
        slots,
        rejected,
        insert_error,
        meta,
    }) = runtime;
    #[cfg(feature = "test-comm-fault-hook")]
    let mut meta = meta;
    let shared = supervisor.shared();
    let ticket = shared.arm_runtime_shutdown(res.clock.now());
    let deadline = ticket.map_or_else(|| res.clock.now().0, |ticket| ticket.deadline().0);
    let mut gates = RuntimeGates {
        boot: &shared,
        res,
        events: meta.events.as_ref(),
        generation: shared.runtime_words().generation(),
        ack: false,
        clear: false,
        flush_requested: false,
        flushed: 0,
        rejected: rejected.as_ref(),
        rejected_stop_requested: false,
    };
    let mut report = robot_s0::shutdown_mechanism::teardown(
        slots,
        &mut gates,
        res.clock.as_ref(),
        deadline.saturating_sub(250_000_000),
        DurationNs(5_000_000),
    );
    let mut writer = WriterSettlement::default();
    if let Some((result, joined)) = report.take_writer() {
        writer.result = Some(result);
        writer.unused_joined = Some(joined);
    }
    let ipc = report.take_ipc();
    let executor = report.take_executor();
    settle_writer_once(&mut writer, &mut standby, res, deadline);
    let gate_ack = gates.ack;
    let gate_clear = gates.clear;
    let gate_flushed = gates.flushed;
    if let Some(handle) = rejected {
        report.residual.retain_extra(handle);
    }
    let true_ipc = matches!(ipc.as_ref(),Some(Ok(exit)) if matches!(exit.reason(),robot_s0::ipc_thread::IpcExitReason::Stopped)&&exit.is_quiescent());
    let runtime_ok = result.is_ok()
        && insert_error.is_none()
        && ticket.is_some()
        && true_ipc
        && executor_exit_clean(executor.as_ref(), meta.executor.as_ref())
        && writer_exit_clean(&writer, &res.writer_shared)
        && matches!(shared.first_runtime_sample_fault(), Ok(None));
    let code =
        robot_s0::shutdown_mechanism::exit_code(&robot_s0::shutdown_mechanism::CleanInputs {
            runtime_ok,
            ack_before_executor_stop: gate_ack,
            clear_retired: gate_clear,
            flushed: gate_flushed,
            failures: &report.failures,
            residual_empty: report.residual.is_empty(),
        });
    #[cfg(feature = "test-comm-fault-hook")]
    {
        meta.publish_failure_probe.ipc_joined = report.ipc_joined;
        meta.publish_failure_probe.executor_joined = report.executor_joined;
        export_publish_failure_attempts(supervisor, &meta);
        if let Some(page) = shared.observation() {
            page.record_runtime_stop_trace(&report);
            page.record_residual_bitmap(report.residual.bitmap());
        }
    }
    // 报告中的其余真 Result 保留原样；这里只移动残余，绝不先 Drop。
    let residual = std::mem::replace(
        &mut report.residual,
        ResidualHandles::from_startup(Ok(std::array::from_fn(|_| None)), None),
    );
    MainExit {
        run_exit: RunExit::new(code, residual),
        settled: ExitSettled {
            _startup: None,
            _runtime: Some(report),
            _ipc: ipc,
            _executor: executor,
            _writer: writer,
            _standby: Some(standby),
            _meta: meta,
        },
    }
}

#[cfg(feature = "test-comm-fault-hook")]
#[derive(Default)]
struct PublishFailureProbe {
    executor: Option<ExecutorMetricsRef>,
    ipc: Option<Arc<robot_s0::ipc_thread::IpcThreadMetrics>>,
    executor_joined: bool,
    ipc_joined: bool,
}
enum ExecutorMetricsRef {
    HardRt(Arc<robot_s0::rt_thread::RtThreadMetrics>),
    SerialRt(Arc<robot_s0::serial_io_thread::SerialIoThreadMetrics>),
}
impl ExecutorMetricsRef {
    fn snapshot_publish_errors(&self) -> u64 {
        match self {
            Self::HardRt(metrics) => metrics.snapshot_publish_errors.load(Ordering::Acquire),
            Self::SerialRt(metrics) => metrics.snapshot_publish_errors.load(Ordering::Acquire),
        }
    }
    #[cfg(feature = "test-comm-fault-hook")]
    fn snapshot_publish_failure_attempt_ns(&self) -> u64 {
        match self {
            Self::HardRt(m) => m
                .snapshot_publish_failure_attempt_ns
                .load(Ordering::Acquire),
            Self::SerialRt(m) => m
                .snapshot_publish_failure_attempt_ns
                .load(Ordering::Acquire),
        }
    }
    fn observer(&self, ctrl: Arc<HardRtCtrlInner>) -> robot_health::systemd::LivenessObserver {
        match self {
            Self::HardRt(m) => {
                let m = m.clone();
                robot_health::systemd::LivenessObserver::from_fn("rt_thread", move || {
                    m.tick_count.load(Ordering::Relaxed)
                })
            }
            Self::SerialRt(_) => {
                robot_health::systemd::LivenessObserver::from_fn("serial_io_thread", move || {
                    ctrl.serial_liveness()
                })
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run_workers(
    args: &ResolvedArgs,
    supervisor: &BootSupervisor,
    stops: &Arc<StopFlags>,
    res: &ExecutionResources,
    start: WorkerStart,
    workers: &mut BootHandles,
    #[cfg(feature = "test-comm-fault-hook")] file_fault: Option<
        robot_s0::comm_fault_hook::FileFaultMode,
    >,
) -> Result<robot_transport::RtSnapshotReader<S0StateSnapshot>, i32> {
    let WorkerStart {
        built,
        slot,
        endpoint,
        client,
    } = start;
    let clock = &res.clock;
    let t = topics(args);
    // writer 先取得真实 slot；后续 IPC 是唯一 PersistClient 所有者。
    #[cfg(feature = "test-comm-fault-hook")]
    robot_s0::comm_fault_hook::before_spawn("latch_writer")
        .map_err(|error| startup_io_error(&error))?;
    #[cfg(feature = "test-comm-fault-hook")]
    let writer_hook = robot_s0::comm_fault_hook::writer_hook(file_fault, clock.clone())
        .map_err(|error| startup_io_error(&error))?;
    #[cfg(not(feature = "test-comm-fault-hook"))]
    let writer_hook: Box<dyn robot_s0::latch_writer::WriterHook> =
        Box::new(robot_s0::latch_writer::NoHook);
    let writer = spawn_managed_writer(
        slot,
        endpoint,
        WriterConfig::default(),
        stops.writer_control(clock.clone()),
        clock.clone(),
        writer_hook,
    )
    .map_err(|error| startup_io_error(&error))?;
    workers.install(WorkerSlot::LatchWriter(writer))?;
    stops.mark_spawned(WorkerKind::LatchWriter);
    let (cmd_writer, cmd_reader) = LatestCommandSlot::new(JointCommandBuf::default());
    let (snapshot_pub, snapshot_reader) = snapshot_publisher_with_test_version(
        RtSnapshot::default(),
        "ROBOT_TEST_SNAPSHOT_INITIAL_VERSION",
    )?;
    let (state_pub, state_reader) = snapshot_publisher_with_test_version(
        S0StateSnapshot::default(),
        "ROBOT_TEST_STATE_SNAPSHOT_INITIAL_VERSION",
    )?;
    let (wd_tx, wd_rx) = bounded::<Event>(64);
    let profile = robot_transport::iceoryx2::request_response::RrProfile::S0_CONTROL;
    let high_capacity = profile.c.checked_mul(2).ok_or(1)?;
    let normal_capacity = profile
        .c
        .checked_mul(profile.l.checked_sub(2).ok_or(1)?)
        .ok_or(1)?;
    let response_capacity = profile.c.checked_mul(profile.l).ok_or(1)?;
    let (clear_high_tx, clear_high_rx) = bounded::<ClearEStopRrRequest>(high_capacity);
    let (clear_normal_tx, clear_normal_rx) = bounded::<ClearEStopRrRequest>(normal_capacity);
    let (clear_response_tx, clear_response_rx) = bounded::<ClearEStopRrResponse>(response_capacity);
    let (set_high_tx, set_high_rx) = bounded::<SetEnabledRrRequest>(high_capacity);
    let (set_normal_tx, set_normal_rx) = bounded::<SetEnabledRrRequest>(normal_capacity);
    let (set_response_tx, set_response_rx) = bounded::<SetEnabledRrResponse>(response_capacity);
    let guards = Guards {
        rt_serial: res.serial,
        estop_latched: res.latched,
        adapter_connected: !res.serial,
        calibration_loaded: true,
        joint_state_valid: true,
        power_ok: true,
        heartbeat_ok: false,
        cmd_deadline_valid: true,
        limits_valid: true,
        hw_enable_ok: false,
        adapter_fault_cleared: !res.serial,
        command_mode: robot_core::adapter::ControlMode::Position,
        on_timeout_hint: robot_s0::state_machine::EnabledSub::CtrlLostHold,
    };
    let ipc_topics = (
        t.sub_command.clone(),
        t.sub_heartbeat.clone(),
        t.sub_estop.clone(),
    );
    let ipc = spawn_ipc_thread(robot_s0::ipc_thread::IpcSpawnInputs {
        cfg: IpcThreadConfig {
            initial_guards: guards,
            initial_state: S0State::Safe,
            authoritative_layout: args.authoritative_layout,
            ..IpcThreadConfig::default()
        },
        clock: clock.clone(),
        subscribers_factory: move || make_subscribers(&ipc_topics),
        cmd_writer,
        state_pub,
        watchdog_rx: wd_rx,
        ctrl: res.client.clone(),
        boot: supervisor.shared(),
        persist: client,
        rr_channels: Some(RrChannels {
            request_rx: clear_high_rx,
            normal_request_rx: clear_normal_rx,
            response_tx: clear_response_tx,
        }),
        set_enabled_rr_channels: Some(SetEnabledRrChannels {
            request_rx: set_high_rx,
            normal_request_rx: set_normal_rx,
            response_tx: set_response_tx,
        }),
        stop: stops.flag(WorkerKind::Ipc),
    })
    .map_err(|error| {
        tracing::error!(?error, "IPC spawn 拒绝");
        1
    })?;
    let last_hb = ipc.last_hb_ns();
    workers.meta.events = Some(ipc.event_sender());
    workers.meta.ipc = Some(ipc.metrics().clone());
    #[cfg(feature = "test-comm-fault-hook")]
    {
        workers.meta.publish_failure_probe.ipc = Some(ipc.metrics().clone());
    }
    workers.install(WorkerSlot::Ipc(ipc))?;
    stops.mark_spawned(WorkerKind::Ipc);
    let clear_topic = t.rr_clear_estop.clone();
    let clear = spawn_rr_server_thread(
        RrServerThreadConfig {
            topic: clear_topic.clone(),
            ..RrServerThreadConfig::default()
        },
        clock.clone(),
        move || {
            let node = NodeBuilder::new()
                .create::<IxService>()
                .map_err(robot_s0::rr_server_thread::RrInitError::Node)?;
            let server = robot_transport::iceoryx2::request_response::rr_server_with::<
                markers::ClearEStopRequest,
                markers::ClearEStopResponse,
            >(&node, &clear_topic, &profile)
            .map_err(robot_s0::rr_server_thread::RrInitError::Service)?;
            Ok(robot_s0::rr_server_thread::OwnedRrServerPorts::new(
                server, node,
            ))
        },
        robot_s0::rr_server_thread::TopicRequestSenders {
            high: clear_high_tx,
            normal: clear_normal_tx,
        },
        clear_response_rx,
        stops.flag(WorkerKind::ClearEStopRr),
    )
    .map_err(|error| startup_io_error(&error))?;
    workers.install(WorkerSlot::ClearRr(clear))?;
    stops.mark_spawned(WorkerKind::ClearEStopRr);
    let set_topic = t.rr_set_enabled.clone();
    let set = spawn_set_enabled_rr_server_thread(
        RrServerThreadConfig {
            topic: set_topic.clone(),
            ..RrServerThreadConfig::default()
        },
        clock.clone(),
        move || {
            let node = NodeBuilder::new()
                .create::<IxService>()
                .map_err(robot_s0::rr_server_thread::RrInitError::Node)?;
            let server = robot_transport::iceoryx2::request_response::rr_server_with::<
                markers::SetEnabledRequest,
                markers::SetEnabledResponse,
            >(&node, &set_topic, &profile)
            .map_err(robot_s0::rr_server_thread::RrInitError::Service)?;
            Ok(robot_s0::rr_server_thread::OwnedRrServerPorts::new(
                server, node,
            ))
        },
        robot_s0::rr_server_thread::TopicRequestSenders {
            high: set_high_tx,
            normal: set_normal_tx,
        },
        set_response_rx,
        stops.flag(WorkerKind::SetEnabledRr),
    )
    .map_err(|error| startup_io_error(&error))?;
    workers.install(WorkerSlot::SetRr(set))?;
    stops.mark_spawned(WorkerKind::SetEnabledRr);
    let watchdog = spawn_watchdog(
        WatchdogConfig {
            heartbeat_timeout_ns: args.heartbeat_timeout_ns,
            ..WatchdogConfig::default()
        },
        clock.clone(),
        last_hb,
        wd_tx,
        stops.flag(WorkerKind::Watchdog),
    )
    .map_err(|error| startup_io_error(&error))?;
    workers.install(WorkerSlot::Raw {
        kind: HandleKind::Watchdog,
        join: watchdog,
        stop: stops.flag(WorkerKind::Watchdog),
    })?;
    stops.mark_spawned(WorkerKind::Watchdog);
    #[cfg(feature = "test-comm-fault-hook")]
    {
        let handle = robot_s0::comm_fault_hook::spawn_control_thread(
            args.robot_id.clone(),
            clock.clone(),
            supervisor.shared().observation_arc(),
            WorkerKind::ALL.map(|worker| stops.flag(worker)),
            workers.meta.events.as_ref().expect("已安装 IPC").clone(),
        )
        .map_err(|error| startup_io_error(&error))?;
        workers.install(WorkerSlot::HookControl(handle))?;
        wait_initial_hook(args, supervisor, res, workers)?;
    }
    let executor = match built {
        BuiltAdapter::HardRt(adapter) => ExecutorHandle::HardRt(
            spawn_rt_thread(
                RtThreadConfig {
                    tick_period_ns: args.tick_period_ns,
                    sched_fifo_priority: 70,
                    cpu_affinity: args.cpu_affinity,
                    strict_rt: args.strict_rt,
                },
                clock.clone(),
                adapter,
                cmd_reader,
                snapshot_pub,
                res.ctrl.clone(),
                stops.flag(WorkerKind::Executor),
            )
            .map_err(|error| startup_io_error(&error))?,
        ),
        BuiltAdapter::SerialRt(adapter) => ExecutorHandle::SerialRt(
            spawn_serial_io_thread(
                SerialIoThreadConfig {
                    sched_fifo_priority: args.serial_fifo_priority,
                    cpu_affinity: args.cpu_affinity,
                    strict_rt: args.strict_rt,
                    ..SerialIoThreadConfig::default()
                },
                clock.clone(),
                adapter,
                cmd_reader,
                snapshot_pub,
                res.ctrl.clone(),
                res.probe.clone(),
                res.diag.clone(),
                res.authorized,
                supervisor.shared(),
                stops.flag(WorkerKind::Executor),
            )
            .map_err(|error| startup_io_error(&error))?,
        ),
        BuiltAdapter::SoftRt(adapter) => {
            tracing::error!(adapter = adapter.info().name, "SoftRT 不得进入执行器");
            return Err(2);
        }
    };
    workers.meta.executor = Some(executor.metrics_ref());
    #[cfg(feature = "test-comm-fault-hook")]
    {
        workers.meta.publish_failure_probe.executor = Some(executor.metrics_ref());
    }
    workers.install(match executor {
        ExecutorHandle::HardRt(handle) => WorkerSlot::HardRt(handle),
        ExecutorHandle::SerialRt(handle) => WorkerSlot::SerialRt(handle),
    })?;
    stops.mark_spawned(WorkerKind::Executor);
    let pub_topics = (t.pub_state.clone(), t.pub_heartbeat.clone());
    let publisher = spawn_pub_thread(
        PubThreadConfig {
            joint_layout_id: args.authoritative_layout,
            ..PubThreadConfig::default()
        },
        clock.clone(),
        move || make_publishers(&pub_topics),
        snapshot_reader,
        state_reader.clone(),
        res.ctrl.clone(),
        res.serial.then(|| res.diag.clone()),
        stops.flag(WorkerKind::Pub),
    )
    .map_err(|error| startup_io_error(&error))?;
    workers.meta.publisher = Some(publisher.metrics().clone());
    workers.install(WorkerSlot::Publisher(publisher))?;
    stops.mark_spawned(WorkerKind::Pub);
    let io = spawn_io_thread(
        IoThreadConfig::default(),
        clock.clone(),
        stops.flag(WorkerKind::Io),
    )
    .map_err(|error| startup_io_error(&error))?;
    workers.meta.io = Some(io.metrics().clone());
    workers.install(WorkerSlot::Io(io))?;
    stops.mark_spawned(WorkerKind::Io);
    let spawned_at = clock.now();
    supervisor.mark_workers_spawned(spawned_at);
    #[cfg(feature = "test-comm-fault-hook")]
    robot_s0::comm_fault_hook::boot_pause("after_spawn");
    wait_startup(args, supervisor, res, workers, spawned_at)?;
    tracing::info!(topic=%t.rr_clear_estop,"rr_server_thread started (ClearEStop RR control plane; A1-3 (1/3c-2))");
    tracing::info!(topic=%t.rr_set_enabled,"set_enabled_rr_server started (SetEnabled RR control plane; v0.5 候选 C)");
    send_auto_enable(args, res, workers, stops)?;
    if workers.any_finished() {
        return Err(1);
    }
    Ok(state_reader)
}

#[allow(
    clippy::too_many_lines,
    reason = "同一监督循环按既定顺序判定原截止、退出首因与真实健康观测"
)]
fn run_runtime(
    args: &ResolvedArgs,
    supervisor: &BootSupervisor,
    res: &ExecutionResources,
    runtime: &mut RuntimeHandles,
    state_reader: &robot_transport::RtSnapshotReader<S0StateSnapshot>,
) -> Result<(), i32> {
    let workers = &mut runtime.0;
    let clock = &res.clock;
    eprintln!(
        "boot_ready spawned_ns={} first_exit_ns={} ready_ns={}",
        supervisor.shared().spawned_at_ns.load(Ordering::Acquire),
        res.ctrl
            .serial_boot_progress()
            .first_exit
            .map_or(0, |at| at.0),
        clock.now().0
    );
    install_signal_handlers();
    #[cfg(feature = "test-freeze-hook")]
    {
        let (rt, serial_io) = match workers.meta.executor.as_ref().expect("原执行器 metrics") {
            ExecutorMetricsRef::HardRt(m) => (Some(m.clone()), None),
            ExecutorMetricsRef::SerialRt(m) => (None, Some(m.clone())),
        };
        let handle = robot_s0::freeze_control::spawn_freeze_control_thread(
            robot_s0::freeze_control::FreezeControlContext {
                robot_id: args.robot_id.clone(),
                clock: clock.clone(),
                ipc: workers.meta.ipc.as_ref().expect("原 IPC metrics").clone(),
                rt,
                serial_io,
                pub_thr: workers
                    .meta
                    .publisher
                    .as_ref()
                    .expect("原 pub metrics")
                    .clone(),
                io: workers.meta.io.as_ref().expect("原 io metrics").clone(),
            },
        )
        .map_err(|error| startup_io_error(&error))?;
        workers.install(WorkerSlot::FreezeControl(handle))?;
    }
    let ipc = workers.meta.ipc.as_ref().expect("原 IPC metrics").clone();
    let publisher = workers
        .meta
        .publisher
        .as_ref()
        .expect("原 pub metrics")
        .clone();
    let io = workers.meta.io.as_ref().expect("原 io metrics").clone();
    let observers = vec![
        robot_health::systemd::LivenessObserver::from_fn("ipc_thread", move || {
            ipc.ipc_tick_count.load(Ordering::Relaxed)
        }),
        workers
            .meta
            .executor
            .as_ref()
            .expect("原执行器 metrics")
            .observer(res.ctrl.clone()),
        robot_health::systemd::LivenessObserver::from_fn("pub_thread", move || {
            publisher.tick_count.load(Ordering::Relaxed)
        }),
        robot_health::systemd::LivenessObserver::from_fn("io_thread", move || {
            io.tick_count.load(Ordering::Relaxed)
        }),
    ];
    if let Some(handle) = robot_health::systemd::spawn_watchdog(
        observers,
        robot_health::systemd::WatchdogConfig::new("s0_systemd_wd"),
    ) {
        workers.install(WorkerSlot::SystemdWatchdog(handle))?;
    }
    let config = args
        .loaded_config
        .as_ref()
        .map(|cfg| robot_health::MonitorConfig {
            interval: Duration::from_secs(cfg.parsed.monitor.interval_secs),
            interested_processes: cfg.parsed.monitor.interested_processes.clone(),
            top_n: cfg.parsed.monitor.top_n,
        })
        .unwrap_or_default();
    workers.install(WorkerSlot::HealthMonitor(robot_health::spawn_monitor(
        config,
    )))?;
    let start = clock.now().0;
    let mut last_report = start;
    loop {
        if workers.any_finished() {
            tracing::error!("READY 后 worker 已退出");
            return Err(1);
        }
        let now = clock.now().0;
        if SHUTDOWN.load(Ordering::Acquire)
            || supervisor
                .shared()
                .runtime_words()
                .shutdown_deadline()
                .is_some()
            || args.smoke_duration.is_some_and(|duration| {
                u128::from(now.saturating_sub(start)) >= duration.as_nanos()
            })
            || state_reader.load().state == S0State::Stopping
        {
            return Ok(());
        }
        if now.saturating_sub(last_report) >= 2_000_000_000 {
            tracing::info!(state=?state_reader.load().state,"heartbeat");
            last_report = now;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// S48 A3b（test-only）：在对应线程真实 join 之后，把 worker 在 `publish` 之前取的
/// 耗尽尝试时刻导出到观测页并记日志；0 表示该 worker 未发生 `VersionExhausted`。
/// 未完成 join 的 worker 不导出（如实记录，不猜测）。
#[cfg(feature = "test-comm-fault-hook")]
fn export_publish_failure_attempts(supervisor: &BootSupervisor, workers: &WorkerMetadata) {
    use robot_s0::comm_fault_observation::PublishFailureWorker;
    let probe = &workers.publish_failure_probe;
    let executor_attempt_ns = match (&probe.executor, probe.executor_joined) {
        (Some(metrics), true) => metrics.snapshot_publish_failure_attempt_ns(),
        (Some(_), false) => {
            tracing::warn!(
                worker = "Executor",
                "收尾未完成 join，不导出快照耗尽尝试时刻"
            );
            0
        }
        (None, _) => 0,
    };
    let ipc_attempt_ns = match (&probe.ipc, probe.ipc_joined) {
        (Some(metrics), true) => metrics
            .snapshot_publish_failure_attempt_ns
            .load(Ordering::Acquire),
        (Some(_), false) => {
            tracing::warn!(worker = "Ipc", "收尾未完成 join，不导出快照耗尽尝试时刻");
            0
        }
        (None, _) => 0,
    };
    let shared = supervisor.shared();
    let page = shared.observation();
    for (worker, attempt_ns) in [
        (PublishFailureWorker::Executor, executor_attempt_ns),
        (PublishFailureWorker::Ipc, ipc_attempt_ns),
    ] {
        if attempt_ns == 0 {
            continue;
        }
        if let Some(page) = page {
            page.record_publish_failure_attempt(worker, attempt_ns);
        }
        tracing::error!(
            ?worker,
            failure_attempt_started_ns = attempt_ns,
            error = "VersionExhausted",
            "snapshot 版本耗尽：worker 在该时刻之前取时钟并调用 publish"
        );
    }
}

/// S48 A3b：快照写端构造。生产路径固定 `RtSnapshotPublisher::new`（版本 0）；
/// 仅 `test-comm-fault-hook` 验收组合读取 `ROBOT_TEST_*_INITIAL_VERSION`（偶数，
/// 不超过最大偶版本）以逼近版本耗尽，非法注入值以启动错误退出（exit 1）。
#[cfg_attr(
    not(feature = "test-comm-fault-hook"),
    allow(
        clippy::unnecessary_wraps,
        reason = "生产组合固定成功；Result 是验收注入路径的真实错误出口，两种组合共用签名"
    )
)]
fn snapshot_publisher_with_test_version<T: robot_transport::SnapshotValue>(
    initial: T,
    env_key: &str,
) -> Result<(RtSnapshotPublisher<T>, robot_transport::RtSnapshotReader<T>), i32> {
    #[cfg(feature = "test-comm-fault-hook")]
    if let Some(raw) = std::env::var_os(env_key) {
        let version = raw
            .to_str()
            .and_then(|text| text.parse::<u64>().ok())
            .ok_or_else(|| {
                tracing::error!(env_key, "快照初始版本注入值不是 u64");
                1
            })?;
        return RtSnapshotPublisher::new_with_version(initial, version).map_err(|error| {
            tracing::error!(env_key, %error, "快照初始版本注入值非法");
            1
        });
    }
    #[cfg(not(feature = "test-comm-fault-hook"))]
    let _ = env_key;
    Ok(RtSnapshotPublisher::new(initial))
}

fn send_auto_enable(
    args: &ResolvedArgs,
    res: &ExecutionResources,
    workers: &BootHandles,
    stops: &StopFlags,
) -> Result<(), i32> {
    use robot_s0::event_ingress::{AdmitError, ControlEvent};
    let auto = args
        .loaded_config
        .as_ref()
        .and_then(|c| resolve_hardware_section(&c.parsed, &args.hardware_id))
        .and_then(|h| h.auto_enable)
        .unwrap_or(true);
    if !auto || res.latched || (res.serial && res.ctrl.serial_boot_progress().outcome != 1) {
        return Ok(());
    }
    #[cfg(feature = "test-comm-fault-hook")]
    if std::env::var("ROBOT_TEST_IPC_EXIT_BEFORE_AUTO_ENABLE").as_deref() == Ok("1") {
        stops.request_worker_stop(WorkerKind::Ipc);
        while workers.slots.live_is_finished(HandleKind::Ipc) == Some(false) {
            thread::sleep(Duration::from_millis(1));
        }
    }
    #[cfg(not(feature = "test-comm-fault-hook"))]
    let _ = stops;
    match workers
        .meta
        .events
        .as_ref()
        .expect("已安装的 IPC 保留真实 sender")
        .control(ControlEvent::Enable)
    {
        Ok(()) => Ok(()),
        Err(AdmitError::Full) => {
            tracing::warn!("auto-enable 队列已满，保留原安全状态");
            Ok(())
        }
        Err(AdmitError::Disconnected) => {
            tracing::error!("auto-enable 的 IPC 接收端已退出");
            Err(1)
        }
    }
}
#[cfg(feature = "test-comm-fault-hook")]
fn wait_initial_hook(
    args: &ResolvedArgs,
    supervisor: &BootSupervisor,
    res: &ExecutionResources,
    workers: &mut BootHandles,
) -> Result<(), i32> {
    use robot_s0::shutdown_mechanism::ReadyProbe;
    let deadline = supervisor
        .started_at()
        .0
        .saturating_add(absolute_boot_budget(usize::from(args.joint_count)).0)
        .min(
            supervisor
                .started_at()
                .0
                .saturating_add(robot_s0::boot_supervisor::PRE_BOOT_BUDGET.0),
        );
    loop {
        if workers.any_finished() {
            return Err(1);
        }
        let now = res.clock.now().0;
        if now >= deadline {
            return Err(1);
        }
        match workers.slots.poll_ready(HandleKind::HookControl) {
            ReadyProbe::Ready => return Ok(()),
            ReadyProbe::Pending => {}
            failure => {
                tracing::error!(?failure, "初始验收 history 未应用，禁止启动执行器");
                return Err(1);
            }
        }
        thread::sleep(Duration::from_nanos(
            deadline.saturating_sub(now).min(5_000_000),
        ));
    }
}
fn wait_startup(
    args: &ResolvedArgs,
    supervisor: &BootSupervisor,
    res: &ExecutionResources,
    workers: &mut BootHandles,
    spawned_at: MonoRawNs,
) -> Result<(), i32> {
    use robot_s0::shutdown_mechanism::ReadyProbe;
    let absolute = supervisor
        .started_at()
        .0
        .saturating_add(absolute_boot_budget(usize::from(args.joint_count)).0);
    let physical = spawned_at
        .0
        .saturating_add(boot_budget(usize::from(args.joint_count)).0);
    loop {
        let mut all_ready = true;
        for kind in [
            HandleKind::Ipc,
            HandleKind::ClearRr,
            HandleKind::SetRr,
            HandleKind::Publisher,
        ] {
            match workers.slots.poll_ready(kind) {
                ReadyProbe::Ready => {}
                ReadyProbe::Pending => all_ready = false,
                failure => {
                    tracing::error!(?kind, ?failure, "worker 初始化失败，句柄仍在原 CP2 集合");
                    return Err(1);
                }
            }
        }
        if workers.any_finished() {
            return Err(1);
        }
        let progress = res.ctrl.serial_boot_progress();
        let now = res.clock.now().0;
        let deadline = progress.first_exit.map_or(absolute, |exit| {
            absolute.min(exit.0.saturating_add(STARTUP_ACK_BUDGET.0))
        });
        if now >= deadline {
            tracing::error!(?progress, "启动物理/ACK 截止失败");
            return Err(1);
        }
        if all_ready && (!res.serial || (progress.outcome != 0 && progress.fault_acknowledged)) {
            return Ok(());
        }
        if res.serial && now >= physical {
            res.ctrl.expire_serial_boot();
        }
        thread::sleep(Duration::from_nanos(
            deadline.saturating_sub(now).min(5_000_000),
        ));
    }
}

fn send_ready(mode: NotifyMode) -> Result<(), i32> {
    use robot_health::systemd::ReadyOutcome;
    let required = match mode {
        NotifyMode::Required => true,
        NotifyMode::Standalone => false,
        NotifyMode::Auto => std::env::var_os("INVOCATION_ID").is_some(),
    };
    match robot_health::systemd::try_notify_ready() {
        ReadyOutcome::Sent => {
            tracing::info!("READY 已送达");
            Ok(())
        }
        ReadyOutcome::NotConfigured if !required => {
            tracing::info!("standalone 无 READY socket");
            Ok(())
        }
        ReadyOutcome::NotConfigured => {
            tracing::error!("required 缺少 READY socket");
            Err(1)
        }
        ReadyOutcome::Err(error) => {
            tracing::error!(?error, "READY 发送失败");
            Err(1)
        }
    }
}

fn make_subscribers(
    topics: &(String, String, String),
) -> Result<robot_s0::ipc_thread::OwnedIpcPorts, robot_s0::ipc_thread::IpcInitError> {
    use robot_s0::ipc_thread::{IpcInitError, OwnedIpcPorts};
    let node = NodeBuilder::new()
        .create::<IxService>()
        .map_err(IpcInitError::Node)?;
    let command = open_sub::<markers::JointCommand>(&node, &topics.0, TopicClass::S0Command)
        .map_err(|error| IpcInitError::Subscriber {
            topic: topics.0.clone(),
            error,
        })?;
    let heartbeat = open_sub::<markers::Heartbeat>(&node, &topics.1, TopicClass::Heartbeat)
        .map_err(|error| IpcInitError::Subscriber {
            topic: topics.1.clone(),
            error,
        })?;
    let estop = open_sub::<markers::EStopEvent>(&node, &topics.2, TopicClass::SafetyEvent)
        .map_err(|error| IpcInitError::Subscriber {
            topic: topics.2.clone(),
            error,
        })?;
    Ok(OwnedIpcPorts::new(
        IpcSubscribers {
            command,
            heartbeat,
            estop,
        },
        node,
    ))
}

fn make_publishers(topics: &(String, String)) -> robot_s0::pub_thread::OwnedIpcPublishers {
    let node = NodeBuilder::new().create::<IxService>().expect("pub node");
    let joint_state = open_pub::<markers::JointState>(&node, &topics.0, TopicClass::S0State)
        .expect("state publisher");
    let heartbeat = open_pub::<markers::Heartbeat>(&node, &topics.1, TopicClass::Heartbeat)
        .expect("heartbeat publisher");
    robot_s0::pub_thread::OwnedIpcPublishers::new(
        IpcPublishers {
            joint_state,
            heartbeat,
        },
        node,
    )
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

/// 按 resolved `hardware_id` match `--config` 的 `[[hardware]].id` 段（无
/// `--config` 时 fallback `mock_hard_rt`）走 `robot_core::factory::find_factory`
/// 拿 factory + build。**v0.5 候选 C B6**：与 `auto_enable` 同段（`resolve_
/// hardware_section`，非 `.first()`），无匹配 → fail-fast。
///
/// **task-30 v9 finding #3**：先试 `build_hard_rt`（`HardRT` factory 必返综合
/// 类型）；若返 `Unsupported` → 走 `build` 普通路径。两条路径各自封装在
/// [`BuiltAdapter`] 变体内。
///
/// 错误返回 i32 退出码（与 `run` 顶层一致），调用方据此 `graceful_stop`。
fn build_adapter(
    args: &ResolvedArgs,
    services: Option<&AdapterBuildServices>,
) -> Result<BuiltAdapter, i32> {
    // 1) 决定 adapter_kind + joint_names + address + motor_ids + calibration_paths 来源
    let (adapter_kind, joint_names, address, motor_ids, calibration_paths): (
        String,
        Vec<String>,
        Option<String>,
        Vec<u8>,
        Vec<String>,
    ) = if let Some(cfg) = &args.loaded_config {
        // **v0.5 候选 C B6**：按 resolved hardware_id match `[[hardware]].id`
        // （非 `.first()`），与 auto_enable 同段——避免 CLI `--hardware-id` 选非
        // first 段时 adapter 按 first 构造的错配。
        if let Some(hw) = resolve_hardware_section(&cfg.parsed, &args.hardware_id) {
            // v0.5 候选 E 段(2)（PC1）：motor_ids typed 化——撤 extras
            // `filter_map` 静默丢弃（缺失已在 TOML parse 层 fail-fast）；
            // resolved 段内容校验（等长/非空/唯一）违者 config 错误族 exit 2。
            if let Err(e) = validate_hardware_section(hw) {
                tracing::error!(error = %e, "fatal: [[hardware]] 内容校验失败");
                return Err(e.exit_code());
            }
            let ids = hw.motor_ids.clone();
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
                hardware_id = %args.hardware_id,
                "fatal: config 内无匹配 [[hardware]].id == hardware_id（B6：CLI \
                 --hardware-id 与 config 错配；fail-fast）"
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
        services,
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
    fn metrics_ref(&self) -> ExecutorMetricsRef {
        match self {
            Self::HardRt(handle) => ExecutorMetricsRef::HardRt(handle.metrics().clone()),
            Self::SerialRt(handle) => ExecutorMetricsRef::SerialRt(handle.metrics().clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "test-comm-fault-hook")]
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "同一真实子进程固定失败时序并保原句柄与结果，完整验证迟到快照错误不能被空退出掩盖"
    )]
    fn late_real_snapshot_failure_cannot_make_empty_executor_exit_clean() {
        struct GateClock {
            entered: AtomicBool,
            released: AtomicBool,
        }
        impl RobotClock for GateClock {
            fn now(&self) -> robot_core::time::MonoRawNs {
                SystemClock.now()
            }
            fn sleep_for(&self, duration: robot_core::time::DurationNs) {
                SystemClock.sleep_for(duration);
            }
            fn sleep_until(&self, _deadline: robot_core::time::Deadline) {
                self.entered.store(true, Ordering::Release);
                // 仅确定性线程夹具：暂停真实publish前的调用，不将它算作RT时延证据。
                while !self.released.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
            }
        }
        const CHILD: &str = "ROBOT_W3_LATE_SNAPSHOT_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("child.log");
            let log = std::fs::File::create(&path).unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::late_real_snapshot_failure_cannot_make_empty_executor_exit_clean",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap();
            let end = SystemClock.now().0 + 5_000_000_000;
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) if SystemClock.now().0 < end => {
                        thread::sleep(std::time::Duration::from_millis(2));
                    }
                    other => {
                        let _ = child.kill();
                        let reap = SystemClock.now().0 + 2_000_000_000;
                        while !matches!(child.try_wait(), Ok(Some(_))) {
                            if SystemClock.now().0 >= reap {
                                std::process::abort();
                            }
                            thread::sleep(std::time::Duration::from_millis(2));
                        }
                        panic!("迟到失败子进程已kill/reap：{other:?}");
                    }
                }
            };
            let raw = std::fs::read_to_string(path).unwrap();
            assert!(status.success(), "{raw}");
            assert_eq!(
                raw.lines()
                    .filter(|line| *line == "late_snapshot_real_join_verified")
                    .count(),
                1,
                "{raw}"
            );
            return;
        }
        let main = thread::current().id();
        std::panic::set_hook(Box::new(move |info| {
            eprintln!("迟到快照反例：{info}");
            if thread::current().id() == main {
                std::process::exit(101);
            }
        }));
        let clock = Arc::new(GateClock {
            entered: AtomicBool::new(false),
            released: AtomicBool::new(false),
        });
        let authority = Arc::new(
            robot_core::control_authority::ControlAuthority::new(
                robot_core::control_authority::AuthorityInit::new(
                    false,
                    robot_core::executor_ctrl::OutputSafetyState::TorqueUnknown,
                    robot_core::control_authority::InhibitSet::EMPTY,
                ),
            )
            .unwrap(),
        );
        let ctrl = Arc::new(HardRtCtrlInner::new(authority));
        let (_commands, reader) = robot_transport::LatestCommandSlot::new(
            robot_core::adapter::JointCommandBuf::default(),
        );
        let (publisher, snapshots) = robot_transport::RtSnapshotPublisher::new_with_version(
            RtSnapshot::default(),
            robot_transport::MAX_PUBLISHED_VERSION - 2,
        )
        .unwrap();
        let handle = spawn_rt_thread(
            RtThreadConfig {
                strict_rt: false,
                ..RtThreadConfig::default()
            },
            clock.clone(),
            robot_adapter_mock::MockHardRTAdapter::with_unlimited_limits(1),
            reader,
            publisher,
            ctrl,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let deadline = SystemClock.now().0 + 2_000_000_000;
        while !clock.entered.load(Ordering::Acquire) {
            assert!(SystemClock.now().0 < deadline);
            thread::yield_now();
        }
        let metrics = ExecutorMetricsRef::HardRt(handle.metrics().clone());
        assert!(!handle.is_finished());
        assert_eq!(metrics.snapshot_publish_errors(), 0);
        // 主层最后一次存活观察之后请求停止；正在执行的tick仍会真实发布并失败。
        handle.request_stop();
        clock.released.store(true, Ordering::Release);
        while !handle.is_finished() {
            assert!(SystemClock.now().0 < deadline);
            thread::yield_now();
        }
        let original = Some(handle.join());
        assert!(matches!(original.as_ref(),Some(Ok(exit)) if !exit.has_pending_enable()));
        assert_eq!(snapshots.version(), robot_transport::MAX_PUBLISHED_VERSION);
        assert_eq!(metrics.snapshot_publish_errors(), 1);
        assert!(!executor_exit_clean(original.as_ref(), Some(&metrics)));
        assert!(!executor_exit_clean(original.as_ref(), None));
        eprintln!("late_snapshot_real_join_verified");
        std::process::exit(0);
    }

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

    // ---- v0.5 候选 E 段(2)：身份锁（P.2.2，沿 gateway 先例）----

    fn base_args() -> Args {
        Args {
            marker_command: None,
            rx_trust_dir_cli: None,
            trust_policy: None,
            notify_mode: NotifyMode::Auto,
            robot_id_cli: None,
            hardware_id_cli: None,
            latch_path_cli: None,
            config_path: None,
            heartbeat_timeout_ns: 200_000_000,
            smoke_duration: None,
            tick_period_ns: 20_000_000,
            strict_rt: false,
            serial_fifo_priority: None,
            joint_count: 3,
            cpu_affinity: None,
        }
    }

    fn write_identity_cfg(robot_id: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            "[robot]\nid = \"{robot_id}\"\nconfig_version = \"0.1\"\n\n\
             [[hardware]]\nid = \"arm\"\nadapter = \"mock_hard_rt\"\n\
             joints = [\"j1\"]\nmotor_ids = [1]\n"
        )
        .unwrap();
        f
    }

    #[test]
    fn resolve_identity_lock_mismatch_err() {
        let f = write_identity_cfg("cfg_id");
        let mut args = base_args();
        args.robot_id_cli = Some("cli_id".into());
        args.config_path = Some(f.path().to_path_buf());
        let Err(err) = args.resolve() else {
            panic!("expected identity mismatch error");
        };
        assert!(err.contains("config/flag mismatch"), "{err}");
    }

    #[test]
    fn resolve_identity_lock_match_ok() {
        let f = write_identity_cfg("same_id");
        let mut args = base_args();
        args.robot_id_cli = Some("same_id".into());
        args.config_path = Some(f.path().to_path_buf());
        let resolved = args.resolve().unwrap();
        assert_eq!(resolved.robot_id, "same_id");
    }

    // ---- v0.5 段(2) 回修 P0-2：内容校验前移 resolve()，不受 latch 影响 ----

    #[test]
    fn resolve_rejects_invalid_motor_ids_before_any_latch_io() {
        use std::io::Write;
        // len(motor_ids)=2 != len(joints)=1；robot_id 的 latch 路径不存在——
        // 校验在 resolve() 完成（config 错误族 exit 2），latch（run() 内
        // exit 1 族）根本不被触碰。
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            "[robot]\nid = \"no_latch_robot\"\nconfig_version = \"0.1\"\n\n\
             [[hardware]]\nid = \"arm\"\nadapter = \"mock_hard_rt\"\n\
             joints = [\"j1\"]\nmotor_ids = [1, 2]\n"
        )
        .unwrap();
        let mut args = base_args();
        args.config_path = Some(f.path().to_path_buf());
        let Err(err) = args.resolve() else {
            panic!("expected LenMismatch resolve error");
        };
        assert!(err.contains("len(motor_ids)=2"), "{err}");
    }

    #[test]
    fn resolve_rejects_unknown_hardware_id() {
        let f = write_identity_cfg("r1");
        let mut args = base_args();
        args.hardware_id_cli = Some("nonexistent".into());
        args.config_path = Some(f.path().to_path_buf());
        let Err(err) = args.resolve() else {
            panic!("expected unknown-hardware resolve error");
        };
        assert!(err.contains("无匹配 [[hardware]] 段"), "{err}");
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "同一子进程逐步验证原 writer join 凭据、原截止与唯一备用写结果"
    )]
    fn real_join_is_required_for_once_only_fallback_and_fallback_never_becomes_clean() {
        const NAME: &str =
            "tests::real_join_is_required_for_once_only_fallback_and_fallback_never_becomes_clean";
        const CHILD: &str = "ROBOT_W3_FALLBACK_CHILD";
        if std::env::var(CHILD).as_deref() != Ok("1") {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("raw.log");
            let file = std::fs::File::create(&path).unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture"])
                .env(CHILD, "1")
                .stdout(file.try_clone().unwrap())
                .stderr(file)
                .spawn()
                .unwrap();
            let until = SystemClock.now().0 + 8_000_000_000;
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) if SystemClock.now().0 < until => {
                        thread::sleep(std::time::Duration::from_millis(2));
                    }
                    other => {
                        let _ = child.kill();
                        let reap = SystemClock.now().0 + 2_000_000_000;
                        while !matches!(child.try_wait(), Ok(Some(_))) {
                            if SystemClock.now().0 >= reap {
                                std::process::abort();
                            }
                            thread::sleep(std::time::Duration::from_millis(2));
                        }
                        panic!("备用写子进程已kill/reap：{other:?}");
                    }
                }
            };
            let raw = std::fs::read_to_string(path).unwrap();
            assert!(status.success(), "{raw}");
            assert_eq!(
                raw.lines()
                    .filter(|line| *line == "fallback_real_join_verified")
                    .count(),
                1,
                "{raw}"
            );
            return;
        }
        let owner = thread::current().id();
        std::panic::set_hook(Box::new(move |info| {
            eprintln!("备用写验收：{info}");
            if thread::current().id() == owner {
                std::process::exit(101);
            }
        }));
        for expired in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("estop_latch");
            robot_s0::estop_latch::save_estop_latch(&path, false).unwrap();
            let lock = robot_s0::latch_lock::acquire_latch_lock(&path).unwrap();
            let first = ClearTxnStore::open(&path, "fallback", "arm").unwrap();
            let second = ClearTxnStore::open(&path, "fallback", "arm").unwrap();
            let (slot, mut standby) = WriterPairing::new(first, second, &lock, MonoRawNs(10))
                .unwrap()
                .into_parts();
            let shared = slot.shared().clone();
            let (client, endpoint) = robot_s0::latch_writer::mailbox(shared.clone());
            let clock: Arc<dyn RobotClock> =
                Arc::new(robot_core::time::MockClock::new(MonoRawNs(10)));
            let authority = Arc::new(
                ControlAuthority::new(AuthorityInit::new(
                    false,
                    OutputSafetyState::TorqueOffConfirmed,
                    InhibitSet::EMPTY,
                ))
                .unwrap(),
            );
            let ctrl = Arc::new(HardRtCtrlInner::new(authority));
            let control = Arc::new(HardRtCtrlClient::new(ctrl.clone()));
            let res = ExecutionResources {
                clock: clock.clone(),
                ctrl: ctrl.clone(),
                client: control,
                writer_shared: shared.clone(),
                probe: Arc::new(robot_s0::serial_preempt::SerialPreemptProbe::new(
                    clock.clone(),
                    ctrl,
                )),
                diag: Arc::new(robot_s0::diag::SerialDiagRing::new()),
                serial: false,
                authorized: false,
                latched: false,
            };
            let _ = shared.obligation.request(MonoRawNs(10));
            let stops = Arc::new(StopFlags::new());
            stops.request_worker_stop(robot_s0::worker_stop::WorkerKind::LatchWriter);
            let writer = robot_s0::latch_writer::spawn_writer(
                slot,
                endpoint,
                robot_s0::latch_writer::WriterConfig::default(),
                stops.writer_control(clock.clone()),
                clock,
                Box::new(robot_s0::latch_writer::NoHook),
            )
            .unwrap();
            let mut settlement = WriterSettlement::default();
            settle_writer_once(&mut settlement, &mut standby, &res, 100);
            assert_eq!(std::fs::read(&path).unwrap(), b"0\n");
            assert!(settlement.fallback.is_none());
            let until = SystemClock.now().0 + 2_000_000_000;
            while !writer.is_finished() {
                assert!(SystemClock.now().0 < until);
                thread::yield_now();
            }
            let (result, joined) = writer.join();
            settlement.result = Some(result);
            settlement.unused_joined = Some(joined);
            let deadline = if expired { 10 } else { 100 };
            settle_writer_once(&mut settlement, &mut standby, &res, deadline);
            if expired {
                assert!(matches!(
                    settlement.fallback,
                    Some(FallbackResult::Deadline)
                ));
                settle_writer_once(&mut settlement, &mut standby, &res, deadline);
                assert_eq!(std::fs::read(&path).unwrap(), b"0\n");
                assert!(!shared.obligation.is_settled());
            } else {
                assert!(matches!(
                    settlement.fallback,
                    Some(FallbackResult::Written(
                        robot_s0::latch_writer::Settle::Settled
                    ))
                ));
                assert!(shared.obligation.is_settled());
                assert_eq!(std::fs::read(&path).unwrap(), b"1\n");
                assert_eq!(std::fs::read(path.with_extension("bak")).unwrap(), b"1\n");
                let _ = shared.obligation.request(MonoRawNs(11));
                settle_writer_once(&mut settlement, &mut standby, &res, deadline);
                assert!(!shared.obligation.is_settled());
                assert!(settlement.unused_joined.is_none());
            }
            assert!(!writer_exit_clean(&settlement, &shared));
            // 原无预算凭证/新未结义务、真实join结果及其配对对象保持到受监督进程退出。
            std::mem::forget((settlement, standby, res, client, shared, lock, directory));
        }
        eprintln!("fallback_real_join_verified");
        std::process::exit(0);
    }
}
