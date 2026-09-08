//! 独立传输验收：夹具只执行单次底层 IO，不实现整包 deadline。
//! 被测循环与真串口使用同一份生产实现，所有时间由注入时钟确定。

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::time::Duration;

use robot_core::time::{DurationNs, MockClock, MonoRawNs, RobotClock};

use super::{SerialPortTimeout, drain_until_quiet_bounded, read_exact_bounded, write_all_bounded};
use crate::error::FeetechError;

#[derive(Clone, Copy)]
enum Outcome {
    Transfer(usize),
    Error(io::ErrorKind),
}

#[derive(Clone, Copy)]
struct Step {
    elapsed_ms: u64,
    outcome: Outcome,
}

struct PartialPort {
    clock: MockClock,
    steps: VecDeque<Step>,
    timeout: Duration,
    installed_timeouts: Vec<Duration>,
    written: Vec<u8>,
    read_value: u8,
    calls: usize,
    fail_timeout: bool,
}

impl PartialPort {
    fn new(clock: &MockClock, steps: impl IntoIterator<Item = Step>) -> Self {
        Self {
            clock: clock.clone(),
            steps: steps.into_iter().collect(),
            timeout: Duration::ZERO,
            installed_timeouts: Vec::new(),
            written: Vec::new(),
            read_value: 0xa0,
            calls: 0,
            fail_timeout: false,
        }
    }

    fn perform_once(&mut self) -> io::Result<usize> {
        self.calls += 1;
        let step = self.steps.pop_front().expect("不得发起脚本之外的 IO");
        let elapsed = Duration::from_millis(step.elapsed_ms);
        let spent = elapsed.min(self.timeout);
        self.clock
            .advance(DurationNs(u64::try_from(spent.as_nanos()).unwrap()));
        // 模拟底层调用遵守 apply_timeout；是否重试及整包是否到期由生产循环决定。
        if elapsed > self.timeout {
            return Err(io::ErrorKind::TimedOut.into());
        }
        match step.outcome {
            Outcome::Transfer(count) => Ok(count),
            Outcome::Error(kind) => Err(kind.into()),
        }
    }
}

impl SerialPortTimeout for PartialPort {
    fn apply_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        if self.fail_timeout {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        self.timeout = timeout;
        self.installed_timeouts.push(timeout);
        Ok(())
    }
}

impl Read for PartialPort {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.perform_once()?.min(buf.len());
        for byte in &mut buf[..count] {
            *byte = self.read_value;
            self.read_value = self.read_value.wrapping_add(1);
        }
        Ok(count)
    }
}

