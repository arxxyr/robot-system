//! `SerialTransport` trait + 内存 mock 实现 + `real-hw` feature gated 真 USB-RS485
//! 实现。
//!
//! - 默认编译路径：仅 trait + [`MockTransport`]（单测 / dry-run 工具）。
//! - `--features real-hw`：额外暴露 [`real::RealSerialTransport`]（拉 `serialport`
//!   crate，Linux 需 `libudev-dev`），task 28 真机阶段使用。

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use robot_core::time::RobotClock;

use crate::error::FeetechError;

// ============================================================================
// trait
// ============================================================================

/// 串口抽象 —— `FeetechBus` 通过它发包/收包。
///
/// 实现：
/// - **生产**（task 28+）：基于 `serialport::SerialPort`，feature gate `real-hw`。
/// - **测试**：[`MockTransport`] —— 内存里塞预写好的 motor 回包，验 host 发出
///   字节流。
///
/// 调用约定：
/// - 每次 `write_all` / `read_exact` 独享一个总调用预算；短 IO 不刷新起点。
/// - 每次底层 IO 前下发剩余预算；中断和暂时不可用只在原预算内继续。
/// - request + response 两次调用仍由 bus 事务租约共同保护，不自动重发整帧。
pub trait SerialTransport: Send {
    /// # Errors
    /// 写失败 → [`FeetechError::TransportWrite`]。
    fn write_all(&mut self, buf: &[u8]) -> Result<(), FeetechError>;

    /// 阻塞读 `buf.len()` 字节。
    ///
    /// # Errors
    /// - 超时 → [`FeetechError::TransportTimeout`]
    /// - IO 失败 → [`FeetechError::TransportRead`]
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), FeetechError>;

    /// 设置后续每次完整读或写调用的总预算。
    ///
    /// # Errors
    /// 设置失败 → [`FeetechError::TransportRead`]（基础 IO 错）。
    fn set_timeout(&mut self, timeout: Duration) -> Result<(), FeetechError>;

    /// 调用方提示：当前传输是否已与硬件 / mock 建立。返回 `false` 时调用方应
    /// 直接报 [`FeetechError::NotConnected`]，避免徒劳尝试。
    fn is_connected(&self) -> bool;

    /// 丢弃当前接收缓冲；失败必须作为独立安全事实传回。
    fn discard_input(&mut self) -> Result<(), FeetechError>;

    /// 分段读取并丢弃字节，等待连续安静窗口，绝对预算不因收到字节而重置。
    fn drain_until_quiet(
        &mut self,
        quiet_required: Duration,
        absolute_budget: Duration,
    ) -> Result<DrainReport, FeetechError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrainReport {
    pub bytes: usize,
    pub deadline_hit: bool,
}

/// 真实串口与独立短 IO 测试共用这个最小接口，避免测试复制截止时间算法。
#[cfg_attr(not(any(test, feature = "real-hw")), allow(dead_code))]
trait SerialPortTimeout {
    fn apply_timeout(&mut self, timeout: Duration) -> io::Result<()>;
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg_attr(not(any(test, feature = "real-hw")), allow(dead_code))]
fn timeout_error(budget: Duration, wanted: usize, got: usize, write: bool) -> FeetechError {
    let timeout_ms = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX);
    if write {
        FeetechError::TransportWriteTimeout {
            timeout_ms,
            wanted,
            got,
        }
    } else {
        FeetechError::TransportTimeout {
            timeout_ms,
            wanted,
            got,
        }
    }
}

#[cfg_attr(not(any(test, feature = "real-hw")), allow(dead_code))]
fn read_exact_bounded<P: Read + SerialPortTimeout + ?Sized>(
    port: &mut P,
    clock: &dyn RobotClock,
    budget: Duration,
    buf: &mut [u8],
) -> Result<(), FeetechError> {
    let deadline = clock.now().0.saturating_add(duration_ns(budget));
    let mut got = 0;
    while got < buf.len() {
        let remaining = deadline.saturating_sub(clock.now().0);
        if remaining == 0 {
            return Err(timeout_error(budget, buf.len(), got, false));
        }
        port.apply_timeout(Duration::from_nanos(remaining))
            .map_err(FeetechError::TransportRead)?;
        match port.read(&mut buf[got..]) {
            Ok(0) => {
                return Err(FeetechError::TransportRead(io::Error::from(
                    io::ErrorKind::UnexpectedEof,
                )));
            }
            Ok(count) => got += count,
            Err(error) => match error.kind() {
                io::ErrorKind::Interrupted
                | io::ErrorKind::WouldBlock
                | io::ErrorKind::TimedOut => {}
                _ => return Err(FeetechError::TransportRead(error)),
            },
        }
    }
    if clock.now().0 > deadline {
        return Err(timeout_error(budget, buf.len(), got, false));
    }
    Ok(())
}

