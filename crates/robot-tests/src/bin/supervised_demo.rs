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
//!     Enabled；hold/单关节定位必须为新帧 Safe/TorqueOffConfirmed/fault0）；未确认 → 立即 `EStop` 并判 FAIL；
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
//! - **原探针硬 gate（AC-E12 ③）**：offset ∈ (0, 0.087]、**wire 实际斜率**
//!   ≤ 0.14 rad/s——段时长按 ms **向上取整**并对量化后的值复核（round 1
//!   P1-1：转换器四舍五入到 ms，浮点秒上的校验会被量化绕过）。超限拒绝
//!   提交，不 clamp。
//!   单关节入口缺省沿原幅度；显式计划可给出不超过30°的独立预算，范围与速率门保持。
//!
//! ## phase（各自独立调用 = operator 显式；`all` 串行，一步 FAIL 即中止）
//!
//! | phase | 上扭 | 判据 |
//! |---|---|---|
//! | `read-state` | 否 | `GetState` 可达 + `layout_id` == config 派生 + `stamp_ns` 在推进 |
//! | `hold` | 是 | 仅左侧，以实际 S1 锚点保持20秒；不提交任务，严格确认断扭 |
//! | `single-joint` | 是 | 仅左侧，提交一次单关节目标；实测到位后断扭，不回程 |
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
/// 仅显式单关节计划可用；不改变原探针或缺省幅度。
const MAX_SINGLE_JOINT_OFFSET_RAD: f64 = std::f64::consts::FRAC_PI_6;
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
    /// 提交回执的本地等待上限；不替代任务的执行期限。
    #[cfg(feature = "supervised-s0")]
    submit: Duration,
    /// disable 确认阶段（`SetEnabled(false)` + readback）总预算。
    disable: Duration,
    /// settle 阶段（静置 + 回程采样 + 终判）总预算。
    settle: Duration,
}

/// 生产预算 = 内层健康路径上限 + 一次 [`CLIENT_HARD_TIMEOUT`] 的穿透余量。
const PROD_STAGE_BUDGETS: StageBudgets = StageBudgets {
    enable: Duration::from_secs(9), // 4 (SetEnabled) + 5 (ENABLE_WAIT)
    #[cfg(feature = "supervised-s0")]
    submit: CLIENT_HARD_TIMEOUT,
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
     single-joint缺省沿原幅度；显式计划例外按 --single-joint-max-offset-rad（最大30°）核对；
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
    SingleJoint,
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
            "single-joint" => Ok(Self::SingleJoint),
            "single-arm" => Ok(Self::SingleArm),
            "dual-arm" => Ok(Self::DualArm),
            "grippers" => Ok(Self::Grippers),
            "all" => Ok(Self::All),
            other => Err(format!(
                "--phase: unknown {other:?} (read-state|hold|single-joint|single-arm|dual-arm|grippers|all)"
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
    joint_name: Option<String>,
    target_position_rad: Option<f64>,
    target_offset_raw: Option<i32>,
    duration_ms: Option<u32>,
    task_timeout_ms: Option<u64>,
    cancel_after_ms: Option<u32>,
    single_joint_max_offset_rad: Option<f64>,
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
            joint_name: None,
            target_position_rad: None,
            target_offset_raw: None,
            duration_ms: None,
            task_timeout_ms: None,
            cancel_after_ms: None,
            single_joint_max_offset_rad: None,
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
                "--joint-name" => {
                    args.joint_name = Some(value(raw, &mut i, "--joint-name")?.into());
                }
                "--target-position-rad" => {
                    args.target_position_rad = Some(
                        value(raw, &mut i, "--target-position-rad")?
                            .parse()
                            .map_err(|e| format!("--target-position-rad: {e}"))?,
                    );
                }
                "--target-offset-raw" => {
                    args.target_offset_raw = Some(
                        value(raw, &mut i, "--target-offset-raw")?
                            .parse()
                            .map_err(|e| format!("--target-offset-raw: {e}"))?,
                    );
                }
                "--duration-ms" => {
                    args.duration_ms = Some(
                        value(raw, &mut i, "--duration-ms")?
                            .parse()
                            .map_err(|e| format!("--duration-ms: {e}"))?,
                    );
                }
                "--task-timeout-ms" => {
                    args.task_timeout_ms = Some(
                        value(raw, &mut i, "--task-timeout-ms")?
                            .parse()
                            .map_err(|e| format!("--task-timeout-ms: {e}"))?,
                    );
                }
                "--cancel-after-ms" => {
                    if args.cancel_after_ms.is_some() {
                        return Err("--cancel-after-ms 不得重复".into());
                    }
                    args.cancel_after_ms = Some(
                        value(raw, &mut i, "--cancel-after-ms")?
                            .parse()
                            .map_err(|e| format!("--cancel-after-ms: {e}"))?,
                    );
                }
                "--single-joint-max-offset-rad" => {
                    if args.single_joint_max_offset_rad.is_some() {
                        return Err("--single-joint-max-offset-rad 不得重复".into());
                    }
                    let budget = value(raw, &mut i, "--single-joint-max-offset-rad")?
                        .parse::<f64>()
                        .map_err(|error| format!("--single-joint-max-offset-rad: {error}"))?;
                    validate_single_joint_budget(budget)?;
                    args.single_joint_max_offset_rad = Some(budget);
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
        } else if args.phase == Phase::SingleJoint {
            if !cfg!(feature = "supervised-s0") || probe_option_seen {
                return Err("single-joint 需要 supervised-s0，禁止旧探针参数".into());
            }
            if args.hold_anchor_rad.is_none()
                || args
                    .joint_name
                    .as_deref()
                    .is_none_or(|name| name.trim().is_empty())
                || args.target_position_rad.is_some() == args.target_offset_raw.is_some()
                || args.target_position_rad.is_some_and(|position| {
                    !position.is_finite() || position.abs() > f64::from(f32::MAX)
                })
                || args.target_offset_raw == Some(0)
                || args.duration_ms.is_none_or(|duration| duration == 0)
                || args
                    .cancel_after_ms
                    .is_some_and(|delay| delay == 0 || delay >= args.duration_ms.unwrap_or(0))
                || args.task_timeout_ms.is_none_or(|timeout| {
                    timeout <= u64::from(args.duration_ms.unwrap_or(0))
                        || timeout.checked_mul(1_000_000).is_none()
                })
            {
                return Err("single-joint 必须显式提供 joint-name、互斥的 target-position-rad/target-offset-raw、duration-ms、task-timeout-ms 和 hold-anchor-rad".into());
            }
        } else if args.hold_anchor_rad.is_some() {
            return Err("--hold-anchor-rad 仅用于 hold 或 single-joint".into());
        }
        if args.phase != Phase::SingleJoint
            && (args.joint_name.is_some()
                || args.target_position_rad.is_some()
                || args.target_offset_raw.is_some()
                || args.duration_ms.is_some()
                || args.cancel_after_ms.is_some()
                || args.single_joint_max_offset_rad.is_some()
                || args.task_timeout_ms.is_some())
        {
            return Err("单关节目标与时长参数仅用于 single-joint".into());
        }
        validate_probe_geometry(args.probe_offset_rad, args.settle_tolerance_rad)?;
        Ok(args)
    }
}

fn validate_single_joint_budget(budget: f64) -> Result<(), String> {
    if !budget.is_finite() || budget <= 0.0 || budget > MAX_SINGLE_JOINT_OFFSET_RAD {
        return Err("单关节显式幅度预算必须为(0,π/6]内的有限rad值".into());
    }
    Ok(())
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
  single-joint  左8位置或右6位置+3轮零速；一次位置关节定位、实测到位后断扭，不回程
  single-arm    单臂探针（--side left|right）：enable → 出→回 + 运动证据 → disable 确认
  dual-arm      双臂 run_chunk_pair 探针（协议级 cancel/EStop 收尾 + 双侧运动证据）
  grippers      双夹爪探针（D6 Scripted 同车；关节 = *_gripper）
  all           串行全部（read-state → single-arm×2 → dual-arm → grippers）

OPTIONS:
  --side <left|right>            single-arm/single-joint 的目标臂；hold仅left（默认 left）
  --left-gateway <ADDR>          默认 127.0.0.1:50051
  --right-gateway <ADDR>         默认 127.0.0.1:50052
  --left-config <PATH>           默认 /etc/robot/xlerobot-left-arm.toml
  --right-config <PATH>          默认 /etc/robot/xlerobot-right-arm.toml
  --left-hardware-id <ID>        默认 left_arm
  --right-hardware-id <ID>       默认 right_arm
  --token <TOKEN>                operator+ token（或 env ROBOT_TOKEN）
  --hold-anchor-rad <JSON>       本次S1启动锚点数组；hold/single-joint必填，固定±4raw
  --joint-name <NAME>           single-joint必填，按当前配置选择关节
  --target-position-rad <RAD>   single-joint必填，绝对位置rad
  --target-offset-raw <RAW>     或相对本次S1锚点的raw偏移；与绝对rad目标互斥
  --duration-ms <MS>            single-joint必填，正整数运动时长
  --task-timeout-ms <MS>        single-joint必填，覆盖运动、到位超时和请求余量
  --cancel-after-ms <MS>        single-joint可选；任务中途仅取消一次，验证最后发布目标保持后断扭
  --single-joint-max-offset-rad <RAD>  仅single-joint显式计划幅度预算，默认0.087；最大π/6（30°）
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

/// hold 与单关节定位共用的身份、源时间和原始计数检查；不将校准边界容差用作保持精度。
#[cfg(feature = "supervised-s0")]
#[derive(Clone)]
struct HoldCheck {
    robot_id: String,
    anchor_raw: Vec<i32>,
    position_slots: Vec<usize>,
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
            position_slots: (0..joints).collect(),
            last_stamp_ns: 0,
        })
    }

    /// 混合布局只换算位置槽；轮槽在命令锚点中必须显式为零。
    fn for_single_joint(
        robot_id: String,
        anchor_rad: &[f64],
        position_slots: Vec<usize>,
    ) -> Result<Self, String> {
        if robot_id.is_empty()
            || position_slots.is_empty()
            || position_slots.iter().any(|&slot| slot >= anchor_rad.len())
            || anchor_rad
                .iter()
                .enumerate()
                .any(|(slot, &value)| !position_slots.contains(&slot) && value != 0.0)
        {
            return Err("单关节锚点位置槽或轮零速槽无效".into());
        }
        let mut check = Self {
            robot_id,
            anchor_raw: vec![0; anchor_rad.len()],
            position_slots,
            last_stamp_ns: 0,
        };
        check.anchor_raw = check.raw_positions_by_slot(anchor_rad)?;
        Ok(check)
    }

    fn raw_positions_by_slot(&self, positions: &[f64]) -> Result<Vec<i32>, String> {
        if positions.len() != self.anchor_raw.len() || positions.iter().any(|p| !p.is_finite()) {
            return Err("hold 完整反馈维度或有限值无效".into());
        }
        let mut raw = vec![0; positions.len()];
        for &slot in &self.position_slots {
            raw[slot] = Self::raw_positions(&[positions[slot]])?[0];
        }
        Ok(raw)
    }

    /// 轮反馈不是位置rad；日志以null保留槽位，不能把命令零速当作实测位置。
    fn logged_raw_positions(&self, positions: &[f64]) -> Result<Vec<Option<i32>>, String> {
        Ok(self
            .raw_positions_by_slot(positions)?
            .into_iter()
            .enumerate()
            .map(|(slot, raw)| self.position_slots.contains(&slot).then_some(raw))
            .collect())
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
        self.raw_positions_by_slot(&state.position_rad)?;
        self.last_stamp_ns = state.stamp_ns;
        Ok(())
    }

    fn check_position(&self, state: &RobotState) -> Result<(), String> {
        let positions = self.raw_positions_by_slot(&state.position_rad)?;
        if positions.len() != self.anchor_raw.len() {
            return Err("hold 状态与固定锚点长度不匹配".into());
        }
        for &slot in &self.position_slots {
            let (actual, target) = (positions[slot], self.anchor_raw[slot]);
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

/// 本次显式单目标请求；完整数组只用于观测，绝不编码为旧全向量请求。
#[cfg(feature = "supervised-s0")]
struct SingleJointPlan {
    hardware_id: String,
    joint_name: String,
    joint_idx: usize,
    target_position_rad: f64,
    duration_ms: u32,
    task_timeout_ms: u64,
    cancel_after_ms: Option<u32>,
    max_offset_rad: f64,
    settle_ms: u32,
    stable_ms: u32,
    expected_raw: Vec<i32>,
    expected_positions: Vec<f64>,
    limits: Vec<SingleJointLimit>,
    position_slots: Vec<usize>,
}

/// 启用前读取同一候选配置的运动边界；执行期仍由 S1 独立守门。
#[cfg(feature = "supervised-s0")]
struct SingleJointLimit {
    min: f64,
    max: f64,
    velocity: f64,
    tolerance_rad: f64,
}

#[cfg(feature = "supervised-s0")]
#[allow(
    clippy::cast_possible_truncation,
    reason = "复核 S0 实际 f32 编码，转换前先检查有限范围"
)]
fn check_single_joint_limits(limits: &[SingleJointLimit], positions: &[f64]) -> Result<(), String> {
    if limits.len() != positions.len() {
        return Err("单关节配置范围与位置维度不匹配".into());
    }
    for (limit, &position) in limits.iter().zip(positions) {
        if !position.is_finite()
            || position.abs() > f64::from(f32::MAX)
            || position < limit.min
            || position > limit.max
            || f64::from(position as f32) < limit.min
            || f64::from(position as f32) > limit.max
        {
            return Err("单关节位置或实际f32编码值超出候选S1配置范围".into());
        }
    }
    Ok(())
}

