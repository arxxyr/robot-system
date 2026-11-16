//! 固定状态与原配置校验；不连接真实网关，不访问硬件。

use super::*;
use crate::client::layout::load_hardware_layout;

fn source(bus: u8) -> Source {
    let config_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs");
    let path = config_dir.join(format!("xlerobot-bus{bus}.toml"));
    let robot: super::super::RobotConfig =
        toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let hardware = &robot.hardware[0];
    let mut source = Source {
        id: format!("bus{bus}"),
        label: "测试总线".into(),
        endpoint: "http://127.0.0.1:1".into(),
        robot_id: robot.robot.id,
        hardware_id: hardware.id.clone(),
        mixed_raw: hardware.feetech_bus2_mixed_raw,
        layout: load_hardware_layout(&path, &hardware.id).unwrap(),
        motion: None,
        enable_alignment_tolerance_rad: Some(0.006_136_5),
    };
    source.motion = Some(
        MotionConfig::load(
            &config_dir.join(format!("xlerobot-bus{bus}-motion.toml")),
            &source,
        )
        .unwrap(),
    );
    source
}

fn request(operation: Operation) -> Request {
    Request {
        kind: "control".into(),
        request_id: "browser-1".into(),
        bus_id: "bus1".into(),
        process_id: "process-1".into(),
        revision: "0".into(),
        operation,
        joint: None,
        target_position_rad: None,
        duration_ms: None,
        confirmation: None,
    }
}

fn view(source: &Source) -> BusView {
    let mut state = super::super::SourceState::new(source.clone());
    state.accept(
        Ok(crate::proto::RobotState {
            robot_id: source.robot_id.clone(),
            hardware_id: source.hardware_id.clone(),
            layout_id: source.layout.layout_id,
            stamp_ns: 1,
            position_rad: vec![0.0; source.layout.joints.len()],
            mode: 3,
            control_link: 1,
            output_mode: 1,
            safety_state: 1,
            fault_code: 0,
        }),
        1,
    );
    state.view(1, 2000)
}

#[test]
fn original_motion_files_cover_position_joints_only() {
    for (bus, expected) in [(1, 8), (2, 6)] {
        let source = source(bus);
        assert_eq!(source.motion.as_ref().unwrap().joints.len(), expected);
        let control = ControlState::new(true, "process").view(source.motion.as_ref());
        assert!(control.enabled);
        assert!(!control.can_enable);
        assert!(control.enable_reason.contains("原位对齐"));
        assert!(
            control
                .joints
                .iter()
                .all(|joint| joint.tolerance_rad.to_bits()
                    == 0.030_679_702_758_789_063_f64.to_bits())
        );
    }
}

#[test]
fn duplicate_missing_or_wheel_motion_limits_are_rejected() {
    let source = source(2);
    let original = source.motion.as_ref().unwrap();
    let mut missing = original.clone();
    missing.joints.pop();
    assert!(missing.validate(&source).is_err());
    let mut duplicate = original.clone();
    duplicate.joints[1] = duplicate.joints[0].clone();
    assert!(duplicate.validate(&source).is_err());
    let mut wheel = original.clone();
    wheel.joints[0].name.clone_from(&source.layout.joints[8]);
    assert!(wheel.validate(&source).is_err());
    let mut invalid = original.clone();
    invalid.joints[0].max_velocity_rad_s = f64::NAN;
    assert!(invalid.validate(&source).is_err());
}

#[test]
fn enable_alignment_never_accepts_out_of_range_original_or_encoded_positions() {
    let source = source(1);
    let mut motion = source.motion.clone().unwrap();
    let mut positions = vec![0.0; source.layout.joints.len()];
    positions[0] = motion.joints[0].max_position_rad + 0.001;
    assert!(motion.check_positions(&source, &positions).is_err());
    motion.joints[0].max_position_rad = 0.1;
    positions[0] = 0.1;
    // f64落在边界，但实际f32编码略大于0.1，仍应拒绝。
    assert!(motion.check_positions(&source, &positions).is_err());
    positions[0] = 0.09;
    assert!(motion.check_positions(&source, &positions).is_ok());
}

