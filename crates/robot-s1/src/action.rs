//! v0.3 Phase B B2 (1/3) action interface 类型骨架。
//!
//! 本模块**只**含 lib 纯类型 + trait 定义；**不**引入 backend 实现 /
//! `BackendRegistry` / `Coordinator` wiring / iceoryx2 / tokio / tonic
//! （reviewer B2 (1/3) 6 闸硬约束，详
//! [`docs/handoffs/v0.3_b2_outline.md`](../../../docs/handoffs/v0.3_b2_outline.md)
//! §10 拍板 + B2 (2/3) / (3/3) 实现 commit）。
//!
//! ## 模块入口
//!
//! - [`ActionId`]：UUIDv7 newtype（§10 决策 #2）。
//! - [`ActionKind`]：B2 enum 仅 [`ActionKind::DwellMs`] +
//!   [`ActionKind::HoldPose`] 两个 variant（§10 决策 #1 + 阻断 3）；
//!   `Stop` / `MoveToPose` / `FollowTrajectory` / `PolicyAction`
//!   **不**进 B2 enum。
//! - [`ActionRequest`] + [`ActionRequest::validate`]：纯字段校验
//!   （deadline / idempotency_key / payload 长度上限 + 哨兵 schema_id
//!   守门）。
//! - [`NO_PAYLOAD_SCHEMA_ID`]：`payload_schema_id` 哨兵值（reviewer 阻
//!   断 1 锁定，去掉 [`ActionBackend::supported_schemas`] 空切片魔法语
//!   义）。
//! - [`JointCommandBody`]：`Vec<u8>` newtype，让"这是 JointCommand body
//!   字节流"在类型上一目了然（§10 决策 #8）。
//! - [`ActionBackend`] / [`ActionInstance`] trait：B3 起按 feature flag
//!   注册具体 backend；B2 (2/3) 加 `BackendRegistry` 静态注册。
//! - [`ActionContext`]：tick 时由 coordinator 借给 backend 的上下文
//!   （`RobotClock` / `HardwareSection` / `CoordinatorConfig` /
//!   `anchor_bias`）；B2 **不**暴露 publisher trait object（§10 决策
//!   #8 + outline §2.5 注释）。
//! - 错误枚举：[`SubmitError`] / [`ActionStartError`] / [`ActionCancelError`]
//!   / [`ClearError`] / [`ActionFailure`]。
//!
//! ## 不做（B2 (1/3) 边界）
//!
//! - **不**引入 `BackendRegistry` / `parse_profile_section_from_toml` /
//!   `inventory::submit!` 注册（→ B2 (2/3)）。
//! - **不**引入 `MockDwellBackend` 真实现（→ B2 (2/3)）。
//! - **不**改 [`coordinator`](crate::coordinator)：`CoordinatorState` /
//!   `Coordinator::submit/cancel/status/clear_error` 都留 B2 (2/3)。
//! - **不**改 `s1_daemon` binary（→ B2 (3/3)）。
//! - **不**引入 `ActionTicket` / `ActionEvent` / `ActionStatus`：这些
//!   是 coordinator-side API（接收事件流 / 查状态），跟 backend trait
//!   层无直接耦合，留 B2 (2/3) 跟 `Coordinator::submit` 一起加，且**必须**
//!   用 `std::sync::mpsc::Receiver`（reviewer 阻断 1 锁定，不 tokio）。
//! - **不**引入跨进程 fbs schema：`payload: Vec<u8>` 字段先占位，B4
//!   真接 RPC `InvokeTask` 时再引入 `action_request.fbs` /
//!   `action_status.fbs`（§10 决策 #9）。

// 中文 doc 块大量 identifier-like 字串（mock_s1 / s1_daemon /
// coordinator / RobotClock 等），跟 lib 同款宽松 doc-markdown。
#![allow(clippy::doc_markdown)]

use robot_core::time::RobotClock;
use thiserror::Error;
use uuid::Uuid;

use crate::coordinator::{CoordinatorConfig, HardwareSection};

// ============================================================================
// 字段上限常量
// ============================================================================

/// `ActionRequest.payload` 字节数上限（B2 mock backend 用空，B3 起
/// trajectory / policy chunk 等 backend 才会用；超此值 [`SubmitError::InvalidRequest`]
/// 拒绝）。1 MiB 是宽松上限，远超任何合理 in-process payload。
pub const MAX_PAYLOAD_LEN: usize = 1024 * 1024;

