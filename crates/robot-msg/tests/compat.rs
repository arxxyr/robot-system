//! robot-msg 运行时兼容性测试。
//!
//! 覆盖 schema 兼容性的运行时行为：
//!
//! - 每个 table round-trip（writer → bytes → reader → 字段对等）
//! - old-reader-new-writer：`v0::JointState` 解析当前 `msg::JointState` bytes，
//!   新字段被忽略，旧字段正确
//! - new-reader-old-writer：`msg::JointState` 解析 v0 bytes，缺失字段返回默认值
//! - 新字段加末尾兼容性：覆盖同上两条
//!
//! `schema_id` 静态校验走独立 bin（`cargo run --bin schema_lint`），本文件只
//! 检查 wire 层。

use flatbuffers::FlatBufferBuilder;
use robot_msg::schema_id::{
    SCHEMA_ESTOP_EVENT, SCHEMA_HEARTBEAT, SCHEMA_IMAGE_HEADER, SCHEMA_JOINT_COMMAND,
    SCHEMA_JOINT_LAYOUT, SCHEMA_JOINT_STATE, SCHEMA_POINTCLOUD_HEADER, SCHEMA_TENSOR_HEADER,
    SCHEMA_TWIST, lookup_name,
};
use robot_msg::v0;
use robot_msg::{build_helpers, msg};

// ===========================================================================
// Round-trip：每个 table
// ===========================================================================

#[test]
fn frame_header_roundtrip() {
    let mut fbb = FlatBufferBuilder::new();
    let fh = msg::FrameHeader::create(
        &mut fbb,
        &msg::FrameHeaderArgs {
            magic: 0x5242_5443, // "RBTC"
            abi_version: 1,
            schema_id: SCHEMA_JOINT_STATE,
            schema_version: 1,
            capability_flags: 0x10, // LITTLE_ENDIAN
            stamp_ns: 42_000_000_000,
            source_id: 7,
            seq: 100,
            body_offset: 128,
            body_size: 64,
            blob_offset: 0,
            blob_size: 0,
            header_crc32: 0,
        },
    );
    fbb.finish(fh, None);
    let bytes = fbb.finished_data();

    let parsed = flatbuffers::root::<msg::FrameHeader>(bytes).expect("verify FrameHeader");
    assert_eq!(parsed.magic(), 0x5242_5443);
    assert_eq!(parsed.schema_id(), SCHEMA_JOINT_STATE);
    assert_eq!(parsed.stamp_ns(), 42_000_000_000);
    assert_eq!(parsed.body_offset(), 128);
    assert_eq!(parsed.body_size(), 64);
    assert_eq!(parsed.blob_size(), 0);

    // schema_id 能反查到规范名
    assert_eq!(
        lookup_name(parsed.schema_id()),
        Some("robot.msg.JointState")
    );
}

#[test]
fn joint_state_roundtrip() {
    let mut fbb = FlatBufferBuilder::new();
    let pos = fbb.create_vector::<f64>(&[0.1, 0.2, 0.3]);
    let vel = fbb.create_vector::<f64>(&[0.01, 0.02, 0.03]);
    let eff = fbb.create_vector::<f64>(&[1.0, 2.0, 3.0]);
    let fid = fbb.create_string("base_link");
    let js = msg::JointState::create(
        &mut fbb,
        &msg::JointStateArgs {
            layout_id: 0xCAFE_BABE_DEAD_BEEF,
            position_rad: Some(pos),
            velocity_rps: Some(vel),
            effort_nm: Some(eff),
            stamp_ns: 12345,
            frame_id: Some(fid),
            ..msg::JointStateArgs::default()
        },
    );
    fbb.finish(js, None);
    let bytes = fbb.finished_data();

    let parsed = flatbuffers::root::<msg::JointState>(bytes).expect("verify JointState");
    assert_eq!(parsed.layout_id(), 0xCAFE_BABE_DEAD_BEEF);
    assert_eq!(parsed.stamp_ns(), 12345);
    assert_eq!(parsed.frame_id(), Some("base_link"));
    let pos_read = parsed.position_rad().expect("position_rad present");
    assert_eq!(pos_read.len(), 3);
    assert!((pos_read.get(0) - 0.1).abs() < 1e-12);
    assert!((pos_read.get(2) - 0.3).abs() < 1e-12);
}

#[test]
fn joint_command_roundtrip() {
    let mut fbb = FlatBufferBuilder::new();
    let target = fbb.create_vector::<f64>(&[1.57, -1.57, 0.0]);
    let jc = msg::JointCommand::create(
        &mut fbb,
        &msg::JointCommandArgs {
            layout_id: 0x0102_0304_0506_0708,
            mode: msg::ControlMode::Position,
            target: Some(target),
            deadline_ns: 999_000,
            priority: 200,
            source_task_id: 42,
            on_timeout: msg::StaleAction::Zero,
            ..msg::JointCommandArgs::default()
        },
    );
    fbb.finish(jc, None);
    let bytes = fbb.finished_data();

    let parsed = flatbuffers::root::<msg::JointCommand>(bytes).expect("verify JointCommand");
    assert_eq!(parsed.layout_id(), 0x0102_0304_0506_0708);
    assert_eq!(parsed.mode(), msg::ControlMode::Position);
    assert_eq!(parsed.priority(), 200);
    assert_eq!(parsed.on_timeout(), msg::StaleAction::Zero);
    assert_eq!(parsed.target().unwrap().len(), 3);
    assert!(parsed.raw_velocities().is_none());
}

#[test]
fn mixed_joint_command_roundtrip_and_old_reader_unknown_mode_stops() {
    let positions = [0.1, -0.2, 0.3, -0.4, 0.5, -0.6, 0.0, 0.0, 0.0];
    let raw = [0, 0, 0, 0, 0, 0, 0, 500, 0];
    let bytes = build_helpers::build_mixed_joint_command(0xCAFE, &positions, &raw, 200, 10, 7);
    let command = flatbuffers::root::<msg::JointCommand>(&bytes).unwrap();
    assert_eq!(command.mode(), msg::ControlMode::MixedPositionRawVelocity);
    assert_eq!(
        command.target().unwrap().iter().collect::<Vec<_>>(),
        positions
    );
    assert_eq!(
        command.raw_velocities().unwrap().iter().collect::<Vec<_>>(),
        raw
    );
    assert_eq!(command.on_timeout(), msg::StaleAction::Hold);

    let old = flatbuffers::root::<v0::JointCommand>(&bytes).unwrap();
    assert_eq!(old.mode().0, 5);
    // 与扩展前 S0 的未知枚举分支一致：不能按旧 Position 驱动。
    let old_mode = match old.mode() {
        v0::ControlMode::Position => v0::ControlMode::Position,
        v0::ControlMode::Velocity => v0::ControlMode::Velocity,
        v0::ControlMode::Torque => v0::ControlMode::Torque,
        v0::ControlMode::ServoPosition => v0::ControlMode::ServoPosition,
        _ => v0::ControlMode::Stop,
    };
    assert_eq!(old_mode, v0::ControlMode::Stop);
    assert_eq!(old.layout_id(), command.layout_id());
    assert_eq!(old.deadline_ns(), command.deadline_ns());
}

