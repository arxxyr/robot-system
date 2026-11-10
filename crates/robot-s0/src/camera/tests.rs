use std::fs;
use std::path::Path;
use std::time::Instant;

use robot_core::time::{MockClock, MonoRawNs};
use tempfile::TempDir;

use super::*;
use crate::test_util::wait_until;

fn config() -> CameraConfig {
    CameraConfig {
        id: "fixture".into(),
        device: PathBuf::from("/fixture/not-a-camera"),
        python: PathBuf::from("/fixture/not-python"),
        helper: PathBuf::from("/fixture/not-helper"),
        width: 2,
        height: 1,
        fps: 10,
    }
}

fn fixture() -> (OpenCvCameraAdapter, Arc<MockClock>, TempDir) {
    let clock = Arc::new(MockClock::new(MonoRawNs(1000)));
    let adapter = OpenCvCameraAdapter::new(config(), clock.clone()).unwrap();
    (adapter, clock, tempfile::tempdir().unwrap())
}

fn packet(stamp: u64, data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::from(*b"RCAM");
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(&stamp.to_le_bytes());
    bytes.extend_from_slice(data);
    bytes
}

fn cat(path: &Path) -> Command {
    let mut command = Command::new("/bin/cat");
    command.arg(path);
    command
}

fn poll(adapter: &mut OpenCvCameraAdapter) -> Result<Option<CameraFrame>, RobotError> {
    adapter.poll_frame(Deadline(MonoRawNs(10_000)))
}

fn next_frame(adapter: &mut OpenCvCameraAdapter) -> Result<CameraFrame, RobotError> {
    let mut result = None;
    assert!(wait_until(Duration::from_secs(2), || {
        match poll(adapter) {
            Ok(None) => return false,
            Ok(Some(frame)) => result = Some(Ok(frame)),
            Err(error) => result = Some(Err(error)),
        }
        true
    }));
    result.unwrap()
}

fn assert_reaped(pid: u32) {
    let pid = libc::pid_t::try_from(pid).unwrap();
    let mut status = 0;
    // SAFETY: 只查询该测试已停止的子进程，不等待其他子进程。
    assert_eq!(
        unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn config_rejects_zero_overflow_and_nonabsolute_paths_without_io() {
    for invalid in [
        CameraConfig {
            width: 0,
            ..config()
        },
        CameraConfig {
            height: u32::MAX,
            ..config()
        },
        CameraConfig { fps: 0, ..config() },
        CameraConfig {
            device: "video0".into(),
            ..config()
        },
        CameraConfig {
            id: "../other".into(),
            ..config()
        },
    ] {
        assert!(invalid.validate().is_err());
    }
    assert!(config().validate().is_ok());
}

#[test]
fn fragmented_helper_frame_keeps_partial_header_and_owns_pixels() {
    let (mut adapter, clock, directory) = fixture();
    let bytes = packet(2000, &[1, 2, 3, 4, 5, 6]);
    fs::write(directory.path().join("prefix"), &bytes[..7]).unwrap();
    fs::write(directory.path().join("tail"), &bytes[7..]).unwrap();
    let ready = directory.path().join("ready");
    let gate = directory.path().join("gate");
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "cat \"$1\"; : > \"$3\"; while [ ! -e \"$4\" ]; do sleep 0.01; done; exec cat \"$2\"",
            "camera-fixture",
        ])
        .arg(directory.path().join("prefix"))
        .arg(directory.path().join("tail"))
        .arg(&ready)
        .arg(&gate);
    adapter.spawn(command).unwrap();
    let pid = adapter.child.as_ref().unwrap().id();
    clock.set(MonoRawNs(3000));
    assert!(wait_until(Duration::from_secs(2), || ready.exists()));
    assert!(poll(&mut adapter).unwrap().is_none());
    assert_eq!(adapter.header_used, 7);
    assert_eq!(adapter.pixels, [] as [u8; 0]);
    fs::write(gate, b"").unwrap();
    let frame = next_frame(&mut adapter).unwrap();
    assert_eq!(
        (frame.width, frame.height, frame.step, frame.stamp_ns),
        (2, 1, 6, 2000)
    );
    assert_eq!(frame.data, [1, 2, 3, 4, 5, 6]);
    assert_eq!(adapter.pixels, [] as [u8; 0]);
    adapter.stop().unwrap();
    assert_reaped(pid);
    assert_eq!(frame.data, [1, 2, 3, 4, 5, 6]);
}

