//! `crash_restart_smoke` —— Step 9 `EStop` latch crash/restart 测试套。
//!
//! 验证 [`docs/00_design.md`](../../../docs/00_design.md) §6.7 不变量 #3 + #4
//! 在真机的 `SIGKILL` 路径下成立：
//!
//! 1. **`EStop` 不可被软件清除为失忆态**：daemon 在 estop 触发后 `SIGKILL`，重启
//!    必须读到 `initial estop_latched = true`，不能因 graceful shutdown 没跑
//!    完而丢 latch。
//! 2. **S0 重启不失忆**：latch primary + bak 两个文件 fsync 双写，对崩溃
//!    安全。
//!
//! 测试场景：`kill_after_estop_fsync_window`
//!   - spawn `s0_daemon` + `mock_s1` (`--amplitude 0.0`，**不**摆动，只发
//!     `Heartbeat` 让 `state→Enabled.Active`，避免对真机做 ballet 写入)
//!   - 等 3 s 让 `state` 进入 `Enabled.Active`
//!   - `robotctl estop` 触发 `EStop`
//!   - 睡 `--fsync-grace-ms`（默认 500 ms，远大于实测 188-255 ms 的 fsync
//!     window）
//!   - `SIGKILL` daemon（**不**给它 graceful shutdown 的机会）
//!   - 读 latch primary + bak 两个文件，**断言**都 = "1"
//!   - 重启 daemon（短 smoke），抓 stderr 按结构化字段断言 message
//!     `initial estop_latched` + `latched=true`（兼容 human + JSON tracing
//!     格式；详 [`log_contains_initial_latched_true`] + 同名单测）
//!   - 一致 = PASS；不一致 = FAIL
//!
//! 多次迭代（`--iterations`）每次用不同 `robot_id` 后缀避免 iceoryx2 service
//! collision，验证可重复性 + 找时序边缘 case。
//!
//! ### 用法
//!
//! ```bash
//! # 默认（dev box 兼容）：5 次迭代，mock_hard_rt 单关节，target/release 已 build
//! cargo run --release -p robot-tests --bin crash_restart_smoke
//!
//! # Orin 真机 bus2 9-motor 复跑：
//! ./target/release/crash_restart_smoke \
//!     --bin-dir target/release \
//!     --robot-id-prefix crash_test \
//!     --hardware-id right_arm_full_with_base \
//!     --config configs/xlerobot_bus2_full.toml \
//!     --joints 9 --frozen-joints 6,7,8 \
//!     --iterations 10 --fsync-grace-ms 500
//! ```
//!
//! ## dev box vs Orin 真机
//!
//! 默认 `--config configs/crash_restart_smoke.toml` 走 `mock_hard_rt`
//! 单关节，**不**依赖 `--features xlerobot-real`（`serialport` /
//! `libudev` 系统依赖只在 Orin 真机 build）。`crash_restart_smoke` 测的
//! 是 §6.7 #4 "S0 重启不失忆" 的 fsync 双写 SIGKILL 路径，跟具体
//! adapter 类型无关 —— `mock_hard_rt` 跟 `feetech_sts3215` 走的是同一
//! 份 `EStop` event → `save_estop_latch(true)` → `1\n` 写 primary +
//! bak fsync 双写 path。
//!
//! Orin 真机额外验证 9-motor `SerialRT` 路径，需要显式 `--config
//! configs/xlerobot_bus2_full.toml` + `--joints 9 --frozen-joints 6,7,8`
//! + binary 用 `--features xlerobot-real` build。
//!
//! ### 退出码
//!
//! - `0`：所有迭代 PASS。
//! - `1`：CLI / 环境错（binary 找不到 / config 缺）。
//! - `2`：至少一次迭代 FAIL。
//!
//! ### 不做的事
//!
//! - 不验证 `mock_s1.anchor + estop` 之间的更细粒度 race（留给 §10
//!   verification crash 矩阵）。
//! - 不模拟 `kernel panic` / `power loss`（fsync 在 ext4 行为相同，但 SIGKILL
//!   是更弱的故障模型；power-loss 测试需要专门 fixture）。

#![forbid(unused_must_use)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

// ============================================================================
// CLI
// ============================================================================

