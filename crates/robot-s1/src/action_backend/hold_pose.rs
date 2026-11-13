//! v0.3 Phase B B3 (1/2) `HoldPoseBackend`：第一个真 action backend。
//!
//! ## 行为
//!
//! - `name()` = `"hold_pose"`（profile `[s1.profile.backends]` 引用名）。
//! - `supported_kinds()` = `&[ActionKind::HoldPose]`（B2 outline §2.2
//!   声明的 variant，B3 (1/2) 补 backend 实现）。
//! - `supported_schemas()` = `&[NO_PAYLOAD_SCHEMA_ID]`（reviewer 阻断 1
//!   锁定：**显式哨兵**，不空切片）。
//! - `start(req, ctx)`：拒非空 payload 防御；返
//!   [`HoldPoseInstance`]（无 duration 字段、无私有 counter；hold 直到
//!   `cancel`）。
//! - `tick(ctx)`：
//!   * cancel flag 置位 → `Completed(PartialSuccess { warning:
//!     "cancelled" })`（reviewer 拍板：cancel 收敛走 PartialSuccess；
//!     coordinator 把这条转成 `ActionEvent::Cancelled` 写事件流）。
//!   * 否则用 `build_hold_command(ctx.config, ctx.layout_id,
//!     ctx.anchor_bias, ctx.command_seq, ctx.clock.now().0)` 产 body
//!     bytes，返 `Continue { progress: 0.0, command: Some(body) }`。
//! - `cancel(ctx)`：set flag，返 `Ok(())`。
//!
//! ## reviewer round 2 阻断 1 真锁
//!
//! HoldPose body 的 `source_task_id` **必须**等于 `ctx.command_seq`，
//! 由 `run_action_tick_loop` 透传当前 publish frame 的 `TickState.cmd_seq`
//! 进 `ActionContext.command_seq`。**不**允许 backend 私有 counter 从
//! 0 起增 —— 那会让 S0 `ipc_thread::join_command_to_buf` 把
//! `fb.source_task_id()` 写到 `JointCommandBuf.seq` 时看到 seq 回退，
//! 破坏 `adapter.rs` 单调递增契约（`rt_thread` 判 `CONTROL_LOST` →
//! `ACTIVE` 恢复用）。详 B3 outline §2.1 #4 + §3.4 + §5.1 单测
//! `tick_returns_continue_with_some_hold_body_bytes_bit_equivalent_to_build_hold_command`。
//!
//! ## progress 不语义化
//!
//! reviewer 拍板 #10：hold-until-cancel 路径 `progress` 没自然计算源；
//! 永远填 `0.0`，doc 注明"不语义化"。如调用方需要进度，需扩 backend
//! 字段或选 (b) duration_ms 路径（B3+ follow-on）。
//!
//! ## inventory 注册
//!
//! 整文件 `#[cfg(feature = "backend-hold-pose")]` gate（默认 on）。
//! Feature off 时 `mod hold_pose` 不 link → `BackendRegistry::from_inventory`
//! 收集不到 hold_pose → `[s1.profile.backends = ["hold_pose"]]` 在
//! [`crate::coordinator::validate_profile_section`] 报 `UnknownBackend`，
//! 符合"配置与编译 profile 不匹配启动期 fail-fast"硬约束。

#![allow(clippy::doc_markdown)]

use crate::action::{
    ActionBackend, ActionCancelError, ActionContext, ActionFailure, ActionInstance, ActionKind,
    ActionOutcome, ActionRequest, ActionStartError, ActionTickOutput, JointCommandBody,
    NO_PAYLOAD_SCHEMA_ID,
};
use crate::coordinator::build_hardware_hold_command;
#[cfg(test)]
use crate::coordinator::build_hold_command;

// ============================================================================
// HoldPoseBackend
// ============================================================================

/// 第一个真 action backend：每 tick 用 [`crate::coordinator::build_hold_command`] 产
/// `target = anchor_bias` 的 hold cmd body bytes。等价 IDLE 状态的 hold
/// loop，但走 EXECUTING 路径（B2 outline §2.2 声明 / B3 round 2 落地）。
///
/// **零字段** struct：hold-until-cancel 语义不需要 duration / counter；
/// `source_task_id` 来自 `ctx.command_seq`（reviewer round 2 阻断 1 修
/// 法 + 详模块顶 doc）。
#[derive(Debug, Clone, Copy)]
pub struct HoldPoseBackend;