#[cfg(feature = "supervised-s0")]
fn motion_config(
    config: &toml::Value,
    layout: &HardwareLayout,
) -> Result<(Vec<SingleJointLimit>, u32, u32, u32), String> {
    let section = config
        .get("s1")
        .and_then(|s1| s1.get("single_joint"))
        .and_then(toml::Value::as_table)
        .ok_or("候选配置缺少 s1.single_joint")?;
    if section.len() != 4
        || section.keys().any(|key| {
            ![
                "max_duration_ms",
                "settle_timeout_ms",
                "stable_window_ms",
                "joints",
            ]
            .contains(&key.as_str())
        })
    {
        return Err("s1.single_joint 字段缺失或未知".into());
    }
    let positive_ms = |key: &str| {
        section
            .get(key)
            .and_then(toml::Value::as_integer)
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| format!("s1.single_joint.{key} 必须为正u32"))
    };
    let max_duration = positive_ms("max_duration_ms")?;
    let settle = positive_ms("settle_timeout_ms")?;
    let stable = positive_ms("stable_window_ms")?;
    if stable >= settle {
        return Err("稳定窗口必须小于到位超时".into());
    }
    let joints = section
        .get("joints")
        .and_then(toml::Value::as_array)
        .ok_or("s1.single_joint.joints 缺失")?;
    if joints.len() != layout.joints.len() {
        return Err("候选单关节配置未完整覆盖当前布局".into());
    }
    let mut limits = Vec::with_capacity(joints.len());
    for name in &layout.joints {
        let matching: Vec<_> = joints
            .iter()
            .filter(|joint| joint.get("name").and_then(toml::Value::as_str) == Some(name.as_str()))
            .collect();
        if matching.len() != 1 {
            return Err(format!("候选单关节配置缺少或重复关节：{name}"));
        }
        let joint = matching[0].as_table().ok_or("单关节约束不是表")?;
        if joint.len() != 5
            || joint.keys().any(|key| {
                ![
                    "name",
                    "min_position_rad",
                    "max_position_rad",
                    "max_velocity_rad_s",
                    "tolerance_rad",
                ]
                .contains(&key.as_str())
            })
        {
            return Err("单关节运动约束字段缺失或未知".into());
        }
        let number = |key: &str| {
            joint
                .get(key)
                .and_then(|value| value.to_string().parse::<f64>().ok())
                .filter(|value| value.is_finite())
                .ok_or_else(|| format!("{name}.{key} 不是有限数值"))
        };
        let (min, max, velocity, tolerance) = (
            number("min_position_rad")?,
            number("max_position_rad")?,
            number("max_velocity_rad_s")?,
            number("tolerance_rad")?,
        );
        if min >= max || velocity <= 0.0 || tolerance < 0.0 {
            return Err(format!("{name} 的范围、速率或容差无效"));
        }
        limits.push(SingleJointLimit {
            min,
            max,
            velocity,
            tolerance_rad: tolerance,
        });
    }
    Ok((limits, max_duration, settle, stable))
}

#[cfg(feature = "supervised-s0")]
impl SingleJointPlan {
    fn position_slots(
        args: &Args,
        layout: &HardwareLayout,
        config: &toml::Value,
    ) -> Result<Vec<usize>, String> {
        if args.side == Side::Left {
            return if layout.joints.len() == 8 {
                Ok((0..8).collect())
            } else {
                Err("single-joint 左侧需要完整8位置关节布局".into())
            };
        }
        let hardware: robot_s0::config_loader::HardwareSection = config
            .get("hardware")
            .and_then(toml::Value::as_array)
            .and_then(|entries| {
                entries.iter().find(|entry| {
                    entry.get("id").and_then(toml::Value::as_str)
                        == Some(args.right_hardware_id.as_str())
                })
            })
            .ok_or("右侧候选配置缺少所选hardware")?
            .clone()
            .try_into()
            .map_err(|error| format!("右侧hardware解析失败：{error}"))?;
        if !hardware.feetech_bus2_mixed_raw
            || hardware.auto_enable != Some(false)
            || hardware.joints != layout.joints
            || hardware.motor_ids != layout.motor_ids
        {
            return Err("右侧需要同一完整布局、显式混合模式及auto_enable=false".into());
        }
        let command = robot_s0::grouped_position::XleRobotBus2Layout::from_hardware(&hardware)
            .and_then(|mixed| mixed.command([0.0; 6], [0; 3], 1, 0, 1))
            .map_err(|error| error.to_string())?;
        let robot_core::adapter::JointSelection::MixedPositionRawVelocity { position_mask, .. } =
            command.selection
        else {
            return Err("右侧配置未生成混合布局".into());
        };
        Ok((0..layout.joints.len())
            .filter(|slot| position_mask & (1 << slot) != 0)
            .collect())
    }

    fn mixed(&self) -> bool {
        self.position_slots.len() != self.expected_positions.len()
    }

    fn check_limits(&self, positions: &[f64]) -> Result<(), String> {
        if positions.len() != self.expected_positions.len()
            || positions.iter().any(|value| !value.is_finite())
        {
            return Err("单关节完整反馈维度或有限值无效".into());
        }
        let positions: Vec<_> = self
            .position_slots
            .iter()
            .map(|&slot| positions[slot])
            .collect();
        check_single_joint_limits(&self.limits, &positions)
    }

    fn prepare(
        args: &Args,
        layout: &HardwareLayout,
        anchor: &[f64],
        config: &toml::Value,
    ) -> Result<Self, String> {
        let position_slots = Self::position_slots(args, layout, config)?;
        if anchor.len() != layout.joints.len() {
            return Err("single-joint 锚点未完整覆盖布局".into());
        }
        let profile = config.get("s1").and_then(|s1| s1.get("profile"));
        if profile
            .and_then(|profile| profile.get("command_mode"))
            .and_then(toml::Value::as_str)
            != Some("joint_position")
            || !profile
                .and_then(|profile| profile.get("backends"))
                .and_then(toml::Value::as_array)
                .is_some_and(|backends| {
                    backends
                        .iter()
                        .any(|backend| backend.as_str() == Some("scripted"))
                })
        {
            return Err("候选S1必须显式使用scripted和joint_position".into());
        }
        let joint_name = args.joint_name.as_deref().ok_or("缺少 joint-name")?;
        let joint_idx = layout
            .joints
            .iter()
            .position(|name| name == joint_name)
            .filter(|slot| position_slots.contains(slot))
            .ok_or("joint-name 必须是当前配置的位置关节，不能选择轮速槽")?;
        let anchor_raw =
            HoldCheck::for_single_joint("preflight".into(), anchor, position_slots.clone())?
                .anchor_raw;
        let target = match (args.target_position_rad, args.target_offset_raw) {
            (Some(target), None) => target,
            (None, Some(offset)) => {
                let raw = anchor_raw[joint_idx]
                    .checked_add(offset)
                    .filter(|raw| (0..=4095).contains(raw))
                    .ok_or("单关节raw目标溢出或超出12位范围")?;
                f64::from(robot_adapter_feetech::raw_to_rad(raw))
            }
            _ => return Err("单关节绝对目标与raw偏移必须提供且互斥".into()),
        };
        let duration_ms = args.duration_ms.ok_or("缺少 duration-ms")?;
        let task_timeout_ms = args.task_timeout_ms.ok_or("缺少 task-timeout-ms")?;
        let position_layout = HardwareLayout {
            joints: position_slots
                .iter()
                .map(|&slot| layout.joints[slot].clone())
                .collect(),
            motor_ids: position_slots
                .iter()
                .map(|&slot| layout.motor_ids[slot])
                .collect(),
            layout_id: layout.layout_id,
        };
        let (limits, max_duration, settle_ms, stable_ms) = motion_config(config, &position_layout)?;
        if duration_ms == 0 || duration_ms > max_duration {
            return Err("单关节时长超出候选S1配置".into());
        }
        let required_ms = u64::from(duration_ms)
            .checked_add(u64::from(settle_ms))
            .ok_or("single-joint 期限预算溢出")?;
        if task_timeout_ms <= required_ms {
            return Err("task-timeout-ms 必须大于运动时长与到位超时之和，预留请求开销".into());
        }
        let max_offset_rad = args
            .single_joint_max_offset_rad
            .unwrap_or(MAX_PROBE_OFFSET_RAD);
        check_single_joint_motion(anchor[joint_idx], target, duration_ms, max_offset_rad)?;
        let mut expected_raw = anchor_raw.clone();
        expected_raw[joint_idx] = HoldCheck::raw_positions(&[target])?[0];
        let mut expected_positions: Vec<_> = anchor.iter().copied().map(position_for_s0).collect();
        expected_positions[joint_idx] = position_for_s0(target);
        let plan = Self {
            hardware_id: match args.side {
                Side::Left => args.left_hardware_id.clone(),
                Side::Right => args.right_hardware_id.clone(),
            },
            joint_name: joint_name.into(),
            joint_idx,
            target_position_rad: target,
            duration_ms,
            task_timeout_ms,
            cancel_after_ms: args.cancel_after_ms,
            max_offset_rad,
            settle_ms,
            stable_ms,
            expected_raw,
            expected_positions,
            limits,
            position_slots,
        };
        plan.check_start(anchor)?;
        Ok(plan)
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "复核已通过范围检查的 S0 f32 运动距离"
    )]
    fn check_start(&self, positions: &[f64]) -> Result<(), String> {
        self.check_limits(positions)?;
        let mut target = positions.to_vec();
        target[self.joint_idx] = self.target_position_rad;
        self.check_limits(&target)?;
        let selected_limit = &self.limits[self
            .position_slots
            .iter()
            .position(|&slot| slot == self.joint_idx)
            .ok_or("所选位置槽缺少约束")?];
        let start = positions[self.joint_idx];
        check_single_joint_motion(
            start,
            self.target_position_rad,
            self.duration_ms,
            self.max_offset_rad,
        )?;
        let distance = (self.target_position_rad - start)
            .abs()
            .max((f64::from(self.target_position_rad as f32) - f64::from(start as f32)).abs());
        if (position_for_s0(self.target_position_rad) - position_for_s0(start)).abs()
            <= 2.0 * selected_limit.tolerance_rad
        {
            return Err("初末位置的配置容差区间相交，不能证明定位动作".into());
        }
        if distance / (f64::from(self.duration_ms) / 1000.0) > selected_limit.velocity {
            return Err("单关节所需速率超过候选S1配置".into());
        }
        Ok(())
    }

    fn action_spec(&self) -> ActionSpec {
        use robot_msg::msg::{SingleJointPositionPayload, SingleJointPositionPayloadArgs};
        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let hardware_id = builder.create_string(&self.hardware_id);
        let joint_name = builder.create_string(&self.joint_name);
        let root = SingleJointPositionPayload::create(
            &mut builder,
            &SingleJointPositionPayloadArgs {
                hardware_id: Some(hardware_id),
                joint_name: Some(joint_name),
                target_position_rad: self.target_position_rad,
                duration_ms: self.duration_ms,
            },
        );
        builder.finish(root, None);
        ActionSpec {
            kind: robot_rpc::client::dual_arm_workflow::ActionKindWire::Scripted,
            payload_schema_id: robot_msg::schema_id::SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD,
            payload: builder.finished_data().to_vec(),
        }
    }
}

#[cfg(feature = "supervised-s0")]
#[allow(
    clippy::cast_possible_truncation,
    reason = "起点和目标已通过有限范围检查，与S1使用同一S0编码"
)]
fn position_for_s0(position: f64) -> f64 {
    f64::from(position as f32)
}

/// 到位只按配置rad差值判断，不把raw四舍五入或额外epsilon变成隐含容差。
#[cfg(feature = "supervised-s0")]
fn positions_within_tolerances(actual: &[f64], targets: &[f64], tolerances: &[f64]) -> bool {
    actual.len() == targets.len()
        && actual.len() == tolerances.len()
        && actual
            .iter()
            .zip(targets)
            .zip(tolerances)
            .all(|((&actual, &target), &tolerance)| {
                actual.is_finite()
                    && target.is_finite()
                    && tolerance.is_finite()
                    && tolerance >= 0.0
                    && (actual - target).abs() <= tolerance
            })
}

#[cfg(feature = "supervised-s0")]
#[allow(
    clippy::cast_possible_truncation,
    reason = "与 S0 实际 f32 位置编码一致，先校验有限值"
)]
fn check_single_joint_motion(
    start: f64,
    target: f64,
    duration_ms: u32,
    max_offset_rad: f64,
) -> Result<(), String> {
    validate_single_joint_budget(max_offset_rad)?;
    if duration_ms == 0
        || !start.is_finite()
        || !target.is_finite()
        || start.abs() > f64::from(f32::MAX)
        || target.abs() > f64::from(f32::MAX)
    {
        return Err("单关节起点、目标或时长无效".into());
    }
    let encoded_start = f64::from(start as f32);
    let encoded_target = f64::from(target as f32);
    let distance = (target - start)
        .abs()
        .max((encoded_target - encoded_start).abs());
    if distance > max_offset_rad
        || distance / (f64::from(duration_ms) / 1000.0) > MAX_SLOPE_RAD_PER_S
    {
        return Err(format!(
            "单关节目标超出本次{max_offset_rad}rad幅度预算或原0.14rad/s速率硬限制"
        ));
    }
    Ok(())
}

/// 每个执行期样本都必须推进；单关节轨迹走廊及到位沿各关节配置容差。
#[cfg(feature = "supervised-s0")]
struct SingleJointCheck {
    source: HoldCheck,
    layout_id: u64,
    joint_idx: usize,
    expected_raw: Vec<i32>,
    expected_positions: Vec<f64>,
    tolerances_rad: Vec<f64>,
}

#[cfg(feature = "supervised-s0")]
impl SingleJointCheck {
    fn within_tolerances(&self, actual: &[f64], targets: &[f64]) -> bool {
        if actual.len() != self.expected_positions.len() || targets.len() != actual.len() {
            return false;
        }
        let positions = |values: &[f64]| {
            self.source
                .position_slots
                .iter()
                .map(|&slot| values[slot])
                .collect::<Vec<_>>()
        };
        positions_within_tolerances(
            &positions(actual),
            &positions(targets),
            &self.tolerances_rad,
        )
    }

    fn observe(
        &mut self,
        state: &RobotState,
        hardware_id: &str,
        now: u64,
        final_sample: bool,
    ) -> Result<(), String> {
        if state.layout_id != self.layout_id {
            return Err("single-joint 反馈 layout 不匹配".into());
        }
        if state.stamp_ns <= self.source.last_stamp_ns {
            return Err("single-joint 来源未推进".into());
        }
        self.source.observe(state, hardware_id, now)?;
        if !HoldCheck::torque_on(state) {
            return Err("single-joint 未保持 Enabled/Ok/Active/TorqueOn/fault0".into());
        }
        let raw = self.source.raw_positions_by_slot(&state.position_rad)?;
        if raw.len() != self.expected_positions.len()
            || self.source.position_slots.len() != self.tolerances_rad.len()
        {
            return Err("single-joint 反馈关节数量变化".into());
        }
        if final_sample {
            return if self.within_tolerances(&state.position_rad, &self.expected_positions) {
                Ok(())
            } else {
                Err("single-joint 最终位置超出配置到位容差".into())
            };
        }
        for (position_index, &idx) in self.source.position_slots.iter().enumerate() {
            let actual = raw[idx];
            let target = self.expected_raw[idx];
            let start = self.source.anchor_raw[idx];
            let actual_rad = state.position_rad[idx];
            let target_rad = self.expected_positions[idx];
            let start_rad = f64::from(robot_adapter_feetech::raw_to_rad(start));
            let tolerance = self.tolerances_rad[position_index];
            let valid = if idx == self.joint_idx {
                actual_rad >= start_rad.min(target_rad) - tolerance
                    && actual_rad <= start_rad.max(target_rad) + tolerance
            } else {
                (actual_rad - target_rad).abs() <= tolerance
            };
            if !valid {
                return Err(format!(
                    "single-joint 位置越界：slot={idx}, actual_raw={actual}, start_raw={start}, target_raw={target}, final={final_sample}"
                ));
            }
        }
        Ok(())
    }
}