struct Args {
    bin_dir: PathBuf,
    config: PathBuf,
    robot_id_prefix: String,
    hardware_id: String,
    iterations: u32,
    fsync_grace_ms: u64,
    /// daemon 启动后等多久（ms）才发 estop（让 state 先到 Enabled.Active）。
    pre_estop_settle_ms: u64,
    /// 重启 daemon 的 smoke-duration（s）—— 短即可，只为读 banner。
    restart_smoke_secs: u64,
    /// latch 临时目录。**不要**用 `/var/lib/robot/...` 防止 root owner 污染；
    /// 也不要用 `/tmp/` 跨进程冲突 —— 默认相对路径 `logs/crash_test/`，
    /// 调用方自行创建可写目录。
    latch_dir: PathBuf,
    /// `mock_s1 --joints` 个数。默认 1（跟 `configs/crash_restart_smoke.toml`
    /// 单 joint `smoke_arm` 配对）；Orin 真机 bus2 9-motor 走 `--joints 9`。
    mock_s1_joints: u8,
    /// `mock_s1 --frozen-joints` mask。默认空（单 joint 不需要）；Orin 真机
    /// bus2 9-motor 走 `--frozen-joints 6,7,8`（base 3 wheel）。
    mock_s1_frozen_joints: Option<String>,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut args = Self {
            bin_dir: PathBuf::from("target/release"),
            config: PathBuf::from("configs/crash_restart_smoke.toml"),
            robot_id_prefix: "crash_test".into(),
            hardware_id: "smoke_arm".into(),
            iterations: 5,
            fsync_grace_ms: 500,
            pre_estop_settle_ms: 3_000,
            restart_smoke_secs: 3,
            latch_dir: PathBuf::from("logs/crash_test"),
            mock_s1_joints: 1,
            mock_s1_frozen_joints: None,
        };
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            match a.as_str() {
                "--bin-dir" => {
                    i += 1;
                    args.bin_dir = PathBuf::from(raw.get(i).ok_or("--bin-dir requires value")?);
                }
                "--config" => {
                    i += 1;
                    args.config = PathBuf::from(raw.get(i).ok_or("--config requires value")?);
                }
                "--robot-id-prefix" => {
                    i += 1;
                    args.robot_id_prefix
                        .clone_from(raw.get(i).ok_or("--robot-id-prefix requires value")?);
                }
                "--hardware-id" => {
                    i += 1;
                    args.hardware_id
                        .clone_from(raw.get(i).ok_or("--hardware-id requires value")?);
                }
                "--iterations" => {
                    i += 1;
                    args.iterations = raw
                        .get(i)
                        .ok_or("--iterations requires value")?
                        .parse()
                        .map_err(|e| format!("--iterations: {e}"))?;
                }
                "--fsync-grace-ms" => {
                    i += 1;
                    args.fsync_grace_ms = raw
                        .get(i)
                        .ok_or("--fsync-grace-ms requires value")?
                        .parse()
                        .map_err(|e| format!("--fsync-grace-ms: {e}"))?;
                }
                "--pre-estop-settle-ms" => {
                    i += 1;
                    args.pre_estop_settle_ms = raw
                        .get(i)
                        .ok_or("--pre-estop-settle-ms requires value")?
                        .parse()
                        .map_err(|e| format!("--pre-estop-settle-ms: {e}"))?;
                }
                "--restart-smoke-secs" => {
                    i += 1;
                    args.restart_smoke_secs = raw
                        .get(i)
                        .ok_or("--restart-smoke-secs requires value")?
                        .parse()
                        .map_err(|e| format!("--restart-smoke-secs: {e}"))?;
                }
                "--latch-dir" => {
                    i += 1;
                    args.latch_dir = PathBuf::from(raw.get(i).ok_or("--latch-dir requires value")?);
                }
                "--joints" => {
                    i += 1;
                    args.mock_s1_joints = raw
                        .get(i)
                        .ok_or("--joints requires value")?
                        .parse()
                        .map_err(|e| format!("--joints: {e}"))?;
                }
                "--frozen-joints" => {
                    i += 1;
                    args.mock_s1_frozen_joints =
                        Some(raw.get(i).ok_or("--frozen-joints requires value")?.clone());
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
        "crash_restart_smoke (Step 9 EStop latch crash/restart 测试套)

USAGE:
  crash_restart_smoke [OPTIONS]

OPTIONS:
  --bin-dir <DIR>           target/release（默认）—— 找 s0_daemon / mock_s1 / robotctl
  --config <PATH>           daemon TOML 配置（默认 configs/crash_restart_smoke.toml；
                            mock_hard_rt 单关节，dev box 兼容；Orin 真机 bus2 9-motor
                            走 `--config configs/xlerobot_bus2_full.toml`）
  --robot-id-prefix <STR>   每次迭代的 robot_id 前缀（避 iceoryx2 collision）
  --hardware-id <ID>        匹配 config 的 hardware_id（默认 smoke_arm）
  --joints <N>              mock_s1 关节数（默认 1，跟默认 config 单 joint 配对；
                            Orin 真机 bus2 9-motor 走 `--joints 9`）
  --frozen-joints <CSV>     mock_s1 frozen joints mask（默认空；Orin bus2 走
                            `--frozen-joints 6,7,8` 防 base 3 wheel 抖动）
  --iterations <N>          迭代次数（默认 5）
  --fsync-grace-ms <MS>     estop 后睡多久才 SIGKILL（默认 500，> 实测 188-255 ms 窗口）
  --pre-estop-settle-ms <MS> daemon 启动后等多久才发 estop（默认 3000）
  --restart-smoke-secs <N>  重启 daemon 跑多久看 banner（默认 3）
  --latch-dir <DIR>         latch 文件目录（默认 logs/crash_test/）
  -h, --help                显示帮助

退出码: 0 = 全过；1 = CLI/env 错；2 = ≥ 1 次迭代 FAIL。

测试场景：kill_after_estop_fsync_window
  spawn daemon + mock_s1 → settle → robotctl estop → sleep grace → SIGKILL daemon
  → assert latch primary+bak = '1' on disk → restart daemon → assert message
  'initial estop_latched' + structured field latched=true (human + JSON
  tracing 格式都兼容) → cleanup → next iteration。
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

    if let Err(msg) = preflight(&args) {
        eprintln!("preflight FAIL: {msg}");
        std::process::exit(1);
    }

    let mut results: Vec<IterResult> = Vec::with_capacity(args.iterations as usize);
    for seq in 0..args.iterations {
        let robot_id = format!("{}_{}_{}", args.robot_id_prefix, std::process::id(), seq);
        eprintln!(
            "\n========== iteration {}/{} robot_id={} ==========",
            seq + 1,
            args.iterations,
            robot_id
        );
        let result = run_iteration(&args, seq, &robot_id);
        results.push(result);
    }

    eprintln!("\n========== summary ==========");
    let mut pass_count = 0;
    let mut fail_count = 0;
    for r in &results {
        let tag = if r.pass { "PASS" } else { "FAIL" };
        eprintln!(
            "  iter {} robot_id={} : {} {}",
            r.seq,
            r.robot_id,
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
// preflight
// ============================================================================

fn preflight(args: &Args) -> Result<(), String> {
    let need = ["s0_daemon", "mock_s1", "robotctl"];
    for name in need {
        let p = args.bin_dir.join(name);
        if !p.exists() {
            return Err(format!(
                "binary not found: {}. 先 cargo build --release 一下",
                p.display()
            ));
        }
    }
    if !args.config.exists() {
        return Err(format!("config not found: {}", args.config.display()));
    }
    std::fs::create_dir_all(&args.latch_dir)
        .map_err(|e| format!("create latch_dir {}: {e}", args.latch_dir.display()))?;
    Ok(())
}

// ============================================================================
// 单次迭代
// ============================================================================

fn run_iteration(args: &Args, seq: u32, robot_id: &str) -> IterResult {
    let latch_path = args.latch_dir.join(format!("estop_latch_{robot_id}"));
    let latch_bak = args.latch_dir.join(format!("estop_latch_{robot_id}.bak"));

    // 清干净（前一次迭代可能留 latch；不同 robot_id 但放同目录）
    let _ = std::fs::remove_file(&latch_path);
    let _ = std::fs::remove_file(&latch_bak);

    // **A1-5**：撤 `--allow-empty-latch` opt-in 后，daemon 默认 strict
    // fail-closed 拒启动缺 latch；测试自己 provision `0\n` 到 primary +
    // bak（跟 `scripts/systemd/README.md` "首次部署" 推荐流程一致）。旧
    // 实现通过 `--allow-empty-latch` 跳过 provision，A1-5 后该 flag 不
    // 存在，必须显式预写。
    if let Err(e) = std::fs::write(&latch_path, "0\n") {
        return fail(seq, robot_id, format!("provision primary latch=0\\n: {e}"));
    }
    if let Err(e) = std::fs::write(&latch_bak, "0\n") {
        return fail(seq, robot_id, format!("provision bak latch=0\\n: {e}"));
    }

    // ---- 1) spawn daemon ----
    let mut daemon = match spawn_daemon(args, robot_id, &latch_path, /*smoke_secs=*/ 30) {
        Ok(c) => c,
        Err(e) => return fail(seq, robot_id, format!("spawn daemon: {e}")),
    };
    sleep_ms(500);

    // ---- 2) spawn mock_s1（amplitude 0.0，不摆动，只为驱动 state→Active）----
    let mut s1 = match spawn_mock_s1(args, robot_id, /*duration_secs=*/ 25) {
        Ok(c) => c,
        Err(e) => {
            kill_child(&mut daemon);
            return fail(seq, robot_id, format!("spawn mock_s1: {e}"));
        }
    };

    // ---- 3) settle 等 daemon 进 Enabled.Active ----
    sleep_ms(args.pre_estop_settle_ms);

    // ---- 4) trigger estop ----
    let estop_status = run_robotctl_estop(args, robot_id);
    if !estop_status {
        kill_child(&mut daemon);
        kill_child(&mut s1);
        return fail(seq, robot_id, "robotctl estop returned non-zero".into());
    }

    // ---- 5) sleep fsync grace ----
    sleep_ms(args.fsync_grace_ms);

    // ---- 6) SIGKILL daemon（**关键**：不给 graceful shutdown 机会）----
    let daemon_pid = daemon.id();
    if let Err(e) = sigkill(daemon_pid) {
        kill_child(&mut daemon);
        kill_child(&mut s1);
        return fail(
            seq,
            robot_id,
            format!("sigkill daemon pid={daemon_pid}: {e}"),
        );
    }
    let _ = daemon.wait(); // reap zombie
    kill_child(&mut s1); // clean up mock_s1 too

    // ---- 7) 读 latch primary+bak，断言都 = "1" ----
    let primary = std::fs::read_to_string(&latch_path).unwrap_or_default();
    let bak = std::fs::read_to_string(&latch_bak).unwrap_or_default();
    let primary_v = primary.trim();
    let bak_v = bak.trim();
    if primary_v != "1" || bak_v != "1" {
        return fail(
            seq,
            robot_id,
            format!(
                "latch fsync FAIL: primary={primary_v:?} bak={bak_v:?} (expected both '1') \
                 — 阻断 1 修复回归！"
            ),
        );
    }
    eprintln!("  ✓ post-SIGKILL latch primary='{primary_v}' bak='{bak_v}' (both = 1)");

    // ---- 8) 重启 daemon，捕获 stderr，按结构化字段断言 latched=true ----
    //
    // task #16 把 s0_daemon 的 `eprintln!("initial estop_latched = ...")` 迁
    // 到 `tracing::info!(latched=..., "initial estop_latched")`，输出格式变成：
    //   - human：`... initial estop_latched latched=true ...`
    //   - JSON ：`{"message":"initial estop_latched","latched":true,...}`
    // 旧 grep "initial estop_latched = true" 在两种格式下都失效。
    // 这里改成"按行匹配 message + 结构化字段"（兼容 human + JSON），由 reviewer
    // 在 commit 8e5a238 阻断后修。
    let restart_log = match run_daemon_briefly(args, robot_id, &latch_path, args.restart_smoke_secs)
    {
        Ok(s) => s,
        Err(e) => return fail(seq, robot_id, format!("restart daemon: {e}")),
    };
    if !log_contains_initial_latched_true(&restart_log) {
        return fail(
            seq,
            robot_id,
            format!(
                "restart daemon did NOT report 'initial estop_latched' with latched=true \
                 (checked human + JSON tracing format). log tail:\n{}",
                tail_lines(&restart_log, 15)
            ),
        );
    }
    eprintln!("  ✓ restart daemon: initial estop_latched (latched=true)");

    // ---- 9) cleanup ----
    let _ = std::fs::remove_file(&latch_path);
    let _ = std::fs::remove_file(&latch_bak);

    IterResult {
        seq,
        robot_id: robot_id.into(),
        pass: true,
        fail_reason: None,
    }
}

fn fail(seq: u32, robot_id: &str, reason: String) -> IterResult {
    eprintln!("  ✗ FAIL: {reason}");
    IterResult {
        seq,
        robot_id: robot_id.into(),
        pass: false,
        fail_reason: Some(reason),
    }
}

// ============================================================================
// 子进程辅助
// ============================================================================

fn spawn_daemon(
    args: &Args,
    robot_id: &str,
    latch_path: &Path,
    smoke_secs: u64,
) -> std::io::Result<Child> {
    Command::new(args.bin_dir.join("s0_daemon"))
        .arg("--robot-id")
        .arg(robot_id)
        .arg("--hardware-id")
        .arg(&args.hardware_id)
        .arg("--config")
        .arg(&args.config)
        .arg("--latch-path")
        .arg(latch_path)
        .arg("--tick-hz")
        .arg("50")
        .arg("--smoke-duration")
        .arg(format!("{smoke_secs}s"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn spawn_mock_s1(args: &Args, robot_id: &str, duration_secs: u64) -> std::io::Result<Child> {
    // 默认 dev box smoke：`--joints 1` 跟 configs/crash_restart_smoke.toml 单
    // joint smoke_arm 配对；amplitude=0.0 → 不摆动，只发 anchor bias 作为
    // Goal，加心跳推动 state→Enabled.Active。Orin 真机 bus2 9-motor 走
    // `--joints 9 --frozen-joints 6,7,8`（base 3 wheel 双重保险）。
    let mut cmd = Command::new(args.bin_dir.join("mock_s1"));
    cmd.arg("--robot-id")
        .arg(robot_id)
        .arg("--hardware-id")
        .arg(&args.hardware_id)
        .arg("--joints")
        .arg(args.mock_s1_joints.to_string())
        .arg("--amplitude")
        .arg("0.0")
        .arg("--frequency")
        .arg("0.25")
        .arg("--anchor-timeout-ms")
        .arg("1000");
    if let Some(frozen) = &args.mock_s1_frozen_joints {
        cmd.arg("--frozen-joints").arg(frozen);
    }
    cmd.arg("--duration")
        .arg(format!("{duration_secs}s"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn run_robotctl_estop(args: &Args, robot_id: &str) -> bool {
    Command::new(args.bin_dir.join("robotctl"))
        .arg("estop")
        .arg("--robot-id")
        .arg(robot_id)
        .arg("--reason")
        .arg("crash_restart_smoke")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// 重启 daemon 短暂跑 `secs`，捕获 stdout + stderr 合并返回。要求 daemon
/// 在这段时间内**不**重新 fsync 改 latch（latch 已是 true，daemon 启动期
/// 读到 true 不会回写）。
///
/// **关键 env 注入**（reviewer 在 commit 8e5a238 后阻断验证发现）：
/// - `tracing-subscriber` 默认走 **stdout** + 即使 piped 也带 ANSI 转义
///   码，`latched=true` 实际字节是 `latched\x1b[0m\x1b[2m=\x1b[0mtrue`，
///   substring `latched=true` 命中失败。
/// - 注入 `ROBOT_LOG_FORMAT=json` 让 child daemon 输出 JSON Lines（无
///   ANSI、固定字段），跟 [`log_contains_initial_latched_true`] 的
///   `"latched":true` 分支命中。
/// - `NO_COLOR=1` 双保险：万一 `ROBOT_LOG_FORMAT` 路径意外退化到 fmt 模式，
///   ANSI 也被强制关闭，`latched=true` 字串干净可 grep。
fn run_daemon_briefly(
    args: &Args,
    robot_id: &str,
    latch_path: &Path,
    secs: u64,
) -> Result<String, String> {
    let out = Command::new(args.bin_dir.join("s0_daemon"))
        .arg("--robot-id")
        .arg(robot_id)
        .arg("--hardware-id")
        .arg(&args.hardware_id)
        .arg("--config")
        .arg(&args.config)
        .arg("--latch-path")
        .arg(latch_path)
        .arg("--tick-hz")
        .arg("50")
        .arg("--smoke-duration")
        .arg(format!("{secs}s"))
        .env("ROBOT_LOG_FORMAT", "json")
        .env("NO_COLOR", "1")
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn restart daemon: {e}"))?;
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(combined)
}

fn sigkill(pid: u32) -> std::io::Result<()> {
    // 直接走 kill -9，最严格的"无信号 handler 机会"故障模型。
    // pid 在 Linux 上是 i32（pid_t），但 std::process::Child::id() 返 u32 是
    // 跨平台兼容（Windows pid 也是 u32）。Linux 真实 pid 上限 <= PID_MAX_LIMIT
    // (4194304)，cast 不会 wrap。但 cast_signed() 仍可读 + 显式语义。
    let pid_signed = libc::pid_t::try_from(pid)
        .map_err(|e| std::io::Error::other(format!("pid {pid} doesn't fit in pid_t: {e}")))?;
    let r = unsafe { libc::kill(pid_signed, libc::SIGKILL) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn sleep_ms(ms: u64) {
    thread::sleep(Duration::from_millis(ms));
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[allow(dead_code)]
fn elapsed_str(start: Instant) -> String {
    format!("{:?}", start.elapsed())
}

/// 跨 human / JSON tracing 格式断言"daemon 重启读到 `initial estop_latched =
/// true`"。
///
/// task #16 (commit 8e5a238) 把 `s0_daemon` 启动 banner 从
/// `eprintln!("initial estop_latched = {latched}")` 迁到
/// `tracing::info!(latched = ..., "initial estop_latched")`，输出格式：
///
/// - human：`... initial estop_latched latched=true ...`
/// - JSON ：`{"message":"initial estop_latched","latched":true,...}`
///
/// 旧 grep `"initial estop_latched = true"` 在两种格式下都不匹配（reviewer
/// 复验 grep 为空 = Step 9 工具失效）。本函数按"同一行同时含 message 和
/// `latched=true|"latched":true`"判断，把语义和格式解耦：
/// - human 形式：`message + space + latched=true`
/// - JSON 形式：`"message":"initial estop_latched"` + `"latched":true`
fn log_contains_initial_latched_true(log: &str) -> bool {
    log.lines().any(|line| {
        line.contains("initial estop_latched")
            && (line.contains("latched=true") || line.contains("\"latched\":true"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_human_tracing_format() {
        let line =
            "2026-04-26T13:00:00Z [+0.001s]  INFO s0_daemon: initial estop_latched latched=true";
        assert!(log_contains_initial_latched_true(line));
    }

    #[test]
    fn matches_json_tracing_format() {
        let line = r#"{"timestamp":"2026-04-26T13:00:00Z [+0.001s]","level":"INFO","message":"initial estop_latched","latched":true,"target":"s0_daemon"}"#;
        assert!(log_contains_initial_latched_true(line));
    }

    #[test]
    fn does_not_match_latched_false_human() {
        let line = "2026-04-26T13:00:00Z  INFO s0_daemon: initial estop_latched latched=false";
        assert!(!log_contains_initial_latched_true(line));
    }

    #[test]
    fn does_not_match_latched_false_json() {
        let line = r#"{"message":"initial estop_latched","latched":false}"#;
        assert!(!log_contains_initial_latched_true(line));
    }

    /// **回归锁**：旧 grep `"initial estop_latched = true"` 字串在 task #16
    /// 迁移后**不**会出现在 daemon 输出里。本测试钉住"按结构化字段判断"
    /// 行为正确，禁止有人退化回 `contains("initial estop_latched = true")`。
    #[test]
    fn old_pre_tracing_string_does_not_appear_in_real_daemon_output() {
        // 模拟一个 daemon 启动期 stderr buffer，**不**含旧字串
        // "initial estop_latched = true"，但有新 tracing 结构化输出。
        let real_like = "2026-04-26T13:00:00Z  INFO s0_daemon: s0_daemon starting robot_id=foo\n\
            2026-04-26T13:00:00Z  INFO s0_daemon: initial estop_latched latched=true\n\
            2026-04-26T13:00:00Z  INFO s0_daemon: heartbeat state=Safe";
        assert!(!real_like.contains("initial estop_latched = true"));
        assert!(log_contains_initial_latched_true(real_like));
    }

    #[test]
    fn missing_field_does_not_match() {
        // message 在但缺 latched=true → 不匹配
        let line = "2026-04-26  INFO s0_daemon: initial estop_latched";
        assert!(!log_contains_initial_latched_true(line));
    }

    #[test]
    fn cross_line_does_not_match() {
        // message 和 latched=true 在不同行 → 不应当匹配（防误报）
        let log = "INFO s0_daemon: initial estop_latched\nINFO s0_daemon: heartbeat latched=true";
        assert!(!log_contains_initial_latched_true(log));
    }
}