/// `ActionRequest.idempotency_key` 字节数上限。RPC `CommandMeta.idempotency_key`
/// 通常是 UUID 或短哈希；256 字节足够覆盖，超此值视为客户端 bug 拒绝。
pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 256;

/// "无 payload" 的 schema_id 哨兵值（reviewer 阻断 1 锁定）。
///
/// 约定：
///
/// - 空 payload backend（如 `MockDwell`）**必须**在
///   [`ActionBackend::supported_schemas`] 显式返回 `&[NO_PAYLOAD_SCHEMA_ID]`，
///   不能返回空切片做隐式 "any payload" 处理。
/// - [`ActionBackend::supported_schemas`] 返回空切片 = 该 backend 不接
///   受任何 schema（理论上 dispatcher 永远找不到匹配，等价于没注册；
///   仅作为退化态保留，不应在生产 backend 出现）。
/// - [`ActionRequest::validate`] 强制 `payload_schema_id == NO_PAYLOAD_SCHEMA_ID`
///   时 `payload` 必须为空 —— 防止客户端用 schema_id 0 把任意 bytes 偷
///   渡进来。
///
/// 数值固定为 0：跟 RPC `CommandMeta.payload_schema_id` 默认值（未设置
/// 字段 = 0）天然对齐，零 payload 调用方不需要显式填值。
pub const NO_PAYLOAD_SCHEMA_ID: u32 = 0;

// ============================================================================
// ActionId
// ============================================================================

/// UUIDv7 newtype（§10 决策 #2 拍板：跟 RPC `CommandMeta.request_id`
/// 同款时间排序，B4 时 robot-rpc-gateway 把 `request_id` 直接转成
/// `ActionId`，无需翻译）。
///
/// `pub` 内部 [`Uuid`] 让单测 / B4 gateway 端可以从已存在的 UUIDv7 直接
/// 构造 `ActionId(uuid)`，不强制走 `new_v7()` 路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActionId(pub Uuid);

impl ActionId {
    /// 生成一个新的 UUIDv7 `ActionId`（time-ordered；前 48 bit 是 ms
    /// since UNIX epoch）。
    ///
    /// **不**在 RT 线程调用（`uuid::Uuid::now_v7` 走系统时钟 + RNG，
    /// allocator 在 v7 编码路径里间接被触发）。B2 (1/3) action 路径全部
    /// 在 coordinator IPC 线程，非 RT。
    #[must_use]
    pub fn new_v7() -> Self {
        Self(Uuid::now_v7())
    }
}

impl core::fmt::Display for ActionId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ============================================================================
// ActionKind
// ============================================================================

/// B2 action 类型枚举。
///
/// **reviewer §10 决策 #1 + 阻断 3 锁定**：B2 仅 2 个 variant；
/// **不**放 `Stop`（混淆 task cancel / S1 degraded / S0 control-lost
/// stop / EStop 4 个语义层）；**不**放 B3 占位 variant
/// （`MoveToPose` / `FollowTrajectory` / `PolicyAction`）—— B3 落地对
/// 应 backend 时再扩 enum，向 `docs/00_design.md` §6.6.4 errata 边界
/// 对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    /// Dwell N ms 后完成；不输出新 `JointCommand`，coordinator 沿用
    /// hold bias。B2 mock backend 唯一支持值之一。
    DwellMs,
    /// 保持当前 anchor，等价 IDLE 但走 EXECUTING 状态路径（用于测试
    /// state machine 转移而不动 wire 输出）。B2 mock backend 唯一支持
    /// 值之一。
    HoldPose,
}

// ============================================================================
// JointCommandBody
// ============================================================================

/// `JointCommand` body 字节流的 newtype 包装（§10 决策 #8 拍板 —— 不
/// 裸 `Vec<u8>`）。
///
/// 让 `ActionTickOutput::Continue::command` 在类型上明确是 "JointCommand
/// body"，防 backend 误塞任意 bytes；coordinator 拿到后直接调
/// `SampleWriter::set_body_bytes(body.as_bytes())`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JointCommandBody(Vec<u8>);

