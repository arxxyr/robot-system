use super::*;
use observation::{MotionWheel, WheelSample};

fn arguments(output: &str) -> Vec<String> {
    [
        "--config",
        "/software-fixture/config.toml",
        "--hardware-id",
        "right_arm_full_with_base",
        "--s0-log",
        "/software-fixture/s0.log",
        "--output",
        output,
        "--go-expires-at-unix-ns",
        "18446744073709551615",
        "--ack-safety-checklist",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn fixture_config() -> Config {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../configs/xlerobot_bus2_full.toml");
    Config::load(&Args {
        config: path,
        hardware_id: "right_arm_full_with_base".into(),
        log: PathBuf::new(),
        output: PathBuf::new(),
        expires_unix_ns: u64::MAX,
    })
    .unwrap()
}

fn state(_config: &Config) -> State {
    let positions = std::array::from_fn(|slot| {
        f64::from(robot_adapter_feetech::raw_to_rad(
            2000 + i32::try_from(slot).unwrap(),
        ))
    });
    State {
        stamp: 100,
        positions,
        raw: std::array::from_fn(|slot| 2000 + i32::try_from(slot).unwrap()),
        mode: msg::S0Mode::Safe,
        link: msg::S0ControlLink::Unknown,
        output: msg::S0OutputMode::Unknown,
        safety: msg::S0SafetyState::TorqueOffConfirmed,
    }
}

#[test]
fn cli_requires_explicit_ack_expiry_absolute_paths_and_fresh_output() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("new.json");
    let mut raw = arguments(output.to_str().unwrap());
    assert!(Args::parse(raw.clone().into_iter()).unwrap().is_some());
    raw.pop();
    assert!(Args::parse(raw.into_iter()).is_err());
    let mut raw = arguments(output.to_str().unwrap());
    let expiry = raw
        .iter()
        .position(|value| value == "--go-expires-at-unix-ns")
        .unwrap();
    raw[expiry + 1] = "0".into();
    assert!(Args::parse(raw.into_iter()).is_err());
    let mut raw = arguments(output.to_str().unwrap());
    raw[1] = "relative.toml".into();
    assert!(Args::parse(raw.into_iter()).is_err());
    std::fs::write(&output, b"existing").unwrap();
    assert!(Args::parse(arguments(output.to_str().unwrap()).into_iter()).is_err());
    assert_eq!(std::fs::read(output).unwrap(), b"existing");
}

#[test]
fn configuration_selects_only_the_front_wheel_from_the_original_toml() {
    let config = fixture_config();
    assert_eq!(config.layout.joints[config.selected], "base_front_wheel");
    assert_eq!(
        config.wheel_ids,
        config.wheel_slots.map(|slot| config.layout.motor_ids[slot])
    );
    let directory = tempfile::tempdir().unwrap();
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../configs/xlerobot_bus2_full.toml");
    let mut value: toml::Value = toml::from_str(&std::fs::read_to_string(source).unwrap()).unwrap();
    let hardware = &mut value["hardware"][0];
    let ids = hardware["motor_ids"].as_array_mut().unwrap();
    let front = config.selected;
    let other = config
        .wheel_slots
        .into_iter()
        .find(|slot| *slot != front)
        .unwrap();
    ids.swap(front, other);
    let path = directory.path().join("remapped.toml");
    std::fs::write(&path, toml::to_string(&value).unwrap()).unwrap();
    let remapped = Config::load(&Args {
        config: path,
        hardware_id: config.hardware,
        log: PathBuf::new(),
        output: PathBuf::new(),
        expires_unix_ns: u64::MAX,
    })
    .unwrap();
    assert_eq!(
        remapped.layout.joints[remapped.selected],
        "base_front_wheel"
    );
    assert_eq!(
        remapped.layout.motor_ids[remapped.selected],
        config.layout.motor_ids[other]
    );
    assert_ne!(remapped.layout.layout_id, config.layout.layout_id);
}

#[test]
fn cycle_schedule_is_absolute_anchor_bounded_and_never_extends_twenty_seconds() {
    let start = FRESH_NS - 1;
    assert!(pulse_deadlines(0, 0, start).is_ok());
    assert!(pulse_deadlines(0, 0, FRESH_NS).is_err());
    assert!(pulse_deadlines(0, ENABLE_NS, ENABLE_NS).is_err());
    for cycle in 0..400 {
        let (scheduled, end) = cycle_deadline(start, 0, cycle).unwrap();
        assert_eq!(scheduled, start + cycle * PERIOD_NS);
        assert!(end <= start + PULSE_NS);
        if cycle == 0 {
            assert_eq!(end, FRESH_NS);
        } else {
            assert_eq!(end, start + (cycle + 1) * PERIOD_NS);
        }
        assert!(before(end, end).is_err());
    }
    assert_eq!(cycle_deadline(start, 0, 399).unwrap().1, start + PULSE_NS);
    assert!(cycle_deadline(start, 0, 400).is_err());
    assert!(cycle_deadline(u64::MAX, 0, 0).is_err());
    assert!(pulse_deadlines(u64::MAX, 0, 0).is_err());
    // IPC有效期独立于本地50ms拍次；不借有效期补发，仍固定21秒上限。
    assert_eq!(
        source_deadline(100, 1_000_000_000, None).unwrap(),
        100 + FRESH_NS
    );
    assert_eq!(source_deadline(100, 150, None).unwrap(), 150);
    assert_eq!(
        source_deadline(100, 1_000_000_000, Some(0)).unwrap(),
        FRESH_NS
    );
    assert!(source_deadline(150, 150, None).is_err());
}

#[test]
fn frozen_anchor_does_not_follow_new_feedback_or_clamp_positions() {
    let config = fixture_config();
    let original = state(&config);
    let anchor = Anchor::freeze(&original, &config);
    for slot in config.wheel_slots {
        assert_eq!(anchor.target[slot], 0.0);
    }
    let arm = config.arm_slots[0];
    let mut changed = original.clone();
    changed.raw[arm] += 4;
    anchor.check(&changed, &config).unwrap();
    changed.raw[arm] += 1;
    assert!(anchor.check(&changed, &config).is_err());
    assert_eq!(anchor.target[arm], original.positions[arm]);
    assert_eq!(anchor.raw, original.raw);
    let held = config
        .wheel_slots
        .into_iter()
        .find(|slot| *slot != config.selected)
        .unwrap();
    let mut changed = original.clone();
    changed.raw[held] += 5;
    assert!(anchor.check(&changed, &config).is_err());
    let mut changed = original.clone();
    changed.raw[config.selected] += 500;
    anchor.check(&changed, &config).unwrap();
}

#[test]
fn safe_unknown_wire_link_allows_the_existing_read_only_enable_probe() {
    let config = fixture_config();
    let snapshot = state(&config);
    assert_eq!(snapshot.link, msg::S0ControlLink::Unknown);
    assert_eq!(snapshot.output, msg::S0OutputMode::Unknown);
    assert!(
        snapshot.torque_off(),
        "Safe+TorqueOffConfirmed必须允许准备阶段探针，不能等不存在的wire Ok"
    );
    assert!(!snapshot.torque_on());
    let wire = build_helpers::build_set_enabled_response_typed(
        "probe",
        msg::RpcStatus::Ok,
        msg::SetEnabledStatus::WouldAccept,
        1,
        "precondition would accept (not a guarantee adapter will ack)",
        msg::SideEffect::None,
        true,
    );
    assert_eq!(
        check_set(&wire, "probe", true).unwrap(),
        msg::SetEnabledStatus::WouldAccept
    );
}

#[test]
fn state_requires_layout_nine_finite_positions_and_absolute_freshness() {
    let config = fixture_config();
    for (layout_id, len, position, stamp, now, accepted) in [
        (config.layout.layout_id, 9, 0.0, 0, FRESH_NS, true),
        (config.layout.layout_id, 9, 0.0, 0, FRESH_NS + 1, false),
        (config.layout.layout_id, 9, 0.0, 2, 1, false),
        (config.layout.layout_id ^ 1, 9, 0.0, 1, 1, false),
        (config.layout.layout_id, 8, 0.0, 1, 1, false),
        (config.layout.layout_id, 9, f64::NAN, 1, 1, false),
    ] {
        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let positions = builder.create_vector(&vec![position; len]);
        let wire = msg::JointState::create(
            &mut builder,
            &msg::JointStateArgs {
                layout_id,
                position_rad: Some(positions),
                stamp_ns: stamp,
                ..msg::JointStateArgs::default()
            },
        );
        builder.finish(wire, None);
        let value = flatbuffers::root::<msg::JointState>(builder.finished_data()).unwrap();
        assert_eq!(State::decode(&value, &config, now).is_ok(), accepted);
    }
}

#[test]
fn pp_rule_preserves_wrap_and_rejects_reverse_or_oversized_steps() {
    assert_eq!(forward_step(4090, 4).unwrap(), 10);
    assert_eq!(forward_step(500, 500).unwrap(), 0);
    assert_eq!(forward_step(1, 513).unwrap(), 512);
    assert!(forward_step(2, 1).is_err());
    assert!(forward_step(1, 514).is_err());
}

#[test]
fn live_responses_require_identity_committed_retired_and_actual_completion() {
    let live_with_error = build_helpers::build_set_enabled_response_typed(
        "request",
        msg::RpcStatus::Ok,
        msg::SetEnabledStatus::Ok,
        1,
        "precondition would accept (not a guarantee adapter will ack)",
        msg::SideEffect::Committed,
        true,
    );
    assert!(check_set(&live_with_error, "request", false).is_err());
    for (id, status, domain, effect, retired, completed, accepted) in [
        (
            "request",
            msg::RpcStatus::Ok,
            msg::SetEnabledStatus::Ok,
            msg::SideEffect::Committed,
            true,
            1,
            true,
        ),
        (
            "other",
            msg::RpcStatus::Ok,
            msg::SetEnabledStatus::Ok,
            msg::SideEffect::Committed,
            true,
            1,
            false,
        ),
        (
            "request",
            msg::RpcStatus::Ok,
            msg::SetEnabledStatus::Ok,
            msg::SideEffect::Started,
            true,
            1,
            false,
        ),
        (
            "request",
            msg::RpcStatus::Ok,
            msg::SetEnabledStatus::Ok,
            msg::SideEffect::Committed,
            false,
            1,
            false,
        ),
        (
            "request",
            msg::RpcStatus::Ok,
            msg::SetEnabledStatus::Ok,
            msg::SideEffect::Committed,
            true,
            0,
            false,
        ),
        (
            "request",
            msg::RpcStatus::FailedPrecondition,
            msg::SetEnabledStatus::LatchBlocked,
            msg::SideEffect::None,
            true,
            0,
            false,
        ),
    ] {
        let wire = build_helpers::build_set_enabled_response_typed(
            id, status, domain, completed, "", effect, retired,
        );
        assert_eq!(check_set(&wire, "request", false).is_ok(), accepted);
    }
    for (domain, retired, superseded, accepted) in [
        (msg::ClearEStopStatus::Cleared, true, false, true),
        (msg::ClearEStopStatus::WouldClear, true, false, false),
        (msg::ClearEStopStatus::Cleared, false, false, false),
        (msg::ClearEStopStatus::Cleared, true, true, false),
    ] {
        let wire = build_helpers::build_clear_estop_response_typed(
            "request",
            msg::RpcStatus::Ok,
            "",
            1,
            false,
            build_helpers::ClearResponseMeta {
                domain,
                superseded,
                effect: msg::SideEffect::Committed,
                retired,
            },
        );
        assert_eq!(check_clear(&wire, "request").is_ok(), accepted);
    }
}

#[test]
fn zero_groups_have_three_original_nonrenewable_intervals() {
    let wheels = [7, 8, 9].map(|id| WheelSample {
        id,
        torque: 1,
        goal: 0,
        velocity: 0,
    });
    for (index, start, finish) in [
        (0, 50_000_000, 250_000_000),
        (1, 250_000_000, 450_000_000),
        (2, 450_000_000, 500_000_000),
    ] {
        let sample = ZeroSample {
            started_ns: start,
            finished_ns: finish,
            wheels,
        };
        assert!(zero_slot(&sample, 0, index).unwrap());
        assert!(
            zero_slot(
                &ZeroSample {
                    finished_ns: finish + 1,
                    ..sample
                },
                0,
                index
            )
            .is_err()
        );
        assert!(
            !zero_slot(
                &ZeroSample {
                    started_ns: start - 1,
                    ..sample
                },
                0,
                index
            )
            .unwrap()
        );
    }
}

fn motion_sample(config: &Config, anchor: &Anchor, window: u64, velocity: u16) -> MotionSample {
    let started_ns = window * 250_000_000;
    MotionSample {
        started_ns,
        finished_ns: started_ns + 1,
        wheels: config.wheel_slots.map(|slot| MotionWheel {
            id: config.layout.motor_ids[slot],
            position: if slot == config.selected {
                3000 + u16::try_from(window).unwrap()
            } else {
                u16::try_from(anchor.raw[slot]).unwrap()
            },
            velocity: if slot == config.selected {
                velocity
            } else {
                0x8032
            },
        }),
    }
}

#[test]
fn motion_requires_79_distinct_windows_positive_original_pv_and_positive_pp() {
    let config = fixture_config();
    let original = state(&config);
    let anchor = Anchor::freeze(&original, &config);
    let mut check = MotionCheck::new(0);
    for window in 1..80 {
        let sample = motion_sample(&config, &anchor, window, 500);
        check.accept(&sample, &config, &anchor).unwrap();
        check.accept(&sample, &config, &anchor).unwrap();
        assert_eq!(check.samples.len(), usize::try_from(window).unwrap());
    }
    check.finish().unwrap();
    assert_eq!(check.travel, 78);
    let mut no_velocity = MotionCheck::new(0);
    for window in 1..80 {
        no_velocity
            .accept(
                &motion_sample(&config, &anchor, window, 0),
                &config,
                &anchor,
            )
            .unwrap();
    }
    assert!(no_velocity.finish().is_err());
    let mut no_position = MotionCheck::new(0);
    for window in 1..80 {
        let mut sample = motion_sample(&config, &anchor, window, 500);
        sample
            .wheels
            .iter_mut()
            .find(|wheel| wheel.id == config.layout.motor_ids[config.selected])
            .unwrap()
            .position = 3000;
        no_position.accept(&sample, &config, &anchor).unwrap();
    }
    assert!(no_position.finish().is_err());
}

#[test]
fn motion_rejects_wrong_window_reverse_velocity_and_unselected_wheel_motion() {
    let config = fixture_config();
    let anchor = Anchor::freeze(&state(&config), &config);
    let sample = motion_sample(&config, &anchor, 1, 500);
    assert!(
        MotionCheck::new(0)
            .accept(
                &MotionSample {
                    finished_ns: 500_000_001,
                    ..sample
                },
                &config,
                &anchor
            )
            .is_err()
    );
    assert!(
        MotionCheck::new(0)
            .accept(&motion_sample(&config, &anchor, 2, 500), &config, &anchor)
            .is_err()
    );
    assert!(MotionCheck::new(0).check_deadline(500_000_000).is_err());
    assert!(
        MotionCheck::new(0)
            .accept(
                &motion_sample(&config, &anchor, 1, 0x8001),
                &config,
                &anchor
            )
            .is_err()
    );
    for move_position in [false, true] {
        let mut wrong = sample;
        let held = wrong
            .wheels
            .iter_mut()
            .find(|wheel| wheel.id != config.layout.motor_ids[config.selected])
            .unwrap();
        if move_position {
            held.position += 5;
        } else {
            held.velocity = 51;
        }
        assert!(
            MotionCheck::new(0)
                .accept(&wrong, &config, &anchor)
                .is_err()
        );
    }
}
