//! `robot-deploy` —— 真机部署件 CLI（v0.5 候选 E 段(9)，Spec P.2.9 +
//! D13）。
//!
//! **仅 `--features xlerobot-real`**（`required-features`，Cargo.toml 显式
//! `[[bin]]`）：真串口读写要 serialport / libudev，dev / CI 默认 build 不该
//! 拉这条系统依赖。
//!
//! ## 子命令
//!
//! ```text
//! robot-deploy guard --arm <left|right> --config <arm/full config>
//!                       --s0-bin <绝对路径> [--runtime-dir <dir>]
//!                       [--robot-id <id>] -- <s0 args，不含 --config>
//! robot-deploy preflight-arm --arm <left|right> --config <arm/full config>
//! robot-deploy preflight-handoff --left-config <path>
//!                                   --right-config <path>
//! robot-deploy provision [--config <arm/full config>] [--robot-id <id>]
//!                           [--latch-path <path>]
//! ```
//!
//! ## unit 接线（QA1 / Y1 终拍）
//!
//! arm S0 unit **无 `ExecStartPre`**——`guard` 必须是 `ExecStart` 主进程，
//! 退 86 才被 `RestartPreventExitStatus=86` 捕获。`guard` 末尾
//! `exec robot-s0` 保住 `MainPID`。handoff unit 走 `preflight-handoff`
//! （`Type=oneshot` + `RemainAfterExit=yes`）。
//!
//! ## 退出码
//!
//! - `0`：成功（`guard` 成功 = `exec` 不返回）。
//! - `2`：CLI 参数解析失败。
//! - `70`：`preflight-arm` / `preflight-handoff` 失败。
//! - `71`：`provision` 三分支业务失败（损坏副本 / 写入 / 目录 fsync）。
//! - `86`：**guard 专用停止态**（预算耗尽 / 计数 I/O / 锁冲突 / 配置级
//!   `exec` 前置失败）——见 `deploy::guard` 码表。
//! - `89`：latch 目标稳定锁被另一进程持有（S48 A3d / W1；`guard` 与
//!   `provision` 同码，unit 禁止重启）。
//! - `1`：latch 名字 / 锁 IO / 对象身份错误（配置或系统错误，不伪称被占用）。

#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use robot_s0::config_loader::load_robot_config;
use robot_s0::deploy::guard::{GuardConfig, PREFLIGHT_FAIL_EXIT_CODE, real::run_guard};
use robot_s0::deploy::preflight::{
    DeployTransport, ensure_safe_arm, ensure_safe_handoff, feetech::open_arm_bus,
};
use robot_s0::deploy::provision::provision_latch;
use robot_s0::deploy::types::DeployArm;

/// systemd `RuntimeDirectory=robot-system/preflight` 的落点。
const DEFAULT_RUNTIME_DIR: &str = "/run/robot-system/preflight";
/// CLI 解析失败码（与 `robot-s0` 一致）。
const USAGE_EXIT_CODE: u8 = 2;

fn main() {
    robot_logging::set_process_name("robot-deploy");
    let _ = robot_logging::init();
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Some(subcommand) = argv.first().and_then(|a| a.to_str()).map(str::to_owned) else {
        print_help();
        std::process::exit(i32::from(USAGE_EXIT_CODE));
    };
    let rest = &argv[1..];

    match subcommand.as_str() {
        "guard" => cmd_guard(rest),
        "preflight-arm" => cmd_preflight_arm(rest),
        "preflight-handoff" => cmd_preflight_handoff(rest),
        "provision" => cmd_provision(rest),
        "-h" | "--help" | "help" => {
            print_help();
            std::process::exit(0);
        }
        other => usage(&format!("unknown subcommand {other:?}")),
    }
}

// ============================================================================
// 子命令
// ============================================================================

fn cmd_guard(args: &[OsString]) -> ! {
    // `--` 之后原样透传给 robot-s0（**不含** --config：guard 自动追加）。
    let (own, s0_args) = split_at_double_dash(args);
    let opts = Options::parse(&own);
    let arm = require_arm(&opts);
    let config_path = require_path(&opts, "--config");
    let s0_bin = require_path(&opts, "--s0-bin");
    let runtime_dir = opts
        .value("--runtime-dir")
        .map_or_else(|| PathBuf::from(DEFAULT_RUNTIME_DIR), PathBuf::from);
    let robot_id = opts
        .value("--robot-id")
        .map_or_else(|| robot_id_from_config(&config_path), |v| to_utf8(&v));
    opts.reject_unknown();

    run_guard(&GuardConfig {
        arm,
        config_path,
        s0_bin,
        runtime_dir,
        robot_id,
        s0_args,
    })
}

