//! `rr_server_thread` —— A1-3 (1/3c-1) `ClearEStop` RR server 线程。
//!
//! ## 角色
//!
//! reviewer A1-3 (1/3b) round 1 拍板的线程模型：
//!
//! - **`rr_server_thread`（本模块）**：只做 iceoryx2 RR I/O —— 收 wire
//!   `ClearEStopRequest` → decode → 转发到 `ipc_thread`（crossbeam channel）；
//!   接 `ipc_thread` 派发的 `ClearEStopRrResponse` → encode wire
//!   `ClearEStopResponse` → `active.send_response(bytes)`。
//!   **不**持 action 状态、**不**调 latch / executor / guard、**不**做
//!   deadline gate（deadline 权威在 `ipc_thread` §5.5/§5.6 两处 check）。
//! - **`ipc_thread`（A1-3 (1/3b) 已落地）**：唯一 action executor。
//!
//! ## 线程语义
//!
//! - **非 RT** 普通优先级线程，独立 `Iceoryx2RrServer` 单 owner（iceoryx2
//!   service 是 SingleThreaded，server 不跨线程）。
//! - 主循环用 `try_recv` 非阻塞；500µs poll 默认。
//! - 退出靠 `Arc<AtomicBool>` stop signal。
//!
//! ## 主循环顺序（reviewer A1-3 (1/3c) round 1 拍板）
//!
//! 1. **drain `rr_response_rx`**：先消耗 `ipc_thread` 的 response，释放
//!    pending 容量再 drain wire request，减少误报 backpressure。
//! 2. **drain wire request**：循环 `server.try_recv()` 一直直到 `Ok(None)`。
//!    `pending.len() >= max_in_flight` 时**也**消费 active，但走 overflow 路径
//!    立刻 reply `RpcStatus::ResourceExhausted`（不入 pending；详
//!    [`handle_pending_overflow`]）—— wire request 不会留在 iceoryx2 队列
//!    里悬挂到 deadline 超时。
//!
//! ## shutdown 行为（reviewer A1-3 (1/3c-2) round 1 拍板）
//!
//! `request_stop()` 触发 main loop 退出后，`run_loop` 调
//! [`abort_pending_on_shutdown`] 遍历 `pending`，给每条 in-flight active
//! best-effort 回 `RpcStatus::Aborted` + `error_msg = "s0 daemon shutting
//! down"`。**不**伪造成功 Ack；**不** drain 等 ipc_thread 真实结果（已
//! 决策：shutdown 路径不做 drain pending，让 gateway 端 timeout/retry）。
//! ipc_thread 之后产出的 response 由于 `rr_response_rx` 已 drop（rr_server_thread
//! 退出后 channel disconnected），按 channel 语义丢弃。
//!
//! ## decode / forward error 路径（reviewer A1-3 (1/3c) round 1 拍板）
//!
//! - **decode error**（语法 parse 失败 / 必填字段缺失 / 空 `request_id`）：
//!   reply `RpcStatus::InvalidArgument`，wire `request_id` 填空串或解析得
//!   到的值，consume active。
//! - **重复 in-flight `request_id`**：reply `RpcStatus::InvalidArgument`，
//!   不 forward；防 ipc_thread response 按 `request_id` 查找把第二条
//!   状态回给第一个 active handle。
//! - **`try_send::Full`**：reply `RpcStatus::ResourceExhausted`（error_msg
//!   "ipc_thread RR channel full"），不入 pending。
//! - **`try_send::Disconnected`**：reply `RpcStatus::Internal`（daemon 内部
//!   链路断），不入 pending。
//!
//! ## orphan response 路径
//!
//! `pending` 找不到对应 `request_id` → metrics + `tracing::error!` + 丢弃。
//! `debug_assert!` 保留兜底（debug build 才 panic；release 保命）。
//!
//! ## ActiveRequest 多 in-flight 模式
//!
//! 锁测 `robot_transport::iceoryx2::request_response::tests::
//! rr_server_can_hold_multiple_active_requests_concurrently` 已锁住 wrapper
//! 行为：`Iceoryx2ActiveRequest` 是独立 owned handle，server 可同时 hold 多个
//! active 后反序 `send_response`。本模块用 `VecDeque<PendingRr>` 容纳多个
//! in-flight active，按 `request_id` 匹配 ipc_thread 的 response。

#![allow(clippy::doc_markdown)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError};
use robot_core::executor_ctrl::OutputSafetyState;
use robot_core::time::RobotClock;
use robot_msg::build_helpers::build_clear_estop_response;
use robot_msg::markers;
use robot_msg::msg::{ClearEStopRequest as FbReq, RpcStatus};
use robot_transport::iceoryx2::request_response::{Iceoryx2ActiveRequest, Iceoryx2RrServer};

use crate::ipc_thread::{ClearEStopRrPayload, ClearEStopRrRequest, ClearEStopRrResponse};
use crate::state_machine::S0State;

// ============================================================================
// Config / Metrics / Handle
// ============================================================================

/// `rr_server_thread` 配置。
#[derive(Clone, Debug)]
pub struct RrServerThreadConfig {
    /// iceoryx2 service topic：通常 `/<robot_id>/control/clear_estop`。
    /// 仅作为 doc / metrics label；实际 service 由 `server_factory` closure
    /// 内部用 `rr_server(node, &topic)` 创建。
    pub topic: String,
    /// 主循环 idle sleep（纳秒）。默认 500µs（2 kHz），足够把 wire request
    /// drain 转发给 ipc_thread + 把 ipc_thread response 回 wire。
    pub poll_interval_ns: u64,
    /// 同时 in-flight 的 active request 上限。reviewer 拍板默认 64，跟
    /// `ipc_thread` RR channel buffer 一致；超限时新进 wire request 被
    /// 丢弃（直接 reply `ResourceExhausted`，不入 pending）。
    pub max_in_flight: usize,
}

impl Default for RrServerThreadConfig {
    fn default() -> Self {
        Self {
            topic: String::new(),
            poll_interval_ns: 500_000,
            max_in_flight: crate::event_ingress::RR_CHANNEL_CAPACITY,
        }
    }
}

/// `rr_server_thread` 运行期 metrics（外部 caller 通过 [`RrServerThreadHandle::metrics`]
/// 拿 `Arc` 观察；线程内部通过 `Arc<RrServerMetrics>` 写）。
#[derive(Default, Debug)]
pub struct RrServerMetrics {
    /// `server.try_recv()` 成功收到的 wire request 数（含 decode 失败）。
    pub requests_received: AtomicU64,
    /// 成功 decode + 入 in_flight pending + 转发到 ipc_thread 的 request 数。
    pub requests_forwarded: AtomicU64,
    /// 成功 `active.send_response()` 的 response 数。
    pub responses_sent: AtomicU64,
    /// FlatBuffers root verifier 失败 / 必填字段缺失 / 空 request_id：直接
    /// reply InvalidArgument。
    pub decode_errors: AtomicU64,
    /// 重复 in-flight `request_id`：直接 reply InvalidArgument，不 forward。
    pub duplicate_request_id: AtomicU64,
    /// `rr_request_tx.try_send` 满：reply ResourceExhausted。
    pub forward_full_errors: AtomicU64,
    /// `rr_request_tx.try_send` Disconnected：reply Internal。
    pub forward_disconnected_errors: AtomicU64,
    /// pending 容量满（>= max_in_flight）时新 wire request 被拒：reply
    /// ResourceExhausted。
    pub overflow_drops: AtomicU64,
    /// `pending` 找不到对应 `request_id` 的 ipc_thread response（理论不该
    /// 发生；release 保命丢弃，debug_assert 兜底）。
    pub orphan_responses: AtomicU64,
    /// `active.send_response` 内部 IO 失败的次数。
    pub send_response_errors: AtomicU64,
}

