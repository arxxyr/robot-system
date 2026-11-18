//! 原位准入纯值与镜像连续性验证；不连接设备。

use super::*;
use crate::client::layout::HardwareLayout;
use robot_transport::mock::MockChannel;
use robot_transport::{LatestSubscriber, Publisher};
use std::fmt::Write as _;

fn source(mixed: bool) -> Source {
    let ids: Vec<u8> = if mixed {
        (1..=9).collect()
    } else {
        (1..=8).collect()
    };
    let mut source = Source {
        id: "bus".into(),
        label: "测试".into(),
        endpoint: String::new(),
        robot_id: "robot".into(),
        hardware_id: "arm".into(),
        mixed_raw: mixed,
        calibration_range_tolerances_raw: None,
        layout: HardwareLayout {
            joints: ids.iter().map(|id| format!("joint_{id}")).collect(),
            motor_ids: ids,
            layout_id: 42,
        },
        motion: None,
        enable_alignment_tolerance_rad: Some(0.006_136_5),
        base_simulation: false,
    };
    let mut joints = String::new();
    for (slot, name) in source.layout.joints.iter().enumerate() {
        if !wheel(&source, slot) {
            writeln!(joints, "\n[[joints]]\nname='{name}'\nmin_position_rad=-3.0\nmax_position_rad=3.0\nmax_velocity_rad_s=0.1\ntolerance_rad=0.03").unwrap();
        }
    }
    source.motion = Some(
        toml::from_str(&format!(
            "max_duration_ms=20000\nsettle_timeout_ms=800\nstable_window_ms=200\n{joints}"
        ))
        .unwrap(),
    );
    source
}

fn state(source: &Source, stamp_ns: u64) -> RobotState {
    RobotState {
        robot_id: source.robot_id.clone(),
        hardware_id: source.hardware_id.clone(),
        layout_id: source.layout.layout_id,
        stamp_ns,
        position_rad: vec![0.0; source.layout.joints.len()],
        mode: S0Mode::Safe as i32,
        safety_state: S0SafetyState::TorqueOffConfirmed as i32,
        fault_code: 0,
        ..Default::default()
    }
}

fn frame(seq: u64) -> Command {
    Command {
        metadata: ObserverMetadata {
            publisher_instance_id: [1; 16],
            observer_seq: seq,
            dropped_frames: 0,
        },
        seq,
        stamp_ns: seq * 10_000_000,
        deadline_ns: seq * 10_000_000 + FRESH_NS,
        target: vec![0.0; 8],
    }
}

fn decode_body(
    source: &Source,
    body: &[u8],
    source_id: u32,
    stamp: u64,
) -> Result<Command, String> {
    let channel = MockChannel::<markers::JointCommand>::new();
    let mut publisher = channel.publisher();
    let mut subscriber = channel.subscriber();
    let mut writer = publisher.loan(8192).unwrap();
    writer.set_stamp_ns(stamp);
    writer.set_source_id(source_id);
    writer.set_seq(1);
    writer.set_body_bytes(body);
    writer.publish().unwrap();
    let lease = subscriber.latest().unwrap().unwrap();
    let result = decode(
        source,
        &lease.view().unwrap(),
        frame(1).metadata,
        10_000_000,
    );
    drop(lease);
    assert_eq!(channel.active_leases(), 0);
    result
}

#[test]
fn original_four_count_alignment_accepts_quantization_but_not_five_or_arrival_twenty() {
    let mut source = source(false);
    // 固定设备换算产生的rad样例；原位对齐容差与校准边界容差分别守门。
    let one = 0.001_533_980_7_f64;
    let base = vec![0.0; 8];
    for ticks in [-4.0, -1.0, 0.0, 1.0, 4.0] {
        assert!(aligned(&source, &base, &[ticks * one; 8]).is_ok());
    }
    for ticks in [-20.0, -5.0, 5.0, 20.0] {
        assert!(aligned(&source, &base, &[ticks * one; 8]).is_err());
    }
    source.enable_alignment_tolerance_rad = None;
    assert!(aligned(&source, &base, &base).is_err());
}

#[test]
fn state_rejects_stale_identity_layout_nonfinite_and_non_torque_off() {
    let source = source(false);
    let valid = state(&source, 10);
    assert!(check_state(&source, &valid, 11).is_ok());
    for kind in 0..9 {
        let mut invalid = valid.clone();
        match kind {
            0 => invalid.stamp_ns = 0,
            1 => invalid.stamp_ns = 12,
            2 => invalid.robot_id = "wrong".into(),
            3 => invalid.hardware_id = "wrong".into(),
            4 => invalid.layout_id += 1,
            5 => invalid.position_rad[0] = f64::NAN,
            6 => invalid.position_rad.clear(),
            7 => invalid.mode = S0Mode::Enabled as i32,
            _ => invalid.fault_code = 1,
        }
        assert!(check_state(&source, &invalid, 11).is_err(), "case {kind}");
    }
    assert!(check_state(&source, &valid, FRESH_NS + 11).is_err());
}