fn cmd_preflight_arm(args: &[OsString]) -> ! {
    let opts = Options::parse(args);
    let arm = require_arm(&opts);
    let config_path = require_path(&opts, "--config");
    opts.reject_unknown();

    let mut bus = match open_arm_bus(arm, &config_path) {
        Ok(bus) => bus,
        Err(err) => {
            tracing::error!(%arm, %err, "preflight-arm: bus unavailable");
            std::process::exit(i32::from(PREFLIGHT_FAIL_EXIT_CODE));
        }
    };
    match ensure_safe_arm(arm, &mut bus) {
        Ok(()) => {
            tracing::info!(%arm, motors = arm.all_ids().len(), "preflight-arm: bus is safe");
            std::process::exit(0)
        }
        Err(report) => {
            tracing::error!(%arm, failures = %report, "preflight-arm: FAIL, refusing handover");
            std::process::exit(i32::from(PREFLIGHT_FAIL_EXIT_CODE))
        }
    }
}

fn cmd_preflight_handoff(args: &[OsString]) -> ! {
    let opts = Options::parse(args);
    let left_config = require_path(&opts, "--left-config");
    let right_config = require_path(&opts, "--right-config");
    opts.reject_unknown();

    // 构造失败**不特判**：以 `Err(构造错误)` 传入，由 ensure_safe_handoff
    // 聚合成 `Connect{arm}`，另一侧照常完整执行（R10-3）。
    let mut left = open_arm_bus(DeployArm::Left, &left_config);
    let mut right = open_arm_bus(DeployArm::Right, &right_config);
    let result = ensure_safe_handoff(as_side(left.as_mut()), as_side(right.as_mut()));
    match result {
        Ok(()) => {
            tracing::info!(
                motors = 17,
                writes = 20,
                "preflight-handoff: both buses are safe"
            );
            std::process::exit(0)
        }
        Err(report) => {
            tracing::error!(failures = %report, "preflight-handoff: FAIL, refusing handover");
            std::process::exit(i32::from(PREFLIGHT_FAIL_EXIT_CODE))
        }
    }
}

fn cmd_provision(args: &[OsString]) -> ! {
    let opts = Options::parse(args);
    // 三个选项一次性认领：`--latch-path` 优先，但同时给出另两个也不算未知
    // 选项（reject_unknown 只拦拼写错误）。
    let explicit_latch = opts.value("--latch-path");
    let explicit_robot_id = opts.value("--robot-id");
    let explicit_config = opts.value("--config");
    opts.reject_unknown();

    let latch_path = if let Some(v) = explicit_latch {
        PathBuf::from(v)
    } else {
        let robot_id = match (explicit_robot_id, explicit_config) {
            (Some(id), _) => to_utf8(&id),
            (None, Some(cfg)) => robot_id_from_config(Path::new(&cfg)),
            (None, None) => usage("provision needs --latch-path, --robot-id or --config"),
        };
        PathBuf::from(format!("/var/lib/robot/{robot_id}/estop_latch"))
    };

    match provision_latch(&latch_path) {
        Ok(outcome) => {
            tracing::info!(latch = %latch_path.display(), ?outcome, "provision: ok");
            std::process::exit(0)
        }
        Err(err) => {
            // W1 类型化分流：锁被占用 89；名字 / 锁 IO / 对象身份 1；业务失败 71。
            let code = err.exit_code();
            tracing::error!(latch = %latch_path.display(), %err, code, "provision: FAIL");
            std::process::exit(i32::from(code))
        }
    }
}

// ============================================================================
// CLI helper（手写解析，与 robot-s0 同风格；不引 clap）
// ============================================================================

/// `--` 分界：前半是本命令自己的选项，后半原样透传。
fn split_at_double_dash(args: &[OsString]) -> (Vec<OsString>, Vec<OsString>) {
    args.iter().position(|a| a == "--").map_or_else(
        || (args.to_vec(), Vec::new()),
        |i| (args[..i].to_vec(), args[i + 1..].to_vec()),
    )
}

/// 已解析的 `--key value` 集合（本 CLI 无 flag 型选项）。
struct Options {
    pairs: Vec<(String, OsString)>,
    consumed: std::cell::RefCell<Vec<String>>,
}

