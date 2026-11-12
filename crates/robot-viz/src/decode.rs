//! 验证并复制观测数据；未知布局不解释关节数组，枚举状态不推导物理安全。

use robot_msg::{markers::BodyMarker, msg, schema_id};
use robot_transport::MessageView;

use crate::config::{Layout, Layouts};
use crate::{VizError, invalid, media};

/// 单条标量曲线。
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub path: Vec<String>,
    pub value: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Media {
    Image(media::Image),
    PointCloud(media::PointCloud),
    Tensor(media::Tensor),
}

/// 拥有数据的单帧；构造后不再持有 transport loan。
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub topic: String,
    pub publisher_instance: String,
    pub observer_seq: u64,
    pub observer_dropped: u64,
    pub schema_id: u32,
    pub source_id: u32,
    pub capability_flags: u64,
    pub seq: i64,
    pub header_stamp_ns: i64,
    pub body_stamp_ns: i64,
    pub received_ns: i64,
    pub series: Vec<Series>,
    pub status: String,
    pub media: Option<Media>,
}

/// 十种已注册 pub/sub schema；RR 与嵌入式 task payload 不属于可订阅数据。
pub const PUBSUB_SCHEMAS: [u32; 10] = [
    schema_id::SCHEMA_JOINT_LAYOUT,
    schema_id::SCHEMA_JOINT_STATE,
    schema_id::SCHEMA_JOINT_COMMAND,
    schema_id::SCHEMA_TWIST,
    schema_id::SCHEMA_IMAGE_HEADER,
    schema_id::SCHEMA_POINTCLOUD_HEADER,
    schema_id::SCHEMA_TENSOR_HEADER,
    schema_id::SCHEMA_ESTOP_EVENT,
    schema_id::SCHEMA_HEARTBEAT,
    schema_id::SCHEMA_ACTION_EVENT_ENVELOPE,
];

pub fn decode<M: BodyMarker>(
    view: &MessageView<'_, M>,
    topic: &str,
    now_ns: u64,
    layouts: &mut Layouts,
) -> Result<Observation, VizError> {
    if !layouts.allows_topic(topic) || topic.len() > 512 || view.body.raw().len() > 8192 {
        return Err(invalid("topic 未授权、名称过长或 body 超过 8 KiB"));
    }
    let mut observation = Observation {
        topic: topic.to_owned(),
        publisher_instance: String::new(),
        observer_seq: 0,
        observer_dropped: 0,
        schema_id: M::SCHEMA_ID,
        source_id: view.header.source_id(),
        capability_flags: view.header.capability_flags(),
        seq: integer(view.header.seq(), "seq")?,
        header_stamp_ns: integer(view.header.stamp_ns(), "header stamp")?,
        body_stamp_ns: integer(view.header.stamp_ns(), "body stamp")?,
        received_ns: integer(now_ns, "接收时刻")?,
        series: Vec::new(),
        status: String::new(),
        media: None,
    };
    if observation.header_stamp_ns == 0 || observation.header_stamp_ns > observation.received_ns {
        return Err(invalid("无效或未来发布时间戳"));
    }
    if observation.capability_flags
        & (robot_msg::capability::CAP_GPU_MEMORY_HANDLE
            | robot_msg::capability::CAP_BLOB_COMPRESSED_LZ4
            | robot_msg::capability::CAP_BLOB_COMPRESSED_ZSTD)
        != 0
    {
        return Err(invalid("不支持 GPU 句柄或压缩媒体 capability"));
    }
    let body = view.body.raw();
    match M::SCHEMA_ID {
        schema_id::SCHEMA_JOINT_LAYOUT => decode_layout(body, topic, &mut observation, layouts)?,
        schema_id::SCHEMA_JOINT_STATE => decode_state(body, topic, &mut observation, layouts)?,
        schema_id::SCHEMA_JOINT_COMMAND => decode_command(body, topic, &mut observation, layouts)?,
        schema_id::SCHEMA_TWIST => decode_twist(body, &mut observation)?,
        schema_id::SCHEMA_HEARTBEAT => decode_heartbeat(body, &mut observation)?,
        schema_id::SCHEMA_ESTOP_EVENT => decode_estop(body, &mut observation)?,
        schema_id::SCHEMA_ACTION_EVENT_ENVELOPE => decode_action(body, topic, &mut observation)?,
        schema_id::SCHEMA_IMAGE_HEADER => decode_image(body, blob(view)?, &mut observation)?,
        schema_id::SCHEMA_POINTCLOUD_HEADER => {
            decode_pointcloud(body, blob(view)?, &mut observation)?;
        }
        schema_id::SCHEMA_TENSOR_HEADER => decode_tensor(body, blob(view)?, &mut observation)?,
        _ => return Err(invalid("非 pub/sub schema")),
    }
    if observation.body_stamp_ns == 0 || observation.header_stamp_ns == 0 {
        return Err(invalid("缺少有效时间戳"));
    }
    if observation.body_stamp_ns > observation.received_ns
        || observation.header_stamp_ns > observation.received_ns
    {
        return Err(invalid("来源时间戳晚于本机接收时刻"));
    }
    Ok(observation)
}

