//! `events_drain` 运行时层 —— 线程生命周期 / 主循环 / iceoryx2 publisher
//! I/O 调度。
//!
//! ## 角色
//!
//! 沿 [`crate::rr_server::runtime`] / A1 `crates/robot-s0/src/rr_server_thread.rs`
//! 同款 "I/O 线程" 模型。本模块**不**做 fbs encode/decode（→ [`super::wire`]），
//! **不**接 [`crate::coordinator::Coordinator`]（drain 是 read-only consumer）。
//!
//! 主循环：`recv_timeout(poll_interval)` → encode envelope → publish → 检测
//! terminal → 命中 → sleep `terminal_grace` + break。stop signal / disconnect
//! / terminal 三路退出互斥（详 [`EventsDrainMetrics::stopped_by_signal`] /
//! `stopped_by_disconnect` / `terminal_events`）。

#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use robot_core::time::RobotClock;
use robot_msg::markers;
use robot_transport::Publisher;
use robot_transport::iceoryx2::Iceoryx2Publisher;

use crate::action::{ActionEvent, ActionId};

use super::wire::{encode_envelope, is_terminal};

// ============================================================================
// Config / Metrics / Handle
// ============================================================================

/// `events_drain_thread` 配置。每个 [`crate::action::ActionTicket`] 一份
/// 独立 config（topic / `action_id` 都不同）。
#[derive(Clone, Debug)]
pub struct EventsDrainThreadConfig {
    /// `ActionId` —— 写入 `ActionEventEnvelope.action_id` 字段（冗余写到
    /// envelope，gateway 端 audit / debug **不**依赖 topic name 解析；详
    /// `crates/robot-msg/schemas/action_event.fbs`）。
    pub action_id: ActionId,
    /// iceoryx2 service topic 串。约定 `/<robot_id>/s1/action_events/<action_id_hex>`
    /// （per-ActionId 动态创建 / 销毁；详 outline §5.2）。仅作 doc /
    /// metrics / tracing label；实际 service create 由调用方传的
    /// `publisher_factory` closure 内部完成。
    pub topic: String,
    /// `events_rx.recv_timeout` 间隔（也是 stop signal 响应粒度）。默认
    /// 50 ms：足够低让 daemon shutdown 内 stop_and_join_all 不感受到延迟，
    /// 又比单事件 publish 路径开销大很多——drain thread 本身 idle 时
    /// CPU 占用可忽略。
    pub poll_interval: Duration,
    /// terminal event publish 后的 grace window（让 gateway sub 有时间
    /// 拉走最后一帧后再 drop publisher / unregister service）。outline
    /// §5.5 拍板默认 100 ms；具体行为 / iceoryx2 动态 service 路径留
    /// B4 (3/3) integration test 回填。
    pub terminal_grace: Duration,
}

impl EventsDrainThreadConfig {
    /// 用 `(robot_id, action_id)` 构造默认 config。
    ///
    /// `topic` = `/<robot_id>/s1/action_events/<action_id_hex>`，跟
    /// docs/handoffs/v0.3_b4_outline.md §5.2 拍板一致。
    #[must_use]
    pub fn for_action(robot_id: &str, action_id: ActionId) -> Self {
        Self {
            action_id,
            topic: format!("/{robot_id}/s1/action_events/{action_id}"),
            poll_interval: Duration::from_millis(50),
            terminal_grace: Duration::from_millis(100),
        }
    }
}

/// `events_drain_thread` 运行期 metrics（每 drain thread 一份独立计数）。
#[derive(Default, Debug)]
pub struct EventsDrainMetrics {
    /// 从 `events_rx` 收到的 `ActionEvent` 数（含未成功 publish 的）。
    pub events_received: AtomicU64,
    /// 成功 publish 到 iceoryx2 pubsub 的 envelope 数。
    pub events_published: AtomicU64,
    /// `publisher.loan()` / `writer.publish()` 失败次数。
    pub publish_errors: AtomicU64,
    /// 终态事件（Completed / Failed / Cancelled）数 —— 正常 ticket 应
    /// 最终命中 1 次（reviewer 拍板"绝不双发 Completed + Cancelled"）。
    pub terminal_events: AtomicU64,
    /// 因 stop signal 触发退出的次数（0 / 1）。
    pub stopped_by_signal: AtomicU64,
    /// 因 channel disconnected 退出的次数（0 / 1）。
    pub stopped_by_disconnect: AtomicU64,
}

/// `events_drain_thread` 句柄。caller 通过 [`Self::request_stop`] +
/// [`Self::join`] 关停。
pub struct EventsDrainThreadHandle {
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    metrics: Arc<EventsDrainMetrics>,
    action_id: ActionId,
    topic: String,
}

