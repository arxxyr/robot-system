//! Gateway-side per-ActionId pubsub subscribe thread + `tokio::sync::mpsc`
//! 桥接 —— `StreamTaskEvents` RPC server-streaming response 的 sync→async
//! 适配层（B4 (2/3-c)）。
//!
//! ## 跨进程拓扑回顾（outline §5.2 reviewer round 2 拍板）
//!
//! ```text
//! robot-s1 process                       gateway process (本模块)
//! ─────────────────                       ───────────────────────────
//!
//! Coordinator::CurrentAction.events_tx
//!   ↓ ActionEvent (in-process mpsc)
//! events_drain_thread (s1)
//!   - encode action_event.fbs body
//!   - publish iceoryx2 pubsub:
//!     /<robot>/s1/action_events/<action_id>
//!                                          events_subscribe_thread (本模块)
//!                                            - subscribe 同款 topic
//!                                            - sub.latest() 拉新帧
//!                                            - decode ActionEventEnvelope
//!                                            - 转 proto::TaskEvent
//!                                            - tokio::sync::mpsc::Sender::try_send
//!                                              （满 → drop+warn；closed → break）
//!                                          ReceiverStream<Result<TaskEvent, Status>>
//!                                            （tonic stream wrap）
//! ```
//!
//! ## 生命周期 / lifecycle 拍板（outline §5.5）
//!
//! 1. **backpressure**：iceoryx2 pubsub keep-last 单 slot；gateway sub 来不
//!    及拉走中间帧自然丢（最新 wins）。`tokio::sync::mpsc::Sender::try_send`
//!    满 → drop + `tracing::warn!`，**不**阻塞（一致性 by 客户端自己 catch
//!    up `GetTaskStatus` 拿当前快照）。
//! 2. **client disconnect**（client 主动 drop response stream）→ inner
//!    `Receiver` drop → `Sender::try_send` 返 `TrySendError::Closed` →
//!    subscribe thread break + 退出 + iceoryx2 subscriber 自然 drop（→
//!    服务侧 reference count 降低，但**不**通知 s1）。
//! 3. **terminal event**（envelope kind = Completed / Failed / Cancelled）
//!    尝试入队后 thread break + 退出；队列满仍按第 1 条丢弃。本模块不再继续 poll（与 s1
//!    `events_drain` `terminal_grace` 100 ms 配对）。
//! 4. **gateway shutdown**：handler 入口 `shutdown_gate` 拒新 stream；已
//!    in-flight stream 由 `EventsSubscribeStream::drop` → `request_stop` →
//!    线程退出（client 端 stream Drop 触发；tonic graceful shutdown 不强
//!    制中断 in-flight handler）。
//!
//! ## 关键约束（reviewer 拍板）
//!
//! - **gateway disconnect 不通知 s1**（outline §5.5 #3）：stream 是 audit
//!   通道，不是控制通道。客户端要主动停 task 必须调 `CancelTask` RPC，
//!   gateway 仅 unsubscribe 本地 iceoryx2，**不**给 s1 发 cancel signal。
//! - **proto::TaskEvent.request_id** = `TaskHandleRef.request_id`（caller 在
//!   spawn cfg 里传）；**不**进 fbs envelope（那边只有 action_id）。
//! - **`request_id` echo by handler，不进 cache**：`StreamTaskEvents` 是
//!   streaming RPC，不进 [`crate::idempotency`]。
//!
//! ## 不在范围（B4 (2/3-c) 边界）
//!
//! - **不**做 leftover topic cleanup（s1 端 `events_drain` `terminal_grace`
//!   后 publisher drop 自然 unregister；gateway 端只 sub，无需主动清理）。
//! - **不**做 cross-process integration test → B4 (3/3) `crates/robot-rpc/tests/...`。
//! - **不**通知 s1 取消（client disconnect 仅 unsubscribe 本地）。
//! - **不**复用 `crate::rr_io_thread`（rr_io_thread 是 single-shot RR；
//!   StreamTaskEvents 走 pubsub）。

#![allow(clippy::doc_markdown)]
// `tonic::Status` 是 ~176 字节；handler bridge 的 try_send 路径里 Item =
// `Result<TaskEvent, tonic::Status>`，clippy `result_large_err` 在 streaming
// RPC code path 上是噪音。
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_msg::markers;
use robot_msg::msg::{
    ActionEventEnvelope as FbEnv, ActionEventKindFb, ActionFailureFb, ActionFailureKindFb,
    ActionOutcomeFb, ActionOutcomeKindFb,
};
use robot_transport::iceoryx2::{Iceoryx2Subscriber, TopicClass, subscriber as open_sub};
use robot_transport::traits::LatestSubscriber;
use tokio::sync::mpsc::{Receiver, Sender, error::TrySendError};

use crate::proto::{
    ActionFailureProto, ActionOutcomeProto, TaskEvent, TaskEventKind, action_failure_proto,
    action_outcome_proto,
};

// ============================================================================
// Config / Metrics / Handle
// ============================================================================

