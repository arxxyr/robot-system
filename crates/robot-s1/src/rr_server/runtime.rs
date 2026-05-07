//! `rr_server` 运行时层 —— 4 个 RR topic 的线程生命周期 / 主循环 /
//! `Iceoryx2RrServer` I/O 调度。
//!
//! ## 角色
//!
//! 沿 A1-3 (1/3c-1) `crates/robot-s0/src/rr_server_thread.rs` 同款
//! "rr_server_thread 只做 iceoryx2 RR I/O" 模型。本模块**不**接
//! [`crate::coordinator::Coordinator`] / **不**做 fbs encode/decode；handler
//! 闭包（来自 [`crate::rr_server::handler`]）拿 `payload: &[u8]` 自行 decode
//! + 调 coordinator API + 编 response bytes 返回。
//!
//! ## 不在范围（runtime 边界）
//!
//! - **不**做 fbs root verifier 之外的 decode；handler 内部自己 decode。
//! - **不**接业务语义（B4 (2/3-a) handler 才接 [`crate::coordinator::Coordinator`]）。
//! - **不**做 idempotency cache / auth / capability 校验（gateway 端做）。
//!
//! ## 线程语义
//!
//! - **非 RT** 普通优先级线程；每个 RR topic 一个独立 `std::thread`，独立
//!   [`Iceoryx2RrServer`] 单 owner（iceoryx2 service `SingleThreaded`，
//!   server `!Send`，必须在 owner 线程内构造；同 A1-3 模式）。
//! - 主循环：drain `try_recv()` →（每条 active）fbs root verify → handler
//!   产 response bytes → `send_response` → 继续 drain 直到 `Ok(None)` →
//!   sleep `poll_interval_ns`。
//! - **同步 reply**（handler 闭包调用即返 response bytes）：handler 闭包必
//!   须不阻塞 IO（B4 (2/3-a) handler 调 [`crate::coordinator::Coordinator`]
//!   in-process API + spawn 普通线程，无网络 / 文件 / 长锁）。
//! - 退出靠 `Arc<AtomicBool>` stop signal（沿 A1 同款）。
//! - `s1_daemon` 主循环退出时**逐个** [`RrServerThreadHandle::request_stop`]
//!   + [`RrServerThreadHandle::join`]；所有 RR server 线程都 join 完才返
//!     main —— **不**悬挂线程到 process exit。详 [`S1RrServerHandles`]。
//!
//! ## decode error 路径
//!
//! `flatbuffers::root::<Req::Root<'_>>` 失败 → 调 `decode_error_reply`
//! （**静态 fn pointer，零捕获**；每 topic 一份）reply
//! [`robot_msg::msg::ActionRpcStatus::InvalidArgument`] + `error_msg = "wire
//! decode failed"`。跟 A1 s0 同款：消费 active handle、**不**让 wire request
//! 留在 iceoryx2 队列里悬挂到客户端 deadline 超时。
//!
//! ## handler 边界（B4 (2/3-a) 落地）
//!
//! handler 闭包签名 `Fn(&[u8]) -> Vec<u8> + Send + 'static`，由 caller 通
//! 过 [`crate::rr_server::handler::RealHandlerCtx`] 构造。runtime 接受闭
//! 包是为锁住"runtime 不知道 coordinator API"边界 —— `RealHandlerCtx` 持
//! `Arc<Coordinator>` / `Arc<EventsDrainRegistry>` / `Arc<dyn RobotClock>` /
//! `source_id` / `robot_id`，闭包捕获后由 spawn fn 一并 move 进 RR server
//! 线程。

#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use robot_msg::markers;
use robot_msg::markers::BodyMarker;
use robot_msg::msg::ActionRpcStatus;
use robot_transport::iceoryx2::request_response::{Iceoryx2ActiveRequest, Iceoryx2RrServer};

use super::wire::{
    encode_cancel_response_err, encode_clear_error_response_err, encode_invoke_response_err,
    encode_status_response_err,
};

// ============================================================================
// decode-error reply 静态 fn pointer
// ============================================================================

