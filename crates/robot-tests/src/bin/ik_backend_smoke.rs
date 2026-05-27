//! `ik_backend_smoke` —— v0.4 候选 2 round 2 (4/4) IkBackend cross-process
//! end-to-end smoke binary。
//!
//! ## Scope
//!
//! 跨进程 e2e（沿 `scripted_backend_smoke` / `pubsub_cleanup_smoke` /
//! `clear_estop_e2e` 同款 inline 模式；reviewer 拍板"smoke 不把 helper
//! 抽公共库"，所有 fbs / tonic / fake-S0 / process spawn helper inline
//! 复制）：
//!
//! 1. spawn fake S0 (50Hz `JointState`) + `s1_daemon` + `robot_rpc_gateway`
//! 2. 用 tonic gRPC client 真跑 `InvokeTask(kind=Ik, payload=IkPayload)`
//!    → iceoryx2 RR shm → s1 coordinator → `IkBackend.start()` 调
//!    `XLeRobotArmIkSolver.solve()` → first tick publish `JointCommand`
//!    → second tick `Completed(Success)` → `StreamTaskEvents` 收 Started
//!    + Progress(1.0) + Completed(Success)
//! 3. 5 phase（按用户拍板顺序）：
//!    - `selftest`：harness bring-up + IkPayload builder round-trip +
//!      XLeRobotArmIkSolver(Side::Right) ground truth round-trip（不调
//!      InvokeTask）
//!    - `happy`（Q-IK17 (a) single-shot）：reachable target → Completed
//!      (Success) + ≥1 Progress (≤ 1.0) + Started 是 informational
//!    - `monotonic`（B3 §3.7 + AC-5 同款契约）：subscribe + invoke 一次
//!      happy → JointCommand.source_task_id 全程严格单调（hold loop 起
//!      点 + IkBackend single-shot 一帧 + hold loop 收尾，多帧覆盖）
//!    - `unreachable_clamp`（Q-IK16 (a)）：极端 target [10, 10, 10] →
//!      XLeRobot solver `Ok(clamped)` → IkBackend Continue + Completed
//!      (Success)；gateway 终态**不**是 Failed
//!    - `invalid_input`：2 子 case
//!      - `iva` yaw≠0 quaternion → solver `IkError::InvalidInput` →
//!        backend `Rejected` → tonic `FailedPrecondition`
//!      - `ivb` 1 KiB garbage payload → backend `PayloadDecode` →
//!        tonic `FailedPrecondition`
//!
//! ## 跟候选 1/3 既有 binary 的边界（reviewer 拍板）
//!
//! - **不**改 `scripted_backend_smoke.rs` / `pubsub_cleanup_smoke.rs` /
//!   `clear_estop_e2e.rs` —— 本 binary 是独立 sibling。
//! - **不**抽公共 helper：CaseCtx / FakeStatePublisher / JointCommand
//!   Subscriber / SubprocessHandle / bring_up_pair / tear_down_pair /
//!   wait_for_*  / build_invoke_task_cmd / uuidv7_compat 全部 inline
//!   复制；如未来 reviewer 决策抽 `robot-tests-helpers` 公共 crate，再
//!   走独立 PR。
//! - **不**改 schema / 既有 backend / coordinator / s1_daemon binary。
//!
//! ## CLI
//!
//! ```text
//! ik_backend_smoke [OPTIONS]
//!
//! options:
//!   --bin-dir <DIR>                  默认 target/release
//!   --robot-id-prefix <STR>          默认 ik_backend
//!   --hardware-id <ID>               必填（跟 --config TOML 一致；默认配置 `ik_arm`）
//!   --config <TOML>                  必填（s1_daemon --config）
//!   --joints <N>                     默认 5（XLeRobot 5-DOF；不可改）
//!   --layout-id <U64>                默认 0xCAFEBABEDEADBEEF
//!   --phase <PHASE>                  默认 all
//!     selftest | happy | monotonic | unreachable_clamp | invalid_input | all
//!   --target-x <F64>                 默认 0.20（happy + monotonic 用）
//!   --target-y <F64>                 默认 0.0
//!   --target-z <F64>                 默认 0.10
//!   --workdir <DIR>                  默认 /tmp/ik_backend_smoke
//!   -h | --help                      打印帮助
//! ```
//!
//! ## 退出码
//!
//! - `0`：所有 phase PASS
//! - `1`：CLI / preflight / 子进程 spawn 错
//! - `2`：≥1 phase FAIL
//!
//! ## 跟 Spec / Plan 的链接
//!
//! - [`team-swe/specs/v0_4_robot_kinematics_round2.md`](../../../../../team-swe/specs/v0_4_robot_kinematics_round2.md)
//!   Q-IK1..Q-IK17 + Plan §P.3.2 #1..#12
//! - [`docs/handoffs/v0.3_b4_outline.md`](../../../../../docs/handoffs/v0.3_b4_outline.md)
//!   §6.1 双层 schema 模型（gateway 透传 payload bytes 不解码）

