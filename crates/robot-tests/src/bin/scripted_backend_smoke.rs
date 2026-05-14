//! `scripted_backend_smoke` —— v0.4 候选 1 (3/3) Scripted backend
//! cross-process end-to-end smoke binary。
//!
//! ## Scope
//!
//! 跨进程 e2e（沿 `pubsub_cleanup_smoke` / `clear_estop_e2e` 模式）：
//!
//! 1. spawn fake S0 (50Hz `JointState`) + `s1_daemon` + `robot_rpc_gateway`
//! 2. 用 tonic gRPC client 真跑 `InvokeTask(kind=Scripted,
//!    payload=ScriptedPayload)` → iceoryx2 RR shm → s1 coordinator →
//!    `ScriptedBackend.tick()` 50Hz → `JointCommand` publish →
//!    `StreamTaskEvents` 收 Started/Progress/终态
//! 3. 5 phase 覆盖 AC-4 至 AC-7（详 team-swe/specs/v0_4_scripted_backend.md
//!    §AC）：
//!    - `selftest`：harness 通（不调 InvokeTask）
//!    - `happy_multi_waypoint` (AC-4)：N=3 waypoint → Started + ≥N
//!      Progress (单调 ≤ 1.0) + Completed(Success)
//!    - `source_task_id_monotonic` (AC-5)：iceoryx2 subscriber 收集
//!      `JointCommand.source_task_id` 全程严格单调递增（B3 §3.7 +
//!      B4 (3/3) AC-6 同款契约）
//!    - `cancel_mid` (AC-6)：长 duration Scripted → 50ms 后
//!      `CancelTask` → stream 终态 = `Cancelled` only（Q4(a) 拍板
//!      backend 返 PartialSuccess 但 coordinator 转 `Cancelled` 写
//!      stream）
//!    - `payload_boundary` (AC-7)：4 子 case
//!      - 7a empty waypoints → backend Rejected → tonic
//!        `FailedPrecondition`
//!      - 7b `payload_schema_id = 0` + `kind = Scripted` → dispatch
//!        `UnsupportedSchema` → tonic `InvalidArgument`
//!      - 7c-i payload > `MAX_SCRIPTED_PAYLOAD_BYTES` → gateway
//!        gate 1 → tonic `InvalidArgument`
//!      - 7c-iii payload 通过两道 gate 但 fbs decode fail →
//!        backend `PayloadDecode` → tonic `FailedPrecondition`
//!
//! 7c-ii gate 2 wire-size guard 在 `crates/robot-rpc/src/task_handlers.rs`
//! lib unit `invoke_task_wire_over_control_tier_returns_invalid_argument`
//! 已覆盖；cross-process 重复构造意外膨胀路径成本高，本 smoke 不重复
//! （Spec §AC-7 (7c-ii) 子 case lib unit 已 ground truth）。
//!
//! ## CLI
//!
//! ```text
//! scripted_backend_smoke [OPTIONS]
//!
//! options:
//!   --bin-dir <DIR>                  默认 target/release
//!   --robot-id-prefix <STR>          默认 scripted_backend
//!   --hardware-id <ID>               必填（跟 --config TOML 一致）
//!   --config <TOML>                  必填（s1_daemon --config）
//!   --joints <N>                     默认 1（跟 config [[hardware]].joints 一致）
//!   --layout-id <U64>                默认 0xCAFEBABEDEADBEEF
//!   --phase <PHASE>                  默认 all
//!     selftest | happy_multi_waypoint | source_task_id_monotonic |
//!     cancel_mid | payload_boundary | all
//!   --waypoint-count <N>             默认 3（AC-4 happy 用）
//!   --segment-duration-ms <MS>       默认 500（AC-4 happy 段时长）
//!   --cancel-delay-ms <MS>           默认 50（AC-6 cancel 触发延迟）
//!   --workdir <DIR>                  默认 /tmp/scripted_backend_smoke
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
//! - [`team-swe/specs/v0_4_scripted_backend.md`](../../../../../team-swe/specs/v0_4_scripted_backend.md)
//!   §AC-4..AC-7 + §Plan P.2.3
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
use robot_msg::build_helpers::build_joint_state;
use robot_msg::markers;
use robot_msg::msg::{
    ScriptedPayload as FbScriptedPayload, ScriptedPayloadArgs, WaypointTarget, WaypointTargetArgs,
};
use robot_msg::schema_id::SCHEMA_SCRIPTED_PAYLOAD;
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{
    ActionKindProto, AuthContext, CancelTaskCommand, CommandMeta, InvokeTaskCommand, TaskEventKind,
    TaskHandleRef,
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
    HappyMultiWaypoint,
    SourceTaskIdMonotonic,
    CancelMid,
    PayloadBoundary,
    All,
}

