//! 只在隔离命名空间发布软件样本，不启动控制进程或访问硬件。

use flatbuffers::FlatBufferBuilder;
use iceoryx2::prelude::{Node, PortFactory, ServiceName};
use iceoryx2::service::ipc::Service;
use robot_msg::build_helpers::{
    build_action_event_envelope, build_estop_event, build_heartbeat, build_joint_command,
    build_joint_state_with_s0,
};
use robot_msg::joint_layout::compute_layout_id;
use robot_msg::markers::{self, BodyMarker};
use robot_msg::msg::{
    ActionEventKindFb, BlobDesc, BlobDescArgs, BlobLayout, Compression, ControlMode, Device, Dtype,
    Endian, ImageHeader, ImageHeaderArgs, JointLayout, JointLayoutArgs, PointCloudHeader,
    PointCloudHeaderArgs, S0ControlLink, S0FaultCode, S0Mode, S0OutputMode, S0SafetyState,
    StaleAction, TensorHeader, TensorHeaderArgs, Twist, TwistArgs, Vec3, Vec3Args,
};
use robot_transport::iceoryx2::{
    Iceoryx2Publisher, ObserverMonitor, TopicClass, observer_topic, publisher,
};
use robot_transport::traits::Publisher;

pub(crate) const INVALID_POSITION: f64 = 1_234.567_89;
pub(crate) const ACTION_ID: &str = "019ded00000070008000000000000001";
pub(crate) const ESTOP_REASONS: [&str; 2] = ["只读桥软件事件夹具", "第二个独立软件发布者"];

pub(crate) struct Payload {
    body: Vec<u8>,
    blob: Option<Vec<u8>>,
}

impl Payload {
    fn body(body: Vec<u8>) -> Self {
        Self { body, blob: None }
    }
}

pub(crate) trait FixtureSource {
    fn publish(&mut self, stamp: u64) -> u64;
    fn schema(&self) -> u32;
    fn topic(&self) -> &str;
    fn monitor(&self) -> ObserverMonitor;
}

struct Source<T: BodyMarker> {
    publisher: Iceoryx2Publisher<T>,
    topic: String,
    next_seq: u64,
    build: Box<dyn Fn(u64) -> Payload>,
}

impl<T: BodyMarker> FixtureSource for Source<T> {
    fn publish(&mut self, stamp: u64) -> u64 {
        let payload = (self.build)(stamp);
        let size = 1_024 + payload.body.len() + payload.blob.as_ref().map_or(0, Vec::len);
        let mut writer = self.publisher.loan(size).expect("软件夹具 loan");
        writer.set_stamp_ns(stamp);
        let sequence = self.next_seq;
        writer.set_seq(sequence);
        writer.set_source_id(41);
        writer.set_body_bytes(&payload.body);
        if let Some(blob) = payload.blob.as_deref() {
            writer.set_blob_bytes(blob);
        }
        writer.publish().expect("观测者不能使原发布失败");
        self.next_seq += 1;
        sequence
    }

    fn schema(&self) -> u32 {
        T::SCHEMA_ID
    }

    fn topic(&self) -> &str {
        &self.topic
    }

    fn monitor(&self) -> ObserverMonitor {
        self.publisher.observer_monitor()
    }
}

fn source<T: BodyMarker>(
    node: &Node<Service>,
    topic: String,
    class: TopicClass,
    build: impl Fn(u64) -> Payload + 'static,
) -> Box<dyn FixtureSource> {
    Box::new(Source::<T> {
        publisher: publisher(node, &topic, class).expect("创建隔离的真实共享内存 publisher"),
        topic,
        next_seq: 1,
        build: Box::new(build),
    })
}

