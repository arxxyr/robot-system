//! EStop 单线程真实发布与唯一缓存提交；退出保留尚未开始的原 Receiver。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::class_cache::{AbandonReason, EntryId, EntryState};
use crate::gateway_ledger::{QueuedGuard, SharedLedger};
use crossbeam_channel::Receiver;
use robot_core::time::RobotClock;
use robot_msg::markers;
use robot_transport::iceoryx2::Iceoryx2Publisher;
use robot_transport::{PublishFailure, PublishReceipt, Publisher, TransportError};
use thiserror::Error;

/// gateway 端 EStop publish 的 envelope `source_id`（PD4：robotctl 直连 =
/// 99；gateway 用独立值，审计区分两条触发路径）。
pub const GATEWAY_ESTOP_SOURCE_ID: u32 = 98;

/// gRPC handler → estop_pub_thread 的转发 payload。
pub struct EStopPubForward {
    /// FlatBuffers `EStopEvent` body bytes（handler 端
    /// [`robot_msg::build_helpers::build_estop_event`] 构造：reason 透传 +
    /// `source="rpc"` + **`latched=false`**〔D3〕）。
    pub body_bytes: Vec<u8>,
    /// envelope `stamp_ns`（handler 端 clock now；informational，S0 不校验）。
    pub stamp_ns: u64,
    /// publish 成功回 `Ok(published_at_ns)`（线程 clock now）；loan/publish
    /// 失败回 `Err`；线程 shutdown 时 drop → handler 收 `RecvError`。
    pub oneshot_tx: tokio::sync::oneshot::Sender<Result<u64, EStopPubError>>,
    /// 仅 tracing / debug 用。
    pub request_id: String,
    pub(crate) entry_id: EntryId,
    pub(crate) guard: QueuedGuard,
}

#[allow(clippy::missing_fields_in_debug)]
impl core::fmt::Debug for EStopPubForward {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EStopPubForward")
            .field("request_id", &self.request_id)
            .field("body_bytes_len", &self.body_bytes.len())
            .finish()
    }
}

/// 线程内 publish 失败分类（handler 全映射 `tonic::Internal`，分类仅 audit）。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EStopPubError {
    #[error("iceoryx2 loan failed: {0}")]
    Loan(String),
    #[error("iceoryx2 publish failed: {0}")]
    Publish(String),
}

/// `estop_pub_thread` 配置。
#[derive(Clone, Debug)]
pub struct EStopPubThreadConfig {
    /// estop topic（仅 doc / 线程名派生；实际 publisher 由 factory 闭包创建）。
    pub topic: String,
    /// 主循环 idle sleep（纳秒）。默认 500µs。
    pub poll_interval_ns: u64,
}

impl Default for EStopPubThreadConfig {
    fn default() -> Self {
        Self {
            topic: String::new(),
            poll_interval_ns: 500_000,
        }
    }
}

/// `estop_pub_thread` 运行期 metrics。
#[derive(Default, Debug)]
pub struct EStopPubMetrics {
    /// publish 成功 + oneshot 回 Ok 的次数。
    pub published: AtomicU64,
    /// loan / publish 失败（oneshot 回 Err → handler Internal）的次数。
    pub publish_errors: AtomicU64,
    /// shutdown 时 channel 残留 forward 丢弃数（oneshot drop → handler
    /// Unavailable）。
    pub orphan_drops_on_shutdown: AtomicU64,
    /// v0.5 候选 E 段(8)：主循环每次 poll 迭代 +1（空闲亦推进）——watchdog
    /// liveness 源。
    pub loop_tick_count: AtomicU64,
    /// test-only：loop 顶端 freeze 截止（absolute ns，0 = 未冻结）。
    #[cfg(any(test, feature = "test-freeze-hook"))]
    pub freeze_until_ns: AtomicU64,
}

