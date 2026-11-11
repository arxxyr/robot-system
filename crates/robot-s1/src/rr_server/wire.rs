//! `rr_server` wire codec 层 —— fbs request decode + response encode +
//! 错误码映射 helpers。
//!
//! ## 边界
//!
//! - **不**接 [`crate::coordinator::Coordinator`] / **不**起线程 / **不**碰
//!   iceoryx2；纯字节 ↔ Rust struct 转换 + 错误枚举 → [`ActionRpcStatus`]
//!   映射表。
//! - 4 个 RR topic 各有一对 `decode_*_request` + `encode_*_response_ok` /
//!   `encode_*_response_err`。命名跟 fbs schema table 名对齐
//!   （`InvokeTaskRequest` / `CancelTaskRequest` / `GetTaskStatusRequest` /
//!   `ClearErrorRequest`）。
//! - 错误映射 fn `submit_err_to_rpc_status` / `cancel_err_to_rpc_status` /
//!   `clear_err_to_rpc_status` 保持显式映射（`BusyExecuting → AlreadyExists` /
//!   `BackendStartFailed::* → FailedPrecondition` 已锁定）。

#![allow(clippy::doc_markdown)]

use robot_msg::build_helpers::{
    build_cancel_task_response_typed, build_clear_error_response_typed,
    build_get_task_status_response, build_invoke_task_response_typed,
};
use robot_msg::msg::{
    ActionFailureKindFb, ActionOutcomeKindFb, ActionRpcStatus, CancelTaskRequest as FbCancelReq,
    ClearErrorRequest as FbClearErrorReq, GetTaskStatusRequest as FbStatusReq, InvokeOutcome,
    InvokeTaskRequest as FbInvokeReq, RpcRole, SideEffect, TaskStateFb,
};

use crate::action::{
    ActionCancelError, ActionFailure, ActionOutcome, ActionStartError, ActionStatus, ClearError,
    SubmitError,
};

// ============================================================================
// Decoded request struct（owned String / Vec<u8>，跟 payload lifetime 解耦）
// ============================================================================

/// `InvokeTaskRequest` 解码后的 owned 视图。
#[derive(Debug, Clone)]
pub struct DecodedInvokeRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub source: String,
    pub role: RpcRole,
    pub audit_reason: String,
    pub kind: robot_msg::msg::ActionKindFb,
    pub deadline_ns: u64,
    pub priority: u32,
    pub payload_schema_id: u32,
    pub payload: Vec<u8>,
    pub stamp_ns: u64,
}

/// `CancelTaskRequest` 解码后的 owned 视图。
#[derive(Debug, Clone)]
pub struct DecodedCancelRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub source: String,
    pub role: RpcRole,
    pub audit_reason: String,
    pub action_id: String,
    pub deadline_ns: u64,
    pub stamp_ns: u64,
}

/// `GetTaskStatusRequest` 解码后的 owned 视图。
#[derive(Debug, Clone)]
pub struct DecodedStatusRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub source: String,
    pub role: RpcRole,
    pub audit_reason: String,
    pub action_id: String,
    pub deadline_ns: u64,
    pub stamp_ns: u64,
}

/// `ClearErrorRequest` 解码后的 owned 视图。
#[derive(Debug, Clone)]
pub struct DecodedClearErrorRequest {
    pub request_id: String,
    pub idempotency_key: String,
    pub source: String,
    pub role: RpcRole,
    pub audit_reason: String,
    pub deadline_ns: u64,
    pub stamp_ns: u64,
}

// ============================================================================
// Decode fn（typed flatbuffers root → owned struct）
// ============================================================================

