//! S0 守护进程：七 worker、SerialRT 启动收敛与独立启动监督。
//!
//! main 首句启动监督器，再执行日志、解析、实例锁与 adapter 构造。共享控制器和
//! bus 服务在 factory 前建立；SerialRT 在七 worker 全部成功后才允许首个总线 IO。
//! 物理预检与故障 ACK 完成后处理自动使能，再发送 READY；运行中 IPC 退出视为错误。
//! 启动失败先 Disabled、仅停止已启动 worker，并在同一绝对截止内回收已结束线程；
//! READY 后保留 RR → pub/io → executor → IPC → watchdog 的顺序关闭。
//!
//! `robot_id` 来源：CLI > TOML > 环境 `ROBOT_ID` > default。具体参数见 `--help`。
//! 退出码：0 正常；1 启动/运行/存储失败；2 参数错误；3 marker 第一阶段已提交但
//! 第二阶段未完成；4 marker 未授权；87 同实例锁竞争；88 独立监督器执法。

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
use robot_core::executor_ctrl::{ExecutorCtrlClient, OutputPolicy, SerialEmergencyCtrl};
use robot_core::factory::{
    AdapterBuildContext, AdapterBuildServices, find_factory, registered_factories,
};
use robot_core::time::{DurationNs, MonoRawNs, RobotClock, SystemClock};
use robot_msg::joint_layout::compute_layout_id;
use robot_msg::markers;
use robot_s0::boot_supervisor::{
    BootSupervisor, STARTUP_ACK_BUDGET, absolute_boot_budget, boot_budget,
};
use robot_s0::config_loader::{
    LoadedConfig, load_robot_config, resolve_hardware_section, validate_hardware_section,
};
use robot_s0::estop_latch::{load_estop_latch, save_estop_latch};
use robot_s0::hard_rt_ctrl::{HardRtCtrlClient, HardRtCtrlInner};
use robot_s0::io_thread::{IoThreadConfig, spawn_io_thread};
use robot_s0::ipc_thread::{
    ClearEStopRrRequest, ClearEStopRrResponse, IpcSubscribers, IpcThreadConfig, IpcThreadHandle,
    RrChannels, SetEnabledRrChannels, SetEnabledRrRequest, SetEnabledRrResponse, spawn_ipc_thread,
};
use robot_s0::pub_thread::{IpcPublishers, PubThreadConfig, spawn_pub_thread};
use robot_s0::rr_server_thread::{RrServerThreadConfig, spawn_rr_server_thread};
use robot_s0::rt_thread::{RtThreadConfig, RtThreadHandle, spawn_rt_thread};
use robot_s0::rx_trust::{MarkerCommand, RealMarkerFs, TrustLockError};
use robot_s0::serial_io_thread::{
    SerialIoThreadConfig, SerialIoThreadHandle, spawn_serial_io_thread,
};
use robot_s0::set_enabled_rr_server::spawn_set_enabled_rr_server_thread;
use robot_s0::state_machine::{Event, Guards, S0State, S0StateSnapshot};
use robot_s0::watchdog::{WatchdogConfig, spawn_watchdog};
use robot_s0::worker_stop::{StopFlags, WorkerKind};
use robot_transport::iceoryx2::request_response::rr_server;
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
    let stops = StopFlags::new();
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
    let code = run(&args, &supervisor, &stops).err().unwrap_or(0);
    std::process::exit(code);
}

struct ExecutionResources {
    clock: Arc<dyn RobotClock>,
    ctrl: Arc<HardRtCtrlInner>,
    client: Arc<dyn ExecutorCtrlClient>,
    emergency: Option<Arc<dyn SerialEmergencyCtrl>>,
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

fn prepare_execution(args: &ResolvedArgs) -> Result<(ExecutionResources, BuiltAdapter), i32> {
    tracing::info!(identity = %args.identity, "S0 实例配置身份");
    let trust = select_trust(args)?;
    let latched = load_estop_latch(&args.latch_path).map_err(|error| startup_io_error(&error))?;
    tracing::info!(latched, "initial estop_latched");
    let serial = trust.is_some();
    let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
    // 服务必须先于 factory 创建，factory、adapter、serial 与 IPC 共享这一组对象。
    let ctrl = Arc::new(if serial {
        HardRtCtrlInner::new_serial()
    } else {
        HardRtCtrlInner::new()
    });
    ctrl.safety_latched.store(latched, Ordering::Release);
    let client = Arc::new(HardRtCtrlClient::new(ctrl.clone()));
    let emergency: Option<Arc<dyn SerialEmergencyCtrl>> =
        if serial { Some(client.clone()) } else { None };
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
            emergency,
            probe,
            diag,
            serial,
            authorized,
            latched,
        },
        built,
    ))
}