/// `estop_pub_thread` 句柄。
pub struct EStopPubThreadHandle {
    join: JoinHandle<EStopExit>,
    stop: Arc<AtomicBool>,
    metrics: Arc<EStopPubMetrics>,
}

impl EStopPubThreadHandle {
    /// 取 metrics 引用观察运行期统计。
    #[must_use]
    pub fn metrics(&self) -> Arc<EStopPubMetrics> {
        Arc::clone(&self.metrics)
    }

    /// 通知 main loop 退出。
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
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
    pub fn join(self) -> std::thread::Result<EStopExit> {
        self.join.join()
    }
}

pub(crate) struct OwnedEStopPublisher {
    pub(crate) publisher: Iceoryx2Publisher<markers::EStopEvent>,
    pub(crate) _node: iceoryx2::node::Node<iceoryx2::service::ipc::Service>,
}

pub struct EStopExit {
    pub(crate) queued: Receiver<EStopPubForward>,
    pub(crate) stranded: Option<EStopPubForward>,
    pub(crate) ledger: SharedLedger,
}
impl EStopExit {
    pub(crate) fn responsibilities_empty(&self) -> bool {
        self.queued.is_empty()
            && self.stranded.is_none()
            && self
                .ledger
                .lock()
                .expect("网关责任锁未中毒")
                .responsibilities_empty()
    }
}
impl std::fmt::Debug for EStopExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EStopExit")
            .field("queued", &self.queued.len())
            .field("stranded", &self.stranded.is_some())
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for EStopPubThreadHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EStopPubThreadHandle")
            .field("finished", &self.join.is_finished())
            .finish_non_exhaustive()
    }
}
#[derive(Debug, Error)]
pub(crate) enum EStopSpawnError {
    #[error("EStop publisher 线程创建失败: {0}")]
    ThreadSpawn(#[source] std::io::Error),
    #[error("EStop publisher ready 失败: {error}")]
    ReadyFailed {
        handle: EStopPubThreadHandle,
        error: String,
    },
}

pub(crate) fn spawn_estop_pub_thread<F>(
    cfg: &EStopPubThreadConfig,
    clock: Arc<dyn RobotClock>,
    publisher_factory: F,
    forward_rx: Receiver<EStopPubForward>,
    ledger: SharedLedger,
) -> Result<EStopPubThreadHandle, EStopSpawnError>
where
    F: FnOnce() -> Result<OwnedEStopPublisher, String> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let metrics = Arc::new(EStopPubMetrics::default());
    let thread_stop = Arc::clone(&stop);
    let thread_metrics = Arc::clone(&metrics);
    let cfg = cfg.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
    let join = thread::Builder::new()
        .name("gw_estop_pub".into())
        .spawn(move || {
            let mut owned = match publisher_factory() {
                Ok(owned) => owned,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return EStopExit {
                        queued: forward_rx,
                        stranded: None,
                        ledger,
                    };
                }
            };
            if ready_tx.send(Ok(())).is_err() {
                return EStopExit {
                    queued: forward_rx,
                    stranded: None,
                    ledger,
                };
            }
            drop(ready_tx);
            run_loop(
                &cfg,
                clock.as_ref(),
                &mut owned.publisher,
                forward_rx,
                &thread_stop,
                &thread_metrics,
                ledger,
            )
        })
        .map_err(EStopSpawnError::ThreadSpawn)?;
    let handle = EStopPubThreadHandle {
        join,
        stop,
        metrics,
    };
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(handle),
        Ok(Err(error)) => Err(EStopSpawnError::ReadyFailed { handle, error }),
        Err(error) => Err(EStopSpawnError::ReadyFailed {
            handle,
            error: format!("factory 在 ready 前断链: {error}"),
        }),
    }
}

