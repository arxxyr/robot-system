//! `serial_rt_e2e` —— task-30 v9 SerialRT 端到端集成测试。
//!
//! ### 入口
//!
//! ```bash
//! cargo test -p robot-s0 --test serial_rt_e2e
//! ```
//!
//! ### 验证什么（reviewer 5 条红灯门槛）
//!
//! 1. **dispatch_chooses_serial_factory**：用 `mock_serial_rt` factory 拿到的
//!    adapter 走 `build_serial_rt` 入口（`build_hard_rt` 拒）；进 SerialRT 路径
//!    后 `info().rt_class == SerialRT`。
//! 2. **heartbeat_drives_enable_via_serial_ack**：auto `Event::Enable` →
//!    `Heartbeat` → `Ready → Enabled.Active` 是 `Effectful Enable(true)`；
//!    `serial_io_thread` 调 `adapter.enable(true)` 并写 `last_completed = Ok`，
//!    `ipc_tick` 才把 state commit 进 `Enabled.Active` 且 policy = `ActiveForward`。
//! 3. **estop_topic_preempts_and_safety_state_reflects_serial_confirm**：
//!    `EStop` topic 抢占（in-flight pending 也清），state → Safe，
//!    `safety_latched=true`；`serial_io_thread` 下 tick `confirm_torque_off`
//!    → `safety_state = TorqueOffConfirmed`，wire `S0SafetyState` 同步。
//! 4. **clear_estop_then_enable_again_no_safety_latch_reject**：`ClearEStop`
//!    走 `ipc_tick` Pure 分支调 `clear_safety_latch()`；再次 `Enable +
//!    Heartbeat` 不被 client / rt 二次 latch check 拒（无 `SafetyLatched`），
//!    state 重新进 `Enabled.Active`。
//! 5. **same_tick_enable_then_clear_estop_via_channel_does_not_misenable**：
//!    通过 ipc_thread 外部事件 channel 连续 send `[Enable, ClearEStop]`，
//!    Enable 必须看到 `estop_latched=true` 被拒（不能因 ClearEStop 提前清而
//!    误进 Ready）。本测试是 ipc_tick 单测 `events_in_same_tick_use_per_event_
//!    guard_view` 的**集成版**，钉死 `ipc_thread::run_loop` 不能恢复 batch
//!    update guards 模式。
//!
//! ### 不做的事
//!
//! - 不接真硬件（用 `MockSerialRTAdapter`）。
//! - 不接 SCHED_FIFO / mlockall（SerialRT 路径本来就不走 RT 线程）。
//! - 不接 systemd / signal（启停由 handle.request_stop 直接控制）。

#![cfg(target_os = "linux")]
#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::unbounded;
use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::adapter::{JointCommandBuf, RtClass, RtSnapshot};
use robot_core::executor_ctrl::{
    ExecutorCtrlClient, OutputPolicy, OutputSafetyState, emergency_codes,
};
use robot_core::factory::{AdapterBuildContext, find_factory};
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::build_helpers::{build_estop_event, build_heartbeat};
use robot_msg::markers;
use robot_s0::hard_rt_ctrl::{HardRtCtrlClient, HardRtCtrlInner};
use robot_s0::ipc_thread::{IpcSubscribers, IpcThreadConfig, IpcThreadHandle, spawn_ipc_thread};
use robot_s0::pub_thread::{IpcPublishers, PubThreadConfig, PubThreadHandle, spawn_pub_thread};
use robot_s0::serial_io_thread::{
    SerialIoThreadConfig, SerialIoThreadHandle, spawn_serial_io_thread,
};
use robot_s0::state_machine::{Event, Guards, S0State, S0StateSnapshot};
use robot_transport::iceoryx2::{Iceoryx2Publisher, TopicClass, publisher, subscriber};
use robot_transport::traits::Publisher;
use robot_transport::{LatestCommandSlot, RtSnapshotPublisher, RtSnapshotReader};
use tempfile::TempDir;

// ============================================================================
// Topic 命名（每测试唯一，避免并行 cargo test 冲突）
// ============================================================================

static TOPIC_SEQ: AtomicU32 = AtomicU32::new(0);

#[derive(Clone)]
struct Topics {
    command: String,
    heartbeat: String,
    estop: String,
    joint_state: String,
    s0_heartbeat: String,
}

