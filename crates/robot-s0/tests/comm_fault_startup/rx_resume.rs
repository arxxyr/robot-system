//! 原 daemon 的 RX 交接验证；仅使用隔离目录、mock 执行器和真实 IPC。

use std::path::Path;

use iceoryx2::prelude::{NodeBuilder, ServiceName};
use iceoryx2::service::ipc::Service;
use robot_core::serial_services::{BusOpClass, CommErrorKind};
use robot_msg::{markers, msg};
use robot_s0::comm_fault_hook::{HookCommandKind, TestCommFaultCommand};
use robot_transport::iceoryx2::{TopicClass, subscriber};
use robot_transport::traits::LatestSubscriber;

use super::{
    Command, Duration, NEXT_INSTANCE, ObservationPage, Ordering, Process, RUNTIME_EXIT_TIMEOUT_MS,
    RobotClock, Stdio, SystemClock,
};

fn prepare(authorized: bool) -> (tempfile::TempDir, String) {
    let directory = tempfile::tempdir().unwrap();
    let robot_id = format!(
        "rx_resume_{}_{}",
        std::process::id(),
        NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed)
    );
    let root = directory.path();
    std::fs::write(
        root.join("config.toml"),
        format!(
            "[robot]\nid={robot_id:?}\nconfig_version=\"0.1.2\"\n\
             [[hardware]]\nid=\"arm\"\nadapter=\"mock_serial_rt\"\n\
             joints=[\"j0\",\"j1\",\"j2\"]\nmotor_ids=[1,2,3]\nauto_enable=false\n"
        ),
    )
    .unwrap();
    std::fs::create_dir(root.join("trust")).unwrap();
    for name in ["rx_trust_marker", "rx_trust_marker.bak"] {
        std::fs::write(
            root.join("trust").join(name),
            if authorized { b"0\n" } else { b"1\n" },
        )
        .unwrap();
    }
    for name in ["latch", "latch.bak"] {
        std::fs::write(root.join(name), b"1\n").unwrap();
    }
    (directory, robot_id)
}

fn spawn(
    directory: tempfile::TempDir,
    robot_id: String,
    resume: bool,
    fail_worker: Option<&str>,
) -> Process {
    let root = directory.path();
    let observation = root.join(format!(
        "observation-{}",
        NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed)
    ));
    let page = ObservationPage::create(&observation).unwrap();
    let log = std::fs::File::create(root.join("s0.log")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_robot-s0"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("ROBOT_TEST_") {
            command.env_remove(key);
        }
    }
    command
        .env_remove("NOTIFY_SOCKET")
        .env_remove("INVOCATION_ID")
        .env_remove("WATCHDOG_USEC")
        .env_remove("ROBOT_LOG_FORMAT")
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "info")
        .env("ROBOT_TEST_OBSERVATION_PATH", observation)
        .arg("--config")
        .arg(root.join("config.toml"))
        .args(["--hardware-id", "arm", "--latch-path"])
        .arg(root.join("latch"))
        .arg("--rx-trust-dir")
        .arg(root.join("trust"))
        .args(["--rx-trust-policy", "marker", "--notify-mode", "standalone"])
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    if resume {
        command.arg("--resume-rx-on-clean-exit");
    }
    if let Some(worker) = fail_worker {
        command.env("ROBOT_TEST_BOOT_FAIL_WORKER", worker);
    }
    Process {
        child: command.spawn().unwrap(),
        page,
        directory: Some(directory),
        robot_id,
    }
}

fn assert_markers(root: &Path, authorized: bool) {
    for name in ["rx_trust_marker", "rx_trust_marker.bak"] {
        assert_eq!(
            std::fs::read(root.join("trust").join(name)).unwrap(),
            if authorized { b"0\n" } else { b"1\n" },
            "RX 双文件必须符合当前交接状态"
        );
    }
    for name in ["latch", "latch.bak"] {
        assert_eq!(
            std::fs::read(root.join(name)).unwrap(),
            b"1\n",
            "RX 交接不得解除 EStop"
        );
    }
}

