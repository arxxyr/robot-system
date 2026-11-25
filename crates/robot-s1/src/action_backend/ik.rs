//! S1 的 IK 后端以肩基座腕点位姿为输入。五轴位置设备沿用单帧求解。
//! XLeRobot 右侧混合总线按配置名称投影五轴，夹爪保持提交时位置，三轮目标为 raw 0。
//! 混合请求复用 Scripted 的限速插值、发布回执、到位确认及中断保持。

#![allow(clippy::doc_markdown)]

use crate::action::{
    ActionBackend, ActionCancelError, ActionContext, ActionFailure, ActionInstance, ActionKind,
    ActionOutcome, ActionRequest, ActionStartError, ActionTickOutput, JointCommandBody,
};
use crate::coordinator::build_hold_command;
use robot_kinematics::{IkError, Pose};
use robot_msg::msg::IkPayload as FbIkPayload;
use robot_msg::schema_id::SCHEMA_IK_PAYLOAD;

// ============================================================================
// IkBackend
// ============================================================================

/// 将笛卡尔请求交给当前求解器，再按硬件布局执行位置命令。
#[derive(Debug, Clone, Copy)]
pub struct IkBackend;

#[allow(clippy::unnecessary_literal_bound)]
impl ActionBackend for IkBackend {
    fn name(&self) -> &str {
        "ik"
    }

    fn supported_kinds(&self) -> &[ActionKind] {
        const KINDS: &[ActionKind] = &[ActionKind::Ik];
        KINDS
    }

    fn supported_schemas(&self) -> &[u32] {
        // **不**含 NO_PAYLOAD_SCHEMA_ID：IkPayload 必带；跟 hold_pose /
        // mock_dwell 显著区分（同 scripted 风格）。
        const SCHEMAS: &[u32] = &[SCHEMA_IK_PAYLOAD];
        SCHEMAS
    }

    #[allow(clippy::too_many_lines)] // 8 步顺序检查链 + 错误映射 + 实例构造，
    // 拆 helper 会丢"短路顺序锁定"的语义；保持单函数审计友好。
    fn start(
        &self,
        req: &ActionRequest,
        ctx: &ActionContext<'_>,
    ) -> Result<Box<dyn ActionInstance>, ActionStartError> {
        // (1) defensive payload_schema_id check（dispatch 已守门，这里
        // 再防御性确认；如未来 dispatch 路径漂移可立刻定位）。
        if req.payload_schema_id != SCHEMA_IK_PAYLOAD {
            return Err(ActionStartError::Rejected(format!(
                "IkBackend expects payload_schema_id == SCHEMA_IK_PAYLOAD \
                 ({SCHEMA_IK_PAYLOAD:#010x}), got {:#010x}",
                req.payload_schema_id
            )));
        }

        // (2) solver 必须注入（Q-IK10b (i) startup 校验兜底）。validate_solver_for_profile
        // 在 robot-s1 启动期早已拦下 profile = ["ik"] + solver = None
        // 的路径；本步 defensive 防 unit test 漏注入 / 未来 wiring 漂移。
        let solver = ctx.solver.ok_or_else(|| {
            ActionStartError::Rejected(
                "IkBackend: solver not injected into ActionContext; \
                 should have been caught at startup by validate_solver_for_profile \
                 (Q-IK10b (i))"
                    .to_string(),
            )
        })?;

        // (3) deadline pre-guard（Plan §P.3.2 #12 (a) backend guard 路径
        // 之一；ActionRequest::validate 已查 deadline > now_ns 但那是
        // coordinator.submit 入口；这里 defensive 再查一次，防 solver
        // 启动到 start 之间有显著延迟）。
        let pre_now_ns = ctx.clock.now().0;
        if req.deadline_ns <= pre_now_ns {
            return Err(ActionStartError::Rejected(format!(
                "IkBackend: deadline_ns ({}) already past at start (now={pre_now_ns})",
                req.deadline_ns
            )));
        }

        // (4) fbs decode + verify（root::<FbIkPayload> 已含 verify）。任
        // 何 verify fail / 字段读失败走 PayloadDecode 路径；跟 semantic
        // 错的 Rejected 严格区分（Plan §P.2.3.d 两层错误分类锁定）。
        let fb = flatbuffers::root::<FbIkPayload<'_>>(&req.payload).map_err(|e| {
            ActionStartError::PayloadDecode(format!(
                "IkPayload fbs verify failed ({} bytes): {e}",
                req.payload.len()
            ))
        })?;

        // (5) target_xyz.len() == 3 + target_quat_xyzw.len() == 4 +
        // seed_joints_rad.len() == 0 || == info().joint_count。decode
        // 期一次性 owned 拷贝；start 完后 fbs 表不再访问。
        let target_xyz_v = fb.target_xyz().ok_or_else(|| {
            ActionStartError::Rejected(
                "IkPayload.target_xyz must be present (got null vector)".into(),
            )
        })?;
        if target_xyz_v.len() != 3 {
            return Err(ActionStartError::Rejected(format!(
                "IkPayload.target_xyz length {} != 3",
                target_xyz_v.len()
            )));
        }
        let target_xyz: [f64; 3] = [
            target_xyz_v.get(0),
            target_xyz_v.get(1),
            target_xyz_v.get(2),
        ];

        let target_quat_v = fb.target_quat_xyzw().ok_or_else(|| {
            ActionStartError::Rejected(
                "IkPayload.target_quat_xyzw must be present (got null vector)".into(),
            )
        })?;
        if target_quat_v.len() != 4 {
            return Err(ActionStartError::Rejected(format!(
                "IkPayload.target_quat_xyzw length {} != 4",
                target_quat_v.len()
            )));
        }
        let target_quat: [f64; 4] = [
            target_quat_v.get(0),
            target_quat_v.get(1),
            target_quat_v.get(2),
            target_quat_v.get(3),
        ];

        // 纯位置布局沿用原有维度约束；混合布局在投影时按右臂名称逐槽核对。
        let info_joint_count = solver.info().joint_count;
        if !ctx.hardware.feetech_bus2_mixed_raw && ctx.hardware.joints.len() != info_joint_count {
            return Err(ActionStartError::HardwareMismatch(format!(
                "IkBackend: solver info().joint_count {} != hardware.joints.len() {}",
                info_joint_count,
                ctx.hardware.joints.len()
            )));
        }

        let seed_owned: Option<Vec<f64>> = match fb.seed_joints_rad() {
            None => None,
            Some(v) if v.is_empty() => None,
            Some(v) => {
                if v.len() != info_joint_count {
                    return Err(ActionStartError::Rejected(format!(
                        "IkPayload.seed_joints_rad length {} != info().joint_count {}",
                        v.len(),
                        info_joint_count
                    )));
                }
                Some(v.iter().collect())
            }
        };

        // (7) solver.solve(&pose, seed, None)。Plan §P.3.2 #12 (a) 拍
        // 板：backend 收 None，让 solver 永远不返 IkError::DeadlineExceeded；
        // deadline 由 backend pre/post guard 单一权威。
        let pose = Pose {
            translation: target_xyz,
            orientation: target_quat,
        };
        let solution = solver
            .solve(&pose, seed_owned.as_deref(), /* deadline_ns */ None)
            .map_err(map_ik_error_to_start_error)?;

        // (8) deadline post-guard（solver 期间可能耗时；如已过 deadline
        // 拒绝 start，不发 cmd）。
        let post_now_ns = ctx.clock.now().0;
        if req.deadline_ns <= post_now_ns {
            return Err(ActionStartError::Rejected(format!(
                "IkBackend: deadline_ns ({}) exceeded during solve (now={post_now_ns})",
                req.deadline_ns
            )));
        }

        if ctx.hardware.feetech_bus2_mixed_raw {
            #[cfg(feature = "backend-scripted")]
            {
                if fb.frame().is_some_and(|name| !name.is_empty()) {
                    return Err(ActionStartError::Rejected(
                        "右臂 IK 只接受默认肩基座坐标系".into(),
                    ));
                }
                return start_right_arm(req, ctx, solver, &pose, &solution.joints_rad);
            }
            #[cfg(not(feature = "backend-scripted"))]
            return Err(ActionStartError::Rejected(
                "右臂混合 IK 需要 backend-scripted 插值及反馈确认".into(),
            ));
        }

        // (9) build instance：target_joints owned + 状态 flag。
        Ok(Box::new(IkInstance {
            target_joints: solution.joints_rad,
            emitted: false,
            cancelled: false,
        }))
    }
}