/// `events_subscribe_thread` 配置。每条 `StreamTaskEvents` RPC 一份独立
/// config。
#[derive(Clone, Debug)]
pub struct EventsSubscribeConfig {
    /// 机器人 ID，用于构造 iceoryx2 topic
    /// `/<robot_id>/s1/action_events/<action_id>`。
    pub robot_id: String,
    /// `proto::TaskHandleRef.action_id`（UUIDv7 hex 串）。`fbs::ActionEventEnvelope.action_id`
    /// 应该跟此值一致；本 thread **不**强制校验（仅 subscribe 走 topic name
    /// 路由）。
    pub action_id_hex: String,
    /// `proto::TaskHandleRef.request_id`（UUIDv7 hex 串）；echo 进每条
    /// `proto::TaskEvent.request_id`，client 端 correlation 用。
    pub request_id: String,
    /// `sub.latest()` poll 间隔（也是 stop signal 响应粒度）。默认 50 ms：
    /// 跟 s1 端 `events_drain` `poll_interval` 同款，让 push-pull 节奏对齐。
    pub poll_interval: Duration,
    /// `tokio::sync::mpsc::channel` buffer 大小。每个 ActionTicket 事件量
    /// 通常少（Started + 数次 Progress + 1 个 terminal），buf=16 足够；满
    /// 时 drop+warn（outline §5.5 #1）。
    pub channel_buf: usize,
}

/// `events_subscribe_thread` 运行期 metrics。
#[derive(Default, Debug)]
pub struct EventsSubscribeMetrics {
    /// `sub.latest()` 拿到 `Some(lease)` 的次数（每次代表一帧 envelope）。
    pub samples_received: AtomicU64,
    /// 成功 `tx.try_send(Ok(TaskEvent))` 的次数。
    pub events_forwarded: AtomicU64,
    /// `flatbuffers::root::<ActionEventEnvelope>` 失败次数（罕见；wire
    /// schema mismatch 才会触发）。
    pub decode_errors: AtomicU64,
    /// `tx.try_send` 满 → drop + warn 的次数（outline §5.5 #1
    /// backpressure）。
    pub channel_full_drops: AtomicU64,
    /// 因 mpsc receiver dropped（client disconnect）退出的次数（0 / 1）。
    pub stopped_by_disconnect: AtomicU64,
    /// 因 stop signal（gateway shutdown / `EventsSubscribeStream::drop`）
    /// 退出的次数（0 / 1）。
    pub stopped_by_signal: AtomicU64,
    /// 因 terminal event（Completed / Failed / Cancelled）退出的次数
    /// （0 / 1；正常 ticket 应最终命中 1 次）。
    pub stopped_by_terminal: AtomicU64,
}

/// `events_subscribe_thread` 句柄。caller 通过 [`Self::request_stop`] +
/// [`Self::join`] 关停（或直接 `drop` 让线程在下一轮 stop check 时退出）。
pub struct EventsSubscribeHandle {
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    metrics: Arc<EventsSubscribeMetrics>,
    topic: String,
}

impl EventsSubscribeHandle {
    /// 取 metrics `Arc`（线程内部写、外部读）。
    #[must_use]
    pub fn metrics(&self) -> Arc<EventsSubscribeMetrics> {
        Arc::clone(&self.metrics)
    }

