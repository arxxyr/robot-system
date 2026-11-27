//! 双侧腕点预览与回位参考核对读取所选侧的一帧反馈及 S1 运动约束。

use super::{JointView, Snapshot, WebState, control::JointLimit, feetech_raw};
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use robot_adapter_feetech::{
    convert::{checked_position_target, within_position_tolerance},
    rad_to_raw, raw_to_rad,
};
use robot_kinematics_xlerobot::{
    ARM_HOME_TARGET_RAD, Side, arm_home_slots, preview::wrist_roundtrip,
};
use serde_json::{Value, json};

pub(super) fn preview(state: &WebState, side: Side) -> Response {
    let snapshot = state.latest.borrow().at_time(state.clock.now().0);
    response(arm_frame(&snapshot, side).and_then(|frame| plan(&frame, side)))
}

pub(super) fn home_reference(state: &WebState, side: Side) -> Response {
    let snapshot = state.latest.borrow().at_time(state.clock.now().0);
    response(arm_frame(&snapshot, side).and_then(|frame| compare_home(&frame)))
}

fn response(result: Result<Value, String>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": error })),
        )
            .into_response(),
    }
}

struct ArmFrame<'a> {
    stamp_ns: &'a str,
    joints: [&'a JointView; 5],
    start_raw: [i32; 5],
    limits: Vec<&'a JointLimit>,
    max_duration_ms: u32,
}

fn arm_frame(snapshot: &Snapshot, side: Side) -> Result<ArmFrame<'_>, String> {
    let (bus_id, robot_id, label) = match side {
        Side::Left => ("bus1", "xlerobot_bus1_full", "左臂"),
        Side::Right => ("bus2", "xlerobot_bus2_full", "右臂"),
    };
    let bus = snapshot
        .buses
        .iter()
        .find(|bus| bus.id == bus_id)
        .ok_or_else(|| format!("尚无 {bus_id} 状态"))?;
    if bus.data_status != "fresh" {
        return Err(format!(
            "{label}反馈不可用（{}）{}",
            bus.data_status,
            bus.error.as_deref().unwrap_or("")
        ));
    }
    let mixed_raw = side == Side::Right;
    let modes_match = bus.joints.iter().all(|joint| {
        let expected = if mixed_raw && (7..=9).contains(&joint.motor_id) {
            "velocity_raw"
        } else {
            "position"
        };
        joint.mode == expected
    });
    if bus.robot_id != robot_id || !modes_match {
        return Err(format!("{bus_id} 的机器人身份或关节模式与{label}不符"));
    }
    let names: Vec<_> = bus.joints.iter().map(|joint| joint.name.clone()).collect();
    let motor_ids: Vec<_> = bus.joints.iter().map(|joint| joint.motor_id).collect();
    let slots = arm_home_slots(side, &bus.hardware_id, &names, &motor_ids, mixed_raw)
        .map_err(|reason| format!("{bus_id} {label}布局不符：{reason}"))?;
    let stamp_ns = bus
        .stamp_ns
        .as_deref()
        .ok_or_else(|| format!("{label}反馈缺少源帧时间戳"))?;
    let joints = slots.map(|slot| &bus.joints[slot]);
    let mut start_raw = [0; 5];
    let mut limits = Vec::with_capacity(joints.len());
    for (i, joint) in joints.iter().enumerate() {
        let name = &joint.name;
        start_raw[i] = joint
            .position_raw
            .ok_or_else(|| format!("{name} 缺少编码器位置反馈"))?;
        limits.push(
            bus.control
                .joints
                .iter()
                .find(|limit| limit.name == *name)
                .ok_or_else(|| format!("{name} 缺少 S1 运动约束"))?,
        );
    }
    Ok(ArmFrame {
        stamp_ns,
        joints,
        start_raw,
        limits,
        max_duration_ms: bus.control.max_duration_ms,
    })
}

fn compare_home(frame: &ArmFrame<'_>) -> Result<Value, String> {
    let target_raw = rad_to_raw(ARM_HOME_TARGET_RAD);
    let encoded_target = f64::from(raw_to_rad(target_raw));
    let mut joints = Vec::with_capacity(frame.joints.len());
    for (axis, joint) in frame.joints.iter().enumerate() {
        let position = joint
            .position_rad
            .ok_or_else(|| format!("{} 缺少位置角反馈", joint.name))?;
        let raw = frame.start_raw[axis];
        joints.push(json!({
            "name": joint.name,
            "position_raw": raw,
            "target_raw": target_raw,
            "error_raw": i64::from(raw) - i64::from(target_raw),
            "within_tolerance": within_position_tolerance(position, encoded_target, frame.limits[axis].tolerance_rad)
        }));
    }
    Ok(json!({ "stamp_ns": frame.stamp_ns, "joints": joints }))
}

fn plan(frame: &ArmFrame<'_>, side: Side) -> Result<Value, String> {
    let result = wrist_roundtrip(
        side,
        frame.start_raw,
        std::array::from_fn(|i| frame.limits[i].tolerance_rad),
        raw_to_rad,
        |i, target, start| {
            let limit = frame.limits[i];
            let range = [limit.min_position_rad, limit.max_position_rad];
            checked_position_target(
                target,
                start,
                frame.max_duration_ms,
                range,
                limit.max_velocity_rad_s,
            )
            .map_err(|reason| {
                let raw = |value| {
                    feetech_raw(value).map_or_else(|| "不可表示".to_owned(), |n| n.to_string())
                };
                format!(
                    "{}：{}（目标 {} raw，主动范围 {}…{} raw）",
                    limit.name,
                    reason,
                    raw(target),
                    raw(range[0]),
                    raw(range[1])
                )
            })
        },
    )?;
    Ok(json!({
        "stamp_ns": frame.stamp_ns,
        "joint_names": frame.joints.map(|joint| joint.name.as_str()),
        "start": {"xyz_mm": result.origin.translation.map(|m| m * 1000.0), "joints_raw": frame.start_raw},
        "target_xyz_mm": result.target.translation.map(|m| m * 1000.0),
        "forward": leg(Some(result.forward)),
        "return": leg(result.back),
    }))
}

fn leg(result: Option<Result<[i32; 5], String>>) -> Value {
    match result {
        Some(Ok(raw)) => json!({"joints_raw": raw, "error": null}),
        Some(Err(reason)) => json!({"joints_raw": null, "error": reason}),
        None => json!({"joints_raw": null, "error": null}),
    }
}