/// 独立镜像只证明主 topic 已成功发布，不代表 S0 已执行或已经停止。
#[cfg(feature = "supervised-s0")]
#[derive(Clone, Debug)]
struct PublishedCommand {
    metadata: robot_transport::iceoryx2::ObserverMetadata,
    seq: u64,
    stamp_ns: u64,
    received_ns: u64,
    source_id: u32,
    layout_id: u64,
    source_task_id: u64,
    deadline_ns: u64,
    mode: robot_msg::msg::ControlMode,
    on_timeout_hold: bool,
    target: Vec<f64>,
    raw_velocities: Vec<i16>,
}

#[cfg(feature = "supervised-s0")]
type CommandQueue = std::sync::Mutex<std::collections::VecDeque<Result<PublishedCommand, String>>>;

/// RPC 等待期间也持续收取 FIFO；满队列即失败，不把漏掉的帧当作最后目标。
#[cfg(feature = "supervised-s0")]
struct CommandObserver {
    queue: Arc<CommandQueue>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "supervised-s0")]
impl CommandObserver {
    async fn open(robot_id: &str, hardware_id: &str) -> Result<Self, String> {
        let topic = format!("/{robot_id}/s1/{hardware_id}/command");
        let queue = Arc::new(CommandQueue::new(std::collections::VecDeque::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (thread_queue, thread_stop) = (Arc::clone(&queue), Arc::clone(&stop));
        let worker = std::thread::Builder::new()
            .name("robot-cmd-watch".into())
            .spawn(move || {
                let result = Self::receive(&topic, &thread_queue, &thread_stop, ready_tx);
                if let Err(error) = result
                    && let Ok(mut queue) = thread_queue.lock()
                {
                    queue.clear();
                    queue.push_back(Err(error));
                }
            })
            .map_err(|error| format!("命令镜像线程创建失败：{error}"))?;
        let observer = Self {
            queue,
            stop,
            worker: Some(worker),
        };
        staged(CLIENT_HARD_TIMEOUT, "command observer ready", async {
            ready_rx.await.map_err(|_| "命令镜像未就绪".into())
        })
        .await?;
        Ok(observer)
    }

    fn receive(
        topic: &str,
        queue: &CommandQueue,
        stop: &AtomicBool,
        ready: tokio::sync::oneshot::Sender<()>,
    ) -> Result<(), String> {
        use iceoryx2::prelude::NodeBuilder;
        use iceoryx2::service::ipc::Service;
        use robot_transport::iceoryx2::observer_subscriber;

        let node = NodeBuilder::new()
            .create::<Service>()
            .map_err(|error| format!("命令镜像 Node：{error:?}"))?;
        let mut subscriber = observer_subscriber::<robot_msg::markers::JointCommand>(&node, topic)
            .map_err(|error| format!("命令镜像不可用：{error}"))?;
        let _ = ready.send(());
        while !stop.load(Ordering::Acquire) {
            // 有界 drain，退出请求不会被持续到达的帧饿死。
            for _ in 0..64 {
                let Some(sample) = subscriber.try_recv().map_err(|error| error.to_string())? else {
                    break;
                };
                let view = sample.view().map_err(|error| error.to_string())?;
                let command = view.body.get();
                let frame = PublishedCommand {
                    metadata: sample.metadata(),
                    seq: view.header.seq(),
                    stamp_ns: view.header.stamp_ns(),
                    received_ns: SystemClock::new().now().0,
                    source_id: view.header.source_id(),
                    layout_id: command.layout_id(),
                    source_task_id: command.source_task_id(),
                    deadline_ns: command.deadline_ns(),
                    mode: command.mode(),
                    on_timeout_hold: command.on_timeout() == robot_msg::msg::StaleAction::Hold,
                    target: command
                        .target()
                        .map_or_else(Vec::new, |target| target.iter().collect()),
                    raw_velocities: command
                        .raw_velocities()
                        .map_or_else(Vec::new, |values| values.iter().collect()),
                };
                let mut queue = queue.lock().map_err(|_| "命令镜像队列锁异常")?;
                if queue.len() >= 1024 {
                    return Err("命令镜像本地队列溢出，证据不完整".into());
                }
                queue.push_back(Ok(frame));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    fn drain(&self) -> Result<Vec<PublishedCommand>, String> {
        let mut queue = self.queue.lock().map_err(|_| "命令镜像队列锁异常")?;
        if let Some(Err(error)) = queue.iter().find(|frame| frame.is_err()) {
            return Err(error.clone());
        }
        queue.drain(..).collect()
    }

    fn peek(&self) -> Result<Vec<PublishedCommand>, String> {
        self.queue
            .lock()
            .map_err(|_| "命令镜像队列锁异常")?
            .iter()
            .cloned()
            .collect()
    }
}

#[cfg(feature = "supervised-s0")]
impl Drop for CommandObserver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(feature = "supervised-s0")]
#[derive(Default)]
struct CommandEvidence {
    frames: std::collections::VecDeque<PublishedCommand>,
}

#[cfg(feature = "supervised-s0")]
impl CommandEvidence {
    fn observe(
        &mut self,
        frames: Vec<PublishedCommand>,
        plan: &SingleJointPlan,
        check: &SingleJointCheck,
        now: u64,
    ) -> Result<(), String> {
        for frame in frames {
            if frame.source_id != 2
                || frame.layout_id != check.layout_id
                || !frame.on_timeout_hold
                || frame.mode
                    != if plan.mixed() {
                        robot_msg::msg::ControlMode::MixedPositionRawVelocity
                    } else {
                        robot_msg::msg::ControlMode::Position
                    }
                || frame.source_task_id != frame.seq
                || frame.target.len() != plan.expected_positions.len()
                || frame.stamp_ns == 0
                || frame.stamp_ns > frame.received_ns
                || frame.received_ns - frame.stamp_ns > 200_000_000
                || frame.deadline_ns <= frame.stamp_ns
            {
                return Err("命令镜像身份、模式、维度或源时间无效".into());
            }
            plan.check_limits(&frame.target)?;
            if plan.mixed()
                && (frame.raw_velocities.len() != frame.target.len()
                    || frame.raw_velocities.iter().any(|&raw| raw != 0)
                    || frame.target.iter().enumerate().any(|(slot, &target)| {
                        !plan.position_slots.contains(&slot) && target != 0.0
                    }))
            {
                return Err("混合单关节命令必须完整覆盖轮零目标及全槽raw零速".into());
            }
            if let Some(previous) = self.frames.back()
                && (frame.metadata.publisher_instance_id != previous.metadata.publisher_instance_id
                    || Some(frame.metadata.observer_seq)
                        != previous.metadata.observer_seq.checked_add(1)
                    || frame.metadata.dropped_frames != previous.metadata.dropped_frames
                    || Some(frame.seq) != previous.seq.checked_add(1)
                    || frame.stamp_ns <= previous.stamp_ns)
            {
                return Err("命令镜像换源、丢帧或序号/时间未严格推进".into());
            }
            let raw = check.source.raw_positions_by_slot(&frame.target)?;
            for &index in &plan.position_slots {
                let target = raw[index];
                let start = check.source.anchor_raw[index];
                let end = plan.expected_raw[index];
                if target < start.min(end) - 4 || target > start.max(end) + 4 {
                    return Err("命令镜像目标超出本次单向轨迹范围".into());
                }
            }
            // 只保留近期边界帧；连续性已逐帧核验，取消窗口不足则显式拒绝。
            if self.frames.len() == 1024 {
                self.frames.pop_front();
            }
            self.frames.push_back(frame);
        }
        let latest = self.frames.back().ok_or("未收到命令镜像")?;
        if latest.stamp_ns > now || now - latest.stamp_ns > 200_000_000 {
            return Err("命令镜像最新帧不新鲜".into());
        }
        Ok(())
    }

    fn cancelled_target(&self, at_ns: u64) -> Result<Option<&PublishedCommand>, String> {
        let last = self
            .frames
            .iter()
            .rev()
            .find(|frame| frame.stamp_ns < at_ns)
            .ok_or("缺少取消前最后发布命令")?;
        let mut after = false;
        for frame in self.frames.iter().filter(|frame| frame.stamp_ns >= at_ns) {
            if frame.stamp_ns == at_ns || !same_command_target(&last.target, &frame.target) {
                return Err("取消后的完整命令目标未冻结在最后成功发布值".into());
            }
            after = true;
        }
        Ok(after.then_some(last))
    }
}

#[cfg(feature = "supervised-s0")]
#[allow(
    clippy::cast_possible_truncation,
    reason = "镜像值已验证有限及 f32 范围；比较实际 S0 编码"
)]
fn same_command_target(left: &[f64], right: &[f64]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(&a, &b)| (a as f32).to_bits() == (b as f32).to_bits())
}

struct SideRt {
    side: Side,
    hardware_id: String,
    layout: HardwareLayout,
    client: RobotControlClient<Channel>,
    token: String,
    seq: AtomicU64,
    last_submission_id: Option<String>,
    #[cfg(feature = "supervised-s0")]
    hold_check: Option<HoldCheck>,
    #[cfg(feature = "supervised-s0")]
    command_observer: Option<CommandObserver>,
    #[cfg(feature = "supervised-s0")]
    task_events: Option<tonic::Streaming<robot_rpc::proto::TaskEvent>>,
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
            last_submission_id: None,
            #[cfg(feature = "supervised-s0")]
            hold_check: None,
            #[cfg(feature = "supervised-s0")]
            command_observer: None,
            #[cfg(feature = "supervised-s0")]
            task_events: None,
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

    async fn submit_action(&mut self, spec: ActionSpec, timeout_ms: u64) -> Result<String, String>;
    #[cfg(feature = "supervised-s0")]
    fn known_submission_id(&self) -> Option<&str>;
    #[cfg(feature = "supervised-s0")]
    async fn checked_state(&mut self) -> Result<RobotState, String>;
    #[cfg(feature = "supervised-s0")]
    fn now_ns(&self) -> u64;
    #[cfg(feature = "supervised-s0")]
    fn published_commands(&mut self) -> Result<Vec<PublishedCommand>, String>;
    #[cfg(feature = "supervised-s0")]
    async fn begin_cancel_watch(&mut self, action_id: &str, timeout_ms: u64) -> Result<(), String>;
    #[cfg(feature = "supervised-s0")]
    async fn next_task_event(&mut self) -> Result<robot_rpc::proto::TaskEvent, String>;
    #[cfg(feature = "supervised-s0")]
    fn record_cancel_evidence(&mut self, line: String) {
        println!("{line}");
    }
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

    async fn submit_action(&mut self, spec: ActionSpec, timeout_ms: u64) -> Result<String, String> {
        let single_joint =
            spec.payload_schema_id == robot_msg::schema_id::SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD;
        let mut meta = self.meta("single-probe");
        meta.timeout_ms = timeout_ms;
        self.last_submission_id = Some(meta.request_id.clone());
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
        if single_joint && self.last_submission_id.as_deref() != Some(handle.action_id.as_str()) {
            return Err("InvokeTask 回执 action_id 与本次 request_id 不匹配".into());
        }
        Ok(handle.action_id)
    }

    #[cfg(feature = "supervised-s0")]
    fn known_submission_id(&self) -> Option<&str> {
        self.last_submission_id.as_deref()
    }

    #[cfg(feature = "supervised-s0")]
    async fn checked_state(&mut self) -> Result<RobotState, String> {
        self.get_state_raw().await
    }

    #[cfg(feature = "supervised-s0")]
    fn now_ns(&self) -> u64 {
        SystemClock::new().now().0
    }

    #[cfg(feature = "supervised-s0")]
    fn published_commands(&mut self) -> Result<Vec<PublishedCommand>, String> {
        self.command_observer
            .as_ref()
            .ok_or("命令镜像尚未建立")?
            .drain()
    }

    #[cfg(feature = "supervised-s0")]
    async fn begin_cancel_watch(&mut self, action_id: &str, timeout_ms: u64) -> Result<(), String> {
        let mut request = tonic::Request::new(TaskHandleRef {
            request_id: uuidv7_compat(),
            action_id: action_id.into(),
        });
        // 握手仍受本地4秒限制，已建立的流覆盖完整任务，不沿普通RPC的短期限。
        request.set_timeout(Duration::from_millis(timeout_ms));
        self.task_events = Some(
            bounded(
                "cancel event stream".into(),
                self.client.stream_task_events(request),
            )
            .await?,
        );
        Ok(())
    }

    #[cfg(feature = "supervised-s0")]
    async fn next_task_event(&mut self) -> Result<robot_rpc::proto::TaskEvent, String> {
        self.task_events
            .as_mut()
            .ok_or("取消事件流未建立")?
            .message()
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "取消事件流提前结束".into())
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
        .submit_action(ActionSpec::scripted(payload), RPC_TIMEOUT_MS)
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

/// 单向一次定位；任何上扭后的返回都交给同一个 finalizer，不重试或回程。
#[cfg(feature = "supervised-s0")]
#[allow(
    clippy::too_many_lines,
    reason = "保持单次启用、提交与有界观察的顺序可审阅"
)]
async fn single_joint_torque_body<O: TorqueOps>(
    ops: &mut O,
    plan: &SingleJointPlan,
    check: &mut SingleJointCheck,
    budgets: StageBudgets,
) -> Result<(), BodyFailure> {
    let mut evidence = CommandEvidence::default();
    if plan.mixed() && plan.cancel_after_ms.is_none() {
        evidence
            .observe(
                ops.published_commands().map_err(BodyFailure::plain)?,
                plan,
                check,
                ops.now_ns(),
            )
            .map_err(BodyFailure::plain)?;
    }
    staged(budgets.enable, "single-joint enable stage", ops.enable())
        .await
        .map_err(BodyFailure::plain)?;
    // 启用后再取完整健康帧；动作前依然检查原幅度与速率硬上限。
    let state = ops.checked_state().await.map_err(BodyFailure::plain)?;
    check
        .observe(&state, &plan.hardware_id, ops.now_ns(), false)
        .map_err(BodyFailure::plain)?;
    plan.check_start(&state.position_rad)
        .map_err(BodyFailure::plain)?;

    let task_started_ns = ops.now_ns();
    let result = staged(
        budgets.submit,
        "single-joint submit stage",
        ops.submit_action(plan.action_spec(), plan.task_timeout_ms),
    )
    .await;
    let action_id = match result {
        Ok(id) => id,
        Err(message) => {
            return Err(BodyFailure {
                message,
                // request_id 就是 S1 的 action_id；未知回执也不得重发原请求。
                cancel_target: ops.known_submission_id().map(str::to_owned),
            });
        }
    };
    println!("[single-joint] submitted action_id={action_id}");
    if plan.cancel_after_ms.is_some() {
        return single_joint_cancel_task(ops, plan, check, &action_id, task_started_ns, budgets)
            .await;
    }
    let mut terminal_seen = false;
    let mut samples = 0usize;
    let poll = async {
        loop {
            tokio::time::sleep(STATUS_POLL).await;
            let state = ops
                .checked_state()
                .await
                .map_err(|message| BodyFailure::with_cancel(message, action_id.clone()))?;
            check
                .observe(&state, &plan.hardware_id, ops.now_ns(), false)
                .map_err(|message| BodyFailure::with_cancel(message, action_id.clone()))?;
            if plan.mixed() {
                evidence
                    .observe(
                        ops.published_commands().map_err(|message| {
                            BodyFailure::with_cancel(message, action_id.clone())
                        })?,
                        plan,
                        check,
                        ops.now_ns(),
                    )
                    .map_err(|message| BodyFailure::with_cancel(message, action_id.clone()))?;
            }
            let status = ops
                .poll_status(&action_id)
                .await
                .map_err(|message| BodyFailure::with_cancel(message, action_id.clone()))?;
            samples += 1;
            if let TaskPhase::Terminal(terminal) = status.phase {
                terminal_seen = true;
                if terminal != TerminalState::Completed || status.outcome != OutcomeKind::Success {
                    return Err(BodyFailure::plain(format!(
                        "single-joint 任务未成功：terminal={terminal:?}, outcome={:?}, failure={:?}",
                        status.outcome, status.failure_reason
                    )));
                }
                return verify_single_joint_success(
                    ops,
                    plan,
                    check,
                    &mut evidence,
                    &action_id,
                    task_started_ns,
                    samples + 1,
                )
                .await
                .map_err(BodyFailure::plain);
            }
        }
    };
    let exec_budget = Duration::from_millis(plan.task_timeout_ms);
    match tokio::time::timeout(exec_budget, poll).await {
        Ok(result) => result,
        Err(_) => Err(BodyFailure {
            message: "single-joint 执行或最终读回超过任务观察预算".into(),
            cancel_target: (!terminal_seen).then_some(action_id),
        }),
    }
}

