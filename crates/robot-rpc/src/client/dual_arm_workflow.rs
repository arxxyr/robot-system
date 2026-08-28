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
//! ## 时间源
//!
//! 所有时刻取自调用方传入的 [`RobotClock`]（业务代码禁 `Instant::now()`，
//! 全局 §"业务代码禁用的 API"）；超时窗口用 `tokio::time` 驱动。

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
    /// 连接 / 传输层失败。
    #[error("transport: {0}")]
    Transport(String),
    /// gRPC status（`code` 用 `tonic::Code` 的 Debug 名，避免本模块 dep tonic 类型）。
    #[error("status {code}: {message}")]
    Status { code: String, message: String },
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

/// **异常收尾报告**（round 1 阻断 2）：任何异常路径返回前，本模块已经把两侧
/// 收拾干净——收敛的侧给终态报告，未收敛 / RPC 失败的侧已触发**双侧 EStop**。
/// 调用方（e2e / 真机 bin）**不需要**再补 catch 逻辑。
#[derive(Debug, Clone, PartialEq)]
pub struct PairCleanup {
    /// 成功 cancel 收敛的侧（0..=2 条）。
    pub cancelled: Vec<TerminalReport>,
    /// 有任一侧未能收敛 / RPC 失败时的双 EStop 记录。
    pub estop: Option<EStopReport>,
}

impl PairCleanup {
    /// 收尾是否"干净"（两侧都 cancel 收敛、未触发 EStop）。
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.estop.is_none()
    }
}

/// 成对提交 / 收敛协议的失败域。**每个变体都已完成安全收尾**（见
/// [`PairCleanup`] / `estop` 字段）。
#[derive(Debug, Clone, Error, PartialEq)]
pub enum PairError {
    /// **双拒绝**（D5 第 6 条）：两侧 `InvokeTask` 都失败 → **不自动重试**，
    /// chunk 判 FAIL。两侧都没接受 action，**无需收尾**。
    #[error("both arms rejected the chunk: left={left}, right={right}")]
    BothRejected {
        left: ArmRpcError,
        right: ArmRpcError,
    },

    /// **提交期部分失败**（D5 第 3 条）：一侧被拒，已接收侧已成功 cancel 收敛。
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

    /// 整体收敛**硬上限**超出（`PairConfig::hard_bound`）——最后一道保险，
    /// 已尽力 cancel + EStop。
    #[error("phase {phase} exceeded hard bound {bound:?}; cleaned up (clean={})", cleanup.is_clean())]
    HardBoundExceeded {
        phase: &'static str,
        bound: Duration,
        cleanup: Box<PairCleanup>,
    },
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
/// **并发形态**：[`submit_pair`] 用 `tokio::join!` 同时 poll 两侧的
/// future——两侧是**不同对象**的可变借用，借用检查天然放行。
pub trait ArmEndpoint: Send {
    /// 本端点属于哪一侧（报告 / 日志用）。
    fn side(&self) -> Side;

    /// 提交一条 action（见 [`ActionSpec`]）；成功返 `action_id`。
    fn invoke_action(
        &mut self,
        spec: ActionSpec,
    ) -> impl Future<Output = Result<String, ArmRpcError>> + Send;

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
// 配置 / 结果类型
// ============================================================================

/// 协议时间窗（AC-E14 定值；e2e 与验收 bin 共用同一默认值）。
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
    /// invoke / cancel / status 调用都包一层，端点实现漏设 transport 超时时
    /// 收敛窗口不会被无限挂起拖垮。
    pub rpc_timeout: Duration,
    /// EStop RPC 硬超时（安全方向也不允许永久挂）。
    pub estop_timeout: Duration,
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
            // gateway RR 默认 timeout + grace 量级；超过即认定该 RPC 挂死。
            rpc_timeout: Duration::from_secs(3),
            estop_timeout: Duration::from_secs(2),
        }
    }
}

