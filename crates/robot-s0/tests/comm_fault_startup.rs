//! C4 分包验收：真实 daemon、独立临时实例和共享观测页，不以父进程调度推断监督时刻。

#![cfg(all(target_os = "linux", feature = "test-comm-fault-hook"))]

#[path = "comm_fault_startup/camera_actuator.rs"]
mod camera_actuator;

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use robot_core::time::{RobotClock, SystemClock};
use robot_s0::boot_supervisor::{RUNTIME_SHUTDOWN_BUDGET, SHUTDOWN_MARGIN, absolute_boot_budget};
use robot_s0::comm_fault_observation::{ObservationPage, ObservationSnapshot};
use robot_s0::shutdown_mechanism::HandleKind;
use robot_s0::worker_stop::WorkerKind;

static NEXT_INSTANCE: AtomicU32 = AtomicU32::new(0);

// 父进程只留原 D 加既有回收余量；真实退出仍须满足下方 L/D 时间包络。
const RUNTIME_EXIT_TIMEOUT_MS: u64 = (RUNTIME_SHUTDOWN_BUDGET.0 + SHUTDOWN_MARGIN.0) / 1_000_000;
const IPC_NO_ACK_RESIDUAL: u64 = (HandleKind::Executor.bit()
    | HandleKind::Watchdog.bit()
    | HandleKind::LatchWriter.bit()) as u64;
const HELPER_STOP_BITMAP: u64 = (1 << WorkerKind::ClearEStopRr as u8)
    | (1 << WorkerKind::SetEnabledRr as u8)
    | (1 << WorkerKind::Pub as u8)
    | (1 << WorkerKind::Io as u8);

#[derive(Clone, Copy)]
enum CameraFixture {
    Frames,
    Eof,
    NoFrames,
}

impl CameraFixture {
    fn name(self) -> &'static str {
        match self {
            Self::Frames => "frames",
            Self::Eof => "eof",
            Self::NoFrames => "no-frames",
        }
    }
}

// 仅标准库协议夹具；由真实 OpenCvCameraAdapter 启动，不导入 cv2 或打开设备。
const CAMERA_HELPER: &str = r"
import os
from pathlib import Path
import signal
import struct
import sys
import time

directory = Path(__file__).parent
stream_index = int(directory.name.rsplit('-', 1)[1])
mode = Path(sys.argv[sys.argv.index('--device') + 1]).name

def stop(_signum, _frame):
    raise SystemExit(0)

signal.signal(signal.SIGTERM, stop)
try:
    (directory / 'camera-pid').write_text(str(os.getpid()))
    while mode == 'no-frames' or not (directory / 'camera-go').exists():
        time.sleep(0.01)
    if mode == 'eof':
        raise SystemExit(0)
    while True:
        if (directory / 'camera-stall').exists():
            while True:
                time.sleep(0.01)
        stamp = time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW)
        if not (directory / 'camera-first-frame').exists():
            (directory / 'camera-first-frame').write_text(str(stamp))
        sys.stdout.buffer.write(struct.pack('<4sIIIQ', b'RCAM', 2, 1, 6, stamp) + bytes(range(stream_index + 1, stream_index + 7)))
        sys.stdout.buffer.flush()
        time.sleep(0.05)
finally:
    (directory / 'camera-stopped').write_text('stopped')