impl EventsDrainThreadHandle {
    /// 取本 drain thread 关联的 `ActionId`（diagnostic / registry lookup）。
    #[must_use]
    pub fn action_id(&self) -> ActionId {
        self.action_id
    }

    /// 取 topic 串（diagnostic / log）。
    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// 取 metrics `Arc`（线程内部写、外部读）。
    #[must_use]
    pub fn metrics(&self) -> Arc<EventsDrainMetrics> {
        Arc::clone(&self.metrics)
    }

    /// 通知主循环退出（下一轮 stop check 命中即跳出，**不**等终态事件 /
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

/// 管理 s1_daemon 内所有活跃 [`EventsDrainThreadHandle`] 的注册表。
///
/// `Coordinator::submit` 成功后 `InvokeTask` handler 调
/// [`spawn_events_drain_thread`] 起一条 drain thread，handle 通过
/// [`Self::register`] 进表；daemon 退出时调 [`Self::request_stop_all`]
/// + [`Self::join_all`] 一并关停所有。
///
/// **方法签名 by ref**（B4 (2/3-a) reviewer round 6 阻断锁定）：
/// `request_stop_all` / `join_all` / `stop_and_join_all` 都接 `&self` —— 内
/// 部 `Mutex<Vec<Handle>>` 通过 `mem::take` 拿 owned `Vec<Handle>` 后释放
/// 锁、再逐个 join。`s1_daemon` 把 registry 包 `Arc<EventsDrainRegistry>`
/// 跟 [`crate::rr_server::handler::RealHandlerCtx`] 共享，shutdown 路径**不**
/// 依赖 `Arc::try_unwrap`（避免 RR thread 残留 closure ref-leak 时打破 join
/// 确定性）。
///
/// **并发语义**：`register` / `request_stop_all` / `join_all` /
/// [`Self::reap_finished`] 都靠同一把 `Mutex` 序列化；`join_all` 内
/// `mem::take` 在锁内完成 + 锁释放 + 锁外逐个 join，避免持锁阻塞；
/// `reap_finished` 内锁内 `swap_remove` 已 `is_finished()` handle 进
/// owned `Vec` + 锁释放 + 锁外 join，同款"锁内 owned + 锁外 join" 模式。
///
/// **典型 shutdown 顺序**（详 [`crate::rr_server`] 顶部 module doc）：
/// `coordinator.mark_shutting_down → rr.request_stop_all → rr.join_all →
/// events_registry.stop_and_join_all`。重复调 `stop_and_join_all` 是 no-op
/// （`mem::take` 返空 `Vec`）。
///
/// **v0.4 候选 3 AC-C1 daemon path 扩展**：`s1_daemon` 在 `rr.join_all`
/// 和 `events_registry.stop_and_join_all` 之间额外停 + join 1s 周期的
/// reaper thread（reaper 调 [`Self::reap_finished`] 回收已自然退出的
/// drain handle）。registry **API 语义不变** —— `reap_finished` 跟
/// `stop_and_join_all` 互斥靠同一把 `Mutex`；daemon shutdown 顺序加
/// reaper.stop 步骤仅是为了让 reaper 在 `stop_and_join_all` 之前彻底
/// 停下，让 shutdown 路径串行化（详 `crates/robot-s1/src/bin/s1_daemon.rs`
/// shutdown 段）。
#[derive(Default)]
pub struct EventsDrainRegistry {
    inner: Mutex<Vec<EventsDrainThreadHandle>>,
}

impl core::fmt::Debug for EventsDrainRegistry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let len = self.inner.lock().map_or(0, |v| v.len());
        f.debug_struct("EventsDrainRegistry")
            .field("active_handles", &len)
            .finish()
    }
}

impl EventsDrainRegistry {
    /// 起空 registry。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一条 drain thread。
    ///
    /// **panic** 仅当内部 `Mutex` poisoned（线程曾在持锁时 panic；理论不
    /// 应发生 —— 本 registry 的所有 mutate 路径都不 panic）。
    pub fn register(&self, handle: EventsDrainThreadHandle) {
        self.inner
            .lock()
            .expect("EventsDrainRegistry mutex poisoned")
            .push(handle);
    }

    /// 给所有 in-flight drain thread 发停止信号（不等 join；**不**消费
    /// `self`）。
    pub fn request_stop_all(&self) {
        let guard = self
            .inner
            .lock()
            .expect("EventsDrainRegistry mutex poisoned");
        for h in guard.iter() {
            h.request_stop();
        }
    }

