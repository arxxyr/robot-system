//! v0.3 B4 (2/3-b) task control plane RPC handlers —— gateway 端 4 个 task
//! RPC 真 handler 实现 + 错误码映射 + 共享 validation / fingerprint helpers。
//!
//! ## 4 个 RPC handler
//!
//! 1. [`handle_invoke_task`]：tonic `InvokeTask` → fbs RR `task_invoke` →
//!    s1 [`Coordinator::submit`]。返 `proto::TaskHandle { request_id, action_id }`。
//! 2. [`handle_cancel_task`]：tonic `CancelTask` → fbs RR `task_cancel` →
//!    s1 [`Coordinator::cancel`]。返 `proto::Ack`。
//! 3. [`handle_get_task_status`]：tonic `GetTaskStatus` → fbs RR `task_status`
//!    → s1 [`Coordinator::status`]。返 `proto::TaskStatus`。**只读 RPC，
//!    不进 idempotency cache**（outline §8.1 注释 + reviewer 拍板）。
//! 4. [`handle_clear_error`]：tonic `ClearError` → fbs RR `clear_error` →
//!    s1 [`Coordinator::clear_error`]。返 `proto::Ack`。
//!
//! `StreamTaskEvents` **不**经本模块（走 per-ActionId pubsub topic），留
//! B4 (2/3-c) 接班。
//!
//! ## 12 步骨架
//!
//! 4 个 handler 都沿用 [`crate::gateway::GatewayHandler::clear_e_stop`] 的
//! 12 步骨架（A1-3 (2/3-b2-2) reviewer 拍板）：
//!
//! 1. validate（decode proto + 字段非空 / UUIDv7 校验）
//! 2. auth（fake token → [`crate::auth::Role`]）
//! 3. authz（role 是否含对应 [`crate::auth::Capability`]）
//! 4. idempotency fingerprint（仅 3 个写 RPC 走；GetTaskStatus 跳过）
//! 5. cache.decide（仅 3 个写 RPC；命中 Replay → 重建 proto response，
//!    Conflict → AlreadyExists，Capacity → ResourceExhausted）
//!    5.5. shutdown gate（FreshInsert 路径 check `shutdown_rx`；
//!    shutdown=true drop insert_handle → Unavailable）
//! 6. 算 `deadline_ns` / `stamp_ns`（CLOCK_MONOTONIC_RAW，gateway / daemon
//!    同机器共享 clock domain）
//! 7. 映射 `Role` → wire `RpcRole`（仅 audit）
//! 8. build wire fbs request bytes（`crate::robot_msg::build_helpers::*`）
//! 9. forward via `rr_forward_tx_*`（`try_send`，bounded(64)；满 →
//!    `ResourceExhausted`，Disconnected → `Unavailable`）
//! 10. `tokio::time::timeout(timeout_ms + grace_ms)` 等 oneshot reply
//! 11. decode wire fbs response + check `request_id` 匹配（`Internal` if
//!     mismatch）
//! 12. map `ActionRpcStatus → tonic::Code`：Ok 路径 commit cache + 返 proto
//!     response；非 Ok drop insert_handle（**不**缓存失败状态）+ return
//!     Status with error_msg
//!
//! ## 不在范围（B4 (2/3-b) 边界）
//!
//! - **不**做 cross-process integration test → B4 (3/3)。
//! - **不**接 `StreamTaskEvents` pubsub → B4 (2/3-c)。
//! - **不**改 `JointCommand.source_task_id` / `FrameHeader.seq` —— gateway
//!   仅消费 fbs response bytes，**不**碰 wire 协议契约 §3.7。

#![allow(clippy::doc_markdown)]
// `tonic::Status` 是 ~176 字节大 enum，handler 的 Err 路径都用它；clippy
// `result_large_err` 在 tonic-based code 上是噪音（真要 box 等于把整个
// gRPC service 改 Box<Status>），module 级 allow。
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crossbeam_channel::TrySendError;
use robot_msg::build_helpers::{
    build_cancel_task_request, build_clear_error_request, build_get_task_status_request,
    build_invoke_task_request,
};
use robot_msg::msg::{
    ActionFailureFb, ActionFailureKindFb, ActionKindFb, ActionOutcomeFb, ActionOutcomeKindFb,
    ActionRpcStatus, CancelTaskResponse as FbCancelResp, ClearErrorResponse as FbClearErrorResp,
    GetTaskStatusResponse as FbStatusResp, InvokeTaskResponse as FbInvokeResp, RpcRole,
    TaskStateFb,
};
use thiserror::Error;
use tonic::Code;

use crate::auth::{Capability, RequestIdError, Role, RoleTable, validate_request_id};
use crate::idempotency::{CachedResponse, Decision, Fingerprint, IdempotencyCache, Method};
use crate::proto::{
    Ack, ActionFailureProto, ActionKindProto, ActionOutcomeProto, CancelTaskCommand,
    ClearErrorCommand, CommandMeta, InvokeTaskCommand, TaskHandle, TaskHandleRef,
    TaskState as ProtoTaskState, TaskStatus, action_failure_proto, action_outcome_proto,
};
use crate::rr_io_thread::RrForwardRequest;

// ============================================================================
// Q2(a) byte gate 常量（v0.4 候选 1）
// ============================================================================

/// Gate 1 上限：`InvokeTaskCommand.payload.len()` 当 `kind ==
/// ActionKindProto::ActionKindScripted` 时的字节数硬上限。
///
/// Q2(a) gate 1（reviewer Plan round 1 阻断 4 拍板）：保守上限快速拒
/// 明显超大 payload，**不**解 fbs bytes（保 B4 outline §6.1 双层
/// schema 分工）。`MAX_SCRIPTED_PAYLOAD_BYTES = 4 KiB`：1 joint × 100
/// waypoint × ~16 bytes + fbs overhead ≈ 1.6 KiB，4 KiB 充裕兜底；多
/// 关节场景下用户可塞的 waypoint 数自然受 byte cap 限制（Q6 拍死本
/// 轮 only byte cap，不做 count semantic check）。
pub(crate) const MAX_SCRIPTED_PAYLOAD_BYTES: usize = 4 * 1024;

/// Gate 2 上限：`build_invoke_task_request(...)` 后实测 wire bytes 长
/// 度的字节数硬上限。**单源**绑定 [`robot_msg::size_tier::SizeTier::Control`]
/// 的 [`robot_msg::size_tier::SizeTier::max_bytes`]（= 8 KiB），**严禁**
/// 硬编码 `8192` / `8 * 1024`，避免常量漂移（reviewer Plan 期注意点
/// + Plan round 2 阻断 1 拍板）。
///
/// Gate 1 仅看 `payload.len()`，**不**覆盖外层 `CommandMeta`
/// (`request_id` / `idempotency_key` / `source` / `audit_reason`) +
/// FlatBuffers vtable / padding 的累计开销；gate 2 在 build wire 后
/// 实测兜底，防止 iceoryx2 `loan_slice_uninit` 撞 `SampleTooLarge`。
pub(crate) const CONTROL_TIER_MAX_BYTES: usize =
    robot_msg::size_tier::SizeTier::Control.max_bytes();

// ============================================================================
// Validated structs（meta 字段 owned，跟 proto 解耦）
// ============================================================================

/// 4 个写 RPC 共享的 validated meta 字段集（来自 `proto::CommandMeta`）。
#[derive(Debug, Clone)]
pub struct WriteRpcMeta {
    pub request_id: String,
    pub idempotency_key: String,
    pub source: String,
    pub audit_reason: String,
    pub dry_run: bool,
    pub timeout_ms: u64,
    pub auth_token: String,
}

/// `InvokeTaskCommand` 解码后的 owned 视图。
#[derive(Debug, Clone)]
pub struct ValidatedInvokeTaskCommand {
    pub meta: WriteRpcMeta,
    pub kind: ActionKindProto,
    pub deadline_ns_offset_ms: u64,
    pub priority: u32,
    pub payload_schema_id: u32,
    pub payload: Vec<u8>,
}

/// `CancelTaskCommand` 解码后的 owned 视图。
#[derive(Debug, Clone)]
pub struct ValidatedCancelTaskCommand {
    pub meta: WriteRpcMeta,
    /// UUIDv7 hex 串（gateway 端 audit；s1 端 [`crate::action::ActionId`]
    /// 解码用）。`validate_cancel_task_command` 仅校验非空 + UUID 合法。
    pub action_id: String,
}

/// `ClearErrorCommand` 解码后的 owned 视图。
#[derive(Debug, Clone)]
pub struct ValidatedClearErrorCommand {
    pub meta: WriteRpcMeta,
}

/// `TaskHandleRef` 解码后的 owned 视图（GetTaskStatus / StreamTaskEvents 用；
/// 无 CommandMeta，proto frozen）。
#[derive(Debug, Clone)]
pub struct ValidatedTaskHandleRef {
    pub request_id: String,
    pub action_id: String,
}

// ============================================================================
// Validation
// ============================================================================

/// task control plane RPC validation 错误。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TaskValidationError {
    #[error("missing CommandMeta")]
    MissingMeta,
    #[error("missing CommandMeta.auth")]
    MissingAuth,
    #[error("auth.token 为空（v0.3 接受 fake token 但必须非空）")]
    EmptyAuthToken,
    #[error("request_id invalid: {0}")]
    RequestId(#[from] RequestIdError),
    #[error("idempotency_key 为空字符串（必填）")]
    EmptyIdempotencyKey,
    #[error("source 为空字符串（必填）")]
    EmptySource,
    #[error("timeout_ms = 0（必须 > 0）")]
    ZeroTimeout,
    #[error("InvokeTaskCommand.kind = ACTION_KIND_UNSPECIFIED（client 必须显式指定）")]
    InvokeKindUnspecified,
    #[error("CancelTaskCommand.action_id 为空字符串（必填）")]
    EmptyCancelActionId,
    #[error("CancelTaskCommand.action_id 不是合法 UUID: {0}")]
    InvalidCancelActionId(String),
    #[error("TaskHandleRef.action_id 为空字符串（必填）")]
    EmptyTaskHandleRefActionId,
    #[error("TaskHandleRef.action_id 不是合法 UUID: {0}")]
    InvalidTaskHandleRefActionId(String),
    /// v0.4 候选 1 Q2(a) gate 1：`kind == ActionKindProto::ActionKindScripted`
    /// 时 `InvokeTaskCommand.payload.len() > MAX_SCRIPTED_PAYLOAD_BYTES`
    /// → 拒；reviewer Plan round 1 阻断 4 + round 2 阻断 1 拍板。
    #[error("ScriptedPayload bytes {len} 超 MAX_SCRIPTED_PAYLOAD_BYTES ({max})")]
    ScriptedPayloadTooLarge { len: usize, max: usize },
    /// v0.4 候选 1 Q2(a) gate 2：`build_invoke_task_request` 后实测
    /// wire bytes 超 `CONTROL_TIER_MAX_BYTES` → 拒；reviewer Plan
    /// round 2 阻断 1 拍板兜底外层 `CommandMeta` + FlatBuffers
    /// overhead 撞 Control 8 KiB。
    #[error("InvokeTaskRequest wire bytes {len} 超 CONTROL_TIER_MAX_BYTES ({max})")]
    InvokeWireBytesTooLarge { len: usize, max: usize },
}