";

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
        Self::spawn_inner(joints, extra, pause, fail, ipc_exit_before_enable, &[], &[])
    }

    /// S48 A3b：standalone（显式移除 `WATCHDOG_USEC` / `NOTIFY_SOCKET`）并注入
    /// 额外 `ROBOT_TEST_*` 环境变量，用于快照版本耗尽回归。
    fn spawn_standalone_with_env(joints: usize, envs: &[(&str, String)]) -> Self {
        Self::spawn_inner(joints, &[], None, None, false, envs, &[])
    }

    fn spawn_with_camera(camera: CameraFixture) -> Self {
        Self::spawn_inner(6, &[], None, None, false, &[], &[camera])
    }

    fn spawn_inner(
        joints: usize,
        extra: &[&str],
        pause: Option<&str>,
        fail: Option<&str>,
        ipc_exit_before_enable: bool,
        envs: &[(&str, String)],
        cameras: &[CameraFixture],
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
        for (index, camera) in cameras.iter().enumerate() {
            let python = PathBuf::from("/usr/bin/python3");
            assert!(python.is_file(), "相机协议夹具需要固定 /usr/bin/python3");
            let camera_directory = directory.path().join(format!("camera-{index}"));
            std::fs::create_dir(&camera_directory).unwrap();
            let helper = camera_directory.join("camera_fixture.py");
            let device = camera_directory.join(camera.name());
            std::fs::write(&helper, CAMERA_HELPER).unwrap();
            let mut contents = std::fs::read_to_string(&config).unwrap();
            // TOML Value 的格式化保留字符串引号和转义，不能改成路径 Display。
            let device = toml::Value::String(device.to_str().unwrap().to_owned());
            let python = toml::Value::String(python.to_str().unwrap().to_owned());
            let helper = toml::Value::String(helper.to_str().unwrap().to_owned());
            write!(
                contents,
                "\n[[sensor]]\nadapter=\"opencv_v4l2\"\nhardware_id=\"arm\"\nid=\"fixture_camera_{index}\"\ndevice={device}\npython={python}\nhelper={helper}\nwidth=2\nheight=1\nfps=20\n"
            )
            .unwrap();
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
            .env_remove("ROBOT_LOG_FORMAT")
            .env("NO_COLOR", "1")
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
        command.env_remove("WATCHDOG_USEC");
        for (key, value) in envs {
            command.env(key, value);
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

    fn camera_file(&self, name: &str) -> PathBuf {
        self.camera_file_at(0, name)
    }

    fn camera_file_at(&self, index: usize, name: &str) -> PathBuf {
        self.directory
            .as_ref()
            .unwrap()
            .path()
            .join(format!("camera-{index}"))
            .join(name)
    }

    fn wait_camera_started(&mut self) {
        self.wait_camera_started_at(0);
    }

    fn wait_camera_started_at(&mut self, index: usize) {
        let deadline = SystemClock.now().0 + 5_000_000_000;
        while !self.camera_file_at(index, "camera-pid").is_file() {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "相机 helper 启动前 daemon 退出：{}",
                self.log()
            );
            assert!(
                SystemClock.now().0 < deadline,
                "等待相机 helper 超时：{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn assert_camera_reaped(&self) {
        self.assert_camera_reaped_at(0);
    }

    fn assert_camera_reaped_at(&self, index: usize) {
        assert!(
            self.camera_file_at(index, "camera-stopped").is_file(),
            "相机 helper 必须走正常停止 finally：{}",
            self.log()
        );
        let pid: u32 = std::fs::read_to_string(self.camera_file_at(index, "camera-pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            !PathBuf::from(format!("/proc/{pid}")).exists(),
            "相机 helper {pid} 尚未回收：{}",
            self.log()
        );
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.log_path()).unwrap()
    }

    fn wait_exit(&mut self, timeout_ms: u64) -> i32 {
        let deadline = SystemClock.now().0 + timeout_ms * 1_000_000;
        self.wait_exit_until(deadline)
    }

    fn wait_exit_until(&mut self, deadline: u64) -> i32 {
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

    fn wait_snapshot_exhaustion_exit(&mut self) -> i32 {
        // READY 后的耗尽等待仍限原 15 秒；运行监督就绪时间不是关闭武装时间。
        let exhaustion_deadline = SystemClock.now().0 + 15_000_000_000;
        loop {
            // 先检查退出再读页，避免退出前最后一次 ticket 发布与读页交错而误拒。
            let status = self.child.try_wait().unwrap();
            let observed = self.page.snapshot();
            assert!(
                SystemClock.now().0 < exhaustion_deadline,
                "等待快照耗尽关闭 ticket 超时：{observed:?}\n{}",
                self.log()
            );
            if observed.shutdown_deadline_ns != 0 {
                // 该字段来自真实 arm 返回的原 ticket；只加既有回收余量，
                // 不能从父进程观察到它的时刻重新开始 10 秒预算。
                let deadline = observed
                    .shutdown_deadline_ns
                    .checked_add(SHUTDOWN_MARGIN.0)
                    .expect("真实关闭期限加回收余量不得溢出");
                return self.wait_exit_until(deadline);
            }
            assert!(
                status.is_none(),
                "子进程退出但未导出真实关闭 ticket：{status:?} {observed:?}\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_ready(&mut self, timeout_ms: u64) -> ObservationSnapshot {
        let deadline = SystemClock.now().0 + timeout_ms * 1_000_000;
        loop {
            // 先核存活与截止，再读 READY 页；历史 READY 页本身不证明事件顺序，
            // 跨线程顺序由 worker 记录的耗尽尝试时刻与真实 ready_ns 比较证明。
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "READY前退出\n{}",
                self.log()
            );
            let observed = self.page.snapshot();
            assert!(
                SystemClock.now().0 < deadline,
                "等待READY超时：{observed:?}\n{}",
                self.log()
            );
            if observed.ready_reached && observed.runtime_armed_ns != 0 {
                return observed;
            }
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

/// `first` 首次出现必须早于 `second` 首次出现（两者都必须存在）。
fn assert_log_order(log: &str, first: &str, second: &str) {
    let first_at = log
        .find(first)
        .unwrap_or_else(|| panic!("缺少 {first:?}：{log}"));
    let second_at = log
        .find(second)
        .unwrap_or_else(|| panic!("缺少 {second:?}：{log}"));
    assert!(first_at < second_at, "{first:?} 必须早于 {second:?}：{log}");
}

fn assert_initialized(observed: &ObservationSnapshot) {
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

/// 同域故障锚早于 runtime 武装；父进程真回收只提供外部时间包络，不冒充 D/L 观测。
fn assert_ipc_no_ack_exit_time(trigger_ns: u64, reaped_ns: u64) {
    let elapsed = reaped_ns.checked_sub(trigger_ns).unwrap();
    let earliest = RUNTIME_SHUTDOWN_BUDGET.0 - 250_000_000;
    let latest = RUNTIME_SHUTDOWN_BUDGET.0 + 100_000_000;
    assert!(
        (earliest..=latest).contains(&elapsed),
        "IPC 无 ACK 必须等原 L，且不越过原 D 加 100ms 迟到容限：elapsed={elapsed}"
    );
}

#[test]
fn initialization_and_all_eight_workers_gate_first_bus_io() {
    for joints in [6, 13, 32] {
        let mut process = Process::spawn(joints, &[], None, None);
        let observed = process.wait_ready(10_500 + 200 * joints as u64);
        assert_initialized(&observed);
        assert_eq!(observed.joint_count, joints as u64);
        assert_eq!(observed.worker_bitmap, 0xff);
        assert_ne!(observed.spawned_ns, 0);
        assert!(observed.first_io_ns >= observed.spawned_ns);
        assert!(observed.preflight_finished_ns >= observed.first_io_ns);
        assert_eq!(observed.first_exit_ns, 0, "健康预检不产生故障 generation");
        assert!(observed.ready_ns >= observed.preflight_finished_ns);
        assert!(observed.runtime_armed_ns >= observed.ready_ns);
        assert_eq!(observed.supervisor_exit_ns, 0);
        assert_eq!(observed.fired_checkpoint, 0);
        process.stop();
    }
}

#[test]
fn configured_camera_commits_ready_only_after_a_real_helper_frame() {
    let mut process = Process::spawn_with_camera(CameraFixture::Frames);
    process.wait_camera_started();
    let before = process.page.snapshot();
    assert!(
        !before.ready_reached,
        "尚未放行首帧，不能 READY：{before:?}"
    );
    assert_eq!(before.ready_ns, 0);
    std::fs::write(process.camera_file("camera-go"), b"go").unwrap();
    let ready = process.wait_ready(5000);
    let frame_ns: u64 = std::fs::read_to_string(process.camera_file("camera-first-frame"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(frame_ns >= ready.spawned_ns);
    assert!(
        ready.ready_ns >= frame_ns,
        "首帧必须早于真实 READY：frame={frame_ns} {ready:?}"
    );
    process.stop();
    let stopped = process.page.snapshot();
    assert_eq!(stopped.stop_bitmap, 0xff);
    assert_eq!(stopped.residual_bitmap, 0);
    assert_eq!(stopped.exit_residual_bitmap, 0);
    process.assert_camera_reaped();
}

#[test]
fn camera_helper_eof_before_first_frame_exits_without_ready_and_reaps() {
    let mut process = Process::spawn_inner(
        6,
        &[],
        None,
        None,
        false,
        &[],
        &[
            CameraFixture::Frames,
            CameraFixture::Eof,
            CameraFixture::Frames,
        ],
    );
    for index in 0..3 {
        process.wait_camera_started_at(index);
    }
    std::fs::write(process.camera_file_at(1, "camera-go"), b"go").unwrap();
    assert_eq!(process.wait_exit(5000), 1);
    let observed = process.page.snapshot();
    assert_initialized(&observed);
    assert!(!observed.ready_reached);
    assert_eq!(observed.ready_ns, 0);
    assert_eq!(observed.runtime_armed_ns, 0);
    assert_eq!(observed.stop_bitmap, 0xff);
    assert_eq!(observed.exit_residual_bitmap, 0);
    assert!(
        process.log().contains("相机首帧初始化失败，拒绝READY"),
        "{}",
        process.log()
    );
    for index in 0..3 {
        process.assert_camera_reaped_at(index);
    }
}

#[test]
fn camera_without_frames_exits_at_original_startup_deadline_without_ready_and_reaps() {
    let mut process = Process::spawn_inner(
        6,
        &[],
        None,
        None,
        false,
        &[],
        &[
            CameraFixture::Frames,
            CameraFixture::NoFrames,
            CameraFixture::Frames,
        ],
    );
    for index in 0..3 {
        process.wait_camera_started_at(index);
        std::fs::write(process.camera_file_at(index, "camera-go"), b"go").unwrap();
    }
    let timeout_ms = (absolute_boot_budget(6).0 + SHUTDOWN_MARGIN.0) / 1_000_000;
    assert_eq!(process.wait_exit(timeout_ms), 1);
    let observed = process.page.snapshot();
    assert_initialized(&observed);
    assert!(!observed.ready_reached);
    assert_eq!(observed.ready_ns, 0);
    assert_eq!(observed.runtime_armed_ns, 0);
    assert_eq!(observed.stop_bitmap, 0xff);
    assert_eq!(observed.exit_residual_bitmap, 0);
    assert_eq!(observed.fired_checkpoint, 0);
    let elapsed = observed.before_process_exit_ns - observed.main_start_ns;
    assert!(
        (absolute_boot_budget(6).0..=absolute_boot_budget(6).0 + SHUTDOWN_MARGIN.0)
            .contains(&elapsed),
        "无首帧必须沿原启动截止退出，不能提前或重启预算：{observed:?}"
    );
    assert!(
        process.log().contains("启动物理/ACK 截止失败"),
        "{}",
        process.log()
    );
    for index in 0..3 {
        process.assert_camera_reaped_at(index);
    }
}

#[test]
fn initial_history_not_applied_keeps_executor_unspawned_under_original_checkpoint() {
    let mut process = Process::spawn_standalone_with_env(
        6,
        &[("ROBOT_TEST_INITIAL_COMMAND_SEQUENCE", "4242".to_owned())],
    );
    // 没有该序号的真实 publisher：不能用已创建 subscriber 冒充 initial_applied。
    let code = process.wait_exit(5300);
    assert!(matches!(code, 1 | 88), "{code}: {}", process.log());
    let observed = process.page.snapshot();
    assert_initialized(&observed);
    assert!(!observed.hook_ready);
    assert_eq!(observed.command_applied, 0);
    assert_eq!(
        observed.worker_bitmap, 0x8f,
        "writer + IPC + 两 RR + watchdog，执行器尚未 spawn"
    );
    assert_eq!(observed.spawned_ns, 0);
    assert_eq!(observed.first_io_ns, 0);
    assert!(observed.serial_calls.iter().all(|count| *count == 0));
    assert!(!observed.ready_reached);
    assert_eq!(observed.runtime_armed_ns, 0);
    if code == 88 {
        assert_eq!(observed.fired_checkpoint, 1);
        assert!(
            (5_000_000_000..=5_100_000_000).contains(&(observed.fired_ns - observed.main_start_ns))
        );
    } else {
        assert_eq!(observed.stop_bitmap, observed.worker_bitmap);
    }
}

#[test]
fn auto_enable_disconnected_exits_before_ready() {
    let mut process = Process::spawn_with(6, &[], None, None, true);
    assert_eq!(process.wait_exit(5000), 1);
    let observed = process.page.snapshot();
    assert_initialized(&observed);
    assert_eq!(observed.worker_bitmap, 0xff);
    assert_ne!(observed.preflight_finished_ns, 0);
    assert!(!observed.ready_reached);
    assert_ne!(observed.disabled_ns, 0);
    assert_eq!(observed.stop_bitmap, 0xff);
    assert_eq!(observed.fired_checkpoint, 0);
    assert_eq!(observed.runtime_armed_ns, 0);
    assert_eq!(observed.ready_ns, 0);
    // 保留旧验收入口；新 IpcExit 仍持原 Receiver，线程已退出不等于 channel 断链。
    // 真实 hook 等待 IPC 退出，main 检出原 worker finished 后保原对象退出，不能为日志丢 Receiver。
    assert_eq!(
        observed.serial_calls[0], 0,
        "NormalEffectful 调用数必须为零，不能实际 Enable：{observed:?}"
    );
}

#[test]
fn ipc_worker_exit_after_ready_terminates_standalone_daemon() {
    use iceoryx2::prelude::{NodeBuilder, ServiceName};
    use iceoryx2::service::ipc::Service;
    use robot_s0::comm_fault_hook::{HookCommandKind, TestCommFaultCommand};

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
    let request_at = SystemClock.now().0;
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
    assert_eq!(process.wait_exit(RUNTIME_EXIT_TIMEOUT_MS), 1);
    let reaped_at = SystemClock.now().0;
    let observed = process.page.snapshot();
    assert_eq!(observed.command_applied, 101, "必须由真实控制器消费注入");
    assert!(observed.ready_reached);
    assert_ne!(observed.disabled_ns, 0);
    assert_eq!(
        observed.stop_bitmap,
        HELPER_STOP_BITMAP | (1 << WorkerKind::Ipc as u8),
        "仅真实注入的 IPC 停止及 helpers；无 ACK 不得停止受保护句柄"
    );
    assert_eq!(observed.residual_bitmap, IPC_NO_ACK_RESIDUAL);
    assert_eq!(observed.fired_checkpoint, 0);
    assert_eq!(
        observed.fired_reason, 0,
        "应由 main 在 L 收尾，不能拖至 supervisor D"
    );
    assert_ipc_no_ack_exit_time(request_at, reaped_at);
    assert!(process.log().contains("READY 后 worker 已退出"));
}

/// S48 A3b：READY 之后执行器（SerialRT mock）因快照版本耗尽结束；standalone
/// （无 `WATCHDOG_USEC` / `NOTIFY_SOCKET`）下 daemon 必须由 READY 循环自己判错并
/// 非零退出，走既有收尾。版本起点留 1000 次发布（5 ms tick ≈ 5 s），保证 READY
/// 先于耗尽发生；若环境慢到 READY 晚于耗尽，本用例按超时失败而不是误判通过。
#[test]
fn executor_snapshot_version_exhaustion_after_ready_terminates_standalone_daemon() {
    let start = robot_transport::MAX_PUBLISHED_VERSION - 2 * 1000;
    let mut process = Process::spawn_standalone_with_env(
        6,
        &[("ROBOT_TEST_SNAPSHOT_INITIAL_VERSION", start.to_string())],
    );
    let ready = process.wait_ready(4000);
    assert!(ready.ready_reached);
    assert_eq!(process.wait_snapshot_exhaustion_exit(), 1);
    let observed = process.page.snapshot();
    assert!(observed.ready_reached);
    assert_ne!(observed.disabled_ns, 0);
    assert_eq!(
        observed.stop_bitmap, 0xff,
        "IPC 仍活并完成原 ACK/Clear/flush，实际停止请求覆盖八个 worker"
    );
    assert_eq!(observed.residual_bitmap, 0, "真实 join 后没有残余句柄");
    assert_eq!(observed.fired_checkpoint, 0);
    // 事件顺序证据：执行器在真实 publish 之前取的时钟（同 CLOCK_MONOTONIC_RAW）
    // 必须晚于 daemon 的真实 READY 时刻。
    assert_ne!(
        observed.executor_publish_failure_attempt_ns, 0,
        "{observed:?}"
    );
    assert!(
        observed.ready_ns < observed.executor_publish_failure_attempt_ns,
        "耗尽尝试必须晚于 READY：{observed:?}"
    );
    assert_eq!(
        observed.ipc_publish_failure_attempt_ns, 0,
        "IPC 未耗尽：{observed:?}"
    );
    let log = process.log();
    assert!(log.contains("READY 后 worker 已退出"), "{log}");
    assert!(
        log.contains("VersionExhausted") && log.contains("worker=Executor"),
        "{log}"
    );
    // 导出发生在 READY 循环退出 + 既有收尾真实 join 之后（不靠 is_finished）：
    // 日志顺序必须是 READY 判错在前、耗尽时刻导出在后。
    assert_log_order(&log, "READY 后 worker 已退出", "VersionExhausted");
    assert!(
        !log.contains("worker=Ipc"),
        "执行器与 IPC 的耗尽出口分开核：{log}"
    );
    assert!(
        log.contains("standalone 无 READY socket"),
        "仍为无 notify 的 standalone：{log}"
    );
    assert!(
        !log.contains("WATCHDOG_USEC"),
        "不应启动 systemd watchdog：{log}"
    );
}

/// S48 A3b：IPC 的状态快照写端耗尽 → IPC worker 结束 → READY 循环既有 IPC 判错。
#[test]
fn ipc_state_snapshot_version_exhaustion_after_ready_terminates_standalone_daemon() {
    // IPC 每 500 µs 发布一次：留 8000 次发布 ≈ 4 s，保证 READY 先于耗尽。
    let start = robot_transport::MAX_PUBLISHED_VERSION - 2 * 8000;
    let mut process = Process::spawn_standalone_with_env(
        6,
        &[(
            "ROBOT_TEST_STATE_SNAPSHOT_INITIAL_VERSION",
            start.to_string(),
        )],
    );
    let ready = process.wait_ready(4000);
    assert!(ready.ready_reached);
    assert_eq!(process.wait_snapshot_exhaustion_exit(), 1);
    let reaped_at = SystemClock.now().0;
    let observed = process.page.snapshot();
    assert!(observed.ready_reached);
    assert_eq!(observed.stop_bitmap, HELPER_STOP_BITMAP);
    assert_eq!(observed.residual_bitmap, IPC_NO_ACK_RESIDUAL);
    assert_eq!(observed.fired_checkpoint, 0);
    assert_eq!(observed.fired_reason, 0);
    assert_ne!(observed.ipc_publish_failure_attempt_ns, 0, "{observed:?}");
    assert!(
        observed.ready_ns < observed.ipc_publish_failure_attempt_ns,
        "耗尽尝试必须晚于 READY：{observed:?}"
    );
    assert_eq!(
        observed.executor_publish_failure_attempt_ns, 0,
        "执行器未耗尽：{observed:?}"
    );
    assert_ipc_no_ack_exit_time(observed.ipc_publish_failure_attempt_ns, reaped_at);
    let log = process.log();
    assert!(log.contains("READY 后 worker 已退出"), "{log}");
    assert!(
        log.contains("VersionExhausted") && log.contains("worker=Ipc"),
        "{log}"
    );
    assert_log_order(&log, "READY 后 worker 已退出", "VersionExhausted");
    assert!(!log.contains("worker=Executor"), "{log}");
    assert!(log.contains("standalone 无 READY socket"), "{log}");
    assert!(!log.contains("WATCHDOG_USEC"), "{log}");
}

/// S48 A3b：非法注入值（奇数）不得启动 daemon。
#[test]
fn odd_snapshot_version_injection_is_rejected_at_startup() {
    let mut process = Process::spawn_standalone_with_env(
        6,
        &[("ROBOT_TEST_SNAPSHOT_INITIAL_VERSION", "3".to_string())],
    );
    assert_eq!(process.wait_exit(5000), 1);
    assert!(
        process.log().contains("快照初始版本注入值非法"),
        "{}",
        process.log()
    );
}

#[test]
fn each_worker_spawn_failure_stops_only_the_successful_prefix() {
    for (index, worker) in [
        "latch_writer",
        "ipc",
        "clear_rr",
        "set_rr",
        "watchdog",
        "executor",
        "pub",
        "io",
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
        assert_initialized(&observed);
        let expected = [7_u8, 0, 1, 2, 3, 4, 5, 6]
            .into_iter()
            .take(index)
            .fold(0_u64, |bits, kind| bits | (1 << kind));
        assert_eq!(observed.worker_bitmap, expected);
        assert_eq!(observed.runtime_armed_ns, 0, "CP2 失败不得武装 Runtime");
        assert_eq!(observed.stop_bitmap, observed.worker_bitmap);
        assert_eq!(observed.spawned_ns, 0);
        assert_eq!(observed.first_io_ns, 0);
        assert!(!observed.ready_reached);
    }
}

#[test]
fn main_pause_before_spawn_is_enforced_by_cp1() {
    let stage = "before_logging";
    let mut process = Process::spawn(6, &[], Some(stage), None);
    assert_eq!(process.wait_exit(5400), 88);
    assert!(
        process
            .log()
            .contains(&format!("comm_fault_hook stage={stage}"))
    );
    let observed = process.page.snapshot();
    assert_initialized(&observed);
    assert_eq!(observed.fired_checkpoint, 1);
    assert!(
        (5_000_000_000..=5_100_000_000).contains(&(observed.fired_ns - observed.main_start_ns))
    );
    assert!(!observed.ready_reached);
    assert_eq!(observed.first_io_ns, 0);
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
        assert_initialized(&observed);
        assert_eq!(observed.fired_checkpoint, 2);
        assert_eq!(observed.worker_bitmap, 0xff);
        assert_ne!(observed.spawned_ns, 0);
        assert!(
            (11_700_000_000..=11_800_000_000)
                .contains(&(observed.fired_ns - observed.main_start_ns))
        );
        assert!(!observed.ready_reached);
    }
}

#[test]
fn ipc_ready_pause_returns_original_residual_before_cp2() {
    let mut process = Process::spawn(6, &[], Some("ipc_ready"), None);
    assert_eq!(process.wait_exit(12_000), 1);
    let reaped_at = SystemClock.now().0;
    let observed = process.page.snapshot();
    assert_initialized(&observed);
    assert_eq!(observed.worker_bitmap, 0xff);
    assert_eq!(observed.stop_bitmap, 0xff);
    assert_eq!(observed.residual_bitmap, u64::from(HandleKind::Ipc.bit()));
    assert_eq!(observed.fired_checkpoint, 0);
    assert_eq!(observed.fired_reason, 0);
    assert_eq!(observed.fired_ns, 0);
    assert_eq!(observed.supervisor_exit_ns, 0);
    assert!(!observed.ready_reached);
    assert_eq!(observed.runtime_armed_ns, 0);
    assert_ne!(observed.spawned_ns, 0);
    assert_ne!(observed.first_io_ns, 0);
    assert!(observed.first_io_ns >= observed.spawned_ns);
    assert!(observed.preflight_finished_ns >= observed.first_io_ns);
    // IPC worker 挂在 ready 前，main 仍按原绝对启动截止返错；父真回收只作时间外包络。
    let cp2 = absolute_boot_budget(6).0 + SHUTDOWN_MARGIN.0;
    let elapsed = reaped_at.checked_sub(observed.main_start_ns).unwrap();
    assert!(
        (cp2 - 50_000_000..=cp2 + 100_000_000).contains(&elapsed),
        "elapsed={elapsed}"
    );
    let log = process.log();
    assert_log_order(
        &log,
        "comm_fault_hook stage=ipc_ready",
        "启动物理/ACK 截止失败",
    );
    assert_log_order(
        &log,
        "启动物理/ACK 截止失败",
        "收尾未完成 join，不导出快照耗尽尝试时刻",
    );
    assert!(
        !log.contains("VersionExhausted"),
        "活 IPC 不能冒充真 join：{log}"
    );
}

#[test]
fn missing_required_notify_socket_never_commits_ready() {
    let mut process = Process::spawn(6, &["--notify-mode", "required"], None, None);
    assert_eq!(process.wait_exit(5000), 1);
    let observed = process.page.snapshot();
    assert_initialized(&observed);
    assert!(!observed.ready_reached);
    assert_eq!(observed.worker_bitmap, 0xff);
    assert_eq!(observed.stop_bitmap, 0xff);
    assert_ne!(observed.disabled_ns, 0);
}

#[test]
fn thirty_three_joints_reject_before_workers_or_bus_io() {
    let mut process = Process::spawn(33, &[], None, None);
    assert_eq!(process.wait_exit(2000), 2);
    let observed = process.page.snapshot();
    assert_initialized(&observed);
    assert_eq!(observed.joint_count, 32);
    assert_eq!(observed.worker_bitmap, 0);
    assert_eq!(observed.first_io_ns, 0);
}
