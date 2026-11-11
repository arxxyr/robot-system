//! v0.3 B1 (2/3) in-process 集成测：mock S0 publisher / subscriber 跑
//! 通 anchor → tick loop → cmd / hb 端到端流。
//!
//! **不**起子进程 / 不接 iceoryx2 真服务，全部走 `robot-transport` 的
//! `MockChannel` 后端。验证目标（B1 outline §7.2）：
//!
//! - **anchor 成功路径**：mock S0 推一帧 state → `read_anchor_from_s0_state`
//!   返 `bias = positions`。
//! - **anchor fail-fast**（已在 lib unit test 覆盖；此处 1 条冒烟即可）：
//!   无 publisher → Timeout（由 lib `read_anchor_timeout_when_no_publisher`
//!   测试覆盖）。
//! - **50 Hz tick**：连续 N 次 `tick_once` 后 cmd / hb 各 publish ≥ N 帧。
//! - **hold target == anchor**：每帧 `JointCommand.target` 恒等于
//!   anchor bias，`mode = Position`，`on_timeout = Hold`。
//! - **deadline 单调**：相邻两帧 `deadline_ns` 严格递增。
//!
//! ## 为什么用 `tick_once` 而不是 `run_tick_loop`
//!
//! `MockChannel` 是 keep-last 单 slot；测试要逐帧抓 body 校验
//! `deadline_ns` 单调，必须每发一帧立刻 `latest()` 一次。`tick_once`
//! 是 lib 暴露的单步原语，方便 step-through；`run_tick_loop` 内部循
//! 环 + `clock.sleep_for`，单步抓帧不便（且 `MockClock::sleep_for`
//! 推时间但**不**让出 CPU，跟"逐帧抓"语义不匹配）。
//!
//! `run_tick_loop` 自身的 happy path 走 lib 单测覆盖（`tick_once_*`）+
//! binary smoke（B1 (3/3) `crash_restart_smoke` 切 `robot-s1`）。

// 中文 doc 块大量 identifier-like 字串（tick_once / run_tick_loop /
// MockClock / MockChannel 等），跟 lib 同款宽松 doc-markdown。
#![allow(clippy::doc_markdown)]

use std::time::Duration;

use flatbuffers::root;
use robot_msg::build_helpers::build_joint_state;
use robot_msg::markers;
use robot_msg::msg::{
    ControlMode, Heartbeat as FbHeartbeat, JointCommand as FbJointCommand, StaleAction,
};
use robot_s1::coordinator::{
    CoordinatorConfig, TickPublished, TickState, build_hold_command, parse_anchor_from_state_bytes,
    read_anchor_from_s0_state, tick_once,
};
use robot_transport::mock::MockChannel;
use robot_transport::traits::{LatestSubscriber, Publisher};

use robot_core::time::{MockClock, MonoRawNs};

const LAYOUT_ID: u64 = 0xCAFE_BABE_DEAD_BEEF;

/// Tick loop 端到端测试跑的步数（每步 25 ms，> 20 ms 周期），最少 10
/// 帧 cmd（B1 outline §7.2 验证目标）。
const TICK_STEPS: u64 = 12;

/// 在 mock state channel 上 publish 一帧 `JointState` body bytes。
fn publish_state(channel: &MockChannel<markers::JointState>, positions: &[f64], stamp_ns: u64) {
    let body = build_joint_state(LAYOUT_ID, stamp_ns, positions, &[], &[], "");
    let mut pub_ = channel.publisher();
    let mut writer = pub_.loan(body.len() + 1024).expect("loan");
    writer.set_body_bytes(&body);
    writer.set_stamp_ns(stamp_ns);
    writer.set_seq(0);
    writer.set_source_id(/* S0 */ 1);
    writer.publish().expect("publish");
}

