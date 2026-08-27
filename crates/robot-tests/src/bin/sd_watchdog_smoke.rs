//! `sd_watchdog_smoke` —— v0.5 候选 E 段(8) S1 / gateway systemd watchdog
//! **双向** e2e smoke（Spec `team-swe/specs/v0_5_dimos_replace.md` P.2.7 +
//! AC-E19 + R5-5 / R6-5）。
//!
//! 验证 `docs/00_design.md` §6.7 不变量 #2 "systemd 是最外层看门狗" 在 S1
//! coordinator 与 robot_rpc_gateway 两个 daemon 上端到端成立，且**不误判
//! 合法空闲**：
//!
//! ```text
//! 每 case = (daemon, worker)：
//! 1. 起 user-mode systemd unit (Type=notify + WatchdogSec=2s + Restart=on-failure)
//!    S1 case：smoke 进程内先起 FakeStatePublisher（派生 layout_id，50 Hz）
//!    gateway case：双 hardware config + 每 case 分配 loopback 端口
//! 2. 等 unit active（= daemon 已发 READY=1）+ 抓 PID0
//! 3. 经 test-only 独立 topic（S1 `/<robot_id>/s1/test/freeze` / gateway
//!    `/<robot_id>/gateway/test/freeze`）publish
//!    TestFreezeCommandV2 { target_pid = PID0, worker, duration_ms = 5000 }
//! 4. 目标 worker 在 loop 顶端 freeze_check 真 park → 其 loop_tick_count
//!    （tonic 探针 = success_count）停滞 → 看门狗 withholding
//!    sd_notify(WATCHDOG=1) → systemd WatchdogSec=2s 处 SIGABRT
//! 5. Restart=on-failure 拉起新 daemon → PID1 ≠ PID0 且 is-active（第二次 READY）
//! 6. journal 硬断言：withholding ≥ 1 / 启动 banner ≥ 2 / "READY=1 sent" ≥ 2 /
//!    "freeze_control thread ready" ≥ 2 / "freeze_control: applied" ≥ 1
//! 7. 反向步（R5-5）：显式再发一条 target_pid = PID0（已死）的命令 → PID1 打
//!    "stale freeze ignored (pid mismatch)"，applied 计数不变
//! 8. 空闲保持 ≥ 3 × WatchdogSec：PID 仍 = PID1、仍 active、withholding /
//!    applied / banner 计数全部不变（零请求不重启 = AC-E19 的另一向）
//! ```
//!
//! ## 跟既有 `freeze_systemd_smoke`（S0，v0.3 A2-3）的关系
//!
//! 同款 8 条硬边界（不依赖真硬件 / user-service 兼容模板 / install 走
//! `$XDG_RUNTIME_DIR/systemd/user/` / 清理顺序 stop → reset-failed → rm →
//! daemon-reload / `--user` 域 preflight 环境错 exit 1 / 每 case 唯一化 /
//! wire struct 从 daemon crate import 不复制）。差异：
//!
//! - **wire = V2**（`target_pid` 精确匹配 + `(kind, index)` 槽位编码），S1 /
//!   gateway 各自 cfg-gated pub 导出（`robot_s1::freeze_control` /
//!   `robot_rpc::freeze_control`），两侧 POD 布局一致但类型独立。
//! - **每类 worker 各一 case**（Spec "冻结每类 worker → 重启"）：S1 默认
//!   `tick, heartbeat_pub, rr_server:0, state_cache`；gateway 默认
//!   `rr_io:0, estop_pub, state_pool:1, tonic_probe`（`state_pool:1` = R6-5
//!   多 hardware 单线程冻结 case：健康的 `StatePool(0)` 不得掩蔽卡死线程）。
//! - **反向步 + 空闲保持**为每 case 必测（AC-E19 双向）。
//!
//! ## 用法
//!
//! ```bash
//! # 1) 编两个 daemon（必须带 test-freeze-hook feature；release 与 gate 同款）
//! cargo build --release -p robot-s1  --bin s1_daemon         --features test-freeze-hook
//! cargo build --release -p robot-rpc --bin robot_rpc_gateway --features test-freeze-hook
//!
//! # 2) 编 + 跑 smoke（默认 8 case：S1 ×4 + gateway ×4）
//! cargo run -p robot-tests --bin sd_watchdog_smoke --features sd-watchdog-smoke -- \
//!     --s1-bin target/release/s1_daemon \
//!     --gateway-bin target/release/robot_rpc_gateway \
//!     --phase all
//! ```
//!
//! ## 退出码
//!
//! - `0`：所有 case PASS。
//! - `1`：preflight / 环境错（XDG_RUNTIME_DIR / user manager / bin / 模板
//!   缺）；**不**算测试失败。
//! - `2`：≥ 1 个 case FAIL。
//!
//! ## 不在本 binary 范围
//!
//! - 真 S0 / 真硬件：S1 的 state 源 = 进程内 FakeStatePublisher；gateway 不
//!   需要 S0 / S1（探针不依赖首帧，QA4）。
//! - 生产 unit（`xlerobot-s1-*.service` / gateway unit）：段(9) 部署件。
//! - Orin 真机回归：段(10) 受监督 demo 前置清单另行安排。

// 中文 doc 大量 identifier-like / config-like 字串（WatchdogSec=2s /
// XDG_RUNTIME_DIR / Restart=on-failure / state_pool:1 …）；test-only binary，
// doc 完整性优先于 doc-markdown 严格化（沿 freeze_systemd_smoke）。
#![allow(clippy::doc_markdown)]
#![forbid(unused_must_use)]

use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use iceoryx2::prelude::{NodeBuilder, ServiceName, ZeroCopySend};
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::build_helpers::build_joint_state;
use robot_msg::joint_layout::compute_layout_id;
use robot_msg::markers;
use robot_rpc::freeze_control as gw_freeze;
use robot_s1::freeze_control as s1_freeze;
use robot_transport::Publisher;
use robot_transport::iceoryx2::{TopicClass, publisher as open_pub};

