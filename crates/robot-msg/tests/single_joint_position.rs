//! 单关节 payload 的必填身份、注册和原任务请求外壳兼容性。

use flatbuffers::FlatBufferBuilder;
use robot_msg::build_helpers::build_invoke_task_request;
use robot_msg::markers::{BodyMarker, SingleJointPositionPayload};
use robot_msg::msg;
use robot_msg::schema_id::{SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD, lookup_name};
use robot_msg::size_tier::{SizeTier, default_tier_of};

fn payload_bytes(hardware_present: bool, joint_present: bool) -> Vec<u8> {
    let mut builder = FlatBufferBuilder::new();
    let hardware_id = builder.create_string("hardware");
    let joint_name = builder.create_string("joint");
    // 直接构造 table，以验证 reader 拒绝缺失 required 字段的外来字节。
    let table = builder.start_table();
    if hardware_present {
        builder.push_slot_always(msg::SingleJointPositionPayload::VT_HARDWARE_ID, hardware_id);
    }
    if joint_present {
        builder.push_slot_always(msg::SingleJointPositionPayload::VT_JOINT_NAME, joint_name);
    }
    builder.push_slot(
        msg::SingleJointPositionPayload::VT_TARGET_POSITION_RAD,
        -0.25_f64,
        0.0,
    );
    builder.push_slot(msg::SingleJointPositionPayload::VT_DURATION_MS, 1000_u32, 0);
    let root = builder.end_table(table);
    builder.finish(root, None);
    builder.finished_data().to_vec()
}

#[test]
fn payload_requires_both_identity_fields() {
    for (hardware_present, joint_present) in [(false, false), (true, false), (false, true)] {
        let bytes = payload_bytes(hardware_present, joint_present);
        assert!(flatbuffers::root::<msg::SingleJointPositionPayload<'_>>(&bytes).is_err());
    }
    let bytes = payload_bytes(true, true);
    let payload =
        flatbuffers::root::<msg::SingleJointPositionPayload<'_>>(&bytes).expect("两项身份均存在");
    assert_eq!(payload.hardware_id(), "hardware");
    assert_eq!(payload.joint_name(), "joint");
    assert_eq!(
        payload.target_position_rad().to_bits(),
        (-0.25_f64).to_bits()
    );
    assert_eq!(payload.duration_ms(), 1000);
    assert_eq!(
        SingleJointPositionPayload::SCHEMA_ID,
        SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD
    );
    assert_eq!(
        lookup_name(SingleJointPositionPayload::SCHEMA_ID),
        Some(SingleJointPositionPayload::SCHEMA_NAME)
    );
    assert_eq!(
        default_tier_of(SingleJointPositionPayload::SCHEMA_ID),
        Some(SizeTier::Control)
    );
}

#[test]
fn old_request_reader_preserves_new_schema_and_payload() {
    let payload = payload_bytes(true, true);
    let bytes = build_invoke_task_request(
        "request",
        "idempotency",
        "source",
        msg::RpcRole::Operator,
        "离线测试",
        msg::ActionKindFb::Scripted,
        10_000,
        0,
        SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD,
        &payload,
        100,
    );
    let old = flatbuffers::root::<msg::v_0::InvokeTaskRequest<'_>>(&bytes)
        .expect("旧 reader 可读原任务外壳");
    assert_eq!(old.kind(), msg::v_0::ActionKindFb::Scripted);
    assert_eq!(
        old.payload_schema_id(),
        SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD
    );
    assert_eq!(old.payload().expect("payload 存在").bytes(), payload);
}

#[test]
fn old_request_writer_can_forward_new_payload_without_translation() {
    let payload = payload_bytes(true, true);
    let mut builder = FlatBufferBuilder::new();
    let body = builder.create_vector(&payload);
    let request = msg::v_0::InvokeTaskRequest::create(
        &mut builder,
        &msg::v_0::InvokeTaskRequestArgs {
            kind: msg::v_0::ActionKindFb::Scripted,
            payload_schema_id: SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD,
            payload: Some(body),
            ..Default::default()
        },
    );
    builder.finish(request, None);
    let current = flatbuffers::root::<msg::InvokeTaskRequest<'_>>(builder.finished_data())
        .expect("当前 reader 可读旧网关外壳");
    assert_eq!(current.kind(), msg::ActionKindFb::Scripted);
    assert_eq!(
        current.payload_schema_id(),
        SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD
    );
    assert_eq!(current.payload().expect("payload 存在").bytes(), payload);
}
