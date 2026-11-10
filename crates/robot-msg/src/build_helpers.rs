//! `FlatBuffers` body builder —— 非 RT 路径构造 `JointState` / `JointCommand` /
//! `Heartbeat` / `EStopEvent` body bytes 的统一入口。
//!
//! ### RT 约束
//!
//! 本模块的所有函数**严禁在 RT 线程调用**（`docs/00_design.md` §0 / §4.9）。
//! `FlatBufferBuilder::new` 会分配 `Vec`，`create_vector` / `create_string` 会
//! 可能 realloc；`finish` + `finished_data().to_vec()` 再 `memcpy` 一次。
//!
//! 合法调用方：
//! - `robot-s1::mock_s1`（pub `JointCommand` / `Heartbeat`）
//! - `robot-s0::pub_thread`（pub `JointState` / `Heartbeat`，非 RT）
//! - `robot-s0::ipc_thread`（收 `EStop` RPC 后发 `EStopEvent` topic，非 RT）
//! - `robot-tests::hello_pubsub`、`iceoryx2_rt_baseline` 等测试 binary
//!
//! ### 返回值
//!
//! 所有函数返回 `Vec<u8>`。调用方可以 `SampleWriter::set_body_bytes(&vec)`
//! 一次 memcpy 到 iceoryx2 loan 内存。如果未来想节省一次拷贝，可以加
//! `build_into_writer(&mut w, args)` 的变体——但 Step 6 Part 2 scope 内不做。
//!
//! ### 测试
//!
//! 每个 builder 有一条 round-trip 测试：build → `flatbuffers::root::<T>(bytes)`
//! 解析回来，确认字段值。

use flatbuffers::FlatBufferBuilder;

use crate::msg::{
    ActionEventEnvelope, ActionEventEnvelopeArgs, ActionEventKindFb, ActionFailureFb,
    ActionFailureFbArgs, ActionFailureKindFb, ActionKindFb, ActionOutcomeFb, ActionOutcomeFbArgs,
    ActionOutcomeKindFb, ActionRpcStatus, CancelTaskRequest, CancelTaskRequestArgs,
    CancelTaskResponse, CancelTaskResponseArgs, ClearEStopRequest, ClearEStopRequestArgs,
    ClearEStopResponse, ClearEStopResponseArgs, ClearEStopStatus, ClearErrorRequest,
    ClearErrorRequestArgs, ClearErrorResponse, ClearErrorResponseArgs, ControlMode, EStopEvent,
    EStopEventArgs, GetTaskStatusRequest, GetTaskStatusRequestArgs, GetTaskStatusResponse,
    GetTaskStatusResponseArgs, Heartbeat, HeartbeatArgs, InvokeOutcome, InvokeTaskRequest,
    InvokeTaskRequestArgs, InvokeTaskResponse, InvokeTaskResponseArgs, JointCommand,
    JointCommandArgs, JointState, JointStateArgs, RpcRole, RpcStatus, S0ControlLink, S0FaultCode,
    S0Mode, S0OutputMode, S0SafetyState, SetEnabledRequest, SetEnabledRequestArgs,
    SetEnabledResponse, SetEnabledResponseArgs, SetEnabledStatus, SideEffect, StaleAction,
    TaskStateFb,
};

// ============================================================================
// JointState（`robot.msg.JointState`）
// ============================================================================

/// 构造 `JointState` body bytes（仅关节字段；S0 自身状态字段 = Unknown）。
///
/// - `layout_id` 来自上游 `JointLayout`。demo / test 常用 `0xCAFE_BABE_DEAD_BEEF`。
/// - `positions` 长度应与 `JointLayout.names` 相同；本函数不校验。
/// - `velocities` / `torques` 为空 slice 时写入 `None`（不占字节）。
/// - `frame_id` 为空字符串时写入 `None`。
/// - `s0_mode` / `s0_control_link` / `s0_output_mode` / `s0_safety_state` /
///   `s0_fault_code` 全部留 `Unknown`（`FlatBuffers` 默认值不写 vtable）。
///   想带 S0 状态用 [`build_joint_state_with_s0`].
#[must_use]
pub fn build_joint_state(
    layout_id: u64,
    stamp_ns: u64,
    positions: &[f64],
    velocities: &[f64],
    torques: &[f64],
    frame_id: &str,
) -> Vec<u8> {
    build_joint_state_with_s0(
        layout_id,
        stamp_ns,
        positions,
        velocities,
        torques,
        frame_id,
        S0Mode::Unknown,
        S0ControlLink::Unknown,
        S0OutputMode::Unknown,
        S0SafetyState::TorqueUnknown,
        S0FaultCode::Unknown,
    )
}

/// 构造 `JointState` body bytes，**带 S0 自身状态字段**（task-30 v9 pre-2）。
///
/// `pub_thread.publish_joint_state` 调本变体把当前 `S0StateSnapshot` 镜像
/// 写进 wire body，让外部订阅者一次拿到 joint + S0 mode + `safety_state`。
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_joint_state_with_s0(
    layout_id: u64,
    stamp_ns: u64,
    positions: &[f64],
    velocities: &[f64],
    torques: &[f64],
    frame_id: &str,
    s0_mode: S0Mode,
    s0_control_link: S0ControlLink,
    s0_output_mode: S0OutputMode,
    s0_safety_state: S0SafetyState,
    s0_fault_code: S0FaultCode,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(256);
    let pos = fbb.create_vector::<f64>(positions);
    let vel = if velocities.is_empty() {
        None
    } else {
        Some(fbb.create_vector::<f64>(velocities))
    };
    let eff = if torques.is_empty() {
        None
    } else {
        Some(fbb.create_vector::<f64>(torques))
    };
    let frame = if frame_id.is_empty() {
        None
    } else {
        Some(fbb.create_string(frame_id))
    };
    let js = JointState::create(
        &mut fbb,
        &JointStateArgs {
            layout_id,
            position_rad: Some(pos),
            velocity_rps: vel,
            effort_nm: eff,
            stamp_ns,
            frame_id: frame,
            s0_mode,
            s0_control_link,
            s0_output_mode,
            s0_safety_state,
            s0_fault_code,
        },
    );
    fbb.finish(js, None);
    fbb.finished_data().to_vec()
}

