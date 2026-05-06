//! `rr_server_thread` —— v0.3 B4 (1/3-b) S1 task control plane RR server
//! skeleton。
//!
//! ## 角色
//!
//! 沿 A1-3 (1/3c-1) `crates/robot-s0/src/rr_server_thread.rs` 同款"`rr_server_thread`
//! 只做 iceoryx2 RR I/O"线程模型，但**不**接 coordinator —— B4 (1/3-b)
//! 仅落地：
//!
//! 1. iceoryx2 RR server 起停 / join（生命周期接线 + 主循环退出不悬挂）。
//! 2. 4 个 RR topic 的类型分发（task_invoke / task_cancel / task_status /
//!    clear_error；每条 topic 一个独立线程，monomorphic over `(ReqMarker,
//!    RespMarker)`）。
//! 3. 每个 active request 立刻同步 reply
//!    [`robot_msg::msg::ActionRpcStatus::Unimplemented`] +
//!    `error_msg = "B4 (1/3-b) skeleton; handler lands B4 (2/3)"`
//!    （**不**走 coordinator API，**不**改
//!    [`crate::action::ActionTicket`]，**不**碰
//!    `JointCommand.source_task_id` / `FrameHeader.seq` —— 不破坏 B3 wire
//!    契约 §3.7）。
//!
//! 真 handler（auth / capability / idempotency / Coordinator::submit /
//! Coordinator::cancel / Coordinator::status / Coordinator::clear_error）
//! 落 B4 (2/3) commit；events drain pubsub 落 B4 (1/3-c) commit；
//! 跨进程 integration test 落 B4 (3/3) commit。详
//! [`docs/handoffs/v0.3_b4_outline.md`](../../../docs/handoffs/v0.3_b4_outline.md)
//! §11.1 拆分计划。
//!
//! ## 不在范围（B4 (1/3-b) 边界）
//!
//! - **不**接 [`crate::coordinator::Coordinator`] API：skeleton 反应**不**碰
//!   coordinator state machine。
//! - **不**起 events drain / per-ActionId pubsub publisher（→ B4 (1/3-c)）。
//! - **不**起 robot-rpc gateway 进程（→ B4 (2/3)）。
//! - **不**做 idempotency cache / auth / capability 校验（gateway 端 →
//!   B4 (2/3)）。
//! - **不**接 `tokio` / `tonic`（与 `crates/robot-s1/src/lib.rs` 顶部"不
//!   链接 tokio / tonic"边界对齐；async gateway adapter 在 robot-rpc，**不**
//!   塞进 robot-s1）。
//!
//! ## 线程语义
//!
//! - **非 RT** 普通优先级线程；每个 RR topic 一个独立 `std::thread`，
//!   独立 [`Iceoryx2RrServer`] 单 owner（iceoryx2 service 是
//!   SingleThreaded，server 不跨线程；同 A1-3 模式）。
//! - 主循环：drain `try_recv()` →（每条 active）build skeleton reply →
//!   `send_response` → 继续 drain 直到 `Ok(None)` → sleep
//!   `poll_interval_ns`。
//! - **同步 reply**（skeleton 不 forward 到任何 channel）→ shutdown 路径
//!   极简：循环退出即结束，无 in-flight pending 队列要 abort。真 handler
//!   landing 后引入 channel forward + pending VecDeque + shutdown abort，
//!   届时跟 A1 s0 模式对齐。
//! - 退出靠 `Arc<AtomicBool>` stop signal（沿 A1 同款）。
//! - `s1_daemon` 主循环退出时**逐个** [`RrServerThreadHandle::request_stop`]
//!   + [`RrServerThreadHandle::join`]；所有 RR server 线程都 join 完才返
//!     main —— **不**悬挂线程到 process exit。详 [`S1RrServerHandles`]。
//!
//! ## decode error 路径（skeleton）
//!
//! FlatBuffers root verifier 失败 / 必填字段缺失 → reply
//! `ActionRpcStatus::InvalidArgument` + `error_msg = "wire decode failed"`。
//! 跟 A1 s0 同款：消费 active handle、**不**让 wire request 留在 iceoryx2
//! 队列里悬挂到客户端 deadline 超时。
//!
//! ## 真 handler landing 时的 evolution（B4 (2/3) 接班 reference）
//!
//! - 引入 `(rr_request_tx, rr_response_rx)` channel pair forward 给
//!   coordinator / handler thread；同 A1 s0 `RrChannels` 双向 pattern。
//! - 引入 `VecDeque<PendingRr>` 管理 in-flight active；按
//!   `request_id` 跟 handler response 对账。
//! - 引入 shutdown abort 路径：给 pending 中所有 in-flight active 回
//!   `ActionRpcStatus::Unavailable` + `error_msg = "s1 daemon shutting
//!   down"`（沿 A1 `abort_pending_on_shutdown` 同款，但 status 改
//!   `Unavailable`，跟 outline §7.1 ShuttingDown → Unavailable 映射对
//!   齐）。
//! - 引入 backpressure（`max_in_flight` 满 → `Unavailable` 直拒）。
//!
//! 本 skeleton 的 [`RrServerThreadConfig::max_in_flight`] 字段已声明但
//! **未生效**（skeleton 同步 reply 不入 pending 队列；字段保留是为 B4
//! (2/3) 真 handler 落地时**不**改 binary wiring 的 config 调用点）。