/// fbs root verifier 失败时通用 `error_msg`。
const DECODE_ERROR_MSG: &str = "wire decode failed";

fn invoke_decode_error_reply(_payload: &[u8]) -> Vec<u8> {
    encode_invoke_response_err("", "", ActionRpcStatus::InvalidArgument, DECODE_ERROR_MSG)
}

fn cancel_decode_error_reply(_payload: &[u8]) -> Vec<u8> {
    encode_cancel_response_err("", "", ActionRpcStatus::InvalidArgument, DECODE_ERROR_MSG)
}

fn status_decode_error_reply(_payload: &[u8]) -> Vec<u8> {
    encode_status_response_err("", "", ActionRpcStatus::InvalidArgument, DECODE_ERROR_MSG)
}

fn clear_error_decode_error_reply(_payload: &[u8]) -> Vec<u8> {
    encode_clear_error_response_err("", ActionRpcStatus::InvalidArgument, DECODE_ERROR_MSG)
}

// ============================================================================
// Config / Metrics / Handle
// ============================================================================

/// `rr_server_thread` 配置。每个 RR topic 一份独立 config（topic 串不同）。
#[derive(Clone, Debug)]
pub struct RrServerThreadConfig {
    /// iceoryx2 service topic 串。语义上一个 topic 一个 [`Iceoryx2RrServer`]
    /// 实例 + 一个独立线程。仅作 doc / metrics / tracing label；实际
    /// service create 由调用方传的 `server_factory` closure 内部完成
    /// （sync `rr_server` 工厂同款；详 A1
    /// `crates/robot-s0/src/rr_server_thread.rs` 顶部说明）。
    pub topic: String,
    /// 主循环 idle sleep（纳秒）。默认 500µs（2 kHz），跟 A1
    /// `crates/robot-s0/src/rr_server_thread.rs` 同款。
    pub poll_interval_ns: u64,
    /// 同时 in-flight 的 active request 上限。**B4 (2/3-a) 真 handler 期
    /// 仍未生效**（同步 reply 不入 pending 队列）；保留是为后续真 backpressure
    /// 引入 pending VecDeque 时**不**改 binary 调用点。
    pub max_in_flight: usize,
}

impl Default for RrServerThreadConfig {
    fn default() -> Self {
        Self {
            topic: String::new(),
            poll_interval_ns: 500_000,
            max_in_flight: 64,
        }
    }
}

/// `rr_server_thread` 运行期 metrics（每 topic 一份独立计数）。
#[derive(Default, Debug)]
pub struct RrServerMetrics {
    /// `server.try_recv()` 成功收到的 wire request 数（含 decode 失败那条
    /// 的入口计数）。
    pub requests_received: AtomicU64,
    /// 成功 `active.send_response()` 的 response 数。
    pub responses_sent: AtomicU64,
    /// `active.send_response` 内部 IO 失败的次数。
    pub send_response_errors: AtomicU64,
    /// FlatBuffers root verifier 失败 / 必填字段缺失：reply InvalidArgument。
    pub decode_errors: AtomicU64,
    /// handler closure 实际被调用的次数（`requests_received`
    /// `-` `decode_errors`）。`responses_sent` 仍包含 decode 失败那条 reply，
    /// `handler_replies` 单列让运维一眼看出"真 handler 路径"和"runtime 直
    /// 接拒"的比例。
    pub handler_replies: AtomicU64,
}

/// `rr_server_thread` 句柄。caller 通过 [`Self::request_stop`] +
/// [`Self::join`] 关停。
pub struct RrServerThreadHandle {
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    metrics: Arc<RrServerMetrics>,
    topic: String,
}

impl RrServerThreadHandle {
    /// 取 metrics `Arc`（线程内部写、外部读）。
    #[must_use]
    pub fn metrics(&self) -> Arc<RrServerMetrics> {
        Arc::clone(&self.metrics)
    }