// ============================================================================
// JointCommand（`robot.msg.JointCommand`）
// ============================================================================

/// 构造 `JointCommand` body bytes。
///
/// - `mode` 用 flatc 生成的 enum（`crate::msg::ControlMode`）。
/// - `target` 长度与 `JointLayout.names` 相同。
/// - `deadline_ns` 基于 `CLOCK_MONOTONIC_RAW`；超过此时刻的命令在 S0 侧丢弃。
/// - `priority`：0..=255，大者优先（§3.5）。默认场景用 10。
/// - `on_timeout`：heartbeat 失联后此条命令的降级策略。默认 `Hold`。
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_joint_command(
    layout_id: u64,
    mode: ControlMode,
    target: &[f64],
    deadline_ns: u64,
    priority: u8,
    on_timeout: StaleAction,
    source_task_id: u64,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(256);
    let tgt = fbb.create_vector::<f64>(target);
    let cmd = JointCommand::create(
        &mut fbb,
        &JointCommandArgs {
            layout_id,
            mode,
            target: Some(tgt),
            deadline_ns,
            priority,
            source_task_id,
            on_timeout,
            raw_velocities: None,
        },
    );
    fbb.finish(cmd, None);
    fbb.finished_data().to_vec()
}

/// 构造显式位置/raw 速度混合命令；两个向量均沿 `JointLayout` 配置槽位。
/// `target` 只承载位置 rad，轮槽必须为零；`raw_velocities` 的臂槽必须为零。
/// 接收端负责校验配置、维度、允许的 raw 值及时间窗口；超时固定 Hold。
#[must_use]
pub fn build_mixed_joint_command(
    layout_id: u64,
    target: &[f64],
    raw_velocities: &[i16],
    deadline_ns: u64,
    priority: u8,
    source_task_id: u64,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(256);
    let target = fbb.create_vector(target);
    let raw_velocities = fbb.create_vector(raw_velocities);
    let command = JointCommand::create(
        &mut fbb,
        &JointCommandArgs {
            layout_id,
            mode: ControlMode::MixedPositionRawVelocity,
            target: Some(target),
            deadline_ns,
            priority,
            source_task_id,
            on_timeout: StaleAction::Hold,
            raw_velocities: Some(raw_velocities),
        },
    );
    fbb.finish(command, None);
    fbb.finished_data().to_vec()
}

// ============================================================================
// Heartbeat（`robot.msg.Heartbeat`）
// ============================================================================

/// 构造 `Heartbeat` body bytes。
///
/// - `source_id`：约定 `1 = S0`, `2 = S1`, `100.. = policy_host`.
/// - `seq`：单调递增；订阅端用 seq 判是否有漏。
/// - `tick_rate_hz`：方便订阅端快速校对周期；S1 = 50.0。
/// - `stamp_ns`：`CLOCK_MONOTONIC_RAW`。
#[must_use]
pub fn build_heartbeat(source_id: u32, seq: u64, tick_rate_hz: f64, stamp_ns: u64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(64);
    let hb = Heartbeat::create(
        &mut fbb,
        &HeartbeatArgs {
            source_id,
            seq,
            tick_rate_hz,
            stamp_ns,
        },
    );
    fbb.finish(hb, None);
    fbb.finished_data().to_vec()
}

// ============================================================================
// EStopEvent（`robot.msg.EStopEvent`）
// ============================================================================

/// 构造 `EStopEvent` body bytes。
///
/// - `reason`：人读字符串，保留到 recorder / RPC 回执。
/// - `source`：`"button"` / `"rpc"` / `"s1_detect"` 三种之一（§3.5 约定）。
/// - `latched`：S0 是否已把 latch 持久化到磁盘（§6.6）。
#[must_use]
pub fn build_estop_event(reason: &str, stamp_ns: u64, source: &str, latched: bool) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let reason_off = fbb.create_string(reason);
    let source_off = fbb.create_string(source);
    let ev = EStopEvent::create(
        &mut fbb,
        &EStopEventArgs {
            reason: Some(reason_off),
            stamp_ns,
            source: Some(source_off),
            latched,
        },
    );
    fbb.finish(ev, None);
    fbb.finished_data().to_vec()
}

// ============================================================================
// ClearEStopRequest（`robot.msg.ClearEStopRequest`）—— v0.3 A1
// ============================================================================