impl Write for PartialPort {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let count = self.perform_once()?.min(buf.len());
        self.written.extend_from_slice(&buf[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        panic!("有界写不得额外插入无预算 flush")
    }
}

#[test]
fn read_drip_preserves_got_and_one_absolute_deadline() {
    let clock = MockClock::new(MonoRawNs(1_000_000));
    let mut port = PartialPort::new(
        &clock,
        [Step {
            elapsed_ms: 20,
            outcome: Outcome::Transfer(1),
        }; 3],
    );
    let mut bytes = [0_u8; 4];
    let error =
        read_exact_bounded(&mut port, &clock, Duration::from_millis(50), &mut bytes).unwrap_err();
    assert!(matches!(
        error,
        FeetechError::TransportTimeout {
            wanted: 4,
            got: 2,
            timeout_ms: 50
        }
    ));
    assert_eq!(&bytes[..2], &[0xa0, 0xa1]);
    assert_eq!(clock.now(), MonoRawNs(51_000_000));
    assert_eq!(port.calls, 3);
    assert_eq!(
        port.installed_timeouts,
        [50, 30, 10].map(Duration::from_millis)
    );
}

#[test]
fn write_drip_reports_actual_progress_and_tx_timeout_kind() {
    let clock = MockClock::default();
    let mut port = PartialPort::new(
        &clock,
        [Step {
            elapsed_ms: 20,
            outcome: Outcome::Transfer(2),
        }; 3],
    );
    let error = write_all_bounded(
        &mut port,
        &clock,
        Duration::from_millis(50),
        &[1, 2, 3, 4, 5, 6],
    )
    .unwrap_err();
    assert!(matches!(
        error,
        FeetechError::TransportWriteTimeout {
            wanted: 6,
            got: 4,
            timeout_ms: 50
        }
    ));
    assert_eq!(port.written, [1, 2, 3, 4]);
    assert_eq!(clock.now(), MonoRawNs(50_000_000));
    assert_eq!(
        port.installed_timeouts,
        [50, 30, 10].map(Duration::from_millis)
    );
}

#[test]
fn interrupted_and_would_block_retry_only_the_remaining_budget() {
    let clock = MockClock::default();
    let mut port = PartialPort::new(
        &clock,
        [
            Step {
                elapsed_ms: 7,
                outcome: Outcome::Error(io::ErrorKind::Interrupted),
            },
            Step {
                elapsed_ms: 13,
                outcome: Outcome::Error(io::ErrorKind::WouldBlock),
            },
            Step {
                elapsed_ms: 20,
                outcome: Outcome::Transfer(1),
            },
        ],
    );
    let mut byte = [0];
    read_exact_bounded(&mut port, &clock, Duration::from_millis(50), &mut byte).unwrap();
    assert_eq!(byte, [0xa0]);
    assert_eq!(clock.now(), MonoRawNs(40_000_000));
    assert_eq!(
        port.installed_timeouts,
        [50, 43, 30].map(Duration::from_millis)
    );
}

#[test]
fn native_timeout_before_absolute_deadline_does_not_end_the_packet() {
    let clock = MockClock::default();
    let mut port = PartialPort::new(
        &clock,
        [
            Step {
                elapsed_ms: 10,
                outcome: Outcome::Error(io::ErrorKind::TimedOut),
            },
            Step {
                elapsed_ms: 10,
                outcome: Outcome::Transfer(2),
            },
        ],
    );
    write_all_bounded(&mut port, &clock, Duration::from_millis(50), &[0xb1, 0xb2]).unwrap();
    assert_eq!(port.written, [0xb1, 0xb2]);
    assert_eq!(clock.now(), MonoRawNs(20_000_000));
    assert_eq!(port.installed_timeouts, [50, 40].map(Duration::from_millis));
}

#[test]
fn next_call_gets_a_fresh_budget_after_a_short_success() {
    let clock = MockClock::default();
    let mut port = PartialPort::new(
        &clock,
        [
            Step {
                elapsed_ms: 20,
                outcome: Outcome::Transfer(1),
            },
            Step {
                elapsed_ms: 25,
                outcome: Outcome::Transfer(1),
            },
            Step {
                elapsed_ms: 40,
                outcome: Outcome::Transfer(1),
            },
        ],
    );
    let mut first = [0; 2];
    read_exact_bounded(&mut port, &clock, Duration::from_millis(50), &mut first).unwrap();
    assert_eq!(first, [0xa0, 0xa1]);
    assert_eq!(port.timeout, Duration::from_millis(30));
    let mut second = [0];
    read_exact_bounded(&mut port, &clock, Duration::from_millis(50), &mut second).unwrap();
    assert_eq!(second, [0xa2]);
    assert_eq!(clock.now(), MonoRawNs(85_000_000));
    assert_eq!(
        port.installed_timeouts,
        [50, 30, 50].map(Duration::from_millis)
    );
}

#[test]
fn empty_buffers_do_not_start_io_or_install_a_timeout() {
    let clock = MockClock::default();
    let mut port = PartialPort::new(&clock, []);
    read_exact_bounded(&mut port, &clock, Duration::from_millis(50), &mut []).unwrap();
    write_all_bounded(&mut port, &clock, Duration::from_millis(50), &[]).unwrap();
    assert_eq!(port.calls, 0);
    assert_eq!(port.installed_timeouts, [] as [Duration; 0]);
}

#[test]
fn zero_progress_and_terminal_io_errors_do_not_spin_or_retry() {
    for outcome in [
        Outcome::Transfer(0),
        Outcome::Error(io::ErrorKind::BrokenPipe),
    ] {
        let clock = MockClock::default();
        let mut port = PartialPort::new(
            &clock,
            [Step {
                elapsed_ms: 1,
                outcome,
            }],
        );
        assert!(
            read_exact_bounded(&mut port, &clock, Duration::from_millis(50), &mut [0]).is_err()
        );
        assert_eq!(port.calls, 1);
        let mut port = PartialPort::new(
            &clock,
            [Step {
                elapsed_ms: 1,
                outcome,
            }],
        );
        assert!(write_all_bounded(&mut port, &clock, Duration::from_millis(50), &[1]).is_err());
        assert_eq!(port.calls, 1);
    }
}

#[test]
fn continuous_noise_cannot_extend_the_drain_absolute_deadline() {
    let clock = MockClock::default();
    let steps = [Step {
        elapsed_ms: 1,
        outcome: Outcome::Transfer(1),
    }; 150];
    let mut port = PartialPort::new(&clock, steps);
    let report = drain_until_quiet_bounded(
        &mut port,
        &clock,
        Duration::from_millis(50),
        Duration::from_millis(150),
    )
    .unwrap();
    assert!(report.deadline_hit);
    assert_eq!(report.bytes, 150);
    assert_eq!(clock.now(), MonoRawNs(150_000_000));
    assert_eq!(port.calls, 150);
    assert!(
        port.installed_timeouts
            .iter()
            .all(|timeout| *timeout == Duration::from_millis(1))
    );
}

#[test]
fn a_received_byte_restarts_only_the_quiet_window() {
    let clock = MockClock::default();
    let quiet = Step {
        elapsed_ms: 1,
        outcome: Outcome::Error(io::ErrorKind::TimedOut),
    };
    let steps = std::iter::repeat_n(quiet, 10)
        .chain([Step {
            elapsed_ms: 1,
            outcome: Outcome::Transfer(1),
        }])
        .chain(std::iter::repeat_n(quiet, 50));
    let mut port = PartialPort::new(&clock, steps);
    let report = drain_until_quiet_bounded(
        &mut port,
        &clock,
        Duration::from_millis(50),
        Duration::from_millis(150),
    )
    .unwrap();
    assert!(!report.deadline_hit);
    assert_eq!(report.bytes, 1);
    assert_eq!(clock.now(), MonoRawNs(61_000_000));
    assert_eq!(port.calls, 61);
    assert!(
        port.installed_timeouts
            .iter()
            .all(|timeout| *timeout == Duration::from_millis(1))
    );
}

#[test]
fn drain_read_errors_are_classified_as_discard_failures() {
    let clock = MockClock::default();
    let mut port = PartialPort::new(
        &clock,
        [Step {
            elapsed_ms: 1,
            outcome: Outcome::Error(io::ErrorKind::BrokenPipe),
        }],
    );
    assert!(matches!(
        drain_until_quiet_bounded(
            &mut port,
            &clock,
            Duration::from_millis(50),
            Duration::from_millis(150)
        ),
        Err(FeetechError::DiscardFailed(_))
    ));
    assert_eq!(port.calls, 1);
}

#[test]
fn timeout_install_failure_prevents_all_following_io() {
    let clock = MockClock::default();
    let mut port = PartialPort::new(&clock, []);
    port.fail_timeout = true;
    assert!(matches!(
        read_exact_bounded(&mut port, &clock, Duration::from_millis(50), &mut [0]),
        Err(FeetechError::TransportRead(_))
    ));
    assert!(matches!(
        write_all_bounded(&mut port, &clock, Duration::from_millis(50), &[1]),
        Err(FeetechError::TransportWrite(_))
    ));
    assert!(matches!(
        drain_until_quiet_bounded(
            &mut port,
            &clock,
            Duration::from_millis(50),
            Duration::from_millis(150)
        ),
        Err(FeetechError::DiscardFailed(_))
    ));
    assert_eq!(port.calls, 0);
    assert_eq!(clock.now(), MonoRawNs(0));
}
