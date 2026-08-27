//! `estop_pub_thread` —— gateway 端 EStop publisher 专用线程（v0.5 候选 D，
//! D1 路由 (a)：gateway publish `/<robot_id>/events/estop` topic，**非** RR）。
//!
//! ## 角色
//!
//! iceoryx2 0.8 `ipc::Service` 家族 SingleThreaded policy → publisher handle
//! **`!Send`**（同 [`crate::rr_io_thread`] 的 RR client / daemon 端
//! subscriber factory 约束）→ tokio gRPC handler 不能直接持 publisher。本
//! 模块镜像 `rr_io_thread` 的线程模型：
//!
//! - **常驻** publisher（gateway 启动 attach，**不**走 robotctl per-call
//!   attach——emergency 路径不付 attach 延迟；SafetyEvent QoS
//!   `max_publishers=4` 预算宽裕，CodeMap §2/§3）。
//! - handler → [`EStopPubForward`]（crossbeam **`bounded(64)`**，PB2 锁死）
//!   → 线程内 loan / set envelope / publish → oneshot 回
//!   `Ok(published_at_ns)` / `Err(EStopPubError)`。
//!
//! ## 失败映射（PD2，handler 侧消费）
//!
//! | 失败点 | handler 行为 |
//! |---|---|
//! | forward `try_send` Full | `tonic::ResourceExhausted` |
//! | forward Disconnected（本线程死）| `tonic::Unavailable` |
//! | 线程内 loan / publish Err → oneshot `Err` | `tonic::Internal` |
//! | oneshot 等待超时（timeout_ms + grace）| `tonic::DeadlineExceeded` |
//!
//! 启动 attach（`open_pub` open_or_create）**不依赖 S0 subscriber 存在**；
//! attach 失败 = factory panic → spawn 返 Err → gateway 启动 fail-fast
//! （沿 rr_io spawn 回滚先例）。
//!
//! ## shutdown
//!
//! `request_stop()` 后 main loop 出循环；channel 内残留 forward 的
//! `oneshot_tx` 随 drop 释放 → handler 收 `RecvError` → `Unavailable`
//! （沿 `rr_io_thread` shutdown 语义）。生命周期由
//! [`crate::gateway::run_gateway_with_rr_io`] 控制（tonic 退出后才 stop）。

#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::Receiver;
use robot_core::time::RobotClock;
use robot_msg::markers;
use robot_transport::Publisher;
use robot_transport::iceoryx2::Iceoryx2Publisher;
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
    join: JoinHandle<()>,
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

    /// 等线程退出。
    ///
    /// # Errors
    ///
    /// 线程内部 panic → `Err(Box<dyn Any + Send>)`。
    pub fn join(self) -> std::thread::Result<()> {
        self.join.join()
    }
}

/// 起 `estop_pub_thread`。
///
/// `publisher_factory` 在新线程内执行（publisher `!Send`，owner 线程内构造；
/// `open_pub` open_or_create——**不依赖 S0 subscriber 存在**）。spawn 同步
/// ready 握手：返回 Ok 时 publisher **已 attach**（PB1：caller 必须在
/// `"rr_io_threads started"` marker log **之前**调本 fn，让 marker 蕴含
/// estop publisher ready）。
///
/// # Errors
///
/// - `publisher_factory` panic（iceoryx2 service / attach 失败）
/// - `thread::Builder::spawn` 失败（极罕见）
pub fn spawn_estop_pub_thread<F>(
    cfg: &EStopPubThreadConfig,
    clock: Arc<dyn RobotClock>,
    publisher_factory: F,
    forward_rx: Receiver<EStopPubForward>,
) -> std::io::Result<EStopPubThreadHandle>
where
    F: FnOnce() -> Iceoryx2Publisher<markers::EStopEvent> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let metrics = Arc::new(EStopPubMetrics::default());

    let stop_clone = Arc::clone(&stop);
    let metrics_clone = Arc::clone(&metrics);
    let cfg_clone = cfg.clone();

    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

    let join = thread::Builder::new()
        .name("gw_estop_pub".into())
        .spawn(move || {
            let mut publisher = publisher_factory();
            let _ = ready_tx.send(());
            drop(ready_tx);
            run_loop(
                &cfg_clone,
                clock.as_ref(),
                &mut publisher,
                &forward_rx,
                &stop_clone,
                &metrics_clone,
            );
        })?;

    if ready_rx.recv().is_err() {
        return Err(std::io::Error::other(
            "estop_pub_thread publisher_factory panicked before signalling ready",
        ));
    }

    Ok(EStopPubThreadHandle {
        join,
        stop,
        metrics,
    })
}