    /// 取 topic 串（diagnostic / log）。
    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// 通知主循环退出（下一轮 stop check 命中即跳出）。
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// 等线程退出。
    ///
    /// # Errors
    ///
    /// 线程内部 panic → `Err(Box<dyn Any + Send>)`。
    pub fn join(self) -> std::thread::Result<()> {
        self.join.join()
    }
}

/// S1 4 个 RR topic 的 handle 聚合，方便 binary 端一次性 `request_stop` /
/// `join` 全部 RR server 线程。
///
/// 字段顺序 = spawn 顺序 = stop / join 顺序，方便 audit log 对账。
pub struct S1RrServerHandles {
    pub task_invoke: RrServerThreadHandle,
    pub task_cancel: RrServerThreadHandle,
    pub task_status: RrServerThreadHandle,
    pub clear_error: RrServerThreadHandle,
}

impl S1RrServerHandles {
    /// 给 4 个线程都发停止信号（不等 join）。
    pub fn request_stop_all(&self) {
        self.task_invoke.request_stop();
        self.task_cancel.request_stop();
        self.task_status.request_stop();
        self.clear_error.request_stop();
    }

    /// 阻塞等所有 4 个线程退出。**不**早返：即使前一个 join 返 panic
    /// 也继续 join 剩下的，最后才返第一个 `Err`。
    ///
    /// # Errors
    ///
    /// 任一线程内部 panic → `Err(Box<dyn Any + Send>)`（首个 panic 优先）。
    pub fn join_all(self) -> std::thread::Result<()> {
        // 全部 join，**不**短路 —— 必须保证 4 个线程都 join 完，避免
        // 剩余线程悬挂到 process exit。
        let r1 = self.task_invoke.join();
        let r2 = self.task_cancel.join();
        let r3 = self.task_status.join();
        let r4 = self.clear_error.join();
        r1.and(r2).and(r3).and(r4)
    }
}

// ============================================================================
// 4 个 typed spawn fn（per RR topic）
// ============================================================================

/// 起 `task_invoke` RR server 线程（监听 `/<robot>/control/task_invoke`）。
///
/// `server_factory` closure 在新线程内执行（iceoryx2 RR server `!Send`，
/// 必须在 owner 线程内构造；同 A1 `spawn_rr_server_thread` factory 模式）。
///
/// `handler` closure 接每条已通过 fbs root verifier 的 wire payload bytes，
/// 返编完的 response bytes。**B4 (2/3-a)** 真 handler 由
/// [`crate::rr_server::handler::RealHandlerCtx::invoke_handler`] 提供，
/// 内部调 `Coordinator::submit` + spawn `events_drain_thread` + register
/// 入 `EventsDrainRegistry` + 编 `InvokeTaskResponse` body bytes。
///
/// # Errors
///
/// - `server_factory` panic（factory 起 iceoryx2 service 失败 / port 耗
///   尽等）。
/// - `thread::Builder::spawn` 失败（极罕见）。
pub fn spawn_task_invoke_server<F, H>(
    cfg: RrServerThreadConfig,
    server_factory: F,
    handler: H,
) -> std::io::Result<RrServerThreadHandle>
where
    F: FnOnce() -> Iceoryx2RrServer<markers::InvokeTaskRequest, markers::InvokeTaskResponse>
        + Send
        + 'static,
    H: Fn(&[u8]) -> Vec<u8> + Send + 'static,
{
    spawn_typed(
        "s1_rr_task_invoke",
        cfg,
        server_factory,
        handler,
        invoke_decode_error_reply,
    )
}

/// 起 `task_cancel` RR server 线程（监听 `/<robot>/control/task_cancel`）。
///
/// **B4 (2/3-a)** 真 handler：
/// [`crate::rr_server::handler::RealHandlerCtx::cancel_handler`] 内部调
/// `Coordinator::cancel`。
///
/// # Errors
///
/// 同 [`spawn_task_invoke_server`]。
pub fn spawn_task_cancel_server<F, H>(
    cfg: RrServerThreadConfig,
    server_factory: F,
    handler: H,
) -> std::io::Result<RrServerThreadHandle>
where
    F: FnOnce() -> Iceoryx2RrServer<markers::CancelTaskRequest, markers::CancelTaskResponse>
        + Send
        + 'static,
    H: Fn(&[u8]) -> Vec<u8> + Send + 'static,
{
    spawn_typed(
        "s1_rr_task_cancel",
        cfg,
        server_factory,
        handler,
        cancel_decode_error_reply,
    )
}

