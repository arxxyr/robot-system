//! 双臂成对提交协议（v0.5 候选 E 段(7)，Spec D4 + D5 "双臂成对提交协议"
//! 8 条 + AC-E14）。
//!
//! ## 为什么住这里
//!
//! PD-E3 / D9：**共享语义单源**——`robot-rpc/tests/dimos_workflow_e2e.rs`
//! （6 子进程 e2e）与 robot-tests 真机验收 bin（段(9)/(10)）**同向消费**本
//! 模块，禁止各自复制协议逻辑；子进程 harness 留在各自 tests 侧。
//!
//! ## 不依赖 tonic `client` feature（RB4 边界延续）
//!
//! 本模块只定义**协议编排**，RPC 通路经 [`ArmEndpoint`] trait 注入：
//!
//! - e2e / 验收 bin（带 `client` feature）用 `RobotControlClient` 实现它；
//! - 单测用脚本化 mock 实现它——**协议分支全部可在无进程环境下确定性覆盖**
//!   （提交期部分失败 / 执行期不对称终态 / cancel 不收敛 → 双 EStop /
//!   双拒绝 / 收敛超时）。
//!
//! 因此本文件不 import `crate::proto` 的 client stub，只在
//! [`TaskSnapshot::from_proto`] 处消费 server-only build 亦生成的
//! `TaskStatus` message。
//!
//! ## 协议（D5 第 1-8 条，逐条落位）
//!
//! | D5 条 | 落点 |
//! |---|---|
//! | 1 并发提交 + Ack 偏差 | [`submit_pair`]（`tokio::join!`）+ [`PairSubmit::ack_skew`] / [`PairSubmit::check_ack_skew`] |
//! | 2 终态精确集 + pair success 定义 | [`TerminalState`]（Completed/Failed/Cancelled）+ [`PairResult::Success`] = 两侧 `Completed(Success)` |
//! | 3 提交期部分失败 → cancel 已接收侧 → 不收敛则双 EStop | [`submit_pair`] → [`cancel_surviving`] → [`escalate_double_estop`] |
//! | 4 执行期不对称终态 → cancel 幸存侧 / partial completion | [`await_pair_terminal`] |
//! | 5 stream 断开 → `GetTaskStatus` 轮询收敛（超时上限） | [`await_pair_terminal`] 全程走 [`ArmEndpoint::task_status`] 轮询（**不**依赖 stream；stream 仅作诊断面） |
//! | 6 双拒绝 → 不自动重试、确定性 FAIL | [`PairError::BothRejected`] |
//! | 7 偏差观测点 = **Ack 偏差**（`Started.at_ns` 仅诊断） | 本模块**不**消费 `Started` 事件；Ack 时刻由 [`RobotClock`] 打点 |
//! | 8 全部路径进 AC-E14 | `tests/dimos_workflow_e2e.rs::pair_partial_failure_paths` |
//!
//! ## 时间源与预算
//!
//! 所有时刻取自调用方传入的 [`RobotClock`]（业务代码禁 `Instant::now()`，
//! 全局 §"业务代码禁用的 API"）；超时窗口用 `tokio::time` 驱动。
//!
//! **墙钟上限 = 协议预算 + 收尾预算**（round 2 阻断 2）：协议阶段被
//! [`PairConfig::phase_budget`] 截断后，cancel / EStop 在独立的
//! [`PairConfig::cleanup_budget`] 内执行，总上限见
//! [`PairConfig::run_chunk_bound`]。
//!
//! ## 安全语义两条硬规矩
//!
//! 1. **超时 ≠ 明确拒绝**（round 2 阻断 1）：只有服务端 typed status 才证明
//!    "未启动"；超时 / transport 失败一律按**结果不确定**处理——cancel 全部
//!    已知 action 后**双 EStop**。
//! 2. **每次收尾至多一次双 EStop**（round 2 阻断 3）：raw 调用层
//!    （`raw_poll_once` / `raw_cancel_side`）**不**产生副作用，升级只在单一
//!    收尾点发生。

#![allow(clippy::doc_markdown)]

use std::fmt;
use std::time::Duration;

use robot_core::time::RobotClock;
use thiserror::Error;

// ============================================================================
// Side / 终态类型
// ============================================================================

/// 双臂中的一侧。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    /// 另一侧。
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }

    /// 日志 / 报告用短名。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// **终态精确集**（D5 第 2 条；对齐 `action_status.fbs` / proto `TaskState`
/// 的三个终态，"Completed 家族"表述已撤回）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalState {
    Completed,
    Failed,
    Cancelled,
}

/// action 结局（`ActionOutcomeProto.Kind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    Unspecified,
    Success,
    PartialSuccess,
}

/// 失败类别（`ActionFailureProto.Kind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Unspecified,
    DeadlineExceeded,
    PreconditionFailed,
    AdapterError,
    SafetyAbort,
    InternalError,
}

/// 任务相位：非终态两种 + 终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPhase {
    /// `TASK_STATE_UNSPECIFIED`（gateway 未知 / 尚未落库）。
    Unspecified,
    Pending,
    Executing,
    Terminal(TerminalState),
}

impl TaskPhase {
    /// 是否已进入终态精确集。
    #[must_use]
    pub const fn terminal(self) -> Option<TerminalState> {
        match self {
            Self::Terminal(t) => Some(t),
            _ => None,
        }
    }
}

/// 一次 `GetTaskStatus` 的快照（协议侧只关心相位 + 结局 + 失败原因）。
#[derive(Debug, Clone, PartialEq)]
pub struct TaskSnapshot {
    pub phase: TaskPhase,
    pub progress: f32,
    pub outcome: OutcomeKind,
    /// `PartialSuccess` 的 warning（其余为空）。
    pub outcome_warning: String,
    pub failure: FailureKind,
    /// 失败原因串（`DeadlineExceeded` 为空）。
    pub failure_reason: String,
}

impl TaskSnapshot {
    /// 从 proto `TaskStatus` 转换（server-only build 亦生成该 message；
    /// **不**需要 tonic `client` feature）。未知枚举值保守映射为
    /// `Unspecified`——**不** panic、**不**当成终态（避免误判 pair 成功）。
    #[must_use]
    pub fn from_proto(status: &crate::proto::TaskStatus) -> Self {
        use crate::proto::TaskState;
        use crate::proto::action_failure_proto::Kind as FKind;
        use crate::proto::action_outcome_proto::Kind as OKind;

        let phase = match TaskState::try_from(status.state) {
            Ok(TaskState::Pending) => TaskPhase::Pending,
            Ok(TaskState::Executing) => TaskPhase::Executing,
            Ok(TaskState::Completed) => TaskPhase::Terminal(TerminalState::Completed),
            Ok(TaskState::Failed) => TaskPhase::Terminal(TerminalState::Failed),
            Ok(TaskState::Cancelled) => TaskPhase::Terminal(TerminalState::Cancelled),
            Ok(TaskState::Unspecified) | Err(_) => TaskPhase::Unspecified,
        };
        let (outcome, outcome_warning) = status.outcome.as_ref().map_or_else(
            || (OutcomeKind::Unspecified, String::new()),
            |o| {
                let kind = match OKind::try_from(o.kind) {
                    Ok(OKind::Success) => OutcomeKind::Success,
                    Ok(OKind::PartialSuccess) => OutcomeKind::PartialSuccess,
                    Ok(OKind::Unspecified) | Err(_) => OutcomeKind::Unspecified,
                };
                (kind, o.warning.clone())
            },
        );
        let (failure, failure_reason) = status.failure.as_ref().map_or_else(
            || (FailureKind::Unspecified, String::new()),
            |f| {
                let kind = match FKind::try_from(f.kind) {
                    Ok(FKind::DeadlineExceeded) => FailureKind::DeadlineExceeded,
                    Ok(FKind::PreconditionFailed) => FailureKind::PreconditionFailed,
                    Ok(FKind::AdapterError) => FailureKind::AdapterError,
                    Ok(FKind::SafetyAbort) => FailureKind::SafetyAbort,
                    Ok(FKind::InternalError) => FailureKind::InternalError,
                    Ok(FKind::Unspecified) | Err(_) => FailureKind::Unspecified,
                };
                (kind, f.reason.clone())
            },
        );
        Self {
            phase,
            progress: status.progress,
            outcome,
            outcome_warning,
            failure,
            failure_reason,
        }
    }
}

/// 单侧终态报告（验收报告显式记录两侧终态——D5 第 4 条）。
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalReport {
    pub side: Side,
    pub action_id: String,
    pub state: TerminalState,
    pub outcome: OutcomeKind,
    pub outcome_warning: String,
    pub failure: FailureKind,
    pub failure_reason: String,
    /// 观测到终态的时刻（`RobotClock` 单调 ns）。
    pub observed_at_ns: u64,
}

impl TerminalReport {
    /// **pair success 的单侧条件**：`Completed` **且** `outcome == Success`
    /// （`Completed(PartialSuccess)` 不算成功——它是 cancel 收敛形态）。
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.state == TerminalState::Completed && self.outcome == OutcomeKind::Success
    }

    fn from_snapshot(
        side: Side,
        action_id: &str,
        snap: TaskSnapshot,
        state: TerminalState,
        observed_at_ns: u64,
    ) -> Self {
        Self {
            side,
            action_id: action_id.to_string(),
            state,
            outcome: snap.outcome,
            outcome_warning: snap.outcome_warning,
            failure: snap.failure,
            failure_reason: snap.failure_reason,
            observed_at_ns,
        }
    }
}

// ============================================================================
// 错误域
// ============================================================================

/// 单臂 RPC 失败（transport 或 gRPC status）；由 [`ArmEndpoint`] 实现方构造。
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ArmRpcError {
    /// 连接 / 传输层失败。**副作用不可证伪**——请求可能已到达服务端。
    #[error("transport: {0}")]
    Transport(String),
    /// gRPC status（`code` 用 `tonic::Code` 的 Debug 名，避免本模块 dep tonic 类型）。
    /// 服务端**已处理并明确拒绝**。
    #[error("status {code}: {message}")]
    Status { code: String, message: String },
}

impl ArmRpcError {
    /// gRPC code 名（`Status` 变体才有）。
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Status { code, .. } => Some(code),
            Self::Transport(_) => None,
        }
    }
}

/// **`InvokeTask` 失败的副作用语义**（round 3 P0-1）。
///
/// 通用 [`ArmRpcError`] **不再**自行推断副作用——gateway 在把请求 forward 给
/// S1 **之后**同样会产生 `Status`（RR 等待超时 `DeadlineExceeded`、response
/// decode / request-id 不匹配 `Internal`、oneshot 丢失 `Unavailable`，见
/// `task_handlers.rs`），端点 transport 超时还会形成 `Cancelled`。把这些当
/// "明确拒绝"会遗留已启动的 action。
///
/// 分类由**端点**按 transport 语义给出（[`Self::from_status_code`] 提供
/// 契约白名单实现），协议层只消费结论。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SubmitFailure {
    /// gateway / S1 契约**保证发生在任何副作用之前**（见
    /// [`DEFINITE_REJECTION_CODES`]）。
    #[error("definitely rejected: {0}")]
    DefiniteRejected(ArmRpcError),
    /// **无法证明**服务端未接受该 action（默认归类）。
    #[error("indeterminate: {0}")]
    Indeterminate(ArmRpcError),
}

