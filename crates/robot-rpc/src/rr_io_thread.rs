//! 每 topic 一个真实 RR 客户端线程；handler 取消不释放已经发送的责任。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::class_cache::{AbandonReason, Class, EntryId, EntryState};
use crate::forward_queue::{Popped, QueueItem};
use crate::gateway_ledger::{QueuedGuard, ReplyDisposition, RrPort, RrRoute, SharedLedger};
use crate::wire_credit::TopicKind;
use robot_core::time::RobotClock;
use robot_msg::markers::BodyMarker;
use robot_transport::iceoryx2::request_response::{
    Iceoryx2PendingResponse, Iceoryx2RrClient, RrSendError, StrictChannelStatus,
};

/// 唯一的转发载荷；guard 只负责尚未发出的原占位。
pub struct RrForwardRequest {
    pub wire_bytes: Vec<u8>,
    pub oneshot_tx: tokio::sync::oneshot::Sender<Vec<u8>>,
    pub request_id: String,
    pub(crate) entry_id: EntryId,
    pub(crate) class: Class,
    pub(crate) deadline_ns: u64,
    pub(crate) route: RrRoute,
    pub(crate) guard: QueuedGuard,
}
impl QueueItem for RrForwardRequest {
    fn entry_id(&self) -> EntryId {
        self.entry_id
    }
    fn class(&self) -> Class {
        self.class
    }
    fn deadline_ns(&self) -> u64 {
        self.deadline_ns
    }
}
impl core::fmt::Debug for RrForwardRequest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RrForwardRequest")
            .field("request_id", &self.request_id)
            .field("entry_id", &self.entry_id)
            .field("class", &self.class)
            .field("wire_bytes_len", &self.wire_bytes.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct RrIoThreadConfig {
    pub topic: String,
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

#[derive(Default, Debug)]
pub struct RrIoMetrics {
    pub requests_sent: AtomicU64,
    pub responses_received: AtomicU64,
    pub send_errors: AtomicU64,
    /// 已入队回执附带诊断，原 `Pending` 仍按 `Sent` 持有。
    pub delivery_issues: AtomicU64,
    pub poll_errors: AtomicU64,
    /// 接收者关闭只登记一次 Orphan，不丢传输对象、不归还信用。
    pub handler_drops: AtomicU64,
    /// 正常关闭时实际释放的传输对象数；不是已退休或已退款计数。
    pub orphan_drops_on_shutdown: AtomicU64,
    pub loop_tick_count: AtomicU64,
    #[cfg(all(test, feature = "test-rr-fixtures"))]
    fixture_snapshots: std::sync::Mutex<
        Vec<
            Result<
                robot_transport::iceoryx2::QueueFixtureSnapshot,
                robot_transport::iceoryx2::FixtureError,
            >,
        >,
    >,
    #[cfg(all(test, feature = "test-rr-fixtures"))]
    unknown_observations: AtomicU64,
    #[cfg(any(test, feature = "test-freeze-hook"))]
    pub freeze_until_ns: AtomicU64,
}

/// 字段析构次序保证真实 client 先于创建它的 Node 释放，不泄漏 Node。
pub(crate) struct OwnedRrClientPorts<Req: BodyMarker, Resp: BodyMarker> {
    pub(crate) client: Iceoryx2RrClient<Req, Resp>,
    pub(crate) _node: iceoryx2::node::Node<iceoryx2::service::ipc::Service>,
}

/// 尚未清偿的 Send 责任随真实 join 返回；跨线程的共享账本仍是同一份。
pub struct RrIoExit {
    pub(crate) port: Option<RrPort>,
    pub(crate) stranded: Option<Popped<RrForwardRequest>>,
}
impl RrIoExit {
    pub(crate) fn responsibilities_empty(&self) -> bool {
        self.stranded.is_none() && self.port.as_ref().is_none_or(RrPort::responsibility_empty)
    }
}
impl std::fmt::Debug for RrIoExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RrIoExit")
            .field("has_port", &self.port.is_some())
            .field("has_stranded", &self.stranded.is_some())
            .finish_non_exhaustive()
    }
}

pub struct RrIoThreadHandle {
    join: JoinHandle<RrIoExit>,
    stop: Arc<AtomicBool>,
    metrics: Arc<RrIoMetrics>,
}
impl std::fmt::Debug for RrIoThreadHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RrIoThreadHandle")
            .field("finished", &self.join.is_finished())
            .finish_non_exhaustive()
    }
}
impl RrIoThreadHandle {
    #[must_use]
    pub fn metrics(&self) -> Arc<RrIoMetrics> {
        Arc::clone(&self.metrics)
    }
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }
    pub fn join(self) -> thread::Result<RrIoExit> {
        self.join.join()
    }
}

/// 只用于 owned 关闭测试：创建真实标准线程，后续一律经过生产 handle.join。
#[cfg(test)]
pub(crate) fn spawn_join_fixture(body: impl FnOnce() + Send + 'static) -> RrIoThreadHandle {
    let join = std::thread::spawn(move || {
        body();
        RrIoExit {
            port: None,
            stranded: None,
        }
    });
    RrIoThreadHandle {
        join,
        stop: Arc::new(AtomicBool::new(false)),
        metrics: Arc::new(RrIoMetrics::default()),
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RrIoSpawnError {
    #[error("RR 线程创建失败: {0}")]
    ThreadSpawn(#[source] std::io::Error),
    #[error("RR ready 失败: {error}")]
    ReadyFailed {
        handle: RrIoThreadHandle,
        error: String,
    },
}

struct InFlight<Resp: BodyMarker> {
    pending: Iceoryx2PendingResponse<Resp>,
    oneshot_tx: tokio::sync::oneshot::Sender<Vec<u8>>,
    entry_id: EntryId,
    orphan: bool,
}

/// factory 与 limits 均在真实客户端线程产生；ready 失败原句柄仍按值返还。
pub(crate) fn spawn_rr_io_thread<Req, Resp, F>(
    cfg: &RrIoThreadConfig,
    client_factory: F,
    ledger: SharedLedger,
    route: RrRoute,
    kind: TopicKind,
    clock: Arc<dyn RobotClock>,
) -> Result<(RrIoThreadHandle, RrPort), RrIoSpawnError>
where
    Req: BodyMarker,
    Resp: BodyMarker,
    F: FnOnce() -> Result<OwnedRrClientPorts<Req, Resp>, String> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let metrics = Arc::new(RrIoMetrics::default());
    let thread_stop = Arc::clone(&stop);
    let thread_metrics = Arc::clone(&metrics);
    let cfg = cfg.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
    let join = thread::Builder::new()
        .name(thread_name_for(&cfg.topic))
        .spawn(move || {
            let owned = match client_factory() {
                Ok(owned) => owned,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return RrIoExit {
                        port: None,
                        stranded: None,
                    };
                }
            };
            let port = match owned.client.limits() {
                Some(limits) => RrPort::new(route, kind, &limits, &ledger),
                None => Err("RR client 未给出受检实际 limits".to_owned()),
            };
            let port = match port {
                Ok(port) => port,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return RrIoExit {
                        port: None,
                        stranded: None,
                    };
                }
            };
            if ready_tx.send(Ok(port.clone())).is_err() {
                return RrIoExit {
                    port: Some(port),
                    stranded: None,
                };
            }
            drop(ready_tx);
            run_loop(
                &cfg,
                &owned.client,
                port,
                &thread_stop,
                &thread_metrics,
                clock.as_ref(),
            )
        })
        .map_err(RrIoSpawnError::ThreadSpawn)?;
    let handle = RrIoThreadHandle {
        join,
        stop,
        metrics,
    };
    match ready_rx.recv() {
        Ok(Ok(port)) => Ok((handle, port)),
        Ok(Err(error)) => Err(RrIoSpawnError::ReadyFailed { handle, error }),
        Err(error) => Err(RrIoSpawnError::ReadyFailed {
            handle,
            error: format!("factory 在 ready 前断链: {error}"),
        }),
    }
}