#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use robot_msg::build_helpers::{
    build_cancel_task_response, build_clear_error_response, build_get_task_status_response,
    build_invoke_task_response,
};
use robot_msg::markers;
use robot_msg::markers::BodyMarker;
use robot_msg::msg::{
    ActionRpcStatus, CancelTaskRequest as FbCancelReq, ClearErrorRequest as FbClearErrorReq,
    GetTaskStatusRequest as FbStatusReq, InvokeTaskRequest as FbInvokeReq, TaskStateFb,
};
use robot_transport::iceoryx2::request_response::{Iceoryx2ActiveRequest, Iceoryx2RrServer};

// ============================================================================
// Skeleton reply error message
// ============================================================================

/// skeleton 路径所有 RR topic 共用的 `error_msg`。B4 (2/3) 真 handler 落
/// 地后此常量被 handler 真路径替换；skeleton 期间所有 reply 都带此字串
/// 让 client / audit log 一眼看出当前 daemon 处于 skeleton 形态。
pub const SKELETON_REPLY_ERROR_MSG: &str = "B4 (1/3-b) skeleton; handler lands B4 (2/3)";

const DECODE_ERROR_MSG: &str = "wire decode failed";

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
    /// 同时 in-flight 的 active request 上限。**B4 (1/3-b) skeleton 期
    /// 未生效**（同步 reply 不入 pending 队列）；保留是为 B4 (2/3) 真
    /// handler 引入 pending VecDeque + backpressure 时**不**改 binary
    /// 调用点。
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
    /// `server.try_recv()` 成功收到的 wire request 数（含 decode 失败）。
    pub requests_received: AtomicU64,
    /// 成功 `active.send_response()` 的 response 数。
    pub responses_sent: AtomicU64,
    /// `active.send_response` 内部 IO 失败的次数。
    pub send_response_errors: AtomicU64,
    /// FlatBuffers root verifier 失败 / 必填字段缺失：reply InvalidArgument。
    pub decode_errors: AtomicU64,
    /// skeleton reply 路径的 reply 数（status = `Unimplemented`）。
    /// **`responses_sent` ⊇ `skeleton_replies`**：skeleton 期间几乎所有
    /// reply 都走此路径（除 decode 失败那几条）；本字段单列让 B4 (2/3)
    /// 真 handler land 后能从 metrics 看到"skeleton reply 数 → 0 / 真
    /// reply 数 ↑"过渡。
    pub skeleton_replies: AtomicU64,
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
// spawn API
// ============================================================================