#![cfg(target_os = "linux")]
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::{self, JoinHandle as StdJoinHandle};
use std::time::Duration;

use flatbuffers::FlatBufferBuilder;
use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::{RobotClock, SystemClock};
use robot_kinematics::{ArmIkSolver, Pose};
use robot_kinematics_xlerobot::{Side, XLeRobotArmIkSolver};
use robot_msg::build_helpers::build_joint_state;
use robot_msg::markers;
use robot_msg::msg::{IkPayload as FbIkPayload, IkPayloadArgs};
use robot_msg::schema_id::SCHEMA_IK_PAYLOAD;
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{
    ActionKindProto, AuthContext, CommandMeta, InvokeTaskCommand, TaskEventKind, TaskHandleRef,
};
use robot_transport::iceoryx2::{TopicClass, publisher as open_pub, subscriber as open_sub};
use robot_transport::{LatestSubscriber, Publisher};
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
    Happy,
    Monotonic,
    UnreachableClamp,
    InvalidInput,
    All,
}

impl Phase {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "selftest" => Ok(Phase::Selftest),
            "happy" => Ok(Phase::Happy),
            "monotonic" => Ok(Phase::Monotonic),
            "unreachable_clamp" => Ok(Phase::UnreachableClamp),
            "invalid_input" => Ok(Phase::InvalidInput),
            "all" => Ok(Phase::All),
            other => Err(format!(
                "unknown --phase {other:?} (expected selftest|happy|monotonic|\
                 unreachable_clamp|invalid_input|all)"
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Phase::Selftest => "selftest",
            Phase::Happy => "happy",
            Phase::Monotonic => "monotonic",
            Phase::UnreachableClamp => "unreachable_clamp",
            Phase::InvalidInput => "invalid_input",
            Phase::All => "all",
        }
    }
}

/// XLeRobot 5-DOF —— `IkBackend` startup 期校验
/// `solver.info().joint_count == hardware.joints.len()`，错位 →
/// HardwareMismatch。本 smoke 锁 5。
const XLEROBOT_JOINTS: usize = 5;

#[derive(Debug, Clone)]
struct Args {
    bin_dir: PathBuf,
    robot_id_prefix: String,
    hardware_id: String,
    config: PathBuf,
    joints: usize,
    layout_id: u64,
    phase: Phase,
    target_x: f64,
    target_y: f64,
    target_z: f64,
    workdir: PathBuf,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut bin_dir = PathBuf::from("target/release");
        let mut robot_id_prefix = "ik_backend".to_string();
        let mut hardware_id: Option<String> = None;
        let mut config: Option<PathBuf> = None;
        let mut joints: usize = XLEROBOT_JOINTS;
        let mut layout_id: u64 = 0xCAFE_BABE_DEAD_BEEF;
        let mut phase = Phase::All;
        let mut target_x: f64 = 0.20;
        let mut target_y: f64 = 0.0;
        let mut target_z: f64 = 0.10;
        let mut workdir = PathBuf::from("/tmp/ik_backend_smoke");

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
                "--phase" => {
                    i += 1;
                    let v = raw.get(i).ok_or("--phase requires value")?;
                    phase = Phase::parse(v)?;
                }
                "--target-x" => {
                    i += 1;
                    target_x = raw
                        .get(i)
                        .ok_or("--target-x requires value")?
                        .parse::<f64>()
                        .map_err(|e| format!("--target-x parse: {e}"))?;
                }
                "--target-y" => {
                    i += 1;
                    target_y = raw
                        .get(i)
                        .ok_or("--target-y requires value")?
                        .parse::<f64>()
                        .map_err(|e| format!("--target-y parse: {e}"))?;
                }
                "--target-z" => {
                    i += 1;
                    target_z = raw
                        .get(i)
                        .ok_or("--target-z requires value")?
                        .parse::<f64>()
                        .map_err(|e| format!("--target-z parse: {e}"))?;
                }
                "--workdir" => {
                    i += 1;
                    workdir = PathBuf::from(raw.get(i).ok_or("--workdir requires value")?);
                }
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => return Err(format!("unknown arg {other:?}")),
            }
            i += 1;
        }

        let hardware_id = hardware_id.ok_or("--hardware-id is required")?;
        let config = config.ok_or("--config is required")?;
        if joints != XLEROBOT_JOINTS {
            return Err(format!(
                "--joints must be {XLEROBOT_JOINTS} (XLeRobot 5-DOF); got {joints}"
            ));
        }

        Ok(Self {
            bin_dir,
            robot_id_prefix,
            hardware_id,
            config,
            joints,
            layout_id,
            phase,
            target_x,
            target_y,
            target_z,
            workdir,
        })
    }
}

fn parse_layout_id(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let parsed = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
    } else {
        s.parse::<u64>()
    };
    parsed.map_err(|e| format!("--layout-id parse {s:?}: {e}"))
}

