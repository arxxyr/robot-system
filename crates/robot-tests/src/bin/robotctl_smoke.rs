//! `robotctl` operator CLI cross-process e2e smoke（v0.5 候选 A 5/5）。
//!
//! 起 `robot_rpc_gateway` + `s1_daemon` + `FakeStatePublisher`（fake s0，给
//! s1 anchor + status/watch 订阅），**唯一被测路径 = spawn `robotctl` 真子
//! 进程**（reviewer 审查重点 3：严禁 smoke 内调 tonic client）。本 binary
//! **不显式启用 `robot-rpc/client`、也不直接启用 `dep:tonic`**——单 feature
//! 构建不生成 `RobotControlClient`，本文件不调用 tonic client（robot-rpc 仅取
//! `client::layout` 派生 layout，段(4) 起；tonic **server** 栈随 robot-rpc 普通
//! 依赖必编，合并 feature 构建亦可能经 unification 生成 client——边界 = 单
//! feature gate + 源码纪律，段(8) round 2 事实更正）—— readiness 走 TCP connect + s1 stderr
//! marker + `robotctl get-task-status` NotFound 探针；action_id 由解析
//! `robotctl invoke` stdout 得到。
//!
//! ## 12 phase（reviewer ACK；详 docs/handoffs/v0.5_operator_cli_5of5_entry.md）
//!
//! 1. selftest 2. clear_estop_auth_denied 3. clear_estop_safety_contract_violations
//! 4. get_task_status_happy 5. cancel_task_happy 6. stream_events_terminal_completed
//! 7. stream_events_sigint 8. invoke_scripted_happy 9. invoke_ik_happy
//! 10. json_output_round_trip 11. existing_subcommands_regression
//! 12. dry_run_safety_matrix
//!
//! ## reviewer 3 硬约束
//!
//! - clear_estop_safety_contract_violations / dry_run_safety_matrix 用**不可达**
//!   `--gateway-addr`：早拒必须 **exit 2，不是 connect-fail exit 5**（证本地
//!   早拒未发 RPC / --dry-run 优先级高于 token/gateway/RPC）。
//! - SIGINT phase 读到首 `event=` 帧（readiness gate）再发信号。
//! - existing_subcommands_regression 用 AC-2 两类判据：status/watch 固定
//!   published state 下 deterministic byte-diff=0；estop exit code + EStopEvent
//!   body（reason/source/latched）字段等价（非 byte-diff）。

#![cfg(target_os = "linux")]
#![allow(
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation
)]

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
use robot_transport::iceoryx2::{TopicClass, publisher as open_pub, subscriber as open_sub};
use robot_transport::{LatestSubscriber, Publisher};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex as TokioMutex;
use tokio::time::Instant;

// ============================================================================
// 常量
// ============================================================================

/// 有 invoke_task / read 权限的 maintainer token（rpc_roles 全 cap）。
const MAINTAINER_TOKEN: &str = "maintainer-fake-token-001";
/// 只有 read_state 的 viewer token（无 clear_estop cap → auth_denied phase）。
const VIEWER_TOKEN: &str = "viewer-fake-token-001";
/// 合法 UUIDv7 格式但永不存在的 action-id（readiness 探针 + dry-run matrix）。
const NEVER_ACTION_ID: &str = "01900000-0000-7000-8000-000000000000";
/// 不可达 gateway 地址（端口 1 privileged，无监听）—— 早拒证据用：若 CLI
/// 真去连，会 connect-fail exit 5；早拒则 exit 2 不连。
const UNREACHABLE_ADDR: &str = "127.0.0.1:1";

// ============================================================================
// CLI
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Selftest,
    ClearEstopAuthDenied,
    ClearEstopSafetyViolations,
    GetTaskStatusHappy,
    CancelTaskHappy,
    StreamEventsTerminalCompleted,
    StreamEventsSigint,
    InvokeScriptedHappy,
    InvokeIkHappy,
    JsonOutputRoundTrip,
    ExistingSubcommandsRegression,
    DryRunSafetyMatrix,
    All,
}

impl Phase {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "selftest" => Ok(Phase::Selftest),
            "clear_estop_auth_denied" => Ok(Phase::ClearEstopAuthDenied),
            "clear_estop_safety_violations" => Ok(Phase::ClearEstopSafetyViolations),
            "get_task_status_happy" => Ok(Phase::GetTaskStatusHappy),
            "cancel_task_happy" => Ok(Phase::CancelTaskHappy),
            "stream_events_terminal_completed" => Ok(Phase::StreamEventsTerminalCompleted),
            "stream_events_sigint" => Ok(Phase::StreamEventsSigint),
            "invoke_scripted_happy" => Ok(Phase::InvokeScriptedHappy),
            "invoke_ik_happy" => Ok(Phase::InvokeIkHappy),
            "json_output_round_trip" => Ok(Phase::JsonOutputRoundTrip),
            "existing_subcommands_regression" => Ok(Phase::ExistingSubcommandsRegression),
            "dry_run_safety_matrix" => Ok(Phase::DryRunSafetyMatrix),
            "all" => Ok(Phase::All),
            other => Err(format!("unknown --phase {other:?}")),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Phase::Selftest => "selftest",
            Phase::ClearEstopAuthDenied => "clear_estop_auth_denied",
            Phase::ClearEstopSafetyViolations => "clear_estop_safety_violations",
            Phase::GetTaskStatusHappy => "get_task_status_happy",
            Phase::CancelTaskHappy => "cancel_task_happy",
            Phase::StreamEventsTerminalCompleted => "stream_events_terminal_completed",
            Phase::StreamEventsSigint => "stream_events_sigint",
            Phase::InvokeScriptedHappy => "invoke_scripted_happy",
            Phase::InvokeIkHappy => "invoke_ik_happy",
            Phase::JsonOutputRoundTrip => "json_output_round_trip",
            Phase::ExistingSubcommandsRegression => "existing_subcommands_regression",
            Phase::DryRunSafetyMatrix => "dry_run_safety_matrix",
            Phase::All => "all",
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
    phase: Phase,
    workdir: PathBuf,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut bin_dir = PathBuf::from("target/debug");
        let mut robot_id_prefix = "robotctl_smoke".to_string();
        let mut hardware_id: Option<String> = None;
        let mut config: Option<PathBuf> = None;
        let mut joints: usize = 5;
        let mut phase = Phase::All;
        let mut workdir = PathBuf::from("/tmp/robotctl_smoke");

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
                        .parse()
                        .map_err(|e| format!("--joints: {e}"))?;
                }
                "--phase" => {
                    i += 1;
                    phase = Phase::parse(raw.get(i).ok_or("--phase requires value")?)?;
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

        Ok(Self {
            bin_dir,
            robot_id_prefix,
            hardware_id: hardware_id.ok_or("--hardware-id required")?,
            config: config.ok_or("--config required")?,
            joints,
            phase,
            workdir,
        })
    }
}

