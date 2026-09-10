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
    Iceoryx2PendingResponse, Iceoryx2RrClient, RrSendError,
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
    pub poll_errors: AtomicU64,
    /// 接收者关闭只登记一次 Orphan，不丢传输对象、不归还信用。
    pub handler_drops: AtomicU64,
    /// 正常关闭时实际释放的传输对象数；不是已退休或已退款计数。
    pub orphan_drops_on_shutdown: AtomicU64,
    pub loop_tick_count: AtomicU64,
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
        // 已发送对象不受 handler 的等待寿命控制；空读与闭连查询之间仍可能收到回复。
        let mut received = in_flight[index].pending.try_recv();
        let mut closed_empty = false;
        if matches!(received, Ok(None)) {
            #[cfg(test)]
            after_empty(in_flight[index].entry_id);
            if !in_flight[index].pending.is_connected() {
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
                    index += 1;
                }
            }
            Err(error) => {
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
                        metrics.requests_sent.fetch_add(1, Ordering::Relaxed);
                    } else {
                        port.stop_admission();
                        drop_transport(item, &port, clock.now().0);
                    }
                }
                Err(error) => {
                    metrics.send_errors.fetch_add(1, Ordering::Relaxed);
                    match error {
                        RrSendError::NotDelivered(_) => {
                            if !port
                                .ledger
                                .lock()
                                .expect("网关责任锁未中毒")
                                .abandon_before_delivery(
                                    id,
                                    AbandonReason::NotDelivered,
                                    clock.now().0,
                                )
                            {
                                port.stop_admission();
                            }
                        }
                        RrSendError::Uncertain(_) => {
                            port.ledger.lock().expect("网关责任锁未中毒").transition(
                                id,
                                EntryState::NoHandle,
                                clock.now().0,
                            );
                        }
                    }
                }
            }
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

        struct ServerWorker {
            handle: Option<JoinHandle<()>>,
            deadline: u64,
        }
        impl Drop for ServerWorker {
            fn drop(&mut self) {
                if let Some(handle) = self.handle.take() {
                    while !handle.is_finished() {
                        if SystemClock.now().0 >= self.deadline {
                            // 同一原测试截止内无法回收时终止测试进程，不能 detach 活线程。
                            std::process::abort();
                        }
                        thread::yield_now();
                    }
                    assert!(handle.join().is_ok(), "真实服务器线程必须成功 join");
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
        let handle = thread::spawn(move || {
            let node = NodeBuilder::new().create::<IxService>().unwrap();
            let server = rr_server_with::<
                markers::GetTaskStatusRequest,
                markers::GetTaskStatusResponse,
            >(&node, &server_topic, &RrProfile::S1_TASK)
            .unwrap();
            ready_tx.send(()).unwrap();
            let active = loop {
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
        let mut worker = ServerWorker {
            handle: Some(handle),
            deadline,
        };
        ready_rx
            .recv_timeout(Duration::from_nanos(
                deadline.saturating_sub(SystemClock.now().0),
            ))
            .unwrap();
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let client =
            rr_client_with::<markers::GetTaskStatusRequest, markers::GetTaskStatusResponse>(
                &node,
                &topic,
                &RrProfile::S1_TASK,
            )
            .unwrap();
        let ledger = GatewayLedger::new(60_000_000_000, 4096).unwrap();
        let port = RrPort::new(
            RrRoute::ReadStatus,
            TopicKind::S1Task,
            &client.limits().expect("显式 S1_TASK 工厂返回真实容量"),
            &ledger,
        )
        .unwrap();
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
        assert_eq!(pending.server_connections(), 1);
        assert!(
            ledger
                .lock()
                .unwrap()
                .transition(entry_id, EntryState::Sent, SystemClock.now().0)
        );
        let mut in_flight = vec![InFlight {
            pending,
            oneshot_tx: request.oneshot_tx,
            entry_id,
            orphan: false,
        }];
        let metrics = RrIoMetrics::default();
        let mut hook_calls = 0;
        poll_inflight_impl(&mut in_flight, &port, &metrics, &SystemClock, &mut |id| {
            assert_eq!(id, entry_id);
            hook_calls += 1;
            assert_eq!(hook_calls, 1);
            release_tx.send(()).unwrap();
            sent_rx
                .recv_timeout(Duration::from_nanos(
                    deadline.saturating_sub(SystemClock.now().0),
                ))
                .unwrap();
        });
        poll_inflight(&mut in_flight, &port, &metrics, &SystemClock);
        let response = waiter.receiver.try_recv();
        finish_tx.send(()).unwrap();
        while !worker.handle.as_ref().unwrap().is_finished() {
            assert!(
                SystemClock.now().0 < deadline,
                "真实服务器必须在原截止前结束"
            );
            thread::yield_now();
        }
        assert!(worker.handle.take().unwrap().join().is_ok());
        let empty = ledger.lock().unwrap().responsibilities_empty();
        let used = port.credits.used(Class::Normal);
        if !empty {
            // RED 保原 NoHandle/permit 到测试进程退出，不能因断言失败伪退款。
            let _held = std::mem::ManuallyDrop::new((in_flight, port.clone()));
        }
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
