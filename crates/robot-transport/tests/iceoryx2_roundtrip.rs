// Step 5 · 真 iceoryx2 后端集成测试（macOS 烟测已前置验证可行性）。
// 覆盖 docs/01_implementation_steps.md Step 5 退出标准中的：
// - "同进程 pub → sub 收到 JointState"
// - "schema_id attr 不匹配 open → 拒绝"（attribute 硬约束，§5 决策 ⑤ C）
// - "sample size 超档 → loan_slice_uninit 失败"（size tier 硬限）

#![cfg(feature = "iceoryx2")]
#![allow(clippy::pedantic, clippy::nursery)]

use flatbuffers::FlatBufferBuilder;
use iceoryx2::prelude::*;
use robot_msg::markers::{self, BodyMarker};
use robot_msg::msg::{JointState as FbJointState, JointStateArgs};
use robot_transport::TransportError;
use robot_transport::iceoryx2::{
    Iceoryx2Publisher, Iceoryx2Subscriber, TopicClass, channel, schema_attribute_spec,
    schema_attribute_verifier, topic_to_service_name,
};
use robot_transport::traits::{LatestSubscriber, Publisher};

fn build_joint_state_body(stamp: u64) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();
    let pos = fbb.create_vector::<f64>(&[0.1, 0.2, 0.3]);
    let js = FbJointState::create(
        &mut fbb,
        &JointStateArgs {
            layout_id: 0xCAFE_BABE_DEAD_BEEF,
            position_rad: Some(pos),
            velocity_rps: None,
            effort_nm: None,
            stamp_ns: stamp,
            frame_id: None,
        },
    );
    fbb.finish(js, None);
    fbb.finished_data().to_vec()
}

/// iceoryx2 service 在同一 node 下的名字是唯一键。每个 test 用不同 topic
/// 避免互相复用底层 service slot 导致的状态污染。
fn unique_topic(label: &str) -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // 用 `/` 前缀坚持 §2.3 命名空间；iceoryx2 0.8.1 接受。
    format!("/test/{label}/{pid}_{nanos}")
}

#[test]
fn roundtrip_joint_state_via_iceoryx2() {
    let node = NodeBuilder::new()
        .create::<ipc::Service>()
        .expect("node create");
    let topic = unique_topic("roundtrip_js");

    let (mut pub_, mut sub) =
        channel::<markers::JointState>(&node, &topic, TopicClass::S0State).expect("channel");

    // 发 3 条，stamp 递增
    for stamp in [1000u64, 2000, 3000] {
        let body = build_joint_state_body(stamp);
        let total = 1024 + body.len(); // 宽松上限：FrameHeader 远小于 1 KiB
        let mut w = pub_.loan(total).expect("loan");
        w.set_stamp_ns(stamp);
        w.set_body_bytes(&body);
        w.publish().expect("publish");
    }

    // 订阅端按 latest() 语义只拿最新一条（drain buffer，保留最后一条）
    let lease = sub.latest().expect("latest").expect("some sample");
    let view = lease.view().expect("view");
    assert_eq!(view.header.stamp_ns(), 3000, "latest 应拿最新样本");
    let js: robot_msg::msg::JointState<'_> = view.body.get();
    assert_eq!(js.layout_id(), 0xCAFE_BABE_DEAD_BEEF);
    assert_eq!(js.stamp_ns(), 3000);

    // 再 latest() 一次：iceoryx2 buffer 已空，返 None
    assert!(sub.latest().expect("latest again").is_none());
}

#[test]
fn sample_size_exceeds_tier_cap_returns_sample_too_large() {
    let node = NodeBuilder::new()
        .create::<ipc::Service>()
        .expect("node create");
    let topic = unique_topic("oversize");

    let (mut pub_, _sub) =
        channel::<markers::JointState>(&node, &topic, TopicClass::S0State).expect("channel");

    // Control 档上限 8 KiB；声明 loan(16 * 1024) = 超档 → 早期拒绝
    // （Publisher::loan impl 对 size > initial_max_slice_len 直接返错，
    // 不构造 writer 也不延迟到 publish）。
    let err = pub_.loan(16 * 1024).unwrap_err();
    match err {
        TransportError::SampleTooLarge { size, limit } => {
            assert_eq!(size, 16 * 1024);
            assert_eq!(limit, 8 * 1024, "Control 档 initial_max_slice_len = 8 KiB");
        }
        other => panic!("expected SampleTooLarge, got {other:?}"),
    }
}

#[test]
fn schema_attribute_mismatch_rejects_open() {
    // 直接走 iceoryx2 service builder 构造 attribute 不匹配场景，
    // 不复用 channel() 因为 channel() 的 open side 会用相同 schema_id。
    let node = NodeBuilder::new()
        .create::<ipc::Service>()
        .expect("node create");
    let topic = unique_topic("attr_mismatch");
    let service_name = topic_to_service_name(&topic).expect("topic → ServiceName");

    let spec = schema_attribute_spec(markers::JointState::SCHEMA_ID, 1).expect("spec");
    let _service = node
        .service_builder(&service_name)
        .publish_subscribe::<[u8]>()
        .create_with_attributes(&spec)
        .expect("create with JointState schema_id");

    // 用 JointCommand 的 schema_id 去 open —— attribute 硬约束下必拒。
    let bad_verifier =
        schema_attribute_verifier(markers::JointCommand::SCHEMA_ID, 1).expect("verifier");
    let reopen = node
        .service_builder(&service_name)
        .publish_subscribe::<[u8]>()
        .open_with_attributes(&bad_verifier);
    assert!(
        reopen.is_err(),
        "用不匹配 schema_id 的 attribute open 应被拒绝，但成功了"
    );
}

#[test]
fn publisher_and_subscriber_debug_formats_dont_panic() {
    let node = NodeBuilder::new()
        .create::<ipc::Service>()
        .expect("node create");
    let topic = unique_topic("debug_fmt");
    let (pub_, sub): (
        Iceoryx2Publisher<markers::JointState>,
        Iceoryx2Subscriber<markers::JointState>,
    ) = channel(&node, &topic, TopicClass::S0State).expect("channel");

    let _ = format!("{pub_:?}");
    let _ = format!("{sub:?}");
}
