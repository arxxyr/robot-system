//! v0.5 候选 E 段(5) G1 语义闭环 in-process 集成测（AC-E6 / AC-E7 / AC-E8 /
//! AC-E17 / AC-E18 **单机维**；跨进程口径落段(7) workflow e2e）。
//!
//! 全走 `MockChannel` + `MockClock`；**真** `ScriptedBackend` / `HoldPoseBackend`
//! / `MockDwellBackend` / `IkBackend`（feature）+ **真** state cache 线程
//! （`spawn_state_cache_thread_with` + mock subscriber），只有 transport 是 mock。
//!
//! ## 覆盖（Spec D3 行为矩阵每行独立 case，RB5：先 Success 再分别 ...）
//!
//! | case | 断言 |
//! |---|---|
//! | `boot_idle_hold_bit_equivalent_to_anchor` | boot 后 Idle hold 帧 == `build_hold_command(anchor)`（B1 bit 等价） |
//! | `scripted_first_frame_starts_from_cache_pose_not_anchor` | AC-E6：α=0 帧 target == cache 当前位（≠ anchor） |
//! | `scripted_success_then_idle_hold_is_last_waypoint` | 矩阵行 1：Success → Idle hold = 末 waypoint；末帧先于 Completed |
//! | `consecutive_chunks_no_pullback` | AC-E7：chunk 2 从 chunk 1 末点起、chunk 间 Idle hold = chunk 1 末点 |
//! | `scripted_cancel_after_success_resets_to_anchor` | 矩阵行 2 |
//! | `precondition_failure_after_success_resets_to_anchor` | 矩阵行 3（→ Idle） |
//! | `safety_abort_after_success_degraded_hold_anchor_then_clear_error_idle_anchor` | 矩阵行 3 + 末行（Degraded 不解锁）+ ClearError 回 Idle |
//! | `hold_pose_after_success_targets_anchor_bit_equivalent_and_cancel_resets` | AC-E8 HoldPose 0 改 + 矩阵行 5 |
//! | `dwell_ms_success_after_success_resets_to_anchor` | 矩阵 DwellMs 行 |
//! | `ik_success_after_success_resets_to_anchor` | 矩阵行 4（feature `backend-ik`） |
//! | `stale_layout_or_missing_cache_rejects_scripted_and_hold_unchanged` | AC-E6 失败语义：拒绝不 fallback、hold 不变 |

#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Duration;

use flatbuffers::{FlatBufferBuilder, root};
use robot_core::time::{DurationNs, MockClock, MonoRawNs, RobotClock, SystemClock};
use robot_msg::build_helpers::build_joint_state;
use robot_msg::markers;
use robot_msg::msg::{
    JointCommand as FbJointCommand, ScriptedPayload as FbScriptedPayload, ScriptedPayloadArgs,
    WaypointTarget, WaypointTargetArgs,
};
use robot_msg::schema_id::SCHEMA_SCRIPTED_PAYLOAD;
use robot_s1::action::{
    ActionBackend, ActionCancelError, ActionContext, ActionEvent, ActionFailure, ActionId,
    ActionInstance, ActionKind, ActionRequest, ActionSource, ActionStartError, ActionTickOutput,
    ActionTicket, NO_PAYLOAD_SCHEMA_ID, SubmitError,
};
use robot_s1::action_backend::BackendRegistry;
use robot_s1::action_backend::hold_pose::HoldPoseBackend;
use robot_s1::action_backend::mock_dwell::MockDwellBackend;
use robot_s1::action_backend::scripted::ScriptedBackend;
use robot_s1::coordinator::{
    Coordinator, CoordinatorConfig, CoordinatorState, HardwareSection, build_hold_command,
    run_action_tick_loop,
};
use robot_s1::state_cache::{READY_TIMEOUT, StateCacheConfig, spawn_state_cache_thread_with};
use robot_transport::mock::{MockChannel, MockPublisher};
use robot_transport::traits::{LatestSubscriber, Publisher};

const LAYOUT: u64 = 0x6161_0000_0000_0005;
const TOPIC: &str = "/g1/s0/arm/state";
/// Scripted 每段时长（ms）；50 Hz cmd 周期下 5 帧插值。
const SEG_MS: u32 = 100;
/// `drive_to_terminal` 上限（MockClock 推进的虚拟 ms）。
const DRIVE_MAX_MS: u64 = 5_000;

