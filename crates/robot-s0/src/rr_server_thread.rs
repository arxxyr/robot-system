//! `ClearEStop` RR：真实服务限额准入、独立双通道、按原 route 退休。
//! 停止时先关闭唯一来源 sender；已交 IPC 的原 active/Slot 留到真实退休回复。
use crate::ipc_thread::{
    ClearEStopRrPayload, ClearEStopRrRequest, ClearEStopRrResponse, ReadyReader, ReadyState,
};
use crate::rr_admission::{ControlPeerLedger, RrRequestClass, Slot};
use crate::state_machine::S0State;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use robot_core::executor_ctrl::OutputSafetyState;
use robot_core::time::RobotClock;
use robot_msg::build_helpers::{ClearResponseMeta, build_clear_estop_response_typed};
use robot_msg::markers;
use robot_msg::msg::{ClearEStopRequest as FbReq, ClearEStopStatus, RpcStatus, SideEffect};
use robot_transport::iceoryx2::RrPeerId;
use robot_transport::iceoryx2::request_response::{Iceoryx2ActiveRequest, Iceoryx2RrServer};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct RrServerThreadConfig {
    /// iceoryx2 service topic：通常 `/<robot_id>/control/clear_estop`。
    /// 仅作为 doc / metrics label；实际 service 由 `server_factory` closure
    /// 内部用 `rr_server(node, &topic)` 创建。
    pub topic: String,
    /// 主循环 idle sleep（纳秒）。默认 500µs（2 kHz），足够把 wire request
    /// drain 转发给 `ipc_thread` + 把 `ipc_thread` response 回 wire。
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
    /// 成功 decode + 入 `in_flight` pending + 转发到 `ipc_thread` 的 request 数。
    pub requests_forwarded: AtomicU64,
    /// 成功 `active.send_response()` 的 response 数。
    pub responses_sent: AtomicU64,
    /// `FlatBuffers` root verifier 失败 / 必填字段缺失 / 空 `request_id：直接`
    /// reply `InvalidArgument`。
    pub decode_errors: AtomicU64,
    /// 重复 in-flight `request_id`：直接 reply InvalidArgument，不 forward。
    pub duplicate_request_id: AtomicU64,
    /// `rr_request_tx.try_send` 满：reply `ResourceExhausted`。
    pub forward_full_errors: AtomicU64,
    /// `rr_request_tx.try_send` Disconnected：reply Internal。
    pub forward_disconnected_errors: AtomicU64,
    /// pending 容量满（>= `max_in_flight）时新` wire request 被拒：reply
    /// `ResourceExhausted`。
    pub overflow_drops: AtomicU64,
    /// `pending` 找不到对应 `request_id` 的 `ipc_thread` response（理论不该
    /// 发生；release `保命丢弃，debug_assert` 兜底）。
    pub orphan_responses: AtomicU64,
    /// `active.send_response` 内部 IO 失败的次数。
    pub send_response_errors: AtomicU64,
}

/// 两个来源 sender 只由对应 RR 线程持有；停止准入时整体析构。
pub struct TopicRequestSenders<T> {
    pub high: Sender<T>,
    pub normal: Sender<T>,
}
impl<T> TopicRequestSenders<T> {
    pub(crate) fn select(&self, class: RrRequestClass) -> &Sender<T> {
        match class {
            RrRequestClass::Normal => &self.normal,
            RrRequestClass::Restore | RrRequestClass::Lower => &self.high,
        }
    }
}
/// 服务在原 Node 之前析构；工厂在 RR 线程内返回整个端口集合。
pub struct OwnedRrServerPorts<
    Req: robot_msg::markers::BodyMarker,
    Resp: robot_msg::markers::BodyMarker,
> {
    pub(crate) server: Iceoryx2RrServer<Req, Resp>,
    _node: iceoryx2::prelude::Node<iceoryx2::service::ipc::Service>,
}
impl<Req: robot_msg::markers::BodyMarker, Resp: robot_msg::markers::BodyMarker>
    OwnedRrServerPorts<Req, Resp>
{
    #[must_use]
    pub fn new(
        server: Iceoryx2RrServer<Req, Resp>,
        node: iceoryx2::prelude::Node<iceoryx2::service::ipc::Service>,
    ) -> Self {
        Self {
            server,
            _node: node,
        }
    }
}
#[derive(Debug)]
pub enum RrInitError {
    Node(iceoryx2::node::NodeCreationFailure),
    Service(robot_transport::TransportError),
    MissingLimits,
    Limits(crate::rr_admission::LimitsError),
    Capacity(std::io::Error),
    Ledger(crate::rr_admission::LedgerError),
}
pub(crate) fn make_ledger<
    Req: robot_msg::markers::BodyMarker,
    Resp: robot_msg::markers::BodyMarker,
    T,
    R,
