//! `supervised_demo` —— v0.5 候选 E 段(10-demo) 真机受监督 demo 验收 bin
//! （Spec D10 拍板 (c)：专用验收 binary 为主，phase 化，机器可判 + 人眼可看；
//! robotctl 作旁路证据面，不新增子命令）。
//!
//! ## 安全模型（D15-R (c1+) 显式豁免下执行；round 1 P0-1/P0-2/P0-3 回修后）
//!
//! - **任何会上扭的 phase 都要求 `--ack-safety-checklist`**：这是 AC-E12
//!   安全边界五条的显式确认位——不带该 flag 时打印 checklist 并退 2，
//!   **绝不 `SetEnabled`**。
//! - **统一 fail-closed finalizer**（round 1 P0-1）：`SetEnabled` 的语义是
//!   **失败响应不代表没上扭**（deadline 不回滚副作用）——故从**第一次
//!   enable 尝试起**，任何退出路径都必经收尾：
//!   * 成功路径：disable 必须**确认**（RPC Ok + readback 模式离开
//!     Enabled；hold 则必须为新帧 Safe/TorqueOffConfirmed/fault0）；未确认 → 立即 `EStop` 并判 FAIL；
//!   * 失败路径：有已知未终态 action 先 best-effort cancel → `EStop` →
//!     FAIL；双臂 phase 右侧 enable 结果不确定 → **双侧 `EStop`**（左已上
//!     扭）；`EStop` 自身失败并进报告并点名人工断电，不吞。
//! - **客户端本地硬超时**（round 1 P0-2）：tonic `Request::set_timeout` 只写
//!   `grpc-timeout` metadata（服务端强制；连接黑洞时客户端永等）——本 bin
//!   每次 RPC 另包 `tokio::time::timeout`（[`CLIENT_HARD_TIMEOUT`]），收尾
//!   动作（cancel / disable / `EStop`）同样有界，绝不因一次挂死挡住后续
//!   `EStop`。（双臂 phase 协议内 RPC 由 `dual_arm_workflow` 的 `rpc_timeout`
//!   / `estop_timeout` 包裹，本 bin 不重复。）
//! - **实际运动证据**（round 1 P0-3）：`Completed(Success)` 不构成"电机动过"
//!   ——Scripted 完成由时钟推进、S0 写失败只计 metric、最终位置可能一直是
//!   旧 anchor。每个探针 phase 带 [`MotionWitness`]：以**新鲜样本**
//!   （`stamp_ns` 严格推进才计入）观测探针关节，**出程**必须真实到达
//!   `anchor+offset ∓ 容差`、**回程**回落 anchor ± 容差；容差有硬上限且
//!   `offset ≥ 2.5×容差`（两判据阈值不相交，卡死在中间点不可能双过）。
//!   证据不足同样走 `EStop` 收尾并 FAIL。
//! - **数值硬 gate（AC-E12 ③）**：offset ∈ (0, 0.087]、**wire 实际斜率**
//!   ≤ 0.14 rad/s——段时长按 ms **向上取整**并对量化后的值复核（round 1
//!   P1-1：转换器四舍五入到 ms，浮点秒上的校验会被量化绕过）。超限拒绝
//!   提交，不 clamp。
//!
//! ## phase（各自独立调用 = operator 显式；`all` 串行，一步 FAIL 即中止）
//!
//! | phase | 上扭 | 判据 |
//! |---|---|---|
//! | `read-state` | 否 | `GetState` 可达 + `layout_id` == config 派生 + `stamp_ns` 在推进 |
//! | `hold` | 是 | 仅左侧，以实际 S1 锚点保持20秒；不提交任务，严格确认断扭 |
//! | `single-arm` | 是 | enable → 探针 → `Completed(Success)` + **witness 出程/回程证据** → disable 确认 |
//! | `dual-arm`   | 是 | 双侧 enable → `run_chunk_pair` → `PairResult::Success` + **双侧 witness** → 双侧 disable 确认 |
//! | `grippers`   | 是 | 同 dual-arm，探针关节 = 双侧 `*_gripper`（D6 Scripted 同车） |
//!
//! ## 与 e2e 的关系（PD-E3 / D9 同向消费）
//!
//! 协议语义 = `robot_rpc::client::dual_arm_workflow`（单源）；本 bin 的
//! [`DemoArm`] 是 `ArmEndpoint` 的第二份**纯 I/O** tonic 适配（第一份 =
//! `robot-rpc/tests/common/pair_endpoint.rs` 的 `TonicArm`，测试 harness
//! 不可被 bin import）——两者无语义分叉，PX-12 同款并存。

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tonic::transport::Channel;

#[cfg(feature = "supervised-s0")]
use robot_core::time::RobotClock;
use robot_core::time::SystemClock;
use robot_rpc::client::dual_arm_workflow::{
    ActionSpec, ArmEndpoint, ArmPair, ArmRpcError, OutcomeKind, PairConfig, PairResult, Side,
    SubmitFailure, TaskPhase, TaskSnapshot, TerminalState, run_chunk_pair,
};
use robot_rpc::client::layout::{HardwareLayout, load_hardware_layout, validate_state_layout};
use robot_rpc::client::trajectory::{
    TrajectoryInput, TrajectoryPoint, build_scripted_payload, to_scripted_waypoints,
};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{
    AuthContext, CancelTaskCommand, CommandMeta, EStopCommand, GetStateRequest, InvokeTaskCommand,
    RobotState, S0Mode, SetEnabledCommand, TaskHandleRef,
};

#[cfg(all(target_os = "linux", feature = "supervised-s0"))]
#[path = "../supervised_mixed.rs"]
mod supervised_mixed;

// ============================================================================
// 数值硬 gate（AC-E12 ③；改动必须回 Spec）
// ============================================================================

/// 探针偏移上限：|target − anchor| ≤ 0.087 rad（≈ 5°，round 8 P1-2 定值）。
const MAX_PROBE_OFFSET_RAD: f64 = 0.087;
/// 轨迹最大斜率上限（rad/s）——按 **wire 量化后**的段时长复核。
const MAX_SLOPE_RAD_PER_S: f64 = 0.14;
/// 缺省探针偏移（≤ 上限，留余量）。
const DEFAULT_PROBE_OFFSET_RAD: f64 = 0.05;
/// 缺省斜率（≤ 上限，留余量；缺省段时长 = offset / slope，**向上取整 ms**）。
const DEFAULT_SLOPE_RAD_PER_S: f64 = 0.10;
/// 回落 anchor 容差缺省（rad）：真机 hold 噪声 ±10 tick ≈ 0.015 rad 留余量。
const DEFAULT_SETTLE_TOLERANCE_RAD: f64 = 0.02;
/// 容差硬上限（round 1 P0-3：无上限时大容差可放空回落判据）。
const MAX_SETTLE_TOLERANCE_RAD: f64 = 0.05;
/// **判据不相交约束**：`offset ≥ TOLERANCE_SEPARATION × tolerance`——出程
/// 阈值（offset − tol）与回程阈值（tol）之间留 ≥ 0.5×tol 的空隙，卡死在
/// 中间点不可能同时满足两判据。
const TOLERANCE_SEPARATION: f64 = 2.5;
/// witness 的最小新鲜样本数（执行 ≥ 1 s、采样 100 ms 周期下远高于此）。
const MIN_WITNESS_SAMPLES: usize = 3;

/// 单次 RPC 的 `CommandMeta.timeout_ms`（gateway 侧 RR 等待，最内层）。
const RPC_TIMEOUT_MS: u64 = 2_000;
/// `grpc-timeout` metadata（服务端强制；> meta 超时）。
const GRPC_TIMEOUT: Duration = Duration::from_millis(RPC_TIMEOUT_MS + 1_000);
/// **客户端本地硬超时**（round 1 P0-2）：> `GRPC_TIMEOUT`，连接黑洞时由
/// 本端计时器兜底——收尾动作（cancel / disable / `EStop`）同受此界。
const CLIENT_HARD_TIMEOUT: Duration = Duration::from_secs(4);
/// `SetEnabled` **成功返回后**等 Enabled 的轮询窗（round 3 口径统一：这
/// **不是**阶段整体上限——整体硬上限 = [`PROD_STAGE_BUDGETS`].enable 9 s，
/// 由外层 `staged()` 强制，PX-53）。
const ENABLE_WAIT: Duration = Duration::from_secs(5);
/// disable 确认 readback（模式离开 Enabled）的轮询上限。
const DISABLE_CONFIRM_WAIT: Duration = Duration::from_secs(2);
/// `read-state` 两次采样的间隔（stamp 推进判据；50 Hz 发布下必然跨帧）。
const STATE_ADVANCE_PROBE: Duration = Duration::from_millis(300);
/// 终态 / witness 轮询周期。
const STATUS_POLL: Duration = Duration::from_millis(100);
/// 探针执行完到回程判据之间的静置窗（hold 回稳）。
const SETTLE_WAIT: Duration = Duration::from_millis(600);
/// witness 采样任务的有界 join 上限。
const SAMPLER_JOIN: Duration = Duration::from_secs(2);
/// sampler `abort()` 后等**取消确认**的上限（round 2 P1-4：超时丢弃
/// `JoinHandle` = detach，必须显式 abort 并有界确认）。
const SAMPLER_ABORT_JOIN: Duration = Duration::from_secs(1);

/// **阶段级墙钟硬上限**（round 2 P0-3）：单 RPC 有界 ≠ 阶段有界——enable
/// 的轮询、disable readback、执行期轮询都可能被"最后一次 4 s RPC"穿透
/// 各自的内层 deadline。每个阶段整体再包一层 `tokio::time::timeout`。
#[derive(Debug, Clone, Copy)]
struct StageBudgets {
    /// enable 阶段（`SetEnabled` RPC + 轮询到 Enabled）总预算。
    enable: Duration,
    /// disable 确认阶段（`SetEnabled(false)` + readback）总预算。
    disable: Duration,
    /// settle 阶段（静置 + 回程采样 + 终判）总预算。
    settle: Duration,
}

/// 生产预算 = 内层健康路径上限 + 一次 [`CLIENT_HARD_TIMEOUT`] 的穿透余量。
const PROD_STAGE_BUDGETS: StageBudgets = StageBudgets {
    enable: Duration::from_secs(9),  // 4 (SetEnabled) + 5 (ENABLE_WAIT)
    disable: Duration::from_secs(6), // 4 (SetEnabled) + 2 (readback)
    settle: Duration::from_secs(10), // 0.6 静置 + 2×4 采样 + 余量
};

/// 阶段级 timeout 包装（超时 → 带 label 的 Err，交给 finalizer 收尾）。
async fn staged<T>(
    budget: Duration,
    label: &str,
    fut: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    match tokio::time::timeout(budget, fut).await {
        Ok(r) => r,
        Err(_) => Err(format!(
            "{label}: stage wall-clock budget {budget:?} exceeded"
        )),
    }
}

/// AC-E12 安全边界五条（`--ack-safety-checklist` 确认对象；P.2.10 权威）。
const SAFETY_CHECKLIST: &str = "\
AC-E12 安全边界五条（全部满足才允许上扭 phase）：
  1. 机械隔离、工作区无人、无负载；操作者全程在场且可立即切断总电源；
  2. auto_enable=false 已生效；每次上扭均为本 bin 的显式 SetEnabled(true)；
  3. 先单臂低速/小幅（|target−anchor| ≤ 0.087 rad、斜率 ≤ 0.14 rad/s）再双臂；
     单侧异常立即 cancel 双侧并双发 EStop（dual phase 由协议模块保证）；
  4. 旧栈保留可恢复启动路径（stop-only，未 disable / 未删除）；
  5. Enabled 期间任一进程（S0/S1/gateway/本 bin）死亡或挂起 → 立即人工切断
     总电源并中止本轮（不得只依赖 EStop 弱 Ack）。
确认全部满足后，在命令行追加 --ack-safety-checklist 重新运行。";

// ============================================================================
// CLI
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    ReadState,
    Hold,
    SingleArm,
    DualArm,
    Grippers,
    All,
}

impl Phase {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "read-state" => Ok(Self::ReadState),
            "hold" => Ok(Self::Hold),
            "single-arm" => Ok(Self::SingleArm),
            "dual-arm" => Ok(Self::DualArm),
            "grippers" => Ok(Self::Grippers),
            "all" => Ok(Self::All),
            other => Err(format!(
                "--phase: unknown {other:?} (read-state|hold|single-arm|dual-arm|grippers|all)"
            )),
        }
    }

    /// 该 phase 是否会 `SetEnabled(true)`（= 需要 checklist 确认）。
    fn enables_torque(self) -> bool {
        !matches!(self, Self::ReadState)
    }
}

/// 探针几何约束（CLI 与 [`Probe`] 构造点共用的**纯**校验；round 1 P0-3
/// 容差上限 + 判据不相交）。
fn validate_probe_geometry(offset_rad: f64, tolerance_rad: f64) -> Result<(), String> {
    if !(offset_rad.is_finite() && offset_rad > 0.0 && offset_rad <= MAX_PROBE_OFFSET_RAD) {
        return Err(format!(
            "probe offset {offset_rad} out of (0, {MAX_PROBE_OFFSET_RAD}] —— AC-E12 ③ 硬上限，拒绝提交"
        ));
    }
    if !(tolerance_rad.is_finite()
        && tolerance_rad > 0.0
        && tolerance_rad <= MAX_SETTLE_TOLERANCE_RAD)
    {
        return Err(format!(
            "settle tolerance {tolerance_rad} out of (0, {MAX_SETTLE_TOLERANCE_RAD}]"
        ));
    }
    // 出程阈值 (offset − tol) 与回程阈值 (tol) 必须留空隙——否则卡死在
    // 中间点可同时满足两判据（round 1 P0-3）。
    if offset_rad < TOLERANCE_SEPARATION * tolerance_rad {
        return Err(format!(
            "offset {offset_rad} < {TOLERANCE_SEPARATION} × tolerance {tolerance_rad} —— \
             出程/回程判据不相交约束不满足（大容差会放空运动证据）"
        ));
    }
    Ok(())
}

struct Args {
    phase: Phase,
    /// `single-arm` 的目标臂；`hold` 固定为 left。
    side: Side,
    left_gateway: String,
    right_gateway: String,
    left_config: PathBuf,
    right_config: PathBuf,
    left_hardware_id: String,
    right_hardware_id: String,
    token: String,
    probe_offset_rad: f64,
    /// 探针关节名（缺省 = joints 里首个含 `wrist_roll` 的；找不到则要求显式）。
    probe_joint: Option<String>,
    settle_tolerance_rad: f64,
    /// 本次 S1 anchored 日志中的固定 bias 数组；外壳负责 PID/Invocation 绑定。
    hold_anchor_rad: Option<String>,
    ack_safety_checklist: bool,
}

fn value<'a>(raw: &'a [String], i: &mut usize, opt: &str) -> Result<&'a str, String> {
    *i += 1;
    raw.get(*i)
        .map(String::as_str)
        .ok_or_else(|| format!("{opt} requires a value"))
}

impl Args {
    #[allow(clippy::too_many_lines)]
    fn parse() -> Result<Self, String> {
        let raw: Vec<String> = std::env::args().skip(1).collect();
        Self::parse_from(&raw)
    }