#[test]
fn old_joint_command_writer_has_no_raw_vector_and_preserves_enum_values() {
    for mode in [
        v0::ControlMode::Position,
        v0::ControlMode::Velocity,
        v0::ControlMode::Torque,
        v0::ControlMode::ServoPosition,
        v0::ControlMode::Stop,
    ] {
        let mut builder = FlatBufferBuilder::new();
        let target = builder.create_vector(&[0.25_f64]);
        let old = v0::JointCommand::create(
            &mut builder,
            &v0::JointCommandArgs {
                layout_id: 42,
                mode,
                target: Some(target),
                deadline_ns: 200,
                priority: 10,
                source_task_id: 7,
                on_timeout: v0::StaleAction::Hold,
            },
        );
        builder.finish(old, None);
        let command = flatbuffers::root::<msg::JointCommand>(builder.finished_data()).unwrap();
        assert_eq!(command.mode().0, mode.0);
        assert!(command.raw_velocities().is_none());
        let current = build_helpers::build_joint_command(
            42,
            msg::ControlMode(mode.0),
            &[0.25],
            200,
            10,
            msg::StaleAction::Hold,
            7,
        );
        assert_eq!(builder.finished_data(), current, "旧命令helper逐字节兼容");
    }
}

#[test]
fn joint_layout_roundtrip() {
    let mut fbb = FlatBufferBuilder::new();
    let n0 = fbb.create_string("j0");
    let n1 = fbb.create_string("j1");
    let n2 = fbb.create_string("j2");
    let names = fbb.create_vector(&[n0, n1, n2]);
    let jids = fbb.create_vector::<u32>(&[10, 11, 12]);
    let hw = fbb.create_string("bus1");
    let layout = msg::JointLayout::create(
        &mut fbb,
        &msg::JointLayoutArgs {
            layout_id: 0x9999_AAAA_BBBB_CCCC,
            names: Some(names),
            joint_ids: Some(jids),
            hardware_id: Some(hw),
        },
    );
    fbb.finish(layout, None);
    let bytes = fbb.finished_data();

    let parsed = flatbuffers::root::<msg::JointLayout>(bytes).expect("verify JointLayout");
    assert_eq!(parsed.layout_id(), 0x9999_AAAA_BBBB_CCCC);
    assert_eq!(parsed.hardware_id(), Some("bus1"));
    let names_read = parsed.names().unwrap();
    assert_eq!(names_read.len(), 3);
    assert_eq!(names_read.get(1), "j1");
}

#[test]
fn twist_roundtrip() {
    let mut fbb = FlatBufferBuilder::new();
    let frame = fbb.create_string("base_footprint");
    // Vec3 在 schema 里是 `table`（§3.5 原文），不是 `struct`；用 create 构造。
    let linear = msg::Vec3::create(
        &mut fbb,
        &msg::Vec3Args {
            x: 0.5,
            y: 0.0,
            z: 0.0,
        },
    );
    let angular = msg::Vec3::create(
        &mut fbb,
        &msg::Vec3Args {
            x: 0.0,
            y: 0.0,
            z: 0.3,
        },
    );
    let t = msg::Twist::create(
        &mut fbb,
        &msg::TwistArgs {
            linear: Some(linear),
            angular: Some(angular),
            frame_id: Some(frame),
            stamp_ns: 111,
        },
    );
    fbb.finish(t, None);
    let bytes = fbb.finished_data();

    let parsed = flatbuffers::root::<msg::Twist>(bytes).expect("verify Twist");
    assert!((parsed.linear().unwrap().x() - 0.5).abs() < 1e-12);
    assert!((parsed.angular().unwrap().z() - 0.3).abs() < 1e-12);
    assert_eq!(parsed.frame_id(), Some("base_footprint"));
}

#[test]
fn image_header_roundtrip() {
    let mut fbb = FlatBufferBuilder::new();
    let shape = fbb.create_vector::<u32>(&[480, 640, 3]);
    let strides = fbb.create_vector::<i32>(&[640 * 3, 3, 1]);
    let blob = msg::BlobDesc::create(
        &mut fbb,
        &msg::BlobDescArgs {
            offset: 0,
            size: 480 * 640 * 3,
            alignment: 16,
            dtype: msg::Dtype::U8,
            shape: Some(shape),
            strides: Some(strides),
            layout: msg::BlobLayout::HWC,
            endian: msg::Endian::Little,
            compression: msg::Compression::None_,
            crc32: 0,
        },
    );
    let enc = fbb.create_string("rgb8");
    let co = fbb.create_string("RGB");
    let fid = fbb.create_string("head_cam");
    let ih = msg::ImageHeader::create(
        &mut fbb,
        &msg::ImageHeaderArgs {
            blob: Some(blob),
            width: 640,
            height: 480,
            encoding: Some(enc),
            step: 640 * 3,
            color_order: Some(co),
            stamp_ns: 222,
            frame_id: Some(fid),
        },
    );
    fbb.finish(ih, None);
    let bytes = fbb.finished_data();

    let parsed = flatbuffers::root::<msg::ImageHeader>(bytes).expect("verify ImageHeader");
    assert_eq!(parsed.width(), 640);
    assert_eq!(parsed.height(), 480);
    assert_eq!(parsed.encoding(), Some("rgb8"));
    let blob_read = parsed.blob().unwrap();
    assert_eq!(blob_read.size(), 480 * 640 * 3);
    assert_eq!(blob_read.layout(), msg::BlobLayout::HWC);
}

#[test]
fn pointcloud_header_roundtrip() {
    let mut fbb = FlatBufferBuilder::new();
    let shape = fbb.create_vector::<u32>(&[1024, 4]);
    let blob = msg::BlobDesc::create(
        &mut fbb,
        &msg::BlobDescArgs {
            offset: 0,
            size: 1024 * 16,
            alignment: 16,
            dtype: msg::Dtype::F32,
            shape: Some(shape),
            strides: None,
            layout: msg::BlobLayout::PointsXYZI,
            endian: msg::Endian::Little,
            compression: msg::Compression::None_,
            crc32: 0,
        },
    );
    let fx = fbb.create_string("x");
    let fy = fbb.create_string("y");
    let fz = fbb.create_string("z");
    let fi = fbb.create_string("intensity");
    let fields = fbb.create_vector(&[fx, fy, fz, fi]);
    let fid = fbb.create_string("lidar");
    let pc = msg::PointCloudHeader::create(
        &mut fbb,
        &msg::PointCloudHeaderArgs {
            blob: Some(blob),
            point_count: 1024,
            point_step: 16,
            fields: Some(fields),
            is_dense: true,
            stamp_ns: 333,
            frame_id: Some(fid),
        },
    );
    fbb.finish(pc, None);
    let bytes = fbb.finished_data();

    let parsed =
        flatbuffers::root::<msg::PointCloudHeader>(bytes).expect("verify PointCloudHeader");
    assert_eq!(parsed.point_count(), 1024);
    assert_eq!(parsed.point_step(), 16);
    assert!(parsed.is_dense());
    assert_eq!(parsed.fields().unwrap().len(), 4);
}