/// `InvokeTask` 的**明确拒绝**白名单——只有这些 code 由 gateway / S1 契约
/// 保证"发生在任何副作用之前"（`task_handlers.rs` 实证）：
///
/// | code | 产生点 | 为什么可证明未启动 |
/// |---|---|---|
/// | `Unauthenticated` | token 不在 role 表 | forward 之前 |
/// | `PermissionDenied` | capability 不足 | forward 之前 |
/// | `InvalidArgument` | 参数 / payload / schema 校验 | forward 之前；或 S1 typed 拒绝 |
/// | `ResourceExhausted` | idempotency cache 满 | forward 之前 |
/// | `Unimplemented` | kind / schema 无 backend | forward 之前或 S1 dispatch 拒 |
/// | `FailedPrecondition` | S1 backend `start()` 拒绝 | action **未**进入执行 |
///
/// **刻意不含** `AlreadyExists`（幂等重放：同 key 的 action 可能**正在跑**）、
/// `DeadlineExceeded` / `Unavailable` / `Internal` / `Cancelled` / `Unknown`
/// （均可能发生在 forward 之后）。白名单之外一律
/// [`SubmitFailure::Indeterminate`]（default-deny）。
pub const DEFINITE_REJECTION_CODES: [&str; 6] = [
    "Unauthenticated",
    "PermissionDenied",
    "InvalidArgument",
    "ResourceExhausted",
    "Unimplemented",
    "FailedPrecondition",
];

impl SubmitFailure {
    /// 按 gRPC code 名分类（端点侧调用；白名单外 → `Indeterminate`）。
    #[must_use]
    pub fn from_status_code(code: &str, err: ArmRpcError) -> Self {
        if DEFINITE_REJECTION_CODES.contains(&code) {
            Self::DefiniteRejected(err)
        } else {
            Self::Indeterminate(err)
        }
    }

    /// transport 层失败恒为 `Indeterminate`。
    #[must_use]
    pub fn from_transport(err: ArmRpcError) -> Self {
        Self::Indeterminate(err)
    }

    /// 是否可证明"未启动"。
    #[must_use]
    pub const fn is_definite(&self) -> bool {
        matches!(self, Self::DefiniteRejected(_))
    }

    /// 取出底层错误。
    #[must_use]
    pub fn into_error(self) -> ArmRpcError {
        match self {
            Self::DefiniteRejected(e) | Self::Indeterminate(e) => e,
        }
    }
}

/// 双 EStop 升级的执行记录（**弱 Ack 语义照旧**：单侧失败只记录不冒泡——
/// 安全方向动作已发出，不能因记录失败再抛错阻断收尾）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EStopReport {
    pub reason: String,
    /// `Err(None)` = EStop RPC 自身超时（未在 `estop_timeout` 内返回）。
    pub left: Result<(), Option<ArmRpcError>>,
    pub right: Result<(), Option<ArmRpcError>>,
}

impl EStopReport {
    /// 两侧是否都 Ack 成功。
    #[must_use]
    pub fn both_acked(&self) -> bool {
        self.left.is_ok() && self.right.is_ok()
    }
}

/// **异常收尾报告**（round 1 阻断 2 + round 2 阻断 2/3）：任何异常路径返回前，
/// 本模块已在**独立的收尾预算**（[`PairConfig::cleanup_budget`]）内把两侧收拾
/// 干净——收敛的侧给终态报告；有任一侧未收敛 / RPC 异常则**恰发一次**双侧
/// EStop。调用方（e2e / 真机 bin）**不需要**再补 catch 逻辑。
#[derive(Debug, Clone, PartialEq)]
pub struct PairCleanup {
    /// 成功 cancel 收敛的侧（0..=2 条）。
    pub cancelled: Vec<TerminalReport>,
    /// 有任一侧未能收敛 / RPC 失败时的双 EStop 记录（**每次收尾至多一次**）。
    pub estop: Option<EStopReport>,
    /// 收尾预算耗尽（安全网触发）：`cancel_both` 内部各步本已有界，本位为
    /// `true` 说明实现或环境异常，**收尾可能不完整**。
    pub budget_exceeded: bool,
}

impl PairCleanup {
    /// 收尾是否"干净"（两侧都 cancel 收敛、未触发 EStop、未超预算）。
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.estop.is_none() && !self.budget_exceeded
    }
}

/// Ack 偏差 gate 的**纯判据**结果（round 2 阻断 5：纯判据不得伪造带"已收尾"
/// 语义的 [`PairError`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("ack skew {skew:?} exceeds limit {limit:?}")]
pub struct AckSkewViolation {
    pub skew: Duration,
    pub limit: Duration,
}

/// 成对提交 / 收敛协议的失败域。**每个变体都已完成安全收尾**（见
/// [`PairCleanup`] / `estop` 字段）——[`AckSkewViolation`] 是独立的纯判据类型，
/// 不在本枚举内。
#[derive(Debug, Clone, Error, PartialEq)]
pub enum PairError {
    /// **双侧明确拒绝**（D5 第 6 条）：两侧都是
    /// [`SubmitFailure::DefiniteRejected`]（契约白名单
    /// [`DEFINITE_REJECTION_CODES`]）→ 可证明两侧都没启动 action，
    /// **无需收尾**、**不自动重试**，chunk 判 FAIL。
    #[error("both arms definitively rejected the chunk: left={left}, right={right}")]
    BothRejected {
        left: ArmRpcError,
        right: ArmRpcError,
    },

    /// **提交结果不确定**（round 2 阻断 1）：至少一侧超时 / transport 失败——
    /// **Ack 未收到 ≠ 服务端未接受**，该侧可能已经启动 action 且我们拿不到
    /// `action_id`（无法 cancel）。已 cancel 全部**已知** action，并**双 EStop**。
    #[error("submit outcome indeterminate for {}: {}; known actions cancelled + double EStop (clean={})",
        causes.iter().map(|(s, _)| s.label()).collect::<Vec<_>>().join(","),
        causes.iter().map(|(s, e)| format!("{s}={e}")).collect::<Vec<_>>().join("; "),
        cleanup.is_clean())]
    IndeterminateSubmit {
        /// 结果不确定的侧及其原因（1..=2 条）。
        causes: Vec<(Side, ArmRpcError)>,
        /// 同时**明确**被拒的侧（若有）。
        rejected: Option<(Side, ArmRpcError)>,
        cleanup: Box<PairCleanup>,
    },

    /// **提交期部分失败**（D5 第 3 条）：一侧**明确**被拒（可证明未启动），
    /// 已接收侧已成功 cancel 收敛。
    #[error("submit-phase partial failure: {rejected_side} rejected ({rejected}); accepted side cancelled → {:?}", cancelled.state)]
    SubmitPartialFailure {
        rejected_side: Side,
        rejected: ArmRpcError,
        cancelled: Box<TerminalReport>,
    },

    /// **Ack 偏差超限**（D5 第 1/7 条 gate）——**两侧已被 cancel**（不留在跑）。
    #[error("ack skew {skew:?} exceeds limit {limit:?}; both sides cancelled (clean={})", cleanup.is_clean())]
    AckSkewExceeded {
        skew: Duration,
        limit: Duration,
        cleanup: Box<PairCleanup>,
    },

    /// 双终态收敛超时（D5 第 5 条上限）——**未终态的侧已被 cancel**。
    #[error("pair did not reach terminal states within {timeout:?}: left={left:?}, right={right:?}; cleaned up (clean={})", cleanup.is_clean())]
    ConvergenceTimeout {
        timeout: Duration,
        left: TaskPhase,
        right: TaskPhase,
        cleanup: Box<PairCleanup>,
    },

    /// cancel 不收敛 → **已发双侧 EStop**（D5 第 3/4 条）。
    #[error("cancel of {side} did not converge within {timeout:?}; double EStop issued (both_acked={})", estop.both_acked())]
    CancelNotConverged {
        side: Side,
        action_id: String,
        timeout: Duration,
        estop: Box<EStopReport>,
    },

    /// 协议期 RPC 失败（cancel / status）→ **已发双侧 EStop**（round 1 阻断 2）。
    #[error("{side} rpc failed during {phase}: {source}; double EStop issued (both_acked={})", estop.both_acked())]
    Rpc {
        side: Side,
        phase: &'static str,
        source: ArmRpcError,
        estop: Box<EStopReport>,
    },

    /// 单次 RPC **硬超时**（`PairConfig::rpc_timeout`）→ **已发双侧 EStop**
    /// （round 1 阻断 1：端点实现若忘了设 transport 超时，协议层也必须有界）。
    #[error("{side} rpc timed out during {phase} after {timeout:?}; double EStop issued (both_acked={})", estop.both_acked())]
    RpcTimeout {
        side: Side,
        phase: &'static str,
        timeout: Duration,
        estop: Box<EStopReport>,
    },

    /// 协议阶段预算（[`PairConfig::phase_budget`]）耗尽——最后一道保险，
    /// 已在独立收尾预算内 cancel + EStop。
    #[error("phase {phase} exceeded budget {budget:?}; cleaned up (clean={})", cleanup.is_clean())]
    PhaseBudgetExceeded {
        phase: &'static str,
        budget: Duration,
        cleanup: Box<PairCleanup>,
    },

    /// **配置非法**（round 3 P0-2 第 3 条）：在**任何 invoke 之前**由
    /// [`PairConfig::validate`] 拦下——收尾预算不足等配置会让安全收尾失效。
    #[error("invalid PairConfig: {0}")]
    InvalidConfig(String),
}

// ============================================================================
// 端点抽象（通用 action seam）
// ============================================================================

/// `payload_schema_id` 哨兵：**无 payload**（镜像 robot-s1
/// `action::NO_PAYLOAD_SCHEMA_ID`，wire 契约固定 0；robot-rpc 不 dep robot-s1）。
pub const NO_PAYLOAD_SCHEMA_ID: u32 = 0;

/// action kind 的 wire 取值（对齐 proto `ActionKindProto`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKindWire {
    DwellMs,
    HoldPose,
    Scripted,
    Ik,
}

impl ActionKindWire {
    /// proto enum 数值（`ActionKindProto`）。
    #[must_use]
    pub const fn proto_value(self) -> i32 {
        match self {
            Self::DwellMs => 1,
            Self::HoldPose => 2,
            Self::Scripted => 3,
            Self::Ik => 4,
        }
    }
}

/// 一次 `InvokeTask` 的**通用** action 描述（round 1 阻断 3：协议层不再只会
/// 发 Scripted；调用点必须显式说明发的是什么 kind / schema，禁止"名叫
/// `invoke_scripted` 却暗中发 Dwell"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionSpec {
    pub kind: ActionKindWire,
    pub payload_schema_id: u32,
    pub payload: Vec<u8>,
}

impl ActionSpec {
    /// Scripted chunk（payload = `ScriptedPayload` fbs bytes，由
    /// [`crate::client::trajectory::build_scripted_payload`] 构造）。
    #[must_use]
    pub fn scripted(payload: Vec<u8>) -> Self {
        Self {
            kind: ActionKindWire::Scripted,
            payload_schema_id: robot_msg::schema_id::SCHEMA_SCRIPTED_PAYLOAD,
            payload,
        }
    }