/// 解码 `InvokeTaskRequest` body bytes。
///
/// runtime 已经在 `handle_active` 内做过 `flatbuffers::root::<...>` verifier
/// check，本 fn 内再调一次只是拿 owned 字段；理论上不会失败，但保留
/// `Result` 让 handler 的 ergonomic match 跟其它 decode fn 一致。
///
/// # Errors
///
/// `flatbuffers::root` 失败（runtime 已 verify 时不会触发）。
pub fn decode_invoke_request(
    payload: &[u8],
) -> Result<DecodedInvokeRequest, flatbuffers::InvalidFlatbuffer> {
    let req = flatbuffers::root::<FbInvokeReq>(payload)?;
    Ok(DecodedInvokeRequest {
        request_id: req.request_id().unwrap_or_default().to_string(),
        idempotency_key: req.idempotency_key().unwrap_or_default().to_string(),
        source: req.source().unwrap_or_default().to_string(),
        role: req.role(),
        audit_reason: req.audit_reason().unwrap_or_default().to_string(),
        kind: req.kind(),
        deadline_ns: req.deadline_ns(),
        priority: req.priority(),
        payload_schema_id: req.payload_schema_id(),
        payload: req.payload().map_or_else(Vec::new, |v| v.bytes().to_vec()),
        stamp_ns: req.stamp_ns(),
    })
}

/// 解码 `CancelTaskRequest` body bytes。
///
/// # Errors
///
/// `flatbuffers::root` 失败。
pub fn decode_cancel_request(
    payload: &[u8],
) -> Result<DecodedCancelRequest, flatbuffers::InvalidFlatbuffer> {
    let req = flatbuffers::root::<FbCancelReq>(payload)?;
    Ok(DecodedCancelRequest {
        request_id: req.request_id().unwrap_or_default().to_string(),
        idempotency_key: req.idempotency_key().unwrap_or_default().to_string(),
        source: req.source().unwrap_or_default().to_string(),
        role: req.role(),
        audit_reason: req.audit_reason().unwrap_or_default().to_string(),
        action_id: req.action_id().unwrap_or_default().to_string(),
        deadline_ns: req.deadline_ns(),
        stamp_ns: req.stamp_ns(),
    })
}

/// 解码 `GetTaskStatusRequest` body bytes。
///
/// # Errors
///
/// `flatbuffers::root` 失败。
pub fn decode_status_request(
    payload: &[u8],
) -> Result<DecodedStatusRequest, flatbuffers::InvalidFlatbuffer> {
    let req = flatbuffers::root::<FbStatusReq>(payload)?;
    Ok(DecodedStatusRequest {
        request_id: req.request_id().unwrap_or_default().to_string(),
        idempotency_key: req.idempotency_key().unwrap_or_default().to_string(),
        source: req.source().unwrap_or_default().to_string(),
        role: req.role(),
        audit_reason: req.audit_reason().unwrap_or_default().to_string(),
        action_id: req.action_id().unwrap_or_default().to_string(),
        deadline_ns: req.deadline_ns(),
        stamp_ns: req.stamp_ns(),
    })
}

/// 解码 `ClearErrorRequest` body bytes。
///
/// # Errors
///
/// `flatbuffers::root` 失败。
pub fn decode_clear_error_request(
    payload: &[u8],
) -> Result<DecodedClearErrorRequest, flatbuffers::InvalidFlatbuffer> {
    let req = flatbuffers::root::<FbClearErrorReq>(payload)?;
    Ok(DecodedClearErrorRequest {
        request_id: req.request_id().unwrap_or_default().to_string(),
        idempotency_key: req.idempotency_key().unwrap_or_default().to_string(),
        source: req.source().unwrap_or_default().to_string(),
        role: req.role(),
        audit_reason: req.audit_reason().unwrap_or_default().to_string(),
        deadline_ns: req.deadline_ns(),
        stamp_ns: req.stamp_ns(),
    })
}

// ============================================================================
// Encode response（success + error 双路径）
// ============================================================================

/// 编 `InvokeTaskResponse` (status = `Ok`) bytes。
#[must_use]
pub fn encode_invoke_response_ok(
    request_id: &str,
    action_id: &str,
    accepted_at_ns: u64,
) -> Vec<u8> {
    build_invoke_task_response_typed(
        request_id,
        ActionRpcStatus::Ok,
        "",
        action_id,
        accepted_at_ns,
        SideEffect::Committed,
        true,
        InvokeOutcome::Registered,
    )
}