/// 构造 `ClearEStopRequest` body bytes（v0.3 A1 `ClearEStop` control-plane
/// slice 跨进程 RR client → server payload）。
///
/// **字段语义** + **`role` 字段安全边界** 详 [`crate::markers::ClearEStopRequest`]
/// + `crates/robot-msg/schemas/clear_estop.fbs`。
///
/// 调用方：
/// - `robot-rpc::clear_estop` handler（v0.3 A1-3 落地后真起 server）。
/// - `crates/robot-tests/src/bin/clear_estop_smoke.rs`（A1-4 落地）。
/// - 单测 / 集成测可以直接调，绕过 RPC 端 auth / idempotency 验证。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn build_clear_estop_request(
    request_id: &str,
    idempotency_key: &str,
    source: &str,
    role: RpcRole,
    confirmation: &str,
    audit_reason: &str,
    dry_run: bool,
    deadline_ns: u64,
    stamp_ns: u64,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(256);
    let request_id_off = fbb.create_string(request_id);
    let idempotency_off = fbb.create_string(idempotency_key);
    let source_off = fbb.create_string(source);
    let confirmation_off = fbb.create_string(confirmation);
    let audit_off = fbb.create_string(audit_reason);
    let req = ClearEStopRequest::create(
        &mut fbb,
        &ClearEStopRequestArgs {
            request_id: Some(request_id_off),
            idempotency_key: Some(idempotency_off),
            source: Some(source_off),
            role,
            confirmation: Some(confirmation_off),
            audit_reason: Some(audit_off),
            dry_run,
            deadline_ns,
            stamp_ns,
        },
    );
    fbb.finish(req, None);
    fbb.finished_data().to_vec()
}

// ============================================================================
// ClearEStopResponse（`robot.msg.ClearEStopResponse`）—— v0.3 A1
// ============================================================================

/// 构造 `ClearEStopResponse` body bytes（v0.3 A1 `ClearEStop` control-plane
/// slice 跨进程 RR server → client reply payload）。
///
/// **字段语义** 详 [`crate::markers::ClearEStopResponse`] +
/// `crates/robot-msg/schemas/clear_estop.fbs`。
///
/// 调用方：
/// - `robot-s0::ipc_thread`（A1-2 落地后真在 daemon 端发 reply）。
/// - 单测 / 集成测可直接调。
#[must_use]
pub fn build_clear_estop_response(
    request_id: &str,
    status: RpcStatus,
    error_msg: &str,
    cleared_at_ns: u64,
    dry_run: bool,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let request_id_off = fbb.create_string(request_id);
    let error_msg_off = fbb.create_string(error_msg);
    let resp = ClearEStopResponse::create(
        &mut fbb,
        &ClearEStopResponseArgs {
            request_id: Some(request_id_off),
            status,
            error_msg: Some(error_msg_off),
            cleared_at_ns,
            dry_run,
            domain: ClearEStopStatus::Unknown,
            superseded: false,
            effect: SideEffect::Unknown,
            retired: false,
        },
    );
    fbb.finish(resp, None);
    fbb.finished_data().to_vec()
}

/// 清锁存响应的事务领域结果与实际副作用元数据。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClearResponseMeta {
    /// 清锁存事务领域结果。
    pub domain: ClearEStopStatus,
    /// 本次事务是否已被后续安全事件撤销。
    pub superseded: bool,
    /// 实际已发生的副作用阶段。
    pub effect: SideEffect,
    /// 本次事务是否完成退休。
    pub retired: bool,
}

/// 构造带事务元数据的控制响应；只编码调用方提供的实际结果。
#[must_use]
pub fn build_clear_estop_response_typed(
    request_id: &str,
    status: RpcStatus,
    error_msg: &str,
    cleared_at_ns: u64,
    dry_run: bool,
    meta: ClearResponseMeta,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let request_id_off = fbb.create_string(request_id);
    let error_msg_off = fbb.create_string(error_msg);
    let resp = ClearEStopResponse::create(
        &mut fbb,
        &ClearEStopResponseArgs {
            request_id: Some(request_id_off),
            status,
            error_msg: Some(error_msg_off),
            cleared_at_ns,
            dry_run,
            domain: meta.domain,
            superseded: meta.superseded,
            effect: meta.effect,
            retired: meta.retired,
        },
    );
    fbb.finish(resp, None);
    fbb.finished_data().to_vec()
}

// ============================================================================
// SetEnabledRequest / SetEnabledResponse（`robot.msg.SetEnabled*`）—— v0.5 候选 C
// ============================================================================

/// 构造 `SetEnabledRequest` body bytes（gateway → S0 `set_enabled` RR request）。
/// 字段语义详 [`crate::markers::SetEnabledRequest`] +
/// `crates/robot-msg/schemas/set_enabled.fbs`。
///
/// 调用方：`robot-rpc` gateway `set_enabled` handler / 单测 / 集成测。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn build_set_enabled_request(
    request_id: &str,
    idempotency_key: &str,
    source: &str,
    role: RpcRole,
    enabled: bool,
    dry_run: bool,
    deadline_ns: u64,
    audit_reason: &str,
    stamp_ns: u64,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(256);
    let request_id_off = fbb.create_string(request_id);
    let idempotency_off = fbb.create_string(idempotency_key);
    let source_off = fbb.create_string(source);
    let audit_off = fbb.create_string(audit_reason);
    let req = SetEnabledRequest::create(
        &mut fbb,
        &SetEnabledRequestArgs {
            request_id: Some(request_id_off),
            idempotency_key: Some(idempotency_off),
            source: Some(source_off),
            role,
            enabled,
            dry_run,
            deadline_ns,
            audit_reason: Some(audit_off),
            stamp_ns,
        },
    );
    fbb.finish(req, None);
    fbb.finished_data().to_vec()
}

