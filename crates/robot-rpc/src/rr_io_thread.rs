//! `rr_io_thread` —— gateway 端 RR I/O 线程（B4 (2/3-b) 起泛型 over
//! `(Req, Resp)` body marker，5 个 RPC 各一条线程：ClearEStop（A1 落地）+
//! 4 个 task control plane（task_invoke / task_cancel / task_status /
//! clear_error；B4 (2/3-b)）+ `StreamTaskEvents` 走 pubsub 不经此模块）。
//!
//! ## 角色
//!
//! reviewer A1-3 (2/3-a) 拍板的 RR I/O 隔离模型，跟 daemon 端
//! [`robot_s0::rr_server_thread`] / [`robot_s1::rr_server::runtime`] 对称。
//! 每个 RR topic 一个独立 `std::thread` + 一个独立
//! [`Iceoryx2RrClient<Req, Resp>`]（`!Send`，必须 single-owner）。
//!
//! - **本模块（gateway 端 client）**：泛型 spawn fn
//!   [`spawn_rr_io_thread`] 接 `Req: BodyMarker` / `Resp: BodyMarker`；
//!   typed 入参由 caller 闭包返 `Iceoryx2RrClient<Req, Resp>` 决定。从
//!   tokio gRPC handler 那边收 [`RrForwardRequest`]（含 `wire_bytes` +
//!   `oneshot::Sender<Vec<u8>>`）→ `client.send(bytes)` 拿
//!   [`Iceoryx2PendingResponse<Resp>`] → 入 in-flight `Vec`；每轮 poll
//!   `pending.try_recv()` → 收到 reply 通过 `oneshot.send` 回 handler。
//! - **daemon 端 server**：[`robot_s0::rr_server_thread`] (ClearEStop) +
//!   [`robot_s1::rr_server::runtime`] (4 个 task control plane) 落地；本
//!   模块不直接接它们，跨进程通过 iceoryx2 RR shm。
//!
//! ## 主循环顺序（500µs poll）
//!
//! 1. **drain `forward_rx`**：循环 `try_recv` 拿 [`RrForwardRequest`]，
//!    `client.send(bytes)` 入 in-flight。`client.send` 失败 → drop
//!    `oneshot_tx` → handler `oneshot_rx.await` 收 `RecvError` → 返
//!    `tonic::Code::Unavailable("gateway RR I/O unavailable")`（reviewer
//!    边界 2 拍板：错误发生在 gateway 本地 RR I/O，**不**说 daemon 失败）。
//! 2. **pre-pass cleanup**：遍历 in-flight，检测 `oneshot_tx.is_closed()`；
//!    closed（handler 已自身 timeout / gRPC client cancel）→ `swap_remove`
//!    并 drop `Iceoryx2PendingResponse`（取消 iceoryx2 pending），同时累加
//!    `handler_drops` metric。**不**等 daemon 永远不回，避免每次 handler
//!    timeout 都堆 in-flight pending（reviewer (2/3-b2-1) round 2 阻断）。
//! 3. **poll in-flight**：遍历 `Vec`，`pending.try_recv()` 拿到 reply →
//!    `oneshot.send(bytes)` + `swap_remove`；`Err` → drop `oneshot` 同样
//!    走 Unavailable 路径。
//!
//! ## shutdown 行为（A1-3 (2/3-b2-1) 范围）
//!
//! `request_stop()` 触发 main loop 退出后，残留 in-flight 在 `Vec` drop
//! 时自动释放 → 每条对应 `oneshot_tx` drop → handler `oneshot_rx.await`
//! 收 `RecvError` → 返 Unavailable。**本轮不**做"已进 handler 的请求等
//! RR 或 timeout"语义（reviewer 拍板留 (2/3-c)）；只保证 shutdown 不
//! panic、handler 收到明确 Unavailable 状态。
//!
//! reviewer 关键边界：rr_io_thread 生命周期**必须**在 tonic server 退出
//! 之后才 stop/join，避免"handler 还在等，RR I/O 被 gateway 自己掐掉"的
//! 竞态。本模块只提供 `RrIoThreadHandle::request_stop` + `join` API；
//! 调用顺序由 [`crate::gateway::run_gateway_with_ready`] (A1-3 (2/3-b2-2))
//! 控制。