fn run_loop(
    cfg: &EStopPubThreadConfig,
    clock: &dyn RobotClock,
    publisher: &mut Iceoryx2Publisher<markers::EStopEvent>,
    forward_rx: &Receiver<EStopPubForward>,
    stop: &AtomicBool,
    metrics: &EStopPubMetrics,
) {
    // envelope seq：线程内单调计数（per-gateway-process）。
    let mut seq: u64 = 0;

    while !stop.load(Ordering::Acquire) {
        // v0.5 候选 E 段(8)：liveness 计数（空闲亦推进）+ test-only freeze hook。
        metrics.loop_tick_count.fetch_add(1, Ordering::Relaxed);
        #[cfg(any(test, feature = "test-freeze-hook"))]
        crate::liveness::freeze_check(&metrics.freeze_until_ns, clock, stop);
        while let Ok(fwd) = forward_rx.try_recv() {
            let result = publish_one(publisher, clock, &mut seq, &fwd);
            match &result {
                Ok(published_at_ns) => {
                    metrics.published.fetch_add(1, Ordering::Relaxed);
                    tracing::info!(
                        request_id = %fwd.request_id,
                        published_at_ns,
                        seq = seq.wrapping_sub(1),
                        "estop_pub_thread: EStopEvent published"
                    );
                }
                Err(e) => {
                    metrics.publish_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        error = %e,
                        request_id = %fwd.request_id,
                        "estop_pub_thread: publish failed; handler returns Internal"
                    );
                }
            }
            // send 失败 = handler 已超时走人；结果丢弃不阻塞本线程。
            let _ = fwd.oneshot_tx.send(result);
        }

        thread::sleep(Duration::from_nanos(cfg.poll_interval_ns));
    }

    // shutdown：channel 残留 forward 丢弃（oneshot drop → handler Unavailable）。
    let mut dropped: u64 = 0;
    while let Ok(fwd) = forward_rx.try_recv() {
        dropped += 1;
        drop(fwd);
    }
    if dropped > 0 {
        tracing::info!(
            dropped,
            "estop_pub_thread: shutdown dropped queued forwards; handlers return Unavailable"
        );
    }
    metrics
        .orphan_drops_on_shutdown
        .fetch_add(dropped, Ordering::Relaxed);
}

/// loan → envelope（seq / `GATEWAY_ESTOP_SOURCE_ID` / stamp）→ body → publish。
/// 成功返 `published_at_ns = clock.now()`。
fn publish_one(
    publisher: &mut Iceoryx2Publisher<markers::EStopEvent>,
    clock: &dyn RobotClock,
    seq: &mut u64,
    fwd: &EStopPubForward,
) -> Result<u64, EStopPubError> {
    let mut writer = publisher
        .loan(fwd.body_bytes.len() + 1024)
        .map_err(|e| EStopPubError::Loan(format!("{e:?}")))?;
    writer.set_seq(*seq);
    writer.set_source_id(GATEWAY_ESTOP_SOURCE_ID);
    writer.set_stamp_ns(fwd.stamp_ns);
    writer.set_body_bytes(&fwd.body_bytes);
    writer
        .publish()
        .map_err(|e| EStopPubError::Publish(format!("{e:?}")))?;
    *seq = seq.wrapping_add(1);
    Ok(clock.now().0)
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
            Box::leak(Box::new(node));
            publisher
        };
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let handle =
            spawn_estop_pub_thread(&cfg, clock, factory, forward_rx).expect("spawn estop_pub");
        (handle, forward_tx)
    }

    /// round trip：forward 一条 body → 真 iceoryx2 订阅者收到（envelope +
    /// body 字段全验）→ oneshot 回 Ok(published_at_ns > 0)。
    #[test]
    fn estop_pub_round_trip_publishes_and_acks() {
        let topic = fresh_topic("rt");

        // 订阅者先建（同进程；attach 早于 publish，沿 PD1 纪律）。
        let sub_node: &'static _ =
            Box::leak(Box::new(NodeBuilder::new().create::<IxService>().unwrap()));
        let mut sub = open_sub::<markers::EStopEvent>(sub_node, &topic, TopicClass::SafetyEvent)
            .expect("open estop subscriber");

        let (handle, forward_tx) = bring_up(&topic);

        let body = build_estop_event("unit-reason", 777, "rpc", false);
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        forward_tx
            .send(EStopPubForward {
                body_bytes: body,
                stamp_ns: 777,
                oneshot_tx,
                request_id: "req-estop-1".into(),
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
        handle.join().unwrap();
    }

    /// spawn + 立即 stop：ready 握手 + 干净退出（不 panic / 不挂）。
    #[test]
    fn estop_pub_spawn_then_stop_cleanly() {
        let topic = fresh_topic("stop");
        let (handle, _forward_tx) = bring_up(&topic);
        handle.request_stop();
        handle.join().unwrap();
    }
}