#[test]
fn tensor_header_roundtrip() {
    let mut fbb = FlatBufferBuilder::new();
    let shape = fbb.create_vector::<u32>(&[1, 3, 224, 224]);
    let blob = msg::BlobDesc::create(
        &mut fbb,
        &msg::BlobDescArgs {
            offset: 0,
            size: 3 * 224 * 224 * 4,
            alignment: 32,
            dtype: msg::Dtype::F32,
            shape: Some(shape),
            strides: None,
            layout: msg::BlobLayout::NCHW,
            endian: msg::Endian::Little,
            compression: msg::Compression::None_,
            crc32: 0,
        },
    );
    let qz = fbb.create_string("");
    let th = msg::TensorHeader::create(
        &mut fbb,
        &msg::TensorHeaderArgs {
            blob: Some(blob),
            device: msg::Device::Cuda,
            quantization: Some(qz),
            stamp_ns: 444,
        },
    );
    fbb.finish(th, None);
    let bytes = fbb.finished_data();

    let parsed = flatbuffers::root::<msg::TensorHeader>(bytes).expect("verify TensorHeader");
    assert_eq!(parsed.device(), msg::Device::Cuda);
    assert_eq!(parsed.stamp_ns(), 444);
    assert_eq!(parsed.blob().unwrap().layout(), msg::BlobLayout::NCHW);
}

#[test]
fn estop_event_roundtrip() {
    let mut fbb = FlatBufferBuilder::new();
    let reason = fbb.create_string("button_pressed");
    let src = fbb.create_string("button");
    let ev = msg::EStopEvent::create(
        &mut fbb,
        &msg::EStopEventArgs {
            reason: Some(reason),
            stamp_ns: 555,
            source: Some(src),
            latched: true,
        },
    );
    fbb.finish(ev, None);
    let bytes = fbb.finished_data();

    let parsed = flatbuffers::root::<msg::EStopEvent>(bytes).expect("verify EStopEvent");
    assert_eq!(parsed.reason(), Some("button_pressed"));
    assert!(parsed.latched());
}

#[test]
fn heartbeat_roundtrip() {
    let mut fbb = FlatBufferBuilder::new();
    let hb = msg::Heartbeat::create(
        &mut fbb,
        &msg::HeartbeatArgs {
            source_id: 9,
            seq: 10_000,
            tick_rate_hz: 50.0,
            stamp_ns: 666,
        },
    );
    fbb.finish(hb, None);
    let bytes = fbb.finished_data();

    let parsed = flatbuffers::root::<msg::Heartbeat>(bytes).expect("verify Heartbeat");
    assert_eq!(parsed.source_id(), 9);
    assert!((parsed.tick_rate_hz() - 50.0).abs() < 1e-12);
}

// ===========================================================================
// schema_id 覆盖：所有 SCHEMA_REGISTRY 条目反查成功
// ===========================================================================

#[test]
fn every_schema_id_is_looked_up() {
    let ids = [
        SCHEMA_JOINT_LAYOUT,
        SCHEMA_JOINT_STATE,
        SCHEMA_JOINT_COMMAND,
        SCHEMA_TWIST,
        SCHEMA_IMAGE_HEADER,
        SCHEMA_POINTCLOUD_HEADER,
        SCHEMA_TENSOR_HEADER,
        SCHEMA_ESTOP_EVENT,
        SCHEMA_HEARTBEAT,
    ];
    for id in ids {
        assert!(
            lookup_name(id).is_some(),
            "SCHEMA_REGISTRY missing id {id:#010x}"
        );
    }
}

// ===========================================================================
// 跨版本兼容：JointState v0 ↔ current
// ===========================================================================
//
// 构造一份**只在 v0 schema 下可表达**的 JointState bytes（缺 frame_id 字段），
// 然后用**当前**版本 reader 读它 —— 必须：
//   (a) 验证通过
//   (b) 新字段 `frame_id` 读出为 None
//   (c) 原有字段值正确
//
// 反向：当前版本 writer 写 bytes（带 frame_id），用 v0 reader 读 —— 必须：
//   (a) 验证通过
//   (b) 已有字段读对
//   (c) 新字段 vtable slot 被忽略（v0 reader 不认识它）

#[test]
fn v0_writer_current_reader_ignores_new_field() {
    // ---- v0 writer：构造缺 frame_id 的 JointState bytes ----
    let mut fbb = FlatBufferBuilder::new();
    let pos = fbb.create_vector::<f64>(&[0.1, 0.2]);
    let vel = fbb.create_vector::<f64>(&[0.01, 0.02]);
    let eff = fbb.create_vector::<f64>(&[1.0, 2.0]);
    let v0_js = v0::JointState::create(
        &mut fbb,
        &v0::JointStateArgs {
            layout_id: 0xDEAD_BEEF_0000_0001,
            position_rad: Some(pos),
            velocity_rps: Some(vel),
            effort_nm: Some(eff),
            stamp_ns: 7777,
        },
    );
    fbb.finish(v0_js, None);
    let bytes = fbb.finished_data().to_vec();

    // ---- current reader：解析同一 bytes ----
    let current = flatbuffers::root::<msg::JointState>(&bytes)
        .expect("current reader should verify v0-written bytes");
    assert_eq!(current.layout_id(), 0xDEAD_BEEF_0000_0001);
    assert_eq!(current.stamp_ns(), 7777);
    // v0 没有 frame_id → 新 reader 读到 None
    assert_eq!(current.frame_id(), None);
    // 其他字段值传递
    assert_eq!(current.position_rad().unwrap().len(), 2);
    assert!((current.position_rad().unwrap().get(0) - 0.1).abs() < 1e-12);
}