#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::Receiver;
use robot_msg::markers::BodyMarker;
use robot_transport::iceoryx2::request_response::{Iceoryx2PendingResponse, Iceoryx2RrClient};

// ============================================================================
// RrForwardRequest
// ============================================================================

/// gRPC handler → rr_io_thread 的转发 payload。
///
/// reviewer A1-3 (2/3-b2-1) 决策点 1 拍板：`oneshot::Sender<Vec<u8>>`
/// 足够；RR I/O 内部失败就 drop sender，handler 统一返
/// `tonic::Code::Unavailable("gateway RR I/O unavailable")`。A1 不把 RR I/O
/// 细节暴露到 gRPC status。
pub struct RrForwardRequest {
    /// FlatBuffers `ClearEStopRequest` wire bytes（handler 端用
    /// [`robot_msg::build_helpers::build_clear_estop_request`] 构造）。
    pub wire_bytes: Vec<u8>,
    /// 拿到 wire `ClearEStopResponse` bytes 时通过本 sender 回 handler；
    /// rr_io_thread shutdown / send 失败 / poll 错误时 drop sender，
    /// handler `oneshot_rx.await` 收 `RecvError`。
    pub oneshot_tx: tokio::sync::oneshot::Sender<Vec<u8>>,
    /// 仅 tracing / debug 用；不参与 RR 路由（PendingResponse 本身已经
    /// 一对一绑定 wire request）。
    pub request_id: String,
}

// `oneshot::Sender<Vec<u8>>` 不实现 Debug；显式自定义 + allow
// `missing_fields_in_debug`（field 列表不全）跟 daemon 端
// `rr_server_thread::PendingRr` 同模式。
#[allow(clippy::missing_fields_in_debug)]
impl core::fmt::Debug for RrForwardRequest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RrForwardRequest")
            .field("request_id", &self.request_id)
            .field("wire_bytes_len", &self.wire_bytes.len())
            .finish()
    }
}

// ============================================================================
// Config / Metrics / Handle
// ============================================================================

/// `rr_io_thread` 配置。
#[derive(Clone, Debug)]
pub struct RrIoThreadConfig {
    /// iceoryx2 service topic（仅 doc / metrics label；实际 client 由
    /// `client_factory` closure 内部用 `rr_client(node, &topic)` 创建）。
    pub topic: String,
    /// 主循环 idle sleep（纳秒）。默认 500µs（2 kHz）。
    pub poll_interval_ns: u64,
}

impl Default for RrIoThreadConfig {
    fn default() -> Self {
        Self {
            topic: String::new(),
            poll_interval_ns: 500_000,
        }
    }
}

/// `rr_io_thread` 运行期 metrics。
#[derive(Default, Debug)]
pub struct RrIoMetrics {
    /// `client.send(bytes)` 成功 + push 进 in-flight 的请求数。
    pub requests_sent: AtomicU64,
    /// `pending.try_recv()` 拿到 daemon reply 的次数（**不论** `oneshot.send`
    /// 是否成功；rx 已 drop 时 send 失败仍计入，那条 reply 直接丢弃）。
    pub responses_received: AtomicU64,
    /// `client.send(bytes)` 失败的次数（loan_slice / send 内部错；drop
    /// oneshot → handler Unavailable）。
    pub send_errors: AtomicU64,
    /// `pending.try_recv()` 返 `Err` 的次数（drop oneshot → handler
    /// Unavailable）。
    pub poll_errors: AtomicU64,
    /// handler 端提前 drop `oneshot_rx`（自身 `timeout_ms +
    /// rr_timeout_grace_ms` 触发 / gRPC client cancel）后，rr_io_thread
    /// 在 poll 前检测 `oneshot_tx.is_closed()` 主动 swap_remove + drop
    /// pending（取消 iceoryx2 PendingResponse）的次数。**不**等 daemon 回
    /// 包，避免每次 handler timeout 都堆 in-flight pending。
    pub handler_drops: AtomicU64,
    /// shutdown 时 in-flight `Vec` drop 释放的请求数（每条 oneshot drop
    /// → handler 收 RecvError → Unavailable）。
    pub orphan_drops_on_shutdown: AtomicU64,
    /// v0.5 候选 E 段(8)：主循环每次 poll 迭代 +1（**空闲亦推进**）——systemd
    /// watchdog liveness 源（D12 r3 T4 第 1 条：不用 `requests_sent` 等请求计数）。
    pub loop_tick_count: AtomicU64,
    /// test-only：loop 顶端 freeze 截止（absolute ns，0 = 未冻结）。
    #[cfg(any(test, feature = "test-freeze-hook"))]
    pub freeze_until_ns: AtomicU64,
}