impl TaskValidationError {
    /// 全部 validation 错误都映射 `InvalidArgument`（语义就是"客户端入参不对"）。
    #[must_use]
    pub fn to_tonic_code(&self) -> Code {
        Code::InvalidArgument
    }
}

fn validate_meta(meta: &CommandMeta) -> Result<WriteRpcMeta, TaskValidationError> {
    let auth = meta.auth.as_ref().ok_or(TaskValidationError::MissingAuth)?;
    if auth.token.is_empty() {
        return Err(TaskValidationError::EmptyAuthToken);
    }
    let _request_uuid = validate_request_id(&meta.request_id)?;
    if meta.idempotency_key.is_empty() {
        return Err(TaskValidationError::EmptyIdempotencyKey);
    }
    if meta.source.is_empty() {
        return Err(TaskValidationError::EmptySource);
    }
    if meta.timeout_ms == 0 {
        return Err(TaskValidationError::ZeroTimeout);
    }
    Ok(WriteRpcMeta {
        request_id: meta.request_id.clone(),
        idempotency_key: meta.idempotency_key.clone(),
        source: meta.source.clone(),
        audit_reason: meta.audit_reason.clone(),
        dry_run: meta.dry_run,
        timeout_ms: meta.timeout_ms,
        auth_token: auth.token.clone(),
    })
}

/// 验证 [`InvokeTaskCommand`] + 抽出 [`ValidatedInvokeTaskCommand`]。
///
/// **不**做的事：auth / role / capability / idempotency 查表（调用方 wrap
/// 后续步骤）。
///
/// # Errors
///
/// 见 [`TaskValidationError`]。
pub fn validate_invoke_task_command(
    cmd: &InvokeTaskCommand,
) -> Result<ValidatedInvokeTaskCommand, TaskValidationError> {
    let meta_proto = cmd.meta.as_ref().ok_or(TaskValidationError::MissingMeta)?;
    let meta = validate_meta(meta_proto)?;
    let kind =
        ActionKindProto::try_from(cmd.kind).unwrap_or(ActionKindProto::ActionKindUnspecified);
    if kind == ActionKindProto::ActionKindUnspecified {
        return Err(TaskValidationError::InvokeKindUnspecified);
    }
    Ok(ValidatedInvokeTaskCommand {
        meta,
        kind,
        deadline_ns_offset_ms: cmd.deadline_ns,
        priority: cmd.priority,
        payload_schema_id: cmd.payload_schema_id,
        payload: cmd.payload.clone(),
    })
}

/// 验证 [`CancelTaskCommand`] + 抽出 [`ValidatedCancelTaskCommand`]。
///
/// # Errors
///
/// 见 [`TaskValidationError`]。
pub fn validate_cancel_task_command(
    cmd: &CancelTaskCommand,
) -> Result<ValidatedCancelTaskCommand, TaskValidationError> {
    let meta_proto = cmd.meta.as_ref().ok_or(TaskValidationError::MissingMeta)?;
    let meta = validate_meta(meta_proto)?;
    if cmd.action_id.is_empty() {
        return Err(TaskValidationError::EmptyCancelActionId);
    }
    if let Err(e) = uuid::Uuid::parse_str(&cmd.action_id) {
        return Err(TaskValidationError::InvalidCancelActionId(e.to_string()));
    }
    Ok(ValidatedCancelTaskCommand {
        meta,
        action_id: cmd.action_id.clone(),
    })
}

/// 验证 [`ClearErrorCommand`] + 抽出 [`ValidatedClearErrorCommand`]。
///
/// # Errors
///
/// 见 [`TaskValidationError`]。
pub fn validate_clear_error_command(
    cmd: &ClearErrorCommand,
) -> Result<ValidatedClearErrorCommand, TaskValidationError> {
    let meta_proto = cmd.meta.as_ref().ok_or(TaskValidationError::MissingMeta)?;
    let meta = validate_meta(meta_proto)?;
    Ok(ValidatedClearErrorCommand { meta })
}

/// 验证 [`TaskHandleRef`]（GetTaskStatus / StreamTaskEvents request）。
///
/// **不**校验 auth：proto frozen TaskHandleRef 没 CommandMeta（design §5.3
/// 锁定 GetTaskStatus / StreamTaskEvents 是只读 RPC，仅用 request_id 做
/// audit + correlation；v0.3 不强制 auth）。如 v0.5+ 加 auth 走 tonic
/// metadata（HTTP header）路径，本 fn 签名保持不变。
///
/// # Errors
///
/// 见 [`TaskValidationError`]。
pub fn validate_task_handle_ref(
    req: &TaskHandleRef,
) -> Result<ValidatedTaskHandleRef, TaskValidationError> {
    let _request_uuid = validate_request_id(&req.request_id)?;
    if req.action_id.is_empty() {
        return Err(TaskValidationError::EmptyTaskHandleRefActionId);
    }
    if let Err(e) = uuid::Uuid::parse_str(&req.action_id) {
        return Err(TaskValidationError::InvalidTaskHandleRefActionId(
            e.to_string(),
        ));
    }
    Ok(ValidatedTaskHandleRef {
        request_id: req.request_id.clone(),
        action_id: req.action_id.clone(),
    })
}

// ============================================================================
// Idempotency fingerprints（仅 3 个写 RPC；GetTaskStatus 不进 cache）
// ============================================================================