// ============================================================================
// 测试 backend：按 failure 类型失败（kind DwellMs，无 payload）
// ============================================================================

#[derive(Clone, Copy)]
enum FailKind {
    SafetyAbort,
    Precondition,
}

struct FailBackend {
    name: &'static str,
    kind: FailKind,
}

#[allow(clippy::unnecessary_literal_bound)]
impl ActionBackend for FailBackend {
    fn name(&self) -> &str {
        self.name
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
        Ok(Box::new(FailInstance(self.kind)))
    }
}

struct FailInstance(FailKind);

impl ActionInstance for FailInstance {
    fn tick(&mut self, _ctx: &mut ActionContext<'_>) -> Result<ActionTickOutput, ActionFailure> {
        Err(match self.0 {
            FailKind::SafetyAbort => ActionFailure::SafetyAbort("g1 test abort".into()),
            FailKind::Precondition => {
                ActionFailure::PreconditionFailed("g1 test precondition".into())
            }
        })
    }
    fn cancel(&mut self, _ctx: &mut ActionContext<'_>) -> Result<(), ActionCancelError> {
        Ok(())
    }
}

static SCRIPTED: ScriptedBackend = ScriptedBackend;
static HOLD_POSE: HoldPoseBackend = HoldPoseBackend;
static SAFETY_ABORT: FailBackend = FailBackend {
    name: "safety_abort",
    kind: FailKind::SafetyAbort,
};
static PRECOND_FAIL: FailBackend = FailBackend {
    name: "precond_fail",
    kind: FailKind::Precondition,
};
#[cfg(feature = "backend-ik")]
static IK: robot_s1::action_backend::ik::IkBackend = robot_s1::action_backend::ik::IkBackend;

// ============================================================================
// Rig：coordinator + 真 state cache 线程（mock transport）+ MockClock
// ============================================================================

struct Rig {
    coordinator: Coordinator,
    state_ch: MockChannel<markers::JointState>,
    clock: MockClock,
    cfg: CoordinatorConfig,
    anchor: Vec<f64>,
}

fn make_rig(
    joints: usize,
    anchor: Vec<f64>,
    enabled: &[&str],
    solver: Option<Box<dyn robot_kinematics::ArmIkSolver>>,
) -> Rig {
    assert_eq!(anchor.len(), joints);
    let hardware = HardwareSection {
        id: "arm".into(),
        joints: (0..joints).map(|i| format!("j{i}")).collect(),
        motor_ids: (0..joints)
            .map(|i| u8::try_from(i + 1).expect("motor id fits u8"))
            .collect(),
        feetech_bus2_mixed_raw: false,
        calibration_range_tolerances_raw: None,
    };
    let mock_dwell = Box::leak(Box::new(MockDwellBackend::with_duration_ms(20)));
    let mut backends: Vec<&'static dyn ActionBackend> = vec![
        &SCRIPTED,
        &HOLD_POSE,
        mock_dwell,
        &SAFETY_ABORT,
        &PRECOND_FAIL,
    ];
    #[cfg(feature = "backend-ik")]
    backends.push(&IK);
    let registry = BackendRegistry::from_explicit(&backends);

    let state_ch = MockChannel::<markers::JointState>::new();
    let ch = state_ch.clone();
    let cache = spawn_state_cache_thread_with(
        StateCacheConfig {
            topic: TOPIC.into(),
            expected_joints: joints,
            expected_layout: LAYOUT,
            max_stale_ms: 200,
            poll_interval: Duration::from_millis(1),
        },
        Arc::new(SystemClock::new()),
        READY_TIMEOUT,
        move || Ok(ch.subscriber()),
    )
    .expect("spawn state cache");

    let cfg = CoordinatorConfig::default();
    let coordinator = Coordinator::new(
        cfg,
        hardware,
        LAYOUT,
        anchor.clone(),
        registry,
        enabled.iter().map(|s| (*s).to_string()).collect(),
        solver,
    )
    .with_state_cache(cache);
    coordinator.mark_idle().expect("Booting → Idle");
    Rig {
        coordinator,
        state_ch,
        clock: MockClock::new(MonoRawNs(1_000_000_000)),
        cfg,
        anchor,
    }
}

fn rig3(enabled: &[&str]) -> Rig {
    make_rig(3, vec![0.11, 0.22, 0.33], enabled, None)
}