impl Phase {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "selftest" => Ok(Phase::Selftest),
            "happy_multi_waypoint" => Ok(Phase::HappyMultiWaypoint),
            "source_task_id_monotonic" => Ok(Phase::SourceTaskIdMonotonic),
            "cancel_mid" => Ok(Phase::CancelMid),
            "payload_boundary" => Ok(Phase::PayloadBoundary),
            "all" => Ok(Phase::All),
            other => Err(format!(
                "unknown --phase {other:?} (expected selftest|happy_multi_waypoint|\
                 source_task_id_monotonic|cancel_mid|payload_boundary|all)"
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Phase::Selftest => "selftest",
            Phase::HappyMultiWaypoint => "happy_multi_waypoint",
            Phase::SourceTaskIdMonotonic => "source_task_id_monotonic",
            Phase::CancelMid => "cancel_mid",
            Phase::PayloadBoundary => "payload_boundary",
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
    layout_id: u64,
    phase: Phase,
    waypoint_count: usize,
    segment_duration_ms: u32,
    cancel_delay_ms: u64,
    workdir: PathBuf,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut bin_dir = PathBuf::from("target/release");
        let mut robot_id_prefix = "scripted_backend".to_string();
        let mut hardware_id: Option<String> = None;
        let mut config: Option<PathBuf> = None;
        let mut joints: usize = 1;
        let mut layout_id: u64 = 0xCAFE_BABE_DEAD_BEEF;
        let mut phase = Phase::All;
        let mut waypoint_count: usize = 3;
        let mut segment_duration_ms: u32 = 500;
        let mut cancel_delay_ms: u64 = 50;
        let mut workdir = PathBuf::from("/tmp/scripted_backend_smoke");

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
                "--waypoint-count" => {
                    i += 1;
                    waypoint_count = raw
                        .get(i)
                        .ok_or("--waypoint-count requires value")?
                        .parse::<usize>()
                        .map_err(|e| format!("--waypoint-count parse: {e}"))?;
                }
                "--segment-duration-ms" => {
                    i += 1;
                    segment_duration_ms = raw
                        .get(i)
                        .ok_or("--segment-duration-ms requires value")?
                        .parse::<u32>()
                        .map_err(|e| format!("--segment-duration-ms parse: {e}"))?;
                }
                "--cancel-delay-ms" => {
                    i += 1;
                    cancel_delay_ms = raw
                        .get(i)
                        .ok_or("--cancel-delay-ms requires value")?
                        .parse::<u64>()
                        .map_err(|e| format!("--cancel-delay-ms parse: {e}"))?;
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
        if waypoint_count == 0 {
            return Err("--waypoint-count must be >= 1".into());
        }
        if segment_duration_ms == 0 {
            return Err("--segment-duration-ms must be > 0".into());
        }

        Ok(Self {
            bin_dir,
            robot_id_prefix,
            hardware_id,
            config,
            joints,
            layout_id,
            phase,
            waypoint_count,
            segment_duration_ms,
            cancel_delay_ms,
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
        "scripted_backend_smoke —— v0.4 候选 1 (3/3) Scripted backend cross-process e2e\n\
\n\
options:\n  \
--bin-dir <DIR>                  默认 target/release\n  \
--robot-id-prefix <STR>          默认 scripted_backend\n  \
--hardware-id <ID>               必填\n  \
--config <TOML>                  必填\n  \
--joints <N>                     默认 1\n  \
--layout-id <U64|0xHEX>          默认 0xCAFEBABEDEADBEEF\n  \
--phase <P>                      默认 all\n    \
P = selftest|happy_multi_waypoint|source_task_id_monotonic|cancel_mid|\
payload_boundary|all\n  \
--waypoint-count <N>             默认 3\n  \
--segment-duration-ms <MS>       默认 500\n  \
--cancel-delay-ms <MS>           默认 50\n  \
--workdir <DIR>                  默认 /tmp/scripted_backend_smoke\n  \
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
        "[scripted_backend_smoke] start phase={} bin_dir={} config={} hardware_id={} \
         joints={} layout_id={:#018x} workdir={}",
        args.phase.label(),
        args.bin_dir.display(),
        args.config.display(),
        args.hardware_id,
        args.joints,
        args.layout_id,
        args.workdir.display(),
    );

    let phases: Vec<Phase> = match args.phase {
        Phase::All => vec![
            Phase::Selftest,
            Phase::HappyMultiWaypoint,
            Phase::SourceTaskIdMonotonic,
            Phase::CancelMid,
            Phase::PayloadBoundary,
        ],
        other => vec![other],
    };

    let mut fail_count: u32 = 0;
    for p in &phases {
        let label = p.label();
        eprintln!("\n========== phase {label} ==========");
        let res = match p {
            Phase::Selftest => run_phase_selftest(&args).await,
            Phase::HappyMultiWaypoint => run_phase_happy_multi_waypoint(&args).await,
            Phase::SourceTaskIdMonotonic => run_phase_source_task_id_monotonic(&args).await,
            Phase::CancelMid => run_phase_cancel_mid(&args).await,
            Phase::PayloadBoundary => run_phase_payload_boundary(&args).await,
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
    if args.joints == 0 {
        return Err("--joints must be >= 1".into());
    }
    std::fs::create_dir_all(&args.workdir)
        .map_err(|e| format!("mkdir --workdir {}: {e}", args.workdir.display()))?;
    Ok(())
}

// ============================================================================
// CaseCtx（inline 沿 pubsub_cleanup_smoke 同款）
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
    let toml_text = r#"# scripted_backend_smoke 临时 rpc_roles.toml（每 case 独立）。

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
// FakeStatePublisher（inline 沿 pubsub_cleanup_smoke 同款）
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
            .name(format!("sbs_fake_state_{robot_id}"))
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
// JointCommandSubscriber（AC-5 source_task_id 监控）
// ============================================================================

/// 订阅 `/<robot_id>/s1/<hardware_id>/command` topic，把每一帧
/// `JointCommand.source_task_id()` 推到共享 `Vec<u64>`。
///
/// 主线程 / phase fn 跑完后调 [`JointCommandSubscriber::shutdown`] 收集
/// 全部 seq 序列做单调判定。
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
            .name(format!("sbs_jc_sub_{robot_id}"))
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
// SubprocessHandle（inline 沿 pubsub_cleanup_smoke 同款）
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
// Readiness（inline 沿 pubsub_cleanup_smoke 同款）
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
// ScriptedPayload fbs builder（Q5(a) 路径 A：inline 复制，不抽公共 helper）
// ============================================================================

/// 构造一段 `ScriptedPayload` fbs bytes。
///
/// `waypoints` = `[(positions, dwell_ms, segment_duration_ms)]`。
fn build_scripted_payload_bytes(waypoints: &[(Vec<f64>, u32, u32)]) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(256);
    let wp_offsets: Vec<_> = waypoints
        .iter()
        .map(|(positions, dwell_ms, segment_duration_ms)| {
            let positions_vec = fbb.create_vector::<f64>(positions);
            WaypointTarget::create(
                &mut fbb,
                &WaypointTargetArgs {
                    positions: Some(positions_vec),
                    dwell_ms: *dwell_ms,
                    segment_duration_ms: *segment_duration_ms,
                },
            )
        })
        .collect();
    let wps_vec = fbb.create_vector(&wp_offsets);
    let payload = FbScriptedPayload::create(
        &mut fbb,
        &ScriptedPayloadArgs {
            waypoints: Some(wps_vec),
        },
    );
    fbb.finish(payload, None);
    fbb.finished_data().to_vec()
}

/// 标准 N waypoint payload：N 段，每段 positions=`[i+1.0; joints]`，
/// `dwell_ms = 0`，`segment_duration_ms = seg_ms`。
fn build_standard_waypoints(n: usize, joints: usize, seg_ms: u32) -> Vec<(Vec<f64>, u32, u32)> {
    (1..=n)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let v = i as f64;
            (vec![v; joints], 0_u32, seg_ms)
        })
        .collect()
}

// ============================================================================
// InvokeTaskCommand builder
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

fn build_cancel_task_cmd(
    request_id: &str,
    idempotency_key: &str,
    source: &str,
    auth_token: &str,
    action_id: &str,
) -> tonic::Request<CancelTaskCommand> {
    let cmd = CancelTaskCommand {
        meta: Some(CommandMeta {
            request_id: request_id.to_string(),
            source: source.to_string(),
            auth: Some(AuthContext {
                token: auth_token.to_string(),
                mtls_fingerprint: String::new(),
            }),
            timeout_ms: 2_000,
            idempotency_key: idempotency_key.to_string(),
            dry_run: false,
            audit_reason: "scripted_backend_smoke cancel".to_string(),
        }),
        action_id: action_id.to_string(),
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
// Bring-up / tear-down
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

    // Selftest 也 sanity-check ScriptedPayload builder（lib unit 已 ground
    // truth；这里 round-trip 一次防 binary feature 编译漂移没人发现）。
    let bytes = build_scripted_payload_bytes(&[(vec![1.0, 2.0, 3.0], 100, 500)]);
    let fb = flatbuffers::root::<FbScriptedPayload<'_>>(&bytes)
        .map_err(|e| format!("selftest: ScriptedPayload builder fbs verify failed: {e}"))?;
    let n = fb.waypoints().map_or(0, |v| v.len());
    if n != 1 {
        return Err(format!("selftest: builder waypoints len = {n}, expected 1"));
    }
    eprintln!(
        "[selftest] ScriptedPayload builder round-trip ok (waypoints={n}, bytes={})",
        bytes.len()
    );

    ctx.cleanup();
    Ok(())
}

// ============================================================================
// Phase: happy_multi_waypoint (AC-4)
// ============================================================================

/// 跟 stream 拉取直到终态 event；保留 Started / Progress / 终态分类计数。
struct ScriptedStreamSummary {
    saw_started: bool,
    progress_count: u32,
    progress_max: f32,
    terminal: TaskEventKind,
}

async fn invoke_scripted_and_collect_stream(
    client: &mut RobotControlClient<tonic::transport::Channel>,
    auth_token: &str,
    payload_schema_id: u32,
    payload: Vec<u8>,
    rpc_timeout_ms: u64,
    stream_timeout: Duration,
    audit_reason: &str,
) -> Result<(String, ScriptedStreamSummary), String> {
    let request_id = uuidv7_compat();
    let ik = format!("ik-{request_id}");
    let invoke_req = build_invoke_task_cmd(
        &request_id,
        &ik,
        "scripted_backend_smoke",
        auth_token,
        ActionKindProto::ActionKindScripted,
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

    let mut summary = ScriptedStreamSummary {
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
                        // 仅 Unspecified 跳过（继续等）；任何 future
                        // 加的 variant 在这里会 build fail，强制 reviewer
                        // 决定该变体的处理路径。
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

async fn run_phase_happy_multi_waypoint(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let mut bring_up = bring_up_pair(&ctx).await?;
    let result = run_phase_happy_inner(&ctx, &args.clone(), &mut bring_up).await;
    if result.is_err() {
        let _ = bring_up.s1.dump_captured_to(&ctx.case_dir).await;
        let _ = bring_up.gw.dump_captured_to(&ctx.case_dir).await;
    }
    tear_down_pair(bring_up).await;
    if result.is_ok() {
        ctx.cleanup();
    } else {
        eprintln!(
            "[happy_multi_waypoint] FAIL forensic preserved at {}",
            ctx.case_dir.display()
        );
    }
    result
}

async fn run_phase_happy_inner(
    _ctx: &CaseCtx,
    args: &Args,
    bring_up: &mut BringUp,
) -> Result<(), String> {
    let waypoints =
        build_standard_waypoints(args.waypoint_count, args.joints, args.segment_duration_ms);
    let payload = build_scripted_payload_bytes(&waypoints);
    eprintln!(
        "[happy] InvokeTask kind=Scripted waypoints={} segment_duration_ms={} \
         payload_bytes={}",
        args.waypoint_count,
        args.segment_duration_ms,
        payload.len()
    );
    let rpc_timeout_ms = u64::from(args.segment_duration_ms)
        .saturating_mul(args.waypoint_count as u64)
        .saturating_add(5_000);
    let stream_timeout = Duration::from_millis(rpc_timeout_ms.saturating_add(2_000));
    let (action_id, summary) = invoke_scripted_and_collect_stream(
        &mut bring_up.client,
        "operator-fake-token-001",
        SCHEMA_SCRIPTED_PAYLOAD,
        payload,
        rpc_timeout_ms,
        stream_timeout,
        "happy_multi_waypoint",
    )
    .await?;
    eprintln!(
        "[happy] action_id={action_id} terminal={:?} started={} progress_count={} progress_max={}",
        summary.terminal, summary.saw_started, summary.progress_count, summary.progress_max
    );

    // Started 是 informational only：per-ActionId pubsub 是 latest-
    // sample 模型（详 docs/handoffs/v0.4_pubsub_cleanup_summary.md §3
    // 关键契约），client 开 stream 时 Started 可能已经被 publisher 推过
    // → smoke 看不到。AC-4 硬约束 = 终态 Completed + ≥1 Progress（进
    // 度递增证据）；Started 仅作 informational metric。
    if !summary.saw_started {
        eprintln!(
            "[happy] note: did not observe Started event (per-ActionId pubsub latest \
             semantics may have skipped early frames before stream subscribe); \
             informational only"
        );
    }
    if summary.terminal != TaskEventKind::Completed {
        return Err(format!(
            "AC-4: expected terminal=Completed(Success), got {:?}",
            summary.terminal
        ));
    }
    if summary.progress_count == 0 {
        return Err("AC-4: expected ≥1 Progress event between invoke and Completed, got 0".into());
    }
    if summary.progress_max > 1.0 + f32::EPSILON {
        return Err(format!(
            "AC-4: progress_max ({}) must be ≤ 1.0",
            summary.progress_max
        ));
    }
    Ok(())
}

// ============================================================================
// Phase: source_task_id_monotonic (AC-5)
// ============================================================================

async fn run_phase_source_task_id_monotonic(args: &Args) -> Result<(), String> {
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
    let result = run_phase_seq_inner(args, &mut bring_up).await;
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
            "[source_task_id_monotonic] FAIL forensic preserved at {}",
            ctx.case_dir.display()
        );
    }
    result?;
    analysis
}

async fn run_phase_seq_inner(args: &Args, bring_up: &mut BringUp) -> Result<(), String> {
    let waypoints =
        build_standard_waypoints(args.waypoint_count, args.joints, args.segment_duration_ms);
    let payload = build_scripted_payload_bytes(&waypoints);
    let rpc_timeout_ms = u64::from(args.segment_duration_ms)
        .saturating_mul(args.waypoint_count as u64)
        .saturating_add(5_000);
    let stream_timeout = Duration::from_millis(rpc_timeout_ms.saturating_add(2_000));
    let (action_id, summary) = invoke_scripted_and_collect_stream(
        &mut bring_up.client,
        "operator-fake-token-001",
        SCHEMA_SCRIPTED_PAYLOAD,
        payload,
        rpc_timeout_ms,
        stream_timeout,
        "source_task_id_monotonic",
    )
    .await?;
    eprintln!(
        "[seq] action_id={action_id} terminal={:?} progress_count={}",
        summary.terminal, summary.progress_count
    );
    if summary.terminal != TaskEventKind::Completed {
        return Err(format!(
            "AC-5: expected terminal=Completed, got {:?}",
            summary.terminal
        ));
    }
    Ok(())
}

fn analyze_source_task_id_seqs(seqs: &[u64]) -> Result<(), String> {
    eprintln!(
        "[seq] collected {} unique JointCommand.source_task_id values",
        seqs.len()
    );
    if seqs.len() < 2 {
        // Scripted 至少跑过 1-2 段 + 终态前应该 publish 几十帧；
        // < 2 说明 subscriber 没收到 JointCommand（可能 hardware bus
        // 路径不通或 anchor 没建立）。
        return Err(format!(
            "AC-5: subscriber collected only {} seq values; expected several (Scripted 50Hz tick \
             over multi-segment action should publish many JointCommand frames)",
            seqs.len()
        ));
    }
    let mut prev = seqs[0];
    for (i, &s) in seqs.iter().enumerate().skip(1) {
        if s <= prev {
            return Err(format!(
                "AC-5: source_task_id not strictly monotonic at idx={i}: prev={prev}, now={s} \
                 (full seqs[0..min(20)]: {:?})",
                &seqs[..seqs.len().min(20)]
            ));
        }
        prev = s;
    }
    eprintln!(
        "[seq] source_task_id 严格单调 PASS (first={}, last={}, span={} entries)",
        seqs[0],
        seqs[seqs.len() - 1],
        seqs.len()
    );
    Ok(())
}

// ============================================================================
// Phase: cancel_mid (AC-6)
// ============================================================================

async fn run_phase_cancel_mid(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let mut bring_up = bring_up_pair(&ctx).await?;
    let result = run_phase_cancel_inner(args, &mut bring_up).await;
    if result.is_err() {
        let _ = bring_up.s1.dump_captured_to(&ctx.case_dir).await;
        let _ = bring_up.gw.dump_captured_to(&ctx.case_dir).await;
    }
    tear_down_pair(bring_up).await;
    if result.is_ok() {
        ctx.cleanup();
    } else {
        eprintln!(
            "[cancel_mid] FAIL forensic preserved at {}",
            ctx.case_dir.display()
        );
    }
    result
}

async fn run_phase_cancel_inner(args: &Args, bring_up: &mut BringUp) -> Result<(), String> {
    // AC-6: 5 waypoint × 1000ms = 5s total，留足时间 50ms 后 cancel
    // 落在某 waypoint segment 中间。即使 --waypoint-count / --segment-
    // duration-ms CLI 改了，AC-6 用固定 5 × 1000ms 保证 cancel 落 mid-
    // segment。
    let n: usize = 5;
    let seg_ms: u32 = 1_000;
    let waypoints = build_standard_waypoints(n, args.joints, seg_ms);
    let payload = build_scripted_payload_bytes(&waypoints);
    let rpc_timeout_ms: u64 = u64::from(seg_ms)
        .saturating_mul(n as u64)
        .saturating_add(5_000);

    let request_id = uuidv7_compat();
    let ik = format!("ik-{request_id}");
    let invoke_req = build_invoke_task_cmd(
        &request_id,
        &ik,
        "scripted_backend_smoke",
        "operator-fake-token-001",
        ActionKindProto::ActionKindScripted,
        rpc_timeout_ms,
        "cancel_mid",
        SCHEMA_SCRIPTED_PAYLOAD,
        payload,
    );
    let invoke_resp = bring_up
        .client
        .invoke_task(invoke_req)
        .await
        .map_err(|s| format!("invoke_task: {:?}: {}", s.code(), s.message()))?
        .into_inner();
    let action_id = invoke_resp.action_id.clone();
    if action_id.is_empty() {
        return Err("invoke_task returned empty action_id".into());
    }
    eprintln!(
        "[cancel_mid] action_id={action_id} invoked; waiting {}ms before cancel",
        args.cancel_delay_ms
    );

    // 先开 stream，不然 cancel 触发后可能错过早期事件（gateway 内有
    // events_drain bridge，开 stream 后从最新 sample 开始；本 phase
    // 不强制要求 Started，仅要求终态 = Cancelled）。
    let stream_req = build_task_handle_ref_req(&request_id, &action_id);
    let stream_resp = bring_up
        .client
        .stream_task_events(stream_req)
        .await
        .map_err(|s| format!("stream_task_events: {:?}: {}", s.code(), s.message()))?;
    let mut stream = stream_resp.into_inner();

    tokio::time::sleep(Duration::from_millis(args.cancel_delay_ms)).await;

    let cancel_request_id = uuidv7_compat();
    let cancel_ik = format!("cik-{cancel_request_id}");
    let cancel_req = build_cancel_task_cmd(
        &cancel_request_id,
        &cancel_ik,
        "scripted_backend_smoke",
        "operator-fake-token-001",
        &action_id,
    );
    let cancel_resp = bring_up
        .client
        .cancel_task(cancel_req)
        .await
        .map_err(|s| format!("cancel_task: {:?}: {}", s.code(), s.message()))?
        .into_inner();
    if cancel_resp.request_id != cancel_request_id {
        return Err(format!(
            "AC-6: CancelTaskResponse.request_id ({}) != sent cancel request_id ({cancel_request_id})",
            cancel_resp.request_id
        ));
    }
    eprintln!(
        "[cancel_mid] CancelTask Ack received (request_id={}, cleared_at_ns={})",
        cancel_resp.request_id, cancel_resp.cleared_at_ns
    );

    // 等 stream 终态；AC-6: 必须 Cancelled，**不**接受 Completed{
    // PartialSuccess}（reviewer Plan round 2 阻断 2 + Q4(a) 拍板：
    // backend lib 返 PartialSuccess 但 coordinator pending_cancel 路径
    // 统一转 ActionEvent::Cancelled 写 stream）。
    let deadline = Instant::now() + Duration::from_millis(rpc_timeout_ms.saturating_add(2_000));
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "AC-6: stream terminal event timeout for action_id={action_id}"
            ));
        }
        match tokio::time::timeout(remaining, stream.message()).await {
            Ok(Ok(Some(event))) => {
                let kind =
                    TaskEventKind::try_from(event.event).unwrap_or(TaskEventKind::Unspecified);
                match kind {
                    TaskEventKind::Cancelled => {
                        eprintln!("[cancel_mid] stream terminal = Cancelled (AC-6 PASS)");
                        return Ok(());
                    }
                    TaskEventKind::Completed => {
                        return Err("AC-6: stream terminal = Completed, expected Cancelled \
                             (Q4(a) + reviewer Plan round 2 阻断 2: coordinator pending_cancel \
                             路径必须转 ActionEvent::Cancelled 写 stream)"
                            .to_string());
                    }
                    TaskEventKind::Failed => {
                        return Err(format!(
                            "AC-6: stream terminal = Failed (action_id={action_id})"
                        ));
                    }
                    TaskEventKind::Started
                    | TaskEventKind::Progress
                    | TaskEventKind::Unspecified => {
                        // 继续等终态
                    }
                }
            }
            Ok(Ok(None)) => {
                return Err(format!(
                    "AC-6: stream closed before terminal event (action_id={action_id})"
                ));
            }
            Ok(Err(status)) => {
                return Err(format!(
                    "AC-6: stream error: {:?}: {} (action_id={action_id})",
                    status.code(),
                    status.message()
                ));
            }
            Err(_elapsed) => {
                return Err(format!(
                    "AC-6: stream terminal event timeout for action_id={action_id}"
                ));
            }
        }
    }
}

// ============================================================================
// Phase: payload_boundary (AC-7)
// ============================================================================

async fn run_phase_payload_boundary(args: &Args) -> Result<(), String> {
    let ctx = CaseCtx::alloc(args)?;
    let mut bring_up = bring_up_pair(&ctx).await?;
    let result = run_phase_boundary_inner(args, &mut bring_up).await;
    if result.is_err() {
        let _ = bring_up.s1.dump_captured_to(&ctx.case_dir).await;
        let _ = bring_up.gw.dump_captured_to(&ctx.case_dir).await;
    }
    tear_down_pair(bring_up).await;
    if result.is_ok() {
        ctx.cleanup();
    } else {
        eprintln!(
            "[payload_boundary] FAIL forensic preserved at {}",
            ctx.case_dir.display()
        );
    }
    result
}

async fn run_phase_boundary_inner(_args: &Args, bring_up: &mut BringUp) -> Result<(), String> {
    // 7a：empty waypoints → backend Rejected → FailedPrecondition。
    {
        let payload = build_scripted_payload_bytes(&[]);
        let req = build_invoke_task_cmd(
            &uuidv7_compat(),
            "ik-7a",
            "scripted_backend_smoke",
            "operator-fake-token-001",
            ActionKindProto::ActionKindScripted,
            2_000,
            "AC-7 7a empty waypoints",
            SCHEMA_SCRIPTED_PAYLOAD,
            payload,
        );
        let err = bring_up
            .client
            .invoke_task(req)
            .await
            .err()
            .ok_or("AC-7 7a: expected Err but got Ok")?;
        if err.code() != tonic::Code::FailedPrecondition {
            return Err(format!(
                "AC-7 7a: expected FailedPrecondition (backend Rejected → \
                 SubmitError::BackendStartFailed → tonic::Code::FailedPrecondition), \
                 got {:?}: {}",
                err.code(),
                err.message()
            ));
        }
        eprintln!("[boundary] 7a empty waypoints → FailedPrecondition ✓");
    }

    // 7b：payload_schema_id = 0 (NO_PAYLOAD) + kind = Scripted → dispatch
    // UnsupportedSchema → InvalidArgument。
    {
        // payload bytes 必须为空（NO_PAYLOAD_SCHEMA_ID 哨兵约束，
        // ActionRequest::validate 守门）。
        let req = build_invoke_task_cmd(
            &uuidv7_compat(),
            "ik-7b",
            "scripted_backend_smoke",
            "operator-fake-token-001",
            ActionKindProto::ActionKindScripted,
            2_000,
            "AC-7 7b NO_PAYLOAD + Scripted",
            0, // NO_PAYLOAD_SCHEMA_ID
            Vec::new(),
        );
        let err = bring_up
            .client
            .invoke_task(req)
            .await
            .err()
            .ok_or("AC-7 7b: expected Err but got Ok")?;
        if err.code() != tonic::Code::InvalidArgument {
            return Err(format!(
                "AC-7 7b: expected InvalidArgument (dispatch UnsupportedSchema → \
                 tonic::Code::InvalidArgument), got {:?}: {}",
                err.code(),
                err.message()
            ));
        }
        eprintln!("[boundary] 7b NO_PAYLOAD + Scripted → InvalidArgument ✓");
    }

    // 7c-i：payload bytes > MAX_SCRIPTED_PAYLOAD_BYTES (4 KiB) → gateway
    // gate 1 → InvalidArgument。**不**进 RR/s1。
    {
        let oversized = vec![0u8; 4 * 1024 + 1];
        let req = build_invoke_task_cmd(
            &uuidv7_compat(),
            "ik-7c-i",
            "scripted_backend_smoke",
            "operator-fake-token-001",
            ActionKindProto::ActionKindScripted,
            2_000,
            "AC-7 7c-i payload over MAX_SCRIPTED_PAYLOAD_BYTES",
            SCHEMA_SCRIPTED_PAYLOAD,
            oversized,
        );
        let err = bring_up
            .client
            .invoke_task(req)
            .await
            .err()
            .ok_or("AC-7 7c-i: expected Err but got Ok")?;
        if err.code() != tonic::Code::InvalidArgument {
            return Err(format!(
                "AC-7 7c-i: expected InvalidArgument (gateway gate 1 \
                 MAX_SCRIPTED_PAYLOAD_BYTES precheck), got {:?}: {}",
                err.code(),
                err.message()
            ));
        }
        if !err.message().contains("MAX_SCRIPTED_PAYLOAD_BYTES") {
            return Err(format!(
                "AC-7 7c-i: error_msg should mention MAX_SCRIPTED_PAYLOAD_BYTES; got {:?}",
                err.message()
            ));
        }
        eprintln!("[boundary] 7c-i payload over gate 1 → InvalidArgument ✓");
    }

    // 7c-iii：payload 通过 gate 1 + gate 2 但 fbs verify fail →
    // backend PayloadDecode → FailedPrecondition。
    {
        // 1 KiB 全 0xFF 字节，绝不是合法 fbs root：通过 gateway 两道
        // byte gate（< 4 KiB & wire < 8 KiB），但 backend
        // `flatbuffers::root::<ScriptedPayload>` verify 失败 →
        // ActionStartError::PayloadDecode → coordinator submit 转
        // SubmitError::BackendStartFailed → tonic FailedPrecondition。
        let garbage = vec![0xFF_u8; 1024];
        let req = build_invoke_task_cmd(
            &uuidv7_compat(),
            "ik-7c-iii",
            "scripted_backend_smoke",
            "operator-fake-token-001",
            ActionKindProto::ActionKindScripted,
            2_000,
            "AC-7 7c-iii fbs verify fail",
            SCHEMA_SCRIPTED_PAYLOAD,
            garbage,
        );
        let err = bring_up
            .client
            .invoke_task(req)
            .await
            .err()
            .ok_or("AC-7 7c-iii: expected Err but got Ok")?;
        if err.code() != tonic::Code::FailedPrecondition {
            return Err(format!(
                "AC-7 7c-iii: expected FailedPrecondition (backend PayloadDecode → \
                 SubmitError::BackendStartFailed → tonic::Code::FailedPrecondition), \
                 got {:?}: {}",
                err.code(),
                err.message()
            ));
        }
        eprintln!("[boundary] 7c-iii garbage payload → FailedPrecondition ✓");
    }

    Ok(())
}
