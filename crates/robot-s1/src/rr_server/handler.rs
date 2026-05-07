//! `rr_server` 真 handler 层 —— 4 个 RR topic 的业务逻辑：调
//! [`crate::coordinator::Coordinator`] in-process API + spawn
//! `events_drain_thread` + register 入 [`EventsDrainRegistry`] + 编 fbs
//! response bytes。
//!
//! ## 角色
//!
//! handler 闭包 `Fn(&[u8]) -> Vec<u8> + Send + 'static` 从 wire payload bytes
//! 起开始算（runtime 已经做过 `flatbuffers::root` verifier check；此后 decode
//! 不会失败）：
//!
//! 1. 调 [`super::wire::decode_*_request`] 拿 owned struct。
//! 2. 字段 normalize（`ActionKindFb` → [`ActionKind`]、UUIDv7 hex →
//!    [`ActionId`]、`u32` → `u8` priority cast 等）。任一非法 → 编
//!    `ActionRpcStatus::InvalidArgument` response。
//! 3. 调 `Coordinator::{submit, cancel, status, clear_error}` API。
//! 4. （`InvokeTask` 路径）`submit` 成功 → 起 `events_drain_thread` →
//!    `EventsDrainRegistry::register`。
//! 5. 错误（`SubmitError` / `ActionCancelError` / `ClearError`）→
//!    [`super::wire::*_err_to_rpc_status`] 映射 →
//!    `encode_*_response_err`。
//! 6. 成功 → `encode_*_response_ok`。
//!
//! ## shutdown 边界（reviewer 拍板）
//!
//! handler 内调 `Coordinator::submit` 时 coordinator 已 `mark_shutting_down`
//! → `submit` 返 [`SubmitError::ShuttingDown`] →
//! [`super::wire::submit_err_to_rpc_status`] 映射 `ActionRpcStatus::Unavailable`
//! → handler 编 error response 并直接返，**不**调 `EventsDrainRegistry::register`。
//! `s1_daemon` shutdown 顺序锁住：`mark_shutting_down → rr.request_stop_all
//! → rr.join_all → events_registry.stop_and_join_all`，保证 RR thread join
//! 完成后才停 events_registry，避免 in-flight invoke 跟 registry stop 撞上
//! late register 路径。详 [`crate::rr_server`] 顶部 module doc。
//!
//! ## 不在范围
//!
//! - **不**做 idempotency cache（gateway 端做，详 outline §8.1 #7b）。
//! - **不**做 auth / capability 校验（gateway 端做）。
//! - **不**起 tonic / async runtime（B2 outline §10 决策 #10 +
//!   `robot-s1` 顶部 module doc 锁定不 link tokio / tonic）。
//!
//! ## handler 构造模式
//!
//! [`RealHandlerCtx`] 持运行时 handle (`Arc<Coordinator>` /
//! `Arc<EventsDrainRegistry>` / `Arc<dyn RobotClock>` / `source_id` /
//! `robot_id`)。`spawn_*_handler` fn 返 `Fn(&[u8]) -> Vec<u8> + Send + 'static`
//! closure 给 [`super::runtime::spawn_*_server`]，闭包 capture
//! `Arc<RealHandlerCtx>`。

#![allow(clippy::doc_markdown)]

use std::sync::Arc;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::RobotClock;
use robot_msg::markers;
use robot_msg::msg::{ActionKindFb, ActionRpcStatus};
use robot_transport::iceoryx2::publisher as open_pub;
use robot_transport::iceoryx2::publisher::Iceoryx2Publisher;
use robot_transport::iceoryx2::qos::TopicClass;

use crate::action::{ActionId, ActionKind, ActionRequest, ActionSource};
use crate::coordinator::Coordinator;
use crate::events_drain_thread::{
    EventsDrainRegistry, EventsDrainThreadConfig, spawn_events_drain_thread,
};

use super::wire::{
    cancel_err_to_rpc_status, clear_err_to_rpc_status, decode_cancel_request,
    decode_clear_error_request, decode_invoke_request, decode_status_request,
    encode_cancel_response_err, encode_cancel_response_ok, encode_clear_error_response_err,
    encode_clear_error_response_ok, encode_invoke_response_err, encode_invoke_response_ok,
    encode_status_response_err, encode_status_response_ok, submit_err_to_rpc_status,
};

// ============================================================================
// RealHandlerCtx
// ============================================================================