// ============================================================================
// 常量
// ============================================================================

/// 两份 unit 模板的 `WatchdogSec`（毫秒）；空闲保持窗默认 = 3 × 本值。
const WATCHDOG_SEC_MS: u64 = 2_000;
/// S1 case 的 mock `[[hardware]]` joints（name, motor_id）——config 与
/// FakeStatePublisher 的 layout 派生同源。
const S1_JOINTS: [(&str, u32); 3] = [("j0", 1), ("j1", 2), ("j2", 3)];
/// S1 case 的 `[[hardware]].id`。
const S1_HARDWARE_ID: &str = "sdwd_arm";
/// gateway case 的双 hardware id（顺序 = config 序 = `StatePool(index)` 域）。
const GW_HARDWARE_IDS: [&str; 2] = ["sdwd_arm_a", "sdwd_arm_b"];
/// FakeStatePublisher 的 source_id / 周期（50 Hz，沿 robotctl_smoke）。
const FAKE_S0_SOURCE_ID: u32 = 1;
const FAKE_PUBLISH_INTERVAL: Duration = Duration::from_millis(20);
/// freeze 命令 publish 后等 daemon subscriber 消费的余量（poll 2 ms）。
const PUBLISH_SETTLE: Duration = Duration::from_millis(200);

/// 两 daemon 共用的 journal 断言子串。
const MARK_WITHHOLDING: &str = "withholding sd_notify";
const MARK_APPLIED: &str = "freeze_control: applied";
const MARK_STALE: &str = "stale freeze ignored (pid mismatch)";

// ============================================================================
// 目标 / phase
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    All,
    S1,
    Gateway,
}

impl Phase {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "all" => Ok(Self::All),
            "s1" => Ok(Self::S1),
            "gateway" => Ok(Self::Gateway),
            other => Err(format!("--phase: unknown {other:?} (all|s1|gateway)")),
        }
    }

    fn includes_s1(self) -> bool {
        matches!(self, Self::All | Self::S1)
    }

    fn includes_gateway(self) -> bool {
        matches!(self, Self::All | Self::Gateway)
    }
}

/// 一个 case 的冻结目标：daemon + 该 daemon 的 worker。
#[derive(Debug, Clone, Copy)]
enum Target {
    S1(s1_freeze::WorkerKind),
    Gateway(gw_freeze::WorkerKind),
}

impl Target {
    fn daemon_tag(self) -> &'static str {
        match self {
            Self::S1(_) => "s1",
            Self::Gateway(_) => "gw",
        }
    }

    fn worker_tag(self) -> String {
        match self {
            Self::S1(w) => s1_worker_tag(w),
            Self::Gateway(w) => gw_worker_tag(w),
        }
    }

    /// daemon 专属 journal 子串：启动 banner / "READY=1 sent" / freeze
    /// control 线程 ready。
    fn markers(self) -> JournalMarkers {
        match self {
            Self::S1(_) => JournalMarkers {
                banner: "s1_daemon starting",
                ready_sent: "s1_daemon systemd READY=1 sent",
                freeze_ready: "s1 freeze_control thread ready",
            },
            Self::Gateway(_) => JournalMarkers {
                banner: "robot_rpc_gateway: starting tonic gRPC server",
                ready_sent: "robot_rpc_gateway systemd READY=1 sent",
                freeze_ready: "gw freeze_control thread ready",
            },
        }
    }
}

fn s1_worker_tag(w: s1_freeze::WorkerKind) -> String {
    match w {
        s1_freeze::WorkerKind::Tick => "tick".into(),
        s1_freeze::WorkerKind::HeartbeatPub => "heartbeat_pub".into(),
        s1_freeze::WorkerKind::RrServer(i) => format!("rr_server_{i}"),
        s1_freeze::WorkerKind::StateCache => "state_cache".into(),
    }
}

fn gw_worker_tag(w: gw_freeze::WorkerKind) -> String {
    match w {
        gw_freeze::WorkerKind::RrIo(i) => format!("rr_io_{i}"),
        gw_freeze::WorkerKind::EstopPub => "estop_pub".into(),
        gw_freeze::WorkerKind::StatePool(i) => format!("state_pool_{i}"),
        gw_freeze::WorkerKind::TonicProbe => "tonic_probe".into(),
    }
}

/// `name` 或 `name:<index>` 拆分。
fn split_index(item: &str) -> Result<(&str, Option<u8>), String> {
    match item.split_once(':') {
        None => Ok((item, None)),
        Some((name, idx)) => {
            let idx: u8 = idx
                .parse()
                .map_err(|e| format!("worker item {item:?}: index parse: {e}"))?;
            Ok((name, Some(idx)))
        }
    }
}

fn parse_s1_workers(spec: &str) -> Result<Vec<s1_freeze::WorkerKind>, String> {
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|item| {
            let worker = match split_index(item)? {
                ("tick", None) => s1_freeze::WorkerKind::Tick,
                ("heartbeat_pub", None) => s1_freeze::WorkerKind::HeartbeatPub,
                ("rr_server", Some(i)) => s1_freeze::WorkerKind::RrServer(i),
                ("state_cache", None) => s1_freeze::WorkerKind::StateCache,
                _ => {
                    return Err(format!(
                        "--s1-workers: unknown item {item:?} \
                         (tick|heartbeat_pub|rr_server:<0-3>|state_cache)"
                    ));
                }
            };
            Ok(worker)
        })
        .collect()
}