    /// 阻塞 join 所有 in-flight drain thread；接 `&self`（**不**消费 self），
    /// daemon 可在 `Arc<EventsDrainRegistry>` 上直接调，**不**需要
    /// `Arc::try_unwrap`。
    ///
    /// **行为**：内部 `Mutex` 锁 → `mem::take` 拿 owned `Vec<Handle>` →
    /// 释放锁 → 锁外逐个 join。**不**短路：即使某条 join 返 panic 也继续
    /// join 剩下的，最后才返第一个 `Err`。重复调 = no-op（`mem::take` 返
    /// 空 `Vec`）。
    ///
    /// # Errors
    ///
    /// 任一线程内部 panic → `Err(Box<dyn Any + Send>)`（首个 panic 优先）。
    pub fn join_all(&self) -> std::thread::Result<()> {
        let handles = self
            .inner
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        let mut first_err: Option<Box<dyn std::any::Any + Send>> = None;
        for h in handles {
            if let Err(e) = h.join()
                && first_err.is_none()
            {
                first_err = Some(e);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// 一键 stop + join（典型 daemon shutdown 路径调本方法即可）；接
    /// `&self`，daemon 可在 `Arc<EventsDrainRegistry>` 上直接调。
    ///
    /// # Errors
    ///
    /// 同 [`Self::join_all`]。
    pub fn stop_and_join_all(&self) -> std::thread::Result<()> {
        self.request_stop_all();
        self.join_all()
    }

    /// v0.4 候选 3 AC-C1：周期性 reap 已结束的 drain thread handle。
    ///
    /// 锁内用 [`std::thread::JoinHandle::is_finished`] 扫
    /// `Mutex<Vec<Handle>>`，`swap_remove` 已退出 thread 的 handle 进
    /// owned `Vec`；锁外逐个 `join()` 回收 OS thread 资源 + drop
    /// [`EventsDrainThreadHandle`] 内的 `Arc<AtomicBool>` /
    /// `Arc<EventsDrainMetrics>` / `JoinHandle` / `topic` String /
    /// `action_id`。
    ///
    /// 返回值：本次从 registry 移除并 join 尝试的 handle 数。**含
    /// drain thread panic 路径**（沿 `join_all` 同款，panic 也 counted
    /// as reaped；详下方 "panic 处理" 段）；不区分 join Ok / Err。caller
    /// (如 `s1_daemon` reaper thread) 可用此值打 info log 让 smoke
    /// binary 通过 stderr scrape 看 `events_registry_len` 收敛曲线
    /// （AC-C1 outline §3 reaped > 0 必须 log）。
    ///
    /// ## 跟 [`Self::stop_and_join_all`] 协作语义
    ///
    /// `reap_finished` 仅处理 `is_finished() == true` 的 handle（drain
    /// thread 已自然退出：terminal+grace / disconnect / stop signal 三
    /// 路退出之一）；还活着的 drain thread **不**动。
    ///
    /// `stop_and_join_all` 后续接管 reap 后剩余的全部活 handle
    /// （`mem::take` 整个 `Vec` → 锁外逐个 stop+join）。reap 后 Vec
    /// 仅含活 handle，`stop_and_join_all` 行为跟 reap 前后完全等价；
    /// 重复调任意组合 = no-op（空 `Vec` 路径现有契约保不变）。
    ///
    /// ## panic 处理（AC-C1 outline §10 #1 拍板）
    ///
    /// 沿 [`Self::join_all`] 同款策略：drain thread 内 panic →
    /// `JoinHandle::join` 返 `Err(Box<dyn Any + Send>)`；本路径**不**
    /// 向调用方传播 panic（接 `&self` + 返 `usize` 是 diagnostic 路径，
    /// caller 不需要处理 panic），但 `tracing::warn!` 显式记录 panic
    /// payload 让 forensic 能看见。返回值仍 += 1（panic 也算"已退出"
    /// = 已 reap）。
    #[must_use]
    pub fn reap_finished(&self) -> usize {
        // step 1: 锁内 swap_remove 已 is_finished() 的 handle 进 owned Vec
        let to_join: Vec<EventsDrainThreadHandle> = {
            let mut guard = self
                .inner
                .lock()
                .expect("EventsDrainRegistry mutex poisoned");
            let mut taken = Vec::new();
            let mut i = 0;
            while i < guard.len() {
                if guard[i].join.is_finished() {
                    taken.push(guard.swap_remove(i));
                    // swap_remove 不递增 i；新 i 位置是原 last 元素，
                    // 下一轮重新判定。
                } else {
                    i += 1;
                }
            }
            taken
        };

        // step 2: 锁外逐个 join（`is_finished() == true` 时 join 几乎
        // 立即返；但写共享状态的 fence 仍可能 wait 几 ns）。panic 记录
        // 不传播。
        let n = to_join.len();
        for handle in to_join {
            let action_id = handle.action_id;
            let topic = handle.topic.clone();
            if let Err(payload) = handle.join() {
                tracing::warn!(
                    action_id = %action_id,
                    topic = %topic,
                    ?payload,
                    "EventsDrainRegistry::reap_finished: drain thread panicked; \
                     panic swallowed but counted as reaped"
                );
            }
        }
        n
    }

    /// 当前已注册的 in-flight handle 数（diagnostic）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map_or(0, |g| g.len())
    }

    /// 当前 registry 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ============================================================================
// spawn API
// ============================================================================

/// 起一条 `events_drain_thread`。
///
/// `publisher_factory` closure 在新线程内执行（iceoryx2 publisher `!Send`，
/// 必须在 owner 线程内构造；同 [`crate::rr_server::runtime`] / A1
/// `crates/robot-s0/src/rr_server_thread.rs::spawn_rr_server_thread`
/// factory 模式）。
///
/// `events_rx` 一般来自 [`crate::action::ActionTicket::events`]。skeleton
/// / 单元测试可以用 `std::sync::mpsc::channel()` 直接构造 sender / receiver
/// pair 模拟事件流。
///
/// `clock` 用于设置 `FrameHeader.stamp_ns`（wire publish 时刻；跟事件**语
/// 义**时刻 `at_ns` 区分；详模块文档"时间戳来源"段）。
///
/// `source_id` 写到 `FrameHeader.source_id`。s1_daemon 一律传 `2`（与
/// v0.2 `JointCommand` / `Heartbeat` 约定一致）。
///
/// # Errors
///
/// - `publisher_factory` panic（factory 起 iceoryx2 service 失败 / port
///   耗尽等）。
/// - `thread::Builder::spawn` 失败（极罕见）。
pub fn spawn_events_drain_thread<F>(
    cfg: EventsDrainThreadConfig,
    events_rx: Receiver<ActionEvent>,
    publisher_factory: F,
    clock: Arc<dyn RobotClock>,
    source_id: u32,
) -> std::io::Result<EventsDrainThreadHandle>
where
    F: FnOnce() -> Iceoryx2Publisher<markers::ActionEventEnvelope> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let metrics = Arc::new(EventsDrainMetrics::default());
    let stop_clone = Arc::clone(&stop);
    let metrics_clone = Arc::clone(&metrics);
    let action_id = cfg.action_id;
    let topic_owned = cfg.topic.clone();

    // ready signal（同 rr_server_thread 模式）：spawn 返前确认
    // publisher_factory 成功执行（factory panic / iceoryx2 错都在这里捕
    // 获）。
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

    let join = thread::Builder::new()
        .name(format!("s1_events_drain_{action_id}"))
        .spawn(move || {
            let mut publisher = publisher_factory();
            let _ = ready_tx.send(());
            drop(ready_tx);
            run_drain_loop(
                &cfg,
                &mut publisher,
                &events_rx,
                clock.as_ref(),
                source_id,
                &stop_clone,
                &metrics_clone,
            );
        })?;

    if ready_rx.recv().is_err() {
        return Err(std::io::Error::other(format!(
            "events_drain_thread {action_id}: publisher_factory panicked before signalling ready"
        )));
    }

    Ok(EventsDrainThreadHandle {
        join,
        stop,
        metrics,
        action_id,
        topic: topic_owned,
    })
}

// ============================================================================
// 主循环
// ============================================================================

fn run_drain_loop(
    cfg: &EventsDrainThreadConfig,
    publisher: &mut Iceoryx2Publisher<markers::ActionEventEnvelope>,
    events_rx: &Receiver<ActionEvent>,
    clock: &dyn RobotClock,
    source_id: u32,
    stop: &AtomicBool,
    metrics: &EventsDrainMetrics,
) {
    let action_id_hex = cfg.action_id.to_string();
    let mut seq: u64 = 0;
    let mut terminal_seen = false;

    while !stop.load(Ordering::Acquire) {
        match events_rx.recv_timeout(cfg.poll_interval) {
            Ok(event) => {
                metrics.events_received.fetch_add(1, Ordering::Relaxed);
                let terminal = is_terminal(&event);
                if let Err(e) = publish_event(
                    publisher,
                    &action_id_hex,
                    &event,
                    seq,
                    source_id,
                    clock.now().0,
                ) {
                    metrics.publish_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        action_id = %action_id_hex,
                        topic = %cfg.topic,
                        error = ?e,
                        "events_drain_thread: publish failed; dropping event"
                    );
                } else {
                    metrics.events_published.fetch_add(1, Ordering::Relaxed);
                    seq = seq.wrapping_add(1);
                }
                if terminal {
                    metrics.terminal_events.fetch_add(1, Ordering::Relaxed);
                    terminal_seen = true;
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                metrics.stopped_by_disconnect.store(1, Ordering::Release);
                tracing::debug!(
                    action_id = %action_id_hex,
                    topic = %cfg.topic,
                    "events_drain_thread: events channel disconnected; exiting"
                );
                return; // **不**等 grace（disconnect 不是 terminal event 的等价）
            }
        }
    }

    if stop.load(Ordering::Acquire) && !terminal_seen {
        metrics.stopped_by_signal.store(1, Ordering::Release);
        tracing::debug!(
            action_id = %action_id_hex,
            topic = %cfg.topic,
            "events_drain_thread: stopped by signal before terminal event"
        );
        return; // **不**等 grace（外部 stop 不发 terminal）
    }

    // terminal event 已发：sleep grace 让 gateway sub catch up，然后 publisher /
    // node 在本函数返回时 drop（caller spawn closure 拥有它们）。
    if terminal_seen {
        thread::sleep(cfg.terminal_grace);
    }
}

fn publish_event(
    publisher: &mut Iceoryx2Publisher<markers::ActionEventEnvelope>,
    action_id_hex: &str,
    event: &ActionEvent,
    seq: u64,
    source_id: u32,
    stamp_ns: u64,
) -> Result<(), robot_transport::TransportError> {
    let body = encode_envelope(action_id_hex, event);
    // 1024 byte 余量给 FrameHeader（128 byte）+ blob padding；envelope body
    // < 256 byte，1024 + body_len 是宽松上限（与 `coordinator::publish_body`
    // 同款 sizing）。
    let mut writer = publisher.loan(body.len() + 1024)?;
    writer.set_seq(seq);
    writer.set_source_id(source_id);
    writer.set_stamp_ns(stamp_ns);
    writer.set_body_bytes(&body);
    writer.publish()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    //! Round-trip lib unit tests：用独立 Node 起 publisher（drain thread 内
    //! 部）+ subscriber（test 主线程），通过 `std::sync::mpsc` 推 5 个事件
    //! variant，验 envelope 字段值 + lifecycle (terminal grace / stop signal
    //! / disconnect 三条退出路径) + registry stop_and_join_all 行为。
    //!
    //! 跨进程 integration test 留 B4 (3/3) commit（
    //! `crates/robot-rpc/tests/...`，沿 A1-3 (3/3) 跨进程 spawn s1_daemon
    //! binary 同款模式）。

    use super::*;
    use crate::action::{ActionFailure, ActionOutcome};
    use iceoryx2::prelude::NodeBuilder;
    use iceoryx2::service::ipc::Service as IxService;
    use robot_core::time::SystemClock;
    use robot_msg::msg::{ActionEventEnvelope as FbEnv, ActionEventKindFb as FbKind};
    use robot_transport::iceoryx2::{
        Iceoryx2Subscriber, TopicClass, publisher as open_pub, subscriber as open_sub,
    };
    use robot_transport::traits::LatestSubscriber;
    use std::sync::mpsc;

    fn fresh_topic(suffix: &str) -> String {
        use std::sync::atomic::AtomicU32;
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        format!("/s1_evt_drain_{}_{}_{suffix}", std::process::id(), seq)
    }

    /// 构造测试主线程的 subscriber（独立 Node + leak），跟 drain thread 内
    /// 独立 Node + leak 的 publisher 通过 iceoryx2 service name 串通。沿
    /// `rr_server_thread` 测试同款"两端独立 Node"模式。
    fn make_subscriber(topic: &str) -> Iceoryx2Subscriber<markers::ActionEventEnvelope> {
        let test_node: &'static _ =
            Box::leak(Box::new(NodeBuilder::new().create::<IxService>().unwrap()));
        open_sub::<markers::ActionEventEnvelope>(test_node, topic, TopicClass::TaskEvent)
            .expect("subscriber open")
    }

    fn make_publisher_factory(
        topic: String,
    ) -> impl FnOnce() -> Iceoryx2Publisher<markers::ActionEventEnvelope> + Send + 'static {
        move || {
            let pub_node = NodeBuilder::new().create::<IxService>().unwrap();
            let publisher =
                open_pub::<markers::ActionEventEnvelope>(&pub_node, &topic, TopicClass::TaskEvent)
                    .expect("publisher open");
            // node 持 publisher 内部 shm fd；leak 让 publisher 整个生命周期
            // 内 node 不被 drop（同 A1 s0_daemon / 1/3-b s1_daemon 同款）。
            Box::leak(Box::new(pub_node));
            publisher
        }
    }

    fn fresh_action_id() -> ActionId {
        ActionId::new_v7()
    }

    fn cfg_for(action_id: ActionId, topic: &str) -> EventsDrainThreadConfig {
        EventsDrainThreadConfig {
            action_id,
            topic: topic.to_owned(),
            poll_interval: Duration::from_millis(10),
            terminal_grace: Duration::from_millis(20),
        }
    }

    /// 一次 lease 同时读 envelope body bytes + `FrameHeader.seq`。lease drop
    /// 后内部 bytes 失效，所以两个值在同一 lease 内拷出来。
    fn wait_for_envelope(
        sub: &mut Iceoryx2Subscriber<markers::ActionEventEnvelope>,
        timeout: Duration,
    ) -> (Vec<u8>, u64) {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(lease) = sub.latest().expect("subscriber latest") {
                let view = lease.view().expect("typed view");
                let env: FbEnv<'_> = view.body.get();
                let seq = view.header.get().seq();
                let mut copy = Vec::new();
                copy.extend_from_slice(env._tab.buf());
                return (copy, seq);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "wait_for_envelope timed out after {timeout:?}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn started_event_round_trip() {
        let topic = fresh_topic("started");
        let mut sub = make_subscriber(&topic);
        let (events_tx, events_rx) = mpsc::channel::<ActionEvent>();
        let action_id = fresh_action_id();
        let action_id_hex = action_id.to_string();
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            events_rx,
            make_publisher_factory(topic.clone()),
            Arc::new(SystemClock::new()),
            2,
        )
        .unwrap();

        events_tx
            .send(ActionEvent::Started { at_ns: 12345 })
            .unwrap();
        let (bytes, _seq) = wait_for_envelope(&mut sub, Duration::from_secs(2));
        let env = flatbuffers::root::<FbEnv>(&bytes).unwrap();
        assert_eq!(env.action_id().unwrap(), action_id_hex);
        assert_eq!(env.kind(), FbKind::Started);
        assert_eq!(env.at_ns(), 12345);

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn progress_event_round_trip() {
        let topic = fresh_topic("progress");
        let mut sub = make_subscriber(&topic);
        let (events_tx, events_rx) = mpsc::channel::<ActionEvent>();
        let action_id = fresh_action_id();
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            events_rx,
            make_publisher_factory(topic.clone()),
            Arc::new(SystemClock::new()),
            2,
        )
        .unwrap();

        events_tx
            .send(ActionEvent::Progress {
                progress: 0.42,
                at_ns: 1,
            })
            .unwrap();
        let (bytes, _) = wait_for_envelope(&mut sub, Duration::from_secs(2));
        let env = flatbuffers::root::<FbEnv>(&bytes).unwrap();
        assert_eq!(env.kind(), FbKind::Progress);
        assert!((env.progress() - 0.42).abs() < f32::EPSILON);

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn completed_terminal_event_breaks_loop_after_grace() {
        let topic = fresh_topic("completed");
        let mut sub = make_subscriber(&topic);
        let (events_tx, events_rx) = mpsc::channel::<ActionEvent>();
        let action_id = fresh_action_id();
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            events_rx,
            make_publisher_factory(topic.clone()),
            Arc::new(SystemClock::new()),
            2,
        )
        .unwrap();

        events_tx
            .send(ActionEvent::Completed {
                outcome: ActionOutcome::Success,
                at_ns: 1,
            })
            .unwrap();
        let (bytes, _) = wait_for_envelope(&mut sub, Duration::from_secs(2));
        let env = flatbuffers::root::<FbEnv>(&bytes).unwrap();
        assert_eq!(env.kind(), FbKind::Completed);

        // terminal_seen → break 后 sleep grace（20ms）；join 在 ~30ms 内返。
        let metrics = handle.metrics();
        handle.join().unwrap();
        assert_eq!(metrics.terminal_events.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.stopped_by_signal.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.stopped_by_disconnect.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn failed_terminal_event_carries_failure_reason() {
        let topic = fresh_topic("failed");
        let mut sub = make_subscriber(&topic);
        let (events_tx, events_rx) = mpsc::channel::<ActionEvent>();
        let action_id = fresh_action_id();
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            events_rx,
            make_publisher_factory(topic.clone()),
            Arc::new(SystemClock::new()),
            2,
        )
        .unwrap();

        events_tx
            .send(ActionEvent::Failed {
                failure: ActionFailure::SafetyAbort("rt safety".into()),
                at_ns: 1,
            })
            .unwrap();
        let (bytes, _) = wait_for_envelope(&mut sub, Duration::from_secs(2));
        let env = flatbuffers::root::<FbEnv>(&bytes).unwrap();
        assert_eq!(env.kind(), FbKind::Failed);
        let failure = env.failure().expect("failure present");
        assert_eq!(failure.reason().unwrap(), "rt safety");

        handle.join().unwrap();
    }

    #[test]
    fn cancelled_terminal_event_round_trip() {
        let topic = fresh_topic("cancelled");
        let mut sub = make_subscriber(&topic);
        let (events_tx, events_rx) = mpsc::channel::<ActionEvent>();
        let action_id = fresh_action_id();
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            events_rx,
            make_publisher_factory(topic.clone()),
            Arc::new(SystemClock::new()),
            2,
        )
        .unwrap();

        events_tx.send(ActionEvent::Cancelled { at_ns: 1 }).unwrap();
        let (bytes, _) = wait_for_envelope(&mut sub, Duration::from_secs(2));
        let env = flatbuffers::root::<FbEnv>(&bytes).unwrap();
        assert_eq!(env.kind(), FbKind::Cancelled);

        handle.join().unwrap();
    }

    #[test]
    fn stop_signal_exits_loop_without_terminal() {
        let topic = fresh_topic("stop_sig");
        let _sub = make_subscriber(&topic);
        let (_events_tx, events_rx) = mpsc::channel::<ActionEvent>();
        let action_id = fresh_action_id();
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            events_rx,
            make_publisher_factory(topic.clone()),
            Arc::new(SystemClock::new()),
            2,
        )
        .unwrap();

        let metrics = handle.metrics();
        let start = std::time::Instant::now();
        handle.request_stop();
        handle.join().unwrap();
        assert!(start.elapsed() < Duration::from_millis(500));
        assert_eq!(metrics.stopped_by_signal.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.terminal_events.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn channel_disconnect_exits_loop_without_terminal() {
        let topic = fresh_topic("disconnect");
        let _sub = make_subscriber(&topic);
        let (events_tx, events_rx) = mpsc::channel::<ActionEvent>();
        let action_id = fresh_action_id();
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            events_rx,
            make_publisher_factory(topic.clone()),
            Arc::new(SystemClock::new()),
            2,
        )
        .unwrap();

        let metrics = handle.metrics();
        drop(events_tx);
        handle.join().unwrap();
        assert_eq!(metrics.stopped_by_disconnect.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.stopped_by_signal.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn frame_header_seq_monotonic_per_topic() {
        let topic = fresh_topic("seq_mono");
        let mut sub = make_subscriber(&topic);
        let (events_tx, events_rx) = mpsc::channel::<ActionEvent>();
        let action_id = fresh_action_id();
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            events_rx,
            make_publisher_factory(topic.clone()),
            Arc::new(SystemClock::new()),
            2,
        )
        .unwrap();

        // 串行 send-recv 配对：每次 wait_for_envelope 拿一帧 + seq；
        // iceoryx2 keep-last 只保留最新 sample，必须读完 sample 才 send 下
        // 一帧。
        events_tx.send(ActionEvent::Started { at_ns: 1 }).unwrap();
        let (_b, seq0) = wait_for_envelope(&mut sub, Duration::from_secs(2));
        events_tx
            .send(ActionEvent::Progress {
                progress: 0.5,
                at_ns: 2,
            })
            .unwrap();
        let (_b, seq1) = wait_for_envelope(&mut sub, Duration::from_secs(2));

        assert_eq!(seq0, 0);
        assert_eq!(seq1, 1);

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn registry_stop_and_join_all_handles_empty() {
        let registry = EventsDrainRegistry::new();
        assert!(registry.is_empty());
        registry.stop_and_join_all().unwrap();
        // no-op；可重复调
        registry.stop_and_join_all().unwrap();
    }

    #[test]
    fn registry_stop_and_join_all_one_thread() {
        let topic = fresh_topic("registry");
        let _sub = make_subscriber(&topic);
        let (_events_tx, events_rx) = mpsc::channel::<ActionEvent>();
        let action_id = fresh_action_id();
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            events_rx,
            make_publisher_factory(topic.clone()),
            Arc::new(SystemClock::new()),
            2,
        )
        .unwrap();

        let registry = EventsDrainRegistry::new();
        registry.register(handle);
        assert_eq!(registry.len(), 1);
        let start = std::time::Instant::now();
        registry.stop_and_join_all().unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "stop_and_join_all took {elapsed:?}, expected < 500ms"
        );
        // 重复调 = no-op（mem::take 后 Vec 已空）。
        registry.stop_and_join_all().unwrap();
    }

    // ========================================================================
    // v0.4 候选 3 AC-C1 `reap_finished` 锁测（AC-C1 outline §5 拍板 4 测）
    // ========================================================================

    /// AC-C1 outline §5：空 registry reap 返 0 + len 不变；重复调 no-op。
    #[test]
    fn reap_finished_empty_returns_zero() {
        let registry = EventsDrainRegistry::new();
        assert_eq!(registry.reap_finished(), 0);
        assert_eq!(registry.len(), 0);
        // 重复调 no-op
        assert_eq!(registry.reap_finished(), 0);
    }

    /// AC-C1 outline §5：drain thread 终态退出后 reap 返 1 + len 归零。
    #[test]
    fn reap_finished_after_terminal_returns_one() {
        let topic = fresh_topic("reap_terminal");
        let _sub = make_subscriber(&topic);
        let (events_tx, events_rx) = mpsc::channel::<ActionEvent>();
        let action_id = fresh_action_id();
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            events_rx,
            make_publisher_factory(topic.clone()),
            Arc::new(SystemClock::new()),
            2,
        )
        .unwrap();
        let registry = EventsDrainRegistry::new();
        registry.register(handle);
        assert_eq!(registry.len(), 1);

        events_tx
            .send(ActionEvent::Completed {
                outcome: ActionOutcome::Success,
                at_ns: 1,
            })
            .unwrap();
        // 等 thread 真退出：terminal_grace 20ms (test cfg) + 安全 margin。
        // 用循环轮询 is_finished 提早收敛（避免固定 sleep 不稳定）。
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if registry.reap_finished() == 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "drain thread did not finish within 500ms"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(registry.len(), 0);
    }