/// 4 个 RR handler 共用的运行时 context。binary 端构造一份，4 个
/// `spawn_*_handler` 闭包 capture `Arc<RealHandlerCtx>` 进各自 RR server
/// 线程。
///
/// **不**含 stop signal：handler 线程的退出靠
/// [`super::runtime::RrServerThreadHandle::request_stop`] 控制；handler 闭包
/// 本身不感知 shutdown。`Coordinator::submit` 在 `mark_shutting_down` 后返
/// [`crate::action::SubmitError::ShuttingDown`]，handler 把它映射为
/// `ActionRpcStatus::Unavailable` reply 即可。
pub struct RealHandlerCtx {
    /// `Coordinator` in-process API 入口（`submit` / `cancel` / `status` /
    /// `clear_error`）。
    pub coordinator: Arc<Coordinator>,
    /// invoke handler 起 `events_drain_thread` 后 register 进表；shutdown
    /// 路径由 binary 端 `events_registry.stop_and_join_all()` 一并关停。
    pub events_registry: Arc<EventsDrainRegistry>,
    /// `events_drain_thread` 用 `clock.now()` 写 `FrameHeader.stamp_ns`；
    /// invoke 路径的 `accepted_at_ns` / cancel 路径的 `cancel_at_ns` /
    /// clear_error 路径的 `cleared_at_ns` 也都用本 clock 取。
    pub clock: Arc<dyn RobotClock>,
    /// `events_drain_thread` 写入 envelope `FrameHeader.source_id`。s1_daemon
    /// 一律传 `2`（与 v0.2 `JointCommand` / `Heartbeat` 约定一致）。
    pub source_id: u32,
    /// 用于构造 events drain pubsub topic
    /// `/<robot_id>/s1/action_events/<action_id>`（per-ActionId 动态创建）。
    pub robot_id: String,
}

impl RealHandlerCtx {
    /// 构造 invoke handler closure（用于
    /// [`super::runtime::spawn_task_invoke_server`]）。
    pub fn invoke_handler(self: &Arc<Self>) -> impl Fn(&[u8]) -> Vec<u8> + Send + 'static {
        let ctx = Arc::clone(self);
        move |payload: &[u8]| -> Vec<u8> { handle_invoke(payload, &ctx) }
    }

    /// 构造 cancel handler closure（用于
    /// [`super::runtime::spawn_task_cancel_server`]）。
    pub fn cancel_handler(self: &Arc<Self>) -> impl Fn(&[u8]) -> Vec<u8> + Send + 'static {
        let ctx = Arc::clone(self);
        move |payload: &[u8]| -> Vec<u8> { handle_cancel(payload, &ctx) }
    }

    /// 构造 status handler closure（用于
    /// [`super::runtime::spawn_task_status_server`]）。
    pub fn status_handler(self: &Arc<Self>) -> impl Fn(&[u8]) -> Vec<u8> + Send + 'static {
        let ctx = Arc::clone(self);
        move |payload: &[u8]| -> Vec<u8> { handle_status(payload, &ctx) }
    }

    /// 构造 clear_error handler closure（用于
    /// [`super::runtime::spawn_clear_error_server`]）。
    pub fn clear_error_handler(self: &Arc<Self>) -> impl Fn(&[u8]) -> Vec<u8> + Send + 'static {
        let ctx = Arc::clone(self);
        move |payload: &[u8]| -> Vec<u8> { handle_clear_error(payload, &ctx) }
    }
}

// ============================================================================
// 4 个 handle_* 业务逻辑
// ============================================================================

