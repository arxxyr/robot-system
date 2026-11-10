//! 单条 `OpenCV` BGR8 相机流；采集进程、非阻塞管道和帧均由 IO 侧独占。
//!
//! 小帧头为 `<4sIIIQ`：`RCAM`、宽、高、像素字节数、采集完成的
//! `CLOCK_MONOTONIC_RAW` 纳秒。它只是本机辅助进程协议，不是 IPC wire。

use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use robot_core::adapter::SensorAdapter;
use robot_core::error::RobotError;
use robot_core::time::{Deadline, RobotClock};

const HEADER_LEN: usize = 24;
const READ_BUDGET: usize = 256 * 1024;
const MAX_READ_CALLS: usize = 64;
/// 为已有 16 MiB 图像 sample 档位预留消息头空间。
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024 - 4096;
const STOP_POLLS: usize = 50;
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraConfig {
    pub id: String,
    pub device: PathBuf,
    pub python: PathBuf,
    pub helper: PathBuf,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl CameraConfig {
    /// 仅校验值；不解析设备链接、不打开相机或启动 Python。
    pub fn validate(&self) -> Result<(), RobotError> {
        if self.id.is_empty()
            || self.id.len() > 128
            || !self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(RobotError::InvalidParameter("相机 id 非法"));
        }
        if !self.device.is_absolute() || !self.python.is_absolute() || !self.helper.is_absolute() {
            return Err(RobotError::InvalidParameter("相机路径必须为绝对路径"));
        }
        if self.width == 0 || self.height == 0 || self.fps == 0 {
            return Err(RobotError::InvalidParameter("相机宽高和帧率必须大于零"));
        }
        self.frame_len().map(|_| ())
    }

    fn frame_len(&self) -> Result<usize, RobotError> {
        self.width
            .checked_mul(3)
            .and_then(|step| step.checked_mul(self.height))
            .and_then(|size| usize::try_from(size).ok())
            .filter(|&size| size <= MAX_FRAME_BYTES)
            .ok_or(RobotError::InvalidParameter("相机帧超过大小上限"))
    }
}

/// 像素已独立拥有；`stamp_ns` 是读取完成时间，不是硬件曝光时间。
#[derive(Debug)]
pub struct CameraFrame {
    pub width: u32,
    pub height: u32,
    pub step: u32,
    pub stamp_ns: u64,
    pub data: Vec<u8>,
}

pub struct OpenCvCameraAdapter {
    config: CameraConfig,
    clock: Arc<dyn RobotClock>,
    child: Option<Child>,
    stdout: Option<ChildStdout>,
    header: [u8; HEADER_LEN],
    header_used: usize,
    pixels: Vec<u8>,
    pixels_used: usize,
    frame_stamp: u64,
    last_stamp: u64,
    started_ns: u64,
    fault: Option<RobotError>,
}

impl OpenCvCameraAdapter {
    pub fn new(config: CameraConfig, clock: Arc<dyn RobotClock>) -> Result<Self, RobotError> {
        config.validate()?;
        Ok(Self {
            config,
            clock,
            child: None,
            stdout: None,
            header: [0; HEADER_LEN],
            header_used: 0,
            pixels: Vec::new(),
            pixels_used: 0,
            frame_stamp: 0,
            last_stamp: 0,
            started_ns: 0,
            fault: None,
        })
    }

    fn reset_frame(&mut self) {
        self.header_used = 0;
        self.pixels.clear();
        self.pixels_used = 0;
        self.frame_stamp = 0;
    }