fn decode_layout(
    body: &[u8],
    topic: &str,
    observation: &mut Observation,
    layouts: &mut Layouts,
) -> Result<(), VizError> {
    let value = flatbuffers::root::<msg::JointLayout<'_>>(body)?;
    let (robot, hardware) = layouts.slot(topic)?;
    if value.hardware_id() != Some(hardware) {
        return Err(invalid("JointLayout hardware_id 与 topic 冲突"));
    }
    let names: Vec<String> = value
        .names()
        .ok_or_else(|| invalid("缺少布局 names"))?
        .iter()
        .map(str::to_owned)
        .collect();
    let ids: Vec<u32> = value
        .joint_ids()
        .ok_or_else(|| invalid("缺少布局 ids"))?
        .iter()
        .collect();
    let layout = Layout::new(robot.to_owned(), hardware.to_owned(), names, ids)?;
    if layout.layout_id != value.layout_id() {
        return Err(invalid("布局 hash 不匹配"));
    }
    observation.status = format!(
        "JointLayout layout_id={} names={:?} ids={:?}",
        layout.layout_id, layout.names, layout.ids
    );
    layouts.insert(layout)?;
    Ok(())
}

fn decode_state(
    body: &[u8],
    topic: &str,
    observation: &mut Observation,
    layouts: &mut Layouts,
) -> Result<(), VizError> {
    let value = flatbuffers::root::<msg::JointState<'_>>(body)?;
    let layout = layouts.get(topic, value.layout_id())?;
    observation.body_stamp_ns = integer(value.stamp_ns(), "state stamp")?;
    joints(
        &mut observation.series,
        "position_rad",
        &layout.names,
        value.position_rad(),
        false,
    )?;
    // 保留 wire 名义；未标定速度和未测力矩的 NaN 表示未知，不补零或冒充 SI。
    let velocity_unknown = joints(
        &mut observation.series,
        "velocity_wire",
        &layout.names,
        value.velocity_rps(),
        true,
    )?;
    let effort_unknown = joints(
        &mut observation.series,
        "effort_wire",
        &layout.names,
        value.effort_nm(),
        true,
    )?;
    observation.status = format!(
        "layout_id={} mode={:?} link={:?} output={:?} safety={:?} fault={:?}; velocity_wire_unknown={velocity_unknown} effort_wire_unknown={effort_unknown} (NaN=未知); OBSERVATION_ONLY UNKNOWN_NOT_SAFE",
        value.layout_id(),
        value.s0_mode(),
        value.s0_control_link(),
        value.s0_output_mode(),
        value.s0_safety_state(),
        value.s0_fault_code()
    );
    for (name, number) in [
        ("mode", value.s0_mode().0),
        ("control_link", value.s0_control_link().0),
        ("output_mode", value.s0_output_mode().0),
        ("safety_state", value.s0_safety_state().0),
        ("fault_code", value.s0_fault_code().0),
    ] {
        observation.series.push(Series {
            path: vec!["state_code".into(), name.into()],
            value: f64::from(number),
        });
    }
    Ok(())
}