#[test]
fn rejects_bad_header_length_before_allocating_and_keeps_fault() {
    for offset in [0, 4, 8, 12] {
        let (mut adapter, clock, directory) = fixture();
        let mut bytes = packet(2000, &[0; 6]);
        bytes[offset] ^= 0xff;
        let path = directory.path().join("frame");
        fs::write(&path, bytes).unwrap();
        adapter.spawn(cat(&path)).unwrap();
        clock.set(MonoRawNs(3000));
        let error = next_frame(&mut adapter).unwrap_err();
        assert_eq!(error, RobotError::HardwareFault("相机帧头或像素长度非法"));
        assert_eq!(adapter.pixels.capacity(), 0);
        assert_eq!(poll(&mut adapter).unwrap_err(), error);
        adapter.stop().unwrap();
    }
}

#[test]
fn rejects_zero_old_and_future_timestamps() {
    for stamp in [0, 999, 3001] {
        let (mut adapter, clock, directory) = fixture();
        let path = directory.path().join("frame");
        fs::write(&path, packet(stamp, &[0; 6])).unwrap();
        adapter.spawn(cat(&path)).unwrap();
        clock.set(MonoRawNs(3000));
        assert_eq!(
            next_frame(&mut adapter).unwrap_err(),
            RobotError::HardwareFault("相机帧时间戳非法")
        );
        assert_eq!(adapter.pixels.capacity(), 0);
        adapter.stop().unwrap();
    }
}

#[test]
fn repeated_stamp_is_not_a_new_frame() {
    let (mut adapter, clock, directory) = fixture();
    let path = directory.path().join("frames");
    let bytes = packet(2000, &[0; 6]);
    fs::write(&path, [bytes.as_slice(), bytes.as_slice()].concat()).unwrap();
    adapter.spawn(cat(&path)).unwrap();
    clock.set(MonoRawNs(3000));
    next_frame(&mut adapter).unwrap();
    assert_eq!(
        next_frame(&mut adapter).unwrap_err(),
        RobotError::HardwareFault("相机帧时间戳非法")
    );
    adapter.stop().unwrap();
}

#[test]
fn partial_frame_eof_is_an_error_and_child_is_reaped() {
    let (mut adapter, clock, directory) = fixture();
    let path = directory.path().join("partial");
    fs::write(&path, &packet(2000, &[0; 6])[..HEADER_LEN + 2]).unwrap();
    adapter.spawn(cat(&path)).unwrap();
    let pid = adapter.child.as_ref().unwrap().id();
    clock.set(MonoRawNs(3000));
    assert_eq!(
        next_frame(&mut adapter).unwrap_err(),
        RobotError::HardwareFault("相机辅助进程管道已关闭")
    );
    adapter.stop().unwrap();
    assert_reaped(pid);
}

#[test]
fn expired_deadline_does_not_read_or_allocate() {
    let (mut adapter, clock, directory) = fixture();
    let path = directory.path().join("frame");
    fs::write(&path, packet(2000, &[0; 6])).unwrap();
    adapter.spawn(cat(&path)).unwrap();
    clock.set(MonoRawNs(3000));
    assert!(
        adapter
            .poll_frame(Deadline(MonoRawNs(3000)))
            .unwrap()
            .is_none()
    );
    assert_eq!(adapter.header_used, 0);
    assert_eq!(adapter.pixels.capacity(), 0);
    adapter.stop().unwrap();
}

#[test]
fn large_frame_is_split_by_the_per_poll_byte_budget() {
    let (mut adapter, clock, directory) = fixture();
    adapter.config.width = 1024;
    adapter.config.height = 128;
    let pixels = vec![7; 1024 * 128 * 3];
    let mut bytes = packet(2000, &pixels);
    bytes[4..8].copy_from_slice(&1024u32.to_le_bytes());
    bytes[8..12].copy_from_slice(&128u32.to_le_bytes());
    let path = directory.path().join("large-frame");
    fs::write(&path, bytes).unwrap();
    adapter.spawn(cat(&path)).unwrap();
    clock.set(MonoRawNs(3000));
    assert!(wait_until(Duration::from_secs(2), || {
        assert!(poll(&mut adapter).unwrap().is_none());
        adapter.pixels_used > 0
    }));
    assert!(adapter.pixels_used <= READ_BUDGET);
    assert!(adapter.pixels_used < pixels.len());
    let frame = next_frame(&mut adapter).unwrap();
    assert_eq!(frame.data, pixels);
    adapter.stop().unwrap();
}

#[test]
fn missing_executable_is_reported_without_child() {
    let (mut adapter, _, _) = fixture();
    assert!(adapter.start().is_err());
    assert!(adapter.child.is_none());
    assert_eq!(poll(&mut adapter).unwrap_err(), RobotError::NotConnected);
    adapter.stop().unwrap();
}