/// 编 `InvokeTaskResponse` (status != `Ok`) bytes。
#[must_use]
pub fn encode_invoke_response_err(
    request_id: &str,
    action_id: &str,
    status: ActionRpcStatus,
    error_msg: &str,
) -> Vec<u8> {
    debug_assert!(
        status != ActionRpcStatus::Ok,
        "encode_invoke_response_err called with status=Ok; use encode_invoke_response_ok"
    );
    build_invoke_task_response_typed(
        request_id,
        status,
        error_msg,
        action_id,
        /* accepted_at_ns */ 0,
        SideEffect::None,
        true,
        InvokeOutcome::NotSubmitted,
    )
}

/// 已提交后的 drain 失败保留真实提交时刻与实际补偿返回值。
#[must_use]
pub fn encode_invoke_drain_failure(
    request_id: &str,
    action_id: &str,
    accepted_at_ns: u64,
    error_msg: &str,
    cancel_ok: bool,
) -> Vec<u8> {
    let outcome = if cancel_ok {
        InvokeOutcome::DrainFailedCancelAccepted
    } else {
        InvokeOutcome::DrainFailedCancelRejected
    };
    build_invoke_task_response_typed(
        request_id,
        ActionRpcStatus::Internal,
        error_msg,
        action_id,
        accepted_at_ns,
        SideEffect::Committed,
        true,
        outcome,
    )
}

/// 编 `CancelTaskResponse` (status = `Ok`) bytes。
#[must_use]
pub fn encode_cancel_response_ok(request_id: &str, action_id: &str, cancel_at_ns: u64) -> Vec<u8> {
    build_cancel_task_response_typed(
        request_id,
        ActionRpcStatus::Ok,
        "",
        action_id,
        cancel_at_ns,
        SideEffect::Committed,
        true,
    )
}

/// 编 `CancelTaskResponse` (status != `Ok`) bytes。
#[must_use]
pub fn encode_cancel_response_err(
    request_id: &str,
    action_id: &str,
    status: ActionRpcStatus,
    error_msg: &str,
) -> Vec<u8> {
    debug_assert!(
        status != ActionRpcStatus::Ok,
        "encode_cancel_response_err called with status=Ok; use encode_cancel_response_ok"
    );
    build_cancel_task_response_typed(
        request_id,
        status,
        error_msg,
        action_id,
        /* cancel_at_ns */ 0,
        SideEffect::None,
        true,
    )
}

/// 编 `GetTaskStatusResponse` (status = `Ok`) bytes，按 [`ActionStatus`]
/// 映射 `state` / `progress` / `outcome` / `failure` 字段。
#[must_use]
pub fn encode_status_response_ok(
    request_id: &str,
    action_id: &str,
    status: &ActionStatus,
) -> Vec<u8> {
    let (state, progress, outcome, failure) = action_status_to_fbs_fields(status);
    build_get_task_status_response(
        request_id,
        ActionRpcStatus::Ok,
        "",
        action_id,
        state,
        progress,
        outcome,
        failure,
    )
}

/// 编 `GetTaskStatusResponse` (status != `Ok`) bytes。`state` 填 `Unknown`，
/// `progress` 填 0.0，`outcome` / `failure` 都填 None。
#[must_use]
pub fn encode_status_response_err(
    request_id: &str,
    action_id: &str,
    status: ActionRpcStatus,
    error_msg: &str,
) -> Vec<u8> {
    debug_assert!(
        status != ActionRpcStatus::Ok,
        "encode_status_response_err called with status=Ok; use encode_status_response_ok"
    );
    build_get_task_status_response(
        request_id,
        status,
        error_msg,
        action_id,
        TaskStateFb::Unknown,
        0.0,
        None,
        None,
    )
}

