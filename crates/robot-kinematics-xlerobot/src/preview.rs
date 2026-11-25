//! 右腕点沿肩基座 X 轴前移 10 mm，再按同一次关节反馈返回。仅计算目标。

use super::{Side, XLeRobotArmIkSolver};
use robot_kinematics::{ArmIkSolver, Pose};

/// 运动学解和反馈使用同一五轴顺序。关节名不含总线侧前缀。
pub const ARM_JOINT_NAMES: [&str; 5] = [
    "shoulder_pan",
    "shoulder_lift",
    "elbow_flex",
    "wrist_flex",
    "wrist_roll",
];

/// 两段独立记录。前移被拒绝时，回程没有可用的量化起点。
#[derive(Debug)]
pub struct RightWristPreview {
    pub origin: Pose,
    pub target: Pose,
    pub forward: Result<[i32; 5], String>,
    pub back: Option<Result<[i32; 5], String>>,
}

/// 起点由调用方从同一帧截取。编码和主动约束由调用方提供，运动学库不依赖硬件适配器。
pub fn right_wrist_roundtrip(
    start_raw: [i32; 5],
    tolerance_rad: [f64; 5],
    raw_to_rad: fn(i32) -> f32,
    encode: impl Fn(usize, f64, f64) -> Result<i32, String>,
) -> Result<RightWristPreview, String> {
    if !start_raw.iter().all(|raw| (0..=4095).contains(raw)) {
        return Err("五轴起点须是单圈编码器反馈 0…4095 raw".into());
    }
    if !tolerance_rad
        .iter()
        .all(|value| value.is_finite() && *value > 0.0)
    {
        return Err("缺少五轴有效的到位容差".into());
    }
    let model = XLeRobotArmIkSolver::new(Side::Right);
    let as_radians = |raw| f64::from(raw_to_rad(raw));
    let start = start_raw.map(as_radians);
    let origin = model.forward_pose(&start).map_err(|e| e.to_string())?;
    let mut target = origin;
    target.translation[0] += 0.010;

    let solve = |pose: &Pose, from: &[f64; 5]| -> Result<[i32; 5], String> {
        // 实际 S1 在每段启动时都会核对当前姿态是否处于可恢复的 IK 分支。
        let measured = model.forward_pose(from).map_err(|e| e.to_string())?;
        let recovered = model
            .solve(&measured, Some(from), None)
            .map_err(|e| e.to_string())?;
        check_return_branch(&recovered.joints_rad, from, &tolerance_rad)?;

        let solution = model
            .solve(pose, Some(from), None)
            .map_err(|e| e.to_string())?;
        model
            .check_solution(pose, &solution.joints_rad)
            .map_err(|e| e.to_string())?;
        let mut raw = [0; 5];
        for (i, &angle) in solution.joints_rad.iter().enumerate() {
            raw[i] = encode(i, angle, from[i])?;
            if !(0..=4095).contains(&raw[i]) {
                return Err(format!("{} 目标超出单圈编码器范围", ARM_JOINT_NAMES[i]));
            }
        }
        Ok(raw)
    };
    let forward = solve(&target, &start);
    let back = forward.as_ref().ok().map(|out| {
        let raw = solve(&origin, &out.map(as_radians))?;
        check_return_branch(&raw.map(as_radians), &start, &tolerance_rad)?;
        Ok(raw)
    });
    Ok(RightWristPreview {
        origin,
        target,
        forward,
        back,
    })
}

/// 同时校验五轴，避免可达的腕点被逆解到另一种关节姿态。
pub fn check_return_branch(
    recovered: &[f64],
    original: &[f64],
    tolerances: &[f64],
) -> Result<(), String> {
    if original.len() != ARM_JOINT_NAMES.len()
        || recovered.len() != original.len()
        || tolerances.len() != original.len()
    {
        return Err("右臂 IK 分支复核需要五轴反馈、解和到位容差".into());
    }
    for (i, ((value, start), tolerance)) in
        recovered.iter().zip(original).zip(tolerances).enumerate()
    {
        if !(0.0..=*tolerance).contains(&(value - start).abs()) {
            return Err(format!(
                "{} 求解与当前姿态的 IK 分支不一致，无法保证回到原关节位置",
                ARM_JOINT_NAMES[i]
            ));
        }
    }
    Ok(())
}