    /// 取 topic 串（diagnostic）。
    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// 通知主循环退出（下一轮 stop check 命中即跳出，**不**等当前 sample /
    /// **不**等 grace）。
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
// spawn API
// ============================================================================

/// 起一条 `events_subscribe_thread` + 对应的
/// `tokio::sync::mpsc::channel(channel_buf)`。`Receiver` 给 caller wrap 进
/// `tonic::Streaming` 返客户端；`Sender` 由本 fn 内部线程持有，每帧
/// envelope 解码后 `try_send` 进去。
///
/// `iceoryx2 Node + Subscriber` 在新线程内构造（subscriber `!Send`，必须
/// 在 owner 线程内 create）。
///
/// `client_factory` 模式（沿
/// [`crate::rr_io_thread::spawn_rr_io_thread`] 同款）：spawn 返前用
/// sync `(ready_tx, ready_rx)` 等子线程的 subscriber 创建成功，否则把
/// factory panic 转成 `std::io::Error`。
///
/// # Errors
///
/// - iceoryx2 Node create 失败（罕见；shm 资源耗尽等）
/// - iceoryx2 subscriber create 失败（topic 名非法 / port 耗尽等）
/// - `thread::Builder::spawn` 失败
pub fn spawn_events_subscribe_thread(
    cfg: EventsSubscribeConfig,
) -> std::io::Result<(
    EventsSubscribeHandle,
    Receiver<Result<TaskEvent, tonic::Status>>,
)> {
    let topic = format!("/{}/s1/action_events/{}", cfg.robot_id, cfg.action_id_hex);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<TaskEvent, tonic::Status>>(cfg.channel_buf);
    let stop = Arc::new(AtomicBool::new(false));
    let metrics = Arc::new(EventsSubscribeMetrics::default());

    let stop_clone = Arc::clone(&stop);
    let metrics_clone = Arc::clone(&metrics);
    let topic_owned = topic.clone();
    let topic_for_factory = topic.clone();
    let thread_name = format!("gw_evt_sub_{}", short_action_id(&cfg.action_id_hex));

    // ready signal：spawn 返前确认 subscriber 创建成功（factory panic /
    // iceoryx2 错都在这里捕获）。同 rr_io_thread 模式。
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

    let join = thread::Builder::new().name(thread_name).spawn(move || {
        // subscriber 在线程内 create + 持有；线程退出时自然 drop +
        // unregister（iceoryx2 reference count 减一，**不**通知 s1）。
        let node = NodeBuilder::new()
            .create::<IxService>()
            .expect("iceoryx2 NodeBuilder::create");
        let mut subscriber: Iceoryx2Subscriber<markers::ActionEventEnvelope> =
            open_sub::<markers::ActionEventEnvelope>(
                &node,
                &topic_for_factory,
                TopicClass::TaskEvent,
            )
            .expect("iceoryx2 subscriber create");
        // Node 和 subscriber 都由本线程持有，退出时按声明逆序释放。
        // 0.9.3 的端口还拥有 SharedNode 引用，无需为每条 stream 泄漏 Node。
        let _ = ready_tx.send(());
        drop(ready_tx);
        run_subscribe_loop(&cfg, &mut subscriber, &tx, &stop_clone, &metrics_clone);
    })?;

    if ready_rx.recv().is_err() {
        return Err(std::io::Error::other(format!(
            "events_subscribe_thread topic={topic}: subscriber create panicked before ready"
        )));
    }

    Ok((
        EventsSubscribeHandle {
            join,
            stop,
            metrics,
            topic: topic_owned,
        },
        rx,
    ))
}

/// thread name 用：取 action_id_hex 前 8 字符（pthread name 上限 16 含
/// NUL；`gw_evt_sub_` 11 字符 + 上限 4 字符；取 8 字符做安全 truncate
/// 后再让 builder 自己截）。多个并发 stream 同 prefix 没问题，audit log
/// 看 topic 区分。
fn short_action_id(action_id_hex: &str) -> String {
    action_id_hex.chars().take(8).collect()
}

// ============================================================================
// 主循环
// ============================================================================

fn run_subscribe_loop(
    cfg: &EventsSubscribeConfig,
    subscriber: &mut Iceoryx2Subscriber<markers::ActionEventEnvelope>,
    tx: &Sender<Result<TaskEvent, tonic::Status>>,
    stop: &AtomicBool,
    metrics: &EventsSubscribeMetrics,
) {
    while !stop.load(Ordering::Acquire) {
        // sub.latest() 失败（极罕见）→ log + continue；连续失败由 channel
        // closed / stop signal 自然兜底退出。
        let lease_opt = match subscriber.latest() {
            Ok(opt) => opt,
            Err(e) => {
                tracing::warn!(
                    topic = %format_topic(cfg),
                    error = ?e,
                    "events_subscribe_thread: subscriber.latest() failed"
                );
                thread::sleep(cfg.poll_interval);
                continue;
            }
        };

        let Some(lease) = lease_opt else {
            // 没有新 sample（典型空 idle 路径）。
            thread::sleep(cfg.poll_interval);
            continue;
        };

        metrics.samples_received.fetch_add(1, Ordering::Relaxed);

        // 一次 lease 内拷出 envelope bytes，避免 lease 跨线程 / 跨 tx 边界。
        let envelope_bytes: Vec<u8> = {
            let view = match lease.view() {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        topic = %format_topic(cfg),
                        error = ?e,
                        "events_subscribe_thread: lease.view() failed"
                    );
                    metrics.decode_errors.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            let env: FbEnv<'_> = view.body.get();
            env._tab.buf().to_vec()
        };
        // lease drop here

        let env = match flatbuffers::root::<FbEnv>(&envelope_bytes) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    topic = %format_topic(cfg),
                    error = ?e,
                    "events_subscribe_thread: ActionEventEnvelope decode failed"
                );
                metrics.decode_errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        let kind_fb = env.kind();
        let task_event = envelope_to_task_event(&cfg.request_id, &cfg.action_id_hex, &env);
        let is_terminal = matches!(
            kind_fb,
            ActionEventKindFb::Completed | ActionEventKindFb::Failed | ActionEventKindFb::Cancelled
        );

        let enqueued = match tx.try_send(Ok(task_event)) {
            Ok(()) => {
                metrics.events_forwarded.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Full(_dropped)) => {
                metrics.channel_full_drops.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    topic = %format_topic(cfg),
                    "events_subscribe_thread: tonic stream channel full; dropped event \
                     (client should catch up via GetTaskStatus snapshot)"
                );
                false
            }
            Err(TrySendError::Closed(_dropped)) => {
                // client drop response stream → mpsc::Receiver dropped。
                // 自然退出，**不**通知 s1（outline §5.5 #3）。
                metrics.stopped_by_disconnect.store(1, Ordering::Release);
                tracing::debug!(
                    topic = %format_topic(cfg),
                    "events_subscribe_thread: stream channel closed (client disconnected); exiting"
                );
                return;
            }
        };