fn decode_command(
    body: &[u8],
    topic: &str,
    observation: &mut Observation,
    layouts: &mut Layouts,
) -> Result<(), VizError> {
    let value = flatbuffers::root::<msg::JointCommand<'_>>(body)?;
    let layout = layouts.get(topic, value.layout_id())?;
    joints(
        &mut observation.series,
        "target",
        &layout.names,
        value.target(),
        false,
    )?;
    integer(value.deadline_ns(), "command deadline")?;
    observation.status = format!(
        "COMMAND_OBSERVATION_ONLY layout_id={} mode={:?} deadline_ns={} priority={} source_task_id={} on_timeout={:?}",
        value.layout_id(),
        value.mode(),
        value.deadline_ns(),
        value.priority(),
        value.source_task_id(),
        value.on_timeout()
    );
    Ok(())
}

fn decode_twist(body: &[u8], observation: &mut Observation) -> Result<(), VizError> {
    let value = flatbuffers::root::<msg::Twist<'_>>(body)?;
    observation.body_stamp_ns = integer(value.stamp_ns(), "twist stamp")?;
    for (name, vector) in [("linear", value.linear()), ("angular", value.angular())] {
        let vector = vector.ok_or_else(|| invalid("Twist 缺少速度分量"))?;
        for (axis, number) in [("x", vector.x()), ("y", vector.y()), ("z", vector.z())] {
            finite(number)?;
            observation.series.push(Series {
                path: vec![name.into(), axis.into()],
                value: number,
            });
        }
    }
    observation.status = format!("Twist frame_id={}", bounded(value.frame_id())?);
    Ok(())
}

fn decode_heartbeat(body: &[u8], observation: &mut Observation) -> Result<(), VizError> {
    let value = flatbuffers::root::<msg::Heartbeat<'_>>(body)?;
    if value.source_id() != observation.source_id {
        return Err(invalid("Heartbeat body/header source_id 冲突"));
    }
    observation.body_stamp_ns = integer(value.stamp_ns(), "heartbeat stamp")?;
    integer(value.seq(), "heartbeat seq")?;
    finite(value.tick_rate_hz())?;
    observation.series.push(Series {
        path: vec!["tick_rate_hz".into()],
        value: value.tick_rate_hz(),
    });
    observation.status = format!(
        "Heartbeat source_id={} seq={}",
        value.source_id(),
        value.seq()
    );
    Ok(())
}

fn decode_estop(body: &[u8], observation: &mut Observation) -> Result<(), VizError> {
    let value = flatbuffers::root::<msg::EStopEvent<'_>>(body)?;
    observation.body_stamp_ns = integer(value.stamp_ns(), "estop stamp")?;
    observation.status = format!(
        "EStop source={} reason={} latched_wire={}; OBSERVATION_ONLY NOT_FSYNC_OR_TORQUE_CONFIRMATION",
        bounded(value.source())?,
        bounded(value.reason())?,
        value.latched()
    );
    Ok(())
}

fn decode_action(body: &[u8], topic: &str, observation: &mut Observation) -> Result<(), VizError> {
    let value = flatbuffers::root::<msg::ActionEventEnvelope<'_>>(body)?;
    let action = bounded(value.action_id())?;
    if topic.rsplit('/').next() != Some(action) {
        return Err(invalid("ActionEvent ID 与 topic 不匹配"));
    }
    observation.body_stamp_ns = integer(value.at_ns(), "action at_ns")?;
    finite(f64::from(value.progress()))?;
    observation.series.push(Series {
        path: vec!["progress".into()],
        value: f64::from(value.progress()),
    });
    let warning = value
        .outcome()
        .map(|outcome| bounded(outcome.warning()))
        .transpose()?
        .unwrap_or("");
    let failure = value
        .failure()
        .map(|failure| bounded(failure.reason()))
        .transpose()?
        .unwrap_or("");
    observation.status = format!(
        "ActionEvent id={action} kind={:?} warning={warning} failure={failure}",
        value.kind()
    );
    Ok(())
}

fn decode_image(body: &[u8], bytes: &[u8], observation: &mut Observation) -> Result<(), VizError> {
    let value = flatbuffers::root::<msg::ImageHeader<'_>>(body)?;
    observation.body_stamp_ns = integer(value.stamp_ns(), "image stamp")?;
    observation.media = Some(Media::Image(media::image(value, bytes)?));
    observation.status = format!(
        "Image encoding={} frame_id={}",
        bounded(value.encoding())?,
        bounded(value.frame_id())?
    );
    Ok(())
}

