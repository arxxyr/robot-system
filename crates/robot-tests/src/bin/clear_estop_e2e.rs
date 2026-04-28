//! `clear_estop_e2e` —— v0.3 A1-3 (3/3) ClearEStop 端到端 smoke binary。
//!
//! ## 角色
//!
//! 起两进程（`s0_daemon` + `robot_rpc_gateway`）+ 用 tonic gRPC client 真跑
//! `RobotControl::ClearEStop` RPC → iceoryx2 RR shm → daemon ipc_thread →
//! clear_estop_action → latch fsync 双写。
//!
//! 跟 `crash_restart_smoke` 同款 `--bin-dir` 风格：CI / 人工显式跑，**不**塞
//! 进 `cargo test --workspace`（两进程 + iceoryx2 shm + loopback 端口属于
//! smoke/e2e，不该默认在 cargo test 里跑）。
//!
//! ## A1-3 (3/3-a) scope（本 commit）
//!
//! - robot-rpc 加 `client` feature 让 build.rs 跑
//!   `tonic_build::build_client(true)` 生成 `RobotControlClient` stub。
//! - 本 binary：harness 骨架 + 进程辅助 + role table / tempdir / loopback
//!   port 分配。
//! - 唯一可跑 case = `selftest`：bring-up → daemon ready (扫 stderr/stdout
//!   `rr_server_thread started`) → gateway ready (TCP poll loopback) →
//!   tonic client connect → ClearEStop dry_run=true maintainer 轮询到 Ok →
//!   tear-down (SIGTERM gateway → SIGTERM daemon → SIGKILL fallback)。
//!
//! ## A1-3 (3/3-b) scope（下一 commit）
//!
//! 加 4 case：`happy_dry_run` / `happy_real` / `reject_viewer` /
//! `idem_replay`。reviewer 拍板 case 集；reject_state 留 follow-up。
//!
//! ## 前置驱动方案（reviewer 拍板）
//!
//! 选"预置 latch=true + mock_hard_rt + dry_run readiness poll"，**不**走
//! mock_s1 / test-only daemon flag / 仅 dry_run：
//!
//! 1. 每 case 独立 latch 文件预置 `1\n`（primary + .bak），daemon 启动后
//!    state 留 Safe。
//! 2. daemon `--config configs/clear_estop_e2e.toml` 走 mock_hard_rt
//!    adapter；rt_thread connect 后把 safety_state 置 TorqueOffConfirmed。
//! 3. harness 用 maintainer fake token + dry_run=true ClearEStop 轮询到 Ok
//!    作为 readiness gate（最贴近真实链路的 ready 信号）。
//! 4. 真路径 (`happy_real`)：dry_run=false maintainer 调一次 → daemon
//!    `save_estop_latch_clear` backup-first 把 primary + .bak 都写
//!    `0\n` → handler 返 Ack(cleared_at_ns)。
//!
//! ## CLI
//!
//! ```text
//! clear_estop_e2e --bin-dir <DIR> [--config <PATH>] [--workdir <DIR>]
//!                 [--case <NAME>] [-h]
//! ```
//!
//! ## 退出码
//!
//! - `0`：所有 case PASS。
//! - `1`：CLI / preflight / 子进程启动错。
//! - `2`：≥ 1 case FAIL。

#![cfg(target_os = "linux")]
// Smoke binary：复杂的进程协调 + tempdir 路径，doc 块大量 identifier-like
// 字串（ClearEStopCommand / RobotControl / `Iceoryx2RrClient` 等），跟其它
// smoke binary（`crash_restart_smoke` / `freeze_systemd_smoke`）一致放宽
// pedantic。
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{AuthContext, ClearEStopCommand, CommandMeta};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tonic::transport::Endpoint;

// ============================================================================
// CLI
// ============================================================================

