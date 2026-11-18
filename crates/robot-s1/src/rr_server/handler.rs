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
//! `robot-s1` shutdown 顺序锁住：`mark_shutting_down → rr.request_stop_all
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
use robot_transport::iceoryx2::qos::TopicClass;

use crate::action::{ActionId, ActionKind, ActionRequest, ActionSource};
use crate::coordinator::Coordinator;
use crate::events_drain::{
    EventsDrainRegistry, EventsDrainSpawnError, EventsDrainThreadConfig, EventsPublisherGuard,
    spawn_owned_events_drain_thread,
};

use super::wire::{
    cancel_err_to_rpc_status, clear_err_to_rpc_status, decode_cancel_request,
    decode_clear_error_request, decode_invoke_request, decode_status_request,
    encode_cancel_response_err, encode_cancel_response_ok, encode_clear_error_response_err,
    encode_clear_error_response_ok, encode_invoke_drain_failure, encode_invoke_response_err,
    encode_invoke_response_ok, encode_status_response_err, encode_status_response_ok,
    submit_err_to_rpc_status,
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
    /// `events_drain_thread` 写入 envelope `FrameHeader.source_id`。robot-s1
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
    #[cfg(feature = "test-fault-backend")]
    let fixture_request_id = flatbuffers::root::<robot_msg::msg::InvokeTaskRequest<'_>>(payload)
        .ok()
        .and_then(|view| view.request_id());
    handle_invoke_with_spawn(payload, ctx, |cfg, events, clock, source_id| {
        #[cfg(feature = "test-fault-backend")]
        match take_drain_fixture(fixture_request_id.unwrap_or("")) {
            Some(DrainFixture::FactoryPanic) => {
                return spawn_owned_events_drain_thread(
                    cfg,
                    events,
                    || panic!("w3_real_drain_factory_panic"),
                    clock,
                    source_id,
                );
            }
            Some(DrainFixture::SpawnRejected) => {
                return Err(EventsDrainSpawnError::ThreadSpawn(std::io::Error::other(
                    "w3 注入 OS spawn 拒绝",
                )));
            }
            None => {}
        }
        let factory = make_events_publisher_factory(cfg.topic.clone());
        spawn_owned_events_drain_thread(cfg, events, factory, clock, source_id)
    })
}

// 默认构建没有此入口；精确匹配单个 request_id，仅在真实 submit 成功后的 spawn 边界消费一次。
#[cfg(feature = "test-fault-backend")]
#[derive(Clone, Copy)]
enum DrainFixture {
    FactoryPanic,
    SpawnRejected,
}
#[cfg(feature = "test-fault-backend")]
fn take_drain_fixture(request_id: &str) -> Option<DrainFixture> {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};
    static CONFIG: OnceLock<Option<(String, DrainFixture)>> = OnceLock::new();
    static USED: AtomicBool = AtomicBool::new(false);
    let config = CONFIG.get_or_init(|| {
        let id = std::env::var("ROBOT_W3_DRAIN_FAILURE_REQUEST_ID").ok()?;
        let mode = match std::env::var("ROBOT_W3_DRAIN_FAILURE_MODE").ok()?.as_str() {
            "factory-panic" => DrainFixture::FactoryPanic,
            "spawn-error" => DrainFixture::SpawnRejected,
            _ => return None,
        };
        Some((id, mode))
    });
    match config {
        Some((id, mode)) if id == request_id && !USED.swap(true, Ordering::AcqRel) => Some(*mode),
        _ => None,
    }
}