>(
    ports: &OwnedRrServerPorts<Req, Resp>,
    tx: &TopicRequestSenders<T>,
    rx: &Receiver<R>,
) -> Result<ControlPeerLedger, RrInitError> {
    let actual = ports.server.limits().ok_or(RrInitError::MissingLimits)?;
    let checked = crate::rr_admission::CheckedLimits::try_from_limits(&actual)
        .map_err(RrInitError::Limits)?;
    let capacity = |n: Option<usize>| {
        n.ok_or_else(|| {
            RrInitError::Capacity(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "RR 通道必须有界",
            ))
        })
    };
    crate::rr_admission::validate_lane_capacity(
        &checked,
        capacity(tx.high.capacity())?,
        capacity(tx.normal.capacity())?,
        capacity(rx.capacity())?,
    )
    .map_err(RrInitError::Capacity)?;
    ControlPeerLedger::new(&checked).map_err(RrInitError::Ledger)
}
/// 失败返原的凭证留在线程中，直到真正的进程拥有出口。
pub(crate) fn release_slot(
    ledger: &mut ControlPeerLedger,
    slot: Slot<RrPeerId>,
    residual: &mut Vec<Slot<RrPeerId>>,
) {
    if let Err(crate::rr_admission::SlotMismatch(slot)) = ledger.release(slot) {
        residual.push(slot);
    }
}
pub struct RrServerThreadHandle {
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    metrics: Arc<RrServerMetrics>,
    ready: ReadyReader<RrInitError>,
}
impl RrServerThreadHandle {
    #[must_use]
    pub fn metrics(&self) -> Arc<RrServerMetrics> {
        Arc::clone(&self.metrics)
    }
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }
    pub fn poll_ready(&mut self) -> ReadyState<'_, RrInitError> {
        self.ready.poll()
    }
    pub fn join(self) -> thread::Result<()> {
        self.join.join()
    }
}
struct PendingRr {
    request_id: String,
    dry_run: bool,
    slot: Slot<RrPeerId>,
    active: Iceoryx2ActiveRequest<markers::ClearEStopRequest, markers::ClearEStopResponse>,
}
/// OS spawn 成功即交付原句柄；服务创建与限额检查通过后才发布 Ready。
pub fn spawn_rr_server_thread<F>(
    cfg: RrServerThreadConfig,
    clock: Arc<dyn RobotClock>,
    factory: F,
    requests: TopicRequestSenders<ClearEStopRrRequest>,
    responses: Receiver<ClearEStopRrResponse>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<RrServerThreadHandle>
where
    F: FnOnce() -> Result<
            OwnedRrServerPorts<markers::ClearEStopRequest, markers::ClearEStopResponse>,
            RrInitError,
        > + Send
        + 'static,
{
    #[cfg(feature = "test-comm-fault-hook")]
    crate::comm_fault_hook::before_spawn("clear_rr")?;
    let metrics = Arc::new(RrServerMetrics::default());
    let thread_metrics = Arc::clone(&metrics);
    let thread_stop = Arc::clone(&stop);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let join = thread::Builder::new()
        .name("s0_clear_rr".into())
        .spawn(move || {
            let ports = match factory() {
                Ok(ports) => ports,
                Err(error) => {
                    let _ = ready_tx.try_send(Err(error));
                    return;
                }
            };
            let ledger = match make_ledger(&ports, &requests, &responses) {
                Ok(ledger) => ledger,
                Err(error) => {
                    let _ = ready_tx.try_send(Err(error));
                    return;
                }
            };
            if ready_tx.try_send(Ok(())).is_err() {
                return;
            }
            drop(ready_tx);
            run_loop(
                &cfg,
                clock.as_ref(),
                &ports.server,
                requests,
                &responses,
                &thread_stop,
                &thread_metrics,
                ledger,
            );
        })?;
    Ok(RrServerThreadHandle {
        join,
        stop,
        metrics,
        ready: ReadyReader::new(ready_rx),
    })
}
#[allow(
    clippy::too_many_arguments,
    reason = "线程逐项持有原服务端、双通道与唯一账本，保持已批准的退出归属"
)]
fn run_loop(
    cfg: &RrServerThreadConfig,
    clock: &dyn RobotClock,
    server: &Iceoryx2RrServer<markers::ClearEStopRequest, markers::ClearEStopResponse>,
    requests: TopicRequestSenders<ClearEStopRrRequest>,
    responses: &Receiver<ClearEStopRrResponse>,
    stop: &AtomicBool,
    metrics: &RrServerMetrics,
    mut ledger: ControlPeerLedger,
) {
    let mut pending = VecDeque::with_capacity(crate::event_ingress::RR_CHANNEL_CAPACITY);
    let mut residual = Vec::new();
    let mut source = Some(requests);
    #[cfg(feature = "test-comm-fault-hook")]
    let mut stopped_fact_recorded = false;
    loop {
        if !crate::worker_stop::worker_running(
            crate::worker_stop::WorkerKind::ClearEStopRr,
            clock,
            stop,
        ) {
            drop(source.take());
        }
        // 只处理有限原槽的完成；响应断链不能冒充原在飞请求退休。
        for _ in 0..crate::event_ingress::RR_CHANNEL_CAPACITY {
            match responses.try_recv() {
                Ok(response) => {
                    handle_response(&mut pending, response, &mut ledger, &mut residual, metrics);
                }
                Err(
                    crossbeam_channel::TryRecvError::Empty
                    | crossbeam_channel::TryRecvError::Disconnected,
                ) => break,
            }
        }
        #[cfg(feature = "test-comm-fault-hook")]
        if source.is_none() && !stopped_fact_recorded {
            // 内层丢来源会在下一轮完成有界响应处理后到这里，不能只看本轮Some→None。
            record_pending_facts("stopped", clock.now().0, &pending, &ledger, &residual);
            stopped_fact_recorded = true;
        }
        if source.is_none() && pending.is_empty() && residual.is_empty() {
            return;
        }
        for _ in 0..crate::event_ingress::RR_CHANNEL_CAPACITY {
            // 每一次 wire receive 前核原停止标志，持续来流也必须交还来源 sender。
            if !crate::worker_stop::worker_running(
                crate::worker_stop::WorkerKind::ClearEStopRr,
                clock,
                stop,
            ) {
                drop(source.take());
                break;
            }
            let Some(requests) = source.as_ref() else {
                break;
            };
            match server.try_recv() {
                Ok(Some(active)) => {
                    metrics.requests_received.fetch_add(1, Ordering::Relaxed);
                    handle_active_request(
                        active,
                        &mut pending,
                        requests,
                        &mut ledger,
                        &mut residual,
                        metrics,
                        #[cfg(feature = "test-comm-fault-hook")]
                        clock,
                    );
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::error!(?error, "RR 接收失败");
                    break;
                }
            }
        }
        thread::sleep(Duration::from_nanos(cfg.poll_interval_ns));
    }
}
fn handle_active_request(
    active: Iceoryx2ActiveRequest<markers::ClearEStopRequest, markers::ClearEStopResponse>,
    pending: &mut VecDeque<PendingRr>,
    requests: &TopicRequestSenders<ClearEStopRrRequest>,
    ledger: &mut ControlPeerLedger,
    residual: &mut Vec<Slot<RrPeerId>>,
    metrics: &RrServerMetrics,
    #[cfg(feature = "test-comm-fault-hook")] clock: &dyn RobotClock,
) {
    let Ok(view) = flatbuffers::root::<FbReq>(active.payload()) else {
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
    };
    let request_id = view.request_id().unwrap_or("").to_owned();
    let dry_run = view.dry_run();
    if request_id.is_empty() {
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
    let peer = active.peer();
    if pending
        .iter()
        .any(|p| p.slot.peer() == peer && p.request_id == request_id)
    {
        metrics.duplicate_request_id.fetch_add(1, Ordering::Relaxed);
        send_error_reply(
            active,
            &request_id,
            dry_run,
            RpcStatus::InvalidArgument,
            "duplicate peer/method/request_id",
            metrics,
        );
        return;
    }
    let class = RrRequestClass::of_clear(dry_run);
    let Ok(slot) = ledger.admit(peer, class) else {
        metrics.overflow_drops.fetch_add(1, Ordering::Relaxed);
        send_error_reply(
            active,
            &request_id,
            dry_run,
            RpcStatus::ResourceExhausted,
            "peer/class capacity full",
            metrics,
        );
        return;
    };
    let request = ClearEStopRrRequest {
        route: slot.identity(),
        request_id: request_id.clone(),
        dry_run,
        confirmation: view.confirmation().unwrap_or("").to_owned(),
        deadline_ns: view.deadline_ns(),
        audit_reason: view.audit_reason().unwrap_or("").to_owned(),
    };
    match requests.select(class).try_send(request) {
        Ok(()) => {
            pending.push_back(PendingRr {
                request_id,
                dry_run,
                slot,
                active,
            });
            metrics.requests_forwarded.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "test-comm-fault-hook")]
            record_pending_facts("queued", clock.now().0, pending, ledger, residual);
        }
        Err(error) => {
            let (status, message) = match error {
                TrySendError::Full(_) => {
                    metrics.forward_full_errors.fetch_add(1, Ordering::Relaxed);
                    (RpcStatus::ResourceExhausted, "IPC lane full")
                }
                TrySendError::Disconnected(_) => {
                    metrics
                        .forward_disconnected_errors
                        .fetch_add(1, Ordering::Relaxed);
                    (RpcStatus::Internal, "IPC lane disconnected")
                }
            };
            // 原请求尚未交付，无后台责任；真实消耗 active 后才归还此原槽。
            send_error_reply(active, &request_id, dry_run, status, message, metrics);
            release_slot(ledger, slot, residual);
        }
    }
}
#[cfg(feature = "test-comm-fault-hook")]
fn record_pending_facts(
    boundary: &str,
    observed_at_ns: u64,
    pending: &VecDeque<PendingRr>,
    ledger: &ControlPeerLedger,
    residual: &[Slot<RrPeerId>],
) {
    let counts = ledger.counts();
    if pending.is_empty() {
        tracing::info!(
            boundary,
            observed_at_ns,
            pending_count = 0,
            residual_count = residual.len(),
            total = counts.total,
            restore = counts.restore,
            "R4原ClearRR责任"
        );
    }
    for entry in pending {
        let active_peer = entry.active.peer();
        let slot_peer = entry.slot.peer();
        let peer_counts = ledger.peer_counts(slot_peer);
        let request = flatbuffers::root::<FbReq>(entry.active.payload()).ok();
        tracing::info!(boundary, observed_at_ns, pending_count=pending.len(),
            residual_count=residual.len(), total=counts.total, restore=counts.restore,
            lower=counts.lower, normal=counts.normal, ledger=?ledger.id(),
            request_id=%entry.request_id, route=?Some(entry.slot.identity()), class=?entry.slot.class(),
            active_peer=?active_peer, slot_peer=?slot_peer, peer_matches=active_peer==slot_peer,
            peer_total=?peer_counts.map(|counts|counts.total),
            peer_restore=?peer_counts.map(|counts|counts.restore),
            peer_lower=?peer_counts.map(|counts|counts.lower),
            peer_normal=?peer_counts.map(|counts|counts.normal),
            active_decoded=request.is_some(), deadline_ns=?request.map(|request|request.deadline_ns()),
            "R4原ClearRR责任");
    }
}