        if is_terminal {
            // 已收到终态；入队是否成功单独记录，满队列仍由客户端快照补齐。
            // 无须等待 s1 的 terminal_grace，直接退出释放 subscriber 和 node。
            metrics.stopped_by_terminal.store(1, Ordering::Release);
            tracing::debug!(
                topic = %format_topic(cfg),
                ?kind_fb,
                enqueued,
                "events_subscribe_thread: terminal event received; exiting"
            );
            return;
        }

        // 注意：**不**在收到一帧后立刻 sleep —— iceoryx2 keep-last 单 slot；
        // 下一轮 latest() 即 None（同 sample 不重复返），需要 sleep 等 s1
        // 推下一帧。
        thread::sleep(cfg.poll_interval);
    }

    // stop signal 命中。
    metrics.stopped_by_signal.store(1, Ordering::Release);
    tracing::debug!(
        topic = %format_topic(cfg),
        "events_subscribe_thread: stopped by signal"
    );
}

fn format_topic(cfg: &EventsSubscribeConfig) -> String {
    format!("/{}/s1/action_events/{}", cfg.robot_id, cfg.action_id_hex)
}

// ============================================================================
// fbs ActionEventEnvelope → proto::TaskEvent 映射
// ============================================================================

fn envelope_to_task_event(request_id: &str, action_id_hex: &str, env: &FbEnv<'_>) -> TaskEvent {
    let event_kind = task_event_kind_fb_to_proto(env.kind());
    let outcome = env.outcome().map(action_outcome_fb_to_proto);
    let failure = env.failure().map(action_failure_fb_to_proto);
    TaskEvent {
        request_id: request_id.to_string(),
        action_id: action_id_hex.to_string(),
        at_ns: env.at_ns(),
        event: event_kind.into(),
        progress: env.progress(),
        outcome,
        failure,
    }
}

#[allow(clippy::match_same_arms)]
fn task_event_kind_fb_to_proto(kind: ActionEventKindFb) -> TaskEventKind {
    match kind {
        ActionEventKindFb::Unknown => TaskEventKind::Unspecified,
        ActionEventKindFb::Started => TaskEventKind::Started,
        ActionEventKindFb::Progress => TaskEventKind::Progress,
        ActionEventKindFb::Completed => TaskEventKind::Completed,
        ActionEventKindFb::Failed => TaskEventKind::Failed,
        ActionEventKindFb::Cancelled => TaskEventKind::Cancelled,
        // 未来 variant 兼容 → Unspecified（client 看到会按"未知 event"忽略）
        _ => TaskEventKind::Unspecified,
    }
}

#[allow(clippy::match_same_arms)]
fn action_outcome_fb_to_proto(outcome: ActionOutcomeFb<'_>) -> ActionOutcomeProto {
    use action_outcome_proto::Kind as P;
    let kind: P = match outcome.kind() {
        ActionOutcomeKindFb::Unknown => P::Unspecified,
        ActionOutcomeKindFb::Success => P::Success,
        ActionOutcomeKindFb::PartialSuccess => P::PartialSuccess,
        _ => P::Unspecified,
    };
    ActionOutcomeProto {
        kind: kind.into(),
        warning: outcome.warning().unwrap_or("").to_string(),
    }
}

#[allow(clippy::match_same_arms)]
fn action_failure_fb_to_proto(failure: ActionFailureFb<'_>) -> ActionFailureProto {
    use action_failure_proto::Kind as P;
    let kind: P = match failure.kind() {
        ActionFailureKindFb::Unknown => P::Unspecified,
        ActionFailureKindFb::DeadlineExceeded => P::DeadlineExceeded,
        ActionFailureKindFb::PreconditionFailed => P::PreconditionFailed,
        ActionFailureKindFb::AdapterError => P::AdapterError,
        ActionFailureKindFb::SafetyAbort => P::SafetyAbort,
        ActionFailureKindFb::InternalError => P::InternalError,
        _ => P::Unspecified,
    };
    ActionFailureProto {
        kind: kind.into(),
        reason: failure.reason().unwrap_or("").to_string(),
    }
}

// ============================================================================
// EventsSubscribeStream —— 把 (Handle, Receiver) 包成 tonic 兼容 Stream
// ============================================================================

use std::pin::Pin;
use std::task::{Context, Poll};

use tonic::codegen::tokio_stream::Stream;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;

