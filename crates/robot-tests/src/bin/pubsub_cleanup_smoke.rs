//! `pubsub_cleanup_smoke` —— v0.4 候选 3 (1/2) per-ActionId pubsub
//! cleanup grace 实测 smoke binary **骨架**。
//!
//! ## (1/2) scope（本 commit）
//!
//! 只 land binary 骨架 + log scrape：
//!
//! 1. CLI parsing（沿 [`crate::clear_estop_e2e`] 风格手写，**不**引 clap）
//! 2. `CaseCtx` + `CASE_SEQ` 全局唯一 `robot_id`（避 iceoryx2 service 撞）
//! 3. Inline `FakeStatePublisher`（沿 `crates/robot-rpc/tests/common/fake_state.rs`
//!    模式 —— 50Hz JointState publish + `SystemClock` 填 `stamp_ns` +
//!    leak Node）；Plan v3 §P.2.1.c 拍板 binary 路径自包含、不改公共 API
//! 4. `SubprocessHandle` + spawn s1_daemon / robot_rpc_gateway 双进程；
//!    `Stdio::piped()` 2 路 drain 到共享 `Arc<TokioMutex<Vec<String>>>`；
//!    `kill_on_drop(true)` RAII 兜底；显式 SIGTERM grace → SIGKILL fallback
//!    shutdown 路径
//! 5. 三段式 ready signal（Plan v3 §P.2.1）：
//!    - Phase 1: s1_daemon stderr marker `"rr_server_threads ready"`
//!    - Phase 2: gateway TCP probe `127.0.0.1:<port>`（gateway binary
//!      没有 stderr ready marker；TCP `connect` 成功代理）
//!    - Phase 3: gateway tonic `GetTaskStatus` 探活，action_id = zero
//!      UUID 期 `tonic::Code::NotFound`（端到端 round-trip 通了）
//! 6. `selftest` case：跑完 3 段 ready + 干净 shutdown（gateway → s1
//!    → fake_s0），**不**调任何 InvokeTask / StreamTaskEvents 真实测
//!    （留 (2/2)）
//! 7. forensic dump（FAIL 路径）：失败时 `dump_captured_to` 把子进程
//!    stdout + stderr 写到 `<workdir>/<robot_id>/{label}.log`
//!
//! ## (2/2) scope（下一 commit）
//!
//! - `--phase A1`：长跑 1000 task 顺序 + 等终态 + 0ms；每 100 task 一档
//!   5 类 metric 曲线（`vmrss_s1_kb` / `vmrss_gw_kb` /
//!   `events_registry_len` / `iceoryx2_storage_count` /
//!   `gw_subscribe_threads`）
//! - `--phase A2`：100 task + 测 client 视角端到端延迟 = `client_receive_ns
//!   minus event.at_ns`（**必须**用
//!   `robot_core::time::SystemClock::new().now().0`，反向时间戳显式 fail，
//!   详 Plan v3 §P.2.2.b）；p50/p95/p99 报告
//! - `--phase A3`：SIGKILL 三子 case (a/b/c) + 重启后 N=10 InvokeTask
//!   探活
//! - `--phase all`：A1 + A2 + A3 全跑
//!
//! `Phase::A1` / `A2` / `A3` / `All` 在 (1/2) 命中返
//! `ExitCode::from(1)`（"not implemented in (1/2); see Plan v3 §P.2.2"），
//! 让外部 user 显式知道 (2/2) 还没 land。
//!
//! ## CLI
//!
//! ```text
//! pubsub_cleanup_smoke [OPTIONS]
//!
//! options:
//!   --bin-dir <DIR>                  默认 target/release
//!   --robot-id-prefix <STR>          默认 pubsub_cleanup
//!   --hardware-id <ID>               必填（跟 --config TOML 的 [[hardware]].id 一致）
//!   --config <TOML>                  必填（s1_daemon --config）
//!   --joints <N>                     默认 1（跟 config [[hardware]].joints 一致）
//!   --layout-id <U64>                默认 0xCAFEBABEDEADBEEF（跟 task_rpc_e2e 同款）
//!   --iterations <N>                 (2/2) 用；默认 1000（Q1 拍板）
//!   --metric-sample-every <K>        (2/2) 用；默认 100
//!   --invoke-dwell-ms <MS>           (2/2) 用；默认 10
//!   --phase selftest|A1|A2|A3|all    默认 selftest（(1/2) 唯一支持的 phase）
//!   --leftover-grace-ms <MS>         (2/2) AC-A3 用；默认 200
//!   --output-csv <PATH>              (2/2) 用；默认 stderr-only
//!   --workdir <DIR>                  默认 /tmp/pubsub_cleanup_smoke
//!   -h | --help                      打印帮助
//! ```
//!
//! ## 退出码（沿 `crash_restart_smoke` / `clear_estop_e2e` 同款）
//!
//! - `0`：selftest PASS（或 (2/2) 加 phase 全过 + 零条件性 AC 触发）
//! - `1`：CLI / preflight / 子进程 spawn 错（含 (1/2) 收 `--phase A1|A2|A3|all`）
//! - `2`：≥1 case FAIL（含 (2/2) AC FAIL + 任一条件性 AC 触发）
//!
//! ## 跟 Spec / Plan 的链接
//!
//! - [`team-swe/specs/v0_4_pubsub_cleanup.md`](../../../../../team-swe/specs/v0_4_pubsub_cleanup.md)
//!   §AC + §Plan §P.2.1 / §P.2.2
//! - [`docs/handoffs/v0.4_entry.md`](../../../../../docs/handoffs/v0.4_entry.md)
//!   §4 当前切入点 + §5 6 条 hard boundaries

#![cfg(target_os = "linux")]
// Smoke binary：复杂的进程协调 + tempdir 路径，doc 块大量 identifier-like
// 字串（s1_daemon / robot_rpc_gateway / iceoryx2 / TaskHandleRef 等），跟
// 其它 smoke binary（`crash_restart_smoke` / `clear_estop_e2e`）一致放宽
// pedantic。
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::{self, JoinHandle as StdJoinHandle};
use std::time::Duration;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::build_helpers::build_joint_state;
use robot_msg::markers;
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{
    ActionKindProto, AuthContext, CommandMeta, InvokeTaskCommand, TaskEventKind, TaskHandleRef,
};
use robot_transport::Publisher;
use robot_transport::iceoryx2::{TopicClass, publisher as open_pub};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex as TokioMutex;
use tokio::time::Instant;
use tonic::transport::Endpoint;

// ============================================================================
// CLI
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Selftest,
    A1,
    A2,
    A3,
    All,
}

impl Phase {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "selftest" => Ok(Phase::Selftest),
            "A1" | "a1" => Ok(Phase::A1),
            "A2" | "a2" => Ok(Phase::A2),
            "A3" | "a3" => Ok(Phase::A3),
            "all" => Ok(Phase::All),
            other => Err(format!(
                "unknown --phase {other:?} (expected selftest|A1|A2|A3|all)"
            )),
        }
    }
}

