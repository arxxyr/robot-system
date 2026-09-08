//! C4 分包验收：真实 daemon、独立临时实例和共享观测页，不以父进程调度推断监督时刻。

#![cfg(all(target_os = "linux", feature = "test-comm-fault-hook"))]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use robot_core::time::{RobotClock, SystemClock};
use robot_s0::comm_fault_observation::{ObservationPage, ObservationSnapshot};

static NEXT_INSTANCE: AtomicU32 = AtomicU32::new(0);

struct Process {
    child: Child,
    page: ObservationPage,
    directory: Option<tempfile::TempDir>,
    robot_id: String,
}

impl Process {
    fn spawn(joints: usize, extra: &[&str], pause: Option<&str>, fail: Option<&str>) -> Self {
        Self::spawn_with(joints, extra, pause, fail, false)
    }

    fn spawn_with(
        joints: usize,
        extra: &[&str],
        pause: Option<&str>,
        fail: Option<&str>,
        ipc_exit_before_enable: bool,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let robot_id = format!(
            "c4_start_{}_{}",
            std::process::id(),
            NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed)
        );
        let config = directory.path().join("config.toml");
        let names: Vec<_> = (0..joints).map(|index| format!("j{index}")).collect();
        let ids: Vec<_> = (1..=joints).collect();
        std::fs::write(&config, format!("[robot]\nid={robot_id:?}\nconfig_version=\"0.1.2\"\n[[hardware]]\nid=\"arm\"\nadapter=\"mock_serial_rt\"\njoints={names:?}\nmotor_ids={ids:?}\nauto_enable=false\n")).unwrap();
        if ipc_exit_before_enable {
            let contents = std::fs::read_to_string(&config)
                .unwrap()
                .replace("auto_enable=false", "auto_enable=true");
            std::fs::write(&config, contents).unwrap();
        }
        let latch = directory.path().join("latch");
        for name in ["latch", "latch.bak"] {
            std::fs::write(directory.path().join(name), b"0\n").unwrap();
        }
        let trust = directory.path().join("trust");
        std::fs::create_dir(&trust).unwrap();
        let observation = directory.path().join("observation");
        let page = ObservationPage::create(&observation).unwrap();
        let log = std::fs::File::create(directory.path().join("s0.log")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_s0_daemon"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("ROBOT_TEST_") {
                command.env_remove(key);
            }
        }
        command
            .env_remove("NOTIFY_SOCKET")
            .env_remove("INVOCATION_ID")
            .env("ROBOT_TEST_OBSERVATION_PATH", observation)
            .env("RUST_LOG", "info")
            .arg("--config")
            .arg(config)
            .args(["--hardware-id", "arm", "--latch-path"])
            .arg(latch)
            .arg("--rx-trust-dir")
            .arg(trust)
            .args(extra)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        if let Some(stage) = pause {
            command.env("ROBOT_TEST_BOOT_PAUSE_STAGE", stage);
        }
        if let Some(worker) = fail {
            command.env("ROBOT_TEST_BOOT_FAIL_WORKER", worker);
        }
        if ipc_exit_before_enable {
            command.env("ROBOT_TEST_IPC_EXIT_BEFORE_AUTO_ENABLE", "1");
        }
        let child = command.spawn().unwrap();
        Self {
            child,
            page,
            directory: Some(directory),
            robot_id,
        }
    }

    fn log_path(&self) -> PathBuf {
        self.directory.as_ref().unwrap().path().join("s0.log")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.log_path()).unwrap()
    }

    fn wait_exit(&mut self, timeout_ms: u64) -> i32 {
        let deadline = SystemClock.now().0 + timeout_ms * 1_000_000;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status
                    .code()
                    .unwrap_or_else(|| panic!("子进程意外被信号终止：{status}\n{}", self.log()));
            }
            assert!(
                SystemClock.now().0 < deadline,
                "等待退出超时\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_ready(&mut self, timeout_ms: u64) -> ObservationSnapshot {
        let deadline = SystemClock.now().0 + timeout_ms * 1_000_000;
        loop {
            let observed = self.page.snapshot();
            if observed.ready_reached && observed.supervisor_exit_ns != 0 {
                return observed;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "READY前退出\n{}",
                self.log()
            );
            assert!(
                SystemClock.now().0 < deadline,
                "等待READY超时：{observed:?}\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn stop(&mut self) {
        let pid = i32::try_from(self.child.id()).unwrap();
        // SAFETY：pid属于本对象仍持有且已确认存活的独立子进程。
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        assert_eq!(self.wait_exit(5000), 0);
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if std::thread::panicking()
            && let Some(directory) = self.directory.take()
        {
            eprintln!("失败证据保留：{}", directory.keep().display());
        }
    }
}

fn assert_initialized(observed: ObservationSnapshot) {
    assert!(
        observed.observation_ready,
        "不能拿零页当未执行证据：{observed:?}"
    );
    assert_ne!(observed.preinit_ns, 0);
    assert!(observed.main_start_ns >= observed.preinit_ns);
    assert!(
        observed.main_start_ns - observed.preinit_ns <= 10_000_000,
        "初始化边界到main的保守包络超过10ms：{observed:?}"
    );
}

#[test]
fn initialization_and_all_seven_workers_gate_first_bus_io() {
    for joints in [6, 13, 32] {
        let mut process = Process::spawn(joints, &[], None, None);
        let observed = process.wait_ready(10_500 + 200 * joints as u64);
        assert_initialized(observed);
        assert_eq!(observed.joint_count, joints as u64);
        assert_eq!(observed.worker_bitmap, 0x7f);
        assert_ne!(observed.spawned_ns, 0);
        assert!(observed.first_io_ns >= observed.spawned_ns);
        assert!(observed.preflight_finished_ns >= observed.first_io_ns);
        assert_eq!(observed.first_exit_ns, 0, "健康预检不产生故障 generation");
        assert!(observed.ready_ns >= observed.preflight_finished_ns);
        assert!(observed.supervisor_exit_ns >= observed.ready_ns);
        assert_eq!(observed.fired_checkpoint, 0);
        process.stop();
    }
}

#[test]
fn auto_enable_disconnected_exits_before_ready() {
    let mut process = Process::spawn_with(6, &[], None, None, true);
    assert_eq!(process.wait_exit(5000), 1);
    let observed = process.page.snapshot();
    assert_initialized(observed);
    assert_eq!(observed.worker_bitmap, 0x7f);
    assert_ne!(observed.preflight_finished_ns, 0);
    assert!(!observed.ready_reached);
    assert_ne!(observed.disabled_ns, 0);
    assert_eq!(observed.stop_bitmap, 0x7f);
    assert_eq!(observed.fired_checkpoint, 0);
    assert!(
        process.log().contains("auto-enable 的 IPC 接收端已退出"),
        "{}",
        process.log()
    );
}

#[test]
fn ipc_worker_exit_after_ready_terminates_standalone_daemon() {
    use iceoryx2::prelude::{NodeBuilder, ServiceName};
    use iceoryx2::service::ipc::Service;
    use robot_s0::comm_fault_hook::{HookCommandKind, TestCommFaultCommand};
    use robot_s0::worker_stop::WorkerKind;

    let mut process = Process::spawn(6, &[], None, None);
    process.wait_ready(5000);
    let node = NodeBuilder::new().create::<Service>().unwrap();
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
            sequence: 101,
            duration_ms: 0,
            count: 0,
            kind: HookCommandKind::RequestWorkerStop as u8,
            target: WorkerKind::Ipc as u8,
            error: 0,
            reserved: [0; 5],
        })
        .send()
        .unwrap();
    assert_eq!(process.wait_exit(5000), 1);
    let observed = process.page.snapshot();
    assert_eq!(observed.command_applied, 101, "必须由真实控制器消费注入");
    assert!(observed.ready_reached);
    assert_ne!(observed.disabled_ns, 0);
    assert_eq!(observed.stop_bitmap, 0x7f);
    assert_eq!(observed.fired_checkpoint, 0);
}