    /// `DwellMs` + 空 payload。**路由取决于 S1 profile**：
    ///
    /// - 常规 profile（含 `mock_dwell`）→ `MockDwellBackend`（N ms 后 Success）；
    /// - **test profile**（含 `fault_inject` 且**不含** `mock_dwell`）→
    ///   `FaultInjectBackend`（3 tick 后 `InternalError`）——AC-E14 执行期
    ///   failure / AC-E18 failure 分支的**唯一触发通路**。
    ///
    /// 两者分发键完全重合是有意的（0 新 wire，Spec R3-B1），因此**调用点
    /// 必须清楚自己跑在哪种 profile 上**。
    #[must_use]
    pub fn dwell_no_payload() -> Self {
        Self {
            kind: ActionKindWire::DwellMs,
            payload_schema_id: NO_PAYLOAD_SCHEMA_ID,
            payload: Vec::new(),
        }
    }
}

/// 一条臂的 RPC 端点。实现方负责 `CommandMeta` / token / deadline / transport
/// 超时等细节；本模块只编排协议，并**在协议层再加一道硬超时**
/// （[`PairConfig::rpc_timeout`]），使端点实现漏设超时时协议仍有界。
///
/// **超时层级（内 → 外，端点实现须遵守）**：gateway typed status 窗
/// （`CommandMeta.timeout_ms`）< 端点 transport 超时 < 协议
/// [`PairConfig::rpc_timeout`]——顺序反了会让协议层先断、拿不到服务端的
/// typed 拒绝，进而把"明确拒绝"误判成"结果不确定"（round 2 阻断 1 相关）。
///
/// **并发形态**：[`submit_pair`] 用 `tokio::join!` 同时 poll 两侧的
/// future——两侧是**不同对象**的可变借用，借用检查天然放行。
pub trait ArmEndpoint: Send {
    /// 本端点属于哪一侧（报告 / 日志用）。
    fn side(&self) -> Side;

    /// 提交一条 action（见 [`ActionSpec`]）；成功返 `action_id`。
    ///
    /// **失败必须分类**（round 3 P0-1）：实现方按 transport / gateway 契约
    /// 判定该失败是否**证明** action 未启动——参考
    /// [`SubmitFailure::from_status_code`]（契约白名单）与
    /// [`SubmitFailure::from_transport`]。分类错会让协议层遗留已启动的
    /// action（误判"明确拒绝"）或无谓触发双 EStop（误判"不确定"）。
    fn invoke_action(
        &mut self,
        spec: ActionSpec,
    ) -> impl Future<Output = Result<String, SubmitFailure>> + Send;

    /// `CancelTask`。
    fn cancel_task(
        &mut self,
        action_id: &str,
    ) -> impl Future<Output = Result<(), ArmRpcError>> + Send;

    /// `GetTaskStatus` → [`TaskSnapshot`]（实现方用
    /// [`TaskSnapshot::from_proto`] 转换）。
    fn task_status(
        &mut self,
        action_id: &str,
    ) -> impl Future<Output = Result<TaskSnapshot, ArmRpcError>> + Send;

    /// `EStop`（安全方向；弱 Ack 语义）。
    fn estop(&mut self, reason: &str) -> impl Future<Output = Result<(), ArmRpcError>> + Send;
}

/// 左右两个端点。
#[derive(Debug)]
pub struct ArmPair<E> {
    pub left: E,
    pub right: E,
}

impl<E: ArmEndpoint> ArmPair<E> {
    /// 按 [`Side`] 取端点。
    pub fn side_mut(&mut self, side: Side) -> &mut E {
        match side {
            Side::Left => &mut self.left,
            Side::Right => &mut self.right,
        }
    }
}

// ============================================================================
// 配置：协议预算 + 独立收尾预算（round 2 阻断 2）
// ============================================================================

/// 协议时间窗（AC-E14 定值；e2e 与验收 bin 共用同一默认值）。
///
/// ## 总硬上限 = **协议预算 + 收尾预算**
///
/// 收尾（cancel / EStop）**不占**协议预算：协议阶段被
/// [`Self::phase_budget`] 截断后，收尾在独立的 [`Self::cleanup_budget`] 内
/// 执行。因此
///
/// ```text
/// run_chunk_pair 墙钟上限 ≈ rpc_timeout            (submit 并发 join)
///                        + cleanup_budget         (submit 异常收尾)
///                        + phase_budget(terminal_timeout)
///                        + cleanup_budget         (terminal 异常收尾)
///                        = PairConfig::run_chunk_bound()
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairConfig {
    /// **Ack 偏差 gate 上限**（D5 第 1/7 条）：两侧 `InvokeTask` 返回时刻之差。
    pub ack_skew_limit: Duration,
    /// 双终态收敛上限（D5 第 5 条）。
    pub terminal_timeout: Duration,
    /// 单侧 cancel 收敛上限；超时 → 双 EStop（D5 第 3/4 条）。
    pub cancel_timeout: Duration,
    /// `GetTaskStatus` 轮询周期。
    pub poll_interval: Duration,
    /// **单次 RPC 硬超时**（round 1 阻断 1）：协议层对每次
    /// invoke / cancel / status 调用都包一层。**须大于端点自身的 transport
    /// 超时**（见 [`ArmEndpoint`] 超时层级）。
    pub rpc_timeout: Duration,
    /// EStop RPC 硬超时（安全方向也不允许永久挂）。
    pub estop_timeout: Duration,
    /// **收尾预算**（round 2 阻断 2）：一次收尾（至多 2 侧 cancel + 各自收敛
    /// 轮询 + 一次双 EStop）的独立墙钟上限。必须满足
    /// [`Self::cleanup_budget_is_sufficient`]；作为安全网，正常不应触发。
    pub cleanup_budget: Duration,
}

impl Default for PairConfig {
    fn default() -> Self {
        Self {
            // loopback gateway 上单次 InvokeTask 往返 = RR 一跳（gateway →
            // iceoryx2 RR → s1），实测毫秒级；250 ms 给调度抖动 + 冷启动首
            // 帧留足余量，同时远小于一个 chunk 时长（10 × 100 ms）——偏差达
            // 到 chunk 量级即视为两臂未真并发（AC-E14 gate；e2e 记录实测值）。
            ack_skew_limit: Duration::from_millis(250),
            // 一个 chunk = 10 waypoint × 100 ms = 1 s；10 s 覆盖 dwell +
            // 终态事件落库 + 轮询粒度。
            terminal_timeout: Duration::from_secs(10),
            // cancel 协作式收敛：下一个 S1 tick（20 ms）即置终态；3 s 是
            // "确实不收敛"的判据而非正常等待。
            cancel_timeout: Duration::from_secs(3),
            poll_interval: Duration::from_millis(20),
            // > 端点 transport 超时（`TonicArm` = 3 s）> gateway meta 窗（2 s）。
            rpc_timeout: Duration::from_secs(4),
            estop_timeout: Duration::from_secs(2),
            // ≥ 2×(rpc_timeout + cancel_timeout) + estop_timeout = 16 s。
            cleanup_budget: Duration::from_secs(20),
        }
    }
}

impl PairConfig {
    /// 某协议阶段的预算（**不含**收尾）：阶段窗 + 一次在环内 cancel 收敛 +
    /// 两次 RPC 超时余量。
    #[must_use]
    pub fn phase_budget(&self, phase_window: Duration) -> Duration {
        phase_window
            .saturating_add(self.cancel_timeout)
            .saturating_add(self.rpc_timeout.saturating_mul(2))
    }

    /// 收尾预算是否足够覆盖最坏收尾（2 侧 × (cancel RPC + 收敛轮询) +
    /// 一次双 EStop）。
    #[must_use]
    pub fn cleanup_budget_is_sufficient(&self) -> bool {
        let need = self
            .rpc_timeout
            .saturating_add(self.cancel_timeout)
            .saturating_mul(2)
            .saturating_add(self.estop_timeout);
        self.cleanup_budget >= need
    }

    /// 配置自检（**任何 invoke 之前**强制调用；round 3 P0-2 第 3 条）。
    ///
    /// # Errors
    ///
    /// 收尾预算不足以覆盖最坏收尾 / 关键窗口为 0 → `Err(说明)`。
    pub fn validate(&self) -> Result<(), String> {
        if !self.cleanup_budget_is_sufficient() {
            return Err(format!(
                "cleanup_budget {:?} < 2×(rpc_timeout {:?} + cancel_timeout {:?}) + \
                 estop_timeout {:?}：收尾可能来不及发 EStop",
                self.cleanup_budget, self.rpc_timeout, self.cancel_timeout, self.estop_timeout
            ));
        }
        if self.estop_timeout.is_zero() || self.rpc_timeout.is_zero() {
            return Err("estop_timeout / rpc_timeout 不能为 0".to_string());
        }
        if self.cleanup_budget <= self.estop_timeout {
            return Err(format!(
                "cleanup_budget {:?} 必须 > estop_timeout {:?}（否则 cancel 阶段预算为 0）",
                self.cleanup_budget, self.estop_timeout
            ));
        }
        Ok(())
    }

    /// 收尾中 **cancel 阶段**的预算——`cleanup_budget` 扣掉给 EStop 预留的
    /// `estop_timeout`（round 3 P0-2 第 2 条：cancel 不得吃掉 EStop 预算）。
    #[must_use]
    pub fn cancel_phase_budget(&self) -> Duration {
        self.cleanup_budget.saturating_sub(self.estop_timeout)
    }

    /// [`run_chunk_pair`] 的墙钟总上限（见结构体 doc 公式）。
    #[must_use]
    pub fn run_chunk_bound(&self) -> Duration {
        self.rpc_timeout
            .saturating_add(self.cleanup_budget.saturating_mul(2))
            .saturating_add(self.phase_budget(self.terminal_timeout))
    }
}

// ============================================================================
// 结果类型
// ============================================================================

/// 单侧提交 Ack。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitAck {
    pub side: Side,
    pub action_id: String,
    /// Ack 返回时刻（`RobotClock` 单调 ns）。
    pub acked_at_ns: u64,
}

/// [`submit_pair`] 成功结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairSubmit {
    pub left: SubmitAck,
    pub right: SubmitAck,
}

impl PairSubmit {
    /// 两侧 Ack 返回时刻之差（D5 第 1/7 条的**唯一** gate 量）。
    #[must_use]
    pub fn ack_skew(&self) -> Duration {
        let a = self.left.acked_at_ns;
        let b = self.right.acked_at_ns;
        Duration::from_nanos(a.abs_diff(b))
    }

    /// **纯判据**（不做任何收尾，round 2 阻断 5）：偏差是否在上限内（`≤`）。
    /// 生产路径用 [`run_chunk_pair`]——它在超限时会 cancel 双侧。
    ///
    /// # Errors
    ///
    /// 偏差 > `cfg.ack_skew_limit` → [`AckSkewViolation`]。
    pub fn check_ack_skew(&self, cfg: &PairConfig) -> Result<(), AckSkewViolation> {
        let skew = self.ack_skew();
        if skew > cfg.ack_skew_limit {
            return Err(AckSkewViolation {
                skew,
                limit: cfg.ack_skew_limit,
            });
        }
        Ok(())
    }