/// 起 `task_invoke` RR server 线程（监听 `/<robot>/control/task_invoke`）。
///
/// `server_factory` closure 在新线程内执行（iceoryx2 RR server `!Send`，
/// 必须在 owner 线程内构造；同 A1 `spawn_rr_server_thread` factory 模式）。
///
/// **B4 (1/3-b) 实现**：每条 wire request 同步 reply
/// [`InvokeTaskResponse`] with `status =
/// ActionRpcStatus::Unimplemented` + `action_id = ""` +
/// `accepted_at_ns = 0`。
///
/// # Errors
///
/// - `server_factory` panic（factory 起 iceoryx2 service 失败 / port 耗
///   尽等）。
/// - `thread::Builder::spawn` 失败（极罕见）。
///
/// [`InvokeTaskResponse`]: robot_msg::markers::InvokeTaskResponse
pub fn spawn_task_invoke_server<F>(
    cfg: RrServerThreadConfig,
    server_factory: F,
) -> std::io::Result<RrServerThreadHandle>
where
    F: FnOnce() -> Iceoryx2RrServer<markers::InvokeTaskRequest, markers::InvokeTaskResponse>
        + Send
        + 'static,
{
    spawn_typed(
        "s1_rr_task_invoke",
        cfg,
        server_factory,
        invoke_skeleton_reply,
        invoke_decode_error_reply,
    )
}

/// 起 `task_cancel` RR server 线程（监听 `/<robot>/control/task_cancel`）。
///
/// **B4 (1/3-b) 实现**：每条 wire request 同步 reply
/// [`CancelTaskResponse`] with `status =
/// ActionRpcStatus::Unimplemented` + `action_id = ""` +
/// `cancel_at_ns = 0`。
///
/// # Errors
///
/// 同 [`spawn_task_invoke_server`]。
///
/// [`CancelTaskResponse`]: robot_msg::markers::CancelTaskResponse
pub fn spawn_task_cancel_server<F>(
    cfg: RrServerThreadConfig,
    server_factory: F,
) -> std::io::Result<RrServerThreadHandle>
where
    F: FnOnce() -> Iceoryx2RrServer<markers::CancelTaskRequest, markers::CancelTaskResponse>
        + Send
        + 'static,
{
    spawn_typed(
        "s1_rr_task_cancel",
        cfg,
        server_factory,
        cancel_skeleton_reply,
        cancel_decode_error_reply,
    )
}

/// 起 `task_status` RR server 线程（监听 `/<robot>/control/task_status`）。
///
/// **B4 (1/3-b) 实现**：每条 wire request 同步 reply
/// [`GetTaskStatusResponse`] with `status =
/// ActionRpcStatus::Unimplemented` + `state = TaskStateFb::Unknown` +
/// 全部 progress / outcome / failure 都空。
///
/// # Errors
///
/// 同 [`spawn_task_invoke_server`]。
///
/// [`GetTaskStatusResponse`]: robot_msg::markers::GetTaskStatusResponse
pub fn spawn_task_status_server<F>(
    cfg: RrServerThreadConfig,
    server_factory: F,
) -> std::io::Result<RrServerThreadHandle>
where
    F: FnOnce() -> Iceoryx2RrServer<markers::GetTaskStatusRequest, markers::GetTaskStatusResponse>
        + Send
        + 'static,
{
    spawn_typed(
        "s1_rr_task_status",
        cfg,
        server_factory,
        status_skeleton_reply,
        status_decode_error_reply,
    )
}

/// 起 `clear_error` RR server 线程（监听 `/<robot>/control/clear_error`）。
///
/// **B4 (1/3-b) 实现**：每条 wire request 同步 reply
/// [`ClearErrorResponse`] with `status =
/// ActionRpcStatus::Unimplemented` + `cleared_at_ns = 0`。
///
/// # Errors
///
/// 同 [`spawn_task_invoke_server`]。
///
/// [`ClearErrorResponse`]: robot_msg::markers::ClearErrorResponse
pub fn spawn_clear_error_server<F>(
    cfg: RrServerThreadConfig,
    server_factory: F,
) -> std::io::Result<RrServerThreadHandle>
where
    F: FnOnce() -> Iceoryx2RrServer<markers::ClearErrorRequest, markers::ClearErrorResponse>
        + Send
        + 'static,
{
    spawn_typed(
        "s1_rr_clear_error",
        cfg,
        server_factory,
        clear_error_skeleton_reply,
        clear_error_decode_error_reply,
    )
}