#[cfg_attr(not(any(test, feature = "real-hw")), allow(dead_code))]
fn write_all_bounded<P: Write + SerialPortTimeout + ?Sized>(
    port: &mut P,
    clock: &dyn RobotClock,
    budget: Duration,
    buf: &[u8],
) -> Result<(), FeetechError> {
    let deadline = clock.now().0.saturating_add(duration_ns(budget));
    let mut got = 0;
    while got < buf.len() {
        let remaining = deadline.saturating_sub(clock.now().0);
        if remaining == 0 {
            return Err(timeout_error(budget, buf.len(), got, true));
        }
        port.apply_timeout(Duration::from_nanos(remaining))
            .map_err(FeetechError::TransportWrite)?;
        match port.write(&buf[got..]) {
            Ok(0) => {
                return Err(FeetechError::TransportWrite(io::Error::from(
                    io::ErrorKind::WriteZero,
                )));
            }
            Ok(count) => got += count,
            Err(error) => match error.kind() {
                io::ErrorKind::Interrupted
                | io::ErrorKind::WouldBlock
                | io::ErrorKind::TimedOut => {}
                _ => return Err(FeetechError::TransportWrite(error)),
            },
        }
    }
    if clock.now().0 > deadline {
        return Err(timeout_error(budget, buf.len(), got, true));
    }
    Ok(())
}

#[cfg_attr(not(any(test, feature = "real-hw")), allow(dead_code))]
fn drain_until_quiet_bounded<P: Read + SerialPortTimeout + ?Sized>(
    port: &mut P,
    clock: &dyn RobotClock,
    quiet_required: Duration,
    absolute_budget: Duration,
) -> Result<DrainReport, FeetechError> {
    let start = clock.now().0;
    let deadline = start.saturating_add(duration_ns(absolute_budget));
    let quiet_ns = duration_ns(quiet_required);
    let mut quiet_since = start;
    let mut bytes = 0_usize;
    let mut scratch = [0_u8; 64];
    loop {
        let now = clock.now().0;
        if now.saturating_sub(quiet_since) >= quiet_ns {
            return Ok(DrainReport {
                bytes,
                deadline_hit: false,
            });
        }
        let remaining = deadline.saturating_sub(now);
        if remaining == 0 {
            return Ok(DrainReport {
                bytes,
                deadline_hit: true,
            });
        }
        let slice = remaining
            .min(quiet_ns.saturating_sub(now.saturating_sub(quiet_since)))
            .min(1_000_000);
        port.apply_timeout(Duration::from_nanos(slice))
            .map_err(FeetechError::DiscardFailed)?;
        match port.read(&mut scratch) {
            Ok(0) => {
                return Err(FeetechError::DiscardFailed(io::Error::from(
                    io::ErrorKind::UnexpectedEof,
                )));
            }
            Ok(count) => {
                bytes = bytes.saturating_add(count);
                quiet_since = clock.now().0;
            }
            Err(error) => match error.kind() {
                io::ErrorKind::Interrupted
                | io::ErrorKind::WouldBlock
                | io::ErrorKind::TimedOut => {}
                _ => return Err(FeetechError::DiscardFailed(error)),
            },
        }
    }
}

// ============================================================================
// MockTransport
// ============================================================================

/// 内存 mock —— 单元测试用。
///
/// **行为**：
/// - `write_all` 把字节追加到 `tx_log`（调用方读出来比对）。
/// - `read_exact` 从 `rx_queue` 头部消费字节（调用方提前 `push_rx` 喂虚假 motor 回包）。
/// - `set_timeout` 仅记录值，不真等。
///
/// **线程安全**：内部用 `Arc<Mutex<...>>` 包裹 `tx_log` / `rx_queue`，因此可以
/// 把 [`MockTransport`] move 进 `FeetechBus`、同时调用方拿 [`MockHandle`] 在外面
/// 喂回包 / 验 tx 字节。
pub struct MockTransport {
    tx_log: Arc<Mutex<Vec<u8>>>,
    rx_queue: Arc<Mutex<VecDeque<u8>>>,
    timeout: Duration,
    connected: bool,
}

impl MockTransport {
    /// 构造一个空 mock + 对应 `MockHandle`。
    #[must_use]
    pub fn new() -> (Self, MockHandle) {
        let tx_log = Arc::new(Mutex::new(Vec::with_capacity(256)));
        let rx_queue = Arc::new(Mutex::new(VecDeque::with_capacity(256)));
        let handle = MockHandle {
            tx_log: Arc::clone(&tx_log),
            rx_queue: Arc::clone(&rx_queue),
        };
        (
            Self {
                tx_log,
                rx_queue,
                timeout: Duration::from_millis(50),
                connected: true,
            },
            handle,
        )
    }
}

