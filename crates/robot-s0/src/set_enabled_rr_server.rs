//! `set_enabled_rr_server` —— v0.5 候选 C `SetEnabled` RR server 线程。
//!
//! ## 角色
//!
//! D5 拍板的线程模型：**第 2 个专用 rr_server**（topic
//! `/<robot_id>/control/set_enabled`），**ClearEStop `rr_server_thread` 0 改**。
//! 本模块沿 `rr_server_thread`（A1-3）**copy** 范式，仅做 iceoryx2 RR I/O：
//!
//! - 收 wire `SetEnabledRequest` → decode → 转发 [`SetEnabledRrRequest`] 到
//!   `ipc_thread`（crossbeam channel）；
//! - 接 `ipc_thread` 派发的 [`SetEnabledRrResponse`] → encode wire
//!   `SetEnabledResponse` → `active.send_response(bytes)`。
//!   **不**持 action 状态、**不**碰 state machine / adapter、**不**做 deadline
//!   gate（deadline 权威在 `ipc_thread` §5.5b drain + §6.6b pending）。
//!
//! ## 与 ClearEStop rr_server 的差异
//!
//! 1. **response 无 `dry_run` 字段**：`set_enabled.fbs` `SetEnabledResponse`
//!    无 `dry_run`（ClearEStop 有），故 [`PendingRr`] 不回传 `dry_run`。
//! 2. **encode 带 domain**：`build_set_enabled_response` 多一个 `domain:
//!    SetEnabledStatus` 参数（细粒度审计 enum），wire `status: RpcStatus` 另带。
//! 3. **多 tick 语义在 `ipc_thread`**：本 server 仍只做 I/O；pending 观测在
//!    `ipc_thread`（区别仅在 response 何时回来，对本 server 透明）。
//!
//! 复用 `rr_server_thread` 的 pub [`RrServerThreadConfig`] / [`RrServerMetrics`]
//! （纯配置 + 计数器，无行为）；句柄 [`SetEnabledRrServerHandle`] 本模块自带
//! （`rr_server_thread` 的 handle 字段私有，不可跨模块构造）。
//!
//! ## shutdown 行为
//!
//! `request_stop()` 后 `run_loop` 调 [`abort_pending_on_shutdown`]：给每条
//! in-flight active best-effort 回 `RpcStatus::Aborted` + domain
//! `SetEnabledStatus::PreemptedByShutdown` + `error_msg = "s0 daemon shutting
//! down"`。**不**伪造 Ack；**不** drain 等 ipc_thread。沿 ClearEStop 拍板。

#![allow(clippy::doc_markdown)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError};
use robot_core::time::RobotClock;
use robot_msg::build_helpers::build_set_enabled_response;
use robot_msg::markers;
use robot_msg::msg::{RpcStatus, SetEnabledRequest as FbReq, SetEnabledStatus};
use robot_transport::iceoryx2::request_response::{Iceoryx2ActiveRequest, Iceoryx2RrServer};

use crate::ipc_thread::{SetEnabledRrPayload, SetEnabledRrRequest, SetEnabledRrResponse};
use crate::rr_server_thread::{RrServerMetrics, RrServerThreadConfig};

// ============================================================================
// Handle
// ============================================================================

/// `set_enabled_rr_server` 句柄。caller 通过 `request_stop()` + `join()` 关停。
pub struct SetEnabledRrServerHandle {
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    metrics: Arc<RrServerMetrics>,
}