fn print_help() {
    eprintln!(
        "ik_backend_smoke —— v0.4 候选 2 round 2 (4/4) IkBackend cross-process e2e\n\
\n\
options:\n  \
--bin-dir <DIR>                  默认 target/release\n  \
--robot-id-prefix <STR>          默认 ik_backend\n  \
--hardware-id <ID>               必填\n  \
--config <TOML>                  必填\n  \
--joints <N>                     默认 5（XLeRobot 5-DOF；不可改）\n  \
--layout-id <U64|0xHEX>          默认 0xCAFEBABEDEADBEEF\n  \
--phase <P>                      默认 all\n    \
P = selftest|happy|monotonic|unreachable_clamp|invalid_input|all\n  \
--target-x <F64>                 默认 0.20（happy + monotonic 用）\n  \
--target-y <F64>                 默认 0.0\n  \
--target-z <F64>                 默认 0.10\n  \
--workdir <DIR>                  默认 /tmp/ik_backend_smoke\n  \
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
            eprintln!("ERROR: {e}");
            print_help();
            return ExitCode::from(1);
        }
    };
    if let Err(e) = preflight(&args) {
        eprintln!("ERROR (preflight): {e}");
        return ExitCode::from(1);
    }
    eprintln!(
        "[ik_backend_smoke] start phase={} bin_dir={} config={} hardware_id={} \
         joints={} layout_id={:#018x} target=({:.3},{:.3},{:.3}) workdir={}",
        args.phase.label(),
        args.bin_dir.display(),
        args.config.display(),
        args.hardware_id,
        args.joints,
        args.layout_id,
        args.target_x,
        args.target_y,
        args.target_z,
        args.workdir.display(),
    );

    let phases: Vec<Phase> = match args.phase {
        Phase::All => vec![
            Phase::Selftest,
            Phase::Happy,
            Phase::Monotonic,
            Phase::UnreachableClamp,
            Phase::InvalidInput,
        ],
        other => vec![other],
    };

    let mut fail_count: u32 = 0;
    for p in &phases {
        let label = p.label();
        eprintln!("\n========== phase {label} ==========");
        let res = match p {
            Phase::Selftest => run_phase_selftest(&args).await,
            Phase::Happy => run_phase_happy(&args).await,
            Phase::Monotonic => run_phase_monotonic(&args).await,
            Phase::UnreachableClamp => run_phase_unreachable_clamp(&args).await,
            Phase::InvalidInput => run_phase_invalid_input(&args).await,
            Phase::All => unreachable!("Phase::All 展开后不应再出现"),
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

// ============================================================================
// preflight
// ============================================================================

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
    std::fs::create_dir_all(&args.workdir)
        .map_err(|e| format!("mkdir --workdir {}: {e}", args.workdir.display()))?;
    Ok(())
}

// ============================================================================
// CaseCtx（inline 沿 scripted_backend_smoke 同款）
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

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.case_dir);
    }
}

fn write_role_table(path: &Path) -> Result<(), String> {
    let toml_text = r#"# ik_backend_smoke 临时 rpc_roles.toml（每 case 独立）。

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
// FakeStatePublisher（inline 沿 scripted_backend_smoke 同款）
// ============================================================================

const PUBLISH_INTERVAL: Duration = Duration::from_millis(20); // 50Hz
const FAKE_S0_SOURCE_ID: u32 = 1;

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
            .name(format!("iks_fake_state_{robot_id}"))
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
                        eprintln!("[fake_s0] open_pub failed for topic {topic_for_thread}: {e:?}");
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
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ============================================================================
// JointCommandSubscriber（monotonic phase 用；inline 沿 scripted_backend
// _smoke 同款）
// ============================================================================

/// 订阅 `/<robot_id>/s1/<hardware_id>/command` topic，把每一帧
/// `JointCommand.source_task_id()` 推到共享 `Vec<u64>`。
///
/// IkBackend single-shot 单 action 仅 publish 一帧；hold loop 在 Idle 时
/// 同样按 50Hz publish hold cmd（cmd_seq 递增）。monotonic phase 收集
/// 多帧（IK 一帧 + hold 多帧）做单调判定。
struct JointCommandSubscriber {
    handle: Option<StdJoinHandle<()>>,
    stop: Arc<AtomicBool>,
    seqs: Arc<std::sync::Mutex<Vec<u64>>>,
}

impl JointCommandSubscriber {
    fn spawn(robot_id: &str, hardware_id: &str) -> std::io::Result<Self> {
        let topic = format!("/{robot_id}/s1/{hardware_id}/command");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let seqs: Arc<std::sync::Mutex<Vec<u64>>> =
            Arc::new(std::sync::Mutex::new(Vec::with_capacity(1024)));
        let seqs_clone = Arc::clone(&seqs);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

        let handle = thread::Builder::new()
            .name(format!("iks_jc_sub_{robot_id}"))
            .spawn(move || {
                let node = NodeBuilder::new()
                    .create::<IxService>()
                    .expect("JointCommandSubscriber: NodeBuilder create");
                let subscriber =
                    match open_sub::<markers::JointCommand>(&node, &topic, TopicClass::S0Command) {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("[jc_sub] open_sub failed for topic {topic}: {e:?}");
                            drop(ready_tx);
                            return;
                        }
                    };
                Box::leak(Box::new(node));
                let _ = ready_tx.send(());
                drop(ready_tx);

                let mut subscriber = subscriber;
                while !stop_clone.load(Ordering::Acquire) {
                    if let Ok(Some(sample)) = subscriber.latest()
                        && let Ok(view) = sample.view()
                    {
                        let cmd = view.body.get();
                        let seq = cmd.source_task_id();
                        let mut v = seqs_clone.lock().expect("seqs lock");
                        // 同一帧 latest() 可能被多次取，按末尾去重避免
                        // 人工 duplicates 误判单调失败。
                        if v.last() != Some(&seq) {
                            v.push(seq);
                        }
                    }
                    thread::sleep(Duration::from_millis(2));
                }
            })?;

        if ready_rx.recv().is_err() {
            return Err(std::io::Error::other(format!(
                "JointCommandSubscriber: factory panicked / iceoryx2 create \
                 failed for robot_id={robot_id}"
            )));
        }
        Ok(Self {
            handle: Some(handle),
            stop,
            seqs,
        })
    }

    /// stop thread + 拿 owned `Vec<u64>`（按到达顺序去重后的 source_task_id 序列）。
    fn shutdown(mut self) -> Vec<u64> {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let guard = self.seqs.lock().expect("seqs lock");
        guard.clone()
    }
}

