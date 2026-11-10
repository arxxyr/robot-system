//! v0.4 候选 2 round 2 `IkBackend`：笛卡尔 pose IK 解算 + 关节位置一次性
//! 输出（Q-IK17 (a) single-shot）。
//!
//! ## 行为
//!
//! - `name()` = `"ik"`（profile `[s1.profile.backends]` 引用名）。
//! - `supported_kinds()` = `&[ActionKind::Ik]`（v0.4 候选 2 round 2 (2/4)
//!   atomic land 后的新 variant；详 `crates/robot-s1/src/action.rs`）。
//! - `supported_schemas()` = `&[SCHEMA_IK_PAYLOAD]`（`0x0000_040C`；
//!   **不**含 `NO_PAYLOAD_SCHEMA_ID`，跟 hold_pose / mock_dwell 显著区分）。
//! - `start(req, ctx)`：六项 semantic check + solver.solve() →
//!   [`IkInstance`] 持 target_joints；first tick publish 一次 cmd，second
//!   tick `Completed(Success)` 收敛（Q-IK17 (a) single-shot）。
//! - `tick(ctx)`：首次 tick 复用 [`crate::coordinator::build_hold_command`]
//!   构 body bytes（source_task_id == `ctx.command_seq`，B3 §3.7 单调契
//!   约）+ `progress = 1.0` 返 `Continue { command: Some(body) }`；标
//!   `emitted = true`；下次 tick 返 `Completed(ActionOutcome::Success)`。
//! - `cancel()`：cooperative，set flag；下次 tick 返
//!   `Completed(ActionOutcome::PartialSuccess { warning: "cancelled_before_emit" })`
//!   或 `"cancelled_after_emit"`（视 `emitted` 状态；沿 scripted Q4(a)
//!   coordinator pending_cancel 路径转 `ActionEvent::Cancelled`）。
//!
//! ## semantic check（`start()` 期；Plan §P.3.2 Q-IK10b (i) + 两层错误映射）
//!
//! `IkBackend::start()` 按下面顺序短路返第一个错：
//!
//! 1. `payload_schema_id == SCHEMA_IK_PAYLOAD`（dispatch 守门已保，本步
//!    defensive 再断言；不匹配 → [`ActionStartError::Rejected`]）。
//! 2. `ctx.solver.is_some()`（Q-IK10b (i) startup 校验已拦绝大多数路径；
//!    runtime 防御性 reject 兜底）。
//! 3. `req.deadline_ns > ctx.clock.now()`（Plan §P.3.2 #12 (a) deadline
//!    backend pre-guard；过期 → [`ActionStartError::Rejected`]）。
//! 4. `flatbuffers::root::<IkPayload>(payload)` decode 成功 → 失败 →
//!    [`ActionStartError::PayloadDecode`]。
//! 5. `target_xyz.len() == 3` + `target_quat_xyzw.len() == 4` +
//!    `seed_joints_rad.len() == 0 || == info().joint_count` → 任一不满
//!    足 [`ActionStartError::Rejected`]。
//! 6. `solver.info().joint_count == ctx.hardware.joints.len()` → 不匹配
//!    [`ActionStartError::HardwareMismatch`]（防 binary 注入了跟当前
//!    hardware 不匹配的 solver）。
//! 7. `solver.solve(&pose, seed.as_deref(), None)` → 任一
//!    [`IkError`] 映射到 [`ActionStartError::Rejected`]（沿 Q-IK16 (a)
//!    `XLeRobot` clamp 路径走 `Ok(clamped)`，正常不返 `OutOfReach`；其
//!    它 `InvalidInput` / `NoSolution` / `SingularityTooClose` /
//!    `DeadlineExceeded` 兜底）。
//! 8. solver 返 Ok 后再查 deadline post-guard（Plan §P.3.2 #12 (a) 拍
//!    板 backend 收 None 让 solver 永远不报 `IkError::DeadlineExceeded`，
//!    backend pre/post guard 是 deadline 唯一权威）。
//!
//! ## 两层错误映射（Plan §P.2.3.d）
//!
//! | 来源 | `ActionStartError` | 跨进程 `ActionRpcStatus` |
//! |------|--------------------|--------------------------|
//! | dispatch 守门 / `payload_schema_id` 错 / solver missing / decode 后字段 semantic 错 / solver `IkError` 全部 | `Rejected(String)` | `FailedPrecondition` |
//! | `flatbuffers::root::<IkPayload>` verify fail | `PayloadDecode(String)` | `FailedPrecondition` |
//! | solver `info().joint_count != hardware.joints.len()` | `HardwareMismatch(String)` | `FailedPrecondition` |
//!
//! coordinator `submit` 把 `ActionStartError` 透成 `SubmitError::BackendStartFailed`，
//! gateway `wire.rs:340` 把所有 `FailedPrecondition`-类映射到 RPC
//! `ActionRpcStatus::FailedPrecondition`。
//!
//! ## source_task_id 单调契约（B3 §3.7）
//!
//! Ik body 的 `source_task_id` **必须**等于 `ctx.command_seq`，由
//! `run_action_tick_loop` 透传当前 publish frame 的 `TickState.cmd_seq`
//! 进 `ActionContext.command_seq`。**不**允许 backend 私有 counter 从
//! 0 起增 —— 那会让 S0 `ipc_thread::join_command_to_buf` 把
//! `fb.source_task_id()` 写到 `JointCommandBuf.seq` 时看到 seq 回退，
//! 破坏 `adapter.rs` 单调递增契约。沿 hold_pose B3 round 2 阻断 1 修法 +
//! scripted v0.4 候选 1 同款。
//!
//! ## inventory 注册
//!
//! 整文件 `#[cfg(feature = "backend-ik")]` gate（Q-IK11 (a) 拍板默认 on）。
//! Feature off 时整文件不 link → `BackendRegistry::from_inventory` 收集
//! 不到 ik → `[s1.profile.backends = ["ik"]]` 在 `validate_profile_section`
//! 报 `UnknownBackend`（Q-IK10 (a-i)），符合"配置与编译 profile 不匹配
//! 启动期 fail-fast"硬约束。

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

/// v0.4 候选 2 round 2 第三个真 action backend：笛卡尔 pose IK 解算 +
/// 关节位置一次性输出。
///
/// **零字段** struct：所有状态在 [`IkInstance`] 上；backend 本身仅声明
/// 能力 + 解码 payload + 调 solver + 创建 instance。
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
        // 在 s1_daemon 启动期早已拦下 profile = ["ik"] + solver = None
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

        // (6) hardware joint 数 vs solver info().joint_count 一致（防
        // binary 注入了跟当前 hardware 不匹配的 solver；典型场景：
        // robot id 变化但 solver 类型没切。HardwareMismatch 路径专给本
        // 类配置错用）。
        let info_joint_count = solver.info().joint_count;
        if ctx.hardware.joints.len() != info_joint_count {
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

        // (9) build instance：target_joints owned + 状态 flag。
        Ok(Box::new(IkInstance {
            target_joints: solution.joints_rad,
            emitted: false,
            cancelled: false,
        }))
    }
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
    use crate::coordinator::{CoordinatorConfig, HardwareSection};
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
        HardwareSection {
            id: "test_arm".into(),
            joints: vec![
                "shoulder_pan".into(),
                "shoulder_lift".into(),
                "elbow_flex".into(),
                "wrist_flex".into(),
                "wrist_roll".into(),
            ],
            motor_ids: vec![1, 2, 3, 4, 5],
            feetech_bus2_mixed_raw: false,
        }
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
