//! `SetEnabled` RR：真实服务限额准入、独立双通道、按原 route 退休。
//! 停止时先关闭唯一来源 sender；已交 IPC 的原 active/Slot 留到真实退休回复。
use crate::ipc_thread::{
    ReadyReader, ReadyState, SetEnabledRrPayload, SetEnabledRrRequest, SetEnabledRrResponse,
};
use crate::rr_admission::{ControlPeerLedger, RrRequestClass, Slot};
use crate::rr_server_thread::{
    OwnedRrServerPorts, RrInitError, RrServerMetrics, RrServerThreadConfig, TopicRequestSenders,
    make_ledger, release_slot,
};
use crossbeam_channel::{Receiver, TrySendError};
use robot_core::time::RobotClock;
use robot_msg::build_helpers::build_set_enabled_response_typed;
use robot_msg::markers;
use robot_msg::msg::{RpcStatus, SetEnabledRequest as FbReq, SetEnabledStatus, SideEffect};
use robot_transport::iceoryx2::RrPeerId;
use robot_transport::iceoryx2::request_response::{Iceoryx2ActiveRequest, Iceoryx2RrServer};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct SetEnabledRrServerHandle {
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    metrics: Arc<RrServerMetrics>,
    ready: ReadyReader<RrInitError>,
}
impl SetEnabledRrServerHandle {
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
    slot: Slot<RrPeerId>,
    active: Iceoryx2ActiveRequest<markers::SetEnabledRequest, markers::SetEnabledResponse>,
}
/// OS spawn 成功即交付原句柄；服务创建与限额检查通过后才发布 Ready。
pub fn spawn_set_enabled_rr_server_thread<F>(
    cfg: RrServerThreadConfig,
    clock: Arc<dyn RobotClock>,
    factory: F,
    requests: TopicRequestSenders<SetEnabledRrRequest>,
    responses: Receiver<SetEnabledRrResponse>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<SetEnabledRrServerHandle>
where
    F: FnOnce() -> Result<
            OwnedRrServerPorts<markers::SetEnabledRequest, markers::SetEnabledResponse>,
            RrInitError,
        > + Send
        + 'static,
{
    #[cfg(feature = "test-comm-fault-hook")]
    crate::comm_fault_hook::before_spawn("set_rr")?;
    let metrics = Arc::new(RrServerMetrics::default());
    let thread_metrics = Arc::clone(&metrics);
    let thread_stop = Arc::clone(&stop);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let join = thread::Builder::new()
        .name("s0_set_rr".into())
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
    Ok(SetEnabledRrServerHandle {
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
    server: &Iceoryx2RrServer<markers::SetEnabledRequest, markers::SetEnabledResponse>,
    requests: TopicRequestSenders<SetEnabledRrRequest>,
    responses: &Receiver<SetEnabledRrResponse>,
    stop: &AtomicBool,
    metrics: &RrServerMetrics,
    mut ledger: ControlPeerLedger,
) {
    let mut pending = VecDeque::with_capacity(crate::event_ingress::RR_CHANNEL_CAPACITY);
    let mut residual = Vec::new();
    let mut source = Some(requests);
    loop {
        if !crate::worker_stop::worker_running(
            crate::worker_stop::WorkerKind::SetEnabledRr,
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
        if source.is_none() && pending.is_empty() && residual.is_empty() {
            return;
        }
        for _ in 0..crate::event_ingress::RR_CHANNEL_CAPACITY {
            // 每一次 wire receive 前核原停止标志，持续来流也必须交还来源 sender。
            if !crate::worker_stop::worker_running(
                crate::worker_stop::WorkerKind::SetEnabledRr,
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
    active: Iceoryx2ActiveRequest<markers::SetEnabledRequest, markers::SetEnabledResponse>,
    pending: &mut VecDeque<PendingRr>,
    requests: &TopicRequestSenders<SetEnabledRrRequest>,
    ledger: &mut ControlPeerLedger,
    residual: &mut Vec<Slot<RrPeerId>>,
    metrics: &RrServerMetrics,
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
    let class = RrRequestClass::of_set_enabled(dry_run, view.enabled());
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
    let request = SetEnabledRrRequest {
        route: slot.identity(),
        request_id: request_id.clone(),
        dry_run,
        enabled: view.enabled(),
        deadline_ns: view.deadline_ns(),
        audit_reason: view.audit_reason().unwrap_or("").to_owned(),
    };
    match requests.select(class).try_send(request) {
        Ok(()) => {
            pending.push_back(PendingRr {
                request_id,
                slot,
                active,
            });
            metrics.requests_forwarded.fetch_add(1, Ordering::Relaxed);
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
fn send_error_reply(
    active: Iceoryx2ActiveRequest<markers::SetEnabledRequest, markers::SetEnabledResponse>,
    request_id: &str,
    dry_run: bool,
    status: RpcStatus,
    error: &str,
    metrics: &RrServerMetrics,
) {
    let _ = dry_run;
    let domain = match status {
        RpcStatus::ResourceExhausted => SetEnabledStatus::QueueFull,
        _ => SetEnabledStatus::Unknown,
    };
    let bytes = build_set_enabled_response_typed(
        request_id,
        status,
        domain,
        0,
        error,
        SideEffect::None,
        true,
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
    response: SetEnabledRrResponse,
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
    let (status, domain, at, error) = encode_payload(&response.payload);
    let bytes = build_set_enabled_response_typed(
        &response.request_id,
        status,
        domain,
        at,
        &error,
        response.effect,
        true,
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
fn encode_payload(payload: &SetEnabledRrPayload) -> (RpcStatus, SetEnabledStatus, u64, String) {
    match payload {
        SetEnabledRrPayload::SupersededByDisable => (
            RpcStatus::Aborted,
            SetEnabledStatus::SupersededByDisable,
            0,
            "superseded by disable".into(),
        ),
        SetEnabledRrPayload::Superseded => (
            RpcStatus::Aborted,
            SetEnabledStatus::Superseded,
            0,
            "superseded".into(),
        ),
        SetEnabledRrPayload::QueueFull => (
            RpcStatus::ResourceExhausted,
            SetEnabledStatus::QueueFull,
            0,
            "IPC lane full".into(),
        ),
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
    #[allow(
        clippy::too_many_lines,
        reason = "逐项固定完整协议映射表，保持十三种结果的对照断言"
    )]
    fn encode_payload_locks_plan_mapping_table() {
        let cases: &[(SetEnabledRrPayload, RpcStatus, SetEnabledStatus, u64)] = &[
            (
                SetEnabledRrPayload::SupersededByDisable,
                RpcStatus::Aborted,
                SetEnabledStatus::SupersededByDisable,
                0,
            ),
            (
                SetEnabledRrPayload::Superseded,
                RpcStatus::Aborted,
                SetEnabledStatus::Superseded,
                0,
            ),
            (
                SetEnabledRrPayload::QueueFull,
                RpcStatus::ResourceExhausted,
                SetEnabledStatus::QueueFull,
                0,
            ),
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
    use crossbeam_channel::{Sender, bounded};
    use iceoryx2::prelude::NodeBuilder;
    use iceoryx2::service::ipc::Service as IxService;
    use robot_core::time::SystemClock;
    use robot_msg::msg::{RpcRole, SetEnabledResponse as FbResp};
    use robot_transport::iceoryx2::request_response::{Iceoryx2PendingResponse, Iceoryx2RrClient};
    use robot_transport::iceoryx2::{RrProfile, rr_client_with, rr_server_with};
    use std::time::Instant;
    type Client = Iceoryx2RrClient<markers::SetEnabledRequest, markers::SetEnabledResponse>;
    type Pending = Iceoryx2PendingResponse<markers::SetEnabledResponse>;
    type Server = Iceoryx2RrServer<markers::SetEnabledRequest, markers::SetEnabledResponse>;
    fn supervised(name: &str) -> bool {
        const ENV: &str = "ROBOT_W3_SET_RR_CASE";
        if std::env::var(ENV).as_deref() == Ok(name) {
            let owner = thread::current().id();
            std::panic::set_hook(Box::new(move |info| {
                eprintln!("Set RR 验收失败：{info}");
                if thread::current().id() == owner {
                    std::process::exit(101);
                }
            }));
            return false;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("child.log");
        let log = std::fs::File::create(&path).unwrap();
        let exact = format!("set_enabled_rr_server::tests::{name}");
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
                    panic!("已 kill/reap Set RR 子进程：{error:?}");
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
            "/w3_set_rr_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }
    fn request(id: &str, enabled: bool, dry: bool) -> Vec<u8> {
        robot_msg::build_helpers::build_set_enabled_request(
            id,
            "idem",
            "source",
            RpcRole::Maintainer,
            enabled,
            dry,
            u64::MAX,
            "audit",
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
    // 仅模拟 IPC 已退休消息来验证 RR 路由；不据此证明真实 Enable/Lower 已执行。
    fn reply(req: SetEnabledRrRequest) -> SetEnabledRrResponse {
        SetEnabledRrResponse {
            route: req.route,
            request_id: req.request_id,
            effect: SideEffect::Started,
            payload: SetEnabledRrPayload::PreemptedByShutdown,
        }
    }
    struct Local {
        server: Server,
        first: Client,
        second: Client,
        ledger: ControlPeerLedger,
        pending: VecDeque<PendingRr>,
        residual: Vec<Slot<RrPeerId>>,
        metrics: RrServerMetrics,
        senders: TopicRequestSenders<SetEnabledRrRequest>,
        high: Receiver<SetEnabledRrRequest>,
        normal: Receiver<SetEnabledRrRequest>,
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
                metrics: RrServerMetrics::default(),
                senders: TopicRequestSenders {
                    high: high_tx,
                    normal: normal_tx,
                },
                high,
                normal,
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
            );
        }
        fn deliver(&mut self, message: SetEnabledRrResponse) {
            handle_response(
                &mut self.pending,
                message,
                &mut self.ledger,
                &mut self.residual,
                &self.metrics,
            );
        }
    }
    #[test]
    fn real_peer_classification_routes_restore_lower_and_dry_to_distinct_lanes() {
        let mut f = Local::new(16);
        for (id, enabled, dry, class) in [
            ("restore", true, false, RrRequestClass::Restore),
            ("lower", false, false, RrRequestClass::Lower),
            ("normal", true, true, RrRequestClass::Normal),
        ] {
            let pending = f.first.send(&request(id, enabled, dry)).unwrap();
            f.consume();
            let forwarded = match class {
                RrRequestClass::Normal => f.normal.try_recv().unwrap(),
                _ => f.high.try_recv().unwrap(),
            };
            assert_eq!(
                (
                    &*forwarded.request_id,
                    forwarded.enabled,
                    forwarded.dry_run,
                    forwarded.deadline_ns
                ),
                (id, enabled, dry, u64::MAX)
            );
            assert_eq!(f.pending.front().unwrap().slot.class(), class);
            assert_eq!(f.ledger.counts().total, 1);
            f.deliver(reply(forwarded));
            let bytes = response(&pending);
            let view = flatbuffers::root::<FbResp>(&bytes).unwrap();
            assert_eq!(view.request_id(), Some(id));
            assert_eq!(view.status(), RpcStatus::Aborted);
            assert_eq!(view.effect(), SideEffect::Started);
            assert!(view.retired());
            assert_eq!(f.ledger.counts().total, 0);
        }
    }
    #[test]
    fn same_peer_duplicate_and_other_peer_identity_do_not_release_original_slots() {
        let mut f = Local::new(16);
        let first = f.first.send(&request("same", true, false)).unwrap();
        f.consume();
        let req1 = f.high.try_recv().unwrap();
        let duplicate = f.first.send(&request("same", true, false)).unwrap();
        f.consume();
        assert_eq!(status(&duplicate), RpcStatus::InvalidArgument);
        assert_eq!(f.ledger.counts().restore, 1);
        let second = f.second.send(&request("same", true, false)).unwrap();
        f.consume();
        let req2 = f.high.try_recv().unwrap();
        assert_ne!(req1.route, req2.route);
        assert_eq!(f.ledger.counts().restore, 2);
        let mut foreign = ControlPeerLedger::new(
            &crate::rr_admission::CheckedLimits::try_from_limits(&f.server.limits().unwrap())
                .unwrap(),
        )
        .unwrap();
        let slot = foreign
            .admit(
                f.pending.front().unwrap().slot.peer(),
                RrRequestClass::Restore,
            )
            .unwrap();
        f.deliver(SetEnabledRrResponse {
            route: slot.identity(),
            request_id: "same".into(),
            payload: SetEnabledRrPayload::AdapterError,
            effect: SideEffect::Started,
        });
        foreign.release(slot).unwrap();
        assert_eq!(f.ledger.counts().restore, 2);
        let old = req1.route;
        f.deliver(reply(req1));
        assert_eq!(status(&first), RpcStatus::Aborted);
        f.deliver(SetEnabledRrResponse {
            route: old,
            request_id: "same".into(),
            payload: SetEnabledRrPayload::AdapterError,
            effect: SideEffect::Started,
        });
        assert_eq!(f.ledger.counts().restore, 1);
        assert!(second.try_recv().unwrap().is_none());
        f.deliver(reply(req2));
        assert_eq!(status(&second), RpcStatus::Aborted);
        assert_eq!(f.ledger.counts().total, 0);
        assert!(f.pending.is_empty() && f.residual.is_empty());
        assert_eq!(f.metrics.duplicate_request_id.load(Ordering::Relaxed), 1);
        assert_eq!(f.metrics.orphan_responses.load(Ordering::Relaxed), 2);
    }
    #[test]
    fn actual_lane_full_returns_only_unsubmitted_slot_and_rejects_bad_factory_capacity() {
        // 不合规小容量仅用于同 handler 的 Full 分支；真实工厂必须拒绝它。
        let mut f = Local::new(1);
        let checked =
            crate::rr_admission::CheckedLimits::try_from_limits(&f.server.limits().unwrap())
                .unwrap();
        assert!(crate::rr_admission::validate_lane_capacity(&checked, 1, 16, 32).is_err());
        let first = f.first.send(&request("first", true, false)).unwrap();
        f.consume();
        let second = f.second.send(&request("second", true, false)).unwrap();
        f.consume();
        assert_eq!(status(&second), RpcStatus::ResourceExhausted);
        assert_eq!(f.ledger.counts().restore, 1);
        assert_eq!(f.metrics.forward_full_errors.load(Ordering::Relaxed), 1);
        let req = f.high.try_recv().unwrap();
        f.deliver(reply(req));
        assert_eq!(status(&first), RpcStatus::Aborted);
        assert_eq!(f.ledger.counts().total, 0);
    }
    #[test]
    fn decode_failure_and_empty_id_never_admit_a_slot() {
        let mut f = Local::new(16);
        for bytes in [b"NOT_FLATBUFFERS".to_vec(), request("", true, false)] {
            let pending = f.first.send(&bytes).unwrap();
            f.consume();
            assert_eq!(status(&pending), RpcStatus::InvalidArgument);
            assert!(f.pending.is_empty());
            assert_eq!(f.ledger.counts().total, 0);
        }
    }
    #[test]
    fn stop_disconnects_both_sources_but_retains_last_admitted_active_until_real_reply() {
        if supervised(
            "stop_disconnects_both_sources_but_retains_last_admitted_active_until_real_reply",
        ) {
            return;
        }
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let topic = topic();
        let profile = RrProfile::S0_CONTROL;
        let client: Client = rr_client_with(&node, &topic, &profile).unwrap();
        let (high_tx, high) = bounded(2 * profile.c);
        let (normal_tx, normal) = bounded(profile.c * (profile.l - 2));
        let (responses, response_rx): (Sender<SetEnabledRrResponse>, _) =
            bounded(profile.c * profile.l);
        let mut handle = spawn_set_enabled_rr_server_thread(
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
        let pending = client.send(&request("retained", true, false)).unwrap();
        let req = high.recv_timeout(Duration::from_secs(2)).unwrap();
        handle.request_stop();
        wait(|| {
            matches!(
                high.try_recv(),
                Err(crossbeam_channel::TryRecvError::Disconnected)
            )
        });
        assert!(matches!(
            normal.try_recv(),
            Err(crossbeam_channel::TryRecvError::Disconnected)
        ));
        assert!(!handle.is_finished());
        assert!(pending.try_recv().unwrap().is_none());
        let late = client.send(&request("after-stop", false, true)).unwrap();
        assert!(late.try_recv().unwrap().is_none());
        responses.send(reply(req)).unwrap();
        assert_eq!(status(&pending), RpcStatus::Aborted);
        wait(|| handle.is_finished());
        assert_eq!(
            handle.metrics().requests_received.load(Ordering::Relaxed),
            1
        );
        handle.join().unwrap();
        drop(late);
    }
}