    /// 按侧取 `action_id`。
    #[must_use]
    pub fn action_id(&self, side: Side) -> &str {
        match side {
            Side::Left => &self.left.action_id,
            Side::Right => &self.right.action_id,
        }
    }
}

/// 双终态收敛结果（D5 第 2/4 条）。
#[derive(Debug, Clone, PartialEq)]
pub enum PairResult {
    /// **pair success**：两侧均 `Completed(Success)`。
    Success {
        left: Box<TerminalReport>,
        right: Box<TerminalReport>,
    },
    /// **partial completion**：恰一侧成功（pair 判 FAIL，但两侧终态都要记录）。
    PartialCompletion {
        succeeded: Box<TerminalReport>,
        other: Box<TerminalReport>,
    },
    /// 两侧都不成功。
    BothUnsuccessful {
        left: Box<TerminalReport>,
        right: Box<TerminalReport>,
    },
}

impl PairResult {
    /// pair 是否成功（**仅** [`Self::Success`]）。
    #[must_use]
    pub fn is_pair_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }

    fn classify(left: TerminalReport, right: TerminalReport) -> Self {
        match (left.is_success(), right.is_success()) {
            (true, true) => Self::Success {
                left: Box::new(left),
                right: Box::new(right),
            },
            (true, false) => Self::PartialCompletion {
                succeeded: Box::new(left),
                other: Box::new(right),
            },
            (false, true) => Self::PartialCompletion {
                succeeded: Box::new(right),
                other: Box::new(left),
            },
            (false, false) => Self::BothUnsuccessful {
                left: Box::new(left),
                right: Box::new(right),
            },
        }
    }
}

/// [`run_chunk_pair`] 的成功产物。
#[derive(Debug, Clone, PartialEq)]
pub struct PairRun {
    pub submit: PairSubmit,
    pub result: PairResult,
    /// 实测 Ack 偏差（D5 第 7 条：仅此量作 gate；`Started.at_ns` 仅诊断）。
    pub ack_skew: Duration,
}

// ============================================================================
// RPC 硬超时原语 + 无副作用（raw）调用层
// ============================================================================

/// 单次 RPC 的失败形态。
#[derive(Debug, Clone, PartialEq, Eq)]
enum RpcFailure {
    Failed(ArmRpcError),
    TimedOut,
}

/// **无副作用**的 RPC 异常（raw 层返回；升级 EStop 由单一收尾点做——round 2
/// 阻断 3：避免重复升级）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RpcAnomaly {
    side: Side,
    phase: &'static str,
    failure: RpcFailure,
}

/// 给 `invoke_action` 包硬超时：**超时恒为 `Indeterminate`**（Ack 未收到
/// 不能证明服务端未接受）。
async fn bounded_submit<F>(fut: F, timeout: Duration) -> Result<String, SubmitFailure>
where
    F: Future<Output = Result<String, SubmitFailure>>,
{
    match tokio::time::timeout(timeout, fut).await {
        Ok(r) => r,
        Err(_elapsed) => Err(SubmitFailure::from_transport(ArmRpcError::Transport(
            format!("invoke rpc timed out after {timeout:?}"),
        ))),
    }
}

/// 给任意端点调用包硬超时。
async fn bounded<T, F>(fut: F, timeout: Duration) -> Result<T, RpcFailure>
where
    F: Future<Output = Result<T, ArmRpcError>>,
{
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(RpcFailure::Failed(e)),
        Err(_elapsed) => Err(RpcFailure::TimedOut),
    }
}

/// **raw** 单次 `GetTaskStatus`（有界；**不**做 EStop 升级）。
async fn raw_poll_once<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    side: Side,
    action_id: &str,
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> Result<(TaskPhase, Option<TerminalReport>), RpcAnomaly> {
    let snap = bounded(pair.side_mut(side).task_status(action_id), cfg.rpc_timeout)
        .await
        .map_err(|failure| RpcAnomaly {
            side,
            phase: "poll",
            failure,
        })?;
    let phase = snap.phase;
    let report = phase
        .terminal()
        .map(|t| TerminalReport::from_snapshot(side, action_id, snap, t, clock.now().0));
    Ok((phase, report))
}

/// **raw** 单侧 cancel + 等终态（有界；**不**做 EStop 升级）。
/// `Ok(None)` = `cancel_timeout` 内未收敛。
async fn raw_cancel_side<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    side: Side,
    action_id: &str,
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> Result<Option<TerminalReport>, RpcAnomaly> {
    bounded(pair.side_mut(side).cancel_task(action_id), cfg.rpc_timeout)
        .await
        .map_err(|failure| RpcAnomaly {
            side,
            phase: "cancel",
            failure,
        })?;
    // round 3 P0-2 第 5 条：**整个收敛阶段**用墙钟限住——否则最后一次 status
    // RPC（各自 rpc_timeout）会把 cancel_timeout 撑过头。
    match tokio::time::timeout(
        cfg.cancel_timeout,
        converge_after_cancel(pair, side, action_id, clock, cfg),
    )
    .await
    {
        Ok(r) => r,
        Err(_elapsed) => Ok(None),
    }
}

/// cancel 之后的收敛轮询（外层已加墙钟上限）。
async fn converge_after_cancel<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    side: Side,
    action_id: &str,
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> Result<Option<TerminalReport>, RpcAnomaly> {
    let deadline_ns = clock
        .now()
        .0
        .saturating_add(duration_to_ns(cfg.cancel_timeout));
    loop {
        if let (_, Some(report)) = raw_poll_once(pair, side, action_id, clock, cfg).await? {
            return Ok(Some(report));
        }
        if clock.now().0 >= deadline_ns {
            return Ok(None);
        }
        tokio::time::sleep(cfg.poll_interval).await;
    }
}

/// [`RpcAnomaly`] + 已完成的 EStop → typed 错误。
fn anomaly_to_error(a: RpcAnomaly, cfg: &PairConfig, estop: EStopReport) -> PairError {
    let estop = Box::new(estop);
    match a.failure {
        RpcFailure::Failed(source) => PairError::Rpc {
            side: a.side,
            phase: a.phase,
            source,
            estop,
        },
        RpcFailure::TimedOut => PairError::RpcTimeout {
            side: a.side,
            phase: a.phase,
            timeout: cfg.rpc_timeout,
            estop,
        },
    }
}

// ============================================================================
// 收尾（独立预算；每次至多一次双 EStop —— round 2 阻断 2/3）
// ============================================================================

/// 双侧 `EStop`（并发下发 + 各自硬超时；**弱 Ack 语义**——单侧失败只进报告
/// 不冒泡）。
pub async fn escalate_double_estop<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    reason: &str,
    cfg: &PairConfig,
) -> EStopReport {
    let t = cfg.estop_timeout;
    let ArmPair { left, right } = &mut *pair;
    let (l, r) = tokio::join!(
        bounded(left.estop(reason), t),
        bounded(right.estop(reason), t)
    );
    let map = |res: Result<(), RpcFailure>| match res {
        Ok(()) => Ok(()),
        Err(RpcFailure::Failed(e)) => Err(Some(e)),
        Err(RpcFailure::TimedOut) => Err(None),
    };
    EStopReport {
        reason: reason.to_string(),
        left: map(l),
        right: map(r),
    }
}

/// 收尾（round 3 P0-2 重排）：
///
/// - `force_estop = true`（提交结果不确定 / 阶段预算耗尽）→ **先立即发一次
///   双 EStop**，再对已知 action 做尽力 cancel。安全动作不排在任何等待之后。
/// - `force_estop = false` → 先 cancel 已知 action；有任一侧失败 / 不收敛才
///   升级一次双 EStop。
///
/// **预算切分**：cancel 阶段跑在 [`PairConfig::cancel_phase_budget`] 内
/// （= `cleanup_budget − estop_timeout`），**为 EStop 保留固定预算**；
/// EStop 自身由 `estop_timeout` 有界。因此收尾总耗时 ≤ `cleanup_budget`，
/// 且**不存在"EStop 从未被调用"**的路径。
async fn cleanup_within_budget<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    known: &[(Side, String)],
    reason: &str,
    force_estop: bool,
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> PairCleanup {
    // 1) 强制路径：EStop **先行**。
    let forced = if force_estop {
        let full = format!("dual_arm_workflow: {reason} → immediate double EStop");
        Some(escalate_double_estop(pair, &full, cfg).await)
    } else {
        None
    };

    // 2) cancel 阶段（独立预算，绝不吃掉 EStop 预留）。
    let cancel_fut = cancel_known(pair, known, clock, cfg);
    let (cancelled, trouble, budget_exceeded) =
        match tokio::time::timeout(cfg.cancel_phase_budget(), cancel_fut).await {
            Ok((c, t)) => (c, t, false),
            Err(_elapsed) => (
                Vec::new(),
                Some("cancel phase budget exceeded".to_string()),
                true,
            ),
        };

    // 3) 非强制路径：只有出问题才升级（**恰一次**）。
    let estop = match forced {
        Some(report) => Some(report),
        None if trouble.is_some() || budget_exceeded => {
            let why = trouble.unwrap_or_else(|| "cleanup budget exceeded".to_string());
            let full = format!("dual_arm_workflow: {reason} ({why}) → double EStop");
            Some(escalate_double_estop(pair, &full, cfg).await)
        }
        None => None,
    };
    PairCleanup {
        cancelled,
        estop,
        budget_exceeded,
    }
}

/// 逐侧 cancel 已知 action（raw，无 EStop 副作用）；返 (已收敛终态, 首个问题)。
async fn cancel_known<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    known: &[(Side, String)],
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> (Vec<TerminalReport>, Option<String>) {
    let mut cancelled = Vec::new();
    let mut trouble: Option<String> = None;
    for (side, action_id) in known {
        match raw_cancel_side(pair, *side, action_id, clock, cfg).await {
            Ok(Some(report)) => cancelled.push(report),
            Ok(None) => {
                trouble.get_or_insert_with(|| format!("{side} cancel did not converge"));
            }
            Err(a) => {
                trouble.get_or_insert_with(|| format!("{} {} rpc anomaly", a.side, a.phase));
            }
        }
    }
    (cancelled, trouble)
}

/// 两侧已知 action 的 `(Side, action_id)` 列表。
fn known_actions(submit: &PairSubmit, sides: &[Side]) -> Vec<(Side, String)> {
    sides
        .iter()
        .map(|&s| (s, submit.action_id(s).to_string()))
        .collect()
}

// ============================================================================
// run_chunk_pair —— **单一安全入口**
// ============================================================================