fn decode_pointcloud(
    body: &[u8],
    bytes: &[u8],
    observation: &mut Observation,
) -> Result<(), VizError> {
    let value = flatbuffers::root::<msg::PointCloudHeader<'_>>(body)?;
    observation.body_stamp_ns = integer(value.stamp_ns(), "pointcloud stamp")?;
    observation.media = Some(Media::PointCloud(media::pointcloud(value, bytes)?));
    observation.status = format!(
        "PointCloud frame_id={}; NO_ASSUMED_EXTRINSICS_OR_FK",
        bounded(value.frame_id())?
    );
    Ok(())
}

fn decode_tensor(body: &[u8], bytes: &[u8], observation: &mut Observation) -> Result<(), VizError> {
    let value = flatbuffers::root::<msg::TensorHeader<'_>>(body)?;
    observation.body_stamp_ns = integer(value.stamp_ns(), "tensor stamp")?;
    observation.media = Some(Media::Tensor(media::tensor(value, bytes)?));
    observation.status = format!(
        "CPU Tensor dtype={:?}",
        value.blob().map(|desc| desc.dtype())
    );
    Ok(())
}

fn integer(value: u64, field: &str) -> Result<i64, VizError> {
    i64::try_from(value).map_err(|_| invalid(format!("{field} 超过 Rerun i64 范围")))
}

fn bounded(value: Option<&str>) -> Result<&str, VizError> {
    let value = value.unwrap_or("");
    if value.len() > 1024 {
        return Err(invalid("消息文本超过 1024 字节"));
    }
    Ok(value)
}

fn finite(value: f64) -> Result<(), VizError> {
    if !value.is_finite() {
        return Err(invalid("数值包含 NaN/Infinity"));
    }
    Ok(())
}

fn joints(
    series: &mut Vec<Series>,
    field: &str,
    names: &[String],
    values: Option<flatbuffers::Vector<'_, f64>>,
    allow_unknown_nan: bool,
) -> Result<usize, VizError> {
    let values = values.ok_or_else(|| invalid(format!("缺少 {field}")))?;
    if values.len() != names.len() {
        return Err(invalid(format!("{field} 长度与布局不同")));
    }
    let mut unknown = 0;
    for (name, value) in names.iter().zip(values) {
        if allow_unknown_nan && value.is_nan() {
            unknown += 1;
        } else {
            finite(value)?;
        }
        series.push(Series {
            path: vec![field.to_owned(), name.clone()],
            value,
        });
    }
    Ok(unknown)
}