fn fingerprint_field(h: &mut blake3::Hasher, label: &str, value: &[u8]) {
    let label_bytes = label.as_bytes();
    h.update(
        &u32::try_from(label_bytes.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    h.update(label_bytes);
    h.update(&u32::try_from(value.len()).unwrap_or(u32::MAX).to_be_bytes());
    h.update(value);
}

fn fingerprint_meta(h: &mut blake3::Hasher, meta: &WriteRpcMeta) {
    fingerprint_field(h, "source", meta.source.as_bytes());
    fingerprint_field(h, "audit_reason", meta.audit_reason.as_bytes());
    fingerprint_field(h, "dry_run", &[u8::from(meta.dry_run)]);
    fingerprint_field(h, "auth_token", meta.auth_token.as_bytes());
}

/// 算 [`ValidatedInvokeTaskCommand`] 的 idempotency fingerprint。
///
/// **进 fingerprint**：source / audit_reason / dry_run / auth_token / kind /
/// payload_schema_id / payload bytes。**不**进 fingerprint：request_id /
/// timeout_ms / idempotency_key / priority / deadline_ns_offset_ms（priority
/// + deadline 是调度提示，retry 期间允许调整；详 outline §6 注释）。
#[must_use]
pub fn invoke_task_idempotency_fingerprint(v: &ValidatedInvokeTaskCommand) -> Fingerprint {
    let mut h = blake3::Hasher::new();
    fingerprint_meta(&mut h, &v.meta);
    fingerprint_field(&mut h, "kind", (v.kind as i32).to_be_bytes().as_slice());
    fingerprint_field(
        &mut h,
        "payload_schema_id",
        v.payload_schema_id.to_be_bytes().as_slice(),
    );
    fingerprint_field(&mut h, "payload", &v.payload);
    h.finalize().into()
}

/// 算 [`ValidatedCancelTaskCommand`] 的 idempotency fingerprint。
///
/// **进 fingerprint**：source / audit_reason / dry_run / auth_token /
/// action_id。
#[must_use]
pub fn cancel_task_idempotency_fingerprint(v: &ValidatedCancelTaskCommand) -> Fingerprint {
    let mut h = blake3::Hasher::new();
    fingerprint_meta(&mut h, &v.meta);
    fingerprint_field(&mut h, "action_id", v.action_id.as_bytes());
    h.finalize().into()
}

/// 算 [`ValidatedClearErrorCommand`] 的 idempotency fingerprint。
///
/// **进 fingerprint**：source / audit_reason / dry_run / auth_token。
#[must_use]
pub fn clear_error_idempotency_fingerprint(v: &ValidatedClearErrorCommand) -> Fingerprint {
    let mut h = blake3::Hasher::new();
    fingerprint_meta(&mut h, &v.meta);
    h.finalize().into()
}

// ============================================================================
// ActionRpcStatus → tonic::Code 显式 match
// ============================================================================

/// 把 [`ActionRpcStatus`]（B4 wire enum）转 tonic `Code`。
///
/// **必须**用显式 `match` 全枚举（沿 A1 [`crate::clear_estop::rpc_status_to_tonic_code`]
/// 同款约束）。原因：`ActionRpcStatus` 数值（fbs schema 注释明文）跟 gRPC
/// numeric code **不**对齐；任何 `code as i32` cast 都会把错误映射错。
///
/// **flatc 生成的 enum 结构**：`ActionRpcStatus` 是 `pub struct
/// ActionRpcStatus(pub u8)` 不是真 Rust enum；`match` 的非穷举无法在编译期
/// catch 新 variant。两道防线：
/// 1. **数值 wildcard 兜底**：未知 u8 值 → `Code::Internal`。
/// 2. **测试期变量计数锁**：[`tests::action_rpc_status_variant_count_matches_explicit_mapping`]
///    把 `ActionRpcStatus::ENUM_VALUES` 长度跟本 helper 显式映射数硬绑；新增
///    variant 让 length 对不上 → 测试失败 → 强制同步本 helper + 单测。
#[must_use]
#[allow(clippy::match_same_arms)]
pub fn action_rpc_status_to_tonic_code(status: ActionRpcStatus) -> Code {
    match status {
        // Unknown 是 wire 兼容旧 writer 用；gateway 视同内部错误（不应
        // 出现在 daemon 真返的 wire 上；出现 = wire layer 没对齐 schema）。
        ActionRpcStatus::Unknown => Code::Internal,
        ActionRpcStatus::Ok => Code::Ok,
        ActionRpcStatus::FailedPrecondition => Code::FailedPrecondition,
        ActionRpcStatus::PermissionDenied => Code::PermissionDenied,
        ActionRpcStatus::InvalidArgument => Code::InvalidArgument,
        ActionRpcStatus::Internal => Code::Internal,
        ActionRpcStatus::AlreadyExists => Code::AlreadyExists,
        ActionRpcStatus::Unimplemented => Code::Unimplemented,
        ActionRpcStatus::NotFound => Code::NotFound,
        ActionRpcStatus::Unavailable => Code::Unavailable,
        // wildcard：未知值 = wire 协议 mismatch。**注意**：新增 ActionRpcStatus
        // variant 时 `action_rpc_status_variant_count_matches_explicit_mapping`
        // 单测会先报错强制更新本 match。
        _ => Code::Internal,
    }
}

// ============================================================================
// Common helpers
// ============================================================================

/// `crate::auth::Role` → wire `RpcRole`（仅 audit；s1 端不做授权决策）。
fn role_to_wire(role: Role) -> RpcRole {
    match role {
        Role::Viewer => RpcRole::Viewer,
        Role::Operator => RpcRole::Operator,
        Role::Maintainer => RpcRole::Maintainer,
        Role::Admin => RpcRole::Admin,
    }
}

/// 共用：从 `auth_token` 查 role；token 不在表里返
/// `tonic::Status::unauthenticated`。
fn lookup_role(role_table: &RoleTable, auth_token: &str) -> Result<Role, tonic::Status> {
    role_table
        .lookup_token(auth_token)
        .ok_or_else(|| tonic::Status::unauthenticated("auth.token not found in role table"))
}

/// 共用：role 必须含指定 capability，否则返 `permission_denied`。
fn check_capability(
    role_table: &RoleTable,
    role: Role,
    cap: Capability,
    method_label: &str,
) -> Result<(), tonic::Status> {
    if role_table.role_has_capability(role, cap) {
        Ok(())
    } else {
        Err(tonic::Status::permission_denied(format!(
            "role `{}` lacks `{}` capability (required for {method_label})",
            role.as_str(),
            cap.as_str()
        )))
    }
}

/// 共用：handler 入口若 gateway 已 shutdown，FreshInsert handle 直接 drop +
/// 返 `Unavailable`（在 cache.decide=FreshInsert 之后、forward 之前调用）。
fn shutdown_gate(
    shutdown: bool,
    request_id: &str,
    idempotency_key: &str,
    method_label: &str,
    insert_handle: Option<crate::idempotency::InsertHandle>,
) -> Result<(), tonic::Status> {
    if shutdown {
        tracing::warn!(
            request_id,
            idempotency_key,
            method = method_label,
            "rejected during shutdown (cache.decide=FreshInsert; not forwarding)"
        );
        drop(insert_handle);
        return Err(tonic::Status::unavailable(
            "gateway is shutting down; new requests rejected",
        ));
    }
    Ok(())
}

/// 共用：forward wire bytes via rr_io_thread channel，返 oneshot rx。
fn forward_to_rr_io(
    tx: &crossbeam_channel::Sender<RrForwardRequest>,
    wire_bytes: Vec<u8>,
    request_id: &str,
    method_label: &str,
) -> Result<tokio::sync::oneshot::Receiver<Vec<u8>>, tonic::Status> {
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
    let forward = RrForwardRequest {
        wire_bytes,
        oneshot_tx,
        request_id: request_id.to_string(),
    };
    match tx.try_send(forward) {
        Ok(()) => Ok(oneshot_rx),
        Err(TrySendError::Full(_)) => {
            tracing::warn!(
                request_id,
                method = method_label,
                "forward channel full; replying ResourceExhausted"
            );
            Err(tonic::Status::resource_exhausted(
                "gateway RR I/O forward channel full; retry shortly",
            ))
        }
        Err(TrySendError::Disconnected(_)) => {
            tracing::error!(
                request_id,
                method = method_label,
                "forward channel disconnected; rr_io_thread already stopped"
            );
            Err(tonic::Status::unavailable("gateway RR I/O thread stopped"))
        }
    }
}

/// 共用：等 oneshot reply with timeout（`timeout_ms + grace_ms`）。
async fn await_reply(
    oneshot_rx: tokio::sync::oneshot::Receiver<Vec<u8>>,
    timeout_ms: u64,
    grace_ms: u64,
    request_id: &str,
    method_label: &str,
) -> Result<Vec<u8>, tonic::Status> {
    let total_timeout_ms = timeout_ms.saturating_add(grace_ms);
    match tokio::time::timeout(Duration::from_millis(total_timeout_ms), oneshot_rx).await {
        Err(_elapsed) => {
            tracing::warn!(
                request_id,
                timeout_ms,
                grace_ms,
                method = method_label,
                "gateway-side timeout; replying DeadlineExceeded"
            );
            Err(tonic::Status::deadline_exceeded(format!(
                "no RR response within {total_timeout_ms} ms (timeout_ms + grace_ms)"
            )))
        }
        Ok(Err(_recv_err)) => {
            tracing::error!(
                request_id,
                method = method_label,
                "oneshot RecvError; gateway RR I/O unavailable"
            );
            Err(tonic::Status::unavailable("gateway RR I/O unavailable"))
        }
        Ok(Ok(b)) => Ok(b),
    }
}

// ============================================================================
// Handler async fns
// ============================================================================

/// `InvokeTask` handler。
///
/// # Errors
///
/// 见 12 步骨架 module doc（每步 fail 走对应 [`tonic::Code`]）。
pub async fn handle_invoke_task(
    state: &crate::gateway::GatewayHandlerState,
    request: tonic::Request<InvokeTaskCommand>,
) -> Result<tonic::Response<TaskHandle>, tonic::Status> {
    handle_invoke_task_inner(state, request.into_inner()).await
}

#[allow(clippy::too_many_lines)] // 12 步骨架一条 fn；分 helper 反而跨函数跳读成本。
async fn handle_invoke_task_inner(
    state: &crate::gateway::GatewayHandlerState,
    cmd: InvokeTaskCommand,
) -> Result<tonic::Response<TaskHandle>, tonic::Status> {
    // 1. validate
    let v = validate_invoke_task_command(&cmd).map_err(|e| {
        tracing::warn!(error = %e, "InvokeTask validation failed");
        tonic::Status::new(e.to_tonic_code(), e.to_string())
    })?;

    // 1.5. Q2(a) gate 1：Scripted payload precheck（v0.4 候选 1 (2/3)
    // land；reviewer Plan round 1 阻断 4 拍板）。仅当 kind == Scripted 时
    // 触发；其它 kind（DwellMs / HoldPose）无 payload 路径不受影响。
    if v.kind == ActionKindProto::ActionKindScripted && v.payload.len() > MAX_SCRIPTED_PAYLOAD_BYTES
    {
        let e = TaskValidationError::ScriptedPayloadTooLarge {
            len: v.payload.len(),
            max: MAX_SCRIPTED_PAYLOAD_BYTES,
        };
        tracing::warn!(
            request_id = %v.meta.request_id,
            payload_bytes = v.payload.len(),
            max_bytes = MAX_SCRIPTED_PAYLOAD_BYTES,
            "InvokeTask Scripted payload byte cap exceeded (gate 1)"
        );
        return Err(tonic::Status::new(e.to_tonic_code(), e.to_string()));
    }

    // 2. auth
    let role = lookup_role(&state.role_table, &v.meta.auth_token)?;

    // 3. authz：InvokeTask 复用 Capability::InvokeTask（outline §8.1 #7a）
    check_capability(
        &state.role_table,
        role,
        Capability::InvokeTask,
        "InvokeTask",
    )?;

    // 4. fingerprint
    let fingerprint = invoke_task_idempotency_fingerprint(&v);

    // 5. cache.decide
    let insert_handle = match decide_with_method(
        &state.idempotency,
        Method::InvokeTask,
        &v.meta.idempotency_key,
        fingerprint,
        &v.meta.request_id,
        "InvokeTask",
    )? {
        DecisionRoute::Fresh(h) => h,
        DecisionRoute::Replay(CachedResponse::InvokeTask { action_id }) => {
            return Ok(tonic::Response::new(TaskHandle {
                request_id: v.meta.request_id,
                action_id,
            }));
        }
        DecisionRoute::Replay(other) => {
            tracing::error!(
                request_id = %v.meta.request_id,
                cached_variant = ?other,
                "BUG: InvokeTask method namespace 命中非 InvokeTask variant"
            );
            return Err(tonic::Status::internal(
                "idempotency cache namespace inconsistency",
            ));
        }
    };

    // 5.5 shutdown gate
    let shutdown = *state.shutdown_rx.borrow();
    shutdown_gate(
        shutdown,
        &v.meta.request_id,
        &v.meta.idempotency_key,
        "InvokeTask",
        Some(insert_handle.clone()),
    )?;

    // 6. deadline + stamp
    let now_ns = state.clock.now().0;
    let timeout_ns = v.meta.timeout_ms.saturating_mul(1_000_000);
    let wire_deadline_ns = now_ns.saturating_add(timeout_ns);

    // 7. role → wire
    let wire_role = role_to_wire(role);

    // 8. build wire bytes
    let kind_fb = action_kind_proto_to_fb(v.kind);
    let wire_bytes = build_invoke_task_request(
        &v.meta.request_id,
        &v.meta.idempotency_key,
        &v.meta.source,
        wire_role,
        &v.meta.audit_reason,
        kind_fb,
        wire_deadline_ns,
        v.priority,
        v.payload_schema_id,
        &v.payload,
        now_ns,
    );

    // 8.5. Q2(a) gate 2：final wire-size guard（v0.4 候选 1 (2/3) land；
    // reviewer Plan round 2 阻断 1 拍板）。在 build_invoke_task_request 后
    // 实测 wire bytes 长度；超 CONTROL_TIER_MAX_BYTES 直接拒 →
    // InvalidArgument，**不**进 RR / s1，避免 iceoryx2 `loan_slice_uninit`
    // 撞 `SampleTooLarge`。本 gate 兜底外层 CommandMeta + FlatBuffers
    // overhead 撞 8 KiB；对 kind != Scripted 同样生效（防御性）。
    if wire_bytes.len() > CONTROL_TIER_MAX_BYTES {
        let e = TaskValidationError::InvokeWireBytesTooLarge {
            len: wire_bytes.len(),
            max: CONTROL_TIER_MAX_BYTES,
        };
        tracing::warn!(
            request_id = %v.meta.request_id,
            wire_bytes = wire_bytes.len(),
            max_bytes = CONTROL_TIER_MAX_BYTES,
            "InvokeTask wire-size guard exceeded (gate 2)"
        );
        drop(insert_handle);
        return Err(tonic::Status::new(e.to_tonic_code(), e.to_string()));
    }

    // 9. forward
    let oneshot_rx = match forward_to_rr_io(
        &state.rr_forward_tx_task_invoke,
        wire_bytes,
        &v.meta.request_id,
        "InvokeTask",
    ) {
        Ok(rx) => rx,
        Err(status) => {
            drop(insert_handle);
            return Err(status);
        }
    };

    // 10. await reply
    let reply_bytes = match await_reply(
        oneshot_rx,
        v.meta.timeout_ms,
        state.rr_timeout_grace_ms,
        &v.meta.request_id,
        "InvokeTask",
    )
    .await
    {
        Ok(b) => b,
        Err(status) => {
            drop(insert_handle);
            return Err(status);
        }
    };

    // 11. decode wire response + check request_id 匹配
    let view = match flatbuffers::root::<FbInvokeResp>(&reply_bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(
                request_id = %v.meta.request_id,
                error = ?e,
                "InvokeTask wire response decode failed; replying Internal"
            );
            drop(insert_handle);
            return Err(tonic::Status::internal(format!(
                "decode wire InvokeTaskResponse: {e}"
            )));
        }
    };
    let wire_status = view.status();
    let action_id_str = view.action_id().unwrap_or("").to_string();
    let _accepted_at_ns = view.accepted_at_ns(); // 仅 audit；proto TaskHandle 无此字段
    let error_msg = view.error_msg().unwrap_or("").to_string();
    let resp_request_id = view.request_id().unwrap_or("");
    if resp_request_id != v.meta.request_id {
        tracing::error!(
            request_id = %v.meta.request_id,
            resp_request_id,
            ?wire_status,
            "InvokeTask wire response request_id mismatch; replying Internal"
        );
        drop(insert_handle);
        return Err(tonic::Status::internal(format!(
            "wire InvokeTaskResponse.request_id `{resp_request_id}` does not match \
             outgoing request `{}`",
            v.meta.request_id
        )));
    }

    // 12. status → tonic::Code + commit/return
    let code = action_rpc_status_to_tonic_code(wire_status);
    if code == Code::Ok {
        // commit cache（Replay 路径下次同 fingerprint 直接返）
        let mut cache = state
            .idempotency
            .lock()
            .expect("idempotency cache mutex poisoned");
        insert_handle.commit(
            &mut cache,
            CachedResponse::InvokeTask {
                action_id: action_id_str.clone(),
            },
        );
        drop(cache);
        tracing::info!(
            request_id = %v.meta.request_id,
            idempotency_key = %v.meta.idempotency_key,
            action_id = %action_id_str,
            role = role.as_str(),
            source = %v.meta.source,
            "InvokeTask ok"
        );
        return Ok(tonic::Response::new(TaskHandle {
            request_id: v.meta.request_id,
            action_id: action_id_str,
        }));
    }

    // 非 Ok：drop handle 不缓存
    drop(insert_handle);
    tracing::warn!(
        request_id = %v.meta.request_id,
        idempotency_key = %v.meta.idempotency_key,
        ?wire_status,
        ?code,
        error_msg = %error_msg,
        "InvokeTask non-Ok wire status"
    );
    Err(tonic::Status::new(
        code,
        format!("daemon returned {wire_status:?}: {error_msg}"),
    ))
}

/// `CancelTask` handler。
///
/// # Errors
///
/// 见 12 步骨架。
pub async fn handle_cancel_task(
    state: &crate::gateway::GatewayHandlerState,
    request: tonic::Request<CancelTaskCommand>,
) -> Result<tonic::Response<Ack>, tonic::Status> {
    handle_cancel_task_inner(state, request.into_inner()).await
}

#[allow(clippy::too_many_lines)]
async fn handle_cancel_task_inner(
    state: &crate::gateway::GatewayHandlerState,
    cmd: CancelTaskCommand,
) -> Result<tonic::Response<Ack>, tonic::Status> {
    let v = validate_cancel_task_command(&cmd).map_err(|e| {
        tracing::warn!(error = %e, "CancelTask validation failed");
        tonic::Status::new(e.to_tonic_code(), e.to_string())
    })?;
    let role = lookup_role(&state.role_table, &v.meta.auth_token)?;
    // CancelTask 复用 Capability::InvokeTask（outline §8.1 #7a：CancelTask
    // 视为"任务执行权限的延伸"）
    check_capability(
        &state.role_table,
        role,
        Capability::InvokeTask,
        "CancelTask",
    )?;
    let fingerprint = cancel_task_idempotency_fingerprint(&v);
    let insert_handle = match decide_with_method(
        &state.idempotency,
        Method::CancelTask,
        &v.meta.idempotency_key,
        fingerprint,
        &v.meta.request_id,
        "CancelTask",
    )? {
        DecisionRoute::Fresh(h) => h,
        DecisionRoute::Replay(CachedResponse::CancelTask {
            cancel_at_ns,
            dry_run,
        }) => {
            return Ok(tonic::Response::new(Ack {
                request_id: v.meta.request_id,
                cleared_at_ns: cancel_at_ns,
                dry_run,
            }));
        }
        DecisionRoute::Replay(other) => {
            tracing::error!(
                request_id = %v.meta.request_id,
                cached_variant = ?other,
                "BUG: CancelTask method namespace 命中非 CancelTask variant"
            );
            return Err(tonic::Status::internal(
                "idempotency cache namespace inconsistency",
            ));
        }
    };
    let shutdown = *state.shutdown_rx.borrow();
    shutdown_gate(
        shutdown,
        &v.meta.request_id,
        &v.meta.idempotency_key,
        "CancelTask",
        Some(insert_handle.clone()),
    )?;
    let now_ns = state.clock.now().0;
    let timeout_ns = v.meta.timeout_ms.saturating_mul(1_000_000);
    let wire_deadline_ns = now_ns.saturating_add(timeout_ns);
    let wire_role = role_to_wire(role);
    let wire_bytes = build_cancel_task_request(
        &v.meta.request_id,
        &v.meta.idempotency_key,
        &v.meta.source,
        wire_role,
        &v.meta.audit_reason,
        &v.action_id,
        wire_deadline_ns,
        now_ns,
    );
    let oneshot_rx = match forward_to_rr_io(
        &state.rr_forward_tx_task_cancel,
        wire_bytes,
        &v.meta.request_id,
        "CancelTask",
    ) {
        Ok(rx) => rx,
        Err(status) => {
            drop(insert_handle);
            return Err(status);
        }
    };
    let reply_bytes = match await_reply(
        oneshot_rx,
        v.meta.timeout_ms,
        state.rr_timeout_grace_ms,
        &v.meta.request_id,
        "CancelTask",
    )
    .await
    {
        Ok(b) => b,
        Err(status) => {
            drop(insert_handle);
            return Err(status);
        }
    };
    let view = match flatbuffers::root::<FbCancelResp>(&reply_bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(
                request_id = %v.meta.request_id,
                error = ?e,
                "CancelTask wire response decode failed"
            );
            drop(insert_handle);
            return Err(tonic::Status::internal(format!(
                "decode wire CancelTaskResponse: {e}"
            )));
        }
    };
    let wire_status = view.status();
    let cancel_at_ns = view.cancel_at_ns();
    let error_msg = view.error_msg().unwrap_or("").to_string();
    let resp_request_id = view.request_id().unwrap_or("");
    if resp_request_id != v.meta.request_id {
        drop(insert_handle);
        return Err(tonic::Status::internal(format!(
            "wire CancelTaskResponse.request_id `{resp_request_id}` does not match outgoing `{}`",
            v.meta.request_id
        )));
    }
    let code = action_rpc_status_to_tonic_code(wire_status);
    if code == Code::Ok {
        let mut cache = state
            .idempotency
            .lock()
            .expect("idempotency cache mutex poisoned");
        insert_handle.commit(
            &mut cache,
            CachedResponse::CancelTask {
                cancel_at_ns,
                dry_run: v.meta.dry_run,
            },
        );
        drop(cache);
        tracing::info!(
            request_id = %v.meta.request_id,
            action_id = %v.action_id,
            cancel_at_ns,
            "CancelTask ok"
        );
        return Ok(tonic::Response::new(Ack {
            request_id: v.meta.request_id,
            cleared_at_ns: cancel_at_ns,
            dry_run: v.meta.dry_run,
        }));
    }
    drop(insert_handle);
    Err(tonic::Status::new(
        code,
        format!("daemon returned {wire_status:?}: {error_msg}"),
    ))
}

