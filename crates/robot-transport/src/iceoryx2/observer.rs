//! 独立观测镜像：主发布成功后仅把 owned wire 放入有界队列。
//!
//! 镜像节点、连接、发送全部由普通线程拥有；不借用主 sample，不改变主服务
//! QoS、发送结果或 recipients。镜像失败只影响观测计数。Drop 请求停止并真实
//! join；该收尾不进入 publish 路径。50 ms 是 SDK 创建预算，不是任意内核故障
//! 下的绝对线程退出保证。
//! publisher 构造与 publish 仅用于非 RT 线程（队列含 Arc/mpsc 操作）。
//! owned wire 全进程预算 32 MiB；镜像 SHM 单独按固定八个样本估算预约，
//! 活跃 worker 的预约总量最多 512 MiB。该估算含头/管理余量，不是 OS 映射
//! 字节的精确计量；接收者仍持有的旧 loan 可以延后底层映射释放。

use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::Duration;

use iceoryx2::config::Config;
use iceoryx2::port::update_connections::UpdateConnections;
use iceoryx2::prelude::SemanticString;
use iceoryx2::prelude::{
    AllocationStrategy, AttributeVerifier, BackpressureStrategy, CallbackProgression, Node,
    NodeBuilder,
};
use iceoryx2::service::Service as _;
use iceoryx2::service::attribute::AttributeSet;
use iceoryx2::service::ipc::Service as IxService;
use robot_msg::markers::BodyMarker;

use super::publisher::IxPubSubPublisher;
use super::subscriber::{Iceoryx2SampleLease, IxPubSubSubscriber};
use super::{schema_attribute_verifier, topic_to_service_name};
use crate::{MessageView, SampleLease, TransportError};

/// 与正式命名空间隔离；后缀原样保留主 topic。
pub const OBSERVER_PREFIX: &str = "/_robot_observe/v1";
const ATTR_OBSERVER: &str = "robot_observer_version";
const ATTR_ORIGIN: &str = "origin_topic";
const MAGIC: &[u8; 8] = b"RBTOBS01";
const PREFIX_SIZE: usize = 48;
const QUEUE_CAPACITY: usize = 8;
const MAX_WORKERS: usize = 32;
const GLOBAL_BYTE_LIMIT: usize = 32 * 1024 * 1024;
const ESTIMATED_SHM_LIMIT: usize = 512 * 1024 * 1024;
const PREALLOCATED_SAMPLES: usize = 8;
const POLL: Duration = Duration::from_millis(25);
const CREATION_TIMEOUT: Duration = Duration::from_millis(50);
static ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);
static QUEUED_BYTES: AtomicUsize = AtomicUsize::new(0);
static ESTIMATED_SHM_BYTES: AtomicUsize = AtomicUsize::new(0);

/// 镜像自己的序号，不能拿主协议可能重复的 seq（例如 EStop）替代。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObserverMetadata {
    pub publisher_instance_id: [u8; 16],
    pub observer_seq: u64,
    pub dropped_frames: u64,
}

/// 枚举只返回具有完整镜像属性的既有 pubsub 服务。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObserverTopic {
    pub primary_topic: String,
    pub observer_topic: String,
    pub schema_id: u32,
    pub schema_version: u16,
}

/// 有界枚举结果；容量触顶显式返回，不能被误解为完整服务目录。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObserverDiscovery {
    pub topics: Vec<ObserverTopic>,
    pub capacity_reached: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ObserverStats {
    pub ready: bool,
    pub finished: bool,
    pub reaped: bool,
    pub pending_frames: usize,
    pub pending_bytes: usize,
    /// 活跃 worker 的 SHM 估算预约，不含接收者延迟释放的历史映射。
    pub estimated_shm_bytes: usize,
    pub published: u64,
    pub dropped_frames: u64,
    pub initialization_failures: u64,
    pub send_failures: u64,
    pub connection_failures: u64,
    pub join_failures: u64,
}

#[derive(Default)]
struct Counters {
    ready: AtomicBool,
    finished: AtomicBool,
    reaped: AtomicBool,
    stop: AtomicBool,
    pending_frames: AtomicUsize,
    pending_bytes: AtomicUsize,
    estimated_shm_bytes: AtomicUsize,
    published: AtomicU64,
    dropped_frames: AtomicU64,
    initialization_failures: AtomicU64,
    send_failures: AtomicU64,
    connection_failures: AtomicU64,
    join_failures: AtomicU64,
}