/// `rr_io_thread` 句柄。caller（`run_gateway_with_ready`）通过
/// `request_stop` + `join` 关停。reviewer 关键边界：必须**先** tonic
/// server 退出再 stop/join，避免 handler 还在等时 RR I/O 被掐。
pub struct RrIoThreadHandle {
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    metrics: Arc<RrIoMetrics>,
}

impl RrIoThreadHandle {
    /// 取 metrics 引用观察运行期统计。
    #[must_use]
    pub fn metrics(&self) -> Arc<RrIoMetrics> {
        Arc::clone(&self.metrics)
    }

    /// 通知 main loop 退出（下一轮 stop check 命中即跳出）。
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

// ============================================================================
// In-flight 元数据
// ============================================================================

/// in-flight 请求的元数据。`pending` 跟 `oneshot_tx` 一对一绑定（client.send
/// 返 pending；handler 持 oneshot_rx）。`request_id` 仅 tracing 用。
///
/// **泛型 over `Resp: BodyMarker`**（B4 (2/3-b)）：5 个 RR topic 各自一
/// 个 typed `Iceoryx2PendingResponse<Resp>`；wire bytes 在 `oneshot_tx`
/// 端仍是 `Vec<u8>`（解码留给 handler 内部 `flatbuffers::root::<FbResp>`
/// 做）。
struct InFlight<Resp: BodyMarker> {
    pending: Iceoryx2PendingResponse<Resp>,
    oneshot_tx: tokio::sync::oneshot::Sender<Vec<u8>>,
    request_id: String,
}

// ============================================================================
// spawn API
// ============================================================================

/// 起 `rr_io_thread`，**泛型 over `(Req, Resp)` body marker**（B4 (2/3-b)）。
///
/// `client_factory` closure 在新线程内执行（iceoryx2 RR client `!Send`，
/// 必须在 owner 线程内构造；同 daemon 端 `rr_server_thread` factory 模式）。
/// 返 `Iceoryx2RrClient<Req, Resp>` 决定 `Req` / `Resp` 类型：
///
/// - `(ClearEStopRequest, ClearEStopResponse)` → ClearEStop RR 通道（A1）
/// - `(InvokeTaskRequest, InvokeTaskResponse)` → task_invoke（B4）
/// - `(CancelTaskRequest, CancelTaskResponse)` → task_cancel（B4）
/// - `(GetTaskStatusRequest, GetTaskStatusResponse)` → task_status（B4）
/// - `(ClearErrorRequest, ClearErrorResponse)` → clear_error（B4）
///
/// `forward_rx` 跟 handler 持的 `Sender<RrForwardRequest>` 是同对：handler
/// async 收 gRPC 后 `try_send` 转发；rr_io_thread `try_recv` drain。channel
/// buffer 由 caller 决定（reviewer 拍板 `bounded(64)` 跟 daemon 端一致）。
///
/// `cfg.topic` 决定线程名 (`gw_rr_io_<topic-safe>`)，方便 audit 日志区分
/// 5 条 RR I/O 线程。`topic` 串以 `/` 开头会被替换成 `_`（thread name 不
/// 接 `/`）。
///
/// # Errors
///
/// - `client_factory` panic（factory 起 iceoryx2 service 失败 / port 耗尽）
/// - `thread::Builder::spawn` 失败（极罕见）
pub fn spawn_rr_io_thread<Req, Resp, F>(
    cfg: &RrIoThreadConfig,
    client_factory: F,
    forward_rx: Receiver<RrForwardRequest>,
) -> std::io::Result<RrIoThreadHandle>
where
    Req: BodyMarker,
    Resp: BodyMarker,
    F: FnOnce() -> Iceoryx2RrClient<Req, Resp> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let metrics = Arc::new(RrIoMetrics::default());