fn parse_gateway_workers(spec: &str) -> Result<Vec<gw_freeze::WorkerKind>, String> {
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|item| {
            let worker = match split_index(item)? {
                ("rr_io", Some(i)) => gw_freeze::WorkerKind::RrIo(i),
                ("estop_pub", None) => gw_freeze::WorkerKind::EstopPub,
                ("state_pool", Some(i)) => gw_freeze::WorkerKind::StatePool(i),
                ("tonic_probe", None) => gw_freeze::WorkerKind::TonicProbe,
                _ => {
                    return Err(format!(
                        "--gateway-workers: unknown item {item:?} \
                         (rr_io:<0-5>|estop_pub|state_pool:<0-1>|tonic_probe)"
                    ));
                }
            };
            Ok(worker)
        })
        .collect()
}

// ============================================================================
// CLI
// ============================================================================

struct Args {
    s1_bin: PathBuf,
    gateway_bin: PathBuf,
    s1_unit_template: PathBuf,
    gateway_unit_template: PathBuf,
    /// gateway `--rpc-roles-toml`（默认仓库 `configs/rpc_roles.toml`）。
    roles_toml: PathBuf,
    workdir_base: PathBuf,
    robot_id_prefix: String,
    unit_name_prefix: String,
    phase: Phase,
    s1_workers: Vec<s1_freeze::WorkerKind>,
    gateway_workers: Vec<gw_freeze::WorkerKind>,
    /// freeze 时长（毫秒；必须 > WatchdogSec 让 systemd 真重启）。
    freeze_duration_ms: u64,
    /// 等 unit active（含 READY）的超时（毫秒）。
    daemon_active_wait_ms: u64,
    /// freeze 后等多久看 PID 切换（毫秒）。
    /// 默认 8s = WatchdogSec(2s) + RestartSec(2s) + 新 daemon READY(≤ 3s) + jitter。
    restart_wait_ms: u64,
    /// 反向步 publish 后等 PID1 消费 + 打 stale 日志的余量（毫秒）。
    stale_settle_ms: u64,
    /// 空闲保持窗（毫秒；默认 3 × WatchdogSec）。
    idle_hold_ms: u64,
}

impl Args {
    #[allow(clippy::too_many_lines)]
    fn parse() -> Result<Self, String> {
        let mut args = Self {
            s1_bin: PathBuf::new(),
            gateway_bin: PathBuf::new(),
            s1_unit_template: PathBuf::from("scripts/systemd/xlerobot-s1-freeze-test.service"),
            gateway_unit_template: PathBuf::from(
                "scripts/systemd/xlerobot-gateway-freeze-test.service",
            ),
            roles_toml: PathBuf::from("configs/rpc_roles.toml"),
            workdir_base: PathBuf::from("logs/sd_watchdog_smoke"),
            robot_id_prefix: "sdwd".into(),
            unit_name_prefix: "sdwd".into(),
            phase: Phase::All,
            s1_workers: parse_s1_workers("tick,heartbeat_pub,rr_server:0,state_cache")?,
            gateway_workers: parse_gateway_workers("rr_io:0,estop_pub,state_pool:1,tonic_probe")?,
            freeze_duration_ms: 5_000,
            daemon_active_wait_ms: 20_000,
            restart_wait_ms: 8_000,
            stale_settle_ms: 1_000,
            idle_hold_ms: 3 * WATCHDOG_SEC_MS,
        };
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            let a = raw[i].as_str();
            let value = |i: usize| -> Result<&String, String> {
                raw.get(i + 1).ok_or_else(|| format!("{a} requires value"))
            };
            match a {
                "--s1-bin" => args.s1_bin = PathBuf::from(value(i)?),
                "--gateway-bin" => args.gateway_bin = PathBuf::from(value(i)?),
                "--s1-unit-template" => args.s1_unit_template = PathBuf::from(value(i)?),
                "--gateway-unit-template" => {
                    args.gateway_unit_template = PathBuf::from(value(i)?);
                }
                "--roles-toml" => args.roles_toml = PathBuf::from(value(i)?),
                "--workdir-base" => args.workdir_base = PathBuf::from(value(i)?),
                "--robot-id-prefix" => args.robot_id_prefix.clone_from(value(i)?),
                "--unit-name-prefix" => args.unit_name_prefix.clone_from(value(i)?),
                "--phase" => args.phase = Phase::parse(value(i)?)?,
                "--s1-workers" => args.s1_workers = parse_s1_workers(value(i)?)?,
                "--gateway-workers" => args.gateway_workers = parse_gateway_workers(value(i)?)?,
                "--freeze-duration-ms" => args.freeze_duration_ms = parse_u64(a, value(i)?)?,
                "--daemon-active-wait-ms" => {
                    args.daemon_active_wait_ms = parse_u64(a, value(i)?)?;
                }
                "--restart-wait-ms" => args.restart_wait_ms = parse_u64(a, value(i)?)?,
                "--stale-settle-ms" => args.stale_settle_ms = parse_u64(a, value(i)?)?,
                "--idle-hold-ms" => args.idle_hold_ms = parse_u64(a, value(i)?)?,
                "-h" | "--help" => return Err("HELP".into()),
                other => return Err(format!("unknown arg: {other}")),
            }
            // 除 -h/--help 外全部带值：跳过 flag + value。
            i += 2;
        }
        if args.phase.includes_s1() && args.s1_bin.as_os_str().is_empty() {
            return Err("--s1-bin <path> is required for --phase all|s1".into());
        }
        if args.phase.includes_gateway() && args.gateway_bin.as_os_str().is_empty() {
            return Err("--gateway-bin <path> is required for --phase all|gateway".into());
        }
        if args.freeze_duration_ms <= WATCHDOG_SEC_MS {
            return Err(format!(
                "--freeze-duration-ms {} must exceed WatchdogSec {WATCHDOG_SEC_MS} ms",
                args.freeze_duration_ms
            ));
        }
        Ok(args)
    }
}

fn parse_u64(flag: &str, v: &str) -> Result<u64, String> {
    v.parse().map_err(|e| format!("{flag}: {e}"))
}