impl Drop for JointCommandSubscriber {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ============================================================================
// SubprocessHandle（inline 沿 scripted_backend_smoke 同款）
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
// Readiness（inline 沿 scripted_backend_smoke 同款）
// ============================================================================

async fn wait_for_s1_marker(s1: &mut SubprocessHandle, timeout: Duration) -> Result<(), String> {
    s1.wait_for_marker("rr_server_threads ready", timeout).await
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
            return Err(format!(
                "gateway TCP {addr} not connectable within {}ms",
                timeout.as_millis()
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

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
            Err(status) if status.code() == tonic::Code::NotFound => return Ok(()),
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

fn uuidv7_compat() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("SystemTime before UNIX_EPOCH");
    let unix_ts_ms = u64::try_from(now.as_millis()).unwrap_or(u64::MAX);
    let nanos = now.subsec_nanos();
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
// IkPayload fbs builder（reviewer 拍板"smoke 不抽公共 helper"，inline 复制）
// ============================================================================

/// 构造一段 `IkPayload` fbs bytes。
///
/// 全部字段 owned slice 输入；frame 字符串占位（trait 层 round 2 不做
/// frame 变换，详 spec Q-IK7 (a)）。
fn build_ik_payload_bytes(
    target_xyz: &[f64],
    target_quat_xyzw: &[f64],
    seed_joints_rad: &[f64],
    frame: &str,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(256);
    let xyz_v = fbb.create_vector::<f64>(target_xyz);
    let quat_v = fbb.create_vector::<f64>(target_quat_xyzw);
    let seed_v = fbb.create_vector::<f64>(seed_joints_rad);
    let frame_v = fbb.create_string(frame);
    let payload = FbIkPayload::create(
        &mut fbb,
        &IkPayloadArgs {
            target_xyz: Some(xyz_v),
            target_quat_xyzw: Some(quat_v),
            seed_joints_rad: Some(seed_v),
            frame: Some(frame_v),
        },
    );
    fbb.finish(payload, None);
    fbb.finished_data().to_vec()
}

/// 标准 happy IkPayload：identity quaternion（yaw=0）+ 无 seed + 空
/// frame 占位。
fn build_happy_ik_payload(x: f64, y: f64, z: f64) -> Vec<u8> {
    build_ik_payload_bytes(&[x, y, z], &[0.0, 0.0, 0.0, 1.0], &[], "")
}

// ============================================================================
// InvokeTaskCommand builder（inline 沿 scripted_backend_smoke 同款）
// ============================================================================

#[allow(clippy::too_many_arguments)]
fn build_invoke_task_cmd(
    request_id: &str,
    idempotency_key: &str,
    source: &str,
    auth_token: &str,
    kind: ActionKindProto,
    timeout_ms: u64,
    audit_reason: &str,
    payload_schema_id: u32,
    payload: Vec<u8>,
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
        deadline_ns: 0,
        priority: 0,
        payload_schema_id,
        payload,
    };
    tonic::Request::new(cmd)
}

fn build_task_handle_ref_req(request_id: &str, action_id: &str) -> tonic::Request<TaskHandleRef> {
    tonic::Request::new(TaskHandleRef {
        request_id: request_id.to_string(),
        action_id: action_id.to_string(),
    })
}

// ============================================================================
// Bring-up / tear-down（inline 沿 scripted_backend_smoke 同款）
// ============================================================================

struct BringUp {
    fake_s0: FakeStatePublisher,
    s1: SubprocessHandle,
    gw: SubprocessHandle,
    client: RobotControlClient<tonic::transport::Channel>,
}

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
// 共享：invoke_ik_and_collect_stream
// ============================================================================

struct IkStreamSummary {
    saw_started: bool,
    progress_count: u32,
    progress_max: f32,
    terminal: TaskEventKind,
}

/// 跑一次 IK action，把 Started/Progress/终态 收 stream 后做汇总。
async fn invoke_ik_and_collect_stream(
    client: &mut RobotControlClient<tonic::transport::Channel>,
    auth_token: &str,
    payload_schema_id: u32,
    payload: Vec<u8>,
    rpc_timeout_ms: u64,
    stream_timeout: Duration,
    audit_reason: &str,
) -> Result<(String, IkStreamSummary), String> {
    let request_id = uuidv7_compat();
    let ik_key = format!("ik-{request_id}");
    let invoke_req = build_invoke_task_cmd(
        &request_id,
        &ik_key,
        "ik_backend_smoke",
        auth_token,
        ActionKindProto::ActionKindIk,
        rpc_timeout_ms,
        audit_reason,
        payload_schema_id,
        payload,
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

    let mut summary = IkStreamSummary {
        saw_started: false,
        progress_count: 0,
        progress_max: 0.0,
        terminal: TaskEventKind::Unspecified,
    };
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
                match kind {
                    TaskEventKind::Started => {
                        summary.saw_started = true;
                    }
                    TaskEventKind::Progress => {
                        summary.progress_count = summary.progress_count.saturating_add(1);
                        if event.progress > summary.progress_max {
                            summary.progress_max = event.progress;
                        }
                    }
                    TaskEventKind::Completed | TaskEventKind::Failed | TaskEventKind::Cancelled => {
                        summary.terminal = kind;
                        return Ok((action_id, summary));
                    }
                    TaskEventKind::Unspecified => {
                        // 仅 Unspecified 跳过；任何 future 加的 variant 在
                        // 这里会编译 fail，强制 reviewer 决定该变体的处理。
                    }
                }
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
// Phase: selftest
// ============================================================================

async fn run_phase_selftest(args: &Args) -> Result<(), String> {
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

    // (1) IkPayload builder round-trip（lib unit 已 ground truth；这里
    // round-trip 一次防 binary feature 编译漂移没人发现）。
    let bytes = build_happy_ik_payload(args.target_x, args.target_y, args.target_z);
    let fb = flatbuffers::root::<FbIkPayload<'_>>(&bytes)
        .map_err(|e| format!("selftest: IkPayload builder fbs verify failed: {e}"))?;
    let xyz_len = fb.target_xyz().map_or(0, |v| v.len());
    let quat_len = fb.target_quat_xyzw().map_or(0, |v| v.len());
    let seed_len = fb.seed_joints_rad().map_or(0, |v| v.len());
    if xyz_len != 3 || quat_len != 4 || seed_len != 0 {
        return Err(format!(
            "selftest: IkPayload builder field lengths wrong (xyz={xyz_len}, quat={quat_len}, seed={seed_len})"
        ));
    }
    eprintln!(
        "[selftest] IkPayload builder round-trip ok (xyz=3 quat=4 seed=0 bytes={})",
        bytes.len()
    );

    // (2) XLeRobotArmIkSolver(Side::Right) ground truth round-trip。
    // 验"binary 端 link 进来的 solver crate 跟 daemon 端一致"路径不漂移。
    let solver = XLeRobotArmIkSolver::new(Side::Right);
    let pose = Pose {
        translation: [args.target_x, args.target_y, args.target_z],
        orientation: [0.0, 0.0, 0.0, 1.0],
    };
    let sol = solver
        .solve(&pose, None, None)
        .map_err(|e| format!("selftest: XLeRobotArmIkSolver.solve failed: {e}"))?;
    if sol.joints_rad.len() != XLEROBOT_JOINTS {
        return Err(format!(
            "selftest: solver returned {} joints, expected {XLEROBOT_JOINTS}",
            sol.joints_rad.len()
        ));
    }
    eprintln!(
        "[selftest] XLeRobotArmIkSolver(Side::Right).solve ok joints={:?}",
        sol.joints_rad
    );

    ctx.cleanup();
    Ok(())
}

// ============================================================================
// Phase: happy（Q-IK17 (a) single-shot）
// ============================================================================

async fn run_phase_happy(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let mut bring_up = bring_up_pair(&ctx).await?;
    let result = run_phase_happy_inner(args, &mut bring_up).await;
    if result.is_err() {
        let _ = bring_up.s1.dump_captured_to(&ctx.case_dir).await;
        let _ = bring_up.gw.dump_captured_to(&ctx.case_dir).await;
    }
    tear_down_pair(bring_up).await;
    if result.is_ok() {
        ctx.cleanup();
    } else {
        eprintln!(
            "[happy] FAIL forensic preserved at {}",
            ctx.case_dir.display()
        );
    }
    result
}

async fn run_phase_happy_inner(args: &Args, bring_up: &mut BringUp) -> Result<(), String> {
    let payload = build_happy_ik_payload(args.target_x, args.target_y, args.target_z);
    eprintln!(
        "[happy] InvokeTask kind=Ik target=({:.3},{:.3},{:.3}) payload_bytes={}",
        args.target_x,
        args.target_y,
        args.target_z,
        payload.len()
    );
    // IK single-shot 实时性高（一次 solve + 一帧 publish），整体 RPC
    // 超时 2s 即足够；stream 终态等额外加 2s 防 hold loop / events drain
    // 抖动。
    let rpc_timeout_ms: u64 = 2_000;
    let stream_timeout = Duration::from_millis(rpc_timeout_ms.saturating_add(2_000));
    let (action_id, summary) = invoke_ik_and_collect_stream(
        &mut bring_up.client,
        "operator-fake-token-001",
        SCHEMA_IK_PAYLOAD,
        payload,
        rpc_timeout_ms,
        stream_timeout,
        "happy",
    )
    .await?;
    eprintln!(
        "[happy] action_id={action_id} terminal={:?} started={} progress_count={} progress_max={}",
        summary.terminal, summary.saw_started, summary.progress_count, summary.progress_max
    );

    // Started + Progress 都是 informational only（per-ActionId pubsub
    // latest-sample 模型 + IkBackend single-shot 模式：first tick emit
    // Progress + Continue，second tick emit Completed；两个非终态事件
    // 在 ~20ms 内连续 publish，gateway events_drain 可能 latest-only 跳
    // 过 Started / Progress 而只见 Completed）。AC 硬约束 = 终态
    // Completed —— Progress=1.0 / source_task_id == ctx.command_seq 已
    // 由 ik.rs `tick_emits_command_with_source_task_id_eq_ctx_command_seq`
    // 单元锁住。
    if !summary.saw_started {
        eprintln!(
            "[happy] note: did not observe Started event (per-ActionId pubsub latest \
             semantics may have skipped early frames before stream subscribe); \
             informational only"
        );
    }
    if summary.progress_count == 0 {
        eprintln!(
            "[happy] note: did not observe Progress event (single-shot backend emits 1 \
             non-terminal Progress before Completed; per-ActionId pubsub latest-sample \
             may have skipped it); informational only — bit-level progress=1.0 is locked \
             by ik.rs `tick_emits_command_with_source_task_id_eq_ctx_command_seq` unit test"
        );
    } else if summary.progress_max > 1.0 + f32::EPSILON {
        return Err(format!(
            "happy: progress_max ({}) must be ≤ 1.0",
            summary.progress_max
        ));
    }
    if summary.terminal != TaskEventKind::Completed {
        return Err(format!(
            "happy: expected terminal=Completed(Success), got {:?}",
            summary.terminal
        ));
    }
    Ok(())
}

// ============================================================================
// Phase: monotonic（B3 §3.7 source_task_id == ctx.command_seq 单调契约）
// ============================================================================

async fn run_phase_monotonic(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let mut bring_up = bring_up_pair(&ctx).await?;
    let jc_sub = match JointCommandSubscriber::spawn(&ctx.robot_id, &ctx.hardware_id) {
        Ok(s) => s,
        Err(e) => {
            let _ = bring_up.s1.dump_captured_to(&ctx.case_dir).await;
            let _ = bring_up.gw.dump_captured_to(&ctx.case_dir).await;
            tear_down_pair(bring_up).await;
            return Err(format!("JointCommandSubscriber::spawn: {e}"));
        }
    };
    // 让 hold loop 先跑几帧，建立 seq 基线（IkBackend 单帧之外的"正常
    // hold publish"上下文）。
    tokio::time::sleep(Duration::from_millis(200)).await;
    let result = run_phase_monotonic_inner(args, &mut bring_up).await;
    // IkBackend 单帧 publish 之后 hold loop 还要继续 tick；多等几帧让
    // subscriber 收集 enough 数据。
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seqs = jc_sub.shutdown();
    let analysis = if result.is_ok() {
        analyze_source_task_id_seqs(&seqs)
    } else {
        Ok(())
    };
    if result.is_err() || analysis.is_err() {
        let _ = bring_up.s1.dump_captured_to(&ctx.case_dir).await;
        let _ = bring_up.gw.dump_captured_to(&ctx.case_dir).await;
    }
    tear_down_pair(bring_up).await;
    if result.is_ok() && analysis.is_ok() {
        ctx.cleanup();
    } else {
        eprintln!(
            "[monotonic] FAIL forensic preserved at {}",
            ctx.case_dir.display()
        );
    }
    result?;
    analysis
}

async fn run_phase_monotonic_inner(args: &Args, bring_up: &mut BringUp) -> Result<(), String> {
    let payload = build_happy_ik_payload(args.target_x, args.target_y, args.target_z);
    let rpc_timeout_ms: u64 = 2_000;
    let stream_timeout = Duration::from_millis(rpc_timeout_ms.saturating_add(2_000));
    let (action_id, summary) = invoke_ik_and_collect_stream(
        &mut bring_up.client,
        "operator-fake-token-001",
        SCHEMA_IK_PAYLOAD,
        payload,
        rpc_timeout_ms,
        stream_timeout,
        "monotonic",
    )
    .await?;
    eprintln!(
        "[monotonic] action_id={action_id} terminal={:?} progress_count={}",
        summary.terminal, summary.progress_count
    );
    if summary.terminal != TaskEventKind::Completed {
        return Err(format!(
            "monotonic: expected terminal=Completed, got {:?}",
            summary.terminal
        ));
    }
    Ok(())
}

fn analyze_source_task_id_seqs(seqs: &[u64]) -> Result<(), String> {
    eprintln!(
        "[monotonic] collected {} unique JointCommand.source_task_id values",
        seqs.len()
    );
    // hold loop 50Hz publish 多帧 + IkBackend 单帧；总数应该数十以上
    // （bring_up + 200ms wait + 2s RPC + 300ms tail ≈ 至少 50 帧）。
    if seqs.len() < 5 {
        return Err(format!(
            "monotonic: subscriber collected only {} seq values; expected several (hold loop \
             50Hz publish over ~2s should yield 50+ JointCommand frames)",
            seqs.len()
        ));
    }
    let mut prev = seqs[0];
    for (i, &s) in seqs.iter().enumerate().skip(1) {
        if s <= prev {
            return Err(format!(
                "monotonic: source_task_id not strictly monotonic at idx={i}: prev={prev}, now={s} \
                 (full seqs[0..min(20)]: {:?})",
                &seqs[..seqs.len().min(20)]
            ));
        }
        prev = s;
    }
    eprintln!(
        "[monotonic] source_task_id 严格单调 PASS (first={}, last={}, span={} entries)",
        seqs[0],
        seqs[seqs.len() - 1],
        seqs.len()
    );
    Ok(())
}

// ============================================================================
// Phase: unreachable_clamp（Q-IK16 (a) ACK：XLeRobot solver Ok(clamped)，
//                          backend Continue + Completed(Success)）
// ============================================================================

async fn run_phase_unreachable_clamp(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let mut bring_up = bring_up_pair(&ctx).await?;
    let result = run_phase_unreachable_clamp_inner(&mut bring_up).await;
    if result.is_err() {
        let _ = bring_up.s1.dump_captured_to(&ctx.case_dir).await;
        let _ = bring_up.gw.dump_captured_to(&ctx.case_dir).await;
    }
    tear_down_pair(bring_up).await;
    if result.is_ok() {
        ctx.cleanup();
    } else {
        eprintln!(
            "[unreachable_clamp] FAIL forensic preserved at {}",
            ctx.case_dir.display()
        );
    }
    result
}

async fn run_phase_unreachable_clamp_inner(bring_up: &mut BringUp) -> Result<(), String> {
    // 极端远 target（XLeRobot L1+L2 大约 0.3m 左右；10m 远超 r_max）。
    // Solver `_planar_ik` 按 dimos 同款 clamp：r > r_max → 缩放到 r_max；
    // joint 限位再 clamp。Ok(IkSolution { clamped_joints })；backend 转
    // Continue → Completed(Success)；gateway 终态**不**是 Failed。
    let payload = build_happy_ik_payload(10.0, 10.0, 10.0);
    eprintln!(
        "[unreachable_clamp] InvokeTask kind=Ik target=(10,10,10) payload_bytes={}",
        payload.len()
    );
    let rpc_timeout_ms: u64 = 2_000;
    let stream_timeout = Duration::from_millis(rpc_timeout_ms.saturating_add(2_000));
    let (action_id, summary) = invoke_ik_and_collect_stream(
        &mut bring_up.client,
        "operator-fake-token-001",
        SCHEMA_IK_PAYLOAD,
        payload,
        rpc_timeout_ms,
        stream_timeout,
        "unreachable_clamp",
    )
    .await?;
    eprintln!(
        "[unreachable_clamp] action_id={action_id} terminal={:?} progress_count={} progress_max={}",
        summary.terminal, summary.progress_count, summary.progress_max
    );
    if summary.terminal != TaskEventKind::Completed {
        return Err(format!(
            "unreachable_clamp: expected terminal=Completed (Q-IK16 (a) clamp 路径)，got {:?}",
            summary.terminal
        ));
    }
    // Progress 是 informational only（详 happy 同款注释 + IkBackend
    // single-shot 解释）。AC 硬约束 = 终态 Completed（证 solver 走
    // Ok(clamped) 而非 Err(OutOfReach)），bit-level progress=1.0 已由
    // ik.rs unit 锁住。
    if summary.progress_count > 0 && summary.progress_max > 1.0 + f32::EPSILON {
        return Err(format!(
            "unreachable_clamp: progress_max ({}) must be ≤ 1.0",
            summary.progress_max
        ));
    }
    eprintln!("[unreachable_clamp] terminal=Completed PASS (XLeRobot solver Ok(clamped) ✓)");
    Ok(())
}

// ============================================================================
// Phase: invalid_input（iva: yaw≠0 → Rejected；ivb: garbage → PayloadDecode）
// ============================================================================

async fn run_phase_invalid_input(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let mut bring_up = bring_up_pair(&ctx).await?;
    let result = run_phase_invalid_input_inner(&mut bring_up).await;
    if result.is_err() {
        let _ = bring_up.s1.dump_captured_to(&ctx.case_dir).await;
        let _ = bring_up.gw.dump_captured_to(&ctx.case_dir).await;
    }
    tear_down_pair(bring_up).await;
    if result.is_ok() {
        ctx.cleanup();
    } else {
        eprintln!(
            "[invalid_input] FAIL forensic preserved at {}",
            ctx.case_dir.display()
        );
    }
    result
}

async fn run_phase_invalid_input_inner(bring_up: &mut BringUp) -> Result<(), String> {
    // iva: yaw ≠ 0 quaternion（绕 Z 90°）→ solver IkError::InvalidInput
    // → backend Rejected → tonic FailedPrecondition。
    {
        let half = std::f64::consts::FRAC_1_SQRT_2;
        let payload = build_ik_payload_bytes(
            &[args_safe_target_x(), 0.0, 0.10],
            // (0, 0, sin(π/4), cos(π/4)) = 绕 Z 90°，quat_to_euler yaw=π/2
            &[0.0, 0.0, half, half],
            &[],
            "",
        );
        let req = build_invoke_task_cmd(
            &uuidv7_compat(),
            "ik-iva",
            "ik_backend_smoke",
            "operator-fake-token-001",
            ActionKindProto::ActionKindIk,
            2_000,
            "invalid_input iva yaw nonzero",
            SCHEMA_IK_PAYLOAD,
            payload,
        );
        let err = bring_up
            .client
            .invoke_task(req)
            .await
            .err()
            .ok_or("invalid_input iva: expected Err but got Ok")?;
        if err.code() != tonic::Code::FailedPrecondition {
            return Err(format!(
                "invalid_input iva: expected FailedPrecondition (solver InvalidInput → \
                 backend Rejected → SubmitError::BackendStartFailed → tonic::Code::FailedPrecondition), \
                 got {:?}: {}",
                err.code(),
                err.message()
            ));
        }
        // backend Rejected 文案含 "IK solver InvalidInput" + "yaw"（详 ik.rs
        // map_ik_error_to_start_error + crates/robot-kinematics-xlerobot
        // /src/lib.rs Q-IK6 (a) 错误文案）。
        if !err.message().contains("InvalidInput") {
            return Err(format!(
                "invalid_input iva: error_msg should mention IkError::InvalidInput; got {:?}",
                err.message()
            ));
        }
        eprintln!("[invalid_input] iva yaw nonzero → FailedPrecondition ✓");
    }

    // ivb: garbage payload bytes（绕 gateway control-tier wire gate；
    // 1 KiB 全 0xFF 通过 wire gate < 8 KiB 但 backend fbs verify 必 fail）
    // → backend PayloadDecode → tonic FailedPrecondition。
    {
        let garbage = vec![0xFF_u8; 1024];
        let req = build_invoke_task_cmd(
            &uuidv7_compat(),
            "ik-ivb",
            "ik_backend_smoke",
            "operator-fake-token-001",
            ActionKindProto::ActionKindIk,
            2_000,
            "invalid_input ivb fbs verify fail",
            SCHEMA_IK_PAYLOAD,
            garbage,
        );
        let err = bring_up
            .client
            .invoke_task(req)
            .await
            .err()
            .ok_or("invalid_input ivb: expected Err but got Ok")?;
        if err.code() != tonic::Code::FailedPrecondition {
            return Err(format!(
                "invalid_input ivb: expected FailedPrecondition (backend PayloadDecode → \
                 SubmitError::BackendStartFailed → tonic::Code::FailedPrecondition), \
                 got {:?}: {}",
                err.code(),
                err.message()
            ));
        }
        if !err.message().contains("IkPayload fbs verify failed") {
            return Err(format!(
                "invalid_input ivb: error_msg should mention 'IkPayload fbs verify failed'; \
                 got {:?}",
                err.message()
            ));
        }
        eprintln!("[invalid_input] ivb garbage payload → FailedPrecondition ✓");
    }

    Ok(())
}

/// iva 用一个跟工作空间不冲突的 target（不让 yaw 检查之后的 reachability
/// 检查再 reject）；这里固定 0.20m（XLeRobot 名义工作空间内）。
fn args_safe_target_x() -> f64 {
    0.20
}