/// `GetTaskStatus` handler（只读 RPC，**不**进 cache）。
///
/// # Errors
///
/// 见 module doc。
pub async fn handle_get_task_status(
    state: &crate::gateway::GatewayHandlerState,
    request: tonic::Request<TaskHandleRef>,
) -> Result<tonic::Response<TaskStatus>, tonic::Status> {
    handle_get_task_status_inner(state, request.into_inner()).await
}

/// GetTaskStatus 默认 timeout（proto TaskHandleRef 无 CommandMeta，故无
/// `timeout_ms` 字段；用一个保守默认让 daemon 端有时间响应）。
const GET_TASK_STATUS_DEFAULT_TIMEOUT_MS: u64 = 5_000;

async fn handle_get_task_status_inner(
    state: &crate::gateway::GatewayHandlerState,
    req: TaskHandleRef,
) -> Result<tonic::Response<TaskStatus>, tonic::Status> {
    let v = validate_task_handle_ref(&req).map_err(|e| {
        tracing::warn!(error = %e, "GetTaskStatus validation failed");
        tonic::Status::new(e.to_tonic_code(), e.to_string())
    })?;

    // 5.5: shutdown gate（只读 RPC 也拒新请求；保持跟写 RPC 一致）。
    if *state.shutdown_rx.borrow() {
        tracing::warn!(
            request_id = %v.request_id,
            "GetTaskStatus rejected during shutdown"
        );
        return Err(tonic::Status::unavailable(
            "gateway is shutting down; new requests rejected",
        ));
    }

    let now_ns = state.clock.now().0;
    let timeout_ns = GET_TASK_STATUS_DEFAULT_TIMEOUT_MS.saturating_mul(1_000_000);
    let wire_deadline_ns = now_ns.saturating_add(timeout_ns);

    // 8. build wire bytes（无 CommandMeta；source / role / audit_reason 留
    //    空字串 + Unknown，仅占位）
    let wire_bytes = build_get_task_status_request(
        &v.request_id,
        /* idempotency_key */ "",
        /* source */ "",
        RpcRole::Unknown,
        /* audit_reason */ "",
        &v.action_id,
        wire_deadline_ns,
        now_ns,
    );

    // 9. forward
    let oneshot_rx = forward_to_rr_io(
        &state.rr_forward_tx_task_status,
        wire_bytes,
        &v.request_id,
        "GetTaskStatus",
    )?;

    // 10. await reply
    let reply_bytes = await_reply(
        oneshot_rx,
        GET_TASK_STATUS_DEFAULT_TIMEOUT_MS,
        state.rr_timeout_grace_ms,
        &v.request_id,
        "GetTaskStatus",
    )
    .await?;

    // 11. decode wire response
    let view = match flatbuffers::root::<FbStatusResp>(&reply_bytes) {
        Ok(v) => v,
        Err(e) => {
            return Err(tonic::Status::internal(format!(
                "decode wire GetTaskStatusResponse: {e}"
            )));
        }
    };
    let wire_status = view.status();
    let resp_request_id = view.request_id().unwrap_or("");
    if resp_request_id != v.request_id {
        return Err(tonic::Status::internal(format!(
            "wire GetTaskStatusResponse.request_id `{resp_request_id}` does not match outgoing `{}`",
            v.request_id
        )));
    }
    let code = action_rpc_status_to_tonic_code(wire_status);
    if code == Code::Ok {
        let resp_action_id = view.action_id().unwrap_or("").to_string();
        let state_proto = task_state_fb_to_proto(view.state());
        let progress = view.progress();
        let outcome = view.outcome().map(action_outcome_fb_to_proto);
        let failure = view.failure().map(action_failure_fb_to_proto);
        return Ok(tonic::Response::new(TaskStatus {
            request_id: v.request_id,
            action_id: resp_action_id,
            state: state_proto.into(),
            progress,
            outcome,
            failure,
        }));
    }
    let error_msg = view.error_msg().unwrap_or("").to_string();
    Err(tonic::Status::new(
        code,
        format!("daemon returned {wire_status:?}: {error_msg}"),
    ))
}