fn print_help() {
    eprintln!(
        "sd_watchdog_smoke (v0.5 候选 E 段(8) S1 / gateway systemd watchdog 双向 e2e smoke, AC-E19)

USAGE:
  sd_watchdog_smoke --s1-bin <PATH> --gateway-bin <PATH> [OPTIONS]

REQUIRED（按 --phase）:
  --s1-bin <PATH>              s1_daemon binary（必须带 --features test-freeze-hook 编译）
  --gateway-bin <PATH>         robot_rpc_gateway binary（同上）

OPTIONS:
  --phase <all|s1|gateway>     默认 all
  --s1-workers <LIST>          逗号分隔：tick|heartbeat_pub|rr_server:<0-3>|state_cache
                               （默认 tick,heartbeat_pub,rr_server:0,state_cache）
  --gateway-workers <LIST>     逗号分隔：rr_io:<0-5>|estop_pub|state_pool:<0-1>|tonic_probe
                               （默认 rr_io:0,estop_pub,state_pool:1,tonic_probe）
  --s1-unit-template <PATH>    默认 scripts/systemd/xlerobot-s1-freeze-test.service
  --gateway-unit-template <P>  默认 scripts/systemd/xlerobot-gateway-freeze-test.service
  --roles-toml <PATH>          gateway rpc roles（默认 configs/rpc_roles.toml）
  --workdir-base <DIR>         每 case workdir 根（默认 logs/sd_watchdog_smoke/）
  --robot-id-prefix <STR>      默认 sdwd（每 case 加 _<pid>_<seq>）
  --unit-name-prefix <STR>     默认 sdwd（每 case 加 -<s1|gw>-<pid>-<seq>.service）
  --freeze-duration-ms <MS>    默认 5000（必须 > WatchdogSec=2000）
  --daemon-active-wait-ms <MS> 等 active（READY）超时，默认 20000
  --restart-wait-ms <MS>       freeze 后等 PID 切换，默认 8000
  --stale-settle-ms <MS>       反向步后等 stale 日志，默认 1000
  --idle-hold-ms <MS>          空闲保持窗，默认 6000（3 × WatchdogSec）
  -h, --help                   显示帮助

退出码:
  0 = 全过；1 = preflight / env 错（XDG_RUNTIME_DIR / user manager / bin / 模板缺）；2 = ≥1 case FAIL
"
    );
}

// ============================================================================
// main
// ============================================================================

#[derive(Debug)]
struct CaseResult {
    seq: u32,
    label: String,
    robot_id: String,
    unit_name: String,
    fail_reason: Option<String>,
}

fn main() {
    let _ = robot_logging::init();
    let args = match Args::parse() {
        Ok(a) => a,
        Err(msg) if msg == "HELP" => {
            print_help();
            std::process::exit(0);
        }
        Err(msg) => {
            eprintln!("error: {msg}");
            print_help();
            std::process::exit(1);
        }
    };

    let env = match preflight(&args) {
        Ok(env) => env,
        Err(msg) => {
            eprintln!("preflight FAIL (environment error): {msg}");
            std::process::exit(1);
        }
    };

    let mut targets: Vec<Target> = Vec::new();
    if args.phase.includes_s1() {
        targets.extend(args.s1_workers.iter().map(|w| Target::S1(*w)));
    }
    if args.phase.includes_gateway() {
        targets.extend(args.gateway_workers.iter().map(|w| Target::Gateway(*w)));
    }

    let mut results: Vec<CaseResult> = Vec::with_capacity(targets.len());
    for (seq, target) in (0u32..).zip(targets.iter().copied()) {
        let ctx = CaseCtx::new(&args, seq, target);
        eprintln!(
            "\n========== case {}/{} {} robot_id={} unit={} ==========",
            seq + 1,
            targets.len(),
            ctx.label,
            ctx.robot_id,
            ctx.unit_name,
        );
        let outcome = run_case(&args, &env, &ctx);
        // 即使 PASS 也要清理 unit / workdir，避免下 case 串污染。
        cleanup_case(&env, &ctx.unit_name);
        if let Err(reason) = &outcome {
            eprintln!("  ✗ FAIL: {reason}");
        }
        results.push(CaseResult {
            seq,
            label: ctx.label.clone(),
            robot_id: ctx.robot_id.clone(),
            unit_name: ctx.unit_name.clone(),
            fail_reason: outcome.err(),
        });
    }

    // 最后再 daemon-reload 一次，让 user manager 状态完全干净。
    let _ = systemctl_user(&["daemon-reload"]);

    eprintln!("\n========== summary ==========");
    let mut pass_count = 0;
    let mut fail_count = 0;
    for r in &results {
        let tag = if r.fail_reason.is_none() {
            "PASS"
        } else {
            "FAIL"
        };
        eprintln!(
            "  case {} {} robot_id={} unit={} : {} {}",
            r.seq,
            r.label,
            r.robot_id,
            r.unit_name,
            tag,
            r.fail_reason.as_deref().unwrap_or(""),
        );
        if r.fail_reason.is_none() {
            pass_count += 1;
        } else {
            fail_count += 1;
        }
    }
    eprintln!("  total: {pass_count} PASS / {fail_count} FAIL");

    if fail_count > 0 {
        std::process::exit(2);
    }
}

// ============================================================================
// preflight + env
// ============================================================================

struct EnvCtx {
    /// `$XDG_RUNTIME_DIR/systemd/user/`，渲染完的 unit 写到这里。
    user_unit_dir: PathBuf,
    s1_template: Option<String>,
    gateway_template: Option<String>,
    /// gateway `--rpc-roles-toml` 绝对路径。
    roles_abs: Option<PathBuf>,
}