#[derive(Debug, Clone)]
struct Args {
    /// 找 `s0_daemon` / `robot_rpc_gateway` 二进制的目录。CI / 人工自定义；
    /// 默认 `target/release`。
    bin_dir: PathBuf,
    /// daemon `--config` TOML。默认 `configs/clear_estop_e2e.toml`。
    config: PathBuf,
    /// 临时工作目录根。默认 `/tmp/clear_estop_e2e_<pid>/`；harness 在内
    /// 部按 case 序号建子目录（`case_<seq>/`）。
    workdir: PathBuf,
    /// 跑哪个 case。(3/3-a) 仅 `selftest`；(3/3-b) 加 happy/idem/reject
    /// 系列 + `all`。
    case: String,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut bin_dir = PathBuf::from("target/release");
        let mut config = PathBuf::from("configs/clear_estop_e2e.toml");
        let mut workdir = PathBuf::from(format!("/tmp/clear_estop_e2e_{}", std::process::id()));
        let mut case = "selftest".to_string();

        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            match raw[i].as_str() {
                "--bin-dir" => {
                    i += 1;
                    bin_dir = PathBuf::from(raw.get(i).ok_or("--bin-dir requires value")?);
                }
                "--config" => {
                    i += 1;
                    config = PathBuf::from(raw.get(i).ok_or("--config requires value")?);
                }
                "--workdir" => {
                    i += 1;
                    workdir = PathBuf::from(raw.get(i).ok_or("--workdir requires value")?);
                }
                "--case" => {
                    i += 1;
                    case.clone_from(raw.get(i).ok_or("--case requires value")?);
                }
                "-h" | "--help" => return Err("HELP".into()),
                other => return Err(format!("unknown arg: {other}")),
            }
            i += 1;
        }
        Ok(Self {
            bin_dir,
            config,
            workdir,
            case,
        })
    }
}

fn print_help() {
    eprintln!(
        "clear_estop_e2e —— v0.3 A1-3 (3/3) ClearEStop control plane 端到端 smoke

USAGE:
  clear_estop_e2e [OPTIONS]

OPTIONS:
  --bin-dir <DIR>     找 s0_daemon / robot_rpc_gateway（默认 target/release）
  --config <PATH>     daemon TOML 配置（默认 configs/clear_estop_e2e.toml）
  --workdir <DIR>     临时工作根目录（默认 /tmp/clear_estop_e2e_<pid>）
  --case <NAME>       跑哪个 case（A1-3 (3/3-a)：仅 selftest）
  -h, --help          显示帮助

可用 case（A1-3 (3/3-a) 阶段）:
  selftest    bring-up + ready gate + tear-down，**不**做真路径

A1-3 (3/3-b) 之后会加：happy_dry_run / happy_real / reject_viewer /
                       idem_replay / all

退出码: 0 全过；1 CLI/preflight 错；2 ≥ 1 case FAIL。
"
    );
}

// ============================================================================
// main
// ============================================================================

fn main() -> std::process::ExitCode {
    let _ = robot_logging::init();

    let args = match Args::parse() {
        Ok(a) => a,
        Err(msg) if msg == "HELP" => {
            print_help();
            return std::process::ExitCode::from(0);
        }
        Err(msg) => {
            eprintln!("error: {msg}");
            print_help();
            return std::process::ExitCode::from(1);
        }
    };

    if let Err(msg) = preflight(&args) {
        eprintln!("preflight FAIL: {msg}");
        return std::process::ExitCode::from(1);
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tokio runtime build FAIL: {e}");
            return std::process::ExitCode::from(1);
        }
    };

    let case_name = args.case.clone();
    let result = runtime.block_on(async move {
        match args.case.as_str() {
            "selftest" => run_selftest(&args).await,
            other => Err(format!(
                "unknown --case {other:?} (A1-3 (3/3-a) 阶段仅 selftest；3/3-b 后加 happy/idem/reject 系列)"
            )),
        }
    });

    match result {
        Ok(()) => {
            eprintln!("[clear_estop_e2e] PASS: case={case_name}");
            std::process::ExitCode::from(0)
        }
        Err(msg) => {
            eprintln!("[clear_estop_e2e] FAIL: case={case_name} reason={msg}");
            std::process::ExitCode::from(2)
        }
    }
}