/// 跑完一对 chunk：提交 → Ack 偏差 gate → 等双终态。**所有异常路径都在本
/// 模块内、在独立收尾预算里完成安全收尾**：
///
/// | 异常 | 收尾 |
/// |---|---|
/// | 双侧**明确**拒绝 | 无副作用，直接 FAIL（不重试，D5 第 6 条） |
/// | 任一侧**超时 / transport 失败**（结果不确定） | cancel 全部已知 action + **双 EStop** |
/// | 一侧明确被拒、另一侧接受 | cancel 已接收侧（不收敛 → 双 EStop） |
/// | **Ack 偏差超限** | **cancel 双侧** |
/// | cancel / status RPC 失败或超时 | **双 EStop**（恰一次） |
/// | 终态不收敛 | cancel 未终态侧 → 失败则**双 EStop** |
/// | 协议预算耗尽 | 独立收尾预算内 cancel + EStop |
///
/// 墙钟总上限 = [`PairConfig::run_chunk_bound`]。
///
/// # Errors
///
/// 见 [`PairError`]——每个变体都带收尾记录。
pub async fn run_chunk_pair<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    left: ActionSpec,
    right: ActionSpec,
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> Result<PairRun, PairError> {
    let submit = submit_pair(pair, left, right, clock, cfg).await?;
    // 注：配置校验在 `submit_pair` 入口（任何 invoke 之前）已做。
    let ack_skew = submit.ack_skew();
    if let Err(v) = submit.check_ack_skew(cfg) {
        // gate 违例 = 两臂未真并发；两个 action 都在跑，必须收掉。
        let known = known_actions(&submit, &[Side::Left, Side::Right]);
        let cleanup = cleanup_within_budget(pair, &known, "ack skew gate", false, clock, cfg).await;
        return Err(PairError::AckSkewExceeded {
            skew: v.skew,
            limit: v.limit,
            cleanup: Box::new(cleanup),
        });
    }
    let result = await_pair_terminal(pair, &submit, clock, cfg).await?;
    Ok(PairRun {
        submit,
        result,
        ack_skew,
    })
}

// ============================================================================
// submit_pair（D5 第 1 / 3 / 6 条 + round 2 阻断 1）
// ============================================================================

/// **并发**提交一对 action：两侧同时 `InvokeTask`（每次调用带
/// [`PairConfig::rpc_timeout`] 硬超时），Ack 时刻由 `clock` 在**各自 future
/// 内**打点。
///
/// **失败分类（round 2 阻断 1 + round 3 P0-1）**：由端点给出的
/// [`SubmitFailure`] 决定——只有契约白名单内的 code
/// （[`DEFINITE_REJECTION_CODES`]）算"明确拒绝、可证明未启动"；超时 /
/// transport 失败 / 白名单外的 status（`DeadlineExceeded` / `Unavailable` /
/// `Internal` / `Cancelled` / `Unknown` …）一律视为**结果不确定**——该侧可能
/// 已接受并启动 action 而我们没有 `action_id`，故**先双 EStop**再 cancel
/// 全部已知 action。
///
/// Ack 偏差 gate **不在**这里施加——见 [`run_chunk_pair`]。
///
/// # Errors
///
/// [`PairError::BothRejected`] / [`PairError::IndeterminateSubmit`] /
/// [`PairError::SubmitPartialFailure`] 等。
pub async fn submit_pair<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    left_spec: ActionSpec,
    right_spec: ActionSpec,
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> Result<PairSubmit, PairError> {
    // round 3 P0-2 第 3 条：**任何 invoke 之前**强制校验配置——收尾预算不足
    // 会让"异常已安全收尾"的不变量失效。
    cfg.validate().map_err(PairError::InvalidConfig)?;
    // **各侧在自己的 future 内打点**（D5 第 1/7 条）：join 返回后统一取 now
    // 会把偏差抹成 0，gate 失效。
    let timeout = cfg.rpc_timeout;
    let ArmPair { left, right } = &mut *pair;
    let left_fut = async {
        let r = bounded_submit(left.invoke_action(left_spec), timeout).await;
        (r, clock.now().0)
    };
    let right_fut = async {
        let r = bounded_submit(right.invoke_action(right_spec), timeout).await;
        (r, clock.now().0)
    };
    let ((left_res, left_at), (right_res, right_at)) = tokio::join!(left_fut, right_fut);
    match (left_res, right_res) {
        (Ok(left_id), Ok(right_id)) => Ok(PairSubmit {
            left: SubmitAck {
                side: Side::Left,
                action_id: left_id,
                acked_at_ns: left_at,
            },
            right: SubmitAck {
                side: Side::Right,
                action_id: right_id,
                acked_at_ns: right_at,
            },
        }),
        (Err(l), Err(r)) => Err(both_failed(pair, l, r, cfg).await),
        (Ok(id), Err(e)) => Err(one_failed(pair, Side::Left, id, Side::Right, e, clock, cfg).await),
        (Err(e), Ok(id)) => Err(one_failed(pair, Side::Right, id, Side::Left, e, clock, cfg).await),
    }
}

/// 两侧提交都失败：都是明确拒绝 → `BothRejected`（无副作用）；否则
/// **结果不确定** → 无已知 action 可 cancel，直接双 EStop。
async fn both_failed<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    left: SubmitFailure,
    right: SubmitFailure,
    cfg: &PairConfig,
) -> PairError {
    if left.is_definite() && right.is_definite() {
        return PairError::BothRejected {
            left: left.into_error(),
            right: right.into_error(),
        };
    }
    let mut causes = Vec::new();
    let mut rejected = None;
    for (side, f) in [(Side::Left, left), (Side::Right, right)] {
        if f.is_definite() {
            rejected = Some((side, f.into_error()));
        } else {
            causes.push((side, f.into_error()));
        }
    }
    // 无 action_id 可 cancel → **立即**双 EStop（round 3 P0-2：不确定路径的
    // 安全动作不排在任何等待之后）。
    let estop = escalate_double_estop(
        pair,
        "dual_arm_workflow: submit outcome indeterminate (no action id to cancel) → double EStop",
        cfg,
    )
    .await;
    PairError::IndeterminateSubmit {
        causes,
        rejected,
        cleanup: Box::new(PairCleanup {
            cancelled: Vec::new(),
            estop: Some(estop),
            budget_exceeded: false,
        }),
    }
}

/// 一侧接受、另一侧失败：另一侧**明确拒绝** → 提交期部分失败（cancel 已接收
/// 侧）；否则**结果不确定** → cancel 已接收侧 + 双 EStop。
#[allow(clippy::too_many_arguments)]
async fn one_failed<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    accepted_side: Side,
    accepted_id: String,
    failed_side: Side,
    failure: SubmitFailure,
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> PairError {
    let definite = failure.is_definite();
    let err = failure.into_error();
    let known = [(accepted_side, accepted_id.clone())];
    if definite {
        // 可证明失败侧未启动 → 只需收掉已接收侧；不收敛才升级 EStop。
        let cleanup =
            cleanup_within_budget(pair, &known, "submit partial failure", false, clock, cfg).await;
        return match (cleanup.cancelled.first(), cleanup.estop.clone()) {
            (Some(report), None) => PairError::SubmitPartialFailure {
                rejected_side: failed_side,
                rejected: err,
                cancelled: Box::new(report.clone()),
            },
            _ => PairError::CancelNotConverged {
                side: accepted_side,
                action_id: accepted_id,
                timeout: cfg.cancel_timeout,
                estop: Box::new(cleanup.estop.unwrap_or_else(|| EStopReport {
                    reason: "cleanup budget exceeded".into(),
                    left: Err(None),
                    right: Err(None),
                })),
            },
        };
    }
    // 结果不确定：cancel 已知的一侧，并**强制**双 EStop。
    let cleanup = cleanup_within_budget(
        pair,
        &known,
        "submit outcome indeterminate",
        true,
        clock,
        cfg,
    )
    .await;
    PairError::IndeterminateSubmit {
        causes: vec![(failed_side, err)],
        rejected: None,
        cleanup: Box::new(cleanup),
    }
}

// ============================================================================
// cancel_surviving（D5 第 3/4 条；公开入口 = raw + 单次升级）
// ============================================================================

/// cancel 指定侧并等其终态；**cancel / status RPC 失败或不收敛都升级为双侧
/// EStop**（安全方向，恰一次）。
///
/// # Errors
///
/// [`PairError::Rpc`] / [`PairError::RpcTimeout`] / [`PairError::CancelNotConverged`]
/// ——三者都已完成 EStop 收尾。
pub async fn cancel_surviving<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    side: Side,
    action_id: &str,
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> Result<TerminalReport, PairError> {
    match raw_cancel_side(pair, side, action_id, clock, cfg).await {
        Ok(Some(report)) => Ok(report),
        Ok(None) => {
            let reason = format!(
                "dual_arm_workflow: cancel of {side} action {action_id} did not converge within {:?}",
                cfg.cancel_timeout
            );
            let estop = escalate_double_estop(pair, &reason, cfg).await;
            Err(PairError::CancelNotConverged {
                side,
                action_id: action_id.to_string(),
                timeout: cfg.cancel_timeout,
                estop: Box::new(estop),
            })
        }
        Err(a) => {
            let reason = format!("dual_arm_workflow: {} {} rpc anomaly", a.side, a.phase);
            let estop = escalate_double_estop(pair, &reason, cfg).await;
            Err(anomaly_to_error(a, cfg, estop))
        }
    }
}

// ============================================================================
// await_pair_terminal（D5 第 2/4/5 条）
// ============================================================================

/// 主循环的退出形态（**不含**任何收尾副作用——收尾统一由
/// [`await_pair_terminal`] 在独立预算内做）。
enum LoopExit {
    Done(PairResult),
    /// `terminal_timeout` 内未双终态。
    Converge {
        last: [TaskPhase; 2],
        pending: Vec<Side>,
    },
    /// 环内 cancel 幸存侧未收敛。
    CancelStuck {
        side: Side,
        action_id: String,
    },
    /// 环内 RPC 异常（raw，未升级）。
    Anomaly(RpcAnomaly),
}

/// 轮询两侧 `GetTaskStatus` 直到**双终态**（D5 第 5 条：stream 断开 / 事件
/// 超时的收敛通路；本模块全程只用轮询，不依赖 stream）。
///
/// 执行期不对称（D5 第 4 条）：一侧先到 `Failed` / `Cancelled`、另一侧仍非
/// 终态 → **立即 cancel 幸存侧**；一侧已 `Completed(Success)` → **不 cancel**，
/// 继续等另一侧自然收敛。
///
/// **有界性**（round 2 阻断 2）：每次 RPC 带 `rpc_timeout`；主循环整体带
/// [`PairConfig::phase_budget`]；**收尾在独立的
/// [`PairConfig::cleanup_budget`] 内执行**，故墙钟上限 = 两段预算之和。
///
/// # Errors
///
/// 见 [`PairError`]。
pub async fn await_pair_terminal<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    submit: &PairSubmit,
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> Result<PairResult, PairError> {
    let budget = cfg.phase_budget(cfg.terminal_timeout);
    let exit = match tokio::time::timeout(budget, terminal_loop(pair, submit, clock, cfg)).await {
        Ok(e) => e,
        Err(_elapsed) => {
            let known = known_actions(submit, &[Side::Left, Side::Right]);
            let cleanup =
                cleanup_within_budget(pair, &known, "phase budget exceeded", true, clock, cfg)
                    .await;
            return Err(PairError::PhaseBudgetExceeded {
                phase: "await_pair_terminal",
                budget,
                cleanup: Box::new(cleanup),
            });
        }
    };
    match exit {
        LoopExit::Done(result) => Ok(result),
        LoopExit::Converge { last, pending } => {
            let known = known_actions(submit, &pending);
            let cleanup = cleanup_within_budget(
                pair,
                &known,
                "terminal convergence timeout",
                false,
                clock,
                cfg,
            )
            .await;
            Err(PairError::ConvergenceTimeout {
                timeout: cfg.terminal_timeout,
                left: last[0],
                right: last[1],
                cleanup: Box::new(cleanup),
            })
        }
        LoopExit::CancelStuck { side, action_id } => {
            let reason = format!(
                "dual_arm_workflow: cancel of {side} action {action_id} did not converge within {:?}",
                cfg.cancel_timeout
            );
            let estop = escalate_double_estop(pair, &reason, cfg).await;
            Err(PairError::CancelNotConverged {
                side,
                action_id,
                timeout: cfg.cancel_timeout,
                estop: Box::new(estop),
            })
        }
        LoopExit::Anomaly(a) => {
            let reason = format!("dual_arm_workflow: {} {} rpc anomaly", a.side, a.phase);
            let estop = escalate_double_estop(pair, &reason, cfg).await;
            Err(anomaly_to_error(a, cfg, estop))
        }
    }
}