fn preflight(args: &Args) -> Result<EnvCtx, String> {
    let mut s1_template = None;
    let mut gateway_template = None;
    let mut roles_abs = None;
    if args.phase.includes_s1() {
        if !args.s1_bin.exists() {
            return Err(format!(
                "--s1-bin not found: {}（先 `cargo build --release -p robot-s1 --bin s1_daemon \
                 --features test-freeze-hook`）",
                args.s1_bin.display()
            ));
        }
        s1_template = Some(read_template(&args.s1_unit_template)?);
    }
    if args.phase.includes_gateway() {
        if !args.gateway_bin.exists() {
            return Err(format!(
                "--gateway-bin not found: {}（先 `cargo build --release -p robot-rpc --bin \
                 robot_rpc_gateway --features test-freeze-hook`）",
                args.gateway_bin.display()
            ));
        }
        gateway_template = Some(read_template(&args.gateway_unit_template)?);
        roles_abs = Some(
            std::fs::canonicalize(&args.roles_toml)
                .map_err(|e| format!("--roles-toml {}: {e}", args.roles_toml.display()))?,
        );
    }
    // XDG_RUNTIME_DIR + user manager 可用性（白名单沿 freeze_systemd_smoke
    // reviewer A2-3 round 2 阻断 1：running / degraded / starting /
    // initializing 视为可用；bus 不可达 / stopped / offline / maintenance =
    // 环境错）。
    let xdg = std::env::var("XDG_RUNTIME_DIR").map_err(|_| {
        "XDG_RUNTIME_DIR not set; user manager 不可用 / 未 login 进 systemd \
         user session（CI 容器场景：先 `loginctl enable-linger $USER` 或在
         systemd-run --user --scope 内跑）"
            .to_string()
    })?;
    let user_unit_dir = PathBuf::from(&xdg).join("systemd/user");
    std::fs::create_dir_all(&user_unit_dir)
        .map_err(|e| format!("create {}: {e}", user_unit_dir.display()))?;
    let probe = Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("systemctl --user is-system-running spawn failed: {e}"))?;
    let stdout_first = String::from_utf8_lossy(&probe.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let stderr_text = String::from_utf8_lossy(&probe.stderr).trim().to_string();
    if stderr_text.contains("Failed to connect to bus")
        || stderr_text.contains("No such file or directory")
        || stderr_text.contains("Failed to get bus connection")
    {
        return Err(format!(
            "systemctl --user is-system-running: user bus unreachable; \
             stderr={stderr_text:?} stdout={stdout_first:?}"
        ));
    }
    let allowed = matches!(
        stdout_first.as_str(),
        "running" | "degraded" | "starting" | "initializing"
    );
    if !allowed {
        return Err(format!(
            "systemctl --user is-system-running: state={stdout_first:?} \
             不在白名单 [running|degraded|starting|initializing]; stderr={stderr_text:?}"
        ));
    }
    std::fs::create_dir_all(&args.workdir_base)
        .map_err(|e| format!("create workdir_base {}: {e}", args.workdir_base.display()))?;
    Ok(EnvCtx {
        user_unit_dir,
        s1_template,
        gateway_template,
        roles_abs,
    })
}

fn read_template(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("read unit template {}: {e}", path.display()))
}

// ============================================================================
// 单 case
// ============================================================================

struct CaseCtx {
    target: Target,
    /// `<daemon>/<worker>` 人类可读标签。
    label: String,
    robot_id: String,
    unit_name: String,
    workdir: PathBuf,
}

impl CaseCtx {
    fn new(args: &Args, seq: u32, target: Target) -> Self {
        let pid = std::process::id();
        let daemon = target.daemon_tag();
        let worker = target.worker_tag();
        Self {
            target,
            label: format!("{daemon}/{worker}"),
            robot_id: format!("{}_{}_{}", args.robot_id_prefix, pid, seq),
            unit_name: format!("{}-{daemon}-{pid}-{seq}.service", args.unit_name_prefix),
            workdir: args.workdir_base.join(format!("{seq}_{daemon}_{worker}")),
        }
    }
}

/// daemon 专属 journal 子串。
#[derive(Debug, Clone, Copy)]
struct JournalMarkers {
    banner: &'static str,
    ready_sent: &'static str,
    freeze_ready: &'static str,
}

/// 一次 journal 快照的计数。
#[derive(Debug)]
struct JournalCounts {
    withholding: usize,
    banner: usize,
    ready_sent: usize,
    freeze_ready: usize,
    applied: usize,
    stale: usize,
    text: String,
}

impl JournalCounts {
    fn query(unit_name: &str, m: JournalMarkers) -> Result<Self, String> {
        let text = journalctl_user(unit_name)?;
        Ok(Self {
            withholding: count_substring(&text, MARK_WITHHOLDING),
            banner: count_substring(&text, m.banner),
            ready_sent: count_substring(&text, m.ready_sent),
            freeze_ready: count_substring(&text, m.freeze_ready),
            applied: count_substring(&text, MARK_APPLIED),
            stale: count_substring(&text, MARK_STALE),
            text,
        })
    }

    fn summary(&self) -> String {
        format!(
            "withholding={} banner={} ready_sent={} freeze_ready={} applied={} stale={}",
            self.withholding,
            self.banner,
            self.ready_sent,
            self.freeze_ready,
            self.applied,
            self.stale
        )
    }

    /// watchdog 重启后的硬断言（全 journal 视角，不切窗口）。
    fn assert_after_restart(&self) -> Result<(), String> {
        let mut hard: Vec<String> = Vec::new();
        if self.withholding == 0 {
            hard.push("missing 'withholding sd_notify' warn (liveness gate 失效); count=0".into());
        }
        if self.banner < 2 {
            hard.push(format!(
                "startup banner count={} (期望 ≥ 2：first + restart)",
                self.banner
            ));
        }
        if self.ready_sent < 2 {
            hard.push(format!(
                "'READY=1 sent' count={} (期望 ≥ 2：PID0 + PID1 各一次 READY)",
                self.ready_sent
            ));
        }
        if self.freeze_ready < 2 {
            hard.push(format!(
                "'freeze_control thread ready' count={} (期望 ≥ 2：first + restart；\
                 =0 多半是 daemon 未带 --features test-freeze-hook)",
                self.freeze_ready
            ));
        }
        if self.applied == 0 {
            hard.push("'freeze_control: applied' count=0 (PID0 未消费 freeze 命令)".into());
        }
        if hard.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "journal hard-asserts FAIL ({} 条):\n  - {}\njournal tail:\n{}",
                hard.len(),
                hard.join("\n  - "),
                tail_lines(&self.text, 40),
            ))
        }
    }
}