// `name()` / `supported_kinds()` / `supported_schemas()` 返回 'static
// 字面量，但 trait 签名是 elided lifetime（`-> &str`）。clippy
// `unnecessary_literal_bound` 建议改 `&'static str` 跟 trait 签名冲突；
// 此处 allow 跟 mock_dwell / 其它 backend 同款 trait 实现路径。
#[allow(clippy::unnecessary_literal_bound)]
impl ActionBackend for HoldPoseBackend {
    fn name(&self) -> &str {
        "hold_pose"
    }

    fn supported_kinds(&self) -> &[ActionKind] {
        // 静态 slice：HoldPose 仅一种 kind；其它 backend 各自声明。
        const KINDS: &[ActionKind] = &[ActionKind::HoldPose];
        KINDS
    }

    fn supported_schemas(&self) -> &[u32] {
        // reviewer 阻断 1（B2 outline）锁定：**显式哨兵**，不空切片。
        const SCHEMAS: &[u32] = &[NO_PAYLOAD_SCHEMA_ID];
        SCHEMAS
    }

    fn start(
        &self,
        req: &ActionRequest,
        _ctx: &ActionContext<'_>,
    ) -> Result<Box<dyn ActionInstance>, ActionStartError> {
        // validate() 已守门 NO_PAYLOAD_SCHEMA_ID + payload empty 一致性
        // （[`crate::action::ActionRequest::validate`]）；这里再做一次
        // 防御性检查，让 backend 自身对"传错 payload"有清晰错误路径。
        if !req.payload.is_empty() {
            return Err(ActionStartError::Rejected(format!(
                "HoldPoseBackend expects empty payload, got {} bytes",
                req.payload.len()
            )));
        }

        Ok(Box::new(HoldPoseInstance { cancelled: false }))
    }
}

// ============================================================================
// HoldPoseInstance
// ============================================================================

/// `HoldPoseBackend::start` 返的 per-action 实例。
///
/// **零状态字段**（除 cancel flag）：每 tick 从 `ActionContext` 借
/// `clock` / `config` / `layout_id` / `anchor_bias` / `command_seq` 重
/// 算 body bytes。**不**capture `anchor_bias` 快照（B3 期间
/// anchor_bias 构造期固化不变；保留每 tick 读为 v0.4 扩动态 anchor 留
/// 扩展点）。
struct HoldPoseInstance {
    /// `cancel()` 设置；下次 [`ActionInstance::tick`] 自查 + 收敛返
    /// `Completed(PartialSuccess { warning: "cancelled" })`。
    cancelled: bool,
}

impl ActionInstance for HoldPoseInstance {
    fn tick(&mut self, ctx: &mut ActionContext<'_>) -> Result<ActionTickOutput, ActionFailure> {
        if self.cancelled {
            // reviewer 拍板：cancel 收敛走 PartialSuccess；coordinator 把
            // 这条转 ActionEvent::Cancelled 写入事件流（不发 Completed）。
            return Ok(ActionTickOutput::Completed(ActionOutcome::PartialSuccess {
                warning: "cancelled".to_string(),
            }));
        }

        // **核心**（B3 round 2 阻断 1 修法）：用 ctx.command_seq 作
        // source_task_id。S0 ipc_thread 把 fb.source_task_id() 写到
        // JointCommandBuf.seq；adapter.rs 要求单调递增。
        let body = build_hardware_hold_command(
            ctx.config,
            ctx.hardware,
            ctx.layout_id,
            ctx.anchor_bias,
            /* source_task_id */ ctx.command_seq,
            /* now_ns */ ctx.clock.now().0,
        );

        Ok(ActionTickOutput::Continue {
            // reviewer 拍板 #10：hold-until-cancel 路径 progress 永远
            // 0.0，**不**语义化（详模块顶 doc）。
            progress: 0.0,
            command: Some(JointCommandBody::new(body)),
        })
    }

    fn cancel(&mut self, _ctx: &mut ActionContext<'_>) -> Result<(), ActionCancelError> {
        // cooperative：set flag，下次 tick 自查收敛。极少失败 —— 总返 Ok。
        self.cancelled = true;
        Ok(())
    }
}