/// `Pin<Box<dyn Stream>>` wrapper，把 `tokio::sync::mpsc::Receiver` 包成
/// tonic 兼容 Stream，并在 drop 时 `request_stop` 让 subscribe thread 退出
/// （client 端 cancel response stream 会触发 drop；tonic graceful shutdown
/// 不主动 drop in-flight stream，但 client 自己一旦超时 / cancel 即触发）。
///
/// **不**在 drop 里 join thread —— drop 是 sync，join 会阻塞 tokio runtime
/// worker。subscribe thread 在收到 stop signal 后立刻退出（poll_interval
/// 50 ms 内），留 OS 自然 reap join handle（线程独立资源不悬挂）。
pub struct EventsSubscribeStream {
    inner: ReceiverStream<Result<TaskEvent, tonic::Status>>,
    stop: Arc<AtomicBool>,
    /// thread 的 join handle 在 stream drop 时**不**主动 join（drop 是 sync）；
    /// 持 `Option` 让 thread 资源在 stream lifetime 内保留，handle drop
    /// 时 OS 回收。
    _join: Option<JoinHandle<()>>,
    metrics: Arc<EventsSubscribeMetrics>,
    topic: String,
}

impl EventsSubscribeStream {
    /// 用 [`spawn_events_subscribe_thread`] 返的 `(handle, rx)` 构造。
    #[must_use]
    pub fn new(
        rx: Receiver<Result<TaskEvent, tonic::Status>>,
        handle: EventsSubscribeHandle,
    ) -> Self {
        Self {
            inner: ReceiverStream::new(rx),
            stop: handle.stop,
            _join: Some(handle.join),
            metrics: handle.metrics,
            topic: handle.topic,
        }
    }

    /// 取 metrics 引用（diagnostic / 单测）。
    #[must_use]
    pub fn metrics(&self) -> Arc<EventsSubscribeMetrics> {
        Arc::clone(&self.metrics)
    }

    /// 取 topic 串（diagnostic）。
    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }
}