/// 可在主 publisher 销毁后保留，核对 worker 是否真实回收。
#[derive(Clone)]
pub struct ObserverMonitor(Arc<Counters>);

impl ObserverMonitor {
    #[must_use]
    pub fn snapshot(&self) -> ObserverStats {
        let c = &self.0;
        ObserverStats {
            ready: c.ready.load(Ordering::Acquire),
            finished: c.finished.load(Ordering::Acquire),
            reaped: c.reaped.load(Ordering::Acquire),
            pending_frames: c.pending_frames.load(Ordering::Acquire),
            pending_bytes: c.pending_bytes.load(Ordering::Acquire),
            estimated_shm_bytes: c.estimated_shm_bytes.load(Ordering::Acquire),
            published: c.published.load(Ordering::Relaxed),
            dropped_frames: c.dropped_frames.load(Ordering::Relaxed),
            initialization_failures: c.initialization_failures.load(Ordering::Relaxed),
            send_failures: c.send_failures.load(Ordering::Relaxed),
            connection_failures: c.connection_failures.load(Ordering::Relaxed),
            join_failures: c.join_failures.load(Ordering::Relaxed),
        }
    }
}

fn backend(message: impl Into<String>) -> TransportError {
    TransportError::Backend(message.into())
}

/// 只计算名称，不创建任何服务。拒绝镜像再镜像。
///
/// # Errors
/// 名称非法、过长或已在镜像命名空间时返回错误。
pub fn observer_topic(primary_topic: &str) -> Result<String, TransportError> {
    if !primary_topic.starts_with('/') || primary_topic.starts_with(OBSERVER_PREFIX) {
        return Err(backend("observer primary topic 必须以 / 开头且不能是镜像"));
    }
    topic_to_service_name(primary_topic)?;
    let result = format!("{OBSERVER_PREFIX}{primary_topic}");
    topic_to_service_name(&result)?;
    Ok(result)
}

fn verifier(schema_id: u32, origin: &str) -> Result<AttributeVerifier, TransportError> {
    let key = ATTR_OBSERVER
        .try_into()
        .map_err(|e| backend(format!("observer attribute: {e:?}")))?;
    let value = "1"
        .try_into()
        .map_err(|e| backend(format!("observer version: {e:?}")))?;
    let origin_key = ATTR_ORIGIN
        .try_into()
        .map_err(|e| backend(format!("origin key: {e:?}")))?;
    let origin_value = origin
        .try_into()
        .map_err(|e| backend(format!("origin topic: {e:?}")))?;
    schema_attribute_verifier(schema_id, 1)?
        .require(&key, &value)
        .and_then(|v| v.require(&origin_key, &origin_value))
        .map_err(|e| backend(format!("observer attributes: {e:?}")))
}

fn attribute<'a>(attrs: &'a AttributeSet, key: &str) -> Option<&'a str> {
    let mut values = attrs
        .iter()
        .filter(|a| a.key().as_bytes() == key.as_bytes());
    let first = std::str::from_utf8(values.next()?.value().as_bytes()).ok()?;
    if values.next().is_some() {
        None
    } else {
        Some(first)
    }
}

/// 枚举既有镜像服务，不创建原服务或镜像服务，不清理其他节点。
///
/// # Errors
/// 无法读取 iceoryx2 服务目录时返回后端错误。
pub fn list_observers(config: &Config) -> Result<Vec<ObserverTopic>, TransportError> {
    let found = list_observers_filtered(config, 256, |_| true)?;
    if found.capacity_reached {
        return Err(backend(
            "observer discovery 超过 256 个上限；请使用带过滤/容量状态的枚举",
        ));
    }
    Ok(found.topics)
}