impl Options {
    fn parse(args: &[OsString]) -> Self {
        let mut pairs = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let Some(key) = args[i].to_str() else {
                usage("non-UTF8 option name");
            };
            if !key.starts_with("--") {
                usage(&format!("unexpected argument {key:?}"));
            }
            if let Some((k, v)) = key.split_once('=') {
                pairs.push((k.to_string(), OsString::from(v)));
                i += 1;
                continue;
            }
            let Some(value) = args.get(i + 1) else {
                usage(&format!("{key} needs a value"));
            };
            pairs.push((key.to_string(), value.clone()));
            i += 2;
        }
        Self {
            pairs,
            consumed: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn value(&self, key: &str) -> Option<OsString> {
        self.consumed.borrow_mut().push(key.to_string());
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    /// 未被任何 `value()` 认领的选项 = 拼写错误 → 直接拒绝（fail-closed，
    /// 防"选项写错被静默忽略"）。
    fn reject_unknown(&self) {
        let consumed = self.consumed.borrow();
        for (key, _) in &self.pairs {
            if !consumed.contains(key) {
                usage(&format!("unknown option {key}"));
            }
        }
    }
}

fn require_arm(opts: &Options) -> DeployArm {
    let raw = opts
        .value("--arm")
        .unwrap_or_else(|| usage("--arm is required"));
    let text = to_utf8(&raw);
    DeployArm::parse(&text).unwrap_or_else(|| usage("--arm must be left or right"))
}

fn require_path(opts: &Options, key: &str) -> PathBuf {
    opts.value(key)
        .map_or_else(|| usage(&format!("{key} is required")), PathBuf::from)
}

fn to_utf8(value: &OsString) -> String {
    value
        .to_str()
        .unwrap_or_else(|| usage("non-UTF8 option value"))
        .to_string()
}

/// 从部署config的 `[robot] id` 取运行身份（单源：不另给默认值）。
fn robot_id_from_config(path: &Path) -> String {
    match load_robot_config(path) {
        Ok(loaded) => loaded.parsed.robot.id,
        Err(e) => {
            tracing::error!(config = %path.display(), error = %e, "cannot read [robot] id");
            std::process::exit(i32::from(USAGE_EXIT_CODE));
        }
    }
}

/// `Result<Bus, String>` → `ensure_safe_handoff` 需要的
/// `Result<&mut dyn DeployTransport, String>`。
fn as_side<'a, B: DeployTransport + 'a>(
    side: Result<&'a mut B, &mut String>,
) -> Result<&'a mut dyn DeployTransport, String> {
    match side {
        Ok(bus) => Ok(bus),
        Err(err) => Err(err.clone()),
    }
}

fn usage(message: &str) -> ! {
    tracing::error!(error = message, "robot-deploy: bad invocation");
    print_help();
    std::process::exit(i32::from(USAGE_EXIT_CODE))
}

fn print_help() {
    println!(
        "\
robot-deploy —— XLeRobot 真机部署件（v0.5 候选 E 段(9)）

用法：
  robot-deploy guard --arm <left|right> --config <arm/full config>
                        --s0-bin <绝对路径> [--runtime-dir <dir>]
                        [--robot-id <id>] -- <s0 args，不含 --config>
  robot-deploy preflight-arm --arm <left|right> --config <arm/full config>
  robot-deploy preflight-handoff --left-config <path> --right-config <path>
  robot-deploy provision [--config <arm/full config>] [--robot-id <id>]
                            [--latch-path <path>]

说明：
  guard              六步 fail-closed 启动链：取实例锁 → 读连败计数（==5 退 86）
                     → 先递增 → 本 bus ensure-safe → latch provision → 清零
                     → exec robot-s0（自动追加 --config，s0 args 自带即退 86）。
  preflight-arm      只对本臂 bus 做 ensure-safe（左 8 条写 / 右 12 条写）。
  preflight-handoff  双 bus 全 17 电机 / 20 条写；一侧串口失败不阻断另一侧。
  provision          latch 三分支 provision + 统一 parent-dir fsync。

退出码：0 成功 / 2 CLI 错误 / 70 preflight 失败 / 71 provision 失败 /
        86 guard 停止态（需人工恢复：修硬件 → 清 <robot_id>.count → systemctl start）。
默认 --runtime-dir：{DEFAULT_RUNTIME_DIR}"
    );
}