#[test]
fn command_requires_matching_source_layout_sequence_mode_and_full_targets() {
    let source = source(false);
    let build = |layout, mode, count, task, timeout| {
        robot_msg::build_helpers::build_joint_command(
            layout,
            mode,
            &vec![0.0; count],
            100_000_000,
            0,
            timeout,
            task,
        )
    };
    let valid = build(42, msg::ControlMode::Position, 8, 1, msg::StaleAction::Hold);
    assert!(decode_body(&source, &valid, 2, 10_000_000).is_ok());
    assert!(decode_body(&source, &valid, 3, 10_000_000).is_err());
    assert!(decode_body(&source, &valid, 2, 0).is_err());
    for invalid in [
        build(41, msg::ControlMode::Position, 8, 1, msg::StaleAction::Hold),
        build(42, msg::ControlMode::Velocity, 8, 1, msg::StaleAction::Hold),
        build(42, msg::ControlMode::Position, 7, 1, msg::StaleAction::Hold),
        build(42, msg::ControlMode::Position, 8, 2, msg::StaleAction::Hold),
    ] {
        assert!(decode_body(&source, &invalid, 2, 10_000_000).is_err());
    }
}

#[test]
fn mixed_command_rejects_nonzero_raw_or_wheel_target_but_ignores_wheel_position_feedback() {
    let source = source(true);
    let mut target = vec![0.0; 9];
    let mut raw = vec![0; 9];
    for kind in 0..3 {
        let body = robot_msg::build_helpers::build_mixed_joint_command(
            42,
            &target,
            &raw,
            100_000_000,
            0,
            1,
        );
        assert_eq!(
            decode_body(&source, &body, 2, 10_000_000).is_ok(),
            kind == 0
        );
        if kind == 0 {
            raw[7] = 50;
        } else {
            raw[7] = 0;
            target[8] = 0.1;
        }
    }
    let mut actual = vec![0.0; 9];
    actual[7] = 2.0;
    assert!(aligned(&source, &actual, &[0.0; 9]).is_ok());
}

#[test]
fn mixed_state_and_command_allow_calibrated_elbow_hold_but_reject_twenty_one() {
    let mut source = source(true);
    let mut tolerances = vec![0; 9];
    tolerances[2] = 20;
    source.calibration_range_tolerances_raw = Some(tolerances);
    let rad = |raw| f64::from(robot_adapter_feetech::raw_to_rad(raw));
    source.motion.as_mut().unwrap().joints[2] = toml::from_str(&format!(
        "name='{}'\nmin_position_rad={}\nmax_position_rad={}\nmax_velocity_rad_s=0.1\ntolerance_rad=0.03",
        source.layout.joints[2], rad(812), rad(2991),
    )).unwrap();
    for raw in [791, 792, 2998, 3011, 3012] {
        let mut actual = state(&source, 10_000_000);
        actual.position_rad[2] = rad(raw);
        // 轮反馈不能拿来做位置保持目标；镜像的轮目标仍为零。
        actual.position_rad[6..].fill(4000.0);
        let mut target = vec![0.0; 9];
        target[2] = rad(raw);
        let body = robot_msg::build_helpers::build_mixed_joint_command(
            42,
            &target,
            &[0; 9],
            100_000_000,
            0,
            1,
        );
        let accepted = (792..=3011).contains(&raw);
        assert_eq!(check_state(&source, &actual, 10_000_001).is_ok(), accepted);
        assert_eq!(decode_body(&source, &body, 2, 10_000_000).is_ok(), accepted);
    }
    source.calibration_range_tolerances_raw = None;
    let mut actual = state(&source, 10_000_000);
    actual.position_rad[2] = rad(2998);
    assert!(check_state(&source, &actual, 10_000_001).is_err());
}

#[test]
fn cache_requires_continuity_and_remembers_interrupted_generation() {
    let source = source(false);
    let mirrors = Mirrors::new(vec![source.clone()]);
    assert!(mirrors.latest(&source, 10_000_000).is_err());
    let mut cache = mirrors.caches.lock().unwrap();
    cache[0].accept(frame(1));
    assert!(!cache[0].continuous);
    cache[0].accept(frame(2));
    assert!(cache[0].continuous);
    cache[0].accept(frame(4));
    assert!(!cache[0].continuous);
    let interrupted = cache[0].generation;
    cache[0].accept(frame(5));
    assert!(cache[0].continuous);
    assert_eq!(cache[0].generation, interrupted);
    let mut changed = frame(6);
    changed.metadata.publisher_instance_id = [2; 16];
    cache[0].accept(changed);
    assert!(cache[0].generation > interrupted);
    drop(cache);
    assert!(mirrors.latest(&source, 60_000_000).is_err());
}

#[test]
fn source_config_keeps_enable_opt_in_and_rejects_bad_tolerance() {
    use super::super::control::SourceConfig;
    let missing: SourceConfig = toml::from_str("motion_config='motion.toml'").unwrap();
    assert!(missing.enable_alignment_tolerance_rad.is_none());
    assert!(missing.validate().is_ok());
    for value in ["nan", "inf", "-0.01"] {
        let config: SourceConfig = toml::from_str(&format!(
            "motion_config='motion.toml'\nenable_alignment_tolerance_rad={value}"
        ))
        .unwrap();
        assert!(config.validate().is_err());
    }
}