/// 先按原 topic 过滤再计入容量；其他机器人的镜像不会抢占目标预算。
/// 返回达到上限后的首个额外匹配项时停止枚举，不复制更多 topic 字符串。
///
/// # Errors
/// limit 大于 1024 或服务枚举失败时返回错误。limit=0 只探测容量状态。
pub fn list_observers_filtered(
    config: &Config,
    limit: usize,
    mut accepts: impl FnMut(&str) -> bool,
) -> Result<ObserverDiscovery, TransportError> {
    if limit > 1024 {
        return Err(backend("observer discovery limit 最大为 1024"));
    }
    let mut result = ObserverDiscovery::default();
    IxService::list(config, |details| {
        let s = &details.static_details;
        if !matches!(
            s.messaging_pattern(),
            iceoryx2::service::static_config::messaging_pattern::MessagingPattern::PublishSubscribe(
                _
            )
        ) {
            return CallbackProgression::Continue;
        }
        let name = s.name().as_str();
        let attrs = s.attributes();
        if attribute(attrs, ATTR_OBSERVER) != Some("1") {
            return CallbackProgression::Continue;
        }
        let Some(origin) = attribute(attrs, ATTR_ORIGIN) else {
            return CallbackProgression::Continue;
        };
        if !accepts(origin) {
            return CallbackProgression::Continue;
        }
        if !observer_topic(origin).is_ok_and(|expected| expected == name) {
            return CallbackProgression::Continue;
        }
        let Some(schema_id) = attribute(attrs, super::ATTR_SCHEMA_ID)
            .and_then(|v| v.strip_prefix("0x"))
            .and_then(|v| u32::from_str_radix(v, 16).ok())
        else {
            return CallbackProgression::Continue;
        };
        let Some(schema_version) =
            attribute(attrs, super::ATTR_SCHEMA_VERSION).and_then(|v| v.parse::<u16>().ok())
        else {
            return CallbackProgression::Continue;
        };
        if result.topics.len() == limit {
            result.capacity_reached = true;
            return CallbackProgression::Stop;
        }
        result.topics.push(ObserverTopic {
            primary_topic: origin.to_owned(),
            observer_topic: name.to_owned(),
            schema_id,
            schema_version,
        });
        CallbackProgression::Continue
    })
    .map_err(|e| backend(format!("observer list: {e:?}")))?;
    result
        .topics
        .sort_by(|a, b| a.observer_topic.cmp(&b.observer_topic));
    Ok(result)
}

/// 镜像订阅者；FIFO 接收，序号缺口由消费者显式记录。
pub struct ObserverSubscriber<T: BodyMarker> {
    inner: IxPubSubSubscriber,
    _ty: PhantomData<T>,
}

/// 持有镜像 loan；view 只看到前缀之后完整且原样的正式 wire。
pub struct ObserverSample<T: BodyMarker> {
    metadata: ObserverMetadata,
    sample: SampleLease<T>,
}

impl<T: BodyMarker> ObserverSample<T> {
    #[must_use]
    pub const fn metadata(&self) -> ObserverMetadata {
        self.metadata
    }

    /// 校验并借用原始 wire。
    ///
    /// # Errors
    /// 原帧或 typed body 校验失败时返回 transport 错误。
    pub fn view(&self) -> Result<MessageView<'_, T>, TransportError> {
        self.sample.view()
    }
}

/// 严格 open-only。镜像未出现时返错，绝不回退到正式 topic。
///
/// # Errors
/// 镜像缺失、属性不匹配、不是 drop-old 服务或订阅资源不足时返回错误。
pub fn observer_subscriber<T: BodyMarker>(
    node: &Node<IxService>,
    primary_topic: &str,
) -> Result<ObserverSubscriber<T>, TransportError> {
    let name = topic_to_service_name(&observer_topic(primary_topic)?)?;
    let service = node
        .service_builder(&name)
        .publish_subscribe::<[u8]>()
        .enable_safe_overflow(true)
        .open_with_attributes(&verifier(T::SCHEMA_ID, primary_topic)?)
        .map_err(|e| backend(format!("observer open: {e:?}")))?;
    let inner = service
        .subscriber_builder()
        .create()
        .map_err(|e| backend(format!("observer subscriber: {e:?}")))?;
    Ok(ObserverSubscriber {
        inner,
        _ty: PhantomData,
    })
}

