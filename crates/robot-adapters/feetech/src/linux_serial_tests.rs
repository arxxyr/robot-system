//! 只使用测试自有 PTY/pipe；真实 fd 错误须在实际 allocator 守卫内返回。

use super::*;
use crate::error::{AbsoluteWriteReason, FeetechError};
use crate::transport::{
    SourceWriteDeadline, read_exact_bounded, write_all_bounded, write_all_until_bounded,
};
use robot_core::time::{Deadline, DurationNs, MockClock, MonoRawNs};
use std::os::fd::{FromRawFd, OwnedFd};

#[test]
fn tty_io_timeout_hangup_and_clear_do_not_allocate() {
    let (mut peer, port) = serialport::TTYPort::pair().unwrap();
    let mut native = NativeSerialPort::new(port, Duration::ZERO);
    peer.write_all(&[1, 2, 3]).unwrap();
    let mut bytes = [0; 3];
    let read = assert_no_alloc::assert_no_alloc(|| native.read(&mut bytes));
    assert_eq!(read.unwrap(), 3);
    assert_eq!(bytes, [1, 2, 3]);
    assert_eq!(
        assert_no_alloc::assert_no_alloc(|| native.write(&[4, 5])).unwrap(),
        2
    );
    let mut echoed = [0; 2];
    peer.read_exact(&mut echoed).unwrap();
    assert_eq!(echoed, [4, 5]);
    assert_no_alloc::assert_no_alloc(|| native.clear_input()).unwrap();
    let timeout = assert_no_alloc::assert_no_alloc(|| native.read(&mut bytes)).unwrap_err();
    assert_eq!(timeout.kind(), io::ErrorKind::TimedOut);
    drop(peer);
    for result in [
        assert_no_alloc::assert_no_alloc(|| native.read(&mut bytes)),
        assert_no_alloc::assert_no_alloc(|| native.write(&[1])),
    ] {
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut descriptors = [-1; 2];
    // SAFETY：数组包含两个可写 fd 槽；成功后每个 fd 仅交给一个 OwnedFd。
    assert_eq!(
        unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) },
        0
    );
    // SAFETY：pipe2 成功，两项均为本测试新取得且互异的有效 fd。
    unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    }
}

/// 真 fd 负责可读/可写和错误；虚拟时钟仅在已真实观察到超时时消耗原预算。
struct ClockedFd {
    fd: OwnedFd,
    timeout: Duration,
    clock: MockClock,
}

impl ClockedFd {
    fn finish(&self, result: &io::Result<usize>) {
        if result
            .as_ref()
            .is_err_and(|error| error.kind() == io::ErrorKind::TimedOut)
        {
            self.clock
                .advance(DurationNs(u64::try_from(self.timeout.as_nanos()).unwrap()));
        }
    }
}
impl SerialPortTimeout for ClockedFd {
    fn apply_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        self.timeout = timeout;
        Ok(())
    }
}
impl DeadlinePortTimeout for ClockedFd {
    fn apply_timeout_kind(&mut self, timeout: Duration) -> Result<(), io::ErrorKind> {
        self.timeout = timeout;
        Ok(())
    }
}
impl Read for ClockedFd {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let result = read_fd(self.fd.as_raw_fd(), Duration::ZERO, bytes);
        self.finish(&result);
        result
    }
}
impl Write for ClockedFd {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let result = write_fd(self.fd.as_raw_fd(), Duration::ZERO, bytes);
        self.finish(&result);
        result
    }
    fn flush(&mut self) -> io::Result<()> {
        panic!("有界发送不得增加 flush")
    }
}

#[test]
fn real_partial_read_timeout_preserves_progress_without_allocation() {
    let (reader, writer) = pipe();
    write_fd(writer.as_raw_fd(), Duration::ZERO, &[7, 8, 9]).unwrap();
    let clock = MockClock::default();
    let mut port = ClockedFd {
        fd: reader,
        timeout: Duration::ZERO,
        clock: clock.clone(),
    };
    let mut output = [0; 8];
    let result = assert_no_alloc::assert_no_alloc(|| {
        read_exact_bounded(&mut port, &clock, Duration::from_millis(50), &mut output)
    });
    assert!(matches!(
        result,
        Err(FeetechError::TransportTimeout {
            wanted: 8,
            got: 3,
            timeout_ms: 50
        })
    ));
    assert_eq!(&output[..3], &[7, 8, 9]);
}

#[test]
fn real_partial_write_timeout_and_source_deadline_preserve_progress_without_allocation() {
    for absolute in [false, true] {
        let (_reader, writer) = pipe();
        // SAFETY：F_GETPIPE_SZ 仅查询仍由 OwnedFd 持有的管道容量。
        let capacity = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETPIPE_SZ) };
        assert!(capacity > 0);
        let capacity = usize::try_from(capacity).unwrap();
        let bytes = vec![0x5a; capacity * 2];
        let clock = MockClock::default();
        let mut port = ClockedFd {
            fd: writer,
            timeout: Duration::ZERO,
            clock: clock.clone(),
        };
        if absolute {
            let source = SourceWriteDeadline::new(&clock, Deadline(MonoRawNs(50_000_000)));
            let result = assert_no_alloc::assert_no_alloc(|| {
                write_all_until_bounded(&mut port, source, Duration::from_millis(50), &bytes)
            });
            let Err(FeetechError::AbsoluteWriteFailure(failure)) = result else {
                panic!("必须保留来源超时")
            };
            assert_eq!(failure.reason, AbsoluteWriteReason::SourceExpired);
            assert_eq!((failure.wanted, failure.got), (bytes.len(), capacity));
            assert!(failure.write_attempted);
        } else {
            let result = assert_no_alloc::assert_no_alloc(|| {
                write_all_bounded(&mut port, &clock, Duration::from_millis(50), &bytes)
            });
            assert!(
                matches!(result, Err(FeetechError::TransportWriteTimeout { wanted, got, timeout_ms: 50 }) if wanted == bytes.len() && got == capacity)
            );
        }
    }
}

#[test]
fn real_clear_and_write_syscall_errors_do_not_allocate() {
    let (reader, _writer) = pipe();
    let result = assert_no_alloc::assert_no_alloc(|| clear_input(reader.as_raw_fd()));
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let result =
        assert_no_alloc::assert_no_alloc(|| write_fd(full.as_raw_fd(), Duration::ZERO, &[1]));
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
}