/// 起 `task_status` RR server 线程（监听 `/<robot>/control/task_status`）。
///
/// **B4 (2/3-a)** 真 handler：
/// [`crate::rr_server::handler::RealHandlerCtx::status_handler`] 内部调
/// `Coordinator::status`。
///
/// # Errors
///
/// 同 [`spawn_task_invoke_server`]。
pub fn spawn_task_status_server<F, H>(
    cfg: RrServerThreadConfig,
    server_factory: F,
    handler: H,
) -> std::io::Result<RrServerThreadHandle>
where
    F: FnOnce() -> Iceoryx2RrServer<markers::GetTaskStatusRequest, markers::GetTaskStatusResponse>
        + Send
        + 'static,
    H: Fn(&[u8]) -> Vec<u8> + Send + 'static,
{
    spawn_typed(
        "s1_rr_task_status",
        cfg,
        server_factory,
        handler,
        status_decode_error_reply,
    )
}

/// 起 `clear_error` RR server 线程（监听 `/<robot>/control/clear_error`）。
///
/// **B4 (2/3-a)** 真 handler：
/// [`crate::rr_server::handler::RealHandlerCtx::clear_error_handler`] 内
/// 部调 `Coordinator::clear_error`。
///
/// # Errors
///
/// 同 [`spawn_task_invoke_server`]。
pub fn spawn_clear_error_server<F, H>(
    cfg: RrServerThreadConfig,
    server_factory: F,
    handler: H,
) -> std::io::Result<RrServerThreadHandle>
where
    F: FnOnce() -> Iceoryx2RrServer<markers::ClearErrorRequest, markers::ClearErrorResponse>
        + Send
        + 'static,
    H: Fn(&[u8]) -> Vec<u8> + Send + 'static,
{
    spawn_typed(
        "s1_rr_clear_error",
        cfg,
        server_factory,
        handler,
        clear_error_decode_error_reply,
    )
}

// ============================================================================
// 通用 spawn / 主循环（generic over (Req, Resp) marker + handler closure）
// ============================================================================

/// `decode_error_reply` 是静态 fn pointer：fbs root verifier 失败时**不**进
/// handler，由 runtime 直接构造 InvalidArgument response。每 topic 一份独立
/// fn pointer（4 个 [`spawn_*_server`](spawn_task_invoke_server) 入口各自传）。
type DecodeErrorReply = fn(&[u8]) -> Vec<u8>;

fn spawn_typed<Req, Resp, F, H>(
    thread_name: &'static str,
    cfg: RrServerThreadConfig,
    server_factory: F,
    handler: H,
    decode_error_reply: DecodeErrorReply,
) -> std::io::Result<RrServerThreadHandle>
where
    Req: BodyMarker,
    Resp: BodyMarker,
    F: FnOnce() -> Iceoryx2RrServer<Req, Resp> + Send + 'static,
    H: Fn(&[u8]) -> Vec<u8> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let metrics = Arc::new(RrServerMetrics::default());

    let stop_clone = Arc::clone(&stop);
    let metrics_clone = Arc::clone(&metrics);
    let topic_owned = cfg.topic.clone();

    // ready signal：spawn 返前确认 server_factory 成功执行（factory panic
    // / iceoryx2 错都在这里捕获）。同 A1 `spawn_rr_server_thread` 模式。
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

    let join = thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            let server = server_factory();
            let _ = ready_tx.send(());
            drop(ready_tx);
            run_handler_loop(
                &cfg,
                &server,
                &handler,
                decode_error_reply,
                &stop_clone,
                &metrics_clone,
            );
        })?;

    if ready_rx.recv().is_err() {
        return Err(std::io::Error::other(format!(
            "rr_server_thread {thread_name}: server_factory panicked before signalling ready"
        )));
    }

    Ok(RrServerThreadHandle {
        join,
        stop,
        metrics,
        topic: topic_owned,
    })
}