impl JointCommandBody {
    /// 用现成的 `JointCommand` body bytes 构造（典型来自
    /// [`crate::coordinator::build_hold_command`] 或 backend 内部
    /// `build_joint_command()`）。
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// 借用底层 bytes（`coordinator` publish 路径 zero-copy 调用）。
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// 按值消费返底层 `Vec<u8>`。
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// body 字节长度。
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// body 字节是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

// ============================================================================
// ActionRequest
// ============================================================================

/// 一次 action 提交请求。
///
/// 字段语义：
///
/// - `action_id`：UUIDv7 全局 ID。B4 时直接复用 RPC `CommandMeta.request_id`。
/// - `kind`：路由用；coordinator 按 `kind` 找 backend
///   （[`ActionBackend::supported_kinds`]）。
/// - `deadline_ns`：monotonic deadline（`CLOCK_MONOTONIC_RAW`）；调用方
///   （B4 时是 gateway）转 RPC `timeout_ms` 为 `now_ns + timeout_ms * 1e6`。
/// - `priority`：同时多 action 时大者优先；B2 单 action 序列化（§10
///   决策 #3）所以本字段保留**不**参与决策，B3 scheduler 用。
/// - `payload_schema_id`：路由二级 key；backend 注册时声明
///   [`ActionBackend::supported_schemas`]，dispatcher 按
///   `(kind, schema_id)` 路由。**[`NO_PAYLOAD_SCHEMA_ID`] 表示"本请求
///   无 payload"**，[`ActionRequest::validate`] 强制此时 `payload` 为
///   空（B2 mock backend / DwellMs / HoldPose 走这条路径）。
/// - `payload`：`FlatBuffers` body bytes（按 `payload_schema_id`）。
///   **B2 mock backend 用空**（配 [`NO_PAYLOAD_SCHEMA_ID`]）；B4 才引
///   入跨进程 fbs schema（§10 决策 #9）。
/// - `idempotency_key`：跨 retry 一致 fingerprint，与 RPC
///   `CommandMeta.idempotency_key` 对齐。**B2 仅**声明字段，**不**做
///   dedup（B4 gateway 自己有 idempotency cache，详 A1 ClearEStop 边界）。
/// - `source`：给 audit / recorder。
#[derive(Debug, Clone)]
pub struct ActionRequest {
    pub action_id: ActionId,
    pub kind: ActionKind,
    pub deadline_ns: u64,
    pub priority: u8,
    pub payload_schema_id: u32,
    pub payload: Vec<u8>,
    pub idempotency_key: String,
    pub source: ActionSource,
}

impl ActionRequest {
    /// 纯字段校验（不依赖 coordinator state / backend registry）。
    ///
    /// **检查项**：
    /// - `deadline_ns > now_ns`（提交时未过期）。
    /// - `idempotency_key` 非空且 ≤ [`MAX_IDEMPOTENCY_KEY_LEN`]。
    /// - `payload.len()` ≤ [`MAX_PAYLOAD_LEN`]。
    /// - `payload_schema_id == ` [`NO_PAYLOAD_SCHEMA_ID`] 时 `payload`
    ///   必须为空（reviewer 阻断 1 锁定 —— 防止客户端用哨兵 schema_id
    ///   偷渡任意 bytes）。
    ///
    /// **不检查**：
    /// - `kind` 是否有匹配 backend（路由查表是 coordinator 的事，留 B2 (2/3)）。
    /// - 非哨兵 `payload_schema_id` 是否在 backend `supported_schemas()`
    ///   集合（同上）。
    /// - `action_id` 唯一性（B2 单 action 序列化，state 守门即可）。
    ///
    /// 返 [`SubmitError::InvalidRequest`]，`String` 是 human-readable
    /// 字段名 + 原因，便于 gateway 透传给客户端 audit。
    pub fn validate(&self, now_ns: u64) -> Result<(), SubmitError> {
        if self.deadline_ns <= now_ns {
            return Err(SubmitError::InvalidRequest(format!(
                "deadline_ns ({}) must be > now_ns ({})",
                self.deadline_ns, now_ns
            )));
        }
        if self.idempotency_key.is_empty() {
            return Err(SubmitError::InvalidRequest(
                "idempotency_key must not be empty".into(),
            ));
        }
        if self.idempotency_key.len() > MAX_IDEMPOTENCY_KEY_LEN {
            return Err(SubmitError::InvalidRequest(format!(
                "idempotency_key length {} exceeds MAX_IDEMPOTENCY_KEY_LEN ({})",
                self.idempotency_key.len(),
                MAX_IDEMPOTENCY_KEY_LEN
            )));
        }
        if self.payload.len() > MAX_PAYLOAD_LEN {
            return Err(SubmitError::InvalidRequest(format!(
                "payload length {} exceeds MAX_PAYLOAD_LEN ({})",
                self.payload.len(),
                MAX_PAYLOAD_LEN
            )));
        }
        if self.payload_schema_id == NO_PAYLOAD_SCHEMA_ID && !self.payload.is_empty() {
            return Err(SubmitError::InvalidRequest(format!(
                "payload_schema_id == NO_PAYLOAD_SCHEMA_ID ({}) requires empty payload, got {} bytes",
                NO_PAYLOAD_SCHEMA_ID,
                self.payload.len()
            )));
        }
        Ok(())
    }
}

// ============================================================================
// ActionSource
// ============================================================================

/// `ActionRequest.source` 来源标识（给 audit / recorder）。
#[derive(Debug, Clone)]
pub enum ActionSource {
    /// 来自 `robot-rpc` gateway（B4 起接 tonic `InvokeTask`）。
    /// `actor` 是 RPC `CommandMeta.source`（如 `"cli.loosqk"`）。
    RpcGateway { actor: String },
    /// coordinator 自己排程（B2 不构造，B3+ scheduler 用）。
    Internal,
}

// ============================================================================
// ActionContext
// ============================================================================

/// Backend `tick()` 时由 coordinator 借给 backend 的上下文。
///
/// **B2 (1/3) 字段集**（**不**暴露 publisher / 最新 S0 state；§10 决策
/// #8 锁定 —— backend 经 `ActionTickOutput::Continue::command` 返
/// [`JointCommandBody`] 让 coordinator publish，最干净）：
///
/// - `clock`：trait object，测试用 `MockClock` 推时间，binary 用
///   `SystemClock`。
/// - `layout_id`：joint layout fingerprint，backend 写
///   `JointCommand.layout_id` 时用。
/// - `hardware`：B1 (1/3) [`HardwareSection`]（`id` + `joints`）。
/// - `config`：B1 (1/3) [`CoordinatorConfig`]（`heartbeat_hz` /
///   `command_hz` / `deadline_ms` / 等）。
/// - `anchor_bias`：启动锚定的 bias，供 `HoldPose` / 后续真 backend
///   计算 target 时用。
pub struct ActionContext<'a> {
    pub clock: &'a dyn RobotClock,
    pub layout_id: u64,
    pub hardware: &'a HardwareSection,
    pub config: &'a CoordinatorConfig,
    pub anchor_bias: &'a [f64],
}