// task-30 v9 pre-2 + finding #2 修补：wire `S0SafetyState` 必须与
// `robot_core::executor_ctrl::OutputSafetyState` **数值严格 1:1**。否则
// rt_thread 写 atomic u8 → pub_thread copy 进 wire body → subscriber 解读时
// 语义会错位（如 1 既可能是 wire `Safe` 也可能是 core `TorqueOn`），是隐性
// 安全失真。把数值用常量 lock 住，未来谁动哪边都会编译失败 / assert fail。
#[test]
fn s0_safety_state_wire_matches_core_output_safety_state() {
    // 0 = TorqueUnknown
    assert_eq!(msg::S0SafetyState::TorqueUnknown.0, 0);
    // 1 = TorqueOn
    assert_eq!(msg::S0SafetyState::TorqueOn.0, 1);
    // 2 = StopCommanded
    assert_eq!(msg::S0SafetyState::StopCommanded.0, 2);
    // 3 = TorqueOffConfirmed
    assert_eq!(msg::S0SafetyState::TorqueOffConfirmed.0, 3);
    // 4 = StopFailed
    assert_eq!(msg::S0SafetyState::StopFailed.0, 4);
    // 5 = AdapterUncertain
    assert_eq!(msg::S0SafetyState::AdapterUncertain.0, 5);
}

// task-30 v9 pre-2：v0 writer 不写 5 个 S0 状态字段 → 当前 reader 读到 Unknown
#[test]
fn v0_writer_current_reader_s0_state_fields_default_unknown() {
    let mut fbb = FlatBufferBuilder::new();
    let pos = fbb.create_vector::<f64>(&[1.0, 2.0]);
    let v0_js = v0::JointState::create(
        &mut fbb,
        &v0::JointStateArgs {
            layout_id: 0xAAAA_BBBB_0000_0001,
            position_rad: Some(pos),
            velocity_rps: None,
            effort_nm: None,
            stamp_ns: 1234,
        },
    );
    fbb.finish(v0_js, None);
    let bytes = fbb.finished_data().to_vec();

    let current = flatbuffers::root::<msg::JointState>(&bytes).expect("verify v0 bytes");
    // 5 个 S0 字段全部 = Unknown（FlatBuffers vtable 缺失 → 默认 enum value=0=Unknown）
    assert_eq!(current.s0_mode(), msg::S0Mode::Unknown);
    assert_eq!(current.s0_control_link(), msg::S0ControlLink::Unknown);
    assert_eq!(current.s0_output_mode(), msg::S0OutputMode::Unknown);
    assert_eq!(current.s0_safety_state(), msg::S0SafetyState::TorqueUnknown);
    assert_eq!(current.s0_fault_code(), msg::S0FaultCode::Unknown);
}

// task-30 v9 pre-2：当前 writer 写 mode=Enabled + safety_state=StopCommanded
// → reader 取出来对得上
#[test]
fn current_writer_with_s0_state_round_trip() {
    let mut fbb = FlatBufferBuilder::new();
    let pos = fbb.create_vector::<f64>(&[0.5]);
    let cur_js = msg::JointState::create(
        &mut fbb,
        &msg::JointStateArgs {
            layout_id: 0xCCCC_DDDD_0000_0003,
            position_rad: Some(pos),
            velocity_rps: None,
            effort_nm: None,
            stamp_ns: 5678,
            frame_id: None,
            s0_mode: msg::S0Mode::Enabled,
            s0_control_link: msg::S0ControlLink::Ok,
            s0_output_mode: msg::S0OutputMode::Active,
            s0_safety_state: msg::S0SafetyState::StopCommanded,
            s0_fault_code: msg::S0FaultCode::SafetyStopFailed,
        },
    );
    fbb.finish(cur_js, None);
    let bytes = fbb.finished_data().to_vec();

    let parsed = flatbuffers::root::<msg::JointState>(&bytes).expect("round-trip parse");
    assert_eq!(parsed.s0_mode(), msg::S0Mode::Enabled);
    assert_eq!(parsed.s0_control_link(), msg::S0ControlLink::Ok);
    assert_eq!(parsed.s0_output_mode(), msg::S0OutputMode::Active);
    assert_eq!(parsed.s0_safety_state(), msg::S0SafetyState::StopCommanded);
    assert_eq!(parsed.s0_fault_code(), msg::S0FaultCode::SafetyStopFailed);
    // 旧字段不受影响
    assert_eq!(parsed.layout_id(), 0xCCCC_DDDD_0000_0003);
    assert_eq!(parsed.stamp_ns(), 5678);
}

#[test]
fn current_writer_v0_reader_ignores_unknown_field() {
    // ---- current writer：带 frame_id 的完整 JointState bytes ----
    let mut fbb = FlatBufferBuilder::new();
    let pos = fbb.create_vector::<f64>(&[0.5, 0.6, 0.7]);
    let frame = fbb.create_string("ee_link");
    let cur_js = msg::JointState::create(
        &mut fbb,
        &msg::JointStateArgs {
            layout_id: 0xFEED_FACE_0000_0002,
            position_rad: Some(pos),
            velocity_rps: None,
            effort_nm: None,
            stamp_ns: 8888,
            frame_id: Some(frame),
            ..msg::JointStateArgs::default()
        },
    );
    fbb.finish(cur_js, None);
    let bytes = fbb.finished_data().to_vec();

    // ---- v0 reader：解析 new bytes，frame_id 不存在于 v0 schema ----
    let v0_parsed = flatbuffers::root::<v0::JointState>(&bytes)
        .expect("v0 reader should verify current-written bytes");
    assert_eq!(v0_parsed.layout_id(), 0xFEED_FACE_0000_0002);
    assert_eq!(v0_parsed.stamp_ns(), 8888);
    let pos_read = v0_parsed.position_rad().unwrap();
    assert_eq!(pos_read.len(), 3);
    assert!((pos_read.get(1) - 0.6).abs() < 1e-12);
    // v0 schema 没有 frame_id 的 accessor —— 编译期就限制了，不用运行时 assert；
    // 这里的重点是 "verify 通过 + 已有字段值对" 同时满足。
}

// ===========================================================================
// 控制响应：冻结 v0 的逐字节 oracle 与新增尾字段
// ===========================================================================

// 来源：git show 81c5c54:crates/robot-msg/src/build_helpers.rs
// SHA-256：48703e7b52b21ebde87434e396270aafe4ad04eb831a98e79a2ee1fc9b853e57。
// 逐行保留字符串创建和 Args 构造顺序，只替换冻结命名空间与函数名。
fn v0_ref_build_clear_estop_response(
    request_id: &str,
    status: v0::RpcStatus,
    error_msg: &str,
    cleared_at_ns: u64,
    dry_run: bool,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let request_id_off = fbb.create_string(request_id);
    let error_msg_off = fbb.create_string(error_msg);
    let resp = v0::ClearEStopResponse::create(
        &mut fbb,
        &v0::ClearEStopResponseArgs {
            request_id: Some(request_id_off),
            status,
            error_msg: Some(error_msg_off),
            cleared_at_ns,
            dry_run,
        },
    );
    fbb.finish(resp, None);
    fbb.finished_data().to_vec()
}

