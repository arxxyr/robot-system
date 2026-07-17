//! v0.3 Phase B B2 (2/3) MockDwellBackend：占位 backend，N ms 后
//! `Completed(Success)`。仅为驱动 in-process 集成测，**不**接真 IK / 真
//! RL / 真 policy。
//!
//! ## 行为
//!
//! - `name()` = `"mock_dwell"`（profile `[s1.profile.backends]` 引用名）。
//! - `supported_kinds()` = `&[ActionKind::DwellMs]`（reviewer 拍板：B2
//!   ActionKind 仅 `DwellMs` / `HoldPose`，HoldPose 在 B2 没 backend
//!   实现）。
//! - `supported_schemas()` = `&[NO_PAYLOAD_SCHEMA_ID]`（reviewer 阻断 1
//!   锁定：**显式哨兵**，不空切片）。
//! - `start(req, ctx)`：构造 [`MockDwellInstance`] `end_ns = ctx.clock.now() +
//!   self.duration_ms * 1_000_000`；记 `started_at_ns` 给 progress 计算。
//! - `tick(ctx)`：
//!   - cancel flag 置位 → `Completed(PartialSuccess { warning: "cancelled" })`
//!     （reviewer 拍板：cancel 收敛走 PartialSuccess；coordinator 转
//!     `ActionEvent::Cancelled` 不直接发 `Completed`）。
//!   - now >= end_ns → `Completed(Success)`。
//!   - 否则 `Continue { progress, command: None }`（不发新
//!     [`crate::action::JointCommandBody`]；hold loop 由 coordinator 在
//!     IDLE 时接管 / EXECUTING 时停发，详 outline §6 共存策略 = override）。
//! - `cancel(_ctx)`：set flag，返 `Ok(())`。
//!
//! ## duration_ms 来源
//!
//! reviewer 拍板：`MockDwellBackend { duration_ms }` 字段配置，**不**走
//! `ActionRequest.payload`（B2 payload 为空）。inventory 注册一个默认
//! 100ms 的静态实例；测试用 [`MockDwellBackend::with_duration_ms`] 构
//! 造独立实例（不进 inventory）。
//!
//! `duration_ms == 0` 在 [`MockDwellBackend::with_duration_ms`] **panic**
//! （reviewer 拍板：programmer error 用 `assert!`，单测 `#[should_panic]`
//! 锁；返 `Result` 包装单条不变量过度工程）。
//!
//! ## inventory 注册
//!
//! 整文件 `#[cfg(feature = "backend-mock-dwell")]` gate（默认 on）。Feature
//! off 时 `mod mock_dwell` 不 link → `BackendRegistry::from_inventory`
//! 收集不到 mock_dwell → `[s1.profile.backends = ["mock_dwell"]]` 在
//! [`crate::coordinator::validate_profile_section`] 报
//! `UnknownBackend`，符合"配置与编译 profile 不匹配启动期 fail-fast"
//! 硬约束。

#![allow(clippy::doc_markdown)]

use crate::action::{
    ActionBackend, ActionContext, ActionFailure, ActionInstance, ActionKind, ActionOutcome,
    ActionRequest, ActionStartError, ActionTickOutput, NO_PAYLOAD_SCHEMA_ID,
};

// ============================================================================
// MockDwellBackend
// ============================================================================

/// 占位 backend。N ms 后 [`ActionOutcome::Success`]，整段不发新
/// [`crate::action::JointCommandBody`]（沿用 hold bias）。
///
/// 字段 `duration_ms`：dwell 时长（ms），reviewer 拍板默认 100ms。
/// `0` 在 [`Self::with_duration_ms`] panic（programmer error）。
#[derive(Debug, Clone, Copy)]
pub struct MockDwellBackend {
    duration_ms: u64,
}

impl MockDwellBackend {
    /// reviewer 拍板默认值：100 ms。
    pub const DEFAULT_DURATION_MS: u64 = 100;

    /// 构造一个**测试用**实例。`duration_ms == 0` panic（不拐弯）。
    ///
    /// 生产路径走 inventory 静态注册的 `MOCK_DWELL_BACKEND`（默认
    /// 100ms）；测试用本入口给自定义 duration。
    ///
    /// **可见性**（B2 (3/3) 升级）：原 `#[cfg(test)] pub(crate)` 改 `pub`
    /// 让 `crates/robot-s1/tests/action_dispatch.rs` integration test 跨
    /// crate 引用。MockDwell 本身就是 B2 mock backend（生产代码也仅靠
    /// inventory 默认实例），暴露此构造器不增加误用风险。
    ///
    /// # Panics
    ///
    /// `duration_ms == 0` 直接 `assert!`：MockDwell 0 ms = 永不执行（end_ns
    /// 立即 ≤ now，第一帧 tick 就 Completed），失去测试用 dwell 行为的语
    /// 义；属 programmer error，不是 runtime IO error。
    #[must_use]
    pub fn with_duration_ms(duration_ms: u64) -> Self {
        assert!(
            duration_ms > 0,
            "MockDwellBackend::with_duration_ms: duration_ms must be > 0 (got 0)"
        );
        Self { duration_ms }
    }
}