// ============================================================================
// ActionTickOutput / ActionOutcome / ActionFailure
// ============================================================================

/// Backend `tick()` 单帧返回值。
#[derive(Debug, Clone)]
pub enum ActionTickOutput {
    /// 还在跑；可选附本 tick 输出的 `JointCommand` body bytes。
    ///
    /// **command 语义**（reviewer 阻断 2 锁定 / 与 outline §6 共存策略 =
    /// override 一致）：
    ///
    /// - `Some(body)`：本 tick coordinator publish 这帧 `JointCommand`；
    ///   S0 的"上次 cmd 直到 deadline"窗口被刷新。Backend 想要持续刷新
    ///   hold（防 S0 deadline 触发 control-lost）必须周期性返
    ///   `Some(...)`。
    /// - `None`：本 tick coordinator **不** publish；S0 沿用**上一帧
    ///   cmd**直到 [`ActionRequest::deadline_ns`] / S0 自己的 command
    ///   timeout 触发。**不**走 coordinator 的 hold loop —— EXECUTING
    ///   状态下 hold loop 已停发（override 策略）；要恢复 hold loop 必
    ///   须返 [`ActionTickOutput::Completed`] 让 coordinator 回 IDLE。
    Continue {
        /// `[0.0, 1.0]` 进度；不参与控制逻辑，仅给 audit / RPC stream。
        progress: f32,
        /// 本 tick 是否产生新 cmd；语义见上方 variant 文档。
        command: Option<JointCommandBody>,
    },
    /// 完成；coordinator 转回 IDLE，恢复 hold loop。
    Completed(ActionOutcome),
}