fn startup_io_error(error: &std::io::Error) -> i32 {
    tracing::error!(?error, "S0 启动或存储失败");
    1
}

fn run(args: &ResolvedArgs, supervisor: &BootSupervisor, stops: &StopFlags) -> Result<(), i32> {
    tracing::info!(
        robot_id = %args.robot_id,
        hardware_id = %args.hardware_id,
        tick_hz = 1_000_000_000 / args.tick_period_ns,
        strict_rt = args.strict_rt,
        joints = args.joint_count,
        cpu = ?args.cpu_affinity,
        "s0_daemon starting"
    );
    let (resources, built) = prepare_execution(args)?;
    let mut workers = WorkerHandles::default();
    let result = run_workers(args, supervisor, stops, &resources, built, &mut workers);
    resources.client.set_policy(OutputPolicy::Disabled);
    #[cfg(feature = "test-comm-fault-hook")]
    if let Some(page) = supervisor.shared().observation() {
        page.record_disabled(resources.clock.now().0);
    }
    if supervisor.shared().ready_reached.load(Ordering::Acquire) {
        workers.stop_normally(stops);
    } else {
        workers.stop_startup(
            stops,
            resources.clock.as_ref(),
            supervisor
                .started_at()
                .0
                .saturating_add(absolute_boot_budget(usize::from(args.joint_count)).0)
                .saturating_add(1_000_000_000),
        );
    }
    result
}

#[derive(Default)]
struct WorkerHandles {
    ipc: Option<IpcThreadHandle>,
    clear: Option<robot_s0::rr_server_thread::RrServerThreadHandle>,
    set: Option<robot_s0::set_enabled_rr_server::SetEnabledRrServerHandle>,
    watchdog: Option<thread::JoinHandle<()>>,
    executor: Option<ExecutorHandle>,
    publisher: Option<robot_s0::pub_thread::PubThreadHandle>,
    io: Option<robot_s0::io_thread::IoThreadHandle>,
}

