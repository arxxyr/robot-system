//! `SerialTransport` trait + 内存 mock 实现（**仅** 单测 / dry-run 工具用）。
//!
//! 真 USB-RS485 实现（基于 `serialport` crate）由 task 28 加 `feature = "real-hw"`
//! 时引入；本文件 v0.2 阶段只做 trait + mock。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
/// - `write_all` 不应拆包（一个 instruction packet 一次写完）。
/// - `read_exact` 阻塞直到收到 `buf.len()` 字节或 [`SerialTransport::timeout`]
///   超时。
/// - `set_timeout` 影响 `read_exact` 的超时；`write_all` 不超时（仅依赖 OS
///   write buffer）。
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

    /// 设置 `read_exact` 的超时。
    ///
    /// # Errors
    /// 设置失败 → [`FeetechError::TransportRead`]（基础 IO 错）。
    fn set_timeout(&mut self, timeout: Duration) -> Result<(), FeetechError>;

    /// 调用方提示：当前传输是否已与硬件 / mock 建立。返回 `false` 时调用方应
    /// 直接报 [`FeetechError::NotConnected`]，避免徒劳尝试。
    fn is_connected(&self) -> bool;
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
                timeout: Duration::from_secs(1),
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
            return Err(FeetechError::TransportTimeout {
                timeout_ms: u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX),
                wanted: buf.len(),
                got: q.len(),
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
// Tests
// ============================================================================

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
        assert!(h.peek_tx().is_empty());
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
