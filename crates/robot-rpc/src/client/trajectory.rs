//! client 侧 `JointTrajectory` 形状 → `ScriptedPayload` waypoint 转换器
//! （v0.5 候选 E 段(6)，Spec D7 / D8 / D11 + CodeMap §3.1 映射表）。
//!
//! ## 契约
//!
//! - **rad-native**（D11）：只收 rad；NaN / Inf、长度不匹配 → reject；**不
//!   clamp**（硬件保护 clamp 的唯一权威 = S0 calibration clamp，client 不复制
//!   第二道）。"度" / `[0,100]` 只存在于 [`crate::client::baseline_shim`]
//!   输入层。
//! - **name→index 权威** = config 派生 [`HardwareLayout::joints`]（D1 链，
//!   与 daemon 同源）；禁裸固定索引；未知名 / 重名 → reject。
//! - **首点语义**（D7 承接 D3）：`points[0]` = 提交时当前位形（dimos
//!   `executor.py:96-105` 两点轨迹形状；chunk 折叠后为 `[current@0, s1@dt,
//!   …, sN@N·dt]`）。转换器**丢弃首点**（S1 段(5) `scripted_start_pose` 已从
//!   state cache 取真实当前位作 segment0 from），但**断言其 ≈ `current`**：
//!   逐命名关节 `|points[0][j] − current[idx_j]| ≤` [`FIRST_POINT_TOLERANCE_RAD`]，
//!   超差 reject（`current` 与首点来自两次独立读取，容差吸收读写间隙漂移）。
//! - **时长差分**：`segment_duration_ms = round((tfs[i] − tfs[i−1]) × 1000)`；
//!   `time_from_start_s` 必须有限、非负、严格递增，且每段 ≥ 1 ms（把
//!   `ScriptedBackend` "`segment_duration_ms > 0`" 守门前移到 client）；
//!   `dwell_ms = 0`；`velocities` / `timestamp` 丢弃（两侧皆分段线性）。
//! - **缺位填充**（D8）：layout 中未出现在 `joint_names` 的关节 → 每个
//!   waypoint 填 `current[idx]`（"同 chunk 一次读"：整条轨迹用同一份
//!   `current`）。D2(i) 6 维臂向量下"只动夹爪"= 5 臂关节填当前位，反之同理。
//! - 输出 [`Waypoint`] 全向量（长度 == `layout.joints.len()`）；fbs 字节由
//!   [`build_scripted_payload`] 生成。gateway 4 KiB byte gate 仍是唯一容量
//!   权威（本层不预判 byte 数）。

use flatbuffers::FlatBufferBuilder;
use robot_msg::msg::{
    ScriptedPayload as FbScriptedPayload, ScriptedPayloadArgs, WaypointTarget, WaypointTargetArgs,
};

use super::layout::HardwareLayout;

/// 首点宽容校验容差（rad）：`|points[0][j] − current[idx_j]|` 上限。
///
/// 取值依据（AC-E10 定值，Spec P.4 登记）：STS3215 1 tick ≈ 0.0015 rad、真机
/// hold 噪声 ±10 tick ≈ 0.015 rad（`adapter.rs` `SYNC_GOAL_TOLERANCE` 注）；
/// 0.05 rad（≈ 2.9°）= 噪声 3× 余量，且**小于** demo 小幅运动上界 0.087 rad
/// （AC-E12 ③）——首点与当前位偏差达到"一次 demo 运动"量级即视为轨迹不是从
/// 当前位起、拒绝。
pub const FIRST_POINT_TOLERANCE_RAD: f64 = 0.05;

/// 轨迹输入（dimos `JointTrajectory` 形状，rad-native）。
#[derive(Debug, Clone, PartialEq)]
pub struct TrajectoryInput {
    /// 命名关节子集（顺序 = 各 point `positions` 的列顺序）；必须 ⊆
    /// [`HardwareLayout::joints`] 且无重名。
    pub joint_names: Vec<String>,
    /// ≥ 2 个点；`points[0]` = 当前位形（丢弃 + 宽容校验），其余 = 目标。
    pub points: Vec<TrajectoryPoint>,
}