/// 将五轴解绑定到当前右总线，并在发命令前核对模型能否到达目标以及恢复原姿态。
#[cfg(feature = "backend-scripted")]
fn start_right_arm(
    req: &ActionRequest,
    ctx: &ActionContext<'_>,
    solver: &dyn robot_kinematics::ArmIkSolver,
    goal: &Pose,
    solution: &[f64],
) -> Result<Box<dyn ActionInstance>, ActionStartError> {
    use super::scripted::ScriptedBackend;
    use robot_kinematics::ArmIkSolver;
    use robot_kinematics_xlerobot::{Side, XLeRobotArmIkSolver};

    let hardware = ctx.hardware;
    let model = XLeRobotArmIkSolver::new(Side::Right);
    let names = &model.info().joint_names;
    if !hardware.mixed_layout_is_valid()
        || solver.info().joint_names != *names
        || solution.len() != names.len()
        || !hardware
            .joints
            .iter()
            .enumerate()
            .any(|(slot, name)| name == "right_arm_gripper" && !hardware.is_wheel_slot(slot))
    {
        return Err(ActionStartError::HardwareMismatch(
            "右总线需要五轴右臂、位置夹爪与三个轮速槽".into(),
        ));
    }

    let start = ctx
        .feedback
        .ok_or_else(|| ActionStartError::Rejected("右臂 IK 缺少提交时完整反馈".into()))?;
    let limits = ctx
        .single_joint_config
        .ok_or_else(|| ActionStartError::Rejected("右臂 IK 缺少位置关节运动约束".into()))?;
    let mut targets = Vec::with_capacity(names.len());
    let mut initial = Vec::with_capacity(names.len());
    let mut tolerances = Vec::with_capacity(names.len());
    for (name, &angle) in names.iter().zip(solution) {
        let slot = hardware
            .joints
            .iter()
            .position(|joint| joint.strip_prefix("right_arm_") == Some(name.as_str()))
            .filter(|&slot| !hardware.is_wheel_slot(slot))
            .ok_or_else(|| {
                ActionStartError::HardwareMismatch(format!("右总线缺少 IK 位置关节 {name}"))
            })?;
        initial.push(
            *start
                .positions
                .get(slot)
                .ok_or_else(|| ActionStartError::Rejected("右臂 IK 提交反馈维度不完整".into()))?,
        );
        let limit = limits
            .joints
            .iter()
            .find(|limit| limit.name == hardware.joints[slot])
            .ok_or_else(|| ActionStartError::Rejected(format!("右臂 IK 缺少运动约束 {name}")))?;
        tolerances.push(limit.tolerance_rad);
        targets.push((slot, angle));
    }

    // 上游闭式逆解固定一个肘分支。先验证这个分支能恢复本次真实原位。
    let initial_pose = model
        .forward_pose(&initial)
        .map_err(map_ik_error_to_start_error)?;
    let recovered = solver
        .solve(&initial_pose, Some(&initial), None)
        .map_err(map_ik_error_to_start_error)?;
    robot_kinematics_xlerobot::preview::check_return_branch(
        &recovered.joints_rad,
        &initial,
        &tolerances,
    )
    .map_err(ActionStartError::Rejected)?;

    model
        .check_solution(goal, solution)
        .map_err(|error| ActionStartError::Rejected(format!("右臂 IK 目标复核失败：{error}")))?;
    ScriptedBackend::start_ik_joints(req, ctx, &targets)
}