/// 一个 case：起 unit → 冻结 → 重启断言 → 反向步 → 空闲保持。失败以
/// `Err(reason)` 上抛（cleanup 由调用方统一做）。
#[allow(clippy::too_many_lines)]
fn run_case(args: &Args, env: &EnvCtx, ctx: &CaseCtx) -> Result<(), String> {
    // ---- 1) 干净 workdir ----
    let _ = std::fs::remove_dir_all(&ctx.workdir);
    std::fs::create_dir_all(&ctx.workdir)
        .map_err(|e| format!("create workdir {}: {e}", ctx.workdir.display()))?;
    let workdir_abs = std::fs::canonicalize(&ctx.workdir)
        .map_err(|e| format!("canonicalize workdir {}: {e}", ctx.workdir.display()))?;

    // ---- 2) config + 渲染（S1 case 同时起 FakeStatePublisher）----
    let config_path = workdir_abs.join("config.toml");
    let (rendered, _fake_s0) = match ctx.target {
        Target::S1(_) => {
            std::fs::write(&config_path, s1_config(&ctx.robot_id))
                .map_err(|e| format!("write config {}: {e}", config_path.display()))?;
            let layout_id = compute_layout_id(&ctx.robot_id, S1_HARDWARE_ID, &S1_JOINTS);
            let fake = FakeStatePublisher::spawn(
                &ctx.robot_id,
                S1_HARDWARE_ID,
                S1_JOINTS.len(),
                layout_id,
            )?;
            eprintln!("  ✓ fake s0 state publisher up (layout_id={layout_id:#018x})");
            let daemon_bin = canonicalize_bin(&args.s1_bin)?;
            let rendered = env
                .s1_template
                .as_deref()
                .ok_or("s1 unit template not loaded")?
                .replace("@ROBOT_ID@", &ctx.robot_id)
                .replace("@WORKDIR@", &workdir_abs.display().to_string())
                .replace("@DAEMON_BIN@", &daemon_bin.display().to_string())
                .replace("@CONFIG_PATH@", &config_path.display().to_string())
                .replace("@HARDWARE_ID@", S1_HARDWARE_ID);
            (rendered, Some(fake))
        }
        Target::Gateway(_) => {
            std::fs::write(&config_path, gateway_config(&ctx.robot_id))
                .map_err(|e| format!("write config {}: {e}", config_path.display()))?;
            let port = allocate_loopback_port()?;
            let listen_addr = format!("127.0.0.1:{port}");
            let daemon_bin = canonicalize_bin(&args.gateway_bin)?;
            let roles = env.roles_abs.as_ref().ok_or("roles toml not resolved")?;
            let rendered = env
                .gateway_template
                .as_deref()
                .ok_or("gateway unit template not loaded")?
                .replace("@ROBOT_ID@", &ctx.robot_id)
                .replace("@WORKDIR@", &workdir_abs.display().to_string())
                .replace("@DAEMON_BIN@", &daemon_bin.display().to_string())
                .replace("@CONFIG_PATH@", &config_path.display().to_string())
                .replace("@ROLES_PATH@", &roles.display().to_string())
                .replace("@LISTEN_ADDR@", &listen_addr);
            eprintln!("  ✓ gateway listen_addr={listen_addr}");
            (rendered, None)
        }
    };
    let unit_path = env.user_unit_dir.join(&ctx.unit_name);
    std::fs::write(&unit_path, &rendered)
        .map_err(|e| format!("write unit {}: {e}", unit_path.display()))?;

    // ---- 3) daemon-reload + start + 等 READY（is-active）+ PID0 ----
    systemctl_user(&["daemon-reload"]).map_err(|e| format!("daemon-reload: {e}"))?;
    systemctl_user(&["start", &ctx.unit_name]).map_err(|e| format!("start: {e}"))?;
    let pid0 = wait_for_active_pid_with(&ctx.unit_name, args.daemon_active_wait_ms, |_| true)
        .map_err(|e| format!("waiting for daemon active (READY): {e}"))?;
    eprintln!("  ✓ daemon active (READY=1 received by systemd), PID0={pid0}");

    // ---- 4) 注入 freeze（target_pid = PID0）----
    publish_freeze(&ctx.robot_id, ctx.target, pid0, args.freeze_duration_ms)?;
    eprintln!(
        "  ✓ freeze published: {} target_pid={pid0} duration_ms={}",
        ctx.label, args.freeze_duration_ms
    );

    // ---- 5) 等 watchdog 超时 + Restart=on-failure；PID1 ≠ PID0 且 is-active ----
    sleep_ms(args.restart_wait_ms);
    let pid1 = wait_for_active_pid_with(&ctx.unit_name, args.daemon_active_wait_ms, |pid| {
        pid != pid0
    })
    .map_err(|e| format!("wait for restarted daemon active: {e}"))?;
    eprintln!(
        "  ✓ PID switched: PID0={pid0} → PID1={pid1} (systemd watchdog restart, is-active confirmed)"
    );

    // ---- 6) journal 硬断言 ----
    let markers = ctx.target.markers();
    let j1 = JournalCounts::query(&ctx.unit_name, markers)?;
    j1.assert_after_restart()?;
    eprintln!("  ✓ journal after restart: {}", j1.summary());

    // ---- 7) 反向步（R5-5）：再发 target_pid = PID0（已死）→ PID1 stale-ignored ----
    publish_freeze(&ctx.robot_id, ctx.target, pid0, args.freeze_duration_ms)?;
    sleep_ms(args.stale_settle_ms);
    let j2 = JournalCounts::query(&ctx.unit_name, markers)?;
    if j2.stale <= j1.stale {
        return Err(format!(
            "reverse step: PID1 did not log '{MARK_STALE}' (before={} after={})\njournal tail:\n{}",
            j1.stale,
            j2.stale,
            tail_lines(&j2.text, 40),
        ));
    }
    if j2.applied != j1.applied {
        return Err(format!(
            "reverse step: stale command was APPLIED by PID1 (applied {} → {})",
            j1.applied, j2.applied
        ));
    }
    eprintln!(
        "  ✓ reverse step: stale freeze ignored by PID1 (stale {} → {}, applied stays {})",
        j1.stale, j2.stale, j2.applied
    );

    // ---- 8) 空闲保持 ≥ 3 × WatchdogSec：不重启、无二次 withholding ----
    sleep_ms(args.idle_hold_ms);
    if !is_active(&ctx.unit_name) {
        return Err(format!(
            "idle hold: unit not active after {} ms idle",
            args.idle_hold_ms
        ));
    }
    let pid_now = read_active_pid(&ctx.unit_name)?;
    if pid_now != pid1 {
        return Err(format!(
            "idle hold: MainPID changed {pid1} → {pid_now} during {} ms idle (unexpected restart)",
            args.idle_hold_ms
        ));
    }
    let j3 = JournalCounts::query(&ctx.unit_name, markers)?;
    let mut idle_fail: Vec<String> = Vec::new();
    if j3.withholding != j1.withholding {
        idle_fail.push(format!(
            "second withholding after restart (withholding {} → {})",
            j1.withholding, j3.withholding
        ));
    }
    if j3.applied != j1.applied {
        idle_fail.push(format!("applied {} → {}", j1.applied, j3.applied));
    }
    if j3.banner != j1.banner {
        idle_fail.push(format!(
            "startup banner {} → {} (unexpected restart)",
            j1.banner, j3.banner
        ));
    }
    if !idle_fail.is_empty() {
        return Err(format!(
            "idle hold FAIL: {}\njournal tail:\n{}",
            idle_fail.join("; "),
            tail_lines(&j3.text, 40)
        ));
    }
    eprintln!(
        "  ✓ idle hold {} ms: PID1={pid1} stable, no restart / no second withholding ({})",
        args.idle_hold_ms,
        j3.summary()
    );
    Ok(())
}

