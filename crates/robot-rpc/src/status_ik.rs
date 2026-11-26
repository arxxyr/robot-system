//! 固定右腕点预览只使用当前总线缓存和 S1 的运动配置。

use super::{Snapshot, WebState, feetech_raw};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use robot_adapter_feetech::{convert::checked_position_target, raw_to_rad};
use robot_kinematics_xlerobot::{preview::right_wrist_roundtrip, right_arm_joint_names};
use serde_json::{Value, json};

pub(super) async fn preview(State(state): State<WebState>) -> Response {
    let snapshot = state.latest.borrow().at_time(state.clock.now().0);
    match plan(&snapshot) {
        Ok(plan) => Json(plan).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": error })),
        )
            .into_response(),
    }
}

fn plan(snapshot: &Snapshot) -> Result<Value, String> {
    let bus = snapshot
        .buses
        .iter()
        .find(|bus| bus.id == "bus2")
        .ok_or("尚无 bus2 状态")?;
    if bus.data_status != "fresh" {
        return Err(format!(
            "右臂反馈不可用（{}）{}",
            bus.data_status,
            bus.error.as_deref().unwrap_or("")
        ));
    }
    if bus.robot_id != "xlerobot_bus2_full"
        || bus.hardware_id != "right_arm_full_with_base"
        || bus.joints.len() != 9
        || bus
            .joints
            .iter()
            .filter(|joint| joint.mode == "velocity_raw")
            .count()
            != 3
    {
        return Err("bus2 身份或布局与 XLeRobot 右臂九槽混合总线不符".into());
    }
    let stamp_ns = bus.stamp_ns.as_deref().ok_or("右臂反馈缺少源帧时间戳")?;
    let joint_names = right_arm_joint_names();
    let mut start_raw = [0; 5];
    let mut limits = Vec::with_capacity(joint_names.len());
    for (i, name) in joint_names.iter().enumerate() {
        start_raw[i] = bus
            .joints
            .iter()
            .find(|joint| joint.name == *name && joint.mode == "position")
            .and_then(|joint| joint.position_raw)
            .ok_or_else(|| format!("{name} 缺少编码器位置反馈"))?;
        limits.push(
            bus.control
                .joints
                .iter()
                .find(|limit| limit.name == *name)
                .ok_or_else(|| format!("{name} 缺少 S1 运动约束"))?,
        );
    }
    let result = right_wrist_roundtrip(
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