/// `rr_server_thread` 句柄。caller 通过 `request_stop()` + `join()` 关停。
pub struct RrServerThreadHandle {
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    metrics: Arc<RrServerMetrics>,
}

impl RrServerThreadHandle {
    /// 取 metrics 引用观察运行期统计。
    #[must_use]
    pub fn metrics(&self) -> Arc<RrServerMetrics> {
        Arc::clone(&self.metrics)
    }

    /// 通知主循环退出（下一轮 stop check 命中即跳出）。
    pub fn request_stop(&self) {
        self.stop.swap(true, Ordering::AcqRel);
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
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

// ============================================================================
// PendingRr: 持 in-flight active request 元数据
// ============================================================================

/// in-flight RR request 的元数据。`active` 是 owned handle（A1-3 (1/3c) prep
/// 锁测验证：`Iceoryx2ActiveRequest` 类型签名无 lifetime；server 可同时
/// hold 多个），按 `request_id` 跟 ipc_thread 的 response 关联。
struct PendingRr {
    /// 跟 wire `ClearEStopRequest.request_id` + ipc_thread
    /// `ClearEStopRrResponse.request_id` 关联。
    request_id: String,
    /// wire 回包字段需要回传 `dry_run`（`clear_estop.fbs` 字段语义）。
    dry_run: bool,
    /// owned active request handle；`send_response` 时 by-value consume。
    active: Iceoryx2ActiveRequest<markers::ClearEStopRequest, markers::ClearEStopResponse>,
}

// ============================================================================
// spawn API
// ============================================================================

/// 起 `rr_server_thread`。
///
/// `server_factory` closure 在新线程内执行（iceoryx2 RR server `!Send`，
/// 必须在 owner 线程内构造；同 `ipc_thread::spawn_ipc_thread` factory 模式）。
///
/// `rr_request_tx` / `rr_response_rx` 跟 `ipc_thread::RrChannels::request_rx`
/// / `response_tx` 是同对：rr_server_thread → ipc_thread 走 `request_tx` /
/// `request_rx`；ipc_thread → rr_server_thread 走 `response_tx` /
/// `response_rx`。channel buffer reviewer 拍板 `bounded(64)`。
///
/// # Errors
///
/// - `server_factory` panic（factory 起 iceoryx2 service 失败 / port 耗尽
///   等）
/// - `thread::Builder::spawn` 失败（极罕见）
pub fn spawn_rr_server_thread<F>(
    cfg: RrServerThreadConfig,
    clock: Arc<dyn RobotClock>,
    server_factory: F,
    rr_request_tx: Sender<ClearEStopRrRequest>,
    rr_response_rx: Receiver<ClearEStopRrResponse>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<RrServerThreadHandle>
where
    F: FnOnce() -> Iceoryx2RrServer<markers::ClearEStopRequest, markers::ClearEStopResponse>
        + Send
        + 'static,
{
    validate_rr_capacity(
        cfg.max_in_flight,
        rr_request_tx.capacity(),
        rr_response_rx.capacity(),
    )?;
    #[cfg(feature = "test-comm-fault-hook")]
    crate::comm_fault_hook::before_spawn("clear_rr")?;
    let metrics = Arc::new(RrServerMetrics::default());

    let stop_clone = Arc::clone(&stop);
    let metrics_clone = Arc::clone(&metrics);

    // 同步 ready signal：spawn 返回前确认 server_factory 已成功执行（factory
    // panic / iceoryx2 错都在这里捕获）。同 `ipc_thread::spawn_ipc_thread` 模式。
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

    let join = thread::Builder::new()
        .name("s0_rr_server_thread".into())
        .spawn(move || {
            let server = server_factory();
            let _ = ready_tx.send(());
            drop(ready_tx);
            run_loop(
                &cfg,
                clock.as_ref(),
                &server,
                &rr_request_tx,
                &rr_response_rx,
                &stop_clone,
                &metrics_clone,
            );
        })?;

    if ready_rx.recv().is_err() {
        return Err(std::io::Error::other(
            "rr_server_thread server_factory panicked before signalling ready",
        ));
    }

    Ok(RrServerThreadHandle {
        join,
        stop,
        metrics,
    })
}

/// 回复 channel 必须容纳全部在飞请求，避免 IPC 等待已停用的 RR server。
pub(crate) fn validate_rr_capacity(
    max_in_flight: usize,
    requests: Option<usize>,
    responses: Option<usize>,
) -> std::io::Result<()> {
    let limit = crate::event_ingress::RR_CHANNEL_CAPACITY;
    if !(1..=limit).contains(&max_in_flight)
        || !requests.is_some_and(|n| (1..=limit).contains(&n))
        || !responses.is_some_and(|n| n >= max_in_flight && n <= limit)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "RR 必须有界：请求容量 1..=64，回复容量 >= 在飞上限且 <=64",
        ));
    }
    Ok(())
}

// ============================================================================
// 主循环
// ============================================================================

