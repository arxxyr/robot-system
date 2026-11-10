//! `serial_rt_e2e` —— task-30 v9 SerialRT 端到端集成测试。
//!
//! ### 入口
//!
//! ```bash
//! cargo test -p robot-s0 --test serial_rt_e2e --features test-comm-fault-hook
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
//!    走真实 RR → 唯一 writer → A2 事务退休；再次 `Enable +
//!    Heartbeat` 经原权限预约/start/finish/commit，
//!    state 重新进 `Enabled.Active`。
//! 5. **same_tick_enable_then_clear_estop_via_channel_does_not_misenable**：
//!    通过 ipc_thread 外部事件 channel 连续 send `[Enable, ClearEStop]`，
//!    Enable 必须被原 latch 拒绝；无 RR 身份的 bare Clear 不再具有解除权限。
//!    原测试的“bare Clear 成功”断言按 W3 合同改为保持 latch；真实清除由第 4 场景验证。
//!
//! ### 不做的事
//!
//! - 不接真硬件（用 `MockSerialRTAdapter`）。
//! - 不接 SCHED_FIFO / mlockall（SerialRT 路径本来就不走 RT 线程）。
//! - 不接 systemd / signal（启停由 handle.request_stop 直接控制）。

#![cfg(target_os = "linux")]
#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded};
use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::adapter::{JointCommandBuf, RtClass, RtSnapshot};
use robot_core::executor_ctrl::{OutputPolicy, OutputSafetyState};
use robot_core::factory::{AdapterBuildContext, AdapterBuildServices, find_factory};
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::build_helpers::{build_estop_event, build_heartbeat};
use robot_msg::markers;
use robot_s0::hard_rt_ctrl::{HardRtCtrlClient, HardRtCtrlInner};
use robot_s0::ipc_thread::{
    IpcSpawnInputs, IpcSubscribers, IpcThreadConfig, IpcThreadHandle, OwnedIpcPorts, ReadyState,
    spawn_ipc_thread,
};
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

/// 父端只计一次真实用例；子进程输出独立保留，失败必 kill/reap，正常先真实 join。
fn supervised(name: &str) -> bool {
    const CHILD: &str = "ROBOT_SERIAL_W3_CASE";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        let owner = std::thread::current().id();
        std::panic::set_hook(Box::new(move |info| {
            eprintln!("SerialRT 验收失败：{info}");
            if std::thread::current().id() == owner {
                std::process::exit(101);
            }
        }));
        return false;
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("case.log");
    let log = std::fs::File::create(&path).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .env("TMPDIR", directory.path())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            result => {
                let _ = child.kill();
                let reap = Instant::now() + Duration::from_secs(2);
                loop {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        break;
                    }
                    if Instant::now() >= reap {
                        std::process::abort();
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                panic!("SerialRT 子进程失败，已 kill/reap：{result:?}");
            }
        }
    };
    assert!(
        status.success(),
        "{name}：{}",
        std::fs::read_to_string(path).unwrap()
    );
    true
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
        rt_serial: true,
        estop_latched: false,
        adapter_connected: false,
        calibration_loaded: true,
        joint_state_valid: true,
        power_ok: true,
        heartbeat_ok: false,
        cmd_deadline_valid: true,
        limits_valid: true,
        hw_enable_ok: true,
        adapter_fault_cleared: false,
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
    /// 真实锁/双文件/writer 事务所在目录，所有线程退出后由父子夹具清理。
    latch_dir: TempDir,
    _lock: robot_s0::latch_lock::LatchTargetLock,
    _standby: robot_s0::latch_writer::StandbyStore,
    writer: robot_s0::latch_writer::WriterHandle,
    writer_stop: Arc<TestWriterStop>,
    writer_shared: Arc<robot_s0::latch_writer::PersistShared>,
    clear_rr: robot_s0::rr_server_thread::RrServerThreadHandle,
    clear_client: robot_transport::iceoryx2::request_response::Iceoryx2RrClient<
        markers::ClearEStopRequest,
        markers::ClearEStopResponse,
    >,
    _clear_node: iceoryx2::prelude::Node<IxService>,
    clock: Arc<dyn RobotClock>,
}