#[test]
fn enable_uses_fresh_probe_instead_of_cached_ui_hint_and_keeps_busy_gate() {
    let source = source(1);
    let mut control = ControlState::new(true, "process-1");
    let state = view(&source);
    assert!(!state.control.can_enable);
    assert!(
        control
            .preflight(&request(Operation::Enable), &source, &state)
            .is_ok()
    );
    control.task = Some(TaskView {
        action_id: "busy".into(),
        state: "unknown",
        progress: None,
        error: None,
    });
    assert!(
        control
            .preflight(&request(Operation::Enable), &source, &state)
            .is_err()
    );
}

#[test]
fn default_config_and_control_state_do_not_open_writes() {
    let source = source(1);
    let control = ControlState::new(false, "process-1");
    assert!(!FileConfig::default().enabled);
    assert!(
        control
            .preflight(&request(Operation::Estop), &source, &view(&source))
            .is_err()
    );
}

#[test]
fn versions_reject_old_pages_for_every_operation() {
    let source = source(1);
    let mut control = ControlState::new(true, "process-1");
    control.revision = 1;
    for operation in [Operation::Disable, Operation::Estop, Operation::Cancel] {
        assert!(
            control
                .preflight(&request(operation), &source, &view(&source))
                .unwrap_err()
                .contains("版本")
        );
    }
    let mut request = request(Operation::Estop);
    request.revision = "1".into();
    request.process_id = "old-process".into();
    assert!(
        control
            .preflight(&request, &source, &view(&source))
            .is_err()
    );
}

#[test]
fn safety_direction_ignores_busy_and_stale_feedback() {
    let source = source(1);
    let mut control = ControlState::new(true, "process-1");
    control.task = Some(TaskView {
        action_id: "task".into(),
        state: "unknown",
        progress: None,
        error: None,
    });
    let mut state = view(&source);
    state.data_status = "stale";
    state.fault_code = Some(42);
    for operation in [Operation::Disable, Operation::Estop, Operation::Cancel] {
        assert!(
            control
                .preflight(&request(operation), &source, &state)
                .is_ok()
        );
    }
}

#[test]
fn move_respects_state_motion_units_and_wheel_exclusion() {
    let source = source(2);
    let state = view(&source);
    let control = ControlState::new(true, "process-1");
    let mut request = request(Operation::Move);
    request.bus_id = source.id.clone();
    request.joint = Some(source.layout.joints[4].clone());
    request.target_position_rad = Some(0.5);
    request.duration_ms = Some(20_000);
    assert!(control.preflight(&request, &source, &state).is_ok());
    request.duration_ms = Some(1);
    assert!(control.preflight(&request, &source, &state).is_err());
    request.duration_ms = Some(20_000);
    request.target_position_rad = Some(f64::INFINITY);
    assert!(control.preflight(&request, &source, &state).is_err());
    request.target_position_rad = Some(0.0);
    request.joint = Some(source.layout.joints[8].clone());
    assert!(control.preflight(&request, &source, &state).is_err());
    request.joint = Some(source.layout.joints[4].clone());
    let mut expired = state.clone();
    expired.data_status = "stale";
    assert!(control.preflight(&request, &source, &expired).is_err());
    let mut inactive = state;
    inactive.mode = Some("Safe".into());
    assert!(control.preflight(&request, &source, &inactive).is_err());
}

#[test]
fn clear_estop_requires_explicit_confirmation_and_never_implies_enable() {
    let mut request = request(Operation::ClearEstop);
    assert!(request.validate_shape().is_err());
    request.confirmation = Some("CONFIRM_CLEAR_ESTOP".into());
    assert!(request.validate_shape().is_ok());
    request.joint = Some("left_arm_wrist_roll".into());
    assert!(request.validate_shape().is_err());
}

#[tokio::test]
async fn enable_is_rejected_before_any_rpc_or_version_change() {
    let source = source(1);
    let mut control = ControlState::new(true, "process-1");
    let mut client = RobotControlClient::new(
        tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
    );
    let (_stop, mut stopped) = tokio::sync::watch::channel(false);
    let reply = control
        .dispatch(
            request(Operation::Enable),
            &source,
            &view(&source),
            RpcContext {
                client: &mut client,
                token: "test-only",
                timeout: Duration::from_millis(1),
                shutdown: &mut stopped,
                commands: None,
                clock: &robot_core::time::SystemClock,
            },
        )
        .await;
    assert_eq!(reply.status, "rejected");
    assert!(reply.message.contains("命令镜像"));
    assert_eq!(control.revision, 0);
    assert!(control.last_request.is_none());
}