fn print_help() {
    eprintln!(
        "robotctl_smoke —— v0.5 候选 A (5/5) robotctl operator CLI cross-process e2e\n\
\n\
options:\n  \
--bin-dir <DIR>                  默认 target/debug（须含 robotctl / robot_rpc_gateway / s1_daemon）\n  \
--robot-id-prefix <STR>          默认 robotctl_smoke\n  \
--hardware-id <ID>               必填\n  \
--config <TOML>                  必填\n  \
--joints <N>                     默认 5\n  \
--phase <P>                      默认 all（见源码 Phase::parse 全表）\n  \
--workdir <DIR>                  默认 /tmp/robotctl_smoke\n  \
-h | --help                      打印帮助\n"
    );
}

// ============================================================================
// main
// ============================================================================

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> ExitCode {
    let _ = robot_logging::init();
    let args = match Args::parse() {
        Ok(a) => a,
        Err(e) => {
            if e != "HELP" {
                eprintln!("ERROR: {e}");
            }
            print_help();
            return ExitCode::from(u8::from(e != "HELP"));
        }
    };
    if let Err(e) = preflight(&args) {
        eprintln!("ERROR (preflight): {e}");
        return ExitCode::from(1);
    }
    eprintln!(
        "[robotctl_smoke] start phase={} bin_dir={} config={} hardware_id={} joints={}",
        args.phase.label(),
        args.bin_dir.display(),
        args.config.display(),
        args.hardware_id,
        args.joints,
    );

    let phases: Vec<Phase> = match args.phase {
        Phase::All => vec![
            Phase::Selftest,
            Phase::ClearEstopAuthDenied,
            Phase::ClearEstopSafetyViolations,
            Phase::GetTaskStatusHappy,
            Phase::CancelTaskHappy,
            Phase::StreamEventsTerminalCompleted,
            Phase::StreamEventsSigint,
            Phase::InvokeScriptedHappy,
            Phase::InvokeIkHappy,
            Phase::JsonOutputRoundTrip,
            Phase::ExistingSubcommandsRegression,
            Phase::DryRunSafetyMatrix,
        ],
        other => vec![other],
    };

    let mut fail_count: u32 = 0;
    for p in &phases {
        let label = p.label();
        eprintln!("\n========== phase {label} ==========");
        let res = match p {
            Phase::Selftest => run_phase_selftest(&args).await,
            Phase::ClearEstopAuthDenied => run_phase_clear_estop_auth_denied(&args).await,
            Phase::ClearEstopSafetyViolations => run_phase_clear_estop_safety(&args).await,
            Phase::GetTaskStatusHappy => run_phase_get_task_status_happy(&args).await,
            Phase::CancelTaskHappy => run_phase_cancel_task_happy(&args).await,
            Phase::StreamEventsTerminalCompleted => run_phase_stream_terminal(&args).await,
            Phase::StreamEventsSigint => run_phase_stream_sigint(&args).await,
            Phase::InvokeScriptedHappy => run_phase_invoke_scripted_happy(&args).await,
            Phase::InvokeIkHappy => run_phase_invoke_ik_happy(&args).await,
            Phase::JsonOutputRoundTrip => run_phase_json_output(&args).await,
            Phase::ExistingSubcommandsRegression => run_phase_existing_regression(&args).await,
            Phase::DryRunSafetyMatrix => run_phase_dry_run_matrix(&args).await,
            Phase::All => unreachable!("Phase::All 展开后不应出现"),
        };
        match res {
            Ok(()) => eprintln!("[{label}] PASS"),
            Err(e) => {
                eprintln!("[{label}] FAIL: {e}");
                fail_count = fail_count.saturating_add(1);
            }
        }
    }
    eprintln!("\n========== summary ==========");
    let total = u32::try_from(phases.len()).unwrap_or(u32::MAX);
    eprintln!(
        "  total phases: {total}  PASS: {pass}  FAIL: {fail_count}",
        pass = total.saturating_sub(fail_count),
    );
    if fail_count > 0 {
        ExitCode::from(2)
    } else {
        ExitCode::from(0)
    }
}

fn preflight(args: &Args) -> Result<(), String> {
    for bin in ["robotctl", "robot_rpc_gateway", "s1_daemon"] {
        let p = args.bin_dir.join(bin);
        if !p.exists() {
            return Err(format!("{bin} not found at {}", p.display()));
        }
    }
    if !args.config.exists() {
        return Err(format!("config not found at {}", args.config.display()));
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

static CASE_SEQ: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone)]
struct CaseCtx {
    robot_id: String,
    hardware_id: String,
    joints: usize,
    layout_id: u64,
    config: PathBuf,
    bin_dir: PathBuf,
    case_dir: PathBuf,
    role_table_path: PathBuf,
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
        // v0.5 候选 E 段(2) 回修 P0-1：样板 config 结构化重写 [robot].id =
        // 本 case 唯一 robot_id（身份锁下"静态 config + 动态 robot_id"消费
        // 面；共享 helper 详 robot_tests::config_fixture + Spec P.4 登记）。
        let config = robot_tests::config_fixture::materialize_config_with_robot_id(
            &args.config,
            &robot_id,
            &case_dir,
        )
        .map_err(|e| format!("materialize per-case config: {e}"))?;
        // 段(4)：layout 由物化 config 派生（PD-E5(a) 单源，与 s0/s1 同值）。
        // 进程级 --layout-id 已删——per-case robot_id 入编码，进程级值不可能
        // 同时匹配多个 case（reviewer 段(4) 复核 P1-1）。
        let derived_layout =
            robot_rpc::client::layout::load_hardware_layout(&config, &args.hardware_id)
                .map_err(|e| format!("derive layout: {e}"))?
                .layout_id;
        let gateway_port = allocate_loopback_port()?;
        Ok(Self {
            robot_id,
            hardware_id: args.hardware_id.clone(),
            joints: args.joints,
            layout_id: derived_layout,
            config,
            bin_dir: args.bin_dir.clone(),
            case_dir,
            role_table_path,
            gateway_port,
        })
    }

    fn gateway_addr(&self) -> String {
        format!("127.0.0.1:{}", self.gateway_port)
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.case_dir);
    }
}

