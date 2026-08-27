//! `s1_daemon` —— v0.3 Phase B production S1 binary（B1 hold + heartbeat
//! 闭环 → B2 (3/3) profile + action dispatch）。
//!
//! ### 职责
//!
//! - 启动期读一次 `/<r>/s0/<hw>/state` 锚定当前位置作 hold bias
//!   （[`coordinator::read_anchor_from_s0_state`] / fail-fast：超时
//!   / stale / joint count 错位均退出非 0；不 fallback bias=0）。
//! - **v0.5 候选 E 段(5)** 加：锚定成功后起**常驻** state cache 线程
//!   （[`robot_s1::state_cache`]，同 topic；Scripted `scripted_start_pose`
//!   = 提交时当前位的唯一来源；spawn 失败 = 启动失败 fail-fast，**先于**
//!   `rr_server_threads ready` marker）；shutdown 末步 `take → stop →
//!   join` 显式收线程（AC-E20）。
//! - **v0.5 候选 E 段(8)**（Spec D12，**显式解锁 B1 决策 #7 "不带
//!   sd_notify"**）：全部 worker 就绪后 `READY=1` + liveness-gated systemd
//!   watchdog（`robot_health::systemd`，线程 `s1_systemd_wd`；observer =
//!   tick loop / heartbeat pub / 4 rr server / state cache，各自
//!   `loop_tick_count` 空闲亦推进）；shutdown 先 `STOPPING=1` 再停 ping。
//!   `NOTIFY_SOCKET` / `WATCHDOG_USEC` 未设 = no-op（非 systemd 零差异）。
//!   test-only `--features test-freeze-hook`：`/<robot_id>/s1/test/freeze`
//!   注入线程（`sd_watchdog_smoke` 用）。
//! - **B2 (3/3)** 加：parse `[s1.profile]` + 构造
//!   [`coordinator::Coordinator`]（`BackendRegistry::from_inventory` +
//!   `validate_profile_section` L2 fail-fast）→ `mark_idle` 转 Idle →
//!   tick loop 走 [`coordinator::run_action_tick_loop`] state-aware
//!   dispatch。
//! - 50 Hz publish [`markers::JointCommand`]：
//!   * `Idle` 状态 → hold cmd（`Position` 模式、`target ==
//!     idle_hold_target`〔初值 = 启动 bias；仅 Success-Scripted 后 = 末
//!     waypoint，Spec D3 第 3 条〕、`on_timeout = Hold`、`deadline_ns = now
//!     + 500 ms`）；`Degraded` 状态 → `target == 启动 bias`（不解锁）。
//!   * `Executing` 状态 → action backend `tick()` 自产 cmd body bytes
//!     （override 策略；backend 返 None 时本帧不 publish）。
//! - 50 Hz publish [`markers::Heartbeat`]（`source_id = 2`），所有状态
//!   均不停发（含 Degraded —— S1 内部 degraded 不触发 S0 watchdog
//!   control-lost，详 outline §6.5 决策 #10d）。
//!
//! **B2 (3/3) 默认 profile** = `backends = ["mock_dwell"]` +
//! `command_mode = "joint_position"`：缺失 `[s1.profile]` 等价 B1 行为
//! （没人调 `submit` 时永远在 Idle，hold loop 50Hz 跟 B1 sample-by-sample
//! bit-equivalent；reviewer 阻断 4 锁定的 wiring 回归门槛）。
//!
//! 组合效果：S0 收到 heartbeat 把 state 推到 `Enabled.Active`；
//! 收到 command 写 `LatestCommandSlot`，target=anchor 物理上不动。
//!
//! ### CLI
//!
//! ```text
//! s1_daemon --robot-id <ID> --hardware-id <ID> --config <PATH> [OPTIONS]
//! ```
//!
//! 详 [`Args::parse`] / [`print_help`].
//!
//! ### 退出码
//!
//! - `0`：正常退出（`--duration` 到期 / SIGINT / SIGTERM）。
//! - `1`：iceoryx2 publisher / subscriber open 失败（含段(5) state cache
//!   线程 node create / subscriber open / ready 握手超时）。
//! - `2`：CLI 参数非法。
//! - `3`：配置 TOML 加载 / 解析失败（`[s1]` / `[[hardware]]` / `[s1.profile]`
//!   L1 + L2 校验：`UnknownBackend` / `UnsupportedCommandMode` /
//!   `JointCountMismatch` / `NoEnabledBackends` 全归此码 —— 都是 profile
//!   错配 fail-fast）。
//! - `4`：启动锚定失败（timeout / stale / joint count 错位）—— B1
//!   outline §4.2 + §9 决策 #2 锁定，**不**退化到 bias=0。
//! - `5`：tick loop 内 publish 失败（罕见；transport / iceoryx2 错）；
//!   线程 spawn 失败族（rr_server / reaper / state cache 线程）。
//!
//! ### 非目标（B2 边界，详 outline §1.2 + §9）
//!
//! - ~~**不**带 `sd_notify` / systemd watchdog（B1 决策 #7；Type=simple）~~
//!   ——v0.5 候选 E 段(8) 起 **带**（D12 显式解锁，unit `Type=notify`）。
//! - **不**做 trajectory / IK / scheduler / 真 RL backend（→ B3 / B4）。
//! - **不**起 robot-rpc gateway 进程（→ B4）。
//! - **不**改 `mock_s1`（保留作 ballet 工具，独立于 B2 wiring）。