/// 编 `ClearErrorResponse` (status = `Ok`) bytes。
#[must_use]
pub fn encode_clear_error_response_ok(request_id: &str, cleared_at_ns: u64) -> Vec<u8> {
    build_clear_error_response_typed(
        request_id,
        ActionRpcStatus::Ok,
        "",
        cleared_at_ns,
        SideEffect::Committed,
        true,
    )
}

/// 编 `ClearErrorResponse` (status != `Ok`) bytes。
#[must_use]
pub fn encode_clear_error_response_err(
    request_id: &str,
    status: ActionRpcStatus,
    error_msg: &str,
) -> Vec<u8> {
    debug_assert!(
        status != ActionRpcStatus::Ok,
        "encode_clear_error_response_err called with status=Ok; use encode_clear_error_response_ok"
    );
    build_clear_error_response_typed(
        request_id,
        status,
        error_msg,
        /* cleared_at_ns */ 0,
        SideEffect::None,
        true,
    )
}

// ============================================================================
// 错误码映射（lib enum → wire ActionRpcStatus）
// ============================================================================

/// [`SubmitError`] → [`ActionRpcStatus`] 映射（outline §7.1 round 4 终版）。
///
/// - `InvalidRequest` / `UnsupportedSchema` → `InvalidArgument`
/// - `NotReady` / `CoordinatorDegraded` / `BackendStartFailed::*` →
///   `FailedPrecondition`
/// - `BusyExecuting` → `AlreadyExists`（reviewer round 4 拍板：单 action 序列
///   化语义 = 资源已存在）
/// - `ShuttingDown` → `Unavailable`
/// - `NoBackendForKind` → `Unimplemented`
#[must_use]
pub fn submit_err_to_rpc_status(err: &SubmitError) -> ActionRpcStatus {
    match err {
        SubmitError::InvalidRequest(_) | SubmitError::UnsupportedSchema(_, _) => {
            ActionRpcStatus::InvalidArgument
        }
        SubmitError::NotReady | SubmitError::CoordinatorDegraded => {
            ActionRpcStatus::FailedPrecondition
        }
        SubmitError::BackendStartFailed(start_err) => match start_err {
            ActionStartError::Rejected(_)
            | ActionStartError::PayloadDecode(_)
            | ActionStartError::HardwareMismatch(_) => ActionRpcStatus::FailedPrecondition,
        },
        SubmitError::BusyExecuting(_) => ActionRpcStatus::AlreadyExists,
        SubmitError::ShuttingDown => ActionRpcStatus::Unavailable,
        SubmitError::NoBackendForKind(_) => ActionRpcStatus::Unimplemented,
    }
}

/// [`ActionCancelError`] → [`ActionRpcStatus`] 映射（outline §7.2）。
///
/// - `NotExecuting` → `NotFound`
/// - `BackendError` → `Internal`
#[must_use]
pub fn cancel_err_to_rpc_status(err: &ActionCancelError) -> ActionRpcStatus {
    match err {
        ActionCancelError::NotExecuting => ActionRpcStatus::NotFound,
        ActionCancelError::BackendError(_) => ActionRpcStatus::Internal,
    }
}

/// [`ClearError`] → [`ActionRpcStatus`] 映射（outline §7.2）。
///
/// - `NotDegraded` → `FailedPrecondition`
/// - `InternalError` → `Internal`
#[must_use]
pub fn clear_err_to_rpc_status(err: &ClearError) -> ActionRpcStatus {
    match err {
        ClearError::NotDegraded(_) => ActionRpcStatus::FailedPrecondition,
        ClearError::InternalError(_) => ActionRpcStatus::Internal,
    }
}

// ============================================================================
// ActionStatus → fbs (state, progress, outcome, failure)
// ============================================================================