// ============================================================================
// preflight
// ============================================================================

fn preflight(args: &Args) -> Result<(), String> {
    let need = ["s0_daemon", "robot_rpc_gateway"];
    for name in need {
        let p = args.bin_dir.join(name);
        if !p.exists() {
            return Err(format!(
                "binary not found: {}. 先 cargo build --release -p robot-s0 -p robot-rpc",
                p.display()
            ));
        }
    }
    if !args.config.exists() {
        return Err(format!("config not found: {}", args.config.display()));
    }
    std::fs::create_dir_all(&args.workdir)
        .map_err(|e| format!("mkdir workdir {}: {e}", args.workdir.display()))?;
    Ok(())
}

// ============================================================================
// CaseCtx：单 case 的 robot_id / tempdir / latch 路径 / role table 路径 /
// loopback port 分配
// ============================================================================

/// 单 case 的运行期上下文。每 case 独立 `robot_id` + tempdir + role table
/// 文件 + latch 文件 + loopback port，避并发 case 串扰 + iceoryx2 service
/// 命名冲突。
#[derive(Debug, Clone)]
struct CaseCtx {
    /// 每 case 全局唯一：`clear_estop_e2e_<pid>_<seq>`。同时作为 daemon
    /// `--robot-id` + iceoryx2 RR topic 前缀（`/<robot_id>/control/clear_estop`）。
    robot_id: String,
    /// `<workdir>/<robot_id>/`，case 内一切文件都放这里。
    case_dir: PathBuf,
    /// `<case_dir>/var/lib/robot/<robot_id>/estop_latch`（primary）。
    latch_primary: PathBuf,
    /// `<case_dir>/var/lib/robot/<robot_id>/estop_latch.bak`。
    latch_bak: PathBuf,
    /// `<case_dir>/rpc_roles.toml`：harness 写一份最小 role 表（含
    /// maintainer + viewer + admin，跟 configs/rpc_roles.toml 同步）。
    role_table_path: PathBuf,
    /// gateway loopback 端口。allocate_loopback_port() 取 OS 自动分配；race
    /// 窗口 < 100ms 通常够（dev box / CI）。
    gateway_port: u16,
    /// daemon `--config` TOML 路径（共享 case 间，复用 `args.config`）。
    config: PathBuf,
    /// daemon / gateway binary 目录（共享 case 间，复用 `args.bin_dir`）。
    bin_dir: PathBuf,
}

/// 全局 case 序列计数（避同一 binary 多 case 跑时 robot_id 撞）。
static CASE_SEQ: AtomicU32 = AtomicU32::new(0);

impl CaseCtx {
    fn alloc(args: &Args) -> Result<Self, String> {
        let seq = CASE_SEQ.fetch_add(1, Ordering::Relaxed);
        let robot_id = format!("clear_estop_e2e_{}_{}", std::process::id(), seq);
        let case_dir = args.workdir.join(&robot_id);
        let latch_dir = case_dir.join("var/lib/robot").join(&robot_id);
        std::fs::create_dir_all(&latch_dir)
            .map_err(|e| format!("mkdir latch_dir {}: {e}", latch_dir.display()))?;
        let latch_primary = latch_dir.join("estop_latch");
        let latch_bak = latch_dir.join("estop_latch.bak");
        let role_table_path = case_dir.join("rpc_roles.toml");
        write_role_table(&role_table_path)?;
        let gateway_port = allocate_loopback_port()?;
        Ok(Self {
            robot_id,
            case_dir,
            latch_primary,
            latch_bak,
            role_table_path,
            gateway_port,
            config: args.config.clone(),
            bin_dir: args.bin_dir.clone(),
        })
    }