impl SetEnabledRrServerHandle {
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

/// in-flight RR request 元数据。按 `request_id` 跟 ipc_thread response 关联。
/// **区别 ClearEStop**：`SetEnabledResponse` 无 `dry_run` 字段，故不回传。
struct PendingRr {
    request_id: String,
    /// owned active request handle；`send_response` 时 by-value consume。
    active: Iceoryx2ActiveRequest<markers::SetEnabledRequest, markers::SetEnabledResponse>,
}

// ============================================================================
// spawn API
// ============================================================================

/// 起 `set_enabled_rr_server` 线程。
///
/// `server_factory` closure 在新线程内执行（iceoryx2 RR server `!Send`，必须
/// 在 owner 线程内构造；同 `rr_server_thread::spawn_rr_server_thread` 模式）。
///
/// `rr_request_tx` / `rr_response_rx` 跟 `ipc_thread::SetEnabledRrChannels::
/// request_rx` / `response_tx` 是同对；channel buffer `bounded(64)`。
///
/// # Errors
///
/// - `server_factory` panic（iceoryx2 service 起失败 / port 耗尽等）
/// - `thread::Builder::spawn` 失败（极罕见）
pub fn spawn_set_enabled_rr_server_thread<F>(
    cfg: RrServerThreadConfig,
    clock: Arc<dyn RobotClock>,
    server_factory: F,
    rr_request_tx: Sender<SetEnabledRrRequest>,
    rr_response_rx: Receiver<SetEnabledRrResponse>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<SetEnabledRrServerHandle>
where
    F: FnOnce() -> Iceoryx2RrServer<markers::SetEnabledRequest, markers::SetEnabledResponse>
        + Send
        + 'static,
{
    crate::rr_server_thread::validate_rr_capacity(
        cfg.max_in_flight,
        rr_request_tx.capacity(),
        rr_response_rx.capacity(),
    )?;
    #[cfg(feature = "test-comm-fault-hook")]
    crate::comm_fault_hook::before_spawn("set_rr")?;
    let metrics = Arc::new(RrServerMetrics::default());

    let stop_clone = Arc::clone(&stop);
    let metrics_clone = Arc::clone(&metrics);

    // 同步 ready signal：spawn 返回前确认 server_factory 已成功执行。
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

    let join = thread::Builder::new()
        .name("s0_set_enabled_rr_server".into())
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
            "set_enabled_rr_server server_factory panicked before signalling ready",
        ));
    }

    Ok(SetEnabledRrServerHandle {
        join,
        stop,
        metrics,
    })
}

// ============================================================================
// 主循环
// ============================================================================

#[allow(clippy::too_many_lines)]
fn run_loop(
    cfg: &RrServerThreadConfig,
    clock: &dyn RobotClock,
    server: &Iceoryx2RrServer<markers::SetEnabledRequest, markers::SetEnabledResponse>,
    rr_request_tx: &Sender<SetEnabledRrRequest>,
    rr_response_rx: &Receiver<SetEnabledRrResponse>,
    stop: &AtomicBool,
    metrics: &RrServerMetrics,
) {
    let mut pending: VecDeque<PendingRr> = VecDeque::with_capacity(cfg.max_in_flight);

    while crate::worker_stop::worker_running(
        crate::worker_stop::WorkerKind::SetEnabledRr,
        clock,
        stop,
    ) {
        // ─── 1) drain rr_response_rx 先（释放 pending 容量减少误报 backpressure）───
        while let Ok(resp) = rr_response_rx.try_recv() {
            handle_response(&mut pending, &resp, metrics);
        }

        // ─── 2) drain wire request from server ───
        //
        // 即使 `pending.len() >= max_in_flight` 仍继续 try_recv 把 wire request
        // 从 iceoryx2 队列拿出 → 直接 reply `ResourceExhausted`（不挂到 deadline
        // 超时；沿 ClearEStop rr_server round 1 阻断修复）。
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
                    tracing::error!(error = ?e, "set_enabled_rr_server: server.try_recv failed");
                    break;
                }
            }
        }

        thread::sleep(Duration::from_nanos(cfg.poll_interval_ns));
    }

    abort_pending_on_shutdown(&mut pending, metrics);
}