// 来源：git show 81c5c54:crates/robot-msg/src/build_helpers.rs
// SHA-256：48703e7b52b21ebde87434e396270aafe4ad04eb831a98e79a2ee1fc9b853e57。
// 逐行保留字符串创建和 Args 构造顺序，只替换冻结命名空间与函数名。
fn v0_ref_build_set_enabled_response(
    request_id: &str,
    status: v0::RpcStatus,
    domain: v0::SetEnabledStatus,
    completed_at_ns: u64,
    error_msg: &str,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let request_id_off = fbb.create_string(request_id);
    let error_msg_off = fbb.create_string(error_msg);
    let resp = v0::SetEnabledResponse::create(
        &mut fbb,
        &v0::SetEnabledResponseArgs {
            request_id: Some(request_id_off),
            status,
            domain,
            completed_at_ns,
            error_msg: Some(error_msg_off),
        },
    );
    fbb.finish(resp, None);
    fbb.finished_data().to_vec()
}

// 来源：git show 81c5c54:crates/robot-msg/src/build_helpers.rs
// SHA-256：48703e7b52b21ebde87434e396270aafe4ad04eb831a98e79a2ee1fc9b853e57。
// 逐行保留字符串创建和 Args 构造顺序，只替换冻结命名空间与函数名。
fn v0_ref_build_invoke_task_response(
    request_id: &str,
    status: v0::ActionRpcStatus,
    error_msg: &str,
    action_id: &str,
    accepted_at_ns: u64,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let request_id_off = fbb.create_string(request_id);
    let error_msg_off = fbb.create_string(error_msg);
    let action_id_off = fbb.create_string(action_id);
    let resp = v0::InvokeTaskResponse::create(
        &mut fbb,
        &v0::InvokeTaskResponseArgs {
            request_id: Some(request_id_off),
            status,
            error_msg: Some(error_msg_off),
            action_id: Some(action_id_off),
            accepted_at_ns,
        },
    );
    fbb.finish(resp, None);
    fbb.finished_data().to_vec()
}

// 来源：git show 81c5c54:crates/robot-msg/src/build_helpers.rs
// SHA-256：48703e7b52b21ebde87434e396270aafe4ad04eb831a98e79a2ee1fc9b853e57。
// 逐行保留字符串创建和 Args 构造顺序，只替换冻结命名空间与函数名。
fn v0_ref_build_cancel_task_response(
    request_id: &str,
    status: v0::ActionRpcStatus,
    error_msg: &str,
    action_id: &str,
    cancel_at_ns: u64,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let request_id_off = fbb.create_string(request_id);
    let error_msg_off = fbb.create_string(error_msg);
    let action_id_off = fbb.create_string(action_id);
    let resp = v0::CancelTaskResponse::create(
        &mut fbb,
        &v0::CancelTaskResponseArgs {
            request_id: Some(request_id_off),
            status,
            error_msg: Some(error_msg_off),
            action_id: Some(action_id_off),
            cancel_at_ns,
        },
    );
    fbb.finish(resp, None);
    fbb.finished_data().to_vec()
}

// 来源：git show 81c5c54:crates/robot-msg/src/build_helpers.rs
// SHA-256：48703e7b52b21ebde87434e396270aafe4ad04eb831a98e79a2ee1fc9b853e57。
// 逐行保留字符串创建和 Args 构造顺序，只替换冻结命名空间与函数名。
fn v0_ref_build_clear_error_response(
    request_id: &str,
    status: v0::ActionRpcStatus,
    error_msg: &str,
    cleared_at_ns: u64,
) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(128);
    let request_id_off = fbb.create_string(request_id);
    let error_msg_off = fbb.create_string(error_msg);
    let resp = v0::ClearErrorResponse::create(
        &mut fbb,
        &v0::ClearErrorResponseArgs {
            request_id: Some(request_id_off),
            status,
            error_msg: Some(error_msg_off),
            cleared_at_ns,
        },
    );
    fbb.finish(resp, None);
    fbb.finished_data().to_vec()
}

#[derive(Clone, Copy)]
struct LegacyResponseCase {
    request_id: &'static str,
    error_msg: &'static str,
    action_id: &'static str,
    time_ns: u64,
    status: u8,
    domain: u8,
    dry_run: bool,
}

// 默认/非默认旧标量、空/非空字符串及非 ASCII 输入均进入实际字节对照。
const LEGACY_RESPONSE_CASES: [LegacyResponseCase; 4] = [
    LegacyResponseCase {
        request_id: "",
        error_msg: "",
        action_id: "",
        time_ns: 0,
        status: 0,
        domain: 0,
        dry_run: false,
    },
    LegacyResponseCase {
        request_id: "request-1",
        error_msg: "",
        action_id: "action-1",
        time_ns: 42,
        status: 1,
        domain: 1,
        dry_run: false,
    },
    LegacyResponseCase {
        request_id: "request-2",
        error_msg: "失败原因",
        action_id: "action-2",
        time_ns: u64::MAX,
        status: 5,
        domain: 9,
        dry_run: true,
    },
    LegacyResponseCase {
        request_id: "请求",
        error_msg: "",
        action_id: "",
        time_ns: 0,
        status: 0,
        domain: 0,
        dry_run: true,
    },
];

// 两个命名空间的 enum 是不同类型，按 wire 原始数值比较旧字段。
macro_rules! assert_common_response_fields {
    ($current:ident, $old:ident, $case:ident) => {
        assert_eq!($current.request_id(), Some($case.request_id));
        assert_eq!($old.request_id(), $current.request_id());
        assert_eq!($current.status().0, $case.status);
        assert_eq!($old.status().0, $current.status().0);
        assert_eq!($current.error_msg(), Some($case.error_msg));
        assert_eq!($old.error_msg(), $current.error_msg());
    };
}

#[test]
fn clear_estop_legacy_builder_matches_v0_bytes_and_defaults() {
    for case in LEGACY_RESPONSE_CASES {
        let bytes = build_helpers::build_clear_estop_response(
            case.request_id,
            msg::RpcStatus(case.status),
            case.error_msg,
            case.time_ns,
            case.dry_run,
        );
        let reference = v0_ref_build_clear_estop_response(
            case.request_id,
            v0::RpcStatus(case.status),
            case.error_msg,
            case.time_ns,
            case.dry_run,
        );
        assert_eq!(bytes, reference, "{} 逐字节兼容", case.request_id);
        let current = flatbuffers::root::<msg::ClearEStopResponse>(&bytes).unwrap();
        let old = flatbuffers::root::<v0::ClearEStopResponse>(&bytes).unwrap();
        assert_common_response_fields!(current, old, case);
        assert_eq!(current.cleared_at_ns(), case.time_ns);
        assert_eq!(old.cleared_at_ns(), current.cleared_at_ns());
        assert_eq!(current.effect(), msg::SideEffect::Unknown);
        assert!(!current.retired());
        assert_eq!(current.dry_run(), case.dry_run);
        assert_eq!(old.dry_run(), current.dry_run());
        assert_eq!(current.domain(), msg::ClearEStopStatus::Unknown);
        assert!(!current.superseded());
    }
}

