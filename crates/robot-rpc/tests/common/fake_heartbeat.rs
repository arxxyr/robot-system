//! `FakeHeartbeatPublisher`：测试侧扮演 **s1** 角色 publish `Heartbeat` 给
//! `/<robot_id>/s1/heartbeat`，驱动 s0 状态机 `Ready → Enabled.Active`
//! （v0.5 候选 C `SetEnabled(true)` D8 观测依赖；Spec D-P3 拍板：e2e 用
//! fake heartbeat 源，**不**起 robot-s1）。
//!
//! ## 拓扑
//!
//! `set_enabled_e2e` 起 **robot-s0（真）+ 本 fake heartbeat + gateway**。
//! s0 `ipc_thread` 订阅该 topic：收 heartbeat → `guards.heartbeat_ok=true` +
//! `Event::Heartbeat` → `Ready + Heartbeat → Enabled.Active`（effectful
//! `Enable(true)`）。无 heartbeat → 滞留 Ready → SetEnabled pending 走
//! deadline（AC-2b）。
//!
//! ## 线程模型（沿 [`crate::common::fake_state::FakeStatePublisher`]）
//!
//! 独立 std::thread 构造 iceoryx2 Node + Publisher（`!Send`，owner 线程内
//! create + leak Node），50Hz publish；`stop` AtomicBool 退出；`Drop` 兜底
//! stop + join。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::build_helpers::build_heartbeat;
use robot_msg::markers;
use robot_transport::Publisher;
use robot_transport::iceoryx2::{TopicClass, publisher as open_pub};

const PUBLISH_INTERVAL: Duration = Duration::from_millis(20); // 50Hz
/// s1 约定 source_id = 2（s0 = 1）。
const FAKE_S1_SOURCE_ID: u32 = 2;
const TICK_RATE_HZ: f64 = 50.0;

pub struct FakeHeartbeatPublisher {
    handle: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    topic: String,
}

impl FakeHeartbeatPublisher {
    /// Spawn 50Hz heartbeat publisher（`/<robot_id>/s1/heartbeat`，fresh
    /// `clock.now()` stamp —— s0 watchdog 按收到时刻 re-arm，不校验 stamp）。
    ///
    /// # Errors
    ///
    /// - iceoryx2 Node / Publisher create 失败
    /// - thread spawn 失败（罕见）
    pub fn spawn(robot_id: &str) -> std::io::Result<Self> {
        let topic = format!("/{robot_id}/s1/heartbeat");
        let topic_owned = topic.clone();
        let topic_for_thread = topic.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

        let handle = thread::Builder::new()
            .name(format!("fake_hb_{robot_id}"))
            .spawn(move || {
                let node = NodeBuilder::new()
                    .create::<IxService>()
                    .expect("FakeHeartbeatPublisher: NodeBuilder create");
                let mut publisher = match open_pub::<markers::Heartbeat>(
                    &node,
                    &topic_for_thread,
                    TopicClass::Heartbeat,
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[fake_hb] open_pub failed for topic {topic_for_thread}: {e:?}");
                        drop(ready_tx);
                        return;
                    }
                };
                Box::leak(Box::new(node));
                let _ = ready_tx.send(());
                drop(ready_tx);

                let clock = SystemClock::new();
                let mut seq: u64 = 0;
                while !stop_clone.load(Ordering::Acquire) {
                    let stamp_ns = clock.now().0;
                    let body = build_heartbeat(FAKE_S1_SOURCE_ID, seq, TICK_RATE_HZ, stamp_ns);
                    if let Ok(mut writer) = publisher.loan(body.len() + 1024) {
                        writer.set_seq(seq);
                        writer.set_source_id(FAKE_S1_SOURCE_ID);
                        writer.set_stamp_ns(stamp_ns);
                        writer.set_body_bytes(&body);
                        let _ = writer.publish();
                    }
                    seq = seq.wrapping_add(1);
                    thread::sleep(PUBLISH_INTERVAL);
                }
            })?;

        if ready_rx.recv().is_err() {
            return Err(std::io::Error::other(format!(
                "FakeHeartbeatPublisher: factory panicked / iceoryx2 service create failed for topic {topic_owned}"
            )));
        }

        Ok(Self {
            handle: Some(handle),
            stop,
            topic: topic_owned,
        })
    }

    /// 显式 graceful shutdown：信号 stop + join thread。
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    /// topic 串（diagnostic）。
    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }
}

impl Drop for FakeHeartbeatPublisher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