    /// 预置 latch=`1\n`（primary + .bak），daemon 启动后 state 留 Safe。
    fn preset_latch_latched(&self) -> Result<(), String> {
        std::fs::write(&self.latch_primary, "1\n")
            .map_err(|e| format!("write latch primary: {e}"))?;
        std::fs::write(&self.latch_bak, "1\n").map_err(|e| format!("write latch bak: {e}"))?;
        Ok(())
    }

    /// 读 latch primary + .bak 当前内容（`trim()` 过空白）。返
    /// `(primary_or_empty, bak_or_empty)`。**(3/3-b) 4 case 使用**；
    /// (3/3-a) selftest 只验 bring-up 不读 latch，allow dead_code。
    #[allow(dead_code)]
    fn read_latch_pair(&self) -> (String, String) {
        let p = std::fs::read_to_string(&self.latch_primary)
            .unwrap_or_default()
            .trim()
            .to_string();
        let b = std::fs::read_to_string(&self.latch_bak)
            .unwrap_or_default()
            .trim()
            .to_string();
        (p, b)
    }

    /// 回收 case_dir。harness 收尾或 case PASS 后调；FAIL 路径不调，留
    /// 文件给人工 forensic。
    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.case_dir);
    }
}

/// 写最小 rpc_roles.toml：含 maintainer / viewer / admin / operator 4 角
/// 色 + 4 fake token，跟 `configs/rpc_roles.toml` 兼容（**仅** maintainer /
/// admin 拥有 `clear_estop`）。
fn write_role_table(path: &Path) -> Result<(), String> {
    let toml_text = r#"# clear_estop_e2e 临时 rpc_roles.toml（每 case 独立）。
# 跟 configs/rpc_roles.toml 同款 capability 集，**仅** maintainer / admin
# 拥有 `clear_estop`。

[tokens]
"viewer-fake-token-001"     = "viewer"
"operator-fake-token-001"   = "operator"
"maintainer-fake-token-001" = "maintainer"
"admin-fake-token-001"      = "admin"

[roles.viewer]
description  = "smoke viewer"
capabilities = ["read_state"]

[roles.operator]
description  = "smoke operator"
capabilities = ["read_state", "set_enabled", "clear_error", "estop"]

[roles.maintainer]
description  = "smoke maintainer (clear_estop)"
capabilities = ["read_state", "clear_estop", "set_enabled", "clear_error", "estop", "reload_calibration"]

[roles.admin]
description  = "smoke admin (clear_estop + shutdown)"
capabilities = ["read_state", "clear_estop", "set_enabled", "clear_error", "estop", "reload_calibration", "set_policy", "shutdown"]
"#;
    std::fs::write(path, toml_text)
        .map_err(|e| format!("write rpc_roles.toml {}: {e}", path.display()))
}

/// 用 `TcpListener::bind("127.0.0.1:0")` 让 OS 自动分配一个 loopback 端口
/// → `local_addr()` 拿真实端口 → drop listener 释放端口给 gateway 用。
///
/// race 窗口（drop ↔ gateway bind）< 100ms 通常够；dev box / CI 实际碰撞概
/// 率极低。如未来反复撞端口可换"gateway bind 失败重试"或在 bin 端加 ready
/// 信号 print。
fn allocate_loopback_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("bind 127.0.0.1:0 to alloc port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("local_addr after alloc: {e}"))?
        .port();
    drop(listener);
    Ok(port)
}

// ============================================================================
// Subprocess: daemon + gateway
// ============================================================================

/// 子进程 + stderr/stdout drain task + 捕获日志行 buffer（按行匹配 ready
/// marker 用）。
///
/// `Drop` 时**不**自动 kill；harness 必须显式调
/// [`SubprocessHandle::shutdown`] 走 SIGTERM → wait → SIGKILL fallback 流
/// 程，避免进程泄漏跨 case。
struct SubprocessHandle {
    label: &'static str,
    child: Child,
    /// 两个 streams 合并行（带 stream tag prefix）；ready marker 扫一份就够。
    captured: Arc<Mutex<Vec<String>>>,
}