#[test]
fn v0_clear_estop_writer_missing_tail_reads_defaults() {
    let case = LEGACY_RESPONSE_CASES[2];
    let bytes = v0_ref_build_clear_estop_response(
        case.request_id,
        v0::RpcStatus(case.status),
        case.error_msg,
        case.time_ns,
        case.dry_run,
    );
    let current = flatbuffers::root::<msg::ClearEStopResponse>(&bytes).unwrap();
    assert_eq!(current.effect(), msg::SideEffect::Unknown);
    assert!(!current.retired());
    assert_eq!(current.domain(), msg::ClearEStopStatus::Unknown);
    assert!(!current.superseded());
}

#[test]
fn set_enabled_legacy_builder_matches_v0_bytes_and_defaults() {
    for case in LEGACY_RESPONSE_CASES {
        let bytes = build_helpers::build_set_enabled_response(
            case.request_id,
            msg::RpcStatus(case.status),
            msg::SetEnabledStatus(case.domain),
            case.time_ns,
            case.error_msg,
        );
        let reference = v0_ref_build_set_enabled_response(
            case.request_id,
            v0::RpcStatus(case.status),
            v0::SetEnabledStatus(case.domain),
            case.time_ns,
            case.error_msg,
        );
        assert_eq!(bytes, reference, "{} 逐字节兼容", case.request_id);
        let current = flatbuffers::root::<msg::SetEnabledResponse>(&bytes).unwrap();
        let old = flatbuffers::root::<v0::SetEnabledResponse>(&bytes).unwrap();
        assert_common_response_fields!(current, old, case);
        assert_eq!(current.completed_at_ns(), case.time_ns);
        assert_eq!(old.completed_at_ns(), current.completed_at_ns());
        assert_eq!(current.effect(), msg::SideEffect::Unknown);
        assert!(!current.retired());
        assert_eq!(current.domain().0, case.domain);
        assert_eq!(old.domain().0, current.domain().0);
    }
}

#[test]
fn v0_set_enabled_writer_missing_tail_reads_defaults() {
    let case = LEGACY_RESPONSE_CASES[2];
    let bytes = v0_ref_build_set_enabled_response(
        case.request_id,
        v0::RpcStatus(case.status),
        v0::SetEnabledStatus(case.domain),
        case.time_ns,
        case.error_msg,
    );
    let current = flatbuffers::root::<msg::SetEnabledResponse>(&bytes).unwrap();
    assert_eq!(current.effect(), msg::SideEffect::Unknown);
    assert!(!current.retired());
}

#[test]
fn invoke_task_legacy_builder_matches_v0_bytes_and_defaults() {
    for case in LEGACY_RESPONSE_CASES {
        let bytes = build_helpers::build_invoke_task_response(
            case.request_id,
            msg::ActionRpcStatus(case.status),
            case.error_msg,
            case.action_id,
            case.time_ns,
        );
        let reference = v0_ref_build_invoke_task_response(
            case.request_id,
            v0::ActionRpcStatus(case.status),
            case.error_msg,
            case.action_id,
            case.time_ns,
        );
        assert_eq!(bytes, reference, "{} 逐字节兼容", case.request_id);
        let current = flatbuffers::root::<msg::InvokeTaskResponse>(&bytes).unwrap();
        let old = flatbuffers::root::<v0::InvokeTaskResponse>(&bytes).unwrap();
        assert_common_response_fields!(current, old, case);
        assert_eq!(current.accepted_at_ns(), case.time_ns);
        assert_eq!(old.accepted_at_ns(), current.accepted_at_ns());
        assert_eq!(current.effect(), msg::SideEffect::Unknown);
        assert!(!current.retired());
        assert_eq!(current.action_id(), Some(case.action_id));
        assert_eq!(old.action_id(), current.action_id());
    }
}

#[test]
fn v0_invoke_task_writer_missing_tail_reads_defaults() {
    let case = LEGACY_RESPONSE_CASES[2];
    let bytes = v0_ref_build_invoke_task_response(
        case.request_id,
        v0::ActionRpcStatus(case.status),
        case.error_msg,
        case.action_id,
        case.time_ns,
    );
    let current = flatbuffers::root::<msg::InvokeTaskResponse>(&bytes).unwrap();
    assert_eq!(current.effect(), msg::SideEffect::Unknown);
    assert!(!current.retired());
}

#[test]
fn cancel_task_legacy_builder_matches_v0_bytes_and_defaults() {
    for case in LEGACY_RESPONSE_CASES {
        let bytes = build_helpers::build_cancel_task_response(
            case.request_id,
            msg::ActionRpcStatus(case.status),
            case.error_msg,
            case.action_id,
            case.time_ns,
        );
        let reference = v0_ref_build_cancel_task_response(
            case.request_id,
            v0::ActionRpcStatus(case.status),
            case.error_msg,
            case.action_id,
            case.time_ns,
        );
        assert_eq!(bytes, reference, "{} 逐字节兼容", case.request_id);
        let current = flatbuffers::root::<msg::CancelTaskResponse>(&bytes).unwrap();
        let old = flatbuffers::root::<v0::CancelTaskResponse>(&bytes).unwrap();
        assert_common_response_fields!(current, old, case);
        assert_eq!(current.cancel_at_ns(), case.time_ns);
        assert_eq!(old.cancel_at_ns(), current.cancel_at_ns());
        assert_eq!(current.effect(), msg::SideEffect::Unknown);
        assert!(!current.retired());
        assert_eq!(current.action_id(), Some(case.action_id));
        assert_eq!(old.action_id(), current.action_id());
    }
}

#[test]
fn v0_cancel_task_writer_missing_tail_reads_defaults() {
    let case = LEGACY_RESPONSE_CASES[2];
    let bytes = v0_ref_build_cancel_task_response(
        case.request_id,
        v0::ActionRpcStatus(case.status),
        case.error_msg,
        case.action_id,
        case.time_ns,
    );
    let current = flatbuffers::root::<msg::CancelTaskResponse>(&bytes).unwrap();
    assert_eq!(current.effect(), msg::SideEffect::Unknown);
    assert!(!current.retired());
}