pub(crate) fn config(robot_id: &str) -> String {
    format!(
        "[robot]\nid = \"{robot_id}\"\n\
         [[hardware]]\nid = \"arm\"\njoints = [\"joint_a\", \"joint_b\"]\nmotor_ids = [1, 2]\n\
         [[hardware]]\nid = \"invalid_arm\"\njoints = [\"joint_a\", \"joint_b\"]\nmotor_ids = [1, 2]\n"
    )
}

fn layout(robot_id: &str, hardware_id: &str) -> u64 {
    compute_layout_id(robot_id, hardware_id, &[("joint_a", 1), ("joint_b", 2)])
}

fn joint_state(layout_id: u64, stamp: u64, positions: &[f64]) -> Payload {
    Payload::body(build_joint_state_with_s0(
        layout_id,
        stamp,
        positions,
        &[0.0, 0.0],
        &[0.0, 0.0],
        "arm/base",
        S0Mode::Safe,
        S0ControlLink::Unknown,
        S0OutputMode::Unknown,
        S0SafetyState::TorqueUnknown,
        S0FaultCode::Unknown,
    ))
}

#[allow(clippy::too_many_lines)] // 十个协议夹具保持按 schema 并列，便于核对覆盖范围。
pub(crate) fn all_sources(node: &Node<Service>, robot_id: &str) -> Vec<Box<dyn FixtureSource>> {
    let layout_id = layout(robot_id, "arm");
    vec![
        source::<markers::JointLayout>(
            node,
            format!("/{robot_id}/s0/arm/layout"),
            TopicClass::S0State,
            move |_| {
                let mut builder = FlatBufferBuilder::new();
                let a = builder.create_string("joint_a");
                let b = builder.create_string("joint_b");
                let names = builder.create_vector(&[a, b]);
                let ids = builder.create_vector(&[1_u32, 2]);
                let hardware = builder.create_string("arm");
                let root = JointLayout::create(
                    &mut builder,
                    &JointLayoutArgs {
                        layout_id,
                        names: Some(names),
                        joint_ids: Some(ids),
                        hardware_id: Some(hardware),
                    },
                );
                builder.finish(root, None);
                Payload::body(builder.finished_data().to_vec())
            },
        ),
        source::<markers::JointState>(
            node,
            format!("/{robot_id}/s0/arm/state"),
            TopicClass::S0State,
            move |stamp| joint_state(layout_id, stamp, &[0.125, -0.25]),
        ),
        source::<markers::JointCommand>(
            node,
            format!("/{robot_id}/s1/arm/command"),
            TopicClass::S0Command,
            move |stamp| {
                Payload::body(build_joint_command(
                    layout_id,
                    ControlMode::Position,
                    &[0.5, -0.75],
                    stamp + 1_000_000_000,
                    10,
                    StaleAction::Stop,
                    42,
                ))
            },
        ),
        source::<markers::Twist>(
            node,
            format!("/{robot_id}/s1/cmd_vel"),
            TopicClass::S1CmdVel,
            |stamp| {
                let mut builder = FlatBufferBuilder::new();
                let linear = Vec3::create(
                    &mut builder,
                    &Vec3Args {
                        x: 0.2,
                        y: 0.0,
                        z: 0.0,
                    },
                );
                let angular = Vec3::create(
                    &mut builder,
                    &Vec3Args {
                        x: 0.0,
                        y: 0.0,
                        z: -0.1,
                    },
                );
                let frame = builder.create_string("base");
                let root = Twist::create(
                    &mut builder,
                    &TwistArgs {
                        linear: Some(linear),
                        angular: Some(angular),
                        frame_id: Some(frame),
                        stamp_ns: stamp,
                    },
                );
                builder.finish(root, None);
                Payload::body(builder.finished_data().to_vec())
            },
        ),
        source::<markers::ImageHeader>(
            node,
            format!("/{robot_id}/sensors/camera/color"),
            TopicClass::Sensor,
            |stamp| image(stamp, false),
        ),
        source::<markers::PointCloudHeader>(
            node,
            format!("/{robot_id}/sensors/lidar/points"),
            TopicClass::Sensor,
            point_cloud,
        ),
        source::<markers::TensorHeader>(
            node,
            format!("/{robot_id}/policy/test/observation"),
            TopicClass::PolicyStream,
            tensor,
        ),
        source::<markers::Heartbeat>(
            node,
            format!("/{robot_id}/s0/heartbeat"),
            TopicClass::Heartbeat,
            |stamp| Payload::body(build_heartbeat(41, stamp, 50.0, stamp)),
        ),
        source::<markers::EStopEvent>(
            node,
            format!("/{robot_id}/events/estop"),
            TopicClass::SafetyEvent,
            |stamp| {
                Payload::body(build_estop_event(
                    ESTOP_REASONS[0],
                    stamp,
                    "software_fixture",
                    false,
                ))
            },
        ),
        source::<markers::ActionEventEnvelope>(
            node,
            format!("/{robot_id}/s1/action_events/{ACTION_ID}"),
            TopicClass::TaskEvent,
            |stamp| {
                Payload::body(build_action_event_envelope(
                    ACTION_ID,
                    stamp,
                    ActionEventKindFb::Progress,
                    0.5,
                    None,
                    None,
                ))
            },
        ),
    ]
}