#![forbid(unused_must_use)]
// 中文 doc 块大量 identifier-like 字串；跟 lib 同款宽松 doc-markdown。
#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::{RobotClock, SystemClock};
use robot_health::systemd::{
    LivenessObserver, WatchdogConfig, notify_ready, notify_stopping, spawn_watchdog,
};
use robot_msg::markers;
use robot_s1::action_backend::BackendRegistry;
use robot_s1::coordinator::{self, Coordinator, CoordinatorConfig, HardwareSection, TickError};
use robot_s1::events_drain::EventsDrainRegistry;
use robot_s1::rr_server::{
    RealHandlerCtx, RrServerThreadConfig, S1RrServerHandles, spawn_clear_error_server,
    spawn_task_cancel_server, spawn_task_invoke_server, spawn_task_status_server,
};
use robot_s1::state_cache::{StateCacheConfig, StateCacheSpawnError, spawn_state_cache_thread};
use robot_transport::iceoryx2::request_response::rr_server;
use robot_transport::iceoryx2::{TopicClass, publisher as open_pub, subscriber as open_sub};

/// `events_drain` 入 `FrameHeader.source_id`：与 v0.2 `JointCommand` /
/// `Heartbeat` 一致约定 = 2（s1 daemon 写出的所有 frame 用此值）。
const S1_FRAME_SOURCE_ID: u32 = 2;

/// v0.5 候选 E 段(5)：常驻 state cache 线程**标称** polling cadence（2 ms，
/// 沿 `robot-rpc::state_pool` 口径）；普通线程调度无硬上界，不构成新鲜度
/// 保证——数据安全由帧 `stamp_ns` + `[s1].max_stale_ms` 在 `latest_fresh`
/// 守门；与 `--poll-interval-ms`（tick loop）无关。
const STATE_CACHE_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// 段(5) state cache spawn 失败 → exit code 族：transport 族（node /
/// subscriber open / ready 握手超时）= 1（沿 "iceoryx2 open 失败"）；线程
/// spawn 失败 = 5（沿 rr_server / reaper 线程 spawn 先例）。
fn state_cache_spawn_exit_code(e: &StateCacheSpawnError) -> i32 {
    match e {
        StateCacheSpawnError::NodeCreate(_)
        | StateCacheSpawnError::SubscriberOpen(_)
        | StateCacheSpawnError::ReadyTimeout => 1,
        StateCacheSpawnError::ThreadSpawn(_) => 5,
    }
}

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
    // SAFETY: libc::signal 是 thread-safe 的 process-global 注册；handler 仅
    // 写一个 `AtomicBool`（async-signal-safe）。重复注册同 handler 等价。
    unsafe {
        libc::signal(libc::SIGINT, h);
        libc::signal(libc::SIGTERM, h);
    }
}

#[cfg(not(target_os = "linux"))]
fn install_signal_handlers() {
    // dev macOS：不装 handler；Ctrl+C 走默认 SIGINT 直接 abort。
}

// ============================================================================
// CLI
// ============================================================================

#[derive(Debug)]
struct Args {
    robot_id: String,
    hardware_id: String,
    config_path: String,
    duration: Option<Duration>,
    poll_interval: Duration,
    /// 段(4)：显式传值仅一致性断言（PD-E5(a)）；None = config 派生。
    layout_id: Option<u64>,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let env_robot_id = std::env::var("ROBOT_ID").ok();
        let mut robot_id = env_robot_id.unwrap_or_default();
        let mut hardware_id = String::new();
        let mut config_path = String::new();
        let mut duration: Option<Duration> = None;
        let mut poll_interval = Duration::from_millis(1);
        let mut layout_id: Option<u64> = None;

        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            match a.as_str() {
                "--robot-id" => {
                    i += 1;
                    robot_id.clone_from(raw.get(i).ok_or("--robot-id requires value")?);
                }
                "--hardware-id" => {
                    i += 1;
                    hardware_id.clone_from(raw.get(i).ok_or("--hardware-id requires value")?);
                }
                "--config" => {
                    i += 1;
                    config_path.clone_from(raw.get(i).ok_or("--config requires value")?);
                }
                "--duration" => {
                    i += 1;
                    duration = Some(parse_duration(
                        raw.get(i).ok_or("--duration requires value")?,
                    )?);
                }
                "--poll-interval-ms" => {
                    i += 1;
                    let ms: u64 = raw
                        .get(i)
                        .ok_or("--poll-interval-ms requires value")?
                        .parse()
                        .map_err(|e| format!("--poll-interval-ms: {e}"))?;
                    if ms == 0 {
                        return Err(
                            "--poll-interval-ms must be > 0 (else MockClock-based tests \
                                    busy-loop; SystemClock 不接受 0 ns sleep)"
                                .into(),
                        );
                    }
                    poll_interval = Duration::from_millis(ms);
                }
                "--layout-id" => {
                    i += 1;
                    let v = raw.get(i).ok_or("--layout-id requires value")?;
                    layout_id = Some(
                        parse_u64_maybe_hex(v).map_err(|e| format!("--layout-id {v:?}: {e}"))?,
                    );
                }
                "--help" | "-h" => return Err("HELP".into()),
                other => return Err(format!("unknown arg: {other}")),
            }
            i += 1;
        }

        if robot_id.is_empty() {
            return Err("--robot-id (or env ROBOT_ID) is required".into());
        }
        if hardware_id.is_empty() {
            return Err("--hardware-id is required".into());
        }
        if config_path.is_empty() {
            return Err("--config is required".into());
        }

        Ok(Self {
            robot_id,
            hardware_id,
            config_path,
            duration,
            poll_interval,
            layout_id,
        })
    }
}