#[test]
fn hung_helper_poll_is_nonblocking_and_forced_stop_reaps_the_child() {
    let (mut adapter, clock, directory) = fixture();
    let ready = directory.path().join("ready");
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "trap '' TERM; : > \"$1\"; exec sleep 30",
            "camera-fixture",
        ])
        .arg(&ready);
    adapter.spawn(command).unwrap();
    let pid = adapter.child.as_ref().unwrap().id();
    clock.set(MonoRawNs(3000));
    assert!(wait_until(Duration::from_secs(2), || ready.exists()));
    let start = Instant::now();
    assert!(poll(&mut adapter).unwrap().is_none());
    assert!(start.elapsed() < Duration::from_secs(1));
    let start = Instant::now();
    assert_eq!(
        adapter.stop(),
        Err(RobotError::HardwareFault("相机辅助进程已强制终止并回收"))
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(adapter.child.is_none());
    assert_reaped(pid);
    adapter.stop().unwrap();
}

#[cfg(target_os = "linux")]
fn death_fixture_process(role: &str, directory: &Path) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "camera::tests::parent_death_signal_kills_helper",
        "--nocapture",
    ]);
    command
        .env("ROBOT_CAMERA_DEATH_ROLE", role)
        .env("ROBOT_CAMERA_DEATH_DIR", directory);
    parent_death_signal(&mut command).unwrap();
    command.spawn().unwrap()
}

#[cfg(target_os = "linux")]
fn death_fixture_parent(directory: &Path) -> ! {
    let (mut adapter, _, _unused_directory) = fixture();
    let ready = directory.join("helper-ready");
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "trap '' TERM; : > \"$1\"; exec sleep 30",
            "camera-death-fixture",
        ])
        .arg(&ready);
    adapter.spawn(command).unwrap();
    assert!(wait_until(Duration::from_secs(2), || ready.exists()));
    fs::write(
        directory.join("helper-pid"),
        adapter.child.as_ref().unwrap().id().to_string(),
    )
    .unwrap();
    // 保留 adapter 到父进程被 SIGKILL；本夹具故意不走 Drop。
    loop {
        thread::park_timeout(Duration::from_secs(1));
    }
}

#[cfg(target_os = "linux")]
fn death_fixture_supervisor(directory: &Path) {
    // SAFETY: 只改变这个隔离夹具进程；使孙进程可由本夹具真正 wait/reap。
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    let mut parent = death_fixture_process("parent", directory);
    let pid_file = directory.join("helper-pid");
    if !wait_until(Duration::from_secs(3), || pid_file.exists()) {
        let _ = parent.kill();
        let _ = parent.wait();
        panic!("父死夹具未创建相机子进程");
    }
    let helper_pid: libc::pid_t = fs::read_to_string(pid_file).unwrap().parse().unwrap();
    parent.kill().unwrap();
    parent.wait().unwrap();
    let mut status = 0;
    let reaped = wait_until(Duration::from_secs(2), || {
        // SAFETY: subreaper 只回收上述已知 helper pid，WNOHANG 不阻塞。
        unsafe { libc::waitpid(helper_pid, &raw mut status, libc::WNOHANG) == helper_pid }
    });
    if !reaped {
        // 即使父死合同回归，也只清理本测试创建的假相机子进程。
        // SAFETY: helper_pid 来自同一夹具的直接记录；父进程已回收。
        unsafe {
            libc::kill(helper_pid, libc::SIGKILL);
            libc::waitpid(helper_pid, &raw mut status, 0);
        }
    }
    assert!(reaped, "PDEATHSIG 未使忽略 SIGTERM 的假相机退出");
    assert!(libc::WIFSIGNALED(status));
    assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
    assert_reaped(u32::try_from(helper_pid).unwrap());
}

#[test]
#[cfg(target_os = "linux")]
fn parent_death_signal_kills_helper() {
    match std::env::var("ROBOT_CAMERA_DEATH_ROLE").as_deref() {
        Ok("parent") => death_fixture_parent(&PathBuf::from(
            std::env::var_os("ROBOT_CAMERA_DEATH_DIR").unwrap(),
        )),
        Ok("supervisor") => {
            death_fixture_supervisor(&PathBuf::from(
                std::env::var_os("ROBOT_CAMERA_DEATH_DIR").unwrap(),
            ));
        }
        _ => {
            let directory = tempfile::tempdir().unwrap();
            let mut supervisor = death_fixture_process("supervisor", directory.path());
            let mut status = None;
            let completed = wait_until(Duration::from_secs(8), || {
                status = supervisor.try_wait().unwrap();
                status.is_some()
            });
            if !completed {
                let _ = supervisor.kill();
                let _ = supervisor.wait();
            }
            assert!(completed, "父死验证夹具未在截止前完成");
            assert!(status.unwrap().success());
        }
    }
}