/// `task_invoke` handler 主路径。
#[must_use]
#[allow(clippy::too_many_lines)] // 一条 happy path + 6 条 InvalidArgument early-return + Submit 错误映射；分成 helper 反而增加跨函数跳读成本。
pub fn handle_invoke(payload: &[u8], ctx: &RealHandlerCtx) -> Vec<u8> {
    // 1. fbs decode（runtime 已 verify；这里再调 owned 化字段）。
    let decoded = match decode_invoke_request(payload) {
        Ok(d) => d,
        Err(e) => {
            return encode_invoke_response_err(
                "",
                "",
                ActionRpcStatus::InvalidArgument,
                &format!("InvokeTaskRequest decode failed: {e}"),
            );
        }
    };

    // 2. request_id → ActionId（design §5.2 锁定 request_id 直接当 ActionId）。
    let action_id = match parse_action_id(&decoded.request_id) {
        Ok(id) => id,
        Err(e) => {
            return encode_invoke_response_err(
                &decoded.request_id,
                "",
                ActionRpcStatus::InvalidArgument,
                &format!("request_id is not a valid UUID: {e}"),
            );
        }
    };

    // 3. ActionKindFb → ActionKind（B2/B3 仅 2 个 variant）。
    let kind = match decoded.kind {
        ActionKindFb::DwellMs => ActionKind::DwellMs,
        ActionKindFb::HoldPose => ActionKind::HoldPose,
        ActionKindFb::Unknown => {
            return encode_invoke_response_err(
                &decoded.request_id,
                &decoded.request_id,
                ActionRpcStatus::InvalidArgument,
                "InvokeTaskRequest.kind is Unknown (not set by client)",
            );
        }
        other => {
            return encode_invoke_response_err(
                &decoded.request_id,
                &decoded.request_id,
                ActionRpcStatus::InvalidArgument,
                &format!("unrecognized ActionKindFb variant: {other:?}"),
            );
        }
    };

    // 4. priority u32 → u8（fbs schema u32，lib u8，溢出拒 InvalidArgument
    //    跟 schema 字段注释对齐）。
    let Ok(priority) = u8::try_from(decoded.priority) else {
        return encode_invoke_response_err(
            &decoded.request_id,
            &decoded.request_id,
            ActionRpcStatus::InvalidArgument,
            &format!(
                "priority {} exceeds u8::MAX (lib ActionRequest.priority is u8)",
                decoded.priority
            ),
        );
    };

    // 5. 构造 ActionRequest。
    let req = ActionRequest {
        action_id,
        kind,
        deadline_ns: decoded.deadline_ns,
        priority,
        payload_schema_id: decoded.payload_schema_id,
        payload: decoded.payload,
        idempotency_key: decoded.idempotency_key,
        source: ActionSource::RpcGateway {
            actor: decoded.source,
        },
    };

    // 6. 调 Coordinator::submit。
    let clock_ref: &dyn RobotClock = ctx.clock.as_ref();
    match ctx.coordinator.submit(req, clock_ref) {
        Ok(ticket) => {
            let action_id = ticket.action_id;
            let accepted_at_ns = clock_ref.now().0;

            // 7. spawn events_drain_thread + register。
            let cfg = EventsDrainThreadConfig::for_action(&ctx.robot_id, action_id);
            let factory = make_events_publisher_factory(cfg.topic.clone());
            match spawn_events_drain_thread(
                cfg,
                ticket.events,
                factory,
                Arc::clone(&ctx.clock),
                ctx.source_id,
            ) {
                Ok(handle) => {
                    ctx.events_registry.register(handle);
                    let action_id_hex = action_id.to_string();
                    encode_invoke_response_ok(&decoded.request_id, &action_id_hex, accepted_at_ns)
                }
                Err(e) => {
                    // events_drain spawn 失败：coordinator 已转 Executing，
                    // 调 cancel 让它走 cooperative cancel 路径回 IDLE，避免
                    // 客户端拿到 Internal 但 action 仍在跑的歧义。cancel 失
                    // 败也吞掉（极少；event 已丢失，state 至多悬挂一阵后被
                    // backend tick 收敛）。
                    tracing::error!(
                        ?e,
                        action_id = %action_id,
                        "rr_server::handle_invoke: spawn_events_drain_thread failed; cancelling action"
                    );
                    let _ = ctx.coordinator.cancel(action_id, clock_ref);
                    encode_invoke_response_err(
                        &decoded.request_id,
                        &action_id.to_string(),
                        ActionRpcStatus::Internal,
                        &format!("spawn_events_drain_thread failed: {e}"),
                    )
                }
            }
        }
        Err(submit_err) => {
            let status = submit_err_to_rpc_status(&submit_err);
            encode_invoke_response_err(
                &decoded.request_id,
                // submit 失败时 action_id echo 仍带回（gateway audit 用），
                // 即使没真注册成功 —— 跟 outline §6.2 InvokeTaskResponse fbs
                // 注释 "status != Ok 时填空串" 不一致。reviewer 拍板：echo
                // request_id 形式即可（client 已知 request_id；schema 现状
                // 容许空 / 非空两种填法，handler 选 echo 让 audit 链路完整）。
                &decoded.request_id,
                status,
                &submit_err.to_string(),
            )
        }
    }
}