/// Action 完成时的具体结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    Success,
    /// Backend 完成了大部分但有警告（例：trajectory 被 limit 削减）。
    PartialSuccess {
        warning: String,
    },
}

/// Action 失败原因。变体决定 coordinator 是回 IDLE（自恢复 hold）还是
/// 进 DEGRADED（拒绝新 action 等 `clear_error`）：
///
/// - `PreconditionFailed` / `DeadlineExceeded` → IDLE（hold 自动恢复）
/// - `SafetyAbort` / `AdapterError` / `InternalError` → DEGRADED
///
/// 详 outline §6.5 fallback 表 + §10 决策 #10 拍板方案 (d)。
#[derive(Debug, Clone, Error)]
pub enum ActionFailure {
    #[error("deadline exceeded")]
    DeadlineExceeded,
    #[error("precondition failed: {0}")]
    PreconditionFailed(String),
    #[error("adapter error: {0}")]
    AdapterError(String),
    #[error("safety abort: {0}")]
    SafetyAbort(String),
    #[error("internal error: {0}")]
    InternalError(String),
}

// ============================================================================
// ActionBackend / ActionInstance trait
// ============================================================================

/// Backend 工厂 trait：声明能力 + 创建 instance。
///
/// B2 (2/3) 时通过 `inventory::submit!` 静态注册到 `BackendRegistry`；
/// `[s1.profile.backends = [...]]` runtime enable list 决定哪些注册的
/// backend 真在 coordinator 启动时被实例化。
pub trait ActionBackend: Send + Sync {
    /// Backend 名字（跟 `inventory` 注册名一致；profile
    /// `[s1.profile.backends]` 选）。
    fn name(&self) -> &str;

    /// 本 backend 支持的 [`ActionKind`] 集合。Coordinator dispatch
    /// 时按 `(kind, schema_id)` 在所有 enabled backend 中找匹配。
    fn supported_kinds(&self) -> &[ActionKind];

    /// 本 backend 接受的 `payload_schema_id` 集合（reviewer 阻断 1 锁定，
    /// 不做空切片魔法）。
    ///
    /// **约定**：
    ///
    /// - 空 payload backend（如 `MockDwell` / `HoldPose`）必须显式返
    ///   `&[NO_PAYLOAD_SCHEMA_ID]`；coordinator dispatch 按
    ///   `(kind, schema_id)` 精确匹配，**不**对空切片做隐式 "any payload"
    ///   处理。
    /// - 返回空切片 = 该 backend 不接受任何 schema_id（dispatcher 永远
    ///   找不到匹配，等价于没注册）；仅作退化态保留，不应在生产 backend
    ///   出现。
    /// - Backend 同时支持有 / 无 payload 两条路径时，把
    ///   `NO_PAYLOAD_SCHEMA_ID` 跟具体 schema_id 一起列出（如
    ///   `&[NO_PAYLOAD_SCHEMA_ID, 0xCAFE_0001]`）。
    fn supported_schemas(&self) -> &[u32];

    /// 创建 action instance；coordinator 之后调
    /// [`ActionInstance::tick`] / [`ActionInstance::cancel`]。
    fn start(
        &self,
        req: &ActionRequest,
        ctx: &ActionContext<'_>,
    ) -> Result<Box<dyn ActionInstance>, ActionStartError>;
}