    #[allow(clippy::too_many_lines)]
    fn parse_from(raw: &[String]) -> Result<Self, String> {
        let mut args = Self {
            phase: Phase::ReadState,
            side: Side::Left,
            left_gateway: "http://127.0.0.1:50051".into(),
            right_gateway: "http://127.0.0.1:50052".into(),
            left_config: PathBuf::from("/etc/robot/xlerobot-left-arm.toml"),
            right_config: PathBuf::from("/etc/robot/xlerobot-right-arm.toml"),
            left_hardware_id: "left_arm".into(),
            right_hardware_id: "right_arm".into(),
            token: std::env::var("ROBOT_TOKEN").unwrap_or_default(),
            probe_offset_rad: DEFAULT_PROBE_OFFSET_RAD,
            probe_joint: None,
            settle_tolerance_rad: DEFAULT_SETTLE_TOLERANCE_RAD,
            hold_anchor_rad: None,
            ack_safety_checklist: false,
        };
        let mut phase_seen = false;
        let mut probe_option_seen = false;
        let mut i = 0;
        while i < raw.len() {
            match raw[i].as_str() {
                "--phase" => {
                    args.phase = Phase::parse(value(raw, &mut i, "--phase")?)?;
                    phase_seen = true;
                }
                "--side" => {
                    args.side = match value(raw, &mut i, "--side")? {
                        "left" => Side::Left,
                        "right" => Side::Right,
                        other => return Err(format!("--side: unknown {other:?} (left|right)")),
                    };
                }
                "--left-gateway" => {
                    args.left_gateway = normalize_endpoint(value(raw, &mut i, "--left-gateway")?);
                }
                "--right-gateway" => {
                    args.right_gateway = normalize_endpoint(value(raw, &mut i, "--right-gateway")?);
                }
                "--left-config" => {
                    args.left_config = PathBuf::from(value(raw, &mut i, "--left-config")?);
                }
                "--right-config" => {
                    args.right_config = PathBuf::from(value(raw, &mut i, "--right-config")?);
                }
                "--left-hardware-id" => {
                    args.left_hardware_id = value(raw, &mut i, "--left-hardware-id")?.into();
                }
                "--right-hardware-id" => {
                    args.right_hardware_id = value(raw, &mut i, "--right-hardware-id")?.into();
                }
                "--token" => args.token = value(raw, &mut i, "--token")?.into(),
                "--probe-offset-rad" => {
                    probe_option_seen = true;
                    args.probe_offset_rad = value(raw, &mut i, "--probe-offset-rad")?
                        .parse()
                        .map_err(|e| format!("--probe-offset-rad: {e}"))?;
                }
                "--probe-joint" => {
                    probe_option_seen = true;
                    args.probe_joint = Some(value(raw, &mut i, "--probe-joint")?.into());
                }
                "--settle-tolerance-rad" => {
                    probe_option_seen = true;
                    args.settle_tolerance_rad = value(raw, &mut i, "--settle-tolerance-rad")?
                        .parse()
                        .map_err(|e| format!("--settle-tolerance-rad: {e}"))?;
                }
                "--hold-anchor-rad" => {
                    args.hold_anchor_rad = Some(value(raw, &mut i, "--hold-anchor-rad")?.into());
                }
                "--ack-safety-checklist" => args.ack_safety_checklist = true,
                "-h" | "--help" => return Err("HELP".into()),
                other => return Err(format!("unknown option {other:?}")),
            }
            i += 1;
        }
        if !phase_seen {
            return Err("--phase is required".into());
        }
        if args.token.is_empty() {
            return Err("--token (or env ROBOT_TOKEN) is required".into());
        }
        if args.phase == Phase::Hold {
            if !cfg!(feature = "supervised-s0") {
                return Err("hold 需要 supervised-s0 feature".into());
            }
            if args.side != Side::Left || probe_option_seen || args.hold_anchor_rad.is_none() {
                return Err(
                    "hold 仅支持左侧，必须提供 --hold-anchor-rad，禁止探针或容差覆盖".into(),
                );
            }
        } else if args.hold_anchor_rad.is_some() {
            return Err("--hold-anchor-rad 仅用于 hold".into());
        }
        validate_probe_geometry(args.probe_offset_rad, args.settle_tolerance_rad)?;
        Ok(args)
    }
}

/// `host:port` → `http://host:port`（tonic endpoint 需要 scheme）。
fn normalize_endpoint(s: &str) -> String {
    if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else {
        format!("http://{s}")
    }
}

fn print_help() {
    println!(
        "\
supervised_demo —— XLeRobot 真机受监督 demo 验收 bin（v0.5 候选 E 段(10-demo)，D10 (c)）

USAGE:
  supervised_demo --phase <PHASE> [OPTIONS]

PHASE:
  read-state    只读：GetState 双臂（layout 校验 + stamp 推进）；不上扭
  hold          仅左侧固定保持20秒；S1唯一发布命令，不提交探针任务
  single-arm    单臂探针（--side left|right）：enable → 出→回 + 运动证据 → disable 确认
  dual-arm      双臂 run_chunk_pair 探针（协议级 cancel/EStop 收尾 + 双侧运动证据）
  grippers      双夹爪探针（D6 Scripted 同车；关节 = *_gripper）
  all           串行全部（read-state → single-arm×2 → dual-arm → grippers）

OPTIONS:
  --side <left|right>            single-arm 的目标臂；hold仅left（默认 left）
  --left-gateway <ADDR>          默认 127.0.0.1:50051
  --right-gateway <ADDR>         默认 127.0.0.1:50052
  --left-config <PATH>           默认 /etc/robot/xlerobot-left-arm.toml
  --right-config <PATH>          默认 /etc/robot/xlerobot-right-arm.toml
  --left-hardware-id <ID>        默认 left_arm
  --right-hardware-id <ID>       默认 right_arm
  --token <TOKEN>                operator+ token（或 env ROBOT_TOKEN）
  --hold-anchor-rad <JSON>       本次S1启动锚点数组；hold必填，固定±4raw
  --probe-offset-rad <F>         探针偏移（默认 {DEFAULT_PROBE_OFFSET_RAD}；硬上限 {MAX_PROBE_OFFSET_RAD}，超限拒绝）
  --probe-joint <NAME>           探针关节名（默认自动选 *wrist_roll*）
  --settle-tolerance-rad <F>     回落容差（默认 {DEFAULT_SETTLE_TOLERANCE_RAD}；上限 {MAX_SETTLE_TOLERANCE_RAD} 且 offset ≥ {TOLERANCE_SEPARATION}×容差）
  --ack-safety-checklist         确认 AC-E12 安全边界五条（上扭 phase 必带）

退出码：0 全 PASS / 1 有 FAIL / 2 CLI 或前置错误。"
    );
}

// ============================================================================
// UUIDv7（gateway `validate_request_id` 硬要求；与既有 smoke 同款 compat）
// ============================================================================

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
        bytes[15]
    )
}

// ============================================================================
// 有界 RPC（round 1 P0-2）
// ============================================================================

/// 每次 RPC 的服务端超时：`grpc-timeout` metadata（**只**由服务端强制——
/// tonic 0.14 的 `set_timeout` 不启动客户端计时器）。
fn with_grpc_timeout<T>(msg: T) -> tonic::Request<T> {
    let mut req = tonic::Request::new(msg);
    req.set_timeout(GRPC_TIMEOUT);
    req
}

/// **客户端本地硬超时**包裹：连接黑洞 / 服务端不守约时由本端计时器兜底。
/// 收尾路径（cancel / disable / `EStop`）同样经此，绝不无限等待。
async fn bounded<T, F>(label: String, fut: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<tonic::Response<T>, tonic::Status>>,
{
    match tokio::time::timeout(CLIENT_HARD_TIMEOUT, fut).await {
        Err(_) => Err(format!(
            "{label}: no response within {CLIENT_HARD_TIMEOUT:?} (client-local hard timeout)"
        )),
        Ok(Err(status)) => Err(format!("{label}: {status}")),
        Ok(Ok(response)) => Ok(response.into_inner()),
    }
}

// ============================================================================
// 状态样本 + 运动证据（round 1 P0-3）
// ============================================================================

/// 一次经 layout 硬校验的状态样本。
#[derive(Debug, Clone)]
struct StateSample {
    stamp_ns: u64,
    positions: Vec<f64>,
}

/// 状态帧的 **typed 形状/数值校验**（round 2 P0-1）：layout 之外必须再锁
/// 三件——短向量会让 witness 索引 panic **绕过 finalizer**；NaN 在
/// `> tolerance` 比较中恒 false 会把回程判据放空成 PASS；`stamp_ns == 0`
/// 是"无权威采样时刻"（PX-26 修过的缺陷态，不得当新鲜样本消费）。
/// `get_state_raw` 与 `fetch_sample` **共用**本校验。
fn check_state_shape(
    label: &str,
    layout: &HardwareLayout,
    stamp_ns: u64,
    positions: &[f64],
) -> Result<(), String> {
    if stamp_ns == 0 {
        return Err(format!("{label}: stamp_ns == 0（无权威采样时刻）"));
    }
    if positions.len() != layout.joints.len() {
        return Err(format!(
            "{label}: positions len {} != layout joints {}（畸形帧）",
            positions.len(),
            layout.joints.len()
        ));
    }
    if let Some((i, v)) = positions.iter().enumerate().find(|(_, v)| !v.is_finite()) {
        return Err(format!("{label}: positions[{i}] not finite ({v})"));
    }
    Ok(())
}

/// **实际运动证据**：`Completed(Success)` 只证明 Scripted 时钟走完了——
/// adapter 不执行、机械卡住、持续回读旧值时它照样为真。witness 用**新鲜
/// 样本**（`stamp_ns` 严格推进才计入）观测探针关节：
///
/// - **出程**：max excursion ≥ offset − tolerance（确实到过目标邻域）；
/// - **回程**：最后一个新鲜样本 |pos − anchor| ≤ tolerance；
/// - 新鲜样本数 ≥ [`MIN_WITNESS_SAMPLES`]。
///
/// [`validate_probe_geometry`] 保证 `offset ≥ 2.5×tolerance` → 两判据阈值
/// 不相交，卡死在中间点不可能双过。
#[derive(Debug)]
struct MotionWitness {
    label: &'static str,
    joint_idx: usize,
    anchor: f64,
    offset: f64,
    tolerance: f64,
    last_stamp_ns: u64,
    fresh_samples: usize,
    stale_samples: usize,
    /// 形状/数值畸形样本计数（belt——上游 `check_state_shape` 已拦，任何
    /// 绕过路径在这里再判死；round 2 P0-1）。
    malformed_samples: usize,
    max_excursion: f64,
    last_fresh_pos: Option<f64>,
}

impl MotionWitness {
    fn new(
        label: &'static str,
        joint_idx: usize,
        anchor_sample: &StateSample,
        offset: f64,
        tolerance: f64,
    ) -> Self {
        Self {
            label,
            joint_idx,
            anchor: anchor_sample.positions[joint_idx],
            offset,
            tolerance,
            last_stamp_ns: anchor_sample.stamp_ns,
            fresh_samples: 0,
            stale_samples: 0,
            malformed_samples: 0,
            max_excursion: 0.0,
            last_fresh_pos: None,
        }
    }

    /// 喂一个样本；`stamp_ns` 未推进的样本按 stale 丢弃（gateway 缓存可能
    /// 连读同帧，不算失败，但**不计入证据**）。
    fn observe(&mut self, sample: &StateSample) {
        if sample.stamp_ns <= self.last_stamp_ns {
            self.stale_samples += 1;
            return;
        }
        self.last_stamp_ns = sample.stamp_ns;
        // belt（round 2 P0-1）：短向量不 panic、NaN 不进判据——一律记
        // malformed，由 verdict 判死。
        let Some(&pos) = sample.positions.get(self.joint_idx) else {
            self.malformed_samples += 1;
            return;
        };
        if !pos.is_finite() {
            self.malformed_samples += 1;
            return;
        }
        self.fresh_samples += 1;
        // 有符号方向：offset 恒正，excursion = pos − anchor 的同向分量。
        let excursion = pos - self.anchor;
        if excursion > self.max_excursion {
            self.max_excursion = excursion;
        }
        self.last_fresh_pos = Some(pos);
    }

    /// 终判（回程样本已喂完后调用）。
    fn verdict(&self) -> Result<(), String> {
        // 畸形样本零容忍（NaN 在阈值比较里恒 false，会放空回程判据）。
        if self.malformed_samples > 0 {
            return Err(format!(
                "{}: observed {} malformed/non-finite samples —— fail-closed",
                self.label, self.malformed_samples
            ));
        }
        if self.fresh_samples < MIN_WITNESS_SAMPLES {
            return Err(format!(
                "{}: motion evidence insufficient — only {} fresh samples (stale={}) —— \
                 stamp_ns 不推进 = S0 采样停滞或状态被冻结",
                self.label, self.fresh_samples, self.stale_samples
            ));
        }
        let reach_threshold = self.offset - self.tolerance;
        if self.max_excursion < reach_threshold {
            return Err(format!(
                "{}: outbound evidence FAILED — max excursion {:.4} rad < required {:.4} rad \
                 (offset {:.4} − tol {:.4})：电机未实际到达探针目标",
                self.label, self.max_excursion, reach_threshold, self.offset, self.tolerance
            ));
        }
        let last = self.last_fresh_pos.expect("fresh_samples ≥ 3 保证有值");
        let return_delta = (last - self.anchor).abs();
        if return_delta > self.tolerance {
            return Err(format!(
                "{}: return evidence FAILED — final pos off anchor by {:.4} rad > tol {:.4}",
                self.label, return_delta, self.tolerance
            ));
        }
        Ok(())
    }
}

// ============================================================================
// 一条臂的运行时（tonic client + layout + 身份）
// ============================================================================

#[cfg(feature = "supervised-s0")]
const HOLD_DURATION: Duration = Duration::from_secs(20);
#[cfg(feature = "supervised-s0")]
const HOLD_FRESH_NS: u64 = 200_000_000;

/// 仅 hold 使用：不重采目标，不将校准边界容差用作保持精度。
#[cfg(feature = "supervised-s0")]
struct HoldCheck {
    robot_id: String,
    anchor_raw: Vec<i32>,
    last_stamp_ns: u64,
}

#[cfg(feature = "supervised-s0")]
impl HoldCheck {
    fn new(robot_id: String, anchor_rad: &[f64], joints: usize) -> Result<Self, String> {
        if robot_id.is_empty() || anchor_rad.len() != joints || joints == 0 {
            return Err("hold 锚点必须完整匹配配置关节表及机器人身份".into());
        }
        Ok(Self {
            robot_id,
            anchor_raw: Self::raw_positions(anchor_rad)?,
            last_stamp_ns: 0,
        })
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "复用原 Feetech f32 换算，先拒绝非有限及不可表示值"
    )]
    fn raw_positions(positions: &[f64]) -> Result<Vec<i32>, String> {
        positions
            .iter()
            .map(|position| {
                if !position.is_finite() || position.abs() > f64::from(f32::MAX) {
                    return Err("hold 位置非有限或不可表示".into());
                }
                let raw = robot_adapter_feetech::rad_to_raw(*position as f32);
                if !(0..=4095).contains(&raw) {
                    return Err("hold 位置超出12位范围".into());
                }
                Ok(raw)
            })
            .collect()
    }

    fn observe(&mut self, state: &RobotState, hardware_id: &str, now: u64) -> Result<(), String> {
        if state.robot_id != self.robot_id || state.hardware_id != hardware_id {
            return Err("hold 状态 robot_id/hardware_id 不匹配".into());
        }
        if state.stamp_ns == 0
            || now
                .checked_sub(state.stamp_ns)
                .is_none_or(|age| age > HOLD_FRESH_NS)
        {
            return Err("hold 状态来自未来或超过200ms".into());
        }
        if state.stamp_ns < self.last_stamp_ns {
            return Err("hold 状态时间戳回退".into());
        }
        if state.fault_code != 0 || state.mode == S0Mode::Fault as i32 {
            return Err(format!(
                "hold 状态故障：mode={}, fault={}",
                state.mode, state.fault_code
            ));
        }
        Self::raw_positions(&state.position_rad)?;
        self.last_stamp_ns = state.stamp_ns;
        Ok(())
    }

    fn check_position(&self, state: &RobotState) -> Result<(), String> {
        let positions = Self::raw_positions(&state.position_rad)?;
        if positions.len() != self.anchor_raw.len() {
            return Err("hold 状态与固定锚点长度不匹配".into());
        }
        for (slot, (actual, target)) in positions.iter().zip(&self.anchor_raw).enumerate() {
            if (actual - target).abs() > 4 {
                return Err(format!(
                    "hold 固定S1锚点偏差超过4raw：slot={slot}, actual={actual}, target={target}"
                ));
            }
        }
        Ok(())
    }

    fn torque_on(state: &RobotState) -> bool {
        use robot_rpc::proto::{S0ControlLink, S0OutputMode, S0SafetyState};
        state.mode == S0Mode::Enabled as i32
            && state.control_link == S0ControlLink::Ok as i32
            && state.output_mode == S0OutputMode::Active as i32
            && state.safety_state == S0SafetyState::TorqueOn as i32
            && state.fault_code == 0
    }

    fn torque_off(state: &RobotState) -> bool {
        state.mode == S0Mode::Safe as i32
            && state.safety_state == robot_rpc::proto::S0SafetyState::TorqueOffConfirmed as i32
            && state.fault_code == 0
    }
}