#[derive(Debug, Clone)]
struct Args {
    bin_dir: PathBuf,
    robot_id_prefix: String,
    hardware_id: String,
    config: PathBuf,
    joints: usize,
    layout_id: u64,
    /// (2/2) AC-A1 用；(1/2) 不消费。Plan v3 Q1 拍板 N=1000。
    #[allow(dead_code)]
    iterations: u32,
    /// (2/2) AC-A1 用。
    #[allow(dead_code)]
    metric_sample_every: u32,
    /// (2/2) 各 phase 用。
    #[allow(dead_code)]
    invoke_dwell_ms: u64,
    phase: Phase,
    /// (2/2) AC-A3 用。
    #[allow(dead_code)]
    leftover_grace_ms: u64,
    /// (2/2) 用。
    #[allow(dead_code)]
    output_csv: Option<PathBuf>,
    workdir: PathBuf,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut bin_dir = PathBuf::from("target/release");
        let mut robot_id_prefix = "pubsub_cleanup".to_string();
        let mut hardware_id: Option<String> = None;
        let mut config: Option<PathBuf> = None;
        let mut joints: usize = 1;
        let mut layout_id: u64 = 0xCAFE_BABE_DEAD_BEEF;
        let mut iterations: u32 = 1000;
        let mut metric_sample_every: u32 = 100;
        let mut invoke_dwell_ms: u64 = 10;
        let mut phase = Phase::Selftest;
        let mut leftover_grace_ms: u64 = 200;
        let mut output_csv: Option<PathBuf> = None;
        // 默认 forensic 路径：Plan v3 §P.2.2.d / §P.2.6 拍板
        // `/tmp/pubsub_cleanup_smoke/<robot_id>/`；robot_id 已含
        // `<prefix>_<pid>_<seq>`，不需要 workdir 再带 pid 后缀。
        let mut workdir = PathBuf::from("/tmp/pubsub_cleanup_smoke");

        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            match raw[i].as_str() {
                "--bin-dir" => {
                    i += 1;
                    bin_dir = PathBuf::from(raw.get(i).ok_or("--bin-dir requires value")?);
                }
                "--robot-id-prefix" => {
                    i += 1;
                    robot_id_prefix
                        .clone_from(raw.get(i).ok_or("--robot-id-prefix requires value")?);
                }
                "--hardware-id" => {
                    i += 1;
                    hardware_id = Some(raw.get(i).ok_or("--hardware-id requires value")?.clone());
                }
                "--config" => {
                    i += 1;
                    config = Some(PathBuf::from(raw.get(i).ok_or("--config requires value")?));
                }
                "--joints" => {
                    i += 1;
                    joints = raw
                        .get(i)
                        .ok_or("--joints requires value")?
                        .parse::<usize>()
                        .map_err(|e| format!("--joints parse: {e}"))?;
                }
                "--layout-id" => {
                    i += 1;
                    let v = raw.get(i).ok_or("--layout-id requires value")?;
                    layout_id = parse_layout_id(v)?;
                }
                "--iterations" => {
                    i += 1;
                    iterations = raw
                        .get(i)
                        .ok_or("--iterations requires value")?
                        .parse::<u32>()
                        .map_err(|e| format!("--iterations parse: {e}"))?;
                }
                "--metric-sample-every" => {
                    i += 1;
                    metric_sample_every = raw
                        .get(i)
                        .ok_or("--metric-sample-every requires value")?
                        .parse::<u32>()
                        .map_err(|e| format!("--metric-sample-every parse: {e}"))?;
                }
                "--invoke-dwell-ms" => {
                    i += 1;
                    invoke_dwell_ms = raw
                        .get(i)
                        .ok_or("--invoke-dwell-ms requires value")?
                        .parse::<u64>()
                        .map_err(|e| format!("--invoke-dwell-ms parse: {e}"))?;
                }
                "--phase" => {
                    i += 1;
                    phase = Phase::parse(raw.get(i).ok_or("--phase requires value")?)?;
                }
                "--leftover-grace-ms" => {
                    i += 1;
                    leftover_grace_ms = raw
                        .get(i)
                        .ok_or("--leftover-grace-ms requires value")?
                        .parse::<u64>()
                        .map_err(|e| format!("--leftover-grace-ms parse: {e}"))?;
                }
                "--output-csv" => {
                    i += 1;
                    output_csv = Some(PathBuf::from(
                        raw.get(i).ok_or("--output-csv requires value")?,
                    ));
                }
                "--workdir" => {
                    i += 1;
                    workdir = PathBuf::from(raw.get(i).ok_or("--workdir requires value")?);
                }
                "-h" | "--help" => return Err("HELP".into()),
                other => return Err(format!("unknown arg: {other}")),
            }
            i += 1;
        }

        let hardware_id = hardware_id.ok_or("--hardware-id is required")?;
        let config = config.ok_or("--config is required")?;

        Ok(Self {
            bin_dir,
            robot_id_prefix,
            hardware_id,
            config,
            joints,
            layout_id,
            iterations,
            metric_sample_every,
            invoke_dwell_ms,
            phase,
            leftover_grace_ms,
            output_csv,
            workdir,
        })
    }
}

fn parse_layout_id(s: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).map_err(|e| format!("--layout-id hex parse: {e}"))
    } else {
        trimmed
            .parse::<u64>()
            .map_err(|e| format!("--layout-id dec parse: {e}"))
    }
}

fn print_help() {
    eprintln!(
        "pubsub_cleanup_smoke —— v0.4 候选 3 (1/2) pubsub cleanup smoke binary 骨架

USAGE:
    pubsub_cleanup_smoke [OPTIONS]

OPTIONS (required):
    --hardware-id <ID>               跟 --config [[hardware]].id 一致
    --config <TOML>                  s1_daemon --config 路径

OPTIONS (defaulted):
    --bin-dir <DIR>                  默认 target/release
    --robot-id-prefix <STR>          默认 pubsub_cleanup
    --joints <N>                     默认 1
    --layout-id <U64|0xHEX>          默认 0xCAFEBABEDEADBEEF
    --workdir <DIR>                  默认 /tmp/pubsub_cleanup_smoke
    --phase selftest|A1|A2|A3|all    默认 selftest（(1/2) 唯一支持）
    --iterations <N>                 (2/2) AC-A1 用；默认 1000
    --metric-sample-every <K>        (2/2) AC-A1 用；默认 100
    --invoke-dwell-ms <MS>           (2/2) 用；默认 10
    --leftover-grace-ms <MS>         (2/2) AC-A3 用；默认 200
    --output-csv <PATH>              (2/2) 用；默认 stderr-only
    -h | --help                      打印本帮助

EXIT CODES:
    0   selftest PASS
    1   CLI / preflight / 子进程 spawn 错 (含 --phase A1|A2|A3|all in (1/2))
    2   ≥1 case FAIL (含 (2/2) AC FAIL)"
    );
}

// ============================================================================
// main
// ============================================================================

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> ExitCode {
    let args = match Args::parse() {
        Ok(a) => a,
        Err(s) if s == "HELP" => {
            print_help();
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("CLI error: {e}\n");
            print_help();
            return ExitCode::from(1);
        }
    };

    if let Err(e) = preflight(&args) {
        eprintln!("preflight failed: {e}");
        return ExitCode::from(1);
    }

    match args.phase {
        Phase::Selftest => match run_selftest(&args).await {
            Ok(()) => {
                eprintln!("[main] selftest PASS");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("[main] selftest FAIL: {e}");
                ExitCode::from(2)
            }
        },
        Phase::A1 | Phase::A2 | Phase::A3 | Phase::All => {
            eprintln!(
                "phase {:?} not implemented in v0.4 候选 3 (1/2); see Plan v3 §P.2.2 \
                 (team-swe/specs/v0_4_pubsub_cleanup.md)",
                args.phase
            );
            ExitCode::from(1)
        }
    }
}