/// 身份锁纯比较（v0.5 候选 E 段(2) 回修 P1-2）：不等 → `Err(先例文案)`
/// （沿 `robot_rpc_gateway.rs:133`）。抽纯函数让 exit 2 分支可 bin unit
/// 直测（match / mismatch 双路）。
fn check_robot_identity(cfg_robot_id: &str, cli_robot_id: &str) -> Result<(), String> {
    if cfg_robot_id == cli_robot_id {
        Ok(())
    } else {
        Err(format!(
            "[robot].id '{cfg_robot_id}' != --robot-id '{cli_robot_id}' (config/flag mismatch; 会订阅错 topic)"
        ))
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

fn parse_u64_maybe_hex(s: &str) -> Result<u64, String> {
    if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(rest, 16).map_err(|e| format!("hex: {e}"))
    } else {
        s.parse::<u64>().map_err(|e| format!("dec: {e}"))
    }
}

fn print_help() {
    eprintln!(
        "s1_daemon (v0.3 B1 production S1)

USAGE:
  s1_daemon --robot-id <ID> --hardware-id <ID> --config <PATH> [OPTIONS]

REQUIRED:
  --robot-id <ID>           Robot identifier (优先级：CLI > env ROBOT_ID)。
  --hardware-id <ID>        匹配 [[hardware]].id 的 section。
  --config <PATH>           TOML 配置（与 daemon --config 同款，[s1] 子段
                            optional）。

OPTIONS:
  --duration <DUR>          运行时长（3s / 500ms / 10）。省略 = 无限。
  --poll-interval-ms <MS>   tick loop 轮询周期（默认 1 ms）。
  --layout-id <ID>          仅一致性断言：与 config 派生值不等 → exit 2
                            （段(4) PD-E5(a)；缺省 = config 派生唯一权威）。
  -h, --help                显示此帮助。

TOPICS:
  sub /<robot_id>/s0/<hardware_id>/state   (启动期一次性锚定 + 段(5) 常驻 state cache)
  pub /<robot_id>/s1/<hardware_id>/command (50 Hz hold)
  pub /<robot_id>/s1/heartbeat             (50 Hz)

SYSTEMD (v0.5 候选 E 段(8), unit Type=notify):
  NOTIFY_SOCKET / WATCHDOG_USEC 由 systemd 注入：全部 worker 就绪后
  READY=1；liveness-gated WATCHDOG=1（tick loop / heartbeat / 4 rr / state
  cache 任一停滞即停喂）；退出先 STOPPING=1。未设置 = no-op。

EXIT CODES:
  0  正常退出（duration / SIGINT / SIGTERM）。
  1  iceoryx2 open publisher / subscriber 失败（含 state cache 线程 node /
     subscriber open / ready 握手超时，v0.5 候选 E 段(5)）。
  2  CLI 参数非法（含 --robot-id 与 [robot].id 冲突，v0.5 候选 E 段(2) 身份锁）。
  3  配置 TOML 加载 / 解析失败（含 motor_ids 必填/等长/唯一校验）。
  4  启动锚定失败（timeout / stale / joint count）—— fail-fast。
  5  tick loop publish 失败（transport / iceoryx2）；线程 spawn 失败族。
"
    );
}

// ============================================================================
// main
// ============================================================================

fn main() {
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
    let topic_state = format!("/{}/s0/{}/state", args.robot_id, args.hardware_id);

    // ---- 加载 TOML 配置（[[hardware]] + [s1]）----
    let toml_text = match std::fs::read_to_string(&args.config_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fatal: read --config {:?}: {e}", args.config_path);
            return Err(3);
        }
    };
    // v0.5 候选 E 段(2) 身份锁（P.2.2；沿 robot_rpc_gateway.rs:133 先例
    // 文案）：`--robot-id`（必填 CLI）与 TOML `[robot].id` 不等 → exit 2
    // （flag/config 冲突族）；`[robot]` 缺失/语法错 → exit 3（config 族）。
    match coordinator::parse_robot_id_from_toml(&toml_text) {
        Ok(cfg_robot_id) => {
            if let Err(msg) = check_robot_identity(&cfg_robot_id, &args.robot_id) {
                eprintln!("fatal: {msg}");
                return Err(2);
            }
        }
        Err(e) => {
            eprintln!("fatal: parse [robot].id: {e}");
            return Err(e.exit_code());
        }
    }
    let hardware =
        match coordinator::parse_hardware_section_from_toml(&toml_text, &args.hardware_id) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("fatal: parse [[hardware]]: {e}");
                return Err(e.exit_code());
            }
        };
    // v0.5 候选 E 段(4)（PD-E5(a)）：layout 由 config 派生（唯一权威；
    // 段(1) 编码单源）。显式 --layout-id 仅一致性断言，冲突 = exit 2。
    let derived_layout = {
        let pairs: Vec<(&str, u32)> = hardware
            .joints
            .iter()
            .map(String::as_str)
            .zip(hardware.motor_ids.iter().map(|&m| u32::from(m)))
            .collect();
        robot_msg::joint_layout::compute_layout_id(&args.robot_id, &args.hardware_id, &pairs)
    };
    if let Some(cli) = args.layout_id
        && cli != derived_layout
    {
        eprintln!(
            "fatal: --layout-id {cli:#018x} != config 派生 {derived_layout:#018x}\
             （PD-E5(a)：config 派生唯一权威，显式传值仅一致性断言）"
        );
        return Err(2);
    }

    let cfg = match coordinator::parse_s1_section_from_toml(&toml_text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fatal: parse [s1]: {e}");
            return Err(3);
        }
    };

    // ---- 加载 [s1.profile]（B2 (3/3) wiring）----
    // 缺失 [s1.profile] 走 ProfileSection::default_b2 = mock_dwell +
    // joint_position（reviewer 拍板：等价 B1 no-active-action 路径）。
    let profile = match coordinator::parse_profile_section_from_toml(&toml_text) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fatal: parse [s1.profile]: {e}");
            return Err(3);
        }
    };

    let joints_count = hardware.joints.len();
    tracing::info!(
        robot_id = %args.robot_id,
        hardware_id = %args.hardware_id,
        joints = joints_count,
        heartbeat_hz = cfg.heartbeat_hz,
        command_hz = cfg.command_hz,
        deadline_ms = cfg.deadline_ms,
        anchor_timeout_ms = cfg.anchor_timeout_ms,
        max_stale_ms = cfg.max_stale_ms,
        topic_cmd = %topic_cmd,
        topic_hb = %topic_hb,
        topic_state = %topic_state,
        duration = ?args.duration,
        layout_id = format!("{derived_layout:#x}"),
        profile_backends = ?profile.backends,
        profile_command_mode = %profile.command_mode,
        "s1_daemon starting"
    );

    // ---- 构造 iceoryx2 publishers + subscriber ----
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

    // ---- 启动锚定（fail-fast：timeout / stale / joint count 不 fallback）----
    install_signal_handlers();
    let clock = SystemClock::new();
    let bias = anchor_or_fail(
        &node,
        &topic_state,
        &hardware,
        &cfg,
        &clock,
        args.poll_interval,
    )?;
    tracing::info!(?bias, "s1_daemon anchored to S0 state");

    // ---- v0.5 候选 E 段(5)：常驻 state cache 线程（Spec D3 第 1 条 +
    //      P.2.5；AC-E20 线程卫生）----
    //
    // 锚定已证 S0 在发 state；此处起常驻 subscriber 线程（线程内建 node +
    // subscriber，sync_channel ready 握手）。失败 = 启动失败 fail-fast（不
    // 降级为"无 cache 仍启动"——那会让每个 Scripted submit 都被拒，且静默）；
    // 成功后才走后续 rr_server 线程 + `rr_server_threads ready` marker。
    let state_cache = match spawn_state_cache_thread(
        StateCacheConfig {
            topic: topic_state.clone(),
            expected_joints: joints_count,
            expected_layout: derived_layout,
            max_stale_ms: cfg.max_stale_ms,
            poll_interval: STATE_CACHE_POLL_INTERVAL,
        },
        Arc::new(SystemClock::new()),
    ) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("fatal: spawn state cache thread: {e}");
            return Err(state_cache_spawn_exit_code(&e));
        }
    };
    tracing::info!(
        topic_state = %topic_state,
        expected_joints = joints_count,
        expected_layout = format!("{derived_layout:#x}"),
        max_stale_ms = cfg.max_stale_ms,
        poll_interval_ms = STATE_CACHE_POLL_INTERVAL.as_millis(),
        "s1_daemon state cache thread ready"
    );

    // ---- 构造 BackendRegistry + L2 校验（B2 (3/3) wiring）----
    // BackendRegistry::from_inventory 永不失败（编译期决定），但
    // validate_profile_section 会拒：unknown backend / unsupported
    // command_mode / joint count mismatch / no enabled backends 任一项
    // → exit 3。
    let registry = BackendRegistry::from_inventory();
    if let Err(e) =
        coordinator::validate_profile_section(&profile, &registry, &hardware, bias.len())
    {
        eprintln!("fatal: validate [s1.profile]: {e}");
        return Err(3);
    }

    // ---- 构造 IK solver（v0.4 候选 2 round 2 (3/4) Q-IK11 (a) backend-ik
    //      default-on + Q-IK3 (a) Option<Box<dyn ArmIkSolver>>）----
    //
    // feature on + profile 启用 `"ik"` → 构造 XLeRobotArmIkSolver(Side::Right)
    // 注入 Coordinator。feature off → solver 类型不存在，传 None；
    // validate_profile_section 会先 UnknownBackend 拦下 `profile = ["ik"]`，
    // 不会漏到这里。feature on + profile 没启 ik → solver 注 None 即可。
    //
    // Q-IK10b (i) startup 期校验：validate_solver_for_profile 在 Coordinator::new
    // 之前调，profile 写 `"ik"` 但 solver = None 直接 exit 3。
    #[cfg(feature = "backend-ik")]
    let solver: Option<Box<dyn robot_kinematics::ArmIkSolver>> = {
        if profile.backends.iter().any(|b| b == "ik") {
            Some(Box::new(
                robot_kinematics_xlerobot::XLeRobotArmIkSolver::new(
                    robot_kinematics_xlerobot::Side::Right,
                ),
            ))
        } else {
            None
        }
    };
    #[cfg(not(feature = "backend-ik"))]
    let solver: Option<Box<dyn robot_kinematics::ArmIkSolver>> = None;

    if let Err(e) = coordinator::validate_solver_for_profile(
        &profile,
        /* solver_present */ solver.is_some(),
    ) {
        eprintln!("fatal: validate solver injection: {e}");
        return Err(3);
    }

    // ---- 构造 Coordinator 并 mark_idle（Booting → Idle）----
    let coordinator = Coordinator::new(
        cfg,
        hardware,
        derived_layout,
        bias,
        registry,
        profile.backends.clone(),
        solver,
    )
    .with_state_cache(state_cache);
    if let Err(e) = coordinator.mark_idle() {
        // Coordinator::new 总是构造在 Booting；mark_idle 仅 Booting → Idle
        // 接受，理论不可能失败。失败 = 内部状态机 bug。
        eprintln!("fatal: coordinator.mark_idle() unexpected failure: {e}");
        return Err(5);
    }
    tracing::info!(state = ?coordinator.current_state(), "s1_daemon coordinator ready");
    let coordinator = Arc::new(coordinator);

    // ---- 起 events drain pubsub registry + RealHandlerCtx ----
    //
    // B4 (2/3-a) wire-up：rr_server handler 闭包真调 Coordinator::{submit,
    // cancel, status, clear_error}，invoke 路径成功时 spawn_events_drain_thread
    // + 调 events_registry.register(handle)。详
    // crates/robot-s1/src/rr_server/handler.rs 顶部 module doc。
    let events_registry = Arc::new(EventsDrainRegistry::new());
    let handler_clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
    let ctx = Arc::new(RealHandlerCtx {
        coordinator: Arc::clone(&coordinator),
        events_registry: Arc::clone(&events_registry),
        clock: Arc::clone(&handler_clock),
        source_id: S1_FRAME_SOURCE_ID,
        robot_id: args.robot_id.clone(),
    });
    tracing::info!(
        active_drains = events_registry.len(),
        "s1_daemon events drain registry ready"
    );

    // ---- spawn 4 个 RR server 线程（B4 (2/3-a) 真 handler）----
    //
    // 每个 RR topic 一个独立 std::thread + 独立 Iceoryx2RrServer + 独立
    // handler 闭包（捕获 Arc<RealHandlerCtx>）。handler 调
    // Coordinator::{submit, cancel, status, clear_error} in-process API；
    // invoke 路径成功 → spawn events_drain_thread + register（详
    // rr_server::handler::handle_invoke）。
    let rr_handles = spawn_s1_rr_server_threads(&args.robot_id, &ctx).map_err(|e| {
        eprintln!("fatal: spawn s1 rr_server_threads: {e}");
        5
    })?;
    tracing::info!(
        topic_invoke = rr_handles.task_invoke.topic(),
        topic_cancel = rr_handles.task_cancel.topic(),
        topic_status = rr_handles.task_status.topic(),
        topic_clear = rr_handles.clear_error.topic(),
        "s1_daemon rr_server_threads ready (B4 (2/3-a) real handler)"
    );

    // ---- v0.4 候选 3 AC-C1 reaper thread（1s cadence；AC-C1 outline
    //      §3 拍板）----
    //
    // 周期调 events_registry.reap_finished() 回收已 self-exit 的 drain
    // thread handle（terminal+grace / disconnect / stop signal 三路退出
    // 之一）。`reaped > 0` 时 info 级 log `events_registry_len` 让
    // smoke binary 通过 stderr scrape 看 C1 收敛曲线（AC-C1 outline
    // §3 reviewer round 1 阻断 2 拍板）。
    //
    // **不**碰 hot path：handler::handle_invoke / events_drain
    // run_drain_loop 不动。**不**改 EventsDrainRegistry::register API。
    //
    // 接 Arc<EventsDrainRegistry>（跟 ctx / main 共享同一 registry；
    // shutdown 顺序保证 reaper 先 stop+join，再走 events_registry.
    // stop_and_join_all）。
    let events_registry_for_reaper = Arc::clone(&events_registry);
    let reaper_stop = Arc::new(AtomicBool::new(false));
    let reaper_stop_clone = Arc::clone(&reaper_stop);
    let reaper_handle = std::thread::Builder::new()
        .name("s1_events_reaper".into())
        .spawn(move || {
            while !reaper_stop_clone.load(Ordering::Acquire) {
                let reaped = events_registry_for_reaper.reap_finished();
                if reaped > 0 {
                    tracing::info!(
                        reaped,
                        events_registry_len = events_registry_for_reaper.len(),
                        op = "reap",
                        "s1_daemon events_drain reaper: reaped finished handles"
                    );
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })
        .map_err(|e| {
            eprintln!("fatal: spawn s1 events reaper thread: {e}");
            5
        })?;
    tracing::info!("s1_daemon events_drain reaper thread spawned (1s cadence)");

    // ---- v0.5 候选 E 段(8)：test-only freeze control（仅 `--features
    //      test-freeze-hook`；沿 s0_daemon 只 gate feature 不带 test）----
    //
    // 所有 worker（tick loop metrics / 4 rr server / state cache）都已存在后
    // 起注入线程；它**不**进 watchdog observer（测试注入器非 worker）。
    #[cfg(feature = "test-freeze-hook")]
    let freeze_control_handle: Option<robot_s1::freeze_control::FreezeControlHandle> =
        if let Some(state_cache) = coordinator.state_cache_liveness() {
            let ctx = robot_s1::freeze_control::FreezeControlContext {
                robot_id: args.robot_id.clone(),
                clock: Arc::clone(&handler_clock),
                tick_loop: coordinator.loop_metrics(),
                rr_servers: vec![
                    rr_handles.task_invoke.metrics(),
                    rr_handles.task_cancel.metrics(),
                    rr_handles.task_status.metrics(),
                    rr_handles.clear_error.metrics(),
                ],
                state_cache,
            };
            match robot_s1::freeze_control::spawn_freeze_control_thread(ctx) {
                Ok(h) => {
                    tracing::info!("s1 freeze_control thread spawned (test-freeze-hook feature)");
                    Some(h)
                }
                Err(e) => {
                    tracing::error!(
                        error = ?e,
                        "s1 freeze_control thread spawn failed; freeze hook control inactive"
                    );
                    None
                }
            }
        } else {
            tracing::error!("s1 freeze_control: state cache not attached; control inactive");
            None
        };

    // ---- v0.5 候选 E 段(8)：systemd READY=1 + liveness-gated watchdog
    //      （Spec D12；B1 决策 #7 显式解锁，登记 CHANGELOG）----
    //
    // 顺序：全部 observer 已就绪（rr ×4 spawn 成功、state cache ready 握手
    // 已过、tick loop metrics 即将由本线程推进）→ READY=1 → 起 watchdog
    // （enforce 自 grace 周期后开始）。observer 全部读 `loop_tick_count` 族
    // （空闲亦推进），不用请求计数（D12 r3 T4 第 1 条）。非 systemd 环境
    // 两者均 no-op / None。
    let observers: Vec<LivenessObserver> = {
        let tick = coordinator.loop_metrics();
        let hb = coordinator.loop_metrics();
        let rr_invoke = rr_handles.task_invoke.metrics();
        let rr_cancel = rr_handles.task_cancel.metrics();
        let rr_status = rr_handles.task_status.metrics();
        let rr_clear = rr_handles.clear_error.metrics();
        let mut v = vec![
            LivenessObserver::from_fn("tick_loop", move || {
                tick.loop_tick_count.load(Ordering::Relaxed)
            }),
            LivenessObserver::from_fn("heartbeat_pub", move || {
                hb.heartbeat_published.load(Ordering::Relaxed)
            }),
            LivenessObserver::from_fn("rr_task_invoke", move || {
                rr_invoke.loop_tick_count.load(Ordering::Relaxed)
            }),
            LivenessObserver::from_fn("rr_task_cancel", move || {
                rr_cancel.loop_tick_count.load(Ordering::Relaxed)
            }),
            LivenessObserver::from_fn("rr_task_status", move || {
                rr_status.loop_tick_count.load(Ordering::Relaxed)
            }),
            LivenessObserver::from_fn("rr_clear_error", move || {
                rr_clear.loop_tick_count.load(Ordering::Relaxed)
            }),
        ];
        if let Some(cache) = coordinator.state_cache_liveness() {
            v.push(LivenessObserver::from_fn("state_cache", move || {
                cache.loop_tick_count.load(Ordering::Relaxed)
            }));
        }
        v
    };
    let observer_count = observers.len();
    notify_ready();
    let systemd_wd_handle = spawn_watchdog(observers, WatchdogConfig::new("s1_systemd_wd"));
    tracing::info!(
        watchdog_active = systemd_wd_handle.is_some(),
        observers = observer_count,
        "s1_daemon systemd READY=1 sent (no-op when NOTIFY_SOCKET unset)"
    );

    // ---- tick loop（state-aware：Idle/Degraded → hold；Executing →
    //      action override；ShuttingDown → break）----
    let tick_result = coordinator::run_action_tick_loop(
        coordinator.as_ref(),
        &mut hb_pub,
        &mut cmd_pub,
        &clock,
        args.poll_interval,
        args.duration,
        &SHUTDOWN,
    );

    // ---- v0.5 候选 E 段(8) D12 第 4 条：先 STOPPING=1 让 systemd 切
    //      deactivating（停 WatchdogSec 计时），再停 watchdog ping 线程；顺序
    //      反过来 graceful 期间稍长会被 systemd 误杀。test-only freeze control
    //      随后 stop + join（测试注入器，不在 worker 停链内）。----
    notify_stopping();
    if let Some(h) = systemd_wd_handle {
        h.stop();
    }
    #[cfg(feature = "test-freeze-hook")]
    if let Some(h) = freeze_control_handle {
        h.request_stop();
        if let Err(panic_payload) = h.join() {
            tracing::error!(
                ?panic_payload,
                "s1_daemon: freeze_control thread join returned panic; continuing shutdown"
            );
        }
    }

    // ---- shutdown 顺序契约（reviewer round 6 阻断锁定 + v0.4 候选 3
    //      AC-C1 outline §4 加 step 4 reaper.stop+join + v0.5 候选 E 段(5)
    //      加 step 6 state cache take+stop+join；段(8) 前置 STOPPING=1 +
    //      watchdog stop）----
    //
    // 1. coordinator.mark_shutting_down → 拒新 submit（Coordinator::submit
    //    返 SubmitError::ShuttingDown → handler 编 ActionRpcStatus::Unavailable
    //    response，不调 EventsDrainRegistry::register）。
    // 2. rr.request_stop_all → 通知所有 RR thread 不接新 wire request。
    // 3. rr.join_all → 等所有 in-flight handler 跑完（含已经在 submit 路
    //    径上的 invoke handler 完成 spawn_events_drain_thread + register）。
    // 4. (v0.4 候选 3 AC-C1) reaper.stop + join → 停 reaper thread 让
    //    shutdown 串行化。reaper 跟 stop_and_join_all 的**并发互斥本质**
    //    是 registry 内部同一把 `Mutex`（reap_finished 锁内 swap_remove
    //    + stop_and_join_all 锁内 mem::take 互斥；详
    //    `EventsDrainRegistry` 顶部 doc "并发语义" 段）；step 4 在 step
    //    5 之前调 reaper_handle.join() 已彻底等 reaper thread 退出，
    //    step 5 看到的 registry inner Vec 状态稳定。cadence (reaper 1s
    //    sleep) 仅影响 shutdown 最坏等待时间（reaper 在下一次轮询 stop
    //    signal 时退出，≤ 1s），**不**是互斥保证。
    // 5. events_registry.stop_and_join_all → 等所有剩余 (未被 reap
    //    回收) drain thread 跑完。通过 `Arc::deref` 直接调用（registry
    //    方法接 `&self`），**不**走 `Arc::try_unwrap` —— 避免 ref-leak
    //    编程 bug 时打破 join 确定性（reviewer round 6：daemon `run`
    //    可能在测试 / 嵌入式 runner 里返回而不是立刻 process exit，未
    //    join 线程会悬挂）。`mark_shutting_down` + 第 3 步 `rr.join_all`
    //    已保证此时不再有新 `register` 调用进来。
    //
    // 关键 invariant：第 3 步 `rr.join_all` 必须先于第 4 步 reaper.stop
    // —— 避免 RR thread stop-but-not-join 期间 in-flight invoke 走
    // submit 成功路径并调 `events_registry.register`，reaper 锁内 reap
    // 跟 stop_and_join_all 撞 late register（沿 reviewer round 6 同款
    // late register race 推理；详 `crates/robot-s1/src/rr_server/mod.rs`
    // 顶部 "shutdown 顺序契约" 段）。
    coordinator.mark_shutting_down();

    rr_handles.request_stop_all();
    if let Err(panic_payload) = rr_handles.join_all() {
        // 任一 RR server 线程内部 panic：不阻塞 daemon shutdown，仅 log。
        // tick loop 已退出 + coordinator 已 mark_shutting_down，主路径
        // 仍按正常 Ok 返；非 0 exit 仅留给 tick loop publish 失败 (5) /
        // anchor 失败 (4) 等真硬错。
        tracing::error!(
            ?panic_payload,
            "s1_daemon: rr_server thread join returned panic; continuing shutdown"
        );
    }

    // v0.4 候选 3 AC-C1: step 4 stop reaper + join。reaper 在下一次 1s
    // 轮询 stop signal 时退出；最坏延迟 ≤ 1s。reaper 内部 reap 完成的
    // handle 已 owned 走 join，不在 inner Vec 内；剩余活 handle 由 step
    // 5 events_registry.stop_and_join_all 接管。
    reaper_stop.store(true, Ordering::Release);
    if let Err(panic_payload) = reaper_handle.join() {
        tracing::error!(
            ?panic_payload,
            "s1_daemon: events reaper thread join returned panic; continuing shutdown"
        );
    }
    tracing::info!(
        events_registry_len = events_registry.len(),
        op = "reaper_stopped",
        "s1_daemon events_drain reaper thread stopped + joined"
    );

    // RR thread 全 join 完 + reaper 已 stop+join 后 handler 闭包均已
    // drop；ctx 不再被任何 RR 线程持有（仅 main + events_registry 内
    // Arc clone）。registry 方法接 `&self`，可在 Arc 上直接调，不依赖
    // try_unwrap。
    //
    // v0.4 候选 3 Plan §P.2.1.d: shutdown_pre log（关停前 registry 当前
    // len，smoke binary 端 stderr scrape 用），跟 handler register 时的
    // 即时 log 对称。
    tracing::info!(
        events_registry_len = events_registry.len(),
        op = "shutdown_pre",
        "s1_daemon events_registry.stop_and_join_all starting"
    );
    if let Err(panic_payload) = events_registry.stop_and_join_all() {
        tracing::error!(
            ?panic_payload,
            "s1_daemon: events_drain thread join returned panic; continuing shutdown"
        );
    }
    // v0.4 候选 3 Plan §P.2.1.d: shutdown_post log。stop_and_join_all
    // 内部 mem::take 拿走整个 Vec 后逐个 join → 返回时 inner Vec 为空，
    // events_registry.len() 必为 0；本 log 是事后验证锚点（smoke binary
    // 用本行确认 shutdown 路径走完）。
    tracing::info!(
        events_registry_len = events_registry.len(),
        op = "shutdown_post",
        "s1_daemon events_registry shutdown complete"
    );

    // 6. (v0.5 候选 E 段(5) AC-E20) state cache 线程 take → stop → join：
    //    所有会调 `submit`（唯一读 cache 的路径）的 RR 线程已在 step 3 join，
    //    cache 不再被读；此处显式收线程，不依赖 Coordinator Drop 时机。
    //    `take` 后 `has_state_cache() == false`，coordinator 已
    //    mark_shutting_down 不会再有 submit。
    if let Some(cache) = coordinator.take_state_cache() {
        let stats = cache.stats();
        match cache.join() {
            Ok(()) => tracing::info!(
                frames_received = stats.frames_received,
                transport_errors = stats.transport_errors,
                op = "state_cache_stopped",
                "s1_daemon state cache thread stopped + joined"
            ),
            Err(panic_payload) => tracing::error!(
                ?panic_payload,
                "s1_daemon: state cache thread join returned panic; continuing shutdown"
            ),
        }
    } else {
        tracing::warn!(
            "s1_daemon: state cache handle already taken before shutdown step 6 (unexpected)"
        );
    }

    // ctx / events_registry / coordinator / handler_clock 的 Arc 在 main
    // 函数返回时按 RAII 顺序 drop；`stop_and_join_all` 已保证所有 drain
    // 线程被 join，state cache 已 step 6 join，drop 自身不再触发任何线程
    // 同步等待。
    drop(ctx);

    let final_state = match tick_result {
        Ok(s) => s,
        Err(TickError::Publish(msg)) => {
            eprintln!("fatal: tick loop publish failed: {msg}");
            return Err(5);
        }
    };

    tracing::info!(
        cmd_seq = final_state.cmd_seq,
        hb_seq = final_state.hb_seq,
        elapsed_ns = final_state.elapsed_ns(clock.now().0),
        final_state = ?coordinator.current_state(),
        "s1_daemon clean shutdown"
    );
    Ok(())
}

/// Spawn 4 个 S1 RR server 线程（task_invoke / task_cancel / task_status
/// / clear_error），返回聚合 handle。每个 topic 一个独立 iceoryx2 Node +
/// server，跟 A1 `crates/robot-s0/src/bin/s0_daemon.rs::run` 同款 server
/// factory 模式（service SingleThreaded → server `!Send`，必须在 owner
/// 线程内 create）。每个 topic 的 handler 闭包从
/// [`RealHandlerCtx`] 派生（`ctx.invoke_handler()` / `cancel_handler()` /
/// `status_handler()` / `clear_error_handler()`），各自捕获
/// `Arc<RealHandlerCtx>` clone。
#[allow(clippy::too_many_lines)] // 4 个 RR topic 各自 cfg + factory closure 一气呵成；拆 helper 反而让 reviewer 跨函数对账每个 topic 的连线。
fn spawn_s1_rr_server_threads(
    robot_id: &str,
    ctx: &Arc<RealHandlerCtx>,
) -> std::io::Result<S1RrServerHandles> {
    let topic_invoke = format!("/{robot_id}/control/task_invoke");
    let topic_cancel = format!("/{robot_id}/control/task_cancel");
    let topic_status = format!("/{robot_id}/control/task_status");
    let topic_clear = format!("/{robot_id}/control/clear_error");

    let task_invoke = {
        let topic = topic_invoke.clone();
        let cfg = RrServerThreadConfig {
            topic: topic.clone(),
            poll_interval_ns: 500_000,
            max_in_flight: 64,
        };
        spawn_task_invoke_server(
            cfg,
            move || {
                let node = NodeBuilder::new()
                    .create::<IxService>()
                    .expect("rr task_invoke node create");
                let server = rr_server::<markers::InvokeTaskRequest, markers::InvokeTaskResponse>(
                    &node, &topic,
                )
                .expect("rr task_invoke server create");
                // node 持 server 内部 shm fd；leak 让 server 整个生命周期内
                // node 不被 drop（同 A1 s0_daemon 同款）。
                Box::leak(Box::new(node));
                server
            },
            ctx.invoke_handler(),
        )?
    };

    let task_cancel = {
        let topic = topic_cancel.clone();
        let cfg = RrServerThreadConfig {
            topic: topic.clone(),
            poll_interval_ns: 500_000,
            max_in_flight: 64,
        };
        spawn_task_cancel_server(
            cfg,
            move || {
                let node = NodeBuilder::new()
                    .create::<IxService>()
                    .expect("rr task_cancel node create");
                let server = rr_server::<markers::CancelTaskRequest, markers::CancelTaskResponse>(
                    &node, &topic,
                )
                .expect("rr task_cancel server create");
                Box::leak(Box::new(node));
                server
            },
            ctx.cancel_handler(),
        )?
    };

    let task_status = {
        let topic = topic_status.clone();
        let cfg = RrServerThreadConfig {
            topic: topic.clone(),
            poll_interval_ns: 500_000,
            max_in_flight: 64,
        };
        spawn_task_status_server(
            cfg,
            move || {
                let node = NodeBuilder::new()
                    .create::<IxService>()
                    .expect("rr task_status node create");
                let server = rr_server::<
                    markers::GetTaskStatusRequest,
                    markers::GetTaskStatusResponse,
                >(&node, &topic)
                .expect("rr task_status server create");
                Box::leak(Box::new(node));
                server
            },
            ctx.status_handler(),
        )?
    };

    let clear_error = {
        let topic = topic_clear.clone();
        let cfg = RrServerThreadConfig {
            topic: topic.clone(),
            poll_interval_ns: 500_000,
            max_in_flight: 64,
        };
        spawn_clear_error_server(
            cfg,
            move || {
                let node = NodeBuilder::new()
                    .create::<IxService>()
                    .expect("rr clear_error node create");
                let server = rr_server::<markers::ClearErrorRequest, markers::ClearErrorResponse>(
                    &node, &topic,
                )
                .expect("rr clear_error server create");
                Box::leak(Box::new(node));
                server
            },
            ctx.clear_error_handler(),
        )?
    };

    Ok(S1RrServerHandles {
        task_invoke,
        task_cancel,
        task_status,
        clear_error,
    })
}

/// 启动锚定专属包装：把 [`AnchorError`] 三条 fail-fast 路径映射到 exit
/// code 4，并打印结构化日志。
fn anchor_or_fail<C: RobotClock>(
    node: &iceoryx2::node::Node<IxService>,
    topic_state: &str,
    hardware: &HardwareSection,
    cfg: &CoordinatorConfig,
    clock: &C,
    poll_interval: Duration,
) -> Result<Vec<f64>, i32> {
    let mut state_sub =
        match open_sub::<markers::JointState>(node, topic_state, TopicClass::S0State) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("fatal: open state subscriber for anchor: {e}");
                return Err(1);
            }
        };

    // 全部 AnchorError variant 都按 fail-fast 映射 exit code 4（B1 outline
    // §4.2 + §9 决策 #2 锁定，**不**退化到 bias=0）；这里**不**做 variant
    // 分流，统一用 `?` + `map_err` 把 Err 路径合并到一个分支。
    coordinator::read_anchor_from_s0_state(
        &mut state_sub,
        hardware.joints.len(),
        clock,
        Duration::from_millis(cfg.anchor_timeout_ms),
        Duration::from_millis(cfg.max_stale_ms),
        poll_interval,
    )
    .map_err(|e| {
        eprintln!("fatal: startup anchor failed: {e}");
        4
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_u64_maybe_hex_decimal() {
        assert_eq!(parse_u64_maybe_hex("100").unwrap(), 100);
        assert_eq!(parse_u64_maybe_hex("0").unwrap(), 0);
    }

    #[test]
    fn parse_u64_maybe_hex_hex_lowercase() {
        assert_eq!(
            parse_u64_maybe_hex("0xCAFEBABEDEADBEEF").unwrap(),
            0xCAFE_BABE_DEAD_BEEF
        );
    }

    #[test]
    fn parse_u64_maybe_hex_hex_uppercase_prefix() {
        assert_eq!(parse_u64_maybe_hex("0Xff").unwrap(), 0xff);
    }

    #[test]
    fn parse_u64_maybe_hex_invalid_returns_err() {
        assert!(parse_u64_maybe_hex("0xZZZ").is_err());
        assert!(parse_u64_maybe_hex("not-a-number").is_err());
    }

    #[test]
    fn parse_duration_ms() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
    }

    #[test]
    fn parse_duration_s() {
        assert_eq!(parse_duration("3s").unwrap(), Duration::from_secs(3));
    }

    #[test]
    fn parse_duration_bare_number_is_seconds() {
        assert_eq!(parse_duration("10").unwrap(), Duration::from_secs(10));
    }

    #[test]
    fn parse_duration_invalid() {
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("10x").is_err());
    }

    // ---- v0.5 段(2) 回修 P1-2：身份锁纯比较双路 ----

    #[test]
    fn robot_identity_mismatch_err() {
        let Err(msg) = check_robot_identity("cfg_id", "cli_id") else {
            panic!("expected identity mismatch");
        };
        assert!(msg.contains("config/flag mismatch"), "{msg}");
        assert!(
            msg.contains("'cfg_id'") && msg.contains("'cli_id'"),
            "{msg}"
        );
    }

    #[test]
    fn robot_identity_match_ok() {
        check_robot_identity("same", "same").unwrap();
    }

    // ---- v0.5 候选 E 段(5)：state cache spawn 失败 exit code 族 ----

    #[test]
    fn state_cache_spawn_exit_code_families() {
        use robot_transport::error::TransportError;
        let backend = || TransportError::Backend("x".into());
        assert_eq!(
            state_cache_spawn_exit_code(&StateCacheSpawnError::NodeCreate(backend())),
            1
        );
        assert_eq!(
            state_cache_spawn_exit_code(&StateCacheSpawnError::SubscriberOpen(backend())),
            1
        );
        assert_eq!(
            state_cache_spawn_exit_code(&StateCacheSpawnError::ReadyTimeout),
            1
        );
        assert_eq!(
            state_cache_spawn_exit_code(&StateCacheSpawnError::ThreadSpawn(std::io::Error::other(
                "no threads"
            ))),
            5
        );
    }
}