    /// AC-C1 outline §5 (reviewer round 1 §10 #4 测试名硬约束)：
    /// active drain thread (idle, 没发任何事件) **不**被 reap；handle
    /// 仍在 registry。
    #[test]
    fn reap_finished_active_handle_not_reaped() {
        let topic = fresh_topic("reap_active_not_reaped");
        let _sub = make_subscriber(&topic);
        let (_events_tx, events_rx) = mpsc::channel::<ActionEvent>();
        let action_id = fresh_action_id();
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            events_rx,
            make_publisher_factory(topic.clone()),
            Arc::new(SystemClock::new()),
            2,
        )
        .unwrap();
        let registry = EventsDrainRegistry::new();
        registry.register(handle);

        // drain thread idle (没发任何事件 + 没发 stop signal) →
        // is_finished() = false → reap 返 0；handle 仍在。
        assert_eq!(registry.reap_finished(), 0);
        assert_eq!(registry.len(), 1, "active handle MUST stay in registry");

        // cleanup: stop_and_join_all 走正常关停路径。
        registry.stop_and_join_all().unwrap();
        assert_eq!(registry.len(), 0);
    }

    /// AC-C1 outline §5：reap 已退出 handle 后，`stop_and_join_all` 对
    /// 已 reap 的部分**no-op**（reap 已 owned 走 join，stop_and_join_all
    /// 处理剩余活 handle）。
    #[test]
    #[allow(clippy::similar_names)] // `events_tx_a` / `events_rx_a` 跟
    // `events_tx_b` / `events_rx_b` 是两 channel pair；测试可读性优先
    // 用 `_a` / `_b` 后缀；clippy similar_names 在测试代码是噪音。
    fn reap_finished_then_stop_and_join_all_is_noop_for_reaped() {
        let topic_a = fresh_topic("reap_noop_a");
        let topic_b = fresh_topic("reap_noop_b");
        let _sub_a = make_subscriber(&topic_a);
        let _sub_b = make_subscriber(&topic_b);
        let (events_tx_a, events_rx_a) = mpsc::channel::<ActionEvent>();
        let (_events_tx_b, events_rx_b) = mpsc::channel::<ActionEvent>();
        let action_id_a = fresh_action_id();
        let action_id_b = fresh_action_id();
        let handle_a = spawn_events_drain_thread(
            cfg_for(action_id_a, &topic_a),
            events_rx_a,
            make_publisher_factory(topic_a.clone()),
            Arc::new(SystemClock::new()),
            2,
        )
        .unwrap();
        let handle_b = spawn_events_drain_thread(
            cfg_for(action_id_b, &topic_b),
            events_rx_b,
            make_publisher_factory(topic_b.clone()),
            Arc::new(SystemClock::new()),
            2,
        )
        .unwrap();

        let registry = EventsDrainRegistry::new();
        registry.register(handle_a);
        registry.register(handle_b);
        assert_eq!(registry.len(), 2);

        // 终态 A（保留 B active）+ 等 thread 真退出。
        events_tx_a
            .send(ActionEvent::Completed {
                outcome: ActionOutcome::Success,
                at_ns: 1,
            })
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if registry.reap_finished() == 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "handle_a did not finish within 500ms"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(registry.len(), 1, "only B should remain after reap");

        // stop_and_join_all 接管 B；不重复处理 A（A 已 reap，不在
        // inner Vec 内）。
        let start = std::time::Instant::now();
        registry.stop_and_join_all().unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "stop_and_join_all after reap took {:?}, expected < 500ms",
            start.elapsed()
        );
        assert_eq!(registry.len(), 0);

        // 重复 reap / stop_and_join_all 全 no-op。
        assert_eq!(registry.reap_finished(), 0);
        registry.stop_and_join_all().unwrap();
    }
}
