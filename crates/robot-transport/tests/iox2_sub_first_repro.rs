// iceoryx2 0.9.3 升级复核 · 最小复现：**subscriber 侧首建 service**。
//
// 现象（升级实验观测）：4 个需要 s1_daemon 的 smoke 全挂在
// `PublishSubscribeOpenError(ServiceInCorruptedState)`，死点 = s1 启动锚定
// 打开 s0 state subscriber（此时 s0 publisher 尚不存在 → subscriber 是该
// service 的**创建方**）；所有 publisher 先建 service 的路径（crash_restart /
// clear_estop_e2e / estop_e2e / state_rpc_e2e）全过。
//
// 本文件把假设钉成 3 个变体：
// 1. sub_first：subscriber 先建（走 robot-transport 封装）→ 复现目标
// 2. pub_first：publisher 先建（对照组）→ 应 PASS
// 3. sub_first_plain：subscriber 先建但**裸 iceoryx2 API 无 attributes** →
//    隔离 `open_or_create_with_attributes` 是否为致因

#![cfg(feature = "iceoryx2")]
#![allow(clippy::pedantic, clippy::nursery)]

use flatbuffers::FlatBufferBuilder;
use iceoryx2::prelude::*;
use robot_msg::markers;
use robot_msg::msg::{JointState as FbJointState, JointStateArgs};
use robot_transport::iceoryx2::{
    TopicClass, publisher as open_pub, subscriber as open_sub, topic_to_service_name,
};
use robot_transport::traits::{LatestSubscriber, Publisher};

fn build_joint_state_body(stamp: u64) -> Vec<u8> {
    build_joint_state_body_n(stamp, &[0.1, 0.2, 0.3])
}

fn build_joint_state_body_n(stamp: u64, positions: &[f64]) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let pos = fbb.create_vector::<f64>(positions);
    let js = FbJointState::create(
        &mut fbb,
        &JointStateArgs {
            layout_id: 0xCAFE_BABE_DEAD_BEEF,
            position_rad: Some(pos),
            velocity_rps: None,
            effort_nm: None,
            stamp_ns: stamp,
            frame_id: None,
            ..JointStateArgs::default()
        },
    );
    fbb.finish(js, None);
    fbb.finished_data().to_vec()
}

fn unique_topic(label: &str) -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("/test/{label}/{pid}_{nanos}")
}

/// 变体 1：subscriber 先建 service（robot-transport 封装路径 = s1 启动锚定
/// 同款）。0.8.1 下合法；本测试锁 0.9.3 下同样必须合法。
#[test]
fn sub_first_then_pub_roundtrip() {
    let node = NodeBuilder::new()
        .create::<ipc::Service>()
        .expect("node create");
    let topic = unique_topic("sub_first");

    // subscriber 是该 service 的创建方（publisher 尚不存在）。
    let mut sub = open_sub::<markers::JointState>(&node, &topic, TopicClass::S0State)
        .expect("subscriber-side first create must succeed");

    let mut pub_ = open_pub::<markers::JointState>(&node, &topic, TopicClass::S0State)
        .expect("publisher open after subscriber-created service");

    let body = build_joint_state_body(42);
    let mut w = pub_.loan(1024 + body.len()).expect("loan");
    w.set_stamp_ns(42);
    w.set_body_bytes(&body);
    w.publish().expect("publish");

    let lease = sub.latest().expect("latest").expect("some sample");
    let view = lease.view().expect("view");
    assert_eq!(view.header.stamp_ns(), 42);
}

/// 变体 2（对照）：publisher 先建 service —— e2e/smoke 里全过的既有路径。
#[test]
fn pub_first_then_sub_roundtrip() {
    let node = NodeBuilder::new()
        .create::<ipc::Service>()
        .expect("node create");
    let topic = unique_topic("pub_first");

    let mut pub_ = open_pub::<markers::JointState>(&node, &topic, TopicClass::S0State)
        .expect("publisher-side first create");
    let mut sub = open_sub::<markers::JointState>(&node, &topic, TopicClass::S0State)
        .expect("subscriber open after publisher-created service");

    let body = build_joint_state_body(7);
    let mut w = pub_.loan(1024 + body.len()).expect("loan");
    w.set_stamp_ns(7);
    w.set_body_bytes(&body);
    w.publish().expect("publish");

    let lease = sub.latest().expect("latest").expect("some sample");
    let view = lease.view().expect("view");
    assert_eq!(view.header.stamp_ns(), 7);
}

/// 变体 4：**并发 open_or_create 创建竞态**。两线程（各自独立 Node，模拟
/// s0/s1 两进程同时 boot）对同一 fresh topic 分别 create publisher /
/// subscriber，×200 轮放大竞态窗口。smoke 失败形态 = s0/s1 并发起时 s1 报
/// `ServiceInCorruptedState`；单独 s1（无并发）不复现 —— 若本测试挂即钉死。
#[test]
fn concurrent_pub_sub_create_race() {
    // 30 轮足够：未修复时 iter 0 即中（0.8.1 / 0.9.3 双实测）；修复后每轮
    // 最坏 500ms 重试，30 轮上界可控。
    for i in 0..30 {
        let topic = unique_topic(&format!("race_{i}"));
        let t_pub = {
            let topic = topic.clone();
            std::thread::spawn(move || {
                let node = NodeBuilder::new()
                    .create::<ipc::Service>()
                    .expect("pub node create");
                open_pub::<markers::JointState>(&node, &topic, TopicClass::S0State)
                    .map(|_| ())
                    .map_err(|e| format!("{e:?}"))
            })
        };
        let t_sub = {
            let topic = topic.clone();
            std::thread::spawn(move || {
                let node = NodeBuilder::new()
                    .create::<ipc::Service>()
                    .expect("sub node create");
                open_sub::<markers::JointState>(&node, &topic, TopicClass::S0State)
                    .map(|_| ())
                    .map_err(|e| format!("{e:?}"))
            })
        };
        let r_pub = t_pub.join().expect("pub thread join");
        let r_sub = t_sub.join().expect("sub thread join");
        assert!(
            r_pub.is_ok() && r_sub.is_ok(),
            "iter {i}: pub={r_pub:?} sub={r_sub:?}"
        );
    }
}