/// 构造 `SetEnabledResponse` body bytes（S0 → gateway `set_enabled` RR reply）。
/// `domain` = 细粒度 [`SetEnabledStatus`]；`status` = wire [`RpcStatus`]。
#[must_use]
pub fn build_set_enabled_response(
    request_id: &str,
    status: RpcStatus,
    domain: SetEnabledStatus,
    completed_at_ns: u64,
    error_msg: &str,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let request_id_off = fbb.create_string(request_id);
    let error_msg_off = fbb.create_string(error_msg);
    let resp = SetEnabledResponse::create(
        &mut fbb,
        &SetEnabledResponseArgs {
            request_id: Some(request_id_off),
            status,
            domain,
            completed_at_ns,
            error_msg: Some(error_msg_off),
            effect: SideEffect::Unknown,
            retired: false,
        },
    );
    fbb.finish(resp, None);
    fbb.finished_data().to_vec()
}

/// 构造带事务元数据的控制响应；只编码调用方提供的实际结果。
#[must_use]
pub fn build_set_enabled_response_typed(
    request_id: &str,
    status: RpcStatus,
    domain: SetEnabledStatus,
    completed_at_ns: u64,
    error_msg: &str,
    effect: SideEffect,
    retired: bool,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let request_id_off = fbb.create_string(request_id);
    let error_msg_off = fbb.create_string(error_msg);
    let resp = SetEnabledResponse::create(
        &mut fbb,
        &SetEnabledResponseArgs {
            request_id: Some(request_id_off),
            status,
            domain,
            completed_at_ns,
            error_msg: Some(error_msg_off),
            effect,
            retired,
        },
    );
    fbb.finish(resp, None);
    fbb.finished_data().to_vec()
}

// ============================================================================
// v0.3 B4 task control plane RR response builders
// ============================================================================
//
// 4 个 RR topic 一对 request/response：
//   - `task_invoke`  → `InvokeTaskResponse`
//   - `task_status`  → `GetTaskStatusResponse`
//   - `task_cancel`  → `CancelTaskResponse`
//   - `clear_error`  → `ClearErrorResponse`
//
// 字段语义详 `crates/robot-msg/schemas/action_status.fbs` +
// `action_request.fbs`。本节 builder 对调用方暴露**全字段**入参；skeleton
// 路径（B4 (1/3-b) `crates/robot-s1/src/rr_server_thread.rs`）传 status =
// `ActionRpcStatus::Unimplemented` + 其它字段空值；真 handler 路径（B4
// (2/3) `crates/robot-rpc/src/...`）按 `Coordinator::*` 返回值填实际字段。
//
// **`outcome` / `failure` Option 语义**：fbs 没有 `optional` 关键字；
// builder 用 `Option<(kind, text)>` 入参映射到 fbs sub-table：
//   - `None` → fbs vtable 不写该字段；reader `Option<TableType<'_>>` 返
//     `None`。
//   - `Some((Unknown, ""))` → 写空 sub-table（kind=Unknown / text 空串），
//     reader 拿到 `Some(...)` 但 kind=Unknown。**本 builder 把
//     `Some((Unknown, ""))` 当退化态语义跟 `None` 等价；调用方建议用
//     `None`。**

/// 构造 `InvokeTaskRequest` body bytes（gateway → s1 RR call body）。
#[must_use]
#[allow(clippy::too_many_arguments)] // wire schema 字段集即如此；不再裹一层 struct 增加间接。
pub fn build_invoke_task_request(
    request_id: &str,
    idempotency_key: &str,
    source: &str,
    role: RpcRole,
    audit_reason: &str,
    kind: ActionKindFb,
    deadline_ns: u64,
    priority: u32,
    payload_schema_id: u32,
    payload: &[u8],
    stamp_ns: u64,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(192);
    let request_id_off = fbb.create_string(request_id);
    let idempotency_key_off = fbb.create_string(idempotency_key);
    let source_off = fbb.create_string(source);
    let audit_reason_off = fbb.create_string(audit_reason);
    let payload_off = fbb.create_vector(payload);
    let req = InvokeTaskRequest::create(
        &mut fbb,
        &InvokeTaskRequestArgs {
            request_id: Some(request_id_off),
            idempotency_key: Some(idempotency_key_off),
            source: Some(source_off),
            role,
            audit_reason: Some(audit_reason_off),
            kind,
            deadline_ns,
            priority,
            payload_schema_id,
            payload: Some(payload_off),
            stamp_ns,
        },
    );
    fbb.finish(req, None);
    fbb.finished_data().to_vec()
}

/// 构造 `GetTaskStatusRequest` body bytes（gateway → s1 RR call body）。
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_get_task_status_request(
    request_id: &str,
    idempotency_key: &str,
    source: &str,
    role: RpcRole,
    audit_reason: &str,
    action_id: &str,
    deadline_ns: u64,
    stamp_ns: u64,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(160);
    let request_id_off = fbb.create_string(request_id);
    let idempotency_key_off = fbb.create_string(idempotency_key);
    let source_off = fbb.create_string(source);
    let audit_reason_off = fbb.create_string(audit_reason);
    let action_id_off = fbb.create_string(action_id);
    let req = GetTaskStatusRequest::create(
        &mut fbb,
        &GetTaskStatusRequestArgs {
            request_id: Some(request_id_off),
            idempotency_key: Some(idempotency_key_off),
            source: Some(source_off),
            role,
            audit_reason: Some(audit_reason_off),
            action_id: Some(action_id_off),
            deadline_ns,
            stamp_ns,
        },
    );
    fbb.finish(req, None);
    fbb.finished_data().to_vec()
}