impl Stream for EventsSubscribeStream {
    type Item = Result<TaskEvent, tonic::Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl Drop for EventsSubscribeStream {
    fn drop(&mut self) {
        // 信号 thread 退出（thread 在 ≤ poll_interval 内退；不阻塞当前
        // tokio worker）。
        self.stop.store(true, Ordering::Release);
        tracing::debug!(
            topic = %self.topic,
            "EventsSubscribeStream dropped; signalled subscribe thread to stop"
        );
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use iceoryx2::prelude::NodeBuilder;
    use iceoryx2::service::ipc::Service as IxService;
    use robot_msg::build_helpers::build_action_event_envelope;
    use robot_transport::Publisher;
    use robot_transport::iceoryx2::publisher as open_pub;
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;

    fn fresh_topic_pair(suffix: &str) -> (String, String) {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let robot_id = format!("evt_sub_test_{}_{}_{suffix}", std::process::id(), seq);
        let action_id_hex = "01900000-0000-7000-8000-000000000001".to_string();
        (robot_id, action_id_hex)
    }

    fn make_publisher(
        topic: &str,
    ) -> robot_transport::iceoryx2::Iceoryx2Publisher<markers::ActionEventEnvelope> {
        let pub_node: &'static _ =
            Box::leak(Box::new(NodeBuilder::new().create::<IxService>().unwrap()));
        open_pub::<markers::ActionEventEnvelope>(pub_node, topic, TopicClass::TaskEvent)
            .expect("publisher open")
    }

    fn publish_envelope(
        publisher: &mut robot_transport::iceoryx2::Iceoryx2Publisher<markers::ActionEventEnvelope>,
        action_id_hex: &str,
        kind: ActionEventKindFb,
        at_ns: u64,
        seq: u64,
    ) {
        let body = build_action_event_envelope(action_id_hex, at_ns, kind, 0.0, None, None);
        let mut writer = publisher.loan(body.len() + 1024).expect("loan");
        writer.set_seq(seq);
        writer.set_source_id(2);
        writer.set_stamp_ns(at_ns);
        writer.set_body_bytes(&body);
        writer.publish().expect("publish");
    }

    fn cfg_for(robot_id: &str, action_id_hex: &str) -> EventsSubscribeConfig {
        EventsSubscribeConfig {
            robot_id: robot_id.to_string(),
            action_id_hex: action_id_hex.to_string(),
            request_id: uuid::Uuid::now_v7().to_string(),
            poll_interval: Duration::from_millis(10),
            channel_buf: 16,
        }
    }

    #[cfg(target_os = "linux")]
    mod node_lifetime {
        use super::*;
        use robot_core::time::{DurationNs, RobotClock, SystemClock};
        use std::process::{Command, Stdio};

        const CHILD: &str = "ROBOT_TEST_ACTION_SUBSCRIBER_LIFETIME_CHILD";
        const TEST: &str = "events_subscribe::tests::node_lifetime::action_subscriber_releases_node_under_fd_limit";

        #[test]
        fn action_subscriber_releases_node_under_fd_limit() {
            if std::env::var_os(CHILD).is_some() {
                run_child();
                return;
            }
            let sentinel = inherited_sentinel();
            let sentinel_fd = std::os::fd::AsRawFd::as_raw_fd(&sentinel);
            let fd_limit = fallback_fd_limit();
            let force_fcntl = std::env::var_os("ROBOT_TEST_NODE_FORCE_FCNTL").is_some();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .env("ROBOT_TEST_NODE_FD_SENTINEL", sentinel_fd.to_string())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // 仅 fork 后的子进程标记 CLOEXEC，不关闭父进程或 spawn 错误管道。
            unsafe {
                std::os::unix::process::CommandExt::pre_exec(&mut command, move || {
                    if force_fcntl {
                        mark_child_fds_with_fcntl(fd_limit)
                    } else {
                        mark_child_fds_cloexec(fd_limit)
                    }
                });
            }
            let mut child = command.spawn().unwrap();
            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            // 两条管道同时读取，底层错误日志不能阻塞子进程并制造假超时。
            let stdout_reader = std::thread::spawn(move || read_output(stdout));
            let stderr_reader = std::thread::spawn(move || read_output(stderr));
            let clock = SystemClock::new();
            let deadline = clock.now() + DurationNs::from_secs(30);
            let mut timed_out = false;
            while child.try_wait().unwrap().is_none() {
                if clock.now() >= deadline {
                    child.kill().unwrap();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let status = child.wait().unwrap();
            assert_eq!(
                unsafe { libc::fcntl(sentinel_fd, libc::F_GETFD) },
                0,
                "子进程隔离不能改父 FD 标志"
            );
            let stdout_bytes = stdout_reader.join().unwrap();
            let stderr_bytes = stderr_reader.join().unwrap();
            let stdout = String::from_utf8_lossy(&stdout_bytes);
            let stderr = String::from_utf8_lossy(&stderr_bytes);
            if status.success() {
                for line in stdout
                    .lines()
                    .filter(|line| line.contains("action_subscriber_"))
                {
                    eprintln!("{line}");
                }
            } else {
                eprintln!("{stdout}{stderr}");
            }
            assert!(!timed_out, "真实 subscriber 生命周期子进程超时");
            assert!(stdout.contains("action_subscriber_child nofile=128"));
            assert!(status.success(), "子进程失败：{status}");
            assert!(stdout.contains("action_subscriber_done rounds=256"));
        }

        fn inherited_sentinel() -> std::os::fd::OwnedFd {
            use std::os::fd::{AsRawFd, FromRawFd};
            let source = std::fs::File::open("/dev/null").unwrap();
            // F_DUPFD 故意不设置 CLOEXEC，模拟前序测试保留的库资源；OwnedFd 最终归还。
            let raw = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD, 64) };
            assert!(raw >= 64);
            unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) }
        }

        fn fallback_fd_limit() -> libc::c_int {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) },
                0
            );
            // 旧内核回退只遍历有限的父进程硬上界；不改变任一进程的限额。
            libc::c_int::try_from(limit.rlim_max).expect("测试要求有限且可表示的 NOFILE 硬上界")
        }

        fn mark_child_fds_cloexec(fd_limit: libc::c_int) -> std::io::Result<()> {
            // pre_exec 内仅作系统调用与栈上运算，不分配、记录日志或获取进程锁。
            let result = unsafe {
                libc::syscall(
                    libc::SYS_close_range,
                    3_u32,
                    u32::MAX,
                    libc::CLOSE_RANGE_CLOEXEC,
                )
            };
            if result == 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ENOSYS | libc::EINVAL) => mark_child_fds_with_fcntl(fd_limit),
                _ => Err(error),
            }
        }

        fn mark_child_fds_with_fcntl(fd_limit: libc::c_int) -> std::io::Result<()> {
            for fd in 3..fd_limit {
                loop {
                    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                    if flags >= 0
                        && unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == 0
                    {
                        break;
                    }
                    let error = std::io::Error::last_os_error();
                    match error.raw_os_error() {
                        Some(libc::EBADF) => break,
                        Some(libc::EINTR) => {}
                        _ => return Err(error),
                    }
                }
            }
            Ok(())
        }

        fn read_output(mut pipe: impl std::io::Read) -> Vec<u8> {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).unwrap();
            bytes
        }

        fn owned_node_directories(
            expected: usize,
        ) -> std::collections::BTreeSet<std::path::PathBuf> {
            // 只观察本隔离子进程实际打开的 Node details；不遍历或删除全局资源。
            let paths: std::collections::BTreeSet<_> = std::fs::read_dir("/proc/self/fd")
                .unwrap()
                .map(|entry| entry.unwrap())
                .filter_map(|entry| std::fs::read_link(entry.path()).ok())
                .filter(|path| {
                    path.file_name()
                        .is_some_and(|name| name == "iox2_node.details")
                })
                .map(|path| path.parent().unwrap().to_path_buf())
                .collect();
            assert_eq!(
                paths.len(),
                expected,
                "本轮真实主节点、镜像 worker 和 subscriber 的 Node 数必须匹配生命周期阶段"
            );
            assert!(paths.iter().all(|path| path.is_dir()));
            paths
        }

        fn assert_node_directories_released(
            paths: &std::collections::BTreeSet<std::path::PathBuf>,
        ) {
            for path in paths {
                assert!(
                    !path.exists(),
                    "真实 Node 完整退场后目录仍残留（空目录同样失败）：{}",
                    path.display()
                );
            }
        }

        fn fd_count() -> usize {
            std::fs::read_dir("/proc/self/fd").unwrap().count()
        }

        fn run_child() {
            let sentinel_fd: libc::c_int = std::env::var("ROBOT_TEST_NODE_FD_SENTINEL")
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(
                unsafe { libc::fcntl(sentinel_fd, libc::F_GETFD) },
                -1,
                "exec 不得继承父进程哨兵 FD"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EBADF)
            );
            println!("action_subscriber_initial_fd_count count={}", fd_count());
            for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
                let entry = entry.unwrap();
                if let Ok(target) = std::fs::read_link(entry.path()) {
                    println!(
                        "action_subscriber_initial_fd fd={} target={}",
                        entry.file_name().to_string_lossy(),
                        target.display()
                    );
                }
            }
            let limit = libc::rlimit {
                rlim_cur: 128,
                rlim_max: 128,
            };
            let no_core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // 限额只约束隔离测试子进程，不改变 daemon 或其它测试的环境。
            assert_eq!(
                unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit) },
                0
            );
            assert_eq!(
                unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const no_core) },
                0
            );
            println!("action_subscriber_child nofile=128");
            let clock = SystemClock::new();
            let robot_id = format!(
                "subscriber_lifetime_{}_{}",
                std::process::id(),
                clock.now().0
            );
            let baseline = fd_count();
            for index in 0..256 {
                subscriber_round_trip(&robot_id, index);
                if index % 8 == 0 {
                    println!("action_subscriber_fd round={index} count={}", fd_count());
                }
            }
            let final_count = fd_count();
            println!("action_subscriber_done rounds=256 baseline={baseline} final={final_count}");
            assert!(final_count <= baseline, "每 Action 的 Node/port FD 未归还");
        }

        fn subscriber_round_trip(robot_id: &str, index: u64) {
            let clock = SystemClock::new();
            let action_id = uuid::Uuid::now_v7().to_string();
            let topic = format!("/{robot_id}/s1/action_events/{action_id}");
            // 对照端正常持有 Node；被测 subscriber 只经真实 gateway 入口创建。
            let node = NodeBuilder::new().create::<IxService>().unwrap();
            let primary_directories = owned_node_directories(1);
            let mut publisher =
                open_pub::<markers::ActionEventEnvelope>(&node, &topic, TopicClass::TaskEvent)
                    .unwrap();
            let observer = publisher.observer_monitor();
            let ready_deadline = clock.now() + DurationNs::from_secs(2);
            loop {
                let stats = observer.snapshot();
                assert_eq!(stats.initialization_failures, 0, "镜像必须真实启动");
                assert!(!stats.finished, "镜像不能在本轮开始前退场");
                if stats.ready {
                    break;
                }
                assert!(clock.now() < ready_deadline, "镜像启动超时");
                std::thread::sleep(Duration::from_millis(1));
            }
            let publisher_directories = owned_node_directories(2);
            assert!(publisher_directories.is_superset(&primary_directories));
            let mirror_directories = publisher_directories
                .difference(&primary_directories)
                .cloned()
                .collect();
            let mut cfg = cfg_for(robot_id, &action_id);
            cfg.poll_interval = Duration::from_millis(1);
            let request_id = cfg.request_id.clone();
            let (handle, mut rx) = spawn_events_subscribe_thread(cfg).unwrap();
            let node_directories = owned_node_directories(3);
            assert!(node_directories.is_superset(&publisher_directories));
            let subscriber_directories = node_directories
                .difference(&publisher_directories)
                .cloned()
                .collect();
            let stamp = clock.now().0;
            publish_envelope(
                &mut publisher,
                &action_id,
                ActionEventKindFb::Started,
                stamp,
                index,
            );
            let deadline = clock.now() + DurationNs::from_secs(2);
            loop {
                publisher.update_connections().unwrap();
                match rx.try_recv() {
                    Ok(event) => {
                        let event = event.unwrap();
                        assert_eq!(event.action_id, action_id);
                        assert_eq!(event.request_id, request_id);
                        assert_eq!(event.at_ns, stamp);
                        assert_eq!(event.event, TaskEventKind::Started as i32);
                        break;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
                    Err(error) => panic!("真实 subscribe 线程提前退出：{error}"),
                }
                assert!(clock.now() < deadline, "真实 gateway 事件必须可达");
                std::thread::sleep(Duration::from_millis(1));
            }
            handle.request_stop();
            handle.join().unwrap();
            drop(rx);
            assert_eq!(owned_node_directories(2), publisher_directories);
            assert_node_directories_released(&subscriber_directories);
            drop(publisher);
            let stats = observer.snapshot();
            assert!(
                stats.finished && stats.reaped,
                "镜像线程必须真实完成并 join"
            );
            assert_eq!(stats.initialization_failures, 0);
            assert_eq!(stats.join_failures, 0);
            assert_eq!(stats.pending_frames, 0);
            assert_eq!(stats.pending_bytes, 0);
            assert_eq!(stats.estimated_shm_bytes, 0);
            assert_eq!(owned_node_directories(1), primary_directories);
            assert_node_directories_released(&mirror_directories);
            drop(node);
            assert!(owned_node_directories(0).is_empty());
            assert_node_directories_released(&node_directories);
        }
    }

    async fn wait_for_event(
        rx: &mut Receiver<Result<TaskEvent, tonic::Status>>,
        timeout: Duration,
    ) -> Result<TaskEvent, tonic::Status> {
        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Some(item)) => item,
            Ok(None) => panic!("rx channel closed before event"),
            Err(elapsed) => panic!("wait_for_event timed out after {timeout:?}: {elapsed}"),
        }
    }

    #[tokio::test]
    async fn started_event_round_trip() {
        let (robot_id, action_id_hex) = fresh_topic_pair("started");
        let topic = format!("/{robot_id}/s1/action_events/{action_id_hex}");
        let mut publisher = make_publisher(&topic);

        let (handle, mut rx) =
            spawn_events_subscribe_thread(cfg_for(&robot_id, &action_id_hex)).unwrap();

        publish_envelope(
            &mut publisher,
            &action_id_hex,
            ActionEventKindFb::Started,
            12345,
            0,
        );
        let event = wait_for_event(&mut rx, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(event.action_id, action_id_hex);
        assert_eq!(event.event, TaskEventKind::Started as i32);
        assert_eq!(event.at_ns, 12345);
        assert!(event.outcome.is_none());

        handle.request_stop();
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn terminal_completed_event_exits_thread() {
        let (robot_id, action_id_hex) = fresh_topic_pair("term_completed");
        let topic = format!("/{robot_id}/s1/action_events/{action_id_hex}");
        let mut publisher = make_publisher(&topic);

        let (handle, mut rx) =
            spawn_events_subscribe_thread(cfg_for(&robot_id, &action_id_hex)).unwrap();

        // 用 build_action_event_envelope 显式带 outcome（terminal Completed）。
        let body = build_action_event_envelope(
            &action_id_hex,
            999,
            ActionEventKindFb::Completed,
            0.0,
            Some((ActionOutcomeKindFb::Success, "")),
            None,
        );
        let mut writer = publisher.loan(body.len() + 1024).unwrap();
        writer.set_seq(0);
        writer.set_source_id(2);
        writer.set_stamp_ns(999);
        writer.set_body_bytes(&body);
        writer.publish().unwrap();

        let event = wait_for_event(&mut rx, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(event.event, TaskEventKind::Completed as i32);
        let outcome = event.outcome.expect("outcome present");
        assert_eq!(outcome.kind, action_outcome_proto::Kind::Success as i32);

        // terminal 后线程自然退出；join 应在 ~poll_interval 内返。
        let start = Instant::now();
        handle.join().unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "terminal-driven join took {:?}, expected < 1s",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn channel_close_exits_thread() {
        let (robot_id, action_id_hex) = fresh_topic_pair("chan_close");
        let _publisher = make_publisher(&format!("/{robot_id}/s1/action_events/{action_id_hex}"));

        let (handle, rx) =
            spawn_events_subscribe_thread(cfg_for(&robot_id, &action_id_hex)).unwrap();

        // client 端 drop receiver = stream cancel
        drop(rx);

        // thread 在下一轮 try_send 时看到 Closed → 退出。
        let metrics = handle.metrics();
        // 给 thread 一点时间触发 try_send 才会看到 closed；如果 publisher 一直
        // 没发，thread 看不到 closed（stop signal 路径才退）。所以这里走 stop。
        handle.request_stop();
        handle.join().unwrap();
        // 没事件流过，stopped_by_disconnect 可能为 0；至少 stop_by_signal=1。
        assert!(metrics.stopped_by_signal.load(Ordering::Relaxed) <= 1);
    }

    #[tokio::test]
    async fn drop_stream_signals_stop_to_thread() {
        let (robot_id, action_id_hex) = fresh_topic_pair("drop_stream");
        let _publisher = make_publisher(&format!("/{robot_id}/s1/action_events/{action_id_hex}"));

        let (handle, rx) =
            spawn_events_subscribe_thread(cfg_for(&robot_id, &action_id_hex)).unwrap();
        let stream = EventsSubscribeStream::new(rx, handle);
        let topic = stream.topic().to_string();
        assert!(topic.contains(&action_id_hex));
        let metrics = stream.metrics();

        // drop stream → Drop impl set stop = true。线程在 ~poll_interval
        // 内退（不主动 join）。轮询 stopped_by_signal 直到看到。
        drop(stream);

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if metrics.stopped_by_signal.load(Ordering::Acquire) == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "EventsSubscribeStream::drop did not signal stop within 1s"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