struct TestWriterStop(AtomicBool);
impl robot_s0::latch_writer::WriterStop for TestWriterStop {
    fn running(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl Harness {
    fn clear_real_transaction(&self) {
        let now = self.clock.now().0;
        let body = robot_msg::build_helpers::build_clear_estop_request(
            "e2e-real-clear",
            "e2e-real-clear",
            "e2e",
            robot_msg::msg::RpcRole::Maintainer,
            "确认停止",
            "验收真实 writer 退休",
            false,
            now + 2_000_000_000,
            now,
        );
        let pending = self.clear_client.send_typed(&body).unwrap();
        let mut bytes = None;
        assert!(wait_until(Duration::from_secs(3), || {
            bytes = pending.try_recv().unwrap();
            bytes.is_some()
        }));
        let bytes = bytes.unwrap();
        let response = flatbuffers::root::<robot_msg::msg::ClearEStopResponse>(&bytes).unwrap();
        assert_eq!(response.request_id(), Some("e2e-real-clear"));
        assert_eq!(response.status(), robot_msg::msg::RpcStatus::Ok);
        assert!(response.retired());
        assert_eq!(response.effect(), robot_msg::msg::SideEffect::Committed);
        assert_eq!(
            std::fs::read(self.latch_dir.path().join("estop_latch")).unwrap(),
            b"0\n"
        );
        assert_eq!(
            std::fs::read(self.latch_dir.path().join("estop_latch.bak")).unwrap(),
            b"0\n"
        );
    }
    fn graceful_stop(self) {
        self.clear_rr.request_stop();
        assert!(wait_until(Duration::from_secs(2), || self
            .clear_rr
            .is_finished()));
        self.clear_rr.join().unwrap();
        // 先等 IPC 真正看见两个 RR sender 断链；没有裸字段封口。
        let tick = self.ipc.metrics().tick_count.load(Ordering::Relaxed);
        assert!(wait_until(Duration::from_secs(2), || self
            .ipc
            .metrics()
            .tick_count
            .load(Ordering::Relaxed)
            > tick + 2));
        self.ipc.request_stop();
        assert!(wait_until(Duration::from_secs(2), || self
            .ipc
            .is_finished()));
        let ipc_exit = self.ipc.join().unwrap();
        assert!(
            ipc_exit.is_quiescent(),
            "测试退出仍有原 IPC 请求责任：{ipc_exit:?}"
        );
        self.pub_h.request_stop();
        self.pub_h.join().unwrap();
        self.serial.request_stop();
        let executor_exit = self.serial.join().unwrap();
        assert!(!executor_exit.has_pending_enable());
        assert!(wait_until(Duration::from_secs(2), || self
            .writer_shared
            .obligation
            .view()
            .is_settled()));
        self.writer_stop.0.store(false, Ordering::Release);
        assert!(wait_until(Duration::from_secs(2), || self
            .writer
            .is_finished()));
        let (result, joined) = self.writer.join();
        result.unwrap();
        // 原 IPC/standby/锁与真实 join 凭证均持到全部工作线程已退出。
        drop((ipc_exit, joined));
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
    let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
    let base = clock.now();
    let authority = Arc::new(
        robot_core::control_authority::ControlAuthority::new(
            robot_core::control_authority::AuthorityInit::new(
                false,
                OutputSafetyState::TorqueUnknown,
                robot_core::control_authority::InhibitSet::EMPTY
                    .with(robot_core::control_authority::InhibitReason::Boot),
            ),
        )
        .unwrap(),
    );
    let rt_ctrl = Arc::new(HardRtCtrlInner::new_serial(authority.clone()));
    let probe = Arc::new(robot_s0::serial_preempt::SerialPreemptProbe::new(
        clock.clone(),
        rt_ctrl.clone(),
    ));
    let diag = Arc::new(robot_s0::diag::SerialDiagRing::new());
    let services = AdapterBuildServices {
        clock: clock.clone(),
        preempt: Some(probe.clone()),
        diag: Some(diag.clone()),
        call_deadline: robot_core::time::DurationNs(50_000_000),
    };
    let boot = Arc::new(robot_s0::boot_supervisor::BootShared::new(base));

    // ── 1) 用 inventory factory 走 build_serial_rt ──
    //
    // 这一步同时验证 reviewer 第 1 条门槛：mock_serial_rt factory 必须返
    // SerialRT trait object，且 info().rt_class == SerialRT。
    let factory = find_factory(factory_name).expect("factory not registered");
    let joint_names = ["j0".into(), "j1".into(), "j2".into()];
    let ctx = AdapterBuildContext {
        services: Some(&services),
        instance_name: "test_arm",
        adapter_kind: factory_name,
        joint_names: &joint_names,
        address: None,
        motor_ids: &[],
        calibration_paths: &[],
        calibration_range_tolerances_raw: &[],
        feetech_bus2_mixed_raw: false,
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
    let (cmd_writer, cmd_reader) =
        LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
    let (rt_snap_pub, rt_snap_rdr) = RtSnapshotPublisher::<RtSnapshot>::new(RtSnapshot::default());
    let (state_pub, state_rdr) =
        RtSnapshotPublisher::<S0StateSnapshot>::new(S0StateSnapshot::default());
    let (_wd_tx, wd_rx) = unbounded::<Event>();

    let executor_ctrl = Arc::new(HardRtCtrlClient::new(Arc::clone(&rt_ctrl)));

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
    // 持锁创建唯一 writer；EStop 义务和 Clear 文件事务均使用此实例。
    let latch_dir = tempfile::tempdir().expect("create latch tempdir");
    let latch_path = latch_dir.path().join("estop_latch");
    std::fs::write(&latch_path, b"0\n").unwrap();
    std::fs::write(latch_dir.path().join("estop_latch.bak"), b"0\n").unwrap();
    let lock = robot_s0::latch_lock::acquire_latch_lock(&latch_path).unwrap();
    let open = || {
        robot_s0::clear_estop_transaction::ClearTxnStore::open(&latch_path, "test", "test_arm")
            .unwrap()
    };
    let (slot, standby) = robot_s0::latch_writer::WriterPairing::new(open(), open(), &lock, base)
        .unwrap()
        .into_parts();
    let writer_shared = slot.shared().clone();
    match boot.install_runtime_for_test(authority, writer_shared.clone()) {
        Ok(ready) => drop(ready),
        Err(refused) => panic!("真实启动绑定拒绝：{:?}", refused.reason),
    }
    let (persist, endpoint) = robot_s0::latch_writer::mailbox(writer_shared.clone());
    let writer_stop = Arc::new(TestWriterStop(AtomicBool::new(true)));
    let writer = robot_s0::latch_writer::spawn_writer(
        slot,
        endpoint,
        robot_s0::latch_writer::WriterConfig::default(),
        writer_stop.clone(),
        clock.clone(),
        Box::new(robot_s0::latch_writer::NoHook),
    )
    .unwrap();
    let ipc_cfg = IpcThreadConfig {
        poll_interval_ns: 500_000,
        initial_guards: healthy_guards(),
        initial_state: S0State::Safe,
        // 段(3)：本 e2e 不发 command 帧（cmd_pub 仅保持 publisher 存活，
        // 驱动全走 heartbeat/estop）——值仅满足必填字段，沿测试惯例。
        authoritative_layout: 0xCAFE_0001,
        mixed_bus2_layout: None,
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
        Ok(OwnedIpcPorts::new(
            IpcSubscribers {
                command,
                heartbeat,
                estop,
            },
            node,
        ))
    };
    let profile = robot_transport::iceoryx2::RrProfile::S0_CONTROL;
    let (clear_high, clear_high_rx) = bounded(2 * profile.c);
    let (clear_normal, clear_normal_rx) = bounded(profile.c * (profile.l - 2));
    let (clear_response_tx, clear_response_rx) = bounded(profile.c * profile.l);
    let clear_topic = format!("{}/clear", topics.estop);
    let server_topic = clear_topic.clone();
    let mut clear_rr = robot_s0::rr_server_thread::spawn_rr_server_thread(
        robot_s0::rr_server_thread::RrServerThreadConfig {
            topic: clear_topic.clone(),
            ..robot_s0::rr_server_thread::RrServerThreadConfig::default()
        },
        clock.clone(),
        move || {
            use robot_s0::rr_server_thread::{OwnedRrServerPorts, RrInitError};
            let node = NodeBuilder::new()
                .create::<IxService>()
                .map_err(RrInitError::Node)?;
            let server = robot_transport::iceoryx2::rr_server_with::<
                markers::ClearEStopRequest,
                markers::ClearEStopResponse,
            >(&node, &server_topic, &profile)
            .map_err(RrInitError::Service)?;
            Ok(OwnedRrServerPorts::new(server, node))
        },
        robot_s0::rr_server_thread::TopicRequestSenders {
            high: clear_high,
            normal: clear_normal,
        },
        clear_response_rx,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert!(wait_until(Duration::from_secs(2), || matches!(
        clear_rr.poll_ready(),
        ReadyState::Ready
    )));
    let clear_node = NodeBuilder::new().create::<IxService>().unwrap();
    let clear_client = robot_transport::iceoryx2::rr_client_with::<
        markers::ClearEStopRequest,
        markers::ClearEStopResponse,
    >(&clear_node, &clear_topic, &profile)
    .unwrap();
    let mut ipc = spawn_ipc_thread(IpcSpawnInputs {
        cfg: ipc_cfg,
        clock: clock.clone(),
        subscribers_factory: ipc_factory,
        cmd_writer,
        state_pub,
        watchdog_rx: wd_rx,
        ctrl: executor_ctrl,
        boot: boot.clone(),
        persist,
        rr_channels: Some(robot_s0::ipc_thread::RrChannels {
            request_rx: clear_high_rx,
            normal_request_rx: clear_normal_rx,
            response_tx: clear_response_tx,
        }),
        set_enabled_rr_channels: None,
        stop: Arc::new(AtomicBool::new(false)),
    })
    .expect("spawn_ipc_thread");
    assert!(
        wait_until(Duration::from_secs(2), || matches!(
            ipc.poll_ready(),
            ReadyState::Ready
        )),
        "真实 factory/首次发布必须报告 ready"
    );

    // ── 5) spawn serial_io_thread（必须把 Box<dyn SerialRtAdapter + Send> 直接交进去）──
    let serial_cfg = SerialIoThreadConfig::default();
    let serial = spawn_serial_io_thread(
        serial_cfg,
        Arc::clone(&clock),
        adapter, // Box<dyn SerialRtAdapter + Send> —— spawn 内部 RtClass guard 通过
        cmd_reader,
        rt_snap_pub,
        Arc::clone(&rt_ctrl),
        probe,
        diag.clone(),
        true,
        boot.clone(),
        Arc::new(AtomicBool::new(false)),
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
    let mut pub_h = spawn_pub_thread(
        pub_cfg,
        Arc::clone(&clock),
        pub_factory,
        rt_snap_rdr,
        state_rdr.clone(),
        Arc::clone(&rt_ctrl),
        Some(diag),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .expect("spawn_pub_thread");
    assert!(
        wait_until(Duration::from_secs(2), || matches!(
            pub_h.poll_ready(),
            ReadyState::Ready
        )),
        "发布线程必须真实完成 factory 初始化"
    );

    boot.spawned_at_ns.store(clock.now().0, Ordering::Relaxed);
    boot.spawned_reached.store(true, Ordering::Release);
    assert!(wait_until(Duration::from_secs(2), || rt_ctrl
        .serial_boot_progress()
        .outcome
        == 1));
    let _ = ipc
        .event_sender()
        .control(robot_s0::event_ingress::ControlEvent::Enable);

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
        latch_dir,
        _lock: lock,
        _standby: standby,
        writer,
        writer_stop,
        writer_shared,
        clear_rr,
        clear_client,
        _clear_node: clear_node,
        clock,
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
    if supervised("test_serial_rt_dispatch_chooses_serial_factory") {
        return;
    }
    // mock_serial_rt factory 必须 build_hard_rt = Unsupported（HardRT 路径拒），
    // build_serial_rt = Ok（SerialRT 路径通），保证 s0_daemon 三段 dispatch
    // 落到 SerialRT。
    let factory = find_factory("mock_serial_rt").expect("mock_serial_rt factory not registered");
    let joint_names = ["j0".into()];
    let ctx = AdapterBuildContext {
        services: None,
        instance_name: "test",
        adapter_kind: "mock_serial_rt",
        joint_names: &joint_names,
        address: None,
        motor_ids: &[],
        calibration_paths: &[],
        calibration_range_tolerances_raw: &[],
        feetech_bus2_mixed_raw: false,
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
    if supervised("test_heartbeat_drives_enable_via_serial_ack") {
        return;
    }
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
    let safety = h.rt_ctrl.safety_state();
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
    if supervised("test_estop_topic_preempts_and_safety_state_reflects_serial_confirm") {
        return;
    }
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
        wait_until(Duration::from_secs(2), || h.rt_ctrl.safety_latched()),
        "EStop 后 safety_latched 必须 true",
    );

    // safety_state 由 serial_io_thread 下 tick confirm_torque_off 推进 ——
    // mock 收到 emergency 后 enabled=false，下一 tick read_torque_enable_mask
    // 返 [false; n] → safety_state = TorqueOffConfirmed。
    assert!(
        wait_until(Duration::from_secs(2), || matches!(
            h.rt_ctrl.safety_state(),
            OutputSafetyState::TorqueOffConfirmed
        )),
        "EStop 后 safety_state 必须由 SerialRT confirm 推进到 TorqueOffConfirmed；\
         实际 {:?}",
        h.rt_ctrl.safety_state()
    );

    assert!(
        h.serial
            .metrics()
            .emergency_safety_stop_count
            .load(Ordering::Relaxed)
            >= 2,
        "启动及急停均必须执行真实 Safety"
    );
    assert!(
        h.serial.metrics().confirm_off_count.load(Ordering::Relaxed) >= 2,
        "急停必须再次确认，不能使用启动已有状态"
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
    if supervised("test_clear_estop_then_enable_again_no_safety_latch_reject") {
        return;
    }
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
        .safety_latched()));

    // **task-30 v9 / SerialRT E2E #4 调用规范**：发 ClearEStop 之前必须等
    // SerialRT 端 confirm torque off（safety_state == TorqueOffConfirmed），
    // 否则 ipc_tick 的 #4 守门会保留 latch。这条等待是工程上合理的：操作员
    // / RPC 调 ClearEStop 时本就该看到 wire S0SafetyState=TorqueOffConfirmed。
    assert!(
        wait_until(Duration::from_secs(2), || matches!(
            h.rt_ctrl.safety_state(),
            OutputSafetyState::TorqueOffConfirmed
        )),
        "前置：safety_state 未升 TorqueOffConfirmed"
    );

    // 3) 真实 RR/peer 槽 → IPC 原 owner → 唯一 writer → 真退休 typed 回复。
    assert!(wait_until(Duration::from_secs(2), || h
        .writer_shared
        .obligation
        .is_settled()));
    h.clear_real_transaction();
    assert!(!h.rt_ctrl.safety_latched());

    // 4) 重新 Enable + heartbeat → state 必须能再回 Enabled.Active；
    //    若 client / rt 二次 latch check 还把 Enable(true) 拒，
    //    ipc_tick A3 会升 Fault(AdapterErr)。
    let completed_before = h
        .serial
        .metrics()
        .normal_completed_count
        .load(Ordering::Acquire);
    let (previous_seq, _) = robot_core::executor_ctrl::unpack_completed(
        h.rt_ctrl.last_completed.load(Ordering::Acquire),
    );
    h.ipc
        .event_sender()
        .control(robot_s0::event_ingress::ControlEvent::Enable)
        .expect("重新 Enable 必须真实进入原控制队列");
    // 原 heartbeat 仍可有效，真实提交可在同一 IPC tick 越过 Ready；不要求父线程采到瞬态。
    publish_heartbeat(&mut h.hb_pub, 2);
    assert!(
        wait_until(Duration::from_secs(2), || {
            h.state_reader.load().state == enabled_active()
                && h.serial
                    .metrics()
                    .normal_completed_count
                    .load(Ordering::Acquire)
                    > completed_before
        }),
        "ClearEStop + Enable + Heartbeat 后未回 Enabled.Active；实际 {:?}",
        h.state_reader.load().state
    );
    let (new_seq, response) = robot_core::executor_ctrl::unpack_completed(
        h.rt_ctrl.last_completed.load(Ordering::Acquire),
    );
    assert_ne!(
        new_seq, previous_seq,
        "必须完成真实的新原序号，不能复用急停前的 Enable"
    );
    assert_eq!(response, robot_core::executor_ctrl::AdapterResp::Ok);
    assert!(!h.rt_ctrl.safety_latched());
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
    if supervised("test_same_tick_enable_then_clear_estop_via_channel_does_not_misenable") {
        return;
    }
    // 原第 5 场景迁移：同一来源的 Enable/bare Clear 均不能借其它事件
    // 的新 guard 视图绕过 A2；bare Clear 没有 RR 身份，永远不能解除 latch。

    let h = spawn_harness("mock_serial_rt");

    // 1) 先把 state 推到 Safe + estop_latched=true
    //    （注入 Event::EStop 走 ipc_tick emergency drain；
    //    ipc_tick 内部 apply_event_guard_side_effect 把 estop_latched 置 true）
    h.ipc.event_sender().estop();
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
            h.rt_ctrl.safety_state(),
            OutputSafetyState::TorqueOffConfirmed
        )),
        "前置：safety_state 未升 TorqueOffConfirmed"
    );

    // 2) 同一来源连续送入两个旧事件；无论落在同一或相邻 tick，
    // Enable 不能恢复权限，bare Clear 也不能变成有凭证的事务。
    let tx = h.ipc.event_sender();
    tx.control(robot_s0::event_ingress::ControlEvent::Enable)
        .unwrap();
    tx.control(robot_s0::event_ingress::ControlEvent::ClearEStop)
        .unwrap();

    // 3) 观察真实接收数，避免仅靠固定 sleep 声称事件已经处理。
    assert!(wait_until(Duration::from_secs(2), || h
        .ipc
        .metrics()
        .external_events
        .load(Ordering::Relaxed)
        >= 3));
    let tick = h.ipc.metrics().ipc_tick_count.load(Ordering::Relaxed);
    assert!(wait_until(Duration::from_secs(2), || h
        .ipc
        .metrics()
        .ipc_tick_count
        .load(Ordering::Relaxed)
        > tick + 2));

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
    // 无原 RR 身份的 bare Clear 无权限，既不能改内存 latch，也不能改磁盘。
    assert!(snap.guards_summary.estop_latched);
    assert!(h.rt_ctrl.safety_latched());
    assert!(wait_until(Duration::from_secs(2), || h
        .writer_shared
        .obligation
        .is_settled()));
    assert_eq!(
        std::fs::read(h.latch_dir.path().join("estop_latch")).unwrap(),
        b"1\n"
    );
    assert_eq!(
        std::fs::read(h.latch_dir.path().join("estop_latch.bak")).unwrap(),
        b"1\n"
    );

    h.graceful_stop();
}