// 唯一提交/注册/取消/编码分支；测试只替换真实 OS spawn 边界，不伪造 cancel 结果。
#[allow(
    clippy::too_many_lines,
    reason = "完整展示同一次提交、原句柄登记、真实取消与编码的责任顺序"
)]
fn handle_invoke_with_spawn<F>(payload: &[u8], ctx: &RealHandlerCtx, spawn: F) -> Vec<u8>
where
    F: FnOnce(
        EventsDrainThreadConfig,
        std::sync::mpsc::Receiver<crate::action::ActionEvent>,
        Arc<dyn RobotClock>,
        u32,
    ) -> Result<crate::events_drain::EventsDrainThreadHandle, EventsDrainSpawnError>,
{
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

    // 3. ActionKindFb → ActionKind（v0.4 候选 2 round 2 (2/4) land 后 4 个
    //    variant：DwellMs / HoldPose / Scripted / Ik）。`other` 兜底 catch-all
    //    保留：forward-compat 拦截"客户端用了 daemon 二进制还不识别的 fbs 变体"
    //    场景（fbs enum 是 open-ended ubyte）。
    let kind = match decoded.kind {
        ActionKindFb::DwellMs => ActionKind::DwellMs,
        ActionKindFb::HoldPose => ActionKind::HoldPose,
        ActionKindFb::Scripted => ActionKind::Scripted,
        ActionKindFb::Ik => ActionKind::Ik,
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
    #[cfg(feature = "test-base-command")]
    if matches!(&req.source, ActionSource::RpcGateway { actor } if actor == "web-base-renew") {
        return match ctx.coordinator.renew_simulated_base(&req, clock_ref) {
            Ok(()) => encode_invoke_response_ok(
                &decoded.request_id,
                &decoded.request_id,
                clock_ref.now().0,
            ),
            Err(error) => encode_invoke_response_err(
                &decoded.request_id,
                &decoded.request_id,
                submit_err_to_rpc_status(&error),
                &error.to_string(),
            ),
        };
    }
    match ctx.coordinator.submit(req, clock_ref) {
        Ok(ticket) => {
            let action_id = ticket.action_id;
            let accepted_at_ns = clock_ref.now().0;

            // 7. spawn events_drain_thread + register。
            let cfg = EventsDrainThreadConfig::for_action(&ctx.robot_id, action_id);
            match spawn(cfg, ticket.events, Arc::clone(&ctx.clock), ctx.source_id) {
                Ok(handle) => {
                    ctx.events_registry.register(handle);
                    // v0.4 候选 3 Plan §P.2.1.d: register 即时 log。
                    // smoke binary 端 stderr scrape 这行抓
                    // `events_registry_len=N` 增长曲线；shutdown 期 final
                    // log 在 robot-s1::main 内对称打。注销路径 log 留
                    // AC-C1 触发（reap_finished outline）。
                    tracing::info!(
                        events_registry_len = ctx.events_registry.len(),
                        action_id = %action_id,
                        op = "register",
                        "robot-s1 events_drain registered"
                    );
                    let action_id_hex = action_id.to_string();
                    encode_invoke_response_ok(&decoded.request_id, &action_id_hex, accepted_at_ns)
                }
                Err(e) => {
                    // 先把真实已创建线程交给唯一 registry，再消费真实 cancel 结果。
                    // 请求退休不表示任务已结束，也不表示该线程已经 join。
                    let error_msg = format!("spawn_events_drain_thread failed: {e}");
                    match e {
                        EventsDrainSpawnError::ReadyDisconnected { handle } => {
                            handle.request_stop();
                            ctx.events_registry.register(handle);
                        }
                        EventsDrainSpawnError::ThreadSpawn(_) => {}
                    }
                    let cancel_ok = ctx.coordinator.cancel(action_id, clock_ref).is_ok();
                    tracing::error!(
                        error = %error_msg,
                        action_id = %action_id,
                        cancel_ok,
                        "rr_server::handle_invoke: drain 启动失败，已移交回收责任并请求取消"
                    );
                    encode_invoke_drain_failure(
                        &decoded.request_id,
                        &action_id.to_string(),
                        accepted_at_ns,
                        &error_msg,
                        cancel_ok,
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

/// 每个 `ActionId` 的端口与 Node 由同一 owner 持有，完整端口先于 Node 析构。
fn make_events_publisher_factory(
    topic: String,
) -> impl FnOnce() -> EventsPublisherGuard + Send + 'static {
    move || {
        let pub_node = NodeBuilder::new()
            .create::<IxService>()
            .expect("rr handle_invoke: events_drain pub Node create");
        let publisher =
            open_pub::<markers::ActionEventEnvelope>(&pub_node, &topic, TopicClass::TaskEvent)
                .expect("rr handle_invoke: events_drain Publisher create");
        // 0.9.3 的 SharedNode 引用足以保活，但可能在 port_tag 析构前归零。
        // 显式 owner 保证先完整删除端口标记，再释放 Node 目录，不永久保留资源。
        EventsPublisherGuard::owned(publisher, pub_node)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(all(test, target_os = "linux"))]
mod node_lifetime_tests {
    use super::*;
    use robot_core::time::{DurationNs, SystemClock};
    use robot_msg::build_helpers::build_action_event_envelope;
    use robot_msg::msg::{ActionEventEnvelope, ActionEventKindFb};
    use robot_transport::iceoryx2::{
        ObserverSubscriber, observer_subscriber, subscriber as open_sub,
    };
    use robot_transport::{LatestSubscriber, Publisher};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    const CHILD: &str = "ROBOT_TEST_ACTION_PUBLISHER_LIFETIME_CHILD";
    const TEST: &str =
        "rr_server::handler::node_lifetime_tests::action_publisher_releases_node_under_fd_limit";

    #[test]
    fn action_publisher_releases_node_under_fd_limit() {
        if std::env::var_os(CHILD).is_some() {
            run_child();
            return;
        }
        let sentinel = inherited_sentinel();
        let sentinel_fd = std::os::fd::AsRawFd::as_raw_fd(&sentinel);
        let fd_limit = fallback_fd_limit();
        let force_fcntl = std::env::var_os("ROBOT_TEST_NODE_FORCE_FCNTL").is_some();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("ROBOT_TEST_NODE_FD_SENTINEL", sentinel_fd.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // 仅 fork 后的子进程标记 CLOEXEC，不关闭父进程或 spawn 错误管道。
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut command, move || {
                if force_fcntl {
                    mark_child_fds_with_fcntl(fd_limit)
                } else {
                    mark_child_fds_cloexec(fd_limit)
                }
            });
        }
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        // 两条管道同时读取，底层错误日志不能阻塞子进程并制造假超时。
        let stdout_reader = std::thread::spawn(move || read_output(stdout));
        let stderr_reader = std::thread::spawn(move || read_output(stderr));
        let clock = SystemClock::new();
        let deadline = clock.now() + DurationNs::from_secs(30);
        let mut timed_out = false;
        while child.try_wait().unwrap().is_none() {
            if clock.now() >= deadline {
                child.kill().unwrap();
                timed_out = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = child.wait().unwrap();
        assert_eq!(
            unsafe { libc::fcntl(sentinel_fd, libc::F_GETFD) },
            0,
            "子进程隔离不能改父 FD 标志"
        );
        let stdout_bytes = stdout_reader.join().unwrap();
        let stderr_bytes = stderr_reader.join().unwrap();
        let stdout = String::from_utf8_lossy(&stdout_bytes);
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        if status.success() {
            for line in stdout
                .lines()
                .filter(|line| line.contains("action_publisher_"))
            {
                eprintln!("{line}");
            }
        } else {
            eprintln!("{stdout}{stderr}");
        }
        assert!(!timed_out, "真实 publisher 生命周期子进程超时");
        assert!(stdout.contains("action_publisher_child nofile=128"));
        assert!(status.success(), "子进程失败：{status}");
        assert!(stdout.contains("action_publisher_done rounds=256"));
    }

    fn inherited_sentinel() -> std::os::fd::OwnedFd {
        use std::os::fd::{AsRawFd, FromRawFd};
        let source = std::fs::File::open("/dev/null").unwrap();
        // F_DUPFD 故意不设置 CLOEXEC，模拟前序测试保留的库资源；OwnedFd 最终归还。
        let raw = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD, 64) };
        assert!(raw >= 64);
        unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) }
    }

    fn fallback_fd_limit() -> libc::c_int {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) },
            0
        );
        // 旧内核回退只遍历有限的父进程硬上界；不改变任一进程的限额。
        libc::c_int::try_from(limit.rlim_max).expect("测试要求有限且可表示的 NOFILE 硬上界")
    }

    fn mark_child_fds_cloexec(fd_limit: libc::c_int) -> std::io::Result<()> {
        // pre_exec 内仅作系统调用与栈上运算，不分配、记录日志或获取进程锁。
        let result = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3_u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENOSYS | libc::EINVAL) => mark_child_fds_with_fcntl(fd_limit),
            _ => Err(error),
        }
    }

    fn mark_child_fds_with_fcntl(fd_limit: libc::c_int) -> std::io::Result<()> {
        for fd in 3..fd_limit {
            loop {
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                if flags >= 0
                    && unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == 0
                {
                    break;
                }
                let error = std::io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::EBADF) => break,
                    Some(libc::EINTR) => {}
                    _ => return Err(error),
                }
            }
        }
        Ok(())
    }

    fn read_output(mut pipe: impl std::io::Read) -> Vec<u8> {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).unwrap();
        bytes
    }

    fn owned_node_directories(expected: usize) -> std::collections::BTreeSet<std::path::PathBuf> {
        // 只观察本隔离子进程实际打开的 Node details；不遍历或删除全局资源。
        let paths: std::collections::BTreeSet<_> = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name == "iox2_node.details")
            })
            .map(|path| path.parent().unwrap().to_path_buf())
            .collect();
        assert_eq!(
            paths.len(),
            expected,
            "本轮真实工厂主节点、镜像 worker 和对照 subscriber 的 Node 数必须匹配生命周期阶段"
        );
        assert!(paths.iter().all(|path| path.is_dir()));
        paths
    }

    fn assert_node_directories_released(paths: &std::collections::BTreeSet<std::path::PathBuf>) {
        for path in paths {
            assert!(
                !path.exists(),
                "真实 Node 完整退场后目录仍残留（空目录同样失败）：{}",
                path.display()
            );
        }
    }

    fn fd_count() -> usize {
        std::fs::read_dir("/proc/self/fd").unwrap().count()
    }

    fn owned_thread_ids() -> std::collections::BTreeSet<std::ffi::OsString> {
        std::fs::read_dir("/proc/self/task")
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect()
    }

    fn wait_observer(
        node: &iceoryx2::node::Node<IxService>,
        topic: &str,
        clock: SystemClock,
    ) -> ObserverSubscriber<markers::ActionEventEnvelope> {
        let deadline = clock.now() + DurationNs::from_secs(2);
        loop {
            match observer_subscriber(node, topic) {
                Ok(subscriber) => return subscriber,
                Err(error) => assert!(clock.now() < deadline, "真实工厂镜像启动超时：{error}"),
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn run_child() {
        let sentinel_fd: libc::c_int = std::env::var("ROBOT_TEST_NODE_FD_SENTINEL")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            unsafe { libc::fcntl(sentinel_fd, libc::F_GETFD) },
            -1,
            "exec 不得继承父进程哨兵 FD"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );
        println!("action_publisher_initial_fd_count count={}", fd_count());
        for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
            let entry = entry.unwrap();
            if let Ok(target) = std::fs::read_link(entry.path()) {
                println!(
                    "action_publisher_initial_fd fd={} target={}",
                    entry.file_name().to_string_lossy(),
                    target.display()
                );
            }
        }
        let limit = libc::rlimit {
            rlim_cur: 128,
            rlim_max: 128,
        };
        let no_core = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // 只收紧这个隔离子进程；真实资源创建错误必须导致测试失败。
        assert_eq!(
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit) },
            0
        );
        assert_eq!(
            unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const no_core) },
            0
        );
        println!("action_publisher_child nofile=128");
        let clock = SystemClock::new();
        let robot_id = format!(
            "publisher_lifetime_{}_{}",
            std::process::id(),
            clock.now().0
        );
        let baseline = fd_count();
        for index in 0..256 {
            publisher_round_trip(&robot_id, index);
            if index % 8 == 0 {
                println!("action_publisher_fd round={index} count={}", fd_count());
            }
        }
        let final_count = fd_count();
        println!("action_publisher_done rounds=256 baseline={baseline} final={final_count}");
        assert!(final_count <= baseline, "每 Action 的 Node/port FD 未归还");
    }

    fn publisher_round_trip(robot_id: &str, index: u64) {
        let clock = SystemClock::new();
        let action_id = uuid::Uuid::now_v7().to_string();
        let topic = format!("/{robot_id}/s1/action_events/{action_id}");
        // 两条对照订阅复用同一个 Node，不增加无法区分归属的镜像读者节点。
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let subscriber_directories = owned_node_directories(1);
        let initial_threads = owned_thread_ids();
        // 调真正 handle_invoke 工厂；返回 owner 必须保活端口并正确归还资源。
        let mut publisher = make_events_publisher_factory(topic.clone())();
        let mut subscriber =
            open_sub::<markers::ActionEventEnvelope>(&node, &topic, TopicClass::TaskEvent).unwrap();
        let mut observer = wait_observer(&node, &topic, clock);
        let stamp = clock.now().0;
        let body = build_action_event_envelope(
            &action_id,
            stamp,
            ActionEventKindFb::Started,
            0.0,
            None,
            None,
        );
        let mut writer = publisher.loan(body.len() + 1024).unwrap();
        writer.set_seq(index);
        writer.set_body_bytes(&body);
        writer.publish().unwrap();
        let deadline = clock.now() + DurationNs::from_secs(2);
        let mut primary_received = false;
        let mut mirror_received = false;
        loop {
            publisher.update_connections().unwrap();
            if let Some(sample) = subscriber.latest().unwrap() {
                let view = sample.view().unwrap();
                let envelope: ActionEventEnvelope<'_> = view.body.get();
                assert_eq!(envelope.action_id(), Some(action_id.as_str()));
                assert_eq!(envelope.at_ns(), stamp);
                assert_eq!(view.header.get().seq(), index);
                primary_received = true;
            }
            if let Some(sample) = observer.try_recv().unwrap() {
                let view = sample.view().unwrap();
                let envelope: ActionEventEnvelope<'_> = view.body.get();
                assert_eq!(envelope.action_id(), Some(action_id.as_str()));
                assert_eq!(envelope.at_ns(), stamp);
                assert_eq!(view.header.get().seq(), index);
                assert!(sample.metadata().observer_seq > 0);
                mirror_received = true;
            }
            if primary_received && mirror_received {
                break;
            }
            assert!(
                clock.now() < deadline,
                "工厂返回的 owner 必须保持正式消息及真实镜像可达"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        // 双路真实回执证明镜像已完成初始化，再锁定其 Node 和线程的生命周期。
        let node_directories = owned_node_directories(3);
        assert!(node_directories.is_superset(&subscriber_directories));
        let factory_directories = node_directories
            .difference(&subscriber_directories)
            .cloned()
            .collect();
        let active_threads = owned_thread_ids();
        assert!(active_threads.is_superset(&initial_threads));
        assert_eq!(active_threads.len(), initial_threads.len() + 1);
        drop(observer);
        drop(subscriber);
        drop(publisher);
        // Linux 在 exit_mm 的 clear_child_tid 唤醒 join，随后 release_task 才移除
        // thread_group；/proc 快照可短暂滞后。仅有界等待可见性收敛，不放宽线程集判据。
        let exit_deadline = clock.now() + DurationNs::from_secs(1);
        let mut remaining_threads = owned_thread_ids();
        while remaining_threads != initial_threads && clock.now() < exit_deadline {
            std::thread::sleep(Duration::from_millis(1));
            remaining_threads = owned_thread_ids();
        }
        assert_eq!(
            remaining_threads, initial_threads,
            "工厂镜像线程必须真实回收"
        );
        assert_eq!(owned_node_directories(1), subscriber_directories);
        assert_node_directories_released(&factory_directories);
        drop(node);
        assert!(owned_node_directories(0).is_empty());
        assert_node_directories_released(&node_directories);
    }
}

#[cfg(test)]
mod tests {
    //! handler 集成测试（in-process）：构造真 `Coordinator` + 真
    //! `EventsDrainRegistry`，跑 4 个 handler 各路径（含 shutdown regression：
    //! `mark_shutting_down` 之后 invoke 必须返 `Unavailable` 且 registry 不进
    //! 表）。
    //!
    //! 跨进程 RR I/O 路径在 [`super::runtime::tests`] 用 stub handler 锁住；
    //! 真 e2e 端到端 (gateway → robot-s1 → coordinator) 在 B4 (3/3) 跨进程
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
            motor_ids: vec![1],
            feetech_bus2_mixed_raw: false,
            calibration_range_tolerances_raw: None,
        };
        let registry = registry_with_mock_dwell();
        let coord = Coordinator::new(
            cfg,
            hardware,
            LAYOUT_ID,
            vec![0.0_f64],
            registry,
            vec!["mock_dwell".into()],
            /* solver */ None,
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

    /// `ActionKindFb` → [`ActionKind`] 显式映射全 variant 锁测（v0.4
    /// 候选 1 Plan round 1 阻断 1 拍板：proto / fbs / lib 三层 enum
    /// append 必须同 commit 一起 land；本测在 (1/3) 阶段断言 ActionKindFb
    /// 已知 variant 集 + 每个 variant 都被 handler match 接受）。
    ///
    /// `ActionKindFb` 是 fbs ubyte enum，wire 上可携任意 0..=255 值；
    /// `ENUM_VALUES` 包含 schema 文件里显式声明的 variant（含
    /// `Unknown = 0`）。新增 variant → 本断言失败 → 强制更新
    /// `handle_invoke` 内 `ActionKindFb → ActionKind` match。
    #[test]
    fn action_kind_fb_variant_count_matches_explicit_mapping() {
        // v0.4 候选 2 round 2 (2/4) land 后：Unknown / DwellMs / HoldPose /
        // Scripted / Ik = 5 variant。新增 variant 时本断言失败强制更新
        // handler match。
        const EXPECTED_VARIANT_COUNT: usize = 5;
        assert_eq!(
            ActionKindFb::ENUM_VALUES.len(),
            EXPECTED_VARIANT_COUNT,
            "新增 ActionKindFb variant 必须同步更新 handle_invoke 内的 \
             ActionKindFb → ActionKind match + 本断言数字（v0.4 候选 1 \
             Plan round 1 阻断 1）"
        );

        // 已知 variant 集 = 显式声明的 5 个（数值 0..=4）。
        let expected: std::collections::HashSet<ActionKindFb> = [
            ActionKindFb::Unknown,
            ActionKindFb::DwellMs,
            ActionKindFb::HoldPose,
            ActionKindFb::Scripted,
            ActionKindFb::Ik,
        ]
        .into_iter()
        .collect();
        let registered: std::collections::HashSet<ActionKindFb> =
            ActionKindFb::ENUM_VALUES.iter().copied().collect();
        assert_eq!(
            registered, expected,
            "ActionKindFb ENUM_VALUES 集合跟显式断言集合不一致；新增 \
             variant 时必须同步本测断言集合 + handler match"
        );
    }

    /// `ActionKindFb::Scripted` → [`ActionKind::Scripted`] 映射在 handler
    /// 里命中（v0.4 候选 1 (1/3) land）。本测进 dispatch 后因 registry
    /// 不含 scripted backend → `Coordinator::submit()` 走
    /// `BackendRegistry::dispatch` 返
    /// [`SubmitError::NoBackendForKind`]（详 Spec Plan P.2.1.b 表 #7 +
    /// reviewer Plan round 2 阻断 3 拍板：(1/3) **不**加手工 dispatch
    /// arm，靠 (2/3) `inventory::submit!` 让 dispatch 自动命中）。
    #[test]
    fn invoke_scripted_kind_without_backend_returns_unimplemented() {
        let coord = make_coordinator();
        let ctx = make_ctx(coord);
        let request_id = ActionId::new_v7().to_string();
        let payload = build_invoke_payload(&request_id, ActionKindFb::Scripted);

        let resp_bytes = handle_invoke(&payload, &ctx);
        let resp = flatbuffers::root::<FbInvokeResp>(&resp_bytes).unwrap();
        // BackendRegistry::dispatch 找不到任何支持 Scripted kind 的
        // backend → SubmitError::NoBackendForKind → wire mapping
        // (crates/robot-s1/src/rr_server/wire.rs) → ActionRpcStatus::Unimplemented。
        assert_eq!(
            resp.status(),
            ActionRpcStatus::Unimplemented,
            "ActionKindFb::Scripted 在 (1/3) 阶段（无 ScriptedBackend）应走 \
             NoBackendForKind 路径返 Unimplemented；error_msg = {:?}",
            resp.error_msg()
        );
        // events drain 不应注册（submit 失败路径）。
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
    #[test]
    fn invoke_real_ready_panic_keeps_handle_and_reports_cancel_accepted() {
        use robot_msg::msg::{InvokeOutcome, InvokeTaskResponse, SideEffect};
        let coordinator = make_coordinator();
        let ctx = make_ctx(Arc::clone(&coordinator));
        let request_id = uuid::Uuid::now_v7().to_string();
        let payload = build_invoke_payload(&request_id, ActionKindFb::DwellMs);
        let wire = handle_invoke_with_spawn(&payload, &ctx, |cfg, events, clock, source| {
            spawn_owned_events_drain_thread(
                cfg,
                events,
                || panic!("w3_real_publisher_factory_panic"),
                clock,
                source,
            )
        });
        let view = flatbuffers::root::<InvokeTaskResponse<'_>>(&wire).unwrap();
        assert_eq!(view.request_id(), Some(request_id.as_str()));
        assert_eq!(view.action_id(), Some(request_id.as_str()));
        assert_ne!(view.accepted_at_ns(), 0);
        assert_eq!(view.status(), ActionRpcStatus::Internal);
        assert_eq!(view.effect(), SideEffect::Committed);
        assert!(view.retired());
        assert_eq!(
            view.invoke_outcome(),
            InvokeOutcome::DrainFailedCancelAccepted
        );
        assert_eq!(ctx.events_registry.len(), 1, "原失败句柄只登记一次");
        coordinator.tick_action(ctx.clock.as_ref(), 0);
        let joined = ctx.events_registry.stop_and_join_all();
        let panic = joined.unwrap_err();
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"w3_real_publisher_factory_panic")
        );
        assert_eq!(ctx.events_registry.len(), 0);
        ctx.events_registry.join_all().unwrap();
    }

    #[test]
    fn invoke_injected_os_spawn_failure_uses_real_cancel_accepted_and_rejected() {
        use robot_core::time::{DurationNs, MockClock, MonoRawNs};
        use robot_msg::msg::{InvokeOutcome, InvokeTaskResponse, SideEffect};
        for completed_before_compensation in [false, true] {
            let coordinator = make_coordinator();
            let clock = Arc::new(MockClock::new(MonoRawNs(41)));
            let mut ctx = Arc::try_unwrap(make_ctx(Arc::clone(&coordinator)))
                .ok()
                .unwrap();
            ctx.clock = clock.clone();
            let request_id = uuid::Uuid::now_v7().to_string();
            let payload = build_invoke_payload(&request_id, ActionKindFb::DwellMs);
            let mut called = 0;
            let wire =
                handle_invoke_with_spawn(&payload, &ctx, |_cfg, _events, _clock, _source| {
                    called += 1;
                    if completed_before_compensation {
                        clock.advance(DurationNs::from_millis(101));
                        coordinator.tick_action(clock.as_ref(), 0);
                    }
                    Err(EventsDrainSpawnError::ThreadSpawn(std::io::Error::other(
                        "w3 注入 OS spawn 边界错误",
                    )))
                });
            let view = flatbuffers::root::<InvokeTaskResponse<'_>>(&wire).unwrap();
            assert_eq!(called, 1);
            assert_eq!(view.request_id(), Some(request_id.as_str()));
            assert_eq!(view.action_id(), Some(request_id.as_str()));
            assert_eq!(view.accepted_at_ns(), 41);
            assert_eq!(view.status(), ActionRpcStatus::Internal);
            assert_eq!(view.effect(), SideEffect::Committed);
            assert!(view.retired());
            assert_eq!(
                view.invoke_outcome(),
                if completed_before_compensation {
                    InvokeOutcome::DrainFailedCancelRejected
                } else {
                    InvokeOutcome::DrainFailedCancelAccepted
                }
            );
            assert_eq!(ctx.events_registry.len(), 0, "OS spawn 拒绝没有可登记句柄");
            if !completed_before_compensation {
                coordinator.tick_action(clock.as_ref(), 0);
            }
            ctx.events_registry.join_all().unwrap();
        }
    }
}