impl PairConfig {
    /// 某阶段的**整体硬上限**（最后一道保险）：阶段窗口 + 一次 cancel 收敛 +
    /// 若干次 RPC 超时的余量。任何单点挂死都会先被 `rpc_timeout` 截断，本值
    /// 只在"多点连环慢"时兜底。
    #[must_use]
    pub fn hard_bound(&self, phase_window: Duration) -> Duration {
        phase_window
            .saturating_add(self.cancel_timeout)
            .saturating_add(self.rpc_timeout.saturating_mul(4))
            .saturating_add(self.estop_timeout.saturating_mul(2))
    }
}

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

    /// 纯判据（不做收尾）：偏差是否在上限内（`≤` 语义）。**生产路径请用
    /// [`run_chunk_pair`]**——它在超限时会 cancel 双侧（round 1 阻断 2）。
    ///
    /// # Errors
    ///
    /// 偏差 > `cfg.ack_skew_limit` → [`PairError::AckSkewExceeded`]（`cleanup`
    /// 为空壳，表示"本判据未做收尾"）。
    pub fn check_ack_skew(&self, cfg: &PairConfig) -> Result<(), PairError> {
        let skew = self.ack_skew();
        if skew > cfg.ack_skew_limit {
            return Err(PairError::AckSkewExceeded {
                skew,
                limit: cfg.ack_skew_limit,
                cleanup: Box::new(PairCleanup {
                    cancelled: Vec::new(),
                    estop: None,
                }),
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
// RPC 硬超时原语（round 1 阻断 1）
// ============================================================================

/// 单次 RPC 的失败形态。
enum RpcFailure {
    Failed(ArmRpcError),
    TimedOut,
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

/// RPC 异常 → **双 EStop 收尾** → typed 错误（安全入口，round 1 阻断 2）。
async fn fail_with_estop<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    side: Side,
    phase: &'static str,
    failure: RpcFailure,
    cfg: &PairConfig,
) -> PairError {
    let reason = format!("dual_arm_workflow: {side} {phase} rpc anomaly → double EStop");
    let estop = Box::new(escalate_double_estop(pair, &reason, cfg).await);
    match failure {
        RpcFailure::Failed(source) => PairError::Rpc {
            side,
            phase,
            source,
            estop,
        },
        RpcFailure::TimedOut => PairError::RpcTimeout {
            side,
            phase,
            timeout: cfg.rpc_timeout,
            estop,
        },
    }
}

// ============================================================================
// run_chunk_pair —— **单一安全入口**
// ============================================================================

/// 跑完一对 chunk：提交 → Ack 偏差 gate → 等双终态。**所有异常路径都在本
/// 模块内完成安全收尾**（round 1 阻断 2）：
///
/// | 异常 | 收尾 |
/// |---|---|
/// | 一收一拒 | cancel 已接收侧（不收敛 → 双 EStop） |
/// | 双拒 | 无副作用，直接 FAIL（不重试，D5 第 6 条） |
/// | **Ack 偏差超限** | **cancel 双侧**（不让两个 action 继续跑） |
/// | cancel / status RPC 失败或超时 | **双 EStop** |
/// | 终态不收敛 | cancel 未终态侧 → 失败则**双 EStop** |
/// | 整体硬上限超出 | 尽力 cancel + **双 EStop** |
///
/// 调用方（e2e / 真机验收 bin）**不需要**再写 catch/cleanup。
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
    let ack_skew = submit.ack_skew();
    if ack_skew > cfg.ack_skew_limit {
        // gate 违例 = 两臂未真并发；两个 action 都在跑，必须收掉。
        let cleanup = cancel_both(pair, &submit, &[Side::Left, Side::Right], clock, cfg).await;
        return Err(PairError::AckSkewExceeded {
            skew: ack_skew,
            limit: cfg.ack_skew_limit,
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
// submit_pair（D5 第 1 / 3 / 6 条）
// ============================================================================

/// **并发**提交一对 action：两侧同时 `InvokeTask`（每次调用带
/// [`PairConfig::rpc_timeout`] 硬超时），Ack 时刻由 `clock` 在**各自 future
/// 内**打点。
///
/// Ack 偏差 gate **不在**这里施加——见 [`run_chunk_pair`]（它在超限时 cancel
/// 双侧）。低层调用方若直接用本 fn，须自行调 [`PairSubmit::check_ack_skew`]
/// **并自行收尾**。
///
/// # Errors
///
/// 见 [`PairError::BothRejected`] / [`PairError::SubmitPartialFailure`] 等。
pub async fn submit_pair<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    left_spec: ActionSpec,
    right_spec: ActionSpec,
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> Result<PairSubmit, PairError> {
    // **各侧在自己的 future 内打点**（D5 第 1/7 条）：join 返回后统一取 now
    // 会把偏差抹成 0，gate 失效。两个 async block 各持一侧的 `&mut E`
    // （字段级不相交借用）+ 共享 `&dyn RobotClock`（只读）。
    let timeout = cfg.rpc_timeout;
    let ArmPair { left, right } = &mut *pair;
    let left_fut = async {
        let r = bounded(left.invoke_action(left_spec), timeout).await;
        (r, clock.now().0)
    };
    let right_fut = async {
        let r = bounded(right.invoke_action(right_spec), timeout).await;
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
        (Err(left_err), Err(right_err)) => Err(PairError::BothRejected {
            left: rpc_failure_to_error(left_err, timeout),
            right: rpc_failure_to_error(right_err, timeout),
        }),
        (Ok(accepted_id), Err(rejected)) => {
            handle_submit_partial(
                pair,
                Side::Left,
                accepted_id,
                Side::Right,
                rejected,
                clock,
                cfg,
            )
            .await
        }
        (Err(rejected), Ok(accepted_id)) => {
            handle_submit_partial(
                pair,
                Side::Right,
                accepted_id,
                Side::Left,
                rejected,
                clock,
                cfg,
            )
            .await
        }
    }
}

/// 提交失败（含超时）→ 可报告的 [`ArmRpcError`]。
fn rpc_failure_to_error(f: RpcFailure, timeout: Duration) -> ArmRpcError {
    match f {
        RpcFailure::Failed(e) => e,
        RpcFailure::TimedOut => {
            ArmRpcError::Transport(format!("invoke rpc timed out after {timeout:?}"))
        }
    }
}

/// 提交期部分失败收尾（D5 第 3 条）：cancel 已接收侧 → 收敛 /
/// 不收敛则双 EStop。
async fn handle_submit_partial<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    accepted_side: Side,
    accepted_id: String,
    rejected_side: Side,
    rejected: RpcFailure,
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> Result<PairSubmit, PairError> {
    let cancelled = cancel_surviving(pair, accepted_side, &accepted_id, clock, cfg).await?;
    Err(PairError::SubmitPartialFailure {
        rejected_side,
        rejected: rpc_failure_to_error(rejected, cfg.rpc_timeout),
        cancelled: Box::new(cancelled),
    })
}

// ============================================================================
// cancel / EStop（D5 第 3/4 条 + round 1 阻断 2）
// ============================================================================

/// cancel 指定侧并等其终态；**cancel / status RPC 失败或不收敛都升级为双侧
/// EStop**（安全方向）。
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
    if let Err(f) = bounded(pair.side_mut(side).cancel_task(action_id), cfg.rpc_timeout).await {
        return Err(fail_with_estop(pair, side, "cancel", f, cfg).await);
    }
    match poll_until_terminal(pair, side, action_id, clock, cfg.cancel_timeout, cfg).await {
        Ok(Some(report)) => Ok(report),
        Ok(None) => {
            // 不收敛 = 安全方向升级：双侧 EStop（D5 第 3/4 条）。
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
        Err(e) => Err(e),
    }
}

/// cancel 指定的若干侧（**收尾用，不返回 Err**）：逐侧 cancel + 等终态；
/// 任一侧失败 / 不收敛 → 触发一次双 EStop 并记进 [`PairCleanup`]。
async fn cancel_both<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    submit: &PairSubmit,
    sides: &[Side],
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> PairCleanup {
    let mut cancelled = Vec::new();
    let mut trouble: Option<String> = None;
    for &side in sides {
        let action_id = submit.action_id(side).to_string();
        if let Err(f) = bounded(pair.side_mut(side).cancel_task(&action_id), cfg.rpc_timeout).await
        {
            trouble.get_or_insert_with(|| match f {
                RpcFailure::Failed(e) => format!("{side} cancel rpc failed: {e}"),
                RpcFailure::TimedOut => format!("{side} cancel rpc timed out"),
            });
            continue;
        }
        match poll_until_terminal(pair, side, &action_id, clock, cfg.cancel_timeout, cfg).await {
            Ok(Some(report)) => cancelled.push(report),
            Ok(None) => {
                trouble.get_or_insert_with(|| format!("{side} cancel did not converge"));
            }
            Err(_) => {
                // poll 期 RPC 异常：`poll_until_terminal` 已在内部发过 EStop，
                // 这里只记录（避免重复升级）。
                trouble.get_or_insert_with(|| format!("{side} status rpc anomaly during cleanup"));
            }
        }
    }
    let estop = match trouble {
        None => None,
        Some(why) => {
            let reason = format!("dual_arm_workflow: cleanup incomplete ({why}) → double EStop");
            Some(escalate_double_estop(pair, &reason, cfg).await)
        }
    };
    PairCleanup { cancelled, estop }
}

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

// ============================================================================
// await_pair_terminal（D5 第 2/4/5 条）
// ============================================================================

/// 轮询两侧 `GetTaskStatus` 直到**双终态**（D5 第 5 条：stream 断开 / 事件
/// 超时的收敛通路；本模块全程只用轮询，不依赖 stream）。
///
/// 执行期不对称（D5 第 4 条）：
///
/// - 一侧先到 `Failed` / `Cancelled`、另一侧仍非终态 → **立即 cancel 幸存侧**；
/// - 一侧已 `Completed(Success)`、另一侧仍在跑 → **不 cancel**，继续等它自然
///   收敛（其后失败即 [`PairResult::PartialCompletion`]）。
///
/// **有界性**（round 1 阻断 1/2）：每次 status RPC 带 `rpc_timeout`；整段带
/// `cfg.hard_bound(terminal_timeout)` 外层硬上限；收敛超时 / 硬上限触发时
/// **先 cancel 未终态侧**（失败则双 EStop）再返错。
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
    let bound = cfg.hard_bound(cfg.terminal_timeout);
    let outcome = tokio::time::timeout(bound, terminal_loop(pair, submit, clock, cfg)).await;
    match outcome {
        Ok(inner) => inner,
        Err(_elapsed) => {
            let cleanup = cancel_both(pair, submit, &[Side::Left, Side::Right], clock, cfg).await;
            Err(PairError::HardBoundExceeded {
                phase: "await_pair_terminal",
                bound,
                cleanup: Box::new(cleanup),
            })
        }
    }
}

/// [`await_pair_terminal`] 的主循环（外层已加硬上限）。
async fn terminal_loop<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    submit: &PairSubmit,
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> Result<PairResult, PairError> {
    let deadline_ns = clock
        .now()
        .0
        .saturating_add(duration_to_ns(cfg.terminal_timeout));
    let mut done: [Option<TerminalReport>; 2] = [None, None];
    let mut last = [TaskPhase::Unspecified; 2];

    loop {
        for (idx, side) in [Side::Left, Side::Right].into_iter().enumerate() {
            if done[idx].is_none() {
                let (phase, report) =
                    poll_once(pair, side, submit.action_id(side), clock, cfg).await?;
                last[idx] = phase;
                done[idx] = report;
            }
        }
        if let (Some(l), Some(r)) = (done[0].as_ref(), done[1].as_ref()) {
            return Ok(PairResult::classify(l.clone(), r.clone()));
        }
        // 不对称：恰一侧终态。非成功侧先到 → cancel 幸存侧收尾；成功侧先到 →
        // 不 cancel，继续等另一侧自然收敛（D5 第 4 条）。
        let asymmetric = match (done[0].as_ref(), done[1].as_ref()) {
            (Some(first), None) => Some((first.clone(), Side::Right)),
            (None, Some(first)) => Some((first.clone(), Side::Left)),
            _ => None,
        };
        if let Some((first, pending_side)) = asymmetric
            && let Some(res) =
                settle_asymmetric(pair, submit, first, pending_side, clock, cfg).await?
        {
            return Ok(res);
        }
        if clock.now().0 >= deadline_ns {
            let pending: Vec<Side> = [Side::Left, Side::Right]
                .into_iter()
                .enumerate()
                .filter(|(i, _)| done[*i].is_none())
                .map(|(_, s)| s)
                .collect();
            let cleanup = cancel_both(pair, submit, &pending, clock, cfg).await;
            return Err(PairError::ConvergenceTimeout {
                timeout: cfg.terminal_timeout,
                left: last[0],
                right: last[1],
                cleanup: Box::new(cleanup),
            });
        }
        tokio::time::sleep(cfg.poll_interval).await;
    }
}

/// 执行期不对称处理：`first` 已终态、`pending_side` 仍在跑。
///
/// - `first` **不成功** → cancel 幸存侧并等终态 → 返 `Some(结果)`；
/// - `first` 成功 → 返 `None`（调用方继续等另一侧自然收敛）。
async fn settle_asymmetric<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    submit: &PairSubmit,
    first: TerminalReport,
    pending_side: Side,
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> Result<Option<PairResult>, PairError> {
    if first.is_success() {
        return Ok(None);
    }
    let pending_id = submit.action_id(pending_side).to_string();
    let other = cancel_surviving(pair, pending_side, &pending_id, clock, cfg).await?;
    let (left, right) = match first.side {
        Side::Left => (first, other),
        Side::Right => (other, first),
    };
    Ok(Some(PairResult::classify(left, right)))
}

/// 单次 `GetTaskStatus`（带硬超时；异常 → 双 EStop + typed 错误）。
async fn poll_once<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    side: Side,
    action_id: &str,
    clock: &dyn RobotClock,
    cfg: &PairConfig,
) -> Result<(TaskPhase, Option<TerminalReport>), PairError> {
    let snap = match bounded(pair.side_mut(side).task_status(action_id), cfg.rpc_timeout).await {
        Ok(s) => s,
        Err(f) => return Err(fail_with_estop(pair, side, "poll", f, cfg).await),
    };
    let phase = snap.phase;
    let report = phase
        .terminal()
        .map(|t| TerminalReport::from_snapshot(side, action_id, snap, t, clock.now().0));
    Ok((phase, report))
}

/// 在 `timeout` 内轮询单侧直到终态；超时返 `Ok(None)`。
async fn poll_until_terminal<E: ArmEndpoint>(
    pair: &mut ArmPair<E>,
    side: Side,
    action_id: &str,
    clock: &dyn RobotClock,
    timeout: Duration,
    cfg: &PairConfig,
) -> Result<Option<TerminalReport>, PairError> {
    let deadline_ns = clock.now().0.saturating_add(duration_to_ns(timeout));
    loop {
        if let (_, Some(report)) = poll_once(pair, side, action_id, clock, cfg).await? {
            return Ok(Some(report));
        }
        if clock.now().0 >= deadline_ns {
            return Ok(None);
        }
        tokio::time::sleep(cfg.poll_interval).await;
    }
}

fn duration_to_ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

// ============================================================================
// Tests（脚本化 mock 端点：协议分支 + 有界性 + 收尾全覆盖，无进程无网络）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use robot_core::time::{MonoRawNs, RobotClock};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    /// 单调递增的测试时钟：每次 `now()` 前进固定步长（让超时判据确定性推进，
    /// 不依赖墙钟）。
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

    /// 脚本化端点。用 `Mutex`（非 `RefCell`）使其天然 `Send`——
    /// `ArmEndpoint: Send` 是协议 trait 的硬要求，测试侧不得用
    /// `unsafe impl Send` 绕过。
    struct MockArm {
        side: Side,
        invoke: Mutex<VecDeque<Result<String, ArmRpcError>>>,
        status: Mutex<VecDeque<TaskSnapshot>>,
        /// cancel 之后改用的状态序列（模拟"cancel 生效"）。
        after_cancel: Mutex<Option<VecDeque<TaskSnapshot>>>,
        /// `task_status` 永不返回（模拟 RPC 挂死；round 1 阻断 1）。
        status_pending: bool,
        /// `task_status` 直接返错。
        status_error: Option<ArmRpcError>,
        /// 记录最后一次 invoke 的 spec（阻断 3：断言发的是哪种 kind）。
        last_spec: Mutex<Option<ActionSpec>>,
        cancel_calls: Arc<AtomicU64>,
        estop_calls: Arc<AtomicU64>,
    }

    impl MockArm {
        fn new(side: Side) -> Self {
            Self {
                side,
                invoke: Mutex::new(VecDeque::new()),
                status: Mutex::new(VecDeque::new()),
                after_cancel: Mutex::new(None),
                status_pending: false,
                status_error: None,
                last_spec: Mutex::new(None),
                cancel_calls: Arc::new(AtomicU64::new(0)),
                estop_calls: Arc::new(AtomicU64::new(0)),
            }
        }

        fn with_invoke(self, r: Result<String, ArmRpcError>) -> Self {
            self.invoke.lock().expect("lock").push_back(r);
            self
        }

        fn with_status(self, s: TaskSnapshot) -> Self {
            self.status.lock().expect("lock").push_back(s);
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

        fn with_status_pending(mut self) -> Self {
            self.status_pending = true;
            self
        }

        fn with_status_error(mut self, e: ArmRpcError) -> Self {
            self.status_error = Some(e);
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
    }

    impl ArmEndpoint for MockArm {
        fn side(&self) -> Side {
            self.side
        }

        async fn invoke_action(&mut self, spec: ActionSpec) -> Result<String, ArmRpcError> {
            // 让出一次：使 `tokio::join!` 的两侧真正交错（而非顺序求值）。
            tokio::task::yield_now().await;
            *self.last_spec.lock().expect("lock") = Some(spec);
            let popped = self.invoke.lock().expect("lock").pop_front();
            popped.unwrap_or_else(|| Ok(format!("{}-action", self.side.label())))
        }

        async fn cancel_task(&mut self, _action_id: &str) -> Result<(), ArmRpcError> {
            tokio::task::yield_now().await;
            self.cancel_calls.fetch_add(1, Ordering::Relaxed);
            let replacement = self.after_cancel.lock().expect("lock").take();
            if let Some(seq) = replacement {
                *self.status.lock().expect("lock") = seq;
            }
            Ok(())
        }

        async fn task_status(&mut self, _action_id: &str) -> Result<TaskSnapshot, ArmRpcError> {
            tokio::task::yield_now().await;
            if self.status_pending {
                // 永不返回：协议层的 `rpc_timeout` 必须兜住（阻断 1 回归锁）。
                std::future::pending::<()>().await;
            }
            if let Some(e) = self.status_error.clone() {
                return Err(e);
            }
            // 队列 > 1 时逐条推进；剩最后一条则**保持**（稳态终态可反复读）。
            let mut q = self.status.lock().expect("lock");
            let front = if q.len() > 1 {
                q.pop_front()
            } else {
                q.front().cloned()
            };
            drop(q);
            Ok(front.unwrap_or_else(|| snap(TaskPhase::Executing)))
        }

        async fn estop(&mut self, _reason: &str) -> Result<(), ArmRpcError> {
            tokio::task::yield_now().await;
            self.estop_calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
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

    fn fast_cfg() -> PairConfig {
        PairConfig {
            ack_skew_limit: Duration::from_millis(250),
            terminal_timeout: Duration::from_millis(200),
            cancel_timeout: Duration::from_millis(100),
            poll_interval: Duration::from_millis(1),
            rpc_timeout: Duration::from_millis(80),
            estop_timeout: Duration::from_millis(50),
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
        let cfg = fast_cfg();
        let run = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &cfg)
            .await
            .expect("pair success");
        assert!(run.result.is_pair_success(), "{:?}", run.result);
        // 阻断 3：发的确实是 Scripted，不是暗中的 Dwell。
        assert_eq!(pair.left.last_kind(), Some(ActionKindWire::Scripted));
        assert_eq!(pair.right.last_kind(), Some(ActionKindWire::Scripted));
    }

    #[tokio::test]
    async fn dwell_spec_is_explicit_at_call_site() {
        // fault_inject 触发通路 = 显式 DwellMs + NO_PAYLOAD（阻断 3）。
        let spec = ActionSpec::dwell_no_payload();
        assert_eq!(spec.kind, ActionKindWire::DwellMs);
        assert_eq!(spec.payload_schema_id, NO_PAYLOAD_SCHEMA_ID);
        assert!(spec.payload.is_empty());
        assert_eq!(ActionKindWire::DwellMs.proto_value(), 1);
        assert_eq!(ActionKindWire::Scripted.proto_value(), 3);

        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left).with_status(failed_internal()),
            right: MockArm::new(Side::Right)
                .with_status(snap(TaskPhase::Executing))
                .with_after_cancel(cancelled()),
        };
        let cfg = fast_cfg();
        let err = run_chunk_pair(
            &mut pair,
            ActionSpec::dwell_no_payload(),
            scripted(),
            &clock,
            &cfg,
        )
        .await;
        assert!(err.is_ok(), "{err:?}");
        assert_eq!(pair.left.last_kind(), Some(ActionKindWire::DwellMs));
        assert_eq!(pair.right.last_kind(), Some(ActionKindWire::Scripted));
    }

    #[tokio::test]
    async fn both_rejected_is_deterministic_fail_without_retry() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left)
                .with_invoke(Err(ArmRpcError::Transport("left down".into()))),
            right: MockArm::new(Side::Right).with_invoke(Err(ArmRpcError::Status {
                code: "ResourceExhausted".into(),
                message: "full".into(),
            })),
        };
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &fast_cfg())
            .await
            .expect_err("双拒必 FAIL");
        assert!(matches!(err, PairError::BothRejected { .. }), "{err:?}");
        // 双拒无副作用：不 cancel、不 EStop。
        assert_eq!(pair.left.cancels(), 0);
        assert_eq!(pair.left.estops(), 0);
        assert_eq!(pair.right.estops(), 0);
    }

    #[tokio::test]
    async fn submit_partial_failure_cancels_accepted_side() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left)
                .with_status(snap(TaskPhase::Executing))
                .with_after_cancel(cancelled()),
            right: MockArm::new(Side::Right)
                .with_invoke(Err(ArmRpcError::Transport("right down".into()))),
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
                assert_eq!(cancelled.side, Side::Left);
                assert_eq!(cancelled.state, TerminalState::Cancelled);
            }
            other => panic!("expected SubmitPartialFailure, got {other:?}"),
        }
        assert_eq!(pair.left.cancels(), 1);
        assert_eq!(pair.left.estops(), 0);
    }

    #[tokio::test]
    async fn cancel_not_converging_escalates_to_double_estop() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            // cancel 后仍恒 Executing → 不收敛。
            left: MockArm::new(Side::Left).with_status(snap(TaskPhase::Executing)),
            right: MockArm::new(Side::Right)
                .with_invoke(Err(ArmRpcError::Transport("right down".into()))),
        };
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &fast_cfg())
            .await
            .expect_err("cancel 不收敛");
        match err {
            PairError::CancelNotConverged {
                side, ref estop, ..
            } => {
                assert_eq!(side, Side::Left);
                assert!(estop.both_acked());
            }
            other => panic!("expected CancelNotConverged, got {other:?}"),
        }
        assert_eq!(pair.left.estops(), 1);
        assert_eq!(pair.right.estops(), 1);
    }

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
                assert_eq!(left.failure, FailureKind::InternalError);
                assert_eq!(right.state, TerminalState::Cancelled);
            }
            other => panic!("expected BothUnsuccessful, got {other:?}"),
        }
        assert_eq!(pair.right.cancels(), 1);
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
                assert_eq!(other.side, Side::Right);
                assert_eq!(other.state, TerminalState::Failed);
            }
            other => panic!("expected PartialCompletion, got {other:?}"),
        }
        // 成功侧先到不得 cancel 幸存侧（D5 第 4 条）。
        assert_eq!(pair.right.cancels(), 0);
        assert!(!run.result.is_pair_success());
    }

    // ---- round 1 阻断 1：硬超时 ----
    //
    // 注：不用 `#[tokio::test(start_paused)]`（需 tokio `test-util` feature，
    // 不为测试扩 dev-dep 面）；`fast_cfg` 的 rpc_timeout = 80 ms，真实时间
    // 开销可忽略。

    #[tokio::test]
    async fn pending_status_rpc_is_bounded_and_escalates() {
        // 端点 `task_status` 永不返回：若协议层没有 rpc_timeout，
        // terminal_timeout / cancel_timeout 全部失效（阻断 1）。
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left).with_status_pending(),
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
                assert!(estop.both_acked(), "异常必须已双 EStop 收尾");
            }
            other => panic!("expected RpcTimeout, got {other:?}"),
        }
        assert_eq!(pair.left.estops(), 1);
        assert_eq!(pair.right.estops(), 1);
    }

    #[tokio::test]
    async fn pending_invoke_rpc_is_bounded_by_rpc_timeout() {
        // invoke 侧挂死 → 视为该侧未接受；另一侧已接受 → 走提交期部分失败收尾。
        struct PendingInvoke(Side, Arc<AtomicU64>);
        impl ArmEndpoint for PendingInvoke {
            fn side(&self) -> Side {
                self.0
            }
            async fn invoke_action(&mut self, _spec: ActionSpec) -> Result<String, ArmRpcError> {
                std::future::pending::<()>().await;
                unreachable!()
            }
            async fn cancel_task(&mut self, _action_id: &str) -> Result<(), ArmRpcError> {
                tokio::task::yield_now().await;
                Ok(())
            }
            async fn task_status(&mut self, _action_id: &str) -> Result<TaskSnapshot, ArmRpcError> {
                tokio::task::yield_now().await;
                Ok(snap(TaskPhase::Executing))
            }
            async fn estop(&mut self, _reason: &str) -> Result<(), ArmRpcError> {
                tokio::task::yield_now().await;
                self.1.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
        let estops = Arc::new(AtomicU64::new(0));
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: PendingInvoke(Side::Left, Arc::clone(&estops)),
            right: PendingInvoke(Side::Right, Arc::clone(&estops)),
        };
        let cfg = fast_cfg();
        let err = submit_pair(&mut pair, scripted(), scripted(), &clock, &cfg)
            .await
            .expect_err("双侧 invoke 挂死 → 双拒（超时形态）");
        match err {
            PairError::BothRejected { left, right } => {
                assert!(matches!(left, ArmRpcError::Transport(ref m) if m.contains("timed out")));
                assert!(matches!(right, ArmRpcError::Transport(ref m) if m.contains("timed out")));
            }
            other => panic!("expected BothRejected(timeout), got {other:?}"),
        }
    }

    // ---- round 1 阻断 2：统一安全收尾 ----

    #[tokio::test]
    async fn ack_skew_violation_cancels_both_sides() {
        // 上限设 0 → 任何非零偏差都违例；断言**两侧都被 cancel**（不留在跑）。
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
                assert_eq!(cleanup.cancelled.len(), 2, "两侧都必须被 cancel");
                assert!(cleanup.is_clean(), "收敛正常时不应升级 EStop");
            }
            other => panic!("expected AckSkewExceeded, got {other:?}"),
        }
        assert_eq!(pair.left.cancels(), 1);
        assert_eq!(pair.right.cancels(), 1);
        assert_eq!(pair.left.estops(), 0);
    }

    #[tokio::test]
    async fn status_rpc_failure_escalates_double_estop() {
        let clock = StepClock::new(1_000_000);
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left).with_status_error(ArmRpcError::Status {
                code: "Unavailable".into(),
                message: "gateway gone".into(),
            }),
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
    async fn convergence_timeout_cancels_pending_sides() {
        let clock = StepClock::new(10_000_000); // 10 ms / 次读，快速逼近上限
        let mut pair = ArmPair {
            left: MockArm::new(Side::Left)
                .with_status(snap(TaskPhase::Executing))
                .with_after_cancel(cancelled()),
            right: MockArm::new(Side::Right)
                .with_status(snap(TaskPhase::Pending))
                .with_after_cancel(cancelled()),
        };
        let cfg = fast_cfg();
        let err = run_chunk_pair(&mut pair, scripted(), scripted(), &clock, &cfg)
            .await
            .expect_err("永不终态");
        match err {
            PairError::ConvergenceTimeout {
                left,
                right,
                ref cleanup,
                ..
            } => {
                assert_eq!(left, TaskPhase::Executing);
                assert_eq!(right, TaskPhase::Pending);
                assert_eq!(cleanup.cancelled.len(), 2, "未终态两侧都要收掉");
                assert!(cleanup.is_clean());
            }
            other => panic!("expected ConvergenceTimeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn convergence_timeout_with_stuck_cancel_escalates_estop() {
        // 未终态 + cancel 也不收敛 → 收尾必须升级双 EStop。
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
                let estop = cleanup.estop.as_ref().expect("必须已双 EStop");
                assert!(estop.both_acked());
            }
            other => panic!("expected ConvergenceTimeout, got {other:?}"),
        }
        assert!(pair.left.estops() >= 1);
        assert!(pair.right.estops() >= 1);
    }

    // ---- 判据 / 映射 ----

    #[test]
    fn ack_skew_gate_rejects_beyond_limit() {
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
        let over = mk(300_000_000); // 300 ms > 250 ms
        assert_eq!(over.ack_skew(), Duration::from_millis(300));
        let err = over.check_ack_skew(&cfg).expect_err("超限必拒");
        assert!(matches!(err, PairError::AckSkewExceeded { .. }), "{err:?}");
        // 边界：恰等于上限 → 通过（≤ 语义）。
        mk(duration_to_ns(cfg.ack_skew_limit))
            .check_ack_skew(&cfg)
            .expect("恰等于上限应通过");
        assert_eq!(over.action_id(Side::Left), "l");
        assert_eq!(over.action_id(Side::Right), "r");
    }

    #[test]
    fn hard_bound_covers_phase_plus_cleanup_windows() {
        let cfg = PairConfig::default();
        let bound = cfg.hard_bound(cfg.terminal_timeout);
        assert!(
            bound > cfg.terminal_timeout + cfg.cancel_timeout,
            "硬上限必须覆盖阶段窗 + 收尾窗，实得 {bound:?}"
        );
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
        // Completed(PartialSuccess) = cancel 收敛形态，不算 pair success。
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
        assert!(s.phase.terminal().is_none());
        assert_eq!(s.failure, FailureKind::Unspecified);
        assert_eq!(s.failure_reason, "weird");
    }

    #[test]
    fn side_helpers() {
        assert_eq!(Side::Left.other(), Side::Right);
        assert_eq!(Side::Right.other(), Side::Left);
        assert_eq!(Side::Left.to_string(), "left");
        assert_eq!(Side::Right.label(), "right");
    }
}