fn thread_name_for(topic: &str) -> String {
    let suffix = topic
        .rsplit('/')
        .next()
        .unwrap_or(topic)
        .chars()
        .take(6)
        .collect::<String>();
    format!("gw_rr_io_{suffix}")
}

fn drop_transport<Resp: BodyMarker>(item: InFlight<Resp>, port: &RrPort, now_ns: u64) {
    let InFlight {
        pending,
        oneshot_tx,
        entry_id,
        ..
    } = item;
    drop(pending);
    // NoHandle 只在真实对象已析构后记录，stop 位本身不推动状态。
    port.ledger.lock().expect("网关责任锁未中毒").transition(
        entry_id,
        EntryState::NoHandle,
        now_ns,
    );
    drop(oneshot_tx);
}

fn poll_inflight<Resp: BodyMarker>(
    in_flight: &mut Vec<InFlight<Resp>>,
    port: &RrPort,
    metrics: &RrIoMetrics,
    clock: &dyn RobotClock,
) {
    #[cfg(test)]
    poll_inflight_impl(in_flight, port, metrics, clock, &mut |_| {});
    #[cfg(not(test))]
    poll_inflight_impl(in_flight, port, metrics, clock);
}

fn poll_inflight_impl<Resp: BodyMarker>(
    in_flight: &mut Vec<InFlight<Resp>>,
    port: &RrPort,
    metrics: &RrIoMetrics,
    clock: &dyn RobotClock,
    #[cfg(test)] after_empty: &mut dyn FnMut(EntryId),
) {
    let mut index = 0;
    while index < in_flight.len() {
        // handler 超时独立于收包结果；持续 Unknown/更新错误也只登记一次 Orphan。
        let item = &mut in_flight[index];
        if !item.orphan && item.oneshot_tx.is_closed() {
            item.orphan = true;
            port.ledger.lock().expect("网关责任锁未中毒").transition(
                item.entry_id,
                EntryState::Orphan,
                clock.now().0,
            );
            metrics.handler_drops.fetch_add(1, Ordering::Relaxed);
        }
        // 已发送对象不受 handler 的等待寿命控制；空读与闭连查询之间仍可能收到回复。
        let mut received = in_flight[index].pending.try_recv();
        let mut closed_empty = false;
        if matches!(received, Ok(None)) {
            #[cfg(test)]
            after_empty(in_flight[index].entry_id);
            let state = in_flight[index].pending.connection_state();
            #[cfg(all(test, feature = "test-rr-fixtures"))]
            if state == StrictChannelStatus::Unknown {
                metrics.unknown_observations.fetch_add(1, Ordering::Relaxed);
            }
            if state == StrictChannelStatus::Closed {
                // 只补收一次以覆盖两次查询之间的发送；闭连本身不是队列排空凭证。
                received = in_flight[index].pending.try_recv();
                closed_empty = matches!(received, Ok(None));
            }
        }
        match received {
            Ok(Some(bytes)) => {
                let id = in_flight[index].entry_id;
                let disposition = port.ledger.lock().expect("网关责任锁未中毒").finish_reply(
                    id,
                    &bytes,
                    clock.now().0,
                );
                match disposition {
                    ReplyDisposition::Mismatch => {
                        index += 1;
                    }
                    ReplyDisposition::Settled | ReplyDisposition::Unretired => {
                        let item = in_flight.swap_remove(index);
                        let InFlight {
                            pending,
                            oneshot_tx,
                            entry_id,
                            ..
                        } = item;
                        drop(pending);
                        if disposition == ReplyDisposition::Unretired {
                            port.ledger.lock().expect("网关责任锁未中毒").transition(
                                entry_id,
                                EntryState::NoHandle,
                                clock.now().0,
                            );
                        }
                        metrics.responses_received.fetch_add(1, Ordering::Relaxed);
                        let _ = oneshot_tx.send(bytes);
                    }
                    ReplyDisposition::Faulted => {
                        port.stop_admission();
                        let item = in_flight.swap_remove(index);
                        drop_transport(item, port, clock.now().0);
                    }
                }
            }
            Ok(None) => {
                if closed_empty {
                    let item = in_flight.swap_remove(index);
                    drop_transport(item, port, clock.now().0);
                } else {
                    index += 1;
                }
            }
            Err(error) => {
                #[cfg(all(test, feature = "test-rr-fixtures"))]
                if in_flight[index].pending.connection_state() == StrictChannelStatus::Unknown {
                    metrics.unknown_observations.fetch_add(1, Ordering::Relaxed);
                }
                // E3 只有收包错误，不证明投递或退休；原对象与原许可继续保留。
                metrics.poll_errors.fetch_add(1, Ordering::Relaxed);
                tracing::error!(?error, "RR 收包失败，保留原在途责任");
                index += 1;
            }
        }
    }
}