impl SubprocessHandle {
    /// spawn 子进程 + 起 stderr/stdout drain task。stdout / stderr 都
    /// `Stdio::piped()` 让 harness 捕获 ready marker；不 drain 的话 child
    /// pipe buffer 满会让 child write 阻塞。
    fn spawn(label: &'static str, mut command: Command) -> std::io::Result<Self> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        // env：固定 ROBOT_LOG_FORMAT=json + NO_COLOR=1 让日志格式确定（按
        // 行匹配可靠），不靠 ANSI 彩色 trick。
        command.env("ROBOT_LOG_FORMAT", "json").env("NO_COLOR", "1");
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let captured = Arc::new(Mutex::new(Vec::<String>::with_capacity(512)));
        spawn_stream_drain(label, "stdout", stdout, Arc::clone(&captured));
        spawn_stream_drain(label, "stderr", stderr, Arc::clone(&captured));

        Ok(Self {
            label,
            child,
            captured,
        })
    }

    fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// 等 captured 行里出现 `needle`，最长等 `timeout`。`needle` 走 substring
    /// 匹配（不解析 JSON 字段）。
    async fn wait_for_marker(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let lines = self.captured.lock().await;
                if lines.iter().any(|l| l.contains(needle)) {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// 干净 shutdown：先 SIGTERM，等 `term_grace`；超时 SIGKILL（不再等）。
    /// 返 child exit status（如果 wait 成功）+ 最近 N 行日志（FAIL 时可贴
    /// summary）。
    async fn shutdown(
        mut self,
        term_grace: Duration,
    ) -> (Option<std::process::ExitStatus>, Vec<String>) {
        let last_log = {
            let lines = self.captured.lock().await;
            let n = lines.len();
            let tail_n = n.saturating_sub(20);
            lines[tail_n..].to_vec()
        };
        if let Some(pid) = self.child.id() {
            // SAFETY: pid 来自 child.id()；libc::kill 接 i32 PID。
            let pid_i32 = i32::try_from(pid).unwrap_or(0);
            unsafe { libc::kill(pid_i32, libc::SIGTERM) };
        }
        let exit = match tokio::time::timeout(term_grace, self.child.wait()).await {
            Ok(Ok(s)) => Some(s),
            Ok(Err(e)) => {
                eprintln!("[{}] wait after SIGTERM err: {e}", self.label);
                None
            }
            Err(_) => {
                eprintln!(
                    "[{}] SIGTERM grace {} ms expired; SIGKILL",
                    self.label,
                    term_grace.as_millis()
                );
                let _ = self.child.kill().await;
                self.child.wait().await.ok()
            }
        };
        (exit, last_log)
    }
}

/// 起一个 task 把 child stdout/stderr 流式 drain 到共享 buffer，同时同步
/// 转发到 harness 自己的 stderr（带 `[label/stream]` 前缀方便 debug）。
fn spawn_stream_drain(
    label: &'static str,
    stream: &'static str,
    handle: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    captured: Arc<Mutex<Vec<String>>>,
) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(handle).lines();
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    eprintln!("[{label}/{stream}] {line}");
                    let mut lines = captured.lock().await;
                    lines.push(line);
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("[{label}/{stream}] read err: {e}");
                    break;
                }
            }
        }
    });
}

/// spawn s0_daemon。latch 文件**必须**已经预置（harness 调
/// `CaseCtx::preset_latch_latched`）；daemon 启动期 strict mode 拒绝缺
/// latch。
fn spawn_daemon(ctx: &CaseCtx) -> std::io::Result<SubprocessHandle> {
    let bin = ctx.bin_dir.join("s0_daemon");
    let mut cmd = Command::new(&bin);
    cmd.arg("--robot-id")
        .arg(&ctx.robot_id)
        .arg("--hardware-id")
        .arg("smoke_arm")
        .arg("--config")
        .arg(&ctx.config)
        .arg("--latch-path")
        .arg(&ctx.latch_primary)
        .arg("--tick-hz")
        .arg("50")
        .arg("--smoke-duration")
        .arg("60s");
    SubprocessHandle::spawn("daemon", cmd)
}

