//! v0.3 B2 (3/3) in-process integration test for action dispatch wiring。
//!
//! 跑通"profile parse + L2 validate → BackendRegistry → Coordinator →
//! `run_action_tick_loop` state-aware dispatch"端到端流。**不**起子进程 /
//! 不接 iceoryx2 真服务，全走 [`MockChannel`] + [`MockClock`]。
//!
//! ## 10 路径覆盖（B2 (3/3) reviewer 验收契约 5 + override 二分 + 阻断 2
//!     + B1 回归 baseline + B3 (2/2) HoldPose happy + cancel monotonicity）
//!
//! 1. `happy_a_mock_dwell_none_override_blocks_hold_loop_cmd`：
//!    MockDwell 长 dwell + override 策略 → tick_action 返 None →
//!    cmd_ch.publishes() **不**递增；hb_ch 仍 50 Hz（reviewer 评审口径
//!    "hold loop 不再发 B1 hold command"）。
//! 2. `happy_b_some_command_backend_publishes_through_override`：
//!    ScriptedSomeBackend 每 tick 返 Some(body) → cmd_ch.publishes()
//!    **递增**（reviewer 评审口径"action command 仍能 publish"）+ body
//!    bytes 与 backend 自产一致。
//! 3. `baseline_no_submit_idle_loop_publishes_b1_hold_at_50hz`：缺失
//!    submit 走 Idle → run_action_tick_loop 跟 B1 sample-by-sample 等
//!    价（reviewer 阻断 4 "外部可观测 no-active-action 行为必须等价
//!    B1"硬约束）。
//! 4. `cancel_during_executing_emits_cancelled_then_idle_resumes`：直接
//!    通过 Coordinator API + tick_action 驱动；submit + tick_action +
//!    cancel + tick_action → emit Cancelled + Idle 恢复。
//! 5. `safety_abort_transitions_to_degraded_then_clear_error_resumes_idle`：
//!    submit SafetyAbortBackend → tick_action 转 Degraded → 后续 submit
//!    被 CoordinatorDegraded 拒 → clear_error → 回 Idle。
//! 6. `profile_validation_unknown_backend_fails_fast`：未注册 backend
//!    name → ProfileError::UnknownBackend。
//! 7. `profile_validation_no_enabled_backends_fails_fast`：空 backends
//!    列表 → ProfileError::NoEnabledBackends（reviewer 阻断 2 验收）。
//! 8. `profile_parse_missing_section_yields_default_b2_passes_validation`：
//!    缺失 `[s1.profile]` → default_b2 = `["mock_dwell"]` → validate
//!    通过（regression baseline，超 reviewer 5 路径但锁住"现有 config
//!    不带 `[s1.profile]` 部署不会被 B2 (3/3) wiring 打断"）。
//! 9. `happy_hold_pose_publishes_hold_cmd_bit_equivalent_through_override`
//!    （B3 (2/2)）：submit `HoldPoseBackend` → 100ms loop →
//!    cmd_ch.publishes() ≥ 4 + body 跟同源参数调 `build_hold_command`
//!    构造的 ground truth 字节级相等（reviewer round 2 阻断 1 +
//!    bit-equivalent 收紧验收）。
//! 10. `cancel_hold_pose_during_executing_emits_cancelled_then_idle_resumes_hold`
//!     （B3 (2/2)，含 reviewer 评审重点 cross-segment monotonicity）：
//!     Idle hold (Phase 1) → submit HoldPose Executing (Phase 2) →
//!     cancel (Phase 3) → Idle hold resume；3 段后取 cmd_ch.latest()
//!     source_task_id 严格递增 + emit Cancelled + final state Idle。
//!
//! ## 为什么 cancel / safety_abort 不走 run_action_tick_loop
//!
//! `run_action_tick_loop` 是 blocking 单调推进调用；测试要在 loop 中途
//! 触发 cancel / 状态切换需要 multi-thread + 同步原语，复杂度高且容易
//! 引入 race-test。直接调 [`Coordinator::tick_action`] 单 step 模拟比 loop
//! 包装更适合测状态机收敛；`run_action_tick_loop` 自身 happy 路径由 1/2/3
//! 三条覆盖。

#![allow(clippy::doc_markdown)]

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use robot_core::time::{MockClock, MonoRawNs, RobotClock};
use robot_msg::markers;
use robot_msg::msg::{ControlMode, JointCommand as FbJointCommand};
use robot_s1::action::{
    ActionBackend, ActionCancelError, ActionContext, ActionEvent, ActionFailure, ActionId,
    ActionInstance, ActionKind, ActionRequest, ActionSource, ActionStartError, ActionStatus,
    ActionTickOutput, JointCommandBody, NO_PAYLOAD_SCHEMA_ID,
};
use robot_s1::action_backend::BackendRegistry;
use robot_s1::action_backend::hold_pose::HoldPoseBackend;
use robot_s1::action_backend::mock_dwell::MockDwellBackend;
use robot_s1::coordinator::build_hold_command;
use robot_s1::coordinator::{
    Coordinator, CoordinatorConfig, CoordinatorState, HardwareSection, ProfileError,
    ProfileSection, parse_profile_section_from_toml, run_action_tick_loop,
    validate_profile_section,
};
use robot_transport::mock::MockChannel;
use robot_transport::traits::LatestSubscriber;

const LAYOUT_ID: u64 = 0xCAFE_BABE_DEAD_BEEF;

// ============================================================================
// 测试用 backend：ScriptedSomeBackend（override 期间持续 publish 自产 body）
// ============================================================================

/// Backend 每 tick 返 `Continue { command: Some(body) }`，不主动完成 ——
/// 用于测 override 路径下 action backend 的 cmd 能否 publish 到
/// cmd_ch。Body 用 ScriptedSomeInstance 构造时存的 marker，方便测试断
/// 言。
struct ScriptedSomeBackend;