impl<T: BodyMarker> ObserverSubscriber<T> {
    /// 取一帧镜像，保留该实例的 FIFO 顺序。
    ///
    /// # Errors
    /// 接收失败或观测前缀损坏时返回错误。
    pub fn try_recv(&mut self) -> Result<Option<ObserverSample<T>>, TransportError> {
        let Some(sample) = self
            .inner
            .receive()
            .map_err(|e| backend(format!("observer receive: {e:?}")))?
        else {
            return Ok(None);
        };
        let metadata = decode_prefix(sample.payload())?;
        let sample =
            SampleLease::from_iceoryx2(Iceoryx2SampleLease::with_offset(sample, PREFIX_SIZE));
        Ok(Some(ObserverSample { metadata, sample }))
    }
}

fn decode_prefix(bytes: &[u8]) -> Result<ObserverMetadata, TransportError> {
    if bytes.len() < PREFIX_SIZE || &bytes[..8] != MAGIC {
        return Err(backend("observer prefix 无效"));
    }
    let number = |at: usize| {
        let mut raw = [0; 8];
        raw.copy_from_slice(&bytes[at..at + 8]);
        u64::from_le_bytes(raw)
    };
    if number(40) != (bytes.len() - PREFIX_SIZE) as u64 || number(24) == 0 {
        return Err(backend("observer wire 长度或序号无效"));
    }
    let mut publisher_instance_id = [0; 16];
    publisher_instance_id.copy_from_slice(&bytes[8..24]);
    Ok(ObserverMetadata {
        publisher_instance_id,
        observer_seq: number(24),
        dropped_frames: number(32),
    })
}

fn reserve(counter: &AtomicUsize, size: usize, limit: usize) -> bool {
    counter
        .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
            old.checked_add(size).filter(|&v| v <= limit)
        })
        .is_ok()
}

struct QueuedFrame {
    wire: Vec<u8>,
    seq: u64,
    counters: Arc<Counters>,
    delivered: bool,
}