fn canonicalize_bin(path: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(path).map_err(|e| format!("canonicalize bin {}: {e}", path.display()))
}

// ============================================================================
// systemctl --user / journalctl 辅助（沿 freeze_systemd_smoke）
// ============================================================================

fn systemctl_user(args: &[&str]) -> Result<(), String> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn systemctl --user {args:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "systemctl --user {args:?} exit={}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    Ok(())
}

fn read_active_pid(unit_name: &str) -> Result<u32, String> {
    let out = Command::new("systemctl")
        .args(["--user", "show", "-p", "MainPID", "--value", unit_name])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn systemctl show: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "systemctl --user show {unit_name} exit={}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim()
        .parse()
        .map_err(|e| format!("MainPID parse {:?}: {e}", s.trim()))
}

fn is_active(unit_name: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", unit_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// 轮询 `MainPID != 0 && accept(pid) && is-active`（Type=notify 下 is-active
/// = READY=1 已收）。
fn wait_for_active_pid_with<F: Fn(u32) -> bool>(
    unit_name: &str,
    timeout_ms: u64,
    accept: F,
) -> Result<u32, String> {
    let start = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    while start.elapsed() < timeout {
        if let Ok(pid) = read_active_pid(unit_name)
            && pid != 0
            && accept(pid)
            && is_active(unit_name)
        {
            return Ok(pid);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "unit {unit_name} did not reach active within {timeout_ms}ms"
    ))
}

fn journalctl_user(unit_name: &str) -> Result<String, String> {
    // `_SYSTEMD_USER_UNIT=<unit>` 系统 journal matcher，跨发行版稳（Orin
    // Tegra 默认无 user journal namespace；详 freeze_systemd_smoke 同名 fn）。
    let matcher = format!("_SYSTEMD_USER_UNIT={unit_name}");
    let out = Command::new("journalctl")
        .args([&matcher, "--no-pager", "--output", "short-iso"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn journalctl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "journalctl {matcher} exit={}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn cleanup_case(env: &EnvCtx, unit_name: &str) {
    let _ = systemctl_user(&["stop", unit_name]);
    let _ = systemctl_user(&["reset-failed", unit_name]);
    let _ = std::fs::remove_file(env.user_unit_dir.join(unit_name));
    // daemon-reload 留到 main 里所有 case 跑完后做。
}

// ============================================================================
// freeze publish（V2 wire，两 daemon 类型独立、布局一致）
// ============================================================================

fn publish_freeze(
    robot_id: &str,
    target: Target,
    target_pid: u32,
    duration_ms: u64,
) -> Result<(), String> {
    match target {
        Target::S1(w) => publish_pod(
            &s1_freeze::freeze_topic(robot_id),
            s1_freeze::TestFreezeCommandV2::new(target_pid, w, duration_ms),
        ),
        Target::Gateway(w) => publish_pod(
            &gw_freeze::freeze_topic(robot_id),
            gw_freeze::TestFreezeCommandV2::new(target_pid, w, duration_ms),
        ),
    }
}

/// QoS 字面与 daemon 端 `open_or_create` 一致（1 pub / 1 sub / history 1 /
/// buffer 2 / safe_overflow）。publisher / service / node 在函数返回时一起
/// drop；先睡 [`PUBLISH_SETTLE`] 让 daemon subscriber（2 ms poll）消费。
fn publish_pod<T: Debug + Copy + ZeroCopySend + 'static>(
    service_path: &str,
    payload: T,
) -> Result<(), String> {
    let node = NodeBuilder::new()
        .create::<IxService>()
        .map_err(|e| format!("NodeBuilder: {e:?}"))?;
    let service_name =
        ServiceName::new(service_path).map_err(|e| format!("ServiceName({service_path}): {e}"))?;
    let service = node
        .service_builder(&service_name)
        .publish_subscribe::<T>()
        .max_publishers(1)
        .max_subscribers(1)
        .history_size(1)
        .subscriber_max_buffer_size(2)
        .enable_safe_overflow(true)
        .open_or_create()
        .map_err(|e| format!("open_or_create {service_path}: {e:?}"))?;
    let publisher = service
        .publisher_builder()
        .create()
        .map_err(|e| format!("publisher create: {e:?}"))?;
    let sample = publisher
        .loan_uninit()
        .map_err(|e| format!("loan_uninit: {e:?}"))?;
    let sample = sample.write_payload(payload);
    sample.send().map_err(|e| format!("send: {e:?}"))?;
    thread::sleep(PUBLISH_SETTLE);
    Ok(())
}

// ============================================================================
// FakeStatePublisher（S1 case 的 fake S0：派生 layout_id，50 Hz，贯穿 case）
// ============================================================================

struct FakeStatePublisher {
    handle: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl FakeStatePublisher {
    fn spawn(
        robot_id: &str,
        hardware_id: &str,
        joints: usize,
        layout_id: u64,
    ) -> Result<Self, String> {
        let topic = format!("/{robot_id}/s0/{hardware_id}/state");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let positions = vec![0.0_f64; joints];
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(0);

        let handle = thread::Builder::new()
            .name(format!("sdwd_fake_s0_{robot_id}"))
            .spawn(move || {
                // node / publisher 都住本线程栈，线程退出时正常 Drop（iceoryx2
                // 0.9.3；不 leak / 不 mem::forget）。
                let node = match NodeBuilder::new().create::<IxService>() {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("fake_s0 NodeBuilder: {e:?}")));
                        return;
                    }
                };
                let mut publisher =
                    match open_pub::<markers::JointState>(&node, &topic, TopicClass::S0State) {
                        Ok(p) => p,
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("fake_s0 open_pub {topic}: {e:?}")));
                            return;
                        }
                    };
                let _ = ready_tx.send(Ok(()));
                drop(ready_tx);

                let clock = SystemClock::new();
                let mut seq: u64 = 0;
                while !stop_clone.load(Ordering::Acquire) {
                    let stamp_ns = clock.now().0;
                    let body = build_joint_state(layout_id, stamp_ns, &positions, &[], &[], "");
                    if let Ok(mut writer) = publisher.loan(body.len() + 1024) {
                        writer.set_seq(seq);
                        writer.set_source_id(FAKE_S0_SOURCE_ID);
                        writer.set_stamp_ns(stamp_ns);
                        writer.set_body_bytes(&body);
                        let _ = writer.publish();
                    }
                    seq = seq.wrapping_add(1);
                    thread::sleep(FAKE_PUBLISH_INTERVAL);
                }
            })
            .map_err(|e| format!("spawn fake_s0 thread: {e}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                handle: Some(handle),
                stop,
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                Err("fake_s0 thread exited before ready".into())
            }
        }
    }
}