/// 完整端到端：mock S0 推一帧 state → s1 anchor → 多次 tick_once →
/// 验证 50 Hz / target == bias / deadline 单调 / on_timeout=Hold /
/// mode=Position / source_id 一致。
#[test]
fn anchor_then_tick_loop_publishes_hold_commands_with_monotonic_deadline() {
    let cfg = CoordinatorConfig::default();

    // 1. mock S0 推一帧 state，positions = [0.5, -0.5, 0.25]。
    let state_ch = MockChannel::<markers::JointState>::new();
    let now_ns_anchor: u64 = 100_000_000;
    publish_state(&state_ch, &[0.5, -0.5, 0.25], now_ns_anchor);

    // 2. s1 端订阅 + anchor。
    let mut state_sub = state_ch.subscriber();
    let clock = MockClock::new(MonoRawNs(now_ns_anchor + 50_000));
    let bias = read_anchor_from_s0_state(
        &mut state_sub,
        /* expected_joints */ 3,
        &clock,
        Duration::from_secs(1),
        Duration::from_millis(200),
        Duration::from_millis(10),
    )
    .expect("anchor must succeed");
    assert_eq!(bias, vec![0.5, -0.5, 0.25]);

    // 3. cmd / hb mock channels + tick_once × N。
    // 起 tick：每 25 ms（> 20 ms cmd/hb 周期）一次，跑 [`TICK_STEPS`] 次拿 ≥ 10 帧。
    let cmd_ch = MockChannel::<markers::JointCommand>::new();
    let hb_ch = MockChannel::<markers::Heartbeat>::new();
    let mut cmd_pub = cmd_ch.publisher();
    let mut hb_pub = hb_ch.publisher();
    let cmd_observer = cmd_ch.subscriber();
    let hb_observer = hb_ch.subscriber();

    let tick_start_ns: u64 = 1_000_000_000;
    let mut state = TickState::new(tick_start_ns);
    let step_ns: u64 = 25_000_000;

    let mut last_deadline_ns: Option<u64> = None;
    let mut cmd_published = 0u64;
    let mut hb_published = 0u64;
    let mut last_cmd_seq: i64 = -1;

    for i in 1..=TICK_STEPS {
        let now_ns = tick_start_ns + i * step_ns;
        let r = tick_once(
            &cfg,
            &mut state,
            LAYOUT_ID,
            &bias,
            now_ns,
            &mut hb_pub,
            &mut cmd_pub,
        )
        .expect("tick_once must succeed");
        assert!(r.cmd, "step {i}: cmd should publish at +25ms past period");
        assert!(r.hb, "step {i}: hb should publish at +25ms past period");

        // 最新一帧 cmd 抓出来校验。
        let lease = cmd_observer
            .clone() // MockSubscriber: Clone
            .latest()
            .expect("latest")
            .expect("must have sample");
        let view = lease.view().expect("view");
        let cmd = view.body.get();
        assert_eq!(cmd.layout_id(), LAYOUT_ID);
        assert_eq!(cmd.mode(), ControlMode::Position);
        assert_eq!(cmd.on_timeout(), StaleAction::Hold);
        let target: Vec<f64> = cmd.target().unwrap().iter().collect();
        assert_eq!(target, bias, "step {i}: target must equal anchor bias");

        // deadline 严格递增。
        let dl = cmd.deadline_ns();
        assert_eq!(
            dl,
            now_ns + cfg.deadline_offset_ns(),
            "step {i}: deadline must be now_ns + deadline_offset_ns"
        );
        if let Some(prev) = last_deadline_ns {
            assert!(
                dl > prev,
                "step {i}: deadline {dl} must be > prev {prev} (monotonic)"
            );
        }
        last_deadline_ns = Some(dl);

        // source_task_id == cmd_seq monotonically increasing。
        let seq = i64::try_from(cmd.source_task_id()).expect("seq fits i64");
        assert!(
            seq > last_cmd_seq,
            "step {i}: source_task_id {seq} must be > prev {last_cmd_seq}"
        );
        last_cmd_seq = seq;

        cmd_published += 1;
        hb_published += 1;
    }

    // 累计计数对得上。
    assert_eq!(state.cmd_seq, cmd_published);
    assert_eq!(state.hb_seq, hb_published);
    assert_eq!(cmd_ch.publishes(), cmd_published);
    assert_eq!(hb_ch.publishes(), hb_published);
    assert!(
        cmd_published >= 10,
        "B1 outline §7.2 wants ≥ 10 frames cmd, got {cmd_published}"
    );

    // 最后一帧 hb 字段校验（source_id = cfg.source_id 默认 2 / hz = 50.0）。
    let hb_lease = hb_observer
        .clone()
        .latest()
        .expect("hb latest")
        .expect("hb sample");
    let hb_view = hb_lease.view().expect("hb view");
    let hb: FbHeartbeat = hb_view.body.get();
    assert_eq!(hb.source_id(), cfg.source_id);
    assert!((hb.tick_rate_hz() - cfg.heartbeat_hz).abs() < 1e-9);
}