impl WorkerHandles {
    fn stop_startup(&mut self, stops: &StopFlags, clock: &dyn RobotClock, deadline: u64) {
        stops.request_spawned_stop();
        loop {
            macro_rules! join_finished {
                ($field:ident) => {
                    if self
                        .$field
                        .as_ref()
                        .is_some_and(|handle| handle.is_finished())
                    {
                        if let Some(handle) = self.$field.take() {
                            let _ = handle.join();
                        }
                    }
                };
            }
            join_finished!(ipc);
            join_finished!(clear);
            join_finished!(set);
            join_finished!(watchdog);
            join_finished!(executor);
            join_finished!(publisher);
            join_finished!(io);
            if self.all_joined() || clock.now().0 >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn all_joined(&self) -> bool {
        self.ipc.is_none()
            && self.clear.is_none()
            && self.set.is_none()
            && self.watchdog.is_none()
            && self.executor.is_none()
            && self.publisher.is_none()
            && self.io.is_none()
    }
    fn any_finished(&self) -> bool {
        self.ipc.as_ref().is_some_and(IpcThreadHandle::is_finished)
            || self
                .clear
                .as_ref()
                .is_some_and(robot_s0::rr_server_thread::RrServerThreadHandle::is_finished)
            || self
                .set
                .as_ref()
                .is_some_and(robot_s0::set_enabled_rr_server::SetEnabledRrServerHandle::is_finished)
            || self
                .watchdog
                .as_ref()
                .is_some_and(thread::JoinHandle::is_finished)
            || self
                .executor
                .as_ref()
                .is_some_and(ExecutorHandle::is_finished)
            || self
                .publisher
                .as_ref()
                .is_some_and(robot_s0::pub_thread::PubThreadHandle::is_finished)
            || self
                .io
                .as_ref()
                .is_some_and(robot_s0::io_thread::IoThreadHandle::is_finished)
    }
    fn stop_normally(&mut self, stops: &StopFlags) {
        if let (Some(ipc), Some(watchdog)) = (self.ipc.take(), self.watchdog.take()) {
            graceful_stop(
                ipc,
                stops,
                watchdog,
                self.executor.take(),
                self.publisher.take(),
                self.io.take(),
                self.clear.take(),
                self.set.take(),
            );
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run_workers(
    args: &ResolvedArgs,
    supervisor: &BootSupervisor,
    stops: &StopFlags,
    res: &ExecutionResources,
    built: BuiltAdapter,
    workers: &mut WorkerHandles,
) -> Result<(), i32> {
    use robot_s0::event_ingress::{ControlEvent, RR_CHANNEL_CAPACITY};
    let clock = &res.clock;
    let t = topics(args);
    let (cmd_writer, cmd_reader) = LatestCommandSlot::new(JointCommandBuf::default());
    let (snapshot_pub, snapshot_reader) = RtSnapshotPublisher::new(RtSnapshot::default());
    let (state_pub, state_reader) = RtSnapshotPublisher::new(S0StateSnapshot::default());
    let (wd_tx, wd_rx) = bounded::<Event>(64);
    let (clear_tx, clear_rx) = bounded::<ClearEStopRrRequest>(RR_CHANNEL_CAPACITY);
    let (clear_response_tx, clear_response_rx) =
        bounded::<ClearEStopRrResponse>(RR_CHANNEL_CAPACITY);
    let (set_tx, set_rx) = bounded::<SetEnabledRrRequest>(RR_CHANNEL_CAPACITY);
    let (set_response_tx, set_response_rx) = bounded::<SetEnabledRrResponse>(RR_CHANNEL_CAPACITY);
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
        hw_enable_ok: true,
        adapter_fault_cleared: !res.serial,
        command_mode: robot_core::adapter::ControlMode::Position,
        on_timeout_hint: robot_s0::state_machine::EnabledSub::CtrlLostHold,
    };
    let ipc_topics = (
        t.sub_command.clone(),
        t.sub_heartbeat.clone(),
        t.sub_estop.clone(),
    );
    workers.ipc = Some(
        spawn_ipc_thread(
            IpcThreadConfig {
                initial_guards: guards,
                initial_state: S0State::Safe,
                latch_path: Some(args.latch_path.clone()),
                authoritative_layout: args.authoritative_layout,
                ..IpcThreadConfig::default()
            },
            clock.clone(),
            move || make_subscribers(&ipc_topics),
            cmd_writer,
            state_pub,
            wd_rx,
            res.client.clone(),
            Some(RrChannels {
                request_rx: clear_rx,
                response_tx: clear_response_tx,
            }),
            Some(SetEnabledRrChannels {
                request_rx: set_rx,
                response_tx: set_response_tx,
            }),
            res.serial.then(|| res.ctrl.clone()),
            res.emergency.clone(),
            stops.flag(WorkerKind::Ipc),
        )
        .map_err(|error| startup_io_error(&error))?,
    );
    stops.mark_spawned(WorkerKind::Ipc);
    let clear_topic = t.rr_clear_estop.clone();
    workers.clear = Some(
        spawn_rr_server_thread(
            RrServerThreadConfig {
                topic: clear_topic.clone(),
                ..RrServerThreadConfig::default()
            },
            clock.clone(),
            move || {
                let node = NodeBuilder::new()
                    .create::<IxService>()
                    .expect("Clear RR node");
                let server = rr_server::<markers::ClearEStopRequest, markers::ClearEStopResponse>(
                    &node,
                    &clear_topic,
                )
                .expect("Clear RR server");
                Box::leak(Box::new(node));
                server
            },
            clear_tx,
            clear_response_rx,
            stops.flag(WorkerKind::ClearEStopRr),
        )
        .map_err(|error| startup_io_error(&error))?,
    );
    stops.mark_spawned(WorkerKind::ClearEStopRr);
    // 既有验收稳定标记：只表明该 RR server 的真实初始化握手已完成。
    tracing::info!(topic = %t.rr_clear_estop, "rr_server_thread started (ClearEStop RR control plane; A1-3 (1/3c-2))");
    let set_topic = t.rr_set_enabled.clone();
    workers.set = Some(
        spawn_set_enabled_rr_server_thread(
            RrServerThreadConfig {
                topic: set_topic.clone(),
                ..RrServerThreadConfig::default()
            },
            clock.clone(),
            move || {
                let node = NodeBuilder::new()
                    .create::<IxService>()
                    .expect("Set RR node");
                let server = rr_server::<markers::SetEnabledRequest, markers::SetEnabledResponse>(
                    &node, &set_topic,
                )
                .expect("Set RR server");
                Box::leak(Box::new(node));
                server
            },
            set_tx,
            set_response_rx,
            stops.flag(WorkerKind::SetEnabledRr),
        )
        .map_err(|error| startup_io_error(&error))?,
    );
    stops.mark_spawned(WorkerKind::SetEnabledRr);
    tracing::info!(topic = %t.rr_set_enabled, "set_enabled_rr_server started (SetEnabled RR control plane; v0.5 候选 C)");
    workers.watchdog = Some(
        spawn_watchdog(
            WatchdogConfig {
                heartbeat_timeout_ns: args.heartbeat_timeout_ns,
                ..WatchdogConfig::default()
            },
            clock.clone(),
            workers.ipc.as_ref().expect("IPC 已启动").last_hb_ns(),
            wd_tx,
            stops.flag(WorkerKind::Watchdog),
        )
        .map_err(|error| startup_io_error(&error))?,
    );
    stops.mark_spawned(WorkerKind::Watchdog);
    #[cfg(feature = "test-comm-fault-hook")]
    let hook_control = robot_s0::comm_fault_hook::spawn_control_thread(
        args.robot_id.clone(),
        clock.clone(),
        supervisor.shared().observation_arc(),
        WorkerKind::ALL.map(|worker| stops.flag(worker)),
    )
    .map_err(|error| startup_io_error(&error))?;
    workers.executor = Some(match built {
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
    });
    stops.mark_spawned(WorkerKind::Executor);
    let pub_topics = (t.pub_state.clone(), t.pub_heartbeat.clone());
    workers.publisher = Some(
        spawn_pub_thread(
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
        .map_err(|error| startup_io_error(&error))?,
    );
    stops.mark_spawned(WorkerKind::Pub);
    workers.io = Some(
        spawn_io_thread(
            IoThreadConfig::default(),
            clock.clone(),
            stops.flag(WorkerKind::Io),
        )
        .map_err(|error| startup_io_error(&error))?,
    );
    stops.mark_spawned(WorkerKind::Io);
    assert!(stops.all_spawned());
    let spawned_at = clock.now();
    supervisor.mark_workers_spawned(spawned_at);
    #[cfg(feature = "test-comm-fault-hook")]
    robot_s0::comm_fault_hook::boot_pause("after_spawn");
    wait_startup(args, supervisor, res, workers, spawned_at)?;
    let ipc = workers.ipc.as_ref().expect("IPC 已启动");
    let event_tx = ipc.event_sender();
    send_auto_enable(args, res, ipc, &event_tx)?;
    if workers.any_finished() {
        tracing::error!("READY 前必需 worker 已退出");
        return Err(1);
    }
    #[cfg(feature = "test-comm-fault-hook")]
    robot_s0::comm_fault_hook::boot_pause("before_ready");
    send_ready(args.notify_mode)?;
    supervisor.mark_ready();
    let first_exit = res
        .ctrl
        .serial_boot_progress()
        .first_exit
        .map_or(0, |t| t.0);
    eprintln!(
        "boot_ready spawned_ns={} first_exit_ns={} ready_ns={}",
        spawned_at.0,
        first_exit,
        clock.now().0
    );
    install_signal_handlers();
    #[cfg(feature = "test-freeze-hook")]
    let freeze_control = {
        let (rt, serial_io) = workers
            .executor
            .as_ref()
            .expect("执行器已启动")
            .freeze_metrics();
        robot_s0::freeze_control::spawn_freeze_control_thread(
            robot_s0::freeze_control::FreezeControlContext {
                robot_id: args.robot_id.clone(),
                clock: clock.clone(),
                ipc: ipc.metrics().clone(),
                rt,
                serial_io,
                pub_thr: workers
                    .publisher
                    .as_ref()
                    .expect("pub 已启动")
                    .metrics()
                    .clone(),
                io: workers.io.as_ref().expect("io 已启动").metrics().clone(),
            },
        )
        .map_err(|error| startup_io_error(&error))?
    };
    let observers = vec![
        {
            let metrics = ipc.metrics().clone();
            robot_health::systemd::LivenessObserver::from_fn("ipc_thread", move || {
                metrics.ipc_tick_count.load(Ordering::Relaxed)
            })
        },
        workers
            .executor
            .as_ref()
            .expect("执行器已启动")
            .make_tick_observer(res.ctrl.clone()),
        {
            let metrics = workers
                .publisher
                .as_ref()
                .expect("pub 已启动")
                .metrics()
                .clone();
            robot_health::systemd::LivenessObserver::from_fn("pub_thread", move || {
                metrics.tick_count.load(Ordering::Relaxed)
            })
        },
        {
            let metrics = workers.io.as_ref().expect("io 已启动").metrics().clone();
            robot_health::systemd::LivenessObserver::from_fn("io_thread", move || {
                metrics.tick_count.load(Ordering::Relaxed)
            })
        },
    ];
    let systemd_watchdog = robot_health::systemd::spawn_watchdog(
        observers,
        robot_health::systemd::WatchdogConfig::new("s0_systemd_wd"),
    );
    let monitor_cfg = args
        .loaded_config
        .as_ref()
        .map(|cfg| robot_health::MonitorConfig {
            interval: Duration::from_secs(cfg.parsed.monitor.interval_secs),
            interested_processes: cfg.parsed.monitor.interested_processes.clone(),
            top_n: cfg.parsed.monitor.top_n,
        })
        .unwrap_or_default();
    let _health_handle = robot_health::spawn_monitor(monitor_cfg);
    let start = clock.now().0;
    let mut last_report = start;
    let mut runtime_result = Ok(());
    loop {
        if ipc.is_finished() {
            tracing::error!(reason = "Disconnected", "READY 后 IPC worker 已退出");
            runtime_result = Err(1);
            break;
        }
        let now = clock.now().0;
        if SHUTDOWN.load(Ordering::Acquire)
            || args.smoke_duration.is_some_and(|duration| {
                u128::from(now.saturating_sub(start)) >= duration.as_nanos()
            })
            || state_reader.load().state == S0State::Stopping
        {
            break;
        }
        if now.saturating_sub(last_report) >= 2_000_000_000 {
            tracing::info!(state = ?state_reader.load().state, "heartbeat");
            last_report = now;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = event_tx.control(ControlEvent::Stop);
    robot_health::systemd::notify_stopping();
    if let Some(handle) = systemd_watchdog {
        handle.stop();
    }
    #[cfg(feature = "test-freeze-hook")]
    {
        freeze_control.request_stop();
        let _ = freeze_control.join();
    }
    #[cfg(feature = "test-comm-fault-hook")]
    {
        hook_control.request_stop();
        if hook_control.is_finished() {
            let _ = hook_control.join();
        }
    }
    let latched = res.ctrl.safety_latched.load(Ordering::Acquire)
        || state_reader.load().guards_summary.estop_latched;
    save_estop_latch(&args.latch_path, latched).map_err(|error| startup_io_error(&error))?;
    tracing::info!("s0_daemon: clean shutdown");
    runtime_result
}

fn send_auto_enable(
    args: &ResolvedArgs,
    res: &ExecutionResources,
    ipc: &IpcThreadHandle,
    event_tx: &robot_s0::event_ingress::ExternalEvents,
) -> Result<(), i32> {
    use robot_s0::event_ingress::{AdmitError, ControlEvent};
    let auto_enable = args
        .loaded_config
        .as_ref()
        .and_then(|c| resolve_hardware_section(&c.parsed, &args.hardware_id))
        .and_then(|h| h.auto_enable)
        .unwrap_or(true);
    if !auto_enable || res.latched || (res.serial && res.ctrl.serial_boot_progress().outcome != 1) {
        return Ok(());
    }
    // 测试仅让真正的 IPC 线程退出，发送与清理仍走原来的生产路径。
    #[cfg(feature = "test-comm-fault-hook")]
    if std::env::var("ROBOT_TEST_IPC_EXIT_BEFORE_AUTO_ENABLE").as_deref() == Ok("1") {
        ipc.request_stop();
        while !ipc.is_finished() {
            thread::sleep(Duration::from_millis(1));
        }
    }
    #[cfg(not(feature = "test-comm-fault-hook"))]
    let _ = ipc;
    match event_tx.control(ControlEvent::Enable) {
        Ok(()) => Ok(()),
        Err(AdmitError::Full) => {
            tracing::warn!(
                reason = "Full",
                "auto-enable 队列已满，保留当前安全状态且不重试"
            );
            Ok(())
        }
        Err(AdmitError::Disconnected) => {
            tracing::error!(reason = "Disconnected", "auto-enable 的 IPC 接收端已退出");
            Err(1)
        }
    }
}

fn wait_startup(
    args: &ResolvedArgs,
    supervisor: &BootSupervisor,
    res: &ExecutionResources,
    workers: &WorkerHandles,
    spawned_at: MonoRawNs,
) -> Result<(), i32> {
    if !res.serial {
        return if workers.any_finished() {
            Err(1)
        } else {
            Ok(())
        };
    }
    let absolute = supervisor
        .started_at()
        .0
        .saturating_add(absolute_boot_budget(usize::from(args.joint_count)).0);
    let physical = spawned_at
        .0
        .saturating_add(boot_budget(usize::from(args.joint_count)).0);
    loop {
        let progress = res.ctrl.serial_boot_progress();
        if workers.any_finished() {
            tracing::error!("启动期间必需 worker 已退出");
            return Err(1);
        }
        if progress.outcome != 0 && progress.fault_acknowledged {
            return Ok(());
        }
        let now = res.clock.now().0;
        if now >= physical {
            res.ctrl.expire_serial_boot();
        }
        let deadline = progress.first_exit.map_or(absolute, |exit| {
            absolute.min(exit.0.saturating_add(STARTUP_ACK_BUDGET.0))
        });
        if now >= deadline {
            tracing::error!(?progress, "启动物理/ACK 截止失败");
            return Err(1);
        }
        thread::sleep(Duration::from_millis(5));
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

fn make_subscribers(topics: &(String, String, String)) -> IpcSubscribers {
    let node = NodeBuilder::new().create::<IxService>().expect("IPC node");
    let command = open_sub::<markers::JointCommand>(&node, &topics.0, TopicClass::S0Command)
        .expect("command subscriber");
    let heartbeat = open_sub::<markers::Heartbeat>(&node, &topics.1, TopicClass::Heartbeat)
        .expect("heartbeat subscriber");
    let estop = open_sub::<markers::EStopEvent>(&node, &topics.2, TopicClass::SafetyEvent)
        .expect("estop subscriber");
    Box::leak(Box::new(node));
    IpcSubscribers {
        command,
        heartbeat,
        estop,
    }
}

fn make_publishers(topics: &(String, String)) -> IpcPublishers {
    let node = NodeBuilder::new().create::<IxService>().expect("pub node");
    let joint_state = open_pub::<markers::JointState>(&node, &topics.0, TopicClass::S0State)
        .expect("state publisher");
    let heartbeat = open_pub::<markers::Heartbeat>(&node, &topics.1, TopicClass::Heartbeat)
        .expect("heartbeat publisher");
    Box::leak(Box::new(node));
    IpcPublishers {
        joint_state,
        heartbeat,
    }
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
    fn is_finished(&self) -> bool {
        match self {
            Self::HardRt(h) => h.is_finished(),
            Self::SerialRt(h) => h.is_finished(),
        }
    }
    fn join(self) -> thread::Result<()> {
        match self {
            Self::HardRt(h) => h.join(),
            Self::SerialRt(h) => h.join(),
        }
    }

    /// systemd watchdog 的 liveness observer：执行 thread `tick_count` 取值。
    /// 在 `spawn_watchdog_thread` 闭包里调用，**不能** RT-thread-aware 加锁
    /// 或 alloc，只用 `AtomicU64::load(Relaxed)`。
    fn make_tick_observer(
        &self,
        ctrl: Arc<HardRtCtrlInner>,
    ) -> robot_health::systemd::LivenessObserver {
        match self {
            Self::HardRt(h) => {
                let m = Arc::clone(h.metrics());
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

    /// **A2-2 freeze hook control routing** (test-only)：把 dispatch 路径活跃
    /// 的 RT 线程 metrics 暴露给 [`robot_s0::freeze_control`] 控制路由。返回
    /// `(rt_metrics, serial_io_metrics)` 二元组，按 dispatch 路径恰好一个为
    /// `Some`。生产 release 不编译本方法。本 bin gate `feature = "test-freeze-hook"`
    /// （不带 `test`），跟 §7b spawn 处一致；详 §7b 注释。
    #[cfg(feature = "test-freeze-hook")]
    fn freeze_metrics(
        &self,
    ) -> (
        Option<Arc<robot_s0::rt_thread::RtThreadMetrics>>,
        Option<Arc<robot_s0::serial_io_thread::SerialIoThreadMetrics>>,
    ) {
        match self {
            Self::HardRt(h) => (Some(Arc::clone(h.metrics())), None),
            Self::SerialRt(h) => (None, Some(Arc::clone(h.metrics()))),
        }
    }
}

/// 有顺序地停线程 + `join`，容忍任一 handle 为 None（早期启动失败场景）。
#[allow(clippy::too_many_arguments)]
fn graceful_stop(
    ipc: IpcThreadHandle,
    stops: &StopFlags,
    wd_handle: std::thread::JoinHandle<()>,
    executor: Option<ExecutorHandle>,
    pub_handle: Option<robot_s0::pub_thread::PubThreadHandle>,
    io_handle: Option<robot_s0::io_thread::IoThreadHandle>,
    rr_handle: Option<robot_s0::rr_server_thread::RrServerThreadHandle>,
    se_rr_handle: Option<robot_s0::set_enabled_rr_server::SetEnabledRrServerHandle>,
) {
    // 顺序（A1-3 (1/3c-2) round 1 拍板 "shutdown 显式 abort pending"）：
    //   rr_server 先停 — 不再接新 RR request 入面；run_loop 出循环后
    //              `abort_pending_on_shutdown` 给所有 in-flight pending
    //              best-effort 回 `RpcStatus::Aborted` + error_msg
    //              "s0 daemon shutting down"。**不**伪造 Ack、**不** drain
    //              等 ipc_thread 真实结果；未送达的 Aborted 由 gateway 按
    //              RR timeout / retry 兜底。详 `crate::rr_server_thread`
    //              shutdown 段 doc。
    //   → pub/io 停（不再往外发）
    //   → executor（rt 或 serial_io）停（不再动 adapter）
    //   → ipc 停（state machine 收尾；rr_server 已停 + rr_response_rx drop
    //              后 ipc_thread 内 RR drain / 派发跑空的 channel；按
    //              crossbeam disconnect 语义自然丢弃）
    //   → watchdog 最后停
    if let Some(h) = rr_handle {
        stops.request_worker_stop(WorkerKind::ClearEStopRr);
        let _ = h.join();
    }
    // **v0.5 候选 C**：第 2 个 rr_server（set_enabled）同 rr_server 阶段先停 —
    // 出循环后 `abort_pending_on_shutdown` 给 in-flight SetEnabled best-effort 回
    // `RpcStatus::Aborted` + domain `PreemptedByShutdown`。
    if let Some(h) = se_rr_handle {
        stops.request_worker_stop(WorkerKind::SetEnabledRr);
        let _ = h.join();
    }
    if let Some(h) = pub_handle {
        stops.request_worker_stop(WorkerKind::Pub);
        let _ = h.join();
    }
    if let Some(h) = io_handle {
        stops.request_worker_stop(WorkerKind::Io);
        let _ = h.join();
    }
    if let Some(h) = executor {
        stops.request_worker_stop(WorkerKind::Executor);
        let _ = h.join();
    }
    stops.request_worker_stop(WorkerKind::Ipc);
    let _ = ipc.join();
    stops.request_worker_stop(WorkerKind::Watchdog);
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
}