impl Drop for FakeStatePublisher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ============================================================================
// config 生成
// ============================================================================

/// S1 case 最小 config：mock_hard_rt + 3 joint + `[s1.profile]`
/// mock_dwell/hold_pose；`[robot].id == robot_id`（段(2) 身份锁）。
fn s1_config(robot_id: &str) -> String {
    let joints = S1_JOINTS
        .iter()
        .map(|(name, _)| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let motor_ids = S1_JOINTS
        .iter()
        .map(|(_, id)| id.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "# v0.5 候选 E 段(8) sd_watchdog_smoke 自动生成的最小 S1 config。\n\
         [robot]\n\
         id             = \"{robot_id}\"\n\
         config_version = \"0.3.0\"\n\
         \n\
         [[hardware]]\n\
         id        = \"{S1_HARDWARE_ID}\"\n\
         adapter   = \"mock_hard_rt\"\n\
         joints    = [{joints}]\n\
         motor_ids = [{motor_ids}]\n\
         \n\
         [s1.profile]\n\
         backends     = [\"mock_dwell\", \"hold_pose\"]\n\
         command_mode = \"joint_position\"\n"
    )
}

/// gateway case 双 hardware config（R6-5 `StatePool(1)` 单线程冻结 case）。
fn gateway_config(robot_id: &str) -> String {
    use std::fmt::Write as _;
    let mut s = format!(
        "# v0.5 候选 E 段(8) sd_watchdog_smoke 自动生成的双 hardware gateway config。\n\
         [robot]\n\
         id             = \"{robot_id}\"\n\
         config_version = \"0.3.0\"\n"
    );
    for hw in GW_HARDWARE_IDS {
        // write! 进 String 不可失败（fmt::Write for String 恒 Ok）。
        let _ = write!(
            s,
            "\n[[hardware]]\n\
             id        = \"{hw}\"\n\
             adapter   = \"mock_hard_rt\"\n\
             joints    = [\"j0\", \"j1\"]\n\
             motor_ids = [1, 2]\n"
        );
    }
    s
}

fn allocate_loopback_port() -> Result<u16, String> {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind 127.0.0.1:0: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?
        .port();
    drop(listener);
    Ok(port)
}

// ============================================================================
// 通用辅助
// ============================================================================

fn sleep_ms(ms: u64) {
    thread::sleep(Duration::from_millis(ms));
}

fn count_substring(s: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    s.matches(needle).count()
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}
