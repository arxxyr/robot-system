// Step 5 · 真 iceoryx2 后端集成测试（macOS 烟测已前置验证可行性）。
// 覆盖真实 iceoryx2 后端的以下行为：
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
            ..JointStateArgs::default()
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

/// **Bug regression（2026-04-26 锁测）**：runbook §4-§7 真机端到端测试时
/// `robotctl watch` 跟其它 observer（例如另一个 watch / mock_s1.anchor /
/// 未来 recorder/viz）同跑会出现"第二个 sub 拿不到 sample"的现象。最初被
/// 怀疑成 iceoryx2 0.8.1 框架 bug，user 复盘后定位到我们自己的 QoS
/// 分类错误：S0 heartbeat 错用 [`TopicClass::S1CmdVel`]
/// （`max_subscribers=1`），导致第二个 subscriber attach 实际失败 / 不发样本。
///
/// 修复：新增 [`TopicClass::Heartbeat`] (`max_subscribers=8` +
/// `safe_overflow=true`)；本测试钉死"同一 service 两个 subscriber 都收到
/// 同一帧 publisher 输出"，防止哪天又被人按回 1。
#[test]
fn heartbeat_class_supports_two_subscribers_receiving_same_publish() {
    use flatbuffers::FlatBufferBuilder;
    use robot_msg::msg::{Heartbeat as FbHeartbeat, HeartbeatArgs};

    fn build_hb_body(seq: u64, stamp: u64) -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::new();
        let hb = FbHeartbeat::create(
            &mut fbb,
            &HeartbeatArgs {
                source_id: 1,
                seq,
                tick_rate_hz: 50.0,
                stamp_ns: stamp,
            },
        );
        fbb.finish(hb, None);
        fbb.finished_data().to_vec()
    }

    let node = NodeBuilder::new()
        .create::<ipc::Service>()
        .expect("node create");
    let topic = unique_topic("hb_multi_sub");

    // Pub 单创建（先于 sub），下面再开两个 sub 都对接到同一 service。
    let mut pub_: Iceoryx2Publisher<markers::Heartbeat> =
        robot_transport::iceoryx2::publisher(&node, &topic, TopicClass::Heartbeat)
            .expect("open heartbeat publisher");

    let mut sub_a: Iceoryx2Subscriber<markers::Heartbeat> =
        robot_transport::iceoryx2::subscriber(&node, &topic, TopicClass::Heartbeat)
            .expect("open heartbeat subscriber A");
    let mut sub_b: Iceoryx2Subscriber<markers::Heartbeat> =
        robot_transport::iceoryx2::subscriber(&node, &topic, TopicClass::Heartbeat)
            .expect("open heartbeat subscriber B（如果 max_subscribers=1 这里就 fail）");

    // 发一帧
    let body = build_hb_body(7, 12_345);
    let mut w = pub_.loan(1024 + body.len()).expect("loan");
    w.set_seq(7);
    w.set_source_id(1);
    w.set_stamp_ns(12_345);
    w.set_body_bytes(&body);
    w.publish().expect("publish");

    let lease_a = sub_a.latest().expect("A latest").expect("A got sample");
    let lease_b = sub_b.latest().expect("B latest").expect("B got sample");
    let view_a = lease_a.view().expect("A view");
    let view_b = lease_b.view().expect("B view");
    assert_eq!(view_a.header.seq(), 7, "A 收到 seq=7");
    assert_eq!(view_b.header.seq(), 7, "B 收到 seq=7");
    let hb_a: robot_msg::msg::Heartbeat<'_> = view_a.body.get();
    let hb_b: robot_msg::msg::Heartbeat<'_> = view_b.body.get();
    assert_eq!(hb_a.stamp_ns(), 12_345);
    assert_eq!(hb_b.stamp_ns(), 12_345);
}