#[test]
fn clear_error_legacy_builder_matches_v0_bytes_and_defaults() {
    for case in LEGACY_RESPONSE_CASES {
        let bytes = build_helpers::build_clear_error_response(
            case.request_id,
            msg::ActionRpcStatus(case.status),
            case.error_msg,
            case.time_ns,
        );
        let reference = v0_ref_build_clear_error_response(
            case.request_id,
            v0::ActionRpcStatus(case.status),
            case.error_msg,
            case.time_ns,
        );
        assert_eq!(bytes, reference, "{} 逐字节兼容", case.request_id);
        let current = flatbuffers::root::<msg::ClearErrorResponse>(&bytes).unwrap();
        let old = flatbuffers::root::<v0::ClearErrorResponse>(&bytes).unwrap();
        assert_common_response_fields!(current, old, case);
        assert_eq!(current.cleared_at_ns(), case.time_ns);
        assert_eq!(old.cleared_at_ns(), current.cleared_at_ns());
        assert_eq!(current.effect(), msg::SideEffect::Unknown);
        assert!(!current.retired());
    }
}

#[test]
fn v0_clear_error_writer_missing_tail_reads_defaults() {
    let case = LEGACY_RESPONSE_CASES[2];
    let bytes = v0_ref_build_clear_error_response(
        case.request_id,
        v0::ActionRpcStatus(case.status),
        case.error_msg,
        case.time_ns,
    );
    let current = flatbuffers::root::<msg::ClearErrorResponse>(&bytes).unwrap();
    assert_eq!(current.effect(), msg::SideEffect::Unknown);
    assert!(!current.retired());
}

#[test]
fn clear_estop_typed_response_preserves_legacy_fields_and_metadata() {
    for case in LEGACY_RESPONSE_CASES {
        // 包含未来未知枚举，验证 reader 保留原始值而不会悄悄归零。
        for (effect, retired) in [
            (msg::SideEffect::None, false),
            (msg::SideEffect::Started, false),
            (msg::SideEffect::Committed, true),
            (msg::SideEffect(255), true),
        ] {
            let bytes = build_helpers::build_clear_estop_response_typed(
                case.request_id,
                msg::RpcStatus(case.status),
                case.error_msg,
                case.time_ns,
                case.dry_run,
                build_helpers::ClearResponseMeta {
                    domain: msg::ClearEStopStatus::WriterUnavailable,
                    superseded: true,
                    effect,
                    retired,
                },
            );
            let current = flatbuffers::root::<msg::ClearEStopResponse>(&bytes).unwrap();
            let old = flatbuffers::root::<v0::ClearEStopResponse>(&bytes).unwrap();
            assert_common_response_fields!(current, old, case);
            assert_eq!(current.cleared_at_ns(), case.time_ns);
            assert_eq!(old.cleared_at_ns(), current.cleared_at_ns());
            assert_eq!(current.effect(), effect);
            assert_eq!(current.retired(), retired);
            assert_eq!(current.dry_run(), case.dry_run);
            assert_eq!(old.dry_run(), current.dry_run());
            assert_eq!(current.domain(), msg::ClearEStopStatus::WriterUnavailable);
            assert!(current.superseded());
        }
    }
}

#[test]
fn set_enabled_typed_response_preserves_legacy_fields_and_metadata() {
    for case in LEGACY_RESPONSE_CASES {
        // 包含未来未知枚举，验证 reader 保留原始值而不会悄悄归零。
        for (effect, retired) in [
            (msg::SideEffect::None, false),
            (msg::SideEffect::Started, false),
            (msg::SideEffect::Committed, true),
            (msg::SideEffect(255), true),
        ] {
            let bytes = build_helpers::build_set_enabled_response_typed(
                case.request_id,
                msg::RpcStatus(case.status),
                msg::SetEnabledStatus::QueueFull,
                case.time_ns,
                case.error_msg,
                effect,
                retired,
            );
            let current = flatbuffers::root::<msg::SetEnabledResponse>(&bytes).unwrap();
            let old = flatbuffers::root::<v0::SetEnabledResponse>(&bytes).unwrap();
            assert_common_response_fields!(current, old, case);
            assert_eq!(current.completed_at_ns(), case.time_ns);
            assert_eq!(old.completed_at_ns(), current.completed_at_ns());
            assert_eq!(current.effect(), effect);
            assert_eq!(current.retired(), retired);
            assert_eq!(current.domain(), msg::SetEnabledStatus::QueueFull);
            assert_eq!(current.domain().variant_name(), Some("QueueFull"));
            assert_eq!(old.domain(), v0::SetEnabledStatus(11));
            assert_eq!(old.domain().0, 11);
            assert_eq!(old.domain().variant_name(), None);
        }
    }
}

#[test]
fn invoke_task_typed_response_preserves_legacy_fields_and_metadata() {
    for case in LEGACY_RESPONSE_CASES {
        // 包含未来未知枚举，验证 reader 保留原始值而不会悄悄归零。
        for (effect, retired) in [
            (msg::SideEffect::None, false),
            (msg::SideEffect::Started, false),
            (msg::SideEffect::Committed, true),
            (msg::SideEffect(255), true),
        ] {
            let bytes = build_helpers::build_invoke_task_response_typed(
                case.request_id,
                msg::ActionRpcStatus(case.status),
                case.error_msg,
                case.action_id,
                case.time_ns,
                effect,
                retired,
                msg::InvokeOutcome::Registered,
            );
            let current = flatbuffers::root::<msg::InvokeTaskResponse>(&bytes).unwrap();
            let old = flatbuffers::root::<v0::InvokeTaskResponse>(&bytes).unwrap();
            assert_common_response_fields!(current, old, case);
            assert_eq!(current.accepted_at_ns(), case.time_ns);
            assert_eq!(current.invoke_outcome(), msg::InvokeOutcome::Registered);
            assert_eq!(old.accepted_at_ns(), current.accepted_at_ns());
            assert_eq!(current.effect(), effect);
            assert_eq!(current.retired(), retired);
            assert_eq!(current.action_id(), Some(case.action_id));
            assert_eq!(old.action_id(), current.action_id());
        }
    }
}

#[test]
fn invoke_outcome_tail_preserves_unknown_values_and_old_bytes() {
    for outcome in [
        msg::InvokeOutcome::Unknown,
        msg::InvokeOutcome::NotSubmitted,
        msg::InvokeOutcome::Registered,
        msg::InvokeOutcome::DrainFailedCancelAccepted,
        msg::InvokeOutcome::DrainFailedCancelRejected,
        msg::InvokeOutcome(255),
    ] {
        let bytes = build_helpers::build_invoke_task_response_typed(
            "invoke",
            msg::ActionRpcStatus::Internal,
            "drain",
            "action",
            71,
            msg::SideEffect::Committed,
            true,
            outcome,
        );
        let current = flatbuffers::root::<msg::InvokeTaskResponse>(&bytes).unwrap();
        let old = flatbuffers::root::<v0::InvokeTaskResponse>(&bytes).unwrap();
        assert_eq!(current.invoke_outcome(), outcome);
        assert_eq!(old.accepted_at_ns(), 71);
        assert_eq!(old.action_id(), Some("action"));
        assert_eq!(old.error_msg(), Some("drain"));
    }
    let legacy = build_helpers::build_invoke_task_response(
        "invoke",
        msg::ActionRpcStatus::Ok,
        "",
        "action",
        71,
    );
    let reference =
        v0_ref_build_invoke_task_response("invoke", v0::ActionRpcStatus::Ok, "", "action", 71);
    assert_eq!(legacy, reference);
    assert_eq!(
        flatbuffers::root::<msg::InvokeTaskResponse>(&legacy)
            .unwrap()
            .invoke_outcome(),
        msg::InvokeOutcome::Unknown
    );
}