/// `(state, progress, outcome, failure)` 元组返出去给
/// [`build_get_task_status_response`]。命名 type alias 避免 clippy
/// `type_complexity`，并跟 fbs schema 字段顺序对齐。
type StatusFbsFields<'a> = (
    TaskStateFb,
    f32,
    Option<(ActionOutcomeKindFb, &'a str)>,
    Option<(ActionFailureKindFb, &'a str)>,
);

fn action_status_to_fbs_fields(status: &ActionStatus) -> StatusFbsFields<'_> {
    match status {
        ActionStatus::Pending => (TaskStateFb::Pending, 0.0, None, None),
        ActionStatus::Executing { progress } => (TaskStateFb::Executing, *progress, None, None),
        ActionStatus::Completed(outcome) => {
            let (kind, warning) = action_outcome_to_fbs(outcome);
            (TaskStateFb::Completed, 0.0, Some((kind, warning)), None)
        }
        ActionStatus::Failed(failure) => {
            let (kind, reason) = action_failure_to_fbs(failure);
            (TaskStateFb::Failed, 0.0, None, Some((kind, reason)))
        }
        ActionStatus::Cancelled => (TaskStateFb::Cancelled, 0.0, None, None),
    }
}

fn action_outcome_to_fbs(outcome: &ActionOutcome) -> (ActionOutcomeKindFb, &str) {
    match outcome {
        ActionOutcome::Success => (ActionOutcomeKindFb::Success, ""),
        ActionOutcome::PartialSuccess { warning } => {
            (ActionOutcomeKindFb::PartialSuccess, warning.as_str())
        }
    }
}