fn fresh_topics() -> Topics {
    let id = TOPIC_SEQ.fetch_add(1, Ordering::Relaxed);
    let base = format!("/test_serial_rt_e2e_{}_{}", std::process::id(), id);
    Topics {
        command: format!("{base}/cmd"),
        heartbeat: format!("{base}/hb"),
        estop: format!("{base}/estop"),
        joint_state: format!("{base}/state"),
        s0_heartbeat: format!("{base}/s0hb"),
    }
}

// ============================================================================
// Harness：模拟 s0_daemon 的"build_adapter → spawn_*_thread"链路（SerialRT 路径）
// ============================================================================

fn healthy_guards() -> Guards {
    Guards {
        estop_latched: false,
        adapter_connected: true,
        calibration_loaded: true,
        joint_state_valid: true,
        power_ok: true,
        heartbeat_ok: false,
        cmd_deadline_valid: true,
        limits_valid: true,
        hw_enable_ok: true,
        adapter_fault_cleared: true,
        command_mode: robot_core::adapter::ControlMode::Position,
        on_timeout_hint: robot_s0::state_machine::EnabledSub::CtrlLostHold,
    }
}

#[allow(dead_code)]
struct Harness {
    ipc: IpcThreadHandle,
    serial: SerialIoThreadHandle,
    pub_h: PubThreadHandle,
    rt_ctrl: Arc<HardRtCtrlInner>,
    state_reader: RtSnapshotReader<S0StateSnapshot>,
    /// 测试线程 publish heartbeat / estop 用。
    cmd_pub: Iceoryx2Publisher<markers::JointCommand>,
    hb_pub: Iceoryx2Publisher<markers::Heartbeat>,
    estop_pub: Iceoryx2Publisher<markers::EStopEvent>,
    /// 用于把 `MockSerialRTAdapter` 的 inner 状态读回（看 enabled / safety_stopped 等）。
    /// 但本 harness 走 trait object，不直接持 Arc<MockSerialRTAdapter>；safety
    /// 信号通过 rt_ctrl 原子观察，无需 inner 引用。
    topics: Topics,
    /// **A1-2**：ClearEStop 走 `clear_estop_action::try_apply_clear_estop`
    /// 需要 `latch_path`；harness 自动创建 tempdir，drop 时清理。EStop 路径
    /// 走 ipc_thread 通用 hook fsync `latch=true`；ClearEStop 路径走 action
    /// handler fsync `latch=false`。
    _latch_dir: TempDir,
}

impl Harness {
    fn graceful_stop(self) {
        // 顺序：pub → serial（不再动 adapter）→ ipc → 测试 publishers drop
        self.pub_h.request_stop();
        let _ = self.pub_h.join();
        self.serial.request_stop();
        let _ = self.serial.join();
        self.ipc.request_stop();
        let _ = self.ipc.join();
    }
}

