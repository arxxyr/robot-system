//! Linux 运行期串口 IO：保留原 TTYPort 所有权和配置，错误只携带固定 ErrorKind。

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::time::Duration;

use super::{DeadlinePortTimeout, SerialPortTimeout};

pub(super) struct NativeSerialPort {
    port: serialport::TTYPort,
    timeout: Duration,
}

impl NativeSerialPort {
    pub(super) const fn new(port: serialport::TTYPort, timeout: Duration) -> Self {
        Self { port, timeout }
    }

    pub(super) fn clear_input(&mut self) -> io::Result<()> {
        clear_input(self.port.as_raw_fd())
    }
}

impl SerialPortTimeout for NativeSerialPort {
    fn apply_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        // TTYPort::set_timeout 原实现也只更新本地 Duration，不改 termios 或 FD。
        self.timeout = timeout;
        Ok(())
    }
}

impl DeadlinePortTimeout for NativeSerialPort {
    fn apply_timeout_kind(&mut self, timeout: Duration) -> Result<(), io::ErrorKind> {
        self.timeout = timeout;
        Ok(())
    }
}

impl Read for NativeSerialPort {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_fd(self.port.as_raw_fd(), self.timeout, buf)
    }
}

impl Write for NativeSerialPort {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_fd(self.port.as_raw_fd(), self.timeout, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        // 原有有界写核心不调用 flush；此方法不向正常发送路径添加事务。
        // SAFETY：TTYPort 独占持有有效 FD，调用期间不会关闭。
        syscall_result(unsafe { libc::tcdrain(self.port.as_raw_fd()) }).map(|_| ())
    }
}

// 与 serialport 4.10.1 的 posix/error.rs 保持 ErrorKind 映射；不构造描述字符串。
fn errno_error() -> io::Error {
    let kind = match io::Error::last_os_error().raw_os_error() {
        Some(libc::EBUSY | libc::ENOENT) => io::ErrorKind::NotFound,
        Some(libc::ETIMEDOUT) => io::ErrorKind::TimedOut,
        Some(libc::ECONNABORTED) => io::ErrorKind::ConnectionAborted,
        Some(libc::ECONNRESET) => io::ErrorKind::ConnectionReset,
        Some(libc::ECONNREFUSED) => io::ErrorKind::ConnectionRefused,
        Some(libc::ENOTCONN) => io::ErrorKind::NotConnected,
        Some(libc::EADDRINUSE) => io::ErrorKind::AddrInUse,
        Some(libc::EADDRNOTAVAIL) => io::ErrorKind::AddrNotAvailable,
        Some(libc::EAGAIN) => io::ErrorKind::WouldBlock,
        Some(libc::EINTR) => io::ErrorKind::Interrupted,
        Some(libc::EACCES) => io::ErrorKind::PermissionDenied,
        _ => io::ErrorKind::Other,
    };
    kind.into()
}

fn syscall_result(value: libc::c_int) -> io::Result<libc::c_int> {
    if value < 0 {
        Err(errno_error())
    } else {
        Ok(value)
    }
}

fn wait_fd(fd: RawFd, events: libc::c_short, timeout: Duration) -> io::Result<()> {
    let mut descriptor = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    let seconds_limit = libc::time_t::MAX as u64;
    let timeout = libc::timespec {
        tv_sec: timeout.as_secs().min(seconds_limit) as libc::time_t,
        tv_nsec: if timeout.as_secs() > seconds_limit {
            999_999_999
        } else {
            timeout.subsec_nanos() as libc::c_long
        },
    };
    let mut mask = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY：sigemptyset 初始化完整 sigset_t，成功后才读取；与原 ppoll 的空信号集一致。
    syscall_result(unsafe { libc::sigemptyset(mask.as_mut_ptr()) })?;
    // SAFETY：前一步已成功初始化；两个栈上对象在阻塞调用期间均有效。
    let mask = unsafe { mask.assume_init() };
    // SAFETY：descriptor 指向一项有效 pollfd；timeout/mask 在本次调用中不可变。
    let ready = syscall_result(unsafe {
        libc::ppoll(&raw mut descriptor, 1, &raw const timeout, &raw const mask)
    })?;
    if ready != 1 {
        return Err(io::ErrorKind::TimedOut.into());
    }
    match descriptor.revents {
        value if value == events => Ok(()),
        value if value & (libc::POLLHUP | libc::POLLNVAL) != 0 => {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        _ => Err(io::ErrorKind::Other.into()),
    }
}

fn read_fd(fd: RawFd, timeout: Duration, buf: &mut [u8]) -> io::Result<usize> {
    wait_fd(fd, libc::POLLIN, timeout)?;
    // SAFETY：调用方独占可写切片，内核最多写入 buf.len() 字节；FD 由 owner 保持存活。
    let count = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if count < 0 {
        Err(errno_error())
    } else {
        Ok(count as usize)
    }
}

fn write_fd(fd: RawFd, timeout: Duration, buf: &[u8]) -> io::Result<usize> {
    wait_fd(fd, libc::POLLOUT, timeout)?;
    // SAFETY：切片在 syscall 期间有效，FD 的唯一 owner 不会并发关闭。
    let count = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
    if count < 0 {
        Err(errno_error())
    } else {
        Ok(count as usize)
    }
}

fn clear_input(fd: RawFd) -> io::Result<()> {
    // SAFETY：仅对本 owner 的 FD 执行原有输入队列清理，不打开或复制串口。
    if unsafe { libc::tcflush(fd, libc::TCIFLUSH) } < 0 {
        // 旧 RealSerialTransport 的 clear 错误统一包为 Other；保留其错误分类。
        Err(io::ErrorKind::Other.into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "linux_serial_tests.rs"]
mod tests;