/// 构造 `CancelTaskRequest` body bytes（gateway → s1 RR call body）。
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_cancel_task_request(
    request_id: &str,
    idempotency_key: &str,
    source: &str,
    role: RpcRole,
    audit_reason: &str,
    action_id: &str,
    deadline_ns: u64,
    stamp_ns: u64,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(160);
    let request_id_off = fbb.create_string(request_id);
    let idempotency_key_off = fbb.create_string(idempotency_key);
    let source_off = fbb.create_string(source);
    let audit_reason_off = fbb.create_string(audit_reason);
    let action_id_off = fbb.create_string(action_id);
    let req = CancelTaskRequest::create(
        &mut fbb,
        &CancelTaskRequestArgs {
            request_id: Some(request_id_off),
            idempotency_key: Some(idempotency_key_off),
            source: Some(source_off),
            role,
            audit_reason: Some(audit_reason_off),
            action_id: Some(action_id_off),
            deadline_ns,
            stamp_ns,
        },
    );
    fbb.finish(req, None);
    fbb.finished_data().to_vec()
}

/// 构造 `ClearErrorRequest` body bytes（gateway → s1 RR call body）。
#[must_use]
pub fn build_clear_error_request(
    request_id: &str,
    idempotency_key: &str,
    source: &str,
    role: RpcRole,
    audit_reason: &str,
    deadline_ns: u64,
    stamp_ns: u64,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(160);
    let request_id_off = fbb.create_string(request_id);
    let idempotency_key_off = fbb.create_string(idempotency_key);
    let source_off = fbb.create_string(source);
    let audit_reason_off = fbb.create_string(audit_reason);
    let req = ClearErrorRequest::create(
        &mut fbb,
        &ClearErrorRequestArgs {
            request_id: Some(request_id_off),
            idempotency_key: Some(idempotency_key_off),
            source: Some(source_off),
            role,
            audit_reason: Some(audit_reason_off),
            deadline_ns,
            stamp_ns,
        },
    );
    fbb.finish(req, None);
    fbb.finished_data().to_vec()
}

/// 构造 `InvokeTaskResponse` body bytes。
///
/// 字段语义详 [`crate::markers::InvokeTaskResponse`] +
/// `crates/robot-msg/schemas/action_request.fbs`。
#[must_use]
pub fn build_invoke_task_response(
    request_id: &str,
    status: ActionRpcStatus,
    error_msg: &str,
    action_id: &str,
    accepted_at_ns: u64,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let request_id_off = fbb.create_string(request_id);
    let error_msg_off = fbb.create_string(error_msg);
    let action_id_off = fbb.create_string(action_id);
    let resp = InvokeTaskResponse::create(
        &mut fbb,
        &InvokeTaskResponseArgs {
            request_id: Some(request_id_off),
            status,
            error_msg: Some(error_msg_off),
            action_id: Some(action_id_off),
            accepted_at_ns,
            effect: SideEffect::Unknown,
            retired: false,
            invoke_outcome: InvokeOutcome::Unknown,
        },
    );
    fbb.finish(resp, None);
    fbb.finished_data().to_vec()
}

/// 构造带事务元数据的控制响应；只编码调用方提供的实际结果。
#[must_use]
#[allow(
    clippy::too_many_arguments,
    reason = "保持已批准的平坦构造接口，参数与完整协议字段逐项对应"
)]
pub fn build_invoke_task_response_typed(
    request_id: &str,
    status: ActionRpcStatus,
    error_msg: &str,
    action_id: &str,
    accepted_at_ns: u64,
    effect: SideEffect,
    retired: bool,
    invoke_outcome: InvokeOutcome,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let request_id_off = fbb.create_string(request_id);
    let error_msg_off = fbb.create_string(error_msg);
    let action_id_off = fbb.create_string(action_id);
    let resp = InvokeTaskResponse::create(
        &mut fbb,
        &InvokeTaskResponseArgs {
            request_id: Some(request_id_off),
            status,
            error_msg: Some(error_msg_off),
            action_id: Some(action_id_off),
            accepted_at_ns,
            effect,
            retired,
            invoke_outcome,
        },
    );
    fbb.finish(resp, None);
    fbb.finished_data().to_vec()
}

/// 构造 `GetTaskStatusResponse` body bytes。
///
/// `outcome` / `failure` 用 `Option<(kind, text)>` —— `None` 不写 sub-table；
/// `Some((kind, reason_or_warning))` 写完整 sub-table。详
/// [`crate::markers::GetTaskStatusResponse`] +
/// `crates/robot-msg/schemas/action_status.fbs`。
#[must_use]
#[allow(clippy::too_many_arguments)] // wire schema 字段集即如此；不再裹一层 struct 增加间接。
pub fn build_get_task_status_response(
    request_id: &str,
    status: ActionRpcStatus,
    error_msg: &str,
    action_id: &str,
    state: TaskStateFb,
    progress: f32,
    outcome: Option<(ActionOutcomeKindFb, &str)>,
    failure: Option<(ActionFailureKindFb, &str)>,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(192);
    let request_id_off = fbb.create_string(request_id);
    let error_msg_off = fbb.create_string(error_msg);
    let action_id_off = fbb.create_string(action_id);
    let outcome_off = outcome.map(|(kind, warning)| {
        let warning_off = fbb.create_string(warning);
        ActionOutcomeFb::create(
            &mut fbb,
            &ActionOutcomeFbArgs {
                kind,
                warning: Some(warning_off),
            },
        )
    });
    let failure_off = failure.map(|(kind, reason)| {
        let reason_off = fbb.create_string(reason);
        ActionFailureFb::create(
            &mut fbb,
            &ActionFailureFbArgs {
                kind,
                reason: Some(reason_off),
            },
        )
    });
    let resp = GetTaskStatusResponse::create(
        &mut fbb,
        &GetTaskStatusResponseArgs {
            request_id: Some(request_id_off),
            status,
            error_msg: Some(error_msg_off),
            action_id: Some(action_id_off),
            state,
            progress,
            outcome: outcome_off,
            failure: failure_off,
        },
    );
    fbb.finish(resp, None);
    fbb.finished_data().to_vec()
}