/// 轨迹点（`TrajectoryPoint{time_from_start, positions}`；`velocities` 不收）。
#[derive(Debug, Clone, PartialEq)]
pub struct TrajectoryPoint {
    /// 自轨迹起点的绝对时刻（s）；有限、非负、严格递增。
    pub time_from_start_s: f64,
    /// 各命名关节目标位（rad）；长度 == `joint_names.len()`。
    pub positions: Vec<f64>,
}

/// 转换输出：`ScriptedPayload.waypoints[i]` 形状（全向量 rad）。
#[derive(Debug, Clone, PartialEq)]
pub struct Waypoint {
    /// 全 hardware 向量（长度 == `layout.joints.len()`，按 layout 索引对位）。
    pub positions: Vec<f64>,
    /// 到点后驻留（ms）；转换固定 0（CodeMap §3.1：新增维度映射置 0）。
    pub dwell_ms: u32,
    /// 到该 waypoint 的插值时长（ms，≥ 1）。
    pub segment_duration_ms: u32,
}

/// [`to_scripted_waypoints`] 的 typed 拒绝原因（AC-E10 reject 族；全部
/// **不可降级**——转换器拒绝 = 该 chunk 不提交）。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ConvertError {
    #[error("current positions length {got} != layout joints {expected}")]
    CurrentLength { got: usize, expected: usize },
    #[error("current[{idx}] is not finite ({value})")]
    CurrentNonFinite { idx: usize, value: f64 },
    #[error("joint_names must not be empty")]
    EmptyJointNames,
    #[error("joint {0:?} not in hardware layout (name→index 权威 = config 派生 joints)")]
    UnknownJointName(String),
    #[error("joint {0:?} listed twice in joint_names")]
    DuplicateJointName(String),
    #[error("trajectory needs >= 2 points (points[0] = current pose, dropped), got {got}")]
    TooFewPoints { got: usize },
    #[error("points[{point}].positions length {got} != joint_names {expected}")]
    PositionsLength {
        point: usize,
        got: usize,
        expected: usize,
    },
    #[error("points[{point}] joint {joint:?} position is not finite ({value})")]
    NonFinitePosition {
        point: usize,
        joint: String,
        value: f64,
    },
    #[error("points[{point}].time_from_start_s invalid ({value}): must be finite and >= 0")]
    InvalidTime { point: usize, value: f64 },
    #[error(
        "points[{point}].time_from_start_s {cur} <= previous {prev}: must be strictly increasing"
    )]
    NonMonotonicTime { point: usize, prev: f64, cur: f64 },
    #[error(
        "segment into points[{point}] lasts {dt_s} s: rounds to < 1 ms or exceeds u32 ms \
         (ScriptedBackend requires segment_duration_ms > 0)"
    )]
    SegmentDuration { point: usize, dt_s: f64 },
    #[error(
        "points[0] joint {joint:?} = {got} deviates {deviation} rad from current {current} \
         (tolerance {tolerance}): trajectory does not start at the current pose"
    )]
    FirstPointDeviation {
        joint: String,
        got: f64,
        current: f64,
        deviation: f64,
        tolerance: f64,
    },
}

