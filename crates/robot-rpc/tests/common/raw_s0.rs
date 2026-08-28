//! 直连 s0 的 iceoryx2 测试工具（v0.5 候选 E 段(7)，Spec R3-B4 + QA6）：
//!
//! - [`inject_joint_commands`]：**raw `JointCommand` 注入器**——绕过 S1 直接
//!   往 `/<robot_id>/s1/<hw>/command` 发帧，可指定任意 `layout_id`。AC-E2 的
//!   跨进程判据用它：**不起对应 S1**（正常 S1 持续发合法命令 + `latest()`
//!   塌缩会造成假 PASS），拓扑 = s0 + `FakeHeartbeatPublisher` + 本注入器。
//! - [`read_positions`]：直接订阅 `/<robot_id>/s0/<hw>/state` 读最新
//!   `position_rad`（观测注入效果；不经 gateway，避免多一层缓存语义）。
//!
//! ## 线程模型
//!
//! iceoryx2 句柄 `!Send`——两个入口都在 `spawn_blocking` 内**当场建、当场
//! 用、当场 drop**（沿 `fake_state.rs` 的"owner 线程内 create"约束，但不
//! leak node：一次性使用）。publish 后 linger 一小段再 drop，避免 sample
//! 在 subscriber 取走前被回收（沿 `freeze_systemd_smoke` 同款经验）。

use std::time::Duration;

use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::build_helpers::build_joint_command;
use robot_msg::markers;
use robot_msg::msg::{ControlMode, JointState as FbJointState, StaleAction};
use robot_transport::iceoryx2::{TopicClass, publisher as open_pub, subscriber as open_sub};
use robot_transport::{LatestSubscriber, Publisher};

/// 注入器发帧间隔（≥ s0 ipc 线程 poll 周期，保证每帧都被看到）。
const INJECT_INTERVAL: Duration = Duration::from_millis(20);
/// 最后一帧后保持 publisher 存活的时长。
const INJECT_LINGER: Duration = Duration::from_millis(300);
/// 注入命令的 deadline 余量（s0 按 `deadline_ns` 丢过期帧）。
const INJECT_DEADLINE_AHEAD_NS: u64 = 2_000_000_000;
/// 测试注入器的 `source_id`（≠ s1 的 2，便于 journal 区分）。
const INJECTOR_SOURCE_ID: u32 = 7;

/// 往 command topic 发 `frames` 帧 `JointCommand`（同一 `layout_id` /
/// `target`）。
///
/// # Errors
///
/// iceoryx2 node / publisher 建失败、loan/publish 失败、blocking task join
/// 失败均返 `Err(String)`。
pub async fn inject_joint_commands(
    topic: String,
    layout_id: u64,
    target: Vec<f64>,
    frames: usize,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let node = NodeBuilder::new()
            .create::<IxService>()
            .map_err(|e| format!("raw_s0 inject: NodeBuilder: {e:?}"))?;
        let mut publisher = open_pub::<markers::JointCommand>(&node, &topic, TopicClass::S0Command)
            .map_err(|e| format!("raw_s0 inject: open_pub {topic}: {e:?}"))?;
        let clock = SystemClock::new();
        for seq in 0..frames {
            let now_ns = clock.now().0;
            let body = build_joint_command(
                layout_id,
                ControlMode::Position,
                &target,
                now_ns.saturating_add(INJECT_DEADLINE_AHEAD_NS),
                10,
                StaleAction::Hold,
                0,
            );
            let mut writer = publisher
                .loan(body.len() + 1024)
                .map_err(|e| format!("raw_s0 inject: loan: {e:?}"))?;
            writer.set_seq(seq as u64);
            writer.set_source_id(INJECTOR_SOURCE_ID);
            writer.set_stamp_ns(now_ns);
            writer.set_body_bytes(&body);
            writer
                .publish()
                .map_err(|e| format!("raw_s0 inject: publish: {e:?}"))?;
            std::thread::sleep(INJECT_INTERVAL);
        }
        std::thread::sleep(INJECT_LINGER);
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| format!("raw_s0 inject: join: {e}"))?
}

/// 订阅 s0 state topic，等到一帧 `position_rad.len() == expected_joints`
/// 的样本并返回其内容。
///
/// # Errors
///
/// 建 subscriber 失败 / `timeout` 内无合规帧 / fbs 解码失败。
pub async fn read_positions(
    topic: String,
    expected_joints: usize,
    timeout: Duration,
) -> Result<Vec<f64>, String> {
    tokio::task::spawn_blocking(move || {
        let node = NodeBuilder::new()
            .create::<IxService>()
            .map_err(|e| format!("raw_s0 read: NodeBuilder: {e:?}"))?;
        let mut sub = open_sub::<markers::JointState>(&node, &topic, TopicClass::S0State)
            .map_err(|e| format!("raw_s0 read: open_sub {topic}: {e:?}"))?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Ok(Some(lease)) = sub.latest() {
                let view = lease
                    .view()
                    .map_err(|e| format!("raw_s0 read: view: {e:?}"))?;
                let js = flatbuffers::root::<FbJointState<'_>>(view.body.raw())
                    .map_err(|e| format!("raw_s0 read: fbs root: {e}"))?;
                if let Some(pos) = js.position_rad()
                    && pos.len() == expected_joints
                {
                    return Ok(pos.iter().collect::<Vec<f64>>());
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "raw_s0 read: no JointState frame with {expected_joints} joints within {timeout:?}"
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })
    .await
    .map_err(|e| format!("raw_s0 read: join: {e}"))?
}
