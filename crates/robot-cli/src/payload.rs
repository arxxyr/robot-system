//! invoke 子命令的 fbs payload 构造 + 输入解析（Q-CLI7(b)：inline 在
//! robot-cli lib，**不**抽到 `robot-msg/build_helpers`；沿
//! `scripted_backend_smoke` / `ik_backend_smoke` 同款 fbs 构造）。仅 (4/5)
//! invoke 用（payload-only，非 7 子命令共享）。

use flatbuffers::FlatBufferBuilder;
use robot_msg::msg::{
    IkPayload as FbIkPayload, IkPayloadArgs, ScriptedPayload as FbScriptedPayload,
    ScriptedPayloadArgs, WaypointTarget, WaypointTargetArgs,
};
use serde::Deserialize;

// ============================================================================
// scripted（AC-8）
// ============================================================================

/// 一个 scripted waypoint（JSON 解析中间态 / fbs 构造输入）。
#[derive(Debug)]
pub struct Waypoint {
    /// 各关节目标位（rad）；长度由 JSON 决定（CLI 仅 ragged 检查，关节数交
    /// server）。
    pub positions: Vec<f64>,
    /// 到点后驻留（ms）。
    pub dwell_ms: u32,
    /// 到该 waypoint 的插值时长（ms）。
    pub segment_duration_ms: u32,
}

#[derive(Deserialize)]
struct WaypointsFile {
    waypoints: Vec<WaypointJson>,
}

#[derive(Deserialize)]
struct WaypointJson {
    positions: Vec<f64>,
    dwell_ms: u32,
    segment_duration_ms: u32,
}

/// 解析 `--waypoints-file` JSON（AC-8）+ ragged 等长检查。`positions` 长度由
/// JSON 决定；CLI 仅校验所有 waypoint 等长（关节数与 hardware 匹配交 server
/// `ScriptedBackend`）。
///
/// # Errors
///
/// JSON 语法 / 字段类型错；`waypoints` 为空；`positions` ragged（不等长）。
pub fn parse_waypoints_json(json: &str) -> Result<Vec<Waypoint>, String> {
    let file: WaypointsFile =
        serde_json::from_str(json).map_err(|e| format!("invalid waypoints JSON: {e}"))?;
    let Some(first) = file.waypoints.first() else {
        return Err("waypoints must not be empty".to_owned());
    };
    let expected = first.positions.len();
    for (i, wp) in file.waypoints.iter().enumerate() {
        if wp.positions.len() != expected {
            return Err(format!(
                "waypoint positions length mismatch (waypoint[{i}] has {}, expected {expected})",
                wp.positions.len()
            ));
        }
    }
    Ok(file
        .waypoints
        .into_iter()
        .map(|w| Waypoint {
            positions: w.positions,
            dwell_ms: w.dwell_ms,
            segment_duration_ms: w.segment_duration_ms,
        })
        .collect())
}

/// 构造 `ScriptedPayload` fbs body（`SCHEMA_SCRIPTED_PAYLOAD`；inline 沿
/// `scripted_backend_smoke`）。
#[must_use]
pub fn build_scripted_payload(waypoints: &[Waypoint]) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(256);
    let wp_offsets: Vec<_> = waypoints
        .iter()
        .map(|wp| {
            let positions_vec = fbb.create_vector::<f64>(&wp.positions);
            WaypointTarget::create(
                &mut fbb,
                &WaypointTargetArgs {
                    positions: Some(positions_vec),
                    dwell_ms: wp.dwell_ms,
                    segment_duration_ms: wp.segment_duration_ms,
                },
            )
        })
        .collect();
    let wps_vec = fbb.create_vector(&wp_offsets);
    let payload = FbScriptedPayload::create(
        &mut fbb,
        &ScriptedPayloadArgs {
            waypoints: Some(wps_vec),
        },
    );
    fbb.finish(payload, None);
    fbb.finished_data().to_vec()
}

// ============================================================================
// ik（AC-9）
// ============================================================================

/// `invoke ik` 解析后的目标（AC-9）。
#[derive(Debug)]
pub struct IkTarget {
    /// 目标位置 `[x, y, z]`（m）。
    pub xyz: [f64; 3],
    /// 目标姿态四元数 `[qx, qy, qz, qw]`（默认 `0 0 0 1`；CLI 不校验
    /// normalization，交 server `IkBackend`）。
    pub quat_xyzw: [f64; 4],
    /// 可选 seed 关节角（rad）；空 = 无 seed。
    pub seed_joints: Vec<f64>,
    /// frame 名（默认空字符串）。
    pub frame: String,
}