/// Backend 单次 action 执行的 instance。
///
/// **lifecycle**：`backend.start(req, ctx) → Box<dyn ActionInstance>`
/// → coordinator 50 Hz 调 `tick()` → 收到 `Completed` 或 `Err` 后 drop
/// instance。`cancel()` 是 cooperative —— set flag，下一次 `tick()`
/// 自查 + 收敛返 `Completed(PartialSuccess)` 或 `Err(...)`（§10 决策
/// #4）。
pub trait ActionInstance: Send {
    /// 50 Hz tick；backend 决定本 tick 是否产生新 `JointCommand`。
    ///
    /// 返 `Err(ActionFailure)` 立刻终止 action；coordinator 按
    /// failure 类型决定回 IDLE 还是进 DEGRADED（详 outline §6.5）。
    fn tick(&mut self, ctx: &mut ActionContext<'_>) -> Result<ActionTickOutput, ActionFailure>;

    /// Cooperative cancel：set flag，下一次 `tick()` 自查 + 收敛。
    /// 返 `Err` 仅当 backend 内部 cancel 路径出错（极少；典型实现总
    /// 返 `Ok`）。
    fn cancel(&mut self, ctx: &mut ActionContext<'_>) -> Result<(), ActionCancelError>;
}

// ============================================================================
// 错误枚举
// ============================================================================

/// `Coordinator::submit` 失败原因（B2 (2/3) 落地 coordinator 端时
/// 引入的具体路径来源）。
#[derive(Debug, Error)]
pub enum SubmitError {
    /// [`ActionRequest::validate`] 字段校验失败（deadline /
    /// idempotency_key / payload 长度）。
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// `[s1.profile.backends]` 没有任何 enabled backend 声明
    /// 支持本 `ActionKind`。
    #[error("no backend registered for kind {0:?}")]
    NoBackendForKind(ActionKind),
    /// 找到 backend 但其 `supported_schemas()` 不含 `payload_schema_id`。
    #[error("backend {0:?} does not support payload schema_id {1:#x}")]
    UnsupportedSchema(String, u32),
    /// B2 单 action 序列化（§10 决策 #3）：已有一个 `EXECUTING`，拒新
    /// `submit`。
    #[error("coordinator already executing action {0} (B2 single-action serialization)")]
    BusyExecuting(ActionId),
    /// Coordinator 在 DEGRADED 状态；调 `clear_error()` 才回 IDLE。
    #[error("coordinator is in DEGRADED state; clear_error required")]
    CoordinatorDegraded,
    /// 收到 SIGINT / SIGTERM 后；coordinator 不接收新 submit。
    #[error("coordinator is shutting down")]
    ShuttingDown,
}

/// `ActionBackend::start` 失败原因。**变体非穷举**（B3 起 backend 自
/// 己定义具体拒绝原因；B2 (1/3) 仅声明通用三类）。
#[derive(Debug, Error)]
pub enum ActionStartError {
    /// Backend 显式拒绝（precondition / hardware mismatch / etc）。
    #[error("backend rejected request: {0}")]
    Rejected(String),
    /// `payload` 字节流按 `payload_schema_id` 解码失败。
    #[error("payload decode failed: {0}")]
    PayloadDecode(String),
    /// `ActionContext.hardware` 跟 backend 期望不匹配（joint count /
    /// adapter capability 等）。
    #[error("hardware mismatch: {0}")]
    HardwareMismatch(String),
}

/// `ActionInstance::cancel` 失败原因（B2 (1/3) 仅声明；典型实现总返
/// `Ok` —— cooperative cancel 是 set flag，几乎不会失败）。
#[derive(Debug, Error)]
pub enum ActionCancelError {
    #[error("backend cancel hook returned error: {0}")]
    BackendError(String),
    #[error("action not currently executing")]
    NotExecuting,
}

/// `Coordinator::clear_error()` 失败原因（B2 (2/3) stub 内部 API；B4
/// 接 RPC `ClearError`）。
#[derive(Debug, Error)]
pub enum ClearError {
    /// 当前不在 DEGRADED 状态；`clear_error()` 仅 DEGRADED → IDLE
    /// 转移路径有效。
    #[error("coordinator not in DEGRADED state (current: {0})")]
    NotDegraded(String),
    /// 极少；保留兜底（state lock poison / 等）。
    #[error("internal error: {0}")]
    InternalError(String),
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ----- ActionId -----

    #[test]
    fn action_id_new_v7_returns_uuid_version_7() {
        let id = ActionId::new_v7();
        // UUIDv7：version field = 7（高 4 bit of byte 6）。
        assert_eq!(id.0.get_version_num(), 7);
    }

    #[test]
    fn action_id_two_v7_calls_are_distinct() {
        let a = ActionId::new_v7();
        let b = ActionId::new_v7();
        assert_ne!(a, b);
    }

    #[test]
    fn action_id_display_matches_uuid() {
        let raw = Uuid::now_v7();
        let id = ActionId(raw);
        assert_eq!(format!("{id}"), raw.to_string());
    }

    // ----- ActionKind -----

    #[test]
    fn action_kind_only_two_b2_variants() {
        // 锁住"B2 enum 仅 DwellMs / HoldPose"边界 —— 任何加新 variant
        // 的实现 commit 必须先回到 outline reviewer 复核（§10 决策 #1）。
        // 用 match 穷举 + 编译期检查：如果有人加新 variant，下面
        // exhaustive 的 unit test 会编不过。
        fn cover(k: ActionKind) -> u8 {
            match k {
                ActionKind::DwellMs => 0,
                ActionKind::HoldPose => 1,
            }
        }
        assert_eq!(cover(ActionKind::DwellMs), 0);
        assert_eq!(cover(ActionKind::HoldPose), 1);
    }

    #[test]
    fn action_kind_eq_and_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(ActionKind::DwellMs);
        set.insert(ActionKind::HoldPose);
        set.insert(ActionKind::DwellMs); // dup
        assert_eq!(set.len(), 2);
        assert!(set.contains(&ActionKind::DwellMs));
    }

    // ----- JointCommandBody -----

    #[test]
    fn joint_command_body_round_trip() {
        let bytes = vec![1u8, 2, 3, 4];
        let body = JointCommandBody::new(bytes.clone());
        assert_eq!(body.as_bytes(), &bytes[..]);
        assert_eq!(body.len(), 4);
        assert!(!body.is_empty());
        assert_eq!(body.into_bytes(), bytes);
    }

    #[test]
    fn joint_command_body_empty() {
        let body = JointCommandBody::new(Vec::new());
        assert!(body.is_empty());
        assert_eq!(body.len(), 0);
        assert_eq!(body.as_bytes(), &[] as &[u8]);
    }

    // ----- ActionRequest::validate -----

    fn sample_request(now_ns: u64) -> ActionRequest {
        ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::DwellMs,
            deadline_ns: now_ns + 500_000_000, // +500 ms
            priority: 10,
            payload_schema_id: NO_PAYLOAD_SCHEMA_ID,
            payload: Vec::new(),
            idempotency_key: "client-retry-key-001".into(),
            source: ActionSource::Internal,
        }
    }

