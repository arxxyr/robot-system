//! `freeze_systemd_smoke` —— v0.3 A2-3 systemd watchdog e2e smoke。
//!
//! 验证 [`docs/00_design.md`](../../../docs/00_design.md) §6.7 不变量 #2
//! "systemd 是最外层看门狗" 在真 user systemd unit 上端到端成立：
//!
//! ```text
//! 1. 起 user-mode systemd unit (Type=notify + WatchdogSec=2s + Restart=on-failure)
//! 2. 等 daemon active + 抓 PID0
//! 3. 通过 test-only iceoryx2 topic `/<robot_id>/test/freeze` publish
//!    TestFreezeCommand { worker=ipc, duration_ms=5000 }
//! 4. daemon ipc_thread 在 tick loop 顶端 freeze_check 真 spin → tick_count
//!    停滞 → LivenessObserver 不喂 sd_notify(WATCHDOG=1) → systemd 在
//!    WatchdogSec=2s 处发 SIGABRT
//! 5. Restart=on-failure 拉起新 daemon → 新 PID1
//! 6. 断言：PID1 ≠ PID0 + PID1 ≠ 0 + journal 含 "withholding sd_notify
//!    (WATCHDOG=1)" + 至少两次 daemon "ready" 事件 + latch 文件保持 "0\n"
//! ```
//!
//! ## 跟 reviewer A2-3 拍板的 8 条硬边界对应
//!
//! 1. **不依赖 Feetech 真硬件**：smoke 自己生成 workdir 内最小 mock config
//!    （`mock_hard_rt` + 1 joint），**不**走 `xlerobot_bus2_full.toml`。
//! 2. **user service 兼容模板**：`scripts/systemd/xlerobot-s0-freeze-test.service`
//!    无 `User=` / `Requires=dev-tty*` / `ReadWritePaths=/var/lib/...`，
//!    保留 `Type=notify` / `WatchdogSec=2s` / `Restart=on-failure`。
//! 3. **install 走 `$XDG_RUNTIME_DIR/systemd/user/`**：user manager 默认扫
//!    这个目录，**不**用 `/tmp/<unit>` + `daemon-reload`。
//! 4. **清理顺序**：stop → reset-failed → rm rendered unit → daemon-reload。
//! 5. **`--user` 域 + preflight**：`XDG_RUNTIME_DIR` + `systemctl --user
//!    is-system-running` 可用；缺则**环境错误退出**（exit 1，**不**算测试
//!    失败 / **不**是 PASS / FAIL 矩阵的一部分）。
//! 6. **5 round 内部唯一化**：每 iter 自带 `<prefix>_<pid>_<iter>` 后缀
//!    （robot_id / unit_name / workdir 都嵌入），避免 journal / iceoryx2
//!    service / unit failed state 串轮污染。
//! 7. **默认只测 ipc_thread**：`--worker ipc`（默认）；`serial_io` / `pub`
//!    / `io` / `rt` 通过 CLI flag 扩展，**不**强制 smoke 多 worker 覆盖。
//! 8. **wire struct 复用**：`robot_s0::freeze_control::{TestFreezeCommand,
//!    WorkerKind}` 通过 `feature = "freeze-systemd-smoke"` 守门的 optional
//!    dep 引入，**不**复制一份。
//!
//! ## 用法
//!
//! ```bash
//! # 1) 编 daemon binary（必须带 test-freeze-hook feature）
//! cargo build -p robot-s0 --bin s0_daemon --features test-freeze-hook
//!
//! # 2) 编 + 跑 smoke
//! cargo run -p robot-tests --bin freeze_systemd_smoke \
//!     --features freeze-systemd-smoke -- \
//!     --daemon-bin target/debug/s0_daemon \
//!     --iterations 5
//! ```
//!
//! ## 退出码
//!
//! - `0`：所有 iteration PASS。
//! - `1`：preflight / 环境错（XDG_RUNTIME_DIR / user manager / daemon-bin
//!   缺）；**不**算测试失败。
//! - `2`：≥ 1 次 iteration FAIL。
//!
//! ## 不在本 binary 范围
//!
//! - **真机回归**：v0.3_entry.md §4.5 要求 Orin 1 round；smoke 本身可在
//!   Orin 上跑（同样 `--user` 域），但 Orin 部分留 A2-4 单独 commit。
//! - **多 worker scope**：smoke 默认 ipc_thread；serial_io 真机 SerialRT
//!   覆盖留 lib 单测 + Orin runbook，**不**作为 5/5 smoke 必测。