fn parse_required_f64(flag: &str, v: Option<&str>) -> Result<f64, String> {
    let s = v.ok_or_else(|| format!("{flag} is required"))?;
    s.parse::<f64>()
        .map_err(|e| format!("{flag}: invalid f64 '{s}': {e}"))
}

fn parse_optional_f64(flag: &str, v: Option<&str>, default: f64) -> Result<f64, String> {
    match v {
        None => Ok(default),
        Some(s) => s
            .parse::<f64>()
            .map_err(|e| format!("{flag}: invalid f64 '{s}': {e}")),
    }
}

/// 解析 invoke ik 目标参数（AC-9）：xyz 必填 f64；quat 可选默认 `0 0 0 1`；
/// seed 可选逗号列表；frame 可选默认空。
///
/// # Errors
///
/// 缺必填 xyz；任一 f64 / seed 元素解析失败。
#[allow(clippy::too_many_arguments, clippy::many_single_char_names)]
pub fn parse_ik_target(
    x: Option<&str>,
    y: Option<&str>,
    z: Option<&str>,
    qx: Option<&str>,
    qy: Option<&str>,
    qz: Option<&str>,
    qw: Option<&str>,
    seed: Option<&str>,
    frame: Option<&str>,
) -> Result<IkTarget, String> {
    let px = parse_required_f64("--target-x", x)?;
    let py = parse_required_f64("--target-y", y)?;
    let pz = parse_required_f64("--target-z", z)?;
    let qx = parse_optional_f64("--target-qx", qx, 0.0)?;
    let qy = parse_optional_f64("--target-qy", qy, 0.0)?;
    let qz = parse_optional_f64("--target-qz", qz, 0.0)?;
    let qw = parse_optional_f64("--target-qw", qw, 1.0)?;
    let seed_joints = match seed {
        None => Vec::new(),
        Some(s) if s.trim().is_empty() => Vec::new(),
        Some(s) => s
            .split(',')
            .map(|t| {
                let t = t.trim();
                t.parse::<f64>()
                    .map_err(|e| format!("--seed-joints: invalid f64 '{t}': {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok(IkTarget {
        xyz: [px, py, pz],
        quat_xyzw: [qx, qy, qz, qw],
        seed_joints,
        frame: frame.unwrap_or("").to_owned(),
    })
}

/// 构造 `IkPayload` fbs body（`SCHEMA_IK_PAYLOAD`；inline 沿
/// `ik_backend_smoke`）。
#[must_use]
pub fn build_ik_payload(target: &IkTarget) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(256);
    let xyz_v = fbb.create_vector::<f64>(&target.xyz);
    let quat_v = fbb.create_vector::<f64>(&target.quat_xyzw);
    let seed_v = fbb.create_vector::<f64>(&target.seed_joints);
    let frame_v = fbb.create_string(&target.frame);
    let payload = FbIkPayload::create(
        &mut fbb,
        &IkPayloadArgs {
            target_xyz: Some(xyz_v),
            target_quat_xyzw: Some(quat_v),
            seed_joints_rad: Some(seed_v),
            frame: Some(frame_v),
        },
    );
    fbb.finish(payload, None);
    fbb.finished_data().to_vec()
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // round-trip 是 bit-exact，== 是正确判据
mod tests {
    use super::{
        IkTarget, build_ik_payload, build_scripted_payload, parse_ik_target, parse_waypoints_json,
    };

    // ---- waypoints JSON（AC-8）----

    #[test]
    fn waypoints_happy_parses() {
        let json = r#"{"waypoints":[
            {"positions":[0.1,0.2,0.3],"dwell_ms":100,"segment_duration_ms":500},
            {"positions":[0.2,0.3,0.4],"dwell_ms":0,"segment_duration_ms":1000}]}"#;
        let wps = parse_waypoints_json(json).expect("parse");
        assert_eq!(wps.len(), 2);
        assert_eq!(wps[0].positions, vec![0.1, 0.2, 0.3]);
        assert_eq!(wps[1].dwell_ms, 0);
        assert_eq!(wps[1].segment_duration_ms, 1000);
    }

    #[test]
    fn waypoints_ragged_rejected() {
        let json = r#"{"waypoints":[
            {"positions":[0.1,0.2,0.3],"dwell_ms":0,"segment_duration_ms":500},
            {"positions":[0.1,0.2],"dwell_ms":0,"segment_duration_ms":500}]}"#;
        assert_eq!(
            parse_waypoints_json(json).unwrap_err(),
            "waypoint positions length mismatch (waypoint[1] has 2, expected 3)"
        );
    }

    #[test]
    fn waypoints_empty_rejected() {
        assert_eq!(
            parse_waypoints_json(r#"{"waypoints":[]}"#).unwrap_err(),
            "waypoints must not be empty"
        );
    }

    #[test]
    fn waypoints_bad_json_rejected() {
        let err = parse_waypoints_json("{not json").unwrap_err();
        assert!(err.starts_with("invalid waypoints JSON:"), "{err}");
    }

    #[test]
    fn waypoints_type_error_rejected() {
        // dwell_ms 应为整数，给字符串。
        let json = r#"{"waypoints":[{"positions":[0.1],"dwell_ms":"x","segment_duration_ms":1}]}"#;
        let err = parse_waypoints_json(json).unwrap_err();
        assert!(err.starts_with("invalid waypoints JSON:"), "{err}");
    }

    #[test]
    fn scripted_payload_round_trips() {
        let json = r#"{"waypoints":[{"positions":[0.1,0.2,0.3,0.4,0.5],"dwell_ms":7,"segment_duration_ms":500}]}"#;
        let wps = parse_waypoints_json(json).expect("parse");
        let bytes = build_scripted_payload(&wps);
        let fb = flatbuffers::root::<robot_msg::msg::ScriptedPayload<'_>>(&bytes).expect("root");
        let fb_wps = fb.waypoints().expect("waypoints");
        assert_eq!(fb_wps.len(), 1);
        let w0 = fb_wps.get(0);
        assert_eq!(w0.dwell_ms(), 7);
        assert_eq!(w0.segment_duration_ms(), 500);
        let pos: Vec<f64> = w0.positions().expect("positions").iter().collect();
        assert_eq!(pos, vec![0.1, 0.2, 0.3, 0.4, 0.5]);
    }

    // ---- ik 参数（AC-9）----

    #[test]
    fn ik_target_happy_with_defaults() {
        let t = parse_ik_target(
            Some("0.2"),
            Some("0.0"),
            Some("0.1"),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("parse");
        assert_eq!(t.xyz, [0.2, 0.0, 0.1]);
        assert_eq!(t.quat_xyzw, [0.0, 0.0, 0.0, 1.0]); // identity 默认
        assert!(
            t.seed_joints.is_empty(),
            "expected no seed joints, got {:?}",
            t.seed_joints
        );
        assert_eq!(t.frame, "");
    }

    #[test]
    fn ik_target_missing_required_rejected() {
        let err = parse_ik_target(
            None,
            Some("0.0"),
            Some("0.1"),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, "--target-x is required");
    }

    #[test]
    fn ik_target_invalid_f64_rejected() {
        let err = parse_ik_target(
            Some("abc"),
            Some("0.0"),
            Some("0.1"),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.starts_with("--target-x: invalid f64 'abc':"), "{err}");
    }

    #[test]
    fn ik_target_seed_list_and_quat_override() {
        let t = parse_ik_target(
            Some("0.1"),
            Some("0.2"),
            Some("0.3"),
            Some("0.0"),
            Some("0.0"),
            Some("0.25"),
            Some("0.75"),
            Some("0.1,0.2,0.3,0.4,0.5"),
            Some("base"),
        )
        .expect("parse");
        assert_eq!(t.quat_xyzw, [0.0, 0.0, 0.25, 0.75]);
        assert_eq!(t.seed_joints, vec![0.1, 0.2, 0.3, 0.4, 0.5]);
        assert_eq!(t.frame, "base");
    }

    #[test]
    fn ik_target_bad_seed_rejected() {
        let err = parse_ik_target(
            Some("0.1"),
            Some("0.2"),
            Some("0.3"),
            None,
            None,
            None,
            None,
            Some("0.1,x,0.3"),
            None,
        )
        .unwrap_err();
        assert!(err.starts_with("--seed-joints: invalid f64 'x':"), "{err}");
    }

    #[test]
    fn ik_payload_round_trips() {
        let t = IkTarget {
            xyz: [0.2, 0.0, 0.1],
            quat_xyzw: [0.0, 0.0, 0.0, 1.0],
            seed_joints: vec![0.1, 0.2],
            frame: "base".to_owned(),
        };
        let bytes = build_ik_payload(&t);
        let fb = flatbuffers::root::<robot_msg::msg::IkPayload<'_>>(&bytes).expect("root");
        let xyz: Vec<f64> = fb.target_xyz().expect("xyz").iter().collect();
        assert_eq!(xyz, vec![0.2, 0.0, 0.1]);
        let quat: Vec<f64> = fb.target_quat_xyzw().expect("quat").iter().collect();
        assert_eq!(quat, vec![0.0, 0.0, 0.0, 1.0]);
        let seed: Vec<f64> = fb.seed_joints_rad().expect("seed").iter().collect();
        assert_eq!(seed, vec![0.1, 0.2]);
        assert_eq!(fb.frame().expect("frame"), "base");
    }
}