impl SerialTransport for MockTransport {
    fn write_all(&mut self, buf: &[u8]) -> Result<(), FeetechError> {
        if !self.connected {
            return Err(FeetechError::NotConnected);
        }
        self.tx_log
            .lock()
            .map_err(|_| {
                FeetechError::TransportWrite(std::io::Error::other("mock tx_log poisoned"))
            })?
            .extend_from_slice(buf);
        Ok(())
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), FeetechError> {
        if !self.connected {
            return Err(FeetechError::NotConnected);
        }
        let mut q = self.rx_queue.lock().map_err(|_| {
            FeetechError::TransportRead(std::io::Error::other("mock rx_queue poisoned"))
        })?;
        if q.len() < buf.len() {
            // mock 立即报 timeout（真 transport 会等 self.timeout 才报）
            let got = q.len();
            for slot in &mut buf[..got] {
                *slot = q.pop_front().unwrap_or(0);
            }
            return Err(FeetechError::TransportTimeout {
                timeout_ms: u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX),
                wanted: buf.len(),
                got,
            });
        }
        for slot in buf.iter_mut() {
            *slot = q.pop_front().unwrap_or(0);
        }
        Ok(())
    }

    fn set_timeout(&mut self, timeout: Duration) -> Result<(), FeetechError> {
        self.timeout = timeout;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    fn discard_input(&mut self) -> Result<(), FeetechError> {
        // Mock 队列装的是后续请求的预编排响应，不模拟 OS 已到达的脏输入。
        Ok(())
    }

    fn drain_until_quiet(
        &mut self,
        quiet_required: Duration,
        absolute_budget: Duration,
    ) -> Result<DrainReport, FeetechError> {
        // 当前队列是已经可读的输入，全部消费计数；后续响应应由测试按 write 注入。
        // Mock 的等待采用虚拟时间，不真实 sleep；预算不足时必须明确标记截止命中。
        let mut rx = self
            .rx_queue
            .lock()
            .map_err(|_| FeetechError::DiscardFailed(io::Error::other("mock rx_queue poisoned")))?;
        let bytes = rx.len();
        rx.clear();
        Ok(DrainReport {
            bytes,
            deadline_hit: quiet_required > absolute_budget,
        })
    }
}

/// 测试 helper：驱动 [`MockTransport`] 内部状态。
#[derive(Clone)]
pub struct MockHandle {
    tx_log: Arc<Mutex<Vec<u8>>>,
    rx_queue: Arc<Mutex<VecDeque<u8>>>,
}

impl MockHandle {
    /// 喂虚假 motor 回包字节（按收到的顺序追加到队列尾）。
    pub fn push_rx(&self, bytes: &[u8]) {
        let mut q = self.rx_queue.lock().expect("mock rx_queue poisoned");
        q.extend(bytes.iter().copied());
    }

    /// 拿走当前累计的 host TX 字节（清零内部 buffer）。
    #[must_use]
    pub fn drain_tx(&self) -> Vec<u8> {
        let mut buf = self.tx_log.lock().expect("mock tx_log poisoned");
        std::mem::take(&mut *buf)
    }

    /// 不清空地 peek。
    #[must_use]
    pub fn peek_tx(&self) -> Vec<u8> {
        self.tx_log.lock().expect("mock tx_log poisoned").clone()
    }
}

// ============================================================================
// Real serial transport（feature = "real-hw"）
// ============================================================================

/// `serialport` crate 的 [`SerialTransport`] 包装。**仅** `--features real-hw`
/// 编译。
///
/// task 28 真机阶段使用：`/dev/serial/by-id/usb-...` 路径 + 1 Mbps baudrate +
/// 50 ms 默认调用预算。dimos `feetech_io/patches.py` 的 `_patched_setPacketTimeout`
/// **不需要**，那是 `scservo_sdk` Python sdk 的 timeout 计算 bug，Rust 端走
/// `serialport::SerialPort::set_timeout` 直传。
#[cfg(feature = "real-hw")]
pub mod real {
    use super::{
        Arc, DrainReport, Duration, FeetechError, RobotClock, SerialPortTimeout, SerialTransport,
        drain_until_quiet_bounded, read_exact_bounded, write_all_bounded,
    };
    use std::path::Path;

    /// 真 USB-RS485 串口包装。
    pub struct RealSerialTransport {
        port: Box<dyn serialport::SerialPort>,
        path: std::path::PathBuf,
        clock: Arc<dyn RobotClock>,
        call_timeout: Duration,
    }