/// 构造 `CancelTaskResponse` body bytes。
#[must_use]
pub fn build_cancel_task_response(
    request_id: &str,
    status: ActionRpcStatus,
    error_msg: &str,
    action_id: &str,
    cancel_at_ns: u64,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let request_id_off = fbb.create_string(request_id);
    let error_msg_off = fbb.create_string(error_msg);
    let action_id_off = fbb.create_string(action_id);
    let resp = CancelTaskResponse::create(
        &mut fbb,
        &CancelTaskResponseArgs {
            request_id: Some(request_id_off),
            status,
            error_msg: Some(error_msg_off),
            action_id: Some(action_id_off),
            cancel_at_ns,
            effect: SideEffect::Unknown,
            retired: false,
        },
    );
    fbb.finish(resp, None);
    fbb.finished_data().to_vec()
}

/// 构造带事务元数据的控制响应；只编码调用方提供的实际结果。
#[must_use]
pub fn build_cancel_task_response_typed(
    request_id: &str,
    status: ActionRpcStatus,
    error_msg: &str,
    action_id: &str,
    cancel_at_ns: u64,
    effect: SideEffect,
    retired: bool,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let request_id_off = fbb.create_string(request_id);
    let error_msg_off = fbb.create_string(error_msg);
    let action_id_off = fbb.create_string(action_id);
    let resp = CancelTaskResponse::create(
        &mut fbb,
        &CancelTaskResponseArgs {
            request_id: Some(request_id_off),
            status,
            error_msg: Some(error_msg_off),
            action_id: Some(action_id_off),
            cancel_at_ns,
            effect,
            retired,
        },
    );
    fbb.finish(resp, None);
    fbb.finished_data().to_vec()
}

/// 构造 `ActionEventEnvelope` body bytes（per-ActionId pubsub 单帧 payload）。
///
/// 字段语义详 [`crate::markers::ActionEventEnvelope`] +
/// `crates/robot-msg/schemas/action_event.fbs`：
///
/// - `action_id_hex`：`UUIDv7` canonical hex string（`robot_s1::action::ActionId`
///   的 `Display` 形式）；冗余写到 envelope，gateway 端 audit / debug **不**
///   依赖 topic name 解析。
/// - `at_ns`：事件时刻（`CLOCK_MONOTONIC_RAW`；s1 端 `clock.now()`）。
/// - `kind`：事件类型 tag（5 个 variant：Started / Progress / Completed /
///   Failed / Cancelled）；`Unknown` 兼容旧 writer。
/// - `progress`：仅 `kind == Progress` 有意义；其它 kind 留 `0.0`。
/// - `outcome`：`Some((kind, warning))` 仅 `kind == Completed` 时填；其它
///   kind 传 `None`（fbs vtable 不写该字段）。`Success` 时 warning 留空字
///   符串，`PartialSuccess` 时 warning 必须非空。
/// - `failure`：`Some((kind, reason))` 仅 `kind == Failed` 时填；其它 kind
///   传 `None`。`DeadlineExceeded` 时 reason 留空，其它 4 个 variant 都带
///   具体 reason。
///
/// 跟 `build_get_task_status_response` 的 `outcome` / `failure` 入参形态
/// 一致。
#[must_use]
pub fn build_action_event_envelope(
    action_id_hex: &str,
    at_ns: u64,
    kind: ActionEventKindFb,
    progress: f32,
    outcome: Option<(ActionOutcomeKindFb, &str)>,
    failure: Option<(ActionFailureKindFb, &str)>,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(160);
    let action_id_off = fbb.create_string(action_id_hex);
    let outcome_off = outcome.map(|(k, warning)| {
        let warning_off = fbb.create_string(warning);
        ActionOutcomeFb::create(
            &mut fbb,
            &ActionOutcomeFbArgs {
                kind: k,
                warning: Some(warning_off),
            },
        )
    });
    let failure_off = failure.map(|(k, reason)| {
        let reason_off = fbb.create_string(reason);
        ActionFailureFb::create(
            &mut fbb,
            &ActionFailureFbArgs {
                kind: k,
                reason: Some(reason_off),
            },
        )
    });
    let env = ActionEventEnvelope::create(
        &mut fbb,
        &ActionEventEnvelopeArgs {
            action_id: Some(action_id_off),
            at_ns,
            kind,
            progress,
            outcome: outcome_off,
            failure: failure_off,
        },
    );
    fbb.finish(env, None);
    fbb.finished_data().to_vec()
}