/// 变体 5：**父进程建 service → spawn 子进程 open**（4 个失败 smoke 的共同
/// 拓扑：harness 线程 fake_s0 publisher 先建 + ready 后 spawn s1_daemon，
/// s1 anchor open 持久报 `ServiceInCorruptedState (attempts=51)`；对照：
/// child→child / child→parent / 同进程全过）。子进程直接用真
/// `target/release/s1_daemon`。若 s1 落 corrupted fatal（exit 1）即钉死
/// "parent-publisher → child-subscriber" 为特异成分。
#[test]
#[ignore = "需要预先 cargo build --release -p robot-s1（spawn 真 s1_daemon）；诊断/升级复核手跑"]
fn parent_pub_then_child_s1_open() {
    // s1_daemon 的 state topic 形状固定 /<robot>/s0/<hw>/state。
    let robot_id = format!("repro_pc_{}", std::process::id());
    let topic = format!("/{robot_id}/s0/smoke_arm/state");

    // 50 Hz 发布线程（iceoryx2 句柄 !Send → node + publisher 线程内建，
    // ready 握手后再 spawn 子进程 —— 跟 smoke harness fake_s0 同款时序）。
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_pub = stop.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let topic_for_thread = topic.clone();
    let pub_thread = std::thread::spawn(move || {
        let node = NodeBuilder::new()
            .create::<ipc::Service>()
            .expect("pub node create");
        let mut pub_ =
            open_pub::<markers::JointState>(&node, &topic_for_thread, TopicClass::S0State)
                .expect("parent publisher create");
        let _ = ready_tx.send(());
        // 真单调钟 stamp（s1 anchor 有 max_stale_ms=200 staleness gate）。
        let clock = robot_core::time::SystemClock;
        while !stop_pub.load(std::sync::atomic::Ordering::Acquire) {
            let stamp = robot_core::time::RobotClock::now(&clock).0;
            // smoke_arm = 1 关节（configs/scripted_backend_smoke.toml）。
            let body = build_joint_state_body_n(stamp, &[0.0]);
            if let Ok(mut w) = pub_.loan(1024 + body.len()) {
                w.set_stamp_ns(stamp);
                w.set_body_bytes(&body);
                let _ = w.publish();
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
    ready_rx.recv().expect("publisher ready handshake");

    // CARGO_MANIFEST_DIR = crates/robot-transport → workspace 根向上两级。
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = std::process::Command::new(root.join("target/release/s1_daemon"))
        .arg("--robot-id")
        .arg(&robot_id)
        .arg("--hardware-id")
        .arg("smoke_arm")
        .arg("--config")
        .arg(root.join("configs/scripted_backend_smoke.toml"))
        .arg("--duration")
        .arg("2s")
        .output()
        .expect("spawn s1_daemon");

    stop.store(true, std::sync::atomic::Ordering::Release);
    pub_thread.join().expect("pub thread join");

    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("[repro] s1 exit={:?}", out.status);
    eprintln!("[repro] s1 stderr tail:");
    for line in stderr
        .lines()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .iter()
        .rev()
    {
        eprintln!("[repro]   {line}");
    }
    assert!(
        !stderr.contains("ServiceInCorruptedState"),
        "s1 child hit corrupted-state on parent-created service:\n{stderr}"
    );
    // 反向防假阳性：s1 必须真的跑过 anchor（正常锚定 exit 0 跑满 duration）。
    assert!(
        out.status.success(),
        "s1 child did not run cleanly (exit={:?}); stderr:\n{stderr}",
        out.status
    );
}

/// 变体 3（隔离）：subscriber 先建但走**裸 iceoryx2 API、无 attributes**。
/// 若变体 1 挂而本变体过 → 致因收敛到 `open_or_create_with_attributes`
/// 的 subscriber-create 分支；若两者同挂 → slice payload subscriber-create
/// 本身的问题。
#[test]
fn sub_first_plain_no_attributes() {
    let node = NodeBuilder::new()
        .create::<ipc::Service>()
        .expect("node create");
    let topic = unique_topic("sub_first_plain");
    let service_name = topic_to_service_name(&topic).expect("service name");

    let service = node
        .service_builder(&service_name)
        .publish_subscribe::<[u8]>()
        .history_size(1)
        .subscriber_max_buffer_size(1)
        .max_publishers(2)
        .max_subscribers(2)
        .enable_safe_overflow(true)
        .open_or_create()
        .expect("plain open_or_create (subscriber side first)");
    let _sub = service
        .subscriber_builder()
        .create()
        .expect("plain subscriber create");
}
