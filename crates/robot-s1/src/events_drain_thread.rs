//! `events_drain_thread` —— v0.3 B4 (1/3-c) S1 action event drain pubsub
//! skeleton。
//!
//! ## 角色
//!
//! [`crate::action::ActionTicket::events`] 是 `Coordinator::submit` 成功后
//! 返给调用方的 `std::sync::mpsc::Receiver<ActionEvent>`（in-process
//! sync channel）。`StreamTaskEvents` RPC 跨进程拓扑（详
//! [`docs/handoffs/v0.3_b4_outline.md`](../../../docs/handoffs/v0.3_b4_outline.md)
//! §5.2 reviewer round 2 拍板）：
//!
//! ```text
//! s1_daemon process                        gateway process (B4 (2/3))
//! ─────────────────                        ───────────────────────────
//!
//! Coordinator::submit
//!   → CurrentAction.events_tx
//!
//!   ↓ ActionEvent (in-process mpsc)
//!
//! events_drain_thread (本模块, 1/3-c)        events_subscribe_thread
//!   - 持 Receiver<ActionEvent>                - 订阅 iceoryx2 pubsub:
//!   - loop recv() / recv_timeout              /<robot>/s1/action_events/<action_id>
//!   - encode action_event.fbs body           - poll latest sample
//!   - publish iceoryx2 pubsub                  ↓ ActionEvent (decoded)
//!     /<robot>/s1/action_events/<action_id>
//!                                            adapter_to_async_thread
//!                                              - tokio::sync::mpsc::Sender
//!                                              - tonic stream forwards
//! ```
//!
//! ## B4 (1/3-c) 边界
//!
//! 本 commit 仅落 lib API + 单元测试 round-trip：spawn → 推 5 个事件
//! variant → subscriber decode → 验 envelope 字段。**不**:
//!
//! - **不**接 [`crate::coordinator::Coordinator`] 真路径：`events_rx` 由
//!   调用方传（B4 (2/3) 真 wiring 落地时由 `InvokeTask` handler 把
//!   `Coordinator::submit` 返回的 `ActionTicket.events` 移交进 drain
//!   thread）。
//! - **不**改 coordinator 状态机决策（drain thread 是 read-only consumer）。
//! - **不**碰 `JointCommand.source_task_id` / `FrameHeader.seq`（drain
//!   thread 走独立 per-ActionId 的 pubsub topic，跟 JointCommand frame
//!   完全分离；本 thread 自己的 `FrameHeader.seq` 是本 topic 内单调递
//!   增计数，跟 wire 协议契约 §3.7 无关）。
//! - **不**起 gateway async adapter / tokio runtime（与
//!   `crates/robot-s1/src/lib.rs` 顶部"不链接 tokio / tonic"边界对齐；
//!   gateway sub thread + sync→async 桥接落 B4 (2/3)）。
//! - **不**做 100ms `terminal_grace` 实测：[`EventsDrainThreadConfig::terminal_grace`]
//!   声明 default = 100 ms（沿 outline §5.5 拍板），但具体行为 / iceoryx2
//!   动态 service unregister 路径 + leftover topic 清理留 B4 (3/3)
//!   integration test 实测后回填（详 §5.6 待拍板 #4b）。
//!
//! ## 时间戳来源（reviewer 1/3-c 提醒明确）
//!
//! - `ActionEventEnvelope.at_ns` = 来自 `ActionEvent::*::at_ns`（事件**语
//!   义**时刻；coordinator 端写入；wire 透传）。
//! - `FrameHeader.stamp_ns` = drain thread `clock.now()`（wire 帧**publish**
//!   时刻；用于 wire 层延迟 / 漏帧统计）。
//! - `FrameHeader.seq` = 本 drain thread 维护的单调递增计数（per-topic，
//!   从 0 起）；跟 `JointCommand.source_task_id` 是完全独立的字段（不同
//!   topic、不同 publisher）。
//! - `FrameHeader.source_id` = 调用方传入；s1_daemon 用 `2`（v0.2 `JointCommand`
//!   / `Heartbeat` 同款 source_id 约定）。
//!
//! ## 终止条件
//!
//! 1. **stop signal**（外部 [`EventsDrainThreadHandle::request_stop`] /
//!    daemon shutdown）→ 主循环下一轮 stop check 命中 → break。
//!    **不**等 channel close、**不**等 grace。
//! 2. **terminal event**（Completed / Failed / Cancelled）→ publish 完
//!    本帧 → sleep `terminal_grace` 让 gateway sub catch up → break。
//! 3. **channel disconnected**（`Coordinator::CurrentAction.events_tx`
//!    drop = ticket lifecycle 自然终止）→ break，**不**等 grace。
//!
//! 跟 `rr_server_thread` 同款：通过 `Arc<AtomicBool>` stop signal +
//! `recv_timeout` 让线程"统一 stop/join"——daemon 主循环退出时调
//! [`EventsDrainThreadHandle::request_stop`] + [`EventsDrainThreadHandle::join`]
//! 即可关停（[`EventsDrainRegistry::stop_and_join_all`] 一键关全场）。
//!
//! ## 真 handler landing 时的 evolution（B4 (2/3) 接班 reference）
//!
//! - `InvokeTask` handler 成功调用 `Coordinator::submit` 后取
//!   `ActionTicket { action_id, events }`，调
//!   [`spawn_events_drain_thread`] 起一条 drain thread（factory 内建
//!   per-ActionId topic name + iceoryx2 publisher），把 handle 注册到
//!   [`EventsDrainRegistry`]。
//! - `s1_daemon` 退出时调 [`EventsDrainRegistry::request_stop_all`] +
//!   [`EventsDrainRegistry::join_all`] 一并关停。
//! - 若 ticket 已自然终止（terminal event drain → thread exit），handle
//!   仍留在 registry 直到 daemon shutdown；可选：B4 (2/3) 加
//!   `purge_finished()` 周期清理（B4 (3/3) impl 期评估必要性）。

