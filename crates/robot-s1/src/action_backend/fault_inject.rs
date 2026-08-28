//! `FaultInjectBackend` —— **test-only** 执行期 failure 触发器（v0.5 候选 E
//! 段(7)，Spec QA6 + R3-B1 锁死）。
//!
//! ## 为什么需要它
//!
//! 生产 backend **没有执行期 failure 触发器**：`ScriptedBackend` 运行期只返
//! `Continue` / `Completed`，`IkBackend` 的错误全部落在 `start()` 阶段
//! （提交即拒，走 `ActionStartError`）。于是 AC-E14 的**执行期不对称终态**
//! （一侧 accepted 后失败、另一侧仍 Executing → cancel 幸存侧）与 AC-E18 的
//! **failure 分支**（Failed → coordinator `Degraded` → `ClearError` 回 Idle）
//! 在 e2e 里无从触发。Spec QA6 拍板：**test-only fault backend**（"Reverse
//! Sync 降 AC 验证层级" 备选未采）。
//!
//! ## 分发键：0 新 wire（R3-B1）
//!
//! | 项 | 值 | 理由 |
//! |---|---|---|
//! | `name()` | `"fault_inject"` | profile `[s1.profile.backends]` 引用名 |
//! | `supported_kinds()` | `&[ActionKind::DwellMs]` | 既有 wire 键，0 新增 |
//! | `supported_schemas()` | `&[NO_PAYLOAD_SCHEMA_ID]` | 同上（显式哨兵，不空切片） |
//!
//! **冲突消除**：`DwellMs + NO_PAYLOAD_SCHEMA_ID` 与 `mock_dwell` 完全重合，
//! 而 dispatch 按 enabled 列表 × `(kind, schema)` 取**首个**命中
//! （`mod.rs::BackendRegistry::dispatch`）——因此 e2e config 的
//! `[s1.profile] backends` **必须启用 `fault_inject` 并显式移除
//! `mock_dwell`**，使 DwellMs 分发唯一化（R3-B1；[`profile_conflict_note`]
//! 把这条写进类型旁，`fault_inject_and_mock_dwell_must_not_coexist` 单测锁）。
//!
//! ## 固定行为（无配置面）
//!
//! - 前 [`FAIL_AFTER_TICKS`] 次 `tick()` 返 `Continue`（不发新
//!   `JointCommandBody`，hold 由 coordinator 接管，同 `mock_dwell`）；
//! - **第 `FAIL_AFTER_TICKS + 1` 次** `tick()` 返
//!   `Err(ActionFailure::InternalError("fault_inject"))`——`InternalError`
//!   是既有枚举变体（`action.rs`），**0 新 variant**；coordinator 收到
//!   `InternalError` 会把自己打进 `Degraded`（AC-E18 failure 分支正是要这个）。
//! - `cancel()` 协作式：置 flag → 下次 `tick()` 先返
//!   `Completed(PartialSuccess { warning: "cancelled" })`（**cancel 优先于
//!   failure**，让"提交期部分失败 → cancel 已接收侧"路径在 fault 侧也可用）。
//!
//! 无 `duration_ms` / 无 payload / 无环境变量——行为完全由常量决定，e2e 判据
//! 可确定性断言（Spec R3-B1"`fail_after_ticks = 3` 固定"）。
//!
//! ## feature 守门
//!
//! 整文件 `#[cfg(feature = "test-fault-backend")]` gate（`mod.rs`），
//! **永不进 `default` features**；生产 `s1_daemon` 不 link 本文件 →
//! `BackendRegistry::from_inventory` 收集不到 `fault_inject` →
//! `[s1.profile.backends = ["fault_inject"]]` 在
//! `validate_profile_section` 启动期 `UnknownBackend` fail-fast（与
//! `backend-*` feature 同款语义）。e2e 前显式：
//!
//! ```bash
//! cargo build --release -p robot-s1 --bin s1_daemon --features test-fault-backend
//! ```
//!
//! （P.0 活清单条目；漏编 = e2e bring-up 时 s1 exit 3。）

#![allow(clippy::doc_markdown)]