fn run_handler_loop<Req, Resp, H>(
    cfg: &RrServerThreadConfig,
    server: &Iceoryx2RrServer<Req, Resp>,
    handler: &H,
    decode_error_reply: DecodeErrorReply,
    stop: &AtomicBool,
    metrics: &RrServerMetrics,
) where
    Req: BodyMarker,
    Resp: BodyMarker,
    H: Fn(&[u8]) -> Vec<u8>,
{
    while !stop.load(Ordering::Acquire) {
        // drain 所有 pending wire request：循环 try_recv 直到 Ok(None) /
        // Err。同步 reply，无 pending 队列。
        loop {
            match server.try_recv() {
                Ok(Some(active)) => {
                    metrics.requests_received.fetch_add(1, Ordering::Relaxed);
                    handle_active(active, handler, decode_error_reply, cfg, metrics);
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::error!(
                        topic = %cfg.topic,
                        error = ?e,
                        "rr_server_thread: server.try_recv failed"
                    );
                    break;
                }
            }
        }
        thread::sleep(Duration::from_nanos(cfg.poll_interval_ns));
    }
}

fn handle_active<Req, Resp, H>(
    active: Iceoryx2ActiveRequest<Req, Resp>,
    handler: &H,
    decode_error_reply: DecodeErrorReply,
    cfg: &RrServerThreadConfig,
    metrics: &RrServerMetrics,
) where
    Req: BodyMarker,
    Resp: BodyMarker,
    H: Fn(&[u8]) -> Vec<u8>,
{
    let payload = active.payload();
    // 校验 fbs root：失败 → decode_error_reply（status = InvalidArgument）；
    // 成功 → handler closure。
    let bytes = match flatbuffers::root::<Req::Root<'_>>(payload) {
        Ok(_view) => {
            metrics.handler_replies.fetch_add(1, Ordering::Relaxed);
            handler(payload)
        }
        Err(e) => {
            tracing::warn!(
                topic = %cfg.topic,
                error = ?e,
                "rr_server_thread: wire decode failed; replying InvalidArgument"
            );
            metrics.decode_errors.fetch_add(1, Ordering::Relaxed);
            decode_error_reply(payload)
        }
    };
    match active.send_response(&bytes) {
        Ok(()) => {
            metrics.responses_sent.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            tracing::error!(
                topic = %cfg.topic,
                error = ?e,
                "rr_server_thread: send_response failed"
            );
            metrics.send_response_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    //! Round-trip lib unit tests（runtime 层）：用 `rr_channel`（同进程
    //! client + server pair）跑端到端 wire request → handler 产 reply →
    //! response 解析。锁住 4 个 topic 的生命周期 (spawn / request_stop / join)
    //! + decode-error 路径 + handler 回填。
    //!
    //! 真 handler 接 [`crate::coordinator::Coordinator`] 的端到端测试在
    //! [`crate::rr_server::handler::tests`] 里覆盖；本模块只测 runtime 路
    //! 径（用 stub handler closure）。

    use super::*;
    use iceoryx2::prelude::NodeBuilder;
    use iceoryx2::service::ipc::Service as IxService;
    use robot_msg::build_helpers::{
        build_cancel_task_request, build_clear_error_request, build_get_task_status_request,
        build_invoke_task_request, build_invoke_task_response,
    };
    use robot_msg::msg::{
        ActionKindFb, CancelTaskResponse as FbCancelResp, ClearErrorResponse as FbClearResp,
        GetTaskStatusResponse as FbStatusResp, InvokeTaskResponse as FbInvokeResp, RpcRole,
    };
    use robot_transport::iceoryx2::request_response::{
        Iceoryx2PendingResponse, rr_client, rr_server,
    };

    fn invoke_req(request_id: &str) -> Vec<u8> {
        build_invoke_task_request(
            request_id,
            "idem-001",
            "rt_test",
            RpcRole::Maintainer,
            "rr_server runtime test",
            ActionKindFb::DwellMs,
            /* deadline_ns */ 0,
            /* priority */ 0,
            /* payload_schema_id */ 0,
            /* payload */ &[],
            /* stamp_ns */ 0,
        )
    }
    fn cancel_req(request_id: &str, action_id: &str) -> Vec<u8> {
        build_cancel_task_request(
            request_id,
            "idem-001",
            "rt_test",
            RpcRole::Maintainer,
            "rr_server runtime test",
            action_id,
            0,
            0,
        )
    }
    fn status_req(request_id: &str, action_id: &str) -> Vec<u8> {
        build_get_task_status_request(
            request_id,
            "idem-001",
            "rt_test",
            RpcRole::Viewer,
            "rr_server runtime test",
            action_id,
            0,
            0,
        )
    }
    fn clear_req(request_id: &str) -> Vec<u8> {
        build_clear_error_request(
            request_id,
            "idem-001",
            "rt_test",
            RpcRole::Maintainer,
            "rr_server runtime test",
            0,
            0,
        )
    }

    fn fresh_topic(suffix: &str) -> String {
        use std::sync::atomic::AtomicU32;
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        format!("/s1_rr_rt_{}_{}_{suffix}", std::process::id(), seq)
    }

    fn wait_for_response<R: BodyMarker>(
        pending: &Iceoryx2PendingResponse<R>,
        timeout: Duration,
    ) -> Vec<u8> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match pending.try_recv() {
                Ok(Some(bytes)) => return bytes,
                Ok(None) => {}
                Err(e) => panic!("pending.try_recv failed: {e:?}"),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "wait_for_response timed out after {timeout:?}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn make_client<Req: BodyMarker, Resp: BodyMarker>(
        topic: &str,
    ) -> robot_transport::iceoryx2::request_response::Iceoryx2RrClient<Req, Resp> {
        let test_node: &'static _ =
            Box::leak(Box::new(NodeBuilder::new().create::<IxService>().unwrap()));
        rr_client::<Req, Resp>(test_node, topic).expect("rr_client create")
    }

    fn make_server_factory<Req: BodyMarker, Resp: BodyMarker>(
        topic: String,
    ) -> impl FnOnce() -> Iceoryx2RrServer<Req, Resp> + Send + 'static {
        move || {
            let server_node = NodeBuilder::new().create::<IxService>().unwrap();
            let server = rr_server::<Req, Resp>(&server_node, &topic).expect("rr_server create");
            Box::leak(Box::new(server_node));
            server
        }
    }

    fn cfg_for(topic: &str) -> RrServerThreadConfig {
        RrServerThreadConfig {
            topic: topic.to_owned(),
            poll_interval_ns: 100_000,
            max_in_flight: 64,
        }
    }

    /// stub handler：echo `request_id` + status = `Ok` + accepted_at_ns = 7。
    /// 用于锁住 runtime 把 verified payload 透传给 handler 的路径。
    fn invoke_stub_handler(payload: &[u8]) -> Vec<u8> {
        let req = flatbuffers::root::<robot_msg::msg::InvokeTaskRequest>(payload).unwrap();
        let request_id = req.request_id().unwrap_or_default();
        build_invoke_task_response(request_id, ActionRpcStatus::Ok, "", request_id, 7)
    }

    #[test]
    fn invoke_runtime_round_trip_invokes_handler() {
        let topic = fresh_topic("invoke");
        let client = make_client::<markers::InvokeTaskRequest, markers::InvokeTaskResponse>(&topic);
        let factory = make_server_factory::<markers::InvokeTaskRequest, markers::InvokeTaskResponse>(
            topic.clone(),
        );

        let handle =
            spawn_task_invoke_server(cfg_for(&topic), factory, invoke_stub_handler).unwrap();

        let req_bytes = invoke_req("inv-rt-001");
        let pending = client.send(&req_bytes).unwrap();
        let bytes = wait_for_response(&pending, Duration::from_secs(2));
        let resp = flatbuffers::root::<FbInvokeResp>(&bytes).unwrap();
        assert_eq!(resp.request_id().unwrap(), "inv-rt-001");
        assert_eq!(resp.status(), ActionRpcStatus::Ok);
        assert_eq!(resp.error_msg().unwrap(), "");
        assert_eq!(resp.action_id().unwrap(), "inv-rt-001");
        assert_eq!(resp.accepted_at_ns(), 7);

        let metrics = handle.metrics();
        assert_eq!(metrics.requests_received.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.responses_sent.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.handler_replies.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.decode_errors.load(Ordering::Relaxed), 0);
        assert_eq!(handle.topic(), topic);

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn invoke_decode_error_returns_invalid_argument_without_handler_call() {
        // 验 runtime 在 fbs root verifier 失败时**不**调 handler；改走静态
        // decode_error_reply。stub handler 返一个明显可识别的 sentinel
        // accepted_at_ns，拿到 sentinel 就说明 handler 被错误调用了。
        let topic = fresh_topic("decode_err");
        let client = make_client::<markers::InvokeTaskRequest, markers::InvokeTaskResponse>(&topic);
        let factory = make_server_factory::<markers::InvokeTaskRequest, markers::InvokeTaskResponse>(
            topic.clone(),
        );
        let sentinel_handler = |payload: &[u8]| -> Vec<u8> {
            // 不应被调到（runtime decode_error_reply 拦截）。
            let req = flatbuffers::root::<robot_msg::msg::InvokeTaskRequest>(payload).unwrap();
            let request_id = req.request_id().unwrap_or_default();
            build_invoke_task_response(
                request_id,
                ActionRpcStatus::Ok,
                "",
                request_id,
                /* sentinel */ 999,
            )
        };

        let handle = spawn_task_invoke_server(cfg_for(&topic), factory, sentinel_handler).unwrap();

        // 故意发非 fbs bytes（4 个 dummy 字节，过不了 root verifier）。
        let bad_bytes: Vec<u8> = vec![0xAA, 0xBB, 0xCC, 0xDD];
        let pending = client.send(&bad_bytes).unwrap();
        let bytes = wait_for_response(&pending, Duration::from_secs(2));
        let resp = flatbuffers::root::<FbInvokeResp>(&bytes).unwrap();
        assert_eq!(resp.request_id().unwrap(), "");
        assert_eq!(resp.status(), ActionRpcStatus::InvalidArgument);
        assert_eq!(resp.error_msg().unwrap(), DECODE_ERROR_MSG);
        // sentinel 验：handler 没被调用。
        assert_ne!(resp.accepted_at_ns(), 999);

        let metrics = handle.metrics();
        assert_eq!(metrics.requests_received.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.responses_sent.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.handler_replies.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.decode_errors.load(Ordering::Relaxed), 1);

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn cancel_runtime_round_trip_invokes_handler() {
        let topic = fresh_topic("cancel");
        let client = make_client::<markers::CancelTaskRequest, markers::CancelTaskResponse>(&topic);
        let factory = make_server_factory::<markers::CancelTaskRequest, markers::CancelTaskResponse>(
            topic.clone(),
        );
        let handler = |payload: &[u8]| -> Vec<u8> {
            use robot_msg::build_helpers::build_cancel_task_response;
            let req = flatbuffers::root::<robot_msg::msg::CancelTaskRequest>(payload).unwrap();
            build_cancel_task_response(
                req.request_id().unwrap_or_default(),
                ActionRpcStatus::Ok,
                "",
                req.action_id().unwrap_or_default(),
                42,
            )
        };

        let handle = spawn_task_cancel_server(cfg_for(&topic), factory, handler).unwrap();

        let req_bytes = cancel_req("cnl-rt-001", "act-uuid-7");
        let pending = client.send(&req_bytes).unwrap();
        let bytes = wait_for_response(&pending, Duration::from_secs(2));
        let resp = flatbuffers::root::<FbCancelResp>(&bytes).unwrap();
        assert_eq!(resp.request_id().unwrap(), "cnl-rt-001");
        assert_eq!(resp.action_id().unwrap(), "act-uuid-7");
        assert_eq!(resp.status(), ActionRpcStatus::Ok);
        assert_eq!(resp.cancel_at_ns(), 42);

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn status_runtime_round_trip_invokes_handler() {
        let topic = fresh_topic("status");
        let client =
            make_client::<markers::GetTaskStatusRequest, markers::GetTaskStatusResponse>(&topic);
        let factory = make_server_factory::<
            markers::GetTaskStatusRequest,
            markers::GetTaskStatusResponse,
        >(topic.clone());
        let handler = |payload: &[u8]| -> Vec<u8> {
            use robot_msg::build_helpers::build_get_task_status_response;
            use robot_msg::msg::TaskStateFb;
            let req = flatbuffers::root::<robot_msg::msg::GetTaskStatusRequest>(payload).unwrap();
            build_get_task_status_response(
                req.request_id().unwrap_or_default(),
                ActionRpcStatus::Ok,
                "",
                req.action_id().unwrap_or_default(),
                TaskStateFb::Executing,
                0.5,
                None,
                None,
            )
        };

        let handle = spawn_task_status_server(cfg_for(&topic), factory, handler).unwrap();

        let req_bytes = status_req("sta-rt-001", "act-uuid-9");
        let pending = client.send(&req_bytes).unwrap();
        let bytes = wait_for_response(&pending, Duration::from_secs(2));
        let resp = flatbuffers::root::<FbStatusResp>(&bytes).unwrap();
        assert_eq!(resp.request_id().unwrap(), "sta-rt-001");
        assert_eq!(resp.action_id().unwrap(), "act-uuid-9");
        assert_eq!(resp.status(), ActionRpcStatus::Ok);
        assert!((resp.progress() - 0.5).abs() < f32::EPSILON);

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn clear_error_runtime_round_trip_invokes_handler() {
        let topic = fresh_topic("clearerr");
        let client = make_client::<markers::ClearErrorRequest, markers::ClearErrorResponse>(&topic);
        let factory = make_server_factory::<markers::ClearErrorRequest, markers::ClearErrorResponse>(
            topic.clone(),
        );
        let handler = |payload: &[u8]| -> Vec<u8> {
            use robot_msg::build_helpers::build_clear_error_response;
            let req = flatbuffers::root::<robot_msg::msg::ClearErrorRequest>(payload).unwrap();
            build_clear_error_response(
                req.request_id().unwrap_or_default(),
                ActionRpcStatus::Ok,
                "",
                /* cleared_at_ns */ 13,
            )
        };

        let handle = spawn_clear_error_server(cfg_for(&topic), factory, handler).unwrap();

        let req_bytes = clear_req("clr-rt-001");
        let pending = client.send(&req_bytes).unwrap();
        let bytes = wait_for_response(&pending, Duration::from_secs(2));
        let resp = flatbuffers::root::<FbClearResp>(&bytes).unwrap();
        assert_eq!(resp.request_id().unwrap(), "clr-rt-001");
        assert_eq!(resp.status(), ActionRpcStatus::Ok);
        assert_eq!(resp.cleared_at_ns(), 13);

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn idle_request_stop_join_clean_exit() {
        // 锁住"主循环退出不悬挂线程"语义：spawn 起来不发任何 wire request，
        // 直接 request_stop + join 应在 ~poll_interval 内 clean exit。
        let topic = fresh_topic("idle_stop");
        let _client =
            make_client::<markers::InvokeTaskRequest, markers::InvokeTaskResponse>(&topic);
        let factory = make_server_factory::<markers::InvokeTaskRequest, markers::InvokeTaskResponse>(
            topic.clone(),
        );
        let handle =
            spawn_task_invoke_server(cfg_for(&topic), factory, invoke_stub_handler).unwrap();

        let start = std::time::Instant::now();
        handle.request_stop();
        handle.join().unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "join took {elapsed:?}, expected < 500ms"
        );
    }
}