fn write_role_table(path: &Path) -> Result<(), String> {
    let toml_text = r#"# robotctl_smoke 临时 rpc_roles.toml（每 case 独立）。
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
// FakeStatePublisher（fake s0；fixed_stamp 模式给 regression 用 deterministic）
// ============================================================================

const PUBLISH_INTERVAL: Duration = Duration::from_millis(20); // 50Hz
const FAKE_S0_SOURCE_ID: u32 = 1;
/// regression phase 固定 stamp（deterministic byte-diff 用）。
const REGRESSION_FIXED_STAMP_NS: u64 = 1_000_000_000;

struct FakeStatePublisher {
    handle: Option<StdJoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl FakeStatePublisher {
    fn spawn(
        robot_id: &str,
        hardware_id: &str,
        joints: usize,
        layout_id: u64,
        fixed_stamp_ns: Option<u64>,
    ) -> std::io::Result<Self> {
        let topic = format!("/{robot_id}/s0/{hardware_id}/state");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        // regression：固定 positions（deterministic）；其它：0.0（fresh stamp）。
        let positions: Vec<f64> = if fixed_stamp_ns.is_some() {
            (0..joints).map(|j| 0.05 * (j as f64 + 1.0)).collect()
        } else {
            vec![0.0_f64; joints]
        };
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

        let handle = thread::Builder::new()
            .name(format!("rcs_fake_state_{robot_id}"))
            .spawn(move || {
                let node = NodeBuilder::new()
                    .create::<IxService>()
                    .expect("FakeStatePublisher: NodeBuilder create");
                let mut publisher =
                    match open_pub::<markers::JointState>(&node, &topic, TopicClass::S0State) {
                        Ok(p) => p,
                        Err(e) => {
                            eprintln!("[fake_s0] open_pub failed {topic}: {e:?}");
                            drop(ready_tx);
                            return;
                        }
                    };
                Box::leak(Box::new(node));
                let _ = ready_tx.send(());
                drop(ready_tx);

                let clock = SystemClock::new();
                let mut seq: u64 = 0;
                while !stop_clone.load(Ordering::Acquire) {
                    let stamp_ns = fixed_stamp_ns.unwrap_or_else(|| clock.now().0);
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
            return Err(std::io::Error::other("FakeStatePublisher: factory failed"));
        }
        Ok(Self {
            handle: Some(handle),
            stop,
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
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ============================================================================
// EStopCapture（regression estop phase：订阅 + 抓 body 字段）
// ============================================================================

/// 抓到的 EStopEvent body 字段（reason / source / latched / stamp_ns）。
type EStopBody = (String, String, bool, u64);

struct EStopCapture {
    handle: Option<StdJoinHandle<()>>,
    stop: Arc<AtomicBool>,
    captured: Arc<std::sync::Mutex<Option<EStopBody>>>,
}

impl EStopCapture {
    fn spawn(robot_id: &str) -> std::io::Result<Self> {
        let topic = format!("/{robot_id}/events/estop");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let captured: Arc<std::sync::Mutex<Option<EStopBody>>> =
            Arc::new(std::sync::Mutex::new(None));
        let captured_clone = Arc::clone(&captured);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

        let handle = thread::Builder::new()
            .name(format!("rcs_estop_cap_{robot_id}"))
            .spawn(move || {
                let node = NodeBuilder::new()
                    .create::<IxService>()
                    .expect("EStopCapture: NodeBuilder create");
                let mut subscriber =
                    match open_sub::<markers::EStopEvent>(&node, &topic, TopicClass::SafetyEvent) {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("[estop_cap] open_sub failed {topic}: {e:?}");
                            drop(ready_tx);
                            return;
                        }
                    };
                Box::leak(Box::new(node));
                let _ = ready_tx.send(());
                drop(ready_tx);

                while !stop_clone.load(Ordering::Acquire) {
                    if let Ok(Some(lease)) = subscriber.latest()
                        && let Ok(view) = lease.view()
                    {
                        let ev = view.body.get();
                        let body = (
                            ev.reason().unwrap_or("").to_string(),
                            ev.source().unwrap_or("").to_string(),
                            ev.latched(),
                            ev.stamp_ns(),
                        );
                        *captured_clone.lock().expect("estop cap lock") = Some(body);
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            })?;

        if ready_rx.recv().is_err() {
            return Err(std::io::Error::other("EStopCapture: factory failed"));
        }
        Ok(Self {
            handle: Some(handle),
            stop,
            captured,
        })
    }

    fn snapshot(&self) -> Option<EStopBody> {
        self.captured.lock().expect("estop cap lock").clone()
    }

    fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for EStopCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ============================================================================
// SubprocessHandle（gateway / s1）
// ============================================================================

struct SubprocessHandle {
    label: &'static str,
    pid: Option<u32>,
    child: Option<Child>,
    captured: Arc<TokioMutex<Vec<String>>>,
}

impl SubprocessHandle {
    fn spawn(label: &'static str, mut command: Command) -> std::io::Result<Self> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command
            .env("ROBOT_LOG_FORMAT", "json")
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "info");
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
            if let Some(child) = self.child.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                return Err(format!(
                    "{label}: exited before marker `{needle}` ({status:?})"
                ));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{label}: marker `{needle}` not found in {timeout:?}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn shutdown(mut self, grace: Duration) -> Result<(), String> {
        let label = self.label;
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        if let Some(pid) = self.pid {
            let pid_i32 = i32::try_from(pid).unwrap_or(0);
            // SAFETY: pid 来自 Child::id()；SIGTERM async-signal-safe。
            unsafe {
                libc::kill(pid_i32, libc::SIGTERM);
            }
        }
        match tokio::time::timeout(grace, child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(format!("{label}: wait err: {e}")),
            Err(_elapsed) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                Ok(())
            }
        }
    }

    async fn dump_captured_to(&self, dir: &Path) {
        let lines = self.captured.lock().await;
        let _ = std::fs::write(dir.join(format!("{}.log", self.label)), lines.join("\n"));
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
        while let Ok(Some(line)) = reader.next_line().await {
            eprintln!("[{label}/{stream}] {line}");
            captured.lock().await.push(line);
        }
    });
}

fn spawn_s1_daemon(ctx: &CaseCtx) -> std::io::Result<SubprocessHandle> {
    let mut cmd = Command::new(ctx.bin_dir.join("s1_daemon"));
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
    let mut cmd = Command::new(ctx.bin_dir.join("robot_rpc_gateway"));
    cmd.arg("--robot-id")
        .arg(&ctx.robot_id)
        .arg("--listen-addr")
        .arg(ctx.gateway_addr())
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

async fn wait_for_gateway_tcp_ready(port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let addr = format!("127.0.0.1:{port}");
    loop {
        if let Ok(s) = tokio::net::TcpStream::connect(&addr).await {
            drop(s);
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("gateway TCP {addr} not connectable in {timeout:?}"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ============================================================================
// run_robotctl —— 审查重点 3 唯一被测路径（spawn 真子进程，NOT tonic）
// ============================================================================

#[derive(Debug)]
struct RobotctlOutcome {
    code: i32,
    stdout: String,
    stderr: String,
}

impl RobotctlOutcome {
    fn last_stderr(&self) -> &str {
        self.stderr.lines().last().unwrap_or("")
    }
}

/// spawn `robotctl <args> --gateway-addr <addr>`（token 经 env `ROBOT_TOKEN`），
/// `output()` 收 stdout/stderr/exit。`None` token = 不设 env（测 token 缺）。
async fn run_robotctl(
    bin_dir: &Path,
    gateway_addr: &str,
    token: Option<&str>,
    args: &[&str],
) -> Result<RobotctlOutcome, String> {
    let mut cmd = Command::new(bin_dir.join("robotctl"));
    cmd.args(args);
    cmd.arg("--gateway-addr").arg(gateway_addr);
    cmd.env("NO_COLOR", "1").env_remove("ROBOT_GATEWAY_ADDR");
    match token {
        Some(t) => {
            cmd.env("ROBOT_TOKEN", t);
        }
        None => {
            cmd.env_remove("ROBOT_TOKEN");
        }
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    let out = cmd
        .output()
        .await
        .map_err(|e| format!("spawn robotctl {args:?}: {e}"))?;
    Ok(RobotctlOutcome {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// 流式 spawn `stream-events <id>`：读到首 `event=` 帧（readiness gate）→ 发
/// SIGINT → 有界等 exit（reviewer 审查重点 4）。
async fn run_stream_events_sigint(
    bin_dir: &Path,
    gateway_addr: &str,
    token: &str,
    action_id: &str,
    readiness: Duration,
    exit_grace: Duration,
) -> Result<RobotctlOutcome, String> {
    let mut cmd = Command::new(bin_dir.join("robotctl"));
    // 大 --timeout-ms：流不能在 readiness gate + SIGINT 之前被整流 timeout 关。
    cmd.args(["stream-events", action_id, "--timeout-ms", "30000"]);
    cmd.arg("--gateway-addr").arg(gateway_addr);
    cmd.env("NO_COLOR", "1")
        .env("ROBOT_TOKEN", token)
        .env_remove("ROBOT_GATEWAY_ADDR");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn stream-events: {e}"))?;
    let pid = child.id().ok_or("stream-events: no pid")?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out_cap = Arc::new(TokioMutex::new(Vec::<String>::new()));
    let err_cap = Arc::new(TokioMutex::new(Vec::<String>::new()));
    spawn_stream_drain("robotctl/stream", "stdout", stdout, Arc::clone(&out_cap));
    spawn_stream_drain("robotctl/stream", "stderr", stderr, Arc::clone(&err_cap));

    // readiness gate：等首个含 `event=` 的 stdout 行。
    let deadline = Instant::now() + readiness;
    loop {
        {
            let g = out_cap.lock().await;
            if g.iter().any(|l| l.contains("event=")) {
                break;
            }
        }
        if let Ok(Some(st)) = child.try_wait() {
            return Err(format!("stream-events exited before first event= ({st:?})"));
        }
        if Instant::now() >= deadline {
            return Err("no `event=` frame within readiness window".into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 发 SIGINT。SAFETY: pid 来自 Child::id()；SIGINT async-signal-safe。
    unsafe {
        libc::kill(i32::try_from(pid).unwrap_or(0), libc::SIGINT);
    }
    let status = match tokio::time::timeout(exit_grace, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("stream-events wait: {e}")),
        Err(_elapsed) => {
            let _ = child.kill().await;
            return Err("stream-events did not exit within grace after SIGINT".into());
        }
    };
    Ok(RobotctlOutcome {
        code: status.code().unwrap_or(-1),
        stdout: out_cap.lock().await.join("\n"),
        stderr: err_cap.lock().await.join("\n"),
    })
}

/// readiness 探针：`robotctl get-task-status <NEVER>` 直到 exit 4（NotFound）—
/// 证 robotctl → gateway → s1 RR 全链就绪（仍走真子进程，不调 tonic）。
async fn wait_for_robotctl_ready(
    bin_dir: &Path,
    gateway_addr: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let out = run_robotctl(
            bin_dir,
            gateway_addr,
            Some(MAINTAINER_TOKEN),
            &["get-task-status", NEVER_ACTION_ID],
        )
        .await?;
        if out.code == 4 {
            return Ok(()); // NotFound = 全链就绪
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "robotctl readiness probe not NotFound in {timeout:?} (last code={} stderr={})",
                out.code,
                out.last_stderr()
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ============================================================================
// bring_up / tear_down（gateway + s1 + fake_s0；NO tonic readiness）
// ============================================================================

struct BringUp {
    fake_s0: FakeStatePublisher,
    s1: SubprocessHandle,
    gw: SubprocessHandle,
}

async fn bring_up(ctx: &CaseCtx) -> Result<BringUp, String> {
    let fake_s0 = FakeStatePublisher::spawn(
        &ctx.robot_id,
        &ctx.hardware_id,
        ctx.joints,
        ctx.layout_id,
        None,
    )
    .map_err(|e| format!("FakeStatePublisher::spawn: {e}"))?;
    eprintln!("[bring_up] fake_s0 spawned");

    let mut s1 = match spawn_s1_daemon(ctx) {
        Ok(h) => h,
        Err(e) => {
            fake_s0.shutdown();
            return Err(format!("spawn s1_daemon: {e}"));
        }
    };
    if let Err(e) = s1
        .wait_for_marker("rr_server_threads ready", Duration::from_secs(8))
        .await
    {
        s1.dump_captured_to(&ctx.case_dir).await;
        fake_s0.shutdown();
        let _ = s1.shutdown(Duration::from_secs(3)).await;
        return Err(format!("s1 marker: {e}"));
    }
    eprintln!("[bring_up] s1_daemon ready");

    let gw = match spawn_gateway(ctx) {
        Ok(h) => h,
        Err(e) => {
            fake_s0.shutdown();
            let _ = s1.shutdown(Duration::from_secs(3)).await;
            return Err(format!("spawn gateway: {e}"));
        }
    };
    if let Err(e) = wait_for_gateway_tcp_ready(ctx.gateway_port, Duration::from_secs(5)).await {
        gw.dump_captured_to(&ctx.case_dir).await;
        fake_s0.shutdown();
        let _ = gw.shutdown(Duration::from_secs(3)).await;
        let _ = s1.shutdown(Duration::from_secs(3)).await;
        return Err(format!("gateway TCP: {e}"));
    }
    if let Err(e) =
        wait_for_robotctl_ready(&ctx.bin_dir, &ctx.gateway_addr(), Duration::from_secs(8)).await
    {
        gw.dump_captured_to(&ctx.case_dir).await;
        s1.dump_captured_to(&ctx.case_dir).await;
        fake_s0.shutdown();
        let _ = gw.shutdown(Duration::from_secs(3)).await;
        let _ = s1.shutdown(Duration::from_secs(3)).await;
        return Err(format!("robotctl readiness: {e}"));
    }
    eprintln!("[bring_up] gateway + full chain ready");
    Ok(BringUp { fake_s0, s1, gw })
}

async fn tear_down(b: BringUp) {
    let BringUp { fake_s0, s1, gw } = b;
    let _ = gw.shutdown(Duration::from_secs(3)).await;
    let _ = s1.shutdown(Duration::from_secs(3)).await;
    fake_s0.shutdown();
}

// ============================================================================
// helpers
// ============================================================================

/// 解析 `robotctl invoke` Ok 输出的 `action_id=<uuid>`（human 模式）。
fn parse_action_id(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("action_id=").map(|s| s.trim().to_string()))
}

/// 写一个 N waypoint 的 waypoints.json（positions 长度 = joints）。
fn write_waypoints_file(
    dir: &Path,
    joints: usize,
    n: usize,
    seg_ms: u32,
) -> Result<PathBuf, String> {
    use std::fmt::Write as _;
    let mut s = String::from("{\"waypoints\":[");
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        let pos: Vec<String> = (0..joints)
            .map(|j| format!("{:.3}", 0.05 * ((i * joints + j) as f64)))
            .collect();
        write!(
            s,
            "{{\"positions\":[{}],\"dwell_ms\":0,\"segment_duration_ms\":{seg_ms}}}",
            pos.join(",")
        )
        .expect("write to String never fails");
    }
    s.push_str("]}");
    let path = dir.join("waypoints.json");
    std::fs::write(&path, s).map_err(|e| format!("write waypoints {}: {e}", path.display()))?;
    Ok(path)
}

/// invoke scripted（默认 human）→ 返 action_id。
async fn invoke_scripted(ctx: &CaseCtx, n: usize, seg_ms: u32) -> Result<String, String> {
    let wp = write_waypoints_file(&ctx.case_dir, ctx.joints, n, seg_ms)?;
    let wp_s = wp.to_string_lossy().into_owned();
    let out = run_robotctl(
        &ctx.bin_dir,
        &ctx.gateway_addr(),
        Some(MAINTAINER_TOKEN),
        &[
            "invoke",
            "scripted",
            "--waypoints-file",
            &wp_s,
            "--audit-reason",
            "robotctl_smoke",
        ],
    )
    .await?;
    if out.code != 0 {
        return Err(format!(
            "invoke scripted code={} stderr={}",
            out.code,
            out.last_stderr()
        ));
    }
    parse_action_id(&out.stdout).ok_or_else(|| format!("no action_id in stdout: {:?}", out.stdout))
}

// ============================================================================
// phases
// ============================================================================

async fn run_phase_selftest(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let b = bring_up(&ctx).await?;
    tear_down(b).await;
    ctx.cleanup();
    Ok(())
}

/// clear-estop viewer token → CLI 四闸过 → RPC 发 → server PermissionDenied → exit 3。
async fn run_phase_clear_estop_auth_denied(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let b = bring_up(&ctx).await?;
    let out = run_robotctl(
        &ctx.bin_dir,
        &ctx.gateway_addr(),
        Some(VIEWER_TOKEN),
        &[
            "clear-estop",
            "--confirmation",
            "CONFIRM_CLEAR_ESTOP",
            "--audit-reason",
            "robotctl_smoke",
        ],
    )
    .await;
    tear_down(b).await;
    ctx.cleanup();
    let out = out?;
    if out.code != 3 {
        return Err(format!(
            "expected exit 3 (PermissionDenied), got code={} stderr={}",
            out.code,
            out.last_stderr()
        ));
    }
    Ok(())
}

/// 约束 1：不可达 gateway + 四闸违例 → 全 exit 2（**断言 != 5**，证本地早拒未连）。
async fn run_phase_clear_estop_safety(args: &Args) -> Result<(), String> {
    // 无需 bring_up（早拒在连接前发生）。
    let bin = &args.bin_dir;
    let cases: &[(&str, Option<&str>, &[&str])] = &[
        (
            "missing confirmation",
            Some(MAINTAINER_TOKEN),
            &["clear-estop", "--audit-reason", "x"],
        ),
        (
            "confirmation mismatch",
            Some(MAINTAINER_TOKEN),
            &[
                "clear-estop",
                "--confirmation",
                "wrong",
                "--audit-reason",
                "x",
            ],
        ),
        (
            "missing audit-reason",
            Some(MAINTAINER_TOKEN),
            &["clear-estop", "--confirmation", "CONFIRM_CLEAR_ESTOP"],
        ),
        (
            "missing token",
            None,
            &[
                "clear-estop",
                "--confirmation",
                "CONFIRM_CLEAR_ESTOP",
                "--audit-reason",
                "x",
            ],
        ),
    ];
    for (name, token, cli) in cases {
        let out = run_robotctl(bin, UNREACHABLE_ADDR, *token, cli).await?;
        if out.code != 2 {
            return Err(format!(
                "case {name:?}: expected exit 2 (local early-reject, no RPC), got code={} stderr={} \
                 —— exit 5 表示真去连了不可达 gateway = 早拒失效",
                out.code,
                out.last_stderr()
            ));
        }
    }

    // 正向反例（reviewer 回归守卫）：clear-estop **不**拒 `--dry-run`（唯一
    // 端到端支持 dry_run 的子命令）。四闸齐全 + --dry-run + 不可达 gateway →
    // CLI 不早拒 dry-run → 走连接 → **exit 5（NOT exit 2）** + stderr **不含**
    // "--dry-run is not supported"（不连到真 gateway 故无副作用）。若有人误接
    // reject_dry_run("clear-estop")，此处变 exit 2 / 含拒绝文案 → 守卫触发。
    let cx = run_robotctl(
        bin,
        UNREACHABLE_ADDR,
        Some(MAINTAINER_TOKEN),
        &[
            "clear-estop",
            "--confirmation",
            "CONFIRM_CLEAR_ESTOP",
            "--audit-reason",
            "x",
            "--dry-run",
        ],
    )
    .await?;
    if cx.code != 5 {
        return Err(format!(
            "clear-estop --dry-run counter-example: expected exit 5（未被拒 → 连不可达 \
             gateway 失败），got code={} stderr={} —— exit 2 表示 clear-estop 误拒了 \
             --dry-run（端到端支持被破坏）",
            cx.code,
            cx.last_stderr()
        ));
    }
    if cx.stderr.contains("--dry-run is not supported") {
        return Err(format!(
            "clear-estop --dry-run counter-example: stderr 不应含 dry-run 拒绝文案 \
             （clear-estop 端到端支持 dry_run）: {:?}",
            cx.last_stderr()
        ));
    }
    Ok(())
}

async fn run_phase_get_task_status_happy(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let b = bring_up(&ctx).await?;
    let res = async {
        let action_id = invoke_scripted(&ctx, 2, 300).await?;
        let out = run_robotctl(
            &ctx.bin_dir,
            &ctx.gateway_addr(),
            Some(MAINTAINER_TOKEN),
            &["get-task-status", &action_id],
        )
        .await?;
        if out.code != 0 {
            return Err(format!(
                "get-task-status code={} stderr={}",
                out.code,
                out.last_stderr()
            ));
        }
        let line = out.stdout.lines().next().unwrap_or("");
        if !(line.contains("state=EXECUTING")
            || line.contains("state=COMPLETED")
            || line.contains("state=PENDING"))
        {
            return Err(format!("unexpected state line: {line:?}"));
        }
        Ok(())
    }
    .await;
    tear_down(b).await;
    ctx.cleanup();
    res
}

async fn run_phase_cancel_task_happy(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let b = bring_up(&ctx).await?;
    let res = async {
        // 长 action（多 waypoint + 长 segment）以便 cancel 命中 in-flight。
        let action_id = invoke_scripted(&ctx, 4, 1500).await?;
        let out = run_robotctl(
            &ctx.bin_dir,
            &ctx.gateway_addr(),
            Some(MAINTAINER_TOKEN),
            &[
                "cancel-task",
                &action_id,
                "--audit-reason",
                "robotctl_smoke",
            ],
        )
        .await?;
        if out.code != 0 {
            return Err(format!(
                "cancel-task code={} stderr={}",
                out.code,
                out.last_stderr()
            ));
        }
        if !out.stdout.contains("cleared_at_ns=") {
            return Err(format!(
                "cancel-task missing cleared_at_ns: {:?}",
                out.stdout
            ));
        }
        Ok(())
    }
    .await;
    tear_down(b).await;
    ctx.cleanup();
    res
}

async fn run_phase_stream_terminal(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let b = bring_up(&ctx).await?;
    let res = async {
        let action_id = invoke_scripted(&ctx, 2, 300).await?;
        let out = run_robotctl(
            &ctx.bin_dir,
            &ctx.gateway_addr(),
            Some(MAINTAINER_TOKEN),
            // 大 --timeout-ms：让流等到 COMPLETED 终态，不被整流 timeout 提前关。
            &["stream-events", &action_id, "--timeout-ms", "30000"],
        )
        .await?;
        if out.code != 0 {
            return Err(format!(
                "stream-events code={} stderr={}",
                out.code,
                out.last_stderr()
            ));
        }
        let terminal = out.stdout.lines().any(|l| {
            l.contains("event=COMPLETED")
                || l.contains("event=FAILED")
                || l.contains("event=CANCELLED")
        });
        if !terminal {
            return Err(format!(
                "no terminal event in stream stdout: {:?}",
                out.stdout
            ));
        }
        Ok(())
    }
    .await;
    tear_down(b).await;
    ctx.cleanup();
    res
}

/// 审查重点 4：invoke 长 action → stream-events 读首帧 → SIGINT → exit 0 + warning。
async fn run_phase_stream_sigint(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let b = bring_up(&ctx).await?;
    let res = async {
        let action_id = invoke_scripted(&ctx, 6, 2000).await?;
        let out = run_stream_events_sigint(
            &ctx.bin_dir,
            &ctx.gateway_addr(),
            MAINTAINER_TOKEN,
            &action_id,
            Duration::from_secs(8),
            Duration::from_secs(5),
        )
        .await?;
        if out.code != 0 {
            return Err(format!(
                "SIGINT exit expected 0, got code={} stderr={}",
                out.code,
                out.last_stderr()
            ));
        }
        if !out
            .stderr
            .contains("stream interrupted before terminal event")
        {
            return Err(format!(
                "missing SIGINT warning in stderr: {:?}",
                out.stderr
            ));
        }
        Ok(())
    }
    .await;
    tear_down(b).await;
    ctx.cleanup();
    res
}

async fn run_phase_invoke_scripted_happy(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let b = bring_up(&ctx).await?;
    let res = async {
        let action_id = invoke_scripted(&ctx, 2, 300).await?;
        if action_id.is_empty() {
            return Err("empty action_id".into());
        }
        Ok(())
    }
    .await;
    tear_down(b).await;
    ctx.cleanup();
    res
}

async fn run_phase_invoke_ik_happy(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let b = bring_up(&ctx).await?;
    let out = run_robotctl(
        &ctx.bin_dir,
        &ctx.gateway_addr(),
        Some(MAINTAINER_TOKEN),
        &[
            "invoke",
            "ik",
            "--target-x",
            "0.20",
            "--target-y",
            "0.0",
            "--target-z",
            "0.10",
            "--audit-reason",
            "robotctl_smoke",
        ],
    )
    .await;
    tear_down(b).await;
    ctx.cleanup();
    let out = out?;
    if out.code != 0 {
        return Err(format!(
            "invoke ik code={} stderr={}",
            out.code,
            out.last_stderr()
        ));
    }
    if parse_action_id(&out.stdout).is_none() {
        return Err(format!(
            "no action_id in invoke ik stdout: {:?}",
            out.stdout
        ));
    }
    Ok(())
}

/// AC-7：invoke scripted --json → stdout 真 serde_json parse + 字段集。
async fn run_phase_json_output(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let b = bring_up(&ctx).await?;
    let res = async {
        let wp = write_waypoints_file(&ctx.case_dir, ctx.joints, 2, 300)?;
        let wp_s = wp.to_string_lossy().into_owned();
        let out = run_robotctl(
            &ctx.bin_dir,
            &ctx.gateway_addr(),
            Some(MAINTAINER_TOKEN),
            &[
                "invoke",
                "scripted",
                "--waypoints-file",
                &wp_s,
                "--audit-reason",
                "robotctl_smoke",
                "--json",
            ],
        )
        .await?;
        if out.code != 0 {
            return Err(format!(
                "invoke scripted --json code={} stderr={}",
                out.code,
                out.last_stderr()
            ));
        }
        let line = out.stdout.lines().next().unwrap_or("").trim();
        let v: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("stdout not JSON ({e}): {line:?}"))?;
        let obj = v.as_object().ok_or("JSON not an object")?;
        for key in ["action_id", "request_id"] {
            let s = obj
                .get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if s.is_empty() {
                return Err(format!("JSON missing/empty `{key}`: {line:?}"));
            }
        }
        Ok(())
    }
    .await;
    tear_down(b).await;
    ctx.cleanup();
    res
}

/// AC-2 两类判据：status/watch deterministic byte-diff=0（fixed state）；
/// estop exit code + EStopEvent body 字段等价。**无 gateway/s1**。
async fn run_phase_existing_regression(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let res = run_existing_regression_inner(&ctx).await;
    ctx.cleanup();
    res
}

async fn run_existing_regression_inner(ctx: &CaseCtx) -> Result<(), String> {
    let fake_s0 = FakeStatePublisher::spawn(
        &ctx.robot_id,
        &ctx.hardware_id,
        ctx.joints,
        ctx.layout_id,
        Some(REGRESSION_FIXED_STAMP_NS),
    )
    .map_err(|e| format!("fixed FakeStatePublisher: {e}"))?;
    // settle：确保首帧已 publish。
    tokio::time::sleep(Duration::from_millis(300)).await;

    let common: &[&str] = &[
        "--robot-id",
        &ctx.robot_id,
        "--hardware-id",
        &ctx.hardware_id,
    ];

    // ---- status：跑两次 → stdout byte-identical（deterministic，无自生 clock）----
    let mut status_args = vec!["status"];
    status_args.extend_from_slice(common);
    let s1 = run_legacy_robotctl(&ctx.bin_dir, &status_args).await?;
    let s2 = run_legacy_robotctl(&ctx.bin_dir, &status_args).await?;
    if s1.code != 0 || s2.code != 0 {
        fake_s0.shutdown();
        return Err(format!("status exit nonzero: {} / {}", s1.code, s2.code));
    }
    if s1.stdout != s2.stdout {
        fake_s0.shutdown();
        return Err(format!(
            "status stdout not deterministic across runs:\n--run1--\n{}\n--run2--\n{}",
            s1.stdout, s2.stdout
        ));
    }
    // status 单发 latest() 读受 iceoryx2 late-subscriber 投递时机影响（投递在
    // 订阅者连接后的下一次 publish；status 立即读 → 通常 "no sample yet"）——
    // 这是单发读的固有竞态，非 robotctl bug。故 status 只断言 exit 0 + 跨运行
    // byte-identical（上方已断言；deterministic，无自生 clock）；**渲染 +
    // fixed-state 字段的 deterministic byte-diff 由下方 watch 证**（status /
    // watch 共用同一 print_snapshot，robotctl.rs:425）。

    // ---- watch：persistent subscriber 循环 → reliable 读到 fixed sample；
    // 过滤"已读到 sample"的 snapshot block → 全 byte-identical（无自生 clock
    // drift）+ 含 layout（AC-2 deterministic byte-diff 主证）----
    let layout_hex = format!("{:#018x}", ctx.layout_id);
    let mut watch_args = vec!["watch"];
    watch_args.extend_from_slice(common);
    watch_args.extend_from_slice(&["--duration-secs", "2", "--interval-ms", "200"]);
    let w = run_legacy_robotctl(&ctx.bin_dir, &watch_args).await?;
    if w.code != 0 {
        fake_s0.shutdown();
        return Err(format!("watch exit nonzero: {}", w.code));
    }
    let sample_blocks: Vec<&str> = w
        .stdout
        .split("\n\n")
        .filter(|b| {
            b.lines()
                .find(|l| l.starts_with("state:"))
                .is_some_and(|l| !l.contains("no sample yet"))
        })
        .collect();
    if sample_blocks.is_empty() {
        fake_s0.shutdown();
        return Err(format!(
            "watch produced no sample snapshot block: {:?}",
            w.stdout
        ));
    }
    if sample_blocks.iter().any(|b| *b != sample_blocks[0]) {
        fake_s0.shutdown();
        return Err("watch sample blocks not byte-identical (self-clock drift?)".into());
    }
    if !sample_blocks[0].contains(&layout_hex) {
        fake_s0.shutdown();
        return Err(format!(
            "watch sample block missing layout {layout_hex}: {:?}",
            sample_blocks[0]
        ));
    }

    // ---- estop：exit code 0 + EStopEvent body 字段等价（reason/source/latched）----
    let estop_cap = EStopCapture::spawn(&ctx.robot_id).map_err(|e| format!("EStopCapture: {e}"))?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut estop_args = vec!["estop"];
    estop_args.extend_from_slice(common);
    estop_args.extend_from_slice(&["--reason", "robotctl_smoke_estop"]);
    let e = run_legacy_robotctl(&ctx.bin_dir, &estop_args).await?;
    if e.code != 0 {
        estop_cap.shutdown();
        fake_s0.shutdown();
        return Err(format!("estop exit nonzero: {}", e.code));
    }
    // 轮询抓 EStopEvent。
    let deadline = Instant::now() + Duration::from_secs(3);
    let body = loop {
        if let Some(b) = estop_cap.snapshot() {
            break Some(b);
        }
        if Instant::now() >= deadline {
            break None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    estop_cap.shutdown();
    fake_s0.shutdown();
    let (reason, source, latched, stamp) = body.ok_or("no EStopEvent captured after estop")?;
    if source != "robotctl" || !latched || reason != "robotctl_smoke_estop" || stamp == 0 {
        return Err(format!(
            "EStopEvent body mismatch: reason={reason:?} source={source:?} latched={latched} stamp={stamp}"
        ));
    }
    Ok(())
}

/// 跑既有 sync 子命令（status/watch/estop；不带 --gateway-addr）。
async fn run_legacy_robotctl(bin_dir: &Path, args: &[&str]) -> Result<RobotctlOutcome, String> {
    let mut cmd = Command::new(bin_dir.join("robotctl"));
    cmd.args(args);
    cmd.env("NO_COLOR", "1").env_remove("ROBOT_TOKEN");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    let out = cmd
        .output()
        .await
        .map_err(|e| format!("spawn legacy robotctl {args:?}: {e}"))?;
    Ok(RobotctlOutcome {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// 约束 2：6 非 clear-estop 子命令 + 不可达 gateway + --dry-run → 全 exit 2 +
/// "--dry-run is not supported"（证 --dry-run 早拒优先级高于 token/gateway/RPC）。
async fn run_phase_dry_run_matrix(args: &Args) -> Result<(), String> {
    let bin = &args.bin_dir;
    let tok = Some(MAINTAINER_TOKEN);
    let cases: &[(&str, &[&str])] = &[
        (
            "clear-error",
            &["clear-error", "--audit-reason", "x", "--dry-run"],
        ),
        (
            "get-task-status",
            &["get-task-status", NEVER_ACTION_ID, "--dry-run"],
        ),
        (
            "cancel-task",
            &[
                "cancel-task",
                NEVER_ACTION_ID,
                "--audit-reason",
                "x",
                "--dry-run",
            ],
        ),
        (
            "stream-events",
            &["stream-events", NEVER_ACTION_ID, "--dry-run"],
        ),
        (
            "invoke scripted",
            &[
                "invoke",
                "scripted",
                "--waypoints-file",
                "/nonexistent.json",
                "--audit-reason",
                "x",
                "--dry-run",
            ],
        ),
        (
            "invoke ik",
            &[
                "invoke",
                "ik",
                "--target-x",
                "0.1",
                "--target-y",
                "0.0",
                "--target-z",
                "0.1",
                "--audit-reason",
                "x",
                "--dry-run",
            ],
        ),
    ];
    for (sub, cli) in cases {
        let out = run_robotctl(bin, UNREACHABLE_ADDR, tok, cli).await?;
        if out.code != 2 {
            return Err(format!(
                "{sub} --dry-run: expected exit 2 (early-reject), got code={} stderr={}",
                out.code,
                out.last_stderr()
            ));
        }
        if !out.stderr.contains("--dry-run is not supported") {
            return Err(format!(
                "{sub} --dry-run: stderr 缺 dry-run 拒绝文案: {:?}",
                out.last_stderr()
            ));
        }
    }
    Ok(())
}
