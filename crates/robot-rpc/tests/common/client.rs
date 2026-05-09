//! tonic gRPC client builder helpers + 4 个 fake token 常量 +
//! `CommandMeta` / `InvokeTaskCommand` / `CancelTaskCommand` /
//! `ClearErrorCommand` / `TaskHandleRef` 5 个 proto message builder。
//!
//! 测试侧用法：
//!
//! ```ignore
//! let req = build_invoke_task_command(
//!     Uuid::now_v7(),
//!     "ik-1",
//!     "test.client",
//!     MAINTAINER_TOKEN,
//!     ActionKindProto::ActionKindDwellMs,
//!     5_000,
//!     "happy_invoke_stream test",
//! );
//! let resp = client.invoke_task(req).await?;
//! ```

use uuid::Uuid;

use robot_rpc::proto::{
    ActionKindProto, AuthContext, CancelTaskCommand, ClearErrorCommand, CommandMeta,
    InvokeTaskCommand, TaskHandleRef,
};

// ============================================================================
// Fake token 常量（对应 configs/rpc_roles.toml + crates/robot-rpc/src/auth.rs）
// ============================================================================

pub const VIEWER_TOKEN: &str = "viewer-fake-token-001";
pub const OPERATOR_TOKEN: &str = "operator-fake-token-001";
pub const MAINTAINER_TOKEN: &str = "maintainer-fake-token-001";
#[allow(dead_code)] // 留作 future test 用
pub const ADMIN_TOKEN: &str = "admin-fake-token-001";

// ============================================================================
// proto message builders
// ============================================================================

#[allow(clippy::too_many_arguments)] // 6 个 RPC 字段 + audit_reason；包 struct 不增可读性
fn build_command_meta(
    request_id: Uuid,
    idempotency_key: &str,
    source: &str,
    auth_token: &str,
    timeout_ms: u64,
    audit_reason: &str,
    dry_run: bool,
) -> CommandMeta {
    CommandMeta {
        request_id: request_id.to_string(),
        source: source.to_string(),
        auth: Some(AuthContext {
            token: auth_token.to_string(),
            mtls_fingerprint: String::new(),
        }),
        timeout_ms,
        idempotency_key: idempotency_key.to_string(),
        dry_run,
        audit_reason: audit_reason.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn build_invoke_task_command(
    request_id: Uuid,
    idempotency_key: &str,
    source: &str,
    auth_token: &str,
    kind: ActionKindProto,
    timeout_ms: u64,
    audit_reason: &str,
) -> InvokeTaskCommand {
    InvokeTaskCommand {
        meta: Some(build_command_meta(
            request_id,
            idempotency_key,
            source,
            auth_token,
            timeout_ms,
            audit_reason,
            false,
        )),
        kind: kind.into(),
        // **重要**：fbs `InvokeTaskRequest.deadline_ns` 由 gateway 端用
        // `clock.now() + timeout_ms*1e6` 计算后填进 wire；proto
        // `InvokeTaskCommand.deadline_ns` 字段在当前 schema 是 absolute
        // monotonic ns（design §5.3 frozen），but task_handlers.rs 的
        // validate_invoke_task_command 把 cmd.deadline_ns 当 deadline_ns_offset_ms
        // 透传给 fbs（详 task_handlers.rs:206 注释）。**测试**直接把 0 填
        // 这里让 gateway 端 timeout_ms 主导 deadline 计算（避免 absolute ns
        // 传错）。
        deadline_ns: 0,
        priority: 0,
        payload_schema_id: 0,
        payload: Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn build_cancel_task_command(
    request_id: Uuid,
    idempotency_key: &str,
    source: &str,
    auth_token: &str,
    action_id: &str,
    timeout_ms: u64,
    audit_reason: &str,
) -> CancelTaskCommand {
    CancelTaskCommand {
        meta: Some(build_command_meta(
            request_id,
            idempotency_key,
            source,
            auth_token,
            timeout_ms,
            audit_reason,
            false,
        )),
        action_id: action_id.to_string(),
    }
}

#[must_use]
pub fn build_clear_error_command(
    request_id: Uuid,
    idempotency_key: &str,
    source: &str,
    auth_token: &str,
    timeout_ms: u64,
    audit_reason: &str,
) -> ClearErrorCommand {
    ClearErrorCommand {
        meta: Some(build_command_meta(
            request_id,
            idempotency_key,
            source,
            auth_token,
            timeout_ms,
            audit_reason,
            false,
        )),
    }
}

#[must_use]
pub fn build_task_handle_ref(request_id: Uuid, action_id: &str) -> TaskHandleRef {
    TaskHandleRef {
        request_id: request_id.to_string(),
        action_id: action_id.to_string(),
    }
}
