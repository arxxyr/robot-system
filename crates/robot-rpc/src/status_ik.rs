//! 双侧腕点预览各自使用同侧总线的一帧反馈和 S1 运动约束。

use super::{Snapshot, WebState, feetech_raw};
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use robot_adapter_feetech::{convert::checked_position_target, raw_to_rad};
use robot_kinematics_xlerobot::{Side, arm_home_slots, preview::wrist_roundtrip};
use serde_json::{Value, json};

pub(super) fn preview(state: &WebState, side: Side) -> Response {
    let snapshot = state.latest.borrow().at_time(state.clock.now().0);
    match plan(&snapshot, side) {
        Ok(plan) => Json(plan).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": error })),
        )
            .into_response(),
    }
}

fn plan(snapshot: &Snapshot, side: Side) -> Result<Value, String> {
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
    let joint_names = slots.map(|slot| bus.joints[slot].name.as_str());
    let mut start_raw = [0; 5];
    let mut limits = Vec::with_capacity(joint_names.len());
    for (i, name) in joint_names.iter().enumerate() {
        start_raw[i] = bus.joints[slots[i]]
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
    let result = wrist_roundtrip(
        side,
        start_raw,
        std::array::from_fn(|i| limits[i].tolerance_rad),
        raw_to_rad,
        |i, target, start| {
            let limit = limits[i];
            let range = [limit.min_position_rad, limit.max_position_rad];
            checked_position_target(
                target,
                start,
                bus.control.max_duration_ms,
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
        "stamp_ns": stamp_ns,
        "joint_names": joint_names,
        "start": {"xyz_mm": result.origin.translation.map(|m| m * 1000.0), "joints_raw": start_raw},
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