// 中文 doc 段大量出现 `Ipc` / `WatchdogSec=2s` / `XDG_RUNTIME_DIR` /
// `Restart=on-failure` / `freeze-systemd-smoke` 等 identifier-like / config-like
// 字串。本 binary 是 test-only，doc 完整性优先于 doc-markdown 严格化。
#![allow(clippy::doc_markdown)]
#![forbid(unused_must_use)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use iceoryx2::prelude::{NodeBuilder, ServiceName};
use iceoryx2::service::ipc::Service as IxService;
use robot_s0::freeze_control::{TestFreezeCommand, WorkerKind};

// ============================================================================
// CLI
// ============================================================================

struct Args {
    daemon_bin: PathBuf,
    unit_template: PathBuf,
    workdir_base: PathBuf,
    robot_id_prefix: String,
    unit_name_prefix: String,
    hardware_id: String,
    tick_hz: u32,
    iterations: u32,
    worker: WorkerKind,
    freeze_duration_ms: u32,
    /// 等 systemd 拉起 daemon active 的超时（毫秒）。
    daemon_active_wait_ms: u64,
    /// freeze 后等多久看 PID 切换 + journal 断言（毫秒）。
    /// 默认 8s = WatchdogSec(2s) + RestartSec(2s) + new-daemon-ready(~3s) + jitter。
    restart_wait_ms: u64,
}

impl Args {
    #[allow(clippy::too_many_lines)]
    fn parse() -> Result<Self, String> {
        let mut args = Self {
            daemon_bin: PathBuf::new(),
            unit_template: PathBuf::from("scripts/systemd/xlerobot-s0-freeze-test.service"),
            workdir_base: PathBuf::from("logs/freeze_smoke"),
            robot_id_prefix: "freeze_smoke".into(),
            unit_name_prefix: "freeze-smoke".into(),
            hardware_id: "mock_arm".into(),
            tick_hz: 50,
            iterations: 5,
            worker: WorkerKind::Ipc,
            freeze_duration_ms: 5000,
            daemon_active_wait_ms: 10_000,
            restart_wait_ms: 8_000,
        };
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            match a.as_str() {
                "--daemon-bin" => {
                    i += 1;
                    args.daemon_bin =
                        PathBuf::from(raw.get(i).ok_or("--daemon-bin requires value")?);
                }
                "--unit-template" => {
                    i += 1;
                    args.unit_template =
                        PathBuf::from(raw.get(i).ok_or("--unit-template requires value")?);
                }
                "--workdir-base" => {
                    i += 1;
                    args.workdir_base =
                        PathBuf::from(raw.get(i).ok_or("--workdir-base requires value")?);
                }
                "--robot-id-prefix" => {
                    i += 1;
                    args.robot_id_prefix
                        .clone_from(raw.get(i).ok_or("--robot-id-prefix requires value")?);
                }
                "--unit-name-prefix" => {
                    i += 1;
                    args.unit_name_prefix
                        .clone_from(raw.get(i).ok_or("--unit-name-prefix requires value")?);
                }
                "--hardware-id" => {
                    i += 1;
                    args.hardware_id
                        .clone_from(raw.get(i).ok_or("--hardware-id requires value")?);
                }
                "--tick-hz" => {
                    i += 1;
                    args.tick_hz = raw
                        .get(i)
                        .ok_or("--tick-hz requires value")?
                        .parse()
                        .map_err(|e| format!("--tick-hz: {e}"))?;
                }
                "--iterations" => {
                    i += 1;
                    args.iterations = raw
                        .get(i)
                        .ok_or("--iterations requires value")?
                        .parse()
                        .map_err(|e| format!("--iterations: {e}"))?;
                }
                "--worker" => {
                    i += 1;
                    let s = raw.get(i).ok_or("--worker requires value")?;
                    args.worker = match s.as_str() {
                        "ipc" => WorkerKind::Ipc,
                        "rt" => WorkerKind::Rt,
                        "serial_io" => WorkerKind::SerialIo,
                        "pub" => WorkerKind::Pub,
                        "io" => WorkerKind::Io,
                        other => return Err(format!("--worker: unknown kind {other:?}")),
                    };
                }
                "--freeze-duration-ms" => {
                    i += 1;
                    args.freeze_duration_ms = raw
                        .get(i)
                        .ok_or("--freeze-duration-ms requires value")?
                        .parse()
                        .map_err(|e| format!("--freeze-duration-ms: {e}"))?;
                }
                "--daemon-active-wait-ms" => {
                    i += 1;
                    args.daemon_active_wait_ms = raw
                        .get(i)
                        .ok_or("--daemon-active-wait-ms requires value")?
                        .parse()
                        .map_err(|e| format!("--daemon-active-wait-ms: {e}"))?;
                }
                "--restart-wait-ms" => {
                    i += 1;
                    args.restart_wait_ms = raw
                        .get(i)
                        .ok_or("--restart-wait-ms requires value")?
                        .parse()
                        .map_err(|e| format!("--restart-wait-ms: {e}"))?;
                }
                "-h" | "--help" => return Err("HELP".into()),
                other => return Err(format!("unknown arg: {other}")),
            }
            i += 1;
        }
        if args.daemon_bin.as_os_str().is_empty() {
            return Err("--daemon-bin <path> is required".into());
        }
        Ok(args)
    }
}