fn preflight(args: &Args) -> Result<(), String> {
    let s1 = args.bin_dir.join("s1_daemon");
    if !s1.exists() {
        return Err(format!(
            "s1_daemon binary not found at {} (build via \
             `cargo build --release -p robot-s1 --bin s1_daemon`)",
            s1.display()
        ));
    }
    let gw = args.bin_dir.join("robot_rpc_gateway");
    if !gw.exists() {
        return Err(format!(
            "robot_rpc_gateway binary not found at {} (build via \
             `cargo build --release -p robot-rpc --features client \
             --bin robot_rpc_gateway`)",
            gw.display()
        ));
    }
    if !args.config.exists() {
        return Err(format!(
            "config TOML not found at {}",
            args.config.display()
        ));
    }
    if args.joints == 0 {
        return Err("--joints must be >= 1".into());
    }
    std::fs::create_dir_all(&args.workdir)
        .map_err(|e| format!("mkdir --workdir {}: {e}", args.workdir.display()))?;
    Ok(())
}

// ============================================================================
// CaseCtx
// ============================================================================

/// 全局 case 序列计数（避同一 binary 内多 case 跑时 robot_id 撞）。
static CASE_SEQ: AtomicU32 = AtomicU32::new(0);

/// 单 case 的运行期上下文。每 case 独立 `robot_id` + tempdir + role table
/// + loopback port，避并发 case 串扰 + iceoryx2 service 命名冲突。
#[derive(Debug, Clone)]
struct CaseCtx {
    /// 每 case 全局唯一：`<robot_id_prefix>_<pid>_<seq>`。同时作为 s1_daemon
    /// `--robot-id` + iceoryx2 RR topic 前缀
    /// （`/<robot_id>/control/{task_invoke,task_cancel,task_status,clear_error}`
    /// + `/<robot_id>/s1/action_events/<action_id>`）。
    robot_id: String,
    /// 跟 `--config` `[[hardware]].id` 一致。
    hardware_id: String,
    /// 同 fake S0 publish JointState 的关节数（必须跟 `--config`
    /// `[[hardware]].joints` 长度一致）。
    joints: usize,
    /// 同 s1 anchor schema 一致的 `layout_id`（默认 `0xCAFEBABEDEADBEEF`
    /// 跟 `crates/robot-rpc/tests/common/case_ctx.rs::LAYOUT_ID` 等价）。
    layout_id: u64,
    /// daemon `--config` TOML 路径。
    config: PathBuf,
    /// daemon / gateway binary 目录。
    bin_dir: PathBuf,
    /// `<workdir>/<robot_id>/`，case 内一切 forensic 文件都放这里。
    case_dir: PathBuf,
    /// `<case_dir>/rpc_roles.toml`：harness 写一份最小 role 表（含 4
    /// fake token + maintainer/operator/viewer/admin 4 role），跟
    /// `configs/rpc_roles.toml` 兼容。
    role_table_path: PathBuf,
    /// gateway loopback 端口。`allocate_loopback_port` 取 OS 自动分配。
    gateway_port: u16,
}

impl CaseCtx {
    fn alloc(args: &Args) -> Result<Self, String> {
        let seq = CASE_SEQ.fetch_add(1, Ordering::Relaxed);
        let robot_id = format!("{}_{}_{}", args.robot_id_prefix, std::process::id(), seq);
        let case_dir = args.workdir.join(&robot_id);
        std::fs::create_dir_all(&case_dir)
            .map_err(|e| format!("mkdir case_dir {}: {e}", case_dir.display()))?;
        let role_table_path = case_dir.join("rpc_roles.toml");
        write_role_table(&role_table_path)?;
        let gateway_port = allocate_loopback_port()?;
        Ok(Self {
            robot_id,
            hardware_id: args.hardware_id.clone(),
            joints: args.joints,
            layout_id: args.layout_id,
            config: args.config.clone(),
            bin_dir: args.bin_dir.clone(),
            case_dir,
            role_table_path,
            gateway_port,
        })
    }

    /// 回收 case_dir。PASS 路径调；FAIL 路径**不**调（留 forensic dump
    /// 给人工排查）。
    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.case_dir);
    }
}

/// 写最小 rpc_roles.toml：4 fake token + 4 role，跟 `configs/rpc_roles.toml`
/// 兼容（B4 (2/3-b) 收紧后 `[roles.operator]` **不**持 `clear_error`）。
fn write_role_table(path: &Path) -> Result<(), String> {
    let toml_text = r#"# pubsub_cleanup_smoke 临时 rpc_roles.toml（每 case 独立）。
# 跟 configs/rpc_roles.toml 同款 capability 集（B4 (2/3-b) 收紧后
# `[roles.operator]` 不持 clear_error；maintainer/admin 保留）。

[tokens]
"viewer-fake-token-001"     = "viewer"
"operator-fake-token-001"   = "operator"
"maintainer-fake-token-001" = "maintainer"
"admin-fake-token-001"      = "admin"

[roles.viewer]
description  = "smoke viewer"
capabilities = ["read_state"]

[roles.operator]
description  = "smoke operator (no clear_error/clear_estop)"
capabilities = ["read_state", "invoke_task", "set_enabled", "estop"]

[roles.maintainer]
description  = "smoke maintainer"
capabilities = ["read_state", "invoke_task", "clear_estop", "set_enabled", "clear_error", "estop", "reload_calibration"]

[roles.admin]
description  = "smoke admin"
capabilities = ["read_state", "invoke_task", "clear_estop", "set_enabled", "clear_error", "estop", "reload_calibration", "set_policy", "shutdown"]
"#;
    std::fs::write(path, toml_text)
        .map_err(|e| format!("write rpc_roles.toml {}: {e}", path.display()))
}

/// 用 `TcpListener::bind("127.0.0.1:0")` 让 OS 自动分配 loopback 端口。
/// drop listener 让端口空出来给 gateway bind；race 窗口 < 100ms 通常够
/// （dev box / CI）。
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
// Inline FakeStatePublisher（沿 crates/robot-rpc/tests/common/fake_state.rs 模式）
// ============================================================================

const PUBLISH_INTERVAL: Duration = Duration::from_millis(20); // 50Hz
const FAKE_S0_SOURCE_ID: u32 = 1; // s0 约定 source_id = 1（s1 = 2）

/// 测试侧扮演 s0 角色 publish `JointState` 给 `/<robot_id>/s0/<hardware_id>/state`，
/// 让 s1_daemon `anchor_or_fail` 能在 5s timeout 内拿到 stable bias 完成
/// anchor。
///
/// 沿 `crates/robot-rpc/tests/common/fake_state.rs::FakeStatePublisher`
/// 同款模式（Plan v3 §P.2.1.c 拍板"binary 路径自包含 inline 等价实现，
/// **不**改 fake_state.rs 公共可见性"）。
struct FakeStatePublisher {
    handle: Option<StdJoinHandle<()>>,
    stop: Arc<AtomicBool>,
    #[allow(dead_code)]
    topic: String,
}