/// spawn robot_rpc_gateway。
fn spawn_gateway(ctx: &CaseCtx) -> std::io::Result<SubprocessHandle> {
    let bin = ctx.bin_dir.join("robot_rpc_gateway");
    let mut cmd = Command::new(&bin);
    cmd.arg("--robot-id")
        .arg(&ctx.robot_id)
        .arg("--rpc-roles-toml")
        .arg(&ctx.role_table_path)
        .arg("--listen-addr")
        .arg(format!("127.0.0.1:{}", ctx.gateway_port))
        .arg("--idempotency-ttl-secs")
        .arg("300")
        .arg("--idempotency-max-entries")
        .arg("4096")
        .arg("--rr-timeout-grace-ms")
        .arg("500");
    SubprocessHandle::spawn("gateway", cmd)
}

// ============================================================================
// Readiness：daemon RR server / gateway TCP / ClearEStop dry_run gate
// ============================================================================

/// 等 daemon 把 `rr_server_thread started` 写出来（reviewer 拍板：daemon
/// ready 不等 `state=Ready` 日志，等 RR server 起来即可）。
async fn wait_for_daemon_rr_server_started(
    daemon: &SubprocessHandle,
    timeout: Duration,
) -> Result<(), String> {
    if daemon
        .wait_for_marker("rr_server_thread started", timeout)
        .await
    {
        Ok(())
    } else {
        Err(format!(
            "daemon did not log `rr_server_thread started` within {} ms",
            timeout.as_millis()
        ))
    }
}