/// 发布一帧 S0 state（stamp = 当前 MockClock）并等 cache 线程观测到它。
fn feed_state(rig: &Rig, positions: &[f64]) {
    let stamp = rig.clock.now().0;
    let body = build_joint_state(LAYOUT, stamp, positions, &[], &[], "");
    publish_state_body(&rig.state_ch, &body, stamp);
    for _ in 0..2000 {
        if let Some(Ok(got)) = rig.coordinator.probe_state_cache(MonoRawNs(stamp))
            && got == positions
        {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!(
        "state cache never observed {positions:?}: {:?}",
        rig.coordinator.probe_state_cache(MonoRawNs(stamp))
    );
}

fn publish_state_body(ch: &MockChannel<markers::JointState>, body: &[u8], stamp_ns: u64) {
    let mut pub_ = ch.publisher();
    let mut w = pub_.loan(body.len() + 1024).expect("loan");
    w.set_body_bytes(body);
    w.set_stamp_ns(stamp_ns);
    w.set_seq(0);
    w.set_source_id(1);
    w.publish().expect("publish");
}

fn scripted_payload(waypoints: &[Vec<f64>]) -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::with_capacity(256);
    let wp_offsets: Vec<_> = waypoints
        .iter()
        .map(|positions| {
            let v = fbb.create_vector::<f64>(positions);
            WaypointTarget::create(
                &mut fbb,
                &WaypointTargetArgs {
                    positions: Some(v),
                    dwell_ms: 0,
                    segment_duration_ms: SEG_MS,
                },
            )
        })
        .collect();
    let wps = fbb.create_vector(&wp_offsets);
    let payload = FbScriptedPayload::create(
        &mut fbb,
        &ScriptedPayloadArgs {
            waypoints: Some(wps),
        },
    );
    fbb.finish(payload, None);
    fbb.finished_data().to_vec()
}

fn request(kind: ActionKind, schema: u32, payload: Vec<u8>, tag: &str) -> ActionRequest {
    ActionRequest {
        action_id: ActionId::new_v7(),
        kind,
        deadline_ns: u64::MAX,
        priority: 10,
        payload_schema_id: schema,
        payload,
        idempotency_key: format!("g1-{tag}"),
        source: ActionSource::Internal,
    }
}

fn submit_scripted(rig: &Rig, waypoints: &[Vec<f64>]) -> Result<ActionTicket, SubmitError> {
    rig.coordinator.submit(
        request(
            ActionKind::Scripted,
            SCHEMA_SCRIPTED_PAYLOAD,
            scripted_payload(waypoints),
            "scripted",
        ),
        &rig.clock,
    )
}

fn submit_no_payload(rig: &Rig, kind: ActionKind, tag: &str) -> ActionTicket {
    rig.coordinator
        .submit(
            request(kind, NO_PAYLOAD_SCHEMA_ID, Vec::new(), tag),
            &rig.clock,
        )
        .expect("submit ok")
}

struct Pubs {
    cmd_ch: MockChannel<markers::JointCommand>,
    hb_ch: MockChannel<markers::Heartbeat>,
    cmd_pub: MockPublisher<markers::JointCommand>,
    hb_pub: MockPublisher<markers::Heartbeat>,
}

fn pubs() -> Pubs {
    let cmd_ch = MockChannel::<markers::JointCommand>::new();
    let hb_ch = MockChannel::<markers::Heartbeat>::new();
    let cmd_pub = cmd_ch.publisher();
    let hb_pub = hb_ch.publisher();
    Pubs {
        cmd_ch,
        hb_ch,
        cmd_pub,
        hb_pub,
    }
}

/// 跑 `run_action_tick_loop` 虚拟 `ms`（poll 1 ms / cmd 50 Hz）。
fn run_loop(rig: &Rig, p: &mut Pubs, ms: u64) -> CoordinatorState {
    let shutdown = AtomicBool::new(false);
    run_action_tick_loop(
        &rig.coordinator,
        &mut p.hb_pub,
        &mut p.cmd_pub,
        &rig.clock,
        Duration::from_millis(1),
        Some(Duration::from_millis(ms)),
        &shutdown,
    )
    .expect("tick loop ok");
    rig.coordinator.current_state()
}

/// 每步推进的虚拟 ms：每次 `run_action_tick_loop` 重建 `TickState`
/// （首 cmd 在 start+20 ms），且 duration 到期检查先于 publish —— 步长必须
/// **大于** cmd 周期（20 ms）才能每步至少 tick 一次；取 30 ms。
const DRIVE_STEP_MS: u64 = 30;

/// 以 [`DRIVE_STEP_MS`] 为步推进直到离开 Executing（上限 [`DRIVE_MAX_MS`]）。
fn drive_to_terminal(rig: &Rig, p: &mut Pubs) -> CoordinatorState {
    let mut elapsed = 0_u64;
    loop {
        let st = run_loop(rig, p, DRIVE_STEP_MS);
        if !matches!(st, CoordinatorState::Executing { .. }) {
            return st;
        }
        elapsed += DRIVE_STEP_MS;
        assert!(
            elapsed <= DRIVE_MAX_MS,
            "action did not reach terminal within {DRIVE_MAX_MS} ms"
        );
    }
}

fn latest_cmd(p: &Pubs) -> (Vec<u8>, Vec<f64>) {
    let mut sub = p.cmd_ch.subscriber();
    let lease = sub.latest().expect("latest").expect("Some sample");
    let view = lease.view().expect("view");
    let body = view.body.raw().to_vec();
    let cmd = root::<FbJointCommand<'_>>(&body).expect("fbs verify");
    let target: Vec<f64> = cmd.target().expect("target").iter().collect();
    (body, target)
}

fn decode_target(body: &[u8]) -> Vec<f64> {
    let cmd = root::<FbJointCommand<'_>>(body).expect("fbs verify");
    cmd.target().expect("target").iter().collect()
}

/// 在 Idle 下跑 50 ms hold 并返最新 hold 帧 target。
fn idle_hold_target_observed(rig: &Rig, p: &mut Pubs) -> Vec<f64> {
    let st = run_loop(rig, p, 50);
    assert!(
        matches!(st, CoordinatorState::Idle),
        "expected Idle, got {st:?}"
    );
    latest_cmd(p).1
}

/// 前置（RB5）：一次真 Scripted Success → Idle hold = `last`。
fn scripted_success(rig: &Rig, p: &mut Pubs, start: &[f64], last: &[f64]) {
    feed_state(rig, start);
    let ticket = submit_scripted(rig, &[last.to_vec()]).expect("submit ok");
    let st = drive_to_terminal(rig, p);
    assert!(matches!(st, CoordinatorState::Idle), "got {st:?}");
    assert!(
        ticket.events.try_iter().any(|ev| matches!(
            ev,
            ActionEvent::Completed {
                outcome: robot_s1::action::ActionOutcome::Success,
                ..
            }
        )),
        "前置 Scripted 必须 Completed(Success)"
    );
    assert_eq!(
        rig.coordinator.idle_hold_target(),
        last,
        "前置：Success-Scripted 后 idle_hold_target = 末 waypoint"
    );
    assert_ne!(last, rig.anchor.as_slice(), "测试夹具：末点必须 ≠ anchor");
}

/// 最新 cmd 帧与 `build_hold_command(cfg, LAYOUT, bias, seq, now)` bit 等价。
fn assert_bit_equivalent_hold(rig: &Rig, body: &[u8], bias: &[f64]) {
    let cmd = root::<FbJointCommand<'_>>(body).expect("fbs verify");
    let seq = cmd.source_task_id();
    let now_ns = cmd.deadline_ns() - rig.cfg.deadline_offset_ns();
    let ground_truth = build_hold_command(&rig.cfg, LAYOUT, bias, seq, now_ns);
    assert_eq!(
        body,
        ground_truth.as_slice(),
        "hold 帧必须 bit-equivalent to build_hold_command(bias={bias:?})"
    );
}

// ============================================================================
// cases
// ============================================================================

#[test]
fn boot_idle_hold_bit_equivalent_to_anchor() {
    let rig = rig3(&["scripted"]);
    let mut p = pubs();
    let target = idle_hold_target_observed(&rig, &mut p);
    assert_eq!(
        target, rig.anchor,
        "boot 后 Idle hold == anchor（B1 bit 等价）"
    );
    let (body, _) = latest_cmd(&p);
    assert_bit_equivalent_hold(&rig, &body, &rig.anchor);
    assert!(p.hb_ch.publishes() >= 2, "hb 50 Hz 不停发");
}

#[test]
fn scripted_first_frame_starts_from_cache_pose_not_anchor() {
    let rig = rig3(&["scripted"]);
    let pose = vec![0.5, -0.5, 0.25];
    feed_state(&rig, &pose);
    submit_scripted(&rig, &[vec![1.0, 1.0, 1.0]]).expect("submit ok");
    // 直接 tick_action：α=0 帧 target == cache 当前位（≠ anchor）。
    let body = rig
        .coordinator
        .tick_action(&rig.clock, 0)
        .expect("α=0 帧 Some(body)");
    assert_eq!(decode_target(body.as_bytes()), pose);
    assert_ne!(pose, rig.anchor);
}

#[test]
fn scripted_success_then_idle_hold_is_last_waypoint() {
    let rig = rig3(&["scripted"]);
    let mut p = pubs();
    let last = vec![1.0, -1.0, 0.5];
    scripted_success(&rig, &mut p, &[0.0, 0.0, 0.0], &last);

    // 末点发布保证（loop 级）：drive 到 Idle 的最后一帧（Completed 帧不
    // 产 cmd）target == 末 waypoint，随后 Idle hold 也 == 末 waypoint。
    let (_, before_idle_hold) = latest_cmd(&p);
    assert_eq!(
        before_idle_hold, last,
        "Completed 之前最后一帧 == 末 waypoint"
    );
    let observed = idle_hold_target_observed(&rig, &mut p);
    assert_eq!(
        observed, last,
        "Success-Scripted 后 Idle hold == 末 waypoint"
    );
    let (body, _) = latest_cmd(&p);
    assert_bit_equivalent_hold(&rig, &body, &last);
}

#[test]
fn consecutive_chunks_no_pullback() {
    // AC-E7 单机维：chunk 1 → Idle hold = W1（不回拉 anchor）→ S0 报当前位
    // = W1 → chunk 2 α=0 帧 == W1（不从 anchor 重放）→ Idle hold = W2。
    let rig = rig3(&["scripted"]);
    let mut p = pubs();
    let w1 = vec![0.6, 0.6, 0.6];
    let w2 = vec![-0.4, 0.9, 0.1];
    scripted_success(&rig, &mut p, &[0.0, 0.0, 0.0], &w1);
    assert_eq!(idle_hold_target_observed(&rig, &mut p), w1);

    feed_state(&rig, &w1);
    let ticket = submit_scripted(&rig, std::slice::from_ref(&w2)).expect("chunk 2 submit ok");
    let body = rig
        .coordinator
        .tick_action(&rig.clock, 0)
        .expect("chunk 2 α=0 帧");
    assert_eq!(
        decode_target(body.as_bytes()),
        w1,
        "chunk 2 从 chunk 1 末点起插值"
    );
    let st = drive_to_terminal(&rig, &mut p);
    assert!(matches!(st, CoordinatorState::Idle));
    assert!(
        ticket
            .events
            .try_iter()
            .any(|ev| matches!(ev, ActionEvent::Completed { .. }))
    );
    assert_eq!(idle_hold_target_observed(&rig, &mut p), w2);
}

#[test]
fn scripted_cancel_after_success_resets_to_anchor() {
    let rig = rig3(&["scripted"]);
    let mut p = pubs();
    scripted_success(&rig, &mut p, &[0.0, 0.0, 0.0], &[1.0, 1.0, 1.0]);

    feed_state(&rig, &[1.0, 1.0, 1.0]);
    // 长 action（5 段）→ 50 ms 后 cancel 命中 in-flight。
    let long: Vec<Vec<f64>> = (1..=5).map(|i| vec![f64::from(i) * 0.1; 3]).collect();
    let ticket = submit_scripted(&rig, &long).expect("submit ok");
    let st = run_loop(&rig, &mut p, 50);
    assert!(matches!(st, CoordinatorState::Executing { .. }));
    rig.coordinator
        .cancel(ticket.action_id, &rig.clock)
        .expect("cancel ok");
    let st = drive_to_terminal(&rig, &mut p);
    assert!(matches!(st, CoordinatorState::Idle));
    assert!(
        ticket
            .events
            .try_iter()
            .any(|ev| matches!(ev, ActionEvent::Cancelled { .. }))
    );
    assert_eq!(rig.coordinator.idle_hold_target(), rig.anchor);
    assert_eq!(
        idle_hold_target_observed(&rig, &mut p),
        rig.anchor,
        "cancel → 回拉启动 anchor"
    );
}

#[test]
fn precondition_failure_after_success_resets_to_anchor() {
    let rig = rig3(&["scripted", "precond_fail"]);
    let mut p = pubs();
    scripted_success(&rig, &mut p, &[0.0, 0.0, 0.0], &[1.0, 1.0, 1.0]);

    let ticket = submit_no_payload(&rig, ActionKind::DwellMs, "precond");
    let st = drive_to_terminal(&rig, &mut p);
    assert!(
        matches!(st, CoordinatorState::Idle),
        "PreconditionFailed → Idle, got {st:?}"
    );
    assert!(ticket.events.try_iter().any(|ev| matches!(
        ev,
        ActionEvent::Failed {
            failure: ActionFailure::PreconditionFailed(_),
            ..
        }
    )));
    assert_eq!(idle_hold_target_observed(&rig, &mut p), rig.anchor);
}

#[test]
fn safety_abort_after_success_degraded_hold_anchor_then_clear_error_idle_anchor() {
    let rig = rig3(&["scripted", "safety_abort"]);
    let mut p = pubs();
    scripted_success(&rig, &mut p, &[0.0, 0.0, 0.0], &[1.0, 1.0, 1.0]);

    let _ticket = submit_no_payload(&rig, ActionKind::DwellMs, "abort");
    let st = drive_to_terminal(&rig, &mut p);
    assert!(
        matches!(st, CoordinatorState::Degraded { .. }),
        "got {st:?}"
    );
    // Degraded hold == 启动 anchor（不解锁；与刚才 Success 无关）。
    let st = run_loop(&rig, &mut p, 50);
    assert!(matches!(st, CoordinatorState::Degraded { .. }));
    let (body, target) = latest_cmd(&p);
    assert_eq!(target, rig.anchor, "Degraded hold == anchor_bias");
    assert_bit_equivalent_hold(&rig, &body, &rig.anchor);
    assert_eq!(rig.coordinator.idle_hold_target(), rig.anchor);
    // ClearError → Idle，hold 仍 == anchor。
    rig.coordinator.clear_error().expect("Degraded → Idle");
    assert_eq!(idle_hold_target_observed(&rig, &mut p), rig.anchor);
}

#[test]
fn hold_pose_after_success_targets_anchor_bit_equivalent_and_cancel_resets() {
    // AC-E8：Success-Scripted 后 idle_hold_target ≠ anchor，但 HoldPose 每帧
    // 仍读 ctx.anchor_bias（0 改）→ bit 等价 build_hold_command(anchor)。
    let rig = rig3(&["scripted", "hold_pose"]);
    let mut p = pubs();
    scripted_success(&rig, &mut p, &[0.0, 0.0, 0.0], &[1.0, 1.0, 1.0]);

    let ticket = submit_no_payload(&rig, ActionKind::HoldPose, "hold");
    let st = run_loop(&rig, &mut p, 100);
    assert!(matches!(st, CoordinatorState::Executing { .. }));
    let (body, target) = latest_cmd(&p);
    assert_eq!(
        target, rig.anchor,
        "HoldPose target == anchor_bias（不受 idle_hold_target 影响）"
    );
    assert_bit_equivalent_hold(&rig, &body, &rig.anchor);

    rig.coordinator
        .cancel(ticket.action_id, &rig.clock)
        .expect("cancel ok");
    let st = drive_to_terminal(&rig, &mut p);
    assert!(matches!(st, CoordinatorState::Idle));
    assert!(
        ticket
            .events
            .try_iter()
            .any(|ev| matches!(ev, ActionEvent::Cancelled { .. }))
    );
    assert_eq!(
        idle_hold_target_observed(&rig, &mut p),
        rig.anchor,
        "HoldPose cancel → anchor"
    );
}

#[test]
fn dwell_ms_success_after_success_resets_to_anchor() {
    let rig = rig3(&["scripted", "mock_dwell"]);
    let mut p = pubs();
    scripted_success(&rig, &mut p, &[0.0, 0.0, 0.0], &[1.0, 1.0, 1.0]);

    let ticket = submit_no_payload(&rig, ActionKind::DwellMs, "dwell");
    let st = drive_to_terminal(&rig, &mut p);
    assert!(matches!(st, CoordinatorState::Idle));
    assert!(ticket.events.try_iter().any(|ev| matches!(
        ev,
        ActionEvent::Completed {
            outcome: robot_s1::action::ActionOutcome::Success,
            ..
        }
    )));
    assert_eq!(
        idle_hold_target_observed(&rig, &mut p),
        rig.anchor,
        "DwellMs Success → anchor"
    );
}

#[cfg(feature = "backend-ik")]
#[test]
fn ik_success_after_success_resets_to_anchor() {
    use robot_kinematics_xlerobot::{Side, XLeRobotArmIkSolver};
    use robot_msg::msg::{IkPayload as FbIkPayload, IkPayloadArgs};
    use robot_msg::schema_id::SCHEMA_IK_PAYLOAD;

    let solver: Box<dyn robot_kinematics::ArmIkSolver> =
        Box::new(XLeRobotArmIkSolver::new(Side::Right));
    let anchor = vec![0.05; 5];
    let rig = make_rig(5, anchor.clone(), &["scripted", "ik"], Some(solver));
    let mut p = pubs();
    scripted_success(&rig, &mut p, &[0.0; 5], &[0.3; 5]);

    // 可达 IK 目标（沿 ik.rs 单测）。
    let payload = {
        let mut fbb = FlatBufferBuilder::with_capacity(256);
        let xyz = fbb.create_vector::<f64>(&[0.20, 0.0, 0.10]);
        let quat = fbb.create_vector::<f64>(&[0.0, 0.0, 0.0, 1.0]);
        let seed = fbb.create_vector::<f64>(&[]);
        let frame = fbb.create_string("");
        let pl = FbIkPayload::create(
            &mut fbb,
            &IkPayloadArgs {
                target_xyz: Some(xyz),
                target_quat_xyzw: Some(quat),
                seed_joints_rad: Some(seed),
                frame: Some(frame),
            },
        );
        fbb.finish(pl, None);
        fbb.finished_data().to_vec()
    };
    let ticket = rig
        .coordinator
        .submit(
            request(ActionKind::Ik, SCHEMA_IK_PAYLOAD, payload, "ik"),
            &rig.clock,
        )
        .expect("ik submit ok");
    let st = drive_to_terminal(&rig, &mut p);
    assert!(matches!(st, CoordinatorState::Idle));
    assert!(ticket.events.try_iter().any(|ev| matches!(
        ev,
        ActionEvent::Completed {
            outcome: robot_s1::action::ActionOutcome::Success,
            ..
        }
    )));
    assert_eq!(
        idle_hold_target_observed(&rig, &mut p),
        anchor,
        "IK Success → anchor（ik.rs 0 改）"
    );
}

#[test]
fn stale_layout_or_missing_cache_rejects_scripted_and_hold_unchanged() {
    let rig = rig3(&["scripted"]);
    let mut p = pubs();

    // (a) 无帧。
    let err = submit_scripted(&rig, &[vec![1.0; 3]]).expect_err("no frame");
    assert!(
        matches!(&err, SubmitError::BackendStartFailed(ActionStartError::Rejected(s)) if s.contains("no frame")),
        "got {err:?}"
    );

    // (b) stale：喂帧后推 201 ms。
    feed_state(&rig, &[0.0; 3]);
    rig.clock.advance(DurationNs::from_nanos(201_000_000));
    let err = submit_scripted(&rig, &[vec![1.0; 3]]).expect_err("stale");
    assert!(
        matches!(&err, SubmitError::BackendStartFailed(ActionStartError::Rejected(s)) if s.contains("stale")),
        "got {err:?}"
    );

    // (c) layout 不匹配。
    let stamp = rig.clock.now().0;
    let body = build_joint_state(LAYOUT ^ 0x1, stamp, &[0.0; 3], &[], &[], "");
    publish_state_body(&rig.state_ch, &body, stamp);
    for _ in 0..2000 {
        if matches!(
            rig.coordinator.probe_state_cache(MonoRawNs(stamp)),
            Some(Err(
                robot_s1::state_cache::StaleOrMissing::LayoutMismatch { .. }
            ))
        ) {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let err = submit_scripted(&rig, &[vec![1.0; 3]]).expect_err("layout");
    assert!(
        matches!(&err, SubmitError::BackendStartFailed(ActionStartError::Rejected(s)) if s.contains("layout_id mismatch")),
        "got {err:?}"
    );

    // 三次拒绝都不改状态、不改 hold（仍 anchor），Idle hold 帧照发。
    assert!(matches!(
        rig.coordinator.current_state(),
        CoordinatorState::Idle
    ));
    assert_eq!(rig.coordinator.idle_hold_target(), rig.anchor);
    assert_eq!(idle_hold_target_observed(&rig, &mut p), rig.anchor);
}