struct SideRt {
    side: Side,
    hardware_id: String,
    layout: HardwareLayout,
    client: RobotControlClient<Channel>,
    token: String,
    seq: AtomicU64,
    #[cfg(feature = "supervised-s0")]
    hold_check: Option<HoldCheck>,
}

/// 独立采样端（witness 用；克隆 channel，双臂 phase 与协议 RPC 并行采样）。
struct StateProbe {
    label: &'static str,
    hardware_id: String,
    layout: HardwareLayout,
    client: RobotControlClient<Channel>,
}

impl StateProbe {
    async fn sample(&mut self) -> Result<StateSample, String> {
        fetch_sample(
            self.label,
            &mut self.client,
            &self.hardware_id,
            &self.layout,
        )
        .await
    }
}

/// `GetState` → 校验 layout → [`StateSample`]（客户端本地硬超时内）。
async fn fetch_sample(
    label: &str,
    client: &mut RobotControlClient<Channel>,
    hardware_id: &str,
    layout: &HardwareLayout,
) -> Result<StateSample, String> {
    let state = bounded(
        format!("{label} GetState"),
        client.get_state(with_grpc_timeout(GetStateRequest {
            hardware_id: hardware_id.to_string(),
        })),
    )
    .await?;
    // AC-E4：layout 不匹配硬失败，不降级消费。
    validate_state_layout(layout, &state).map_err(|e| format!("{label}: {e}"))?;
    // round 2 P0-1：形状/数值 typed 校验（与 get_state_raw 共用）。
    check_state_shape(label, layout, state.stamp_ns, &state.position_rad)?;
    Ok(StateSample {
        stamp_ns: state.stamp_ns,
        positions: state.position_rad,
    })
}

impl SideRt {
    async fn connect(
        side: Side,
        gateway: &str,
        config: &std::path::Path,
        hardware_id: &str,
        token: &str,
    ) -> Result<Self, String> {
        let layout = load_hardware_layout(config, hardware_id)
            .map_err(|e| format!("{side:?} layout from {}: {e}", config.display()))?;
        let channel = Channel::from_shared(gateway.to_string())
            .map_err(|e| format!("{side:?} endpoint {gateway}: {e}"))?
            .connect_timeout(Duration::from_secs(3))
            .connect()
            .await
            .map_err(|e| format!("{side:?} connect {gateway}: {e}"))?;
        Ok(Self {
            side,
            hardware_id: hardware_id.to_string(),
            layout,
            client: RobotControlClient::new(channel),
            token: token.to_string(),
            seq: AtomicU64::new(0),
            #[cfg(feature = "supervised-s0")]
            hold_check: None,
        })
    }

    fn label(&self) -> &'static str {
        match self.side {
            Side::Left => "left",
            Side::Right => "right",
        }
    }

    /// witness 用独立采样端（克隆 channel）。
    fn state_probe(&self) -> StateProbe {
        StateProbe {
            label: self.label(),
            hardware_id: self.hardware_id.clone(),
            layout: self.layout.clone(),
            client: self.client.clone(),
        }
    }

    fn meta(&self, purpose: &str) -> CommandMeta {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        CommandMeta {
            request_id: uuidv7_compat(),
            source: format!("supervised_demo/{}", self.label()),
            auth: Some(AuthContext {
                token: self.token.clone(),
                mtls_fingerprint: String::new(),
            }),
            timeout_ms: RPC_TIMEOUT_MS,
            idempotency_key: format!("demo-{}-{purpose}-{n}-{}", self.label(), uuidv7_compat()),
            dry_run: false,
            audit_reason: format!("supervised demo {purpose} (AC-E12)"),
        }
    }

    async fn get_state_raw(&mut self) -> Result<RobotState, String> {
        let state = bounded(
            format!("{} GetState", self.label()),
            self.client.get_state(with_grpc_timeout(GetStateRequest {
                hardware_id: self.hardware_id.clone(),
            })),
        )
        .await?;
        validate_state_layout(&self.layout, &state)
            .map_err(|e| format!("{}: {e}", self.label()))?;
        // round 2 P0-1：与 fetch_sample 共用形状/数值校验。
        check_state_shape(
            self.label(),
            &self.layout,
            state.stamp_ns,
            &state.position_rad,
        )?;
        #[cfg(feature = "supervised-s0")]
        if let Some(check) = &mut self.hold_check {
            check.observe(&state, &self.hardware_id, SystemClock::new().now().0)?;
        }
        Ok(state)
    }

    async fn set_enabled(&mut self, enabled: bool, purpose: &str) -> Result<(), String> {
        let meta = self.meta(purpose);
        bounded(
            format!("{} SetEnabled({enabled})", self.label()),
            self.client
                .set_enabled(with_grpc_timeout(SetEnabledCommand {
                    meta: Some(meta),
                    enabled,
                })),
        )
        .await
        .map(|_ack| ())
    }
}

// ============================================================================
// TorqueOps：上扭区段的操作面（round 1 P0-1 的 finalizer 可测缝）
// ============================================================================

/// 上扭 phase 的操作面。生产实现 = [`SideRt`]（每个操作客户端本地有界）；
/// 测试实现注入失败，覆盖 finalizer 的全部收尾路径。
trait TorqueOps {
    fn side_label(&self) -> &'static str;

    /// `SetEnabled(true)` + 轮询到 Enabled（轮询窗 = [`ENABLE_WAIT`]；
    /// **阶段整体硬上限**由调用方 `staged(budgets.enable)` 提供，PX-53）。
    ///
    /// **契约**：本方法一旦被调用，无论返回什么，都视为"已尝试上扭"——
    /// `SetEnabled` 失败响应不代表没上扭（deadline 不回滚副作用）。
    async fn enable(&mut self) -> Result<(), String>;

    /// `SetEnabled(false)` 且 **readback 确认**模式离开 Enabled。
    async fn disable_confirmed(&mut self) -> Result<(), String>;

    /// `EStop`（有界；失败也返回——调用方把结果并进报告，绝不因此中断收尾）。
    async fn estop(&mut self, reason: &str) -> Result<(), String>;

    async fn submit_scripted(&mut self, payload: Vec<u8>) -> Result<String, String>;
    async fn poll_status(&mut self, action_id: &str) -> Result<TaskSnapshot, String>;
    async fn cancel(&mut self, action_id: &str) -> Result<(), String>;
    async fn sample(&mut self) -> Result<StateSample, String>;
}

impl TorqueOps for SideRt {
    fn side_label(&self) -> &'static str {
        self.label()
    }

    async fn enable(&mut self) -> Result<(), String> {
        #[cfg(feature = "supervised-s0")]
        let initial_stamp = self.hold_check.as_ref().map(|check| check.last_stamp_ns);
        self.set_enabled(true, "enable").await?;
        let deadline = std::time::Instant::now() + ENABLE_WAIT;
        let mut last_mode = S0Mode::Unspecified as i32;
        while std::time::Instant::now() < deadline {
            let state = self.get_state_raw().await?;
            last_mode = state.mode;
            #[cfg(feature = "supervised-s0")]
            if let Some(check) = &self.hold_check {
                check.check_position(&state)?;
                if HoldCheck::torque_on(&state) && Some(state.stamp_ns) > initial_stamp {
                    return Ok(());
                }
                tokio::time::sleep(STATUS_POLL).await;
                continue;
            }
            if state.mode == S0Mode::Enabled as i32 {
                return Ok(());
            }
            tokio::time::sleep(STATUS_POLL).await;
        }
        Err(format!(
            "{}: not Enabled within {ENABLE_WAIT:?} (last mode={last_mode}) —— \
             若停在 Safe：latch 可能仍 latched，先 `robotctl clear-estop`（maintainer+）",
            self.label()
        ))
    }

    async fn disable_confirmed(&mut self) -> Result<(), String> {
        #[cfg(feature = "supervised-s0")]
        let initial_stamp = self.hold_check.as_ref().map(|check| check.last_stamp_ns);
        self.set_enabled(false, "disable").await?;
        // readback：模式必须离开 Enabled（round 1 P0-1 "disable 未确认"）。
        let deadline = std::time::Instant::now() + DISABLE_CONFIRM_WAIT;
        loop {
            let state = self.get_state_raw().await?;
            let confirmed = state.mode != S0Mode::Enabled as i32;
            #[cfg(feature = "supervised-s0")]
            let confirmed = if initial_stamp.is_some() {
                HoldCheck::torque_off(&state) && Some(state.stamp_ns) > initial_stamp
            } else {
                confirmed
            };
            if confirmed {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "{}: disable 状态未确认，SetEnabled(false) 后已等待 {DISABLE_CONFIRM_WAIT:?}",
                    self.label()
                ));
            }
            tokio::time::sleep(STATUS_POLL).await;
        }
    }

    async fn estop(&mut self, reason: &str) -> Result<(), String> {
        let meta = self.meta("estop");
        bounded(
            format!("{} EStop", self.label()),
            self.client.e_stop(with_grpc_timeout(EStopCommand {
                meta: Some(meta),
                estop_reason: reason.to_string(),
            })),
        )
        .await
        .map(|_ack| ())
    }

    async fn submit_scripted(&mut self, payload: Vec<u8>) -> Result<String, String> {
        let spec = ActionSpec::scripted(payload);
        let meta = self.meta("single-probe");
        let handle = bounded(
            format!("{} InvokeTask", self.label()),
            self.client
                .invoke_task(with_grpc_timeout(InvokeTaskCommand {
                    meta: Some(meta),
                    kind: spec.kind.proto_value(),
                    deadline_ns: 0,
                    priority: 10,
                    payload_schema_id: spec.payload_schema_id,
                    payload: spec.payload,
                })),
        )
        .await?;
        Ok(handle.action_id)
    }

    async fn poll_status(&mut self, action_id: &str) -> Result<TaskSnapshot, String> {
        let status = bounded(
            format!("{} GetTaskStatus", self.label()),
            self.client
                .get_task_status(with_grpc_timeout(TaskHandleRef {
                    request_id: uuidv7_compat(),
                    action_id: action_id.to_string(),
                })),
        )
        .await?;
        Ok(TaskSnapshot::from_proto(&status))
    }

    async fn cancel(&mut self, action_id: &str) -> Result<(), String> {
        let meta = self.meta("cancel");
        bounded(
            format!("{} CancelTask", self.label()),
            self.client
                .cancel_task(with_grpc_timeout(CancelTaskCommand {
                    meta: Some(meta),
                    action_id: action_id.to_string(),
                })),
        )
        .await
        .map(|_ack| ())
    }

    async fn sample(&mut self) -> Result<StateSample, String> {
        let (label, hardware_id, layout) =
            (self.label(), self.hardware_id.clone(), self.layout.clone());
        fetch_sample(label, &mut self.client, &hardware_id, &layout).await
    }
}

// ============================================================================
// 单臂上扭区段 + 统一 fail-closed finalizer（round 1 P0-1）
// ============================================================================

/// 上扭区段 body 的失败形态：携带"是否有已知未终态 action 需要 cancel"。
#[derive(Debug, PartialEq, Eq)]
struct BodyFailure {
    message: String,
    /// `Some(id)` = 已提交且**未观测到终态**——finalizer 先 best-effort
    /// cancel 再 `EStop`；`None` = 无需 cancel（未提交 / 已终态）。
    cancel_target: Option<String>,
}

impl BodyFailure {
    fn plain(message: String) -> Self {
        Self {
            message,
            cancel_target: None,
        }
    }

    fn with_cancel(message: String, action_id: String) -> Self {
        Self {
            message,
            cancel_target: Some(action_id),
        }
    }
}