    impl RealSerialTransport {
        /// 打开串口。`path` 推荐用 `/dev/serial/by-id/usb-...`（udev 稳定路径，
        /// reboot / 拔插顺序变化也不漂移）；ttyACM* 是动态分配，**不**推荐。
        ///
        /// `baudrate` 典型 `1_000_000`（[`crate::registers::DEFAULT_BAUDRATE`]）。
        ///
        /// # Errors
        /// open 失败 → [`FeetechError::TransportOpen`]。
        pub fn open(path: &Path, baudrate: u32) -> Result<Self, FeetechError> {
            Self::open_with_clock(
                path,
                baudrate,
                Arc::new(robot_core::time::SystemClock::new()),
            )
        }

        /// 构造期注入运行时共同的单调时钟。
        pub fn open_with_clock(
            path: &Path,
            baudrate: u32,
            clock: Arc<dyn RobotClock>,
        ) -> Result<Self, FeetechError> {
            let call_timeout = Duration::from_millis(50);
            let port = serialport::new(path.to_string_lossy(), baudrate)
                .timeout(call_timeout)
                .open()
                .map_err(|e| FeetechError::TransportOpen {
                    path: path.to_path_buf(),
                    source: std::io::Error::other(e.to_string()),
                })?;
            Ok(Self {
                port,
                path: path.to_path_buf(),
                clock,
                call_timeout,
            })
        }

        /// 用于 Display / 日志。
        #[must_use]
        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl SerialPortTimeout for dyn serialport::SerialPort {
        fn apply_timeout(&mut self, timeout: Duration) -> std::io::Result<()> {
            self.set_timeout(timeout)
                .map_err(|error| std::io::Error::other(error.to_string()))
        }
    }

    impl SerialTransport for RealSerialTransport {
        fn write_all(&mut self, buf: &[u8]) -> Result<(), FeetechError> {
            write_all_bounded(&mut *self.port, self.clock.as_ref(), self.call_timeout, buf)
        }

        fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), FeetechError> {
            read_exact_bounded(&mut *self.port, self.clock.as_ref(), self.call_timeout, buf)
        }

        fn set_timeout(&mut self, timeout: Duration) -> Result<(), FeetechError> {
            self.port
                .set_timeout(timeout)
                .map_err(|e| FeetechError::TransportRead(std::io::Error::other(e.to_string())))?;
            self.call_timeout = timeout;
            Ok(())
        }

        fn is_connected(&self) -> bool {
            // serialport 没显式 is_open；open 后视为 connected，错误时调用方报
            // TransportRead/Write 自动失败。
            true
        }

        fn discard_input(&mut self) -> Result<(), FeetechError> {
            self.port
                .clear(serialport::ClearBuffer::Input)
                .map_err(|error| {
                    FeetechError::DiscardFailed(std::io::Error::other(error.to_string()))
                })
        }

        fn drain_until_quiet(
            &mut self,
            quiet_required: Duration,
            absolute_budget: Duration,
        ) -> Result<DrainReport, FeetechError> {
            drain_until_quiet_bounded(
                &mut *self.port,
                self.clock.as_ref(),
                quiet_required,
                absolute_budget,
            )
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[path = "post_e_transport_acceptance.rs"]
mod post_e_acceptance;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_drain_roundtrip() {
        let (mut t, h) = MockTransport::new();
        t.write_all(&[1, 2, 3]).unwrap();
        t.write_all(&[4, 5]).unwrap();
        assert_eq!(h.drain_tx(), vec![1, 2, 3, 4, 5]);
        // drain 之后再 peek 应空
        assert!(
            h.peek_tx().is_empty(),
            "expected empty tx, got {:?}",
            h.peek_tx()
        );
    }

    #[test]
    fn push_rx_then_read_exact() {
        let (mut t, h) = MockTransport::new();
        h.push_rx(&[0xFF, 0xFF, 0x01, 0x02, 0x00, 0xFC]);
        let mut buf = [0u8; 6];
        t.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [0xFF, 0xFF, 0x01, 0x02, 0x00, 0xFC]);
    }

    #[test]
    fn read_exact_short_returns_timeout() {
        let (mut t, h) = MockTransport::new();
        h.push_rx(&[0xFF, 0xFF]);
        let mut buf = [0u8; 6];
        match t.read_exact(&mut buf) {
            Err(FeetechError::TransportTimeout {
                wanted: 6, got: 2, ..
            }) => {}
            other => panic!("expected TransportTimeout{{6,2}}, got {other:?}"),
        }
    }

    #[test]
    fn set_timeout_does_not_fail() {
        let (mut t, _h) = MockTransport::new();
        t.set_timeout(Duration::from_millis(50)).unwrap();
    }
}