    let stop_clone = Arc::clone(&stop);
    let metrics_clone = Arc::clone(&metrics);
    let cfg_clone = cfg.clone();

    // 同步 ready signal：spawn 返回前确认 client_factory 已成功执行。
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

    // thread name 从 topic 串派生，方便多 RR 通道并存时 audit 日志区分
    // （per-process 唯一性：5 条 RR topic 串各不同）。
    let thread_name = thread_name_for(&cfg.topic);

    let join = thread::Builder::new().name(thread_name).spawn(move || {
        let client = client_factory();
        let _ = ready_tx.send(());
        drop(ready_tx);
        run_loop::<Req, Resp>(
            &cfg_clone,
            &client,
            &forward_rx,
            &stop_clone,
            &metrics_clone,
        );
    })?;

    if ready_rx.recv().is_err() {
        return Err(std::io::Error::other(
            "rr_io_thread client_factory panicked before signalling ready",
        ));
    }

    Ok(RrIoThreadHandle {
        join,
        stop,
        metrics,
    })
}

/// 把 iceoryx2 service topic 串转 thread name。`/` 替成 `_`，前缀
/// `gw_rr_io_`，截断到 14 字符（pthread thread name 上限 16 含 NUL；
/// `gw_rr_io_` 9 + 上限 7 字符即 16 截断）。
fn thread_name_for(topic: &str) -> String {
    // pthread name 上限是 16（含 NUL），gw_rr_io_ 9 字符；剩 6 字符后缀。
    // topic 串末尾通常是 method 名（如 "task_invoke"），直接截后缀几乎一定
    // 撞名（"task_i"），所以用 hash 后缀更稳。但为了 debuggable 优先取
    // topic 末尾 method 名截断 —— prod 5 条 topic 末尾不重复（clear_estop /
    // task_invoke / task_cancel / task_status / clear_error）。
    let suffix = topic
        .rsplit('/')
        .next()
        .unwrap_or(topic)
        .chars()
        .take(6)
        .collect::<String>();
    format!("gw_rr_io_{suffix}")
}

// ============================================================================
// 主循环
// ============================================================================