    fn spawn(&mut self, mut command: Command) -> Result<(), RobotError> {
        if self.child.is_some() {
            return Err(RobotError::Busy);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        parent_death_signal(&mut command)?;
        self.started_ns = self.clock.now().0;
        let child = command.spawn().map_err(|error| {
            tracing::error!(%error, camera = %self.config.id, "相机辅助进程启动失败");
            RobotError::HardwareFault("相机辅助进程启动失败")
        })?;
        self.child = Some(child);
        self.stdout = self.child.as_mut().and_then(|child| child.stdout.take());
        let result = self
            .stdout
            .as_ref()
            .ok_or_else(|| io::Error::other("相机 stdout 未创建"))
            .and_then(set_nonblocking);
        if let Err(error) = result {
            tracing::error!(%error, "相机管道初始化失败");
            if let Err(stop_error) = self.stop() {
                tracing::error!(%stop_error, "相机初始化失败后的回收未正常完成");
            }
            return Err(RobotError::HardwareFault("相机管道初始化失败"));
        }
        self.reset_frame();
        self.last_stamp = 0;
        self.fault = None;
        Ok(())
    }

    fn decode_header(&mut self) -> Result<(), RobotError> {
        let width = u32::from_le_bytes(self.header[4..8].try_into().expect("固定宽度"));
        let height = u32::from_le_bytes(self.header[8..12].try_into().expect("固定宽度"));
        let size = u32::from_le_bytes(self.header[12..16].try_into().expect("固定宽度"));
        let stamp = u64::from_le_bytes(self.header[16..24].try_into().expect("固定宽度"));
        let expected = self.config.frame_len()?;
        if &self.header[..4] != b"RCAM"
            || width != self.config.width
            || height != self.config.height
            || usize::try_from(size).ok() != Some(expected)
        {
            return Err(RobotError::HardwareFault("相机帧头或像素长度非法"));
        }
        if stamp == 0
            || stamp < self.started_ns
            || stamp <= self.last_stamp
            || stamp > self.clock.now().0
        {
            return Err(RobotError::HardwareFault("相机帧时间戳非法"));
        }
        // 只在完整帧头与长度均通过后分配，不相信辅助进程给出的长度。
        self.pixels.resize(expected, 0);
        self.pixels_used = 0;
        self.frame_stamp = stamp;
        Ok(())
    }

    fn read_frame(&mut self, deadline: Deadline) -> Result<Option<CameraFrame>, RobotError> {
        let mut budget = READ_BUDGET;
        for _ in 0..MAX_READ_CALLS {
            if budget == 0 || self.clock.now() >= deadline.as_mono_raw_ns() {
                break;
            }
            let header_pending = self.header_used < HEADER_LEN;
            let target = if header_pending {
                let end = HEADER_LEN.min(self.header_used + budget);
                &mut self.header[self.header_used..end]
            } else {
                let end = self.pixels.len().min(self.pixels_used + budget);
                &mut self.pixels[self.pixels_used..end]
            };
            let result = self
                .stdout
                .as_mut()
                .ok_or(RobotError::NotConnected)?
                .read(target);
            let count = match result {
                Ok(0) => return Err(RobotError::HardwareFault("相机辅助进程管道已关闭")),
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    tracing::error!(%error, "相机管道读取失败");
                    return Err(RobotError::HardwareFault("相机管道读取失败"));
                }
            };
            budget -= count;
            if header_pending {
                self.header_used += count;
                if self.header_used == HEADER_LEN {
                    self.decode_header()?;
                }
            } else {
                self.pixels_used += count;
                if self.pixels_used == self.pixels.len() {
                    let frame = CameraFrame {
                        width: self.config.width,
                        height: self.config.height,
                        step: self.config.width * 3,
                        stamp_ns: self.frame_stamp,
                        data: std::mem::take(&mut self.pixels),
                    };
                    self.last_stamp = frame.stamp_ns;
                    self.reset_frame();
                    return Ok(Some(frame));
                }
            }
        }
        Ok(None)
    }
}

impl SensorAdapter for OpenCvCameraAdapter {
    type Frame = CameraFrame;

    fn start(&mut self) -> Result<(), RobotError> {
        let mut command = Command::new(&self.config.python);
        command
            .args(["-I", "-S", "-B", "-u"])
            .arg(&self.config.helper)
            .arg("--device")
            .arg(&self.config.device)
            .arg("--width")
            .arg(self.config.width.to_string())
            .arg("--height")
            .arg(self.config.height.to_string())
            .arg("--fps")
            .arg(self.config.fps.to_string());
        self.spawn(command)
    }