// ============================================================================
// IkInstance
// ============================================================================

/// `IkBackend::start` 返的 per-action 实例。
///
/// **owned 状态**：`target_joints` 在 `start()` 期 owned，tick 期只读复
/// 用；coordinator drop instance 时一起回收。`emitted` / `cancelled` 是
/// 状态 flag（无堆分配）。
///
/// **single-shot 语义**（Q-IK17 (a) 拍板）：
///
/// - first tick → `Continue { progress: 1.0, command: Some(body) }`
///   publish 一次目标关节位置；`emitted = true`。
/// - second tick → `Completed(ActionOutcome::Success)`，coordinator 回
///   IDLE 恢复 hold loop。
///
/// 不**预热**到目标位置 / 不做平滑过渡 —— 那是 scripted backend 的职责。
/// Ik backend 仅"算一次目标，发一次 cmd，立刻完成"。
struct IkInstance {
    /// `start()` 期 solver.solve() 算出的目标关节位置；tick 期只读复用。
    target_joints: Vec<f64>,
    /// first tick publish 一次 cmd 后置 `true`；second tick 返
    /// `Completed(Success)`。
    emitted: bool,
    /// `cancel()` 设置；下次 [`ActionInstance::tick`] 自查 + 收敛返
    /// `Completed(PartialSuccess { warning: ... })`（coordinator
    /// pending_cancel 路径转 `ActionEvent::Cancelled` 写 stream）。
    cancelled: bool,
}

impl ActionInstance for IkInstance {
    fn tick(&mut self, ctx: &mut ActionContext<'_>) -> Result<ActionTickOutput, ActionFailure> {
        // (1) cancel 短路（沿 scripted Q4(a)）：backend lib 返
        // PartialSuccess；coordinator pending_cancel 转 Cancelled。warning
        // 文案带 emitted 状态便于 audit 区分"cancel 时是否已 publish 过 cmd"。
        if self.cancelled {
            let warning = if self.emitted {
                "cancelled_after_emit"
            } else {
                "cancelled_before_emit"
            };
            return Ok(ActionTickOutput::Completed(ActionOutcome::PartialSuccess {
                warning: warning.to_string(),
            }));
        }

        // (2) second tick 收敛：emitted == true → Completed(Success)。
        if self.emitted {
            return Ok(ActionTickOutput::Completed(ActionOutcome::Success));
        }

        // (3) first tick：build hold cmd 用 target_joints 作 bias（同
        // hold_pose / scripted 风格；coordinator.tick_action 透传
        // ctx.command_seq 作 source_task_id 保 S0 单调契约）。
        let now_ns = ctx.clock.now().0;
        let body = build_hold_command(
            ctx.config,
            ctx.layout_id,
            &self.target_joints,
            /* source_task_id */ ctx.command_seq,
            /* now_ns */ now_ns,
        );
        self.emitted = true;

        Ok(ActionTickOutput::Continue {
            progress: 1.0,
            command: Some(JointCommandBody::new(body)),
        })
    }

    fn cancel(&mut self, _ctx: &mut ActionContext<'_>) -> Result<(), ActionCancelError> {
        // cooperative：set flag，下次 tick 自查收敛走 cancel_outcome。
        self.cancelled = true;
        Ok(())
    }
}

// ============================================================================
// IkError → ActionStartError 映射
// ============================================================================

/// 把 [`IkError`] 5 variant 全部映射到 [`ActionStartError::Rejected`]
/// （Plan §P.2.3.d + Q-IK16 (a) ACK：`XLeRobot` solver 沿 dimos clamp
/// 路径走 `Ok(clamped)`，正常不返 `OutOfReach`；本映射兜底任何 solver
/// 实现的 reject 路径，统一走 `FailedPrecondition` RPC status）。
///
/// `DeadlineExceeded` 在 Q-IK 拍板路径下永不构造（backend 收 `None` 传
/// solver），但保留 exhaustive match 避免漏 variant。
fn map_ik_error_to_start_error(err: IkError) -> ActionStartError {
    match err {
        IkError::InvalidInput { reason } => {
            ActionStartError::Rejected(format!("IK solver InvalidInput: {reason}"))
        }
        IkError::NoSolution => {
            ActionStartError::Rejected("IK solver returned NoSolution".to_string())
        }
        IkError::OutOfReach { distance_m } => ActionStartError::Rejected(format!(
            "IK solver returned OutOfReach (distance {distance_m:.4} m); \
             XLeRobot solver should normally clamp to workspace (Q-IK16 (a))"
        )),
        IkError::SingularityTooClose { condition_number } => ActionStartError::Rejected(format!(
            "IK solver SingularityTooClose (condition_number {condition_number:.3e})"
        )),
        IkError::DeadlineExceeded => ActionStartError::Rejected(
            "IK solver returned DeadlineExceeded (should never happen; \
             backend passes deadline_ns = None per Plan §P.3.2 #12 (a))"
                .to_string(),
        ),
    }
}

// ============================================================================
// inventory 注册
// ============================================================================

