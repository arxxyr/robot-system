// 复用 coordinator 的真实任务分发、缓存及混合命令；只在内存中模拟电机回读。
use super::*;
use crate::action_backend::ik::IkBackend;
use robot_adapter_feetech::{rad_to_raw, raw_to_rad};
use robot_kinematics::{ArmIkSolver, Pose};
use robot_kinematics_xlerobot::{Side, XLeRobotArmIkSolver};
use robot_msg::msg::{IkPayload, IkPayloadArgs};
use robot_msg::schema_id::SCHEMA_IK_PAYLOAD;

const START_RAW: [i32; 5] = [1917, 2210, 2275, 1982, 2243];
const FORWARD_RAW: [i32; 5] = [1924, 2257, 2204, 1988, 2243];

struct Bus2 {
    coord: Coordinator,
    channel: MockChannel<markers::JointState>,
    clock: MockClock,
    feedback: Vec<f64>,
    arm: [usize; 5],
    gripper: usize,
    command_seq: u64,
}

impl Bus2 {
    fn new() -> Self {
        let hardware = parse_hardware_section_from_toml(
            include_str!("../../../configs/xlerobot-bus2.toml"),
            "right_arm_full_with_base",
        )
        .unwrap();
        let limits = parse_single_joint_config_from_toml(
            include_str!("../../../configs/xlerobot-bus2-motion.toml"),
            &hardware,
        )
        .unwrap()
        .unwrap();
        let solver = XLeRobotArmIkSolver::new(Side::Right);
        let slot = |name: &str| hardware.joints.iter().position(|n| n == name).unwrap();
        let arm: [usize; 5] =
            std::array::from_fn(|i| slot(&format!("right_arm_{}", solver.info().joint_names[i])));
        let gripper = slot("right_arm_gripper");

        // 轮编码器有非零位置，夹爪实时值也与开机锚点不同，可检出错误保持来源。
        let mut feedback = vec![0.5; hardware.joints.len()];
        for (&i, raw) in arm.iter().zip(START_RAW) {
            feedback[i] = f64::from(raw_to_rad(raw));
        }
        feedback[gripper] = f64::from(raw_to_rad(1800));
        let mut anchor = feedback.clone();
        anchor[gripper] = f64::from(raw_to_rad(2000));
        let coord = Coordinator::new(
            CoordinatorConfig::default(),
            hardware,
            LAYOUT,
            anchor,
            BackendRegistry::from_explicit(&[&IkBackend, &RealScriptedBackend]),
            vec!["ik".into(), "scripted".into()],
            Some(Box::new(solver)),
        )
        .with_single_joint_config(Some(limits));
        let channel = MockChannel::new();
        let coord = attach_cache(coord, &channel);
        coord.mark_idle().unwrap();
        let sim = Self {
            coord,
            channel,
            clock: MockClock::new(MonoRawNs(1_000_000_000)),
            feedback,
            arm,
            gripper,
            command_seq: 0,
        };
        sim.publish_feedback();
        sim
    }

    fn publish_feedback(&self) {
        publish_motion_feedback(
            &self.coord,
            &self.channel,
            &self.clock,
            &self.feedback,
            true,
        );
    }

    fn pose(&self) -> Pose {
        let angles = self.arm.map(|slot| self.feedback[slot]);
        XLeRobotArmIkSolver::new(Side::Right)
            .forward_pose(&angles)
            .unwrap()
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "模拟器沿用 S0 的 f32 位置及整数编码器刻度"
    )]
    fn move_to(&mut self, goal: Pose, elbow_error_raw: i32) -> ActionStatus {
        let mut fb = flatbuffers::FlatBufferBuilder::new();
        let xyz = fb.create_vector(&goal.translation);
        let quat = fb.create_vector(&goal.orientation);
        let payload = IkPayload::create(
            &mut fb,
            &IkPayloadArgs {
                target_xyz: Some(xyz),
                target_quat_xyzw: Some(quat),
                ..Default::default()
            },
        );
        fb.finish(payload, None);
        let id = ActionId::new_v7();
        let req = ActionRequest {
            action_id: id,
            kind: ActionKind::Ik,
            priority: 10,
            deadline_ns: self.clock.now().0 + 22_000_000_000,
            payload_schema_id: SCHEMA_IK_PAYLOAD,
            payload: fb.finished_data().to_vec(),
            idempotency_key: id.to_string(),
            source: ActionSource::Internal,
        };
        let ticket = self
            .coord
            .submit(req, &self.clock)
            .expect("新鲜右总线反馈应接受 IK 位姿");
        for tick in 0..=1100_u64 {
            self.command_seq += 1;
            if let Some(body) = self.coord.tick_action(&self.clock, self.command_seq) {
                let cmd = root::<FbJointCommand<'_>>(body.as_bytes()).unwrap();
                assert_eq!(
                    (cmd.layout_id(), cmd.source_task_id()),
                    (LAYOUT, self.command_seq)
                );
                let target = decode_target(&body);
                assert_eq!(target.len(), 9);
                assert_mixed_target(body.as_bytes(), &self.coord.hardware, &target);
                assert_eq!(
                    target[self.gripper].to_bits(),
                    self.feedback[self.gripper].to_bits()
                );
                self.coord.command_published(self.clock.now().0);
                for &slot in &self.arm {
                    let offset = if slot == self.arm[2] {
                        elbow_error_raw
                    } else {
                        0
                    };
                    self.feedback[slot] =
                        f64::from(raw_to_rad(rad_to_raw(target[slot] as f32) + offset));
                }
            } else {
                let status = self.coord.status(ticket.action_id).unwrap();
                assert!(tick >= 1000, "20 秒轨迹提前结束：第 {tick} 帧，{status:?}");
                if matches!(status, ActionStatus::Completed(ActionOutcome::Success)) {
                    let hold = self.coord.idle_hold_target_for_tick(&self.clock);
                    let body = build_hardware_hold_command(
                        self.coord.config(),
                        &self.coord.hardware,
                        LAYOUT,
                        &hold,
                        self.command_seq,
                        self.clock.now().0,
                    );
                    assert_mixed_target(&body, &self.coord.hardware, &hold);
                    assert_eq!(
                        hold[self.gripper].to_bits(),
                        self.feedback[self.gripper].to_bits()
                    );
                    for &slot in &self.arm {
                        assert!(
                            (rad_to_raw(hold[slot] as f32)
                                - rad_to_raw(self.feedback[slot] as f32))
                            .abs()
                                <= 20
                        );
                    }
                }
                return status;
            }
            self.clock.advance(DurationNs::from_nanos(20_000_000));
            self.publish_feedback();
        }
        panic!("22 秒内 IK 未结束")
    }
}