fn print_help() {
    eprintln!(
        "freeze_systemd_smoke (v0.3 A2-3 systemd watchdog e2e smoke)

USAGE:
  freeze_systemd_smoke --daemon-bin <PATH> [OPTIONS]

REQUIRED:
  --daemon-bin <PATH>         s0_daemon binary path（必须带 --features test-freeze-hook 编译）

OPTIONS:
  --unit-template <PATH>      systemd unit 模板（默认 scripts/systemd/xlerobot-s0-freeze-test.service）
  --workdir-base <DIR>        每 iter workdir 根（默认 logs/freeze_smoke/）
  --robot-id-prefix <STR>     robot_id 前缀（默认 freeze_smoke；每 iter 加 _<pid>_<iter>）
  --unit-name-prefix <STR>    unit name 前缀（默认 freeze-smoke；每 iter 加 -<pid>-<iter>.service）
  --hardware-id <ID>          配置内 [[hardware]].id（默认 mock_arm）
  --tick-hz <HZ>              daemon rt_thread tick 频率（默认 50）
  --iterations <N>            iteration 数（默认 5）
  --worker <KIND>             ipc|rt|serial_io|pub|io（默认 ipc，systemd e2e 必测之一）
  --freeze-duration-ms <MS>   freeze 时长（默认 5000；> WatchdogSec=2s 让 systemd 真重启）
  --daemon-active-wait-ms <MS> 等 systemd active 超时（默认 10000）
  --restart-wait-ms <MS>      freeze 后等多久看 PID 切换（默认 8000 = Watchdog+Restart+新 daemon ready）
  -h, --help                  显示帮助

退出码:
  0 = 全过；1 = preflight / env 错（XDG_RUNTIME_DIR / user manager 缺）；2 = ≥1 iter FAIL
"
    );
}

// ============================================================================
// main
// ============================================================================

#[derive(Debug)]
struct IterResult {
    seq: u32,
    robot_id: String,
    unit_name: String,
    pass: bool,
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

    let mut results: Vec<IterResult> = Vec::with_capacity(args.iterations as usize);
    for seq in 0..args.iterations {
        let pid = std::process::id();
        let robot_id = format!("{}_{}_{}", args.robot_id_prefix, pid, seq);
        let unit_name = format!("{}-{}-{}.service", args.unit_name_prefix, pid, seq);
        let workdir = args.workdir_base.join(seq.to_string());
        eprintln!(
            "\n========== iteration {}/{} robot_id={} unit={} ==========",
            seq + 1,
            args.iterations,
            robot_id,
            unit_name,
        );
        let result = run_iteration(&args, &env, seq, &robot_id, &unit_name, &workdir);
        // 即使 PASS 也要清理 unit / workdir，避免下轮串污染
        cleanup_iteration(&env, &unit_name);
        results.push(result);
    }

    // 最后再 daemon-reload 一次，让 user manager 状态完全干净
    let _ = systemctl_user(&["daemon-reload"]);