#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use robot_core::time::RobotClock;
use robot_msg::build_helpers::build_action_event_envelope;
use robot_msg::markers;
use robot_msg::msg::{ActionEventKindFb, ActionFailureKindFb, ActionOutcomeKindFb};
use robot_transport::Publisher;
use robot_transport::iceoryx2::Iceoryx2Publisher;

use crate::action::{ActionEvent, ActionFailure, ActionId, ActionOutcome};

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
/// 确定性，把"未 join 线程悬挂到 process exit"当 fallback —— 本 daemon
/// 可能跑在测试 / 嵌入式 runner 里 `run` 返回但不立刻 process exit）。
///
/// **并发语义**：`register` / `request_stop_all` / `join_all` 都靠同一把
/// `Mutex` 序列化；`join_all` 内 `mem::take` 在锁内完成 + 锁释放 + 锁外逐个
/// join，避免持锁阻塞（join 阻塞时间 ≈ drain thread `terminal_grace`）。
///
/// **典型 shutdown 顺序**（详 [`crate::rr_server`] 顶部 module doc）：
/// `coordinator.mark_shutting_down → rr.request_stop_all → rr.join_all →
/// events_registry.stop_and_join_all`。RR thread 完全 join 后才进
/// `events_registry.stop_and_join_all`，保证此时**不**会有新 register 调用
/// （所有 RR handler closure 已 drop）。重复调 `stop_and_join_all` 是 no-op
/// （`mem::take` 返空 `Vec`）。
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
    ///
    /// 典型用法：daemon shutdown 顺序里"先发 stop 让所有线程并发退出 ..
    /// .. 再 `join_all` 等收敛"，比挨个 stop+join 串行快。
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
/// 必须在 owner 线程内构造；同 [`crate::rr_server_thread`] / A1
/// `crates/robot-s0/src/rr_server_thread.rs::spawn_rr_server_thread`
/// factory 模式）。
///
/// `events_rx` 一般来自 [`crate::action::ActionTicket::events`]。 skeleton
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
                let is_terminal = is_terminal(&event);
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
                if is_terminal {
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

#[inline]
fn is_terminal(event: &ActionEvent) -> bool {
    matches!(
        event,
        ActionEvent::Completed { .. } | ActionEvent::Failed { .. } | ActionEvent::Cancelled { .. }
    )
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
// `ActionEvent` → `ActionEventEnvelope` 编码
// ============================================================================

/// 把 lib `ActionEvent` 5 个 variant 映射到 fbs `ActionEventEnvelope`
/// 字段集；返完整 wire body bytes（`build_action_event_envelope` 内部走
/// `FlatBufferBuilder`）。
///
/// 时间戳来源：`at_ns` = `event` 本身的 `at_ns` 字段（事件**语义**时刻）。
/// 跟 `FrameHeader.stamp_ns`（wire publish 时刻）区分。
#[must_use]
pub fn encode_envelope(action_id_hex: &str, event: &ActionEvent) -> Vec<u8> {
    match event {
        ActionEvent::Started { at_ns } => build_action_event_envelope(
            action_id_hex,
            *at_ns,
            ActionEventKindFb::Started,
            0.0,
            None,
            None,
        ),
        ActionEvent::Progress { progress, at_ns } => build_action_event_envelope(
            action_id_hex,
            *at_ns,
            ActionEventKindFb::Progress,
            *progress,
            None,
            None,
        ),
        ActionEvent::Completed { outcome, at_ns } => build_action_event_envelope(
            action_id_hex,
            *at_ns,
            ActionEventKindFb::Completed,
            0.0,
            Some(map_outcome(outcome)),
            None,
        ),
        ActionEvent::Failed { failure, at_ns } => build_action_event_envelope(
            action_id_hex,
            *at_ns,
            ActionEventKindFb::Failed,
            0.0,
            None,
            Some(map_failure(failure)),
        ),
        ActionEvent::Cancelled { at_ns } => build_action_event_envelope(
            action_id_hex,
            *at_ns,
            ActionEventKindFb::Cancelled,
            0.0,
            None,
            None,
        ),
    }
}

#[inline]
fn map_outcome(outcome: &ActionOutcome) -> (ActionOutcomeKindFb, &str) {
    match outcome {
        ActionOutcome::Success => (ActionOutcomeKindFb::Success, ""),
        ActionOutcome::PartialSuccess { warning } => {
            (ActionOutcomeKindFb::PartialSuccess, warning.as_str())
        }
    }
}

#[inline]
fn map_failure(failure: &ActionFailure) -> (ActionFailureKindFb, &str) {
    match failure {
        ActionFailure::DeadlineExceeded => (ActionFailureKindFb::DeadlineExceeded, ""),
        ActionFailure::PreconditionFailed(s) => {
            (ActionFailureKindFb::PreconditionFailed, s.as_str())
        }
        ActionFailure::AdapterError(s) => (ActionFailureKindFb::AdapterError, s.as_str()),
        ActionFailure::SafetyAbort(s) => (ActionFailureKindFb::SafetyAbort, s.as_str()),
        ActionFailure::InternalError(s) => (ActionFailureKindFb::InternalError, s.as_str()),
    }
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
    use iceoryx2::prelude::NodeBuilder;
    use iceoryx2::service::ipc::Service as IxService;
    use robot_core::time::{RobotClock, SystemClock};
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
        let action_id = fresh_action_id();

        let (tx, rx) = mpsc::channel::<ActionEvent>();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            rx,
            make_publisher_factory(topic.clone()),
            clock,
            /* source_id */ 2,
        )
        .unwrap();

        tx.send(ActionEvent::Started {
            at_ns: 1_234_567_890,
        })
        .unwrap();

        let (bytes, seq) = wait_for_envelope(&mut sub, Duration::from_secs(2));
        let env = flatbuffers::root::<FbEnv<'_>>(&bytes).unwrap();
        assert_eq!(env.action_id().unwrap(), action_id.to_string());
        assert_eq!(env.kind(), FbKind::Started);
        assert_eq!(env.at_ns(), 1_234_567_890);
        assert!(env.outcome().is_none());
        assert!(env.failure().is_none());
        assert_eq!(seq, 0, "first publish should have FrameHeader.seq = 0");

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn completed_partial_success_carries_warning() {
        let topic = fresh_topic("completed");
        let mut sub = make_subscriber(&topic);
        let action_id = fresh_action_id();

        let (tx, rx) = mpsc::channel::<ActionEvent>();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            rx,
            make_publisher_factory(topic.clone()),
            clock,
            2,
        )
        .unwrap();

        tx.send(ActionEvent::Completed {
            outcome: ActionOutcome::PartialSuccess {
                warning: "trajectory clamped".into(),
            },
            at_ns: 42_000,
        })
        .unwrap();

        let (bytes, _seq) = wait_for_envelope(&mut sub, Duration::from_secs(2));
        let env = flatbuffers::root::<FbEnv<'_>>(&bytes).unwrap();
        assert_eq!(env.kind(), FbKind::Completed);
        assert_eq!(env.at_ns(), 42_000);
        let outcome = env.outcome().expect("outcome present on Completed");
        assert_eq!(outcome.kind(), ActionOutcomeKindFb::PartialSuccess);
        assert_eq!(outcome.warning().unwrap(), "trajectory clamped");
        assert!(env.failure().is_none());

        // terminal event → 自然退出（grace 之后），handle.join() 不阻塞。
        handle.join().unwrap();
    }

    #[test]
    fn failed_event_carries_failure_kind_and_reason() {
        let topic = fresh_topic("failed");
        let mut sub = make_subscriber(&topic);
        let action_id = fresh_action_id();

        let (tx, rx) = mpsc::channel::<ActionEvent>();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            rx,
            make_publisher_factory(topic.clone()),
            clock,
            2,
        )
        .unwrap();

        tx.send(ActionEvent::Failed {
            failure: ActionFailure::SafetyAbort("estop latched".into()),
            at_ns: 9_000,
        })
        .unwrap();

        let (bytes, _seq) = wait_for_envelope(&mut sub, Duration::from_secs(2));
        let env = flatbuffers::root::<FbEnv<'_>>(&bytes).unwrap();
        assert_eq!(env.kind(), FbKind::Failed);
        let failure = env.failure().expect("failure present on Failed");
        assert_eq!(failure.kind(), ActionFailureKindFb::SafetyAbort);
        assert_eq!(failure.reason().unwrap(), "estop latched");
        assert!(env.outcome().is_none());

        handle.join().unwrap();
    }

    #[test]
    fn stop_signal_breaks_loop_when_no_event() {
        // 锁住"统一 stop/join"：spawn 起来不发任何 event，request_stop +
        // join 应在 ~poll_interval 内 clean exit（**不**等终态 / **不**等
        // grace）。
        let topic = fresh_topic("stop_sig");
        let _sub = make_subscriber(&topic);
        let action_id = fresh_action_id();

        let (_tx, rx) = mpsc::channel::<ActionEvent>();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            rx,
            make_publisher_factory(topic.clone()),
            clock,
            2,
        )
        .unwrap();

        let metrics = handle.metrics();
        let start = std::time::Instant::now();
        handle.request_stop();
        handle.join().unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "join took {elapsed:?}, expected < 500ms"
        );
        assert_eq!(metrics.stopped_by_signal.load(Ordering::Acquire), 1);
        assert_eq!(metrics.stopped_by_disconnect.load(Ordering::Acquire), 0);
        assert_eq!(metrics.terminal_events.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn channel_disconnect_breaks_loop_immediately() {
        let topic = fresh_topic("disconnect");
        let _sub = make_subscriber(&topic);
        let action_id = fresh_action_id();

        let (tx, rx) = mpsc::channel::<ActionEvent>();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            rx,
            make_publisher_factory(topic.clone()),
            clock,
            2,
        )
        .unwrap();

        let metrics = handle.metrics();
        // drop sender → channel disconnected → drain thread 下一轮
        // recv_timeout 命中 Disconnected → break。
        drop(tx);
        handle.join().unwrap();
        assert_eq!(metrics.stopped_by_disconnect.load(Ordering::Acquire), 1);
        assert_eq!(metrics.stopped_by_signal.load(Ordering::Acquire), 0);
        assert_eq!(metrics.terminal_events.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn terminal_event_increments_metrics_and_self_exits() {
        // Cancelled 是终态：thread 收到后应 publish + sleep grace + 自然
        // 退出，metrics.terminal_events == 1。
        let topic = fresh_topic("terminal");
        let mut sub = make_subscriber(&topic);
        let action_id = fresh_action_id();

        let (tx, rx) = mpsc::channel::<ActionEvent>();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            rx,
            make_publisher_factory(topic.clone()),
            clock,
            2,
        )
        .unwrap();

        let metrics = handle.metrics();
        tx.send(ActionEvent::Cancelled { at_ns: 500_000 }).unwrap();

        let (bytes, _seq) = wait_for_envelope(&mut sub, Duration::from_secs(2));
        let env = flatbuffers::root::<FbEnv<'_>>(&bytes).unwrap();
        assert_eq!(env.kind(), FbKind::Cancelled);

        // thread 应在 terminal_grace 后自然 exit（无需 request_stop）。
        handle.join().unwrap();
        assert_eq!(metrics.events_received.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.events_published.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.terminal_events.load(Ordering::Relaxed), 1);
        // terminal 退出**不**置 stopped_by_signal / stopped_by_disconnect
        // —— 三条退出路径互斥（reviewer 拍板：terminal 是"正常完结"，跟
        // signal / disconnect 语义独立）。
        assert_eq!(metrics.stopped_by_signal.load(Ordering::Acquire), 0);
        assert_eq!(metrics.stopped_by_disconnect.load(Ordering::Acquire), 0);
    }

    #[test]
    fn frame_header_seq_monotonic_per_topic() {
        // 锁住"FrameHeader.seq 单调递增"语义（per-topic；跟
        // JointCommand.source_task_id 是不同字段）。两个 Progress 事件 →
        // seq=0, seq=1。
        //
        // **iceoryx2 keep-last 单 slot**：一次 `latest()` 消费 sample 后
        // 第二次返 None；所以两次 publish 之间必须等第一帧被 sub 收走，
        // 否则第二次 publish 覆盖第一次（reader 直接看到 seq=1，看不到
        // seq=0）。本测试用 mpsc 发送一帧 + wait_for_envelope 收一帧的
        // 串行节奏，保证两帧都被 sub 收到。
        let topic = fresh_topic("seq_mono");
        let mut sub = make_subscriber(&topic);
        let action_id = fresh_action_id();

        let (tx, rx) = mpsc::channel::<ActionEvent>();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            rx,
            make_publisher_factory(topic.clone()),
            clock,
            2,
        )
        .unwrap();

        tx.send(ActionEvent::Progress {
            progress: 0.25,
            at_ns: 1,
        })
        .unwrap();
        let (bytes0, seq0) = wait_for_envelope(&mut sub, Duration::from_secs(2));
        let env0 = flatbuffers::root::<FbEnv<'_>>(&bytes0).unwrap();
        assert_eq!(env0.kind(), FbKind::Progress);
        assert_eq!(seq0, 0, "first publish should have FrameHeader.seq = 0");

        tx.send(ActionEvent::Progress {
            progress: 0.5,
            at_ns: 2,
        })
        .unwrap();
        let (bytes1, seq1) = wait_for_envelope(&mut sub, Duration::from_secs(2));
        let env1 = flatbuffers::root::<FbEnv<'_>>(&bytes1).unwrap();
        assert_eq!(env1.kind(), FbKind::Progress);
        assert_eq!(seq1, 1, "second publish should have FrameHeader.seq = 1");

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn registry_stop_and_join_all_handles_empty() {
        let registry = EventsDrainRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        registry.stop_and_join_all().unwrap();
    }

    #[test]
    fn registry_stop_and_join_all_one_thread() {
        let topic = fresh_topic("registry");
        let _sub = make_subscriber(&topic);
        let action_id = fresh_action_id();

        let (_tx, rx) = mpsc::channel::<ActionEvent>();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let handle = spawn_events_drain_thread(
            cfg_for(action_id, &topic),
            rx,
            make_publisher_factory(topic.clone()),
            clock,
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
    }
}