use crate::action::{
    ActionBackend, ActionCancelError, ActionContext, ActionFailure, ActionInstance, ActionKind,
    ActionOutcome, ActionRequest, ActionStartError, ActionTickOutput, NO_PAYLOAD_SCHEMA_ID,
};

/// 连续返 `Continue` 的 tick 数；**第 `FAIL_AFTER_TICKS + 1` 次** tick 失败
/// （Spec R3-B1 固定值，无配置面）。
pub const FAIL_AFTER_TICKS: u32 = 3;

/// `ActionFailure::InternalError` 的固定 message（e2e 断言用）。
pub const FAILURE_MESSAGE: &str = "fault_inject";

/// 与 `fault_inject` 分发键完全重合、必须在 profile 中被移除的 backend 名
/// （R3-B1 冲突消除；见模块 doc"冲突消除"）。
pub const CONFLICTING_BACKEND: &str = "mock_dwell";

/// profile 冲突提示串——e2e / runbook 直接引用，避免各处复述走样。
#[must_use]
pub fn profile_conflict_note() -> String {
    format!(
        "[s1.profile] backends 必须含 \"{}\" 且**不含** \"{CONFLICTING_BACKEND}\"\
         （两者 (DwellMs, NO_PAYLOAD_SCHEMA_ID) 分发键重合，dispatch 取首个命中）",
        FaultInjectBackend.name_str()
    )
}

// ============================================================================
// FaultInjectBackend
// ============================================================================

/// test-only backend：N tick 后必失败。字段为空——行为全由常量决定。
#[derive(Debug, Clone, Copy)]
pub struct FaultInjectBackend;

impl FaultInjectBackend {
    /// `'static` 名字（`profile_conflict_note` 等非 trait 路径用；trait
    /// `name()` 签名是 elided lifetime，不能直接返 `&'static str`）。
    #[must_use]
    pub const fn name_str(self) -> &'static str {
        "fault_inject"
    }
}

// `name()` / `supported_kinds()` / `supported_schemas()` 返回 'static 字面量，
// 但 trait 签名是 elided lifetime；clippy `unnecessary_literal_bound` 的建议
// 与 trait 签名冲突（沿 mock_dwell 同款 allow）。
#[allow(clippy::unnecessary_literal_bound)]
impl ActionBackend for FaultInjectBackend {
    fn name(&self) -> &str {
        self.name_str()
    }

    fn supported_kinds(&self) -> &[ActionKind] {
        // 既有 wire 键，0 新增（R3-B1）。
        const KINDS: &[ActionKind] = &[ActionKind::DwellMs];
        KINDS
    }

    fn supported_schemas(&self) -> &[u32] {
        // 显式哨兵，不空切片（沿 mock_dwell / reviewer B2 阻断 1）。
        const SCHEMAS: &[u32] = &[NO_PAYLOAD_SCHEMA_ID];
        SCHEMAS
    }

    fn start(
        &self,
        req: &ActionRequest,
        _ctx: &ActionContext<'_>,
    ) -> Result<Box<dyn ActionInstance>, ActionStartError> {
        // `ActionRequest::validate` 已守门 NO_PAYLOAD_SCHEMA_ID ↔ payload 空
        // 一致性；这里再做一次防御性检查（沿 mock_dwell），让"传错 payload"
        // 有清晰的 backend 侧错误路径。
        if !req.payload.is_empty() {
            return Err(ActionStartError::Rejected(format!(
                "FaultInjectBackend expects empty payload, got {} bytes",
                req.payload.len()
            )));
        }
        Ok(Box::new(FaultInjectInstance {
            ticks_done: 0,
            cancelled: false,
        }))
    }
}

// ============================================================================
// FaultInjectInstance
// ============================================================================

/// per-action 实例：数 tick，到点返 `Err`。
struct FaultInjectInstance {
    /// 已返回 `Continue` 的 tick 数。
    ticks_done: u32,
    /// `cancel()` 置位；下次 tick **优先**收敛成 `PartialSuccess`。
    cancelled: bool,
}