/// `JointTrajectory` 形状 → `ScriptedPayload` waypoints（契约详模块 doc）。
///
/// `current` = 同 chunk 一次 `GetState` 读到的全向量（长度 ==
/// `layout.joints.len()`，已过 `validate_state_layout`）。
///
/// # Errors
///
/// 见 [`ConvertError`]；任一 `Err` 表示该轨迹**整体**不可提交。
pub fn to_scripted_waypoints(
    traj: &TrajectoryInput,
    layout: &HardwareLayout,
    current: &[f64],
) -> Result<Vec<Waypoint>, ConvertError> {
    let n = layout.joints.len();
    if current.len() != n {
        return Err(ConvertError::CurrentLength {
            got: current.len(),
            expected: n,
        });
    }
    if let Some((idx, &value)) = current.iter().enumerate().find(|(_, v)| !v.is_finite()) {
        return Err(ConvertError::CurrentNonFinite { idx, value });
    }
    if traj.joint_names.is_empty() {
        return Err(ConvertError::EmptyJointNames);
    }

    // name → layout index（D1 链；未知名 / 重名 reject）。
    let k = traj.joint_names.len();
    let mut index_of: Vec<usize> = Vec::with_capacity(k);
    let mut seen = vec![false; n];
    for name in &traj.joint_names {
        let idx = layout
            .joints
            .iter()
            .position(|j| j == name)
            .ok_or_else(|| ConvertError::UnknownJointName(name.clone()))?;
        if std::mem::replace(&mut seen[idx], true) {
            return Err(ConvertError::DuplicateJointName(name.clone()));
        }
        index_of.push(idx);
    }

    if traj.points.len() < 2 {
        return Err(ConvertError::TooFewPoints {
            got: traj.points.len(),
        });
    }

    // 逐点形状 / 有限性 / 时间单调校验（整体校验先于任何构造）。
    for (p, pt) in traj.points.iter().enumerate() {
        if pt.positions.len() != k {
            return Err(ConvertError::PositionsLength {
                point: p,
                got: pt.positions.len(),
                expected: k,
            });
        }
        if let Some((j, &value)) = pt
            .positions
            .iter()
            .enumerate()
            .find(|(_, v)| !v.is_finite())
        {
            return Err(ConvertError::NonFinitePosition {
                point: p,
                joint: traj.joint_names[j].clone(),
                value,
            });
        }
        let t = pt.time_from_start_s;
        if !t.is_finite() || t < 0.0 {
            return Err(ConvertError::InvalidTime { point: p, value: t });
        }
        if p > 0 {
            let prev = traj.points[p - 1].time_from_start_s;
            if t <= prev {
                return Err(ConvertError::NonMonotonicTime {
                    point: p,
                    prev,
                    cur: t,
                });
            }
        }
    }

    // 首点宽容校验（D7）：points[0] ≈ current（逐命名关节）。
    let first = &traj.points[0];
    for (j, &idx) in index_of.iter().enumerate() {
        let got = first.positions[j];
        let deviation = (got - current[idx]).abs();
        if deviation > FIRST_POINT_TOLERANCE_RAD {
            return Err(ConvertError::FirstPointDeviation {
                joint: traj.joint_names[j].clone(),
                got,
                current: current[idx],
                deviation,
                tolerance: FIRST_POINT_TOLERANCE_RAD,
            });
        }
    }

    // 构造：丢首点；每段时长差分；缺位填 current（D8）。
    let mut out: Vec<Waypoint> = Vec::with_capacity(traj.points.len() - 1);
    for p in 1..traj.points.len() {
        let dt_s = traj.points[p].time_from_start_s - traj.points[p - 1].time_from_start_s;
        let segment_duration_ms =
            segment_ms(dt_s).ok_or(ConvertError::SegmentDuration { point: p, dt_s })?;
        let mut positions = current.to_vec();
        for (j, &idx) in index_of.iter().enumerate() {
            positions[idx] = traj.points[p].positions[j];
        }
        out.push(Waypoint {
            positions,
            dwell_ms: 0,
            segment_duration_ms,
        });
    }
    Ok(out)
}

/// 段时长（s）→ ms：四舍五入后必须 ∈ `[1, u32::MAX]`，否则 `None`。
fn segment_ms(dt_s: f64) -> Option<u32> {
    let ms = (dt_s * 1000.0).round();
    // 上面已保证 dt_s 有限且 > 0；这里只挡 "< 1 ms" 与 u32 溢出。
    if ms < 1.0 || ms > f64::from(u32::MAX) {
        return None;
    }
    // 已范围检查：[1, u32::MAX] 内的整数值，truncation / sign loss 不可能。
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(ms as u32)
}

