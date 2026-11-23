//! 根据五轴编码器原位计算右臂腕点前移一厘米的两段目标，只输出 JSON。
use robot_adapter_feetech::{rad_to_raw, raw_to_rad};
use robot_kinematics::{ArmIkSolver, Pose};
use robot_kinematics_xlerobot::{Side, XLeRobotArmIkSolver};
use robot_s1::coordinator::{
    parse_hardware_section_from_toml, parse_single_joint_config_from_toml,
};
use robot_s1::single_joint::checked_encoder_target;
use serde_json::{Value, json};
use std::{env, fs, process::ExitCode};

fn waypoint(pose: &Pose, raw: &[i32]) -> Value {
    json!({"xyz":pose.translation,"quat":pose.orientation,"joints_raw":raw})
}

#[allow(clippy::cast_possible_truncation, reason = "沿用 S0 的 f32 电机接口")]
fn plan(config: &str, start_raw: [i32; 5]) -> Result<Value, String> {
    let arm = XLeRobotArmIkSolver::new(Side::Right);
    let hw = parse_hardware_section_from_toml(config, "right_arm_full_with_base")
        .map_err(|e| e.to_string())?;
    let motion = parse_single_joint_config_from_toml(config, &hw)
        .map_err(|e| e.to_string())?
        .ok_or("缺少右臂运动约束")?;
    let start = start_raw.map(|r| f64::from(raw_to_rad(r)));
    let origin = arm.forward_pose(&start).map_err(|e| e.to_string())?;
    let mut forward = origin;
    forward.translation[0] += 0.010;

    let solve = |pose: &Pose, from: &[f64; 5]| -> Result<[i32; 5], String> {
        let solution = arm
            .solve(pose, Some(from), None)
            .map_err(|e| e.to_string())?;
        arm.check_solution(pose, &solution.joints_rad)
            .map_err(|e| e.to_string())?;
        let mut raw = [0; 5];
        for (i, (name, &target)) in arm
            .info()
            .joint_names
            .iter()
            .zip(&solution.joints_rad)
            .enumerate()
        {
            let limit = motion
                .joints
                .iter()
                .find(|j| j.name.strip_prefix("right_arm_") == Some(name.as_str()))
                .ok_or_else(|| format!("缺少 {name} 运动约束"))?;
            raw[i] = checked_encoder_target(target, from[i], motion.max_duration_ms, limit)
                .map_err(|e| {
                    format!(
                        "{name} 目标 {} raw，主动范围 [{}, {}] raw：{e}",
                        rad_to_raw(target as f32),
                        rad_to_raw(limit.min_position_rad as f32),
                        rad_to_raw(limit.max_position_rad as f32)
                    )
                })?;
        }
        Ok(raw)
    };
    let out = solve(&forward, &start).map_err(|e| format!("前移预检：{e}"))?;
    let back = solve(&origin, &out.map(|r| f64::from(raw_to_rad(r))))
        .map_err(|e| format!("回程预检：{e}"))?;
    for (i, name) in arm.info().joint_names.iter().enumerate() {
        let limit = motion
            .joints
            .iter()
            .find(|j| j.name.strip_prefix("right_arm_") == Some(name.as_str()))
            .ok_or_else(|| format!("缺少 {name} 运动约束"))?;
        if (f64::from(raw_to_rad(back[i])) - start[i]).abs() > limit.tolerance_rad {
            return Err(format!("{name} 求解回程与实际原位不同分支"));
        }
    }
    Ok(
        json!({"start":waypoint(&origin,&start_raw),"forward":waypoint(&forward,&out),"return":waypoint(&origin,&back)}),
    )
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1).peekable();
    let config = if args.next_if(|a| a == "--config").is_some() {
        fs::read_to_string(args.next().ok_or("缺少配置路径")?).map_err(|e| e.to_string())?
    } else {
        concat!(
            include_str!("../../../configs/xlerobot-bus2.toml"),
            "\n",
            include_str!("../../../configs/xlerobot-bus2-motion.toml")
        )
        .into()
    };
    let raw: Vec<_> = args
        .map(|a| {
            a.parse::<u16>()
                .ok()
                .filter(|v| *v <= 4095)
                .map(i32::from)
                .ok_or_else(|| format!("无效的 12 位编码器位置 {a}"))
        })
        .collect::<Result<_, _>>()?;
    let raw = raw.try_into().map_err(
        |_| "用法：arm-ik-roundtrip [--config 配置] 肩水平 肩俯仰 肘 腕俯仰 腕旋转（raw）",
    )?;
    println!("{}", plan(&config, raw)?);
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