/// 等 gateway loopback 端口可 `connect`（gateway binary 没有 ready_tx 对
/// 外，靠 TCP poll 替代）。
async fn wait_for_gateway_tcp_ready(port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let addr = format!("127.0.0.1:{port}");
    loop {
        if let Ok(s) = tokio::net::TcpStream::connect(&addr).await {
            drop(s);
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "gateway TCP {addr} not connectable within {} ms",
                timeout.as_millis()
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 用 maintainer fake token + dry_run=true ClearEStop 轮询到 Ok 作为
/// readiness gate（reviewer 拍板：最贴近真实链路的 ready 信号；`safety_state
/// == TorqueOffConfirmed` 是 dry_run 路径的硬前置，daemon 真到 ready 即
/// 第一次 dry_run 返 Ok）。
///
/// 每次 attempt 用新 UUIDv7 `idempotency_key` 让请求都走 FreshInsert + 真
/// daemon evaluate（避免 Replay 走 cached Ack 误判 ready）。
async fn poll_clear_estop_dry_run_ready(
    client: &mut RobotControlClient<tonic::transport::Channel>,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let req = build_cmd(&BuildCmdArgs {
            token: "maintainer-fake-token-001",
            idempotency_key: &format!("ready_attempt_{attempt}"),
            audit_reason: "clear_estop_e2e selftest readiness probe",
            dry_run: true,
        });
        match client.clear_e_stop(req).await {
            Ok(_resp) => return Ok(()),
            Err(status) => {
                let err_msg = format!(
                    "attempt={attempt} {:?}: {}",
                    status.code(),
                    status.message()
                );
                if Instant::now() >= deadline {
                    return Err(format!(
                        "ClearEStop dry_run readiness gate timed out after {} ms; last err: {err_msg}",
                        timeout.as_millis(),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// 构造 `ClearEStopCommand` 给 tonic client 用。（A1-3 (3/3-b) 4 case 共
/// 用同一份 helper；3/3-a selftest 也用。）
struct BuildCmdArgs<'a> {
    token: &'a str,
    idempotency_key: &'a str,
    audit_reason: &'a str,
    dry_run: bool,
}

fn build_cmd(args: &BuildCmdArgs<'_>) -> tonic::Request<ClearEStopCommand> {
    // request_id 必须 UUIDv7（auth::validate_request_id 拒 v4）。手动构造
    // 简化版：48-bit unix_ts_ms + 4-bit version=7 + 12-bit rand + 64-bit
    // rand。避免拉 uuid crate 进 dev workspace（已经在 robot-rpc 里有但
    // robot-tests 还没加）。
    let request_id = uuidv7_compat();
    let cmd = ClearEStopCommand {
        meta: Some(CommandMeta {
            request_id,
            source: "clear_estop_e2e".into(),
            auth: Some(AuthContext {
                token: args.token.into(),
                mtls_fingerprint: String::new(),
            }),
            timeout_ms: 5_000,
            idempotency_key: args.idempotency_key.into(),
            dry_run: args.dry_run,
            audit_reason: args.audit_reason.into(),
        }),
        confirmation: "smoke-confirm-001".into(),
    };
    tonic::Request::new(cmd)
}

/// 手写最小 UUIDv7 兼容字串（`SystemTime::UNIX_EPOCH` 取 unix_ts_ms +
/// nanosecond seed 当 randomness）。robot-rpc::auth::validate_request_id
/// 只 check `get_version_num() == 7`，不真做密码学随机性校验。
///
/// **不**走 `uuid` crate workspace dep：robot-tests 还没 link 它，加进
/// 来又得登记 dep 白名单；smoke 单文件场景手写更轻。
fn uuidv7_compat() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("SystemTime before UNIX_EPOCH");
    let unix_ts_ms = u64::try_from(now.as_millis()).unwrap_or(u64::MAX);
    let nanos = now.subsec_nanos();
    // 48-bit unix_ts_ms + 4-bit ver=7 + 12-bit rand_a + 2-bit var=10 + 62-bit rand_b
    let mut bytes = [0u8; 16];
    bytes[0..6].copy_from_slice(&unix_ts_ms.to_be_bytes()[2..8]);
    // version 7 in high nibble of byte 6; lower nibble = rand_a (high 4 bits)
    let rand_a = (nanos & 0x0FFF) as u16;
    bytes[6] = 0x70 | ((rand_a >> 8) as u8 & 0x0F);
    bytes[7] = (rand_a & 0xFF) as u8;
    // variant 10 in top 2 bits of byte 8; remaining 62 bits = rand_b
    let rand_b: u64 = u64::from(nanos)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(unix_ts_ms);
    bytes[8] = 0x80 | ((rand_b >> 56) as u8 & 0x3F);
    bytes[9..16].copy_from_slice(&rand_b.to_be_bytes()[1..8]);
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    )
}

// ============================================================================
// Tonic client connect helper
// ============================================================================

async fn connect_gateway_client(
    port: u16,
    timeout: Duration,
) -> Result<RobotControlClient<tonic::transport::Channel>, String> {
    let url = format!("http://127.0.0.1:{port}");
    let endpoint = Endpoint::try_from(url.clone())
        .map_err(|e| format!("Endpoint::try_from {url}: {e}"))?
        .connect_timeout(timeout)
        .timeout(timeout);
    let channel = endpoint
        .connect()
        .await
        .map_err(|e| format!("tonic connect {url}: {e}"))?;
    Ok(RobotControlClient::new(channel))
}

// ============================================================================
// Selftest case
// ============================================================================

/// (3/3-a) 唯一 case：bring-up → daemon ready → gateway ready → tonic
/// client connect → ClearEStop dry_run readiness gate → tear-down。
///
/// **不**断言真路径；不读 latch 内容。仅锁住 harness 骨架可工作 +
/// 子进程协调路径稳定，给 (3/3-b) 4 个真 case 打地基。
async fn run_selftest(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    eprintln!(
        "[selftest] robot_id={} workdir={} gateway_port={}",
        ctx.robot_id,
        ctx.case_dir.display(),
        ctx.gateway_port
    );
    ctx.preset_latch_latched()?;
    eprintln!("[selftest] preset latch primary+bak = `1\\n`");

    // ---- bring up daemon ----
    let daemon = spawn_daemon(&ctx).map_err(|e| format!("spawn daemon: {e}"))?;
    eprintln!(
        "[selftest] daemon spawned pid={}",
        daemon.pid().unwrap_or(0)
    );

    let bring_up_res = bring_up_and_probe(&ctx, &daemon).await;

    // ---- tear down 顺序：gateway 先（如果 spawn 成功了），daemon 后 ----
    match bring_up_res {
        Ok(gateway) => {
            eprintln!("[selftest] PASS readiness probe; tearing down");
            shutdown_pair(gateway, daemon).await;
            ctx.cleanup();
            Ok(())
        }
        Err((maybe_gateway, err_msg)) => {
            eprintln!("[selftest] FAIL bring-up: {err_msg}");
            if let Some(gw) = maybe_gateway {
                shutdown_pair(gw, daemon).await;
            } else {
                let _ = daemon.shutdown(Duration::from_secs(2)).await;
            }
            // FAIL 路径**不** cleanup workdir，留文件给人工 forensic
            Err(err_msg)
        }
    }
}

/// daemon 已起；等 daemon RR server ready → spawn gateway → 等 gateway
/// TCP ready → tonic connect → poll dry_run readiness gate。
///
/// 失败时返 `Err((Option<gateway_handle>, msg))`：caller 用 gateway_handle
/// 走 shutdown 路径不漏进程。
#[allow(clippy::type_complexity)]
async fn bring_up_and_probe(
    ctx: &CaseCtx,
    daemon: &SubprocessHandle,
) -> Result<SubprocessHandle, (Option<SubprocessHandle>, String)> {
    // daemon RR server ready
    if let Err(e) = wait_for_daemon_rr_server_started(daemon, Duration::from_secs(5)).await {
        return Err((None, e));
    }
    eprintln!("[selftest] daemon RR server started");

    // spawn gateway
    let gateway = match spawn_gateway(ctx) {
        Ok(g) => g,
        Err(e) => return Err((None, format!("spawn gateway: {e}"))),
    };
    eprintln!(
        "[selftest] gateway spawned pid={}",
        gateway.pid().unwrap_or(0)
    );

    // gateway TCP ready
    if let Err(e) = wait_for_gateway_tcp_ready(ctx.gateway_port, Duration::from_secs(5)).await {
        return Err((Some(gateway), e));
    }
    eprintln!(
        "[selftest] gateway TCP ready on 127.0.0.1:{}",
        ctx.gateway_port
    );

    // tonic client connect
    let mut client = match connect_gateway_client(ctx.gateway_port, Duration::from_secs(2)).await {
        Ok(c) => c,
        Err(e) => return Err((Some(gateway), e)),
    };
    eprintln!("[selftest] tonic client connected");

    // ClearEStop dry_run=true maintainer readiness gate
    if let Err(e) = poll_clear_estop_dry_run_ready(&mut client, Duration::from_secs(5)).await {
        return Err((Some(gateway), e));
    }
    eprintln!("[selftest] ClearEStop dry_run readiness gate PASS");

    Ok(gateway)
}

/// shutdown 顺序（reviewer 拍板）：先 SIGTERM gateway，再 SIGTERM daemon；
/// 各自 grace 超时 SIGKILL；都 reap zombie。
async fn shutdown_pair(gateway: SubprocessHandle, daemon: SubprocessHandle) {
    let (gw_exit, gw_tail) = gateway.shutdown(Duration::from_secs(3)).await;
    eprintln!(
        "[selftest] gateway exited {:?}; tail {} lines",
        gw_exit,
        gw_tail.len()
    );
    let (dm_exit, dm_tail) = daemon.shutdown(Duration::from_secs(3)).await;
    eprintln!(
        "[selftest] daemon exited {:?}; tail {} lines",
        dm_exit,
        dm_tail.len()
    );
}