/// 起一份 SerialRT 端到端 harness。**关键差异**于 daemon：本 harness
/// auto 发 `Event::Enable`（不依赖 latch 文件 / signal），让 state machine
/// 直接进 SAFE→READY，配合 heartbeat 推进到 Enabled.Active。
///
/// `factory_name`：默认 `"mock_serial_rt"` 走 `build_serial_rt`；测试 #1 也
/// 会用本 helper 验证 dispatch 路径。
#[allow(clippy::too_many_lines)] // E2E harness 拼装 5 个线程 + iceoryx2 publishers
fn spawn_harness(factory_name: &str) -> Harness {
    let topics = fresh_topics();

    // ── 1) 用 inventory factory 走 build_serial_rt ──
    //
    // 这一步同时验证 reviewer 第 1 条门槛：mock_serial_rt factory 必须返
    // SerialRT trait object，且 info().rt_class == SerialRT。
    let factory = find_factory(factory_name).expect("factory not registered");
    let joint_names = ["j0".into(), "j1".into(), "j2".into()];
    let ctx = AdapterBuildContext {
        instance_name: "test_arm",
        adapter_kind: factory_name,
        joint_names: &joint_names,
        address: None,
        motor_ids: &[],
        calibration_paths: &[],
    };
    // **关键**：调 build_serial_rt 而不是 build / build_hard_rt。
    let adapter = factory
        .build_serial_rt(&ctx)
        .expect("factory does not support SerialRT path");
    assert_eq!(
        adapter.info().rt_class,
        RtClass::SerialRT,
        "build_serial_rt 返回的 adapter info.rt_class 必须是 SerialRT；\
         实际 {:?}",
        adapter.info().rt_class,
    );

    // ── 2) 共享 channel / atomic / seqlock / slot ──
    let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
    let (cmd_writer, cmd_reader) =
        LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
    let (rt_snap_pub, rt_snap_rdr) = RtSnapshotPublisher::<RtSnapshot>::new(RtSnapshot::default());
    let (state_pub, state_rdr) =
        RtSnapshotPublisher::<S0StateSnapshot>::new(S0StateSnapshot::default());
    let (_wd_tx, wd_rx) = unbounded::<Event>();

    let rt_ctrl = Arc::new(HardRtCtrlInner::new());
    let executor_ctrl: Arc<dyn ExecutorCtrlClient> =
        Arc::new(HardRtCtrlClient::new(Arc::clone(&rt_ctrl)));

    // ── 3) test 端 publishers（先 open，后 ipc_thread 内部 subscribers）──
    //
    // 测试线程的 Node leak 到 'static —— 否则 drop 后 iceoryx2 service 被回收，
    // ipc_thread 内还在订阅。
    let pub_node: &'static _ =
        Box::leak(Box::new(NodeBuilder::new().create::<IxService>().unwrap()));
    let cmd_pub =
        publisher::<markers::JointCommand>(pub_node, &topics.command, TopicClass::S0Command)
            .expect("open command publisher");
    let hb_pub =
        publisher::<markers::Heartbeat>(pub_node, &topics.heartbeat, TopicClass::Heartbeat)
            .expect("open heartbeat publisher");
    let estop_pub =
        publisher::<markers::EStopEvent>(pub_node, &topics.estop, TopicClass::SafetyEvent)
            .expect("open estop publisher");

    // ── 4) spawn ipc_thread ──
    //
    // **A1-2**：ClearEStop 走 `clear_estop_action::try_apply_clear_estop`，
    // 必须有 `latch_path`，否则会被 `ipc_tick` 的 `latch_path=None` 守门
    // 静默丢弃。harness 用 tempdir 自动 provision；EStop 路径走 ipc_thread
    // 通用 hook fsync `latch=true`，ClearEStop 路径走 action handler fsync
    // `latch=false`。drop 时 `_latch_dir` 自动清理整个临时目录。
    let latch_dir = tempfile::tempdir().expect("create latch tempdir");
    let latch_path = latch_dir.path().join("estop_latch");
    let ipc_cfg = IpcThreadConfig {
        poll_interval_ns: 500_000,
        initial_guards: healthy_guards(),
        initial_state: S0State::Safe,
        latch_path: Some(latch_path),
        // 段(3)：本 e2e 不发 command 帧（cmd_pub 仅保持 publisher 存活，
        // 驱动全走 heartbeat/estop）——值仅满足必填字段，沿测试惯例。
        authoritative_layout: 0xCAFE_0001,
    };
    let topics_for_ipc = topics.clone();
    let ipc_factory = move || {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let command = subscriber::<markers::JointCommand>(
            &node,
            &topics_for_ipc.command,
            TopicClass::S0Command,
        )
        .unwrap();
        let heartbeat = subscriber::<markers::Heartbeat>(
            &node,
            &topics_for_ipc.heartbeat,
            TopicClass::Heartbeat,
        )
        .unwrap();
        let estop = subscriber::<markers::EStopEvent>(
            &node,
            &topics_for_ipc.estop,
            TopicClass::SafetyEvent,
        )
        .unwrap();
        Box::leak(Box::new(node));
        IpcSubscribers {
            command,
            heartbeat,
            estop,
        }
    };
    let ipc = spawn_ipc_thread(
        ipc_cfg,
        Arc::clone(&clock),
        ipc_factory,
        cmd_writer,
        state_pub,
        wd_rx,
        Arc::clone(&executor_ctrl),
        // SerialRT e2e 测试不接 RR control plane（A1-3 (1/3c) 不在本套
        // harness 范围；A1-3 (3/3) 端到端时另起 harness）。
        None,
        // SetEnabled RR control plane（v0.5 候选 C）：本 harness 不接。
        None,
    )
    .expect("spawn_ipc_thread");

    // auto Event::Enable（s0_daemon 同款逻辑：daemon 启动期发一次让 SAFE→READY）
    let _ = ipc.event_sender().send(Event::Enable);

    // ── 5) spawn serial_io_thread（必须把 Box<dyn SerialRtAdapter + Send> 直接交进去）──
    let serial_cfg = SerialIoThreadConfig::default();
    let serial = spawn_serial_io_thread(
        serial_cfg,
        Arc::clone(&clock),
        adapter, // Box<dyn SerialRtAdapter + Send> —— spawn 内部 RtClass guard 通过
        cmd_reader,
        rt_snap_pub,
        Arc::clone(&rt_ctrl),
    )
    .expect("spawn_serial_io_thread");

    // ── 6) spawn pub_thread ──
    let pub_cfg = PubThreadConfig {
        state_hz: 200.0,
        heartbeat_hz: 200.0,
        ..PubThreadConfig::default()
    };
    let topics_for_pub = topics.clone();
    let pub_factory = move || {
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let joint_state = publisher::<markers::JointState>(
            &node,
            &topics_for_pub.joint_state,
            TopicClass::S0State,
        )
        .unwrap();
        let heartbeat = publisher::<markers::Heartbeat>(
            &node,
            &topics_for_pub.s0_heartbeat,
            TopicClass::Heartbeat,
        )
        .unwrap();
        Box::leak(Box::new(node));
        IpcPublishers {
            joint_state,
            heartbeat,
        }
    };
    let pub_h = spawn_pub_thread(
        pub_cfg,
        Arc::clone(&clock),
        pub_factory,
        rt_snap_rdr,
        state_rdr.clone(),
        Arc::clone(&rt_ctrl),
    )
    .expect("spawn_pub_thread");

    Harness {
        ipc,
        serial,
        pub_h,
        rt_ctrl,
        state_reader: state_rdr,
        cmd_pub,
        hb_pub,
        estop_pub,
        topics,
        _latch_dir: latch_dir,
    }
}