#[allow(clippy::too_many_lines)]
fn run_loop(
    cfg: &RrServerThreadConfig,
    clock: &dyn RobotClock,
    server: &Iceoryx2RrServer<markers::ClearEStopRequest, markers::ClearEStopResponse>,
    rr_request_tx: &Sender<ClearEStopRrRequest>,
    rr_response_rx: &Receiver<ClearEStopRrResponse>,
    stop: &AtomicBool,
    metrics: &RrServerMetrics,
) {
    let mut pending: VecDeque<PendingRr> = VecDeque::with_capacity(cfg.max_in_flight);

    while crate::worker_stop::worker_running(
        crate::worker_stop::WorkerKind::ClearEStopRr,
        clock,
        stop,
    ) {
        // ─── 1) drain rr_response_rx 先（reviewer 拍板：释放 pending 容量
        //        减少误报 backpressure）───
        while let Ok(resp) = rr_response_rx.try_recv() {
            handle_response(&mut pending, &resp, clock, metrics);
        }

        // ─── 2) drain wire request from server ───
        //
        // **关键**：即使 `pending.len() >= max_in_flight`，仍**继续** try_recv
        // 把 wire request 从 iceoryx2 队列里拿出来 → 消费 active handle →
        // 直接 reply `ResourceExhausted`（不 forward、不入 pending、不挂在
        // iceoryx2 队列里等到 deadline 超时；reviewer A1-3 (1/3c-1) round 1
        // 阻断修复）。drain 不停直到 server.try_recv() 返 None。
        loop {
            match server.try_recv() {
                Ok(Some(active)) => {
                    metrics.requests_received.fetch_add(1, Ordering::Relaxed);
                    if pending.len() >= cfg.max_in_flight {
                        handle_pending_overflow(active, metrics);
                    } else {
                        handle_active_request(active, &mut pending, rr_request_tx, metrics);
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::error!(error = ?e, "rr_server_thread: server.try_recv failed");
                    break;
                }
            }
        }

        thread::sleep(Duration::from_nanos(cfg.poll_interval_ns));
    }

    // **A1-3 (1/3c-2) round 1 拍板**：shutdown 路径 best-effort 给所有
    // in-flight pending 回 `RpcStatus::Aborted`，不 drain 等 ipc_thread 真实
    // 结果。
    abort_pending_on_shutdown(&mut pending, metrics);
}

/// 退出前 best-effort 给 pending 中所有 in-flight active 回
/// `RpcStatus::Aborted` + `error_msg = "s0 daemon shutting down"`。
///
/// reviewer A1-3 (1/3c-2) round 1 拍板（"shutdown 显式 abort pending"，
/// 不 drain）：
///
/// - **不**伪造成功 Ack（cleared_at_ns=0；status=Aborted 让 gateway 区分
///   shutdown abort vs preempt-by-emergency 无歧义）。
/// - `error_msg` 固定 `"s0 daemon shutting down"`，gateway 可按 audit
///   trace 关联到 daemon 退出事件。
/// - `dry_run` 字段从原 request 复用（保持 wire response 字段语义跟
///   `clear_estop.fbs` `ClearEStopResponse.dry_run` 一致）。
/// - `send_response` 失败只 metrics + tracing，**不**阻塞 shutdown
///   （gateway 端按 RR timeout/retry 兜底；client side 链路断也合理）。
/// - ipc_thread 之后产出的 response：`rr_response_rx` drop 后 channel
///   disconnected → ipc_thread 内 `rr.response_tx.send(...)` 返 `Err`，被
///   `let _ =` 忽略（**不**会有第二条 wire response 出现；wire 客户端
///   已经收到 `Aborted` 即终态，gateway 端不应等更多 reply）。
fn abort_pending_on_shutdown(pending: &mut VecDeque<PendingRr>, metrics: &RrServerMetrics) {
    if pending.is_empty() {
        return;
    }
    tracing::info!(
        pending_count = pending.len(),
        "rr_server_thread: shutting down; aborting pending in-flight requests with RpcStatus::Aborted"
    );
    while let Some(p) = pending.pop_front() {
        let bytes = build_clear_estop_response(
            &p.request_id,
            RpcStatus::Aborted,
            "s0 daemon shutting down",
            0,
            p.dry_run,
        );
        if let Err(e) = p.active.send_response(&bytes) {
            tracing::error!(
                error = ?e,
                request_id = %p.request_id,
                "rr_server_thread: shutdown abort send_response failed"
            );
            metrics.send_response_errors.fetch_add(1, Ordering::Relaxed);
        } else {
            metrics.responses_sent.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// ============================================================================
// handle_active_request：decode + forward / 直接 reply error
// ============================================================================

fn handle_active_request(
    active: Iceoryx2ActiveRequest<markers::ClearEStopRequest, markers::ClearEStopResponse>,
    pending: &mut VecDeque<PendingRr>,
    rr_request_tx: &Sender<ClearEStopRrRequest>,
    metrics: &RrServerMetrics,
) {
    let payload = active.payload();

    // FlatBuffers root verifier
    let view = match flatbuffers::root::<FbReq>(payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = ?e, "rr_server_thread: wire decode failed");
            metrics.decode_errors.fetch_add(1, Ordering::Relaxed);
            send_error_reply(
                active,
                "",
                false,
                RpcStatus::InvalidArgument,
                "wire decode failed",
                metrics,
            );
            return;
        }
    };

    let request_id = view.request_id().unwrap_or("").to_string();
    let dry_run = view.dry_run();

    // 必填字段：空 request_id 拒（reviewer 拍板：必填字段缺失算 invalid）
    if request_id.is_empty() {
        tracing::warn!("rr_server_thread: empty request_id");
        metrics.decode_errors.fetch_add(1, Ordering::Relaxed);
        send_error_reply(
            active,
            "",
            dry_run,
            RpcStatus::InvalidArgument,
            "request_id is empty",
            metrics,
        );
        return;
    }

    // 重复 in-flight request_id：直接 reply InvalidArgument 不 forward
    // （reviewer 拍板：防 response 按 request_id 查找把第二条状态回给第一个
    // active handle）
    if pending.iter().any(|p| p.request_id == request_id) {
        tracing::warn!(
            request_id = %request_id,
            "rr_server_thread: duplicate in-flight request_id"
        );
        metrics.duplicate_request_id.fetch_add(1, Ordering::Relaxed);
        send_error_reply(
            active,
            &request_id,
            dry_run,
            RpcStatus::InvalidArgument,
            "duplicate request_id (already in-flight)",
            metrics,
        );
        return;
    }

    let confirmation = view.confirmation().unwrap_or("").to_string();
    let audit_reason = view.audit_reason().unwrap_or("").to_string();
    let deadline_ns = view.deadline_ns();

    let req = ClearEStopRrRequest {
        request_id: request_id.clone(),
        dry_run,
        confirmation,
        audit_reason,
        deadline_ns,
    };

    match rr_request_tx.try_send(req) {
        Ok(()) => {
            pending.push_back(PendingRr {
                request_id,
                dry_run,
                active,
            });
            metrics.requests_forwarded.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Full(_)) => {
            tracing::warn!(
                request_id = %request_id,
                "rr_server_thread: ipc_thread RR channel full; replying ResourceExhausted"
            );
            metrics.forward_full_errors.fetch_add(1, Ordering::Relaxed);
            send_error_reply(
                active,
                &request_id,
                dry_run,
                RpcStatus::ResourceExhausted,
                "ipc_thread RR channel full",
                metrics,
            );
        }
        Err(TrySendError::Disconnected(_)) => {
            tracing::error!(
                request_id = %request_id,
                "rr_server_thread: ipc_thread RR channel disconnected; replying Internal"
            );
            metrics
                .forward_disconnected_errors
                .fetch_add(1, Ordering::Relaxed);
            send_error_reply(
                active,
                &request_id,
                dry_run,
                RpcStatus::Internal,
                "ipc_thread RR channel disconnected",
                metrics,
            );
        }
    }
}

/// pending 容量满（`pending.len() >= max_in_flight`）时直接 reply
/// `ResourceExhausted`，consume active handle，不入 pending、不 forward
/// 到 ipc_thread。
///
/// reviewer A1-3 (1/3c-1) round 1 拍板修复：避免 wire request 在 iceoryx2
/// 队列里悬挂到客户端 deadline 超时；rr_server_thread 一拿到 active 就立刻
/// 给 client 一个明确的 ResourceExhausted 回包。
///
/// best-effort 解 `request_id` + `dry_run` 字段填回 wire response：
/// FlatBuffers verifier 失败时 fallback 空 request_id + `dry_run=false`，
/// 不再走 InvalidArgument 路径（gateway 看 ResourceExhausted 已能识别拒绝
/// 类型）。
fn handle_pending_overflow(
    active: Iceoryx2ActiveRequest<markers::ClearEStopRequest, markers::ClearEStopResponse>,
    metrics: &RrServerMetrics,
) {
    metrics.overflow_drops.fetch_add(1, Ordering::Relaxed);
    let (request_id, dry_run) = match flatbuffers::root::<FbReq>(active.payload()) {
        Ok(view) => (view.request_id().unwrap_or("").to_string(), view.dry_run()),
        Err(_) => (String::new(), false),
    };
    tracing::warn!(
        request_id = %request_id,
        "rr_server_thread: pending full; replying ResourceExhausted"
    );
    send_error_reply(
        active,
        &request_id,
        dry_run,
        RpcStatus::ResourceExhausted,
        "rr_server pending full",
        metrics,
    );
}

/// 直接 reply 错误 wire response，consume active handle。
fn send_error_reply(
    active: Iceoryx2ActiveRequest<markers::ClearEStopRequest, markers::ClearEStopResponse>,
    request_id: &str,
    dry_run: bool,
    status: RpcStatus,
    error_msg: &str,
    metrics: &RrServerMetrics,
) {
    let bytes = build_clear_estop_response(request_id, status, error_msg, 0, dry_run);
    if let Err(e) = active.send_response(&bytes) {
        tracing::error!(
            error = ?e,
            request_id = %request_id,
            "rr_server_thread: send_response (error reply) failed"
        );
        metrics.send_response_errors.fetch_add(1, Ordering::Relaxed);
    } else {
        metrics.responses_sent.fetch_add(1, Ordering::Relaxed);
    }
}

// ============================================================================
// handle_response：encode ipc_thread payload → wire response
// ============================================================================

fn handle_response(
    pending: &mut VecDeque<PendingRr>,
    resp: &ClearEStopRrResponse,
    _clock: &dyn RobotClock,
    metrics: &RrServerMetrics,
) {
    let pos = pending.iter().position(|p| p.request_id == resp.request_id);
    let Some(idx) = pos else {
        tracing::error!(
            request_id = %resp.request_id,
            "rr_server_thread: orphan response (no pending active for request_id)"
        );
        metrics.orphan_responses.fetch_add(1, Ordering::Relaxed);
        debug_assert!(
            false,
            "orphan response: request_id={} (release build 保命丢弃；debug build panic)",
            resp.request_id
        );
        return;
    };
    let pending_rr = pending.remove(idx).expect("position 找到");
    let (status, error_msg, cleared_at_ns) = encode_payload(&resp.payload);
    let bytes = build_clear_estop_response(
        &resp.request_id,
        status,
        &error_msg,
        cleared_at_ns,
        pending_rr.dry_run,
    );
    if let Err(e) = pending_rr.active.send_response(&bytes) {
        tracing::error!(
            error = ?e,
            request_id = %resp.request_id,
            "rr_server_thread: send_response failed"
        );
        metrics.send_response_errors.fetch_add(1, Ordering::Relaxed);
    } else {
        metrics.responses_sent.fetch_add(1, Ordering::Relaxed);
    }
}

/// `ClearEStopRrPayload` → wire `(RpcStatus, error_msg, cleared_at_ns)`。
///
/// reviewer 拍板硬边界：S0 端**不**直接 cast tonic code；wire 状态由
/// `RpcStatus` enum 表达，gateway 端通过 `rpc_status_to_tonic_code` 显式
/// match 到 `tonic::Code`。
fn encode_payload(payload: &ClearEStopRrPayload) -> (RpcStatus, String, u64) {
    match payload {
        ClearEStopRrPayload::Cleared { cleared_at_ns } => {
            (RpcStatus::Ok, String::new(), *cleared_at_ns)
        }
        ClearEStopRrPayload::WouldClear { would_clear_at_ns } => {
            (RpcStatus::Ok, String::new(), *would_clear_at_ns)
        }
        ClearEStopRrPayload::StateNotSafe { actual } => (
            RpcStatus::FailedPrecondition,
            format_state_not_safe(*actual),
            0,
        ),
        ClearEStopRrPayload::NotTorqueOffConfirmed { actual } => (
            RpcStatus::FailedPrecondition,
            format_torque_state(*actual),
            0,
        ),
        ClearEStopRrPayload::FsyncFailed => (
            RpcStatus::Internal,
            "fsync failed; disk latch preserved as latched=true (backup-first)".into(),
            0,
        ),
        ClearEStopRrPayload::AlreadyInProgress => (
            RpcStatus::ResourceExhausted,
            "another live ClearEStop in flight".into(),
            0,
        ),
        ClearEStopRrPayload::PreemptedByEmergency => (
            RpcStatus::Aborted,
            "in-flight live ClearEStop preempted by EStop / SafetyStopFailed".into(),
            0,
        ),
        ClearEStopRrPayload::DeadlineExceeded => (
            RpcStatus::DeadlineExceeded,
            "deadline exceeded at drain entry or preempt check".into(),
            0,
        ),
    }
}

fn format_state_not_safe(actual: S0State) -> String {
    format!("state not Safe (actual: {actual:?})")
}

fn format_torque_state(actual: OutputSafetyState) -> String {
    format!("safety_state not TorqueOffConfirmed (actual: {actual:?})")
}

// ============================================================================
// Tests —— A1-3 (1/3c-1) in-process wire/client/server 端到端
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::AtomicU32;
    use std::time::Instant;

    use crossbeam_channel::bounded;
    #[cfg(not(debug_assertions))]
    use crossbeam_channel::unbounded;
    use iceoryx2::prelude::NodeBuilder;
    use iceoryx2::service::ipc::Service as IxService;
    use robot_core::time::SystemClock;
    use robot_msg::build_helpers::build_clear_estop_request;
    use robot_msg::msg::{ClearEStopResponse as FbResp, RpcRole};
    use robot_transport::iceoryx2::request_response::{
        Iceoryx2PendingResponse, Iceoryx2RrClient, rr_client, rr_server,
    };

    /// 同进程 in-process server + client 拉起方式：
    /// - test 主线程持 Node + client（送 wire request + 收 wire response）
    /// - 通过 `rr_channel` 一次性创建 client + server，server 通过
    ///   `Option<Iceoryx2RrServer>` move 到 `spawn_rr_server_thread` 的
    ///   factory closure 内
    fn fresh_topic(suffix: &str) -> String {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        format!(
            "/rr_server_thread_test_{}_{}_{suffix}",
            std::process::id(),
            seq
        )
    }

    /// 起 rr_server_thread + 配套 in-process iceoryx2 client。
    ///
    /// `Iceoryx2RrServer` 是 `!Send`（iceoryx2 SingleThreaded service），必须
    /// 在 owner 线程内构造。两端用 `rr_client` / `rr_server` 单端工厂分别
    /// create + open 同一 topic（first-create-after-open 模式）：
    /// - test 主线程：独立 Node + `rr_client`
    /// - spawn factory：独立 Node + `rr_server`
    ///
    /// 返回:
    /// - handle: rr_server_thread handle
    /// - client: 测试线程发 wire request 用
    /// - rr_request_rx: mock ipc_thread 收 forwarded request 用
    /// - rr_response_tx: mock ipc_thread 派发 response 用
    #[allow(clippy::type_complexity)]
    fn bring_up(
        max_in_flight: usize,
        request_buf: usize,
        response_buf: usize,
    ) -> (
        RrServerThreadHandle,
        Iceoryx2RrClient<markers::ClearEStopRequest, markers::ClearEStopResponse>,
        Receiver<ClearEStopRrRequest>,
        Sender<ClearEStopRrResponse>,
    ) {
        let topic = fresh_topic("bringup");

        // test 主线程 Node + client（先 create service）
        let test_node: &'static _ =
            Box::leak(Box::new(NodeBuilder::new().create::<IxService>().unwrap()));
        let client =
            rr_client::<markers::ClearEStopRequest, markers::ClearEStopResponse>(test_node, &topic)
                .expect("rr_client create");

        let (req_tx, req_rx) = bounded::<ClearEStopRrRequest>(request_buf);
        let (resp_tx, resp_rx) = bounded::<ClearEStopRrResponse>(response_buf);

        let cfg = RrServerThreadConfig {
            topic: topic.clone(),
            poll_interval_ns: 500_000,
            max_in_flight,
        };
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());

        // factory 内部独立 Node + server（后 open service）
        let topic_for_factory = topic;
        let factory = move || {
            let server_node = NodeBuilder::new().create::<IxService>().unwrap();
            let server = rr_server::<markers::ClearEStopRequest, markers::ClearEStopResponse>(
                &server_node,
                &topic_for_factory,
            )
            .expect("rr_server create");
            // server 持 server_node 内部 shm fd；leak 让 server 整个生命周期
            // 内 node 不被 drop（rr_server_thread 退出时整个 process exit；
            // test 同进程 leak 是 test scope 的常规处理）。
            Box::leak(Box::new(server_node));
            server
        };

        let handle = spawn_rr_server_thread(
            cfg,
            clock,
            factory,
            req_tx,
            resp_rx,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .expect("spawn_rr_server_thread");

        (handle, client, req_rx, resp_tx)
    }

    fn wait_for_response(
        pending: &Iceoryx2PendingResponse<markers::ClearEStopResponse>,
        max: Duration,
    ) -> (RpcStatus, String, String, u64, bool) {
        let start = Instant::now();
        loop {
            if let Some(bytes) = pending.try_recv().expect("pending recv") {
                let view = flatbuffers::root::<FbResp>(&bytes).expect("decode response");
                return (
                    view.status(),
                    view.request_id().unwrap_or("").to_string(),
                    view.error_msg().unwrap_or("").to_string(),
                    view.cleared_at_ns(),
                    view.dry_run(),
                );
            }
            assert!(
                start.elapsed() < max,
                "client did not receive response within {max:?}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// 测试 helper：等 mock ipc_thread 收到 forwarded request（最多 2s）。
    fn wait_for_request(rx: &Receiver<ClearEStopRrRequest>, max: Duration) -> ClearEStopRrRequest {
        rx.recv_timeout(max)
            .expect("ipc_thread did not receive forwarded request within timeout")
    }

    // ────────────────────────────────────────────────────────────────────────
    // 1) 成功路径：rr_server forwards decoded wire request to ipc_thread channel
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn rr_server_forwards_decoded_request_to_ipc_thread() {
        let (handle, client, req_rx, resp_tx) = bring_up(64, 64, 64);

        let bytes = build_clear_estop_request(
            "req-1",
            "idem-1",
            "src.test",
            RpcRole::Maintainer,
            "CONFIRM-1",
            "audit-reason-1",
            false,
            u64::MAX,
            42,
        );
        let pending = client.send(&bytes).expect("client send");

        // mock ipc_thread 收到 forwarded request
        let req = wait_for_request(&req_rx, Duration::from_secs(2));
        assert_eq!(req.request_id, "req-1");
        assert!(!req.dry_run);
        assert_eq!(req.confirmation, "CONFIRM-1");
        assert_eq!(req.audit_reason, "audit-reason-1");
        assert_eq!(req.deadline_ns, u64::MAX);

        // mock ipc_thread reply Cleared
        resp_tx
            .send(ClearEStopRrResponse {
                request_id: "req-1".to_string(),
                payload: ClearEStopRrPayload::Cleared {
                    cleared_at_ns: 999_999,
                },
            })
            .unwrap();

        // wire response 端验证
        let (status, rid, _err, cleared_at_ns, dry_run) =
            wait_for_response(&pending, Duration::from_secs(2));
        assert_eq!(status, RpcStatus::Ok);
        assert_eq!(rid, "req-1");
        assert_eq!(cleared_at_ns, 999_999);
        assert!(!dry_run);

        handle.request_stop();
        handle.join().unwrap();
    }

    // ────────────────────────────────────────────────────────────────────────
    // 2) 8 个 ClearEStopRrPayload variant → wire RpcStatus 全枚举锁
    // ────────────────────────────────────────────────────────────────────────

    fn run_variant(
        payload: ClearEStopRrPayload,
        expected_status: RpcStatus,
        expected_cleared_at_ns: u64,
    ) {
        let (handle, client, req_rx, resp_tx) = bring_up(64, 64, 64);
        let pending = client
            .send(&build_clear_estop_request(
                "rid",
                "idem",
                "src",
                RpcRole::Maintainer,
                "confirm",
                "audit",
                false,
                u64::MAX,
                0,
            ))
            .unwrap();

        let _req = wait_for_request(&req_rx, Duration::from_secs(2));
        resp_tx
            .send(ClearEStopRrResponse {
                request_id: "rid".to_string(),
                payload,
            })
            .unwrap();

        let (status, rid, _err, cleared_at_ns, _dry_run) =
            wait_for_response(&pending, Duration::from_secs(2));
        assert_eq!(status, expected_status);
        assert_eq!(rid, "rid");
        assert_eq!(cleared_at_ns, expected_cleared_at_ns);

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn rr_server_replies_ok_for_cleared_variant() {
        run_variant(
            ClearEStopRrPayload::Cleared {
                cleared_at_ns: 12_345,
            },
            RpcStatus::Ok,
            12_345,
        );
    }

    #[test]
    fn rr_server_replies_ok_for_would_clear_variant() {
        run_variant(
            ClearEStopRrPayload::WouldClear {
                would_clear_at_ns: 67_890,
            },
            RpcStatus::Ok,
            67_890,
        );
    }

    #[test]
    fn rr_server_replies_failed_precondition_for_state_not_safe() {
        run_variant(
            ClearEStopRrPayload::StateNotSafe {
                actual: S0State::Ready,
            },
            RpcStatus::FailedPrecondition,
            0,
        );
    }

    #[test]
    fn rr_server_replies_failed_precondition_for_not_torque_off_confirmed() {
        run_variant(
            ClearEStopRrPayload::NotTorqueOffConfirmed {
                actual: OutputSafetyState::TorqueOn,
            },
            RpcStatus::FailedPrecondition,
            0,
        );
    }

    #[test]
    fn rr_server_replies_internal_for_fsync_failed() {
        run_variant(ClearEStopRrPayload::FsyncFailed, RpcStatus::Internal, 0);
    }

    #[test]
    fn rr_server_replies_resource_exhausted_for_already_in_progress() {
        run_variant(
            ClearEStopRrPayload::AlreadyInProgress,
            RpcStatus::ResourceExhausted,
            0,
        );
    }

    #[test]
    fn rr_server_replies_aborted_for_preempted_by_emergency() {
        run_variant(
            ClearEStopRrPayload::PreemptedByEmergency,
            RpcStatus::Aborted,
            0,
        );
    }

    #[test]
    fn rr_server_replies_deadline_exceeded_for_deadline_exceeded() {
        run_variant(
            ClearEStopRrPayload::DeadlineExceeded,
            RpcStatus::DeadlineExceeded,
            0,
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // 3) decode error → InvalidArgument
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn rr_server_decodes_invalid_wire_replies_invalid_argument() {
        let (handle, client, req_rx, _resp_tx) = bring_up(64, 64, 64);
        // 故意发垃圾 bytes（不是合法 FlatBuffers payload）
        let pending = client.send(b"NOT_A_VALID_FLATBUFFER").expect("client send");

        let (status, _rid, err_msg, _cleared, _dry_run) =
            wait_for_response(&pending, Duration::from_secs(2));
        assert_eq!(status, RpcStatus::InvalidArgument);
        assert!(
            err_msg.contains("decode"),
            "error_msg 应含 'decode' 关键词；实际 = {err_msg:?}"
        );

        // mock ipc_thread 不应收到 forwarded request
        assert!(req_rx.try_recv().is_err());
        let metrics = handle.metrics();
        assert_eq!(metrics.decode_errors.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.requests_forwarded.load(Ordering::Relaxed), 0);

        handle.request_stop();
        handle.join().unwrap();
    }

    /// reviewer 拍板：必填字段缺失也算 invalid（即使 FlatBuffers parse 成功）。
    #[test]
    fn rr_server_replies_invalid_argument_for_empty_request_id() {
        let (handle, client, req_rx, _resp_tx) = bring_up(64, 64, 64);

        // request_id 空串
        let bytes = build_clear_estop_request(
            "",
            "idem",
            "src",
            RpcRole::Maintainer,
            "confirm",
            "audit",
            false,
            u64::MAX,
            0,
        );
        let pending = client.send(&bytes).expect("client send");
        let (status, rid, err_msg, _cleared, _dry_run) =
            wait_for_response(&pending, Duration::from_secs(2));
        assert_eq!(status, RpcStatus::InvalidArgument);
        assert_eq!(rid, "", "request_id 空时 wire response request_id 也填空");
        assert!(
            err_msg.contains("request_id"),
            "error_msg 应含 'request_id'；实际 = {err_msg:?}"
        );

        assert!(req_rx.try_recv().is_err(), "request 不应被 forward");

        handle.request_stop();
        handle.join().unwrap();
    }

    // ────────────────────────────────────────────────────────────────────────
    // 4) 重复 in-flight request_id → InvalidArgument，不 forward
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn rr_server_replies_invalid_argument_for_duplicate_in_flight_request_id() {
        let (handle, client, req_rx, resp_tx) = bring_up(64, 64, 64);

        // 第一条 request 正常进 in_flight pending
        let pending_1 = client
            .send(&build_clear_estop_request(
                "dup",
                "idem",
                "src",
                RpcRole::Maintainer,
                "c1",
                "a1",
                false,
                u64::MAX,
                0,
            ))
            .unwrap();
        let _req1 = wait_for_request(&req_rx, Duration::from_secs(2));

        // 第二条 same request_id 进来，rr_server 应拒（duplicate）
        let pending_2 = client
            .send(&build_clear_estop_request(
                "dup",
                "idem",
                "src",
                RpcRole::Maintainer,
                "c2",
                "a2",
                false,
                u64::MAX,
                0,
            ))
            .unwrap();

        let (status_2, rid_2, err_msg_2, _cleared_2, _dry_run_2) =
            wait_for_response(&pending_2, Duration::from_secs(2));
        assert_eq!(status_2, RpcStatus::InvalidArgument);
        assert_eq!(rid_2, "dup");
        assert!(
            err_msg_2.contains("duplicate"),
            "error_msg 应含 'duplicate'；实际 = {err_msg_2:?}"
        );

        // mock ipc_thread 应只收到 1 条 forwarded request
        assert!(
            req_rx.try_recv().is_err(),
            "第二条 duplicate 不应被 forward"
        );

        // 第一条收 ipc_thread 的 reply（让 pending 释放）
        resp_tx
            .send(ClearEStopRrResponse {
                request_id: "dup".to_string(),
                payload: ClearEStopRrPayload::Cleared {
                    cleared_at_ns: 1_000,
                },
            })
            .unwrap();
        let (status_1, _, _, _, _) = wait_for_response(&pending_1, Duration::from_secs(2));
        assert_eq!(status_1, RpcStatus::Ok);

        let metrics = handle.metrics();
        assert_eq!(metrics.duplicate_request_id.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.requests_forwarded.load(Ordering::Relaxed), 1);

        handle.request_stop();
        handle.join().unwrap();
    }

    // ────────────────────────────────────────────────────────────────────────
    // 5) forward error: ipc_thread channel 满 → ResourceExhausted
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn rr_server_drops_request_when_ipc_thread_channel_full_replies_resource_exhausted() {
        // ipc_thread RR request channel 故意 size=1，预先塞一个不让 rr_server
        // forward 第二条
        let (handle, client, req_rx, _resp_tx) = bring_up(64, 1, 64);

        // 第一条占满 channel
        let pending_1 = client
            .send(&build_clear_estop_request(
                "first",
                "idem",
                "src",
                RpcRole::Maintainer,
                "c1",
                "a1",
                false,
                u64::MAX,
                0,
            ))
            .unwrap();
        // 不立刻 drain req_rx，让 channel 保持满状态
        let _req1 = wait_for_request(&req_rx, Duration::from_secs(2));
        // 现在 req_rx 收完了一条；channel 又空了。需要再 send 一条占住 channel
        // 同时不 drain（让第三条 forward 撞 Full）。
        //
        // 实际更简单：channel 满判定基于 channel buffer，drain 后 buffer 恢复
        // 容量 1。再 send 一条不 drain，channel 会再次满。
        //
        // 直接两条 connection 测：
        let pending_2 = client
            .send(&build_clear_estop_request(
                "second",
                "idem",
                "src",
                RpcRole::Maintainer,
                "c2",
                "a2",
                false,
                u64::MAX,
                0,
            ))
            .unwrap();
        // 第二条 forward 到 req_rx 但不 drain（channel 满 = 1 个 in flight）
        // 用 sleep 等 rr_server poll 一轮
        thread::sleep(Duration::from_millis(20));
        // 第三条进来时 channel 已满，触发 try_send::Full
        let pending_3 = client
            .send(&build_clear_estop_request(
                "third",
                "idem",
                "src",
                RpcRole::Maintainer,
                "c3",
                "a3",
                false,
                u64::MAX,
                0,
            ))
            .unwrap();

        let (status_3, rid_3, err_msg_3, _cleared_3, _dry_run_3) =
            wait_for_response(&pending_3, Duration::from_secs(2));
        assert_eq!(status_3, RpcStatus::ResourceExhausted);
        assert_eq!(rid_3, "third");
        assert!(
            err_msg_3.contains("channel full"),
            "error_msg 应含 'channel full'；实际 = {err_msg_3:?}"
        );

        let metrics = handle.metrics();
        assert!(metrics.forward_full_errors.load(Ordering::Relaxed) >= 1);

        // 让 first / second pending 等不到 reply（test 主线程不 drain
        // resp_tx）；测试只验 third 的拒
        let _ = pending_1;
        let _ = pending_2;

        handle.request_stop();
        handle.join().unwrap();
    }

    // ────────────────────────────────────────────────────────────────────────
    // 6) forward error: ipc_thread channel disconnected → Internal
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn rr_server_replies_internal_when_ipc_thread_channel_disconnected() {
        let (handle, client, req_rx, _resp_tx) = bring_up(64, 64, 64);
        // 立即 drop req_rx 让 channel disconnected
        drop(req_rx);

        let pending = client
            .send(&build_clear_estop_request(
                "disc",
                "idem",
                "src",
                RpcRole::Maintainer,
                "c",
                "a",
                false,
                u64::MAX,
                0,
            ))
            .unwrap();

        let (status, rid, err_msg, _cleared, _dry_run) =
            wait_for_response(&pending, Duration::from_secs(2));
        assert_eq!(status, RpcStatus::Internal);
        assert_eq!(rid, "disc");
        assert!(
            err_msg.contains("disconnected"),
            "error_msg 应含 'disconnected'；实际 = {err_msg:?}"
        );

        let metrics = handle.metrics();
        assert!(metrics.forward_disconnected_errors.load(Ordering::Relaxed) >= 1);

        handle.request_stop();
        handle.join().unwrap();
    }

    // ────────────────────────────────────────────────────────────────────────
    // 7) orphan response：mock ipc_thread 发 unknown request_id；
    //    rr_server metrics + tracing 保命，不 panic（release 行为；debug
    //    会 hit debug_assert，但 test build profile 看是否 release）
    // ────────────────────────────────────────────────────────────────────────

    /// debug_assert 在 test profile（debug build）会触发；这条单测目的是
    /// 验证 release 不 panic + metrics 计数。我们用 `unbounded` channel 让
    /// rr_server 不阻塞。手动跳过 debug_assert：用 release-only sanity 测
    /// 不容易（cargo test 默认 debug）。改方案：跑 `metrics + tracing` 路径，
    /// 显式标 `#[cfg(not(debug_assertions))]` 守门，确保 debug build 跳过。
    #[test]
    #[cfg(not(debug_assertions))]
    fn rr_server_handles_orphan_response_no_panic_release_only() {
        let topic = fresh_topic("orphan");
        let test_node: &'static _ =
            Box::leak(Box::new(NodeBuilder::new().create::<IxService>().unwrap()));
        let _client =
            rr_client::<markers::ClearEStopRequest, markers::ClearEStopResponse>(test_node, &topic)
                .unwrap();

        let (req_tx, _req_rx) = unbounded::<ClearEStopRrRequest>();
        let (resp_tx, resp_rx) = unbounded::<ClearEStopRrResponse>();

        let cfg = RrServerThreadConfig {
            topic: topic.clone(),
            poll_interval_ns: 500_000,
            max_in_flight: 64,
        };
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let topic_for_factory = topic;
        let factory = move || {
            let server_node = NodeBuilder::new().create::<IxService>().unwrap();
            let server = rr_server::<markers::ClearEStopRequest, markers::ClearEStopResponse>(
                &server_node,
                &topic_for_factory,
            )
            .unwrap();
            Box::leak(Box::new(server_node));
            server
        };

        let handle = spawn_rr_server_thread(
            cfg,
            clock,
            factory,
            req_tx,
            resp_rx,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .expect("spawn_rr_server_thread");

        // 直接发 orphan response（pending 里没 "ghost"）
        resp_tx
            .send(ClearEStopRrResponse {
                request_id: "ghost".to_string(),
                payload: ClearEStopRrPayload::Cleared { cleared_at_ns: 1 },
            })
            .unwrap();
        thread::sleep(Duration::from_millis(50));

        let metrics = handle.metrics();
        assert!(metrics.orphan_responses.load(Ordering::Relaxed) >= 1);

        handle.request_stop();
        handle.join().unwrap();
    }

    // ────────────────────────────────────────────────────────────────────────
    // 8) dry_run 字段在 wire response 端正确回传
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn rr_server_round_trip_dry_run_preserves_dry_run_field_in_response() {
        let (handle, client, req_rx, resp_tx) = bring_up(64, 64, 64);
        let pending = client
            .send(&build_clear_estop_request(
                "dr",
                "idem",
                "src",
                RpcRole::Maintainer,
                "c",
                "a",
                /* dry_run */ true,
                u64::MAX,
                0,
            ))
            .unwrap();
        let req = wait_for_request(&req_rx, Duration::from_secs(2));
        assert!(req.dry_run, "forwarded request 应 carry dry_run=true");

        resp_tx
            .send(ClearEStopRrResponse {
                request_id: "dr".to_string(),
                payload: ClearEStopRrPayload::WouldClear {
                    would_clear_at_ns: 314_159,
                },
            })
            .unwrap();
        let (status, rid, _err, cleared, dry_run) =
            wait_for_response(&pending, Duration::from_secs(2));
        assert_eq!(status, RpcStatus::Ok);
        assert_eq!(rid, "dr");
        assert_eq!(cleared, 314_159);
        assert!(dry_run, "wire response 应回传 dry_run=true");

        handle.request_stop();
        handle.join().unwrap();
    }

    // ────────────────────────────────────────────────────────────────────────
    // 9) drain 顺序锁：先 drain rr_response_rx 再 drain wire request
    //    （reviewer 拍板：减少 backpressure 误报）
    // ────────────────────────────────────────────────────────────────────────

    /// **A1-3 (1/3c-2) round 1 拍板锁测**：daemon graceful shutdown 时，
    /// rr_server_thread 出 main loop 后 best-effort 给所有 in-flight
    /// pending 回 `RpcStatus::Aborted` + error_msg `"s0 daemon shutting
    /// down"`。
    ///
    /// 场景：发一条 live request，mock ipc_thread **不**回 response（pending
    /// 持续占着）；调 `handle.request_stop()` → main loop 退出 → run_loop
    /// 末尾 `abort_pending_on_shutdown` 跑；client 应收到 wire response
    /// status=Aborted, error_msg="s0 daemon shutting down", cleared_at_ns=0,
    /// dry_run 沿用原 request 字段；metrics.responses_sent == 1。
    #[test]
    fn rr_server_aborts_pending_on_shutdown() {
        let (handle, client, req_rx, _resp_tx) = bring_up(64, 64, 64);

        // 发 live request（dry_run=true 用于验证 wire response.dry_run 字段沿用）
        let pending = client
            .send(&build_clear_estop_request(
                "abort-on-shutdown",
                "idem",
                "src",
                RpcRole::Maintainer,
                "confirm",
                "audit",
                /* dry_run */ true,
                u64::MAX,
                0,
            ))
            .unwrap();
        // mock ipc_thread 收到 forwarded request 但**不**回 response
        let req = wait_for_request(&req_rx, Duration::from_secs(2));
        assert_eq!(req.request_id, "abort-on-shutdown");
        assert!(req.dry_run);

        // 调 handle.request_stop() → main loop 退出 → abort_pending_on_shutdown
        handle.request_stop();

        // client 应在 2s 内收到 wire response with RpcStatus::Aborted
        let (status, rid, err_msg, cleared_at_ns, dry_run) =
            wait_for_response(&pending, Duration::from_secs(2));
        assert_eq!(status, RpcStatus::Aborted);
        assert_eq!(rid, "abort-on-shutdown");
        assert_eq!(err_msg, "s0 daemon shutting down");
        assert_eq!(cleared_at_ns, 0, "shutdown abort 不伪造 cleared_at_ns");
        assert!(dry_run, "wire response.dry_run 沿用原 request");

        // join 等线程退出（main loop 已经 break，run_loop 已返回）
        let metrics_handle = std::thread::spawn({
            let metrics_arc = handle.metrics();
            move || {
                // shutdown 路径走完后 responses_sent 应至少 == 1
                metrics_arc.responses_sent.load(Ordering::Acquire)
            }
        });
        handle.join().unwrap();
        let responses_sent = metrics_handle.join().unwrap();
        assert!(
            responses_sent >= 1,
            "shutdown abort send_response 应使 responses_sent >= 1，实际 {responses_sent}"
        );
    }

    /// **A1-3 (1/3c-1) round 1 阻断锁测**：`pending.len() >= max_in_flight`
    /// 时新 wire request 必须立刻 reply `ResourceExhausted`，**不**等
    /// pending 释放、**不**挂在 iceoryx2 队列里到 deadline 超时。
    ///
    /// 场景：max_in_flight=1；第一条占 pending，mock ipc_thread 故意不回
    /// response（pending 持续满）；第二条进来必须在 2s 内收到
    /// `ResourceExhausted`（"rr_server pending full"），并且 `req_rx` **不**
    /// 看到第二条 forward，metrics.overflow_drops 计 1。
    #[test]
    fn rr_server_replies_resource_exhausted_when_pending_overflow() {
        let (handle, client, req_rx, _resp_tx) = bring_up(/* max_in_flight */ 1, 64, 64);

        // 第一条占 pending；不发 response 让 pending 持续满
        let _pending_1 = client
            .send(&build_clear_estop_request(
                "first",
                "idem",
                "src",
                RpcRole::Maintainer,
                "c1",
                "a1",
                false,
                u64::MAX,
                0,
            ))
            .unwrap();
        let req1 = wait_for_request(&req_rx, Duration::from_secs(2));
        assert_eq!(req1.request_id, "first");

        // 第二条进来：pending 满（1/1），必须立刻被 rr_server consume + reply
        // ResourceExhausted（不挂在 iceoryx2 队列里）
        let pending_2 = client
            .send(&build_clear_estop_request(
                "second",
                "idem",
                "src",
                RpcRole::Maintainer,
                "c2",
                "a2",
                false,
                u64::MAX,
                0,
            ))
            .unwrap();
        let (status, rid, err_msg, _cleared, _dry_run) =
            wait_for_response(&pending_2, Duration::from_secs(2));
        assert_eq!(status, RpcStatus::ResourceExhausted);
        assert_eq!(rid, "second");
        assert!(
            err_msg.contains("pending full"),
            "error_msg 应含 'pending full'；实际 = {err_msg:?}"
        );

        // mock ipc_thread 不应收到第二条 forward（pending 满拒不进 ipc_thread channel）
        assert!(
            req_rx.try_recv().is_err(),
            "第二条 wire request 不应被 forward 到 ipc_thread"
        );

        let metrics = handle.metrics();
        assert_eq!(metrics.overflow_drops.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.requests_forwarded.load(Ordering::Relaxed), 1);

        handle.request_stop();
        handle.join().unwrap();
    }

    /// 测试方法：max_in_flight=1 + 第一条 in flight 占用槽位；mock ipc_thread
    /// 提前发 response（释放槽位）；rr_server 主循环下一轮顺序：
    ///   §1) drain response → take pending → 空槽
    ///   §2) drain wire request → 第二条入新 pending
    /// 如果反过来（先 drain wire 再 drain response），第二条会因为 max_in_flight
    /// 满被丢（这条测试断言第二条最终成功 → 间接验证 §1 在前）。
    #[test]
    fn rr_server_drains_response_before_wire_request_to_release_pending_slot() {
        let (handle, client, req_rx, resp_tx) = bring_up(/* max_in_flight */ 1, 64, 64);

        let pending_1 = client
            .send(&build_clear_estop_request(
                "first",
                "idem",
                "src",
                RpcRole::Maintainer,
                "c1",
                "a1",
                false,
                u64::MAX,
                0,
            ))
            .unwrap();
        let _req1 = wait_for_request(&req_rx, Duration::from_secs(2));

        // 此时 pending 槽位已满（1/1）。先发 response 让槽位释放，再发第二条
        resp_tx
            .send(ClearEStopRrResponse {
                request_id: "first".to_string(),
                payload: ClearEStopRrPayload::Cleared { cleared_at_ns: 100 },
            })
            .unwrap();

        // wait 第一条 wire response（确认 §1 drain response 走通）
        let (s1, rid1, _err, c1, _dr) = wait_for_response(&pending_1, Duration::from_secs(2));
        assert_eq!(s1, RpcStatus::Ok);
        assert_eq!(rid1, "first");
        assert_eq!(c1, 100);

        // 现在 pending 槽位空，第二条来时应能 forward
        let pending_2 = client
            .send(&build_clear_estop_request(
                "second",
                "idem",
                "src",
                RpcRole::Maintainer,
                "c2",
                "a2",
                false,
                u64::MAX,
                0,
            ))
            .unwrap();
        let _req2 = wait_for_request(&req_rx, Duration::from_secs(2));
        resp_tx
            .send(ClearEStopRrResponse {
                request_id: "second".to_string(),
                payload: ClearEStopRrPayload::Cleared { cleared_at_ns: 200 },
            })
            .unwrap();
        let (s2, rid2, _err2, c2, _dr2) = wait_for_response(&pending_2, Duration::from_secs(2));
        assert_eq!(s2, RpcStatus::Ok);
        assert_eq!(rid2, "second");
        assert_eq!(c2, 200);

        handle.request_stop();
        handle.join().unwrap();
    }
}