fn run_loop(
    cfg: &EStopPubThreadConfig,
    clock: &dyn RobotClock,
    publisher: &mut Iceoryx2Publisher<markers::EStopEvent>,
    forward_rx: Receiver<EStopPubForward>,
    stop: &AtomicBool,
    metrics: &EStopPubMetrics,
    ledger: SharedLedger,
) -> EStopExit {
    let mut seq = 0;
    let mut stranded = None;
    'running: while !stop.load(Ordering::Acquire) {
        metrics.loop_tick_count.fetch_add(1, Ordering::Relaxed);
        #[cfg(any(test, feature = "test-freeze-hook"))]
        crate::liveness::freeze_check(&metrics.freeze_until_ns, clock, stop);
        // 空闲也建立真实新连接，补送原 history；失败不撤销既有发布，不额外发 seq。
        if let Err(error) = publisher.update_connections() {
            tracing::warn!(?error, "EStop publisher 刷新连接失败");
        }
        for _ in 0..64 {
            if stop.load(Ordering::Acquire) {
                break 'running;
            }
            let Ok(mut forward) = forward_rx.try_recv() else {
                break;
            };
            if forward.oneshot_tx.is_closed() {
                drop(forward);
                continue;
            }
            let attached = forward.guard.belongs_to(&ledger, forward.entry_id)
                && ledger.lock().expect("网关责任锁未中毒").transition(
                    forward.entry_id,
                    EntryState::Publishing,
                    clock.now().0,
                );
            if !attached {
                stranded = Some(forward);
                break 'running;
            }
            forward.guard.disarm();
            let result = publish_one(publisher, clock, &mut seq, &forward);
            // 先记录真实结果与清偿原责任，再向 handler 通知。日志排在实际处理之后。
            let reported = match result {
                Ok((published_at_ns, recipients)) => {
                    if ledger.lock().expect("网关责任锁未中毒").finish_estop(
                        forward.entry_id,
                        published_at_ns,
                        recipients,
                    ) {
                        metrics.published.fetch_add(1, Ordering::Relaxed);
                        Ok(published_at_ns)
                    } else {
                        Err(EStopPubError::Publish(
                            "发布成功但缓存提交拒绝，保留原责任".into(),
                        ))
                    }
                }
                Err(PublishFailure::BeforeHistory(error)) => {
                    ledger
                        .lock()
                        .expect("网关责任锁未中毒")
                        .abandon_before_delivery(
                            forward.entry_id,
                            AbandonReason::PublishFailedPreHistory,
                            clock.now().0,
                        );
                    Err(EStopPubError::Publish(format!("{error:?}")))
                }
                Err(PublishFailure::Uncertain(error)) => {
                    ledger.lock().expect("网关责任锁未中毒").transition(
                        forward.entry_id,
                        EntryState::NoHandle,
                        clock.now().0,
                    );
                    Err(EStopPubError::Publish(format!("{error:?}")))
                }
            };
            if reported.is_err() {
                metrics.publish_errors.fetch_add(1, Ordering::Relaxed);
            }
            let _ = forward.oneshot_tx.send(reported);
        }
        thread::sleep(Duration::from_nanos(cfg.poll_interval_ns));
    }
    // 保留原 Receiver 与 queued guard，不能为了退出而 drain/drop 冒充未开始退款。
    EStopExit {
        queued: forward_rx,
        stranded,
        ledger,
    }
}