fn send_error_reply(
    active: Iceoryx2ActiveRequest<markers::ClearEStopRequest, markers::ClearEStopResponse>,
    request_id: &str,
    dry_run: bool,
    status: RpcStatus,
    error: &str,
    metrics: &RrServerMetrics,
) {
    let domain = match status {
        RpcStatus::ResourceExhausted => ClearEStopStatus::QueueFull,
        _ => ClearEStopStatus::Unknown,
    };
    let bytes = build_clear_estop_response_typed(
        request_id,
        status,
        error,
        0,
        dry_run,
        ClearResponseMeta {
            domain,
            superseded: false,
            effect: SideEffect::None,
            retired: true,
        },
    );
    match active.send_response(&bytes) {
        Ok(()) => {
            metrics.responses_sent.fetch_add(1, Ordering::Relaxed);
        }
        Err(error) => {
            metrics.send_response_errors.fetch_add(1, Ordering::Relaxed);
            tracing::error!(?error, request_id, "RR 拒绝回复失败");
        }
    }
}
#[allow(
    clippy::needless_pass_by_value,
    reason = "退休回复必须按值消费一次，错路由与重复回复不得再次归还原槽"
)]
fn handle_response(
    pending: &mut VecDeque<PendingRr>,
    response: ClearEStopRrResponse,
    ledger: &mut ControlPeerLedger,
    residual: &mut Vec<Slot<RrPeerId>>,
    metrics: &RrServerMetrics,
) {
    let index = pending
        .iter()
        .position(|p| p.slot.identity() == response.route && p.request_id == response.request_id);
    let Some(index) = index else {
        metrics.orphan_responses.fetch_add(1, Ordering::Relaxed);
        tracing::error!(request_id=%response.request_id,"错 route 或重复退休回复，不归还任何槽");
        return;
    };
    let original = pending.remove(index).expect("已核原 route");
    let (status, error, at) = encode_payload(&response.payload);
    let bytes = build_clear_estop_response_typed(
        &response.request_id,
        status,
        &error,
        at,
        original.dry_run,
        ClearResponseMeta {
            domain: clear_domain(&response.payload),
            superseded: response.superseded,
            effect: response.effect,
            retired: true,
        },
    );
    match original.active.send_response(&bytes) {
        Ok(()) => {
            metrics.responses_sent.fetch_add(1, Ordering::Relaxed);
        }
        Err(error) => {
            metrics.send_response_errors.fetch_add(1, Ordering::Relaxed);
            tracing::error!(?error, "RR 退休回复发送失败");
        }
    }
    release_slot(ledger, original.slot, residual);
}
fn authority_clear_domain(reason: robot_core::control_authority::Refusal) -> ClearEStopStatus {
    use robot_core::control_authority::Refusal;
    match reason {
        Refusal::Disabled => ClearEStopStatus::SupersededByDisable,
        Refusal::Shutdown => ClearEStopStatus::SupersededByShutdown,
        Refusal::Inhibited(_) => ClearEStopStatus::SupersededByInhibit,
        Refusal::Exhausted | Refusal::EpochExhausted => ClearEStopStatus::SupersededByExhausted,
        Refusal::Latched | Refusal::SerialEStopPending => ClearEStopStatus::PreemptedByEmergency,
        _ => ClearEStopStatus::AuthorityRefused,
    }
}
fn clear_domain(payload: &ClearEStopRrPayload) -> ClearEStopStatus {
    use crate::clear_coordination::{ClearOutcome as O, SupersedeReason};
    match payload {
        ClearEStopRrPayload::Coordination(outcome) => match outcome {
            O::Cleared { .. } => ClearEStopStatus::Cleared,
            O::WouldClear { .. } => ClearEStopStatus::WouldClear,
            O::Superseded(SupersedeReason::Shutdown) => ClearEStopStatus::SupersededByShutdown,
            O::Superseded(SupersedeReason::Authority(reason)) => authority_clear_domain(*reason),
            O::AuthorityRefused(_) => ClearEStopStatus::AuthorityRefused,
            O::PreemptedByEmergency => ClearEStopStatus::PreemptedByEmergency,
            O::FsyncFailed { .. } => ClearEStopStatus::FsyncFailed,
            O::PersistPending => ClearEStopStatus::PersistPending,
            O::WriterUnavailable => ClearEStopStatus::WriterUnavailable,
            O::AlreadyInProgress => ClearEStopStatus::AlreadyInProgress,
            O::BusyLowering => ClearEStopStatus::BusyLowering,
            O::StateNotSafe => ClearEStopStatus::StateNotSafe,
            O::NotTorqueOffConfirmed { .. } => ClearEStopStatus::NotTorqueOffConfirmed,
            O::DeadlineExceeded => ClearEStopStatus::DeadlineExceeded,
        },
        ClearEStopRrPayload::QueueFull => ClearEStopStatus::QueueFull,
        ClearEStopRrPayload::ShuttingDown => ClearEStopStatus::SupersededByShutdown,
        ClearEStopRrPayload::Cleared { .. } => ClearEStopStatus::Cleared,
        ClearEStopRrPayload::WouldClear { .. } => ClearEStopStatus::WouldClear,
        ClearEStopRrPayload::StateNotSafe { .. } => ClearEStopStatus::StateNotSafe,
        ClearEStopRrPayload::NotTorqueOffConfirmed { .. } => {
            ClearEStopStatus::NotTorqueOffConfirmed
        }
        ClearEStopRrPayload::FsyncFailed => ClearEStopStatus::FsyncFailed,
        ClearEStopRrPayload::AlreadyInProgress => ClearEStopStatus::AlreadyInProgress,
        ClearEStopRrPayload::PreemptedByEmergency => ClearEStopStatus::PreemptedByEmergency,
        ClearEStopRrPayload::DeadlineExceeded => ClearEStopStatus::DeadlineExceeded,
    }
}
fn encode_coordination(
    outcome: crate::clear_coordination::ClearOutcome,
) -> (RpcStatus, String, u64) {
    use crate::clear_coordination::ClearOutcome as O;
    match outcome {
        O::Cleared { cleared_at_ns, .. } => (RpcStatus::Ok, String::new(), cleared_at_ns),
        O::WouldClear { at } => (RpcStatus::Ok, String::new(), at),
        O::DeadlineExceeded => (RpcStatus::DeadlineExceeded, "deadline exceeded".into(), 0),
        O::Superseded(_) | O::PreemptedByEmergency => {
            (RpcStatus::Aborted, format!("{outcome:?}"), 0)
        }
        O::FsyncFailed { .. } | O::WriterUnavailable => {
            (RpcStatus::Internal, format!("{outcome:?}"), 0)
        }
        O::AlreadyInProgress | O::BusyLowering | O::PersistPending => {
            (RpcStatus::ResourceExhausted, format!("{outcome:?}"), 0)
        }
        O::AuthorityRefused(_) | O::StateNotSafe | O::NotTorqueOffConfirmed { .. } => {
            (RpcStatus::FailedPrecondition, format!("{outcome:?}"), 0)
        }
    }
}
fn encode_payload(payload: &ClearEStopRrPayload) -> (RpcStatus, String, u64) {
    match payload {
        ClearEStopRrPayload::Coordination(outcome) => encode_coordination(*outcome),
        ClearEStopRrPayload::QueueFull => (RpcStatus::ResourceExhausted, "IPC lane full".into(), 0),
        ClearEStopRrPayload::ShuttingDown => (RpcStatus::Aborted, "shutdown".into(), 0),
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
    use crossbeam_channel::bounded;
    use iceoryx2::prelude::NodeBuilder;
    use iceoryx2::service::ipc::Service as IxService;
    use robot_core::time::SystemClock;
    use robot_msg::build_helpers::build_clear_estop_request;
    use robot_msg::msg::{ClearEStopResponse as FbResp, RpcRole};
    use robot_transport::iceoryx2::request_response::{Iceoryx2PendingResponse, Iceoryx2RrClient};
    use robot_transport::iceoryx2::{RrProfile, rr_client_with, rr_server_with};
    use std::time::Instant;

    type Client = Iceoryx2RrClient<markers::ClearEStopRequest, markers::ClearEStopResponse>;
    type Pending = Iceoryx2PendingResponse<markers::ClearEStopResponse>;
    type Server = Iceoryx2RrServer<markers::ClearEStopRequest, markers::ClearEStopResponse>;

    // 每个真实 worker 用例由父进程兜底：断言失败不先 Drop 活句柄，父必 kill/reap。
    fn supervised(name: &str) -> bool {
        const ENV: &str = "ROBOT_W3_CLEAR_RR_CASE";
        if std::env::var(ENV).as_deref() == Ok(name) {
            let owner = thread::current().id();
            std::panic::set_hook(Box::new(move |info| {
                eprintln!("Clear RR 验收失败：{info}");
                if thread::current().id() == owner {
                    std::process::exit(101);
                }
            }));
            return false;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("child.log");
        let log = std::fs::File::create(&path).unwrap();
        let exact = format!("rr_server_thread::tests::{name}");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &exact, "--nocapture"])
            .env(ENV, name)
            .env("TMPDIR", temp.path())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(12);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(2)),
                error => {
                    let _ = child.kill();
                    let reap = Instant::now() + Duration::from_secs(2);
                    loop {
                        if matches!(child.try_wait(), Ok(Some(_))) {
                            break;
                        }
                        if Instant::now() >= reap {
                            std::process::abort();
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                    panic!("已 kill/reap Clear RR 子进程：{error:?}");
                }
            }
        };
        assert!(
            status.success(),
            "{}",
            std::fs::read_to_string(path).unwrap()
        );
        true
    }
    fn wait(mut condition: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(2);
        while !condition() {
            assert!(Instant::now() < until, "真实 RR 操作未在验收截止内结束");
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn topic() -> String {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        format!(
            "/w3_clear_rr_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }
    fn request(id: &str, dry: bool) -> Vec<u8> {
        build_clear_estop_request(
            id,
            "idem",
            "src.test",
            RpcRole::Maintainer,
            "confirm",
            "audit",
            dry,
            u64::MAX,
            42,
        )
    }
    fn response(pending: &Pending) -> Vec<u8> {
        let mut bytes = None;
        wait(|| {
            bytes = pending.try_recv().unwrap();
            bytes.is_some()
        });
        bytes.unwrap()
    }
    fn status(pending: &Pending) -> RpcStatus {
        let bytes = response(pending);
        let view = flatbuffers::root::<FbResp>(&bytes).unwrap();
        assert!(view.retired());
        view.status()
    }
    // 本模块只模拟 IPC 发回的已退休消息，验证路由和编码；不冒充文件物理退休。
    fn reply(req: ClearEStopRrRequest, payload: ClearEStopRrPayload) -> ClearEStopRrResponse {
        ClearEStopRrResponse {
            route: req.route,
            request_id: req.request_id,
            payload,
            effect: SideEffect::Started,
            superseded: false,
        }
    }
    fn cleared(at: u64) -> ClearEStopRrPayload {
        ClearEStopRrPayload::Cleared { cleared_at_ns: at }
    }

    struct Harness {
        handle: RrServerThreadHandle,
        client: Client,
        high: Receiver<ClearEStopRrRequest>,
        normal: Receiver<ClearEStopRrRequest>,
        responses: Sender<ClearEStopRrResponse>,
        _node: iceoryx2::prelude::Node<IxService>,
    }
    impl Harness {
        fn new() -> Self {
            let topic = topic();
            let profile = RrProfile::S0_CONTROL;
            let node = NodeBuilder::new().create::<IxService>().unwrap();
            let client = rr_client_with(&node, &topic, &profile).unwrap();
            let (high_tx, high) = bounded(2 * profile.c);
            let (normal_tx, normal) = bounded(profile.c * (profile.l - 2));
            let (responses, response_rx) = bounded(profile.c * profile.l);
            let mut handle = spawn_rr_server_thread(
                RrServerThreadConfig {
                    topic: topic.clone(),
                    ..RrServerThreadConfig::default()
                },
                Arc::new(SystemClock::new()),
                move || {
                    let node = NodeBuilder::new()
                        .create::<IxService>()
                        .map_err(RrInitError::Node)?;
                    let server =
                        rr_server_with(&node, &topic, &profile).map_err(RrInitError::Service)?;
                    Ok(OwnedRrServerPorts::new(server, node))
                },
                TopicRequestSenders {
                    high: high_tx,
                    normal: normal_tx,
                },
                response_rx,
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
            wait(|| matches!(handle.poll_ready(), ReadyState::Ready));
            Self {
                handle,
                client,
                high,
                normal,
                responses,
                _node: node,
            }
        }
        fn received(&self, dry: bool) -> ClearEStopRrRequest {
            if dry { &self.normal } else { &self.high }
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
        }
        fn finish(self) {
            self.handle.request_stop();
            wait(|| self.handle.is_finished());
            self.handle.join().unwrap();
        }
    }

    #[test]
    fn rr_server_forwards_decoded_request_to_ipc_thread() {
        if supervised("rr_server_forwards_decoded_request_to_ipc_thread") {
            return;
        }
        let h = Harness::new();
        let pending = h.client.send(&request("req-1", false)).unwrap();
        let req = h.received(false);
        assert_eq!(
            (&*req.request_id, &*req.confirmation, &*req.audit_reason),
            ("req-1", "confirm", "audit")
        );
        assert!(!req.dry_run);
        assert_eq!(req.deadline_ns, u64::MAX);
        h.responses.send(reply(req, cleared(999_999))).unwrap();
        let bytes = response(&pending);
        let view = flatbuffers::root::<FbResp>(&bytes).unwrap();
        assert_eq!(view.status(), RpcStatus::Ok);
        assert_eq!(view.request_id(), Some("req-1"));
        assert_eq!(view.cleared_at_ns(), 999_999);
        assert!(!view.dry_run());
        assert!(view.retired());
        h.finish();
    }
    fn run_variant(payload: ClearEStopRrPayload, expected: RpcStatus, at: u64, dry: bool) {
        let h = Harness::new();
        let pending = h.client.send(&request("variant", dry)).unwrap();
        h.responses.send(reply(h.received(dry), payload)).unwrap();
        let bytes = response(&pending);
        let view = flatbuffers::root::<FbResp>(&bytes).unwrap();
        assert_eq!(view.status(), expected);
        assert_eq!(view.cleared_at_ns(), at);
        assert_eq!(view.dry_run(), dry);
        assert!(view.retired());
        h.finish();
    }
    #[test]
    fn rr_server_replies_ok_for_cleared_variant() {
        if supervised("rr_server_replies_ok_for_cleared_variant") {
            return;
        }
        run_variant(cleared(12_345), RpcStatus::Ok, 12_345, false);
    }
    #[test]
    fn rr_server_replies_ok_for_would_clear_variant() {
        if supervised("rr_server_replies_ok_for_would_clear_variant") {
            return;
        }
        run_variant(
            ClearEStopRrPayload::WouldClear {
                would_clear_at_ns: 67_890,
            },
            RpcStatus::Ok,
            67_890,
            true,
        );
    }
    #[test]
    fn rr_server_replies_failed_precondition_for_state_not_safe() {
        if supervised("rr_server_replies_failed_precondition_for_state_not_safe") {
            return;
        }
        run_variant(
            ClearEStopRrPayload::StateNotSafe {
                actual: S0State::Ready,
            },
            RpcStatus::FailedPrecondition,
            0,
            false,
        );
    }
    #[test]
    fn rr_server_replies_failed_precondition_for_not_torque_off_confirmed() {
        if supervised("rr_server_replies_failed_precondition_for_not_torque_off_confirmed") {
            return;
        }
        run_variant(
            ClearEStopRrPayload::NotTorqueOffConfirmed {
                actual: OutputSafetyState::TorqueOn,
            },
            RpcStatus::FailedPrecondition,
            0,
            false,
        );
    }
    #[test]
    fn rr_server_replies_internal_for_fsync_failed() {
        if supervised("rr_server_replies_internal_for_fsync_failed") {
            return;
        }
        run_variant(
            ClearEStopRrPayload::FsyncFailed,
            RpcStatus::Internal,
            0,
            false,
        );
    }
    #[test]
    fn rr_server_replies_resource_exhausted_for_already_in_progress() {
        if supervised("rr_server_replies_resource_exhausted_for_already_in_progress") {
            return;
        }
        run_variant(
            ClearEStopRrPayload::AlreadyInProgress,
            RpcStatus::ResourceExhausted,
            0,
            false,
        );
    }
    #[test]
    fn rr_server_replies_aborted_for_preempted_by_emergency() {
        if supervised("rr_server_replies_aborted_for_preempted_by_emergency") {
            return;
        }
        run_variant(
            ClearEStopRrPayload::PreemptedByEmergency,
            RpcStatus::Aborted,
            0,
            false,
        );
    }
    #[test]
    fn rr_server_replies_deadline_exceeded_for_deadline_exceeded() {
        if supervised("rr_server_replies_deadline_exceeded_for_deadline_exceeded") {
            return;
        }
        run_variant(
            ClearEStopRrPayload::DeadlineExceeded,
            RpcStatus::DeadlineExceeded,
            0,
            false,
        );
    }
    #[test]
    fn rr_server_round_trip_dry_run_preserves_dry_run_field_in_response() {
        if supervised("rr_server_round_trip_dry_run_preserves_dry_run_field_in_response") {
            return;
        }
        run_variant(
            ClearEStopRrPayload::WouldClear {
                would_clear_at_ns: 314_159,
            },
            RpcStatus::Ok,
            314_159,
            true,
        );
    }

    #[test]
    fn rr_server_decodes_invalid_wire_replies_invalid_argument() {
        if supervised("rr_server_decodes_invalid_wire_replies_invalid_argument") {
            return;
        }
        let h = Harness::new();
        let pending = h.client.send(b"NOT_A_VALID_FLATBUFFER").unwrap();
        assert_eq!(status(&pending), RpcStatus::InvalidArgument);
        assert!(h.high.is_empty() && h.normal.is_empty());
        assert_eq!(h.handle.metrics().decode_errors.load(Ordering::Relaxed), 1);
        h.finish();
    }
    #[test]
    fn rr_server_replies_invalid_argument_for_empty_request_id() {
        if supervised("rr_server_replies_invalid_argument_for_empty_request_id") {
            return;
        }
        let h = Harness::new();
        let pending = h.client.send(&request("", false)).unwrap();
        assert_eq!(status(&pending), RpcStatus::InvalidArgument);
        assert!(h.high.is_empty());
        h.finish();
    }
    #[test]
    fn rr_server_replies_invalid_argument_for_duplicate_in_flight_request_id() {
        if supervised("rr_server_replies_invalid_argument_for_duplicate_in_flight_request_id") {
            return;
        }
        let h = Harness::new();
        let first = h.client.send(&request("dup", false)).unwrap();
        let req = h.received(false);
        let second = h.client.send(&request("dup", false)).unwrap();
        assert_eq!(status(&second), RpcStatus::InvalidArgument);
        assert!(h.high.is_empty());
        h.responses.send(reply(req, cleared(1))).unwrap();
        assert_eq!(status(&first), RpcStatus::Ok);
        assert_eq!(
            h.handle
                .metrics()
                .duplicate_request_id
                .load(Ordering::Relaxed),
            1
        );
        h.finish();
    }
    #[test]
    fn rr_server_replies_internal_when_ipc_thread_channel_disconnected() {
        if supervised("rr_server_replies_internal_when_ipc_thread_channel_disconnected") {
            return;
        }
        let h = Harness::new();
        drop(h.high);
        let pending = h.client.send(&request("disconnected", false)).unwrap();
        assert_eq!(status(&pending), RpcStatus::Internal);
        h.handle.request_stop();
        wait(|| h.handle.is_finished());
        h.handle.join().unwrap();
    }
    #[test]
    fn rr_server_retains_pending_after_stop_until_original_retired_reply() {
        if supervised("rr_server_retains_pending_after_stop_until_original_retired_reply") {
            return;
        }
        let h = Harness::new();
        let first = h.client.send(&request("stop", false)).unwrap();
        let req = h.received(false);
        h.handle.request_stop();
        wait(|| {
            matches!(
                h.high.try_recv(),
                Err(crossbeam_channel::TryRecvError::Disconnected)
            )
        });
        assert!(matches!(
            h.normal.try_recv(),
            Err(crossbeam_channel::TryRecvError::Disconnected)
        ));
        assert!(!h.handle.is_finished());
        assert!(first.try_recv().unwrap().is_none());
        // 停止准入后新的 wire 仍不允许进入 IPC；原服务因旧 Active 保持存活。
        let late = h.client.send(&request("after-stop", true)).unwrap();
        assert!(late.try_recv().unwrap().is_none());
        h.responses
            .send(reply(req, ClearEStopRrPayload::ShuttingDown))
            .unwrap();
        assert_eq!(status(&first), RpcStatus::Aborted);
        wait(|| h.handle.is_finished());
        assert_eq!(
            h.handle.metrics().requests_received.load(Ordering::Relaxed),
            1
        );
        h.handle.join().unwrap();
        // late 未被服务接纳，测试不把其 Drop 算作任何后台执行退休。
        drop(late);
    }
    #[test]
    fn rr_server_replies_resource_exhausted_when_pending_overflow() {
        if supervised("rr_server_replies_resource_exhausted_when_pending_overflow") {
            return;
        }
        let h = Harness::new();
        let first = h.client.send(&request("first", false)).unwrap();
        let req = h.received(false);
        let second = h.client.send(&request("second", false)).unwrap();
        assert_eq!(status(&second), RpcStatus::ResourceExhausted);
        assert!(h.high.is_empty());
        assert!(first.try_recv().unwrap().is_none());
        h.responses.send(reply(req, cleared(1))).unwrap();
        assert_eq!(status(&first), RpcStatus::Ok);
        assert_eq!(h.handle.metrics().overflow_drops.load(Ordering::Relaxed), 1);
        h.finish();
    }
    #[test]
    fn rr_server_drains_response_before_wire_request_to_release_pending_slot() {
        if supervised("rr_server_drains_response_before_wire_request_to_release_pending_slot") {
            return;
        }
        let h = Harness::new();
        let first = h.client.send(&request("first", false)).unwrap();
        h.responses
            .send(reply(h.received(false), cleared(100)))
            .unwrap();
        assert_eq!(status(&first), RpcStatus::Ok);
        let second = h.client.send(&request("second", false)).unwrap();
        h.responses
            .send(reply(h.received(false), cleared(200)))
            .unwrap();
        assert_eq!(status(&second), RpcStatus::Ok);
        h.finish();
    }

    // 真服务/peer 的同步处理夹具：不启动线程，只调用同一生产 handler，能直接核 ledger。
    struct Local {
        server: Server,
        first: Client,
        second: Client,
        ledger: ControlPeerLedger,
        pending: VecDeque<PendingRr>,
        residual: Vec<Slot<RrPeerId>>,
        senders: TopicRequestSenders<ClearEStopRrRequest>,
        high: Receiver<ClearEStopRrRequest>,
        _normal: Receiver<ClearEStopRrRequest>,
        metrics: RrServerMetrics,
        _node: iceoryx2::prelude::Node<IxService>,
    }
    impl Local {
        fn new(high_capacity: usize) -> Self {
            let node = NodeBuilder::new().create::<IxService>().unwrap();
            let topic = topic();
            let profile = RrProfile::S0_CONTROL;
            let server = rr_server_with(&node, &topic, &profile).unwrap();
            let first = rr_client_with(&node, &topic, &profile).unwrap();
            let second = rr_client_with(&node, &topic, &profile).unwrap();
            let checked =
                crate::rr_admission::CheckedLimits::try_from_limits(&server.limits().unwrap())
                    .unwrap();
            let (high_tx, high) = bounded(high_capacity);
            let (normal_tx, normal) = bounded(profile.c * (profile.l - 2));
            Self {
                server,
                first,
                second,
                ledger: ControlPeerLedger::new(&checked).unwrap(),
                pending: VecDeque::new(),
                residual: Vec::new(),
                senders: TopicRequestSenders {
                    high: high_tx,
                    normal: normal_tx,
                },
                high,
                _normal: normal,
                metrics: RrServerMetrics::default(),
                _node: node,
            }
        }
        fn consume(&mut self) {
            let mut active = None;
            wait(|| {
                active = self.server.try_recv().unwrap();
                active.is_some()
            });
            handle_active_request(
                active.unwrap(),
                &mut self.pending,
                &self.senders,
                &mut self.ledger,
                &mut self.residual,
                &self.metrics,
                #[cfg(feature = "test-comm-fault-hook")]
                &robot_core::time::SystemClock,
            );
        }
        fn deliver(&mut self, response: ClearEStopRrResponse) {
            handle_response(
                &mut self.pending,
                response,
                &mut self.ledger,
                &mut self.residual,
                &self.metrics,
            );
        }
    }
    #[test]
    fn rr_server_drops_request_when_ipc_thread_channel_full_replies_resource_exhausted() {
        // 合规工厂会拒绝过小来源容量；仅此错误分类夹具用真小 channel 触发同 handler Full。
        let mut f = Local::new(1);
        let checked =
            crate::rr_admission::CheckedLimits::try_from_limits(&f.server.limits().unwrap())
                .unwrap();
        assert!(crate::rr_admission::validate_lane_capacity(&checked, 1, 16, 32).is_err());
        let first = f.first.send(&request("first", false)).unwrap();
        f.consume();
        let second = f.second.send(&request("second", false)).unwrap();
        f.consume();
        assert_eq!(status(&second), RpcStatus::ResourceExhausted);
        assert_eq!(f.ledger.counts().restore, 1);
        assert_eq!(f.pending.len(), 1);
        assert_eq!(f.metrics.forward_full_errors.load(Ordering::Relaxed), 1);
        let req = f.high.try_recv().unwrap();
        f.deliver(reply(req, cleared(1)));
        assert_eq!(status(&first), RpcStatus::Ok);
        assert_eq!(f.ledger.counts().restore, 0);
        assert!(f.residual.is_empty());
    }
    #[test]
    fn distinct_peers_share_request_id_but_not_route_or_refund() {
        let mut f = Local::new(16);
        let first = f.first.send(&request("same", false)).unwrap();
        f.consume();
        let req1 = f.high.try_recv().unwrap();
        let second = f.second.send(&request("same", false)).unwrap();
        f.consume();
        let req2 = f.high.try_recv().unwrap();
        assert_ne!(req1.route, req2.route);
        assert_eq!(f.ledger.counts().restore, 2);
        // 用另一真实 ledger 的 route 冒充当前回复，不允许释放任何原槽。
        let mut other = ControlPeerLedger::new(
            &crate::rr_admission::CheckedLimits::try_from_limits(&f.server.limits().unwrap())
                .unwrap(),
        )
        .unwrap();
        let foreign = other
            .admit(
                f.pending.front().unwrap().slot.peer(),
                RrRequestClass::Restore,
            )
            .unwrap();
        f.deliver(ClearEStopRrResponse {
            route: foreign.identity(),
            request_id: "same".into(),
            payload: cleared(1),
            effect: SideEffect::Committed,
            superseded: false,
        });
        other.release(foreign).unwrap();
        assert_eq!(f.ledger.counts().restore, 2);
        assert!(first.try_recv().unwrap().is_none() && second.try_recv().unwrap().is_none());
        let old_route = req1.route;
        f.deliver(reply(req1, cleared(1)));
        assert_eq!(status(&first), RpcStatus::Ok);
        f.deliver(ClearEStopRrResponse {
            route: old_route,
            request_id: "same".into(),
            payload: cleared(2),
            effect: SideEffect::Committed,
            superseded: false,
        });
        assert_eq!(f.ledger.counts().restore, 1);
        assert!(second.try_recv().unwrap().is_none());
        f.deliver(reply(req2, cleared(2)));
        assert_eq!(status(&second), RpcStatus::Ok);
        assert_eq!(f.ledger.counts().restore, 0);
        assert_eq!(f.metrics.orphan_responses.load(Ordering::Relaxed), 2);
        assert!(f.pending.is_empty() && f.residual.is_empty());
    }
    #[test]
    fn rr_server_handles_orphan_response_without_refunding_other_request() {
        let mut f = Local::new(16);
        let pending = f.first.send(&request("live", false)).unwrap();
        f.consume();
        let req = f.high.try_recv().unwrap();
        f.deliver(ClearEStopRrResponse {
            route: req.route,
            request_id: "ghost".into(),
            payload: cleared(1),
            effect: SideEffect::Committed,
            superseded: false,
        });
        assert_eq!(f.ledger.counts().restore, 1);
        assert!(pending.try_recv().unwrap().is_none());
        f.deliver(reply(req, cleared(1)));
        assert_eq!(status(&pending), RpcStatus::Ok);
        assert_eq!(f.ledger.counts().restore, 0);
    }
}