#[test]
#[allow(
    clippy::cast_possible_truncation,
    reason = "按 S0 的 f32 反馈回算编码器整数值"
)]
fn bus2_ik_forward_10mm_and_return() {
    let mut sim = Bus2::new();
    let origin = sim.pose();
    let mut forward = origin;
    forward.translation[0] += 0.010;

    for (goal, expected_raw) in [(forward, FORWARD_RAW), (origin, START_RAW)] {
        let status = sim.move_to(goal, 0);
        assert!(
            matches!(status, ActionStatus::Completed(ActionOutcome::Success)),
            "{status:?}"
        );
        let actual = sim.pose();
        let delta_mm: [f64; 3] =
            std::array::from_fn(|i| (actual.translation[i] - origin.translation[i]) * 1000.0);
        let error_mm: f64 = actual
            .translation
            .iter()
            .zip(goal.translation)
            .map(|(a, b)| (a - b).powi(2))
            .sum::<f64>()
            .sqrt()
            * 1000.0;
        let actual_raw = sim.arm.map(|slot| rad_to_raw(sim.feedback[slot] as f32));
        assert!(error_mm <= 2.0, "腕点误差 {error_mm} mm");
        assert!(
            actual_raw
                .iter()
                .zip(expected_raw)
                .all(|(&a, b)| (a - b).abs() <= 20)
        );
        eprintln!(
            "相对原点位移 {delta_mm:.3?} mm，误差 {error_mm:.3} mm，五轴回读 {actual_raw:?} raw"
        );
    }
}

#[test]
fn bus2_ik_arrival_uses_configured_encoder_tolerance() {
    // 肘俯仰的连续 IK 目标落在两个编码器刻度之间，检验必须相对实际下发的整数目标。
    for error in [-21_i32, -20, 20, 21] {
        let mut sim = Bus2::new();
        let mut goal = sim.pose();
        goal.translation[0] += 0.010;
        let status = sim.move_to(goal, error);
        if error.abs() == 20 {
            assert!(
                matches!(status, ActionStatus::Completed(ActionOutcome::Success)),
                "{error} raw：{status:?}"
            );
        } else {
            assert!(
                matches!(status, ActionStatus::Failed(ActionFailure::PreconditionFailed(ref reason)) if reason.contains("到位")),
                "{error} raw：{status:?}"
            );
        }
    }
}

#[test]
fn right_arm_home_reads_live_bus2_feedback_through_completion() {
    use robot_msg::msg::{RightArmHomePayload, RightArmHomePayloadArgs};

    let mut sim = Bus2::new();
    let mut builder = flatbuffers::FlatBufferBuilder::new();
    let hardware_id = builder.create_string(&sim.coord.hardware.id);
    let root = RightArmHomePayload::create(&mut builder, &RightArmHomePayloadArgs {hardware_id: Some(hardware_id)});
    builder.finish(root, None);
    let id = ActionId::new_v7();
    let ticket = sim.coord.submit(ActionRequest {
        action_id: id,
        kind: ActionKind::Scripted,
        priority: 10,
        deadline_ns: sim.clock.now().0 + 12_000_000_000,
        payload_schema_id: robot_msg::schema_id::SCHEMA_RIGHT_ARM_HOME_PAYLOAD,
        payload: builder.finished_data().to_vec(),
        idempotency_key: id.to_string(),
        source: ActionSource::Internal,
    }, &sim.clock).unwrap();
    let gripper = sim.feedback[sim.gripper];
    for frame in 0..=550_u64 {
        if let Some(body) = sim.coord.tick_action(&sim.clock, frame + 1) {
            let target = decode_target(&body);
            assert_mixed_target(body.as_bytes(), &sim.coord.hardware, &target);
            assert_eq!(target[sim.gripper], gripper);
            for &slot in &sim.arm { sim.feedback[slot] = target[slot]; }
            sim.coord.command_published(sim.clock.now().0);
        } else {
            assert!(frame > 500);
            assert!(matches!(sim.coord.status(ticket.action_id), Some(ActionStatus::Completed(ActionOutcome::Success))));
            assert_eq!(sim.arm.map(|slot| sim.feedback[slot]), [0.0; 5]);
            return;
        }
        sim.clock.advance(DurationNs::from_nanos(20_000_000));
        sim.publish_feedback();
    }
    panic!("右臂回位经过 10 秒插值与配置的稳定窗口仍未完成");
}