fn publish_one(
    publisher: &mut Iceoryx2Publisher<markers::EStopEvent>,
    clock: &dyn RobotClock,
    seq: &mut u64,
    forward: &EStopPubForward,
) -> Result<(u64, usize), PublishFailure> {
    let mut writer = publisher
        .loan(forward.body_bytes.len() + 1024)
        .map_err(PublishFailure::BeforeHistory)?;
    writer.set_seq(*seq);
    writer.set_source_id(GATEWAY_ESTOP_SOURCE_ID);
    writer.set_stamp_ns(forward.stamp_ns);
    writer.set_body_bytes(&forward.body_bytes);
    match writer.publish_typed()? {
        PublishReceipt::Iceoryx2 { recipients } => {
            *seq = seq.wrapping_add(1);
            Ok((clock.now().0, recipients))
        }
        PublishReceipt::MockStored => Err(PublishFailure::Uncertain(TransportError::Backend(
            "EStop publisher 后端绑定错误".into(),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::AtomicU32;

    use crossbeam_channel::bounded;
    use iceoryx2::prelude::NodeBuilder;
    use iceoryx2::service::ipc::Service as IxService;
    use robot_core::time::SystemClock;
    use robot_msg::build_helpers::build_estop_event;
    use robot_msg::msg::EStopEvent as FbEStop;
    use robot_transport::iceoryx2::{TopicClass, publisher as open_pub, subscriber as open_sub};

    fn fresh_topic(suffix: &str) -> String {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        format!("/estop_pub_test_{}_{}_{suffix}", std::process::id(), seq)
    }

    fn bring_up(
        topic: &str,
    ) -> (
        EStopPubThreadHandle,
        crossbeam_channel::Sender<EStopPubForward>,
        SharedLedger,
    ) {
        let (forward_tx, forward_rx) = bounded::<EStopPubForward>(64);
        let cfg = EStopPubThreadConfig {
            topic: topic.to_string(),
            poll_interval_ns: 500_000,
        };
        let topic_for_factory = topic.to_string();
        let factory = move || {
            let node = NodeBuilder::new().create::<IxService>().unwrap();
            let publisher =
                open_pub::<markers::EStopEvent>(&node, &topic_for_factory, TopicClass::SafetyEvent)
                    .expect("open estop publisher");
            Ok(OwnedEStopPublisher {
                publisher,
                _node: node,
            })
        };
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let ledger = crate::gateway_ledger::GatewayLedger::new(60_000_000_000, 4096).unwrap();
        let handle = spawn_estop_pub_thread(&cfg, clock, factory, forward_rx, Arc::clone(&ledger))
            .expect("spawn estop_pub");
        (handle, forward_tx, ledger)
    }

    /// round trip：forward 一条 body → 真 iceoryx2 订阅者收到（envelope +
    /// body 字段全验）→ oneshot 回 Ok(published_at_ns > 0)。
    #[test]
    fn estop_pub_round_trip_publishes_and_acks() {
        let topic = fresh_topic("rt");

        // 订阅者先建（同进程；attach 早于 publish，沿 PD1 纪律）。
        let sub_node = NodeBuilder::new().create::<IxService>().unwrap();
        let mut sub = open_sub::<markers::EStopEvent>(&sub_node, &topic, TopicClass::SafetyEvent)
            .expect("open estop subscriber");

        let (handle, forward_tx, ledger) = bring_up(&topic);

        let body = build_estop_event("unit-reason", 777, "rpc", false);
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let crate::gateway_ledger::BeginWrite::Fresh(entry_id) =
            ledger.lock().unwrap().begin_write(
                crate::idempotency::Method::EStop,
                "req-estop-1",
                [1; 32],
                crate::class_cache::Class::Emergency,
                "req-estop-1",
                777,
            )
        else {
            panic!("真实准入");
        };
        forward_tx
            .send(EStopPubForward {
                body_bytes: body,
                stamp_ns: 777,
                oneshot_tx,
                request_id: "req-estop-1".into(),
                entry_id,
                guard: crate::gateway_ledger::QueuedGuard::new(&ledger, entry_id),
            })
            .unwrap();

        let published_at = oneshot_rx
            .blocking_recv()
            .expect("oneshot not dropped")
            .expect("publish ok");
        assert!(published_at > 0, "published_at_ns 应非零");

        // 订阅者收到 + 字段验证（逐条 try_recv，禁 latest()——PD1 同款纪律 +
        // PE1 additive API 消费点）。
        let start = std::time::Instant::now();
        let lease = loop {
            if let Ok(Some(l)) = sub.try_recv() {
                break l;
            }
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "subscriber did not receive EStopEvent within 2s"
            );
            thread::sleep(Duration::from_millis(2));
        };
        let msg = lease.view().expect("decode EStopEvent");
        assert_eq!(msg.header.source_id(), GATEWAY_ESTOP_SOURCE_ID);
        assert_eq!(msg.header.stamp_ns(), 777);
        let view: FbEStop = msg.body.get();
        assert_eq!(view.reason().unwrap_or(""), "unit-reason");
        assert_eq!(view.source().unwrap_or(""), "rpc");
        assert!(!view.latched(), "RPC 路径 latched 必须 false（D3）");

        let metrics = handle.metrics();
        assert_eq!(metrics.published.load(Ordering::Acquire), 1);
        assert_eq!(metrics.publish_errors.load(Ordering::Acquire), 0);

        handle.request_stop();
        assert!(handle.join().unwrap().responsibilities_empty());
    }

    /// spawn + 立即 stop：ready 握手 + 干净退出（不 panic / 不挂）。
    #[test]
    fn estop_pub_spawn_then_stop_cleanly() {
        let topic = fresh_topic("stop");
        let (handle, _forward_tx, _ledger) = bring_up(&topic);
        handle.request_stop();
        assert!(handle.join().unwrap().responsibilities_empty());
    }
    #[test]
    fn idle_estop_worker_refreshes_late_subscriber_without_second_publish() {
        let topic = fresh_topic("idle_history");
        let (handle, forward, ledger) = bring_up(&topic);
        let crate::gateway_ledger::BeginWrite::Fresh(entry_id) =
            ledger.lock().unwrap().begin_write(
                crate::idempotency::Method::EStop,
                "late-history",
                [7; 32],
                crate::class_cache::Class::Emergency,
                "late-history",
                SystemClock.now().0,
            )
        else {
            panic!("真实准入");
        };
        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        forward
            .send(EStopPubForward {
                body_bytes: build_estop_event("late", 91, "rpc", false),
                stamp_ns: 91,
                oneshot_tx: sender,
                request_id: "late-history".into(),
                entry_id,
                guard: crate::gateway_ledger::QueuedGuard::new(&ledger, entry_id),
            })
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let at = loop {
            match receiver.try_recv() {
                Ok(result) => break result.unwrap(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                Err(error) => panic!("真实 publish 断链: {error}"),
            }
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        };
        let cached = ledger.lock().unwrap().begin_write(
            crate::idempotency::Method::EStop,
            "late-history",
            [7; 32],
            crate::class_cache::Class::Emergency,
            "replay",
            at,
        );
        assert!(
            matches!(cached, crate::gateway_ledger::BeginWrite::Replay(crate::class_cache::CachedTerminal { outcome: crate::class_cache::TypedOutcome::EStop { recipients: 0, published_at_ns }, .. }) if published_at_ns == at)
        );
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let mut subscriber =
            open_sub::<markers::EStopEvent>(&node, &topic, TopicClass::SafetyEvent).unwrap();
        let sample = loop {
            if let Some(sample) = subscriber.try_recv().unwrap() {
                break sample;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "生产 idle update_connections 必须补送 history"
            );
            thread::sleep(Duration::from_millis(1));
        };
        let view = sample.view().unwrap();
        assert_eq!(view.header.seq(), 0);
        assert_eq!(view.header.stamp_ns(), 91);
        let body: FbEStop = view.body.get();
        assert_eq!(body.reason(), Some("late"));
        assert_eq!(handle.metrics().published.load(Ordering::Acquire), 1);
        handle.request_stop();
        assert!(handle.join().unwrap().responsibilities_empty());
    }

    /// 持续真实生产/消费后请求停止；生产者直到真实 join 之后才收到自己的退出信号。
    /// 原满队列及其 guards 保留到受监督子进程退出，不以 Drop 冒充清偿。
    #[cfg(unix)]
    #[test]
    fn continuous_estop_producer_stops_with_original_full_queue_owned_until_process_exit() {
        const CHILD: &str = "ROBOT_W3_ESTOP_FLOW_CHILD";
        const NAME: &str = "estop_pub_thread::tests::continuous_estop_producer_stops_with_original_full_queue_owned_until_process_exit";
        if std::env::var_os(CHILD).is_some() {
            run_continuous_estop_child();
            return;
        }
        let stdout = tempfile::tempfile().unwrap();
        let stderr = tempfile::tempfile().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap())
            .spawn()
            .unwrap();
        // 沿 gateway owned join 测试既有 15 秒进程边界，不改变任何生产停止预算。
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut expired = false;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                expired = true;
                child.kill().unwrap();
                break child.wait().unwrap();
            }
            thread::sleep(Duration::from_millis(5));
        };
        let read_all = |mut file: std::fs::File| {
            use std::io::{Read, Seek, SeekFrom};
            file.seek(SeekFrom::Start(0)).unwrap();
            let mut text = String::new();
            file.read_to_string(&mut text).unwrap();
            text
        };
        let output = read_all(stdout);
        let errors = read_all(stderr);
        assert!(
            !expired && status.success(),
            "真实子进程 {status}: {output} {errors}"
        );
        let markers: Vec<_> = output
            .lines()
            .filter(|line| line.starts_with("estop_continuous_owned_verified "))
            .collect();
        assert_eq!(markers.len(), 1, "{output}");
        println!("{} child_reaped=true", markers[0]);
    }

    #[cfg(unix)]
    #[allow(
        clippy::too_many_lines,
        reason = "连续生产、实际发布、停止和原队内责任回收必须在同一有界子进程验证"
    )]
    fn run_continuous_estop_child() {
        type ReplyReceiver = tokio::sync::oneshot::Receiver<Result<u64, EStopPubError>>;
        #[derive(Default)]
        struct FlowCounts {
            accepted: AtomicU64,
            after_stop: AtomicU64,
        }
        struct OwnedFlow {
            worker: Option<EStopPubThreadHandle>,
            joined: Option<std::thread::Result<EStopExit>>,
            producer: Option<JoinHandle<Vec<(String, ReplyReceiver)>>>,
            producer_joined: Option<std::thread::Result<Vec<(String, ReplyReceiver)>>>,
            forward: crossbeam_channel::Sender<EStopPubForward>,
            ledger: SharedLedger,
        }
        let topic = fresh_topic("continuous");
        let (worker, forward, ledger) = bring_up(&topic);
        let metrics = worker.metrics();
        let worker_stop = Arc::clone(&worker.stop);
        let producer_running = Arc::new(AtomicBool::new(true));
        let counts = Arc::new(FlowCounts::default());
        let producer_switch = Arc::clone(&producer_running);
        let producer_counts = Arc::clone(&counts);
        let producer_ledger = Arc::clone(&ledger);
        let producer_forward = forward.clone();
        // 若断言失败，整个原聚合仍留到真实子进程退出，父进程承担唯一看门边界。
        let mut owned = std::mem::ManuallyDrop::new(OwnedFlow {
            worker: Some(worker),
            joined: None,
            producer: None,
            producer_joined: None,
            forward,
            ledger,
        });
        owned.producer = Some(thread::spawn(move || {
            let mut receivers = Vec::new();
            let mut sequence = 0_u64;
            while producer_switch.load(Ordering::Acquire) {
                let key = format!("continuous-{sequence}");
                sequence += 1;
                let now = SystemClock.now().0;
                let decision = producer_ledger.lock().unwrap().begin_write(
                    crate::idempotency::Method::EStop,
                    &key,
                    [4; 32],
                    crate::class_cache::Class::Emergency,
                    &key,
                    now,
                );
                let crate::gateway_ledger::BeginWrite::Fresh(entry_id) = decision else {
                    thread::yield_now();
                    continue;
                };
                let (sender, receiver) = tokio::sync::oneshot::channel();
                let item = EStopPubForward {
                    body_bytes: build_estop_event("continuous", now, "rpc", false),
                    stamp_ns: now,
                    oneshot_tx: sender,
                    request_id: key.clone(),
                    entry_id,
                    guard: crate::gateway_ledger::QueuedGuard::new(&producer_ledger, entry_id),
                };
                let after_stop = worker_stop.load(Ordering::Acquire);
                let result = producer_forward.try_send(item);
                if after_stop {
                    producer_counts.after_stop.fetch_add(1, Ordering::Release);
                }
                match result {
                    Ok(()) => {
                        receivers.push((key, receiver));
                        producer_counts.accepted.fetch_add(1, Ordering::Release);
                    }
                    Err(crossbeam_channel::TrySendError::Full(original)) => drop(original),
                    Err(crossbeam_channel::TrySendError::Disconnected(original)) => {
                        drop(original);
                        break;
                    }
                }
                thread::yield_now();
            }
            receivers
        }));
        // 先真实超过两个整队容量，证明 producer 和真实发布消费曾并存，非只预填。
        while metrics.published.load(Ordering::Acquire) < 128 {
            assert!(
                !owned.worker.as_ref().unwrap().is_finished(),
                "持续消费阶段 worker 提前结束"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert!(counts.accepted.load(Ordering::Acquire) >= 128);
        metrics
            .freeze_until_ns
            .store(SystemClock.now().0 + 10_000_000_000, Ordering::Release);
        while owned.forward.len() < 64 {
            thread::yield_now();
        }
        owned.worker.as_ref().unwrap().request_stop();
        // producer 看见同一真实 stop 位后仍尝试原队列；直到真 join 才停 producer。
        while counts.after_stop.load(Ordering::Acquire) == 0 {
            thread::yield_now();
        }
        owned.joined = Some(owned.worker.take().unwrap().join());
        producer_running.store(false, Ordering::Release);
        owned.producer_joined = Some(owned.producer.take().unwrap().join());
        let published = metrics.published.load(Ordering::Acquire);
        assert!(published >= 128);
        assert_eq!(metrics.publish_errors.load(Ordering::Acquire), 0);
        let mut pending = 0;
        let mut replied = 0;
        let check_ledger = Arc::clone(&owned.ledger);
        for (key, receiver) in owned.producer_joined.as_mut().unwrap().as_mut().unwrap() {
            match receiver.try_recv() {
                Ok(Ok(at)) => {
                    assert!(at > 0);
                    replied += 1;
                }
                Ok(Err(error)) => panic!("真实发布失败: {error}"),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    pending += 1;
                    assert!(
                        check_ledger
                            .lock()
                            .unwrap()
                            .cached_terminal(crate::idempotency::Method::EStop, key)
                            .is_none()
                    );
                }
                Err(error) => panic!("queued 原发送端被提前丢弃: {error}"),
            }
        }
        let exit = owned.joined.as_ref().unwrap().as_ref().unwrap();
        assert!(Arc::ptr_eq(&exit.ledger, &owned.ledger));
        // stop 前已进入的一个 Publishing 可以完成；其余原 queued 不得析构。
        assert!((1..=64).contains(&exit.queued.len()));
        assert_eq!(pending, exit.queued.len());
        assert_eq!(replied, published);
        assert!(!exit.responsibilities_empty());
        assert!(!owned.ledger.lock().unwrap().responsibilities_empty());
        assert_eq!(metrics.orphan_drops_on_shutdown.load(Ordering::Acquire), 0);
        // 单线程 libtest 先输出未换行的测试名；证据标记必须独占新行。
        println!(
            "\nestop_continuous_owned_verified consumed={published} attempts_after_stop={} queued_before_stop=64 queued_at_join={pending} worker_joined=true producer_joined=true",
            counts.after_stop.load(Ordering::Acquire)
        );
        // 不从 exit.queued 取出/析构原载荷，也不清账本；原聚合由进程退出收尾。
    }
}