/// 单独测 build_hold_command + parse_anchor_from_state_bytes 的对接：
/// hold command 字段直接通过 fbs root 解码后跟 cfg / bias 对得上。
///
/// 这一条本质是 lib unit test 的延伸，但放在集成测是为了证明 lib 暴露
/// 的 API 在 binary 路径上能直接拼起来（不依赖内部 mod）。
#[test]
fn lib_api_surface_supports_full_anchor_to_hold_pipeline() {
    let cfg = CoordinatorConfig::default();

    // mock 一帧 state body，模拟 s0 publish 的 wire bytes。
    let now_ns: u64 = 200_000_000;
    let positions = [1.0, 2.0, 3.0];
    let body = build_joint_state(LAYOUT_ID, now_ns - 10_000, &positions, &[], &[], "");

    // 直接 parse_anchor_from_state_bytes（绕开 subscriber 路径）。
    let bias = parse_anchor_from_state_bytes(&body, 3, now_ns, 1_000_000).expect("parse anchor");
    assert_eq!(bias, vec![1.0, 2.0, 3.0]);

    // build_hold_command + verify 字段。
    let cmd_bytes = build_hold_command(&cfg, LAYOUT_ID, &bias, /* seq */ 5, now_ns);
    let cmd = root::<FbJointCommand<'_>>(&cmd_bytes).expect("verify");
    assert_eq!(cmd.layout_id(), LAYOUT_ID);
    assert_eq!(cmd.mode(), ControlMode::Position);
    assert_eq!(cmd.on_timeout(), StaleAction::Hold);
    assert_eq!(cmd.deadline_ns(), now_ns + cfg.deadline_offset_ns());
    assert_eq!(cmd.priority(), cfg.priority);
    assert_eq!(cmd.source_task_id(), 5);
    let target: Vec<f64> = cmd.target().unwrap().iter().collect();
    assert_eq!(target, vec![1.0, 2.0, 3.0]);
}

/// 验证 tick_once 在 cmd 周期未到时**不**发；hb 周期到时仍发（独立计时）。
#[test]
fn tick_once_independent_timers_for_cmd_and_hb() {
    // 把 hb_hz 调高到 100 Hz，cmd_hz 留 50；20 ms 内 hb 应发 1 次，cmd
    // 应不发（cmd 周期 = 20 ms，just-not-yet）。
    let cfg = CoordinatorConfig {
        heartbeat_hz: 100.0,
        command_hz: 50.0,
        ..CoordinatorConfig::default()
    };
    let cmd_ch = MockChannel::<markers::JointCommand>::new();
    let hb_ch = MockChannel::<markers::Heartbeat>::new();
    let mut cmd_pub = cmd_ch.publisher();
    let mut hb_pub = hb_ch.publisher();

    let now_ns: u64 = 5_000_000_000;
    let mut state = TickState::new(now_ns);

    // +12 ms：hb 周期 10 ms 已到 → 发；cmd 周期 20 ms 未到 → 不发。
    let r = tick_once(
        &cfg,
        &mut state,
        LAYOUT_ID,
        &[0.0],
        now_ns + 12_000_000,
        &mut hb_pub,
        &mut cmd_pub,
    )
    .expect("tick_once");
    assert_eq!(
        r,
        TickPublished {
            cmd: false,
            hb: true
        }
    );
    assert_eq!(cmd_ch.publishes(), 0);
    assert_eq!(hb_ch.publishes(), 1);
    assert_eq!(state.cmd_seq, 0);
    assert_eq!(state.hb_seq, 1);

    // +25 ms total（即 +13 ms 后再 tick）：hb 周期再到（25-12=13ms > 10ms）
    // → 发；cmd 周期 25ms > 20ms → 发。
    let r = tick_once(
        &cfg,
        &mut state,
        LAYOUT_ID,
        &[0.0],
        now_ns + 25_000_000,
        &mut hb_pub,
        &mut cmd_pub,
    )
    .expect("tick_once");
    assert_eq!(
        r,
        TickPublished {
            cmd: true,
            hb: true
        }
    );
    assert_eq!(cmd_ch.publishes(), 1);
    assert_eq!(hb_ch.publishes(), 2);
}