// ============================================================================
// 共用断言 helper
// ============================================================================

fn wait_until<F: FnMut() -> bool>(timeout: Duration, mut pred: F) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    pred()
}

fn publish_heartbeat(hb_pub: &mut Iceoryx2Publisher<markers::Heartbeat>, seq: u64) {
    let body = build_heartbeat(2, seq, 50.0, 1_000_000);
    let mut w = hb_pub.loan(1024 + body.len()).unwrap();
    w.set_seq(seq);
    w.set_source_id(2);
    w.set_stamp_ns(1_000_000);
    w.set_body_bytes(&body);
    w.publish().unwrap();
}

fn publish_estop(estop_pub: &mut Iceoryx2Publisher<markers::EStopEvent>, stamp_ns: u64) {
    let body = build_estop_event("e2e_test", stamp_ns, "test_publisher", true);
    let mut w = estop_pub.loan(1024 + body.len()).unwrap();
    w.set_seq(1);
    w.set_source_id(99);
    w.set_stamp_ns(stamp_ns);
    w.set_body_bytes(&body);
    w.publish().unwrap();
}

fn enabled_active() -> S0State {
    S0State::Enabled {
        sub: robot_s0::state_machine::EnabledSub::Active,
        control_link: robot_s0::state_machine::ControlLink::Ok,
        output_mode: robot_s0::state_machine::OutputMode::Active,
    }
}

// ============================================================================
// 红灯门槛 #1：dispatch_chooses_serial_factory
// ============================================================================