/// `task_cancel` handler 主路径。
#[must_use]
pub fn handle_cancel(payload: &[u8], ctx: &RealHandlerCtx) -> Vec<u8> {
    let decoded = match decode_cancel_request(payload) {
        Ok(d) => d,
        Err(e) => {
            return encode_cancel_response_err(
                "",
                "",
                ActionRpcStatus::InvalidArgument,
                &format!("CancelTaskRequest decode failed: {e}"),
            );
        }
    };

    let action_id = match parse_action_id(&decoded.action_id) {
        Ok(id) => id,
        Err(e) => {
            return encode_cancel_response_err(
                &decoded.request_id,
                &decoded.action_id,
                ActionRpcStatus::InvalidArgument,
                &format!("action_id is not a valid UUID: {e}"),
            );
        }
    };

    let clock_ref: &dyn RobotClock = ctx.clock.as_ref();
    match ctx.coordinator.cancel(action_id, clock_ref) {
        Ok(()) => {
            let cancel_at_ns = clock_ref.now().0;
            encode_cancel_response_ok(&decoded.request_id, &decoded.action_id, cancel_at_ns)
        }
        Err(cancel_err) => {
            let status = cancel_err_to_rpc_status(&cancel_err);
            encode_cancel_response_err(
                &decoded.request_id,
                &decoded.action_id,
                status,
                &cancel_err.to_string(),
            )
        }
    }
}

/// `task_status` handler 主路径。
#[must_use]
pub fn handle_status(payload: &[u8], ctx: &RealHandlerCtx) -> Vec<u8> {
    let decoded = match decode_status_request(payload) {
        Ok(d) => d,
        Err(e) => {
            return encode_status_response_err(
                "",
                "",
                ActionRpcStatus::InvalidArgument,
                &format!("GetTaskStatusRequest decode failed: {e}"),
            );
        }
    };

    let action_id = match parse_action_id(&decoded.action_id) {
        Ok(id) => id,
        Err(e) => {
            return encode_status_response_err(
                &decoded.request_id,
                &decoded.action_id,
                ActionRpcStatus::InvalidArgument,
                &format!("action_id is not a valid UUID: {e}"),
            );
        }
    };

    match ctx.coordinator.status(action_id) {
        Some(status) => encode_status_response_ok(&decoded.request_id, &decoded.action_id, &status),
        None => encode_status_response_err(
            &decoded.request_id,
            &decoded.action_id,
            ActionRpcStatus::NotFound,
            &format!(
                "action {action_id} not found (not currently executing and not in coordinator finished ring)"
            ),
        ),
    }
}

/// `clear_error` handler 主路径。
#[must_use]
pub fn handle_clear_error(payload: &[u8], ctx: &RealHandlerCtx) -> Vec<u8> {
    let decoded = match decode_clear_error_request(payload) {
        Ok(d) => d,
        Err(e) => {
            return encode_clear_error_response_err(
                "",
                ActionRpcStatus::InvalidArgument,
                &format!("ClearErrorRequest decode failed: {e}"),
            );
        }
    };

    match ctx.coordinator.clear_error() {
        Ok(()) => {
            let cleared_at_ns = ctx.clock.now().0;
            encode_clear_error_response_ok(&decoded.request_id, cleared_at_ns)
        }
        Err(clear_err) => {
            let status = clear_err_to_rpc_status(&clear_err);
            encode_clear_error_response_err(&decoded.request_id, status, &clear_err.to_string())
        }
    }
}

// ============================================================================
// 内部 helpers
// ============================================================================

/// UUIDv7 hex (canonical) → [`ActionId`]。接受任何合法 UUID 形式（不强制
/// version 7），跟 fbs schema "request_id 是 UUIDv7 hex" 文档约定的宽松
/// 解读对齐：gateway 负责生成 v7，daemon 这边只校验合法 UUID。
fn parse_action_id(s: &str) -> Result<ActionId, uuid::Error> {
    Ok(ActionId(uuid::Uuid::parse_str(s)?))
}