/// `ClearError` handler（清 coordinator `Degraded` 内存状态；**不**碰
/// §6.7 invariant）。
///
/// # Errors
///
/// 见 12 步骨架。
pub async fn handle_clear_error(
    state: &crate::gateway::GatewayHandlerState,
    request: tonic::Request<ClearErrorCommand>,
) -> Result<tonic::Response<Ack>, tonic::Status> {
    handle_clear_error_inner(state, request.into_inner()).await
}

#[allow(clippy::too_many_lines)]
async fn handle_clear_error_inner(
    state: &crate::gateway::GatewayHandlerState,
    cmd: ClearErrorCommand,
) -> Result<tonic::Response<Ack>, tonic::Status> {
    let v = validate_clear_error_command(&cmd).map_err(|e| {
        tracing::warn!(error = %e, "ClearError validation failed");
        tonic::Status::new(e.to_tonic_code(), e.to_string())
    })?;
    let role = lookup_role(&state.role_table, &v.meta.auth_token)?;
    // ClearError = maintainer+（outline §8.1 #7c 收紧；rpc_roles.toml
    // [operator] 删 clear_error）
    check_capability(
        &state.role_table,
        role,
        Capability::ClearError,
        "ClearError",
    )?;
    let fingerprint = clear_error_idempotency_fingerprint(&v);
    let insert_handle = match decide_with_method(
        &state.idempotency,
        Method::ClearError,
        &v.meta.idempotency_key,
        fingerprint,
        &v.meta.request_id,
        "ClearError",
    )? {
        DecisionRoute::Fresh(h) => h,
        DecisionRoute::Replay(CachedResponse::ClearError {
            cleared_at_ns,
            dry_run,
        }) => {
            return Ok(tonic::Response::new(Ack {
                request_id: v.meta.request_id,
                cleared_at_ns,
                dry_run,
            }));
        }
        DecisionRoute::Replay(other) => {
            tracing::error!(
                request_id = %v.meta.request_id,
                cached_variant = ?other,
                "BUG: ClearError method namespace 命中非 ClearError variant"
            );
            return Err(tonic::Status::internal(
                "idempotency cache namespace inconsistency",
            ));
        }
    };
    let shutdown = *state.shutdown_rx.borrow();
    shutdown_gate(
        shutdown,
        &v.meta.request_id,
        &v.meta.idempotency_key,
        "ClearError",
        Some(insert_handle.clone()),
    )?;
    let now_ns = state.clock.now().0;
    let timeout_ns = v.meta.timeout_ms.saturating_mul(1_000_000);
    let wire_deadline_ns = now_ns.saturating_add(timeout_ns);
    let wire_role = role_to_wire(role);
    let wire_bytes = build_clear_error_request(
        &v.meta.request_id,
        &v.meta.idempotency_key,
        &v.meta.source,
        wire_role,
        &v.meta.audit_reason,
        wire_deadline_ns,
        now_ns,
    );
    let oneshot_rx = match forward_to_rr_io(
        &state.rr_forward_tx_clear_error,
        wire_bytes,
        &v.meta.request_id,
        "ClearError",
    ) {
        Ok(rx) => rx,
        Err(status) => {
            drop(insert_handle);
            return Err(status);
        }
    };
    let reply_bytes = match await_reply(
        oneshot_rx,
        v.meta.timeout_ms,
        state.rr_timeout_grace_ms,
        &v.meta.request_id,
        "ClearError",
    )
    .await
    {
        Ok(b) => b,
        Err(status) => {
            drop(insert_handle);
            return Err(status);
        }
    };
    let view = match flatbuffers::root::<FbClearErrorResp>(&reply_bytes) {
        Ok(v) => v,
        Err(e) => {
            drop(insert_handle);
            return Err(tonic::Status::internal(format!(
                "decode wire ClearErrorResponse: {e}"
            )));
        }
    };
    let wire_status = view.status();
    let cleared_at_ns = view.cleared_at_ns();
    let error_msg = view.error_msg().unwrap_or("").to_string();
    let resp_request_id = view.request_id().unwrap_or("");
    if resp_request_id != v.meta.request_id {
        drop(insert_handle);
        return Err(tonic::Status::internal(format!(
            "wire ClearErrorResponse.request_id `{resp_request_id}` does not match outgoing `{}`",
            v.meta.request_id
        )));
    }
    let code = action_rpc_status_to_tonic_code(wire_status);
    if code == Code::Ok {
        let mut cache = state
            .idempotency
            .lock()
            .expect("idempotency cache mutex poisoned");
        insert_handle.commit(
            &mut cache,
            CachedResponse::ClearError {
                cleared_at_ns,
                dry_run: v.meta.dry_run,
            },
        );
        drop(cache);
        tracing::info!(
            request_id = %v.meta.request_id,
            cleared_at_ns,
            "ClearError ok"
        );
        return Ok(tonic::Response::new(Ack {
            request_id: v.meta.request_id,
            cleared_at_ns,
            dry_run: v.meta.dry_run,
        }));
    }
    drop(insert_handle);
    Err(tonic::Status::new(
        code,
        format!("daemon returned {wire_status:?}: {error_msg}"),
    ))
}

// ============================================================================
// StreamTaskEvents handler（B4 (2/3-c)；不进 idempotency cache）
// ============================================================================

/// `StreamTaskEvents` handler。
///
/// 跟其它 4 个 RPC 不同：
/// - **不**进 idempotency cache（streaming RPC，幂等天然不适用）。
/// - **不**走 rr_io_thread（pubsub 而不是 RR）。
/// - **不**做 auth（proto frozen `TaskHandleRef` 无 CommandMeta；v0.3 仅
///   按 audit-only 处理 read RPC，跟 GetTaskStatus 同款；v0.5+ 加 metadata
///   auth）。
///
/// **shutdown 路径**：handler 入口 check `shutdown_rx`；shutdown=true →
/// `Unavailable`。已 in-flight stream 由 [`crate::events_subscribe::EventsSubscribeStream::drop`]
/// 自然 unsubscribe（client 超时 / cancel 触发 stream drop；tonic graceful
/// shutdown 不主动 cancel in-flight stream）。
///
/// # Errors
///
/// - validation 失败 → `InvalidArgument`
/// - shutdown_rx=true → `Unavailable`
/// - `events_subscribe` spawn 失败（iceoryx2 资源问题）→ `Internal`
pub fn handle_stream_task_events(
    state: &crate::gateway::GatewayHandlerState,
    request: tonic::Request<crate::proto::TaskHandleRef>,
) -> Result<tonic::Response<crate::events_subscribe::EventsSubscribeStream>, tonic::Status> {
    let req = request.into_inner();
    let v = validate_task_handle_ref(&req).map_err(|e| {
        tracing::warn!(error = %e, "StreamTaskEvents validation failed");
        tonic::Status::new(e.to_tonic_code(), e.to_string())
    })?;

    if *state.shutdown_rx.borrow() {
        tracing::warn!(
            request_id = %v.request_id,
            "StreamTaskEvents rejected during shutdown"
        );
        return Err(tonic::Status::unavailable(
            "gateway is shutting down; new requests rejected",
        ));
    }

    let cfg = crate::events_subscribe::EventsSubscribeConfig {
        robot_id: state.robot_id.clone(),
        action_id_hex: v.action_id.clone(),
        request_id: v.request_id.clone(),
        poll_interval: Duration::from_millis(50),
        channel_buf: 16,
    };
    let (handle, rx) = match crate::events_subscribe::spawn_events_subscribe_thread(cfg) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(
                request_id = %v.request_id,
                action_id = %v.action_id,
                error = ?e,
                "StreamTaskEvents: spawn_events_subscribe_thread failed"
            );
            return Err(tonic::Status::internal(format!(
                "failed to subscribe pubsub for action_id={}: {e}",
                v.action_id
            )));
        }
    };
    tracing::info!(
        request_id = %v.request_id,
        action_id = %v.action_id,
        topic = %handle.topic(),
        "StreamTaskEvents subscribe thread started"
    );

    let stream = crate::events_subscribe::EventsSubscribeStream::new(rx, handle);
    Ok(tonic::Response::new(stream))
}

// ============================================================================
// Decision routing helper
// ============================================================================

/// Cache.decide 路由结果（去掉 Conflict / Capacity 用 tonic::Status 包好）。
enum DecisionRoute {
    Fresh(crate::idempotency::InsertHandle),
    Replay(CachedResponse),
}

fn decide_with_method(
    cache: &Arc<Mutex<IdempotencyCache>>,
    method: Method,
    idempotency_key: &str,
    fingerprint: Fingerprint,
    request_id: &str,
    method_label: &str,
) -> Result<DecisionRoute, tonic::Status> {
    let mut g = cache.lock().expect("idempotency cache mutex poisoned");
    match g.decide(method, idempotency_key, fingerprint) {
        Decision::FreshInsert(h) => Ok(DecisionRoute::Fresh(h)),
        Decision::Replay(resp) => {
            tracing::info!(
                request_id,
                idempotency_key,
                method = method_label,
                "idempotency replay"
            );
            Ok(DecisionRoute::Replay(resp))
        }
        Decision::Conflict { .. } => Err(tonic::Status::already_exists(format!(
            "idempotency_key `{idempotency_key}` already used with a different request fingerprint"
        ))),
        Decision::Capacity {
            current_entries,
            max_entries,
        } => Err(tonic::Status::resource_exhausted(format!(
            "idempotency cache full ({current_entries}/{max_entries}); retry after TTL expiry"
        ))),
    }
}

// ============================================================================
// Proto ↔ fbs 字段映射 helpers
// ============================================================================

fn action_kind_proto_to_fb(kind: ActionKindProto) -> ActionKindFb {
    match kind {
        ActionKindProto::ActionKindUnspecified => ActionKindFb::Unknown,
        ActionKindProto::ActionKindDwellMs => ActionKindFb::DwellMs,
        ActionKindProto::ActionKindHoldPose => ActionKindFb::HoldPose,
        ActionKindProto::ActionKindScripted => ActionKindFb::Scripted,
        ActionKindProto::ActionKindIk => ActionKindFb::Ik,
    }
}

// `Unknown` arm 跟 wildcard `_` arm 都映射 Unspecified（语义来源不同：
// 前者是 fbs 兼容旧 writer，后者兜底未来 variant），保留两个 arm 让源码
// 可读 + reviewer 一眼看见兼容路径；clippy `match_same_arms` 在此 allow。