// ============================================================================
// inventory 注册
// ============================================================================

/// inventory 静态实例。
///
/// `BackendRegistry::from_inventory` 启动期一次性扫描，把本实例的
/// `&'static dyn ActionBackend` 收进 `by_name` map（key = `"hold_pose"`）。
static HOLD_POSE_BACKEND: HoldPoseBackend = HoldPoseBackend;

inventory::submit! {
    &HOLD_POSE_BACKEND as &'static dyn ActionBackend
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use crate::action::{ActionId, ActionKind, ActionRequest, ActionSource};
    use crate::coordinator::{CoordinatorConfig, HardwareSection};
    use flatbuffers::root;
    use robot_core::time::{MockClock, MonoRawNs};
    use robot_msg::msg::{ControlMode, JointCommand as FbJointCommand, StaleAction};

    const TEST_LAYOUT_ID: u64 = 0xCAFE_BABE_DEAD_BEEF;

    // ----- helpers -----

    fn sample_request() -> ActionRequest {
        ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::HoldPose,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: NO_PAYLOAD_SCHEMA_ID,
            payload: Vec::new(),
            idempotency_key: "test-hold-pose-key".into(),
            source: ActionSource::Internal,
        }
    }

    fn sample_hardware() -> HardwareSection {
        HardwareSection {
            id: "test_arm".into(),
            joints: vec!["j0".into(), "j1".into(), "j2".into()],
            motor_ids: vec![1, 2, 3],
            feetech_bus2_mixed_raw: false,
        }
    }

    /// 构造一个 backend instance + 配套 cfg / clock / hardware / anchor_bias，
    /// 返 (instance, cfg, hardware, anchor_bias) 让测试共用。
    fn fresh_instance(
        clock_now_ns: u64,
    ) -> (
        Box<dyn ActionInstance>,
        CoordinatorConfig,
        HardwareSection,
        Vec<f64>,
    ) {
        let backend = HoldPoseBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = sample_hardware();
        let anchor_bias = vec![0.5_f64, -0.5, 0.25];
        let clock = MockClock::new(MonoRawNs(clock_now_ns));
        let req = sample_request();
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
            solver: None,
        };
        let instance = backend.start(&req, &ctx).expect("start ok");
        (instance, cfg, hardware, anchor_bias)
    }

    // ----- 1. name() -----

    #[test]
    fn name_is_hold_pose() {
        let b = HoldPoseBackend;
        assert_eq!(b.name(), "hold_pose");
    }

    // ----- 2. supported_kinds() -----

    #[test]
    fn supported_kinds_is_hold_pose_only() {
        let b = HoldPoseBackend;
        let kinds = b.supported_kinds();
        assert_eq!(kinds.len(), 1);
        assert_eq!(kinds[0], ActionKind::HoldPose);
    }

    // ----- 3. supported_schemas() -----

    #[test]
    fn supported_schemas_is_explicit_no_payload_sentinel() {
        // reviewer 阻断 1（B2 outline）验收契约：必须显式
        // &[NO_PAYLOAD_SCHEMA_ID]，**不**空切片。
        let b = HoldPoseBackend;
        let schemas = b.supported_schemas();
        assert_eq!(schemas, &[NO_PAYLOAD_SCHEMA_ID]);
        assert!(!schemas.is_empty(), "must NOT be empty slice (no magic)");
    }

    // ----- 4. start 防御性 payload 校验 -----

    #[test]
    fn start_rejects_non_empty_payload() {
        // 防御性：即使 ActionRequest::validate 已守门，backend 自身对
        // "传错 payload"也有清晰错误路径。
        let backend = HoldPoseBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = sample_hardware();
        let anchor_bias = vec![0.0_f64; 3];
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let mut req = sample_request();
        req.payload = vec![1, 2, 3]; // 故意非空
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
            solver: None,
        };
        // Box<dyn ActionInstance> 不 impl Debug，不能 expect_err；走
        // let-else（跟 mock_dwell 同款）。
        let Err(err) = backend.start(&req, &ctx) else {
            panic!("expected Err");
        };
        assert!(
            matches!(err, ActionStartError::Rejected(ref s) if s.contains("empty payload")),
            "got {err:?}"
        );
    }

    // ----- 5. **核心** bit-equivalent 字节比对（reviewer round 2 收紧） -----

    #[test]
    fn mixed_hold_pose_preserves_only_arm_position_and_stops_wheels() {
        let mut hardware = crate::coordinator::parse_hardware_section_from_toml(
            include_str!("../../../../configs/xlerobot-bus2.toml"),
            "right_arm_full_with_base",
        )
        .unwrap();
        hardware.feetech_bus2_mixed_raw = true;
        hardware.joints.rotate_left(3);
        hardware.motor_ids.rotate_left(3);
        let anchor_bias = vec![0.5; 9];
        let clock = MockClock::new(MonoRawNs(100));
        let cfg = CoordinatorConfig::default();
        let mut ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 42,
            scripted_start_pose: None,
            solver: None,
        };
        let mut instance = HoldPoseBackend.start(&sample_request(), &ctx).unwrap();
        let ActionTickOutput::Continue {
            command: Some(body),
            ..
        } = instance.tick(&mut ctx).unwrap()
        else {
            panic!("需要hold帧")
        };
        let command = root::<FbJointCommand>(body.as_bytes()).unwrap();
        assert_eq!(command.mode(), ControlMode::MixedPositionRawVelocity);
        assert_eq!(
            command.raw_velocities().unwrap().iter().collect::<Vec<_>>(),
            [0; 9]
        );
        for (slot, id) in hardware.motor_ids.iter().enumerate() {
            assert_eq!(
                command.target().unwrap().get(slot),
                if *id <= 6 { 0.5 } else { 0.0 }
            );
        }
        assert_eq!(command.source_task_id(), 42);
    }

    #[test]
    fn tick_returns_continue_with_some_hold_body_bytes_bit_equivalent_to_build_hold_command() {
        // reviewer round 2 阻断 1 + 中度收紧 验收：用同源 cfg /
        // layout_id / anchor_bias / command_seq / now_ns 调
        // build_hold_command 构造 ground truth，跟 backend tick 返的 body
        // 字节级相等 + 字段断言。
        let now_ns: u64 = 1_234_567_890;
        let command_seq: u64 = 42;
        let cfg = CoordinatorConfig::default();
        let hardware = sample_hardware();
        let anchor_bias = vec![0.5_f64, -0.5, 0.25];
        let clock = MockClock::new(MonoRawNs(now_ns));

        let backend = HoldPoseBackend;
        let req = sample_request();
        let mut ctx = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq,
            scripted_start_pose: None,
            solver: None,
        };
        let mut instance = backend.start(&req, &ctx).expect("start ok");

        // 驱动 tick；MockClock now 不变（不 advance）让 ground truth
        // 用 same now_ns 构造。
        let out = instance.tick(&mut ctx).expect("tick ok");
        let body = match out {
            ActionTickOutput::Continue {
                progress,
                command: Some(body),
            } => {
                assert!(
                    (progress - 0.0).abs() < f32::EPSILON,
                    "progress 应永远 0.0（reviewer 拍板 #10 不语义化），got {progress}"
                );
                body
            }
            other => panic!("expected Continue with Some(body), got {other:?}"),
        };

        // ground truth：用同样 cfg / layout_id / anchor_bias / command_seq
        // / now_ns 调 build_hold_command。
        let ground_truth = build_hold_command(
            &cfg,
            TEST_LAYOUT_ID,
            &anchor_bias,
            /* source_task_id */ command_seq,
            /* now_ns */ now_ns,
        );

        // 字节级相等 —— 锁住 backend 的 body 跟 build_hold_command 的字节
        // 完全同源（reviewer round 2 阻断 1 修法真生效的最强证据）。
        assert_eq!(
            body.as_bytes(),
            ground_truth.as_slice(),
            "HoldPose body bytes 必须 bit-equivalent to build_hold_command(cfg, \
             layout_id, anchor_bias, ctx.command_seq, ctx.clock.now().0)"
        );

        // 附加字段断言（fbs 解码）：double-check 关键 wire 字段。
        let cmd = root::<FbJointCommand<'_>>(body.as_bytes()).expect("fbs verify");
        assert_eq!(cmd.layout_id(), TEST_LAYOUT_ID);
        assert_eq!(cmd.mode(), ControlMode::Position);
        let target: Vec<f64> = cmd.target().expect("target").iter().collect();
        assert_eq!(target, anchor_bias, "target == anchor_bias");
        assert_eq!(
            cmd.source_task_id(),
            command_seq,
            "source_task_id 必须 == ctx.command_seq（reviewer round 2 阻断 1 锁定）"
        );
        assert_eq!(
            cmd.deadline_ns(),
            now_ns + cfg.deadline_offset_ns(),
            "deadline_ns == now_ns + cfg.deadline_offset_ns()"
        );
        assert_eq!(cmd.priority(), cfg.priority);
        assert_eq!(cmd.on_timeout(), StaleAction::Hold);
    }

    // ----- 6. cancel 收敛 -----

    #[test]
    fn tick_after_cancel_returns_completed_partial_success() {
        let (mut instance, cfg, hardware, anchor_bias) = fresh_instance(1_000_000_000);
        let clock = MockClock::new(MonoRawNs(1_000_000_000));

        // 第一帧正常 tick（验状态确实在跑）。
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
            solver: None,
        };
        let out = instance.tick(&mut ctx).expect("tick ok");
        assert!(matches!(out, ActionTickOutput::Continue { .. }));

        // cancel + 下次 tick → PartialSuccess { warning: "cancelled" }。
        instance.cancel(&mut ctx).expect("cancel ok");
        let out = instance.tick(&mut ctx).expect("tick ok");
        match out {
            ActionTickOutput::Completed(ActionOutcome::PartialSuccess { warning }) => {
                assert_eq!(warning, "cancelled");
            }
            ActionTickOutput::Completed(ActionOutcome::Success) => {
                panic!("expected PartialSuccess(cancelled), got Completed(Success)")
            }
            ActionTickOutput::Continue { .. } => {
                panic!("expected Completed after cancel, got Continue")
            }
        }
    }

    // ----- 7. progress 永远 0.0 -----

    #[test]
    fn tick_progress_field_pegged_to_zero() {
        // 驱动 backend N tick + 推 clock 每次；progress 永远 0.0
        // （reviewer 拍板 #10 hold-until-cancel 路径不语义化）。
        let backend = HoldPoseBackend;
        let cfg = CoordinatorConfig::default();
        let hardware = sample_hardware();
        let anchor_bias = vec![0.0_f64; 3];
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let req = sample_request();

        let ctx_initial = ActionContext {
            single_joint_config: None,
            feedback: None,
            clock: &clock,
            layout_id: TEST_LAYOUT_ID,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor_bias,
            command_seq: 0,
            scripted_start_pose: None,
            solver: None,
        };
        let mut instance = backend.start(&req, &ctx_initial).expect("start ok");

        for i in 0..5 {
            // 每次 tick 推 20ms 模拟 50Hz cadence；命令 seq 累增。
            clock.advance(robot_core::time::DurationNs::from_nanos(20_000_000));
            let mut ctx = ActionContext {
                single_joint_config: None,
                feedback: None,
                clock: &clock,
                layout_id: TEST_LAYOUT_ID,
                hardware: &hardware,
                config: &cfg,
                anchor_bias: &anchor_bias,
                command_seq: i + 1,
                scripted_start_pose: None,
                solver: None,
            };
            let out = instance.tick(&mut ctx).expect("tick ok");
            match out {
                ActionTickOutput::Continue { progress, .. } => {
                    assert!(
                        (progress - 0.0).abs() < f32::EPSILON,
                        "iter {i}: progress 永远 0.0，got {progress}"
                    );
                }
                ActionTickOutput::Completed(out) => {
                    panic!("iter {i}: expected Continue, got Completed({out:?})")
                }
            }
        }
    }

    // ----- 8. inventory default instance present -----

    #[test]
    fn inventory_default_instance_present() {
        // 锁住 HoldPoseBackend 静态实例真注册到 inventory；改 inventory
        // 注册口要同步改这条测试。
        let names: Vec<&'static str> = inventory::iter::<&'static dyn ActionBackend>
            .into_iter()
            .map(|b| b.name())
            .collect();
        assert!(
            names.contains(&"hold_pose"),
            "HoldPoseBackend 必须通过 inventory::submit! 真注册；inventory 名字集 = {names:?}"
        );
    }
}