// `name()` / `supported_kinds()` / `supported_schemas()` 返回 'static
// 字面量，但 trait 签名是 elided lifetime（`-> &str`）。clippy
// `unnecessary_literal_bound` 会建议改 `&'static str`，跟 trait 签名冲
// 突；此处 allow 跟 trait 保持一致。
#[allow(clippy::unnecessary_literal_bound)]
impl ActionBackend for MockDwellBackend {
    fn name(&self) -> &str {
        "mock_dwell"
    }

    fn supported_kinds(&self) -> &[ActionKind] {
        // 静态 slice：MockDwell 仅 DwellMs；HoldPose 在 B2 无 backend。
        const KINDS: &[ActionKind] = &[ActionKind::DwellMs];
        KINDS
    }

    fn supported_schemas(&self) -> &[u32] {
        // reviewer 阻断 1 锁定：**显式哨兵**，不空切片。
        const SCHEMAS: &[u32] = &[NO_PAYLOAD_SCHEMA_ID];
        SCHEMAS
    }

    fn start(
        &self,
        req: &ActionRequest,
        ctx: &ActionContext<'_>,
    ) -> Result<Box<dyn ActionInstance>, ActionStartError> {
        // validate() 已守门 NO_PAYLOAD_SCHEMA_ID + payload empty 一致性
        // （[`crate::action::ActionRequest::validate`]）；这里再做一次防御
        // 性检查，让 backend 自身对"传错 payload"有清晰错误路径。
        if !req.payload.is_empty() {
            return Err(ActionStartError::Rejected(format!(
                "MockDwellBackend expects empty payload, got {} bytes",
                req.payload.len()
            )));
        }

        let now_ns = ctx.clock.now().0;
        // saturating add 防极端 now_ns + duration_ms*1e6 溢出（u64::MAX
        // 远超系统寿命，clip 到饱和值；正常 duration_ms <= 86400_000 ms
        // 不会触发）。
        let end_ns = now_ns.saturating_add(self.duration_ms.saturating_mul(1_000_000));

        Ok(Box::new(MockDwellInstance {
            started_at_ns: now_ns,
            end_ns,
            cancelled: false,
        }))
    }
}

// ============================================================================
// MockDwellInstance
// ============================================================================

/// `MockDwellBackend::start` 返的 per-action 实例。
struct MockDwellInstance {
    started_at_ns: u64,
    end_ns: u64,
    /// `cancel()` 设置；下次 [`ActionInstance::tick`] 自查 + 收敛返
    /// `Completed(PartialSuccess { warning: "cancelled" })`。
    cancelled: bool,
}

impl ActionInstance for MockDwellInstance {
    fn tick(&mut self, ctx: &mut ActionContext<'_>) -> Result<ActionTickOutput, ActionFailure> {
        if self.cancelled {
            // reviewer 拍板：cancel 收敛走 PartialSuccess；coordinator 把
            // 这条转成 ActionEvent::Cancelled 写入事件流（不发 Completed）。
            return Ok(ActionTickOutput::Completed(ActionOutcome::PartialSuccess {
                warning: "cancelled".to_string(),
            }));
        }

        let now_ns = ctx.clock.now().0;
        if now_ns >= self.end_ns {
            return Ok(ActionTickOutput::Completed(ActionOutcome::Success));
        }

        // 进度估算：(now - start) / (end - start)，clip 到 [0.0, 1.0]。
        // duration > 0 在 backend 构造时已断言，total > 0 安全。
        let total = self.end_ns.saturating_sub(self.started_at_ns).max(1);
        let elapsed = now_ns.saturating_sub(self.started_at_ns);
        // u64 → f64 精度丢失 ms 级以下；MockDwell 默认 100ms = 1e8 ns，
        // f64 mantissa 53 bit ~ 2^53 = 9e15 ns ≈ 100 days，不踩精度坑。
        // f64 → f32 经 clamp 到 [0.0, 1.0]，f32 mantissa 23 bit 完全够用。
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let progress = (elapsed as f64 / total as f64).clamp(0.0, 1.0) as f32;

        Ok(ActionTickOutput::Continue {
            progress,
            command: None,
        })
    }