impl FakeStatePublisher {
    fn spawn(
        robot_id: &str,
        hardware_id: &str,
        joints: usize,
        layout_id: u64,
    ) -> std::io::Result<Self> {
        let topic = format!("/{robot_id}/s0/{hardware_id}/state");
        let topic_owned = topic.clone();
        let topic_for_thread = topic.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let positions = vec![0.0_f64; joints];

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

        let handle = thread::Builder::new()
            .name(format!("pcs_fake_state_{robot_id}"))
            .spawn(move || {
                let node = NodeBuilder::new()
                    .create::<IxService>()
                    .expect("FakeStatePublisher: NodeBuilder create");
                let mut publisher = match open_pub::<markers::JointState>(
                    &node,
                    &topic_for_thread,
                    TopicClass::S0State,
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        // factory 失败：drop ready_tx 让 spawn() 见 Err。
                        eprintln!("[fake_s0] open_pub failed for topic {topic_for_thread}: {e:?}");
                        drop(ready_tx);
                        return;
                    }
                };
                // node 持 publisher 内部 shm fd；leak 让 publisher 整生命
                // 周期内 node 不被 drop（同 daemon / events_drain / rr_io
                // leak 模式；详 `team-swe/codemap/v0_4_pubsub_cleanup.md`
                // §3.6）。
                Box::leak(Box::new(node));
                let _ = ready_tx.send(());
                drop(ready_tx);

                // 用 SystemClock 拿真 CLOCK_MONOTONIC_RAW ns —— 跟 s1_daemon
                // `read_anchor_from_s0_state` 同款 clock domain（max_stale_ms
                // = 200ms）。
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
                    thread::sleep(PUBLISH_INTERVAL);
                }
            })?;

        if ready_rx.recv().is_err() {
            return Err(std::io::Error::other(format!(
                "FakeStatePublisher: factory panicked / iceoryx2 create \
                 failed for topic {topic_owned}"
            )));
        }

        Ok(Self {
            handle: Some(handle),
            stop,
            topic: topic_owned,
        })
    }

    fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for FakeStatePublisher {
    fn drop(&mut self) {
        // best-effort：若 shutdown() 已显式调，handle 是 None，跳过。
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ============================================================================
// SubprocessHandle
// ============================================================================

/// 子进程 + stderr/stdout drain task + 捕获日志行 buffer（按行匹配 ready
/// marker 用）。
///
/// `Drop` 走 `kill_on_drop(true)` 自动 SIGKILL（test panic / harness
/// early return 路径兜底）；正常 PASS 路径必须显式调
/// [`SubprocessHandle::shutdown`] 走 SIGTERM grace → SIGKILL fallback，
/// 拿到 ExitStatus 才算干净退。
struct SubprocessHandle {
    label: &'static str,
    pid: Option<u32>,
    /// `Option` 让 `shutdown()` 能 `take()` child 拿 owned wait。
    child: Option<Child>,
    /// stdout + stderr 合并行；`wait_for_marker` 扫一份就够。
    captured: Arc<TokioMutex<Vec<String>>>,
}

impl SubprocessHandle {
    fn spawn(label: &'static str, mut command: Command) -> std::io::Result<Self> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        // ROBOT_LOG_FORMAT=json + NO_COLOR=1 + RUST_LOG=info：让日志
        // 格式确定（按行 substring 匹配可靠），不靠 ANSI 彩色 trick；
        // RUST_LOG=info 让 s1_daemon `rr_server_threads ready` info!
        // marker 必出现。
        command
            .env("ROBOT_LOG_FORMAT", "json")
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "info");
        // RAII 兜底：harness panic / early return 时自动 SIGKILL（沿 B4
        // (3/3) `tests/common/subprocess.rs` 同款）。
        command.kill_on_drop(true);

        let mut child = command.spawn()?;
        let pid = child.id();
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let captured = Arc::new(TokioMutex::new(Vec::<String>::with_capacity(512)));
        spawn_stream_drain(label, "stdout", stdout, Arc::clone(&captured));
        spawn_stream_drain(label, "stderr", stderr, Arc::clone(&captured));

        Ok(Self {
            label,
            pid,
            child: Some(child),
            captured,
        })
    }

    #[allow(dead_code)]
    fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// 等 captured 行里出现 `needle`，最长等 `timeout`。`needle` 走 substring
    /// 匹配（不解析 JSON 字段）。
    async fn wait_for_marker(&mut self, needle: &str, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let label = self.label;
        loop {
            {
                let lines = self.captured.lock().await;
                if lines.iter().any(|l| l.contains(needle)) {
                    return Ok(());
                }
            }
            // 检查子进程是否已退出（早夭路径）。
            if let Some(child) = self.child.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                return Err(format!(
                    "{label}: subprocess exited before marker `{needle}` (status={status:?})"
                ));
            }
            if Instant::now() >= deadline {
                let n = self.captured.lock().await.len();
                return Err(format!(
                    "{label}: marker `{needle}` not found within {}ms (captured_lines={n})",
                    timeout.as_millis()
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// 干净 shutdown：SIGTERM → wait `grace` → 超时 SIGKILL → wait。
    async fn shutdown(
        mut self,
        grace: Duration,
    ) -> Result<Option<std::process::ExitStatus>, String> {
        let label = self.label;
        let Some(mut child) = self.child.take() else {
            return Ok(None);
        };
        if let Some(pid) = self.pid {
            let pid_i32 = i32::try_from(pid).unwrap_or(0);
            // SAFETY: pid 来自 Child::id()；signal 是 async-signal-safe。
            unsafe {
                libc::kill(pid_i32, libc::SIGTERM);
            }
        }
        match tokio::time::timeout(grace, child.wait()).await {
            Ok(Ok(status)) => Ok(Some(status)),
            Ok(Err(e)) => Err(format!("{label}: wait err: {e}")),
            Err(_elapsed) => {
                eprintln!(
                    "[{label}] SIGTERM grace {}ms expired; SIGKILL fallback",
                    grace.as_millis()
                );
                let _ = child.kill().await;
                let s = child.wait().await.ok();
                Ok(s)
            }
        }
    }

    /// Forensic dump：把 captured 行写到 `<case_dir>/<label>.log`。失败
    /// 时调，留人工排查；PASS 路径**不**调（`CaseCtx::cleanup` 已删整
    /// 个 case_dir）。
    async fn dump_captured_to(&self, dir: &Path) -> std::io::Result<()> {
        let lines = self.captured.lock().await;
        let path = dir.join(format!("{}.log", self.label));
        std::fs::write(path, lines.join("\n"))
    }
}

fn spawn_stream_drain(
    label: &'static str,
    stream: &'static str,
    handle: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    captured: Arc<TokioMutex<Vec<String>>>,
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

// ============================================================================
// spawn s1_daemon + spawn gateway
// ============================================================================

fn spawn_s1_daemon(ctx: &CaseCtx) -> std::io::Result<SubprocessHandle> {
    let bin = ctx.bin_dir.join("s1_daemon");
    let mut cmd = Command::new(&bin);
    cmd.arg("--robot-id")
        .arg(&ctx.robot_id)
        .arg("--hardware-id")
        .arg(&ctx.hardware_id)
        .arg("--config")
        .arg(&ctx.config)
        .arg("--poll-interval-ms")
        .arg("1");
    SubprocessHandle::spawn("s1_daemon", cmd)
}

fn spawn_gateway(ctx: &CaseCtx) -> std::io::Result<SubprocessHandle> {
    let bin = ctx.bin_dir.join("robot_rpc_gateway");
    let mut cmd = Command::new(&bin);
    cmd.arg("--robot-id")
        .arg(&ctx.robot_id)
        .arg("--listen-addr")
        .arg(format!("127.0.0.1:{}", ctx.gateway_port))
        .arg("--rpc-roles-toml")
        .arg(&ctx.role_table_path)
        .arg("--idempotency-ttl-secs")
        .arg("300")
        .arg("--idempotency-max-entries")
        .arg("4096")
        .arg("--rr-timeout-grace-ms")
        .arg("500");
    SubprocessHandle::spawn("gateway", cmd)
}

// ============================================================================
// Readiness：s1 marker / gateway TCP / GetTaskStatus 探活
// ============================================================================

/// Phase 1：等 s1_daemon stderr 出现 `"rr_server_threads ready"` marker
/// （`crates/robot-s1/src/bin/s1_daemon.rs:453` info! 行）。
async fn wait_for_s1_marker(s1: &mut SubprocessHandle, timeout: Duration) -> Result<(), String> {
    s1.wait_for_marker("rr_server_threads ready", timeout).await
}

/// Phase 2：等 gateway loopback 端口可 `connect`（gateway binary 没有
/// stderr ready marker，靠 TCP probe 替代；同 `clear_estop_e2e` 模式）。
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
                "gateway TCP {addr} not connectable within {}ms",
                timeout.as_millis()
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Phase 3：用 GetTaskStatus + zero UUID action_id 探活。
///
/// 探活语义（沿 B4 (3/3) `tests/common/ready.rs::poll_rpc_ready` 同款）：
/// - tonic 路径通 = gateway 收到请求
/// - gateway forward 到 s1 RR → s1 status handler 查 `Coordinator::status`
///   返 `None` → fbs `ActionRpcStatus::NotFound`
/// - gateway 映射 → `tonic::Code::NotFound`
/// - 任何其它 status code（含 transport err）= 路径未通；继续轮询
///
/// 每 attempt 用新 UUIDv7 request_id 避免命中 cache（GetTaskStatus 是
/// 只读 RPC，**不**进 idempotency cache —— B4 §3.2 拍板；但用新
/// request_id 让 audit log 各 attempt 区分）。
async fn poll_get_task_status_ready(
    client: &mut RobotControlClient<tonic::transport::Channel>,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut attempt: u32 = 0;
    let unknown_action = "00000000-0000-7000-8000-000000000000";
    loop {
        attempt += 1;
        let req = tonic::Request::new(TaskHandleRef {
            request_id: uuidv7_compat(),
            action_id: unknown_action.to_string(),
        });
        match client.get_task_status(req).await {
            // NotFound = path 通了；s1 真返了 NotFound 说明 coordinator
            // 收到了请求 + 没找到 action_id。
            Err(status) if status.code() == tonic::Code::NotFound => return Ok(()),
            // 其它 Ok（理论不应有，zero UUID 永远 NotFound）：path 通；
            // 不该走但兼容。
            Ok(_) => return Ok(()),
            Err(status) => {
                let err = format!(
                    "attempt={attempt} {:?}: {}",
                    status.code(),
                    status.message()
                );
                if Instant::now() >= deadline {
                    return Err(format!(
                        "GetTaskStatus readiness gate timed out after {}ms; last err: {err}",
                        timeout.as_millis()
                    ));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

// ============================================================================
// Tonic client helpers
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

/// 手写最小 UUIDv7 兼容字串（沿 `clear_estop_e2e::uuidv7_compat` 同款）。
/// `robot-rpc::auth::validate_request_id` 只 check `get_version_num() == 7`，
/// 不真做密码学随机性校验。
fn uuidv7_compat() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("SystemTime before UNIX_EPOCH");
    let unix_ts_ms = u64::try_from(now.as_millis()).unwrap_or(u64::MAX);
    let nanos = now.subsec_nanos();
    // 48-bit unix_ts_ms + 4-bit ver=7 + 12-bit rand_a + 2-bit var=10 + 62-bit rand_b
    let mut bytes = [0u8; 16];
    bytes[0..6].copy_from_slice(&unix_ts_ms.to_be_bytes()[2..8]);
    let rand_a = (nanos & 0x0FFF) as u16;
    bytes[6] = 0x70 | ((rand_a >> 8) as u8 & 0x0F);
    bytes[7] = (rand_a & 0xFF) as u8;
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
// Metric helpers + Report struct (2/2-a)
// ============================================================================
//
// AC-A1/A2/A3 (2/2-b) 用；(2/2-a) 仅 land helper + struct，**无** caller
// → 全 `#[allow(dead_code)]`。

/// 读 `/proc/<pid>/status::VmRSS` 行返 KB 数值。
///
/// `VmRSS` 行格式：`VmRSS:\t12345 kB`。
#[allow(dead_code)]
fn read_proc_vmrss_kb(pid: u32) -> Result<u64, String> {
    let path = format!("/proc/{pid}/status");
    let content = std::fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let trimmed = rest.trim().trim_end_matches("kB").trim();
            return trimmed
                .parse::<u64>()
                .map_err(|e| format!("parse VmRSS value {trimmed:?}: {e}"));
        }
    }
    Err(format!("VmRSS not found in {path}"))
}

/// Glob `/dev/shm/iceoryx2_*` 文件 / 目录数。Plan v3 §P.3 #3 ACK 默认
/// `/dev/shm` + 实测 fallback；如 iceoryx2 0.8.1 真 storage path 不在
/// `/dev/shm` → (2/2-b) 实测期 reviewer round 复核调整。
#[allow(dead_code)]
fn count_iceoryx2_storage_files() -> Result<u64, String> {
    let dir = std::path::Path::new("/dev/shm");
    if !dir.exists() {
        return Ok(0);
    }
    let mut count: u64 = 0;
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read_dir /dev/shm: {e}"))?;
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str()
            && name.starts_with("iceoryx2_")
        {
            count += 1;
        }
    }
    Ok(count)
}

/// 读 `/proc/<pid>/task` 子目录数 = 进程线程数（含主线程）。
#[allow(dead_code)]
fn count_proc_threads(pid: u32) -> Result<u64, String> {
    let path = format!("/proc/{pid}/task");
    let entries = std::fs::read_dir(&path).map_err(|e| format!("read_dir {path}: {e}"))?;
    let count = u64::try_from(entries.count()).unwrap_or(u64::MAX);
    Ok(count)
}

/// 从 captured 日志行（s1_daemon stderr）倒序扫最近一条
/// `events_registry_len=N` 数值（兼容 JSON `"events_registry_len":N` +
/// human `events_registry_len=N` 两种 tracing format）。
///
/// `handle_invoke` register / s1_daemon shutdown_pre/post 时打这字段；
/// 倒序取最近值反映当前 in-flight registry 长度。captured 内一行都没
/// 匹配 → `None`。
#[allow(dead_code)]
async fn scrape_events_registry_len(captured: &Arc<TokioMutex<Vec<String>>>) -> Option<u64> {
    let lines = captured.lock().await;
    let needle = "events_registry_len";
    for line in lines.iter().rev() {
        if let Some(idx) = line.rfind(needle) {
            // 跳过 needle 之后所有非数字字符（`":` / `=` / 空格 / 引号）
            let mut pos = idx + needle.len();
            let bytes = line.as_bytes();
            while pos < bytes.len() && !bytes[pos].is_ascii_digit() {
                pos += 1;
            }
            let start = pos;
            while pos < bytes.len() && bytes[pos].is_ascii_digit() {
                pos += 1;
            }
            if start < pos
                && let Ok(s) = std::str::from_utf8(&bytes[start..pos])
                && let Ok(n) = s.parse::<u64>()
            {
                return Some(n);
            }
        }
    }
    None
}

/// (2/2) AC-A1 metric 行 —— 每 `--metric-sample-every K` task 一档 + final。
/// 5 类字段对应 Plan v3 §P.2.2.c 拍板曲线。
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct MetricRow {
    /// task iter 索引（0..N，N=`args.iterations`，default 1000）。
    iter: u32,
    /// s1_daemon `/proc/<pid>/status::VmRSS` KB。
    vmrss_s1_kb: u64,
    /// gateway `/proc/<pid>/status::VmRSS` KB。
    vmrss_gw_kb: u64,
    /// 从 s1_daemon stderr scrape 的最近一条 `events_registry_len`（如
    /// 当前还没打任一 register/shutdown event log → `None`，CSV 写
    /// `"NA"`）。
    events_registry_len: Option<u64>,
    /// `/dev/shm/iceoryx2_*` 文件 / 目录数。
    iceoryx2_storage_count: u64,
    /// gateway `/proc/<pid>/task` 数（含 tokio worker + subscribe thread
    /// 等所有线程）。
    gw_subscribe_threads: u64,
}

/// (2/2) AC-A1 report。
#[allow(dead_code)]
#[derive(Debug, Default)]
struct A1Report {
    /// 每 100 task + final 共 ~11 行。
    rows: Vec<MetricRow>,
    /// Plan v3 Q2 触发条件 #1：`events_registry_len` 在 N task + grace 期
    /// 后线性增长不收敛 → 触发 AC-C1。
    triggered_c1: bool,
    /// Plan v3 Q2 触发条件 #2：gateway subscribe thread count 线性增长
    /// 不收敛 → 触发 AC-C2。
    triggered_c2: bool,
}

/// (2/2) AC-A2 report —— terminal_grace 真延迟 + 终态成功率。
#[allow(dead_code)]
#[derive(Debug, Default)]
struct A2Report {
    /// `--iterations` task 拿到终态帧 → latency_ns 样本（client_receive_ns
    /// - event.at_ns；Plan v3 §P.2.2.b 拍板）。
    samples_ns: Vec<u64>,
    p50_ns: u64,
    p95_ns: u64,
    p99_ns: u64,
    /// 拿到终态帧的 task 数。
    success_count: u32,
    /// 预期总 task 数（=`args.iterations` for AC-A2，default 100）。
    expected_count: u32,
}

/// (2/2) AC-A3 单个 SIGKILL 子 case 结果（A3a/b/c 各一份）。
#[allow(dead_code)]
#[derive(Debug, Default)]
struct A3CaseResult {
    label: &'static str,
    /// SIGKILL 前 `/dev/shm/iceoryx2_*` 文件数（基线）。
    storage_before: u64,
    /// SIGKILL 后 + `--leftover-grace-ms` 之后文件数（看 OS 是否回收）。
    storage_after: u64,
    /// 重启 daemon / gateway 是否成功（ready signal 三段都过）。
    restart_ok: bool,
    /// 重启后 N=10 InvokeTask 探活成功数（Plan v3 Q5）。
    post_restart_invoke_ok: u32,
    /// 探活总数（恒 10）。
    post_restart_invoke_total: u32,
}

/// (2/2) AC-A3 report（3 子 case + 触发判定）。
#[allow(dead_code)]
#[derive(Debug, Default)]
struct A3Report {
    case_a: A3CaseResult,
    case_b: A3CaseResult,
    case_c: A3CaseResult,
    /// 任一子 case 重启被阻塞 / 新 task 失败 → 触发 AC-C3（Plan v3 Q2
    /// 触发条件 #3）。
    triggered_c3: bool,
}

/// 把 `A1Report.rows` 写到 CSV `path`，沿 `MetricRow` 字段顺序输出 header
/// + 每行。
///
/// `events_registry_len = None` 时写 `"NA"`（CSV 标准缺失值）。
#[allow(dead_code)]
fn write_csv(path: &Path, rows: &[MetricRow]) -> std::io::Result<()> {
    use std::fmt::Write as _;
    let mut out = String::new();
    out.push_str(
        "iter,vmrss_s1_kb,vmrss_gw_kb,events_registry_len,\
         iceoryx2_storage_count,gw_subscribe_threads\n",
    );
    for r in rows {
        let er = r
            .events_registry_len
            .map_or_else(|| "NA".to_string(), |v| v.to_string());
        // writeln! into String 不会真失败（String::write_fmt 永远
        // Ok）；用 writeln! 避开 clippy `format_push_string`。
        let _ = writeln!(
            &mut out,
            "{},{},{},{},{},{}",
            r.iter,
            r.vmrss_s1_kb,
            r.vmrss_gw_kb,
            er,
            r.iceoryx2_storage_count,
            r.gw_subscribe_threads,
        );
    }
    std::fs::write(path, out)
}

// ============================================================================
// Task RPC helpers (2/2-a)
// ============================================================================

/// 构造 `InvokeTaskCommand` for `DwellMs` action（沿
/// `crates/robot-rpc/tests/common/client.rs::build_invoke_task_command`
/// 同款；本地 `request_id` 接 `&str`（uuidv7_compat 返字串），不引
/// `uuid` crate）。
///
/// **`server-side DwellMs duration_ms` 由 s1_daemon `--config` 注入的
/// `MockDwellBackend::with_duration_ms` 决定**（B2 frozen lib API；详
/// `crates/robot-s1/src/action_backend/mock_dwell.rs:60-67`），client
/// 端**不**直接控制。`timeout_ms` 参数是 client `CommandMeta.timeout_ms`
/// = RPC 请求-响应超时上限（gateway 端 `tokio::time::timeout` 用），
/// **不**是 server-side dwell duration。
#[allow(dead_code)]
fn build_invoke_task_cmd(
    request_id: &str,
    idempotency_key: &str,
    source: &str,
    auth_token: &str,
    kind: ActionKindProto,
    timeout_ms: u64,
    audit_reason: &str,
) -> tonic::Request<InvokeTaskCommand> {
    let cmd = InvokeTaskCommand {
        meta: Some(CommandMeta {
            request_id: request_id.to_string(),
            source: source.to_string(),
            auth: Some(AuthContext {
                token: auth_token.to_string(),
                mtls_fingerprint: String::new(),
            }),
            timeout_ms,
            idempotency_key: idempotency_key.to_string(),
            dry_run: false,
            audit_reason: audit_reason.to_string(),
        }),
        kind: kind.into(),
        // `deadline_ns` = 0 让 gateway 端用 `clock.now() + timeout_ms*1e6`
        // 主导 deadline 计算（详 task_handlers.rs:206 注释 + B4 (3/3)
        // `tests/common/client.rs:88-95`）。
        deadline_ns: 0,
        priority: 0,
        payload_schema_id: 0,
        payload: Vec::new(),
    };
    tonic::Request::new(cmd)
}

/// 构造 `tonic::Request<TaskHandleRef>`（`GetTaskStatus` / `StreamTaskEvents`
/// 共用 wire 形态）。
#[allow(dead_code)]
fn build_task_handle_ref_req(request_id: &str, action_id: &str) -> tonic::Request<TaskHandleRef> {
    tonic::Request::new(TaskHandleRef {
        request_id: request_id.to_string(),
        action_id: action_id.to_string(),
    })
}

/// 单 task：调 `InvokeTask` → 拿 `action_id` → `StreamTaskEvents` 等终
/// 态帧 → 返 `(action_id, terminal_kind, latency_ns)`。
///
/// **`latency_ns` 时钟域**（Plan v3 §P.2.2.b 写死硬约束）：
/// - `client_receive_ns` = `robot_core::time::SystemClock::new().now().0`
///   （`CLOCK_MONOTONIC_RAW`；跟 s1 端 `event.at_ns` 同 clock domain）
/// - 反向 (`client_receive_ns < event.at_ns`) → 显式 fail
///   (`clock_domain_violation`)，**不**用 `saturating_sub` 掩盖时钟域
///   假设破裂
///
/// smoke binary 跟 s1_daemon / gateway 必须**同机器**跑（跨机器跨 clock
/// domain 不在候选 3 范围）。
///
/// 调用方典型：AC-A1 长跑（丢 `latency_ns`，关心 throughput / leak）+
/// AC-A2 grace 实测（收集 `latency_ns` 算 p50/p95/p99）。
#[allow(dead_code)]
async fn invoke_and_wait_terminal(
    client: &mut RobotControlClient<tonic::transport::Channel>,
    auth_token: &str,
    rpc_timeout_ms: u64,
    stream_timeout: Duration,
) -> Result<(String, TaskEventKind, u64), String> {
    let request_id = uuidv7_compat();
    let ik = format!("ik-{request_id}");
    let invoke_req = build_invoke_task_cmd(
        &request_id,
        &ik,
        "pubsub_cleanup_smoke",
        auth_token,
        ActionKindProto::ActionKindDwellMs,
        rpc_timeout_ms,
        "pubsub_cleanup_smoke task",
    );
    let resp = client
        .invoke_task(invoke_req)
        .await
        .map_err(|s| format!("invoke_task: {:?}: {}", s.code(), s.message()))?
        .into_inner();
    let action_id = resp.action_id;
    if action_id.is_empty() {
        return Err("invoke_task returned empty action_id".into());
    }

    let stream_req = build_task_handle_ref_req(&request_id, &action_id);
    let stream_resp = client
        .stream_task_events(stream_req)
        .await
        .map_err(|s| format!("stream_task_events: {:?}: {}", s.code(), s.message()))?;
    let mut stream = stream_resp.into_inner();

    let clock = SystemClock::new();
    let deadline = Instant::now() + stream_timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "stream terminal event timeout ({}ms) for action_id={action_id}",
                stream_timeout.as_millis()
            ));
        }
        match tokio::time::timeout(remaining, stream.message()).await {
            Ok(Ok(Some(event))) => {
                let kind =
                    TaskEventKind::try_from(event.event).unwrap_or(TaskEventKind::Unspecified);
                if matches!(
                    kind,
                    TaskEventKind::Completed | TaskEventKind::Failed | TaskEventKind::Cancelled
                ) {
                    let client_receive_ns: u64 = clock.now().0;
                    let event_at_ns: u64 = event.at_ns;
                    // 时钟域显式检查（Plan v3 §P.2.2.b 拍板）。
                    let latency_ns = if client_receive_ns >= event_at_ns {
                        client_receive_ns - event_at_ns
                    } else {
                        return Err(format!(
                            "clock_domain_violation: client_receive_ns={client_receive_ns} \
                             < event_at_ns={event_at_ns} (action_id={action_id}, kind={kind:?})"
                        ));
                    };
                    return Ok((action_id, kind, latency_ns));
                }
                // 非终态（Started / Progress）继续等
            }
            Ok(Ok(None)) => {
                return Err(format!(
                    "stream closed before terminal event (action_id={action_id})"
                ));
            }
            Ok(Err(status)) => {
                return Err(format!(
                    "stream error: {:?}: {} (action_id={action_id})",
                    status.code(),
                    status.message()
                ));
            }
            Err(_elapsed) => {
                return Err(format!(
                    "stream terminal event timeout ({}ms) for action_id={action_id}",
                    stream_timeout.as_millis()
                ));
            }
        }
    }
}

// ============================================================================
// Bring-up / tear-down 共用骨架 (2/2-a)
// ============================================================================

/// `bring_up_pair` 成功返回值：fake_s0 + s1 + gateway + 已 ready 的
/// tonic client（已通过 3 段 ready signal）。caller 借用 `client` 字段
/// 调 RPC，无需再 connect。
///
/// **不**走 RAII shutdown（`Drop` 仅靠 `SubprocessHandle::kill_on_drop`
/// 兜底 SIGKILL）；PASS 路径 caller 必须显式调 [`tear_down_pair`] 走
/// SIGTERM grace + warn-on-error，沿 [`run_selftest`] / B4 (3/3) 同款
/// 模式。
#[allow(dead_code)]
struct BringUp {
    fake_s0: FakeStatePublisher,
    s1: SubprocessHandle,
    gw: SubprocessHandle,
    client: RobotControlClient<tonic::transport::Channel>,
}

/// Bring up s1_daemon + gateway 双进程 + fake S0 + 三段式 ready signal。
///
/// 顺序：
/// 1. spawn `FakeStatePublisher`（50Hz JointState publisher；放 s1 spawn
///    之前保证 anchor 可用）
/// 2. spawn s1_daemon → wait marker `"rr_server_threads ready"` (5s)
/// 3. spawn gateway → wait TCP `127.0.0.1:<port>` (2s)
/// 4. tonic connect → `GetTaskStatus` zero-UUID 探活（5s）→ 期 NotFound
///
/// FAIL 路径：partial-up 已 spawn 的子进程**显式** dump_captured_to +
/// shutdown（不靠 `kill_on_drop` RAII，因为 caller 拿不到 BringUp 没法
/// 触发显式清理；inline 收尾避免悬挂）。case_dir 留 forensic（PASS
/// 路径由 caller 调 `ctx.cleanup()`）。
async fn bring_up_pair(ctx: &CaseCtx) -> Result<BringUp, String> {
    let fake_s0 =
        FakeStatePublisher::spawn(&ctx.robot_id, &ctx.hardware_id, ctx.joints, ctx.layout_id)
            .map_err(|e| format!("FakeStatePublisher::spawn: {e}"))?;
    eprintln!("[bring_up] fake_s0 spawned (50Hz JointState publisher)");

    let mut s1 = match spawn_s1_daemon(ctx) {
        Ok(h) => h,
        Err(e) => {
            fake_s0.shutdown();
            return Err(format!("spawn s1_daemon: {e}"));
        }
    };
    eprintln!("[bring_up] s1_daemon spawned pid={:?}", s1.pid);

    if let Err(e) = wait_for_s1_marker(&mut s1, Duration::from_secs(5)).await {
        let _ = s1.dump_captured_to(&ctx.case_dir).await;
        fake_s0.shutdown();
        let _ = s1.shutdown(Duration::from_secs(3)).await;
        return Err(format!(
            "Phase 1 (s1 marker) failed: {e}; forensic at {}",
            ctx.case_dir.display()
        ));
    }
    eprintln!("[bring_up] Phase 1 PASS: s1_daemon rr_server_threads ready");

    let gw = match spawn_gateway(ctx) {
        Ok(h) => h,
        Err(e) => {
            let _ = s1.dump_captured_to(&ctx.case_dir).await;
            fake_s0.shutdown();
            let _ = s1.shutdown(Duration::from_secs(3)).await;
            return Err(format!(
                "spawn gateway: {e}; forensic at {}",
                ctx.case_dir.display()
            ));
        }
    };
    eprintln!("[bring_up] gateway spawned pid={:?}", gw.pid);

    if let Err(e) = wait_for_gateway_tcp_ready(ctx.gateway_port, Duration::from_secs(2)).await {
        let _ = gw.dump_captured_to(&ctx.case_dir).await;
        let _ = s1.dump_captured_to(&ctx.case_dir).await;
        fake_s0.shutdown();
        let _ = gw.shutdown(Duration::from_secs(3)).await;
        let _ = s1.shutdown(Duration::from_secs(3)).await;
        return Err(format!(
            "Phase 2 (gateway TCP) failed: {e}; forensic at {}",
            ctx.case_dir.display()
        ));
    }
    eprintln!("[bring_up] Phase 2 PASS: gateway TCP connectable");

    let mut client = match connect_gateway_client(ctx.gateway_port, Duration::from_secs(2)).await {
        Ok(c) => c,
        Err(e) => {
            let _ = gw.dump_captured_to(&ctx.case_dir).await;
            let _ = s1.dump_captured_to(&ctx.case_dir).await;
            fake_s0.shutdown();
            let _ = gw.shutdown(Duration::from_secs(3)).await;
            let _ = s1.shutdown(Duration::from_secs(3)).await;
            return Err(format!(
                "tonic connect: {e}; forensic at {}",
                ctx.case_dir.display()
            ));
        }
    };
    if let Err(e) = poll_get_task_status_ready(&mut client, Duration::from_secs(5)).await {
        let _ = gw.dump_captured_to(&ctx.case_dir).await;
        let _ = s1.dump_captured_to(&ctx.case_dir).await;
        fake_s0.shutdown();
        let _ = gw.shutdown(Duration::from_secs(3)).await;
        let _ = s1.shutdown(Duration::from_secs(3)).await;
        return Err(format!(
            "Phase 3 (GetTaskStatus probe) failed: {e}; forensic at {}",
            ctx.case_dir.display()
        ));
    }
    eprintln!("[bring_up] Phase 3 PASS: GetTaskStatus zero-UUID → NotFound");

    Ok(BringUp {
        fake_s0,
        s1,
        gw,
        client,
    })
}

/// Tear down pair：gateway → daemon → fake_s0（沿 B4 (3/3) 同款顺序）。
/// shutdown 错仅 warn 不返 Err，让 caller 主路径不被 shutdown 噪音
/// 干扰；shutdown 调用本身已经处理 SIGTERM grace → SIGKILL fallback。
async fn tear_down_pair(bring_up: BringUp) {
    let BringUp {
        fake_s0,
        s1,
        gw,
        client: _,
    } = bring_up;
    if let Err(e) = gw.shutdown(Duration::from_secs(3)).await {
        eprintln!("[tear_down] gateway shutdown warn: {e}");
    }
    if let Err(e) = s1.shutdown(Duration::from_secs(3)).await {
        eprintln!("[tear_down] s1_daemon shutdown warn: {e}");
    }
    fake_s0.shutdown();
}

// ============================================================================
// run_selftest
// ============================================================================

/// (1/2) selftest case：spawn fake_s0 → s1_daemon → wait marker →
/// gateway → wait TCP → tonic connect → GetTaskStatus 探活 → shutdown
/// (gateway → s1 → fake_s0) → cleanup。
///
/// **不**调任何 InvokeTask / StreamTaskEvents 真实测（留 (2/2-b)）；
/// 仅验证 spawn + ready + shutdown 整套 harness 通。
///
/// (2/2-a) refactor 后用 [`bring_up_pair`] + [`tear_down_pair`] 共用骨架
/// （3 phase ready signal + shutdown 顺序）；selftest 本身退化到
/// "alloc CaseCtx + bring_up + tear_down + cleanup" 一条直路。
async fn run_selftest(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    eprintln!(
        "[selftest] start robot_id={} hardware_id={} joints={} layout_id={:#018x} \
         gateway_port={} workdir={}",
        ctx.robot_id,
        ctx.hardware_id,
        ctx.joints,
        ctx.layout_id,
        ctx.gateway_port,
        ctx.case_dir.display()
    );

    let bring_up = bring_up_pair(&ctx).await?;
    eprintln!("[selftest] all 3 ready phases passed; tearing down");
    tear_down_pair(bring_up).await;

    // PASS path cleanup（FAIL 已在 bring_up_pair 内 dump 到 case_dir + 显
    // 式 shutdown；PASS 走 ctx.cleanup() 清空）
    ctx.cleanup();
    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_layout_id_decimal() {
        assert_eq!(parse_layout_id("123").unwrap(), 123);
        assert_eq!(parse_layout_id("  42  ").unwrap(), 42);
    }

    #[test]
    fn parse_layout_id_hex() {
        assert_eq!(parse_layout_id("0xff").unwrap(), 255);
        assert_eq!(
            parse_layout_id("0XCAFEBABEDEADBEEF").unwrap(),
            0xCAFE_BABE_DEAD_BEEF
        );
    }

    #[test]
    fn parse_layout_id_invalid() {
        assert!(parse_layout_id("not_a_number").is_err());
        assert!(parse_layout_id("0xZZ").is_err());
    }

    #[test]
    fn phase_parse_all_variants() {
        assert_eq!(Phase::parse("selftest").unwrap(), Phase::Selftest);
        assert_eq!(Phase::parse("A1").unwrap(), Phase::A1);
        assert_eq!(Phase::parse("a1").unwrap(), Phase::A1);
        assert_eq!(Phase::parse("A2").unwrap(), Phase::A2);
        assert_eq!(Phase::parse("A3").unwrap(), Phase::A3);
        assert_eq!(Phase::parse("all").unwrap(), Phase::All);
        assert!(Phase::parse("bogus").is_err());
    }

    #[test]
    fn uuidv7_compat_format_and_version() {
        let s = uuidv7_compat();
        // 8-4-4-4-12 = 36 chars
        assert_eq!(s.len(), 36);
        let bytes: Vec<&str> = s.split('-').collect();
        assert_eq!(bytes.len(), 5);
        assert_eq!(bytes[0].len(), 8);
        assert_eq!(bytes[1].len(), 4);
        assert_eq!(bytes[2].len(), 4);
        assert_eq!(bytes[3].len(), 4);
        assert_eq!(bytes[4].len(), 12);
        // version 7 in high nibble of byte 6 = first char of group 3
        assert_eq!(&bytes[2][..1], "7");
        // variant 10xx in top 2 bits of byte 8 = first char of group 4
        // should be 8/9/a/b
        let var_char = bytes[3].chars().next().unwrap();
        assert!(matches!(var_char, '8' | '9' | 'a' | 'b'));
    }

    #[test]
    fn write_role_table_smoke_writes_valid_toml() {
        let tmp = std::env::temp_dir().join(format!(
            "pcs_test_role_table_{}_{}",
            std::process::id(),
            uuidv7_compat()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("rpc_roles.toml");
        write_role_table(&path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        // 4 fake token + 4 role + B4 收紧（operator 不含 clear_error）
        assert!(content.contains("maintainer-fake-token-001"));
        assert!(content.contains("operator-fake-token-001"));
        assert!(content.contains("viewer-fake-token-001"));
        assert!(content.contains("admin-fake-token-001"));
        // operator 不含 clear_error capability（B4 (2/3-b) 收紧）；
        // 用 `"\"clear_error\""` 字面匹配 capabilities 列表里的 quoted
        // string，避开 description 字面 "clear_error" 假阳性。
        let operator_block = content
            .split("[roles.operator]")
            .nth(1)
            .unwrap()
            .split("[roles.")
            .next()
            .unwrap();
        assert!(
            !operator_block.contains("\"clear_error\""),
            "operator capabilities must not include `clear_error` (B4 (2/3-b) 收紧)"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