fn assert_reading_without_motion(process: &mut Process) {
    process.wait_ready(5000);
    let node = NodeBuilder::new().create::<Service>().unwrap();
    let mut states = subscriber::<markers::JointState>(
        &node,
        &format!("/{}/s0/arm/state", process.robot_id),
        TopicClass::S0State,
    )
    .unwrap();
    let deadline = SystemClock.now().0 + 5_000_000_000;
    let mut first = None;
    loop {
        assert!(
            process.child.try_wait().unwrap().is_none(),
            "{}",
            process.log()
        );
        assert!(
            SystemClock.now().0 < deadline,
            "未收到连续可信采样：{}",
            process.log()
        );
        if let Some(lease) = states.latest().unwrap() {
            let view = lease.view().unwrap();
            let state: msg::JointState = view.body.get();
            assert_eq!(state.s0_mode(), msg::S0Mode::Safe);
            assert_eq!(
                state.s0_safety_state(),
                msg::S0SafetyState::TorqueOffConfirmed
            );
            // READY 可早于第一份位置样本；仅跳过原预算内、尚无样本的启动空帧。
            if first.is_none()
                && state.stamp_ns() == 0
                && state
                    .position_rad()
                    .is_none_or(|positions| positions.is_empty())
            {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            let positions = state.position_rad().unwrap();
            assert_eq!(positions.len(), 3);
            assert!(positions.iter().all(f64::is_finite));
            assert!(state.stamp_ns() > 0);
            let stamp = state.stamp_ns();
            if first.is_some_and(|first| stamp > first) {
                break;
            }
            first = Some(stamp);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let observed = process.page.snapshot();
    assert_eq!(observed.serial_calls[0], 0, "不得执行使能等普通控制");
    assert!(observed.serial_calls[1] >= 2, "须真实执行周期读取");
    assert_eq!(observed.serial_calls[2], 0, "不得写动作目标");
    assert_markers(process.directory.as_ref().unwrap().path(), false);
}

fn stop_faulted(process: &mut Process) {
    let pid = i32::try_from(process.child.id()).unwrap();
    // SAFETY：只给本测试仍持有的独立 mock 子进程发送停止信号。
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    assert_eq!(process.wait_exit(RUNTIME_EXIT_TIMEOUT_MS), 1);
}

#[test]
fn clean_exit_resumes_next_boot_with_latch_and_real_feedback_preserved() {
    let (directory, robot_id) = prepare(true);
    let mut first = spawn(directory, robot_id.clone(), true, None);
    assert_reading_without_motion(&mut first);
    first.stop();
    assert_markers(first.directory.as_ref().unwrap().path(), true);
    let stopped = first.page.snapshot();
    assert_eq!(stopped.exit_residual_bitmap, 0);
    let directory = first.directory.take().unwrap();
    std::fs::rename(
        directory.path().join("s0.log"),
        directory.path().join("first.log"),
    )
    .unwrap();
    drop(first);

    // 第二次不写保护文件、不发维护授权，消费上一次干净退出的原授权。
    let mut second = spawn(directory, robot_id, true, None);
    assert_reading_without_motion(&mut second);
    second.stop();
    assert_markers(second.directory.as_ref().unwrap().path(), true);
}

#[test]
fn default_exit_keeps_dirty_maintenance_contract() {
    let (directory, robot_id) = prepare(true);
    let mut process = spawn(directory, robot_id, false, None);
    assert_reading_without_motion(&mut process);
    process.stop();
    assert_markers(process.directory.as_ref().unwrap().path(), false);
}

#[test]
fn initially_dirty_boot_is_not_automatically_authorized() {
    let (directory, robot_id) = prepare(false);
    let mut process = spawn(directory, robot_id, true, None);
    process.wait_ready(5000);
    assert_eq!(process.page.snapshot().serial_calls[1], 0);
    stop_faulted(&mut process);
    assert_markers(process.directory.as_ref().unwrap().path(), false);
}

#[test]
fn killed_running_daemon_does_not_leave_restart_authorization() {
    let (directory, robot_id) = prepare(true);
    let mut process = spawn(directory, robot_id, true, None);
    assert_reading_without_motion(&mut process);
    process.child.kill().unwrap();
    assert!(!process.child.wait().unwrap().success());
    assert_markers(process.directory.as_ref().unwrap().path(), false);
}

#[test]
fn startup_worker_failure_keeps_consumed_authorization_dirty() {
    let (directory, robot_id) = prepare(true);
    let mut process = spawn(directory, robot_id, true, Some("pub"));
    assert_eq!(process.wait_exit(5300), 1);
    assert!(!process.page.snapshot().ready_reached);
    assert!(process.log().contains("comm_fault_hook fail_worker=pub"));
    assert_markers(process.directory.as_ref().unwrap().path(), false);
}

#[test]
fn real_comm_fault_prevents_clean_restart_handoff() {
    let (directory, robot_id) = prepare(true);
    let mut process = spawn(directory, robot_id, true, None);
    assert_reading_without_motion(&mut process);
    let node = NodeBuilder::new().create::<Service>().unwrap();
    let mut states = subscriber::<markers::JointState>(
        &node,
        &format!("/{}/s0/arm/state", process.robot_id),
        TopicClass::S0State,
    )
    .unwrap();
    let name = ServiceName::new(&format!("/{}/test/comm_fault", process.robot_id)).unwrap();
    let service = node
        .service_builder(&name)
        .publish_subscribe::<TestCommFaultCommand>()
        .max_publishers(1)
        .max_subscribers(1)
        .history_size(1)
        .subscriber_max_buffer_size(8)
        .enable_safe_overflow(false)
        .open_or_create()
        .unwrap();
    let publisher = service.publisher_builder().create().unwrap();
    publisher
        .loan_uninit()
        .unwrap()
        .write_payload(TestCommFaultCommand {
            sequence: 201,
            duration_ms: 0,
            count: 1,
            kind: HookCommandKind::SerialResult as u8,
            target: BusOpClass::PeriodicRead as u8,
            error: CommErrorKind::Timeout.to_u8(),
            reserved: [0; 5],
        })
        .send()
        .unwrap();
    let deadline = SystemClock.now().0 + 5_000_000_000;
    loop {
        assert!(
            process.child.try_wait().unwrap().is_none(),
            "{}",
            process.log()
        );
        assert!(
            SystemClock.now().0 < deadline,
            "注入故障未进入收敛：{}",
            process.log()
        );
        if states.latest().unwrap().is_some_and(|lease| {
            let view = lease.view().unwrap();
            let state: msg::JointState = view.body.get();
            state.s0_mode() == msg::S0Mode::Fault
                && state.s0_fault_code() == msg::S0FaultCode::CommFault
        }) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(process.page.snapshot().command_applied, 201);
    stop_faulted(&mut process);
    assert_markers(process.directory.as_ref().unwrap().path(), false);
}