impl ActionInstance for FaultInjectInstance {
    fn tick(&mut self, _ctx: &mut ActionContext<'_>) -> Result<ActionTickOutput, ActionFailure> {
        if self.cancelled {
            // cancel 优先于 failure：让"cancel 已接收侧"路径在 fault 侧同样
            // 可用（AC-E14 提交期部分失败）。语义沿 mock_dwell：coordinator
            // 把 PartialSuccess 转 ActionEvent::Cancelled。
            return Ok(ActionTickOutput::Completed(ActionOutcome::PartialSuccess {
                warning: "cancelled".to_string(),
            }));
        }
        if self.ticks_done >= FAIL_AFTER_TICKS {
            return Err(ActionFailure::InternalError(FAILURE_MESSAGE.to_string()));
        }
        self.ticks_done += 1;
        // progress 只作诊断：ticks_done / FAIL_AFTER_TICKS，恒 ≤ 1.0。
        #[allow(clippy::cast_precision_loss)]
        let progress = self.ticks_done as f32 / FAIL_AFTER_TICKS as f32;
        Ok(ActionTickOutput::Continue {
            progress,
            command: None,
        })
    }

    fn cancel(&mut self, _ctx: &mut ActionContext<'_>) -> Result<(), ActionCancelError> {
        self.cancelled = true;
        Ok(())
    }
}

// ============================================================================
// inventory 注册
// ============================================================================

/// inventory 静态实例（整文件已被 `test-fault-backend` feature 守门，
/// 生产 binary 不 link → registry 收集不到 `fault_inject`）。
static FAULT_INJECT_BACKEND: FaultInjectBackend = FaultInjectBackend;