/// 构造 `ClearErrorResponse` body bytes。
///
/// **跟 `ClearEStopResponse` 不同**：`ClearError` 仅清 coordinator
/// `Degraded` 内存状态（B2 outline §10 决策 #5），**不**碰磁盘 `EStop`
/// latch；本 builder 跟 `build_clear_estop_response` 字段集刻意分开。
#[must_use]
pub fn build_clear_error_response(
    request_id: &str,
    status: ActionRpcStatus,
    error_msg: &str,
    cleared_at_ns: u64,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let request_id_off = fbb.create_string(request_id);
    let error_msg_off = fbb.create_string(error_msg);
    let resp = ClearErrorResponse::create(
        &mut fbb,
        &ClearErrorResponseArgs {
            request_id: Some(request_id_off),
            status,
            error_msg: Some(error_msg_off),
            cleared_at_ns,
            effect: SideEffect::Unknown,
            retired: false,
        },
    );
    fbb.finish(resp, None);
    fbb.finished_data().to_vec()
}

/// 构造带事务元数据的控制响应；只编码调用方提供的实际结果。
#[must_use]
pub fn build_clear_error_response_typed(
    request_id: &str,
    status: ActionRpcStatus,
    error_msg: &str,
    cleared_at_ns: u64,
    effect: SideEffect,
    retired: bool,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let request_id_off = fbb.create_string(request_id);
    let error_msg_off = fbb.create_string(error_msg);
    let resp = ClearErrorResponse::create(
        &mut fbb,
        &ClearErrorResponseArgs {
            request_id: Some(request_id_off),
            status,
            error_msg: Some(error_msg_off),
            cleared_at_ns,
            effect,
            retired,
        },
    );
    fbb.finish(resp, None);
    fbb.finished_data().to_vec()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use flatbuffers::root;

    #[test]
    fn joint_state_round_trip() {
        let bytes = build_joint_state(
            0xDEAD_BEEF_0000_0001,
            1_234_567_890,
            &[0.1, 0.2, 0.3],
            &[0.01, 0.02, 0.03],
            &[0.001, 0.002, 0.003],
            "world",
        );
        let js = root::<JointState<'_>>(&bytes).unwrap();
        assert_eq!(js.layout_id(), 0xDEAD_BEEF_0000_0001);
        assert_eq!(js.stamp_ns(), 1_234_567_890);
        let pos: Vec<f64> = js.position_rad().unwrap().iter().collect();
        assert_eq!(pos, vec![0.1, 0.2, 0.3]);
        let vel: Vec<f64> = js.velocity_rps().unwrap().iter().collect();
        assert_eq!(vel, vec![0.01, 0.02, 0.03]);
        let eff: Vec<f64> = js.effort_nm().unwrap().iter().collect();
        assert_eq!(eff, vec![0.001, 0.002, 0.003]);
        assert_eq!(js.frame_id(), Some("world"));
    }

    #[test]
    fn joint_state_omits_empty_vectors_and_frame() {
        let bytes = build_joint_state(0xCAFE, 0, &[1.0], &[], &[], "");
        let js = root::<JointState<'_>>(&bytes).unwrap();
        assert!(js.velocity_rps().is_none());
        assert!(js.effort_nm().is_none());
        assert!(js.frame_id().is_none());
    }

    #[test]
    fn joint_command_round_trip() {
        let bytes = build_joint_command(
            0xCAFE_0001,
            ControlMode::Position,
            &[0.5, -0.5],
            999_999_999,
            42,
            StaleAction::Hold,
            7,
        );
        let cmd = root::<JointCommand<'_>>(&bytes).unwrap();
        assert_eq!(cmd.layout_id(), 0xCAFE_0001);
        assert_eq!(cmd.mode(), ControlMode::Position);
        let tgt: Vec<f64> = cmd.target().unwrap().iter().collect();
        assert_eq!(tgt, vec![0.5, -0.5]);
        assert_eq!(cmd.deadline_ns(), 999_999_999);
        assert_eq!(cmd.priority(), 42);
        assert_eq!(cmd.source_task_id(), 7);
        assert_eq!(cmd.on_timeout(), StaleAction::Hold);
    }

    #[test]
    fn heartbeat_round_trip() {
        let bytes = build_heartbeat(2, 1_234, 50.0, 111_222_333);
        let hb = root::<Heartbeat<'_>>(&bytes).unwrap();
        assert_eq!(hb.source_id(), 2);
        assert_eq!(hb.seq(), 1_234);
        assert!((hb.tick_rate_hz() - 50.0).abs() < 1e-9);
        assert_eq!(hb.stamp_ns(), 111_222_333);
    }

    #[test]
    fn estop_event_round_trip() {
        let bytes = build_estop_event("physical button", 555, "button", true);
        let ev = root::<EStopEvent<'_>>(&bytes).unwrap();
        assert_eq!(ev.reason(), Some("physical button"));
        assert_eq!(ev.stamp_ns(), 555);
        assert_eq!(ev.source(), Some("button"));
        assert!(ev.latched());
    }

    #[test]
    fn clear_estop_request_round_trip() {
        let bytes = build_clear_estop_request(
            "req-uuidv7-aaaa",
            "idem-bbbb",
            "cli.loosqk",
            RpcRole::Maintainer,
            "operator-loosqk-confirm-001",
            "manual recovery after EStop button press",
            /* dry_run */ false,
            /* deadline_ns */ 1_234_567_890_000,
            /* stamp_ns */ 1_234_567_000_000,
        );
        let req = root::<ClearEStopRequest>(&bytes).unwrap();
        assert_eq!(req.request_id().unwrap(), "req-uuidv7-aaaa");
        assert_eq!(req.idempotency_key().unwrap(), "idem-bbbb");
        assert_eq!(req.source().unwrap(), "cli.loosqk");
        assert_eq!(req.role(), RpcRole::Maintainer);
        assert_eq!(req.confirmation().unwrap(), "operator-loosqk-confirm-001");
        assert_eq!(
            req.audit_reason().unwrap(),
            "manual recovery after EStop button press"
        );
        assert!(!req.dry_run());
        assert_eq!(req.deadline_ns(), 1_234_567_890_000);
        assert_eq!(req.stamp_ns(), 1_234_567_000_000);
    }

    #[test]
    fn clear_estop_request_dry_run_default_role_unknown() {
        // 验证 dry_run = true 路径 + role 字段不传时默认 Unknown。
        let bytes = build_clear_estop_request(
            "req-2",
            "idem-2",
            "smoke-test",
            RpcRole::Unknown,
            "smoke-confirm",
            "test only",
            /* dry_run */ true,
            0,
            0,
        );
        let req = root::<ClearEStopRequest>(&bytes).unwrap();
        assert!(req.dry_run());
        assert_eq!(req.role(), RpcRole::Unknown);
    }

    #[test]
    fn clear_estop_response_round_trip_ok() {
        let bytes = build_clear_estop_response(
            "req-uuidv7-aaaa",
            RpcStatus::Ok,
            "",
            /* cleared_at_ns */ 1_234_567_890_001,
            /* dry_run */ false,
        );
        let resp = root::<ClearEStopResponse>(&bytes).unwrap();
        assert_eq!(resp.request_id().unwrap(), "req-uuidv7-aaaa");
        assert_eq!(resp.status(), RpcStatus::Ok);
        assert_eq!(resp.error_msg().unwrap(), "");
        assert_eq!(resp.cleared_at_ns(), 1_234_567_890_001);
        assert!(!resp.dry_run());
    }

    #[test]
    fn clear_estop_response_round_trip_failed_precondition() {
        // safety_state != TorqueOffConfirmed 时 daemon 端发的 reply：
        // status = FailedPrecondition / cleared_at_ns = 0 / dry_run 回传 false。
        let bytes = build_clear_estop_response(
            "req-3",
            RpcStatus::FailedPrecondition,
            "safety_state must be TorqueOffConfirmed before ClearEStop",
            0,
            false,
        );
        let resp = root::<ClearEStopResponse>(&bytes).unwrap();
        assert_eq!(resp.status(), RpcStatus::FailedPrecondition);
        assert_eq!(resp.cleared_at_ns(), 0);
        assert!(resp.error_msg().unwrap().contains("TorqueOffConfirmed"));
    }

    // ---- v0.5 候选 C SetEnabled ----

    #[test]
    fn set_enabled_request_round_trip() {
        let bytes = build_set_enabled_request(
            "req-se-aaaa",
            "idem-se-bbbb",
            "cli.loosqk",
            RpcRole::Operator,
            /* enabled */ true,
            /* dry_run */ false,
            /* deadline_ns */ 2_000_000_000_000,
            "operator enable robot",
            /* stamp_ns */ 1_999_000_000_000,
        );
        let req = root::<SetEnabledRequest>(&bytes).unwrap();
        assert_eq!(req.request_id().unwrap(), "req-se-aaaa");
        assert_eq!(req.idempotency_key().unwrap(), "idem-se-bbbb");
        assert_eq!(req.source().unwrap(), "cli.loosqk");
        assert_eq!(req.role(), RpcRole::Operator);
        assert!(req.enabled());
        assert!(!req.dry_run());
        assert_eq!(req.deadline_ns(), 2_000_000_000_000);
        assert_eq!(req.audit_reason().unwrap(), "operator enable robot");
        assert_eq!(req.stamp_ns(), 1_999_000_000_000);
    }

    #[test]
    fn set_enabled_request_disable_dry_run_default_role_unknown() {
        let bytes = build_set_enabled_request(
            "req-se-2",
            "idem-se-2",
            "smoke",
            RpcRole::Unknown,
            /* enabled */ false,
            /* dry_run */ true,
            0,
            "test only",
            0,
        );
        let req = root::<SetEnabledRequest>(&bytes).unwrap();
        assert!(!req.enabled());
        assert!(req.dry_run());
        assert_eq!(req.role(), RpcRole::Unknown);
    }

    #[test]
    fn set_enabled_response_round_trip_ok() {
        let bytes = build_set_enabled_response(
            "req-se-aaaa",
            RpcStatus::Ok,
            SetEnabledStatus::Ok,
            /* completed_at_ns */ 2_000_000_000_123,
            "",
        );
        let resp = root::<SetEnabledResponse>(&bytes).unwrap();
        assert_eq!(resp.request_id().unwrap(), "req-se-aaaa");
        assert_eq!(resp.status(), RpcStatus::Ok);
        assert_eq!(resp.domain(), SetEnabledStatus::Ok);
        assert_eq!(resp.completed_at_ns(), 2_000_000_000_123);
        assert_eq!(resp.error_msg().unwrap(), "");
    }

    #[test]
    fn set_enabled_response_round_trip_adapter_error() {
        let bytes = build_set_enabled_response(
            "req-se-3",
            RpcStatus::Internal,
            SetEnabledStatus::AdapterError,
            0,
            "adapter Enable(true) ack failed",
        );
        let resp = root::<SetEnabledResponse>(&bytes).unwrap();
        assert_eq!(resp.status(), RpcStatus::Internal);
        assert_eq!(resp.domain(), SetEnabledStatus::AdapterError);
        assert_eq!(resp.completed_at_ns(), 0);
        assert!(resp.error_msg().unwrap().contains("ack failed"));
    }
}
