//! 根据五轴编码器原位计算右臂腕点前移一厘米的两段目标，只输出 JSON。
use robot_adapter_feetech::{rad_to_raw, raw_to_rad};
use robot_kinematics::Pose;
use robot_kinematics_xlerobot::preview::{ARM_JOINT_NAMES, right_wrist_roundtrip};
use robot_s1::coordinator::{
    parse_hardware_section_from_toml, parse_single_joint_config_from_toml,
};
use robot_s1::single_joint::checked_encoder_target;
use serde_json::{Value, json};
use std::{env, fs, process::ExitCode};

const BUS2_CONFIG: &str = concat!(
    include_str!("../../../configs/xlerobot-bus2.toml"),
    "\n",
    include_str!("../../../configs/xlerobot-bus2-motion.toml")
);

fn waypoint(pose: &Pose, raw: &[i32]) -> Value {
    json!({"xyz":pose.translation,"quat":pose.orientation,"joints_raw":raw})
}

#[allow(clippy::cast_possible_truncation, reason = "沿用 S0 的 f32 电机接口")]
fn plan(config: &str, start_raw: [i32; 5]) -> Result<Value, String> {
    let hw = parse_hardware_section_from_toml(config, "right_arm_full_with_base")
        .map_err(|e| e.to_string())?;
    let motion = parse_single_joint_config_from_toml(config, &hw)
        .map_err(|e| e.to_string())?
        .ok_or("缺少右臂运动约束")?;
    let limits: Vec<_> = ARM_JOINT_NAMES
        .iter()
        .map(|name| {
            motion
                .joints
                .iter()
                .find(|joint| joint.name.strip_prefix("right_arm_") == Some(*name))
                .ok_or_else(|| format!("缺少 {name} 运动约束"))
        })
        .collect::<Result<_, _>>()?;
    let preview = right_wrist_roundtrip(
        start_raw,
        std::array::from_fn(|i| limits[i].tolerance_rad),
        raw_to_rad,
        |i, requested, start| {
            let limit = limits[i];
            checked_encoder_target(requested, start, motion.max_duration_ms, limit).map_err(
                |error| {
                    format!(
                        "{} 目标 {} raw，主动范围 [{}, {}] raw：{error}",
                        limit.name,
                        rad_to_raw(requested as f32),
                        rad_to_raw(limit.min_position_rad as f32),
                        rad_to_raw(limit.max_position_rad as f32),
                    )
                },
            )
        },
    )?;
    let out = preview
        .forward
        .map_err(|error| format!("前移预检：{error}"))?;
    let back = preview
        .back
        .ok_or("缺少回程预检")?
        .map_err(|error| format!("回程预检：{error}"))?;
    Ok(
        json!({"start":waypoint(&preview.origin,&start_raw),"forward":waypoint(&preview.target,&out),"return":waypoint(&preview.origin,&back)}),
    )
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1).peekable();
    let config = if args.next_if(|a| a == "--config").is_some() {
        fs::read_to_string(args.next().ok_or("缺少配置路径")?).map_err(|e| e.to_string())?
    } else {
        BUS2_CONFIG.into()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_plan_matches_nine_slot_s1_mock() {
        let start = [1917, 2210, 2275, 1982, 2243];
        let result = plan(BUS2_CONFIG, start).unwrap();
        assert_eq!(
            result["forward"]["joints_raw"],
            json!([1924, 2257, 2204, 1988, 2243])
        );
        assert_eq!(result["return"]["joints_raw"], json!(start));
        let x = |step: &str| result[step]["xyz"][0].as_f64().unwrap();
        assert!((x("forward") - x("start") - 0.010).abs() < 1e-12);
    }

    #[test]
    fn six_counts_beyond_elbow_limit_only_rejects_return() {
        let error = plan(BUS2_CONFIG, [2086, 1068, 2997, 2336, 1009]).unwrap_err();
        assert!(error.starts_with("回程预检"), "{error}");
        assert!(
            error.contains("2997 raw") && error.contains("2991"),
            "{error}"
        );
    }

    #[test]
    fn rejects_unrecoverable_elbow_branch_before_forward() {
        let error = plan(BUS2_CONFIG, [1917, 2210, 1069, 1982, 2243]).unwrap_err();
        assert!(
            error.starts_with("前移预检") && error.contains("分支"),
            "{error}"
        );
    }
}
