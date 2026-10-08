//! 发布身份专项：实际样本、完整标识与未知来源边界。

#![cfg(any(feature = "mock", feature = "iceoryx2"))]

use robot_msg::build_helpers::build_heartbeat;
use robot_msg::markers::{self, BodyMarker};
use robot_transport::{LatestSubscriber, PublishReceipt, Publisher, PublisherOrigin, SampleLease};

const SOURCE_ID: u32 = 73;
const SEQUENCE: u64 = 9;
const STAMP_NS: u64 = 123;

fn publish_body<T: BodyMarker>(publisher: &mut impl Publisher<T>, body: &[u8]) -> PublishReceipt {
    let mut writer = publisher.loan(1024).expect("申请样本 writer");
    writer.set_source_id(SOURCE_ID);
    writer.set_seq(SEQUENCE);
    writer.set_stamp_ns(STAMP_NS);
    writer.set_body_bytes(body);
    writer.publish_typed().expect("发布实际样本")
}

fn assert_received_body<T: BodyMarker>(lease: &SampleLease<T>, body: &[u8]) {
    let view = lease.view().expect("验证实际收到的样本");
    assert_eq!(view.header.source_id(), SOURCE_ID);
    assert_eq!(view.header.seq(), SEQUENCE);
    assert_eq!(view.header.stamp_ns(), STAMP_NS);
    assert_eq!(view.body.raw(), body);
}

#[cfg(feature = "mock")]
mod mock {
    use super::*;
    use robot_transport::mock::MockChannel;

    #[test]
    fn legacy_mock_channels_do_not_infer_origin_from_wire() {
        let channels = [
            MockChannel::<markers::Heartbeat>::new(),
            MockChannel::<markers::Heartbeat>::default(),
        ];
        let body = build_heartbeat(SOURCE_ID, SEQUENCE, 50.0, STAMP_NS);
        for channel in channels {
            let mut publisher = channel.publisher();
            let mut subscriber = channel.subscriber();
            assert!(subscriber.latest().expect("空通道").is_none());
            assert_eq!(
                publish_body(&mut publisher, &body),
                PublishReceipt::MockStored
            );
            let lease = subscriber.latest().expect("接收").expect("确有一帧");
            assert_received_body(&lease, &body);
            assert_eq!(lease.publisher_origin(), None);
            assert_eq!(channel.publishes(), 1);
            assert_eq!(channel.active_leases(), 1);
            drop(lease);
            assert_eq!(channel.active_leases(), 0);
        }
    }

    #[test]
    fn explicit_mock_origin_preserves_full_bits_and_lease_counts() {
        let origin = PublisherOrigin::for_mock(u128::MAX - 1, u128::MAX);
        let channel = MockChannel::<markers::Heartbeat>::with_publisher_origin(origin);
        let mut publisher = channel.publisher();
        let mut subscriber = channel.subscriber();
        let body = build_heartbeat(SOURCE_ID, SEQUENCE, 50.0, STAMP_NS);
        assert_eq!(
            publish_body(&mut publisher, &body),
            PublishReceipt::MockStored
        );
        let lease = subscriber.latest().expect("接收").expect("确有一帧");
        assert_received_body(&lease, &body);
        assert_eq!(lease.publisher_origin(), Some(origin));
        assert_eq!(origin.node_id().value(), u128::MAX - 1);
        assert_eq!(origin.publisher_id().value(), u128::MAX);
        assert_eq!(channel.active_leases(), 1);
        for _ in 0..8 {
            assert_eq!(lease.publisher_origin(), Some(origin));
        }
        assert_eq!(channel.active_leases(), 1);
        assert_eq!(channel.active_writers(), 0);
        drop(lease);
        assert_eq!(channel.active_leases(), 0);
    }