pub(crate) fn invalid_sources(node: &Node<Service>, robot_id: &str) -> Vec<Box<dyn FixtureSource>> {
    let wrong_layout = layout(robot_id, "invalid_arm") ^ 1;
    vec![
        source::<markers::JointState>(
            node,
            format!("/{robot_id}/s0/invalid_arm/state"),
            TopicClass::S0State,
            move |stamp| joint_state(wrong_layout, stamp, &[INVALID_POSITION, -999.125]),
        ),
        source::<markers::JointState>(
            node,
            format!("/{robot_id}/s0/bad_wire/state"),
            TopicClass::S0State,
            |_| Payload::body(vec![1, 2, 3]),
        ),
        source::<markers::ImageHeader>(
            node,
            format!("/{robot_id}/sensors/bad_camera/color"),
            TopicClass::Sensor,
            |stamp| image(stamp, true),
        ),
    ]
}

pub(crate) fn foreign_source(node: &Node<Service>, robot_id: &str) -> Box<dyn FixtureSource> {
    source::<markers::Heartbeat>(
        node,
        format!("/{robot_id}_other/s0/heartbeat"),
        TopicClass::Heartbeat,
        |stamp| Payload::body(build_heartbeat(42, stamp, 50.0, stamp)),
    )
}

pub(crate) fn second_estop_source(node: &Node<Service>, robot_id: &str) -> Box<dyn FixtureSource> {
    source::<markers::EStopEvent>(
        node,
        format!("/{robot_id}/events/estop"),
        TopicClass::SafetyEvent,
        |stamp| {
            Payload::body(build_estop_event(
                ESTOP_REASONS[1],
                stamp,
                "software_fixture_second",
                false,
            ))
        },
    )
}

pub(crate) fn action_source(
    node: &Node<Service>,
    robot_id: &str,
    index: u32,
) -> Box<dyn FixtureSource> {
    let action_id = format!("019ded000000700080000000{index:08x}");
    source::<markers::ActionEventEnvelope>(
        node,
        format!("/{robot_id}/s1/action_events/{action_id}"),
        TopicClass::TaskEvent,
        move |stamp| {
            Payload::body(build_action_event_envelope(
                &action_id,
                stamp,
                ActionEventKindFb::Progress,
                0.5,
                None,
                None,
            ))
        },
    )
}

/// 只 open 已存在镜像，读活跃订阅端数量；本探针不创建端口，不能冒充桥接收。
pub(crate) fn mirror_has_reader(node: &Node<Service>, primary: &str) -> bool {
    let mirror = observer_topic(primary).expect("本测试镜像名");
    let name = ServiceName::new(&mirror).unwrap();
    node.service_builder(&name)
        .publish_subscribe::<[u8]>()
        .enable_safe_overflow(true)
        .open()
        .is_ok_and(|service| service.dynamic_config().number_of_subscribers() > 0)
}