/// inventory 静态实例。
///
/// `BackendRegistry::from_inventory` 启动期一次性扫描，把本实例的
/// `&'static dyn ActionBackend` 收进 `by_name` map（key = `"ik"`）。
static IK_BACKEND: IkBackend = IkBackend;

inventory::submit! {
    &IK_BACKEND as &'static dyn ActionBackend
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use crate::action::{ActionId, ActionKind, ActionRequest, ActionSource};
    use crate::coordinator::{
        CoordinatorConfig, HardwareSection, parse_hardware_section_from_toml,
    };
    use flatbuffers::{FlatBufferBuilder, root};
    use robot_core::time::{MockClock, MonoRawNs};
    use robot_kinematics::ArmIkSolver;
    use robot_kinematics_xlerobot::{Side, XLeRobotArmIkSolver};
    use robot_msg::msg::{
        ControlMode, IkPayload as FbIkPayloadTable, IkPayloadArgs, JointCommand as FbJointCommand,
    };

    const TEST_LAYOUT_ID: u64 = 0xCAFE_BABE_DEAD_BEEF;

    // ----- helpers -----

    /// 5-DOF XLeRobot hardware section（matches XLeRobotArmIkSolver.info()）。
    fn xlerobot_hardware() -> HardwareSection {
        parse_hardware_section_from_toml(
            include_str!("../../../../configs/ik-backend-smoke.toml"),
            "ik_arm",
        )
        .unwrap()
    }

    /// 构造一个最小合法 `IkPayload` fbs body bytes。
    fn build_ik_payload_bytes(
        target_xyz: &[f64],
        target_quat_xyzw: &[f64],
        seed_joints_rad: &[f64],
        frame: &str,
    ) -> Vec<u8> {
        let mut fbb = FlatBufferBuilder::with_capacity(256);
        let xyz_v = fbb.create_vector::<f64>(target_xyz);
        let quat_v = fbb.create_vector::<f64>(target_quat_xyzw);
        let seed_v = fbb.create_vector::<f64>(seed_joints_rad);
        let frame_v = fbb.create_string(frame);
        let payload = FbIkPayloadTable::create(
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

    /// 用给定 payload + clock_now_ns + solver 构造 backend instance。
    fn fresh_instance_with_payload(
        payload: Vec<u8>,
        anchor_bias: Vec<f64>,
        clock_now_ns: u64,
        solver: &dyn ArmIkSolver,
    ) -> (
        Box<dyn ActionInstance>,
        CoordinatorConfig,
        HardwareSection,
        Vec<f64>,
        MockClock,
    ) {
        let backend = IkBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = xlerobot_hardware();
        let clock = MockClock::new(MonoRawNs(clock_now_ns));
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Ik,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: SCHEMA_IK_PAYLOAD,
            payload,
            idempotency_key: "test-ik-key".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 0,
            scripted_start_pose: None,
            solver: Some(solver),
        };
        let instance = backend.start(&req, &ctx).expect("start ok");
        (instance, cfg, hardware, anchor_bias, clock)
    }

    // ----- 1. name() -----

    #[test]
    fn name_is_ik() {
        let b = IkBackend;
        assert_eq!(b.name(), "ik");
    }

    // ----- 2. supported_kinds() -----

    #[test]
    fn supported_kinds_is_ik_only() {
        let b = IkBackend;
        let kinds = b.supported_kinds();
        assert_eq!(kinds.len(), 1);
        assert_eq!(kinds[0], ActionKind::Ik);
    }

    // ----- 3. supported_schemas() -----

    #[test]
    fn supported_schemas_is_ik_payload_only() {
        let b = IkBackend;
        let schemas = b.supported_schemas();
        assert_eq!(schemas, &[SCHEMA_IK_PAYLOAD]);
    }

    // ----- 4. start_rejects_payload_schema_id_mismatch -----

    #[test]
    fn start_rejects_payload_schema_id_mismatch() {
        let backend = IkBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = xlerobot_hardware();
        let anchor_bias = vec![0.0_f64; 5];
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let solver = XLeRobotArmIkSolver::new(Side::Right);
        let payload = build_ik_payload_bytes(&[0.2, 0.0, 0.1], &[0.0, 0.0, 0.0, 1.0], &[], "");
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Ik,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: 0xDEAD_BEEF, // 错的 schema_id
            payload,
            idempotency_key: "test-bad-schema".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 0,
            scripted_start_pose: None,
            solver: Some(&solver),
        };
        let Err(err) = backend.start(&req, &ctx) else {
            panic!("expected Err");
        };
        assert!(
            matches!(err, ActionStartError::Rejected(ref s) if s.contains("payload_schema_id")),
            "got {err:?}"
        );
    }

    // ----- 5. start_rejects_when_solver_missing -----

    #[test]
    fn start_rejects_when_solver_missing() {
        let backend = IkBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = xlerobot_hardware();
        let anchor_bias = vec![0.0_f64; 5];
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let payload = build_ik_payload_bytes(&[0.2, 0.0, 0.1], &[0.0, 0.0, 0.0, 1.0], &[], "");
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Ik,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: SCHEMA_IK_PAYLOAD,
            payload,
            idempotency_key: "test-solver-missing".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            scripted_start_pose: None,
            command_seq: 0,
            solver: None, // <-- 漏注入兜底
        };
        let Err(err) = backend.start(&req, &ctx) else {
            panic!("expected Err");
        };
        assert!(
            matches!(err, ActionStartError::Rejected(ref s) if s.contains("solver not injected")),
            "got {err:?}"
        );
    }

    // ----- 6. start_rejects_deadline_already_past -----

    #[test]
    fn start_rejects_deadline_already_past() {
        let backend = IkBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = xlerobot_hardware();
        let anchor_bias = vec![0.0_f64; 5];
        let clock = MockClock::new(MonoRawNs(2_000_000_000));
        let solver = XLeRobotArmIkSolver::new(Side::Right);
        let payload = build_ik_payload_bytes(&[0.2, 0.0, 0.1], &[0.0, 0.0, 0.0, 1.0], &[], "");
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Ik,
            deadline_ns: 1_000_000_000, // 已过期
            priority: 10,
            payload_schema_id: SCHEMA_IK_PAYLOAD,
            payload,
            idempotency_key: "test-past-deadline".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 0,
            scripted_start_pose: None,
            solver: Some(&solver),
        };
        let Err(err) = backend.start(&req, &ctx) else {
            panic!("expected Err");
        };
        assert!(
            matches!(err, ActionStartError::Rejected(ref s) if s.contains("deadline_ns")),
            "got {err:?}"
        );
    }

    // ----- 7. start_rejects_payload_decode_fail（两层错误映射的
    //         PayloadDecode 一路）-----

    #[test]
    fn start_rejects_payload_decode_fail() {
        let backend = IkBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = xlerobot_hardware();
        let anchor_bias = vec![0.0_f64; 5];
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let solver = XLeRobotArmIkSolver::new(Side::Right);
        // 故意构一段非法 fbs bytes（FlatBuffers verifier 要求 root
        // offset 在 buffer 内 + table 长度合理；这里给一段 0xFF
        // 让 root offset 超界，verify 必 fail）。
        let payload = vec![0xFFu8; 8];
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Ik,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: SCHEMA_IK_PAYLOAD,
            payload,
            idempotency_key: "test-decode-fail".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 0,
            scripted_start_pose: None,
            solver: Some(&solver),
        };
        let Err(err) = backend.start(&req, &ctx) else {
            panic!("expected Err");
        };
        // 关键：必须走 PayloadDecode（**不**是 Rejected），跟 semantic
        // 错的两层映射严格区分。
        assert!(
            matches!(err, ActionStartError::PayloadDecode(ref s)
                if s.contains("IkPayload fbs verify failed")),
            "got {err:?}"
        );
    }

    // ----- 8. start_rejects_target_xyz_wrong_length -----

    #[test]
    fn start_rejects_target_xyz_wrong_length() {
        let backend = IkBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = xlerobot_hardware();
        let anchor_bias = vec![0.0_f64; 5];
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let solver = XLeRobotArmIkSolver::new(Side::Right);
        let payload = build_ik_payload_bytes(&[0.2, 0.0], &[0.0, 0.0, 0.0, 1.0], &[], "");
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Ik,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: SCHEMA_IK_PAYLOAD,
            payload,
            idempotency_key: "test-xyz-len".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 0,
            scripted_start_pose: None,
            solver: Some(&solver),
        };
        let Err(err) = backend.start(&req, &ctx) else {
            panic!("expected Err");
        };
        assert!(
            matches!(err, ActionStartError::Rejected(ref s) if s.contains("target_xyz length")),
            "got {err:?}"
        );
    }

    // ----- 9. start_rejects_seed_length_mismatch -----

    #[test]
    fn start_rejects_seed_length_mismatch() {
        let backend = IkBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = xlerobot_hardware();
        let anchor_bias = vec![0.0_f64; 5];
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let solver = XLeRobotArmIkSolver::new(Side::Right);
        // seed.len() = 3，跟 info().joint_count = 5 不匹配
        let payload = build_ik_payload_bytes(
            &[0.2, 0.0, 0.1],
            &[0.0, 0.0, 0.0, 1.0],
            &[0.0, 0.0, 0.0],
            "",
        );
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Ik,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: SCHEMA_IK_PAYLOAD,
            payload,
            idempotency_key: "test-seed-len".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 0,
            scripted_start_pose: None,
            solver: Some(&solver),
        };
        let Err(err) = backend.start(&req, &ctx) else {
            panic!("expected Err");
        };
        assert!(
            matches!(err, ActionStartError::Rejected(ref s)
                if s.contains("seed_joints_rad length") && s.contains("joint_count")),
            "got {err:?}"
        );
    }

    // ----- 10. start_rejects_hardware_mismatch -----

    #[test]
    fn start_rejects_hardware_mismatch() {
        let backend = IkBackend;
        let cfg = CoordinatorConfig::default();
        // 故意给 3-joint hardware（跟 XLeRobotArmIkSolver 5-joint 不匹配）
        let hardware = HardwareSection {
            id: "wrong_arm".into(),
            joints: vec!["a".into(), "b".into(), "c".into()],
            motor_ids: vec![1, 2, 3],
            feetech_bus2_mixed_raw: false,
            calibration_range_tolerances_raw: None,
        };
        let anchor_bias = vec![0.0_f64; 3];
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let solver = XLeRobotArmIkSolver::new(Side::Right);
        let payload = build_ik_payload_bytes(&[0.2, 0.0, 0.1], &[0.0, 0.0, 0.0, 1.0], &[], "");
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Ik,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: SCHEMA_IK_PAYLOAD,
            payload,
            idempotency_key: "test-hw-mismatch".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 0,
            scripted_start_pose: None,
            solver: Some(&solver),
        };
        let Err(err) = backend.start(&req, &ctx) else {
            panic!("expected Err");
        };
        assert!(
            matches!(err, ActionStartError::HardwareMismatch(ref s)
                if s.contains("joint_count") && s.contains("hardware.joints.len()")),
            "got {err:?}"
        );
    }

    // ----- 11. start_rejects_solver_invalid_input_yaw_nonzero（Q-IK6 (a)
    //          路径：solver 拒 yaw != 0 → backend 映射到 Rejected）-----

    #[test]
    fn start_rejects_solver_invalid_input_yaw_nonzero() {
        let backend = IkBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = xlerobot_hardware();
        let anchor_bias = vec![0.0_f64; 5];
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let solver = XLeRobotArmIkSolver::new(Side::Right);
        // yaw ≠ 0 的 quaternion（绕 Z 轴 90 度）
        let half = std::f64::consts::FRAC_1_SQRT_2;
        let payload = build_ik_payload_bytes(&[0.2, 0.0, 0.1], &[0.0, 0.0, half, half], &[], "");
        let req = ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::Ik,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: SCHEMA_IK_PAYLOAD,
            payload,
            idempotency_key: "test-yaw-nonzero".into(),
            source: ActionSource::Internal,
        };
        let ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 0,
            scripted_start_pose: None,
            solver: Some(&solver),
        };
        let Err(err) = backend.start(&req, &ctx) else {
            panic!("expected Err");
        };
        // solver 返 IkError::InvalidInput；backend map 到 Rejected。
        assert!(
            matches!(err, ActionStartError::Rejected(ref s)
                if s.contains("IK solver InvalidInput") && s.contains("yaw")),
            "got {err:?}"
        );
    }

    // ----- 12. tick_emits_command_with_source_task_id_eq_ctx_command_seq
    //          （**bit-equivalent test**；B3 §3.7 单调契约）-----

    /// first tick body 必 bit-equivalent to
    /// `build_hold_command(..., source_task_id == ctx.command_seq, ...)`。
    const TEST_CMD_SEQ: u64 = 0xCAFE_BABE;

    #[test]
    fn tick_emits_command_with_source_task_id_eq_ctx_command_seq() {
        let payload = build_ik_payload_bytes(&[0.20, 0.0, 0.10], &[0.0, 0.0, 0.0, 1.0], &[], "");
        let anchor_bias = vec![0.0_f64; 5];
        let now_ns: u64 = 1_000_000_000;
        let solver = XLeRobotArmIkSolver::new(Side::Right);
        let (mut instance, cfg, hardware, _anchor, clock) =
            fresh_instance_with_payload(payload, anchor_bias.clone(), now_ns, &solver);

        let mut ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: TEST_CMD_SEQ,
            scripted_start_pose: None,
            solver: Some(&solver),
        };
        let out = instance.tick(&mut ctx).expect("tick ok");
        let body = match out {
            ActionTickOutput::Continue {
                progress: 1.0,
                command: Some(body),
            } => body,
            other => panic!("expected Continue progress=1.0 Some(body), got {other:?}"),
        };

        let cmd = root::<FbJointCommand<'_>>(body.as_bytes()).expect("fbs verify");
        let seq = cmd.source_task_id();
        assert_eq!(
            seq, TEST_CMD_SEQ,
            "source_task_id 必须 == ctx.command_seq (B3 §3.7 单调契约)"
        );

        // ground truth：bit-equivalent to build_hold_command(target = solved_joints)
        let target: Vec<f64> = cmd.target().expect("target").iter().collect();
        let ground_truth = build_hold_command(
            &cfg,
            TEST_LAYOUT_ID,
            &target,
            /* source_task_id */ seq,
            /* now_ns */ now_ns,
        );
        assert_eq!(
            body.as_bytes(),
            ground_truth.as_slice(),
            "body bytes 必须 bit-equivalent to build_hold_command(...)"
        );
        assert_eq!(cmd.mode(), ControlMode::Position);
        assert_eq!(cmd.layout_id(), TEST_LAYOUT_ID);

        // target == solver.solve(...).joints_rad（5-DOF 顺序）
        let expected = solver
            .solve(
                &Pose {
                    translation: [0.20, 0.0, 0.10],
                    orientation: [0.0, 0.0, 0.0, 1.0],
                },
                None,
                None,
            )
            .expect("solver ok");
        assert_eq!(target.len(), 5);
        for (i, (got, want)) in target.iter().zip(expected.joints_rad.iter()).enumerate() {
            assert!(
                (got - want).abs() < 1e-12,
                "target[{i}] = {got}, expected {want}"
            );
        }
    }

    /// 数学上不可达、当前关节不在闭式解分支、坐标系或槽位错误均应在首条命令前拒绝。
    #[cfg(feature = "backend-scripted")]
    #[test]
    fn mixed_ik_rejects_clamped_goals_and_wrong_start_branch() {
        use crate::coordinator::parse_single_joint_config_from_toml;
        use crate::state_cache::JointFeedbackSnapshot;

        let bus = parse_hardware_section_from_toml(
            include_str!("../../../../configs/xlerobot-bus2.toml"),
            "right_arm_full_with_base",
        )
        .unwrap();
        let limits = parse_single_joint_config_from_toml(
            include_str!("../../../../configs/xlerobot-bus2-motion.toml"),
            &bus,
        )
        .unwrap()
        .unwrap();
        let model = XLeRobotArmIkSolver::new(Side::Right);
        let initial = [-0.2, 0.25, 0.35, -0.1, 0.3];
        let original = model.forward_pose(&initial).unwrap();
        let mut goal = original;
        goal.translation[0] += 0.010;
        let cfg = CoordinatorConfig::default();
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let start = |hardware: &HardwareSection, goal: Pose, joints: &[f64], frame: &str| {
            let mut positions = joints.to_vec();
            positions.extend([0.1, 0.0, 0.0, 0.0]);
            let feedback = JointFeedbackSnapshot {
                stamp_ns: 1_000_000_000,
                motion_ready: true,
                positions,
            };
            let ctx = ActionContext {
                single_joint_config: Some(&limits),
                feedback: Some(&feedback),
                clock: &clock,
                layout_id: TEST_LAYOUT_ID,
                hardware,
                config: &cfg,
                anchor_bias: &[],
                command_seq: 0,
                scripted_start_pose: Some(&feedback.positions),
                solver: Some(&model),
            };
            let req = ActionRequest {
                action_id: ActionId::new_v7(),
                kind: ActionKind::Ik,
                deadline_ns: u64::MAX,
                priority: 10,
                payload_schema_id: SCHEMA_IK_PAYLOAD,
                payload: build_ik_payload_bytes(&goal.translation, &goal.orientation, &[], frame),
                idempotency_key: "mixed-ik".into(),
                source: ActionSource::Internal,
            };
            IkBackend.start(&req, &ctx).map(drop)
        };
        assert!(start(&bus, goal, &initial, "").is_ok());
        let mut unreachable = goal;
        unreachable.translation[0] = 1.0;
        assert!(matches!(
            start(&bus, unreachable, &initial, ""),
            Err(ActionStartError::Rejected(_))
        ));
        let mut roll = goal;
        roll.orientation = [1.0, 0.0, 0.0, 0.0];
        assert!(matches!(
            start(&bus, roll, &initial, ""),
            Err(ActionStartError::Rejected(_))
        ));
        let mut other_branch = initial;
        other_branch[2] = -1.5;
        assert!(
            matches!(start(&bus, goal, &other_branch, ""), Err(ActionStartError::Rejected(message)) if message.contains("分支"))
        );
        assert!(
            matches!(start(&bus, goal, &initial, "world"), Err(ActionStartError::Rejected(message)) if message.contains("坐标系"))
        );
        let mut swapped = bus;
        swapped.joints.swap(0, 6);
        assert!(matches!(
            start(&swapped, goal, &initial, ""),
            Err(ActionStartError::HardwareMismatch(_))
        ));
    }

    /// 只在内存中执行右臂五轴命令，按电机整数刻度回读后再求腕点位置。
    #[test]
    #[allow(
        clippy::cast_possible_truncation,
        reason = "复用 S0 从 f32 关节目标到编码器整数的转换"
    )]
    fn offline_right_arm_forward_10mm() {
        use robot_adapter_feetech::{rad_to_raw, raw_to_rad};
        let solver = XLeRobotArmIkSolver::new(Side::Right);
        let start_raw = [1917, 2210, 2275, 1982, 2243];
        let start_joints = start_raw.map(|raw| f64::from(raw_to_rad(raw)));
        let start = solver.forward_pose(&start_joints).unwrap();
        let mut goal = start;
        goal.translation[0] += 0.010;
        let payload =
            build_ik_payload_bytes(&goal.translation, &goal.orientation, &start_joints, "");
        let (mut action, cfg, arm, anchor, clock) =
            fresh_instance_with_payload(payload, start_joints.to_vec(), 1_000_000_000, &solver);
        let mut ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &arm,
            config: &cfg,
            anchor_bias: &anchor,
            command_seq: TEST_CMD_SEQ,
            scripted_start_pose: None,
            solver: Some(&solver),
        };
        let ActionTickOutput::Continue {
            command: Some(body),
            ..
        } = action.tick(&mut ctx).unwrap()
        else {
            panic!("首拍应产生位置命令")
        };
        let cmd = root::<FbJointCommand<'_>>(body.as_bytes()).unwrap();
        assert_eq!(cmd.mode(), ControlMode::Position);
        assert_eq!(cmd.layout_id(), TEST_LAYOUT_ID);
        let target = cmd.target().unwrap();
        assert_eq!(target.len(), start_raw.len());
        let targets_raw: [i32; 5] = std::array::from_fn(|i| {
            let angle = target.get(i);
            let limit = &solver.info().joint_limits[i];
            assert!((limit.min_rad..=limit.max_rad).contains(&angle));
            rad_to_raw(angle as f32)
        });
        assert_eq!(arm.joints, solver.info().joint_names);

        let reached_joints = targets_raw.map(|raw| f64::from(raw_to_rad(raw)));
        let reached = solver.forward_pose(&reached_joints).unwrap();
        let delta_mm: [f64; 3] =
            std::array::from_fn(|i| 1000.0 * (reached.translation[i] - start.translation[i]));
        let [dx, dy, dz] = delta_mm;
        let error_mm = (dx - 10.0).hypot(dy).hypot(dz);
        assert!(
            error_mm <= 2.0,
            "腕点位移 {delta_mm:?} mm，目标误差 {error_mm} mm"
        );
        let dot: f64 = goal
            .orientation
            .iter()
            .zip(reached.orientation)
            .map(|(a, b)| a * b)
            .sum();
        assert!(dot.is_finite());
        let orientation_error_deg = (2.0 * dot.abs().min(1.0).acos()).to_degrees();
        assert!(
            orientation_error_deg < 1.0,
            "模型姿态偏差 {orientation_error_deg}°"
        );
        assert!(matches!(
            action.tick(&mut ctx).unwrap(),
            ActionTickOutput::Completed(ActionOutcome::Success)
        ));
        let degrees = target.iter().map(f64::to_degrees).collect::<Vec<_>>();
        eprintln!(
            "右臂模型腕点位移 {delta_mm:.3?} mm，位置误差 {error_mm:.3} mm，姿态误差 {orientation_error_deg:.3}°，五轴目标 {degrees:.2?}°，编码器目标 {targets_raw:?} raw"
        );
    }

    // ----- 13. tick_second_call_returns_completed_success（single-shot
    //          收敛，Q-IK17 (a)）-----

    #[test]
    fn tick_second_call_returns_completed_success() {
        let payload = build_ik_payload_bytes(&[0.20, 0.0, 0.10], &[0.0, 0.0, 0.0, 1.0], &[], "");
        let anchor_bias = vec![0.0_f64; 5];
        let now_ns: u64 = 1_000_000_000;
        let solver = XLeRobotArmIkSolver::new(Side::Right);
        let (mut instance, cfg, hardware, _anchor, clock) =
            fresh_instance_with_payload(payload, anchor_bias.clone(), now_ns, &solver);

        let mut ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 1,
            scripted_start_pose: None,
            solver: Some(&solver),
        };
        // first tick: Continue
        let out = instance.tick(&mut ctx).expect("tick ok");
        assert!(matches!(
            out,
            ActionTickOutput::Continue {
                progress: 1.0,
                command: Some(_)
            }
        ));

        // second tick: Completed(Success)
        let mut ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 2,
            scripted_start_pose: None,
            solver: Some(&solver),
        };
        let out = instance.tick(&mut ctx).expect("tick ok");
        match out {
            ActionTickOutput::Completed(ActionOutcome::Success) => {}
            other => panic!("expected Completed(Success), got {other:?}"),
        }
    }

    // ----- 14. tick_after_cancel_before_emit_returns_partial_success -----

    #[test]
    fn tick_after_cancel_before_emit_returns_partial_success() {
        let payload = build_ik_payload_bytes(&[0.20, 0.0, 0.10], &[0.0, 0.0, 0.0, 1.0], &[], "");
        let anchor_bias = vec![0.0_f64; 5];
        let now_ns: u64 = 1_000_000_000;
        let solver = XLeRobotArmIkSolver::new(Side::Right);
        let (mut instance, cfg, hardware, _anchor, clock) =
            fresh_instance_with_payload(payload, anchor_bias.clone(), now_ns, &solver);

        let mut ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 1,
            scripted_start_pose: None,
            solver: Some(&solver),
        };
        instance.cancel(&mut ctx).expect("cancel ok");
        let out = instance.tick(&mut ctx).expect("tick ok");
        match out {
            ActionTickOutput::Completed(ActionOutcome::PartialSuccess { warning }) => {
                assert_eq!(warning, "cancelled_before_emit");
            }
            other => panic!("expected Completed(PartialSuccess), got {other:?}"),
        }
    }

    // ----- 15. tick_after_cancel_after_emit_returns_partial_success_after -----

    #[test]
    fn tick_after_cancel_after_emit_returns_partial_success_after() {
        let payload = build_ik_payload_bytes(&[0.20, 0.0, 0.10], &[0.0, 0.0, 0.0, 1.0], &[], "");
        let anchor_bias = vec![0.0_f64; 5];
        let now_ns: u64 = 1_000_000_000;
        let solver = XLeRobotArmIkSolver::new(Side::Right);
        let (mut instance, cfg, hardware, _anchor, clock) =
            fresh_instance_with_payload(payload, anchor_bias.clone(), now_ns, &solver);

        // first tick: emit
        let mut ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 1,
            scripted_start_pose: None,
            solver: Some(&solver),
        };
        let _ = instance.tick(&mut ctx).expect("tick ok");

        // cancel + next tick: cancelled_after_emit
        instance.cancel(&mut ctx).expect("cancel ok");
        let out = instance.tick(&mut ctx).expect("tick ok");
        match out {
            ActionTickOutput::Completed(ActionOutcome::PartialSuccess { warning }) => {
                assert_eq!(warning, "cancelled_after_emit");
            }
            other => panic!("expected Completed(PartialSuccess), got {other:?}"),
        }
    }

    // ----- 16. inventory_default_instance_present -----

    #[test]
    fn inventory_default_instance_present() {
        // 锁住 IkBackend 静态实例真注册到 inventory；改 inventory 注册
        // 口要同步改这条测试。沿 hold_pose / scripted 同款 + Q-IK11 (a)
        // default-on 拍板。
        let names: Vec<&'static str> = inventory::iter::<&'static dyn ActionBackend>
            .into_iter()
            .map(|b| b.name())
            .collect();
        assert!(
            names.contains(&"ik"),
            "IkBackend 必须通过 inventory::submit! 真注册；inventory 名字集 = {names:?}"
        );
        // 同时验 hold_pose / scripted 也在（Q15 拍板：names 集断言含多个）。
        assert!(
            names.contains(&"hold_pose"),
            "hold_pose 仍应在 inventory；names = {names:?}"
        );
        assert!(
            names.contains(&"scripted"),
            "scripted 仍应在 inventory；names = {names:?}"
        );
    }

    // ----- 17. ik_error_mapping_covers_all_variants -----

    #[test]
    fn ik_error_mapping_covers_all_variants() {
        // 锁住 map_ik_error_to_start_error 5 variant exhaustive 映射。
        // 任何 IkError 加新 variant 时本函数和测试同步 fail。
        // IkError 不 impl Clone（trait crate 决策），所以 by-value 一次一构。
        let check = |err: IkError, marker: &str| {
            let mapped = map_ik_error_to_start_error(err);
            let ActionStartError::Rejected(s) = mapped else {
                panic!("variant {marker} 未走 Rejected 路径");
            };
            assert!(
                s.contains(marker),
                "variant {marker} 映射后 message 应含 marker；got: {s}"
            );
        };
        check(IkError::InvalidInput { reason: "x".into() }, "InvalidInput");
        check(IkError::NoSolution, "NoSolution");
        check(IkError::OutOfReach { distance_m: 0.5 }, "OutOfReach");
        check(
            IkError::SingularityTooClose {
                condition_number: 1e6,
            },
            "SingularityTooClose",
        );
        check(IkError::DeadlineExceeded, "DeadlineExceeded");
    }
}