#[test]
fn each_worker_spawn_failure_stops_only_the_successful_prefix() {
    for (index, worker) in [
        "ipc", "clear_rr", "set_rr", "watchdog", "executor", "pub", "io",
    ]
    .into_iter()
    .enumerate()
    {
        let mut process = Process::spawn(6, &[], None, Some(worker));
        assert_eq!(process.wait_exit(5300), 1, "正常合作的worker不应拖到CP1");
        assert!(
            process
                .log()
                .contains(&format!("comm_fault_hook fail_worker={worker}"))
        );
        let observed = process.page.snapshot();
        assert_initialized(observed);
        assert_eq!(observed.worker_bitmap, (1_u64 << index) - 1);
        assert_eq!(observed.stop_bitmap, observed.worker_bitmap);
        assert_eq!(observed.spawned_ns, 0);
        assert_eq!(observed.first_io_ns, 0);
        assert!(!observed.ready_reached);
    }
}

#[test]
fn main_pause_before_spawn_is_enforced_by_cp1() {
    for stage in ["before_logging", "ipc_ready"] {
        let mut process = Process::spawn(6, &[], Some(stage), None);
        assert_eq!(process.wait_exit(5400), 88);
        assert!(
            process
                .log()
                .contains(&format!("comm_fault_hook stage={stage}"))
        );
        let observed = process.page.snapshot();
        assert_initialized(observed);
        assert_eq!(observed.fired_checkpoint, 1);
        assert!(
            (5_000_000_000..=5_100_000_000).contains(&(observed.fired_ns - observed.main_start_ns))
        );
        assert!(!observed.ready_reached);
        assert_eq!(observed.first_io_ns, 0);
    }
}

#[test]
fn main_pause_after_spawn_is_enforced_by_cp2() {
    for stage in ["after_spawn", "before_ready"] {
        let mut process = Process::spawn(6, &[], Some(stage), None);
        assert_eq!(process.wait_exit(12_000), 88);
        assert!(
            process
                .log()
                .contains(&format!("comm_fault_hook stage={stage}"))
        );
        let observed = process.page.snapshot();
        assert_initialized(observed);
        assert_eq!(observed.fired_checkpoint, 2);
        assert_eq!(observed.worker_bitmap, 0x7f);
        assert!(
            (11_700_000_000..=11_800_000_000)
                .contains(&(observed.fired_ns - observed.main_start_ns))
        );
        assert!(!observed.ready_reached);
    }
}

#[test]
fn missing_required_notify_socket_never_commits_ready() {
    let mut process = Process::spawn(6, &["--notify-mode", "required"], None, None);
    assert_eq!(process.wait_exit(5000), 1);
    let observed = process.page.snapshot();
    assert_initialized(observed);
    assert!(!observed.ready_reached);
    assert_eq!(observed.worker_bitmap, 0x7f);
    assert_eq!(observed.stop_bitmap, 0x7f);
    assert_ne!(observed.disabled_ns, 0);
}

#[test]
fn thirty_three_joints_reject_before_workers_or_bus_io() {
    let mut process = Process::spawn(33, &[], None, None);
    assert_eq!(process.wait_exit(2000), 2);
    let observed = process.page.snapshot();
    assert_initialized(observed);
    assert_eq!(observed.joint_count, 32);
    assert_eq!(observed.worker_bitmap, 0);
    assert_eq!(observed.first_io_ns, 0);
}