fn image(stamp: u64, malformed: bool) -> Payload {
    let bytes = if malformed {
        vec![255, 0, 0, 0, 255]
    } else {
        vec![255, 0, 0, 0, 255, 0]
    };
    let mut builder = FlatBufferBuilder::new();
    let shape = builder.create_vector(&[1_u32, 2, 3]);
    let blob = BlobDesc::create(
        &mut builder,
        &BlobDescArgs {
            size: u64::try_from(bytes.len()).unwrap(),
            alignment: 1,
            dtype: Dtype::U8,
            shape: Some(shape),
            layout: BlobLayout::HWC,
            endian: Endian::Little,
            compression: Compression::None_,
            ..BlobDescArgs::default()
        },
    );
    let encoding = builder.create_string("rgb8");
    let order = builder.create_string("RGB");
    let frame = builder.create_string("camera");
    let root = ImageHeader::create(
        &mut builder,
        &ImageHeaderArgs {
            blob: Some(blob),
            width: 2,
            height: 1,
            encoding: Some(encoding),
            step: 6,
            color_order: Some(order),
            stamp_ns: stamp,
            frame_id: Some(frame),
        },
    );
    builder.finish(root, None);
    Payload {
        body: builder.finished_data().to_vec(),
        blob: Some(bytes),
    }
}

fn point_cloud(stamp: u64) -> Payload {
    let bytes: Vec<u8> = [1.0_f32, 2.0, 3.0, -1.0, -2.0, -3.0]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect();
    let mut builder = FlatBufferBuilder::new();
    let shape = builder.create_vector(&[2_u32, 3]);
    let blob = BlobDesc::create(
        &mut builder,
        &BlobDescArgs {
            size: u64::try_from(bytes.len()).unwrap(),
            alignment: 1,
            dtype: Dtype::F32,
            shape: Some(shape),
            layout: BlobLayout::PointsXYZ,
            endian: Endian::Little,
            compression: Compression::None_,
            ..BlobDescArgs::default()
        },
    );
    let x = builder.create_string("x");
    let y = builder.create_string("y");
    let z = builder.create_string("z");
    let fields = builder.create_vector(&[x, y, z]);
    let frame = builder.create_string("lidar");
    let root = PointCloudHeader::create(
        &mut builder,
        &PointCloudHeaderArgs {
            blob: Some(blob),
            point_count: 2,
            point_step: 12,
            fields: Some(fields),
            is_dense: true,
            stamp_ns: stamp,
            frame_id: Some(frame),
        },
    );
    builder.finish(root, None);
    Payload {
        body: builder.finished_data().to_vec(),
        blob: Some(bytes),
    }
}

fn tensor(stamp: u64) -> Payload {
    let bytes: Vec<u8> = [0.5_f32, 1.5, 2.5, 3.5]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect();
    let mut builder = FlatBufferBuilder::new();
    let shape = builder.create_vector(&[2_u32, 2]);
    let blob = BlobDesc::create(
        &mut builder,
        &BlobDescArgs {
            size: u64::try_from(bytes.len()).unwrap(),
            alignment: 1,
            dtype: Dtype::F32,
            shape: Some(shape),
            layout: BlobLayout::Opaque,
            endian: Endian::Little,
            compression: Compression::None_,
            ..BlobDescArgs::default()
        },
    );
    let root = TensorHeader::create(
        &mut builder,
        &TensorHeaderArgs {
            blob: Some(blob),
            device: Device::Cpu,
            stamp_ns: stamp,
            ..TensorHeaderArgs::default()
        },
    );
    builder.finish(root, None);
    Payload {
        body: builder.finished_data().to_vec(),
        blob: Some(bytes),
    }
}