#[test]
fn test_serial_rt_dispatch_chooses_serial_factory() {
    // mock_serial_rt factory 必须 build_hard_rt = Unsupported（HardRT 路径拒），
    // build_serial_rt = Ok（SerialRT 路径通），保证 s0_daemon 三段 dispatch
    // 落到 SerialRT。
    let factory = find_factory("mock_serial_rt").expect("mock_serial_rt factory not registered");
    let joint_names = ["j0".into()];
    let ctx = AdapterBuildContext {
        instance_name: "test",
        adapter_kind: "mock_serial_rt",
        joint_names: &joint_names,
        address: None,
        motor_ids: &[],
        calibration_paths: &[],
    };

    // 1) build_hard_rt 必须返 Unsupported（factory 不 override）
    let hard_rt = factory.build_hard_rt(&ctx);
    assert!(
        matches!(
            hard_rt.as_ref().err(),
            Some(robot_core::error::RobotError::Unsupported(_))
        ),
        "mock_serial_rt factory.build_hard_rt 应返 Unsupported；实际 {:?}",
        hard_rt.as_ref().err(),
    );
    drop(hard_rt);

    // 2) build_serial_rt 必须 Ok 且 rt_class == SerialRT
    let serial_rt = factory
        .build_serial_rt(&ctx)
        .expect("mock_serial_rt factory.build_serial_rt should succeed");
    assert_eq!(serial_rt.info().rt_class, RtClass::SerialRT);
    assert_eq!(serial_rt.info().name, "mock_serial_rt");

    // 3) MockHardRT factory 反过来：build_hard_rt 通，build_serial_rt
    //    返 Unsupported（互斥保护，避免 mock_hard_rt 误走 SerialRT 路径）。
    let mock_hard = find_factory("mock_hard_rt").expect("mock_hard_rt factory not registered");
    let serial_via_hard = mock_hard.build_serial_rt(&ctx);
    assert!(
        matches!(
            serial_via_hard.as_ref().err(),
            Some(robot_core::error::RobotError::Unsupported(_))
        ),
        "mock_hard_rt factory.build_serial_rt 应返 Unsupported（HardRT 不应走 SerialRT 路径）；\
         实际 {:?}",
        serial_via_hard.as_ref().err(),
    );
}

// ============================================================================
// 红灯门槛 #2：heartbeat_drives_enable_via_serial_ack
// ============================================================================

#[test]
fn test_heartbeat_drives_enable_via_serial_ack() {
    let mut h = spawn_harness("mock_serial_rt");

    // auto Event::Enable 已经 send；先等 SAFE → READY
    assert!(
        wait_until(Duration::from_secs(2), || matches!(
            h.state_reader.load().state,
            S0State::Ready
        )),
        "auto Enable 后未观察到 SAFE→READY；当前 state={:?}",
        h.state_reader.load().state,
    );

    // 发 heartbeat → READY → Enabled.Active 是 Effectful Enable(true)
    publish_heartbeat(&mut h.hb_pub, 1);

    // 等 state 进 Enabled.Active —— 这要求 serial_io_thread 真上扭后 ack OK
    assert!(
        wait_until(Duration::from_secs(2), || h.state_reader.load().state
            == enabled_active()),
        "heartbeat 后 state 未进 Enabled.Active；当前 state={:?}",
        h.state_reader.load().state,
    );

    // policy 必须是 ActiveForward（ipc_tick Effectful ack OK 后 set_policy）
    let policy = OutputPolicy::from_u8(h.rt_ctrl.policy.load(Ordering::Acquire));
    assert_eq!(
        policy,
        OutputPolicy::ActiveForward,
        "Enable ack OK 后 policy 必须 ActiveForward；实际 {policy:?}"
    );
    // safety_state：rt_thread mock 反映 enabled=true → TorqueOn
    let safety = OutputSafetyState::from_u8(h.rt_ctrl.safety_state.load(Ordering::Acquire));
    assert!(
        matches!(
            safety,
            OutputSafetyState::TorqueOn | OutputSafetyState::TorqueOffConfirmed
        ),
        "enable 后 safety_state 应是 TorqueOn 或 TorqueOffConfirmed（mock 内部状态）；\
         实际 {safety:?}"
    );

    h.graceful_stop();
}

// ============================================================================
// 红灯门槛 #3：estop_topic_preempts_and_safety_state_reflects_serial_confirm
// ============================================================================

