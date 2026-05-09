//! `FakeStatePublisher`：测试侧扮演 s0 角色 publish `JointState` 给
//! `/<robot_id>/s0/<hardware_id>/state`，让 s1_daemon `anchor_or_fail` 能
//! 在 5s timeout 内拿到 stable bias 完成 anchor。
//!
//! ## 拓扑选择（详 Spec § Q3 Research Findings）
//!
//! B4 (3/3) test 拓扑下**不**真起 s0_daemon —— 测试侧 publish state 即可
//! 满足 s1 anchor 需求；同时不冲突 `S0Command` topic（max_sub=1）：测试
//! 侧扮演 sub command 端，无第二 sub 撞配额。
//!
//! ## QoS
//!
//! `TopicClass::S0State`：`max_publishers=1` / `max_subscribers=8` /
//! `safe_overflow=true`（详 `crates/robot-transport/src/iceoryx2/qos.rs:95-102`）。
//! 测试侧是唯一 publisher，s1_daemon 是唯一 subscriber，配额安全。
//!
//! ## 线程模型
//!
//! 起一条 std::thread，构造 iceoryx2 Node + Publisher（!Send，必须 owner
//! 线程内 create + leak Node），50Hz publish 固定 bias `JointState` 帧。
//! `stop` AtomicBool 触发退出；`Drop` 信号 stop + join。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::build_helpers::build_joint_state;
use robot_msg::markers;
use robot_transport::Publisher;
use robot_transport::iceoryx2::{TopicClass, publisher as open_pub};

const PUBLISH_INTERVAL: Duration = Duration::from_millis(20); // 50Hz
const FAKE_S0_SOURCE_ID: u32 = 1; // s0 约定 source_id = 1（s1 = 2）

pub struct FakeStatePublisher {
    handle: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    topic: String,
}

impl FakeStatePublisher {
    /// Spawn 50Hz publisher。`positions` 长度 = `joints`，所有元素 0.0
    /// （s1 anchor 拿到 zero-bias，hold loop target 也是 zero）。
    ///
    /// # Errors
    ///
    /// - iceoryx2 Node / Publisher create 失败（factory panic 路径会 propagate）
    /// - thread spawn 失败（罕见）
    pub fn spawn(
        robot_id: &str,
        hardware_id: &str,
        joints: usize,
        layout_id: u64,
    ) -> std::io::Result<Self> {
        let topic = format!("/{robot_id}/s0/{hardware_id}/state");
        let topic_owned = topic.clone();
        let topic_for_thread = topic.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let positions = vec![0.0_f64; joints];

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

        let handle = thread::Builder::new()
            .name(format!("fake_state_{robot_id}"))
            .spawn(move || {
                let node = NodeBuilder::new()
                    .create::<IxService>()
                    .expect("FakeStatePublisher: NodeBuilder create");
                let mut publisher = match open_pub::<markers::JointState>(
                    &node,
                    &topic_for_thread,
                    TopicClass::S0State,
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        // factory 失败：直接 drop ready_tx 让 spawn() 见 Err。
                        eprintln!(
                            "[fake_state] open_pub failed for topic {topic_for_thread}: {e:?}"
                        );
                        drop(ready_tx);
                        return;
                    }
                };
                // node 持 publisher 内部 shm fd；leak 让 publisher 整生命
                // 周期内 node 不被 drop（同 daemon / events_drain leak 模式）。
                Box::leak(Box::new(node));
                let _ = ready_tx.send(());
                drop(ready_tx);

                // 用 SystemClock 拿真 monotonic_raw ns —— 跟 s1_daemon
                // `read_anchor_from_s0_state` 用同款 clock domain 比较
                // `stamp_ns` 跟 `clock.now()`，否则 stamp_ns=0/fake 会被
                // max_stale_ms=200ms 判 stale → s1 anchor 失败 exit 4。
                let clock = SystemClock::new();
                let mut seq: u64 = 0;
                while !stop_clone.load(Ordering::Acquire) {
                    let stamp_ns = clock.now().0;
                    let body = build_joint_state(layout_id, stamp_ns, &positions, &[], &[], "");
                    if let Ok(mut writer) = publisher.loan(body.len() + 1024) {
                        writer.set_seq(seq);
                        writer.set_source_id(FAKE_S0_SOURCE_ID);
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
                "FakeStatePublisher: factory panicked / iceoryx2 service create failed for topic {topic_owned}"
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

impl Drop for FakeStatePublisher {
    fn drop(&mut self) {
        // best-effort：信号 stop + join。若 shutdown() 已显式调，handle
        // is None，跳过。
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