#[tokio::test]
async fn exact_replay_returns_cached_unknown_without_rpc() {
    let source = source(1);
    let request = request(Operation::Estop);
    let mut control = ControlState::new(true, "process-1");
    control.revision = 1;
    let mut reply = Reply::rejected(Some(&request), 1, "原请求结果未知");
    reply.status = "unknown";
    control.last_request = Some(request.clone());
    control.last_result = Some(reply);
    let mut client = RobotControlClient::new(
        tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
    );
    let (_stop, mut stopped) = tokio::sync::watch::channel(false);
    let reply = control
        .dispatch(
            request,
            &source,
            &view(&source),
            RpcContext {
                client: &mut client,
                token: "test-only",
                timeout: Duration::from_millis(1),
                shutdown: &mut stopped,
                commands: None,
                clock: &robot_core::time::SystemClock,
            },
        )
        .await;
    assert_eq!(reply.status, "unknown");
    assert_eq!(control.revision, 1);
}

#[test]
fn wrong_task_identity_or_unknown_outcome_cannot_clear_busy() {
    let mut task = TaskView {
        action_id: "known".into(),
        state: "pending",
        progress: None,
        error: None,
    };
    let mut status = TaskStatus {
        request_id: "rpc".into(),
        action_id: "wrong".into(),
        state: 3,
        progress: 1.0,
        outcome: Some(crate::proto::ActionOutcomeProto {
            kind: 1,
            warning: String::new(),
        }),
        failure: None,
    };
    apply_task_status(&mut task, status.clone(), "rpc");
    assert!(task.busy());
    assert_eq!(task.state, "unknown");
    status.action_id = "known".into();
    status.outcome = None;
    apply_task_status(&mut task, status.clone(), "rpc");
    assert!(task.busy());
    status.state = 5;
    apply_task_status(&mut task, status, "rpc");
    assert!(!task.busy());
    assert_eq!(task.state, "cancelled");
}

#[test]
fn origin_is_required_for_writes_but_accepts_proxy_https_same_authority() {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::HOST,
        "robot.example:8443".parse().unwrap(),
    );
    assert!(!super::super::same_origin(&headers));
    headers.insert(
        axum::http::header::ORIGIN,
        "https://robot.example:8443".parse().unwrap(),
    );
    assert!(super::super::same_origin(&headers));
    headers.insert(
        axum::http::header::ORIGIN,
        "https://other.example:8443".parse().unwrap(),
    );
    assert!(!super::super::same_origin(&headers));
    headers.insert(axum::http::header::ORIGIN, "null".parse().unwrap());
    assert!(!super::super::same_origin(&headers));
}

#[test]
fn next_move_requires_source_progress_after_last_write() {
    let source = source(1);
    let mut state = view(&source);
    let mut control = ControlState::new(true, "process-1");
    control.after_stamp_ns = Some(1);
    let mut request = request(Operation::Move);
    request.joint = Some(source.layout.joints[4].clone());
    request.target_position_rad = Some(0.5);
    request.duration_ms = Some(20_000);
    assert!(
        control
            .preflight(&request, &source, &state)
            .unwrap_err()
            .contains("源反馈")
    );
    state.stamp_ns = Some("2".into());
    assert!(control.preflight(&request, &source, &state).is_ok());
    state.control_link = Some("Lost".into());
    assert!(control.preflight(&request, &source, &state).is_err());
}

#[test]
fn missing_or_unknown_failure_keeps_unknown_task_busy() {
    for failure in [
        None,
        Some(crate::proto::ActionFailureProto {
            kind: 0,
            reason: String::new(),
        }),
        Some(crate::proto::ActionFailureProto {
            kind: 99,
            reason: String::new(),
        }),
    ] {
        let mut task = TaskView {
            action_id: "task".into(),
            state: "executing",
            progress: None,
            error: None,
        };
        apply_task_status(
            &mut task,
            TaskStatus {
                request_id: "rpc".into(),
                action_id: "task".into(),
                state: 4,
                progress: 0.0,
                failure,
                outcome: None,
            },
            "rpc",
        );
        assert_eq!(task.state, "unknown");
        assert!(task.busy());
    }
}