/// 退出前 best-effort 给 pending 全部 in-flight active 回 `RpcStatus::Aborted`
/// （domain `PreemptedByShutdown`，`error_msg = "s0 daemon shutting down"`）。
/// **不**伪造 Ack、**不** drain 等 ipc_thread（沿 ClearEStop 拍板）。
fn abort_pending_on_shutdown(pending: &mut VecDeque<PendingRr>, metrics: &RrServerMetrics) {
    if pending.is_empty() {
        return;
    }
    tracing::info!(
        pending_count = pending.len(),
        "set_enabled_rr_server: shutting down; aborting pending with RpcStatus::Aborted"
    );
    while let Some(p) = pending.pop_front() {
        let bytes = build_set_enabled_response(
            &p.request_id,
            RpcStatus::Aborted,
            SetEnabledStatus::PreemptedByShutdown,
            0,
            "s0 daemon shutting down",
        );
        if let Err(e) = p.active.send_response(&bytes) {
            tracing::error!(
                error = ?e,
                request_id = %p.request_id,
                "set_enabled_rr_server: shutdown abort send_response failed"
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
    active: Iceoryx2ActiveRequest<markers::SetEnabledRequest, markers::SetEnabledResponse>,
    pending: &mut VecDeque<PendingRr>,
    rr_request_tx: &Sender<SetEnabledRrRequest>,
    metrics: &RrServerMetrics,
) {
    let payload = active.payload();

    // FlatBuffers root verifier
    let view = match flatbuffers::root::<FbReq>(payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = ?e, "set_enabled_rr_server: wire decode failed");
            metrics.decode_errors.fetch_add(1, Ordering::Relaxed);
            send_error_reply(
                active,
                "",
                RpcStatus::InvalidArgument,
                SetEnabledStatus::Unknown,
                "wire decode failed",
                metrics,
            );
            return;
        }
    };

    let request_id = view.request_id().unwrap_or("").to_string();

    // 必填字段：空 request_id 拒。
    if request_id.is_empty() {
        tracing::warn!("set_enabled_rr_server: empty request_id");
        metrics.decode_errors.fetch_add(1, Ordering::Relaxed);
        send_error_reply(
            active,
            "",
            RpcStatus::InvalidArgument,
            SetEnabledStatus::Unknown,
            "request_id is empty",
            metrics,
        );
        return;
    }

    // 重复 in-flight request_id：直接 reply InvalidArgument 不 forward（防 response
    // 按 request_id 查找把第二条状态回给第一个 active handle）。
    if pending.iter().any(|p| p.request_id == request_id) {
        tracing::warn!(
            request_id = %request_id,
            "set_enabled_rr_server: duplicate in-flight request_id"
        );
        metrics.duplicate_request_id.fetch_add(1, Ordering::Relaxed);
        send_error_reply(
            active,
            &request_id,
            RpcStatus::InvalidArgument,
            SetEnabledStatus::Unknown,
            "duplicate request_id (already in-flight)",
            metrics,
        );
        return;
    }

    let req = SetEnabledRrRequest {
        request_id: request_id.clone(),
        enabled: view.enabled(),
        dry_run: view.dry_run(),
        deadline_ns: view.deadline_ns(),
        audit_reason: view.audit_reason().unwrap_or("").to_string(),
    };

    match rr_request_tx.try_send(req) {
        Ok(()) => {
            pending.push_back(PendingRr { request_id, active });
            metrics.requests_forwarded.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Full(_)) => {
            tracing::warn!(
                request_id = %request_id,
                "set_enabled_rr_server: ipc_thread RR channel full; replying ResourceExhausted"
            );
            metrics.forward_full_errors.fetch_add(1, Ordering::Relaxed);
            send_error_reply(
                active,
                &request_id,
                RpcStatus::ResourceExhausted,
                SetEnabledStatus::Unknown,
                "ipc_thread RR channel full",
                metrics,
            );
        }
        Err(TrySendError::Disconnected(_)) => {
            tracing::error!(
                request_id = %request_id,
                "set_enabled_rr_server: ipc_thread RR channel disconnected; replying Internal"
            );
            metrics
                .forward_disconnected_errors
                .fetch_add(1, Ordering::Relaxed);
            send_error_reply(
                active,
                &request_id,
                RpcStatus::Internal,
                SetEnabledStatus::Unknown,
                "ipc_thread RR channel disconnected",
                metrics,
            );
        }
    }
}

/// pending 容量满时直接 reply `ResourceExhausted`，consume active，不入 pending。
fn handle_pending_overflow(
    active: Iceoryx2ActiveRequest<markers::SetEnabledRequest, markers::SetEnabledResponse>,
    metrics: &RrServerMetrics,
) {
    metrics.overflow_drops.fetch_add(1, Ordering::Relaxed);
    let request_id = match flatbuffers::root::<FbReq>(active.payload()) {
        Ok(view) => view.request_id().unwrap_or("").to_string(),
        Err(_) => String::new(),
    };
    tracing::warn!(
        request_id = %request_id,
        "set_enabled_rr_server: pending full; replying ResourceExhausted"
    );
    send_error_reply(
        active,
        &request_id,
        RpcStatus::ResourceExhausted,
        SetEnabledStatus::Unknown,
        "rr_server pending full",
        metrics,
    );
}

/// 直接 reply 错误 wire response，consume active handle。
fn send_error_reply(
    active: Iceoryx2ActiveRequest<markers::SetEnabledRequest, markers::SetEnabledResponse>,
    request_id: &str,
    status: RpcStatus,
    domain: SetEnabledStatus,
    error_msg: &str,
    metrics: &RrServerMetrics,
) {
    let bytes = build_set_enabled_response(request_id, status, domain, 0, error_msg);
    if let Err(e) = active.send_response(&bytes) {
        tracing::error!(
            error = ?e,
            request_id = %request_id,
            "set_enabled_rr_server: send_response (error reply) failed"
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
    resp: &SetEnabledRrResponse,
    metrics: &RrServerMetrics,
) {
    let pos = pending.iter().position(|p| p.request_id == resp.request_id);
    let Some(idx) = pos else {
        tracing::error!(
            request_id = %resp.request_id,
            "set_enabled_rr_server: orphan response (no pending active for request_id)"
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
    let (status, domain, completed_at_ns, error_msg) = encode_payload(&resp.payload);
    let bytes = build_set_enabled_response(
        &resp.request_id,
        status,
        domain,
        completed_at_ns,
        &error_msg,
    );
    if let Err(e) = pending_rr.active.send_response(&bytes) {
        tracing::error!(
            error = ?e,
            request_id = %resp.request_id,
            "set_enabled_rr_server: send_response failed"
        );
        metrics.send_response_errors.fetch_add(1, Ordering::Relaxed);
    } else {
        metrics.responses_sent.fetch_add(1, Ordering::Relaxed);
    }
}

/// [`SetEnabledRrPayload`] → wire `(RpcStatus, SetEnabledStatus, completed_at_ns,
/// error_msg)`。**显式 match** 每 variant（不直接 cast，沿 ClearEStop 约束）；
/// 映射表锁 Plan §P.2.3 encode 表。
fn encode_payload(payload: &SetEnabledRrPayload) -> (RpcStatus, SetEnabledStatus, u64, String) {
    match payload {
        // enable / disable 成功 wire 形态相同（Ok + domain Ok + completed_at_ns）；
        // 客户端按自己请求的 enabled 区分语义，wire 不重复编码。
        SetEnabledRrPayload::Enabled { completed_at_ns }
        | SetEnabledRrPayload::Disabled { completed_at_ns } => (
            RpcStatus::Ok,
            SetEnabledStatus::Ok,
            *completed_at_ns,
            String::new(),
        ),
        SetEnabledRrPayload::WouldAccept { at_ns } => (
            RpcStatus::Ok,
            SetEnabledStatus::WouldAccept,
            *at_ns,
            // D7/B4：precondition 文案，**不**写 "would succeed"。
            "precondition would accept (not a guarantee adapter will ack)".into(),
        ),
        SetEnabledRrPayload::GuardRejected { actual } => (
            RpcStatus::FailedPrecondition,
            SetEnabledStatus::GuardRejected,
            0,
            format!("set_enabled precondition rejected; state={actual:?}"),
        ),
        SetEnabledRrPayload::LatchBlocked => (
            RpcStatus::FailedPrecondition,
            SetEnabledStatus::LatchBlocked,
            0,
            "estop latched; clear estop before enabling".into(),
        ),
        SetEnabledRrPayload::AdapterError => (
            RpcStatus::Internal,
            SetEnabledStatus::AdapterError,
            0,
            "adapter enable/disable ack failed; state faulted (AdapterErr)".into(),
        ),
        SetEnabledRrPayload::AlreadyInProgress => (
            RpcStatus::ResourceExhausted,
            SetEnabledStatus::AlreadyInProgress,
            0,
            "another set_enabled in flight".into(),
        ),
        SetEnabledRrPayload::PreemptedByEStop => (
            RpcStatus::Aborted,
            SetEnabledStatus::PreemptedByEStop,
            0,
            "in-flight set_enabled preempted by EStop / SafetyStopFailed".into(),
        ),
        SetEnabledRrPayload::PreemptedByShutdown => (
            RpcStatus::Aborted,
            SetEnabledStatus::PreemptedByShutdown,
            0,
            "in-flight set_enabled preempted by shutdown".into(),
        ),
        SetEnabledRrPayload::DeadlineExceeded => (
            RpcStatus::DeadlineExceeded,
            SetEnabledStatus::DeadlineExceeded,
            0,
            "deadline exceeded before reaching target state".into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_machine::{FaultCode, S0State};

    /// 锁 Plan §P.2.3 encode 映射表：每个 [`SetEnabledRrPayload`] variant →
    /// 期望 `(RpcStatus, SetEnabledStatus)` + `completed_at_ns` 透传。
    #[test]
    fn encode_payload_locks_plan_mapping_table() {
        let cases: &[(SetEnabledRrPayload, RpcStatus, SetEnabledStatus, u64)] = &[
            (
                SetEnabledRrPayload::Enabled {
                    completed_at_ns: 11,
                },
                RpcStatus::Ok,
                SetEnabledStatus::Ok,
                11,
            ),
            (
                SetEnabledRrPayload::Disabled {
                    completed_at_ns: 22,
                },
                RpcStatus::Ok,
                SetEnabledStatus::Ok,
                22,
            ),
            (
                SetEnabledRrPayload::WouldAccept { at_ns: 33 },
                RpcStatus::Ok,
                SetEnabledStatus::WouldAccept,
                33,
            ),
            (
                SetEnabledRrPayload::GuardRejected {
                    actual: S0State::Fault(FaultCode::AdapterErr),
                },
                RpcStatus::FailedPrecondition,
                SetEnabledStatus::GuardRejected,
                0,
            ),
            (
                SetEnabledRrPayload::LatchBlocked,
                RpcStatus::FailedPrecondition,
                SetEnabledStatus::LatchBlocked,
                0,
            ),
            (
                SetEnabledRrPayload::AdapterError,
                RpcStatus::Internal,
                SetEnabledStatus::AdapterError,
                0,
            ),
            (
                SetEnabledRrPayload::AlreadyInProgress,
                RpcStatus::ResourceExhausted,
                SetEnabledStatus::AlreadyInProgress,
                0,
            ),
            (
                SetEnabledRrPayload::PreemptedByEStop,
                RpcStatus::Aborted,
                SetEnabledStatus::PreemptedByEStop,
                0,
            ),
            (
                SetEnabledRrPayload::PreemptedByShutdown,
                RpcStatus::Aborted,
                SetEnabledStatus::PreemptedByShutdown,
                0,
            ),
            (
                SetEnabledRrPayload::DeadlineExceeded,
                RpcStatus::DeadlineExceeded,
                SetEnabledStatus::DeadlineExceeded,
                0,
            ),
        ];
        for (payload, want_status, want_domain, want_completed) in cases {
            let (status, domain, completed, msg) = encode_payload(payload);
            assert_eq!(status, *want_status, "status mismatch for {payload:?}");
            assert_eq!(domain, *want_domain, "domain mismatch for {payload:?}");
            assert_eq!(
                completed, *want_completed,
                "completed_at_ns mismatch for {payload:?}"
            );
            // Ok 成功态 error_msg 空；非 Ok 携带诊断文案。
            match payload {
                SetEnabledRrPayload::Enabled { .. } | SetEnabledRrPayload::Disabled { .. } => {
                    assert!(msg.is_empty(), "成功态 error_msg 应空：{payload:?}");
                }
                _ => assert!(!msg.is_empty(), "非成功态应带诊断文案：{payload:?}"),
            }
        }
    }
}