fn action_failure_to_fbs(failure: &ActionFailure) -> (ActionFailureKindFb, &str) {
    match failure {
        ActionFailure::DeadlineExceeded => (ActionFailureKindFb::DeadlineExceeded, ""),
        ActionFailure::PreconditionFailed(reason) => {
            (ActionFailureKindFb::PreconditionFailed, reason.as_str())
        }
        ActionFailure::AdapterError(reason) => (ActionFailureKindFb::AdapterError, reason.as_str()),
        ActionFailure::SafetyAbort(reason) => (ActionFailureKindFb::SafetyAbort, reason.as_str()),
        ActionFailure::InternalError(reason) => {
            (ActionFailureKindFb::InternalError, reason.as_str())
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::ActionKind;
    use robot_msg::build_helpers::{
        build_cancel_task_request, build_clear_error_request, build_get_task_status_request,
        build_invoke_task_request,
    };
    use robot_msg::msg::ActionKindFb;

    // ----- decode round-trip -----

    #[test]
    fn decode_invoke_round_trip() {
        let bytes = build_invoke_task_request(
            "req-001",
            "idem-001",
            "cli.user",
            RpcRole::Maintainer,
            "B4 demo",
            ActionKindFb::DwellMs,
            123_456,
            7,
            0,
            &[1, 2, 3, 4],
            999,
        );
        let d = decode_invoke_request(&bytes).expect("decode");
        assert_eq!(d.request_id, "req-001");
        assert_eq!(d.idempotency_key, "idem-001");
        assert_eq!(d.source, "cli.user");
        assert_eq!(d.role, RpcRole::Maintainer);
        assert_eq!(d.audit_reason, "B4 demo");
        assert_eq!(d.kind, ActionKindFb::DwellMs);
        assert_eq!(d.deadline_ns, 123_456);
        assert_eq!(d.priority, 7);
        assert_eq!(d.payload_schema_id, 0);
        assert_eq!(d.payload, vec![1, 2, 3, 4]);
        assert_eq!(d.stamp_ns, 999);
    }

    #[test]
    fn decode_cancel_round_trip() {
        let bytes = build_cancel_task_request(
            "req-002",
            "idem-002",
            "cli.user",
            RpcRole::Maintainer,
            "B4 demo",
            "act-uuid-7",
            123_456,
            999,
        );
        let d = decode_cancel_request(&bytes).expect("decode");
        assert_eq!(d.request_id, "req-002");
        assert_eq!(d.action_id, "act-uuid-7");
        assert_eq!(d.deadline_ns, 123_456);
    }

    #[test]
    fn decode_status_round_trip() {
        let bytes = build_get_task_status_request(
            "req-003",
            "idem-003",
            "cli.user",
            RpcRole::Viewer,
            "B4 demo",
            "act-uuid-8",
            0,
            0,
        );
        let d = decode_status_request(&bytes).expect("decode");
        assert_eq!(d.request_id, "req-003");
        assert_eq!(d.role, RpcRole::Viewer);
        assert_eq!(d.action_id, "act-uuid-8");
    }

    #[test]
    fn decode_clear_error_round_trip() {
        let bytes = build_clear_error_request(
            "req-004",
            "idem-004",
            "cli.user",
            RpcRole::Maintainer,
            "B4 demo",
            123,
            456,
        );
        let d = decode_clear_error_request(&bytes).expect("decode");
        assert_eq!(d.request_id, "req-004");
        assert_eq!(d.deadline_ns, 123);
        assert_eq!(d.stamp_ns, 456);
    }

    #[test]
    fn decode_garbage_returns_err() {
        let bytes = vec![0xAA, 0xBB, 0xCC, 0xDD];
        assert!(decode_invoke_request(&bytes).is_err());
        assert!(decode_cancel_request(&bytes).is_err());
        assert!(decode_status_request(&bytes).is_err());
        assert!(decode_clear_error_request(&bytes).is_err());
    }

    // ----- encode round-trip -----

    #[test]
    fn encode_invoke_ok_round_trip() {
        let bytes = encode_invoke_response_ok("req-001", "act-uuid-7", 42);
        let resp =
            flatbuffers::root::<robot_msg::msg::InvokeTaskResponse>(&bytes).expect("decode resp");
        assert_eq!(resp.request_id().unwrap(), "req-001");
        assert_eq!(resp.status(), ActionRpcStatus::Ok);
        assert_eq!(resp.error_msg().unwrap(), "");
        assert_eq!(resp.action_id().unwrap(), "act-uuid-7");
        assert_eq!(resp.accepted_at_ns(), 42);
    }

    #[test]
    fn encode_invoke_err_zeroes_accepted_at_ns() {
        let bytes = encode_invoke_response_err(
            "req-001",
            "act-uuid-7",
            ActionRpcStatus::AlreadyExists,
            "busy",
        );
        let resp =
            flatbuffers::root::<robot_msg::msg::InvokeTaskResponse>(&bytes).expect("decode resp");
        assert_eq!(resp.status(), ActionRpcStatus::AlreadyExists);
        assert_eq!(resp.error_msg().unwrap(), "busy");
        assert_eq!(resp.accepted_at_ns(), 0);
    }

    #[test]
    fn encode_status_executing_round_trip() {
        let st = ActionStatus::Executing { progress: 0.42 };
        let bytes = encode_status_response_ok("req-001", "act-uuid-7", &st);
        let resp = flatbuffers::root::<robot_msg::msg::GetTaskStatusResponse>(&bytes)
            .expect("decode resp");
        assert_eq!(resp.status(), ActionRpcStatus::Ok);
        assert_eq!(resp.state(), TaskStateFb::Executing);
        assert!((resp.progress() - 0.42).abs() < f32::EPSILON);
        assert!(resp.outcome().is_none());
        assert!(resp.failure().is_none());
    }

    #[test]
    fn encode_status_completed_with_partial_success_round_trip() {
        let st = ActionStatus::Completed(ActionOutcome::PartialSuccess {
            warning: "limit reduced".into(),
        });
        let bytes = encode_status_response_ok("req-001", "act-uuid-7", &st);
        let resp = flatbuffers::root::<robot_msg::msg::GetTaskStatusResponse>(&bytes)
            .expect("decode resp");
        assert_eq!(resp.state(), TaskStateFb::Completed);
        let outcome = resp.outcome().expect("outcome present");
        assert_eq!(outcome.kind(), ActionOutcomeKindFb::PartialSuccess);
        assert_eq!(outcome.warning().unwrap(), "limit reduced");
    }

    #[test]
    fn encode_status_failed_with_safety_abort_round_trip() {
        let st = ActionStatus::Failed(ActionFailure::SafetyAbort("e-stop".into()));
        let bytes = encode_status_response_ok("req-001", "act-uuid-7", &st);
        let resp = flatbuffers::root::<robot_msg::msg::GetTaskStatusResponse>(&bytes)
            .expect("decode resp");
        assert_eq!(resp.state(), TaskStateFb::Failed);
        let failure = resp.failure().expect("failure present");
        assert_eq!(failure.kind(), ActionFailureKindFb::SafetyAbort);
        assert_eq!(failure.reason().unwrap(), "e-stop");
    }

    #[test]
    fn encode_status_cancelled_round_trip() {
        let bytes = encode_status_response_ok("req-001", "act-uuid-7", &ActionStatus::Cancelled);
        let resp = flatbuffers::root::<robot_msg::msg::GetTaskStatusResponse>(&bytes)
            .expect("decode resp");
        assert_eq!(resp.state(), TaskStateFb::Cancelled);
    }

    // ----- 错误码映射表 -----

    #[test]
    fn submit_err_mapping_outline_round4() {
        // 锁住 outline §7.1 round 4 终版表（避免后续 reviewer round 误改）。
        assert_eq!(
            submit_err_to_rpc_status(&SubmitError::InvalidRequest("x".into())),
            ActionRpcStatus::InvalidArgument
        );
        assert_eq!(
            submit_err_to_rpc_status(&SubmitError::UnsupportedSchema("b".into(), 0xFFFF)),
            ActionRpcStatus::InvalidArgument
        );
        assert_eq!(
            submit_err_to_rpc_status(&SubmitError::NotReady),
            ActionRpcStatus::FailedPrecondition
        );
        assert_eq!(
            submit_err_to_rpc_status(&SubmitError::CoordinatorDegraded),
            ActionRpcStatus::FailedPrecondition
        );
        assert_eq!(
            submit_err_to_rpc_status(&SubmitError::BackendStartFailed(
                ActionStartError::Rejected("x".into())
            )),
            ActionRpcStatus::FailedPrecondition
        );
        assert_eq!(
            submit_err_to_rpc_status(&SubmitError::BackendStartFailed(
                ActionStartError::PayloadDecode("x".into())
            )),
            ActionRpcStatus::FailedPrecondition
        );
        assert_eq!(
            submit_err_to_rpc_status(&SubmitError::BackendStartFailed(
                ActionStartError::HardwareMismatch("x".into())
            )),
            ActionRpcStatus::FailedPrecondition
        );
        assert_eq!(
            submit_err_to_rpc_status(&SubmitError::BusyExecuting(
                crate::action::ActionId::new_v7()
            )),
            ActionRpcStatus::AlreadyExists
        );
        assert_eq!(
            submit_err_to_rpc_status(&SubmitError::ShuttingDown),
            ActionRpcStatus::Unavailable
        );
        assert_eq!(
            submit_err_to_rpc_status(&SubmitError::NoBackendForKind(ActionKind::DwellMs)),
            ActionRpcStatus::Unimplemented
        );
    }

    #[test]
    fn cancel_err_mapping() {
        assert_eq!(
            cancel_err_to_rpc_status(&ActionCancelError::NotExecuting),
            ActionRpcStatus::NotFound
        );
        assert_eq!(
            cancel_err_to_rpc_status(&ActionCancelError::BackendError("x".into())),
            ActionRpcStatus::Internal
        );
    }

    #[test]
    fn clear_err_mapping() {
        assert_eq!(
            clear_err_to_rpc_status(&ClearError::NotDegraded("Idle".into())),
            ActionRpcStatus::FailedPrecondition
        );
        assert_eq!(
            clear_err_to_rpc_status(&ClearError::InternalError("x".into())),
            ActionRpcStatus::Internal
        );
    }
}