#[test]
fn test_estop_topic_preempts_and_safety_state_reflects_serial_confirm() {
    let mut h = spawn_harness("mock_serial_rt");

    // 先把 state 推到 Enabled.Active
    assert!(wait_until(Duration::from_secs(2), || matches!(
        h.state_reader.load().state,
        S0State::Ready
    )));
    publish_heartbeat(&mut h.hb_pub, 1);
    assert!(
        wait_until(Duration::from_secs(2), || h.state_reader.load().state
            == enabled_active()),
        "前置：未到 Enabled.Active"
    );

    // 注入 EStop topic
    publish_estop(&mut h.estop_pub, 42_000_000);

    // state 进 Safe + safety_latched=true（client.safety_stop 同步 store）
    assert!(
        wait_until(Duration::from_secs(2), || h.state_reader.load().state
            == S0State::Safe),
        "EStop 后未观察到 state→Safe；当前 {:?}",
        h.state_reader.load().state,
    );
    assert!(
        wait_until(Duration::from_secs(2), || h
            .rt_ctrl
            .safety_latched
            .load(Ordering::Acquire)),
        "EStop 后 safety_latched 必须 true",
    );

    // safety_state 由 serial_io_thread 下 tick confirm_torque_off 推进 ——
    // mock 收到 emergency 后 enabled=false，下一 tick read_torque_enable_mask
    // 返 [false; n] → safety_state = TorqueOffConfirmed。
    assert!(
        wait_until(Duration::from_secs(2), || matches!(
            OutputSafetyState::from_u8(h.rt_ctrl.safety_state.load(Ordering::Acquire)),
            OutputSafetyState::TorqueOffConfirmed
        )),
        "EStop 后 safety_state 必须由 SerialRT confirm 推进到 TorqueOffConfirmed；\
         实际 {:?}",
        OutputSafetyState::from_u8(h.rt_ctrl.safety_state.load(Ordering::Acquire))
    );

    // emergency_action 已被 rt 端 swap 走，此时应回到 NONE
    assert_eq!(
        h.rt_ctrl.emergency_action.load(Ordering::Acquire),
        emergency_codes::NONE,
        "rt 端应已消费 emergency_action 回归 NONE"
    );

    // policy = StopTorque（client.safety_stop 同步 + emergency 兜底）
    let policy = OutputPolicy::from_u8(h.rt_ctrl.policy.load(Ordering::Acquire));
    assert_eq!(
        policy,
        OutputPolicy::StopTorque,
        "EStop 后 policy 必须 StopTorque；实际 {policy:?}"
    );

    h.graceful_stop();
}

// ============================================================================
// 红灯门槛 #4：clear_estop_then_enable_again_no_safety_latch_reject
// ============================================================================

#[test]
fn test_clear_estop_then_enable_again_no_safety_latch_reject() {
    let mut h = spawn_harness("mock_serial_rt");

    // 1) 进 Enabled.Active
    assert!(wait_until(Duration::from_secs(2), || matches!(
        h.state_reader.load().state,
        S0State::Ready
    )));
    publish_heartbeat(&mut h.hb_pub, 1);
    assert!(wait_until(Duration::from_secs(2), || h
        .state_reader
        .load()
        .state
        == enabled_active()));

    // 2) EStop → Safe，safety_latched=true
    publish_estop(&mut h.estop_pub, 42_000_000);
    assert!(wait_until(Duration::from_secs(2), || h
        .state_reader
        .load()
        .state
        == S0State::Safe));
    assert!(wait_until(Duration::from_secs(2), || h
        .rt_ctrl
        .safety_latched
        .load(Ordering::Acquire)));

    // **task-30 v9 / SerialRT E2E #4 调用规范**：发 ClearEStop 之前必须等
    // SerialRT 端 confirm torque off（safety_state == TorqueOffConfirmed），
    // 否则 ipc_tick 的 #4 守门会保留 latch。这条等待是工程上合理的：操作员
    // / RPC 调 ClearEStop 时本就该看到 wire S0SafetyState=TorqueOffConfirmed。
    assert!(
        wait_until(Duration::from_secs(2), || matches!(
            OutputSafetyState::from_u8(h.rt_ctrl.safety_state.load(Ordering::Acquire)),
            OutputSafetyState::TorqueOffConfirmed
        )),
        "前置：safety_state 未升 TorqueOffConfirmed"
    );

    // 3) ClearEStop → ipc_tick Pure 分支（safety_state 守门通过）调
    //    clear_safety_latch
    let _ = h.ipc.event_sender().send(Event::ClearEStop);
    assert!(
        wait_until(Duration::from_secs(2), || !h
            .rt_ctrl
            .safety_latched
            .load(Ordering::Acquire)),
        "ClearEStop 后 executor.safety_latched 必须清；实际 latch 仍为 true"
    );

    // 4) 重新 Enable + heartbeat → state 必须能再回 Enabled.Active；
    //    若 client / rt 二次 latch check 还把 Enable(true) 拒，
    //    ipc_tick A3 会升 Fault(AdapterErr)。
    let _ = h.ipc.event_sender().send(Event::Enable);
    assert!(
        wait_until(Duration::from_secs(2), || matches!(
            h.state_reader.load().state,
            S0State::Ready
        )),
        "ClearEStop 后再 Enable 未到 Ready；实际 {:?}",
        h.state_reader.load().state
    );
    publish_heartbeat(&mut h.hb_pub, 2);
    assert!(
        wait_until(Duration::from_secs(2), || h.state_reader.load().state
            == enabled_active()),
        "ClearEStop + Enable + Heartbeat 后未回 Enabled.Active；实际 {:?}",
        h.state_reader.load().state
    );
    // 不应升 Fault
    assert!(
        !matches!(h.state_reader.load().state, S0State::Fault(_)),
        "Enable 不应被 SafetyLatched 拒升 Fault；实际 {:?}",
        h.state_reader.load().state
    );

    h.graceful_stop();
}