/// 反向证明：[`TopicClass::S1CmdVel`] 故意保留 `max_subscribers=1`（cmd_vel
/// 的真单订阅语义）。开第二个 sub 应当被 iceoryx2 拒绝（不是静默 0
/// sample），用以**反证**前一个测试不是因为框架始终允许多 sub。
///
/// 这条同时锁住"S1CmdVel 不能被人手抖改成多订阅"——如果未来想让 cmd_vel
/// 有多订阅者，应当走新 class 而不是动这条配置。
#[test]
fn s1_cmdvel_class_rejects_second_subscriber() {
    let node = NodeBuilder::new()
        .create::<ipc::Service>()
        .expect("node create");
    let topic = unique_topic("cmdvel_single_sub");

    let _pub_: Iceoryx2Publisher<markers::Heartbeat> =
        robot_transport::iceoryx2::publisher(&node, &topic, TopicClass::S1CmdVel)
            .expect("open S1CmdVel publisher");
    let _sub_a: Iceoryx2Subscriber<markers::Heartbeat> =
        robot_transport::iceoryx2::subscriber(&node, &topic, TopicClass::S1CmdVel)
            .expect("first subscriber attaches");

    // 第二个 sub 必须 fail（不能 silent succeed），否则 max_subscribers=1
    // 守门已坏，前一个 multi-sub 测试也变成无意义的恒真。
    let result = robot_transport::iceoryx2::subscriber::<markers::Heartbeat>(
        &node,
        &topic,
        TopicClass::S1CmdVel,
    );
    assert!(
        result.is_err(),
        "S1CmdVel max_subscribers=1，第二个 sub 应被拒绝；但 create 成功了 = QoS 失守"
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

#[test]
fn typed_publish_zero_two_recipients_and_idle_connection_update_replays_history() {
    use robot_transport::PublishReceipt;
    use robot_transport::iceoryx2::{publisher, subscriber};
    let node = NodeBuilder::new().create::<ipc::Service>().unwrap();
    let topic = unique_topic("typed_publish_history");
    let mut publisher =
        publisher::<markers::JointState>(&node, &topic, TopicClass::SafetyEvent).unwrap();
    let body = build_joint_state_body(41);
    let mut writer = publisher.loan(1024).unwrap();
    writer.set_body_bytes(&body);
    writer.set_stamp_ns(41);
    writer.set_seq(9);
    assert_eq!(
        writer.publish_typed().unwrap(),
        PublishReceipt::Iceoryx2 { recipients: 0 }
    );

    let mut first =
        subscriber::<markers::JointState>(&node, &topic, TopicClass::SafetyEvent).unwrap();
    publisher.update_connections().unwrap();
    let historical = first
        .latest()
        .unwrap()
        .expect("空闲刷新必须补送之前的 history");
    let view = historical.view().unwrap();
    assert_eq!((view.header.stamp_ns(), view.header.seq()), (41, 9));
    assert_eq!(view.body.raw(), body);
    drop(historical);
    let mut second =
        subscriber::<markers::JointState>(&node, &topic, TopicClass::SafetyEvent).unwrap();
    publisher.update_connections().unwrap();
    drop(
        second
            .latest()
            .unwrap()
            .expect("第二订阅者也收到真实旧 history"),
    );
    let mut writer = publisher.loan(1024).unwrap();
    writer.set_body_bytes(&build_joint_state_body(42));
    writer.set_stamp_ns(42);
    writer.set_seq(10);
    assert_eq!(
        writer.publish_typed().unwrap(),
        PublishReceipt::Iceoryx2 { recipients: 2 }
    );
    for subscriber in [&mut first, &mut second] {
        let lease = subscriber.latest().unwrap().expect("两个真实订阅者都收到");
        let view = lease.view().unwrap();
        assert_eq!((view.header.stamp_ns(), view.header.seq()), (42, 10));
        assert_eq!(view.body.get().stamp_ns(), 42);
    }
}

#[test]
fn typed_and_legacy_publish_deliver_identical_complete_wire_bytes() {
    use robot_transport::{PublishFailure, PublishReceipt};
    let node = NodeBuilder::new().create::<ipc::Service>().unwrap();
    let topic = unique_topic("typed_legacy_wire");
    let mut publisher = robot_transport::iceoryx2::publisher::<markers::JointState>(
        &node,
        &topic,
        TopicClass::SafetyEvent,
    )
    .unwrap();
    let service = node
        .service_builder(&topic_to_service_name(&topic).unwrap())
        .publish_subscribe::<[u8]>()
        .open()
        .unwrap();
    let subscriber = service
        .subscriber_builder()
        .buffer_size(32)
        .create()
        .unwrap();
    publisher.update_connections().unwrap();
    let body = build_joint_state_body(77);
    let mut received = Vec::new();
    for (typed, owned) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut writer = publisher.loan(1024).unwrap();
        if owned {
            writer.set_body_owned(body.clone());
        } else {
            writer.set_body_bytes(&body);
        }
        writer.set_blob_bytes(&[1, 3, 5, 7]);
        writer.set_stamp_ns(77);
        writer.set_seq(8);
        writer.set_source_id(98);
        match typed {
            false => writer.publish().unwrap(),
            true => assert_eq!(
                writer.publish_typed().unwrap(),
                PublishReceipt::Iceoryx2 { recipients: 1 }
            ),
        }
        received.push(
            subscriber
                .receive()
                .unwrap()
                .expect("实际完整 shm payload")
                .payload()
                .to_vec(),
        );
    }
    for wire in &received[1..] {
        assert_eq!(
            &received[0], wire,
            "借用/所有权入口及两种发布方式的完整 wire 一致"
        );
    }
    let mut writer = publisher.loan(1).unwrap();
    writer.set_body_owned(body);
    assert!(matches!(
        writer.publish_typed(),
        Err(PublishFailure::BeforeHistory(
            TransportError::SampleTooLarge { .. }
        ))
    ));
    assert!(
        subscriber.receive().unwrap().is_none(),
        "history 前失败没有新样本"
    );
}