    fn cancel(
        &mut self,
        _ctx: &mut ActionContext<'_>,
    ) -> Result<(), crate::action::ActionCancelError> {
        // cooperative：set flag，下次 tick 自查收敛。极少失败 —— 总返 Ok。
        self.cancelled = true;
        Ok(())
    }
}

// ============================================================================
// inventory 注册
// ============================================================================

/// inventory 静态实例：默认 100ms。
///
/// `BackendRegistry::from_inventory` 启动期一次性扫描，把本实例的
/// `&'static dyn ActionBackend` 收进 `by_name` map（key = `"mock_dwell"`）。
static MOCK_DWELL_BACKEND: MockDwellBackend = MockDwellBackend {
    duration_ms: MockDwellBackend::DEFAULT_DURATION_MS,
};

inventory::submit! {
    &MOCK_DWELL_BACKEND as &'static dyn ActionBackend
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use crate::action::{ActionId, ActionKind, ActionRequest, ActionSource};
    use crate::coordinator::{CoordinatorConfig, HardwareSection};
    use robot_core::time::{MockClock, MonoRawNs, RobotClock};

    // ----- helpers -----

    fn sample_request(deadline_ns: u64) -> ActionRequest {
        ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::DwellMs,
            deadline_ns,
            priority: 10,
            payload_schema_id: NO_PAYLOAD_SCHEMA_ID,
            payload: Vec::new(),
            idempotency_key: "test-key-001".into(),
            source: ActionSource::Internal,
        }
    }

    fn sample_hardware() -> HardwareSection {
        HardwareSection {
            id: "test_arm".into(),
            joints: vec!["j0".into()],
            motor_ids: vec![1],
        }
    }

    // ----- with_duration_ms -----

    #[test]
    fn with_duration_ms_constructs_with_given_value() {
        let b = MockDwellBackend::with_duration_ms(50);
        assert_eq!(b.duration_ms, 50);
    }

    #[test]
    #[should_panic(expected = "duration_ms must be > 0")]
    fn with_duration_ms_panics_on_zero() {
        let _ = MockDwellBackend::with_duration_ms(0);
    }

    // ----- ActionBackend impl -----

    #[test]
    fn name_is_mock_dwell() {
        let b = MockDwellBackend::with_duration_ms(100);
        assert_eq!(b.name(), "mock_dwell");
    }

    #[test]
    fn supported_kinds_is_dwell_ms_only() {
        let b = MockDwellBackend::with_duration_ms(100);
        let kinds = b.supported_kinds();
        assert_eq!(kinds.len(), 1);
        assert_eq!(kinds[0], ActionKind::DwellMs);
    }

    #[test]
    fn supported_schemas_is_explicit_no_payload_sentinel() {
        // reviewer 阻断 1 验收契约：必须显式 &[NO_PAYLOAD_SCHEMA_ID]，
        // 不能空切片走"any payload"魔法。
        let b = MockDwellBackend::with_duration_ms(100);
        let schemas = b.supported_schemas();
        assert_eq!(schemas, &[NO_PAYLOAD_SCHEMA_ID]);
        // 强证非空切片（防 review 时混淆）。
        assert!(!schemas.is_empty());
    }

    #[test]
    fn inventory_default_instance_uses_default_duration() {
        // 静态 inventory 实例的 duration 固定 = DEFAULT_DURATION_MS。
        // 这条测试锁住"改默认值要同步改 const + 测试"。
        assert_eq!(MockDwellBackend::DEFAULT_DURATION_MS, 100);
        assert_eq!(MOCK_DWELL_BACKEND.duration_ms, 100);
    }

    // ----- start / tick happy path -----

    fn run_tick<F>(
        // MockDwellBackend 是 Copy + 8 字节，按值传更省（clippy
        // trivially_copy_pass_by_ref 拍板）。
        backend: MockDwellBackend,
        clock: &MockClock,
        mut step_clock_ns: F,
    ) -> Box<dyn ActionInstance>
    where
        F: FnMut(),
    {
        let cfg = CoordinatorConfig::default();
        let hardware = sample_hardware();
        let anchor = vec![0.0_f64];
        let req = sample_request(clock.now().0 + 1_000_000_000); // +1s deadline (足够 dwell)

        let ctx = ActionContext {
            clock,
            layout_id: 0xCAFE,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor,
            command_seq: 0,
            solver: None,
        };
        let instance = backend.start(&req, &ctx).expect("start ok");
        // 让调用方推 clock 之后再 tick。
        step_clock_ns();
        instance
    }

    #[test]
    fn tick_returns_continue_before_duration_elapsed() {
        let backend = MockDwellBackend::with_duration_ms(100); // 100 ms = 1e8 ns
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let mut instance = run_tick(backend, &clock, || {
            // 推 50 ms（< 100 ms duration）。
            clock.advance(robot_core::time::DurationNs::from_nanos(50_000_000));
        });

        let cfg = CoordinatorConfig::default();
        let hardware = sample_hardware();
        let anchor = vec![0.0];
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: 0xCAFE,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor,
            command_seq: 0,
            solver: None,
        };
        let out = instance.tick(&mut ctx).expect("tick ok");
        match out {
            ActionTickOutput::Continue { progress, command } => {
                assert!(
                    (0.45..=0.55).contains(&progress),
                    "progress should be ~0.5, got {progress}"
                );
                assert!(command.is_none(), "MockDwell never publishes new cmd");
            }
            ActionTickOutput::Completed(out) => {
                panic!("expected Continue mid-dwell, got Completed({out:?})")
            }
        }
    }

    #[test]
    fn tick_returns_completed_success_when_duration_elapsed() {
        let backend = MockDwellBackend::with_duration_ms(50);
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let mut instance = run_tick(backend, &clock, || {
            // 推 60 ms（> 50 ms duration）。
            clock.advance(robot_core::time::DurationNs::from_nanos(60_000_000));
        });

        let cfg = CoordinatorConfig::default();
        let hardware = sample_hardware();
        let anchor = vec![0.0];
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: 0xCAFE,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor,
            command_seq: 0,
            solver: None,
        };
        let out = instance.tick(&mut ctx).expect("tick ok");
        assert!(matches!(
            out,
            ActionTickOutput::Completed(ActionOutcome::Success)
        ));
    }

    // ----- cancel path -----

    #[test]
    fn cancel_flag_makes_next_tick_return_partial_success_warning() {
        let backend = MockDwellBackend::with_duration_ms(1000); // 长 dwell
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let cfg = CoordinatorConfig::default();
        let hardware = sample_hardware();
        let anchor = vec![0.0];
        let req = sample_request(clock.now().0 + 5_000_000_000);

        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: 0xCAFE,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor,
            command_seq: 0,
            solver: None,
        };
        let mut instance = backend.start(&req, &ctx).expect("start ok");

        // 先正常 tick 一次确认在跑。
        clock.advance(robot_core::time::DurationNs::from_nanos(20_000_000));
        let out = instance.tick(&mut ctx).expect("tick ok");
        assert!(matches!(out, ActionTickOutput::Continue { .. }));

        // cancel + 下次 tick → PartialSuccess
        instance.cancel(&mut ctx).expect("cancel ok");
        clock.advance(robot_core::time::DurationNs::from_nanos(10_000_000));
        let out = instance.tick(&mut ctx).expect("tick ok");
        match out {
            ActionTickOutput::Completed(ActionOutcome::PartialSuccess { warning }) => {
                assert_eq!(warning, "cancelled");
            }
            other => panic!("expected PartialSuccess(cancelled), got {other:?}"),
        }
    }

    // ----- start payload validation -----

    #[test]
    fn start_rejects_non_empty_payload() {
        // 防御性检查：即使 ActionRequest::validate 已经守门，backend 自身
        // 也对"传错 payload"有清晰错误路径。
        let backend = MockDwellBackend::with_duration_ms(100);
        let clock = MockClock::new(MonoRawNs(1_000_000_000));
        let cfg = CoordinatorConfig::default();
        let hardware = sample_hardware();
        let anchor = vec![0.0];
        let mut req = sample_request(clock.now().0 + 1_000_000_000);
        // 模拟 dispatcher 误把非空 payload + sentinel schema_id 路由进来。
        req.payload = vec![1, 2, 3];

        let ctx = ActionContext {
            clock: &clock,
            layout_id: 0xCAFE,
            hardware: &hardware,
            config: &cfg,
            anchor_bias: &anchor,
            command_seq: 0,
            solver: None,
        };
        // Box<dyn ActionInstance> 不 impl Debug，不能 expect_err；走
        // let-else 把 Ok 排除。
        let Err(err) = backend.start(&req, &ctx) else {
            panic!("expected Err");
        };
        assert!(
            matches!(err, ActionStartError::Rejected(ref s) if s.contains("empty payload")),
            "got {err:?}"
        );
    }
}