// ============================================================================
// 红灯门槛 #5：same_tick_enable_then_clear_estop_via_channel_does_not_misenable
// ============================================================================

#[test]
fn test_same_tick_enable_then_clear_estop_via_channel_does_not_misenable() {
    // 这条门槛是 ipc_tick 单测 events_in_same_tick_use_per_event_guard_view
    // 的**集成版**：通过真实 ipc_thread::run_loop 的外部事件 channel 验证
    // 修订后的"per-event apply"语义 *端到端* 生效（不只是 ipc_tick 内部）。

    let h = spawn_harness("mock_serial_rt");

    // 1) 先把 state 推到 Safe + estop_latched=true
    //    （注入 Event::EStop 走 ipc_tick emergency drain；
    //    ipc_tick 内部 apply_event_guard_side_effect 把 estop_latched 置 true）
    let _ = h.ipc.event_sender().send(Event::EStop);
    assert!(
        wait_until(Duration::from_secs(2), || h.state_reader.load().state
            == S0State::Safe
            && h.state_reader.load().guards_summary.estop_latched),
        "前置：未到 Safe + estop_latched=true；实际 {:?}",
        h.state_reader.load(),
    );
    // 等 SerialRT confirm 完成（同 #4 调用规范）
    assert!(
        wait_until(Duration::from_secs(2), || matches!(
            OutputSafetyState::from_u8(h.rt_ctrl.safety_state.load(Ordering::Acquire)),
            OutputSafetyState::TorqueOffConfirmed
        )),
        "前置：safety_state 未升 TorqueOffConfirmed"
    );

    // 2) 关键步骤：**同 tick** 内连续 send [Enable, ClearEStop]。
    //    crossbeam unbounded channel 是 FIFO，发送顺序 = 接收顺序 =
    //    EventQueue.push 顺序。如果 ipc_thread 走 batch update guards 模式，
    //    Enable 紧随 ClearEStop drain 时已被 estop_latched=false 污染 →
    //    plan(Safe, Enable, guards) 会通过 → state→Ready ❌。
    //    修订后 apply 在 ipc_tick 内逐 event 执行，Enable 看 estop_latched=true
    //    plan_transition 拒绝；ClearEStop 自己处理时再清 latch。
    let tx = h.ipc.event_sender();
    tx.send(Event::Enable).unwrap();
    tx.send(Event::ClearEStop).unwrap();

    // 3) 等足够时间让 ipc_thread 处理这两个事件 + 后续若干 tick
    std::thread::sleep(Duration::from_millis(50));

    // **关键断言**：state 必须留 Safe（Enable 不能误通过到 Ready）。
    let snap = h.state_reader.load();
    assert_eq!(
        snap.state,
        S0State::Safe,
        "[Enable, ClearEStop] 同 tick 入队，Enable 不应误通过到 Ready；\
         实际 state={:?}, estop_latched={}",
        snap.state,
        snap.guards_summary.estop_latched,
    );
    // ClearEStop 自己已处理 → guards.estop_latched=false + executor latch 已清
    assert!(
        !snap.guards_summary.estop_latched,
        "ClearEStop 应已被处理（estop_latched 应清）"
    );
    assert!(
        !h.rt_ctrl.safety_latched.load(Ordering::Acquire),
        "ClearEStop transition 应调 clear_safety_latch；实际 executor latch 仍 true"
    );

    h.graceful_stop();
}