/// 单臂上扭区段 body：**从 enable 起**（调用本函数 = 已进入上扭窗口，
/// 任何返回都必须交给 [`finalize_single`]）。
///
/// enable → submit → 轮询（witness 采样 + status 交替）→ 终态
/// `Completed(Success)` → 静置 → 回程采样 → witness 终判。
async fn single_arm_torque_body<O: TorqueOps>(
    ops: &mut O,
    payload: Vec<u8>,
    witness: &mut MotionWitness,
    exec_budget: Duration,
    budgets: StageBudgets,
) -> Result<(), BodyFailure> {
    // round 2 P0-3：enable 阶段整体墙钟上限（内层 5 s 轮询窗可被最后一次
    // 4 s RPC 穿透——外层 timeout 才是硬上界）。
    staged(budgets.enable, "enable stage", ops.enable())
        .await
        .map_err(BodyFailure::plain)?;
    println!("[single-arm] side={} Enabled", ops.side_label());

    let action_id = ops
        .submit_scripted(payload)
        .await
        .map_err(BodyFailure::plain)?;
    println!(
        "[single-arm] side={} submitted action_id={action_id}",
        ops.side_label()
    );

    // 轮询到终态；每轮同时喂 witness 一个执行期样本。
    // round 2 P0-3：整段包 exec_budget 外层墙钟（内层逐次检查会被最后一轮
    // sample+status ≈ 8 s 穿透）；**超时保留 action_id 走 cancel → EStop**。
    let poll = async {
        loop {
            // witness 采样失败与 status 失败同级：action 状态未知 → 带 cancel。
            match ops.sample().await {
                Ok(sample) => witness.observe(&sample),
                Err(e) => break Err(BodyFailure::with_cancel(e, action_id.clone())),
            }
            let snap = match ops.poll_status(&action_id).await {
                Ok(s) => s,
                // round 1 P0-1：status 失败也必须走 cancel → EStop 收尾。
                Err(e) => break Err(BodyFailure::with_cancel(e, action_id.clone())),
            };
            match snap.phase {
                TaskPhase::Terminal(TerminalState::Completed) => {
                    if snap.outcome == OutcomeKind::Success {
                        break Ok(());
                    }
                    break Err(BodyFailure::plain(format!(
                        "{}: terminal Completed but outcome {:?} (warning={:?})",
                        ops.side_label(),
                        snap.outcome,
                        snap.outcome_warning
                    )));
                }
                TaskPhase::Terminal(other) => {
                    break Err(BodyFailure::plain(format!(
                        "{}: terminal {other:?} (failure={:?} reason={:?})",
                        ops.side_label(),
                        snap.failure,
                        snap.failure_reason
                    )));
                }
                TaskPhase::Unspecified | TaskPhase::Pending | TaskPhase::Executing => {
                    tokio::time::sleep(STATUS_POLL).await;
                }
            }
        }
    };
    match tokio::time::timeout(exec_budget, poll).await {
        Ok(inner) => inner?,
        Err(_) => {
            return Err(BodyFailure::with_cancel(
                format!(
                    "{}: exec stage wall-clock budget {exec_budget:?} exceeded",
                    ops.side_label()
                ),
                action_id,
            ));
        }
    }

    // 静置 + 回程证据采样（终判在 disable 之前——断扭后位置可能下垂）；
    // 整段受 settle 阶段墙钟（round 2 P0-3）。
    staged(budgets.settle, "settle stage", async {
        tokio::time::sleep(SETTLE_WAIT).await;
        for _ in 0..2 {
            let sample = ops.sample().await?;
            witness.observe(&sample);
            tokio::time::sleep(STATUS_POLL).await;
        }
        witness.verdict()
    })
    .await
    .map_err(BodyFailure::plain)
}

/// **统一 fail-closed finalizer（单臂）**：body 的任何结果都必经此处。
///
/// - `Ok` → [`TorqueOps::disable_confirmed`]；未确认 → **`EStop` + FAIL**；
/// - `Err` → 有 `cancel_target` 先 best-effort cancel → **`EStop`** → FAIL
///   （`EStop` 自身失败并进报告并点名人工断电，不吞）。
async fn finalize_single<O: TorqueOps>(
    ops: &mut O,
    body: Result<(), BodyFailure>,
    budgets: StageBudgets,
) -> Result<(), String> {
    match body {
        Ok(()) => match staged(budgets.disable, "disable stage", ops.disable_confirmed()).await {
            Ok(()) => Ok(()),
            Err(disable_err) => {
                let estop = ops.estop("supervised_demo: disable unconfirmed").await;
                Err(format!(
                    "{}: disable NOT confirmed ({disable_err}) → EStop {} —— fail-closed 判 FAIL",
                    ops.side_label(),
                    estop_report(estop)
                ))
            }
        },
        Err(failure) => {
            if let Some(action_id) = &failure.cancel_target
                && let Err(e) = ops.cancel(action_id).await
            {
                eprintln!(
                    "[warn] {} cancel {action_id}: {e}（继续 EStop）",
                    ops.side_label()
                );
            }
            let estop = ops.estop("supervised_demo: single-arm failure").await;
            Err(format!(
                "{} → EStop {}",
                failure.message,
                estop_report(estop)
            ))
        }
    }
}

fn estop_report(result: Result<(), String>) -> String {
    match result {
        Ok(()) => "sent (weak Ack)".to_string(),
        Err(e) => format!("FAILED ({e}) —— 立即人工切断总电源"),
    }
}

// ============================================================================
// 双臂上扭区段的 enable / finalize（round 1 P0-1；协议中段由
// run_chunk_pair 自带收尾）
// ============================================================================

/// 双侧 enable：左先右后。**右侧结果不确定时双侧 `EStop`**（左已上扭，右
/// 是否上扭未知——`SetEnabled` 失败响应不代表没上扭）。左侧失败时只
/// `EStop` 左侧（右侧从未尝试）。
async fn pair_enable<L: TorqueOps, R: TorqueOps>(
    left: &mut L,
    right: &mut R,
    budgets: StageBudgets,
) -> Result<(), String> {
    if let Err(e) = staged(budgets.enable, "left enable stage", left.enable()).await {
        let estop = left
            .estop("supervised_demo: left enable indeterminate")
            .await;
        return Err(format!(
            "left enable failed: {e} → left EStop {}",
            estop_report(estop)
        ));
    }
    if let Err(e) = staged(budgets.enable, "right enable stage", right.enable()).await {
        // round 2 P0-2：双 EStop **并发**发起——串行时第一侧黑洞会让第二侧
        // 多带扭最多一个 RPC 硬超时。
        let (el, er) = tokio::join!(
            left.estop("supervised_demo: right enable indeterminate"),
            right.estop("supervised_demo: right enable indeterminate")
        );
        return Err(format!(
            "right enable failed: {e} → double EStop (left {}, right {})",
            estop_report(el),
            estop_report(er)
        ));
    }
    Ok(())
}

/// **统一 fail-closed finalizer（双臂）**：
///
/// - body `Ok` → 双侧 disable 确认；任一未确认 → **双侧 `EStop` + FAIL**；
/// - body `Err` → **双侧 `EStop`** + FAIL（协议模块可能已 `EStop` 过——弱 Ack
///   幂等，重复无害）。
async fn pair_finalize<L: TorqueOps, R: TorqueOps>(
    left: &mut L,
    right: &mut R,
    body: Result<(), String>,
    budgets: StageBudgets,
) -> Result<(), String> {
    match body {
        Ok(()) => {
            // round 2 P0-2 + P0-3：双 disable **并发**发起且各自受阶段墙钟
            // ——串行时第一侧黑洞会让第二侧多带扭最多一个 RPC 硬超时。
            let (dl, dr) = tokio::join!(
                staged(
                    budgets.disable,
                    "left disable stage",
                    left.disable_confirmed()
                ),
                staged(
                    budgets.disable,
                    "right disable stage",
                    right.disable_confirmed()
                )
            );
            match (dl, dr) {
                (Ok(()), Ok(())) => Ok(()),
                (dl, dr) => {
                    let (el, er) = tokio::join!(
                        left.estop("supervised_demo: pair disable unconfirmed"),
                        right.estop("supervised_demo: pair disable unconfirmed")
                    );
                    Err(format!(
                        "pair disable NOT confirmed (left={:?}, right={:?}) → double EStop \
                         (left {}, right {}) —— fail-closed 判 FAIL",
                        dl.err(),
                        dr.err(),
                        estop_report(el),
                        estop_report(er)
                    ))
                }
            }
        }
        Err(e) => {
            // round 2 P0-2：失败路径的双 EStop 并发发起。
            let (el, er) = tokio::join!(
                left.estop("supervised_demo: pair failure"),
                right.estop("supervised_demo: pair failure")
            );
            Err(format!(
                "{e} → double EStop (left {}, right {})",
                estop_report(el),
                estop_report(er)
            ))
        }
    }
}

// ============================================================================
// ArmEndpoint 适配（协议语义单源消费；纯 I/O，无语义分叉）
// ============================================================================

struct DemoArm {
    rt: SideRt,
}

fn map_status(status: &tonic::Status) -> ArmRpcError {
    ArmRpcError::Status {
        code: format!("{:?}", status.code()),
        message: status.message().to_string(),
    }
}

impl ArmEndpoint for DemoArm {
    fn side(&self) -> Side {
        self.rt.side
    }

    async fn invoke_action(&mut self, spec: ActionSpec) -> Result<String, SubmitFailure> {
        let cmd = InvokeTaskCommand {
            meta: Some(self.rt.meta("invoke")),
            kind: spec.kind.proto_value(),
            deadline_ns: 0,
            priority: 10,
            payload_schema_id: spec.payload_schema_id,
            payload: spec.payload,
        };
        let handle = self
            .rt
            .client
            .invoke_task(with_grpc_timeout(cmd))
            .await
            .map_err(|s| {
                SubmitFailure::from_status_code(&format!("{:?}", s.code()), map_status(&s))
            })?
            .into_inner();
        Ok(handle.action_id)
    }

    async fn cancel_task(&mut self, action_id: &str) -> Result<(), ArmRpcError> {
        let cmd = CancelTaskCommand {
            meta: Some(self.rt.meta("cancel")),
            action_id: action_id.to_string(),
        };
        self.rt
            .client
            .cancel_task(with_grpc_timeout(cmd))
            .await
            .map_err(|s| map_status(&s))?;
        Ok(())
    }

    async fn task_status(&mut self, action_id: &str) -> Result<TaskSnapshot, ArmRpcError> {
        let req = TaskHandleRef {
            request_id: uuidv7_compat(),
            action_id: action_id.to_string(),
        };
        let status = self
            .rt
            .client
            .get_task_status(with_grpc_timeout(req))
            .await
            .map_err(|s| map_status(&s))?
            .into_inner();
        Ok(TaskSnapshot::from_proto(&status))
    }

    async fn estop(&mut self, reason: &str) -> Result<(), ArmRpcError> {
        let cmd = EStopCommand {
            meta: Some(self.rt.meta("estop")),
            estop_reason: reason.to_string(),
        };
        self.rt
            .client
            .e_stop(with_grpc_timeout(cmd))
            .await
            .map_err(|s| map_status(&s))?;
        Ok(())
    }
}

// ============================================================================
// 探针轨迹（wire 量化后复核斜率，round 1 P1-1）
// ============================================================================

/// 探针参数（校验后的产物；段时长以 **ms 整数**持有，与 wire 编码一致）。
struct Probe {
    joint: String,
    offset_rad: f64,
    /// 单段时长（ms；`to_scripted_waypoints` 的 ms 量化对它是恒等）。
    segment_ms: u32,
}

impl Probe {
    /// 选定关节 + 校验数值 gate。`joint_hint = None` 时自动选首个含
    /// `wrist_roll` 的关节（历史 Orin 验证关节族）。
    fn resolve(
        layout: &HardwareLayout,
        joint_hint: Option<&str>,
        offset_rad: f64,
        tolerance_rad: f64,
    ) -> Result<Self, String> {
        let joint = match joint_hint {
            Some(name) => {
                if !layout.joints.iter().any(|j| j == name) {
                    return Err(format!(
                        "--probe-joint {name:?} not in layout joints {:?}",
                        layout.joints
                    ));
                }
                name.to_string()
            }
            None => layout
                .joints
                .iter()
                .find(|j| j.contains("wrist_roll"))
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "no *wrist_roll* joint in layout {:?}; pass --probe-joint",
                        layout.joints
                    )
                })?,
        };
        Self::checked(joint, offset_rad, tolerance_rad)
    }

    /// 夹爪探针：关节 = 首个 `*_gripper`。
    fn resolve_gripper(
        layout: &HardwareLayout,
        offset_rad: f64,
        tolerance_rad: f64,
    ) -> Result<Self, String> {
        let joint = layout
            .joints
            .iter()
            .find(|j| j.ends_with("_gripper"))
            .cloned()
            .ok_or_else(|| format!("no *_gripper joint in layout {:?}", layout.joints))?;
        Self::checked(joint, offset_rad, tolerance_rad)
    }

    fn checked(joint: String, offset_rad: f64, tolerance_rad: f64) -> Result<Self, String> {
        // AC-E12 ③ 双 gate：CLI 层已拦一次，这里在构造点再强制一次
        // （防未来新调用路径绕过 CLI 校验）。
        validate_probe_geometry(offset_rad, tolerance_rad)?;
        // round 1 P1-1：段时长**向上取整到 ms**——浮点秒会被转换器的 ms
        // 四舍五入改写（offset 0.000149 rad 曾被编码成 1 ms，名义斜率
        // 0.149 > 0.14）。ceil 后 wire 斜率恒 ≤ 缺省斜率。
        let ideal_ms = offset_rad / DEFAULT_SLOPE_RAD_PER_S * 1000.0;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let segment_ms = ideal_ms.ceil().max(1.0) as u32;
        // wire 真值复核（belt：ceil 之下按构造恒成立，仍显式锁死）。
        let wire_slope = offset_rad / (f64::from(segment_ms) / 1000.0);
        if wire_slope > MAX_SLOPE_RAD_PER_S {
            return Err(format!(
                "wire slope {wire_slope:.4} rad/s > {MAX_SLOPE_RAD_PER_S}（segment {segment_ms} ms）"
            ));
        }
        Ok(Self {
            joint,
            offset_rad,
            segment_ms,
        })
    }

    /// 单段时长（s，恰为 ms 的整数倍）。
    fn segment_s(&self) -> f64 {
        f64::from(self.segment_ms) / 1000.0
    }

    /// 总时长（出 + 回）。
    fn total(&self) -> Duration {
        Duration::from_millis(u64::from(self.segment_ms) * 2)
    }

    /// 构造 Scripted payload：`t=0` 当前位（转换器丢弃 + 宽容校验）→
    /// `t=T` anchor+offset → `t=2T` anchor（回落；D3 `idle_hold_target` =
    /// 末 waypoint）。时刻均为 ms 整数倍 → 转换器的 ms 量化是恒等。
    fn build_payload(&self, layout: &HardwareLayout, anchor: &[f64]) -> Result<Vec<u8>, String> {
        let idx = self.joint_idx(layout);
        let a = anchor[idx];
        let t1 = self.segment_s();
        let input = TrajectoryInput {
            joint_names: vec![self.joint.clone()],
            points: vec![
                TrajectoryPoint {
                    time_from_start_s: 0.0,
                    positions: vec![a],
                },
                TrajectoryPoint {
                    time_from_start_s: t1,
                    positions: vec![a + self.offset_rad],
                },
                TrajectoryPoint {
                    time_from_start_s: t1 * 2.0,
                    positions: vec![a],
                },
            ],
        };
        let waypoints = to_scripted_waypoints(&input, layout, anchor)
            .map_err(|e| format!("to_scripted_waypoints: {e}"))?;
        Ok(build_scripted_payload(&waypoints))
    }

    fn joint_idx(&self, layout: &HardwareLayout) -> usize {
        layout
            .joints
            .iter()
            .position(|j| *j == self.joint)
            .expect("resolve 已校验关节在 layout 内")
    }
}

// ============================================================================
// 输出 helper
// ============================================================================

fn rad_to_deg(r: f64) -> f64 {
    r * 180.0 / std::f64::consts::PI
}

fn print_state(tag: &str, label: &str, state: &RobotState) {
    let positions: Vec<String> = state
        .position_rad
        .iter()
        .map(|p| format!("{p:+.4}"))
        .collect();
    println!(
        "[{tag}] side={label} robot_id={} hardware_id={} layout_id={:#018x} mode={} safety={} stamp_ns={}",
        state.robot_id,
        state.hardware_id,
        state.layout_id,
        state.mode,
        state.safety_state,
        state.stamp_ns
    );
    println!(
        "[{tag}]   positions_rad=[{}] ({} joints)",
        positions.join(", "),
        state.position_rad.len()
    );
}