/// [`await_pair_terminal`] 的主循环（外层已加阶段预算；**无收尾副作用**）。
async fn terminal_loop<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    submit: &PairSubmit,
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> LoopExit {
    let deadline_ns = clock
        .now()
        .0
        .saturating_add(duration_to_ns(cfg.terminal_timeout));
    let mut done: [Option<TerminalReport>; 2] = [None, None];
    let mut last = [TaskPhase::Unspecified; 2];

    loop {
        for (idx, side) in [Side::Left, Side::Right].into_iter().enumerate() {
            if done[idx].is_none() {
                match raw_poll_once(pair, side, submit.action_id(side), clock, cfg).await {
                    Ok((phase, report)) => {
                        last[idx] = phase;
                        done[idx] = report;
                    }
                    Err(a) => return LoopExit::Anomaly(a),
                }
            }
        }
        if let (Some(l), Some(r)) = (done[0].as_ref(), done[1].as_ref()) {
            return LoopExit::Done(PairResult::classify(l.clone(), r.clone()));
        }
        let asymmetric = match (done[0].as_ref(), done[1].as_ref()) {
            (Some(first), None) => Some((first.clone(), Side::Right)),
            (None, Some(first)) => Some((first.clone(), Side::Left)),
            _ => None,
        };
        if let Some((first, pending_side)) = asymmetric
            && !first.is_success()
        {
            // 非成功侧先到 → cancel 幸存侧收尾（成功侧先到则不 cancel，
            // 继续等另一侧自然收敛，D5 第 4 条）。
            let pending_id = submit.action_id(pending_side).to_string();
            return match raw_cancel_side(pair, pending_side, &pending_id, clock, cfg).await {
                Ok(Some(other)) => {
                    let (l, r) = match first.side {
                        Side::Left => (first, other),
                        Side::Right => (other, first),
                    };
                    LoopExit::Done(PairResult::classify(l, r))
                }
                Ok(None) => LoopExit::CancelStuck {
                    side: pending_side,
                    action_id: pending_id,
                },
                Err(a) => LoopExit::Anomaly(a),
            };
        }
        if clock.now().0 >= deadline_ns {
            let pending: Vec<Side> = [Side::Left, Side::Right]
                .into_iter()
                .enumerate()
                .filter(|(i, _)| done[*i].is_none())
                .map(|(_, s)| s)
                .collect();
            return LoopExit::Converge { last, pending };
        }
        tokio::time::sleep(cfg.poll_interval).await;
    }
}

fn duration_to_ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