impl Drop for QueuedFrame {
    fn drop(&mut self) {
        if !self.delivered {
            self.counters.dropped_frames.fetch_add(1, Ordering::Relaxed);
        }
        self.counters
            .pending_bytes
            .fetch_sub(self.wire.len(), Ordering::AcqRel);
        QUEUED_BYTES.fetch_sub(self.wire.len(), Ordering::AcqRel);
        self.counters.pending_frames.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) struct ObserverDispatcher {
    sender: Option<SyncSender<QueuedFrame>>,
    worker: Option<JoinHandle<()>>,
    counters: Arc<Counters>,
    next_seq: AtomicU64,
    byte_limit: usize,
}

impl ObserverDispatcher {
    pub(crate) fn start(
        node: &Node<IxService>,
        topic: &str,
        schema_id: u32,
        max_publishers: usize,
        byte_limit: usize,
        instance: [u8; 16],
    ) -> Self {
        let counters = Arc::new(Counters::default());
        let mut result = Self {
            sender: None,
            worker: None,
            counters: counters.clone(),
            next_seq: AtomicU64::new(0),
            byte_limit,
        };
        if topic.starts_with(OBSERVER_PREFIX) {
            return result;
        }
        if !reserve(&ACTIVE_WORKERS, 1, MAX_WORKERS) {
            counters
                .initialization_failures
                .fetch_add(1, Ordering::Relaxed);
            return result;
        }
        let mut config = node.config().clone();
        config.global.creation_timeout = CREATION_TIMEOUT;
        config.global.node.cleanup_dead_nodes_on_creation = false;
        config.global.node.cleanup_dead_nodes_on_destruction = false;
        let origin = topic.to_owned();
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let shared = counters.clone();
        if let Ok(worker) = std::thread::Builder::new()
            .name("robot_observer".into())
            .spawn(move || {
                let _exit = WorkerExit(shared.clone());
                worker_loop(
                    &config,
                    &origin,
                    schema_id,
                    max_publishers,
                    byte_limit,
                    instance,
                    &receiver,
                    &shared,
                );
                // 只释放积压 owned wire；stop 后绝不为积压继续设备外发送。
                // receiver 随线程返回销毁，RAII 为全部未发帧退预算并计 drop。
            })
        {
            result.sender = Some(sender);
            result.worker = Some(worker);
        } else {
            ACTIVE_WORKERS.fetch_sub(1, Ordering::AcqRel);
            counters
                .initialization_failures
                .fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    pub(crate) fn monitor(&self) -> ObserverMonitor {
        ObserverMonitor(self.counters.clone())
    }

    pub(crate) fn enqueue(&self, wire: Vec<u8>) {
        let Some(seq) = self
            .next_seq
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .ok()
            .and_then(|n| n.checked_add(1))
        else {
            self.counters.dropped_frames.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let Some(sender) = self.sender.as_ref() else {
            self.counters.dropped_frames.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let size = wire.len();
        if !reserve(&QUEUED_BYTES, size, GLOBAL_BYTE_LIMIT) {
            self.counters.dropped_frames.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if !reserve(&self.counters.pending_bytes, size, self.byte_limit) {
            QUEUED_BYTES.fetch_sub(size, Ordering::AcqRel);
            self.counters.dropped_frames.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.counters.pending_frames.fetch_add(1, Ordering::AcqRel);
        let frame = QueuedFrame {
            wire,
            seq,
            counters: self.counters.clone(),
            delivered: false,
        };
        let _ = sender.try_send(frame);
    }
}

impl Drop for ObserverDispatcher {
    fn drop(&mut self) {
        self.counters.stop.store(true, Ordering::Release);
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                self.counters.join_failures.fetch_add(1, Ordering::Relaxed);
            }
            self.counters.reaped.store(true, Ordering::Release);
        }
    }
}

struct WorkerExit(Arc<Counters>);
impl Drop for WorkerExit {
    fn drop(&mut self) {
        self.0.finished.store(true, Ordering::Release);
        ACTIVE_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn create_publisher(
    node: &Node<IxService>,
    origin: &str,
    schema_id: u32,
    max_publishers: usize,
    byte_limit: usize,
) -> Result<IxPubSubPublisher, TransportError> {
    let name = topic_to_service_name(&observer_topic(origin)?)?;
    let service = node
        .service_builder(&name)
        .publish_subscribe::<[u8]>()
        .history_size(2)
        .subscriber_max_buffer_size(4)
        .max_publishers(max_publishers)
        .max_subscribers(8)
        .enable_safe_overflow(true)
        .open_or_create_with_attributes(&verifier(schema_id, origin)?)
        .map_err(|e| backend(format!("observer service: {e:?}")))?;
    service
        .publisher_builder()
        .initial_max_slice_len(byte_limit + PREFIX_SIZE)
        .allocation_strategy(AllocationStrategy::Static)
        .max_loaned_samples(1)
        .override_sample_preallocation(|_| PREALLOCATED_SAMPLES)
        .backpressure_strategy(BackpressureStrategy::DiscardData)
        .create()
        .map_err(|e| backend(format!("observer publisher: {e:?}")))
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    config: &Config,
    origin: &str,
    schema_id: u32,
    max_publishers: usize,
    byte_limit: usize,
    instance: [u8; 16],
    receiver: &Receiver<QueuedFrame>,
    counters: &Counters,
) {
    if counters.stop.load(Ordering::Acquire) {
        return;
    }
    let Ok(node) = NodeBuilder::new().config(config).create::<IxService>() else {
        counters
            .initialization_failures
            .fetch_add(1, Ordering::Relaxed);
        return;
    };
    if counters.stop.load(Ordering::Acquire) {
        return;
    }
    let estimate = PREALLOCATED_SAMPLES * (byte_limit + PREFIX_SIZE + 256) + 64 * 1024;
    let Some(_shm_budget) = ShmReservation::acquire(counters, &ESTIMATED_SHM_BYTES, estimate)
    else {
        counters
            .initialization_failures
            .fetch_add(1, Ordering::Relaxed);
        return;
    };
    let Ok(publisher) = create_publisher(&node, origin, schema_id, max_publishers, byte_limit)
    else {
        counters
            .initialization_failures
            .fetch_add(1, Ordering::Relaxed);
        return;
    };
    counters.ready.store(true, Ordering::Release);
    while !counters.stop.load(Ordering::Acquire) {
        match receiver.recv_timeout(POLL) {
            Ok(mut frame) => {
                if counters.stop.load(Ordering::Acquire) {
                    break;
                }
                let result = send_frame(
                    &publisher,
                    &frame,
                    instance,
                    counters.dropped_frames.load(Ordering::Relaxed),
                );
                if result.is_ok() {
                    frame.delivered = true;
                    counters.published.fetch_add(1, Ordering::Relaxed);
                } else {
                    counters.send_failures.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if counters.stop.load(Ordering::Acquire) {
                    break;
                }
                if publisher.update_connections().is_err() {
                    counters.connection_failures.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

struct ShmReservation<'a> {
    estimate: usize,
    counters: &'a Counters,
    total: &'a AtomicUsize,
}
impl<'a> ShmReservation<'a> {
    fn acquire(counters: &'a Counters, total: &'a AtomicUsize, estimate: usize) -> Option<Self> {
        if !reserve(total, estimate, ESTIMATED_SHM_LIMIT) {
            return None;
        }
        counters
            .estimated_shm_bytes
            .store(estimate, Ordering::Release);
        Some(Self {
            estimate,
            counters,
            total,
        })
    }
}
impl Drop for ShmReservation<'_> {
    fn drop(&mut self) {
        self.counters
            .estimated_shm_bytes
            .store(0, Ordering::Release);
        self.total.fetch_sub(self.estimate, Ordering::AcqRel);
    }
}

fn send_frame(
    publisher: &IxPubSubPublisher,
    frame: &QueuedFrame,
    instance: [u8; 16],
    dropped: u64,
) -> Result<(), ()> {
    let sample = publisher
        .loan_slice_uninit(frame.wire.len() + PREFIX_SIZE)
        .map_err(|_| ())?;
    let mut prefix = [0; PREFIX_SIZE];
    prefix[..8].copy_from_slice(MAGIC);
    prefix[8..24].copy_from_slice(&instance);
    prefix[24..32].copy_from_slice(&frame.seq.to_le_bytes());
    prefix[32..40].copy_from_slice(&dropped.to_le_bytes());
    prefix[40..48].copy_from_slice(&(frame.wire.len() as u64).to_le_bytes());
    sample
        .write_from_fn(|index| {
            if index < PREFIX_SIZE {
                prefix[index]
            } else {
                frame.wire[index - PREFIX_SIZE]
            }
        })
        .send()
        .map(|_| ())
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic, clippy::nursery)]
    use super::*;
    use iceoryx2::prelude::ServiceName;

    fn unserved(byte_limit: usize) -> (ObserverDispatcher, Receiver<QueuedFrame>) {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        (
            ObserverDispatcher {
                sender: Some(sender),
                worker: None,
                counters: Arc::new(Counters::default()),
                next_seq: AtomicU64::new(0),
                byte_limit,
            },
            receiver,
        )
    }

    #[test]
    fn worker_and_owned_byte_caps_reject_overflow_without_changing_reservation() {
        let workers = AtomicUsize::new(0);
        for _ in 0..MAX_WORKERS {
            assert!(reserve(&workers, 1, MAX_WORKERS));
        }
        assert!(!reserve(&workers, 1, MAX_WORKERS));
        assert_eq!(workers.load(Ordering::Acquire), MAX_WORKERS);
        workers.fetch_sub(1, Ordering::AcqRel);
        assert!(reserve(&workers, 1, MAX_WORKERS));

        let bytes = AtomicUsize::new(0);
        assert!(reserve(&bytes, GLOBAL_BYTE_LIMIT, GLOBAL_BYTE_LIMIT));
        assert!(!reserve(&bytes, 1, GLOBAL_BYTE_LIMIT));
        assert!(!reserve(&bytes, usize::MAX, GLOBAL_BYTE_LIMIT));
        assert_eq!(bytes.load(Ordering::Acquire), GLOBAL_BYTE_LIMIT);
    }

    #[test]
    fn estimated_shm_reservation_rejects_excess_and_releases_on_drop_or_unwind() {
        // 使用同一真实 RAII 类型及局部总额，避免并行测试互相占用全进程预算。
        let total = AtomicUsize::new(0);
        let a = Counters::default();
        let b = Counters::default();
        let full = ShmReservation::acquire(&a, &total, ESTIMATED_SHM_LIMIT).unwrap();
        assert!(ShmReservation::acquire(&b, &total, 1).is_none());
        assert_eq!(b.estimated_shm_bytes.load(Ordering::Acquire), 0);
        assert_eq!(total.load(Ordering::Acquire), ESTIMATED_SHM_LIMIT);
        drop(full);
        assert_eq!(a.estimated_shm_bytes.load(Ordering::Acquire), 0);
        assert_eq!(total.load(Ordering::Acquire), 0);

        let result = std::panic::catch_unwind(|| {
            let _held = ShmReservation::acquire(&b, &total, 4096).unwrap();
            panic!("仅测试镜像 SHM 估算预约的 RAII");
        });
        assert!(result.is_err());
        assert_eq!(b.estimated_shm_bytes.load(Ordering::Acquire), 0);
        assert_eq!(total.load(Ordering::Acquire), 0);
    }

    #[test]
    fn queue_full_and_disconnected_release_all_byte_reservations() {
        let (d, r) = unserved(1024);
        for _ in 0..9 {
            d.enqueue(vec![1; 16]);
        }
        let s = d.monitor().snapshot();
        assert_eq!(
            (s.pending_frames, s.pending_bytes, s.dropped_frames),
            (8, 128, 1)
        );
        drop(r);
        let s = d.monitor().snapshot();
        assert_eq!(
            (s.pending_frames, s.pending_bytes, s.dropped_frames),
            (0, 0, 9)
        );
        d.enqueue(vec![1; 16]);
        let s = d.monitor().snapshot();
        assert_eq!(
            (s.pending_frames, s.pending_bytes, s.dropped_frames),
            (0, 0, 10)
        );
    }

    #[test]
    fn byte_limit_is_independent_of_queue_count_and_sequence_records_gap() {
        let (d, r) = unserved(16);
        d.enqueue(vec![1; 17]);
        d.enqueue(vec![1; 16]);
        let f = r.try_recv().unwrap();
        assert_eq!(f.seq, 2);
        assert_eq!(d.monitor().snapshot().dropped_frames, 1);
        assert_eq!(d.monitor().snapshot().pending_bytes, 16);
        drop(f);
        assert_eq!(d.monitor().snapshot().pending_bytes, 0);
    }

    #[test]
    fn panic_unwind_releases_owned_frame_budget() {
        let (d, r) = unserved(1024);
        d.enqueue(vec![1; 16]);
        let f = r.try_recv().unwrap();
        let result = std::panic::catch_unwind(move || {
            let _held = f;
            panic!("仅测试 owned frame 的 RAII");
        });
        assert!(result.is_err());
        assert_eq!(d.monitor().snapshot().pending_bytes, 0);
        assert_eq!(d.monitor().snapshot().pending_frames, 0);
        assert_eq!(d.monitor().snapshot().dropped_frames, 1);
    }

    #[test]
    fn real_mirror_loan_failure_keeps_drop_accounting() {
        let n = NodeBuilder::new().create::<IxService>().unwrap();
        let name = ServiceName::new(&format!("/obs_small_{}", std::process::id())).unwrap();
        let service = n
            .service_builder(&name)
            .publish_subscribe::<[u8]>()
            .create()
            .unwrap();
        let publisher = service
            .publisher_builder()
            .initial_max_slice_len(1)
            .allocation_strategy(AllocationStrategy::Static)
            .backpressure_strategy(BackpressureStrategy::DiscardData)
            .create()
            .unwrap();
        let (d, r) = unserved(1024);
        d.enqueue(vec![0; 100]);
        let frame = r.try_recv().unwrap();
        assert!(send_frame(&publisher, &frame, [1; 16], 0).is_err());
        drop(frame);
        assert_eq!(d.monitor().snapshot().pending_bytes, 0);
        assert_eq!(d.monitor().snapshot().dropped_frames, 1);
    }

    #[test]
    fn prefix_rejects_truncation_and_raw_length_mismatch() {
        assert!(decode_prefix(&[]).is_err());
        let mut bytes = [0; PREFIX_SIZE + 1];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[24..32].copy_from_slice(&1_u64.to_le_bytes());
        assert!(decode_prefix(&bytes).is_err());
        bytes[40..48].copy_from_slice(&1_u64.to_le_bytes());
        assert_eq!(decode_prefix(&bytes).unwrap().observer_seq, 1);
        assert!(observer_topic("/_robot_observe/v1/r/a").is_err());
    }
}