// ============================================================================
// phases
// ============================================================================

/// read-state：只读。可达 + layout 硬校验 + stamp 推进。
async fn phase_read_state(rt: &mut SideRt) -> Result<(), String> {
    let first = rt.get_state_raw().await?;
    print_state("read-state", rt.label(), &first);
    tokio::time::sleep(STATE_ADVANCE_PROBE).await;
    let second = rt.get_state_raw().await?;
    if second.stamp_ns <= first.stamp_ns {
        return Err(format!(
            "{}: stamp_ns not advancing ({} -> {}) —— S0 采样时刻停滞",
            rt.label(),
            first.stamp_ns,
            second.stamp_ns
        ));
    }
    println!(
        "[read-state] side={} stamp_advance_ns={} PASS",
        rt.label(),
        second.stamp_ns - first.stamp_ns
    );
    Ok(())
}

/// 左侧原位保持：核对实际 S1 锚点 → 固定20秒 → 原 finalizer；不提交任务。
#[cfg(feature = "supervised-s0")]
async fn phase_hold(rt: &mut SideRt, args: &Args) -> Result<(), String> {
    let config_path = args.left_config.clone();
    let config_text = tokio::task::spawn_blocking(move || std::fs::read_to_string(config_path))
        .await
        .map_err(|e| format!("hold 配置读取任务失败：{e}"))?
        .map_err(|e| format!("hold 读取配置：{e}"))?;
    let config: toml::Value =
        toml::from_str(&config_text).map_err(|e| format!("hold 解析配置：{e}"))?;
    let robot_id = config
        .get("robot")
        .and_then(|robot| robot.get("id"))
        .and_then(toml::Value::as_str)
        .ok_or("hold 配置缺少 robot.id")?;
    let anchor: Vec<f64> = serde_json::from_str(
        args.hold_anchor_rad
            .as_deref()
            .ok_or("hold 缺少实际S1锚点")?,
    )
    .map_err(|e| format!("hold 锚点数组无效：{e}"))?;
    let check = HoldCheck::new(robot_id.into(), &anchor, rt.layout.joints.len())?;
    println!("HOLD_ANCHOR_RAW={:?}", check.anchor_raw);
    rt.hold_check = Some(check);
    // 启用前只读：固定锚点不能用当前反馈替换，两帧均须已断扭且来源推进。
    let before = rt.get_state_raw().await?;
    check_hold_pre_enable(rt, &before)?;
    tokio::time::sleep(STATUS_POLL).await;
    let current = rt.get_state_raw().await?;
    check_hold_pre_enable(rt, &current)?;
    if current.stamp_ns <= before.stamp_ns {
        return Err("hold 启用前状态来源没有推进".into());
    }
    println!("[hold] side=left duration_ms=20000 tolerance_raw=4 anchor_rad={anchor:?}");

    // 沿用原 enable/finalizer：第一次启用尝试之后，任何错误都必经 EStop。
    let body = async {
        staged(PROD_STAGE_BUDGETS.enable, "hold enable stage", rt.enable()).await?;
        let clock = SystemClock::new();
        let start_ns = clock.now().0;
        let end_ns =
            start_ns + u64::try_from(HOLD_DURATION.as_nanos()).map_err(|_| "hold 时长不可表示")?;
        let mut samples = 0usize;
        loop {
            let remaining = Duration::from_nanos(end_ns.saturating_sub(clock.now().0));
            if remaining <= STATUS_POLL {
                tokio::time::sleep(remaining).await;
                break;
            }
            tokio::time::sleep(STATUS_POLL).await;
            let previous_stamp = rt
                .hold_check
                .as_ref()
                .ok_or("hold 校验器缺失")?
                .last_stamp_ns;
            // 最后一轮 RPC 不得把20秒保持窗拖长；超时仍按失败收尾。
            let remaining = Duration::from_nanos(end_ns.saturating_sub(clock.now().0));
            let state = staged(remaining, "hold state stage", rt.get_state_raw()).await?;
            if state.stamp_ns <= previous_stamp || !HoldCheck::torque_on(&state) {
                return Err("hold 来源未推进或未保持 Enabled/Ok/Active/TorqueOn".into());
            }
            rt.hold_check
                .as_ref()
                .ok_or("hold 校验器缺失")?
                .check_position(&state)?;
            samples += 1;
        }
        if samples < MIN_WITNESS_SAMPLES {
            return Err("hold 新鲜反馈样本不足".into());
        }
        println!(
            "[hold] samples={samples} elapsed_ns={}",
            clock.now().0 - start_ns
        );
        Ok(())
    }
    .await
    .map_err(BodyFailure::plain);
    finalize_single(rt, body, PROD_STAGE_BUDGETS).await?;
    println!("[hold] side=left fixed_anchor_hold_20s + Safe/TorqueOffConfirmed/fault0 PASS");
    println!("HOLD_PASS");
    Ok(())
}

#[cfg(feature = "supervised-s0")]
fn check_hold_pre_enable(rt: &SideRt, state: &RobotState) -> Result<(), String> {
    if !HoldCheck::torque_off(state) {
        return Err("hold 启用前必须为 Safe/TorqueOffConfirmed/fault0".into());
    }
    rt.hold_check
        .as_ref()
        .ok_or("hold 校验器缺失")?
        .check_position(state)
}

async fn phase_single_arm(rt: &mut SideRt, args: &Args) -> Result<(), String> {
    let probe = Probe::resolve(
        &rt.layout,
        args.probe_joint.as_deref(),
        args.probe_offset_rad,
        args.settle_tolerance_rad,
    )?;
    let anchor_state = rt.get_state_raw().await?;
    print_state("single-arm", rt.label(), &anchor_state);
    let anchor_sample = StateSample {
        stamp_ns: anchor_state.stamp_ns,
        positions: anchor_state.position_rad.clone(),
    };
    println!(
        "[single-arm] side={} probe_joint={} offset_rad={:+.4} ({:.2}°) segment_ms={}",
        rt.label(),
        probe.joint,
        probe.offset_rad,
        rad_to_deg(probe.offset_rad),
        probe.segment_ms
    );
    let payload = probe.build_payload(&rt.layout, &anchor_sample.positions)?;
    let mut witness = MotionWitness::new(
        rt.label(),
        probe.joint_idx(&rt.layout),
        &anchor_sample,
        probe.offset_rad,
        args.settle_tolerance_rad,
    );

    // ---- 上扭窗口：body 的任何结果都必经 finalize_single ----
    let exec_budget = probe.total() + Duration::from_secs(5);
    let body =
        single_arm_torque_body(rt, payload, &mut witness, exec_budget, PROD_STAGE_BUDGETS).await;
    finalize_single(rt, body, PROD_STAGE_BUDGETS).await?;
    println!(
        "[single-arm] side={} Completed(Success) + motion evidence (max_excursion={:+.4} rad) + disable confirmed PASS",
        rt.label(),
        witness.max_excursion
    );
    Ok(())
}

/// witness 采样任务：以 [`STATUS_POLL`] 周期采样直到 stop 置位；采样错误
/// 计数（协议 RPC 与采样共享 gateway，偶发失败不即刻判死——证据不足由
/// verdict 兜底）。
fn spawn_sampler(
    mut probe: StateProbe,
    mut witness: MotionWitness,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<(MotionWitness, usize)> {
    tokio::spawn(async move {
        let mut errors = 0usize;
        while !stop.load(Ordering::Relaxed) {
            match probe.sample().await {
                Ok(sample) => witness.observe(&sample),
                Err(_) => errors += 1,
            }
            tokio::time::sleep(STATUS_POLL).await;
        }
        (witness, errors)
    })
}

/// sampler 的**有界 join**（round 2 P1-4）：`timeout` 超时后丢弃
/// `JoinHandle` 在 Tokio 语义上是 **detach**——必须显式 `abort()` 并有界等
/// 取消确认；两侧各自独立收（调用方用 `tokio::join!`，禁止 `?` 短路丢弃
/// 另一侧的 handle）。
async fn join_sampler(
    label: &str,
    mut handle: tokio::task::JoinHandle<(MotionWitness, usize)>,
) -> Result<(MotionWitness, usize), String> {
    match tokio::time::timeout(SAMPLER_JOIN, &mut handle).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{label} sampler panicked: {e}")),
        Err(_) => {
            handle.abort();
            match tokio::time::timeout(SAMPLER_ABORT_JOIN, handle).await {
                Ok(_) => Err(format!(
                    "{label} sampler join timeout ({SAMPLER_JOIN:?}); aborted, cancellation confirmed"
                )),
                Err(_) => Err(format!(
                    "{label} sampler join timeout; abort NOT confirmed within {SAMPLER_ABORT_JOIN:?}"
                )),
            }
        }
    }
}

/// 双臂 / 双夹爪：prep → [`pair_enable`] → `run_chunk_pair`（并行 witness
/// 采样）→ 回程证据 → [`pair_finalize`]。
#[allow(clippy::too_many_lines)]
async fn phase_pair(
    mut left: SideRt,
    mut right: SideRt,
    args: &Args,
    grippers: bool,
) -> Result<(), String> {
    let tag = if grippers { "grippers" } else { "dual-arm" };
    let resolve = |layout: &HardwareLayout| {
        if grippers {
            Probe::resolve_gripper(layout, args.probe_offset_rad, args.settle_tolerance_rad)
        } else {
            Probe::resolve(
                layout,
                args.probe_joint.as_deref(),
                args.probe_offset_rad,
                args.settle_tolerance_rad,
            )
        }
    };
    let left_probe = resolve(&left.layout)?;
    let right_probe = resolve(&right.layout)?;

    let left_anchor_state = left.get_state_raw().await?;
    let right_anchor_state = right.get_state_raw().await?;
    print_state(tag, left.label(), &left_anchor_state);
    print_state(tag, right.label(), &right_anchor_state);
    let left_anchor = StateSample {
        stamp_ns: left_anchor_state.stamp_ns,
        positions: left_anchor_state.position_rad.clone(),
    };
    let right_anchor = StateSample {
        stamp_ns: right_anchor_state.stamp_ns,
        positions: right_anchor_state.position_rad.clone(),
    };
    let left_payload = left_probe.build_payload(&left.layout, &left_anchor.positions)?;
    let right_payload = right_probe.build_payload(&right.layout, &right_anchor.positions)?;
    println!(
        "[{tag}] probes: left={} right={} offset_rad={:+.4} segment_ms={}",
        left_probe.joint, right_probe.joint, args.probe_offset_rad, left_probe.segment_ms
    );

    let left_witness = MotionWitness::new(
        "left",
        left_probe.joint_idx(&left.layout),
        &left_anchor,
        left_probe.offset_rad,
        args.settle_tolerance_rad,
    );
    let right_witness = MotionWitness::new(
        "right",
        right_probe.joint_idx(&right.layout),
        &right_anchor,
        right_probe.offset_rad,
        args.settle_tolerance_rad,
    );
    let mut left_probe_client = left.state_probe();
    let mut right_probe_client = right.state_probe();

    // ---- 上扭窗口开始：从 pair_enable 起，任何路径必经 pair_finalize ----
    pair_enable(&mut left, &mut right, PROD_STAGE_BUDGETS).await?;
    println!("[{tag}] both Enabled");

    // 并行 witness 采样（独立克隆 channel，不与协议 RPC 抢 client）。
    let stop = Arc::new(AtomicBool::new(false));
    let left_sampler = spawn_sampler(left.state_probe(), left_witness, Arc::clone(&stop));
    let right_sampler = spawn_sampler(right.state_probe(), right_witness, Arc::clone(&stop));

    let clock = SystemClock;
    let cfg = PairConfig::default();
    let mut pair = ArmPair {
        left: DemoArm { rt: left },
        right: DemoArm { rt: right },
    };
    let run = run_chunk_pair(
        &mut pair,
        ActionSpec::scripted(left_payload),
        ActionSpec::scripted(right_payload),
        &clock,
        &cfg,
    )
    .await;
    let (mut left, mut right) = (pair.left.rt, pair.right.rt);

    // 收采样任务（round 2 P1-4：两侧**同时**有界 join，超时显式 abort +
    // 取消确认；禁止 `?` 短路丢弃另一侧 handle——timeout 丢弃 JoinHandle
    // 在 Tokio 语义上是 detach）。
    stop.store(true, Ordering::Relaxed);
    let (jl, jr) = tokio::join!(
        join_sampler("left", left_sampler),
        join_sampler("right", right_sampler)
    );
    let joined: Result<_, String> = match (jl, jr) {
        (Ok(l), Ok(r)) => Ok((l, r)),
        (l, r) => Err(format!(
            "sampler join failed (left={:?}, right={:?})",
            l.err(),
            r.err()
        )),
    };

    // body 判定：协议结果 + 回程证据（采样与终判都在 disable 之前）。
    let body: Result<(), String> = async {
        let run = run.map_err(|e| format!("[{tag}] run_chunk_pair: {e:?}"))?;
        println!("[{tag}] ack_skew={:?}", run.ack_skew);
        match run.result {
            PairResult::Success { .. } => {}
            other => return Err(format!("[{tag}] pair result not Success: {other:?}")),
        }
        let ((mut lw, le), (mut rw, re)) = joined?;
        if le + re > 0 {
            eprintln!("[warn] [{tag}] sampler errors: left={le} right={re}");
        }
        // 静置 + 回程采样（双侧**并发**采样，round 2 P0-2；整段受 settle
        // 阶段墙钟，round 2 P0-3）。
        staged(PROD_STAGE_BUDGETS.settle, "pair settle stage", async {
            tokio::time::sleep(SETTLE_WAIT).await;
            for _ in 0..2 {
                let (ls, rs) =
                    tokio::join!(left_probe_client.sample(), right_probe_client.sample());
                lw.observe(&ls?);
                rw.observe(&rs?);
                tokio::time::sleep(STATUS_POLL).await;
            }
            Ok(())
        })
        .await?;
        lw.verdict()?;
        rw.verdict()?;
        println!(
            "[{tag}] motion evidence: left max_excursion={:+.4} right max_excursion={:+.4}",
            lw.max_excursion, rw.max_excursion
        );
        Ok(())
    }
    .await;

    Box::pin(pair_finalize(
        &mut left,
        &mut right,
        body,
        PROD_STAGE_BUDGETS,
    ))
    .await?;
    println!("[{tag}] pair Completed(Success) + motion evidence + both disable confirmed PASS");
    Ok(())
}

// ============================================================================
// main
// ============================================================================

struct PhaseOutcome {
    name: String,
    result: Result<(), String>,
}

async fn connect_side(args: &Args, side: Side) -> Result<SideRt, String> {
    match side {
        Side::Left => {
            SideRt::connect(
                side,
                &args.left_gateway,
                &args.left_config,
                &args.left_hardware_id,
                &args.token,
            )
            .await
        }
        Side::Right => {
            SideRt::connect(
                side,
                &args.right_gateway,
                &args.right_config,
                &args.right_hardware_id,
                &args.token,
            )
            .await
        }
    }
}