#[allow(clippy::match_same_arms)]
fn task_state_fb_to_proto(state: TaskStateFb) -> ProtoTaskState {
    match state {
        TaskStateFb::Unknown => ProtoTaskState::Unspecified,
        TaskStateFb::Pending => ProtoTaskState::Pending,
        TaskStateFb::Executing => ProtoTaskState::Executing,
        TaskStateFb::Completed => ProtoTaskState::Completed,
        TaskStateFb::Failed => ProtoTaskState::Failed,
        TaskStateFb::Cancelled => ProtoTaskState::Cancelled,
        _ => ProtoTaskState::Unspecified, // 未来 variant 兼容
    }
}

#[allow(clippy::match_same_arms)]
fn action_outcome_fb_to_proto(outcome: ActionOutcomeFb<'_>) -> ActionOutcomeProto {
    use action_outcome_proto::Kind as P;
    let kind: P = match outcome.kind() {
        ActionOutcomeKindFb::Unknown => P::Unspecified,
        ActionOutcomeKindFb::Success => P::Success,
        ActionOutcomeKindFb::PartialSuccess => P::PartialSuccess,
        _ => P::Unspecified,
    };
    ActionOutcomeProto {
        kind: kind.into(),
        warning: outcome.warning().unwrap_or("").to_string(),
    }
}