fn run_loop<Req: BodyMarker, Resp: BodyMarker>(
    cfg: &RrIoThreadConfig,
    client: &Iceoryx2RrClient<Req, Resp>,
    forward_rx: &Receiver<RrForwardRequest>,
    stop: &AtomicBool,
    metrics: &RrIoMetrics,
) {
    let mut in_flight: Vec<InFlight<Resp>> = Vec::new();

    while !stop.load(Ordering::Acquire) {
        // v0.5 候选 E 段(8)：liveness 计数（空闲亦推进）+ test-only freeze hook。
        metrics.loop_tick_count.fetch_add(1, Ordering::Relaxed);
        #[cfg(any(test, feature = "test-freeze-hook"))]
        crate::liveness::freeze_check(
            &metrics.freeze_until_ns,
            &robot_core::time::SystemClock::new(),
            stop,
        );
        // ─── 1) drain forward_rx → client.send → push 进 in-flight ───
        while let Ok(req) = forward_rx.try_recv() {
            match client.send(&req.wire_bytes) {
                Ok(pending) => {
                    in_flight.push(InFlight {
                        pending,
                        oneshot_tx: req.oneshot_tx,
                        request_id: req.request_id,
                    });
                    metrics.requests_sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::error!(
                        error = ?e,
                        request_id = %req.request_id,
                        "rr_io_thread: client.send failed; drop oneshot → handler returns Unavailable"
                    );
                    metrics.send_errors.fetch_add(1, Ordering::Relaxed);
                    drop(req.oneshot_tx);
                }
            }
        }

        // ─── 2) pre-pass：handler 端提前 drop oneshot_rx（自身 timeout /
        //     gRPC client cancel）的 in-flight 主动清掉，drop pending 取消
        //     iceoryx2 PendingResponse，避免堆积等到 daemon 永远不回。
        //     reviewer (2/3-b2-1) round 2 阻断：handler timeout 跟主动取消
        //     必须在 rr_io_thread 端真清 in-flight，不能等 shutdown。
        let mut i = 0;
        while i < in_flight.len() {
            if in_flight[i].oneshot_tx.is_closed() {
                let item = in_flight.swap_remove(i);
                metrics.handler_drops.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    request_id = %item.request_id,
                    "rr_io_thread: handler oneshot_rx already dropped; cancelling in-flight pending"
                );
                drop(item);
                // swap_remove 把最后一个移到 i；本轮再检查同位置
            } else {
                i += 1;
            }
        }

        // ─── 3) poll in-flight → 收到 reply → fire oneshot + remove ───
        let mut i = 0;
        while i < in_flight.len() {
            match in_flight[i].pending.try_recv() {
                Ok(Some(bytes)) => {
                    let item = in_flight.swap_remove(i);
                    metrics.responses_received.fetch_add(1, Ordering::Relaxed);
                    // oneshot.send 失败 = handler oneshot_rx 已 drop（handler
                    // 已经因为自己的 timeout / cancel 走人）；这条 wire
                    // response 直接丢弃，不阻塞 rr_io_thread。
                    if item.oneshot_tx.send(bytes).is_err() {
                        tracing::warn!(
                            request_id = %item.request_id,
                            "rr_io_thread: oneshot recv side dropped (handler already returned); reply discarded"
                        );
                    }
                    // swap_remove 把最后一个移到 i 位置；不 i += 1 让本轮再
                    // poll 同位置（新的最后一个）
                }
                Ok(None) => {
                    i += 1;
                }
                Err(e) => {
                    tracing::error!(
                        error = ?e,
                        request_id = %in_flight[i].request_id,
                        "rr_io_thread: pending.try_recv failed; drop oneshot → handler returns Unavailable"
                    );
                    metrics.poll_errors.fetch_add(1, Ordering::Relaxed);
                    let item = in_flight.swap_remove(i);
                    drop(item.oneshot_tx);
                }
            }
        }

        thread::sleep(Duration::from_nanos(cfg.poll_interval_ns));
    }

    // ─── shutdown：in-flight Vec drop → 残留 oneshot 自动 drop →
    //     handler 端 oneshot_rx.await 收 RecvError → 返 Unavailable
    let dropped = u64::try_from(in_flight.len()).unwrap_or(u64::MAX);
    if dropped > 0 {
        tracing::info!(
            dropped_pending = dropped,
            "rr_io_thread: shutdown with {dropped} in-flight pending; dropping → handler returns Unavailable"
        );
    }
    metrics
        .orphan_drops_on_shutdown
        .fetch_add(dropped, Ordering::Relaxed);
    // in_flight Vec 在此 drop（Iceoryx2PendingResponse + oneshot_tx 都释放）
    drop(in_flight);
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::AtomicU32;

    use crossbeam_channel::bounded;
    use iceoryx2::prelude::NodeBuilder;
    use iceoryx2::service::ipc::Service as IxService;
    use robot_msg::build_helpers::{build_clear_estop_request, build_clear_estop_response};
    use robot_msg::markers;
    use robot_msg::msg::{ClearEStopResponse as FbResp, RpcRole, RpcStatus};
    use robot_transport::iceoryx2::request_response::{Iceoryx2RrServer, rr_client, rr_server};

    fn fresh_topic(suffix: &str) -> String {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        format!("/rr_io_thread_test_{}_{}_{suffix}", std::process::id(), seq)
    }

    /// 起 rr_io_thread + 配套 mock daemon RR server。
    ///
    /// 跟 daemon 端 rr_server_thread 测试用同一对称模式：rr_io_thread 内
    /// 独立 Node + rr_client；mock daemon server 在 test 主线程独立 Node +
    /// rr_server。
    #[allow(clippy::type_complexity)]
    fn bring_up(
        forward_buf: usize,
    ) -> (
        RrIoThreadHandle,
        Iceoryx2RrServer<markers::ClearEStopRequest, markers::ClearEStopResponse>,
        crossbeam_channel::Sender<RrForwardRequest>,
    ) {
        let topic = fresh_topic("bringup");

        // daemon 端 mock server (test 主线程持)
        let server_node: &'static _ =
            Box::leak(Box::new(NodeBuilder::new().create::<IxService>().unwrap()));
        let server = rr_server::<markers::ClearEStopRequest, markers::ClearEStopResponse>(
            server_node,
            &topic,
        )
        .expect("rr_server create");

        let (forward_tx, forward_rx) = bounded::<RrForwardRequest>(forward_buf);

        let cfg = RrIoThreadConfig {
            topic: topic.clone(),
            poll_interval_ns: 500_000,
        };
        let topic_for_factory = topic;
        let client_factory = move || {
            let client_node = NodeBuilder::new().create::<IxService>().unwrap();
            let client = rr_client::<markers::ClearEStopRequest, markers::ClearEStopResponse>(
                &client_node,
                &topic_for_factory,
            )
            .expect("rr_client create");
            Box::leak(Box::new(client_node));
            client
        };

        let handle = spawn_rr_io_thread(&cfg, client_factory, forward_rx).expect("spawn rr_io");

        (handle, server, forward_tx)
    }

    /// daemon 端 mock：等到一条 wire request → reply 给定 wire response bytes。
    fn mock_daemon_reply(
        server: &Iceoryx2RrServer<markers::ClearEStopRequest, markers::ClearEStopResponse>,
        reply_bytes: &[u8],
        max: Duration,
    ) {
        let start = std::time::Instant::now();
        loop {
            if let Some(active) = server.try_recv().expect("server recv") {
                active.send_response(reply_bytes).expect("send_response");
                return;
            }
            assert!(
                start.elapsed() < max,
                "mock daemon did not receive request within {max:?}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn build_test_request_bytes(request_id: &str, dry_run: bool) -> Vec<u8> {
        build_clear_estop_request(
            request_id,
            "idem",
            "src.test",
            RpcRole::Maintainer,
            "CONFIRM",
            "audit",
            dry_run,
            u64::MAX,
            0,
        )
    }

    fn build_test_ok_response_bytes(request_id: &str, cleared_at_ns: u64) -> Vec<u8> {
        build_clear_estop_response(request_id, RpcStatus::Ok, "", cleared_at_ns, false)
    }

    // ────────────────────────────────────────────────────────────────────────
    // 1) round-trip 端到端：handler 发 → rr_io_thread 转发 → daemon → reply
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn rr_io_thread_round_trip_forwards_request_and_replies_to_oneshot() {
        let (handle, server, forward_tx) = bring_up(64);

        // handler 端发 forward request + 持 oneshot_rx
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        forward_tx
            .send(RrForwardRequest {
                wire_bytes: build_test_request_bytes("req-1", false),
                oneshot_tx,
                request_id: "req-1".to_string(),
            })
            .unwrap();

        // mock daemon 收到 + reply Ok
        mock_daemon_reply(
            &server,
            &build_test_ok_response_bytes("req-1", 999_999),
            Duration::from_secs(2),
        );

        // handler 端收到 wire response bytes
        let bytes = oneshot_rx.blocking_recv().expect("oneshot_rx not dropped");
        let view = flatbuffers::root::<FbResp>(&bytes).expect("decode");
        assert_eq!(view.request_id().unwrap_or(""), "req-1");
        assert_eq!(view.status(), RpcStatus::Ok);
        assert_eq!(view.cleared_at_ns(), 999_999);

        let metrics = handle.metrics();
        assert_eq!(metrics.requests_sent.load(Ordering::Acquire), 1);
        assert_eq!(metrics.responses_received.load(Ordering::Acquire), 1);

        handle.request_stop();
        handle.join().unwrap();
    }

    // ────────────────────────────────────────────────────────────────────────
    // 2) 多个 in-flight + 反序 reply（PendingResponse 一对一绑定 client.send）
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn rr_io_thread_concurrent_in_flight_routes_replies_correctly() {
        let (handle, server, forward_tx) = bring_up(64);

        let (sender_a, recver_a) = tokio::sync::oneshot::channel();
        let (sender_b, recver_b) = tokio::sync::oneshot::channel();
        forward_tx
            .send(RrForwardRequest {
                wire_bytes: build_test_request_bytes("req-A", false),
                oneshot_tx: sender_a,
                request_id: "req-A".to_string(),
            })
            .unwrap();
        forward_tx
            .send(RrForwardRequest {
                wire_bytes: build_test_request_bytes("req-B", true),
                oneshot_tx: sender_b,
                request_id: "req-B".to_string(),
            })
            .unwrap();

        // 等到两条 active request；iceoryx2 RR 内部到达顺序未定，按 wire
        // request_id 解出来再 reply 各自的 wire response。
        let wait_active = || {
            let start = std::time::Instant::now();
            loop {
                if let Some(a) = server.try_recv().unwrap() {
                    return a;
                }
                assert!(
                    start.elapsed() <= Duration::from_secs(2),
                    "active request not received within 2s"
                );
                thread::sleep(Duration::from_millis(2));
            }
        };
        let active_first = wait_active();
        let active_second = wait_active();

        let id_first =
            flatbuffers::root::<robot_msg::msg::ClearEStopRequest>(active_first.payload())
                .unwrap()
                .request_id()
                .unwrap_or("")
                .to_string();
        let id_second =
            flatbuffers::root::<robot_msg::msg::ClearEStopRequest>(active_second.payload())
                .unwrap()
                .request_id()
                .unwrap_or("")
                .to_string();

        // 反序 reply：先 reply 后到的，再 reply 先到的
        active_second
            .send_response(&build_test_ok_response_bytes(&id_second, 200))
            .unwrap();
        active_first
            .send_response(&build_test_ok_response_bytes(&id_first, 100))
            .unwrap();

        // handler 端：req-A oneshot 收 wire response 对应 request_id
        let bytes_a = recver_a.blocking_recv().expect("recver_a");
        let bytes_b = recver_b.blocking_recv().expect("recver_b");
        let view_a = flatbuffers::root::<FbResp>(&bytes_a).unwrap();
        let view_b = flatbuffers::root::<FbResp>(&bytes_b).unwrap();
        assert_eq!(view_a.request_id().unwrap_or(""), "req-A");
        assert_eq!(view_b.request_id().unwrap_or(""), "req-B");

        let metrics = handle.metrics();
        assert_eq!(metrics.requests_sent.load(Ordering::Acquire), 2);
        assert_eq!(metrics.responses_received.load(Ordering::Acquire), 2);

        handle.request_stop();
        handle.join().unwrap();
    }

    // ────────────────────────────────────────────────────────────────────────
    // 3) shutdown drops 残留 in-flight oneshot → handler 收 RecvError
    // ────────────────────────────────────────────────────────────────────────

    /// reviewer 边界 2 拍板：shutdown 时 in-flight pending 没回 reply
    /// → drop oneshot → handler `oneshot_rx.await` 收 RecvError → 返
    /// `Code::Unavailable("gateway RR I/O unavailable")`。
    #[test]
    fn rr_io_thread_shutdown_drops_oneshot_for_in_flight_pending() {
        let (handle, _server, forward_tx) = bring_up(64);

        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        forward_tx
            .send(RrForwardRequest {
                wire_bytes: build_test_request_bytes("req-pending", false),
                oneshot_tx,
                request_id: "req-pending".to_string(),
            })
            .unwrap();

        // 等 rr_io_thread drain forward + 入 in-flight（mock daemon 不 reply）
        let metrics = handle.metrics();
        let start = std::time::Instant::now();
        while metrics.requests_sent.load(Ordering::Acquire) == 0 {
            assert!(
                start.elapsed() <= Duration::from_secs(2),
                "rr_io_thread did not drain forward within 2s"
            );
            thread::sleep(Duration::from_millis(5));
        }

        // 触发 shutdown
        handle.request_stop();
        // 让 main loop 跑完 + Vec drop
        let stop_handle = handle.metrics();
        let join_h = handle.join();
        let _ = join_h;

        // handler 端 oneshot_rx 应收到 RecvError（Closed = sender drop）
        let result = oneshot_rx.blocking_recv();
        assert!(
            result.is_err(),
            "shutdown 应 drop oneshot；handler 收 RecvError"
        );

        // metrics: orphan_drops >= 1
        assert!(
            stop_handle.orphan_drops_on_shutdown.load(Ordering::Acquire) >= 1,
            "metrics.orphan_drops_on_shutdown 应 >= 1"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // 4) handler 端 oneshot_rx 提前 drop（handler timeout / gRPC cancel）→
    //    rr_io_thread pre-pass 检测 is_closed → swap_remove + drop pending →
    //    handler_drops += 1；不等 daemon reply 也不堆 in-flight
    // ────────────────────────────────────────────────────────────────────────

    /// reviewer (2/3-b2-1) round 2 阻断锁测：handler 端提前 drop
    /// `oneshot_rx`（自身 `timeout_ms + rr_timeout_grace_ms` 触发或 gRPC
    /// client cancel）后，rr_io_thread 必须在下一轮 poll 前 pre-pass
    /// 检测 `oneshot_tx.is_closed()` → `swap_remove` + drop
    /// `Iceoryx2PendingResponse`（取消 iceoryx2 pending），**不**等 daemon
    /// 永远不回 → 累 `handler_drops` metric。shutdown 时 in-flight 已空，
    /// `orphan_drops_on_shutdown` 应为 0。
    #[test]
    fn rr_io_thread_pre_pass_cleans_up_when_handler_drops_oneshot_rx_before_reply() {
        // mock daemon RR server 创建后**故意**不收 / 不 reply（test 主线程
        // 不调 mock_daemon_reply）；必须靠 pre-pass 主动清在 daemon 之前。
        let (handle, _server, forward_tx) = bring_up(64);

        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        forward_tx
            .send(RrForwardRequest {
                wire_bytes: build_test_request_bytes("req-cancel", false),
                oneshot_tx,
                request_id: "req-cancel".to_string(),
            })
            .unwrap();

        // 等 rr_io_thread drain + client.send + push in-flight
        let metrics = handle.metrics();
        let start = std::time::Instant::now();
        while metrics.requests_sent.load(Ordering::Acquire) == 0 {
            assert!(
                start.elapsed() <= Duration::from_secs(2),
                "rr_io_thread did not drain forward within 2s"
            );
            thread::sleep(Duration::from_millis(5));
        }

        // handler 端模拟 timeout / cancel：drop oneshot_rx
        drop(oneshot_rx);

        // 下一轮 pre-pass 检测 is_closed → swap_remove + drop pending →
        // handler_drops += 1（daemon 一直不 reply 也不会堆 in-flight）
        let start = std::time::Instant::now();
        while metrics.handler_drops.load(Ordering::Acquire) == 0 {
            assert!(
                start.elapsed() <= Duration::from_secs(2),
                "rr_io_thread did not pre-pass-drop closed in-flight within 2s"
            );
            thread::sleep(Duration::from_millis(5));
        }

        handle.request_stop();
        handle.join().unwrap();

        // 关键断言：
        //  - handler_drops == 1（pre-pass 清掉一条 closed in-flight）
        //  - responses_received == 0（daemon 一直没 reply；gateway 也没在
        //    in-flight Vec 等到 reply）
        //  - orphan_drops_on_shutdown == 0（pre-pass 已 drop in-flight，
        //    shutdown 时 Vec 已空，不再 orphan-drop）
        assert_eq!(
            metrics.handler_drops.load(Ordering::Acquire),
            1,
            "pre-pass 应 drop 一条 closed in-flight"
        );
        assert_eq!(
            metrics.responses_received.load(Ordering::Acquire),
            0,
            "daemon 一直没 reply，responses_received 应为 0"
        );
        assert_eq!(
            metrics.orphan_drops_on_shutdown.load(Ordering::Acquire),
            0,
            "in-flight 应在 shutdown 前被 pre-pass 清掉，shutdown 时不应再 orphan-drop"
        );
    }
}