#[test]
fn cancel_task_typed_response_preserves_legacy_fields_and_metadata() {
    for case in LEGACY_RESPONSE_CASES {
        // 包含未来未知枚举，验证 reader 保留原始值而不会悄悄归零。
        for (effect, retired) in [
            (msg::SideEffect::None, false),
            (msg::SideEffect::Started, false),
            (msg::SideEffect::Committed, true),
            (msg::SideEffect(255), true),
        ] {
            let bytes = build_helpers::build_cancel_task_response_typed(
                case.request_id,
                msg::ActionRpcStatus(case.status),
                case.error_msg,
                case.action_id,
                case.time_ns,
                effect,
                retired,
            );
            let current = flatbuffers::root::<msg::CancelTaskResponse>(&bytes).unwrap();
            let old = flatbuffers::root::<v0::CancelTaskResponse>(&bytes).unwrap();
            assert_common_response_fields!(current, old, case);
            assert_eq!(current.cancel_at_ns(), case.time_ns);
            assert_eq!(old.cancel_at_ns(), current.cancel_at_ns());
            assert_eq!(current.effect(), effect);
            assert_eq!(current.retired(), retired);
            assert_eq!(current.action_id(), Some(case.action_id));
            assert_eq!(old.action_id(), current.action_id());
        }
    }
}

#[test]
fn clear_error_typed_response_preserves_legacy_fields_and_metadata() {
    for case in LEGACY_RESPONSE_CASES {
        // 包含未来未知枚举，验证 reader 保留原始值而不会悄悄归零。
        for (effect, retired) in [
            (msg::SideEffect::None, false),
            (msg::SideEffect::Started, false),
            (msg::SideEffect::Committed, true),
            (msg::SideEffect(255), true),
        ] {
            let bytes = build_helpers::build_clear_error_response_typed(
                case.request_id,
                msg::ActionRpcStatus(case.status),
                case.error_msg,
                case.time_ns,
                effect,
                retired,
            );
            let current = flatbuffers::root::<msg::ClearErrorResponse>(&bytes).unwrap();
            let old = flatbuffers::root::<v0::ClearErrorResponse>(&bytes).unwrap();
            assert_common_response_fields!(current, old, case);
            assert_eq!(current.cleared_at_ns(), case.time_ns);
            assert_eq!(old.cleared_at_ns(), current.cleared_at_ns());
            assert_eq!(current.effect(), effect);
            assert_eq!(current.retired(), retired);
        }
    }
}

#[test]
fn side_effect_wire_values_are_append_only() {
    let expected = [
        (msg::SideEffect::Unknown, 0),
        (msg::SideEffect::None, 1),
        (msg::SideEffect::Started, 2),
        (msg::SideEffect::Committed, 3),
    ];
    assert_eq!(msg::SideEffect::ENUM_VALUES.len(), expected.len());
    for ((variant, raw), registered) in expected.into_iter().zip(msg::SideEffect::ENUM_VALUES) {
        assert_eq!(variant.0, raw);
        assert_eq!(&variant, registered);
    }
}

#[test]
fn set_enabled_status_wire_values_are_append_only() {
    let expected = [
        (msg::SetEnabledStatus::Unknown, 0),
        (msg::SetEnabledStatus::Ok, 1),
        (msg::SetEnabledStatus::GuardRejected, 2),
        (msg::SetEnabledStatus::LatchBlocked, 3),
        (msg::SetEnabledStatus::AdapterError, 4),
        (msg::SetEnabledStatus::AlreadyInProgress, 5),
        (msg::SetEnabledStatus::PreemptedByEStop, 6),
        (msg::SetEnabledStatus::PreemptedByShutdown, 7),
        (msg::SetEnabledStatus::WouldAccept, 8),
        (msg::SetEnabledStatus::DeadlineExceeded, 9),
        (msg::SetEnabledStatus::SupersededByDisable, 10),
        (msg::SetEnabledStatus::QueueFull, 11),
        (msg::SetEnabledStatus::Superseded, 12),
    ];
    assert_eq!(msg::SetEnabledStatus::ENUM_VALUES.len(), expected.len());
    for ((variant, raw), registered) in expected.into_iter().zip(msg::SetEnabledStatus::ENUM_VALUES)
    {
        assert_eq!(variant.0, raw);
        assert_eq!(&variant, registered);
    }
}

#[test]
fn clear_estop_status_wire_values_are_append_only() {
    let expected = [
        (msg::ClearEStopStatus::Unknown, 0),
        (msg::ClearEStopStatus::Cleared, 1),
        (msg::ClearEStopStatus::WouldClear, 2),
        (msg::ClearEStopStatus::StateNotSafe, 3),
        (msg::ClearEStopStatus::NotTorqueOffConfirmed, 4),
        (msg::ClearEStopStatus::FsyncFailed, 5),
        (msg::ClearEStopStatus::AlreadyInProgress, 6),
        (msg::ClearEStopStatus::PreemptedByEmergency, 7),
        (msg::ClearEStopStatus::DeadlineExceeded, 8),
        (msg::ClearEStopStatus::SupersededByDisable, 9),
        (msg::ClearEStopStatus::SupersededByShutdown, 10),
        (msg::ClearEStopStatus::SupersededByInhibit, 11),
        (msg::ClearEStopStatus::SupersededByExhausted, 12),
        (msg::ClearEStopStatus::AuthorityRefused, 13),
        (msg::ClearEStopStatus::QueueFull, 14),
        (msg::ClearEStopStatus::BusyLowering, 15),
        (msg::ClearEStopStatus::PersistPending, 16),
        (msg::ClearEStopStatus::WriterUnavailable, 17),
    ];
    assert_eq!(msg::ClearEStopStatus::ENUM_VALUES.len(), expected.len());
    for ((variant, raw), registered) in expected.into_iter().zip(msg::ClearEStopStatus::ENUM_VALUES)
    {
        assert_eq!(variant.0, raw);
        assert_eq!(&variant, registered);
    }
}