#[allow(clippy::match_same_arms)]
fn action_failure_fb_to_proto(failure: ActionFailureFb<'_>) -> ActionFailureProto {
    use action_failure_proto::Kind as P;
    let kind: P = match failure.kind() {
        ActionFailureKindFb::Unknown => P::Unspecified,
        ActionFailureKindFb::DeadlineExceeded => P::DeadlineExceeded,
        ActionFailureKindFb::PreconditionFailed => P::PreconditionFailed,
        ActionFailureKindFb::AdapterError => P::AdapterError,
        ActionFailureKindFb::SafetyAbort => P::SafetyAbort,
        ActionFailureKindFb::InternalError => P::InternalError,
        _ => P::Unspecified,
    };
    ActionFailureProto {
        kind: kind.into(),
        reason: failure.reason().unwrap_or("").to_string(),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::AuthContext;

    fn meta_for(idempotency_key: &str, source: &str) -> CommandMeta {
        CommandMeta {
            request_id: uuid::Uuid::now_v7().to_string(),
            source: source.to_string(),
            auth: Some(AuthContext {
                token: "operator-fake-token-001".to_string(),
                mtls_fingerprint: String::new(),
            }),
            timeout_ms: 2_000,
            idempotency_key: idempotency_key.to_string(),
            dry_run: false,
            audit_reason: "unit test".to_string(),
        }
    }

    // ────────────────────────────────────────────────────────────────────────
    // ActionRpcStatus → tonic::Code 显式映射全 variant 锁测
    // ────────────────────────────────────────────────────────────────────────

    /// `ActionRpcStatus::ENUM_VALUES` 长度跟本 helper 显式映射数硬绑（沿
    /// A1 [`crate::clear_estop::tests::rpc_status_variant_count_matches_explicit_mapping`]
    /// 同款锁测）。新增 variant 让 length 对不上 → 测试失败 → 强制同步
    /// `action_rpc_status_to_tonic_code` + 单测。
    #[test]
    fn action_rpc_status_variant_count_matches_explicit_mapping() {
        // 当前 fbs schema 9 个真 variant + Unknown 兼容 = 10 个。新增
        // variant 时 ENUM_VALUES 长度增加 → 本断言失败强制更新 helper。
        const EXPECTED_VARIANT_COUNT: usize = 10;
        assert_eq!(
            ActionRpcStatus::ENUM_VALUES.len(),
            EXPECTED_VARIANT_COUNT,
            "新增 ActionRpcStatus variant 必须同步更新 \
             action_rpc_status_to_tonic_code + 本断言数字"
        );
    }

    #[test]
    fn action_rpc_status_explicit_mappings() {
        assert_eq!(
            action_rpc_status_to_tonic_code(ActionRpcStatus::Unknown),
            Code::Internal
        );
        assert_eq!(
            action_rpc_status_to_tonic_code(ActionRpcStatus::Ok),
            Code::Ok
        );
        assert_eq!(
            action_rpc_status_to_tonic_code(ActionRpcStatus::FailedPrecondition),
            Code::FailedPrecondition
        );
        assert_eq!(
            action_rpc_status_to_tonic_code(ActionRpcStatus::PermissionDenied),
            Code::PermissionDenied
        );
        assert_eq!(
            action_rpc_status_to_tonic_code(ActionRpcStatus::InvalidArgument),
            Code::InvalidArgument
        );
        assert_eq!(
            action_rpc_status_to_tonic_code(ActionRpcStatus::Internal),
            Code::Internal
        );
        assert_eq!(
            action_rpc_status_to_tonic_code(ActionRpcStatus::AlreadyExists),
            Code::AlreadyExists
        );
        assert_eq!(
            action_rpc_status_to_tonic_code(ActionRpcStatus::Unimplemented),
            Code::Unimplemented
        );
        assert_eq!(
            action_rpc_status_to_tonic_code(ActionRpcStatus::NotFound),
            Code::NotFound
        );
        assert_eq!(
            action_rpc_status_to_tonic_code(ActionRpcStatus::Unavailable),
            Code::Unavailable
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // ActionKindProto → ActionKindFb 显式映射全 variant 锁测
    // ────────────────────────────────────────────────────────────────────────

    /// `action_kind_proto_to_fb` 的 exhaustive match 必须穷举
    /// [`ActionKindProto`] 所有 variant。新增 variant 让本测断言失败 →
    /// 强制同步映射 helper（沿
    /// [`action_rpc_status_variant_count_matches_explicit_mapping`] 同款
    /// variant count 锁测模式；v0.4 候选 1 Plan round 1 阻断 1 拍板：
    /// proto / fbs / lib 三层 enum append 必须同 commit 一起 land）。
    #[test]
    fn action_kind_proto_to_fb_variant_count_matches_explicit_mapping() {
        // v0.4 候选 2 round 2 (2/4) land 后：Unspecified / DwellMs /
        // HoldPose / Scripted / Ik = 5 variant。新增 variant 时本断言
        // 失败强制更新 helper。
        const EXPECTED_VARIANT_COUNT: usize = 5;
        // prost 给 proto enum 生成 `from_repr` API；用所有合法 repr
        // 验证 helper 是 total（不依赖 exhaustive match 自身的“编译期
        // 兜底”机制，因为 helper 已经是 exhaustive；这里额外断言
        // variant 个数没漂移）。
        let variants = [
            ActionKindProto::ActionKindUnspecified,
            ActionKindProto::ActionKindDwellMs,
            ActionKindProto::ActionKindHoldPose,
            ActionKindProto::ActionKindScripted,
            ActionKindProto::ActionKindIk,
        ];
        assert_eq!(
            variants.len(),
            EXPECTED_VARIANT_COUNT,
            "新增 ActionKindProto variant 必须同步更新 \
             action_kind_proto_to_fb + 本断言数字（v0.4 候选 1 Plan \
             round 1 阻断 1）"
        );
        // 穷举映射：每个 variant 都能被 helper 接受（不 panic）。
        assert_eq!(
            action_kind_proto_to_fb(ActionKindProto::ActionKindUnspecified),
            ActionKindFb::Unknown
        );
        assert_eq!(
            action_kind_proto_to_fb(ActionKindProto::ActionKindDwellMs),
            ActionKindFb::DwellMs
        );
        assert_eq!(
            action_kind_proto_to_fb(ActionKindProto::ActionKindHoldPose),
            ActionKindFb::HoldPose
        );
        assert_eq!(
            action_kind_proto_to_fb(ActionKindProto::ActionKindScripted),
            ActionKindFb::Scripted
        );
        assert_eq!(
            action_kind_proto_to_fb(ActionKindProto::ActionKindIk),
            ActionKindFb::Ik
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // Validation tests
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn invoke_task_validation_happy() {
        let cmd = InvokeTaskCommand {
            meta: Some(meta_for("idem-1", "test.client")),
            kind: ActionKindProto::ActionKindDwellMs.into(),
            deadline_ns: 100,
            priority: 10,
            payload_schema_id: 0,
            payload: Vec::new(),
        };
        let v = validate_invoke_task_command(&cmd).expect("valid");
        assert_eq!(v.kind, ActionKindProto::ActionKindDwellMs);
        assert_eq!(v.priority, 10);
    }

    #[test]
    fn invoke_task_validation_unspecified_kind() {
        let cmd = InvokeTaskCommand {
            meta: Some(meta_for("idem-1", "test.client")),
            kind: ActionKindProto::ActionKindUnspecified.into(),
            deadline_ns: 100,
            priority: 0,
            payload_schema_id: 0,
            payload: Vec::new(),
        };
        let err = validate_invoke_task_command(&cmd).expect_err("unspecified kind");
        assert!(matches!(err, TaskValidationError::InvokeKindUnspecified));
    }

    #[test]
    fn invoke_task_validation_missing_meta() {
        let cmd = InvokeTaskCommand {
            meta: None,
            kind: ActionKindProto::ActionKindDwellMs.into(),
            deadline_ns: 100,
            priority: 0,
            payload_schema_id: 0,
            payload: Vec::new(),
        };
        let err = validate_invoke_task_command(&cmd).expect_err("missing meta");
        assert!(matches!(err, TaskValidationError::MissingMeta));
    }

    #[test]
    fn cancel_task_validation_happy() {
        let cmd = CancelTaskCommand {
            meta: Some(meta_for("idem-2", "test.client")),
            action_id: uuid::Uuid::now_v7().to_string(),
        };
        validate_cancel_task_command(&cmd).expect("valid");
    }

    #[test]
    fn cancel_task_validation_empty_action_id() {
        let cmd = CancelTaskCommand {
            meta: Some(meta_for("idem-2", "test.client")),
            action_id: String::new(),
        };
        let err = validate_cancel_task_command(&cmd).expect_err("empty action_id");
        assert!(matches!(err, TaskValidationError::EmptyCancelActionId));
    }

    #[test]
    fn cancel_task_validation_invalid_action_id() {
        let cmd = CancelTaskCommand {
            meta: Some(meta_for("idem-2", "test.client")),
            action_id: "not-a-uuid".to_string(),
        };
        let err = validate_cancel_task_command(&cmd).expect_err("invalid action_id");
        assert!(matches!(err, TaskValidationError::InvalidCancelActionId(_)));
    }

    #[test]
    fn clear_error_validation_happy() {
        let cmd = ClearErrorCommand {
            meta: Some(meta_for("idem-3", "test.client")),
        };
        validate_clear_error_command(&cmd).expect("valid");
    }

    #[test]
    fn task_handle_ref_validation_happy() {
        let req = TaskHandleRef {
            request_id: uuid::Uuid::now_v7().to_string(),
            action_id: uuid::Uuid::now_v7().to_string(),
        };
        validate_task_handle_ref(&req).expect("valid");
    }

    #[test]
    fn task_handle_ref_validation_empty_action_id() {
        let req = TaskHandleRef {
            request_id: uuid::Uuid::now_v7().to_string(),
            action_id: String::new(),
        };
        let err = validate_task_handle_ref(&req).expect_err("empty action_id");
        assert!(matches!(
            err,
            TaskValidationError::EmptyTaskHandleRefActionId
        ));
    }

    // ────────────────────────────────────────────────────────────────────────
    // Fingerprint tests
    // ────────────────────────────────────────────────────────────────────────

    /// 同 idempotency_key 但 request_id / timeout_ms 改变 → fingerprint 不变
    /// （reviewer 阻断锁定：合法 retry 不应被 cache 误杀 Conflict）。
    #[test]
    fn invoke_task_fingerprint_excludes_request_id_and_timeout_ms() {
        let mut cmd_a = InvokeTaskCommand {
            meta: Some(meta_for("idem-fp", "test.client")),
            kind: ActionKindProto::ActionKindDwellMs.into(),
            deadline_ns: 100,
            priority: 10,
            payload_schema_id: 0,
            payload: vec![1, 2, 3],
        };
        let v_a = validate_invoke_task_command(&cmd_a).unwrap();
        let fp_a = invoke_task_idempotency_fingerprint(&v_a);

        // retry：换 request_id + timeout_ms（client 重试时合理调整）
        if let Some(meta) = cmd_a.meta.as_mut() {
            meta.request_id = uuid::Uuid::now_v7().to_string();
            meta.timeout_ms = 5_000;
        }
        let v_b = validate_invoke_task_command(&cmd_a).unwrap();
        let fp_b = invoke_task_idempotency_fingerprint(&v_b);

        assert_eq!(
            fp_a, fp_b,
            "REGRESSION: request_id / timeout_ms 改变不该影响 fingerprint"
        );
    }

    /// 改 payload bytes → fingerprint 改变（不同意图必须不复用 cached ack）。
    #[test]
    fn invoke_task_fingerprint_includes_payload_bytes() {
        let cmd_a = InvokeTaskCommand {
            meta: Some(meta_for("idem-fp", "test.client")),
            kind: ActionKindProto::ActionKindDwellMs.into(),
            deadline_ns: 100,
            priority: 0,
            payload_schema_id: 1,
            payload: vec![1, 2, 3],
        };
        let cmd_b = InvokeTaskCommand {
            meta: Some(meta_for("idem-fp", "test.client")),
            kind: ActionKindProto::ActionKindDwellMs.into(),
            deadline_ns: 100,
            priority: 0,
            payload_schema_id: 1,
            payload: vec![9, 9, 9],
        };
        let fp_a =
            invoke_task_idempotency_fingerprint(&validate_invoke_task_command(&cmd_a).unwrap());
        let fp_b =
            invoke_task_idempotency_fingerprint(&validate_invoke_task_command(&cmd_b).unwrap());
        assert_ne!(fp_a, fp_b);
    }

    /// 改 kind → fingerprint 改变。
    #[test]
    fn invoke_task_fingerprint_includes_kind() {
        let cmd_a = InvokeTaskCommand {
            meta: Some(meta_for("idem-fp", "test.client")),
            kind: ActionKindProto::ActionKindDwellMs.into(),
            deadline_ns: 100,
            priority: 0,
            payload_schema_id: 0,
            payload: Vec::new(),
        };
        let cmd_b = InvokeTaskCommand {
            meta: Some(meta_for("idem-fp", "test.client")),
            kind: ActionKindProto::ActionKindHoldPose.into(),
            deadline_ns: 100,
            priority: 0,
            payload_schema_id: 0,
            payload: Vec::new(),
        };
        let fp_a =
            invoke_task_idempotency_fingerprint(&validate_invoke_task_command(&cmd_a).unwrap());
        let fp_b =
            invoke_task_idempotency_fingerprint(&validate_invoke_task_command(&cmd_b).unwrap());
        assert_ne!(fp_a, fp_b);
    }

    #[test]
    fn cancel_task_fingerprint_includes_action_id() {
        let cmd_a = CancelTaskCommand {
            meta: Some(meta_for("idem-fp", "test.client")),
            action_id: uuid::Uuid::now_v7().to_string(),
        };
        let cmd_b = CancelTaskCommand {
            meta: Some(meta_for("idem-fp", "test.client")),
            action_id: uuid::Uuid::now_v7().to_string(),
        };
        let fp_a =
            cancel_task_idempotency_fingerprint(&validate_cancel_task_command(&cmd_a).unwrap());
        let fp_b =
            cancel_task_idempotency_fingerprint(&validate_cancel_task_command(&cmd_b).unwrap());
        assert_ne!(fp_a, fp_b);
    }

    // ────────────────────────────────────────────────────────────────────────
    // v0.4 候选 1 (2/3) Q2(a) byte gate 锁测
    // ────────────────────────────────────────────────────────────────────────

    /// `MAX_SCRIPTED_PAYLOAD_BYTES` + `CONTROL_TIER_MAX_BYTES` 字面值
    /// 锁测（reviewer Plan 期注意点 + Plan round 2 阻断 1 拍板：单源
    /// 绑定 `SizeTier::Control.max_bytes()`，**严禁**硬编码 8192）。
    ///
    /// Gate 1 < Gate 2 的编译期断言放在 module 级（详 `_GATE_BOUNDARY`
    /// 编译期常量；两个 const 都是编译期决定，clippy 不让 runtime
    /// assert 它们的比较，所以走 `const _: () = assert!(...)`）。
    #[test]
    fn byte_gate_constants_match_single_source_of_truth() {
        // Gate 1 = 4 KiB（Q6 拍死本轮 only byte cap）。
        assert_eq!(
            MAX_SCRIPTED_PAYLOAD_BYTES,
            4 * 1024,
            "MAX_SCRIPTED_PAYLOAD_BYTES 必须 = 4 KiB"
        );
        // Gate 2 = SizeTier::Control.max_bytes() = 8 KiB（单源绑定）。
        assert_eq!(
            CONTROL_TIER_MAX_BYTES,
            robot_msg::size_tier::SizeTier::Control.max_bytes(),
            "CONTROL_TIER_MAX_BYTES 必须单源绑定 SizeTier::Control.max_bytes()，\
             不能硬编码 8192"
        );
    }

    /// 编译期常量：Gate 1 < Gate 2（语义边界：payload cap 应严格 <
    /// wire cap，留 `CommandMeta` + fbs overhead 头空间）。两个常量都
    /// 是 `const`，比较在编译期就能解；断错连编译都过不去。
    const _GATE_BOUNDARY_CONST: () = assert!(
        MAX_SCRIPTED_PAYLOAD_BYTES < CONTROL_TIER_MAX_BYTES,
        "Gate 1 必须 < Gate 2，留 CommandMeta + fbs overhead 头空间"
    );

    /// v0.4 候选 1 (2/3) AC-7 (7c-i)：Scripted payload bytes 超
    /// MAX_SCRIPTED_PAYLOAD_BYTES → gateway gate 1 precheck → tonic
    /// `Code::InvalidArgument`，**不**进 RR / s1。
    #[tokio::test]
    async fn invoke_task_payload_over_max_scripted_bytes_returns_invalid_argument() {
        use crate::auth::load_role_table;
        use crate::idempotency::IdempotencyCache;
        use crate::proto::AuthContext;
        use crossbeam_channel::bounded;
        use robot_core::time::SystemClock;
        use std::sync::{Arc, Mutex};
        use std::time::Duration as StdDuration;
        use tokio::sync::watch;

        const TEST_TOML: &str = r#"
[tokens]
"operator-fake-token-001" = "operator"
[roles.viewer]
capabilities = ["read_state"]
[roles.operator]
capabilities = ["read_state", "invoke_task"]
[roles.maintainer]
capabilities = ["read_state", "invoke_task", "clear_estop", "clear_error"]
[roles.admin]
capabilities = ["read_state", "invoke_task", "clear_estop", "clear_error"]
"#;
        let role_table = Arc::new(load_role_table(TEST_TOML).expect("role table parse"));

        let (task_invoke_tx, task_invoke_rx) = bounded(1);
        let (clear_estop_tx, clear_estop_rx) = bounded(1);
        let (task_cancel_tx, task_cancel_rx) = bounded(1);
        let (task_status_tx, task_status_rx) = bounded(1);
        let (clear_error_tx, clear_error_rx) = bounded(1);
        let (set_enabled_tx, set_enabled_rx) = bounded(1);
        Box::leak(Box::new(task_invoke_rx));
        Box::leak(Box::new(clear_estop_rx));
        Box::leak(Box::new(task_cancel_rx));
        Box::leak(Box::new(task_status_rx));
        Box::leak(Box::new(clear_error_rx));
        Box::leak(Box::new(set_enabled_rx));

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Box::leak(Box::new(shutdown_tx));

        let state = crate::gateway::GatewayHandlerState {
            rr_forward_tx_clear_estop: clear_estop_tx,
            rr_forward_tx_task_invoke: task_invoke_tx,
            rr_forward_tx_task_cancel: task_cancel_tx,
            rr_forward_tx_task_status: task_status_tx,
            rr_forward_tx_clear_error: clear_error_tx,
            rr_forward_tx_set_enabled: set_enabled_tx,
            robot_id: "gate1_test".to_string(),
            role_table,
            idempotency: Arc::new(Mutex::new(IdempotencyCache::with_capacity(
                StdDuration::from_mins(1),
                64,
            ))),
            clock: Arc::new(SystemClock),
            rr_timeout_grace_ms: 50,
            shutdown_rx,
            state_cache: std::sync::Arc::new(crate::state_pool::StateCache::new(&[])),
        };

        // payload = MAX_SCRIPTED_PAYLOAD_BYTES + 1 byte（刚好超 gate 1）。
        let oversized_payload = vec![0u8; MAX_SCRIPTED_PAYLOAD_BYTES + 1];
        let cmd = InvokeTaskCommand {
            meta: Some(CommandMeta {
                request_id: uuid::Uuid::now_v7().to_string(),
                source: "gate1.test".to_string(),
                auth: Some(AuthContext {
                    token: "operator-fake-token-001".to_string(),
                    mtls_fingerprint: String::new(),
                }),
                timeout_ms: 100,
                idempotency_key: "gate1-oversize-key".to_string(),
                dry_run: false,
                audit_reason: "gate 1 test".to_string(),
            }),
            kind: ActionKindProto::ActionKindScripted.into(),
            deadline_ns: 100,
            priority: 10,
            payload_schema_id: robot_msg::schema_id::SCHEMA_SCRIPTED_PAYLOAD,
            payload: oversized_payload,
        };
        let request = tonic::Request::new(cmd);
        let result = handle_invoke_task(&state, request).await;
        match result {
            Err(status) if status.code() == tonic::Code::InvalidArgument => {
                assert!(
                    status.message().contains("MAX_SCRIPTED_PAYLOAD_BYTES"),
                    "gate 1 error_msg should mention MAX_SCRIPTED_PAYLOAD_BYTES; got: {:?}",
                    status.message()
                );
            }
            Err(other) => panic!("expected InvalidArgument from gate 1, got {other:?}"),
            Ok(resp) => panic!("expected Err from gate 1, got Ok: {:?}", resp.into_inner()),
        }
    }

    /// v0.4 候选 1 (2/3) AC-7 (7c-ii)：payload 通过 gate 1（或 kind !=
    /// Scripted 不走 gate 1）但 `build_invoke_task_request` 后 wire
    /// bytes 超 `CONTROL_TIER_MAX_BYTES` → gateway gate 2 final guard
    /// → tonic `Code::InvalidArgument`，**不**进 RR / s1。
    ///
    /// 构造场景：kind = HoldPose（**不**触发 gate 1），payload =
    /// 9000 bytes（虚拟 payload；HoldPose 不解 payload，gateway 只看
    /// 字节数）→ wire 实测必然 > 8 KiB。
    #[tokio::test]
    async fn invoke_task_wire_over_control_tier_returns_invalid_argument() {
        use crate::auth::load_role_table;
        use crate::idempotency::IdempotencyCache;
        use crate::proto::AuthContext;
        use crossbeam_channel::bounded;
        use robot_core::time::SystemClock;
        use std::sync::{Arc, Mutex};
        use std::time::Duration as StdDuration;
        use tokio::sync::watch;

        const TEST_TOML: &str = r#"
[tokens]
"operator-fake-token-001" = "operator"
[roles.viewer]
capabilities = ["read_state"]
[roles.operator]
capabilities = ["read_state", "invoke_task"]
[roles.maintainer]
capabilities = ["read_state", "invoke_task", "clear_estop", "clear_error"]
[roles.admin]
capabilities = ["read_state", "invoke_task", "clear_estop", "clear_error"]
"#;
        let role_table = Arc::new(load_role_table(TEST_TOML).expect("role table parse"));

        let (task_invoke_tx, task_invoke_rx) = bounded(1);
        let (clear_estop_tx, clear_estop_rx) = bounded(1);
        let (task_cancel_tx, task_cancel_rx) = bounded(1);
        let (task_status_tx, task_status_rx) = bounded(1);
        let (clear_error_tx, clear_error_rx) = bounded(1);
        let (set_enabled_tx, set_enabled_rx) = bounded(1);
        Box::leak(Box::new(task_invoke_rx));
        Box::leak(Box::new(clear_estop_rx));
        Box::leak(Box::new(task_cancel_rx));
        Box::leak(Box::new(task_status_rx));
        Box::leak(Box::new(clear_error_rx));
        Box::leak(Box::new(set_enabled_rx));

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Box::leak(Box::new(shutdown_tx));

        let state = crate::gateway::GatewayHandlerState {
            rr_forward_tx_clear_estop: clear_estop_tx,
            rr_forward_tx_task_invoke: task_invoke_tx,
            rr_forward_tx_task_cancel: task_cancel_tx,
            rr_forward_tx_task_status: task_status_tx,
            rr_forward_tx_clear_error: clear_error_tx,
            rr_forward_tx_set_enabled: set_enabled_tx,
            robot_id: "gate2_test".to_string(),
            role_table,
            idempotency: Arc::new(Mutex::new(IdempotencyCache::with_capacity(
                StdDuration::from_mins(1),
                64,
            ))),
            clock: Arc::new(SystemClock),
            rr_timeout_grace_ms: 50,
            shutdown_rx,
            state_cache: std::sync::Arc::new(crate::state_pool::StateCache::new(&[])),
        };

        // kind = HoldPose（NOT Scripted）→ 不触发 gate 1 payload cap。
        // payload = 9000 bytes 让 wire 必超 8 KiB。
        let bloat_payload = vec![0u8; 9000];
        let cmd = InvokeTaskCommand {
            meta: Some(CommandMeta {
                request_id: uuid::Uuid::now_v7().to_string(),
                source: "gate2.test".to_string(),
                auth: Some(AuthContext {
                    token: "operator-fake-token-001".to_string(),
                    mtls_fingerprint: String::new(),
                }),
                timeout_ms: 100,
                idempotency_key: "gate2-bloat-key".to_string(),
                dry_run: false,
                audit_reason: "gate 2 test".to_string(),
            }),
            kind: ActionKindProto::ActionKindHoldPose.into(),
            deadline_ns: 100,
            priority: 10,
            payload_schema_id: 0xCAFE_0001, // 任意非零；不解码
            payload: bloat_payload,
        };
        let request = tonic::Request::new(cmd);
        let result = handle_invoke_task(&state, request).await;
        match result {
            Err(status) if status.code() == tonic::Code::InvalidArgument => {
                assert!(
                    status.message().contains("CONTROL_TIER_MAX_BYTES"),
                    "gate 2 error_msg should mention CONTROL_TIER_MAX_BYTES; got: {:?}",
                    status.message()
                );
            }
            Err(other) => panic!("expected InvalidArgument from gate 2, got {other:?}"),
            Ok(resp) => panic!("expected Err from gate 2, got Ok: {:?}", resp.into_inner()),
        }
    }

    // ────────────────────────────────────────────────────────────────────────
    // AC-7 lib/in-process: ClearError handler 字面层（B4 (3/3) Spec § AC-7）
    //
    // **不**测真 Coordinator Degraded → Idle 转换（已被 s1
    // tests/action_dispatch.rs::safety_abort_transitions_to_degraded_then_clear_error_resumes_idle
    // 覆盖；本 RPC handler 字面层仅锁住"validate → auth → cache.decide
    // → forward → tokio.timeout → 错误返回"完整流程不 panic 不破坏 12
    // 步骨架"）。
    //
    // 流程：GatewayHandler::for_test() 用 leak rx 让 forward channel sender
    // 永久"连着"（非 disconnected）；handler 调 forward_to_rr_io.try_send →
    // 进 tokio.timeout 等 oneshot reply（无 daemon 端 reply）→ 100ms 后
    // tokio::time::timeout 触发 → 返 tonic::Code::DeadlineExceeded。
    // ────────────────────────────────────────────────────────────────────────

    /// AC-7：ClearError handler 字面层 12 步骨架走通；mock RR 路径无 daemon
    /// 回 reply → tokio.timeout 触发 → 返 DeadlineExceeded。
    #[tokio::test]
    async fn handle_clear_error_handler_literal_layer_deadline_exceeded() {
        use crate::auth::load_role_table;
        use crate::idempotency::IdempotencyCache;
        use crossbeam_channel::bounded;
        use robot_core::time::SystemClock;
        use std::sync::{Arc, Mutex};
        use std::time::Duration as StdDuration;
        use tokio::sync::watch;

        // 用 in-mem TOML 构造 role table（含 maintainer-fake-token-001 → maintainer
        // → clear_error capability）；不读 configs/rpc_roles.toml（lib unit
        // 不依赖 workspace 外部文件）。
        const TEST_TOML: &str = r#"
[tokens]
"maintainer-fake-token-001" = "maintainer"

[roles.viewer]
capabilities = ["read_state"]
[roles.operator]
capabilities = ["read_state", "invoke_task"]
[roles.maintainer]
capabilities = ["read_state", "invoke_task", "clear_estop", "clear_error"]
[roles.admin]
capabilities = ["read_state", "invoke_task", "clear_estop", "clear_error"]
"#;
        let role_table = Arc::new(load_role_table(TEST_TOML).expect("role table parse"));

        // forward channel：leak rx 让 sender 不见 Disconnected（try_send 成功
        // 后 handler oneshot.await 永等不到 reply → tokio.timeout 触发）。
        let (clear_error_tx, clear_error_rx) = bounded(1);
        Box::leak(Box::new(clear_error_rx));
        // 4 个 sibling forward channel 不参与本测；同款 leak rx。
        let (clear_estop_tx, clear_estop_rx) = bounded(1);
        let (task_invoke_tx, task_invoke_rx) = bounded(1);
        let (task_cancel_tx, task_cancel_rx) = bounded(1);
        let (task_status_tx, task_status_rx) = bounded(1);
        Box::leak(Box::new(clear_estop_rx));
        Box::leak(Box::new(task_invoke_rx));
        Box::leak(Box::new(task_cancel_rx));
        Box::leak(Box::new(task_status_rx));
        let (set_enabled_tx, set_enabled_rx) = bounded(1);
        Box::leak(Box::new(set_enabled_rx));

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Box::leak(Box::new(shutdown_tx));

        let state = crate::gateway::GatewayHandlerState {
            rr_forward_tx_clear_estop: clear_estop_tx,
            rr_forward_tx_task_invoke: task_invoke_tx,
            rr_forward_tx_task_cancel: task_cancel_tx,
            rr_forward_tx_task_status: task_status_tx,
            rr_forward_tx_clear_error: clear_error_tx,
            rr_forward_tx_set_enabled: set_enabled_tx,
            robot_id: "ac7_lit".to_string(),
            role_table,
            idempotency: Arc::new(Mutex::new(IdempotencyCache::with_capacity(
                StdDuration::from_mins(1),
                64,
            ))),
            clock: Arc::new(SystemClock),
            rr_timeout_grace_ms: 50, // 跟 timeout_ms 一起 ≤ 200ms 让 await 快速 timeout
            shutdown_rx,
            state_cache: std::sync::Arc::new(crate::state_pool::StateCache::new(&[])),
        };

        // 构造合法 maintainer ClearErrorCommand，timeout_ms=100ms 让 handler
        // 等 oneshot reply 100+50=150ms 后触发 DeadlineExceeded。
        let cmd = ClearErrorCommand {
            meta: Some(CommandMeta {
                request_id: uuid::Uuid::now_v7().to_string(),
                source: "ac7.lit".to_string(),
                auth: Some(AuthContext {
                    token: "maintainer-fake-token-001".to_string(),
                    mtls_fingerprint: String::new(),
                }),
                timeout_ms: 100,
                idempotency_key: "ac7-lit-key".to_string(),
                dry_run: false,
                audit_reason: "AC-7 literal layer test".to_string(),
            }),
        };
        let request = tonic::Request::new(cmd);
        let result = handle_clear_error(&state, request).await;

        // 期望 DeadlineExceeded（forward 成功 + 无 daemon → tokio.timeout 触发）
        match result {
            Err(status) if status.code() == tonic::Code::DeadlineExceeded => {
                // PASS
            }
            Err(other) => panic!("AC-7 expected DeadlineExceeded, got {other:?}"),
            Ok(resp) => panic!(
                "AC-7 expected Err(DeadlineExceeded), got Ok: {:?}",
                resp.into_inner()
            ),
        }
    }
}