async fn run_phase(args: &Args, phase: Phase, side: Side) -> PhaseOutcome {
    let name = match phase {
        Phase::ReadState => "read-state".to_string(),
        Phase::Hold => "hold/left".to_string(),
        Phase::SingleArm => format!(
            "single-arm/{}",
            if side == Side::Left { "left" } else { "right" }
        ),
        Phase::DualArm => "dual-arm".to_string(),
        Phase::Grippers => "grippers".to_string(),
        Phase::All => unreachable!("all 在 main 展开"),
    };
    let result = match phase {
        Phase::Hold => {
            #[cfg(feature = "supervised-s0")]
            {
                let step = async {
                    let mut rt = connect_side(args, Side::Left).await?;
                    phase_hold(&mut rt, args).await
                };
                step.await
            }
            #[cfg(not(feature = "supervised-s0"))]
            {
                Err("hold 需要 supervised-s0 feature".into())
            }
        }
        Phase::ReadState => {
            // 双侧都读；一侧失败即 FAIL（read-state 也用于确认双栈健康）。
            let mut out = Ok(());
            for s in [Side::Left, Side::Right] {
                let step = async {
                    let mut rt = connect_side(args, s).await?;
                    phase_read_state(&mut rt).await
                };
                if let Err(e) = step.await {
                    out = Err(e);
                    break;
                }
            }
            out
        }
        Phase::SingleArm => {
            let step = async {
                let mut rt = connect_side(args, side).await?;
                phase_single_arm(&mut rt, args).await
            };
            step.await
        }
        Phase::DualArm | Phase::Grippers => {
            let step = async {
                let left = connect_side(args, Side::Left).await?;
                let right = connect_side(args, Side::Right).await?;
                Box::pin(phase_pair(left, right, args, phase == Phase::Grippers)).await
            };
            step.await
        }
        Phase::All => unreachable!(),
    };
    PhaseOutcome { name, result }
}