    #[test]
    fn mock_channel_clones_share_one_fixed_port_and_immutable_samples() {
        let origin = PublisherOrigin::for_mock(17, 31);
        let channel = MockChannel::<markers::Heartbeat>::with_publisher_origin(origin);
        let cloned = channel.clone();
        let mut publisher = channel.publisher();
        let mut other_handle = cloned.publisher();
        let mut subscriber = channel.subscriber();
        let first = build_heartbeat(SOURCE_ID, SEQUENCE, 50.0, STAMP_NS);
        assert_eq!(
            publish_body(&mut publisher, &first),
            PublishReceipt::MockStored
        );
        let old_lease = subscriber.latest().expect("接收首帧").expect("首帧存在");
        let second = build_heartbeat(SOURCE_ID, SEQUENCE + 1, 50.0, STAMP_NS);
        assert_eq!(
            publish_body(&mut other_handle, &second),
            PublishReceipt::MockStored
        );
        let new_lease = subscriber.latest().expect("接收新帧").expect("新帧存在");
        assert_received_body(&old_lease, &first);
        assert_received_body(&new_lease, &second);
        assert_eq!(old_lease.publisher_origin(), Some(origin));
        assert_eq!(new_lease.publisher_origin(), Some(origin));
        assert_eq!(channel.publishes(), 2);
        assert_eq!(cloned.active_leases(), 2);
        drop(old_lease);
        drop(new_lease);
        assert_eq!(channel.active_leases(), 0);
    }

    #[test]
    fn distinct_mock_logical_publishers_have_distinct_port_ids() {
        let first = PublisherOrigin::for_mock(17, 31);
        let second = PublisherOrigin::for_mock(17, 32);
        let body = build_heartbeat(SOURCE_ID, SEQUENCE, 50.0, STAMP_NS);
        let mut received = Vec::new();
        for origin in [first, second] {
            let channel = MockChannel::<markers::Heartbeat>::with_publisher_origin(origin);
            let mut publisher = channel.publisher();
            let mut subscriber = channel.subscriber();
            assert_eq!(
                publish_body(&mut publisher, &body),
                PublishReceipt::MockStored
            );
            let lease = subscriber.latest().expect("接收").expect("确有一帧");
            assert_received_body(&lease, &body);
            received.push(lease.publisher_origin().expect("显式 mock 身份"));
        }
        assert_eq!(received, [first, second]);
        assert_eq!(received[0].node_id(), received[1].node_id());
        assert_ne!(received[0].publisher_id(), received[1].publisher_id());
    }

    #[test]
    fn mock_origin_presence_does_not_validate_body() {
        let origin = PublisherOrigin::for_mock(17, 31);
        let channel = MockChannel::<markers::Heartbeat>::with_publisher_origin(origin);
        let mut publisher = channel.publisher();
        let mut subscriber = channel.subscriber();
        assert_eq!(
            publish_body(&mut publisher, &[0, 1, 2]),
            PublishReceipt::MockStored
        );
        let lease = subscriber.latest().expect("接收").expect("确有一帧");
        assert_eq!(lease.publisher_origin(), Some(origin));
        assert!(lease.view().is_err(), "有身份不代表 body 已通过验证");
    }
}

#[cfg(feature = "iceoryx2")]
mod native {
    use super::*;
    use iceoryx2::prelude::{Node, NodeBuilder, ipc};
    use robot_msg::build_helpers::build_joint_command;
    use robot_msg::msg::{ControlMode, StaleAction};
    use robot_transport::iceoryx2::{
        Iceoryx2Publisher, Iceoryx2Subscriber, TopicClass, publisher, subscriber,
    };

    fn topic(node: &Node<ipc::Service>, label: &str) -> String {
        format!("/test/publisher_origin/{}/{label}", node.id().value())
    }

    fn connect_once<T: BodyMarker>(
        publisher: &Iceoryx2Publisher<T>,
        subscriber: &mut Iceoryx2Subscriber<T>,
    ) {
        // 空读建立接收连接，再明确刷新发布端；不靠延时或重复业务帧凑成功。
        assert!(subscriber.latest().expect("首次空读").is_none());
        publisher.update_connections().expect("刷新真实连接");
    }