// `name()` 等返回 'static 字面量，但 trait 签名是 elided lifetime；
// clippy `unnecessary_literal_bound` 建议改 `&'static str` 与 trait 签名
// 冲突，跟 mock_dwell 同款 allow。
#[allow(clippy::unnecessary_literal_bound)]
impl ActionBackend for ScriptedSomeBackend {
    fn name(&self) -> &str {
        "scripted_some"
    }
    fn supported_kinds(&self) -> &[ActionKind] {
        const KINDS: &[ActionKind] = &[ActionKind::DwellMs];
        KINDS
    }
    fn supported_schemas(&self) -> &[u32] {
        const SCHEMAS: &[u32] = &[NO_PAYLOAD_SCHEMA_ID];
        SCHEMAS
    }
    fn start(
        &self,
        _req: &ActionRequest,
        _ctx: &ActionContext<'_>,
    ) -> Result<Box<dyn ActionInstance>, ActionStartError> {
        Ok(Box::new(ScriptedSomeInstance))
    }
}

struct ScriptedSomeInstance;

impl ActionInstance for ScriptedSomeInstance {
    fn tick(&mut self, _ctx: &mut ActionContext<'_>) -> Result<ActionTickOutput, ActionFailure> {
        // 持续 publish 自产 body bytes（含一个可识别的 marker tag，方便
        // 测试在 cmd_ch.latest() 取 body 时验。Body 用 build_joint_command
        // 构造一帧合法 JointCommand（target=[1.23] 标识 ScriptedSome 来
        // 源）让 fbs verify 能过。
        let body = robot_msg::build_helpers::build_joint_command(
            LAYOUT_ID,
            ControlMode::Position,
            &[1.23], // marker target
            /* deadline_ns */ u64::MAX,
            /* priority */ 99, // marker priority 区别于 hold cmd 的 10
            robot_msg::msg::StaleAction::Hold,
            /* source_task_id */ 0,
        );
        Ok(ActionTickOutput::Continue {
            progress: 0.5,
            command: Some(JointCommandBody::new(body)),
        })
    }
    fn cancel(&mut self, _ctx: &mut ActionContext<'_>) -> Result<(), ActionCancelError> {
        Ok(())
    }
}

static SCRIPTED_SOME_BACKEND: ScriptedSomeBackend = ScriptedSomeBackend;

// ============================================================================
// 测试用 backend：SafetyAbortBackend（首次 tick 即 Failed(SafetyAbort)）
// ============================================================================

struct SafetyAbortBackend;

#[allow(clippy::unnecessary_literal_bound)]
impl ActionBackend for SafetyAbortBackend {
    fn name(&self) -> &str {
        "safety_abort"
    }
    fn supported_kinds(&self) -> &[ActionKind] {
        const KINDS: &[ActionKind] = &[ActionKind::DwellMs];
        KINDS
    }
    fn supported_schemas(&self) -> &[u32] {
        const SCHEMAS: &[u32] = &[NO_PAYLOAD_SCHEMA_ID];
        SCHEMAS
    }
    fn start(
        &self,
        _req: &ActionRequest,
        _ctx: &ActionContext<'_>,
    ) -> Result<Box<dyn ActionInstance>, ActionStartError> {
        Ok(Box::new(SafetyAbortInstance))
    }
}

struct SafetyAbortInstance;

impl ActionInstance for SafetyAbortInstance {
    fn tick(&mut self, _ctx: &mut ActionContext<'_>) -> Result<ActionTickOutput, ActionFailure> {
        // 首次 tick 即 SafetyAbort —— 验 coordinator 转 Degraded 路径。
        Err(ActionFailure::SafetyAbort("integration test abort".into()))
    }
    fn cancel(&mut self, _ctx: &mut ActionContext<'_>) -> Result<(), ActionCancelError> {
        Ok(())
    }
}

static SAFETY_ABORT_BACKEND: SafetyAbortBackend = SafetyAbortBackend;

// ============================================================================
// helpers
// ============================================================================

/// 构造 1-joint coordinator，用指定 registry + enabled backend 列表。
fn make_coordinator(registry: BackendRegistry, enabled: Vec<String>) -> Coordinator {
    let cfg = CoordinatorConfig::default();
    let hardware = HardwareSection {
        id: "test_arm".into(),
        joints: vec!["j0".into()],
    };
    let anchor_bias = vec![0.0_f64];
    Coordinator::new(
        cfg,
        hardware,
        LAYOUT_ID,
        anchor_bias,
        registry,
        enabled,
        /* solver */ None,
    )
}