fn blob<'a, M: BodyMarker>(view: &MessageView<'a, M>) -> Result<&'a [u8], VizError> {
    view.blob
        .as_ref()
        .map(robot_transport::BlobView::bytes)
        .ok_or_else(|| invalid("缺少媒体 blob"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flatbuffers::FlatBufferBuilder;
    use robot_msg::markers;
    use robot_transport::mock::MockChannel;
    use robot_transport::traits::{LatestSubscriber, Publisher};

    fn state_body(layout_id: u64, stamp: u64) -> Vec<u8> {
        state_body_values(layout_id, stamp, [Some(&[0.125]); 3])
    }

    fn state_body_values(layout_id: u64, stamp: u64, values: [Option<&[f64]>; 3]) -> Vec<u8> {
        let mut builder = FlatBufferBuilder::new();
        let [position, velocity, effort] =
            values.map(|values| values.map(|values| builder.create_vector(values)));
        let state = msg::JointState::create(
            &mut builder,
            &msg::JointStateArgs {
                layout_id,
                stamp_ns: stamp,
                position_rad: position,
                velocity_rps: velocity,
                effort_nm: effort,
                ..msg::JointStateArgs::default()
            },
        );
        builder.finish(state, None);
        builder.finished_data().to_vec()
    }

    fn attempt(
        body: &[u8],
        seq: u64,
        header_stamp: u64,
        topic: &str,
    ) -> Result<Observation, VizError> {
        attempt_message::<markers::JointState>(body, seq, header_stamp, topic)
    }

    fn attempt_message<M: BodyMarker>(
        body: &[u8],
        seq: u64,
        header_stamp: u64,
        topic: &str,
    ) -> Result<Observation, VizError> {
        let channel = MockChannel::<M>::new();
        let mut publisher = channel.publisher();
        let mut subscriber = channel.subscriber();
        let mut writer = publisher.loan(8192).unwrap();
        writer.set_body_bytes(body);
        writer.set_seq(seq);
        writer.set_stamp_ns(header_stamp);
        writer.publish().unwrap();
        let lease = subscriber.latest().unwrap().unwrap();
        let mut layouts = Layouts::default();
        layouts
            .add_toml(
                "[robot]\nid='test'\n[[hardware]]\nid='arm'\njoints=['joint']\nmotor_ids=[1]\n",
            )
            .unwrap();
        let result = decode(&lease.view().unwrap(), topic, 1000, &mut layouts);
        drop(lease);
        assert_eq!(channel.active_leases(), 0, "输出对象不能保留共享内存");
        result
    }

    #[test]
    fn state_unknown_measurements_preserve_nan_without_losing_position_or_status() {
        let layout = robot_msg::joint_layout::compute_layout_id("test", "arm", &[("joint", 1)]);
        for (velocity, effort) in [(f64::NAN, f64::NAN), (0.25, f64::NAN), (f64::NAN, 0.5)] {
            let body = state_body_values(
                layout,
                900,
                [Some(&[0.125]), Some(&[velocity]), Some(&[effort])],
            );
            let observation = attempt(&body, 42, 950, "/test/s0/arm/state").unwrap();
            assert_eq!(observation.series.len(), 8);
            assert_eq!(observation.series[0].value, 0.125);
            for (series, expected) in observation.series[1..3].iter().zip([velocity, effort]) {
                if expected.is_nan() {
                    assert!(series.value.is_nan(), "未知值不能补零或丢弃");
                } else {
                    assert_eq!(series.value, expected);
                }
            }
            assert!(observation.status.contains(&format!(
                "velocity_wire_unknown={} effort_wire_unknown={}",
                usize::from(velocity.is_nan()),
                usize::from(effort.is_nan())
            )));
            assert!(observation.status.contains("NaN=未知"));
            assert!(observation.status.contains("UNKNOWN_NOT_SAFE"));
        }
    }

    #[test]
    fn state_still_rejects_nonfinite_positions_infinite_measurements_and_bad_shapes() {
        let layout = robot_msg::joint_layout::compute_layout_id("test", "arm", &[("joint", 1)]);
        for field in 0..3 {
            for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                if field != 0 && bad.is_nan() {
                    continue;
                }
                let mut values: [Option<&[f64]>; 3] = [Some(&[0.125]); 3];
                let invalid_values = [bad];
                values[field] = Some(&invalid_values);
                let body = state_body_values(layout, 900, values);
                assert!(attempt(&body, 42, 950, "/test/s0/arm/state").is_err());
            }
            for bad_shape in [
                None,
                Some([].as_slice()),
                Some([0.125, f64::NAN].as_slice()),
            ] {
                let mut values: [Option<&[f64]>; 3] = [Some(&[0.125]); 3];
                values[field] = bad_shape;
                let body = state_body_values(layout, 900, values);
                assert!(attempt(&body, 42, 950, "/test/s0/arm/state").is_err());
            }
        }
        assert!(attempt(&[0; 16], 42, 950, "/test/s0/arm/state").is_err());
    }

    #[test]
    fn command_and_twist_still_reject_nan_and_infinity() {
        let layout = robot_msg::joint_layout::compute_layout_id("test", "arm", &[("joint", 1)]);
        for value in [0.125, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut builder = FlatBufferBuilder::new();
            let target = builder.create_vector(&[value]);
            let command = msg::JointCommand::create(
                &mut builder,
                &msg::JointCommandArgs {
                    layout_id: layout,
                    target: Some(target),
                    ..msg::JointCommandArgs::default()
                },
            );
            builder.finish(command, None);
            assert_eq!(
                attempt_message::<markers::JointCommand>(
                    builder.finished_data(),
                    42,
                    950,
                    "/test/s1/arm/command"
                )
                .is_ok(),
                value.is_finite()
            );
            let mut builder = FlatBufferBuilder::new();
            let linear = msg::Vec3::create(
                &mut builder,
                &msg::Vec3Args {
                    x: value,
                    ..msg::Vec3Args::default()
                },
            );
            let angular = msg::Vec3::create(&mut builder, &msg::Vec3Args::default());
            let twist = msg::Twist::create(
                &mut builder,
                &msg::TwistArgs {
                    stamp_ns: 900,
                    linear: Some(linear),
                    angular: Some(angular),
                    ..msg::TwistArgs::default()
                },
            );
            builder.finish(twist, None);
            assert_eq!(
                attempt_message::<markers::Twist>(
                    builder.finished_data(),
                    42,
                    950,
                    "/test/s1/base/twist"
                )
                .is_ok(),
                value.is_finite()
            );
        }
    }

    #[test]
    fn unknown_layout_wrong_slot_future_and_time_overflow_are_rejected() {
        let layout = robot_msg::joint_layout::compute_layout_id("test", "arm", &[("joint", 1)]);
        let body = state_body(layout, 900);
        let observation = attempt(&body, 42, 950, "/test/s0/arm/state").unwrap();
        assert_eq!(
            (
                observation.body_stamp_ns,
                observation.header_stamp_ns,
                observation.seq
            ),
            (900, 950, 42)
        );
        assert!(attempt(&body, u64::MAX, 950, "/test/s0/arm/state").is_err());
        assert!(attempt(&body, 42, u64::MAX, "/test/s0/arm/state").is_err());
        assert!(attempt(&body, 42, 1001, "/test/s0/arm/state").is_err());
        assert!(attempt(&body, 42, 950, "/foreign/s0/arm/state").is_err());
        assert!(attempt(&body, 42, 950, "/test/s0/other/state").is_err());
        assert!(attempt(&state_body(layout + 1, 900), 42, 950, "/test/s0/arm/state").is_err());
        assert!(attempt(&state_body(layout, 1001), 42, 950, "/test/s0/arm/state").is_err());
    }

    #[test]
    fn layout_cache_never_overwrites_conflicting_identity() {
        let mut layouts = Layouts::default();
        layouts
            .add_toml(
                "[robot]\nid='test'\n[[hardware]]\nid='arm'\njoints=['joint']\nmotor_ids=[1]\n",
            )
            .unwrap();
        let old = robot_msg::joint_layout::compute_layout_id("test", "arm", &[("joint", 1)]);
        let conflicting =
            Layout::new("test".into(), "arm".into(), vec!["other".into()], vec![2]).unwrap();
        assert!(layouts.insert(conflicting).is_err());
        assert_eq!(
            layouts.get("/test/s0/arm/state", old).unwrap().names,
            ["joint"]
        );
        assert_eq!(PUBSUB_SCHEMAS.len(), 10);
    }

    #[test]
    fn rejected_layout_timestamp_cannot_populate_the_cache() {
        let layout_id = robot_msg::joint_layout::compute_layout_id("test", "arm", &[("joint", 1)]);
        let mut builder = FlatBufferBuilder::new();
        let name = builder.create_string("joint");
        let names = builder.create_vector(&[name]);
        let ids = builder.create_vector(&[1_u32]);
        let hardware = builder.create_string("arm");
        let layout = msg::JointLayout::create(
            &mut builder,
            &msg::JointLayoutArgs {
                layout_id,
                names: Some(names),
                joint_ids: Some(ids),
                hardware_id: Some(hardware),
            },
        );
        builder.finish(layout, None);
        let channel = MockChannel::<markers::JointLayout>::new();
        let mut publisher = channel.publisher();
        let mut subscriber = channel.subscriber();
        let mut layouts = Layouts::default();
        layouts
            .add_toml("[robot]\nid='test'\n[[hardware]]\nid='arm'\n")
            .unwrap();
        for stamp in [0, 1001] {
            let mut writer = publisher.loan(8192).unwrap();
            writer.set_body_bytes(builder.finished_data());
            writer.set_stamp_ns(stamp);
            writer.publish().unwrap();
            let lease = subscriber.latest().unwrap().unwrap();
            assert!(
                decode(
                    &lease.view().unwrap(),
                    "/test/s0/arm/layout",
                    1000,
                    &mut layouts
                )
                .is_err()
            );
            assert!(layouts.get("/test/s0/arm/state", layout_id).is_err());
        }
        assert_eq!(channel.active_leases(), 0);
    }
}