    #[test]
    fn native_same_node_command_and_heartbeat_have_distinct_ports() {
        let source = NodeBuilder::new()
            .create::<ipc::Service>()
            .expect("发布节点");
        let receiver = NodeBuilder::new()
            .create::<ipc::Service>()
            .expect("独立接收节点");
        let command_topic = topic(&source, "command");
        let heartbeat_topic = topic(&source, "heartbeat");
        let mut command_sender =
            publisher::<markers::JointCommand>(&source, &command_topic, TopicClass::S0Command)
                .expect("命令发布端");
        let mut heartbeat_sender =
            publisher::<markers::Heartbeat>(&source, &heartbeat_topic, TopicClass::Heartbeat)
                .expect("心跳发布端");
        let mut command_receiver =
            subscriber::<markers::JointCommand>(&receiver, &command_topic, TopicClass::S0Command)
                .expect("命令接收端");
        let mut heartbeat_receiver =
            subscriber::<markers::Heartbeat>(&receiver, &heartbeat_topic, TopicClass::Heartbeat)
                .expect("心跳接收端");
        connect_once(&command_sender, &mut command_receiver);
        connect_once(&heartbeat_sender, &mut heartbeat_receiver);

        let command = build_joint_command(
            17,
            ControlMode::Velocity,
            &[0.0; 3],
            100_000_000,
            0,
            StaleAction::Zero,
            41,
        );
        let heartbeat = build_heartbeat(SOURCE_ID, SEQUENCE, 50.0, STAMP_NS);
        assert_eq!(
            publish_body(&mut command_sender, &command),
            PublishReceipt::Iceoryx2 { recipients: 1 }
        );
        assert_eq!(
            publish_body(&mut heartbeat_sender, &heartbeat),
            PublishReceipt::Iceoryx2 { recipients: 1 }
        );
        let command_lease = command_receiver
            .latest()
            .expect("接收命令")
            .expect("确有命令帧");
        let heartbeat_lease = heartbeat_receiver
            .latest()
            .expect("接收心跳")
            .expect("确有心跳帧");
        assert_received_body(&command_lease, &command);
        assert_received_body(&heartbeat_lease, &heartbeat);
        let command_origin = command_lease.publisher_origin().expect("实际命令来源");
        let heartbeat_origin = heartbeat_lease.publisher_origin().expect("实际心跳来源");
        assert_eq!(command_origin.node_id().value(), source.id().value());
        assert_eq!(heartbeat_origin.node_id(), command_origin.node_id());
        assert_ne!(command_origin.node_id().value(), receiver.id().value());
        assert_ne!(
            command_origin.publisher_id(),
            heartbeat_origin.publisher_id()
        );
        assert_ne!(command_origin, heartbeat_origin);
        assert!(command_receiver.latest().expect("命令已消费").is_none());
        assert!(heartbeat_receiver.latest().expect("心跳已消费").is_none());
    }

    fn received_heartbeat_origin(
        source: &Node<ipc::Service>,
        receiver: &Node<ipc::Service>,
    ) -> PublisherOrigin {
        let heartbeat_topic = topic(source, "different_nodes");
        let mut publisher =
            publisher::<markers::Heartbeat>(source, &heartbeat_topic, TopicClass::Heartbeat)
                .expect("发布端");
        let mut subscriber =
            subscriber::<markers::Heartbeat>(receiver, &heartbeat_topic, TopicClass::Heartbeat)
                .expect("接收端");
        connect_once(&publisher, &mut subscriber);
        let body = build_heartbeat(SOURCE_ID, SEQUENCE, 50.0, STAMP_NS);
        assert_eq!(
            publish_body(&mut publisher, &body),
            PublishReceipt::Iceoryx2 { recipients: 1 }
        );
        let lease = subscriber.latest().expect("接收").expect("确有一帧");
        assert_received_body(&lease, &body);
        let origin = lease.publisher_origin().expect("实际来源");
        assert_eq!(origin.node_id().value(), source.id().value());
        assert!(subscriber.latest().expect("帧已消费").is_none());
        origin
    }

    #[test]
    fn native_different_nodes_with_same_wire_source_remain_distinct() {
        let first = NodeBuilder::new()
            .create::<ipc::Service>()
            .expect("第一发布节点");
        let second = NodeBuilder::new()
            .create::<ipc::Service>()
            .expect("第二发布节点");
        let receiver = NodeBuilder::new()
            .create::<ipc::Service>()
            .expect("接收节点");
        let first_origin = received_heartbeat_origin(&first, &receiver);
        let second_origin = received_heartbeat_origin(&second, &receiver);
        assert_ne!(first_origin.node_id(), second_origin.node_id());
        assert_ne!(first_origin.publisher_id(), second_origin.publisher_id());
        assert_ne!(first_origin, second_origin);
    }
}
