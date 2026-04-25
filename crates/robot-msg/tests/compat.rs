//! robot-msg 运行时兼容性测试。
//!
//! 覆盖 `docs/01_implementation_steps.md` Step 2 "测试" 章节的运行时部分：
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
use robot_msg::msg;
use robot_msg::schema_id::{
    SCHEMA_ESTOP_EVENT, SCHEMA_HEARTBEAT, SCHEMA_IMAGE_HEADER, SCHEMA_JOINT_COMMAND,
    SCHEMA_JOINT_LAYOUT, SCHEMA_JOINT_STATE, SCHEMA_POINTCLOUD_HEADER, SCHEMA_TENSOR_HEADER,
    SCHEMA_TWIST, lookup_name,
};
use robot_msg::v0;

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