/// 构造 events_drain `publisher_factory` closure：每个 ActionId 一个独立
/// iceoryx2 Node + Publisher，Node leak 让 publisher 整个生命周期内 Node
/// 不被 drop（同 1/3-c events_drain_thread 测试 / 1/3-b s1_daemon rr_server
/// factory 同款 leak 模式）。
fn make_events_publisher_factory(
    topic: String,
) -> impl FnOnce() -> Iceoryx2Publisher<markers::ActionEventEnvelope> + Send + 'static {
    move || {
        let pub_node = NodeBuilder::new()
            .create::<IxService>()
            .expect("rr handle_invoke: events_drain pub Node create");
        let publisher =
            open_pub::<markers::ActionEventEnvelope>(&pub_node, &topic, TopicClass::TaskEvent)
                .expect("rr handle_invoke: events_drain Publisher create");
        // node 持 publisher 内部 shm fd；leak 让 publisher 整个生命周期内
        // node 不被 drop。
        Box::leak(Box::new(pub_node));
        publisher
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    //! handler 集成测试（in-process）：构造真 `Coordinator` + 真
    //! `EventsDrainRegistry`，跑 4 个 handler 各路径（含 shutdown regression：
    //! `mark_shutting_down` 之后 invoke 必须返 `Unavailable` 且 registry 不进
    //! 表）。
    //!
    //! 跨进程 RR I/O 路径在 [`super::runtime::tests`] 用 stub handler 锁住；
    //! 真 e2e 端到端 (gateway → s1_daemon → coordinator) 在 B4 (3/3) 跨进程
    //! integration test 覆盖（`crates/robot-rpc/tests/...`）。

    use super::*;
    use crate::action::{ActionId, NO_PAYLOAD_SCHEMA_ID};
    use crate::action_backend::{BackendRegistry, mock_dwell::MockDwellBackend};
    use crate::coordinator::{CoordinatorConfig, HardwareSection};
    use robot_core::time::SystemClock;
    use robot_msg::build_helpers::{
        build_cancel_task_request, build_clear_error_request, build_get_task_status_request,
        build_invoke_task_request,
    };
    use robot_msg::msg::{
        ActionFailureKindFb, ActionKindFb, ActionOutcomeKindFb, ActionRpcStatus,
        CancelTaskResponse as FbCancelResp, ClearErrorResponse as FbClearResp,
        GetTaskStatusResponse as FbStatusResp, InvokeTaskResponse as FbInvokeResp, RpcRole,
        TaskStateFb,
    };

    const LAYOUT_ID: u64 = 0xCAFE_BABE_DEAD_BEEF;

    fn make_ctx(coord: Arc<Coordinator>) -> Arc<RealHandlerCtx> {
        Arc::new(RealHandlerCtx {
            coordinator: coord,
            events_registry: Arc::new(EventsDrainRegistry::new()),
            clock: Arc::new(SystemClock),
            source_id: 2,
            robot_id: format!("rrh_test_{}_{}", std::process::id(), unique_seq()),
        })
    }

    fn unique_seq() -> u32 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        SEQ.fetch_add(1, Ordering::Relaxed)
    }

    fn make_coordinator() -> Arc<Coordinator> {
        let cfg = CoordinatorConfig::default();
        let hardware = HardwareSection {
            id: "test_arm".into(),
            joints: vec!["j0".into()],
        };
        let registry = registry_with_mock_dwell();
        let coord = Coordinator::new(
            cfg,
            hardware,
            LAYOUT_ID,
            vec![0.0_f64],
            registry,
            vec!["mock_dwell".into()],
        );
        coord.mark_idle().unwrap();
        Arc::new(coord)
    }

    fn registry_with_mock_dwell() -> BackendRegistry {
        let mock = Box::leak(Box::new(MockDwellBackend::with_duration_ms(100)));
        BackendRegistry::from_explicit(&[mock as &'static dyn crate::action::ActionBackend])
    }

    fn build_invoke_payload(request_id: &str, kind: ActionKindFb) -> Vec<u8> {
        build_invoke_task_request(
            request_id,
            "idem-001",
            "rrh_test",
            RpcRole::Maintainer,
            "B4 handler test",
            kind,
            /* deadline_ns */ u64::MAX,
            /* priority */ 0,
            /* payload_schema_id */ NO_PAYLOAD_SCHEMA_ID,
            /* payload */ &[],
            /* stamp_ns */ 0,
        )
    }

    fn build_invoke_with_priority(request_id: &str, priority: u32) -> Vec<u8> {
        build_invoke_task_request(
            request_id,
            "idem-001",
            "rrh_test",
            RpcRole::Maintainer,
            "B4 handler test",
            ActionKindFb::DwellMs,
            u64::MAX,
            priority,
            NO_PAYLOAD_SCHEMA_ID,
            &[],
            0,
        )
    }

    fn build_cancel_payload(request_id: &str, action_id: &str) -> Vec<u8> {
        build_cancel_task_request(
            request_id,
            "idem-001",
            "rrh_test",
            RpcRole::Maintainer,
            "B4 handler test",
            action_id,
            u64::MAX,
            0,
        )
    }

    fn build_status_payload(request_id: &str, action_id: &str) -> Vec<u8> {
        build_get_task_status_request(
            request_id,
            "idem-001",
            "rrh_test",
            RpcRole::Viewer,
            "B4 handler test",
            action_id,
            u64::MAX,
            0,
        )
    }

    fn build_clear_payload(request_id: &str) -> Vec<u8> {
        build_clear_error_request(
            request_id,
            "idem-001",
            "rrh_test",
            RpcRole::Maintainer,
            "B4 handler test",
            u64::MAX,
            0,
        )
    }

    // ----- handle_invoke -----

    #[test]
    fn invoke_happy_returns_ok_and_registers_drain() {
        let coord = make_coordinator();
        let ctx = make_ctx(Arc::clone(&coord));

        let request_id = ActionId::new_v7().to_string();
        let payload = build_invoke_payload(&request_id, ActionKindFb::DwellMs);

        let resp_bytes = handle_invoke(&payload, &ctx);
        let resp = flatbuffers::root::<FbInvokeResp>(&resp_bytes).expect("decode resp");
        assert_eq!(resp.request_id().unwrap(), request_id);
        assert_eq!(resp.status(), ActionRpcStatus::Ok);
        assert_eq!(resp.action_id().unwrap(), request_id);
        assert!(resp.accepted_at_ns() > 0);

        // events_drain registered。
        assert_eq!(ctx.events_registry.len(), 1);

        // 收尾：cancel + 让 backend 跑完 + stop drain（registry 接 `&self`，
        // 直接 Arc deref 调即可，**不**走 Arc::try_unwrap）。
        let action_id =
            ActionId(uuid::Uuid::parse_str(&request_id).expect("uuid parse")).to_string();
        let cancel_payload = build_cancel_payload(&ActionId::new_v7().to_string(), &action_id);
        let _ = handle_cancel(&cancel_payload, &ctx);
        coord.mark_shutting_down();
        let _ = ctx.events_registry.stop_and_join_all();
    }

    #[test]
    fn invoke_invalid_request_id_returns_invalid_argument() {
        let coord = make_coordinator();
        let ctx = make_ctx(coord);
        let payload = build_invoke_payload("not-a-uuid", ActionKindFb::DwellMs);

        let resp_bytes = handle_invoke(&payload, &ctx);
        let resp = flatbuffers::root::<FbInvokeResp>(&resp_bytes).unwrap();
        assert_eq!(resp.status(), ActionRpcStatus::InvalidArgument);
        assert!(
            resp.error_msg().unwrap().contains("request_id"),
            "error_msg = {:?}",
            resp.error_msg()
        );
        assert_eq!(ctx.events_registry.len(), 0);
    }

    #[test]
    fn invoke_unknown_kind_returns_invalid_argument() {
        let coord = make_coordinator();
        let ctx = make_ctx(coord);
        let request_id = ActionId::new_v7().to_string();
        let payload = build_invoke_payload(&request_id, ActionKindFb::Unknown);

        let resp_bytes = handle_invoke(&payload, &ctx);
        let resp = flatbuffers::root::<FbInvokeResp>(&resp_bytes).unwrap();
        assert_eq!(resp.status(), ActionRpcStatus::InvalidArgument);
        assert!(resp.error_msg().unwrap().contains("Unknown"));
        assert_eq!(ctx.events_registry.len(), 0);
    }

    #[test]
    fn invoke_priority_overflow_returns_invalid_argument() {
        let coord = make_coordinator();
        let ctx = make_ctx(coord);
        let request_id = ActionId::new_v7().to_string();
        let payload = build_invoke_with_priority(&request_id, 999);

        let resp_bytes = handle_invoke(&payload, &ctx);
        let resp = flatbuffers::root::<FbInvokeResp>(&resp_bytes).unwrap();
        assert_eq!(resp.status(), ActionRpcStatus::InvalidArgument);
        assert!(resp.error_msg().unwrap().contains("priority"));
        assert_eq!(ctx.events_registry.len(), 0);
    }

    #[test]
    fn invoke_garbage_bytes_returns_invalid_argument() {
        let coord = make_coordinator();
        let ctx = make_ctx(coord);
        let payload = vec![0xAA, 0xBB, 0xCC, 0xDD];
        let resp_bytes = handle_invoke(&payload, &ctx);
        let resp = flatbuffers::root::<FbInvokeResp>(&resp_bytes).unwrap();
        assert_eq!(resp.status(), ActionRpcStatus::InvalidArgument);
        assert!(resp.error_msg().unwrap().contains("decode failed"));
        assert_eq!(ctx.events_registry.len(), 0);
    }

    #[test]
    fn invoke_busy_executing_returns_already_exists() {
        let coord = make_coordinator();
        let ctx = make_ctx(Arc::clone(&coord));

        // 第一个 submit 成功（→ Executing）。
        let r1 = ActionId::new_v7().to_string();
        let _ = handle_invoke(&build_invoke_payload(&r1, ActionKindFb::DwellMs), &ctx);

        // 第二个 submit → BusyExecuting → AlreadyExists。
        let r2 = ActionId::new_v7().to_string();
        let resp_bytes = handle_invoke(&build_invoke_payload(&r2, ActionKindFb::DwellMs), &ctx);
        let resp = flatbuffers::root::<FbInvokeResp>(&resp_bytes).unwrap();
        assert_eq!(resp.status(), ActionRpcStatus::AlreadyExists);

        // events_registry 只进一条（第二条 submit 失败不 register）。
        assert_eq!(ctx.events_registry.len(), 1);

        // 收尾。
        coord.mark_shutting_down();
    }

    /// **shutdown regression（reviewer 阻断要求）**：
    ///
    /// `mark_shutting_down` 之后，handler 调 `Coordinator::submit` 必须返
    /// `SubmitError::ShuttingDown` → wire `ActionRpcStatus::Unavailable`，
    /// 且 [`EventsDrainRegistry`] 不被新 register（registry.len() 保持 0）。
    /// 这条契约让 binary 端 shutdown 顺序
    /// `mark_shutting_down → rr.request_stop_all → rr.join_all →
    /// events_registry.stop_and_join_all` 期间任何 in-flight invoke 都不会
    /// 跟 events_registry stop 撞上 late register。
    #[test]
    fn shutdown_in_flight_invoke_rejected_no_register() {
        let coord = make_coordinator();
        let ctx = make_ctx(Arc::clone(&coord));

        coord.mark_shutting_down();

        let request_id = ActionId::new_v7().to_string();
        let payload = build_invoke_payload(&request_id, ActionKindFb::DwellMs);
        let resp_bytes = handle_invoke(&payload, &ctx);
        let resp = flatbuffers::root::<FbInvokeResp>(&resp_bytes).unwrap();

        assert_eq!(resp.status(), ActionRpcStatus::Unavailable);
        assert!(
            resp.error_msg().unwrap().contains("shutting down"),
            "error_msg = {:?}",
            resp.error_msg()
        );
        assert_eq!(
            ctx.events_registry.len(),
            0,
            "events_registry must not register late drain after mark_shutting_down"
        );
    }

    // ----- handle_cancel -----

    #[test]
    fn cancel_executing_action_returns_ok() {
        let coord = make_coordinator();
        let ctx = make_ctx(Arc::clone(&coord));

        let r1 = ActionId::new_v7().to_string();
        let _ = handle_invoke(&build_invoke_payload(&r1, ActionKindFb::DwellMs), &ctx);

        let cancel_req_id = ActionId::new_v7().to_string();
        let payload = build_cancel_payload(&cancel_req_id, &r1);
        let resp_bytes = handle_cancel(&payload, &ctx);
        let resp = flatbuffers::root::<FbCancelResp>(&resp_bytes).unwrap();
        assert_eq!(resp.request_id().unwrap(), cancel_req_id);
        assert_eq!(resp.action_id().unwrap(), r1);
        assert_eq!(resp.status(), ActionRpcStatus::Ok);
        assert!(resp.cancel_at_ns() > 0);

        coord.mark_shutting_down();
    }

    #[test]
    fn cancel_unknown_action_returns_not_found() {
        let coord = make_coordinator();
        let ctx = make_ctx(coord);

        let action_id = ActionId::new_v7().to_string();
        let payload = build_cancel_payload(&ActionId::new_v7().to_string(), &action_id);
        let resp_bytes = handle_cancel(&payload, &ctx);
        let resp = flatbuffers::root::<FbCancelResp>(&resp_bytes).unwrap();
        assert_eq!(resp.status(), ActionRpcStatus::NotFound);
    }

    #[test]
    fn cancel_invalid_action_id_returns_invalid_argument() {
        let coord = make_coordinator();
        let ctx = make_ctx(coord);
        let payload = build_cancel_payload(&ActionId::new_v7().to_string(), "not-a-uuid");
        let resp_bytes = handle_cancel(&payload, &ctx);
        let resp = flatbuffers::root::<FbCancelResp>(&resp_bytes).unwrap();
        assert_eq!(resp.status(), ActionRpcStatus::InvalidArgument);
    }

    #[test]
    fn cancel_garbage_bytes_returns_invalid_argument() {
        let coord = make_coordinator();
        let ctx = make_ctx(coord);
        let resp_bytes = handle_cancel(&[0xAA, 0xBB, 0xCC, 0xDD], &ctx);
        let resp = flatbuffers::root::<FbCancelResp>(&resp_bytes).unwrap();
        assert_eq!(resp.status(), ActionRpcStatus::InvalidArgument);
    }

    // ----- handle_status -----

    #[test]
    fn status_executing_action_returns_executing_state() {
        let coord = make_coordinator();
        let ctx = make_ctx(Arc::clone(&coord));

        let r1 = ActionId::new_v7().to_string();
        let _ = handle_invoke(&build_invoke_payload(&r1, ActionKindFb::DwellMs), &ctx);

        let payload = build_status_payload(&ActionId::new_v7().to_string(), &r1);
        let resp_bytes = handle_status(&payload, &ctx);
        let resp = flatbuffers::root::<FbStatusResp>(&resp_bytes).unwrap();
        assert_eq!(resp.status(), ActionRpcStatus::Ok);
        assert_eq!(resp.state(), TaskStateFb::Executing);
        assert_eq!(resp.action_id().unwrap(), r1);

        coord.mark_shutting_down();
    }

    #[test]
    fn status_unknown_action_returns_not_found() {
        let coord = make_coordinator();
        let ctx = make_ctx(coord);
        let payload = build_status_payload(
            &ActionId::new_v7().to_string(),
            &ActionId::new_v7().to_string(),
        );
        let resp_bytes = handle_status(&payload, &ctx);
        let resp = flatbuffers::root::<FbStatusResp>(&resp_bytes).unwrap();
        assert_eq!(resp.status(), ActionRpcStatus::NotFound);
        assert_eq!(resp.state(), TaskStateFb::Unknown);
    }

    #[test]
    fn status_invalid_action_id_returns_invalid_argument() {
        let coord = make_coordinator();
        let ctx = make_ctx(coord);
        let payload = build_status_payload(&ActionId::new_v7().to_string(), "not-a-uuid");
        let resp_bytes = handle_status(&payload, &ctx);
        let resp = flatbuffers::root::<FbStatusResp>(&resp_bytes).unwrap();
        assert_eq!(resp.status(), ActionRpcStatus::InvalidArgument);
    }

    // ----- handle_clear_error -----

    #[test]
    fn clear_error_idle_returns_failed_precondition() {
        let coord = make_coordinator();
        let ctx = make_ctx(coord);
        let payload = build_clear_payload(&ActionId::new_v7().to_string());
        let resp_bytes = handle_clear_error(&payload, &ctx);
        let resp = flatbuffers::root::<FbClearResp>(&resp_bytes).unwrap();
        assert_eq!(resp.status(), ActionRpcStatus::FailedPrecondition);
        assert!(resp.error_msg().unwrap().contains("DEGRADED"));
    }

    #[test]
    fn clear_error_garbage_bytes_returns_invalid_argument() {
        let coord = make_coordinator();
        let ctx = make_ctx(coord);
        let resp_bytes = handle_clear_error(&[0xAA, 0xBB], &ctx);
        let resp = flatbuffers::root::<FbClearResp>(&resp_bytes).unwrap();
        assert_eq!(resp.status(), ActionRpcStatus::InvalidArgument);
    }

    // ----- closure construction -----

    fn assert_send_static<F: Fn(&[u8]) -> Vec<u8> + Send + 'static>(_: F) {}

    #[test]
    fn ctx_handler_closures_are_sendable() {
        // 锁住 RealHandlerCtx::*_handler 返 Send + 'static closure（不会
        // 退化成借用，能进 thread::spawn / spawn_*_server 闭包）。
        let coord = make_coordinator();
        let ctx = make_ctx(coord);
        assert_send_static(ctx.invoke_handler());
        assert_send_static(ctx.cancel_handler());
        assert_send_static(ctx.status_handler());
        assert_send_static(ctx.clear_error_handler());
    }

    // ----- 引用，避免编译期 unused 警告 -----

    #[allow(dead_code)]
    fn _force_use_kind_enums() -> (ActionFailureKindFb, ActionOutcomeKindFb) {
        (ActionFailureKindFb::Unknown, ActionOutcomeKindFb::Unknown)
    }
}