// ============================================================================
// Tests（脚本化 mock 端点：协议分支 + 有界性 + 收尾语义/顺序全覆盖）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use robot_core::time::{MonoRawNs, RobotClock};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    /// 单调递增的测试时钟：每次 `now()` 前进固定步长。
    #[derive(Debug)]
    struct StepClock {
        ns: AtomicU64,
        step_ns: u64,
    }

    impl StepClock {
        fn new(step_ns: u64) -> Self {
            Self {
                ns: AtomicU64::new(0),
                step_ns,
            }
        }
    }

    impl RobotClock for StepClock {
        fn now(&self) -> MonoRawNs {
            MonoRawNs(self.ns.fetch_add(self.step_ns, Ordering::Relaxed))
        }
        fn sleep_until(&self, _deadline: robot_core::time::Deadline) {}
        fn sleep_for(&self, _d: robot_core::time::DurationNs) {}
    }

    /// 端点方法的脚本化行为。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Behavior {
        Normal,
        /// 永不返回（RPC 挂死）。
        Pending,
        /// 指定 gRPC code 的 status 失败。
        Status(&'static str),
        /// transport 失败。
        TransportFail,
    }

    /// 两侧共享的**有序事件日志**（round 3 P0-2：断言 EStop 早于 cancel 收敛）。
    type EventLog = Arc<Mutex<Vec<String>>>;

    struct MockArm {
        side: Side,
        invoke: Behavior,
        status: Behavior,
        cancel: Behavior,
        estop: Behavior,
        statuses: Mutex<VecDeque<TaskSnapshot>>,
        after_cancel: Mutex<Option<VecDeque<TaskSnapshot>>>,
        last_spec: Mutex<Option<ActionSpec>>,
        cancel_calls: Arc<AtomicU64>,
        estop_calls: Arc<AtomicU64>,
        events: EventLog,
    }

    impl MockArm {
        fn new(side: Side) -> Self {
            Self::with_log(side, Arc::new(Mutex::new(Vec::new())))
        }

        fn with_log(side: Side, events: EventLog) -> Self {
            Self {
                side,
                invoke: Behavior::Normal,
                status: Behavior::Normal,
                cancel: Behavior::Normal,
                estop: Behavior::Normal,
                statuses: Mutex::new(VecDeque::new()),
                after_cancel: Mutex::new(None),
                last_spec: Mutex::new(None),
                cancel_calls: Arc::new(AtomicU64::new(0)),
                estop_calls: Arc::new(AtomicU64::new(0)),
                events,
            }
        }

        fn invoke(mut self, b: Behavior) -> Self {
            self.invoke = b;
            self
        }

        fn status_behavior(mut self, b: Behavior) -> Self {
            self.status = b;
            self
        }

        fn cancel_behavior(mut self, b: Behavior) -> Self {
            self.cancel = b;
            self
        }

        fn estop_behavior(mut self, b: Behavior) -> Self {
            self.estop = b;
            self
        }

        fn with_status(self, s: TaskSnapshot) -> Self {
            self.statuses.lock().expect("lock").push_back(s);
            self
        }

        fn with_after_cancel(self, s: TaskSnapshot) -> Self {
            self.after_cancel
                .lock()
                .expect("lock")
                .get_or_insert_with(VecDeque::new)
                .push_back(s);
            self
        }

        fn cancels(&self) -> u64 {
            self.cancel_calls.load(Ordering::Relaxed)
        }

        fn estops(&self) -> u64 {
            self.estop_calls.load(Ordering::Relaxed)
        }

        fn last_kind(&self) -> Option<ActionKindWire> {
            self.last_spec
                .lock()
                .expect("lock")
                .as_ref()
                .map(|s| s.kind)
        }

        fn log(&self, what: &str) {
            self.events
                .lock()
                .expect("lock")
                .push(format!("{}:{what}", self.side.label()));
        }
    }

    /// 非 invoke 方法的行为应用（失败域 = 通用 `ArmRpcError`）。
    async fn apply<T>(b: Behavior, ok: T) -> Result<T, ArmRpcError> {
        tokio::task::yield_now().await;
        match b {
            Behavior::Normal => Ok(ok),
            Behavior::Pending => {
                std::future::pending::<()>().await;
                unreachable!()
            }
            Behavior::Status(code) => Err(ArmRpcError::Status {
                code: code.to_string(),
                message: format!("mock {code}"),
            }),
            Behavior::TransportFail => Err(ArmRpcError::Transport("connection reset".into())),
        }
    }

    impl ArmEndpoint for MockArm {
        fn side(&self) -> Side {
            self.side
        }

        async fn invoke_action(&mut self, spec: ActionSpec) -> Result<String, SubmitFailure> {
            tokio::task::yield_now().await;
            self.log("invoke");
            *self.last_spec.lock().expect("lock") = Some(spec);
            match self.invoke {
                Behavior::Normal => Ok(format!("{}-action", self.side.label())),
                Behavior::Pending => {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
                // 端点侧按契约白名单分类（与 `TonicArm` 同款）。
                Behavior::Status(code) => Err(SubmitFailure::from_status_code(
                    code,
                    ArmRpcError::Status {
                        code: code.to_string(),
                        message: format!("mock {code}"),
                    },
                )),
                Behavior::TransportFail => Err(SubmitFailure::from_transport(
                    ArmRpcError::Transport("connection reset".into()),
                )),
            }
        }

        async fn cancel_task(&mut self, _action_id: &str) -> Result<(), ArmRpcError> {
            self.cancel_calls.fetch_add(1, Ordering::Relaxed);
            self.log("cancel");
            apply(self.cancel, ()).await?;
            let replacement = self.after_cancel.lock().expect("lock").take();
            if let Some(seq) = replacement {
                *self.statuses.lock().expect("lock") = seq;
            }
            Ok(())
        }

        async fn task_status(&mut self, _action_id: &str) -> Result<TaskSnapshot, ArmRpcError> {
            let next = {
                let mut q = self.statuses.lock().expect("lock");
                if q.len() > 1 {
                    q.pop_front()
                } else {
                    q.front().cloned()
                }
            };
            apply(
                self.status,
                next.unwrap_or_else(|| snap(TaskPhase::Executing)),
            )
            .await
        }

        async fn estop(&mut self, _reason: &str) -> Result<(), ArmRpcError> {
            self.estop_calls.fetch_add(1, Ordering::Relaxed);
            self.log("estop");
            apply(self.estop, ()).await
        }
    }

    fn snap(phase: TaskPhase) -> TaskSnapshot {
        TaskSnapshot {
            phase,
            progress: 0.0,
            outcome: OutcomeKind::Unspecified,
            outcome_warning: String::new(),
            failure: FailureKind::Unspecified,
            failure_reason: String::new(),
        }
    }

    fn completed_success() -> TaskSnapshot {
        TaskSnapshot {
            outcome: OutcomeKind::Success,
            ..snap(TaskPhase::Terminal(TerminalState::Completed))
        }
    }

    fn failed_internal() -> TaskSnapshot {
        TaskSnapshot {
            failure: FailureKind::InternalError,
            failure_reason: "fault_inject".into(),
            ..snap(TaskPhase::Terminal(TerminalState::Failed))
        }
    }

    fn cancelled() -> TaskSnapshot {
        snap(TaskPhase::Terminal(TerminalState::Cancelled))
    }

    /// 快窗口配置（真实时间；各窗 ≤ 100 ms）。
    fn fast_cfg() -> PairConfig {
        PairConfig {
            ack_skew_limit: Duration::from_millis(250),
            terminal_timeout: Duration::from_millis(200),
            cancel_timeout: Duration::from_millis(100),
            poll_interval: Duration::from_millis(1),
            rpc_timeout: Duration::from_millis(80),
            estop_timeout: Duration::from_millis(50),
            // ≥ 2×(80+100)+50 = 410 ms。
            cleanup_budget: Duration::from_millis(500),
        }
    }

    fn scripted() -> ActionSpec {
        ActionSpec::scripted(vec![1, 2, 3])
    }

    // ---- 协议主干 ----

    #[tokio::test]
    async fn both_accept_then_both_success_is_pair_success() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left).with_status(completed_success()),
            right: MockArm::new(Side::Right).with_status(completed_success()),
        };
        let run = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &fast_cfg())
            .await
            .expect("pair success");
        assert!(run.result.is_pair_success(), "{:?}", run.result);
        assert_eq!(pair.left.last_kind(), Some(ActionKindWire::Scripted));
        assert_eq!(pair.left.estops(), 0);
    }

    #[tokio::test]
    async fn dwell_spec_is_explicit_at_call_site() {
        let spec = ActionSpec::dwell_no_payload();
        assert_eq!(spec.kind, ActionKindWire::DwellMs);
        assert_eq!(spec.payload_schema_id, NO_PAYLOAD_SCHEMA_ID);
        assert_eq!(ActionKindWire::DwellMs.proto_value(), 1);
        assert_eq!(ActionKindWire::Scripted.proto_value(), 3);

        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left).with_status(failed_internal()),
            right: MockArm::new(Side::Right)
                .with_status(snap(TaskPhase::Executing))
                .with_after_cancel(cancelled()),
        };
        let run = run_chunk_pair(
            &mut pair,
            ActionSpec::dwell_no_payload(),
            scripted(),
            &clock,
            &fast_cfg(),
        )
        .await;
        assert!(run.is_ok(), "{run:?}");
        assert_eq!(pair.left.last_kind(), Some(ActionKindWire::DwellMs));
    }

    // ---- round 3 P0-1：invoke 失败域分类 ----

    #[test]
    fn submit_failure_classification_follows_contract_allowlist() {
        let err = || ArmRpcError::Transport("x".into());
        // 白名单内 = 明确拒绝（可证明未启动）。
        for code in DEFINITE_REJECTION_CODES {
            assert!(
                SubmitFailure::from_status_code(code, err()).is_definite(),
                "{code} 应在白名单内"
            );
        }
        // reviewer 点名的五种 code 必须是**不确定**（gateway 可能已 forward）。
        for code in [
            "DeadlineExceeded",
            "Cancelled",
            "Unavailable",
            "Internal",
            "Unknown",
        ] {
            assert!(
                !SubmitFailure::from_status_code(code, err()).is_definite(),
                "{code} 必须按不确定处理"
            );
        }
        // 幂等重放：同 key 的 action 可能正在跑 → 不确定。
        assert!(!SubmitFailure::from_status_code("AlreadyExists", err()).is_definite());
        // transport 恒不确定。
        assert!(!SubmitFailure::from_transport(err()).is_definite());
        assert_eq!(
            ArmRpcError::Status {
                code: "Internal".into(),
                message: String::new()
            }
            .code(),
            Some("Internal")
        );
        assert!(err().code().is_none());
    }

    /// 五种"可能发生在 forward 之后"的 code 都必须走
    /// [`PairError::IndeterminateSubmit`]（round 3 P0-1 点名）。
    #[tokio::test]
    async fn post_forward_status_codes_are_indeterminate_submits() {
        for code in [
            "DeadlineExceeded",
            "Cancelled",
            "Unavailable",
            "Internal",
            "Unknown",
        ] {
            let clock = StepClock::new(1_000_000);
            let mut pair = ArmPair {
                left: MockArm::new(Side::Left)
                    .with_status(snap(TaskPhase::Executing))
                    .with_after_cancel(cancelled()),
                right: MockArm::new(Side::Right).invoke(Behavior::Status(code)),
            };
            let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &fast_cfg())
                .await
                .expect_err("post-forward code 必须视为不确定");
            match err {
                PairError::IndeterminateSubmit { ref causes, .. } => {
                    assert_eq!(causes.len(), 1, "{code}");
                    assert_eq!(causes[0].0, Side::Right, "{code}");
                }
                other => panic!("{code}: expected IndeterminateSubmit, got {other:?}"),
            }
            assert_eq!(pair.left.estops(), 1, "{code}: 必须双 EStop");
            assert_eq!(pair.right.estops(), 1, "{code}");
        }
    }

    #[tokio::test]
    async fn both_definitely_rejected_needs_no_cleanup() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left).invoke(Behavior::Status("PermissionDenied")),
            right: MockArm::new(Side::Right).invoke(Behavior::Status("InvalidArgument")),
        };
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &fast_cfg())
            .await
            .expect_err("双明确拒绝必 FAIL");
        assert!(matches!(err, PairError::BothRejected { .. }), "{err:?}");
        assert_eq!(pair.left.cancels(), 0);
        assert_eq!(pair.left.estops(), 0);
        assert_eq!(pair.right.estops(), 0);
    }

    #[tokio::test]
    async fn both_pending_invokes_are_indeterminate_and_estop() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left).invoke(Behavior::Pending),
            right: MockArm::new(Side::Right).invoke(Behavior::Pending),
        };
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &fast_cfg())
            .await
            .expect_err("双超时 = 结果不确定");
        match err {
            PairError::IndeterminateSubmit {
                ref causes,
                ref rejected,
                ref cleanup,
            } => {
                assert_eq!(causes.len(), 2);
                assert!(rejected.is_none());
                assert!(cleanup.estop.as_ref().expect("EStop").both_acked());
            }
            other => panic!("expected IndeterminateSubmit, got {other:?}"),
        }
        assert_eq!(pair.left.cancels(), 0);
        assert_eq!(pair.left.estops(), 1);
    }

    #[tokio::test]
    async fn transport_failure_is_indeterminate_not_rejection() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left)
                .with_status(snap(TaskPhase::Executing))
                .with_after_cancel(cancelled()),
            right: MockArm::new(Side::Right).invoke(Behavior::TransportFail),
        };
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &fast_cfg())
            .await
            .expect_err("transport 失败 = 不确定");
        match err {
            PairError::IndeterminateSubmit {
                ref causes,
                ref cleanup,
                ..
            } => {
                assert!(matches!(causes[0].1, ArmRpcError::Transport(_)));
                assert_eq!(cleanup.cancelled.len(), 1);
                assert!(cleanup.estop.is_some());
            }
            other => panic!("expected IndeterminateSubmit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn accepted_plus_definite_rejection_is_partial_failure_without_estop() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left)
                .with_status(snap(TaskPhase::Executing))
                .with_after_cancel(cancelled()),
            right: MockArm::new(Side::Right).invoke(Behavior::Status("FailedPrecondition")),
        };
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &fast_cfg())
            .await
            .expect_err("一收一拒");
        match err {
            PairError::SubmitPartialFailure {
                rejected_side,
                ref cancelled,
                ..
            } => {
                assert_eq!(rejected_side, Side::Right);
                assert_eq!(cancelled.state, TerminalState::Cancelled);
            }
            other => panic!("expected SubmitPartialFailure, got {other:?}"),
        }
        assert_eq!(pair.left.estops(), 0, "可证明未启动 → 不必 EStop");
    }

    // ---- round 3 P0-2：EStop 先行 + 预算切分 ----

    /// accepted + indeterminate + **cancel 挂死**：EStop 必须在 cancel 收敛
    /// **之前**就各执行一次（安全动作不排在等待之后）。
    #[tokio::test]
    async fn indeterminate_submit_estops_before_cancel_convergence() {
        let events: EventLog = Arc::new(Mutex::new(Vec::new()));
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::with_log(Side::Left, Arc::clone(&events))
                .with_status(snap(TaskPhase::Executing))
                .cancel_behavior(Behavior::Pending),
            right: MockArm::with_log(Side::Right, Arc::clone(&events)).invoke(Behavior::Pending),
        };
        let cfg = fast_cfg();
        let t0 = Instant::now();
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &cfg)
            .await
            .expect_err("不确定提交");
        let elapsed = t0.elapsed();
        match err {
            PairError::IndeterminateSubmit { ref cleanup, .. } => {
                assert!(cleanup.estop.is_some(), "必须已 EStop");
                assert!(cleanup.cancelled.is_empty(), "cancel 挂死 → 无收敛终态");
            }
            other => panic!("expected IndeterminateSubmit, got {other:?}"),
        }
        assert_eq!(pair.left.estops(), 1, "恰一次");
        assert_eq!(pair.right.estops(), 1, "恰一次");
        // **顺序**：两侧 estop 都早于 left 的 cancel。
        let log = events.lock().expect("lock").clone();
        let first_estop = log
            .iter()
            .position(|e| e.ends_with(":estop"))
            .expect("estop");
        let first_cancel = log.iter().position(|e| e.ends_with(":cancel"));
        if let Some(c) = first_cancel {
            assert!(first_estop < c, "EStop 必须先于 cancel，实际日志 = {log:?}");
        }
        assert!(
            elapsed <= cfg.run_chunk_bound(),
            "墙钟 {elapsed:?} ≤ {:?}",
            cfg.run_chunk_bound()
        );
    }

    #[tokio::test]
    async fn accepted_plus_pending_cancels_known_and_estops() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left)
                .with_status(snap(TaskPhase::Executing))
                .with_after_cancel(cancelled()),
            right: MockArm::new(Side::Right).invoke(Behavior::Pending),
        };
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &fast_cfg())
            .await
            .expect_err("一侧不确定");
        match err {
            PairError::IndeterminateSubmit {
                ref causes,
                ref cleanup,
                ..
            } => {
                assert_eq!(causes.len(), 1);
                assert_eq!(cleanup.cancelled.len(), 1, "已知 action 必须被 cancel");
                assert!(cleanup.estop.is_some());
            }
            other => panic!("expected IndeterminateSubmit, got {other:?}"),
        }
        assert_eq!(pair.left.cancels(), 1);
        assert_eq!(pair.left.estops(), 1);
    }

    #[tokio::test]
    async fn invalid_config_is_rejected_before_any_invoke() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left),
            right: MockArm::new(Side::Right),
        };
        // 收尾预算不足 → 必须在**任何 invoke 之前**拒绝。
        let bad = PairConfig {
            cleanup_budget: Duration::from_millis(10),
            ..fast_cfg()
        };
        assert!(bad.validate().is_err());
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &bad)
            .await
            .expect_err("非法配置");
        assert!(matches!(err, PairError::InvalidConfig(_)), "{err:?}");
        assert_eq!(pair.left.last_kind(), None, "不得发出任何 invoke");
        assert_eq!(pair.right.last_kind(), None);
    }

    // ---- 执行期 ----

    #[tokio::test]
    async fn exec_phase_failure_cancels_surviving_side() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left).with_status(failed_internal()),
            right: MockArm::new(Side::Right)
                .with_status(snap(TaskPhase::Executing))
                .with_after_cancel(cancelled()),
        };
        let run = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &fast_cfg())
            .await
            .expect("converges");
        match run.result {
            PairResult::BothUnsuccessful { left, right } => {
                assert_eq!(left.state, TerminalState::Failed);
                assert_eq!(right.state, TerminalState::Cancelled);
            }
            other => panic!("expected BothUnsuccessful, got {other:?}"),
        }
        assert_eq!(pair.right.cancels(), 1);
        assert_eq!(pair.right.estops(), 0);
    }

    #[tokio::test]
    async fn success_then_other_fails_is_partial_completion_without_cancel() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left).with_status(completed_success()),
            right: MockArm::new(Side::Right)
                .with_status(snap(TaskPhase::Executing))
                .with_status(failed_internal()),
        };
        let run = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &fast_cfg())
            .await
            .expect("converges");
        match &run.result {
            PairResult::PartialCompletion { succeeded, other } => {
                assert_eq!(succeeded.side, Side::Left);
                assert_eq!(other.state, TerminalState::Failed);
            }
            other => panic!("expected PartialCompletion, got {other:?}"),
        }
        assert_eq!(pair.right.cancels(), 0, "成功侧先到不得 cancel 幸存侧");
    }

    // ---- 有界 + 恰一次升级 ----

    #[tokio::test]
    async fn pending_status_rpc_is_bounded_and_escalates_exactly_once() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left).status_behavior(Behavior::Pending),
            right: MockArm::new(Side::Right).with_status(completed_success()),
        };
        let cfg = fast_cfg();
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &cfg)
            .await
            .expect_err("挂死 RPC 必须被 rpc_timeout 截断");
        match err {
            PairError::RpcTimeout {
                side,
                phase,
                timeout,
                ref estop,
            } => {
                assert_eq!(side, Side::Left);
                assert_eq!(phase, "poll");
                assert_eq!(timeout, cfg.rpc_timeout);
                assert!(estop.both_acked());
            }
            other => panic!("expected RpcTimeout, got {other:?}"),
        }
        assert_eq!(pair.left.estops(), 1);
        assert_eq!(pair.right.estops(), 1);
    }

    #[tokio::test]
    async fn status_rpc_failure_escalates_double_estop_exactly_once() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left).status_behavior(Behavior::Status("Unavailable")),
            right: MockArm::new(Side::Right).with_status(completed_success()),
        };
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &fast_cfg())
            .await
            .expect_err("status RPC 失败");
        match err {
            PairError::Rpc {
                side,
                phase,
                ref estop,
                ..
            } => {
                assert_eq!(side, Side::Left);
                assert_eq!(phase, "poll");
                assert!(estop.both_acked());
            }
            other => panic!("expected Rpc, got {other:?}"),
        }
        assert_eq!(pair.left.estops(), 1);
        assert_eq!(pair.right.estops(), 1);
    }

    #[tokio::test]
    async fn convergence_timeout_cleanup_escalates_exactly_once() {
        let clock = StepClock::new(10_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left).with_status(snap(TaskPhase::Executing)),
            right: MockArm::new(Side::Right).with_status(snap(TaskPhase::Executing)),
        };
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &fast_cfg())
            .await
            .expect_err("永不终态且 cancel 无效");
        match err {
            PairError::ConvergenceTimeout { ref cleanup, .. } => {
                assert!(cleanup.cancelled.is_empty());
                assert!(cleanup.estop.as_ref().expect("EStop").both_acked());
                assert!(!cleanup.budget_exceeded);
            }
            other => panic!("expected ConvergenceTimeout, got {other:?}"),
        }
        assert_eq!(pair.left.estops(), 1, "恰一次");
        assert_eq!(pair.right.estops(), 1, "恰一次");
    }

    #[tokio::test]
    async fn convergence_timeout_cancels_pending_sides() {
        let clock = StepClock::new(10_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left)
                .with_status(snap(TaskPhase::Executing))
                .with_after_cancel(cancelled()),
            right: MockArm::new(Side::Right)
                .with_status(snap(TaskPhase::Pending))
                .with_after_cancel(cancelled()),
        };
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &fast_cfg())
            .await
            .expect_err("永不终态");
        match err {
            PairError::ConvergenceTimeout { ref cleanup, .. } => {
                assert_eq!(cleanup.cancelled.len(), 2);
                assert!(cleanup.is_clean());
            }
            other => panic!("expected ConvergenceTimeout, got {other:?}"),
        }
        assert_eq!(pair.left.estops(), 0);
    }

    #[tokio::test]
    async fn ack_skew_violation_cancels_both_sides() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left)
                .with_status(snap(TaskPhase::Executing))
                .with_after_cancel(cancelled()),
            right: MockArm::new(Side::Right)
                .with_status(snap(TaskPhase::Executing))
                .with_after_cancel(cancelled()),
        };
        let cfg = PairConfig {
            ack_skew_limit: Duration::ZERO,
            ..fast_cfg()
        };
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &cfg)
            .await
            .expect_err("偏差超限");
        match err {
            PairError::AckSkewExceeded { ref cleanup, .. } => {
                assert_eq!(cleanup.cancelled.len(), 2);
                assert!(cleanup.is_clean());
            }
            other => panic!("expected AckSkewExceeded, got {other:?}"),
        }
        assert_eq!(pair.left.estops(), 0);
    }

    // ---- 墙钟上限 ----

    #[tokio::test]
    async fn wall_clock_stays_within_two_budgets_when_everything_hangs() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left)
                .status_behavior(Behavior::Pending)
                .cancel_behavior(Behavior::Pending)
                .estop_behavior(Behavior::Pending),
            right: MockArm::new(Side::Right)
                .status_behavior(Behavior::Pending)
                .cancel_behavior(Behavior::Pending)
                .estop_behavior(Behavior::Pending),
        };
        let cfg = fast_cfg();
        let t0 = Instant::now();
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &cfg)
            .await
            .expect_err("全挂死");
        let elapsed = t0.elapsed();
        assert!(
            matches!(err, PairError::RpcTimeout { .. }),
            "expected RpcTimeout, got {err:?}"
        );
        assert!(
            elapsed <= cfg.run_chunk_bound(),
            "墙钟 {elapsed:?} ≤ {:?}",
            cfg.run_chunk_bound()
        );
    }

    #[tokio::test]
    async fn wall_clock_bounded_when_cancel_hangs_during_cleanup() {
        let clock = StepClock::new(10_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left)
                .with_status(snap(TaskPhase::Executing))
                .cancel_behavior(Behavior::Pending),
            right: MockArm::new(Side::Right)
                .with_status(snap(TaskPhase::Executing))
                .cancel_behavior(Behavior::Pending),
        };
        let cfg = fast_cfg();
        let t0 = Instant::now();
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &cfg)
            .await
            .expect_err("cancel 挂死");
        let elapsed = t0.elapsed();
        match err {
            PairError::ConvergenceTimeout { ref cleanup, .. } => {
                assert!(cleanup.estop.is_some(), "cancel 挂死必须升级 EStop");
                assert!(cleanup.cancelled.is_empty());
            }
            other => panic!("expected ConvergenceTimeout, got {other:?}"),
        }
        assert!(
            elapsed <= cfg.run_chunk_bound(),
            "墙钟 {elapsed:?} ≤ {:?}",
            cfg.run_chunk_bound()
        );
        assert_eq!(pair.left.estops(), 1);
    }

    // ---- 判据 / 配置 / 映射 ----

    #[test]
    fn ack_skew_check_is_pure_and_returns_violation_type() {
        let cfg = PairConfig::default();
        let mk = |right_ns: u64| PairSubmit {
            left: SubmitAck {
                side: Side::Left,
                action_id: "l".into(),
                acked_at_ns: 0,
            },
            right: SubmitAck {
                side: Side::Right,
                action_id: "r".into(),
                acked_at_ns: right_ns,
            },
        };
        let over = mk(300_000_000);
        assert_eq!(over.ack_skew(), Duration::from_millis(300));
        let v: AckSkewViolation = over.check_ack_skew(&cfg).expect_err("超限必拒");
        assert_eq!(v.skew, Duration::from_millis(300));
        mk(duration_to_ns(cfg.ack_skew_limit))
            .check_ack_skew(&cfg)
            .expect("恰等于上限应通过");
        assert_eq!(over.action_id(Side::Left), "l");
    }

    #[test]
    fn budgets_are_separate_sufficient_and_validated() {
        let cfg = PairConfig::default();
        cfg.validate().expect("默认配置必须自洽");
        assert!(cfg.cleanup_budget_is_sufficient());
        // cancel 阶段预算 = 收尾预算 − EStop 预留（EStop 预算不可被吃掉）。
        assert_eq!(
            cfg.cancel_phase_budget(),
            cfg.cleanup_budget.saturating_sub(cfg.estop_timeout)
        );
        let phase = cfg.phase_budget(cfg.terminal_timeout);
        assert_eq!(
            cfg.run_chunk_bound(),
            cfg.rpc_timeout + cfg.cleanup_budget * 2 + phase
        );
        fast_cfg().validate().expect("测试配置自洽");
    }

    #[test]
    fn terminal_report_success_requires_completed_and_success_outcome() {
        let base = TerminalReport {
            side: Side::Left,
            action_id: "a".into(),
            state: TerminalState::Completed,
            outcome: OutcomeKind::Success,
            outcome_warning: String::new(),
            failure: FailureKind::Unspecified,
            failure_reason: String::new(),
            observed_at_ns: 0,
        };
        assert!(base.is_success());
        let partial = TerminalReport {
            outcome: OutcomeKind::PartialSuccess,
            outcome_warning: "cancelled".into(),
            ..base.clone()
        };
        assert!(!partial.is_success());
        let failed = TerminalReport {
            state: TerminalState::Failed,
            ..base
        };
        assert!(!failed.is_success());
    }

    #[test]
    fn snapshot_from_proto_maps_states_and_unknown_is_not_terminal() {
        use crate::proto::{ActionFailureProto, ActionOutcomeProto, TaskState, TaskStatus};
        let st = TaskStatus {
            request_id: "r".into(),
            action_id: "a".into(),
            state: TaskState::Completed as i32,
            progress: 1.0,
            outcome: Some(ActionOutcomeProto {
                kind: crate::proto::action_outcome_proto::Kind::Success as i32,
                warning: String::new(),
            }),
            failure: None,
        };
        let s = TaskSnapshot::from_proto(&st);
        assert_eq!(s.phase, TaskPhase::Terminal(TerminalState::Completed));
        assert_eq!(s.outcome, OutcomeKind::Success);

        let unknown = TaskStatus {
            state: 99,
            outcome: None,
            failure: Some(ActionFailureProto {
                kind: 42,
                reason: "weird".into(),
            }),
            ..st
        };
        let s = TaskSnapshot::from_proto(&unknown);
        assert_eq!(s.phase, TaskPhase::Unspecified, "未知 state 不得当终态");
        assert_eq!(s.failure_reason, "weird");
    }

    #[test]
    fn side_helpers() {
        assert_eq!(Side::Left.other(), Side::Right);
        assert_eq!(Side::Left.to_string(), "left");
        assert_eq!(Side::Right.label(), "right");
    }
}