fn main() -> ExitCode {
    robot_logging::set_process_name("robot-demo");
    let _ = robot_logging::init();
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("--s0-mixed-hold" | "--s0-position-hold")
    ) {
        #[cfg(all(target_os = "linux", feature = "supervised-s0"))]
        return supervised_mixed::main_entry(
            std::env::args().skip(2),
            std::env::args().nth(1).as_deref() == Some("--s0-position-hold"),
        );
        #[cfg(not(all(target_os = "linux", feature = "supervised-s0")))]
        {
            eprintln!("爸爸，S0受监督发送仅支持 Linux，并须启用 supervised-s0 feature");
            return ExitCode::from(2);
        }
    }
    let args = match Args::parse() {
        Ok(a) => a,
        Err(msg) => {
            if msg == "HELP" {
                print_help();
                return ExitCode::SUCCESS;
            }
            eprintln!("error: {msg}");
            print_help();
            return ExitCode::from(2);
        }
    };

    // 上扭 phase 的 checklist 硬门：不带 flag 绝不 SetEnabled。
    if args.phase.enables_torque() && !args.ack_safety_checklist {
        eprintln!("{SAFETY_CHECKLIST}");
        return ExitCode::from(2);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let outcomes: Vec<PhaseOutcome> = runtime.block_on(async {
        let mut out = Vec::new();
        let plan: Vec<(Phase, Side)> = match args.phase {
            Phase::All => vec![
                (Phase::ReadState, Side::Left),
                (Phase::SingleArm, Side::Left),
                (Phase::SingleArm, Side::Right),
                (Phase::DualArm, Side::Left),
                (Phase::Grippers, Side::Left),
            ],
            p => vec![(p, args.side)],
        };
        for (phase, side) in plan {
            let outcome = Box::pin(run_phase(&args, phase, side)).await;
            let failed = outcome.result.is_err();
            out.push(outcome);
            if failed {
                // 串行计划：一步 FAIL 即中止（受监督 demo 不带伤续跑）。
                break;
            }
        }
        out
    });

    println!("========== summary ==========");
    let mut pass = 0usize;
    let mut fail = 0usize;
    for o in &outcomes {
        match &o.result {
            Ok(()) => {
                pass += 1;
                println!("  phase {} : PASS", o.name);
            }
            Err(e) => {
                fail += 1;
                println!("  phase {} : FAIL — {e}", o.name);
            }
        }
    }
    println!("total: {pass} PASS / {fail} FAIL");
    if fail == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

// ============================================================================
// 单测（`--features supervised-demo` 下随 cargo test 跑）
// ============================================================================

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    #[cfg(feature = "supervised-s0")]
    mod hold_tests {
        use super::*;

        fn state() -> RobotState {
            RobotState {
                robot_id: "robot".into(),
                hardware_id: "left".into(),
                layout_id: 0xDEAD_BEEF,
                stamp_ns: 1_000_000_000,
                position_rad: vec![0.0; 6],
                mode: S0Mode::Safe as i32,
                safety_state: robot_rpc::proto::S0SafetyState::TorqueOffConfirmed as i32,
                ..RobotState::default()
            }
        }

        fn check() -> HoldCheck {
            HoldCheck::new("robot".into(), &[0.0; 6], 6).unwrap()
        }

        fn args(extra: &[&str]) -> Vec<String> {
            [
                "--phase",
                "hold",
                "--token",
                "test-only",
                "--hold-anchor-rad",
                "[0,0,0,0,0,0]",
            ]
            .into_iter()
            .chain(extra.iter().copied())
            .map(str::to_string)
            .collect()
        }

        #[test]
        fn hold_requires_left_fixed_target_and_rejects_probe_or_duration_overrides() {
            Args::parse_from(&args(&[])).unwrap();
            for extra in [
                vec!["--side", "right"],
                vec!["--probe-offset-rad", "0.05"],
                vec!["--probe-joint", "wrist"],
                vec!["--settle-tolerance-rad", "0.02"],
                vec!["--duration", "21"],
            ] {
                assert!(Args::parse_from(&args(&extra)).is_err());
            }
            assert!(
                Args::parse_from(&["--phase", "hold", "--token", "test-only"].map(str::to_string))
                    .is_err()
            );
            assert_eq!(HOLD_DURATION, Duration::from_secs(20));
            assert!(Phase::Hold.enables_torque());
        }

        #[test]
        fn anchor_rejects_shape_nonfinite_and_unrepresentable_raw() {
            for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MAX, 10.0] {
                assert!(HoldCheck::new("robot".into(), &[bad; 6], 6).is_err());
            }
            assert!(HoldCheck::new("robot".into(), &[0.0; 5], 6).is_err());
            assert!(HoldCheck::new(String::new(), &[0.0; 6], 6).is_err());
        }

        #[test]
        fn hold_uses_original_four_raw_boundary_for_every_joint() {
            for slot in 0..6 {
                for delta in [-4, 4] {
                    let mut sample = state();
                    sample.position_rad[slot] =
                        f64::from(robot_adapter_feetech::raw_to_rad(2047 + delta));
                    check().check_position(&sample).unwrap();
                }
                for delta in [-5, 5] {
                    let mut sample = state();
                    sample.position_rad[slot] =
                        f64::from(robot_adapter_feetech::raw_to_rad(2047 + delta));
                    assert!(check().check_position(&sample).is_err());
                }
            }
        }

        #[test]
        fn hold_rejects_wrong_identity_layout_and_bad_position() {
            let mut sample = state();
            sample.robot_id = "other".into();
            assert!(check().observe(&sample, "left", sample.stamp_ns).is_err());
            sample = state();
            assert!(check().observe(&sample, "other", sample.stamp_ns).is_err());
            sample.layout_id += 1;
            assert!(validate_state_layout(&layout(), &sample).is_err());
            for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                sample = state();
                sample.position_rad[2] = bad;
                assert!(check().observe(&sample, "left", sample.stamp_ns).is_err());
            }
            sample = state();
            sample.position_rad.pop();
            assert!(
                check_state_shape("left", &layout(), sample.stamp_ns, &sample.position_rad)
                    .is_err()
            );
        }

        #[test]
        fn hold_rejects_stale_future_zero_and_regressing_source() {
            let sample = state();
            check()
                .observe(&sample, "left", sample.stamp_ns + HOLD_FRESH_NS)
                .unwrap();
            for now in [sample.stamp_ns - 1, sample.stamp_ns + HOLD_FRESH_NS + 1] {
                assert!(check().observe(&sample, "left", now).is_err());
            }
            let mut zero = sample.clone();
            zero.stamp_ns = 0;
            assert!(check().observe(&zero, "left", 0).is_err());
            let mut guard = check();
            guard.observe(&sample, "left", sample.stamp_ns).unwrap();
            let mut earlier = sample.clone();
            earlier.stamp_ns -= 1;
            assert!(guard.observe(&earlier, "left", sample.stamp_ns).is_err());
        }

        #[test]
        fn hold_never_accepts_fault_or_merely_non_enabled_as_torque_off() {
            use robot_rpc::proto::{S0ControlLink, S0OutputMode, S0SafetyState};
            let sample = state();
            assert!(HoldCheck::torque_off(&sample));
            for mode in [
                S0Mode::Unspecified,
                S0Mode::Ready,
                S0Mode::Fault,
                S0Mode::Stopping,
            ] {
                let mut bad = sample.clone();
                bad.mode = mode as i32;
                assert!(!HoldCheck::torque_off(&bad));
            }
            for safety in [
                S0SafetyState::TorqueUnknown,
                S0SafetyState::TorqueOn,
                S0SafetyState::StopCommanded,
                S0SafetyState::StopFailed,
                S0SafetyState::AdapterUncertain,
            ] {
                let mut bad = sample.clone();
                bad.safety_state = safety as i32;
                assert!(!HoldCheck::torque_off(&bad));
            }
            let mut bad = sample.clone();
            bad.fault_code = 1;
            assert!(!HoldCheck::torque_off(&bad));
            assert!(check().observe(&bad, "left", bad.stamp_ns).is_err());
            let mut enabled = sample;
            enabled.mode = S0Mode::Enabled as i32;
            enabled.safety_state = S0SafetyState::TorqueOn as i32;
            enabled.control_link = S0ControlLink::Ok as i32;
            enabled.output_mode = S0OutputMode::Active as i32;
            assert!(HoldCheck::torque_on(&enabled));
            enabled.output_mode = S0OutputMode::Hold as i32;
            assert!(!HoldCheck::torque_on(&enabled));
        }

        #[test]
        fn hold_finalizer_never_submits_or_cancels_a_task() {
            run(async {
                let mut ops = ScriptedOps::new("left");
                finalize_single(&mut ops, Ok(()), tb()).await.unwrap();
                assert_eq!(ops.calls, ["disable"]);
                ops.calls.clear();
                assert!(
                    finalize_single(&mut ops, Err(BodyFailure::plain("hold stale".into())), tb())
                        .await
                        .is_err()
                );
                assert_eq!(ops.calls, ["estop"]);
                ops.calls.clear();
                ops.disable_r
                    .push_back(Scripted::Err("not TorqueOffConfirmed".into()));
                assert!(finalize_single(&mut ops, Ok(()), tb()).await.is_err());
                assert_eq!(ops.calls, ["disable", "estop"]);
            });
        }
    }

    fn layout() -> HardwareLayout {
        HardwareLayout {
            joints: vec![
                "left_arm_shoulder_pan".into(),
                "left_arm_shoulder_lift".into(),
                "left_arm_elbow_flex".into(),
                "left_arm_wrist_flex".into(),
                "left_arm_wrist_roll".into(),
                "left_arm_gripper".into(),
            ],
            motor_ids: vec![1, 2, 3, 4, 5, 6],
            layout_id: 0xDEAD_BEEF,
        }
    }

    fn probe(offset: f64) -> Probe {
        Probe::resolve(&layout(), None, offset, offset / TOLERANCE_SEPARATION).expect("合法探针")
    }

    // ------------------------------------------------------------------
    // 数值 gate
    // ------------------------------------------------------------------

    /// AC-E12 ③：偏移超限 / 非法一律拒绝（构造点二道门，不 clamp）。
    #[test]
    fn probe_rejects_offsets_beyond_the_hard_gate() {
        let l = layout();
        for bad in [
            0.0,
            -0.01,
            MAX_PROBE_OFFSET_RAD + 1e-9,
            f64::NAN,
            f64::INFINITY,
        ] {
            assert!(
                Probe::resolve(&l, None, bad, 0.01).is_err(),
                "offset {bad} 必须拒绝"
            );
        }
        // 上限本身（含等）接受。
        Probe::resolve(&l, None, MAX_PROBE_OFFSET_RAD, 0.02).expect("边界含等");
    }

    /// **round 1 P0-3**：容差上限 + 判据不相交约束（大容差放空回落判据）。
    #[test]
    fn tolerance_gate_rejects_vacuous_settle_criteria() {
        // 上限之上拒绝。
        assert!(validate_probe_geometry(0.087, MAX_SETTLE_TOLERANCE_RAD + 1e-9).is_err());
        // offset < 2.5×tol 拒绝（reached 与 returned 阈值会相交）。
        assert!(validate_probe_geometry(0.04, 0.02).is_err());
        assert!(validate_probe_geometry(0.05, 0.021).is_err());
        // 缺省组合与边界（offset == 2.5×tol）接受。
        validate_probe_geometry(DEFAULT_PROBE_OFFSET_RAD, DEFAULT_SETTLE_TOLERANCE_RAD)
            .expect("缺省组合");
        validate_probe_geometry(0.05, 0.02).expect("边界含等");
        // 非法容差。
        for bad in [0.0, -0.01, f64::NAN] {
            assert!(validate_probe_geometry(0.05, bad).is_err(), "{bad}");
        }
    }

    /// **round 1 P1-1**：斜率按 **wire 量化后**复核——浮点秒校验会被转换器
    /// 的 ms 四舍五入绕过（offset 0.000149 曾被编码成 1 ms → 名义 0.149
    /// rad/s，超 0.14）。
    #[test]
    fn wire_slope_never_exceeds_the_limit_after_ms_quantization() {
        let l = layout();
        for offset in [
            0.000_149,
            0.000_101,
            0.001,
            0.01,
            DEFAULT_PROBE_OFFSET_RAD,
            MAX_PROBE_OFFSET_RAD,
        ] {
            let tol = (offset / TOLERANCE_SEPARATION).min(MAX_SETTLE_TOLERANCE_RAD);
            let p = Probe::resolve(&l, None, offset, tol).expect("合法 offset");
            let wire_slope = p.offset_rad / (f64::from(p.segment_ms) / 1000.0);
            assert!(
                wire_slope <= MAX_SLOPE_RAD_PER_S + 1e-12,
                "offset {offset}: segment_ms={} wire_slope={wire_slope}",
                p.segment_ms
            );
        }
        // reviewer 反例本体：0.000149 → ceil(1.49) = 2 ms → 0.0745 rad/s。
        let p = Probe::resolve(&l, None, 0.000_149, 0.000_050).expect("反例本体");
        assert_eq!(p.segment_ms, 2);
    }

    /// 探针时刻均为 ms 整数倍 → 转换器 ms 量化是恒等（段时长回读一致）。
    #[test]
    fn probe_times_survive_converter_quantization() {
        let l = layout();
        let p = probe(0.05);
        let anchor = vec![0.1, -0.2, 0.3, -0.4, 0.5, 0.0];
        let input = TrajectoryInput {
            joint_names: vec![p.joint.clone()],
            points: vec![
                TrajectoryPoint {
                    time_from_start_s: 0.0,
                    positions: vec![anchor[4]],
                },
                TrajectoryPoint {
                    time_from_start_s: p.segment_s(),
                    positions: vec![anchor[4] + p.offset_rad],
                },
                TrajectoryPoint {
                    time_from_start_s: p.segment_s() * 2.0,
                    positions: vec![anchor[4]],
                },
            ],
        };
        let wps = to_scripted_waypoints(&input, &l, &anchor).expect("convert");
        assert_eq!(wps.len(), 2);
        assert_eq!(wps[0].segment_duration_ms, p.segment_ms, "量化必须恒等");
        assert_eq!(wps[1].segment_duration_ms, p.segment_ms);
        assert!(
            (wps[1].positions[4] - anchor[4]).abs() < 1e-12,
            "末点回 anchor"
        );
        for wp in &wps {
            for (i, v) in wp.positions.iter().enumerate() {
                if i != 4 {
                    assert!((v - anchor[i]).abs() < 1e-12, "joint {i} 不得移动");
                }
            }
        }
    }

    /// 关节解析：缺省选 `wrist_roll`；显式名必须在 layout 内；gripper 探针
    /// 选 `*_gripper`。
    #[test]
    fn probe_joint_resolution() {
        let l = layout();
        assert_eq!(probe(0.05).joint, "left_arm_wrist_roll");
        assert_eq!(
            Probe::resolve(&l, Some("left_arm_elbow_flex"), 0.05, 0.02)
                .unwrap()
                .joint,
            "left_arm_elbow_flex"
        );
        assert!(Probe::resolve(&l, Some("nope"), 0.05, 0.02).is_err());
        assert_eq!(
            Probe::resolve_gripper(&l, 0.05, 0.02).unwrap().joint,
            "left_arm_gripper"
        );
    }

    /// checklist 门：上扭 phase 判定表。
    #[test]
    fn torque_phases_require_the_checklist() {
        assert!(!Phase::ReadState.enables_torque());
        for p in [
            Phase::Hold,
            Phase::SingleArm,
            Phase::DualArm,
            Phase::Grippers,
            Phase::All,
        ] {
            assert!(p.enables_torque(), "{p:?}");
        }
    }

    /// endpoint 归一：裸 host:port 补 http scheme，带 scheme 原样。
    #[test]
    fn endpoint_normalization() {
        assert_eq!(
            normalize_endpoint("127.0.0.1:50051"),
            "http://127.0.0.1:50051"
        );
        assert_eq!(normalize_endpoint("http://x:1"), "http://x:1");
    }

    /// `request_id` 形态：gateway `validate_request_id` 要求 `UUIDv7`。
    #[test]
    fn uuidv7_compat_shape() {
        let id = uuidv7_compat();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "7", "version nibble");
    }

    // ------------------------------------------------------------------
    // MotionWitness（round 1 P0-3 证据逻辑）
    // ------------------------------------------------------------------

    fn sample(stamp: u64, pos_j4: f64) -> StateSample {
        StateSample {
            stamp_ns: stamp,
            positions: vec![0.0, 0.0, 0.0, 0.0, pos_j4, 0.0],
        }
    }

    fn witness() -> MotionWitness {
        // anchor = 0.5，offset 0.05，tol 0.02（缺省几何）。
        MotionWitness::new("left", 4, &sample(1_000, 0.5), 0.05, 0.02)
    }

    /// 正向：出程到达 + 回程回落 + stamp 推进 → PASS。
    #[test]
    fn witness_passes_with_real_out_and_back_motion() {
        let mut w = witness();
        for (stamp, pos) in [
            (2_000, 0.52),
            (3_000, 0.545), // 到达 anchor+offset−tol = 0.53 之上
            (4_000, 0.52),
            (5_000, 0.505),
            (6_000, 0.500),
        ] {
            w.observe(&sample(stamp, pos));
        }
        w.verdict().expect("真实出→回必须 PASS");
        assert!((w.max_excursion - 0.045).abs() < 1e-12);
    }

    /// **电机完全没动**（持续回读 anchor，stamp 正常推进）→ 出程证据 FAIL
    /// ——round 1 P0-3 的主反例：`Completed(Success)` 靠时钟推进，adapter
    /// 不执行时照样为真。
    #[test]
    fn witness_fails_when_motor_never_moved() {
        let mut w = witness();
        for stamp in [2_000, 3_000, 4_000, 5_000] {
            w.observe(&sample(stamp, 0.5));
        }
        let err = w.verdict().expect_err("零位移必须 FAIL");
        assert!(err.contains("outbound evidence FAILED"), "{err}");
    }

    /// **卡死在中间点**（判据不相交约束的意义）：到不了 reached 阈值 →
    /// FAIL；即便它同时"接近 anchor"也救不回来。
    #[test]
    fn witness_fails_when_stuck_between_thresholds() {
        let mut w = witness();
        for stamp in [2_000, 3_000, 4_000, 5_000] {
            w.observe(&sample(stamp, 0.52)); // excursion 0.02 < 0.03
        }
        let err = w.verdict().expect_err("中间点必须 FAIL");
        assert!(err.contains("outbound evidence FAILED"), "{err}");
    }

    /// **stamp 不推进**（S0 冻结 / 状态被缓存重放）→ 样本全 stale →
    /// 证据不足 FAIL。
    #[test]
    fn witness_fails_when_stamps_do_not_advance() {
        let mut w = witness();
        for _ in 0..10 {
            w.observe(&sample(1_000, 0.545)); // stamp == anchor stamp
        }
        let err = w.verdict().expect_err("stale 样本必须 FAIL");
        assert!(err.contains("insufficient"), "{err}");
        assert_eq!(w.fresh_samples, 0);
        assert_eq!(w.stale_samples, 10);
    }

    /// 出程到达但**没回落** → 回程证据 FAIL。
    #[test]
    fn witness_fails_when_arm_does_not_return() {
        let mut w = witness();
        for (stamp, pos) in [
            (2_000, 0.53),
            (3_000, 0.548),
            (4_000, 0.548),
            (5_000, 0.547),
        ] {
            w.observe(&sample(stamp, pos));
        }
        let err = w.verdict().expect_err("未回落必须 FAIL");
        assert!(err.contains("return evidence FAILED"), "{err}");
    }

    /// 新鲜样本数 < 下限 → FAIL（哪怕值都对）。
    #[test]
    fn witness_needs_minimum_fresh_samples() {
        let mut w = witness();
        w.observe(&sample(2_000, 0.545));
        w.observe(&sample(3_000, 0.5));
        let err = w.verdict().expect_err("样本不足必须 FAIL");
        assert!(err.contains("insufficient"), "{err}");
    }

    // ------------------------------------------------------------------
    // finalizer 故障注入（round 1 P0-1）
    // ------------------------------------------------------------------

    /// 脚本化结果：`Hang` = 永挂（模拟连接黑洞，round 2 P0-3 预算测试）。
    enum Scripted<T> {
        Ok(T),
        Err(String),
        Hang,
    }

    impl<T> Scripted<T> {
        async fn resolve(self) -> Result<T, String> {
            match self {
                Self::Ok(v) => Ok(v),
                Self::Err(e) => Err(e),
                Self::Hang => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Err("unreachable: hang resolved".into())
                }
            }
        }
    }

    /// 可编排的 [`TorqueOps`]：每个操作一个结果队列 + 调用序记录；
    /// `*_signals` / `*_waits_for` 是并发性判据 gadget（round 2 P0-2）：
    /// 某操作**开始**即置位 flag / 自旋等对侧 flag——串行实现会死等，
    /// 只有并发发起才能完成。
    struct ScriptedOps {
        label: &'static str,
        calls: Vec<String>,
        enable_r: VecDeque<Scripted<()>>,
        disable_r: VecDeque<Scripted<()>>,
        estop_r: VecDeque<Scripted<()>>,
        submit_r: VecDeque<Scripted<String>>,
        status_r: VecDeque<Scripted<TaskSnapshot>>,
        cancel_r: VecDeque<Scripted<()>>,
        sample_r: VecDeque<Scripted<StateSample>>,
        estop_signals: Option<Arc<AtomicBool>>,
        estop_waits_for: Option<Arc<AtomicBool>>,
        disable_signals: Option<Arc<AtomicBool>>,
        disable_waits_for: Option<Arc<AtomicBool>>,
    }

    impl ScriptedOps {
        fn new(label: &'static str) -> Self {
            Self {
                label,
                calls: Vec::new(),
                enable_r: VecDeque::new(),
                disable_r: VecDeque::new(),
                estop_r: VecDeque::new(),
                submit_r: VecDeque::new(),
                status_r: VecDeque::new(),
                cancel_r: VecDeque::new(),
                sample_r: VecDeque::new(),
                estop_signals: None,
                estop_waits_for: None,
                disable_signals: None,
                disable_waits_for: None,
            }
        }

        /// 队列耗尽默认成功（脚本只注入感兴趣的失败）。
        fn pop(q: &mut VecDeque<Scripted<()>>) -> Scripted<()> {
            q.pop_front().unwrap_or(Scripted::Ok(()))
        }

        async fn sync_point(signal: Option<&Arc<AtomicBool>>, wait: Option<&Arc<AtomicBool>>) {
            if let Some(flag) = signal {
                flag.store(true, Ordering::SeqCst);
            }
            if let Some(flag) = wait {
                while !flag.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        }
    }

    impl TorqueOps for ScriptedOps {
        fn side_label(&self) -> &'static str {
            self.label
        }

        async fn enable(&mut self) -> Result<(), String> {
            tokio::task::yield_now().await;
            self.calls.push("enable".into());
            Self::pop(&mut self.enable_r).resolve().await
        }

        async fn disable_confirmed(&mut self) -> Result<(), String> {
            self.calls.push("disable".into());
            Self::sync_point(
                self.disable_signals.as_ref(),
                self.disable_waits_for.as_ref(),
            )
            .await;
            Self::pop(&mut self.disable_r).resolve().await
        }

        async fn estop(&mut self, _reason: &str) -> Result<(), String> {
            self.calls.push("estop".into());
            Self::sync_point(self.estop_signals.as_ref(), self.estop_waits_for.as_ref()).await;
            Self::pop(&mut self.estop_r).resolve().await
        }

        async fn submit_scripted(&mut self, _payload: Vec<u8>) -> Result<String, String> {
            tokio::task::yield_now().await;
            self.calls.push("submit".into());
            self.submit_r
                .pop_front()
                .unwrap_or_else(|| Scripted::Ok("action-1".into()))
                .resolve()
                .await
        }

        async fn poll_status(&mut self, _id: &str) -> Result<TaskSnapshot, String> {
            tokio::task::yield_now().await;
            self.calls.push("status".into());
            self.status_r
                .pop_front()
                .unwrap_or_else(|| Scripted::Err("status queue exhausted".into()))
                .resolve()
                .await
        }

        async fn cancel(&mut self, id: &str) -> Result<(), String> {
            tokio::task::yield_now().await;
            self.calls.push(format!("cancel:{id}"));
            Self::pop(&mut self.cancel_r).resolve().await
        }

        async fn sample(&mut self) -> Result<StateSample, String> {
            tokio::task::yield_now().await;
            self.calls.push("sample".into());
            self.sample_r
                .pop_front()
                .unwrap_or_else(|| Scripted::Ok(sample(9_000_000, 0.5)))
                .resolve()
                .await
        }
    }

    /// 既有测试用的宽松预算（语义不变）；预算注入测试另用毫秒级。
    /// settle 覆盖真实 `SETTLE_WAIT`(600 ms) + 2×(采样 + 100 ms 轮询)。
    fn tb() -> StageBudgets {
        StageBudgets {
            enable: Duration::from_millis(500),
            disable: Duration::from_millis(500),
            settle: Duration::from_secs(2),
        }
    }

    fn run<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(fut)
    }

    fn success_snapshot() -> TaskSnapshot {
        TaskSnapshot {
            phase: TaskPhase::Terminal(TerminalState::Completed),
            progress: 1.0,
            outcome: OutcomeKind::Success,
            outcome_warning: String::new(),
            failure: robot_rpc::client::dual_arm_workflow::FailureKind::Unspecified,
            failure_reason: String::new(),
        }
    }

    /// enable 失败（响应失败 ≠ 没上扭）→ **`EStop`**，绝不静默返回。
    #[test]
    fn enable_failure_is_finalized_with_estop() {
        run(async {
            let mut ops = ScriptedOps::new("left");
            ops.enable_r.push_back(Scripted::Err("deadline".into()));
            let mut w = witness();
            let body =
                single_arm_torque_body(&mut ops, vec![], &mut w, Duration::from_secs(1), tb())
                    .await;
            let err = finalize_single(&mut ops, body, tb())
                .await
                .expect_err("必须 FAIL");
            assert!(err.contains("EStop"), "{err}");
            assert_eq!(
                ops.calls,
                vec!["enable", "estop"],
                "enable 失败 → 直接 EStop"
            );
        });
    }

    /// **round 1 P0-1 主反例**：submit 后 `poll_status` RPC 失败 → 必须先
    /// cancel 再 `EStop`（旧实现直接 `?` 返回，两者都不做）。
    #[test]
    fn status_failure_after_submit_cancels_then_estops() {
        run(async {
            let mut ops = ScriptedOps::new("left");
            ops.status_r
                .push_back(Scripted::Err("status rpc timeout".into()));
            let mut w = witness();
            let body =
                single_arm_torque_body(&mut ops, vec![], &mut w, Duration::from_secs(1), tb())
                    .await;
            let err = finalize_single(&mut ops, body, tb())
                .await
                .expect_err("必须 FAIL");
            assert!(err.contains("EStop"), "{err}");
            assert_eq!(
                ops.calls,
                vec![
                    "enable",
                    "submit",
                    "sample",
                    "status",
                    "cancel:action-1",
                    "estop"
                ],
                "status 失败 → cancel → EStop"
            );
        });
    }

    /// witness 采样失败（action 状态未知）→ 同样 cancel → `EStop`。
    #[test]
    fn sample_failure_after_submit_cancels_then_estops() {
        run(async {
            let mut ops = ScriptedOps::new("left");
            ops.sample_r.push_back(Scripted::Err("gateway hang".into()));
            let mut w = witness();
            let body =
                single_arm_torque_body(&mut ops, vec![], &mut w, Duration::from_secs(1), tb())
                    .await;
            let err = finalize_single(&mut ops, body, tb())
                .await
                .expect_err("必须 FAIL");
            assert!(err.contains("EStop"), "{err}");
            assert_eq!(
                ops.calls,
                vec!["enable", "submit", "sample", "cancel:action-1", "estop"]
            );
        });
    }

    /// 成功路径但 **disable 未确认** → `EStop` + FAIL（round 1 P0-1
    /// "`disable_best_effort` 失败只 warning"的反面）。
    #[test]
    fn unconfirmed_disable_estops_and_fails() {
        run(async {
            let mut ops = ScriptedOps::new("left");
            ops.disable_r
                .push_back(Scripted::Err("still Enabled".into()));
            let err = finalize_single(&mut ops, Ok(()), tb())
                .await
                .expect_err("disable 未确认必须 FAIL");
            assert!(err.contains("disable NOT confirmed"), "{err}");
            assert!(err.contains("EStop"), "{err}");
            assert_eq!(ops.calls, vec!["disable", "estop"]);
        });
    }

    /// 成功路径 + disable 确认 → 不 `EStop`。
    #[test]
    fn confirmed_disable_passes_without_estop() {
        run(async {
            let mut ops = ScriptedOps::new("left");
            finalize_single(&mut ops, Ok(()), tb())
                .await
                .expect("必须 PASS");
            assert_eq!(ops.calls, vec!["disable"]);
        });
    }

    /// 运动证据失败（body Err 无 `cancel_target`：action 已终态）→ `EStop`，
    /// 不 cancel。
    #[test]
    fn motion_evidence_failure_estops_without_cancel() {
        run(async {
            let mut ops = ScriptedOps::new("left");
            let body = Err(BodyFailure::plain("outbound evidence FAILED".into()));
            let err = finalize_single(&mut ops, body, tb())
                .await
                .expect_err("必须 FAIL");
            assert!(err.contains("EStop"), "{err}");
            assert_eq!(ops.calls, vec!["estop"], "已终态失败不 cancel，直接 EStop");
        });
    }

    /// `EStop` 自身失败 → 错误文本必须点名人工断电（弱 Ack 不可依赖）。
    #[test]
    fn estop_failure_demands_manual_power_cut() {
        run(async {
            let mut ops = ScriptedOps::new("left");
            ops.enable_r.push_back(Scripted::Err("boom".into()));
            ops.estop_r
                .push_back(Scripted::Err("estop rpc dead".into()));
            let mut w = witness();
            let body =
                single_arm_torque_body(&mut ops, vec![], &mut w, Duration::from_secs(1), tb())
                    .await;
            let err = finalize_single(&mut ops, body, tb())
                .await
                .expect_err("必须 FAIL");
            assert!(err.contains("立即人工切断总电源"), "{err}");
        });
    }

    /// **双臂：右侧 enable 结果不确定 → 双侧 `EStop`**（round 1 P0-1 点名）。
    #[test]
    fn right_enable_indeterminate_double_estops() {
        run(async {
            let mut left = ScriptedOps::new("left");
            let mut right = ScriptedOps::new("right");
            right
                .enable_r
                .push_back(Scripted::Err("deadline exceeded".into()));
            let err = pair_enable(&mut left, &mut right, tb())
                .await
                .expect_err("必须 FAIL");
            assert!(err.contains("double EStop"), "{err}");
            assert_eq!(left.calls, vec!["enable", "estop"], "左侧已上扭必须 EStop");
            assert_eq!(right.calls, vec!["enable", "estop"], "右侧不确定必须 EStop");
        });
    }

    /// 左侧 enable 失败（右侧从未尝试）→ 只 `EStop` 左侧。
    #[test]
    fn left_enable_failure_estops_left_only() {
        run(async {
            let mut left = ScriptedOps::new("left");
            let mut right = ScriptedOps::new("right");
            left.enable_r.push_back(Scripted::Err("latched".into()));
            let err = pair_enable(&mut left, &mut right, tb())
                .await
                .expect_err("必须 FAIL");
            assert!(err.contains("left enable failed"), "{err}");
            assert_eq!(left.calls, vec!["enable", "estop"]);
            assert!(right.calls.is_empty(), "右侧未尝试不得触碰");
        });
    }

    /// 双臂成功路径但一侧 disable 未确认 → **双侧 `EStop` + FAIL**。
    #[test]
    fn pair_disable_unconfirmed_double_estops() {
        run(async {
            let mut left = ScriptedOps::new("left");
            let mut right = ScriptedOps::new("right");
            right
                .disable_r
                .push_back(Scripted::Err("still Enabled".into()));
            let err = pair_finalize(&mut left, &mut right, Ok(()), tb())
                .await
                .expect_err("必须 FAIL");
            assert!(err.contains("double EStop"), "{err}");
            assert_eq!(left.calls, vec!["disable", "estop"]);
            assert_eq!(right.calls, vec!["disable", "estop"]);
        });
    }

    /// 双臂 body 失败 → 双侧 `EStop`（协议模块可能已 `EStop`，幂等重复无害）。
    #[test]
    fn pair_body_failure_double_estops() {
        run(async {
            let mut left = ScriptedOps::new("left");
            let mut right = ScriptedOps::new("right");
            let err = pair_finalize(&mut left, &mut right, Err("pair not Success".into()), tb())
                .await
                .expect_err("必须 FAIL");
            assert!(err.contains("double EStop"), "{err}");
            assert_eq!(left.calls, vec!["estop"]);
            assert_eq!(right.calls, vec!["estop"]);
        });
    }

    /// 双臂全绿 → 双侧 disable、零 `EStop`。
    #[test]
    fn pair_success_disables_both_without_estop() {
        run(async {
            let mut left = ScriptedOps::new("left");
            let mut right = ScriptedOps::new("right");
            pair_finalize(&mut left, &mut right, Ok(()), tb())
                .await
                .expect("必须 PASS");
            assert_eq!(left.calls, vec!["disable"]);
            assert_eq!(right.calls, vec!["disable"]);
        });
    }

    // ------------------------------------------------------------------
    // round 2 回修锁（P0-1 畸形帧 / P0-2 并发安全动作 / P0-3 阶段墙钟）
    // ------------------------------------------------------------------

    /// **P0-1**：typed 状态校验四态（`get_state_raw` / `fetch_sample` 共用）。
    #[test]
    fn state_shape_check_rejects_malformed_frames() {
        let l = layout();
        check_state_shape("left", &l, 1, &[0.0; 6]).expect("合法帧");
        assert!(
            check_state_shape("left", &l, 0, &[0.0; 6]).is_err(),
            "stamp 0"
        );
        assert!(
            check_state_shape("left", &l, 1, &[0.0; 3]).is_err(),
            "短向量"
        );
        assert!(
            check_state_shape("left", &l, 1, &[0.0; 7]).is_err(),
            "长向量"
        );
        let mut bad = [0.0; 6];
        bad[4] = f64::NAN;
        assert!(check_state_shape("left", &l, 1, &bad).is_err(), "NaN");
        bad[4] = f64::INFINITY;
        assert!(check_state_shape("left", &l, 1, &bad).is_err(), "Inf");
    }

    /// **P0-1 belt**：witness 对短向量不 panic、对 NaN 不放行——一律记
    /// malformed 并在终判判死（NaN 在 `> tolerance` 比较里恒 false，旧实现
    /// 会 PASS）。
    #[test]
    fn witness_rejects_malformed_samples_without_panicking() {
        // 短向量：joint_idx 4，positions 只有 3 个 → 不得 panic。
        let mut w = witness();
        w.observe(&StateSample {
            stamp_ns: 2_000,
            positions: vec![0.0, 0.0, 0.0],
        });
        assert_eq!(w.malformed_samples, 1);

        // 出程真实到达后，最终样本 NaN → 必须 FAIL（不是 PASS）。
        let mut w = witness();
        w.observe(&sample(2_000, 0.546));
        w.observe(&sample(3_000, 0.52));
        w.observe(&sample(4_000, 0.5));
        w.observe(&sample(5_000, f64::NAN));
        let err = w.verdict().expect_err("最终 NaN 必须 FAIL");
        assert!(err.contains("malformed"), "{err}");
    }

    /// **P0-1 端到端**：执行期采样返回畸形帧（经 `check_state_shape` 变
    /// Err）→ cancel → `EStop`，绝不 panic 绕过 finalizer。
    #[test]
    fn malformed_frame_during_execution_estops() {
        run(async {
            let mut ops = ScriptedOps::new("left");
            ops.sample_r.push_back(Scripted::Err(
                "left: positions len 3 != layout joints 6（畸形帧）".into(),
            ));
            let mut w = witness();
            let body =
                single_arm_torque_body(&mut ops, vec![], &mut w, Duration::from_secs(1), tb())
                    .await;
            let err = finalize_single(&mut ops, body, tb())
                .await
                .expect_err("必须 FAIL");
            assert!(err.contains("畸形帧"), "{err}");
            assert!(err.contains("EStop"), "{err}");
            assert_eq!(
                ops.calls,
                vec!["enable", "submit", "sample", "cancel:action-1", "estop"]
            );
        });
    }

    /// **P0-3**：enable 阶段黑洞（RPC 永挂）→ 阶段墙钟兜住 → `EStop`；
    /// 真实墙钟 < 1 s（预算 50 ms）。
    #[test]
    fn enable_stage_hang_is_wall_clock_bounded_and_estopped() {
        run(async {
            let mut ops = ScriptedOps::new("left");
            ops.enable_r.push_back(Scripted::Hang);
            let budgets = StageBudgets {
                enable: Duration::from_millis(50),
                ..tb()
            };
            let started = std::time::Instant::now();
            let mut w = witness();
            let body =
                single_arm_torque_body(&mut ops, vec![], &mut w, Duration::from_secs(1), budgets)
                    .await;
            let err = finalize_single(&mut ops, body, budgets)
                .await
                .expect_err("必须 FAIL");
            assert!(started.elapsed() < Duration::from_secs(1), "必须墙钟有界");
            assert!(err.contains("enable stage"), "{err}");
            assert!(err.contains("EStop"), "{err}");
            assert_eq!(ops.calls, vec!["enable", "estop"]);
        });
    }

    /// **P0-3**：执行期 status 黑洞 → exec 阶段墙钟兜住，**保留 `action_id`
    /// 走 cancel → `EStop`**（reviewer："超时必须保留 action id"）。
    #[test]
    fn exec_stage_hang_keeps_action_id_for_cancel() {
        run(async {
            let mut ops = ScriptedOps::new("left");
            ops.status_r.push_back(Scripted::Hang);
            let started = std::time::Instant::now();
            let mut w = witness();
            let body =
                single_arm_torque_body(&mut ops, vec![], &mut w, Duration::from_millis(80), tb())
                    .await;
            let err = finalize_single(&mut ops, body, tb())
                .await
                .expect_err("必须 FAIL");
            assert!(started.elapsed() < Duration::from_secs(1), "必须墙钟有界");
            assert!(err.contains("exec stage"), "{err}");
            assert!(
                ops.calls.contains(&"cancel:action-1".to_string()),
                "超时必须保留 action id 并 cancel：{:?}",
                ops.calls
            );
            assert_eq!(ops.calls.last().map(String::as_str), Some("estop"));
        });
    }

    /// **P0-3**：disable readback 黑洞 → disable 阶段墙钟兜住 → `EStop` + FAIL。
    #[test]
    fn disable_stage_hang_is_wall_clock_bounded_and_estopped() {
        run(async {
            let mut ops = ScriptedOps::new("left");
            ops.disable_r.push_back(Scripted::Hang);
            let budgets = StageBudgets {
                disable: Duration::from_millis(50),
                ..tb()
            };
            let started = std::time::Instant::now();
            let err = finalize_single(&mut ops, Ok(()), budgets)
                .await
                .expect_err("必须 FAIL");
            assert!(started.elapsed() < Duration::from_secs(1), "必须墙钟有界");
            assert!(err.contains("disable stage"), "{err}");
            assert!(err.contains("EStop"), "{err}");
            assert_eq!(ops.calls, vec!["disable", "estop"]);
        });
    }

    /// **P0-2**：pair 失败路径的双 `EStop` **并发**——左侧 `EStop` 自旋等
    /// "右侧 `EStop` 已开始"的 flag：串行实现（左先右后）会死等，只有并发
    /// 发起才能在 2 s 内完成。
    #[test]
    fn pair_failure_estops_run_concurrently() {
        run(async {
            let flag = Arc::new(AtomicBool::new(false));
            let mut left = ScriptedOps::new("left");
            left.estop_waits_for = Some(Arc::clone(&flag));
            let mut right = ScriptedOps::new("right");
            right.estop_signals = Some(Arc::clone(&flag));
            let out = tokio::time::timeout(
                Duration::from_secs(2),
                pair_finalize(&mut left, &mut right, Err("boom".into()), tb()),
            )
            .await
            .expect("串行会死等——必须并发完成");
            assert!(out.is_err());
            assert!(left.calls.contains(&"estop".to_string()));
            assert!(right.calls.contains(&"estop".to_string()));
        });
    }

    /// **P0-2**：pair 成功路径的双 disable 并发（同款 flag 判据）。
    #[test]
    fn pair_disables_run_concurrently() {
        run(async {
            let flag = Arc::new(AtomicBool::new(false));
            let mut left = ScriptedOps::new("left");
            left.disable_waits_for = Some(Arc::clone(&flag));
            let mut right = ScriptedOps::new("right");
            right.disable_signals = Some(Arc::clone(&flag));
            tokio::time::timeout(
                Duration::from_secs(2),
                pair_finalize(&mut left, &mut right, Ok(()), tb()),
            )
            .await
            .expect("串行会死等——必须并发完成")
            .expect("双 disable 确认必须 PASS");
            assert_eq!(left.calls, vec!["disable"]);
            assert_eq!(right.calls, vec!["disable"]);
        });
    }

    /// **P0-2**：右 enable 不确定后的双 `EStop` 并发（round 1 场景 +
    /// round 2 并发判据）。
    #[test]
    fn pair_enable_indeterminate_estops_concurrently() {
        run(async {
            let flag = Arc::new(AtomicBool::new(false));
            let mut left = ScriptedOps::new("left");
            left.estop_waits_for = Some(Arc::clone(&flag));
            let mut right = ScriptedOps::new("right");
            right.enable_r.push_back(Scripted::Err("deadline".into()));
            right.estop_signals = Some(Arc::clone(&flag));
            let out = tokio::time::timeout(
                Duration::from_secs(2),
                pair_enable(&mut left, &mut right, tb()),
            )
            .await
            .expect("串行会死等——必须并发完成");
            let err = out.expect_err("必须 FAIL");
            assert!(err.contains("double EStop"), "{err}");
        });
    }
    /// 单臂全绿链路（含 witness 出→回证据 + disable 确认，零 `EStop`）。
    #[test]
    fn single_arm_happy_path_with_motion_evidence() {
        run(async {
            let mut ops = ScriptedOps::new("left");
            let executing = || {
                Scripted::Ok(TaskSnapshot {
                    phase: TaskPhase::Executing,
                    ..success_snapshot()
                })
            };
            ops.status_r.push_back(executing());
            ops.status_r.push_back(executing());
            ops.status_r.push_back(Scripted::Ok(success_snapshot()));
            // 执行期样本：出程到达；静置后回落两样本。
            ops.sample_r.push_back(Scripted::Ok(sample(2_000, 0.52)));
            ops.sample_r.push_back(Scripted::Ok(sample(3_000, 0.546)));
            ops.sample_r.push_back(Scripted::Ok(sample(4_000, 0.52)));
            ops.sample_r.push_back(Scripted::Ok(sample(5_000, 0.503)));
            ops.sample_r.push_back(Scripted::Ok(sample(6_000, 0.500)));
            let mut w = witness();
            let body =
                single_arm_torque_body(&mut ops, vec![], &mut w, Duration::from_secs(5), tb())
                    .await;
            assert!(body.is_ok(), "{body:?}");
            finalize_single(&mut ops, body, tb())
                .await
                .expect("必须 PASS");
            assert_eq!(ops.calls.last().map(String::as_str), Some("disable"));
            assert!(!ops.calls.iter().any(|c| c == "estop"), "{:?}", ops.calls);
        });
    }
}