/// 构造一个 registry 含 MockDwellBackend with 自定义 duration_ms（leak 到
/// `'static`，进程退出时 OS 回收）。
fn registry_with_mock_dwell_duration_ms(duration_ms: u64) -> BackendRegistry {
    let backend = Box::leak(Box::new(MockDwellBackend::with_duration_ms(duration_ms)));
    BackendRegistry::from_explicit(&[backend as &'static dyn ActionBackend])
}

/// 构造一个 registry 含 default-100ms MockDwell + 自定义额外 backend。
fn registry_with_mock_dwell_and(extra: &'static dyn ActionBackend) -> BackendRegistry {
    let mock = Box::leak(Box::new(MockDwellBackend::with_duration_ms(100)));
    BackendRegistry::from_explicit(&[mock as &'static dyn ActionBackend, extra])
}

/// 构造一个 ActionRequest（远期 deadline，过 validate 守门）。
fn sample_request_for(name_hint: &str) -> ActionRequest {
    ActionRequest {
        action_id: ActionId::new_v7(),
        kind: ActionKind::DwellMs,
        deadline_ns: u64::MAX,
        priority: 10,
        payload_schema_id: NO_PAYLOAD_SCHEMA_ID,
        payload: Vec::new(),
        idempotency_key: format!("test-key-{name_hint}"),
        source: ActionSource::Internal,
    }
}

/// 跑 100 ms 的 run_action_tick_loop（poll 1 ms / cmd 50 Hz cmd / hb 50 Hz）。
/// 返回最终 (cmd_publishes, hb_publishes, final_state)。MockChannel 的
/// `publishes()` 返 `u64`；这里用 `u64` 直接传出，断言用 `>=` / `==` 比
/// 较字面值即可。
fn run_loop_100ms(
    coordinator: &Coordinator,
    cmd_ch: &MockChannel<markers::JointCommand>,
    hb_ch: &MockChannel<markers::Heartbeat>,
    clock: &MockClock,
) -> (u64, u64, CoordinatorState) {
    let mut cmd_pub = cmd_ch.publisher();
    let mut hb_pub = hb_ch.publisher();
    let shutdown = AtomicBool::new(false);
    run_action_tick_loop(
        coordinator,
        &mut hb_pub,
        &mut cmd_pub,
        clock,
        Duration::from_millis(1),
        Some(Duration::from_millis(100)),
        &shutdown,
    )
    .expect("tick loop ok");
    (
        cmd_ch.publishes(),
        hb_ch.publishes(),
        coordinator.current_state(),
    )
}

// ============================================================================
// 1. happy_a：MockDwell None override 阻塞 hold loop（cmd_ch 不递增）
// ============================================================================

#[test]
fn happy_a_mock_dwell_none_override_blocks_hold_loop_cmd() {
    // MockDwell 1000ms duration > 100ms loop duration → 整段 100ms 都
    // 在 EXECUTING 状态，tick_action 返 None；override 策略下 hold loop
    // 不发 B1 hold cmd → cmd_ch.publishes() == 0。hb 仍 50 Hz 发。
    let registry = registry_with_mock_dwell_duration_ms(1000);
    let coordinator = make_coordinator(registry, vec!["mock_dwell".into()]);
    coordinator.mark_idle().expect("Booting → Idle");

    let clock = MockClock::new(MonoRawNs(1_000_000_000));
    coordinator
        .submit(sample_request_for("happy_a"), &clock)
        .expect("submit ok in Idle");
    assert!(matches!(
        coordinator.current_state(),
        CoordinatorState::Executing { .. }
    ));

    let cmd_ch = MockChannel::<markers::JointCommand>::new();
    let hb_ch = MockChannel::<markers::Heartbeat>::new();
    let (cmd_publishes, hb_publishes, final_state) =
        run_loop_100ms(&coordinator, &cmd_ch, &hb_ch, &clock);

    // cmd_ch 不递增（MockDwell 持续 None；override 阻塞 hold loop）
    assert_eq!(
        cmd_publishes, 0,
        "EXECUTING + MockDwell None: cmd_ch must NOT advance \
         (override 阻塞 hold loop；MockDwell 不产 cmd)"
    );
    // hb 50 Hz 不停（100ms / 20ms ≈ 4-5 拍）
    assert!(
        hb_publishes >= 4,
        "hb_ch should publish ≥4 in 100ms (50Hz): got {hb_publishes}"
    );
    // 状态仍 EXECUTING（dwell 未到）
    assert!(
        matches!(final_state, CoordinatorState::Executing { .. }),
        "still EXECUTING after 100ms (1000ms dwell): got {final_state:?}"
    );
}

// ============================================================================
// 2. happy_b：Some override → action backend publish 通过 cmd_ch
// ============================================================================

#[test]
fn happy_b_some_command_backend_publishes_through_override() {
    // ScriptedSomeBackend 每 tick 返 Some(body) → override 把 body 直
    // publish 到 cmd_ch。reviewer 评审口径："action command 仍能
    // publish"，证 hold loop 阻塞**不**误伤 backend 自产 cmd。
    let registry =
        BackendRegistry::from_explicit(&[&SCRIPTED_SOME_BACKEND as &'static dyn ActionBackend]);
    let coordinator = make_coordinator(registry, vec!["scripted_some".into()]);
    coordinator.mark_idle().expect("Booting → Idle");

    let clock = MockClock::new(MonoRawNs(1_000_000_000));
    coordinator
        .submit(sample_request_for("happy_b"), &clock)
        .expect("submit ok");

    let cmd_ch = MockChannel::<markers::JointCommand>::new();
    let hb_ch = MockChannel::<markers::Heartbeat>::new();
    let (cmd_publishes, hb_publishes, final_state) =
        run_loop_100ms(&coordinator, &cmd_ch, &hb_ch, &clock);

    // cmd_ch 递增（ScriptedSomeBackend 每 cmd-period 产 Some(body)）
    assert!(
        cmd_publishes >= 4,
        "EXECUTING + ScriptedSomeBackend Some: cmd_ch must advance ≥4 \
         (override **不** 误伤 action publish): got {cmd_publishes}"
    );
    assert!(
        hb_publishes >= 4,
        "hb_ch should publish ≥4: got {hb_publishes}"
    );

    // cmd_ch.latest() body 必须是 ScriptedSome 自产（target=[1.23]，
    // priority=99；与 B1 hold cmd 的 [0.0] / priority=10 不同），证不是
    // hold loop 误漏。
    let mut sub = cmd_ch.subscriber();
    let lease = sub.latest().expect("latest").expect("Some sample");
    let view = lease.view().expect("view");
    let body_bytes = view.body.raw();
    let cmd = flatbuffers::root::<FbJointCommand<'_>>(body_bytes).expect("verify");
    let target: Vec<f64> = cmd.target().expect("target").iter().collect();
    assert_eq!(
        target,
        vec![1.23],
        "cmd target must come from ScriptedSomeBackend, not hold loop"
    );
    assert_eq!(cmd.priority(), 99, "ScriptedSome priority marker");

    assert!(
        matches!(final_state, CoordinatorState::Executing { .. }),
        "still EXECUTING after 100ms: got {final_state:?}"
    );
}

// ============================================================================
// 3. baseline：no submit, Idle → B1 hold loop 等价（reviewer 阻断 4 锁定）
// ============================================================================

#[test]
fn baseline_no_submit_idle_loop_publishes_b1_hold_at_50hz() {
    // 无 submit，coordinator 永远 Idle → run_action_tick_loop 走 hold
    // 分支 = 调 build_hold_command + publish。外部可观测行为必须跟 B1
    // run_tick_loop sample-by-sample bit-equivalent（reviewer 阻断 4：
    // wiring 接入不能退化）。
    let registry = registry_with_mock_dwell_duration_ms(100);
    let coordinator = make_coordinator(registry, vec!["mock_dwell".into()]);
    coordinator.mark_idle().expect("Booting → Idle");

    let clock = MockClock::new(MonoRawNs(1_000_000_000));
    let cmd_ch = MockChannel::<markers::JointCommand>::new();
    let hb_ch = MockChannel::<markers::Heartbeat>::new();
    let (cmd_publishes, hb_publishes, final_state) =
        run_loop_100ms(&coordinator, &cmd_ch, &hb_ch, &clock);

    assert!(
        cmd_publishes >= 4,
        "Idle baseline: hold cmd 50Hz publish ≥4 in 100ms: got {cmd_publishes}"
    );
    assert!(
        hb_publishes >= 4,
        "Idle baseline: hb 50Hz publish ≥4 in 100ms: got {hb_publishes}"
    );
    assert!(
        matches!(final_state, CoordinatorState::Idle),
        "no submit → must stay Idle: got {final_state:?}"
    );

    // 验最新 cmd_ch body 的 target == anchor_bias[0] = 0.0（B1 hold
    // 行为，sample-by-sample 兼容）。
    let mut sub = cmd_ch.subscriber();
    let lease = sub.latest().expect("latest").expect("Some sample");
    let view = lease.view().expect("view");
    let cmd = flatbuffers::root::<FbJointCommand<'_>>(view.body.raw()).expect("verify");
    let target: Vec<f64> = cmd.target().expect("target").iter().collect();
    assert_eq!(target, vec![0.0], "B1 hold target == anchor_bias");
    assert_eq!(cmd.priority(), 10, "B1 default priority");
    assert_eq!(
        cmd.on_timeout(),
        robot_msg::msg::StaleAction::Hold,
        "B1 default on_timeout"
    );
}

// ============================================================================
// 4. cancel：直接 coord API 驱动；submit + tick_action + cancel + tick_action
// ============================================================================

#[test]
fn cancel_during_executing_emits_cancelled_then_idle_resumes() {
    // 直接驱动 coord.tick_action 模拟 tick；cancel 是 cooperative，需要
    // 看连续 2 个 tick 收敛行为；run_action_tick_loop blocking 不便分步
    // 触发，故走 lib API 直驱（B2 (3/3) 集成 = 走 binary wiring 那条路
    // 径已被 happy_a/b/3 覆盖）。
    let registry = registry_with_mock_dwell_duration_ms(1000);
    let coordinator = make_coordinator(registry, vec!["mock_dwell".into()]);
    coordinator.mark_idle().expect("Booting → Idle");

    let clock = MockClock::new(MonoRawNs(0));
    let ticket = coordinator
        .submit(sample_request_for("cancel"), &clock)
        .expect("submit ok");

    // 1. 第一 tick：Continue（MockDwell None）→ emit Progress。
    clock.advance(robot_core::time::DurationNs::from_nanos(20_000_000));
    let cmd = coordinator.tick_action(&clock, 0);
    assert!(cmd.is_none(), "MockDwell tick returns None");
    assert!(matches!(
        coordinator.current_state(),
        CoordinatorState::Executing { .. }
    ));

    // 2. cancel；状态保 Executing。
    coordinator
        .cancel(ticket.action_id, &clock)
        .expect("cancel ok");
    assert!(
        matches!(
            coordinator.current_state(),
            CoordinatorState::Executing { .. }
        ),
        "cancel 不立即终态；仍 Executing 等下次 tick 收敛"
    );

    // 3. 下次 tick：MockDwell 自查 cancel flag → Completed(PartialSuccess)；
    //    coordinator emit Cancelled（不双发 Completed），状态 → Idle。
    clock.advance(robot_core::time::DurationNs::from_nanos(20_000_000));
    let cmd = coordinator.tick_action(&clock, 0);
    assert!(cmd.is_none(), "Completed 帧不产 cmd body");
    assert!(matches!(
        coordinator.current_state(),
        CoordinatorState::Idle
    ));

    // 事件流：必须看到 Cancelled，**不**双发 Completed。
    let mut saw_cancelled = false;
    let mut saw_completed = false;
    for ev in ticket.events.try_iter() {
        match ev {
            ActionEvent::Cancelled { .. } => saw_cancelled = true,
            ActionEvent::Completed { .. } => saw_completed = true,
            _ => {}
        }
    }
    assert!(saw_cancelled, "应 emit Cancelled");
    assert!(!saw_completed, "**绝不**双发 Completed + Cancelled");

    // status() 查 finished ring：返 Cancelled。
    assert!(matches!(
        coordinator.status(ticket.action_id),
        Some(ActionStatus::Cancelled)
    ));
}

// ============================================================================
// 5. safety_abort → DEGRADED → clear_error → IDLE
// ============================================================================

#[test]
fn safety_abort_transitions_to_degraded_then_clear_error_resumes_idle() {
    let registry = registry_with_mock_dwell_and(&SAFETY_ABORT_BACKEND);
    let coordinator = make_coordinator(
        registry,
        // safety_abort backend 优先在 dispatch 中匹配
        vec!["safety_abort".into(), "mock_dwell".into()],
    );
    coordinator.mark_idle().expect("Booting → Idle");

    let clock = MockClock::new(MonoRawNs(0));
    let ticket = coordinator
        .submit(sample_request_for("safety_abort"), &clock)
        .expect("submit ok");

    // 第一 tick → Failed(SafetyAbort) → Degraded { reason: "..." }。
    clock.advance(robot_core::time::DurationNs::from_nanos(20_000_000));
    let _ = coordinator.tick_action(&clock, 0);
    match coordinator.current_state() {
        CoordinatorState::Degraded { reason } => {
            assert_eq!(reason, "integration test abort");
        }
        other => panic!("expected Degraded, got {other:?}"),
    }

    // 事件流应见 Failed。
    let mut saw_failed = false;
    for ev in ticket.events.try_iter() {
        if matches!(ev, ActionEvent::Failed { .. }) {
            saw_failed = true;
        }
    }
    assert!(saw_failed, "应 emit Failed");

    // 后续 submit 应被 CoordinatorDegraded 拒。
    let err = coordinator
        .submit(sample_request_for("post-degraded"), &clock)
        .expect_err("submit in Degraded must be rejected");
    assert!(matches!(
        err,
        robot_s1::action::SubmitError::CoordinatorDegraded
    ));

    // clear_error → 回 Idle。
    coordinator.clear_error().expect("Degraded → Idle ok");
    assert!(matches!(
        coordinator.current_state(),
        CoordinatorState::Idle
    ));

    // 恢复后 submit 应该能成功（接受新 action）。
    coordinator
        .submit(sample_request_for("post-clear"), &clock)
        .expect("submit OK after clear_error → Idle");
    assert!(matches!(
        coordinator.current_state(),
        CoordinatorState::Executing { .. }
    ));
}

// ============================================================================
// 6. profile validation：unknown backend → fail-fast
// ============================================================================

#[test]
fn profile_validation_unknown_backend_fails_fast() {
    let toml = "
[s1.profile]
backends     = [\"does_not_exist\"]
command_mode = \"joint_position\"
";
    let profile = parse_profile_section_from_toml(toml).expect("L1 ok");

    // registry 里只有 mock_dwell；does_not_exist 应返 UnknownBackend。
    let registry = registry_with_mock_dwell_duration_ms(100);
    let hardware = HardwareSection {
        id: "test_arm".into(),
        joints: vec!["j0".into()],
    };
    let err = validate_profile_section(&profile, &registry, &hardware, 1)
        .expect_err("expected UnknownBackend");
    match err {
        ProfileError::UnknownBackend { name } => {
            assert_eq!(name, "does_not_exist");
        }
        other => panic!("expected UnknownBackend, got {other:?}"),
    }
}

// ============================================================================
// 7. profile validation：no enabled backends → fail-fast
// ============================================================================

#[test]
fn profile_validation_no_enabled_backends_fails_fast() {
    // [s1.profile] 显式存在但 backends 缺失 → parse 给 [] → validate 必拦。
    let toml = "[s1.profile]\n";
    let profile = parse_profile_section_from_toml(toml).expect("L1 ok");
    assert!(profile.backends.is_empty());

    let registry = registry_with_mock_dwell_duration_ms(100);
    let hardware = HardwareSection {
        id: "test_arm".into(),
        joints: vec!["j0".into()],
    };
    let err = validate_profile_section(&profile, &registry, &hardware, 1)
        .expect_err("expected NoEnabledBackends");
    assert!(
        matches!(err, ProfileError::NoEnabledBackends),
        "got {err:?}"
    );
}

// ============================================================================
// 8. profile parse + default_b2 路径不破坏（regression baseline）
// ============================================================================

#[test]
fn profile_parse_missing_section_yields_default_b2_passes_validation() {
    // 缺失 [s1.profile] → default_b2 = ["mock_dwell"]；validate 应过。
    // 这条不在 reviewer 5 路径里，但是 wiring 的 critical regression
    // baseline —— 现有 config（不带 [s1.profile]）必须继续 work。
    let toml = "[s1]\nheartbeat_hz = 50.0\n";
    let profile = parse_profile_section_from_toml(toml).expect("L1 ok");
    assert_eq!(profile, ProfileSection::default_b2());

    let registry = registry_with_mock_dwell_duration_ms(100);
    let hardware = HardwareSection {
        id: "test_arm".into(),
        joints: vec!["j0".into()],
    };
    validate_profile_section(&profile, &registry, &hardware, 1)
        .expect("default_b2 must pass validate (B1 config compat)");
}

// ============================================================================
// B3 (2/2) HoldPoseBackend integration tests
// ============================================================================
//
// 跟 B2 (3/3) `happy_b_some_command_backend_publishes_through_override` 的
// 区别：本段用**真** `HoldPoseBackend`（B3 (1/2) 落地的第一个真 backend），
// 跑通 ActionContext.command_seq → build_hold_command → publish
// 端到端字节级 ground truth 对齐 + cross-segment cmd_seq monotonicity
// （reviewer round 2 阻断 1 修法 + 评审重点）。

/// 测试用 HoldPoseBackend 静态实例（不依赖 lib 内部 `HOLD_POSE_BACKEND`
/// 全局，避免 inventory 全局污染影响 from_explicit 行为）。
static TEST_HOLD_POSE_BACKEND: HoldPoseBackend = HoldPoseBackend;

/// 构造 1-joint coordinator + hold_pose-only registry。anchor_bias 用
/// 一个**非零**值方便后面 ground truth 比对时区别于"全 0 默认"。
fn make_hold_pose_coordinator(anchor_bias: Vec<f64>) -> Coordinator {
    let cfg = CoordinatorConfig::default();
    let hardware = HardwareSection {
        id: "test_arm".into(),
        joints: vec!["j0".into()],
    };
    let registry =
        BackendRegistry::from_explicit(&[&TEST_HOLD_POSE_BACKEND as &'static dyn ActionBackend]);
    Coordinator::new(
        cfg,
        hardware,
        LAYOUT_ID,
        anchor_bias,
        registry,
        vec!["hold_pose".into()],
        /* solver */ None,
    )
}

/// 构造一个 ActionRequest（kind = HoldPose；远期 deadline）。
fn hold_pose_request(name_hint: &str) -> ActionRequest {
    ActionRequest {
        action_id: ActionId::new_v7(),
        kind: ActionKind::HoldPose,
        deadline_ns: u64::MAX,
        priority: 10,
        payload_schema_id: NO_PAYLOAD_SCHEMA_ID,
        payload: Vec::new(),
        idempotency_key: format!("test-hold-pose-{name_hint}"),
        source: ActionSource::Internal,
    }
}

/// 取 cmd_ch.latest 的 body bytes + 解码 source_task_id / deadline_ns
/// 反推 publish 时的 now_ns（= deadline_ns - cfg.deadline_offset_ns）。
/// 返 (body_bytes, source_task_id, now_ns_at_publish)。
fn read_latest_cmd(
    cmd_ch: &MockChannel<markers::JointCommand>,
    cfg: &CoordinatorConfig,
) -> (Vec<u8>, u64, u64) {
    let mut sub = cmd_ch.subscriber();
    let lease = sub.latest().expect("latest").expect("Some sample");
    let view = lease.view().expect("view");
    let body = view.body.raw().to_vec();
    let cmd = flatbuffers::root::<FbJointCommand<'_>>(&body).expect("fbs verify");
    let seq = cmd.source_task_id();
    let now_ns = cmd.deadline_ns() - cfg.deadline_offset_ns();
    (body, seq, now_ns)
}

/// 测试内单步 publish 模拟（cancel_hold_pose monotonicity 测专用）：
/// 按 `run_action_tick_loop` 的 cmd-publish 分支语义产 body bytes +
/// 推 cmd_seq。返 publish 时使用的 seq；`None` 表示本帧不 publish
/// （Executing tick_action 返 None / Booting / ShuttingDown）。
///
/// 同时做字节级 ground truth 对齐验证：Idle 路径 / Executing 路径产
/// 的 body source_task_id 必须 == 调用方传入的 cmd_seq —— 锁住
/// reviewer round 2 阻断 1 修法（HoldPose body 用 ctx.command_seq；
/// Idle hold 同款）。
fn try_publish_step(
    coordinator: &Coordinator,
    clock: &MockClock,
    cfg: &CoordinatorConfig,
    layout_id: u64,
    bias: &[f64],
    cmd_seq: &mut u64,
) -> Option<u64> {
    let now_ns = clock.now().0;
    match coordinator.current_state() {
        CoordinatorState::Idle | CoordinatorState::Degraded { .. } => {
            let body = build_hold_command(cfg, layout_id, bias, *cmd_seq, now_ns);
            let parsed =
                flatbuffers::root::<FbJointCommand<'_>>(&body).expect("Idle body fbs verify");
            assert_eq!(
                parsed.source_task_id(),
                *cmd_seq,
                "Idle hold body source_task_id 必须 == cmd_seq"
            );
            let used = *cmd_seq;
            *cmd_seq = cmd_seq.wrapping_add(1);
            Some(used)
        }
        CoordinatorState::Executing { .. } => {
            if let Some(body_struct) = coordinator.tick_action(clock, *cmd_seq) {
                let parsed = flatbuffers::root::<FbJointCommand<'_>>(body_struct.as_bytes())
                    .expect("Executing body fbs verify");
                assert_eq!(
                    parsed.source_task_id(),
                    *cmd_seq,
                    "HoldPose body source_task_id 必须 == ctx.command_seq \
                     (reviewer round 2 阻断 1 修法核心断言)"
                );
                let used = *cmd_seq;
                *cmd_seq = cmd_seq.wrapping_add(1);
                Some(used)
            } else {
                None
            }
        }
        CoordinatorState::Booting | CoordinatorState::ShuttingDown => None,
    }
}

/// 跑 run_action_tick_loop 给定 duration（ms），用同一组 hb_pub /
/// cmd_pub —— 跨段调用复用同一对，让 cmd_ch.publishes() 累加。
fn run_loop_ms(
    coordinator: &Coordinator,
    cmd_pub: &mut robot_transport::mock::MockPublisher<markers::JointCommand>,
    hb_pub: &mut robot_transport::mock::MockPublisher<markers::Heartbeat>,
    clock: &MockClock,
    duration_ms: u64,
) -> CoordinatorState {
    let shutdown = AtomicBool::new(false);
    run_action_tick_loop(
        coordinator,
        hb_pub,
        cmd_pub,
        clock,
        Duration::from_millis(1),
        Some(Duration::from_millis(duration_ms)),
        &shutdown,
    )
    .expect("tick loop ok");
    coordinator.current_state()
}

// ============================================================================
// 9. happy_hold_pose：HoldPose 走 override 路径，body bit-equivalent
//    to build_hold_command ground truth（reviewer round 2 阻断 1 +
//    bit-equivalent 收紧验收）
// ============================================================================

#[test]
fn happy_hold_pose_publishes_hold_cmd_bit_equivalent_through_override() {
    // 用非零 anchor_bias 让 ground truth 比对更"实"（非全 0）。
    let anchor_bias = vec![0.42_f64];
    let coordinator = make_hold_pose_coordinator(anchor_bias.clone());
    coordinator.mark_idle().expect("Booting → Idle");

    let clock = MockClock::new(MonoRawNs(1_000_000_000));

    // submit HoldPose 让 coordinator 进 Executing。
    coordinator
        .submit(hold_pose_request("happy"), &clock)
        .expect("submit ok");
    assert!(matches!(
        coordinator.current_state(),
        CoordinatorState::Executing { .. }
    ));

    let cmd_ch = MockChannel::<markers::JointCommand>::new();
    let hb_ch = MockChannel::<markers::Heartbeat>::new();
    let mut cmd_pub = cmd_ch.publisher();
    let mut hb_pub = hb_ch.publisher();
    let final_state = run_loop_ms(&coordinator, &mut cmd_pub, &mut hb_pub, &clock, 100);

    // 100ms loop / 20ms cmd_period ≈ 4-5 publish；hb 同款。
    assert!(
        cmd_ch.publishes() >= 4,
        "EXECUTING + HoldPose Some(body): cmd_ch ≥ 4 in 100ms (override 路径下 \
         action backend 自产 cmd 真 publish), got {}",
        cmd_ch.publishes()
    );
    assert!(
        hb_ch.publishes() >= 4,
        "hb 50Hz 不停发: got {}",
        hb_ch.publishes()
    );
    // HoldPose hold-until-cancel：100ms 内未 cancel，仍 Executing。
    assert!(
        matches!(final_state, CoordinatorState::Executing { .. }),
        "still EXECUTING after 100ms (no cancel): got {final_state:?}"
    );

    // **核心** bit-equivalent 验收（reviewer round 2 收紧版）：
    // 取 cmd_ch.latest body → 反推 publish 时 cfg / source_task_id /
    // now_ns → 用 build_hold_command 构造 ground truth → 字节级 assert_eq。
    let cfg = *coordinator.config();
    let (body, seq, now_ns) = read_latest_cmd(&cmd_ch, &cfg);
    let ground_truth = build_hold_command(
        &cfg,
        coordinator.layout_id(),
        coordinator.anchor_bias(),
        seq,
        now_ns,
    );
    assert_eq!(
        body,
        ground_truth.as_slice(),
        "HoldPose publish body 必须 bit-equivalent to build_hold_command(cfg, \
         layout_id, anchor_bias, source_task_id, now_ns)"
    );

    // 附加字段断言：source_task_id 非零（跨过 Executing 进入后已有 publish）+
    // target == anchor_bias + priority/on_timeout 跟 cfg 一致。
    let cmd = flatbuffers::root::<FbJointCommand<'_>>(&body).expect("fbs verify");
    let target: Vec<f64> = cmd.target().expect("target").iter().collect();
    assert_eq!(target, anchor_bias, "target == anchor_bias");
    assert_eq!(cmd.layout_id(), LAYOUT_ID);
    assert_eq!(cmd.mode(), ControlMode::Position);
    assert_eq!(cmd.priority(), cfg.priority);
    assert_eq!(cmd.on_timeout(), robot_msg::msg::StaleAction::Hold);
    assert_eq!(cmd.source_task_id(), seq);
    assert_eq!(cmd.deadline_ns(), now_ns + cfg.deadline_offset_ns());
}

// ============================================================================
// 10. cancel_hold_pose：cross-segment cmd_seq monotonicity（reviewer 评审重点）
// ============================================================================
//
// **设计取舍**：reviewer round 2 要求"覆盖 Idle hold 帧 → HoldPose
// Executing 帧 → cancel 后 Idle hold 帧，确认 source_task_id 严格递增"。
// 这要求**单**调用窗口内观察 3 段所有 publish 的 seq —— `run_action_tick_loop`
// 每次 invoke 重建 `TickState` 让 `cmd_seq` 从 0 起，跨多次 invoke 比对
// 不能验单调。
//
// 解法：**测试内手动 tick 循环**，按 cmd_period 节拍模拟
// `run_action_tick_loop` 的 cmd-publish 分支（state-aware：Idle/Degraded
// → `build_hold_command` ground truth；Executing → `coord.tick_action(clock,
// cmd_seq)`）。本测试的目的是验**契约**：当 Coordinator API + Idle hold
// 路径用同一份 `cmd_seq` counter 时，跨 state 转移 source_task_id 严格
// 递增。`run_action_tick_loop` 自身 happy-path 由 `happy_hold_pose_*` /
// `happy_a` / `happy_b` / `baseline_*` 等覆盖。
//
// 如未来 `run_action_tick_loop` 行为变更（如 cmd-period 计算改、cmd_seq
// 推进规则改），本测试的手动循环需同步更新。

// `#[allow(clippy::too_many_lines)]` rationale：3 段 monotonicity 验证
// 天然较长；拆 helper 仍需主体保留段间状态机 sequence 注释，拆出去反
// 而碎；本测试一段连贯叙事更易读。
#[test]
#[allow(clippy::too_many_lines)]
fn cancel_hold_pose_during_executing_emits_cancelled_then_idle_resumes_hold() {
    let anchor_bias = vec![0.42_f64];
    let coordinator = make_hold_pose_coordinator(anchor_bias);
    coordinator.mark_idle().expect("Booting → Idle");

    let clock = MockClock::new(MonoRawNs(1_000_000_000));
    let cfg = *coordinator.config();
    let layout_id = coordinator.layout_id();
    let bias_owned: Vec<f64> = coordinator.anchor_bias().to_vec();
    let cmd_period_ns: u64 = u64::try_from(cfg.command_period().as_nanos()).unwrap();

    // 手动 tick loop 内 cmd_seq counter（模拟 TickState.cmd_seq 行为）。
    let mut cmd_seq: u64 = 0;
    let mut last_cmd_ns: u64 = clock.now().0;

    // 记每段 publish 时对应的 source_task_id（即 publish 时的 cmd_seq）。
    let mut seqs_p1: Vec<u64> = Vec::new(); // Phase 1: Idle hold
    let mut seqs_p2: Vec<u64> = Vec::new(); // Phase 2: HoldPose Executing
    let mut seqs_p3: Vec<u64> = Vec::new(); // Phase 3: post-cancel Idle hold

    // Phase 1: Idle hold —— 模拟 50ms = 2-3 个 cmd_period。
    let phase1_end = clock.now().0 + 50_000_000;
    while clock.now().0 < phase1_end {
        let now = clock.now().0;
        if now.saturating_sub(last_cmd_ns) >= cmd_period_ns {
            if let Some(seq) = try_publish_step(
                &coordinator,
                &clock,
                &cfg,
                layout_id,
                &bias_owned,
                &mut cmd_seq,
            ) {
                seqs_p1.push(seq);
            }
            last_cmd_ns = now;
        }
        clock.advance(robot_core::time::DurationNs::from_nanos(1_000_000)); // +1ms
    }
    assert!(
        !seqs_p1.is_empty(),
        "Phase 1 Idle 应至少 1 publish (50ms / 20ms cmd_period)"
    );
    assert!(matches!(
        coordinator.current_state(),
        CoordinatorState::Idle
    ));

    // Submit HoldPose → 状态转 Executing。
    let ticket = coordinator
        .submit(hold_pose_request("monotonicity"), &clock)
        .expect("submit ok in Idle");
    assert!(matches!(
        coordinator.current_state(),
        CoordinatorState::Executing { .. }
    ));

    // Phase 2: Executing 50ms。
    let phase2_end = clock.now().0 + 50_000_000;
    while clock.now().0 < phase2_end {
        let now = clock.now().0;
        if now.saturating_sub(last_cmd_ns) >= cmd_period_ns {
            if let Some(seq) = try_publish_step(
                &coordinator,
                &clock,
                &cfg,
                layout_id,
                &bias_owned,
                &mut cmd_seq,
            ) {
                seqs_p2.push(seq);
            }
            last_cmd_ns = now;
        }
        clock.advance(robot_core::time::DurationNs::from_nanos(1_000_000));
    }
    assert!(
        !seqs_p2.is_empty(),
        "Phase 2 Executing 应至少 1 publish（HoldPose Some(body)）"
    );
    assert!(matches!(
        coordinator.current_state(),
        CoordinatorState::Executing { .. }
    ));

    // Cancel（reviewer 拍板 cancel 不是 terminal；状态保 Executing
    // 等下 tick 收敛）。
    coordinator
        .cancel(ticket.action_id, &clock)
        .expect("cancel ok");
    assert!(matches!(
        coordinator.current_state(),
        CoordinatorState::Executing { .. }
    ));

    // Phase 3: cancel 收敛 + post-cancel Idle resume，60ms。
    // 第一帧 tick HoldPose 发现 cancelled flag → Completed(PartialSuccess)
    // → coordinator 转 Idle，tick_action 返 None；后续帧 Idle hold publish。
    let phase3_end = clock.now().0 + 60_000_000;
    while clock.now().0 < phase3_end {
        let now = clock.now().0;
        if now.saturating_sub(last_cmd_ns) >= cmd_period_ns {
            if let Some(seq) = try_publish_step(
                &coordinator,
                &clock,
                &cfg,
                layout_id,
                &bias_owned,
                &mut cmd_seq,
            ) {
                seqs_p3.push(seq);
            }
            last_cmd_ns = now;
        }
        clock.advance(robot_core::time::DurationNs::from_nanos(1_000_000));
    }
    assert!(
        !seqs_p3.is_empty(),
        "Phase 3 post-cancel Idle 应至少 1 publish"
    );
    assert!(
        matches!(coordinator.current_state(), CoordinatorState::Idle),
        "Phase 3 cancel 收敛后回 Idle，got {:?}",
        coordinator.current_state()
    );

    // **monotonicity 跨段总链验证**（reviewer round 2 评审重点）：
    // 把 3 段 publish 的 seq 平铺成单链，验**严格递增** —— S0
    // ipc_thread:903 把 fb.source_task_id() 写 JointCommandBuf.seq；
    // adapter.rs:119-120 要求单调（rt_thread 判 CONTROL_LOST → ACTIVE
    // 恢复用）。如本断言挂 = reviewer round 2 阻断 1 修法回退。
    let all_seqs: Vec<u64> = seqs_p1
        .iter()
        .chain(seqs_p2.iter())
        .chain(seqs_p3.iter())
        .copied()
        .collect();
    for window in all_seqs.windows(2) {
        assert!(
            window[1] > window[0],
            "monotonicity 跨段必须严格递增；all_seqs={all_seqs:?} \
             (Phase 1 Idle hold = {seqs_p1:?}, Phase 2 HoldPose = {seqs_p2:?}, \
             Phase 3 post-cancel Idle = {seqs_p3:?})"
        );
    }

    // 段间分界明示：last(p1) < first(p2) < last(p2) < first(p3)（前后
    // 已含在 windows 严格递增里；这里再断言段尾→下段头）。
    assert!(
        seqs_p1.last().unwrap() < seqs_p2.first().unwrap(),
        "Phase 1 Idle 末帧 seq < Phase 2 Executing 首帧 seq: \
         p1.last={:?} p2.first={:?}",
        seqs_p1.last(),
        seqs_p2.first()
    );
    assert!(
        seqs_p2.last().unwrap() < seqs_p3.first().unwrap(),
        "Phase 2 Executing 末帧 seq < Phase 3 post-cancel Idle 首帧 seq: \
         p2.last={:?} p3.first={:?}",
        seqs_p2.last(),
        seqs_p3.first()
    );

    // 事件流：Cancelled 必须 emit；Completed 绝不双发（reviewer 拍板）。
    let mut saw_cancelled = false;
    let mut saw_completed = false;
    for ev in ticket.events.try_iter() {
        match ev {
            ActionEvent::Cancelled { .. } => saw_cancelled = true,
            ActionEvent::Completed { .. } => saw_completed = true,
            _ => {}
        }
    }
    assert!(saw_cancelled, "应 emit Cancelled");
    assert!(!saw_completed, "**绝不**双发 Completed + Cancelled");

    // status() finished ring 验：Cancelled 入 ring。
    assert!(matches!(
        coordinator.status(ticket.action_id),
        Some(ActionStatus::Cancelled)
    ));
}