// ============================================================================
// 通用 spawn / 主循环（generic over (Req, Resp) marker）
// ============================================================================

/// `(payload bytes) -> reply bytes`。skeleton / decode-error 路径分别有
/// 各自实现；真 handler 路径（B4 (2/3)）不通过此 fn pointer。
type ReplyBuilder = fn(&[u8]) -> Vec<u8>;

fn spawn_typed<Req, Resp, F>(
    thread_name: &'static str,
    cfg: RrServerThreadConfig,
    server_factory: F,
    skeleton_reply: ReplyBuilder,
    decode_error_reply: ReplyBuilder,
) -> std::io::Result<RrServerThreadHandle>
where
    Req: BodyMarker,
    Resp: BodyMarker,
    F: FnOnce() -> Iceoryx2RrServer<Req, Resp> + Send + 'static,
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
            run_skeleton_loop(
                &cfg,
                &server,
                skeleton_reply,
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

fn run_skeleton_loop<Req: BodyMarker, Resp: BodyMarker>(
    cfg: &RrServerThreadConfig,
    server: &Iceoryx2RrServer<Req, Resp>,
    skeleton_reply: ReplyBuilder,
    decode_error_reply: ReplyBuilder,
    stop: &AtomicBool,
    metrics: &RrServerMetrics,
) {
    while !stop.load(Ordering::Acquire) {
        // drain 所有 pending wire request：循环 try_recv 直到 Ok(None) /
        // Err。skeleton 同步 reply，无 pending 队列。
        loop {
            match server.try_recv() {
                Ok(Some(active)) => {
                    metrics.requests_received.fetch_add(1, Ordering::Relaxed);
                    handle_active(active, skeleton_reply, decode_error_reply, cfg, metrics);
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

fn handle_active<Req: BodyMarker, Resp: BodyMarker>(
    active: Iceoryx2ActiveRequest<Req, Resp>,
    skeleton_reply: ReplyBuilder,
    decode_error_reply: ReplyBuilder,
    cfg: &RrServerThreadConfig,
    metrics: &RrServerMetrics,
) {
    let payload = active.payload();
    // 校验 fbs root：失败 → decode_error_reply（status = InvalidArgument）。
    let bytes = match flatbuffers::root::<Req::Root<'_>>(payload) {
        Ok(_view) => {
            metrics.skeleton_replies.fetch_add(1, Ordering::Relaxed);
            skeleton_reply(payload)
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
// per-topic skeleton / decode-error reply builders
// ============================================================================
//
// 每 topic 一对 reply builder：
//   - `*_skeleton_reply`：fbs decode 成功 → echo `request_id` + status =
//     `Unimplemented` + `error_msg = SKELETON_REPLY_ERROR_MSG`。
//   - `*_decode_error_reply`：fbs decode 失败 → `request_id = ""` +
//     status = `InvalidArgument` + `error_msg = DECODE_ERROR_MSG`。
//
// 两类都是 `fn(&[u8]) -> Vec<u8>` 静态 fn pointer，零捕获 / 零 alloc 开
// 销（除 reply bytes 自身的 FlatBufferBuilder）。

fn invoke_skeleton_reply(payload: &[u8]) -> Vec<u8> {
    let request_id = flatbuffers::root::<FbInvokeReq>(payload)
        .ok()
        .and_then(|v| v.request_id().map(str::to_owned))
        .unwrap_or_default();
    build_invoke_task_response(
        &request_id,
        ActionRpcStatus::Unimplemented,
        SKELETON_REPLY_ERROR_MSG,
        "",
        0,
    )
}

fn invoke_decode_error_reply(_payload: &[u8]) -> Vec<u8> {
    build_invoke_task_response(
        "",
        ActionRpcStatus::InvalidArgument,
        DECODE_ERROR_MSG,
        "",
        0,
    )
}

fn cancel_skeleton_reply(payload: &[u8]) -> Vec<u8> {
    let view = flatbuffers::root::<FbCancelReq>(payload).ok();
    let request_id = view
        .as_ref()
        .and_then(|v| v.request_id().map(str::to_owned))
        .unwrap_or_default();
    let action_id = view
        .as_ref()
        .and_then(|v| v.action_id().map(str::to_owned))
        .unwrap_or_default();
    build_cancel_task_response(
        &request_id,
        ActionRpcStatus::Unimplemented,
        SKELETON_REPLY_ERROR_MSG,
        &action_id,
        0,
    )
}

fn cancel_decode_error_reply(_payload: &[u8]) -> Vec<u8> {
    build_cancel_task_response(
        "",
        ActionRpcStatus::InvalidArgument,
        DECODE_ERROR_MSG,
        "",
        0,
    )
}

fn status_skeleton_reply(payload: &[u8]) -> Vec<u8> {
    let view = flatbuffers::root::<FbStatusReq>(payload).ok();
    let request_id = view
        .as_ref()
        .and_then(|v| v.request_id().map(str::to_owned))
        .unwrap_or_default();
    let action_id = view
        .as_ref()
        .and_then(|v| v.action_id().map(str::to_owned))
        .unwrap_or_default();
    build_get_task_status_response(
        &request_id,
        ActionRpcStatus::Unimplemented,
        SKELETON_REPLY_ERROR_MSG,
        &action_id,
        TaskStateFb::Unknown,
        0.0,
        None,
        None,
    )
}

fn status_decode_error_reply(_payload: &[u8]) -> Vec<u8> {
    build_get_task_status_response(
        "",
        ActionRpcStatus::InvalidArgument,
        DECODE_ERROR_MSG,
        "",
        TaskStateFb::Unknown,
        0.0,
        None,
        None,
    )
}

fn clear_error_skeleton_reply(payload: &[u8]) -> Vec<u8> {
    let request_id = flatbuffers::root::<FbClearErrorReq>(payload)
        .ok()
        .and_then(|v| v.request_id().map(str::to_owned))
        .unwrap_or_default();
    build_clear_error_response(
        &request_id,
        ActionRpcStatus::Unimplemented,
        SKELETON_REPLY_ERROR_MSG,
        0,
    )
}

fn clear_error_decode_error_reply(_payload: &[u8]) -> Vec<u8> {
    build_clear_error_response("", ActionRpcStatus::InvalidArgument, DECODE_ERROR_MSG, 0)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    //! Round-trip lib unit tests:
    //!
    //! 用 `rr_channel`（同进程 client + server pair）跑端到端 wire
    //! request → skeleton reply → response 解析；锁住 4 个 topic 的
    //! lifecycle (spawn / request_stop / join) 跟 reply 字段值（status =
    //! `Unimplemented` + `error_msg = SKELETON_REPLY_ERROR_MSG` +
    //! `request_id` echo）。
    //!
    //! 跨进程 integration test 留 B4 (3/3) commit（`crates/robot-rpc/tests/...`
    //! task_rpc_e2e.rs，沿 A1-3 (3/3) 跨进程 spawn s1_daemon binary 同款
    //! 模式）。

    use super::*;
    use iceoryx2::prelude::NodeBuilder;
    use iceoryx2::service::ipc::Service as IxService;
    use robot_msg::build_helpers::{
        build_cancel_task_request, build_clear_error_request, build_get_task_status_request,
        build_invoke_task_request,
    };
    use robot_msg::msg::{
        ActionKindFb, CancelTaskResponse as FbCancelResp, ClearErrorResponse as FbClearResp,
        GetTaskStatusResponse as FbStatusResp, InvokeTaskResponse as FbInvokeResp, RpcRole,
    };
    use robot_transport::iceoryx2::request_response::{
        Iceoryx2PendingResponse, rr_client, rr_server,
    };

    // skeleton 测试统一用的 stub 入参（除 request_id / action_id 外所有字段
    // 不参与 skeleton 行为；real handler 在 B4 (2/3) 落地时由 gateway 端
    // 真填）。
    fn invoke_req(request_id: &str) -> Vec<u8> {
        build_invoke_task_request(
            request_id,
            "idem-001",
            "skeleton_test",
            RpcRole::Maintainer,
            "rr_server skeleton test",
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
            "skeleton_test",
            RpcRole::Maintainer,
            "rr_server skeleton test",
            action_id,
            0,
            0,
        )
    }
    fn status_req(request_id: &str, action_id: &str) -> Vec<u8> {
        build_get_task_status_request(
            request_id,
            "idem-001",
            "skeleton_test",
            RpcRole::Viewer,
            "rr_server skeleton test",
            action_id,
            0,
            0,
        )
    }
    fn clear_req(request_id: &str) -> Vec<u8> {
        build_clear_error_request(
            request_id,
            "idem-001",
            "skeleton_test",
            RpcRole::Maintainer,
            "rr_server skeleton test",
            0,
            0,
        )
    }

    fn fresh_topic(suffix: &str) -> String {
        use std::sync::atomic::AtomicU32;
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        format!("/s1_rr_skel_{}_{}_{suffix}", std::process::id(), seq)
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

    /// 测试主线程持的 client（独立 Node + leak），跟 server_factory 内
    /// 独立 Node + leak 的 server 通过 iceoryx2 service name 串通。
    /// 沿 A1 `crates/robot-s0/src/rr_server_thread.rs::tests::bring_up`
    /// 同款"两端独立 Node"模式（service SingleThreaded → server `!Send`，
    /// 不能从 test 主线程跨线程 move 进 spawn closure）。
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

    #[test]
    fn invoke_task_skeleton_round_trip() {
        let topic = fresh_topic("invoke");
        let client = make_client::<markers::InvokeTaskRequest, markers::InvokeTaskResponse>(&topic);
        let factory = make_server_factory::<markers::InvokeTaskRequest, markers::InvokeTaskResponse>(
            topic.clone(),
        );

        let handle = spawn_task_invoke_server(cfg_for(&topic), factory).unwrap();

        let req_bytes = invoke_req("inv-skel-001");
        let pending = client.send(&req_bytes).unwrap();
        let bytes = wait_for_response(&pending, Duration::from_secs(2));
        let resp = flatbuffers::root::<FbInvokeResp>(&bytes).unwrap();
        assert_eq!(resp.request_id().unwrap(), "inv-skel-001");
        assert_eq!(resp.status(), ActionRpcStatus::Unimplemented);
        assert_eq!(resp.error_msg().unwrap(), SKELETON_REPLY_ERROR_MSG);
        assert_eq!(resp.action_id().unwrap(), "");
        assert_eq!(resp.accepted_at_ns(), 0);

        let metrics = handle.metrics();
        assert_eq!(metrics.requests_received.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.responses_sent.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.skeleton_replies.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.decode_errors.load(Ordering::Relaxed), 0);
        assert_eq!(handle.topic(), topic);

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn cancel_task_skeleton_round_trip() {
        let topic = fresh_topic("cancel");
        let client = make_client::<markers::CancelTaskRequest, markers::CancelTaskResponse>(&topic);
        let factory = make_server_factory::<markers::CancelTaskRequest, markers::CancelTaskResponse>(
            topic.clone(),
        );

        let handle = spawn_task_cancel_server(cfg_for(&topic), factory).unwrap();

        let req_bytes = cancel_req("cnl-skel-001", "act-uuid-7");
        let pending = client.send(&req_bytes).unwrap();
        let bytes = wait_for_response(&pending, Duration::from_secs(2));
        let resp = flatbuffers::root::<FbCancelResp>(&bytes).unwrap();
        assert_eq!(resp.request_id().unwrap(), "cnl-skel-001");
        assert_eq!(resp.action_id().unwrap(), "act-uuid-7");
        assert_eq!(resp.status(), ActionRpcStatus::Unimplemented);
        assert_eq!(resp.error_msg().unwrap(), SKELETON_REPLY_ERROR_MSG);
        assert_eq!(resp.cancel_at_ns(), 0);

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn task_status_skeleton_round_trip() {
        let topic = fresh_topic("status");
        let client =
            make_client::<markers::GetTaskStatusRequest, markers::GetTaskStatusResponse>(&topic);
        let factory = make_server_factory::<
            markers::GetTaskStatusRequest,
            markers::GetTaskStatusResponse,
        >(topic.clone());

        let handle = spawn_task_status_server(cfg_for(&topic), factory).unwrap();

        let req_bytes = status_req("sta-skel-001", "act-uuid-9");
        let pending = client.send(&req_bytes).unwrap();
        let bytes = wait_for_response(&pending, Duration::from_secs(2));
        let resp = flatbuffers::root::<FbStatusResp>(&bytes).unwrap();
        assert_eq!(resp.request_id().unwrap(), "sta-skel-001");
        assert_eq!(resp.action_id().unwrap(), "act-uuid-9");
        assert_eq!(resp.status(), ActionRpcStatus::Unimplemented);
        assert_eq!(resp.error_msg().unwrap(), SKELETON_REPLY_ERROR_MSG);
        assert_eq!(resp.state(), TaskStateFb::Unknown);
        assert!((resp.progress() - 0.0).abs() < f32::EPSILON);
        assert!(resp.outcome().is_none());
        assert!(resp.failure().is_none());

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn clear_error_skeleton_round_trip() {
        let topic = fresh_topic("clearerr");
        let client = make_client::<markers::ClearErrorRequest, markers::ClearErrorResponse>(&topic);
        let factory = make_server_factory::<markers::ClearErrorRequest, markers::ClearErrorResponse>(
            topic.clone(),
        );

        let handle = spawn_clear_error_server(cfg_for(&topic), factory).unwrap();

        let req_bytes = clear_req("clr-skel-001");
        let pending = client.send(&req_bytes).unwrap();
        let bytes = wait_for_response(&pending, Duration::from_secs(2));
        let resp = flatbuffers::root::<FbClearResp>(&bytes).unwrap();
        assert_eq!(resp.request_id().unwrap(), "clr-skel-001");
        assert_eq!(resp.status(), ActionRpcStatus::Unimplemented);
        assert_eq!(resp.error_msg().unwrap(), SKELETON_REPLY_ERROR_MSG);
        assert_eq!(resp.cleared_at_ns(), 0);

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn invoke_task_decode_error_returns_invalid_argument() {
        let topic = fresh_topic("decode_err");
        let client = make_client::<markers::InvokeTaskRequest, markers::InvokeTaskResponse>(&topic);
        let factory = make_server_factory::<markers::InvokeTaskRequest, markers::InvokeTaskResponse>(
            topic.clone(),
        );

        let handle = spawn_task_invoke_server(cfg_for(&topic), factory).unwrap();

        // 故意发非 fbs bytes（4 个 dummy 字节，不会通过 root verifier）。
        let bad_bytes: Vec<u8> = vec![0xAA, 0xBB, 0xCC, 0xDD];
        let pending = client.send(&bad_bytes).unwrap();
        let bytes = wait_for_response(&pending, Duration::from_secs(2));
        let resp = flatbuffers::root::<FbInvokeResp>(&bytes).unwrap();
        assert_eq!(resp.request_id().unwrap(), "");
        assert_eq!(resp.status(), ActionRpcStatus::InvalidArgument);
        assert_eq!(resp.error_msg().unwrap(), DECODE_ERROR_MSG);

        let metrics = handle.metrics();
        assert_eq!(metrics.requests_received.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.responses_sent.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.skeleton_replies.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.decode_errors.load(Ordering::Relaxed), 1);

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn handle_request_stop_join_idempotent_when_no_traffic() {
        // 锁住"主循环退出不悬挂线程"语义：spawn 起来不发任何 wire request，
        // 直接 request_stop + join 应在 ~poll_interval 内 clean exit。
        let topic = fresh_topic("idle_stop");
        let _client =
            make_client::<markers::InvokeTaskRequest, markers::InvokeTaskResponse>(&topic);
        let factory = make_server_factory::<markers::InvokeTaskRequest, markers::InvokeTaskResponse>(
            topic.clone(),
        );

        let handle = spawn_task_invoke_server(cfg_for(&topic), factory).unwrap();

        // 直接 stop + join。
        let start = std::time::Instant::now();
        handle.request_stop();
        handle.join().unwrap();
        let elapsed = start.elapsed();
        // poll_interval=100us；允许宽松 500ms 上限（CI 调度抖动）。
        assert!(
            elapsed < Duration::from_millis(500),
            "join took {elapsed:?}, expected < 500ms"
        );
    }
}