/// [`Waypoint`] 列表 → `ScriptedPayload` fbs body bytes（`SCHEMA_SCRIPTED_PAYLOAD`
/// 由调用方填 `CommandMeta.payload_schema_id`）。与 robotctl / smoke 的 inline
/// 构造同构（robotctl 0 改：辅助入口保留自己的 JSON 输入面）。
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
// Tests（AC-E10：差分 / 首点 / reject 族 / 缺位填充 / fbs 往返）
// ============================================================================

#[cfg(test)]
// 转换器对 rad 值是**原样拷贝**（rad-native 不 clamp 不换算），精确相等即契约；
// 仅时长走 round，其断言是整数。
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use robot_msg::joint_layout::compute_layout_id;

    /// D2(i) 6 维臂向量：5 臂关节 + 夹爪。
    fn arm_layout() -> HardwareLayout {
        let joints: Vec<String> = [
            "left_arm_shoulder_pan",
            "left_arm_shoulder_lift",
            "left_arm_elbow_flex",
            "left_arm_wrist_flex",
            "left_arm_wrist_roll",
            "left_arm_gripper",
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        let motor_ids = vec![1, 2, 3, 4, 5, 6];
        let pairs: Vec<(&str, u32)> = joints
            .iter()
            .map(String::as_str)
            .zip(motor_ids.iter().map(|&m| u32::from(m)))
            .collect();
        let layout_id = compute_layout_id("xlerobot_left", "left_arm", &pairs);
        HardwareLayout {
            joints,
            motor_ids,
            layout_id,
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    fn pt(t: f64, positions: &[f64]) -> TrajectoryPoint {
        TrajectoryPoint {
            time_from_start_s: t,
            positions: positions.to_vec(),
        }
    }

    const CURRENT: [f64; 6] = [0.10, 0.20, 0.30, 0.40, 0.50, 0.60];

    /// dimos 两点形状：5 臂关节子集，首点 = current 的臂分量。
    fn two_point_arm_traj(target: [f64; 5]) -> TrajectoryInput {
        TrajectoryInput {
            joint_names: names(&[
                "left_arm_shoulder_pan",
                "left_arm_shoulder_lift",
                "left_arm_elbow_flex",
                "left_arm_wrist_flex",
                "left_arm_wrist_roll",
            ]),
            points: vec![pt(0.0, &CURRENT[..5]), pt(0.1, &target)],
        }
    }

    // ----- happy：两点 → 一段；缺位（夹爪）填 current；dwell 0；100 ms -----

    #[test]
    fn two_point_arm_subset_fills_gripper_with_current() {
        let layout = arm_layout();
        let target = [0.11, 0.22, 0.33, 0.44, 0.55];
        let wps = to_scripted_waypoints(&two_point_arm_traj(target), &layout, &CURRENT).unwrap();
        assert_eq!(wps.len(), 1);
        let w = &wps[0];
        assert_eq!(&w.positions[..5], &target);
        assert_eq!(w.positions[5], CURRENT[5], "夹爪缺位填 current（D8）");
        assert_eq!(w.dwell_ms, 0);
        assert_eq!(w.segment_duration_ms, 100);
    }

    // ----- chunk 折叠：11 点（current + 10 步）→ 10 段各 100 ms -----

    #[test]
    fn ten_step_chunk_folds_into_ten_segments() {
        let layout = arm_layout();
        let mut points = vec![pt(0.0, &CURRENT[..5])];
        for i in 1..=10 {
            let t = f64::from(i) * 0.1;
            let p: Vec<f64> = CURRENT[..5]
                .iter()
                .map(|c| c + 0.001 * f64::from(i))
                .collect();
            points.push(pt(t, &p));
        }
        let traj = TrajectoryInput {
            joint_names: two_point_arm_traj([0.0; 5]).joint_names,
            points,
        };
        let wps = to_scripted_waypoints(&traj, &layout, &CURRENT).unwrap();
        assert_eq!(wps.len(), 10);
        for (i, w) in wps.iter().enumerate() {
            assert_eq!(w.segment_duration_ms, 100, "segment {i}");
            assert_eq!(w.positions.len(), 6);
            assert_eq!(w.positions[5], CURRENT[5]);
            let expect0 = CURRENT[0] + 0.001 * f64::from(u8::try_from(i + 1).unwrap());
            assert!((w.positions[0] - expect0).abs() < 1e-12);
        }
    }

    // ----- name→index：子集顺序与 layout 不同也按名对位 -----

    #[test]
    fn joint_names_order_independent_of_layout_order() {
        let layout = arm_layout();
        let traj = TrajectoryInput {
            joint_names: names(&["left_arm_gripper", "left_arm_shoulder_pan"]),
            points: vec![pt(0.0, &[CURRENT[5], CURRENT[0]]), pt(0.25, &[0.65, 0.15])],
        };
        let wps = to_scripted_waypoints(&traj, &layout, &CURRENT).unwrap();
        assert_eq!(wps.len(), 1);
        assert_eq!(wps[0].positions, vec![0.15, 0.20, 0.30, 0.40, 0.50, 0.65]);
        assert_eq!(wps[0].segment_duration_ms, 250);
    }

    // ----- 只动夹爪：5 臂关节填 current（D8 反向） -----

    #[test]
    fn gripper_only_trajectory_fills_arm_with_current() {
        let layout = arm_layout();
        let traj = TrajectoryInput {
            joint_names: names(&["left_arm_gripper"]),
            points: vec![pt(0.0, &[CURRENT[5]]), pt(0.1, &[0.9])],
        };
        let wps = to_scripted_waypoints(&traj, &layout, &CURRENT).unwrap();
        assert_eq!(&wps[0].positions[..5], &CURRENT[..5]);
        assert_eq!(wps[0].positions[5], 0.9);
    }

    // ----- 时长差分：非等距 + 四舍五入 -----

    #[test]
    fn segment_durations_are_rounded_differences() {
        let layout = arm_layout();
        let traj = TrajectoryInput {
            joint_names: names(&["left_arm_shoulder_pan"]),
            points: vec![
                pt(0.0, &[CURRENT[0]]),
                pt(0.0504, &[0.1]), // 50.4 → 50
                pt(0.1010, &[0.1]), // +50.6 → 51
                pt(1.1010, &[0.1]), // +1000
            ],
        };
        let wps = to_scripted_waypoints(&traj, &layout, &CURRENT).unwrap();
        let ms: Vec<u32> = wps.iter().map(|w| w.segment_duration_ms).collect();
        assert_eq!(ms, vec![50, 51, 1000]);
    }

    #[test]
    fn first_point_time_need_not_be_zero_only_differences_matter() {
        let layout = arm_layout();
        let traj = TrajectoryInput {
            joint_names: names(&["left_arm_shoulder_pan"]),
            points: vec![pt(2.0, &[CURRENT[0]]), pt(2.3, &[0.1])],
        };
        let wps = to_scripted_waypoints(&traj, &layout, &CURRENT).unwrap();
        assert_eq!(wps[0].segment_duration_ms, 300);
    }

    // ----- 首点宽容校验：边界接受、超差 reject（AC-E10） -----

    #[test]
    fn first_point_within_tolerance_accepted_at_boundary() {
        let layout = arm_layout();
        let mut first = CURRENT[..5].to_vec();
        first[2] += FIRST_POINT_TOLERANCE_RAD - 1e-9;
        let traj = TrajectoryInput {
            joint_names: two_point_arm_traj([0.0; 5]).joint_names,
            points: vec![pt(0.0, &first), pt(0.1, &[0.0; 5])],
        };
        to_scripted_waypoints(&traj, &layout, &CURRENT).expect("within tolerance");
    }

    #[test]
    fn first_point_deviation_rejected() {
        let layout = arm_layout();
        let mut first = CURRENT[..5].to_vec();
        first[3] -= FIRST_POINT_TOLERANCE_RAD + 1e-6;
        let traj = TrajectoryInput {
            joint_names: two_point_arm_traj([0.0; 5]).joint_names,
            points: vec![pt(0.0, &first), pt(0.1, &[0.0; 5])],
        };
        let err = to_scripted_waypoints(&traj, &layout, &CURRENT).unwrap_err();
        match err {
            ConvertError::FirstPointDeviation {
                joint,
                deviation,
                tolerance,
                ..
            } => {
                assert_eq!(joint, "left_arm_wrist_flex");
                assert!(deviation > tolerance);
                assert!((tolerance - FIRST_POINT_TOLERANCE_RAD).abs() < f64::EPSILON);
            }
            other => panic!("expected FirstPointDeviation, got {other:?}"),
        }
    }

    #[test]
    fn first_point_deviation_only_checks_named_joints() {
        // 夹爪不在 joint_names → 其 current 与"首点"无关（首点无该列）。
        let layout = arm_layout();
        let mut current = CURRENT;
        current[5] = 5.0; // 夹爪当前位与任何首点无关
        let wps = to_scripted_waypoints(&two_point_arm_traj([0.0; 5]), &layout, &current)
            .expect("gripper not named → not checked");
        assert_eq!(wps[0].positions[5], 5.0);
    }

    // ----- reject 族 -----

    #[test]
    fn rejects_current_length_and_non_finite_current() {
        let layout = arm_layout();
        let traj = two_point_arm_traj([0.0; 5]);
        assert!(matches!(
            to_scripted_waypoints(&traj, &layout, &CURRENT[..5]).unwrap_err(),
            ConvertError::CurrentLength {
                got: 5,
                expected: 6
            }
        ));
        let mut bad = CURRENT;
        bad[1] = f64::NAN;
        assert!(matches!(
            to_scripted_waypoints(&traj, &layout, &bad).unwrap_err(),
            ConvertError::CurrentNonFinite { idx: 1, .. }
        ));
    }

    #[test]
    fn rejects_empty_unknown_and_duplicate_joint_names() {
        let layout = arm_layout();
        let empty = TrajectoryInput {
            joint_names: vec![],
            points: vec![pt(0.0, &[]), pt(0.1, &[])],
        };
        assert!(matches!(
            to_scripted_waypoints(&empty, &layout, &CURRENT).unwrap_err(),
            ConvertError::EmptyJointNames
        ));
        let unknown = TrajectoryInput {
            joint_names: names(&["right_arm_gripper"]),
            points: vec![pt(0.0, &[0.0]), pt(0.1, &[0.0])],
        };
        assert!(matches!(
            to_scripted_waypoints(&unknown, &layout, &CURRENT).unwrap_err(),
            ConvertError::UnknownJointName(ref n) if n == "right_arm_gripper"
        ));
        let dup = TrajectoryInput {
            joint_names: names(&["left_arm_gripper", "left_arm_gripper"]),
            points: vec![pt(0.0, &[CURRENT[5], CURRENT[5]]), pt(0.1, &[0.0, 0.0])],
        };
        assert!(matches!(
            to_scripted_waypoints(&dup, &layout, &CURRENT).unwrap_err(),
            ConvertError::DuplicateJointName(ref n) if n == "left_arm_gripper"
        ));
    }

    #[test]
    fn rejects_too_few_points() {
        let layout = arm_layout();
        for points in [vec![], vec![pt(0.0, &CURRENT[..5])]] {
            let traj = TrajectoryInput {
                joint_names: two_point_arm_traj([0.0; 5]).joint_names,
                points,
            };
            let got = traj.points.len();
            assert!(matches!(
                to_scripted_waypoints(&traj, &layout, &CURRENT).unwrap_err(),
                ConvertError::TooFewPoints { got: g } if g == got
            ));
        }
    }

    #[test]
    fn rejects_positions_length_and_non_finite_position() {
        let layout = arm_layout();
        let ragged = TrajectoryInput {
            joint_names: two_point_arm_traj([0.0; 5]).joint_names,
            points: vec![pt(0.0, &CURRENT[..5]), pt(0.1, &[0.0; 4])],
        };
        assert!(matches!(
            to_scripted_waypoints(&ragged, &layout, &CURRENT).unwrap_err(),
            ConvertError::PositionsLength {
                point: 1,
                got: 4,
                expected: 5
            }
        ));
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut target = [0.0; 5];
            target[4] = bad;
            let traj = two_point_arm_traj(target);
            assert!(matches!(
                to_scripted_waypoints(&traj, &layout, &CURRENT).unwrap_err(),
                ConvertError::NonFinitePosition { point: 1, ref joint, .. }
                    if joint == "left_arm_wrist_roll"
            ));
        }
    }

    #[test]
    fn rejects_invalid_and_non_monotonic_time() {
        let layout = arm_layout();
        let base = two_point_arm_traj([0.0; 5]);
        for bad_t in [f64::NAN, f64::INFINITY, -0.1] {
            let mut traj = base.clone();
            traj.points[0].time_from_start_s = bad_t;
            assert!(matches!(
                to_scripted_waypoints(&traj, &layout, &CURRENT).unwrap_err(),
                ConvertError::InvalidTime { point: 0, .. }
            ));
        }
        for (t0, t1) in [(0.1, 0.1), (0.2, 0.1)] {
            let mut traj = base.clone();
            traj.points[0].time_from_start_s = t0;
            traj.points[1].time_from_start_s = t1;
            assert!(matches!(
                to_scripted_waypoints(&traj, &layout, &CURRENT).unwrap_err(),
                ConvertError::NonMonotonicTime { point: 1, .. }
            ));
        }
    }

    #[test]
    fn rejects_segment_shorter_than_one_ms_or_overflowing_u32() {
        let layout = arm_layout();
        let mut traj = two_point_arm_traj([0.0; 5]);
        traj.points[1].time_from_start_s = 0.0004; // 0.4 ms → round 0 → reject
        assert!(matches!(
            to_scripted_waypoints(&traj, &layout, &CURRENT).unwrap_err(),
            ConvertError::SegmentDuration { point: 1, .. }
        ));
        traj.points[1].time_from_start_s = 0.0005; // 0.5 ms → round 1 → accept
        assert_eq!(
            to_scripted_waypoints(&traj, &layout, &CURRENT).unwrap()[0].segment_duration_ms,
            1
        );
        traj.points[1].time_from_start_s = 5.0e6; // 5e9 ms > u32::MAX → reject
        assert!(matches!(
            to_scripted_waypoints(&traj, &layout, &CURRENT).unwrap_err(),
            ConvertError::SegmentDuration { point: 1, .. }
        ));
    }

    #[test]
    fn rad_native_no_clamp_passes_large_values_through() {
        // D11：不 clamp——越界 rad 原样透传，由 S0 calibration clamp 兜底。
        let layout = arm_layout();
        let wps = to_scripted_waypoints(&two_point_arm_traj([9.0; 5]), &layout, &CURRENT).unwrap();
        assert_eq!(&wps[0].positions[..5], &[9.0; 5]);
    }

    // ----- fbs 往返 -----

    #[test]
    fn build_scripted_payload_round_trips() {
        let layout = arm_layout();
        let wps = to_scripted_waypoints(
            &two_point_arm_traj([0.11, 0.22, 0.33, 0.44, 0.55]),
            &layout,
            &CURRENT,
        )
        .unwrap();
        let bytes = build_scripted_payload(&wps);
        let fb = flatbuffers::root::<FbScriptedPayload<'_>>(&bytes).expect("verify");
        let fb_wps = fb.waypoints().expect("waypoints");
        assert_eq!(fb_wps.len(), 1);
        let w = fb_wps.get(0);
        let positions: Vec<f64> = w.positions().expect("positions").iter().collect();
        assert_eq!(positions, wps[0].positions);
        assert_eq!(w.dwell_ms(), 0);
        assert_eq!(w.segment_duration_ms(), 100);
    }
}