inventory::submit! {
    &FAULT_INJECT_BACKEND as &'static dyn ActionBackend
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use crate::action::{ActionId, ActionRequest, ActionSource};
    use crate::action_backend::BackendRegistry;
    use crate::coordinator::{CoordinatorConfig, HardwareSection};
    use robot_core::time::{MockClock, MonoRawNs};

    fn request() -> ActionRequest {
        ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::DwellMs,
            deadline_ns: u64::MAX,
            priority: 10,
            payload_schema_id: NO_PAYLOAD_SCHEMA_ID,
            payload: Vec::new(),
            idempotency_key: "fault-inject-test".into(),
            source: ActionSource::Internal,
        }
    }

    fn hardware() -> HardwareSection {
        HardwareSection {
            id: "fault_arm".into(),
            joints: vec!["j0".into()],
            motor_ids: vec![1],
        }
    }

    /// 建一个最小 `ActionContext`：本 backend 只用到 ctx 的生命周期，不读
    /// clock / anchor（行为纯 tick 计数）。
    fn with_ctx<R>(f: impl FnOnce(&mut ActionContext<'_>) -> R) -> R {
        let clock = MockClock::new(MonoRawNs(0));
        let config = CoordinatorConfig::default();
        let hw = hardware();
        let anchor = vec![0.0_f64];
        let mut ctx = ActionContext {
            clock: &clock,
            layout_id: 0xABCD_0001,
            hardware: &hw,
            config: &config,
            anchor_bias: &anchor,
            scripted_start_pose: None,
            command_seq: 0,
            solver: None,
        };
        f(&mut ctx)
    }

    #[test]
    fn dispatch_keys_are_existing_wire_values() {
        let b = FaultInjectBackend;
        assert_eq!(b.name(), "fault_inject");
        assert_eq!(b.supported_kinds(), &[ActionKind::DwellMs]);
        assert_eq!(b.supported_schemas(), &[NO_PAYLOAD_SCHEMA_ID]);
    }

    #[test]
    fn continues_for_fail_after_ticks_then_fails_with_internal_error() {
        let mut inst = with_ctx(|ctx| FaultInjectBackend.start(&request(), ctx).expect("start ok"));
        with_ctx(|ctx| {
            for i in 1..=FAIL_AFTER_TICKS {
                match inst.tick(ctx).expect("前 FAIL_AFTER_TICKS 次必 Continue") {
                    ActionTickOutput::Continue { progress, command } => {
                        assert!(command.is_none(), "tick {i} 不得发新命令帧");
                        assert!(
                            (0.0..=1.0).contains(&progress),
                            "tick {i} progress {progress}"
                        );
                    }
                    ActionTickOutput::Completed(o) => {
                        panic!("tick {i} 期望 Continue，得 Completed({o:?})")
                    }
                }
            }
            // 第 FAIL_AFTER_TICKS + 1 次：失败。
            let err = inst.tick(ctx).expect_err("到点必 Err");
            match err {
                ActionFailure::InternalError(msg) => assert_eq!(msg, FAILURE_MESSAGE),
                other => panic!("期望 InternalError(\"{FAILURE_MESSAGE}\")，得 {other:?}"),
            }
            // 失败后再 tick 仍失败（幂等，不会意外 Completed）。
            assert!(matches!(
                inst.tick(ctx),
                Err(ActionFailure::InternalError(_))
            ));
        });
    }

    #[test]
    fn cancel_takes_precedence_over_pending_failure() {
        let mut inst = with_ctx(|ctx| FaultInjectBackend.start(&request(), ctx).expect("start"));
        with_ctx(|ctx| {
            // 先跑满 FAIL_AFTER_TICKS（下一次本应失败），再 cancel。
            for _ in 0..FAIL_AFTER_TICKS {
                let _ = inst.tick(ctx).expect("Continue");
            }
            inst.cancel(ctx).expect("cancel ok");
            match inst.tick(ctx).expect("cancel 后应收敛而非 Err") {
                ActionTickOutput::Completed(ActionOutcome::PartialSuccess { warning }) => {
                    assert_eq!(warning, "cancelled");
                }
                ActionTickOutput::Completed(o) => {
                    panic!("期望 PartialSuccess(cancelled)，得 Completed({o:?})")
                }
                ActionTickOutput::Continue { .. } => panic!("cancel 后不得再 Continue"),
            }
        });
    }

    #[test]
    fn non_empty_payload_is_rejected() {
        let mut req = request();
        req.payload = vec![1, 2, 3];
        let err = with_ctx(|ctx| {
            FaultInjectBackend
                .start(&req, ctx)
                .err()
                .expect("payload 非空必拒")
        });
        assert!(matches!(err, ActionStartError::Rejected(ref m) if m.contains("empty payload")));
    }

    #[test]
    fn successful_hold_target_defaults_to_none() {
        // 本 backend 永不 Success；即便被问也必须 reset 回 anchor（D3 穷尽规则）。
        let inst = with_ctx(|ctx| FaultInjectBackend.start(&request(), ctx).expect("start"));
        assert!(inst.successful_hold_target().is_none());
    }

    #[test]
    fn fault_inject_and_mock_dwell_must_not_coexist() {
        // R3-B1 冲突消除的可执行证据：两者分发键完全重合，同时 enable 时
        // dispatch 取 enabled 列表里的首个命中——e2e config 必须移除
        // mock_dwell，否则 DwellMs 会落到 mock_dwell（100 ms 后 Success），
        // failure 分支永不触发。
        let fault: &'static dyn ActionBackend = &FAULT_INJECT_BACKEND;
        assert_eq!(
            fault.supported_kinds(),
            &[ActionKind::DwellMs],
            "分发键与 mock_dwell 重合是有意的（0 新 wire）"
        );
        let r = BackendRegistry::from_explicit(&[fault]);
        let enabled = vec!["fault_inject".to_string()];
        let b = r
            .dispatch(&enabled, ActionKind::DwellMs, NO_PAYLOAD_SCHEMA_ID)
            .expect("单独启用时 DwellMs 唯一落到 fault_inject");
        assert_eq!(b.name(), "fault_inject");
        assert!(profile_conflict_note().contains(CONFLICTING_BACKEND));
    }

    #[test]
    fn registered_in_inventory_when_feature_enabled() {
        // 本文件被编译 = feature 打开 = inventory 必须收得到（e2e 依赖它）。
        let r = BackendRegistry::from_inventory();
        assert!(
            r.has("fault_inject"),
            "test-fault-backend feature 下 inventory 必须含 fault_inject；实得 {:?}",
            r.names().collect::<Vec<_>>()
        );
    }
}