    fn poll_frame(&mut self, deadline: Deadline) -> Result<Option<Self::Frame>, RobotError> {
        if let Some(error) = self.fault {
            return Err(error);
        }
        if self.child.is_none() {
            return Err(RobotError::NotConnected);
        }
        let result = self.read_frame(deadline);
        if let Err(error) = result {
            self.fault = Some(error);
        }
        result
    }

    fn stop(&mut self) -> Result<(), RobotError> {
        self.reset_frame();
        let Some(mut child) = self.child.take() else {
            self.stdout = None;
            return Ok(());
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                self.stdout = None;
                return exit_result(status, false);
            }
            Ok(None) => {}
            Err(error) => tracing::error!(%error, "相机进程状态查询失败，继续终止与回收"),
        }
        if let Err(error) = terminate(&child) {
            tracing::error!(%error, "相机 SIGTERM 发送失败，继续尝试回收");
        }
        // 先请求正常释放，再关闭读端解除可能卡住的像素写入。
        self.stdout = None;
        if let Some(status) = wait_bounded(&mut child) {
            return exit_result(status, true);
        }
        if let Err(error) = child.kill() {
            tracing::error!(%error, "相机 SIGKILL 发送失败");
        }
        if wait_bounded(&mut child).is_some() {
            return Err(RobotError::HardwareFault("相机辅助进程已强制终止并回收"));
        }
        // 未完成回收时保留句柄，调用方不能把错误记成停止成功。
        self.child = Some(child);
        Err(RobotError::HardwareFault("相机辅助进程退出尚未确认"))
    }
}

impl Drop for OpenCvCameraAdapter {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::error!(%error, camera = %self.config.id, "相机资源回收未正常完成");
        }
    }
}

fn wait_bounded(child: &mut Child) -> Option<ExitStatus> {
    // 这是 OS 子进程回收的轮询上限，不推进业务时钟或依赖 MockClock 前进。
    for _ in 0..STOP_POLLS {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => thread::sleep(STOP_POLL_INTERVAL),
            Err(error) => {
                tracing::error!(%error, "相机进程回收状态查询失败");
                return None;
            }
        }
    }
    child.try_wait().ok().flatten()
}

fn exit_result(status: ExitStatus, terminated: bool) -> Result<(), RobotError> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if terminated && matches!(status.signal(), Some(libc::SIGTERM | libc::SIGPIPE)) {
            return Ok(());
        }
    }
    #[cfg(not(unix))]
    let _ = terminated;
    if status.success() {
        Ok(())
    } else {
        tracing::error!(%status, "相机辅助进程异常退出");
        Err(RobotError::HardwareFault("相机辅助进程异常退出"))
    }
}

#[cfg(unix)]
fn set_nonblocking(stdout: &ChildStdout) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: stdout 在两次调用间存活；只读取并设置同一有效 fd 的整数标志。
    let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_nonblocking(_: &ChildStdout) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "相机管道仅支持 Unix",
    ))
}

#[cfg(unix)]
fn terminate(child: &Child) -> io::Result<()> {
    let pid = libc::pid_t::try_from(child.id()).map_err(|_| io::Error::other("相机 pid 越界"))?;
    // SAFETY: pid 来自尚未 wait/reap 的直接子进程，只发送 SIGTERM。
    if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn terminate(_: &Child) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "相机进程仅支持 Unix",
    ))
}

fn parent_death_signal(command: &mut Command) -> Result<(), RobotError> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let parent = libc::pid_t::try_from(std::process::id())
            .map_err(|_| RobotError::InvalidParameter("父进程 pid 越界"))?;
        // SAFETY: fork 后仅调用不分配内存的 libc 系统接口；不触及 Rust 锁。
        unsafe {
            command.pre_exec(move || {
                // 父线程消失后不能依赖 Python 从阻塞的 cap.read 返回。
                // SIGKILL 仅针对该相机辅助子进程；正常停止仍先发 SIGTERM。
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ECHILD));
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
    #[cfg(not(unix))]
    return Err(RobotError::Unsupported("OpenCV 相机仅支持 Unix"));
    #[cfg(unix)]
    Ok(())
}

#[cfg(all(test, unix))]
mod tests;