/// 成功回执之后核对新源帧及覆盖该帧的轮零速命令；沿调用方原任务预算收尾。
#[cfg(feature = "supervised-s0")]
async fn verify_single_joint_success<O: TorqueOps>(
    ops: &mut O,
    plan: &SingleJointPlan,
    check: &mut SingleJointCheck,
    evidence: &mut CommandEvidence,
    action_id: &str,
    started_ns: u64,
    samples: usize,
) -> Result<(), String> {
    let success_received_ns = ops.now_ns();
    if success_received_ns.saturating_sub(started_ns) < u64::from(plan.duration_ms) * 1_000_000 {
        return Err("single-joint 成功回执早于显式运动时长".into());
    }
    tokio::time::sleep(STATUS_POLL).await;
    let state = ops.checked_state().await?;
    if state.stamp_ns <= success_received_ns {
        return Err("single-joint 最终反馈并非成功回执后的新源帧".into());
    }
    check.observe(&state, &plan.hardware_id, ops.now_ns(), true)?;
    if plan.mixed() {
        // 末帧位置验收必须有覆盖同一源时刻的连续轮零速命令证据。
        loop {
            evidence.observe(ops.published_commands()?, plan, check, ops.now_ns())?;
            if evidence
                .frames
                .back()
                .is_some_and(|frame| frame.stamp_ns >= state.stamp_ns)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        if ops.now_ns().saturating_sub(state.stamp_ns) > HOLD_FRESH_NS {
            return Err("轮零速命令覆盖终帧时反馈已过期".into());
        }
    }
    println!(
        "SINGLE_JOINT_FINAL_RAW={}",
        serde_json::json!(check.source.logged_raw_positions(&state.position_rad)?)
    );
    println!(
        "SINGLE_JOINT_TASK action_id={action_id} elapsed_ns={} samples={samples}",
        ops.now_ns().saturating_sub(started_ns)
    );
    Ok(())
}

/// 主动取消一旦尝试，未知结果直接交给 EStop；外层超时也不能再发第二次取消。
#[cfg(feature = "supervised-s0")]
#[allow(clippy::too_many_lines)]
async fn single_joint_cancel_task<O: TorqueOps>(
    ops: &mut O,
    plan: &SingleJointPlan,
    check: &mut SingleJointCheck,
    action_id: &str,
    started_ns: u64,
    budgets: StageBudgets,
) -> Result<(), BodyFailure> {
    let mut cancel_attempted = false;
    let mut terminal_seen = false;
    let run = async {
        let mut evidence = CommandEvidence::default();
        evidence.observe(ops.published_commands()?, plan, check, ops.now_ns())?;
        staged(
            budgets.submit,
            "cancel event stream handshake",
            ops.begin_cancel_watch(action_id, plan.task_timeout_ms),
        )
        .await?;
        let cancel_after_ns = u64::from(plan.cancel_after_ms.ok_or("取消时间缺失")?) * 1_000_000;
        loop {
            tokio::time::sleep(STATUS_POLL).await;
            let state = ops.checked_state().await?;
            check.observe(&state, &plan.hardware_id, ops.now_ns(), false)?;
            evidence.observe(ops.published_commands()?, plan, check, ops.now_ns())?;
            let status = ops.poll_status(action_id).await?;
            if matches!(status.phase, TaskPhase::Terminal(_)) {
                terminal_seen = true;
                return Err("主动取消前任务已经终止".into());
            }
            if status.phase != TaskPhase::Executing {
                return Err("主动取消任务未处于 Executing".into());
            }
            let elapsed = ops.now_ns().saturating_sub(started_ns);
            if elapsed >= u64::from(plan.duration_ms) * 1_000_000 {
                return Err("主动取消已错过显式运动时长".into());
            }
            if elapsed < cancel_after_ns {
                continue;
            }
            let requested_ns = ops.now_ns();
            cancel_attempted = true;
            println!(
                "SINGLE_JOINT_CANCEL_REQUEST action_id={action_id} started_at_ns={started_ns} requested_at_ns={requested_ns} cancel_after_ms={} duration_ms={} task_timeout_ms={}",
                cancel_after_ns / 1_000_000,
                plan.duration_ms,
                plan.task_timeout_ms
            );
            staged(budgets.submit, "active CancelTask", ops.cancel(action_id)).await?;
            let at_ns = staged(budgets.submit, "Cancelled event", async {
                loop {
                    let event = ops.next_task_event().await?;
                    if event.action_id != action_id {
                        return Err("取消事件 action_id 不匹配".into());
                    }
                    match robot_rpc::proto::TaskEventKind::try_from(event.event) {
                        Ok(robot_rpc::proto::TaskEventKind::Cancelled) => {
                            terminal_seen = true;
                            if event.at_ns <= requested_ns
                                || event.at_ns > ops.now_ns()
                                || event.at_ns.saturating_sub(started_ns)
                                    >= u64::from(plan.duration_ms) * 1_000_000
                            {
                                return Err("Cancelled 源时间不在本次中途取消窗口".into());
                            }
                            ops.record_cancel_evidence(format!(
                                "SINGLE_JOINT_CANCEL_EVENT action_id={action_id} terminal_at_ns={} received_at_ns={}",
                                event.at_ns, ops.now_ns()
                            ));
                            return Ok(event.at_ns);
                        }
                        Ok(
                            robot_rpc::proto::TaskEventKind::Completed
                            | robot_rpc::proto::TaskEventKind::Failed,
                        ) => {
                            terminal_seen = true;
                            return Err("主动取消收到非 Cancelled 终态".into());
                        }
                        Ok(
                            robot_rpc::proto::TaskEventKind::Started
                            | robot_rpc::proto::TaskEventKind::Progress,
                        ) => {}
                        _ => return Err("取消事件种类未知".into()),
                    }
                }
            })
            .await?;
            if ops.poll_status(action_id).await?.phase
                != TaskPhase::Terminal(TerminalState::Cancelled)
            {
                return Err("Cancelled 事件与任务终态不一致".into());
            }
            return verify_cancel_hold(
                ops,
                plan,
                check,
                &mut evidence,
                action_id,
                requested_ns,
                at_ns,
            )
            .await;
        }
    };
    let result = tokio::time::timeout(Duration::from_millis(plan.task_timeout_ms), run)
        .await
        .unwrap_or_else(|_| Err("主动取消流程超过任务观察预算".into()));
    result.map_err(|message| BodyFailure {
        message,
        cancel_target: (!cancel_attempted && !terminal_seen).then(|| action_id.to_owned()),
    })
}

/// 只记录现有反馈，不发起额外读取；尚无可信保持参考时误差显式为空。
#[cfg(feature = "supervised-s0")]
fn record_cancel_sample<O: TorqueOps>(
    ops: &mut O,
    state: &RobotState,
    check: &HoldCheck,
    reference: Option<&PublishedCommand>,
    sample: usize,
) {
    let actual_raw = check.logged_raw_positions(&state.position_rad).ok();
    let expected_raw = reference.and_then(|frame| check.logged_raw_positions(&frame.target).ok());
    let error_raw = actual_raw
        .as_ref()
        .zip(expected_raw.as_ref())
        .filter(|(actual, expected)| actual.len() == expected.len())
        .map(|(actual, expected)| {
            actual
                .iter()
                .zip(expected)
                .map(|(&actual, &expected)| {
                    actual
                        .zip(expected)
                        .map(|(actual, expected)| i64::from(actual) - i64::from(expected))
                })
                .collect::<Vec<_>>()
        });
    ops.record_cancel_evidence(format!(
        "SINGLE_JOINT_CANCEL_SAMPLE={}",
        serde_json::json!({
            "sample": sample, "stamp_ns": state.stamp_ns, "received_at_ns": ops.now_ns(),
            "actual_raw": actual_raw, "error_raw": error_raw,
        })
    ));
}

/// 事件时刻切开完整命令流：末次动作目标与后续保持目标必须逐 f32 位一致。
#[cfg(feature = "supervised-s0")]
#[allow(clippy::too_many_arguments)]
async fn verify_cancel_hold<O: TorqueOps>(
    ops: &mut O,
    plan: &SingleJointPlan,
    check: &mut SingleJointCheck,
    evidence: &mut CommandEvidence,
    action_id: &str,
    requested_ns: u64,
    at_ns: u64,
) -> Result<(), String> {
    let deadline_ns = at_ns
        .checked_add(u64::from(plan.settle_ms) * 1_000_000)
        .ok_or("取消到位期限溢出")?;
    let remaining = deadline_ns
        .checked_sub(ops.now_ns())
        .filter(|value| *value > 0)
        .ok_or("Cancelled 事件或回执已超过原到位预算")?;
    let mut stable_since = None;
    let mut samples = 0usize;
    let mut sample_logs = 0usize;
    let mut logged_reference = None;
    staged(Duration::from_nanos(remaining), "cancel hold settle", async {
        loop {
            tokio::time::sleep(STATUS_POLL).await;
            let state = ops.checked_state().await?;
            let reference_result = ops.published_commands()
                .and_then(|frames| evidence.observe(frames, plan, check, ops.now_ns()))
                .and_then(|()| evidence.cancelled_target(at_ns).map(Option::<&PublishedCommand>::cloned));
            if logged_reference.is_none() && let Ok(Some(reference)) = &reference_result {
                let raw = check.source.raw_positions_by_slot(&reference.target)?;
                ops.record_cancel_evidence(format!("SINGLE_JOINT_CANCEL_HOLD_RAW={raw:?}"));
                ops.record_cancel_evidence(format!("SINGLE_JOINT_CANCEL_REFERENCE={}", serde_json::json!({
                    "action_id": action_id, "terminal_at_ns": at_ns, "stamp_ns": reference.stamp_ns,
                    "last_command_seq": reference.seq, "observer_seq": reference.metadata.observer_seq,
                    "source_id": reference.source_id, "layout_id": reference.layout_id,
                    "publisher_instance_id": format!("{:032x}", u128::from_le_bytes(reference.metadata.publisher_instance_id)),
                    "target_rad": reference.target,
                })));
                logged_reference = Some(reference.clone());
            }
            if sample_logs < 8 {
                sample_logs += 1;
                record_cancel_sample(ops, &state, &check.source, logged_reference.as_ref(), sample_logs);
            }
            let reference = reference_result?;
            if ops.now_ns() >= deadline_ns || state.stamp_ns <= at_ns {
                return Err("取消保持反馈超时或不是终态后的新源帧".into());
            }
            check.observe(&state, &plan.hardware_id, ops.now_ns(), false)?;
            let Some(reference) = reference else {
                continue;
            };
            let expected_raw = check.source.raw_positions_by_slot(&reference.target)?;
            let start = check.source.anchor_raw[plan.joint_idx];
            let end = plan.expected_raw[plan.joint_idx];
            if expected_raw[plan.joint_idx] <= start.min(end)
                || expected_raw[plan.joint_idx] >= start.max(end)
            {
                return Err("取消保持目标不是已发布的途中目标".into());
            }
            let actual_raw = check.source.logged_raw_positions(&state.position_rad)?;
            samples += 1;
            if !check.within_tolerances(&state.position_rad, &reference.target) {
                stable_since = None;
                continue;
            }
            let since = *stable_since.get_or_insert(state.stamp_ns);
            let stable_end = since.checked_add(u64::from(plan.stable_ms) * 1_000_000).ok_or("取消稳定窗口溢出")?;
            if state.stamp_ns < stable_end {
                continue;
            }
            // 固定已稳定的实测终帧，等待镜像覆盖它；不追逐新的反馈延长窗口。
            while evidence.frames.back().is_none_or(|frame| frame.stamp_ns < state.stamp_ns) {
                tokio::time::sleep(Duration::from_millis(2)).await;
                evidence.observe(ops.published_commands()?, plan, check, ops.now_ns())?;
                evidence.cancelled_target(at_ns)?;
            }
            if ops.now_ns() >= deadline_ns || ops.now_ns().saturating_sub(state.stamp_ns) > 200_000_000 {
                return Err("镜像覆盖终帧时已超过到位预算或终帧不新鲜".into());
            }
            println!("SINGLE_JOINT_FINAL_RAW={}", serde_json::json!(actual_raw));
            println!(
                "SINGLE_JOINT_CANCEL action_id={action_id} requested_at_ns={requested_ns} terminal_at_ns={at_ns} last_command_seq={} observer_seq={} source_id={} publisher_instance_id={:032x} samples={samples} stable_ns={}",
                reference.seq, reference.metadata.observer_seq, reference.source_id,
                u128::from_le_bytes(reference.metadata.publisher_instance_id), state.stamp_ns - since
            );
            return Ok(());
        }
    }).await
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
async fn read_fixed_anchor(
    args: &Args,
    layout: &HardwareLayout,
) -> Result<(toml::Value, Vec<f64>, HoldCheck), String> {
    let config_path = match args.side {
        Side::Left => args.left_config.clone(),
        Side::Right => args.right_config.clone(),
    };
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
    let check = if args.phase == Phase::SingleJoint {
        let slots = SingleJointPlan::position_slots(args, layout, &config)?;
        HoldCheck::for_single_joint(robot_id.into(), &anchor, slots)?
    } else {
        HoldCheck::new(robot_id.into(), &anchor, layout.joints.len())?
    };
    Ok((config, anchor, check))
}

#[cfg(feature = "supervised-s0")]
async fn phase_hold(rt: &mut SideRt, args: &Args) -> Result<(), String> {
    let (_, anchor, check) = read_fixed_anchor(args, &rt.layout).await?;
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

#[cfg(feature = "supervised-s0")]
async fn phase_single_joint(rt: &mut SideRt, args: &Args) -> Result<(), String> {
    let (config, anchor, check) = read_fixed_anchor(args, &rt.layout).await?;
    let plan = SingleJointPlan::prepare(args, &rt.layout, &anchor, &config)?;
    println!("SINGLE_JOINT_ANCHOR_RAW={:?}", check.anchor_raw);
    println!("SINGLE_JOINT_EXPECTED_RAW={:?}", plan.expected_raw);
    println!("SINGLE_JOINT_POSITION_SLOTS={:?}", plan.position_slots);
    println!(
        "[single-joint] hardware={} joint={} target_position_rad={:.17} duration_ms={} task_timeout_ms={} max_offset_rad={:.17}",
        plan.hardware_id,
        plan.joint_name,
        plan.target_position_rad,
        plan.duration_ms,
        plan.task_timeout_ms,
        plan.max_offset_rad
    );
    rt.hold_check = Some(check);
    let before = rt.get_state_raw().await?;
    check_hold_pre_enable(rt, &before)?;
    plan.check_start(&before.position_rad)?;
    tokio::time::sleep(STATUS_POLL).await;
    let current = rt.get_state_raw().await?;
    check_hold_pre_enable(rt, &current)?;
    plan.check_start(&current.position_rad)?;
    if current.stamp_ns <= before.stamp_ns {
        return Err("single-joint 启用前两帧来源没有推进".into());
    }
    let mut check = SingleJointCheck {
        source: rt
            .hold_check
            .as_ref()
            .ok_or("单关节固定锚点检查器缺失")?
            .clone(),
        layout_id: rt.layout.layout_id,
        joint_idx: plan.joint_idx,
        expected_raw: plan.expected_raw.clone(),
        expected_positions: plan.expected_positions.clone(),
        tolerances_rad: plan
            .limits
            .iter()
            .map(|limit| limit.tolerance_rad)
            .collect(),
    };
    if plan.cancel_after_ms.is_some() || plan.mixed() {
        rt.command_observer =
            Some(CommandObserver::open(&check.source.robot_id, &plan.hardware_id).await?);
        tokio::time::sleep(STATUS_POLL).await;
        // 首帧证据在 enable 之前验证；保留队列，启用等待期间仍逐帧记录。
        let frames = rt
            .command_observer
            .as_ref()
            .ok_or("命令镜像未建立")?
            .peek()?;
        let initial_raw = check
            .source
            .raw_positions_by_slot(&frames.last().ok_or("启用前缺少命令镜像")?.target)?;
        if initial_raw.len() != check.source.anchor_raw.len()
            || initial_raw
                .iter()
                .zip(&check.source.anchor_raw)
                .any(|(target, anchor)| (target - anchor).abs() > 4)
        {
            return Err("启用前发布目标不在本次启动锚点±4raw内".into());
        }
        CommandEvidence::default().observe(frames, &plan, &check, rt.now_ns())?;
    }
    let body = single_joint_torque_body(rt, &plan, &mut check, PROD_STAGE_BUDGETS).await;
    let result = finalize_single(rt, body, PROD_STAGE_BUDGETS).await;
    rt.task_events = None;
    rt.command_observer = None;
    result?;
    if plan.cancel_after_ms.is_some() {
        println!("SINGLE_JOINT_CANCEL_PASS");
    } else {
        println!("SINGLE_JOINT_PASS");
    }
    Ok(())
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
        Phase::SingleJoint => format!(
            "single-joint/{}",
            if side == Side::Left { "left" } else { "right" }
        ),
        Phase::SingleArm => format!(
            "single-arm/{}",
            if side == Side::Left { "left" } else { "right" }
        ),
        Phase::DualArm => "dual-arm".to_string(),
        Phase::Grippers => "grippers".to_string(),
        Phase::All => unreachable!("all 在 main 展开"),
    };
    let result = match phase {
        Phase::Hold | Phase::SingleJoint => {
            #[cfg(feature = "supervised-s0")]
            {
                let step = async {
                    let mut rt = connect_side(args, side).await?;
                    if phase == Phase::SingleJoint {
                        phase_single_joint(&mut rt, args).await
                    } else {
                        phase_hold(&mut rt, args).await
                    }
                };
                step.await
            }
            #[cfg(not(feature = "supervised-s0"))]
            {
                Err("hold/single-joint 需要 supervised-s0 feature".into())
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
            Phase::SingleJoint,
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
        #[cfg(feature = "supervised-s0")]
        Late(T, Duration, Arc<AtomicBool>),
    }

    impl<T: Send + 'static> Scripted<T> {
        async fn resolve(self) -> Result<T, String> {
            match self {
                Self::Ok(v) => Ok(v),
                Self::Err(e) => Err(e),
                Self::Hang => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Err("unreachable: hang resolved".into())
                }
                #[cfg(feature = "supervised-s0")]
                Self::Late(value, delay, completed) => {
                    let (sender, receiver) = tokio::sync::oneshot::channel();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        completed.store(true, Ordering::SeqCst);
                        let _ = sender.send(value);
                    });
                    receiver.await.map_err(|error| error.to_string())
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
        submitted: Vec<(ActionSpec, u64)>,
        #[cfg(feature = "supervised-s0")]
        raw_state_r: VecDeque<Scripted<RobotState>>,
        #[cfg(feature = "supervised-s0")]
        raw_state_now: u64,
        #[cfg(feature = "supervised-s0")]
        status_now: Option<u64>,
        #[cfg(feature = "supervised-s0")]
        command_r: VecDeque<Result<Vec<PublishedCommand>, String>>,
        #[cfg(feature = "supervised-s0")]
        watch_r: VecDeque<Scripted<()>>,
        #[cfg(feature = "supervised-s0")]
        event_r: VecDeque<Scripted<robot_rpc::proto::TaskEvent>>,
        #[cfg(feature = "supervised-s0")]
        event_received_ns: Option<u64>,
        #[cfg(feature = "supervised-s0")]
        cancel_logs: Vec<String>,
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
                submitted: Vec::new(),
                #[cfg(feature = "supervised-s0")]
                raw_state_r: VecDeque::new(),
                #[cfg(feature = "supervised-s0")]
                raw_state_now: 0,
                #[cfg(feature = "supervised-s0")]
                status_now: None,
                #[cfg(feature = "supervised-s0")]
                command_r: VecDeque::new(),
                #[cfg(feature = "supervised-s0")]
                watch_r: VecDeque::new(),
                #[cfg(feature = "supervised-s0")]
                event_r: VecDeque::new(),
                #[cfg(feature = "supervised-s0")]
                event_received_ns: None,
                #[cfg(feature = "supervised-s0")]
                cancel_logs: Vec::new(),
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

        async fn submit_action(
            &mut self,
            spec: ActionSpec,
            timeout_ms: u64,
        ) -> Result<String, String> {
            tokio::task::yield_now().await;
            self.calls.push("submit".into());
            self.submitted.push((spec, timeout_ms));
            self.submit_r
                .pop_front()
                .unwrap_or_else(|| Scripted::Ok("action-1".into()))
                .resolve()
                .await
        }

        #[cfg(feature = "supervised-s0")]
        fn known_submission_id(&self) -> Option<&str> {
            (!self.submitted.is_empty()).then_some("action-1")
        }

        #[cfg(feature = "supervised-s0")]
        async fn checked_state(&mut self) -> Result<RobotState, String> {
            self.calls.push("state".into());
            let state = self
                .raw_state_r
                .pop_front()
                .unwrap_or_else(|| Scripted::Err("raw state queue exhausted".into()))
                .resolve()
                .await?;
            self.raw_state_now = self.raw_state_now.max(state.stamp_ns);
            Ok(state)
        }

        #[cfg(feature = "supervised-s0")]
        fn now_ns(&self) -> u64 {
            self.raw_state_now
        }

        #[cfg(feature = "supervised-s0")]
        fn published_commands(&mut self) -> Result<Vec<PublishedCommand>, String> {
            self.calls.push("commands".into());
            self.command_r
                .pop_front()
                .unwrap_or_else(|| Err("command queue exhausted".into()))
        }

        #[cfg(feature = "supervised-s0")]
        async fn begin_cancel_watch(&mut self, _id: &str, timeout_ms: u64) -> Result<(), String> {
            self.calls.push(format!("watch:{timeout_ms}"));
            Self::pop(&mut self.watch_r).resolve().await
        }

        #[cfg(feature = "supervised-s0")]
        async fn next_task_event(&mut self) -> Result<robot_rpc::proto::TaskEvent, String> {
            self.calls.push("event".into());
            let event = self
                .event_r
                .pop_front()
                .unwrap_or_else(|| Scripted::Err("event queue exhausted".into()))
                .resolve()
                .await?;
            self.raw_state_now = self
                .event_received_ns
                .unwrap_or_else(|| self.raw_state_now.max(event.at_ns));
            Ok(event)
        }

        #[cfg(feature = "supervised-s0")]
        fn record_cancel_evidence(&mut self, line: String) {
            self.cancel_logs.push(line);
        }

        async fn poll_status(&mut self, _id: &str) -> Result<TaskSnapshot, String> {
            tokio::task::yield_now().await;
            self.calls.push("status".into());
            #[cfg(feature = "supervised-s0")]
            if let Some(now) = self.status_now {
                self.raw_state_now = now;
            }
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
            #[cfg(feature = "supervised-s0")]
            submit: Duration::from_millis(500),
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
    #[cfg(feature = "supervised-s0")]
    mod single_joint_tests {
        use super::*;

        fn raw_args() -> Vec<String> {
            [
                "--phase",
                "single-joint",
                "--side",
                "left",
                "--token",
                "test-only",
                "--joint-name",
                "j4",
                "--target-offset-raw",
                "24",
                "--duration-ms",
                "20000",
                "--task-timeout-ms",
                "25000",
                "--hold-anchor-rad",
                "[0,0,0,0,0,0,0,0]",
                "--ack-safety-checklist",
            ]
            .map(str::to_string)
            .to_vec()
        }

        fn layout() -> HardwareLayout {
            HardwareLayout {
                joints: (0..8).map(|i| format!("j{i}")).collect(),
                motor_ids: (1..=8).collect(),
                layout_id: 42,
            }
        }

        fn config() -> toml::Value {
            use std::fmt::Write as _;

            let mut text = "[s1.profile]\nbackends=['scripted']\ncommand_mode='joint_position'\n[s1.single_joint]\nmax_duration_ms=20000\nsettle_timeout_ms=800\nstable_window_ms=200\n".to_string();
            for i in 0..8 {
                writeln!(text, "[[s1.single_joint.joints]]\nname='j{i}'\nmin_position_rad=-1.0\nmax_position_rad=1.0\nmax_velocity_rad_s=0.1\ntolerance_rad=0.001")
                    .expect("写入测试配置字符串");
            }
            toml::from_str(&text).unwrap()
        }

        fn plan() -> SingleJointPlan {
            SingleJointPlan::prepare(
                &Args::parse_from(&raw_args()).unwrap(),
                &layout(),
                &[0.0; 8],
                &config(),
            )
            .unwrap()
        }

        fn check() -> SingleJointCheck {
            check_for(&plan())
        }

        fn check_for(plan: &SingleJointPlan) -> SingleJointCheck {
            let mut source = HoldCheck::for_single_joint(
                "robot".into(),
                &vec![0.0; plan.expected_raw.len()],
                plan.position_slots.clone(),
            )
            .unwrap();
            source.last_stamp_ns = 1_000_000_000;
            SingleJointCheck {
                source,
                layout_id: 42,
                joint_idx: plan.joint_idx,
                expected_raw: plan.expected_raw.clone(),
                expected_positions: plan.expected_positions.clone(),
                tolerances_rad: plan
                    .limits
                    .iter()
                    .map(|limit| limit.tolerance_rad)
                    .collect(),
            }
        }

        fn state(step: u64, selected_offset: i32) -> RobotState {
            let mut positions = vec![0.0; 8];
            positions[4] = f64::from(robot_adapter_feetech::raw_to_rad(2047 + selected_offset));
            RobotState {
                robot_id: "robot".into(),
                hardware_id: "left_arm".into(),
                layout_id: 42,
                stamp_ns: 1_000_000_000 + step * 100_000_000,
                position_rad: positions,
                mode: S0Mode::Enabled as i32,
                control_link: robot_rpc::proto::S0ControlLink::Ok as i32,
                output_mode: robot_rpc::proto::S0OutputMode::Active as i32,
                safety_state: robot_rpc::proto::S0SafetyState::TorqueOn as i32,
                ..RobotState::default()
            }
        }

        fn ops() -> ScriptedOps {
            let mut ops = ScriptedOps::new("left");
            ops.raw_state_r.extend([
                Scripted::Ok(state(1, 0)),
                Scripted::Ok(state(201, 24)),
                Scripted::Ok(state(202, 24)),
            ]);
            ops.status_r.push_back(Scripted::Ok(success_snapshot()));
            ops
        }

        async fn execute(
            ops: &mut ScriptedOps,
            plan: &SingleJointPlan,
            budgets: StageBudgets,
        ) -> Result<(), String> {
            let body = single_joint_torque_body(ops, plan, &mut check_for(plan), budgets).await;
            finalize_single(ops, body, budgets).await
        }

        fn cancel_plan() -> SingleJointPlan {
            let mut plan = plan();
            plan.cancel_after_ms = Some(10_000);
            plan
        }

        fn configured_limits() -> Vec<SingleJointLimit> {
            let config: toml::Value = toml::from_str(include_str!(
                "../../../../configs/xlerobot-bus1-motion.toml"
            ))
            .unwrap();
            let joints = config["s1"]["single_joint"]["joints"]
                .as_array()
                .unwrap()
                .iter()
                .map(|joint| joint["name"].as_str().unwrap().to_owned())
                .collect();
            motion_config(
                &config,
                &HardwareLayout {
                    joints,
                    motor_ids: Vec::new(),
                    layout_id: 42,
                },
            )
            .unwrap()
            .0
        }

        fn configured_plan(offset: i32) -> Result<SingleJointPlan, String> {
            let mut args = Args::parse_from(&raw_args()).unwrap();
            args.target_offset_raw = Some(offset);
            args.single_joint_max_offset_rad = Some(MAX_SINGLE_JOINT_OFFSET_RAD);
            let mut config = config();
            for (joint, limit) in config["s1"]["single_joint"]["joints"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .zip(configured_limits())
            {
                joint["tolerance_rad"] = toml::Value::Float(limit.tolerance_rad);
            }
            SingleJointPlan::prepare(&args, &layout(), &[0.0; 8], &config)
        }

        fn mixed_fixture() -> (Args, HardwareLayout, toml::Value) {
            let config: toml::Value = toml::from_str(&format!(
                "{}\n{}\n[s1.profile]\nbackends=['scripted']\ncommand_mode='joint_position'\n",
                include_str!("../../../../configs/xlerobot-bus2.toml"),
                include_str!("../../../../configs/xlerobot-bus2-motion.toml"),
            ))
            .unwrap();
            let hardware: robot_s0::config_loader::HardwareSection =
                config["hardware"][0].clone().try_into().unwrap();
            let mut args = Args::parse_from(&raw_args()).unwrap();
            args.side = Side::Right;
            args.right_hardware_id = hardware.id;
            args.joint_name = Some(
                hardware.joints[hardware.motor_ids.iter().position(|&id| id == 5).unwrap()].clone(),
            );
            args.target_offset_raw = Some(341);
            args.single_joint_max_offset_rad = Some(MAX_SINGLE_JOINT_OFFSET_RAD);
            (
                args,
                HardwareLayout {
                    joints: hardware.joints,
                    motor_ids: hardware.motor_ids,
                    layout_id: 42,
                },
                config,
            )
        }

        fn mixed_plan() -> SingleJointPlan {
            let (args, layout, config) = mixed_fixture();
            SingleJointPlan::prepare(&args, &layout, &[0.0; 9], &config).unwrap()
        }

        fn mixed_state(step: u64, offset: i32) -> RobotState {
            let mut state = state(step, offset);
            state.hardware_id = mixed_plan().hardware_id;
            state.position_rad.truncate(6);
            // 原始轮反馈有限，但不能被当作rad转换或位置到位目标。
            state.position_rad.extend([1e100, -1e100, 9000.0]);
            state
        }

        fn mixed_command(seq: u64, stamp_ns: u64, offset: i32) -> PublishedCommand {
            let mut frame = command(seq, stamp_ns, offset);
            frame.mode = robot_msg::msg::ControlMode::MixedPositionRawVelocity;
            frame.target.resize(9, 0.0);
            frame.raw_velocities = vec![0; 9];
            frame
        }

        #[test]
        fn mixed_slots_follow_configuration_and_reject_wheel_selection_or_anchor() {
            let (mut args, mut layout, mut config) = mixed_fixture();
            for key in ["joints", "motor_ids"] {
                config["hardware"][0][key]
                    .as_array_mut()
                    .unwrap()
                    .rotate_left(7);
            }
            layout.joints.rotate_left(7);
            layout.motor_ids.rotate_left(7);
            let plan = SingleJointPlan::prepare(&args, &layout, &[0.0; 9], &config).unwrap();
            assert_eq!(plan.position_slots, (2..8).collect::<Vec<_>>());
            assert_eq!(layout.motor_ids[plan.joint_idx], 5);
            assert_eq!(plan.limits.len(), 6);
            for slot in [0, 1, 8] {
                assert_eq!(plan.expected_raw[slot], 0);
            }
            let spec = plan.action_spec();
            assert_eq!(spec.payload_schema_id, 0x040F);
            let payload =
                flatbuffers::root::<robot_msg::msg::SingleJointPositionPayload<'_>>(&spec.payload)
                    .unwrap();
            assert_eq!(payload.hardware_id(), args.right_hardware_id);
            let mut anchor = [0.0; 9];
            anchor[0] = 0.001;
            assert!(SingleJointPlan::prepare(&args, &layout, &anchor, &config).is_err());
            args.joint_name = Some(layout.joints[0].clone());
            args.target_offset_raw = None;
            args.target_position_rad = Some(0.0);
            assert!(SingleJointPlan::prepare(&args, &layout, &[0.0; 9], &config).is_err());
        }

        #[test]
        fn mixed_feedback_checks_only_position_slots_and_logs_wheels_as_null() {
            let plan = mixed_plan();
            let mut check = check_for(&plan);
            let state = mixed_state(1, 0);
            check.source.check_position(&state).unwrap();
            plan.check_start(&state.position_rad).unwrap();
            check
                .observe(&state, &plan.hardware_id, state.stamp_ns, false)
                .unwrap();
            assert_eq!(
                check
                    .source
                    .logged_raw_positions(&state.position_rad)
                    .unwrap(),
                vec![
                    Some(2047),
                    Some(2047),
                    Some(2047),
                    Some(2047),
                    Some(2047),
                    Some(2047),
                    None,
                    None,
                    None
                ]
            );
            let mut drift = mixed_state(2, 0);
            drift.position_rad[0] = f64::from(robot_adapter_feetech::raw_to_rad(2052));
            assert!(
                check.source.check_position(&drift).is_err(),
                "启用前原±4raw未放宽"
            );
            for offset in [-21, -20, 20, 21] {
                let mut check = check_for(&plan);
                let mut state = mixed_state(2, 341);
                state.position_rad[0] = f64::from(robot_adapter_feetech::raw_to_rad(2047 + offset));
                assert_eq!(
                    check
                        .observe(&state, &plan.hardware_id, state.stamp_ns, true)
                        .is_ok(),
                    offset.abs() == 20
                );
            }
            let mut malformed = mixed_state(2, 341);
            malformed.position_rad[8] = f64::NAN;
            assert!(
                check
                    .observe(&malformed, &plan.hardware_id, malformed.stamp_ns, true)
                    .is_err()
            );
        }

        #[test]
        fn mixed_command_requires_complete_zero_wheel_and_raw_velocity_fields() {
            let plan = mixed_plan();
            let check = check_for(&plan);
            let good = mixed_command(1, 1_100_000_000, 0);
            CommandEvidence::default()
                .observe(vec![good.clone()], &plan, &check, good.stamp_ns)
                .unwrap();
            for case in 0..5 {
                let mut bad = good.clone();
                match case {
                    0 => bad.target[6] = 0.001,
                    1 => bad.raw_velocities[8] = 1,
                    2 => bad.raw_velocities[0] = 1,
                    3 => bad.raw_velocities.truncate(8),
                    _ => bad.mode = robot_msg::msg::ControlMode::Position,
                }
                assert!(
                    CommandEvidence::default()
                        .observe(vec![bad], &plan, &check, good.stamp_ns)
                        .is_err()
                );
            }
        }

        #[test]
        fn mixed_success_requires_final_tolerance_and_zero_command_evidence() {
            for (offset, bad_wheel) in [(20, false), (21, false), (0, true)] {
                run(async {
                    let plan = mixed_plan();
                    let mut ops = ScriptedOps::new("right");
                    ops.raw_state_now = 1_000_000_000;
                    ops.raw_state_r.extend([
                        Scripted::Ok(mixed_state(1, 0)),
                        Scripted::Ok(mixed_state(201, 341)),
                        Scripted::Ok(mixed_state(202, 341 + offset)),
                    ]);
                    ops.status_r.push_back(Scripted::Ok(success_snapshot()));
                    let mut moving = mixed_command(2, 21_100_000_000, 341);
                    if bad_wheel {
                        moving.raw_velocities[7] = 1;
                    }
                    ops.command_r.extend([
                        Ok(vec![mixed_command(1, 1_000_000_000, 0)]),
                        Ok(vec![moving]),
                        Ok(vec![mixed_command(3, 21_200_000_000, 341)]),
                    ]);
                    assert_eq!(
                        execute(&mut ops, &plan, tb()).await.is_ok(),
                        offset == 20 && !bad_wheel
                    );
                    assert_eq!(
                        ops.calls.last().map(String::as_str),
                        Some(if offset == 20 && !bad_wheel {
                            "disable"
                        } else {
                            "estop"
                        })
                    );
                    assert_eq!(ops.submitted.len(), 1);
                });
            }
            run(async {
                let mut ops = ScriptedOps::new("right");
                assert!(execute(&mut ops, &mixed_plan(), tb()).await.is_err());
                assert!(
                    !ops.calls
                        .iter()
                        .any(|call| call == "enable" || call == "submit")
                );
            });
        }

        #[test]
        fn mixed_cancel_freezes_zero_wheels_and_uses_position_tolerance() {
            run(async {
                let mut plan = mixed_plan();
                plan.cancel_after_ms = Some(10_000);
                let mut ops = cancel_ops();
                ops.raw_state_r = [
                    mixed_state(1, 0),
                    mixed_state(101, 12),
                    mixed_state(102, 33),
                    mixed_state(103, 33),
                    mixed_state(104, 33),
                ]
                .into_iter()
                .map(Scripted::Ok)
                .collect();
                for frames in &mut ops.command_r {
                    for frame in frames.as_mut().unwrap() {
                        frame.mode = robot_msg::msg::ControlMode::MixedPositionRawVelocity;
                        frame.target.resize(9, 0.0);
                        frame.raw_velocities = vec![0; 9];
                    }
                }
                execute(&mut ops, &plan, tb()).await.unwrap();
                assert_eq!(
                    ops.calls
                        .iter()
                        .filter(|call| call.starts_with("cancel:"))
                        .count(),
                    1
                );
                assert_eq!(ops.calls.last().map(String::as_str), Some("disable"));
                let sample = ops
                    .cancel_logs
                    .iter()
                    .find_map(|line| line.strip_prefix("SINGLE_JOINT_CANCEL_SAMPLE="))
                    .unwrap();
                let sample: serde_json::Value = serde_json::from_str(sample).unwrap();
                assert!(sample["actual_raw"][8].is_null());
                assert_eq!(sample["error_raw"][4], 20);
            });
        }

        #[test]
        fn configured_tolerances_accept_twenty_raw_reject_twenty_one_and_keep_each_slot() {
            let limits = configured_limits();
            for limit in &limits {
                let ends = HoldCheck::raw_positions(&[limit.min, limit.max]).unwrap();
                for target_raw in ends[0]..=ends[1] {
                    for offset in [-21, -20, 20, 21] {
                        let actual_raw = target_raw + offset;
                        if !(ends[0]..=ends[1]).contains(&actual_raw) {
                            continue;
                        }
                        assert_eq!(
                            positions_within_tolerances(
                                &[f64::from(robot_adapter_feetech::raw_to_rad(actual_raw))],
                                &[f64::from(robot_adapter_feetech::raw_to_rad(target_raw))],
                                &[limit.tolerance_rad],
                            ),
                            offset.abs() == 20,
                            "target_raw={target_raw}, offset={offset}"
                        );
                    }
                }
            }
            let broad = limits[0].tolerance_rad;
            let one_raw = f64::from(robot_adapter_feetech::raw_to_rad(2048));
            assert!(positions_within_tolerances(
                &[0.0, one_raw],
                &[0.0, 0.0],
                &[0.0, broad]
            ));
            assert!(!positions_within_tolerances(
                &[one_raw, 0.0],
                &[0.0, 0.0],
                &[0.0, broad]
            ));
            assert!(!positions_within_tolerances(
                &[broad.next_up()],
                &[0.0],
                &[broad]
            ));
        }

        #[test]
        fn configured_arrival_intervals_must_not_overlap() {
            for offset in [24, 40, -24, -40] {
                assert!(
                    configured_plan(offset)
                        .err()
                        .unwrap()
                        .contains("容差区间相交")
                );
            }
            assert!(configured_plan(41).is_ok());
            assert!(configured_plan(-41).is_ok());
            assert!(configured_plan(341).is_ok());
            assert_eq!(
                plan().limits[4].tolerance_rad.to_bits(),
                0.001_f64.to_bits()
            );
        }

        #[test]
        fn final_and_cancel_hold_share_configured_arrival_tolerance() {
            for offset in [-21, -20, 20, 21] {
                run(async {
                    let plan = configured_plan(341).unwrap();
                    let mut ops = ops();
                    ops.raw_state_r[1] = Scripted::Ok(state(201, 341));
                    ops.raw_state_r[2] = Scripted::Ok(state(202, 341 + offset));
                    assert_eq!(
                        execute(&mut ops, &plan, tb()).await.is_ok(),
                        offset.abs() == 20
                    );
                });
                run(async {
                    let mut plan = configured_plan(341).unwrap();
                    plan.cancel_after_ms = Some(10000);
                    let mut ops = cancel_ops();
                    ops.raw_state_r[1] = Scripted::Ok(state(101, 170));
                    for (index, step) in [(2, 102), (3, 103), (4, 104)] {
                        ops.raw_state_r[index] = Scripted::Ok(state(step, 171 + offset));
                    }
                    for frames in ops
                        .command_r
                        .iter_mut()
                        .filter_map(|frames| frames.as_mut().ok())
                    {
                        for frame in frames {
                            if frame.seq > 1 {
                                frame.target[4] = f64::from(robot_adapter_feetech::raw_to_rad(
                                    2047 + if frame.seq == 2 { 170 } else { 171 },
                                ));
                            }
                        }
                    }
                    assert_eq!(
                        execute(&mut ops, &plan, tb()).await.is_ok(),
                        offset.abs() == 20
                    );
                    assert_eq!(
                        ops.calls
                            .iter()
                            .filter(|call| call.starts_with("cancel:"))
                            .count(),
                        1
                    );
                });
            }
        }

        #[test]
        fn explicit_single_joint_budget_preserves_default_and_other_phase_caps() {
            assert_eq!(
                plan().max_offset_rad.to_bits(),
                MAX_PROBE_OFFSET_RAD.to_bits()
            );
            let mut raw = raw_args();
            let index = raw
                .iter()
                .position(|arg| arg == "--target-offset-raw")
                .unwrap();
            raw[index + 1] = "341".into();
            let args = Args::parse_from(&raw).unwrap();
            assert!(SingleJointPlan::prepare(&args, &layout(), &[0.0; 8], &config()).is_err());
            raw.extend([
                "--single-joint-max-offset-rad".into(),
                MAX_SINGLE_JOINT_OFFSET_RAD.to_string(),
            ]);
            let args = Args::parse_from(&raw).unwrap();
            let visible = SingleJointPlan::prepare(&args, &layout(), &[0.0; 8], &config()).unwrap();
            assert_eq!(visible.expected_raw[4], 2388);
            assert_eq!(visible.action_spec().payload_schema_id, 0x040F);
            assert!(validate_probe_geometry(MAX_SINGLE_JOINT_OFFSET_RAD, 0.02).is_err());
            for invalid in ["0", "-1", "NaN", "inf", "0.523599"] {
                *raw.last_mut().unwrap() = invalid.into();
                assert!(Args::parse_from(&raw).is_err());
            }
            *raw.last_mut().unwrap() = MAX_SINGLE_JOINT_OFFSET_RAD.to_string();
            raw.extend(["--single-joint-max-offset-rad".into(), "0.1".into()]);
            assert!(Args::parse_from(&raw).is_err());
            for phase in ["read-state", "single-arm", "dual-arm", "grippers", "all"] {
                assert!(
                    Args::parse_from(
                        &[
                            "--phase",
                            phase,
                            "--token",
                            "test",
                            "--single-joint-max-offset-rad",
                            "0.1"
                        ]
                        .map(str::to_owned)
                    )
                    .is_err()
                );
            }
        }

        #[test]
        fn explicit_budget_keeps_request_f32_range_and_rate_checks() {
            let max = MAX_SINGLE_JOINT_OFFSET_RAD;
            assert!(
                check_single_joint_motion(
                    0.0,
                    f64::from(robot_adapter_feetech::raw_to_rad(2388)),
                    20000,
                    max
                )
                .is_ok()
            );
            assert!(check_single_joint_motion(0.0, 0.1, 20000, 0.1).is_err());
            assert!(check_single_joint_motion(0.0, 0.35, 20000, 0.349_999_995).is_err());
            assert!(check_single_joint_motion(0.0, 0.125, 20000, 0.125).is_ok());
            assert!(check_single_joint_motion(0.0, 0.5, 1000, max).is_err());
            let mut args = Args::parse_from(&raw_args()).unwrap();
            args.target_offset_raw = Some(341);
            args.single_joint_max_offset_rad = Some(max);
            let mut narrow = config();
            narrow["s1"]["single_joint"]["joints"][4]["max_position_rad"] = toml::Value::Float(0.5);
            assert!(SingleJointPlan::prepare(&args, &layout(), &[0.0; 8], &narrow).is_err());
            let mut slow = config();
            slow["s1"]["single_joint"]["joints"][4]["max_velocity_rad_s"] =
                toml::Value::Float(0.02);
            assert!(SingleJointPlan::prepare(&args, &layout(), &[0.0; 8], &slow).is_err());
            let visible = SingleJointPlan::prepare(&args, &layout(), &[0.0; 8], &config()).unwrap();
            let mut moved_start = [0.0; 8];
            moved_start[4] = f64::from(robot_adapter_feetech::raw_to_rad(2046));
            assert!(
                visible.check_start(&moved_start).is_err(),
                "不以既有±4raw误差放宽运动幅度预算"
            );
        }

        fn command(seq: u64, stamp_ns: u64, offset: i32) -> PublishedCommand {
            PublishedCommand {
                metadata: robot_transport::iceoryx2::ObserverMetadata {
                    publisher_instance_id: [1; 16],
                    observer_seq: seq,
                    dropped_frames: 0,
                },
                seq,
                stamp_ns,
                received_ns: stamp_ns,
                source_id: 2,
                layout_id: 42,
                source_task_id: seq,
                deadline_ns: stamp_ns + 500_000_000,
                mode: robot_msg::msg::ControlMode::Position,
                on_timeout_hold: true,
                target: state(1, offset).position_rad,
                raw_velocities: Vec::new(),
            }
        }

        fn cancelled_event() -> robot_rpc::proto::TaskEvent {
            robot_rpc::proto::TaskEvent {
                action_id: "action-1".into(),
                event: robot_rpc::proto::TaskEventKind::Cancelled as i32,
                at_ns: 11_110_000_000,
                ..Default::default()
            }
        }

        fn cancel_ops() -> ScriptedOps {
            let mut ops = ScriptedOps::new("left");
            ops.raw_state_r.extend([
                Scripted::Ok(state(1, 0)),
                Scripted::Ok(state(101, 12)),
                Scripted::Ok(state(102, 13)),
                Scripted::Ok(state(103, 13)),
                Scripted::Ok(state(104, 13)),
            ]);
            ops.status_r.extend([
                Scripted::Ok(TaskSnapshot {
                    phase: TaskPhase::Executing,
                    ..success_snapshot()
                }),
                Scripted::Ok(TaskSnapshot {
                    phase: TaskPhase::Terminal(TerminalState::Cancelled),
                    ..success_snapshot()
                }),
            ]);
            ops.event_r.push_back(Scripted::Ok(cancelled_event()));
            ops.command_r.extend([
                Ok(vec![command(1, 1_100_000_000, 0)]),
                Ok(vec![command(2, 11_100_000_000, 12)]),
                // Cancel 发起后、终态前仍可成功发布；参考必须取13raw，不能冻结12raw。
                Ok(vec![
                    command(3, 11_105_000_000, 13),
                    command(4, 11_130_000_000, 13),
                    command(5, 11_200_000_000, 13),
                ]),
                Ok(vec![command(6, 11_300_000_000, 13)]),
                Ok(vec![command(7, 11_400_000_000, 13)]),
            ]);
            ops
        }

        #[test]
        fn cancel_arguments_are_single_joint_only_once_and_strictly_mid_duration() {
            for delay in ["0", "20000", "20001", "-1"] {
                let mut args = raw_args();
                args.extend(["--cancel-after-ms".into(), delay.into()]);
                assert!(Args::parse_from(&args).is_err());
            }
            let mut args = raw_args();
            args.extend(["--cancel-after-ms".into(), "10000".into()]);
            assert_eq!(
                Args::parse_from(&args).unwrap().cancel_after_ms,
                Some(10000)
            );
            args.extend(["--cancel-after-ms".into(), "9000".into()]);
            assert!(Args::parse_from(&args).is_err());
            assert!(
                Args::parse_from(
                    &[
                        "--phase",
                        "read-state",
                        "--token",
                        "test",
                        "--cancel-after-ms",
                        "10000"
                    ]
                    .map(str::to_owned)
                )
                .is_err()
            );
        }

        #[test]
        fn cancel_success_uses_last_publish_after_request_then_one_disable() {
            run(async {
                let mut ops = cancel_ops();
                execute(&mut ops, &cancel_plan(), tb()).await.unwrap();
                assert_eq!(
                    ops.calls
                        .iter()
                        .filter(|call| call.starts_with("cancel:"))
                        .count(),
                    1
                );
                assert_eq!(ops.calls.iter().filter(|call| *call == "enable").count(), 1);
                assert_eq!(ops.submitted.len(), 1);
                assert!(ops.calls.contains(&"watch:25000".into()));
                assert_eq!(ops.calls.last().map(String::as_str), Some("disable"));
                assert!(!ops.calls.contains(&"estop".into()));
                assert_eq!(
                    ops.cancel_logs
                        .iter()
                        .filter(|line| line.starts_with("SINGLE_JOINT_CANCEL_HOLD_RAW="))
                        .count(),
                    1
                );
            });
        }

        #[test]
        fn failed_cancel_hold_keeps_event_reference_and_original_sample_errors() {
            run(async {
                let mut ops = cancel_ops();
                ops.raw_state_r[2] = Scripted::Ok(state(102, 8));
                ops.raw_state_r[3] = Scripted::Ok(state(103, 8));
                ops.raw_state_r[4] = Scripted::Ok(state(110, 8));
                assert!(execute(&mut ops, &cancel_plan(), tb()).await.is_err());
                assert!(ops.cancel_logs[0].starts_with("SINGLE_JOINT_CANCEL_EVENT "));
                assert_eq!(
                    ops.cancel_logs
                        .iter()
                        .filter(|line| line.starts_with("SINGLE_JOINT_CANCEL_HOLD_RAW="))
                        .count(),
                    1
                );
                let reference = ops
                    .cancel_logs
                    .iter()
                    .find_map(|line| line.strip_prefix("SINGLE_JOINT_CANCEL_REFERENCE="))
                    .unwrap();
                let reference: serde_json::Value = serde_json::from_str(reference).unwrap();
                assert_eq!(reference["last_command_seq"], 3);
                assert_eq!(reference["terminal_at_ns"], 11_110_000_000_u64);
                let samples: Vec<serde_json::Value> = ops
                    .cancel_logs
                    .iter()
                    .filter_map(|line| line.strip_prefix("SINGLE_JOINT_CANCEL_SAMPLE="))
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(samples.len(), 3);
                assert!(samples.iter().all(|sample| sample["error_raw"][4] == -5));
                assert_eq!(samples[0]["actual_raw"][4], 2055);
                assert_eq!(samples[2]["stamp_ns"], 12_000_000_000_u64);
                assert_eq!(
                    ops.calls.iter().filter(|call| *call == "state").count(),
                    5,
                    "日志不增加读回"
                );
                assert_eq!(ops.calls.last().map(String::as_str), Some("estop"));
                assert_eq!(
                    ops.calls
                        .iter()
                        .filter(|call| call.starts_with("cancel:"))
                        .count(),
                    1
                );
            });
            run(async {
                let mut ops = cancel_ops();
                ops.status_r[1] = Scripted::Err("status after Cancelled unavailable".into());
                assert!(execute(&mut ops, &cancel_plan(), tb()).await.is_err());
                assert_eq!(ops.cancel_logs.len(), 1);
                assert!(ops.cancel_logs[0].contains("terminal_at_ns=11110000000"));
            });
        }

        #[test]
        fn cancel_ack_unknown_timeout_or_missing_event_never_sends_second_cancel() {
            for failure in 0..4 {
                run(async {
                    let mut ops = cancel_ops();
                    let mut budgets = tb();
                    budgets.submit = Duration::from_millis(15);
                    match failure {
                        0 => ops
                            .cancel_r
                            .push_back(Scripted::Err("unknown cancel reply".into())),
                        1 => ops.cancel_r.push_back(Scripted::Hang),
                        2 => ops.event_r = VecDeque::from([Scripted::Hang]),
                        _ => {
                            ops.event_r =
                                VecDeque::from([Scripted::Err("event stream closed".into())]);
                        }
                    }
                    assert!(execute(&mut ops, &cancel_plan(), budgets).await.is_err());
                    assert_eq!(
                        ops.calls
                            .iter()
                            .filter(|call| call.starts_with("cancel:"))
                            .count(),
                        1
                    );
                    assert_eq!(ops.calls.last().map(String::as_str), Some("estop"));
                    assert!(!ops.calls.contains(&"disable".into()));
                });
            }
        }

        #[test]
        fn cancel_rejects_wrong_terminal_identity_time_and_late_receipt() {
            for failure in 0..6 {
                run(async {
                    let mut ops = cancel_ops();
                    let mut event = cancelled_event();
                    match failure {
                        0 => event.action_id = "other-action".into(),
                        1 => event.event = robot_rpc::proto::TaskEventKind::Completed as i32,
                        2 => event.at_ns = 11_100_000_000,
                        3 => ops.status_r[1] = Scripted::Ok(success_snapshot()),
                        4 => ops.event_received_ns = Some(11_950_000_000),
                        _ => ops.event_received_ns = Some(11_105_000_000),
                    }
                    ops.event_r[0] = Scripted::Ok(event);
                    assert!(execute(&mut ops, &cancel_plan(), tb()).await.is_err());
                    assert_eq!(
                        ops.calls
                            .iter()
                            .filter(|call| call.starts_with("cancel:"))
                            .count(),
                        1
                    );
                    assert_eq!(ops.calls.last().map(String::as_str), Some("estop"));
                });
            }
        }

        #[test]
        fn cancel_rejects_target_reversion_endpoint_or_actual_as_reference() {
            for offset in [0, 12, 24] {
                run(async {
                    let mut ops = cancel_ops();
                    let frames = ops.command_r[2].as_mut().unwrap();
                    frames[1].target = state(1, offset).position_rad;
                    frames[2].target = state(1, offset).position_rad;
                    assert!(execute(&mut ops, &cancel_plan(), tb()).await.is_err());
                    assert_eq!(ops.calls.last().map(String::as_str), Some("estop"));
                });
            }
            for offset in [0, 24] {
                run(async {
                    let mut ops = cancel_ops();
                    for frame in ops.command_r[2].as_mut().unwrap() {
                        frame.target = state(1, offset).position_rad;
                    }
                    assert!(
                        execute(&mut ops, &cancel_plan(), tb())
                            .await
                            .unwrap_err()
                            .contains("途中目标")
                    );
                });
            }
        }

        #[test]
        fn cancel_requires_fresh_continuous_actual_stability_within_original_deadline() {
            for failure in 0..4 {
                run(async {
                    let mut ops = cancel_ops();
                    match failure {
                        0 => ops.raw_state_r[3] = Scripted::Ok(state(102, 13)),
                        1 => ops.raw_state_r[3] = Scripted::Ok(state(110, 13)),
                        2 => ops.raw_state_r[3] = Scripted::Ok(state(103, 18)),
                        _ => {
                            let mut drift = state(103, 13);
                            drift.position_rad[0] =
                                f64::from(robot_adapter_feetech::raw_to_rad(2052));
                            ops.raw_state_r[3] = Scripted::Ok(drift);
                        }
                    }
                    assert!(execute(&mut ops, &cancel_plan(), tb()).await.is_err());
                    assert_eq!(
                        ops.calls
                            .iter()
                            .filter(|call| call.starts_with("cancel:"))
                            .count(),
                        1
                    );
                    assert_eq!(ops.calls.last().map(String::as_str), Some("estop"));
                });
            }
        }

        #[test]
        fn command_evidence_rejects_missing_boundary_gaps_overflow_and_changed_bits() {
            let plan = cancel_plan();
            let check = check();
            for failure in 0..11 {
                let mut evidence = CommandEvidence::default();
                let before = command(1, 11_100_000_000, 12);
                evidence
                    .observe(vec![before.clone()], &plan, &check, before.stamp_ns)
                    .unwrap();
                let mut after = command(2, 11_130_000_000, 12);
                match failure {
                    0 => after.metadata.observer_seq = 3,
                    1 => after.seq = 3,
                    2 => after.metadata.publisher_instance_id = [2; 16],
                    3 => after.metadata.dropped_frames = 1,
                    4 => after.stamp_ns = before.stamp_ns,
                    5 => after.source_id = 1,
                    6 => after.layout_id = 7,
                    7 => after.target[0] = f64::NAN,
                    8 => after.received_ns = after.stamp_ns - 1,
                    9 => after.received_ns = after.stamp_ns + 200_000_001,
                    _ => after.on_timeout_hold = false,
                }
                assert!(
                    evidence
                        .observe(vec![after], &plan, &check, 11_130_000_000)
                        .is_err()
                );
            }
            let mut evidence = CommandEvidence::default();
            evidence
                .observe(
                    vec![command(u64::MAX, 11_100_000_000, 12)],
                    &plan,
                    &check,
                    11_100_000_000,
                )
                .unwrap();
            assert!(
                evidence
                    .observe(
                        vec![command(0, 11_130_000_000, 12)],
                        &plan,
                        &check,
                        11_130_000_000
                    )
                    .is_err()
            );
            let mut evidence = CommandEvidence::default();
            evidence
                .observe(
                    vec![command(1, 11_100_000_000, 12)],
                    &plan,
                    &check,
                    11_100_000_000,
                )
                .unwrap();
            assert!(evidence.cancelled_target(11_110_000_000).unwrap().is_none());
            assert!(evidence.cancelled_target(11_090_000_000).is_err());
            evidence
                .observe(
                    vec![command(2, 11_110_000_000, 12)],
                    &plan,
                    &check,
                    11_110_000_000,
                )
                .unwrap();
            assert!(evidence.cancelled_target(11_110_000_000).is_err());
            assert!(!same_command_target(&[0.0], &[-0.0]));
            assert!(!same_command_target(&[0.001], &[0.001_000_1]));
        }

        #[test]
        fn command_observer_failure_is_sticky_without_new_samples() {
            let observer = CommandObserver {
                queue: Arc::new(CommandQueue::new(VecDeque::from([Err(
                    "queue overflow".into()
                )]))),
                stop: Arc::new(AtomicBool::new(false)),
                worker: None,
            };
            assert_eq!(observer.drain().unwrap_err(), "queue overflow");
            assert_eq!(observer.drain().unwrap_err(), "queue overflow");
        }

        #[test]
        fn cancel_waits_for_mirror_to_cover_fixed_final_feedback_and_rejects_tail_change() {
            for changed in [false, true] {
                run(async {
                    let mut ops = cancel_ops();
                    let mut final_state = state(104, 13);
                    final_state.stamp_ns = 11_450_000_000;
                    ops.raw_state_r[4] = Scripted::Ok(final_state);
                    ops.command_r[4] = Ok(vec![command(7, 11_420_000_000, 13)]);
                    ops.command_r.push_back(Ok(vec![command(
                        8,
                        11_450_000_000,
                        if changed { 14 } else { 13 },
                    )]));
                    let result = execute(&mut ops, &cancel_plan(), tb()).await;
                    assert_eq!(result.is_err(), changed, "{result:?}");
                    assert_eq!(
                        ops.calls.last().map(String::as_str),
                        Some(if changed { "estop" } else { "disable" })
                    );
                    assert!(ops.raw_state_r.is_empty());
                    assert!(ops.command_r.is_empty());
                });
            }
        }

        #[test]
        fn single_joint_arguments_are_explicit_for_either_side() {
            let valid = raw_args();
            assert_eq!(Args::parse_from(&valid).unwrap().phase, Phase::SingleJoint);
            for flag in [
                "--joint-name",
                "--target-offset-raw",
                "--duration-ms",
                "--task-timeout-ms",
                "--hold-anchor-rad",
            ] {
                let mut missing = valid.clone();
                let index = missing.iter().position(|value| value == flag).unwrap();
                missing.drain(index..=index + 1);
                assert!(Args::parse_from(&missing).is_err(), "{flag}");
            }
            for extra in [
                ["--probe-joint", "j4"],
                ["--target-position-rad", "0.05"],
                ["--duration-ms", "0"],
                ["--task-timeout-ms", "20000"],
                ["--task-timeout-ms", "18446744073709551615"],
            ] {
                let mut invalid = valid.clone();
                invalid.extend(extra.map(str::to_string));
                assert!(Args::parse_from(&invalid).is_err(), "{extra:?}");
            }
            for phase in ["hold", "single-arm", "all"] {
                let mut invalid = valid.clone();
                invalid[1] = phase.into();
                assert!(Args::parse_from(&invalid).is_err());
            }
            let mut absolute = valid;
            absolute[3] = "right".into();
            let index = absolute
                .iter()
                .position(|value| value == "--target-offset-raw")
                .unwrap();
            absolute[index] = "--target-position-rad".into();
            absolute[index + 1] = "-0.04".into();
            assert!(Args::parse_from(&absolute).is_ok());
            for bad in ["NaN", "inf", "-inf", "1e100"] {
                absolute[index + 1] = bad.into();
                assert!(Args::parse_from(&absolute).is_err());
            }
        }

        #[test]
        fn single_joint_spec_is_only_new_schema_and_original_raw_conversion() {
            let plan = plan();
            let spec = plan.action_spec();
            assert_eq!(
                spec.kind,
                robot_rpc::client::dual_arm_workflow::ActionKindWire::Scripted
            );
            assert_eq!(spec.payload_schema_id, 0x040F);
            let payload =
                flatbuffers::root::<robot_msg::msg::SingleJointPositionPayload<'_>>(&spec.payload)
                    .unwrap();
            assert_eq!(payload.hardware_id(), "left_arm");
            assert_eq!(payload.joint_name(), "j4");
            assert_eq!(payload.duration_ms(), 20000);
            assert_eq!(
                payload.target_position_rad().to_bits(),
                f64::from(robot_adapter_feetech::raw_to_rad(2071)).to_bits()
            );
            assert_eq!(
                plan.expected_raw,
                [2047, 2047, 2047, 2047, 2071, 2047, 2047, 2047]
            );
            assert_eq!(ActionSpec::scripted(vec![]).payload_schema_id, 0x040B);
        }

        #[test]
        fn single_joint_rejects_config_range_rate_and_budget_before_enable() {
            let mut args = Args::parse_from(&raw_args()).unwrap();
            let layout = layout();
            let good = config();
            args.task_timeout_ms = Some(20800);
            assert!(SingleJointPlan::prepare(&args, &layout, &[0.0; 8], &good).is_err());
            args.task_timeout_ms = Some(20801);
            assert!(SingleJointPlan::prepare(&args, &layout, &[0.0; 8], &good).is_ok());
            for (field, value) in [
                ("max_position_rad", 0.01),
                ("max_velocity_rad_s", 0.0001),
                ("tolerance_rad", -1.0),
            ] {
                let mut bad = good.clone();
                bad["s1"]["single_joint"]["joints"][4][field] = toml::Value::Float(value);
                assert!(SingleJointPlan::prepare(&args, &layout, &[0.0; 8], &bad).is_err());
            }
            let mut bad = good.clone();
            bad["s1"]["single_joint"]["joints"][0]["min_position_rad"] = toml::Value::Float(0.01);
            assert!(SingleJointPlan::prepare(&args, &layout, &[0.0; 8], &bad).is_err());
            for name in ["j4", "unknown"] {
                let mut bad = good.clone();
                bad["s1"]["single_joint"]["joints"][0]["name"] = name.into();
                assert!(SingleJointPlan::prepare(&args, &layout, &[0.0; 8], &bad).is_err());
            }
            let mut bad = good.clone();
            bad["s1"]["single_joint"]["stable_window_ms"] = toml::Value::Integer(800);
            assert!(SingleJointPlan::prepare(&args, &layout, &[0.0; 8], &bad).is_err());
            let mut short = layout.clone();
            short.joints.pop();
            assert!(SingleJointPlan::prepare(&args, &short, &[0.0; 8], &good).is_err());
            args.target_offset_raw = Some(1);
            assert!(SingleJointPlan::prepare(&args, &layout, &[0.0; 8], &good).is_err());
            assert!(check_single_joint_motion(0.0, 0.088, 20000, MAX_PROBE_OFFSET_RAD).is_err());
            assert!(check_single_joint_motion(0.0, 0.05, 100, MAX_PROBE_OFFSET_RAD).is_err());
            let limits = [SingleJointLimit {
                min: -0.1,
                max: 0.1,
                velocity: 1.0,
                tolerance_rad: 0.0,
            }];
            assert!(check_single_joint_limits(&limits, &[0.1]).is_err());
            assert!(check_single_joint_limits(&limits, &[-0.1]).is_err());
        }

        #[test]
        fn single_joint_observer_enforces_source_ready_corridor_and_all_joints() {
            let mut good = check();
            for (step, offset) in [(1, 0), (2, 24)] {
                let sample = state(step, offset);
                good.observe(&sample, "left_arm", sample.stamp_ns, false)
                    .unwrap();
            }
            for (slot, offset) in [(0, 1), (4, -1), (4, 25)] {
                let mut sample = state(1, 12);
                sample.position_rad[slot] =
                    f64::from(robot_adapter_feetech::raw_to_rad(2047 + offset));
                assert!(
                    check()
                        .observe(&sample, "left_arm", sample.stamp_ns, false)
                        .is_err()
                );
            }
            for stamp in [0, 1_000_000_000, 900_000_000, 1_200_000_000] {
                let mut sample = state(1, 0);
                sample.stamp_ns = stamp;
                assert!(
                    check()
                        .observe(&sample, "left_arm", 1_100_000_000, false)
                        .is_err()
                );
            }
            assert!(
                check()
                    .observe(&state(1, 0), "left_arm", 1_400_000_000, false)
                    .is_err()
            );
            for field in 0..8 {
                let mut sample = state(1, 0);
                match field {
                    0 => sample.robot_id = "wrong".into(),
                    1 => sample.hardware_id = "wrong".into(),
                    2 => sample.layout_id = 0,
                    3 => sample.mode = S0Mode::Safe as i32,
                    4 => sample.control_link = 0,
                    5 => sample.output_mode = 0,
                    6 => sample.safety_state = 0,
                    _ => sample.fault_code = 1,
                }
                assert!(
                    check()
                        .observe(&sample, "left_arm", sample.stamp_ns, false)
                        .is_err()
                );
            }
            assert!(
                check()
                    .observe(&state(1, 0), "left_arm", 1_100_000_000, true)
                    .is_err()
            );
            assert!(
                check()
                    .observe(&state(1, 24), "left_arm", 1_100_000_000, true)
                    .is_ok()
            );
        }

        #[test]
        fn single_joint_success_uses_one_task_then_fresh_final_sample_and_disable() {
            run(async {
                let mut ops = ops();
                execute(&mut ops, &plan(), tb()).await.unwrap();
                assert_eq!(
                    ops.calls,
                    [
                        "enable", "state", "submit", "state", "status", "state", "disable"
                    ]
                );
                assert_eq!(ops.submitted.len(), 1);
                assert_eq!(ops.submitted[0].1, 25000);
                assert_eq!(ops.submitted[0].0.payload_schema_id, 0x040F);
            });
        }

        #[test]
        fn single_joint_success_status_without_final_position_or_freshness_estops() {
            for final_state in [state(202, 0), state(201, 24)] {
                run(async {
                    let mut ops = ops();
                    ops.raw_state_r[2] = Scripted::Ok(final_state);
                    assert!(execute(&mut ops, &plan(), tb()).await.is_err());
                    assert_eq!(ops.calls.last().map(String::as_str), Some("estop"));
                    assert!(
                        !ops.calls
                            .iter()
                            .any(|call| call.starts_with("cancel") || call == "disable")
                    );
                });
            }
        }

        #[test]
        fn single_joint_final_feedback_must_be_newer_than_success_receipt() {
            run(async {
                let mut ops = ops();
                ops.status_now = Some(21_150_000_000);
                let mut cached = state(202, 24);
                cached.stamp_ns = 21_125_000_000;
                ops.raw_state_r[2] = Scripted::Ok(cached);
                let error = execute(&mut ops, &plan(), tb()).await.unwrap_err();
                assert!(error.contains("成功回执后的新源帧"), "{error}");
                assert_eq!(ops.calls.last().map(String::as_str), Some("estop"));
                assert!(
                    !ops.calls
                        .iter()
                        .any(|call| call.starts_with("cancel") || call == "disable")
                );
            });
        }

        #[test]
        fn single_joint_active_drift_or_status_failure_cancels_then_estops() {
            for drift in [false, true] {
                run(async {
                    let mut ops = ops();
                    if drift {
                        let mut bad = state(2, 12);
                        bad.position_rad[0] = f64::from(robot_adapter_feetech::raw_to_rad(2052));
                        ops.raw_state_r[1] = Scripted::Ok(bad);
                    } else {
                        ops.status_r[0] = Scripted::Err("status unknown".into());
                    }
                    ops.cancel_r
                        .push_back(Scripted::Err("cancel unknown".into()));
                    assert!(execute(&mut ops, &plan(), tb()).await.is_err());
                    assert_eq!(
                        &ops.calls[ops.calls.len() - 2..],
                        ["cancel:action-1", "estop"]
                    );
                    assert_eq!(ops.submitted.len(), 1);
                });
            }
        }

        #[test]
        fn single_joint_terminal_failure_and_unconfirmed_disable_are_failures() {
            run(async {
                let mut ops = ops();
                ops.status_r[0] = Scripted::Ok(TaskSnapshot {
                    phase: TaskPhase::Terminal(TerminalState::Cancelled),
                    ..success_snapshot()
                });
                assert!(execute(&mut ops, &plan(), tb()).await.is_err());
                assert_eq!(ops.calls.last().map(String::as_str), Some("estop"));
                assert!(
                    !ops.calls
                        .iter()
                        .any(|call| call.starts_with("cancel") || call == "disable")
                );
            });
            run(async {
                let mut ops = ops();
                ops.disable_r
                    .push_back(Scripted::Err("not TorqueOffConfirmed".into()));
                assert!(execute(&mut ops, &plan(), tb()).await.is_err());
                assert_eq!(&ops.calls[ops.calls.len() - 2..], ["disable", "estop"]);
            });
        }

        #[test]
        fn single_joint_late_enable_or_submit_ack_never_resumes_or_retries() {
            for late_submit in [false, true] {
                run(async {
                    let mut ops = ops();
                    let completed = Arc::new(AtomicBool::new(false));
                    if late_submit {
                        ops.submit_r.push_back(Scripted::Late(
                            "action-1".into(),
                            Duration::from_millis(60),
                            Arc::clone(&completed),
                        ));
                    } else {
                        ops.enable_r.push_back(Scripted::Late(
                            (),
                            Duration::from_millis(60),
                            Arc::clone(&completed),
                        ));
                    }
                    let budgets = StageBudgets {
                        enable: Duration::from_millis(20),
                        submit: Duration::from_millis(20),
                        ..tb()
                    };
                    assert!(execute(&mut ops, &plan(), budgets).await.is_err());
                    let calls = ops.calls.clone();
                    tokio::time::sleep(Duration::from_millis(90)).await;
                    assert!(completed.load(Ordering::SeqCst));
                    assert_eq!(ops.calls, calls);
                    assert_eq!(
                        ops.calls
                            .iter()
                            .filter(|call| call.as_str() == "enable")
                            .count(),
                        1
                    );
                    assert_eq!(ops.submitted.len(), usize::from(late_submit));
                    assert_eq!(ops.calls.last().map(String::as_str), Some("estop"));
                    if late_submit {
                        assert_eq!(
                            &ops.calls[ops.calls.len() - 2..],
                            ["cancel:action-1", "estop"]
                        );
                    }
                });
            }
        }

        #[test]
        fn single_joint_execution_timeout_keeps_known_action_for_finalizer() {
            run(async {
                let mut ops = ops();
                ops.status_r[0] = Scripted::Hang;
                let mut plan = plan();
                plan.task_timeout_ms = 150;
                assert!(execute(&mut ops, &plan, tb()).await.is_err());
                assert_eq!(
                    &ops.calls[ops.calls.len() - 2..],
                    ["cancel:action-1", "estop"]
                );
                assert_eq!(ops.submitted.len(), 1);
            });
        }
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