fn run_loop<Req: BodyMarker, Resp: BodyMarker>(
    cfg: &RrIoThreadConfig,
    client: &Iceoryx2RrClient<Req, Resp>,
    port: RrPort,
    stop: &AtomicBool,
    metrics: &RrIoMetrics,
    clock: &dyn RobotClock,
) -> RrIoExit {
    let mut in_flight = Vec::new();
    let mut stranded = None;
    while !stop.load(Ordering::Acquire) {
        metrics.loop_tick_count.fetch_add(1, Ordering::Relaxed);
        #[cfg(any(test, feature = "test-freeze-hook"))]
        crate::liveness::freeze_check(&metrics.freeze_until_ns, clock, stop);
        poll_inflight(&mut in_flight, &port, metrics, clock);
        // 两种既定 queue profile 总容量均为 64；每轮最多接管这一有界批次。
        for _ in 0..64 {
            if stop.load(Ordering::Acquire) || port.is_stopped() {
                break;
            }
            let Some(popped) = port.queue.pop_eligible(&port.credits) else {
                break;
            };
            let request = match port.attach_pop(popped, clock.now().0) {
                Ok(request) => request,
                Err(original) => {
                    stranded = Some(original);
                    break;
                }
            };
            let id = request.entry_id;
            if clock.now().0 >= request.deadline_ns {
                if !port
                    .ledger
                    .lock()
                    .expect("网关责任锁未中毒")
                    .abandon_before_delivery(id, AbandonReason::ExpiredBeforeSend, clock.now().0)
                {
                    port.stop_admission();
                }
                continue;
            }
            match client.send_typed(&request.wire_bytes) {
                Ok(pending) => {
                    let sent = port.ledger.lock().expect("网关责任锁未中毒").transition(
                        id,
                        EntryState::Sent,
                        clock.now().0,
                    );
                    let item = InFlight {
                        pending,
                        oneshot_tx: request.oneshot_tx,
                        entry_id: id,
                        orphan: false,
                    };
                    if sent {
                        in_flight.push(item);
                        if let Some(issue) = in_flight
                            .last()
                            .and_then(|item| item.pending.delivery_issue())
                        {
                            metrics.delivery_issues.fetch_add(1, Ordering::Relaxed);
                            tracing::error!(?issue, "RR 请求已入队，保留原 Pending 与 Sent 责任");
                        }
                        metrics.requests_sent.fetch_add(1, Ordering::Relaxed);
                    } else {
                        port.stop_admission();
                        drop_transport(item, &port, clock.now().0);
                    }
                }
                Err(RrSendError::NotDelivered(_)) => {
                    metrics.send_errors.fetch_add(1, Ordering::Relaxed);
                    if !port
                        .ledger
                        .lock()
                        .expect("网关责任锁未中毒")
                        .abandon_before_delivery(id, AbandonReason::NotDelivered, clock.now().0)
                    {
                        port.stop_admission();
                    }
                }
            }
            #[cfg(all(test, feature = "test-rr-fixtures"))]
            metrics
                .fixture_snapshots
                .lock()
                .expect("真实夹具快照锁未中毒")
                .push(client.request_queue_fixture_snapshot());
        }
        thread::sleep(Duration::from_nanos(cfg.poll_interval_ns));
    }
    port.stop_admission();
    metrics.orphan_drops_on_shutdown.fetch_add(
        u64::try_from(in_flight.len()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    for item in in_flight {
        drop_transport(item, &port, clock.now().0);
    }
    RrIoExit {
        port: Some(port),
        stranded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway_ledger::{BeginWrite, GatewayLedger, HandlerWait};
    use crate::idempotency::Method;
    use iceoryx2::prelude::NodeBuilder;
    use iceoryx2::service::ipc::Service as IxService;
    use robot_core::time::SystemClock;
    use robot_msg::{
        markers,
        msg::{RpcRole, RpcStatus, SetEnabledStatus, SideEffect},
    };
    use robot_transport::iceoryx2::request_response::{Iceoryx2ActiveRequest, Iceoryx2RrServer};
    use robot_transport::iceoryx2::{RrProfile, rr_client_with, rr_server_with};
    use std::sync::atomic::AtomicU32;
    type Server = Iceoryx2RrServer<markers::SetEnabledRequest, markers::SetEnabledResponse>;
    type Active = Iceoryx2ActiveRequest<markers::SetEnabledRequest, markers::SetEnabledResponse>;

    // fixture 只提供 daemon 响应，client 线程、profile、队列、permit、分类/清偿全部为生产路径。
    struct Fixture {
        handle: Option<RrIoThreadHandle>,
        server: Server,
        port: RrPort,
        _node: iceoryx2::node::Node<IxService>,
    }
    impl Fixture {
        fn new() -> Self {
            static SEQ: AtomicU32 = AtomicU32::new(0);
            let topic = format!(
                "/w3_rr_{}_{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            );
            let node = NodeBuilder::new().create::<IxService>().unwrap();
            let server: Server = rr_server_with(&node, &topic, &RrProfile::S0_CONTROL).unwrap();
            let cfg = RrIoThreadConfig {
                topic: topic.clone(),
                poll_interval_ns: 500_000,
            };
            let ledger = GatewayLedger::new(60_000_000_000, 4096).unwrap();
            let (handle, port) = spawn_rr_io_thread(
                &cfg,
                move || {
                    let node = NodeBuilder::new().create::<IxService>().unwrap();
                    let client = rr_client_with::<
                        markers::SetEnabledRequest,
                        markers::SetEnabledResponse,
                    >(&node, &topic, &RrProfile::S0_CONTROL)
                    .unwrap();
                    Ok(OwnedRrClientPorts {
                        client,
                        _node: node,
                    })
                },
                ledger,
                RrRoute::Write(Method::SetEnabled),
                TopicKind::S0Control,
                Arc::new(SystemClock),
            )
            .unwrap();
            Self {
                handle: Some(handle),
                server,
                port,
                _node: node,
            }
        }
        fn enqueue(&self, id: &str, class: Class) -> HandlerWait {
            let BeginWrite::Fresh(entry_id) = self.port.ledger.lock().unwrap().begin_write(
                Method::SetEnabled,
                id,
                [1; 32],
                class,
                id,
                SystemClock.now().0,
            ) else {
                panic!("新身份必须准入");
            };
            let (enabled, dry_run) = match class {
                Class::Normal => (false, true),
                Class::Restore => (true, false),
                Class::Lower => (false, false),
                Class::Emergency => panic!("RR 不使用 Emergency"),
            };
            let bytes = robot_msg::build_helpers::build_set_enabled_request(
                id,
                id,
                "fixture",
                RpcRole::Maintainer,
                enabled,
                dry_run,
                u64::MAX,
                "w3",
                SystemClock.now().0,
            );
            let (oneshot_tx, receiver) = tokio::sync::oneshot::channel();
            let request = RrForwardRequest {
                wire_bytes: bytes,
                oneshot_tx,
                request_id: id.into(),
                entry_id,
                class,
                deadline_ns: u64::MAX,
                route: self.port.route,
                guard: QueuedGuard::new(&self.port.ledger, entry_id),
            };
            let ticket = self.port.push(request).unwrap();
            HandlerWait::new(&self.port, entry_id, ticket, receiver)
        }
        fn active(&self) -> Active {
            wait("真实 wire active", || self.server.try_recv().unwrap())
        }
        fn metrics(&self) -> Arc<RrIoMetrics> {
            self.handle.as_ref().unwrap().metrics()
        }
        fn finish(&mut self) -> RrIoExit {
            let handle = self.handle.take().unwrap();
            handle.request_stop();
            handle.join().unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                handle.request_stop();
                let result = handle.join();
                if !result.as_ref().is_ok_and(RrIoExit::responsibilities_empty) {
                    let _held = std::mem::ManuallyDrop::new((result, self.port.clone()));
                }
            }
        }
    }
    fn wait<T>(what: &str, mut poll: impl FnMut() -> Option<T>) -> T {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(value) = poll() {
                return value;
            }
            assert!(std::time::Instant::now() < deadline, "等待 {what} 超时");
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn receive(waiter: &mut HandlerWait) -> Vec<u8> {
        wait("原 oneshot 响应", || match waiter.receiver.try_recv() {
            Ok(bytes) => Some(bytes),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
            Err(error) => panic!("原响应断链: {error}"),
        })
    }
    fn id(active: &Active) -> String {
        flatbuffers::root::<robot_msg::msg::SetEnabledRequest>(active.payload())
            .unwrap()
            .request_id()
            .unwrap()
            .into()
    }
    fn reply(active: Active, at: u64) {
        let request_id = id(&active);
        active
            .send_response(&robot_msg::build_helpers::build_set_enabled_response_typed(
                &request_id,
                RpcStatus::Ok,
                SetEnabledStatus::Ok,
                at,
                "",
                SideEffect::Committed,
                true,
            ))
            .unwrap();
    }
    fn assert_response(bytes: &[u8], request_id: &str, at: u64) {
        let response = flatbuffers::root::<robot_msg::msg::SetEnabledResponse>(bytes).unwrap();
        assert_eq!(response.request_id(), Some(request_id));
        assert_eq!(response.status(), RpcStatus::Ok);
        assert_eq!(response.completed_at_ns(), at);
        assert!(response.retired());
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "同一真实 RR 场景固定空读后发送并闭连的交错，保留原许可、响应与服务器回收链"
    )]
    fn last_read_reply_sent_after_empty_is_consumed_before_transport_drop() {
        use robot_msg::msg::{ActionRpcStatus, GetTaskStatusResponse, TaskStateFb};
        use std::sync::mpsc;

        struct OwnedAfterEmpty {
            handle: Option<JoinHandle<()>>,
            deadline: u64,
            cancel: Arc<AtomicBool>,
            release: mpsc::Sender<()>,
            finish: mpsc::Sender<()>,
            in_flight: Vec<InFlight<markers::GetTaskStatusResponse>>,
            port: Option<RrPort>,
            client: Option<
                Iceoryx2RrClient<markers::GetTaskStatusRequest, markers::GetTaskStatusResponse>,
            >,
            node: Option<iceoryx2::node::Node<IxService>>,
        }
        impl OwnedAfterEmpty {
            fn join(&mut self) -> thread::Result<()> {
                let handle = self.handle.take().expect("原服务器尚未 join");
                while !handle.is_finished() {
                    if SystemClock.now().0 >= self.deadline {
                        // 原截止内无法真 join 时终止父进程；不先析构 Pending/Node。
                        std::process::abort();
                    }
                    thread::yield_now();
                }
                handle.join()
            }
        }
        impl Drop for OwnedAfterEmpty {
            fn drop(&mut self) {
                // panic 可能发生于请求安装前或任一同步点；释放原信号后先真 join。
                self.cancel.store(true, Ordering::Release);
                let _ = self.release.send(());
                let _ = self.finish.send(());
                let joined = self.handle.is_none() || self.join().is_ok();
                if let Some(port) = self.port.take()
                    && !port
                        .ledger
                        .lock()
                        .is_ok_and(|ledger| ledger.responsibilities_empty())
                {
                    // RED 保原 NoHandle/permit 到测试进程退出，不能因异常析构伪退款。
                    let _held =
                        std::mem::ManuallyDrop::new((std::mem::take(&mut self.in_flight), port));
                }
                if !thread::panicking() {
                    assert!(joined, "真实服务器线程必须成功 join");
                }
            }
        }
        const REQUEST_ID: &str = "after-empty-read";
        const ACTION_ID: &str = "original-action";
        let deadline = SystemClock.now().0 + 3_000_000_000;
        let topic = format!("/w3_last_read_reply_{}", std::process::id());
        let server_topic = topic.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (sent_tx, sent_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let server_cancel = Arc::clone(&cancel);
        let handle = thread::spawn(move || {
            let node = NodeBuilder::new().create::<IxService>().unwrap();
            let server = rr_server_with::<
                markers::GetTaskStatusRequest,
                markers::GetTaskStatusResponse,
            >(&node, &server_topic, &RrProfile::S1_TASK)
            .unwrap();
            ready_tx.send(()).unwrap();
            let active = loop {
                if server_cancel.load(Ordering::Acquire) {
                    return;
                }
                if let Some(active) = server.try_recv().unwrap() {
                    break active;
                }
                assert!(SystemClock.now().0 < deadline, "等待原 ReadStatus 请求超时");
                thread::yield_now();
            };
            let request =
                flatbuffers::root::<robot_msg::msg::GetTaskStatusRequest>(active.payload())
                    .unwrap();
            assert_eq!(request.request_id(), Some(REQUEST_ID));
            assert_eq!(request.action_id(), Some(ACTION_ID));
            release_rx
                .recv_timeout(Duration::from_nanos(
                    deadline.saturating_sub(SystemClock.now().0),
                ))
                .unwrap();
            let bytes = robot_msg::build_helpers::build_get_task_status_response(
                REQUEST_ID,
                ActionRpcStatus::Ok,
                "",
                ACTION_ID,
                TaskStateFb::Executing,
                0.5,
                None,
                None,
            );
            // 按值 send_response 返回前已真实发送并 Drop 原 ActiveRequest。
            active.send_response(&bytes).unwrap();
            sent_tx.send(()).unwrap();
            // 原服务保持存在，隔离 ActiveRequest 关闭与整个服务退出。
            finish_rx
                .recv_timeout(Duration::from_nanos(
                    deadline.saturating_sub(SystemClock.now().0),
                ))
                .unwrap();
        });
        let mut owned = OwnedAfterEmpty {
            handle: Some(handle),
            deadline,
            cancel,
            release: release_tx,
            finish: finish_tx,
            in_flight: Vec::new(),
            port: None,
            client: None,
            node: None,
        };
        ready_rx
            .recv_timeout(Duration::from_nanos(
                deadline.saturating_sub(SystemClock.now().0),
            ))
            .unwrap();
        owned.node = Some(NodeBuilder::new().create::<IxService>().unwrap());
        owned.client = Some(
            rr_client_with::<markers::GetTaskStatusRequest, markers::GetTaskStatusResponse>(
                owned.node.as_ref().unwrap(),
                &topic,
                &RrProfile::S1_TASK,
            )
            .unwrap(),
        );
        let client = owned.client.as_ref().unwrap();
        let ledger = GatewayLedger::new(60_000_000_000, 4096).unwrap();
        let port = RrPort::new(
            RrRoute::ReadStatus,
            TopicKind::S1Task,
            &client.limits().expect("显式 S1_TASK 工厂返回真实容量"),
            &ledger,
        )
        .unwrap();
        owned.port = Some(port.clone());
        let entry_id = ledger
            .lock()
            .unwrap()
            .begin_read(REQUEST_ID, &port.credits)
            .unwrap();
        let (oneshot_tx, receiver) = tokio::sync::oneshot::channel();
        let request = RrForwardRequest {
            wire_bytes: robot_msg::build_helpers::build_get_task_status_request(
                REQUEST_ID,
                "",
                "fixture",
                RpcRole::Maintainer,
                "",
                ACTION_ID,
                deadline,
                SystemClock.now().0,
            ),
            oneshot_tx,
            request_id: REQUEST_ID.into(),
            entry_id,
            class: Class::Normal,
            deadline_ns: deadline,
            route: RrRoute::ReadStatus,
            guard: QueuedGuard::new(&ledger, entry_id),
        };
        let ticket = port.push(request).unwrap();
        let mut waiter = HandlerWait::new(&port, entry_id, ticket, receiver);
        let popped = port.queue.pop_eligible(&port.credits).unwrap();
        let request = match port.attach_pop(popped, SystemClock.now().0) {
            Ok(request) => request,
            Err(original) => {
                // 装配失败也保留原队列责任与许可，不能靠断言析构充作归还。
                let _held = std::mem::ManuallyDrop::new((original, port.clone()));
                panic!("真实队列弹出必须能移交原只读责任");
            }
        };
        let pending = client.send_typed(&request.wire_bytes).unwrap();
        owned.in_flight.push(InFlight {
            pending,
            oneshot_tx: request.oneshot_tx,
            entry_id,
            orphan: false,
        });
        // 入队 issue/三态断言也必须先有同层 owner 持原 Pending 到服务器真 join。
        let pending = &owned.in_flight[0].pending;
        assert!(pending.delivery_issue().is_none());
        assert_eq!(pending.connection_state(), StrictChannelStatus::Open);
        assert!(
            ledger
                .lock()
                .unwrap()
                .transition(entry_id, EntryState::Sent, SystemClock.now().0)
        );
        let metrics = RrIoMetrics::default();
        let mut hook_calls = 0;
        poll_inflight_impl(
            &mut owned.in_flight,
            &port,
            &metrics,
            &SystemClock,
            &mut |id| {
                assert_eq!(id, entry_id);
                hook_calls += 1;
                assert_eq!(hook_calls, 1);
                owned.release.send(()).unwrap();
                sent_rx
                    .recv_timeout(Duration::from_nanos(
                        deadline.saturating_sub(SystemClock.now().0),
                    ))
                    .unwrap();
            },
        );
        poll_inflight(&mut owned.in_flight, &port, &metrics, &SystemClock);
        let response = waiter.receiver.try_recv();
        owned.finish.send(()).unwrap();
        assert!(owned.join().is_ok());
        let empty = ledger.lock().unwrap().responsibilities_empty();
        let used = port.credits.used(Class::Normal);
        assert_eq!(hook_calls, 1, "必须实际经过空读后发送的同步点");
        let bytes = response.expect("最后真实 ReadStatus 响应不能被闭连分支丢弃");
        let view = flatbuffers::root::<GetTaskStatusResponse>(&bytes).unwrap();
        assert_eq!(view.request_id(), Some(REQUEST_ID));
        assert_eq!(view.action_id(), Some(ACTION_ID));
        assert_eq!(view.status(), ActionRpcStatus::Ok);
        assert_eq!(view.state(), TaskStateFb::Executing);
        assert_eq!(view.progress(), 0.5);
        assert_eq!(metrics.responses_received.load(Ordering::Acquire), 1);
        assert!(empty, "真实只读 owner 必须随匹配响应退休");
        assert_eq!(used, 0, "原只读 permit 必须且只能归还一次");
    }

    #[cfg(feature = "test-rr-fixtures")]
    mod fault_bridge {
        use super::*;
        use crate::gateway_ledger::exchange_read;
        use robot_msg::msg::{ActionRpcStatus, GetTaskStatusResponse, TaskStateFb};
        use robot_transport::iceoryx2::{
            QueueFailure, QueueFixtureSnapshot, REQUEST_QUEUE_FIXTURE_AUXILIARY,
            SenderCleanupRecord,
        };
        use std::sync::mpsc;

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Mode {
            Queue(QueueFailure),
            Unknown,
            Closed,
        }

        #[derive(Clone, Copy, Debug, Default)]
        struct ServerProof {
            connection_empty_polls: usize,
            auxiliary: usize,
            business: usize,
            cleanup: Option<SenderCleanupRecord>,
        }

        struct FaultOwned {
            server: Option<JoinHandle<Result<ServerProof, String>>>,
            gateway: Option<RrIoThreadHandle>,
            exit: Option<RrIoExit>,
            port: Option<RrPort>,
            cancel: Arc<AtomicBool>,
            connect: mpsc::Sender<()>,
            release: mpsc::Sender<()>,
            finish: mpsc::Sender<()>,
            proof: mpsc::Receiver<ServerProof>,
            deadline: u64,
        }

        impl FaultOwned {
            #[allow(
                clippy::too_many_lines,
                reason = "原双线程、启动屏障与异常收尾须在同一拥有域构建"
            )]
            fn new(mode: Mode) -> Self {
                static SEQ: AtomicU32 = AtomicU32::new(0);
                let topic = format!(
                    "/r3_gateway_fixture_{}_{}",
                    std::process::id(),
                    SEQ.fetch_add(1, Ordering::Relaxed)
                );
                let server_topic = topic.clone();
                let deadline = SystemClock.now().0 + 3_000_000_000;
                let cancel = Arc::new(AtomicBool::new(false));
                let server_cancel = Arc::clone(&cancel);
                let (ready_tx, ready_rx) = mpsc::channel();
                let (connect_tx, connect_rx) = mpsc::channel();
                let (connected_tx, connected_rx) = mpsc::channel();
                let (release, release_rx) = mpsc::channel();
                let (finish, finish_rx) = mpsc::channel();
                let (proof_tx, proof) = mpsc::channel();
                let server = thread::spawn(move || {
                    let node = NodeBuilder::new()
                        .create::<IxService>()
                        .map_err(|e| e.to_string())?;
                    let server = rr_server_with::<
                        markers::GetTaskStatusRequest,
                        markers::GetTaskStatusResponse,
                    >(&node, &server_topic, &RrProfile::S1_TASK)
                    .map_err(|e| e.to_string())?;
                    ready_tx.send(()).map_err(|e| e.to_string())?;
                    connect_rx
                        .recv_timeout(remaining(deadline))
                        .map_err(|e| e.to_string())?;
                    if server_cancel.load(Ordering::Acquire) {
                        return Ok(ServerProof::default());
                    }
                    if server.try_recv().map_err(|e| e.to_string())?.is_some() {
                        return Err("seed前原连接握手不应已有请求".into());
                    }
                    connected_tx.send(()).map_err(|e| e.to_string())?;
                    release_rx
                        .recv_timeout(remaining(deadline))
                        .map_err(|e| e.to_string())?;
                    let mut proof = ServerProof {
                        connection_empty_polls: 1,
                        ..ServerProof::default()
                    };
                    let mut held = None;
                    let total = match mode {
                        Mode::Queue(_) => 4,
                        Mode::Unknown | Mode::Closed => 1,
                    };
                    for _ in 0..total {
                        let active = loop {
                            if server_cancel.load(Ordering::Acquire) {
                                return Ok(proof);
                            }
                            if let Some(active) = server.try_recv().map_err(|e| e.to_string())? {
                                break active;
                            }
                            if SystemClock.now().0 >= deadline {
                                return Err("原服务器等待请求超过3s观察窗".into());
                            }
                            thread::yield_now();
                        };
                        if active.payload() == REQUEST_QUEUE_FIXTURE_AUXILIARY {
                            if mode != Mode::Queue(QueueFailure::NotEnqueued)
                                || proof.auxiliary != 0
                                || proof.business != 0
                            {
                                return Err("辅助A不在原队首或意外进入业务".into());
                            }
                            proof.auxiliary += 1;
                            drop(active);
                            continue;
                        }
                        let request = flatbuffers::root::<robot_msg::msg::GetTaskStatusRequest>(
                            active.payload(),
                        )
                        .map_err(|e| e.to_string())?;
                        let request_id = request.request_id().ok_or("原请求id缺失")?;
                        let bytes = robot_msg::build_helpers::build_get_task_status_response(
                            request_id,
                            ActionRpcStatus::Ok,
                            "",
                            "original-action",
                            TaskStateFb::Executing,
                            0.5,
                            None,
                            None,
                        );
                        proof.business += 1;
                        match mode {
                            Mode::Queue(_) => {
                                active.send_response(&bytes).map_err(|e| e.to_string())?;
                            }
                            Mode::Unknown => {
                                proof.cleanup = Some(
                                    active
                                        .fixture_abandon_response_sender()
                                        .map_err(|e| e.to_string())?,
                                );
                                held = Some(active);
                            }
                            Mode::Closed => drop(active),
                        }
                    }
                    proof_tx.send(proof).map_err(|e| e.to_string())?;
                    // 原Active（Unknown）与server/Node留到真实finish，不以清理记录代替退出。
                    finish_rx
                        .recv_timeout(remaining(deadline))
                        .map_err(|e| e.to_string())?;
                    drop(held);
                    drop(server);
                    drop(node);
                    Ok(proof)
                });
                let mut owned = Self {
                    server: Some(server),
                    gateway: None,
                    exit: None,
                    port: None,
                    cancel,
                    connect: connect_tx.clone(),
                    release,
                    finish,
                    proof,
                    deadline,
                };
                ready_rx
                    .recv_timeout(remaining(deadline))
                    .expect("原服务器已就绪");
                let cfg = RrIoThreadConfig {
                    topic: topic.clone(),
                    poll_interval_ns: 500_000,
                };
                let ledger = GatewayLedger::new(60_000_000_000, 4096).unwrap();
                let spawned = spawn_rr_io_thread(
                    &cfg,
                    move || {
                        let node = NodeBuilder::new()
                            .create::<IxService>()
                            .map_err(|e| e.to_string())?;
                        let client = rr_client_with::<
                            markers::GetTaskStatusRequest,
                            markers::GetTaskStatusResponse,
                        >(&node, &topic, &RrProfile::S1_TASK)
                        .map_err(|e| e.to_string())?;
                        connect_tx.send(()).map_err(|e| e.to_string())?;
                        connected_rx
                            .recv_timeout(remaining(deadline))
                            .map_err(|e| e.to_string())?;
                        if let Mode::Queue(mode) = mode {
                            client
                                .install_request_queue_fixture(mode)
                                .map_err(|e| e.to_string())?;
                        }
                        Ok(OwnedRrClientPorts {
                            client,
                            _node: node,
                        })
                    },
                    ledger,
                    RrRoute::ReadStatus,
                    TopicKind::S1Task,
                    Arc::new(SystemClock),
                );
                match spawned {
                    Ok((handle, port)) => {
                        owned.gateway = Some(handle);
                        owned.port = Some(port);
                    }
                    Err(RrIoSpawnError::ReadyFailed { handle, error }) => {
                        owned.gateway = Some(handle);
                        panic!("原Gateway工厂失败：{error}");
                    }
                    Err(RrIoSpawnError::ThreadSpawn(error)) => {
                        panic!("原Gateway线程未创建：{error}")
                    }
                }
                owned
            }

            fn port(&self) -> &RrPort {
                self.port.as_ref().unwrap()
            }
            fn metrics(&self) -> Arc<RrIoMetrics> {
                self.gateway.as_ref().unwrap().metrics()
            }

            fn enqueue(&self, id: &str) -> HandlerWait {
                let port = self.port();
                let entry_id = port
                    .ledger
                    .lock()
                    .unwrap()
                    .begin_read(id, &port.credits)
                    .unwrap();
                let (oneshot_tx, receiver) = tokio::sync::oneshot::channel();
                let request = RrForwardRequest {
                    wire_bytes: request_bytes(id, self.deadline),
                    oneshot_tx,
                    request_id: id.into(),
                    entry_id,
                    class: Class::Normal,
                    deadline_ns: self.deadline,
                    route: RrRoute::ReadStatus,
                    guard: QueuedGuard::new(&port.ledger, entry_id),
                };
                let ticket = port.push(request).unwrap();
                HandlerWait::new(port, entry_id, ticket, receiver)
            }

            fn snapshot(&self, count: usize) -> QueueFixtureSnapshot {
                let metrics = self.metrics();
                loop {
                    if let Some(result) = metrics
                        .fixture_snapshots
                        .lock()
                        .unwrap()
                        .get(count - 1)
                        .copied()
                    {
                        return result.expect("原发送后夹具纯值快照必须成功");
                    }
                    assert!(
                        SystemClock.now().0 < self.deadline,
                        "原Gateway发送快照超过3s观察窗"
                    );
                    thread::yield_now();
                }
            }

            fn join_server(&mut self) -> Result<ServerProof, String> {
                let _ = self.finish.send(());
                let handle = self.server.as_ref().unwrap();
                while !handle.is_finished() {
                    if SystemClock.now().0 >= self.deadline {
                        std::process::abort();
                    }
                    thread::yield_now();
                }
                self.server
                    .take()
                    .unwrap()
                    .join()
                    .map_err(|_| "原服务器线程panic".to_string())?
            }

            fn join_gateway(&mut self) -> thread::Result<()> {
                let handle = self.gateway.as_ref().unwrap();
                handle.request_stop();
                while !handle.is_finished() {
                    if SystemClock.now().0 >= self.deadline {
                        std::process::abort();
                    }
                    thread::yield_now();
                }
                self.exit = Some(self.gateway.take().unwrap().join()?);
                Ok(())
            }
        }

        impl Drop for FaultOwned {
            fn drop(&mut self) {
                self.cancel.store(true, Ordering::Release);
                let _ = self.connect.send(());
                let _ = self.release.send(());
                let server_joined = self.server.is_none() || self.join_server().is_ok();
                // 原server真join后才停止Gateway；A/client/Node随原worker结束，绝不提前释放。
                let gateway_joined = self.gateway.is_none() || self.join_gateway().is_ok();
                if let Some(exit) = self.exit.take()
                    && !exit.responsibilities_empty()
                {
                    let _held = std::mem::ManuallyDrop::new((exit, self.port.take()));
                }
                if !thread::panicking() {
                    assert!(server_joined && gateway_joined, "两个原线程均须真join成功");
                }
            }
        }

        fn remaining(deadline: u64) -> Duration {
            Duration::from_nanos(deadline.saturating_sub(SystemClock.now().0))
        }

        fn request_bytes(id: &str, deadline: u64) -> Vec<u8> {
            robot_msg::build_helpers::build_get_task_status_request(
                id,
                "",
                "R3真实故障",
                RpcRole::Maintainer,
                "",
                "original-action",
                deadline,
                SystemClock.now().0,
            )
        }

        #[test]
        fn original_full_queue_returns_only_fourth_gateway_credit() {
            exercise_queue(QueueFailure::NotEnqueued);
        }

        #[test]
        fn original_enqueue_issue_stays_sent_until_real_gateway_reply() {
            exercise_queue(QueueFailure::EnqueueIssue);
        }

        #[allow(
            clippy::too_many_lines,
            reason = "同一原队列四次发送与信用退休证据按原拥有域核对"
        )]
        fn exercise_queue(mode: QueueFailure) {
            let mut owned = FaultOwned::new(Mode::Queue(mode));
            let mut first = Vec::new();
            let mut identity = None;
            for (index, id) in ["P1", "P2", "P3"].into_iter().enumerate() {
                first.push(owned.enqueue(id));
                let snapshot = owned.snapshot(index + 1);
                let original = *identity.get_or_insert((snapshot.client_id, snapshot.server_id));
                assert_eq!((snapshot.client_id, snapshot.server_id), original);
                assert_eq!(
                    (
                        snapshot.loans,
                        snapshot.active,
                        snapshot.detached,
                        snapshot.pool_free
                    ),
                    (1, index + 1, 0, 8 - index)
                );
                assert_eq!(
                    (snapshot.channels, snapshot.samples, snapshot.queue_capacity),
                    (10, 10, 4)
                );
                assert!(snapshot.auxiliary_used && !snapshot.target_seen && !snapshot.armed);
                assert_eq!((snapshot.auxiliary_references, snapshot.hits), (2, 0));
            }
            let mut fourth = owned.enqueue("B");
            let snapshot = owned.snapshot(4);
            let metrics = owned.metrics();
            let count = match mode {
                QueueFailure::NotEnqueued => 3,
                QueueFailure::EnqueueIssue => 4,
            };
            assert_eq!((snapshot.client_id, snapshot.server_id), identity.unwrap());
            assert_eq!(
                (
                    snapshot.loans,
                    snapshot.active,
                    snapshot.detached,
                    snapshot.pool_free
                ),
                (1, count, 0, 9 - count)
            );
            assert!(snapshot.target_seen && !snapshot.armed);
            assert_eq!(snapshot.auxiliary_references, 2);
            assert_eq!(snapshot.auxiliary_used, mode == QueueFailure::NotEnqueued);
            assert_eq!(
                snapshot.hits,
                usize::from(mode == QueueFailure::EnqueueIssue)
            );
            assert_eq!(metrics.requests_sent.load(Ordering::Acquire), count as u64);
            assert_eq!(
                metrics.send_errors.load(Ordering::Acquire),
                u64::from(mode == QueueFailure::NotEnqueued)
            );
            assert_eq!(
                metrics.delivery_issues.load(Ordering::Acquire),
                u64::from(mode == QueueFailure::EnqueueIssue)
            );
            assert_eq!(owned.port().credits.used(Class::Normal), count);
            assert!(!owned.port().ledger.lock().unwrap().responsibilities_empty());
            if mode == QueueFailure::NotEnqueued {
                // 发送快照发布先于原迭代末 sender Drop，必须等真实关闭，不能按快照推断。
                loop {
                    match fourth.receiver.try_recv() {
                        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => break,
                        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                        Ok(bytes) => panic!("真实未入队B不应收到回复：{bytes:?}"),
                    }
                    assert!(
                        SystemClock.now().0 < owned.deadline,
                        "原未入队sender未在3s观察窗内关闭"
                    );
                    thread::yield_now();
                }
            }
            owned.release.send(()).unwrap();
            for (waiter, id) in first.iter_mut().zip(["P1", "P2", "P3"]) {
                let bytes = receive(waiter);
                let response = flatbuffers::root::<GetTaskStatusResponse>(&bytes).unwrap();
                assert_eq!(response.request_id(), Some(id));
            }
            if mode == QueueFailure::EnqueueIssue {
                let bytes = receive(&mut fourth);
                let response = flatbuffers::root::<GetTaskStatusResponse>(&bytes).unwrap();
                assert_eq!(response.request_id(), Some("B"));
            }
            let proof = owned.proof.recv_timeout(remaining(owned.deadline)).unwrap();
            assert_eq!(proof.connection_empty_polls, 1);
            assert_eq!(proof.business, count);
            assert_eq!(
                proof.auxiliary,
                usize::from(mode == QueueFailure::NotEnqueued)
            );
            assert_eq!(
                metrics.responses_received.load(Ordering::Acquire),
                count as u64
            );
            assert_eq!(owned.port().credits.used(Class::Normal), 0);
            assert!(owned.port().ledger.lock().unwrap().responsibilities_empty());
            owned.join_server().unwrap();
            owned.join_gateway().unwrap();
            assert!(owned.exit.as_ref().unwrap().responsibilities_empty());
            eprintln!(
                "Gateway原同队列 {mode:?}：B快照={snapshot:?} 原server={proof:?}；两原线程已真join、原信用归零"
            );
        }

        #[test]
        #[allow(
            clippy::too_many_lines,
            reason = "原三代server、两个Pending和真实容量错误必须在同一拥有域逐步核对"
        )]
        fn original_retention_update_error_keeps_both_pending_and_one_orphan() {
            type StatusClient =
                Iceoryx2RrClient<markers::GetTaskStatusRequest, markers::GetTaskStatusResponse>;
            type StatusServer =
                Iceoryx2RrServer<markers::GetTaskStatusRequest, markers::GetTaskStatusResponse>;
            struct RetainedOwned {
                server: Option<StatusServer>,
                in_flight: Vec<InFlight<markers::GetTaskStatusResponse>>,
                port: RrPort,
                client: StatusClient,
                node: iceoryx2::node::Node<IxService>,
            }
            impl Drop for RetainedOwned {
                fn drop(&mut self) {
                    // 原server同线程真Drop后，走生产退出链实际释放Pending并登记NoHandle。
                    self.server.take();
                    for original in self.in_flight.drain(..) {
                        drop_transport(original, &self.port, SystemClock.now().0);
                    }
                    if !self
                        .port
                        .ledger
                        .lock()
                        .is_ok_and(|ledger| ledger.responsibilities_empty())
                    {
                        // 只保未退休业务账本/许可；原client和Node仍在本次真实析构。
                        let _held = std::mem::ManuallyDrop::new(self.port.clone());
                    }
                }
            }
            let mut config = iceoryx2::config::Config::global_config().clone();
            config
                .defaults
                .request_response
                .client_expired_connection_buffer = 1;
            let node = NodeBuilder::new()
                .config(&config)
                .create::<IxService>()
                .unwrap();
            let topic = format!("/r3_gateway_retained_{}", std::process::id());
            let server = rr_server_with::<
                markers::GetTaskStatusRequest,
                markers::GetTaskStatusResponse,
            >(&node, &topic, &RrProfile::S1_TASK)
            .unwrap();
            let client = rr_client_with::<
                markers::GetTaskStatusRequest,
                markers::GetTaskStatusResponse,
            >(&node, &topic, &RrProfile::S1_TASK)
            .unwrap();
            let ledger = GatewayLedger::new(60_000_000_000, 4096).unwrap();
            let port = RrPort::new(
                RrRoute::ReadStatus,
                TopicKind::S1Task,
                &client.limits().unwrap(),
                &ledger,
            )
            .unwrap();
            let mut owned = RetainedOwned {
                server: Some(server),
                in_flight: Vec::new(),
                port,
                client,
                node,
            };
            let deadline = SystemClock.now().0 + 3_000_000_000;
            let mut waiters = Vec::new();
            for id in ["E1", "E2"] {
                let entry_id = ledger
                    .lock()
                    .unwrap()
                    .begin_read(id, &owned.port.credits)
                    .unwrap();
                let (oneshot_tx, receiver) = tokio::sync::oneshot::channel();
                let request = RrForwardRequest {
                    wire_bytes: request_bytes(id, deadline),
                    oneshot_tx,
                    request_id: id.into(),
                    entry_id,
                    class: Class::Normal,
                    deadline_ns: deadline,
                    route: RrRoute::ReadStatus,
                    guard: QueuedGuard::new(&ledger, entry_id),
                };
                let ticket = owned.port.push(request).unwrap();
                waiters.push(HandlerWait::new(&owned.port, entry_id, ticket, receiver));
                let popped = owned.port.queue.pop_eligible(&owned.port.credits).unwrap();
                let request = match owned.port.attach_pop(popped, SystemClock.now().0) {
                    Ok(request) => request,
                    Err(original) => {
                        let _held = std::mem::ManuallyDrop::new((original, owned.port.clone()));
                        panic!("原两代请求必须接入同一责任账本");
                    }
                };
                let pending = owned.client.send_typed(&request.wire_bytes).unwrap();
                owned.in_flight.push(InFlight {
                    pending,
                    oneshot_tx: request.oneshot_tx,
                    entry_id,
                    orphan: false,
                });
                assert!(
                    owned
                        .in_flight
                        .last()
                        .unwrap()
                        .pending
                        .delivery_issue()
                        .is_none()
                );
                assert!(ledger.lock().unwrap().transition(
                    entry_id,
                    EntryState::Sent,
                    SystemClock.now().0
                ));
                let active = wait("原server接纳后无回复正常关闭", || {
                    owned.server.as_ref().unwrap().try_recv().unwrap()
                });
                drop(active);
                owned.server.take();
                // 同topic下一原server，实际连接换代，绝不改profile或改Pending连接资格。
                owned.server =
                    Some(
                        rr_server_with::<
                            markers::GetTaskStatusRequest,
                            markers::GetTaskStatusResponse,
                        >(&owned.node, &topic, &RrProfile::S1_TASK)
                        .unwrap(),
                    );
            }
            for item in &owned.in_flight {
                let error = item
                    .pending
                    .try_recv()
                    .expect_err("两份pin占满原保留容量，更新必须实际报错");
                assert!(
                    matches!(&error, robot_transport::TransportError::Backend(message) if message.contains("RetentionCapacityExceeded")),
                    "真实错误不符：{error}"
                );
            }
            let original: [_; 2] = std::array::from_fn(|index| {
                let item = &owned.in_flight[index];
                assert_eq!(
                    ledger.lock().unwrap().fixture_read_state(item.entry_id),
                    Some(EntryState::Sent)
                );
                (item.entry_id, core::ptr::from_ref(&item.pending))
            });
            drop(waiters.remove(0));
            let metrics = RrIoMetrics::default();
            poll_inflight(&mut owned.in_flight, &owned.port, &metrics, &SystemClock);
            assert_eq!(metrics.poll_errors.load(Ordering::Acquire), 2);
            assert_eq!(metrics.handler_drops.load(Ordering::Acquire), 1);
            assert_eq!(owned.in_flight.len(), 2);
            assert!(owned.in_flight[0].orphan && !owned.in_flight[1].orphan);
            assert_eq!(
                ledger.lock().unwrap().fixture_read_state(original[0].0),
                Some(EntryState::Orphan)
            );
            assert_eq!(
                ledger.lock().unwrap().fixture_read_state(original[1].0),
                Some(EntryState::Sent)
            );
            assert_eq!(owned.port.credits.used(Class::Normal), 2);
            assert!(!ledger.lock().unwrap().responsibilities_empty());
            poll_inflight(&mut owned.in_flight, &owned.port, &metrics, &SystemClock);
            assert_eq!(metrics.poll_errors.load(Ordering::Acquire), 4);
            assert_eq!(metrics.handler_drops.load(Ordering::Acquire), 1);
            assert_eq!(owned.in_flight.len(), 2);
            for (item, (entry_id, pending)) in owned.in_flight.iter().zip(original) {
                assert_eq!(item.entry_id, entry_id);
                assert!(core::ptr::eq(core::ptr::from_ref(&item.pending), pending));
            }
            assert_eq!(owned.port.credits.used(Class::Normal), 2);
            assert_eq!(metrics.responses_received.load(Ordering::Acquire), 0);
            assert_eq!(
                ledger.lock().unwrap().fixture_read_state(original[0].0),
                Some(EntryState::Orphan)
            );
            assert_eq!(
                ledger.lock().unwrap().fixture_read_state(original[1].0),
                Some(EntryState::Sent)
            );
            eprintln!(
                "原保留容量真实耗尽：两次poll各2个RetentionCapacityExceeded；仅关闭一份handler，Orphan=1，原Pending/credit各2未退"
            );
            let credits = Arc::clone(&owned.port.credits);
            drop(owned);
            assert_eq!(
                ledger.lock().unwrap().fixture_read_state(original[0].0),
                Some(EntryState::NoHandle)
            );
            assert_eq!(
                ledger.lock().unwrap().fixture_read_state(original[1].0),
                Some(EntryState::NoHandle)
            );
            assert_eq!(credits.used(Class::Normal), 2);
            eprintln!(
                "原第三server/Pending/client/Node已实际退出；原两个entry为NoHandle且未退休credit仍2"
            );
        }

        #[tokio::test]
        async fn original_unknown_handler_timeout_keeps_orphan_and_credit_until_join() {
            exercise_no_reply(Mode::Unknown).await;
        }

        #[tokio::test]
        async fn original_closed_empty_drops_transport_without_refunding_credit() {
            exercise_no_reply(Mode::Closed).await;
        }

        async fn exercise_no_reply(mode: Mode) {
            let mut owned = FaultOwned::new(mode);
            let metrics = owned.metrics();
            let port = owned.port().clone();
            let exchange = exchange_read(
                &port,
                "original-no-reply",
                owned.deadline,
                request_bytes("original-no-reply", owned.deadline),
                500,
                0,
            );
            let observe = async {
                while metrics.requests_sent.load(Ordering::Acquire) != 1 {
                    assert!(SystemClock.now().0 < owned.deadline, "原请求未Sent");
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                owned.release.send(()).unwrap();
                loop {
                    match owned.proof.try_recv() {
                        Ok(proof) => break proof,
                        Err(mpsc::TryRecvError::Empty) => {}
                        Err(error) => panic!("原server观测断链：{error}"),
                    }
                    assert!(SystemClock.now().0 < owned.deadline, "原server未交清理证据");
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            };
            let (result, proof) = tokio::join!(exchange, observe);
            assert_eq!(proof.connection_empty_polls, 1);
            let error = result.expect_err("无原业务回复不能假退休");
            match mode {
                Mode::Unknown => {
                    assert_eq!(error.code(), tonic::Code::DeadlineExceeded);
                    assert!(proof.cleanup.is_some_and(|record| record.removed));
                    wait("Unknown原handler超时登记Orphan", || {
                        (metrics.handler_drops.load(Ordering::Acquire) == 1).then_some(())
                    });
                    assert!(metrics.unknown_observations.load(Ordering::Acquire) > 0);
                }
                Mode::Closed => {
                    assert_eq!(error.code(), tonic::Code::Unavailable);
                    assert!(proof.cleanup.is_none());
                    assert_eq!(metrics.unknown_observations.load(Ordering::Acquire), 0);
                }
                Mode::Queue(_) => panic!("本例只接受无回复原连接"),
            }
            assert_eq!(metrics.responses_received.load(Ordering::Acquire), 0);
            assert_eq!(port.credits.used(Class::Normal), 1);
            assert!(!port.ledger.lock().unwrap().responsibilities_empty());
            owned.join_server().unwrap();
            owned.join_gateway().unwrap();
            assert_eq!(
                metrics.orphan_drops_on_shutdown.load(Ordering::Acquire),
                u64::from(mode == Mode::Unknown)
            );
            assert_eq!(port.credits.used(Class::Normal), 1);
            assert!(!owned.exit.as_ref().unwrap().responsibilities_empty());
            eprintln!(
                "Gateway原无回复 {mode:?}：server={proof:?} handler={error}；两原线程已真join，未退休原permit仍为1"
            );
        }
    }

    #[test]
    fn rr_io_thread_round_trip_forwards_request_and_replies_to_oneshot() {
        let mut fixture = Fixture::new();
        let mut waiter = fixture.enqueue("one", Class::Restore);
        reply(fixture.active(), 999_999);
        assert_response(&receive(&mut waiter), "one", 999_999);
        assert_eq!(fixture.metrics().requests_sent.load(Ordering::Acquire), 1);
        assert_eq!(
            fixture.metrics().responses_received.load(Ordering::Acquire),
            1
        );
        assert!(fixture.finish().responsibilities_empty());
    }
    #[test]
    fn rr_io_thread_concurrent_in_flight_routes_replies_correctly() {
        let mut fixture = Fixture::new();
        let mut first = fixture.enqueue("A", Class::Restore);
        let mut second = fixture.enqueue("B", Class::Normal);
        let active_a = fixture.active();
        let active_b = fixture.active();
        let id_a = id(&active_a);
        let id_b = id(&active_b);
        assert_ne!(id_a, id_b);
        reply(active_b, 200);
        reply(active_a, 100);
        assert_response(
            &receive(&mut first),
            "A",
            if id_a == "A" { 100 } else { 200 },
        );
        assert_response(
            &receive(&mut second),
            "B",
            if id_b == "B" { 200 } else { 100 },
        );
        assert_eq!(
            fixture.metrics().responses_received.load(Ordering::Acquire),
            2
        );
        assert!(fixture.finish().responsibilities_empty());
    }
    #[test]
    fn rr_io_thread_fifth_request_waits_for_real_credit_without_replacing_first_four() {
        let mut fixture = Fixture::new();
        let mut original = [
            fixture.enqueue("n0", Class::Normal),
            fixture.enqueue("n1", Class::Normal),
            fixture.enqueue("restore", Class::Restore),
            fixture.enqueue("lower", Class::Lower),
        ];
        let mut active = (0..4).map(|_| fixture.active()).collect::<Vec<_>>();
        let ids = active
            .iter()
            .map(id)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), 4);
        let mut fifth = fixture.enqueue("n2", Class::Normal);
        assert_eq!(fixture.port.queue.len(Class::Normal), 1);
        assert_eq!(fixture.port.credits.used(Class::Normal), 2);
        assert!(fixture.server.try_recv().unwrap().is_none());
        assert!(matches!(
            fifth.receiver.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(fixture.metrics().requests_sent.load(Ordering::Acquire), 4);
        assert_eq!(fixture.metrics().send_errors.load(Ordering::Acquire), 0);
        let index = active.iter().position(|item| id(item) == "n0").unwrap();
        reply(active.swap_remove(index), 100);
        assert_response(&receive(&mut original[0]), "n0", 100);
        let fifth_active = fixture.active();
        assert_eq!(id(&fifth_active), "n2");
        reply(fifth_active, 200);
        for request in active {
            reply(request, 100);
        }
        for (waiter, name) in original[1..].iter_mut().zip(["n1", "restore", "lower"]) {
            assert_response(&receive(waiter), name, 100);
        }
        assert_response(&receive(&mut fifth), "n2", 200);
        assert_eq!(fixture.metrics().requests_sent.load(Ordering::Acquire), 5);
        assert_eq!(fixture.metrics().send_errors.load(Ordering::Acquire), 0);
        assert!(fixture.finish().responsibilities_empty());
    }
    #[test]
    fn rr_io_thread_shutdown_drops_oneshot_but_retains_in_flight_credit() {
        let mut fixture = Fixture::new();
        let mut waiter = fixture.enqueue("pending", Class::Restore);
        let active = fixture.active();
        let exit = fixture.finish();
        assert!(matches!(
            waiter.receiver.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
        assert!(!exit.responsibilities_empty());
        assert_eq!(fixture.port.credits.used(Class::Restore), 1);
        assert!(!fixture.port.ledger.lock().unwrap().responsibilities_empty());
        let _held = std::mem::ManuallyDrop::new((exit, fixture.port.clone()));
        drop(active);
    }
    #[test]
    fn rr_io_thread_orphan_keeps_pending_until_real_retired_reply() {
        let mut fixture = Fixture::new();
        let waiter = fixture.enqueue("orphan", Class::Restore);
        let active = fixture.active();
        let metrics = fixture.metrics();
        drop(waiter);
        wait("真实接收者断链观测", || {
            (metrics.handler_drops.load(Ordering::Acquire) == 1).then_some(())
        });
        assert_eq!(fixture.port.credits.used(Class::Restore), 1);
        assert!(!fixture.port.ledger.lock().unwrap().responsibilities_empty());
        reply(active, 300);
        wait("晚到真实退休结果清偿", || {
            fixture
                .port
                .ledger
                .lock()
                .unwrap()
                .responsibilities_empty()
                .then_some(())
        });
        assert_eq!(fixture.port.credits.used(Class::Restore), 0);
        assert_eq!(metrics.handler_drops.load(Ordering::Acquire), 1);
        assert_eq!(metrics.responses_received.load(Ordering::Acquire), 1);
        assert!(fixture.finish().responsibilities_empty());
        assert_eq!(metrics.orphan_drops_on_shutdown.load(Ordering::Acquire), 0);
    }
}