    eprintln!("\n========== summary ==========");
    let mut pass_count = 0;
    let mut fail_count = 0;
    for r in &results {
        let tag = if r.pass { "PASS" } else { "FAIL" };
        eprintln!(
            "  iter {} robot_id={} unit={} : {} {}",
            r.seq,
            r.robot_id,
            r.unit_name,
            tag,
            r.fail_reason.as_deref().unwrap_or(""),
        );
        if r.pass {
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
    /// 模板内容（每 iter 复用，避免重复 read）。
    template: String,
}

fn preflight(args: &Args) -> Result<EnvCtx, String> {
    // (1) daemon_bin
    if !args.daemon_bin.exists() {
        return Err(format!(
            "--daemon-bin not found: {}（先 `cargo build -p robot-s0 --bin s0_daemon \
             --features test-freeze-hook`）",
            args.daemon_bin.display()
        ));
    }
    // (2) unit_template
    let template = std::fs::read_to_string(&args.unit_template)
        .map_err(|e| format!("read unit template {}: {e}", args.unit_template.display()))?;
    // (3) XDG_RUNTIME_DIR
    let xdg = std::env::var("XDG_RUNTIME_DIR").map_err(|_| {
        "XDG_RUNTIME_DIR not set; user manager 不可用 / 未 login 进 systemd \
         user session（CI 容器场景：先 `loginctl enable-linger $USER` 或在
         systemd-run --user --scope 内跑）"
            .to_string()
    })?;
    let user_unit_dir = PathBuf::from(&xdg).join("systemd/user");
    std::fs::create_dir_all(&user_unit_dir)
        .map_err(|e| format!("create {}: {e}", user_unit_dir.display()))?;
    // (4) systemctl --user 可用
    let probe = Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .status();
    match probe {
        Ok(s) if s.code().is_some() => {
            // is-system-running 可能返 degraded / running / starting 都算可用；
            // 真正不可用是 systemctl 本身退出错误（命令不存在 / 找不到 user manager）
        }
        Ok(_) => {
            return Err("systemctl --user is-system-running terminated by signal".into());
        }
        Err(e) => {
            return Err(format!(
                "systemctl --user is-system-running spawn failed: {e}"
            ));
        }
    }
    // (5) workdir_base 准备好
    std::fs::create_dir_all(&args.workdir_base)
        .map_err(|e| format!("create workdir_base {}: {e}", args.workdir_base.display()))?;
    Ok(EnvCtx {
        user_unit_dir,
        template,
    })
}

// ============================================================================
// 单次迭代
// ============================================================================

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn run_iteration(
    args: &Args,
    env: &EnvCtx,
    seq: u32,
    robot_id: &str,
    unit_name: &str,
    workdir: &Path,
) -> IterResult {
    let pass = |fail_reason: Option<String>| IterResult {
        seq,
        robot_id: robot_id.into(),
        unit_name: unit_name.into(),
        pass: fail_reason.is_none(),
        fail_reason,
    };
    let fail = |reason: String| -> IterResult {
        eprintln!("  ✗ FAIL: {reason}");
        IterResult {
            seq,
            robot_id: robot_id.into(),
            unit_name: unit_name.into(),
            pass: false,
            fail_reason: Some(reason),
        }
    };

    // ---- 1) 干净 workdir ----
    let _ = std::fs::remove_dir_all(workdir);
    if let Err(e) = std::fs::create_dir_all(workdir) {
        return fail(format!("create workdir {}: {e}", workdir.display()));
    }
    let workdir_abs = match std::fs::canonicalize(workdir) {
        Ok(p) => p,
        Err(e) => return fail(format!("canonicalize workdir {}: {e}", workdir.display())),
    };
    let daemon_bin_abs = match std::fs::canonicalize(&args.daemon_bin) {
        Ok(p) => p,
        Err(e) => {
            return fail(format!(
                "canonicalize daemon_bin {}: {e}",
                args.daemon_bin.display()
            ));
        }
    };

    // ---- 2) 写 minimal mock config TOML ----
    let config_path = workdir_abs.join("config.toml");
    let config_text = minimal_mock_config(robot_id, &args.hardware_id);
    if let Err(e) = std::fs::write(&config_path, &config_text) {
        return fail(format!("write config {}: {e}", config_path.display()));
    }

    // ---- 3) provision latch primary + bak = "0\n" ----
    let latch_path = workdir_abs.join("estop_latch");
    let latch_bak = workdir_abs.join("estop_latch.bak");
    if let Err(e) = std::fs::write(&latch_path, "0\n") {
        return fail(format!("write latch primary {}: {e}", latch_path.display()));
    }
    if let Err(e) = std::fs::write(&latch_bak, "0\n") {
        return fail(format!("write latch bak {}: {e}", latch_bak.display()));
    }

    // ---- 4) 渲染 unit 模板进 $XDG_RUNTIME_DIR/systemd/user/ ----
    let rendered = env
        .template
        .replace("@ROBOT_ID@", robot_id)
        .replace("@WORKDIR@", &workdir_abs.display().to_string())
        .replace("@DAEMON_BIN@", &daemon_bin_abs.display().to_string())
        .replace("@CONFIG_PATH@", &config_path.display().to_string())
        .replace("@LATCH_PATH@", &latch_path.display().to_string())
        .replace("@HARDWARE_ID@", &args.hardware_id)
        .replace("@TICK_HZ@", &args.tick_hz.to_string());
    let unit_path = env.user_unit_dir.join(unit_name);
    if let Err(e) = std::fs::write(&unit_path, &rendered) {
        return fail(format!("write unit {}: {e}", unit_path.display()));
    }

    // ---- 5) systemctl --user daemon-reload + start ----
    if let Err(e) = systemctl_user(&["daemon-reload"]) {
        return fail(format!("daemon-reload: {e}"));
    }
    if let Err(e) = systemctl_user(&["start", unit_name]) {
        return fail(format!("start {unit_name}: {e}"));
    }

    // ---- 6) 等 daemon active + 抓 PID0 ----
    let pid0 = match wait_for_active_pid(unit_name, args.daemon_active_wait_ms) {
        Ok(pid) => pid,
        Err(e) => return fail(format!("waiting for daemon active: {e}")),
    };
    eprintln!("  ✓ daemon active, PID0={pid0}");

    // ---- 7) freeze ipc_thread (or other worker) ----
    if let Err(e) = publish_freeze(robot_id, args.worker, args.freeze_duration_ms) {
        return fail(format!("publish freeze: {e}"));
    }
    eprintln!(
        "  ✓ published freeze cmd: worker={:?} duration_ms={}",
        args.worker, args.freeze_duration_ms,
    );

    // ---- 8) 等 systemd 触发 watchdog timeout + Restart=on-failure 拉起新 daemon ----
    sleep_ms(args.restart_wait_ms);

    // ---- 9) 抓 PID1 ----
    let pid1 = match read_active_pid(unit_name) {
        Ok(p) => p,
        Err(e) => return fail(format!("read PID1: {e}")),
    };
    eprintln!("  PID after wait: PID1={pid1}");

    // ---- 10) 断言 PID0 ≠ PID1 + PID1 ≠ 0 ----
    if pid1 == 0 {
        return fail(format!(
            "PID1=0 (unit not active after restart-wait-ms={}; \
             possibly StartLimitBurst hit or restart never triggered)",
            args.restart_wait_ms
        ));
    }
    if pid1 == pid0 {
        return fail(format!(
            "PID didn't change: PID0=PID1={pid0} (systemd watchdog 没真重启 \
             daemon —— freeze hook 路径或 LivenessObserver liveness-gating 失效)"
        ));
    }
    eprintln!("  ✓ PID switched: PID0={pid0} → PID1={pid1} (systemd watchdog restart)");

    // ---- 11) journalctl 看 withholding warn + 两次 ready ----
    let journal = match journalctl_user(unit_name) {
        Ok(j) => j,
        Err(e) => return fail(format!("journalctl: {e}")),
    };
    let withholding_count = count_substring(&journal, "withholding sd_notify");
    if withholding_count == 0 {
        return fail(format!(
            "journal missing 'withholding sd_notify' warn (LivenessObserver 可能没 \
             liveness-gate worker stall); journal tail:\n{}",
            tail_lines(&journal, 30)
        ));
    }
    let ready_count = count_substring(&journal, "freeze_control thread ready")
        + count_substring(&journal, "watchdog ping started");
    if ready_count < 2 {
        return fail(format!(
            "journal expected ≥ 2 daemon ready markers (first daemon + restart \
             daemon)，got {ready_count}; journal tail:\n{}",
            tail_lines(&journal, 30)
        ));
    }
    eprintln!("  ✓ journal: withholding-warns={withholding_count} ready-markers={ready_count}");

    // ---- 12) latch 文件保持 "0\n"（fsync 不被 watchdog kill 干扰）----
    let primary_after = std::fs::read_to_string(&latch_path).unwrap_or_default();
    let bak_after = std::fs::read_to_string(&latch_bak).unwrap_or_default();
    if primary_after.trim() != "0" || bak_after.trim() != "0" {
        return fail(format!(
            "latch corrupted post-restart: primary={primary_after:?} bak={bak_after:?} \
             (expected '0\\n' both); fsync window 可能被 watchdog kill 干扰"
        ));
    }
    eprintln!("  ✓ latch primary/bak both preserved as '0\\n'");

    pass(None)
}

// ============================================================================
// systemctl --user / journalctl --user 辅助
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
    let pid: u32 = s
        .trim()
        .parse()
        .map_err(|e| format!("MainPID parse {:?}: {e}", s.trim()))?;
    Ok(pid)
}

fn wait_for_active_pid(unit_name: &str, timeout_ms: u64) -> Result<u32, String> {
    let start = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    while start.elapsed() < timeout {
        if let Ok(pid) = read_active_pid(unit_name)
            && pid != 0
        {
            // 还要确认 active（pid != 0 也可能在 activating）
            let active = Command::new("systemctl")
                .args(["--user", "is-active", unit_name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if active {
                return Ok(pid);
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "unit {unit_name} did not reach active within {timeout_ms}ms"
    ))
}

fn journalctl_user(unit_name: &str) -> Result<String, String> {
    let out = Command::new("journalctl")
        .args([
            "--user",
            "-u",
            unit_name,
            "--no-pager",
            "--output",
            "short-iso",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn journalctl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "journalctl --user -u {unit_name} exit={}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn cleanup_iteration(env: &EnvCtx, unit_name: &str) {
    let _ = systemctl_user(&["stop", unit_name]);
    let _ = systemctl_user(&["reset-failed", unit_name]);
    let unit_path = env.user_unit_dir.join(unit_name);
    let _ = std::fs::remove_file(&unit_path);
    // daemon-reload 留到 main 里所有 iter 跑完后做（避免每轮 reload 慢）
}

// ============================================================================
// iceoryx2 publish + minimal config 生成
// ============================================================================

fn publish_freeze(robot_id: &str, worker: WorkerKind, duration_ms: u32) -> Result<(), String> {
    let service_path = format!("/{robot_id}/test/freeze");
    let node = NodeBuilder::new()
        .create::<IxService>()
        .map_err(|e| format!("NodeBuilder: {e:?}"))?;
    let service_name =
        ServiceName::new(&service_path).map_err(|e| format!("ServiceName({service_path}): {e}"))?;
    let service = node
        .service_builder(&service_name)
        .publish_subscribe::<TestFreezeCommand>()
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
    let sample = sample.write_payload(TestFreezeCommand {
        kind: worker as u8,
        pad: [0; 3],
        duration_ms,
    });
    sample.send().map_err(|e| format!("send: {e:?}"))?;
    // **关键**：publisher / service / node 在函数返回时一起 drop。如果 publisher
    // 端口在 daemon subscriber 完成 poll-receive 之前关闭，sample 可能丢（实测：
    // sample 写进 shm，subscriber 在 2ms poll 周期里没拉到就被 publisher 关闭
    // 时回收）。睡 ≥ 1 个 subscriber poll cycle (2ms) + 余量，让 daemon 有
    // 机会消费一次。100ms 远大于必需，但本 publish 是 5 round / 5s 周期里的
    // 单次操作，开销可忽略。
    thread::sleep(Duration::from_millis(100));
    Ok(())
}

fn minimal_mock_config(robot_id: &str, hardware_id: &str) -> String {
    // mock_hard_rt + 1 joint，**不**走 feetech / mock_omni3。跟 daemon
    // robot-s0 `mock-only` feature 一致；不依赖 Feetech / 真硬件。
    format!(
        "# v0.3 A2-3 freeze_systemd_smoke 自动生成的最小 mock config。\n\
         # 不依赖 Feetech / mock_omni3 / 真硬件。robot_id 每 iter 唯一。\n\
         [robot]\n\
         id             = \"{robot_id}\"\n\
         config_version = \"0.3.0\"\n\
         \n\
         [[hardware]]\n\
         id          = \"{hardware_id}\"\n\
         adapter     = \"mock_hard_rt\"\n\
         address     = \"mock://freeze_smoke\"\n\
         joints      = [\"j0\"]\n\
         auto_enable = true\n",
    )
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
    let mut count = 0;
    let mut from = 0;
    while let Some(idx) = s[from..].find(needle) {
        count += 1;
        from = from + idx + needle.len();
    }
    count
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}