    #[test]
    fn validate_happy_path() {
        let now_ns: u64 = 1_000_000_000;
        let req = sample_request(now_ns);
        req.validate(now_ns).expect("should pass");
    }

    #[test]
    fn validate_rejects_deadline_eq_now() {
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        req.deadline_ns = now_ns;
        let err = req.validate(now_ns).unwrap_err();
        assert!(
            matches!(err, SubmitError::InvalidRequest(ref s) if s.contains("deadline_ns")),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_rejects_deadline_before_now() {
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        req.deadline_ns = now_ns - 1;
        let err = req.validate(now_ns).unwrap_err();
        assert!(
            matches!(err, SubmitError::InvalidRequest(ref s) if s.contains("deadline_ns")),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_rejects_empty_idempotency_key() {
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        req.idempotency_key.clear();
        let err = req.validate(now_ns).unwrap_err();
        assert!(
            matches!(err, SubmitError::InvalidRequest(ref s) if s.contains("idempotency_key")),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_rejects_oversize_idempotency_key() {
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        req.idempotency_key = "x".repeat(MAX_IDEMPOTENCY_KEY_LEN + 1);
        let err = req.validate(now_ns).unwrap_err();
        assert!(
            matches!(err, SubmitError::InvalidRequest(ref s)
                if s.contains("idempotency_key") && s.contains("MAX_IDEMPOTENCY_KEY_LEN")),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_rejects_oversize_payload() {
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        req.payload = vec![0u8; MAX_PAYLOAD_LEN + 1];
        let err = req.validate(now_ns).unwrap_err();
        assert!(
            matches!(err, SubmitError::InvalidRequest(ref s)
                if s.contains("payload") && s.contains("MAX_PAYLOAD_LEN")),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_accepts_payload_at_max_len() {
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        // 显式给非哨兵 schema_id —— 新 reviewer 规则要求
        // `schema_id == NO_PAYLOAD_SCHEMA_ID` 时 payload 必须为空。
        req.payload_schema_id = 0xCAFE_0001;
        req.payload = vec![0u8; MAX_PAYLOAD_LEN];
        req.validate(now_ns).expect("len == MAX must pass");
    }

    #[test]
    fn validate_accepts_idempotency_key_at_max_len() {
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        req.idempotency_key = "x".repeat(MAX_IDEMPOTENCY_KEY_LEN);
        req.validate(now_ns).expect("len == MAX must pass");
    }

    #[test]
    fn validate_rejects_no_payload_schema_id_with_non_empty_payload() {
        // reviewer 阻断 1 锁定：客户端不能用哨兵 schema_id 偷渡 bytes。
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        assert_eq!(req.payload_schema_id, NO_PAYLOAD_SCHEMA_ID);
        req.payload = vec![1, 2, 3];
        let err = req.validate(now_ns).unwrap_err();
        assert!(
            matches!(err, SubmitError::InvalidRequest(ref s)
                if s.contains("NO_PAYLOAD_SCHEMA_ID") && s.contains("empty payload")),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_accepts_no_payload_schema_id_with_empty_payload() {
        // 哨兵 schema_id + 空 payload 是 MockDwell / HoldPose 的合法路径。
        let now_ns: u64 = 1_000_000_000;
        let req = sample_request(now_ns);
        assert_eq!(req.payload_schema_id, NO_PAYLOAD_SCHEMA_ID);
        assert!(req.payload.is_empty());
        req.validate(now_ns).expect("must pass");
    }

    #[test]
    fn validate_accepts_non_sentinel_schema_id_with_non_empty_payload() {
        // 真 backend（B3 起 trajectory / policy chunk）的合法路径。
        let now_ns: u64 = 1_000_000_000;
        let mut req = sample_request(now_ns);
        req.payload_schema_id = 0xCAFE_0001;
        req.payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
        req.validate(now_ns).expect("must pass");
    }

    #[test]
    fn no_payload_schema_id_is_zero() {
        // 锁住 "默认 / unset = 哨兵" 跟 RPC `CommandMeta.payload_schema_id`
        // 字段未设置默认值的对齐关系；改这个常量等于改 wire 协议。
        assert_eq!(NO_PAYLOAD_SCHEMA_ID, 0);
    }

    // ----- ActionOutcome / ActionFailure / SubmitError Display -----

    #[test]
    fn action_outcome_equality() {
        assert_eq!(ActionOutcome::Success, ActionOutcome::Success);
        let warn = ActionOutcome::PartialSuccess {
            warning: "trajectory clamped".into(),
        };
        let warn2 = ActionOutcome::PartialSuccess {
            warning: "trajectory clamped".into(),
        };
        assert_eq!(warn, warn2);
        assert_ne!(warn, ActionOutcome::Success);
    }

    #[test]
    fn action_failure_display_covers_all_variants() {
        let cases: &[(ActionFailure, &str)] = &[
            (ActionFailure::DeadlineExceeded, "deadline exceeded"),
            (
                ActionFailure::PreconditionFailed("limits".into()),
                "precondition failed: limits",
            ),
            (
                ActionFailure::AdapterError("serial timeout".into()),
                "adapter error: serial timeout",
            ),
            (
                ActionFailure::SafetyAbort("estop fired".into()),
                "safety abort: estop fired",
            ),
            (
                ActionFailure::InternalError("oops".into()),
                "internal error: oops",
            ),
        ];
        for (failure, expected) in cases {
            assert_eq!(failure.to_string(), *expected);
        }
    }

    #[test]
    fn submit_error_display_main_variants() {
        let cases: &[(SubmitError, &str)] = &[
            (
                SubmitError::InvalidRequest("foo".into()),
                "invalid request: foo",
            ),
            (
                SubmitError::NoBackendForKind(ActionKind::DwellMs),
                "no backend registered for kind DwellMs",
            ),
            (
                SubmitError::CoordinatorDegraded,
                "coordinator is in DEGRADED state; clear_error required",
            ),
            (SubmitError::ShuttingDown, "coordinator is shutting down"),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), *expected);
        }
    }

    #[test]
    fn submit_error_unsupported_schema_renders_hex() {
        let err = SubmitError::UnsupportedSchema("rl_onnx".into(), 0xABCD_0001);
        assert_eq!(
            err.to_string(),
            "backend \"rl_onnx\" does not support payload schema_id 0xabcd0001"
        );
    }

    #[test]
    fn submit_error_busy_executing_renders_uuid() {
        let id = ActionId::new_v7();
        let err = SubmitError::BusyExecuting(id);
        let s = err.to_string();
        assert!(
            s.contains(&id.to_string()),
            "expected UUID in display, got {s}"
        );
        assert!(s.contains("single-action"));
    }
}
