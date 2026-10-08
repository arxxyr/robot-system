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

/// 固定夹具只完成单次 IO；期限和重试仍完全由同一份生产核心决定。
mod absolute {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::{Arc, Mutex};

    use robot_core::control_authority::{
        AuthorityInit, ControlAuthority, EnableResult, InhibitSet, NormalIoIntent,
    };
    use robot_core::error::RobotError;
    use robot_core::executor_ctrl::OutputSafetyState;
    use robot_core::serial_services::{
        AbortReason, BusLease, BusOpClass, CommErrorKind, LeasedPreemptProbe, PreemptProbe,
    };
    use robot_core::time::Deadline;

    use super::*;
    use crate::bus::FeetechBus;
    use crate::error::{AbsoluteWriteFailure, AbsoluteWriteReason as R};
    use crate::transport::{
        DeadlinePortTimeout, DrainReport, SerialTransport, SourceWriteDeadline,
        write_all_until_bounded,
    };

    #[derive(Clone, Copy)]
    struct NativeStep {
        elapsed_ns: u64,
        outcome: Outcome,
        clock_override: Option<MonoRawNs>,
    }

    const fn transfer(elapsed_ns: u64, count: usize) -> NativeStep {
        NativeStep {
            elapsed_ns,
            outcome: Outcome::Transfer(count),
            clock_override: None,
        }
    }

    const fn io_error(elapsed_ns: u64, kind: io::ErrorKind) -> NativeStep {
        NativeStep {
            elapsed_ns,
            outcome: Outcome::Error(kind),
            clock_override: None,
        }
    }

    #[derive(Clone, Copy)]
    struct TimeoutStep {
        elapsed_ns: u64,
        error: Option<io::ErrorKind>,
    }

    struct FixedPort {
        clock: MockClock,
        steps: [NativeStep; 16],
        step_count: usize,
        calls: usize,
        timeout_steps: [TimeoutStep; 16],
        timeouts: [Duration; 16],
        timeout_calls: usize,
        written: [u8; 256],
        written_len: usize,
        request_first: [u8; 16],
        request_len: [usize; 16],
        legacy_calls: usize,
    }

    impl FixedPort {
        fn new(clock: &MockClock, steps: &[NativeStep]) -> Self {
            assert!(steps.len() <= 16);
            let mut value = Self {
                clock: clock.clone(),
                steps: [transfer(0, 0); 16],
                step_count: steps.len(),
                calls: 0,
                timeout_steps: [TimeoutStep {
                    elapsed_ns: 0,
                    error: None,
                }; 16],
                timeouts: [Duration::ZERO; 16],
                timeout_calls: 0,
                written: [0; 256],
                written_len: 0,
                request_first: [0; 16],
                request_len: [0; 16],
                legacy_calls: 0,
            };
            value.steps[..steps.len()].copy_from_slice(steps);
            value
        }
    }

    impl DeadlinePortTimeout for FixedPort {
        fn apply_timeout_kind(&mut self, timeout: Duration) -> Result<(), io::ErrorKind> {
            let index = self.timeout_calls;
            let step = self.timeout_steps[index];
            self.timeouts[index] = timeout;
            self.timeout_calls += 1;
            self.clock.advance(DurationNs(step.elapsed_ns));
            match step.error {
                Some(kind) => Err(kind),
                None => Ok(()),
            }
        }
    }

    impl Write for FixedPort {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let index = self.calls;
            assert!(index < self.step_count, "不得触发脚本之外的 Write");
            let step = self.steps[index];
            self.calls += 1;
            self.request_first[index] = buf[0];
            self.request_len[index] = buf.len();
            self.clock.advance(DurationNs(step.elapsed_ns));
            if let Some(now) = step.clock_override {
                self.clock.set(now);
            }
            match step.outcome {
                Outcome::Transfer(count) => {
                    if count <= buf.len() {
                        self.written[self.written_len..self.written_len + count]
                            .copy_from_slice(&buf[..count]);
                        self.written_len += count;
                    }
                    Ok(count)
                }
                Outcome::Error(kind) => Err(kind.into()),
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            panic!("绝对期限路径不得 flush")
        }
    }

    fn context(clock: &dyn RobotClock, source: u64) -> SourceWriteDeadline<'_> {
        SourceWriteDeadline::new(clock, Deadline(MonoRawNs(source)))
    }

    fn failure(error: FeetechError) -> AbsoluteWriteFailure {
        match error {
            FeetechError::AbsoluteWriteFailure(value) => value,
            other => panic!("不是固定绝对期限错误：{other:?}"),
        }
    }

    fn failure_ref(error: &FeetechError) -> &AbsoluteWriteFailure {
        match error {
            FeetechError::AbsoluteWriteFailure(value) => value,
            other => panic!("不是固定绝对期限错误：{other:?}"),
        }
    }

    struct NoReadClock;
    impl RobotClock for NoReadClock {
        fn now(&self) -> MonoRawNs {
            panic!("该前置拒绝不得读取时钟")
        }
        fn sleep_until(&self, _deadline: Deadline) {
            panic!("不得 sleep")
        }
        fn sleep_for(&self, _duration: DurationNs) {
            panic!("不得 sleep")
        }
    }

    struct DefaultReject;
    impl SerialTransport for DefaultReject {
        fn write_all(&mut self, _buf: &[u8]) -> Result<(), FeetechError> {
            panic!("不得回退旧写")
        }
        fn read_exact(&mut self, _buf: &mut [u8]) -> Result<(), FeetechError> {
            panic!("不得读ACK")
        }
        fn set_timeout(&mut self, _timeout: Duration) -> Result<(), FeetechError> {
            panic!("不得设置旧预算")
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn discard_input(&mut self) -> Result<(), FeetechError> {
            panic!("不得清理")
        }
        fn drain_until_quiet(
            &mut self,
            _quiet: Duration,
            _budget: Duration,
        ) -> Result<DrainReport, FeetechError> {
            panic!("不得 drain")
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Trace {
        Empty,
        Begin,
        Enter,
        Return,
        Checkpoint,
        Finish,
        Rejected,
    }
    struct FixedTrace {
        entries: [Trace; 32],
        len: usize,
    }

    struct LeaseProbe {
        authority: ControlAuthority,
        trace: Mutex<FixedTrace>,
        abort: AtomicU8,
        clock: MockClock,
        acquire_elapsed_ns: u64,
    }

    impl LeaseProbe {
        fn new(clock: &MockClock, acquire_elapsed_ns: u64) -> Self {
            let authority = ControlAuthority::new(AuthorityInit::new(
                false,
                OutputSafetyState::TorqueOffConfirmed,
                InhibitSet::EMPTY,
            ))
            .unwrap();
            let (owner, view) = authority.reserve_enable().unwrap();
            let execution = authority.start_enable(view).unwrap();
            authority
                .finish_enable(execution, EnableResult::Succeeded)
                .unwrap();
            authority.commit_enable(owner).unwrap();
            let trace = Mutex::new(FixedTrace {
                entries: [Trace::Empty; 32],
                len: 0,
            });
            // macOS std::Mutex 首锁可初始化原生锁；固定捕获夹具在guard外完成。
            drop(trace.lock().unwrap());
            Self {
                authority,
                trace,
                abort: AtomicU8::new(0),
                clock: clock.clone(),
                acquire_elapsed_ns,
            }
        }
        fn record(&self, item: Trace) {
            let mut trace = self.trace.lock().unwrap();
            let index = trace.len;
            trace.entries[index] = item;
            trace.len += 1;
        }
        fn assert_trace(&self, expected: &[Trace]) {
            let trace = self.trace.lock().unwrap();
            assert_eq!(&trace.entries[..trace.len], expected);
            assert!(!self.authority.snapshot().normal_io_active);
        }
    }

    impl PreemptProbe for LeaseProbe {
        fn should_abort(&self, _class: BusOpClass) -> Option<AbortReason> {
            match self.abort.load(Ordering::Acquire) {
                0 => None,
                1 => Some(AbortReason::Emergency),
                _ => Some(AbortReason::Revoked),
            }
        }
        fn checkpoint(&self) {
            assert!(self.authority.snapshot().normal_io_active);
            self.record(Trace::Checkpoint);
        }
        fn sleep_or_abort(
            &self,
            class: BusOpClass,
            _duration: DurationNs,
        ) -> Result<(), AbortReason> {
            self.should_abort(class).map_or(Ok(()), Err)
        }
        fn operation_class(&self) -> BusOpClass {
            BusOpClass::NormalEffectful
        }
        fn confirmation_trusted(&self) -> bool {
            true
        }
        fn report_comm_error(&self, _kind: CommErrorKind) {
            panic!("夹具不替代故障路由")
        }
    }

    impl LeasedPreemptProbe for LeaseProbe {
        fn begin_leased(&self, class: BusOpClass) -> Result<BusLease, AbortReason> {
            if let Some(reason) = self.should_abort(class) {
                self.record(Trace::Rejected);
                return Err(reason);
            }
            let lease = self
                .authority
                .acquire_normal_io(NormalIoIntent::Motion)
                .map_err(|_| AbortReason::Revoked)?;
            let wrapper = BusLease::with_normal(class, lease).unwrap();
            assert!(self.authority.snapshot().normal_io_active);
            self.record(Trace::Begin);
            self.clock.advance(DurationNs(self.acquire_elapsed_ns));
            Ok(wrapper)
        }
        fn finish_leased(&self, lease: BusLease) {
            assert!(self.authority.snapshot().normal_io_active);
            lease.release(&self.authority).unwrap();
            self.record(Trace::Finish);
        }
    }

    struct FixedTransport {
        port: Arc<Mutex<FixedPort>>,
        probe: Arc<LeaseProbe>,
        budget: Duration,
    }
    impl SerialTransport for FixedTransport {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), FeetechError> {
            let mut port = self.port.lock().unwrap();
            port.legacy_calls += 1;
            let start = port.written_len;
            port.written[start..start + bytes.len()].copy_from_slice(bytes);
            port.written_len += bytes.len();
            Ok(())
        }
        fn write_all_until(
            &mut self,
            bytes: &[u8],
            deadline: SourceWriteDeadline<'_>,
        ) -> Result<(), FeetechError> {
            assert!(self.probe.authority.snapshot().normal_io_active);
            self.probe.record(Trace::Enter);
            let result = write_all_until_bounded(
                &mut *self.port.lock().unwrap(),
                deadline,
                self.budget,
                bytes,
            );
            assert!(self.probe.authority.snapshot().normal_io_active);
            self.probe.record(Trace::Return);
            result
        }
        fn read_exact(&mut self, _buf: &mut [u8]) -> Result<(), FeetechError> {
            panic!("broadcast不得读取ACK")
        }
        fn set_timeout(&mut self, timeout: Duration) -> Result<(), FeetechError> {
            self.budget = timeout;
            Ok(())
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn discard_input(&mut self) -> Result<(), FeetechError> {
            panic!("本件不得清理")
        }
        fn drain_until_quiet(
            &mut self,
            _quiet: Duration,
            _budget: Duration,
        ) -> Result<DrainReport, FeetechError> {
            panic!("本件不得drain")
        }
    }

    const WHEELS: [(u8, i32); 3] = [(8, 7), (7, -9), (9, 0)];
    const GOLDEN: [u8; 17] = [
        0xff, 0xff, 0xfe, 13, 0x83, 46, 2, 8, 7, 0, 7, 9, 0x80, 9, 0, 0, 0x99,
    ];

    fn bus_fixture(
        clock: &MockClock,
        steps: &[NativeStep],
        acquire_ns: u64,
    ) -> (
        FeetechBus<FixedTransport>,
        Arc<LeaseProbe>,
        Arc<Mutex<FixedPort>>,
    ) {
        let probe = Arc::new(LeaseProbe::new(clock, acquire_ns));
        let port = Arc::new(Mutex::new(FixedPort::new(clock, steps)));
        // 此锁只用于夹具观测，初始化不属于所测生产写核心。
        drop(port.lock().unwrap());
        let transport = FixedTransport {
            port: Arc::clone(&port),
            probe: Arc::clone(&probe),
            budget: Duration::from_nanos(100),
        };
        let mut bus = FeetechBus::new(transport, HashMap::new()).unwrap();
        bus.set_services(Some(probe.clone()), None);
        (bus, probe, port)
    }

    #[test]
    fn absolute_default_refuses_even_empty_without_legacy_io() {
        let mut transport = DefaultReject;
        let object: &mut dyn SerialTransport = &mut transport;
        for bytes in [&[][..], &[1, 2][..]] {
            for limit in [0, 100] {
                let error = failure(
                    object
                        .write_all_until(bytes, context(&NoReadClock, limit))
                        .unwrap_err(),
                );
                assert_eq!(
                    error,
                    AbsoluteWriteFailure::before_write(
                        R::Unsupported,
                        bytes.len(),
                        Deadline(MonoRawNs(limit)),
                        None
                    )
                );
            }
        }
    }

    #[test]
    fn absolute_source_equal_or_past_rejects_before_first_native_write() {
        for now in [10, 11] {
            let clock = MockClock::new(MonoRawNs(now));
            let mut port = FixedPort::new(&clock, &[]);
            let error = failure(
                write_all_until_bounded(
                    &mut port,
                    context(&clock, 10),
                    Duration::from_nanos(100),
                    &[1, 2, 3],
                )
                .unwrap_err(),
            );
            assert_eq!(error.reason, R::SourceExpired);
            assert_eq!(
                (error.wanted, error.got, error.write_attempted),
                (3, 0, false)
            );
            assert_eq!(error.observed_at, Some(MonoRawNs(now)));
            assert_eq!(error.call_deadline, None);
            assert_eq!((port.calls, port.timeout_calls), (0, 0));
        }
    }

    #[test]
    fn absolute_context_does_not_refresh_after_prepare_delay() {
        let clock = MockClock::default();
        let deadline = context(&clock, 10);
        let mut port = FixedPort::new(&clock, &[]);
        clock.advance(DurationNs(11));
        let error = failure(
            write_all_until_bounded(&mut port, deadline, Duration::from_nanos(100), &[1, 2])
                .unwrap_err(),
        );
        assert_eq!(error.source_deadline, Deadline(MonoRawNs(10)));
        assert_eq!(error.reason, R::SourceExpired);
        assert_eq!((port.calls, port.timeout_calls), (0, 0));
    }

    #[test]
    fn absolute_native_timeout_is_min_of_source_and_call_budget() {
        for (source, budget, steps, wanted_timeouts, reason) in [
            (
                30,
                50,
                vec![transfer(10, 1); 3],
                vec![30, 20, 10],
                R::SourceExpired,
            ),
            (
                100,
                20,
                vec![transfer(10, 1); 2],
                vec![20, 10],
                R::CallBudgetExpired,
            ),
        ] {
            let clock = MockClock::default();
            let mut port = FixedPort::new(&clock, &steps);
            let error = failure(
                write_all_until_bounded(
                    &mut port,
                    context(&clock, source),
                    Duration::from_nanos(budget),
                    &[1, 2, 3],
                )
                .unwrap_err(),
            );
            assert_eq!(error.reason, reason);
            assert_eq!(
                &port.timeouts[..port.timeout_calls],
                wanted_timeouts
                    .iter()
                    .copied()
                    .map(Duration::from_nanos)
                    .collect::<Vec<_>>()
            );
            assert_eq!(error.call_deadline, Some(Deadline(MonoRawNs(budget))));
            assert_eq!(error.source_deadline, Deadline(MonoRawNs(source)));
        }
    }

    #[test]
    fn absolute_equal_limits_have_deterministic_source_reason() {
        for elapsed in [10, 11] {
            let clock = MockClock::default();
            let mut port = FixedPort::new(&clock, &[transfer(elapsed, 1)]);
            let error = failure(
                write_all_until_bounded(
                    &mut port,
                    context(&clock, 10),
                    Duration::from_nanos(10),
                    &[1],
                )
                .unwrap_err(),
            );
            assert_eq!(error.reason, R::SourceExpired);
            assert_eq!(error.got, 1);
            assert_eq!(error.call_deadline, Some(error.source_deadline));
        }
    }

    #[test]
    fn absolute_partial_expiry_preserves_prefix_without_packet_replay() {
        let clock = MockClock::default();
        let mut port = FixedPort::new(&clock, &[transfer(10, 2), transfer(10, 2)]);
        let error = failure(
            write_all_until_bounded(
                &mut port,
                context(&clock, 20),
                Duration::from_nanos(100),
                &[1, 2, 3, 4, 5, 6],
            )
            .unwrap_err(),
        );
        assert_eq!(error.reason, R::SourceExpired);
        assert_eq!(
            (error.wanted, error.got, error.write_attempted),
            (6, 4, true)
        );
        assert_eq!(&port.written[..port.written_len], [1, 2, 3, 4]);
        assert_eq!(&port.request_len[..port.calls], [6, 4]);
        assert_eq!(&port.request_first[..port.calls], [1, 3]);
    }

    #[test]
    fn absolute_full_write_returning_at_or_after_limit_is_failure() {
        for (source, budget, reason) in
            [(10, 100, R::SourceExpired), (100, 10, R::CallBudgetExpired)]
        {
            for elapsed in [10, 11] {
                let clock = MockClock::default();
                let mut port = FixedPort::new(&clock, &[transfer(elapsed, 2)]);
                let error = failure(
                    write_all_until_bounded(
                        &mut port,
                        context(&clock, source),
                        Duration::from_nanos(budget),
                        &[1, 2],
                    )
                    .unwrap_err(),
                );
                assert_eq!(error.reason, reason);
                assert_eq!(
                    (error.wanted, error.got, error.write_attempted),
                    (2, 2, true)
                );
                assert_eq!(&port.written[..port.written_len], [1, 2]);
                assert_eq!(error.observed_at, Some(MonoRawNs(elapsed)));
            }
        }
    }

    #[test]
    fn absolute_expiry_during_timeout_install_suppresses_native_write() {
        for after_partial in [false, true] {
            let clock = MockClock::default();
            let mut port = FixedPort::new(&clock, &[transfer(2, 1)]);
            let index = usize::from(after_partial);
            port.timeout_steps[index].elapsed_ns = if after_partial { 8 } else { 10 };
            let error = failure(
                write_all_until_bounded(
                    &mut port,
                    context(&clock, 10),
                    Duration::from_nanos(100),
                    &[1, 2],
                )
                .unwrap_err(),
            );
            assert_eq!(error.reason, R::SourceExpired);
            assert_eq!(error.got, index);
            assert_eq!(error.write_attempted, after_partial);
            assert_eq!(port.calls, index);
            assert_eq!(port.timeout_calls, index + 1);
        }
    }

    #[test]
    fn absolute_retryable_errors_preserve_both_absolute_windows() {
        let clock = MockClock::default();
        let mut port = FixedPort::new(
            &clock,
            &[
                io_error(3, io::ErrorKind::Interrupted),
                io_error(3, io::ErrorKind::WouldBlock),
                io_error(3, io::ErrorKind::TimedOut),
                transfer(5, 2),
            ],
        );
        write_all_until_bounded(
            &mut port,
            context(&clock, 20),
            Duration::from_nanos(30),
            &[1, 2],
        )
        .unwrap();
        assert_eq!(
            &port.timeouts[..port.timeout_calls],
            [20, 17, 14, 11].map(Duration::from_nanos)
        );
        assert_eq!(&port.request_first[..port.calls], [1, 1, 1, 1]);
        for (source, budget, reason) in
            [(10, 100, R::SourceExpired), (100, 10, R::CallBudgetExpired)]
        {
            let clock = MockClock::default();
            let mut port = FixedPort::new(
                &clock,
                &[
                    io_error(3, io::ErrorKind::Interrupted),
                    io_error(3, io::ErrorKind::WouldBlock),
                    io_error(4, io::ErrorKind::TimedOut),
                ],
            );
            let error = failure(
                write_all_until_bounded(
                    &mut port,
                    context(&clock, source),
                    Duration::from_nanos(budget),
                    &[1],
                )
                .unwrap_err(),
            );
            assert_eq!(error.reason, reason);
            assert_eq!(error.last_io_error, Some(io::ErrorKind::TimedOut));
            assert_eq!(error.got, 0);
            assert_eq!(port.calls, 3);
        }
    }

    #[test]
    fn absolute_zero_or_terminal_io_preserves_progress_and_does_not_retry() {
        for (outcome, reason) in [
            (Outcome::Transfer(0), R::WriteZero),
            (
                Outcome::Error(io::ErrorKind::BrokenPipe),
                R::WriteIo {
                    kind: io::ErrorKind::BrokenPipe,
                },
            ),
        ] {
            let clock = MockClock::default();
            let mut port = FixedPort::new(
                &clock,
                &[
                    transfer(1, 2),
                    NativeStep {
                        elapsed_ns: 1,
                        outcome,
                        clock_override: None,
                    },
                ],
            );
            let error = failure(
                write_all_until_bounded(
                    &mut port,
                    context(&clock, 100),
                    Duration::from_nanos(100),
                    &[1, 2, 3, 4],
                )
                .unwrap_err(),
            );
            assert_eq!(error.reason, reason);
            assert_eq!(error.got, 2);
            assert_eq!(port.calls, 2);
            assert_eq!(&port.written[..port.written_len], [1, 2]);
        }
    }

    #[test]
    fn absolute_timeout_install_failure_preserves_progress() {
        for index in [0, 1] {
            let clock = MockClock::default();
            let mut port = FixedPort::new(&clock, &[transfer(1, 2)]);
            port.timeout_steps[index].error = Some(io::ErrorKind::PermissionDenied);
            let error = failure(
                write_all_until_bounded(
                    &mut port,
                    context(&clock, 100),
                    Duration::from_nanos(100),
                    &[1, 2, 3, 4],
                )
                .unwrap_err(),
            );
            assert_eq!(
                error.reason,
                R::TimeoutConfiguration {
                    kind: io::ErrorKind::PermissionDenied
                }
            );
            assert_eq!(error.got, index * 2);
            assert_eq!(port.calls, index);
            assert_eq!(port.timeout_calls, index + 1);
        }
    }

    #[test]
    fn absolute_regressed_clock_and_budget_overflow_fail_closed() {
        let clock = MockClock::new(MonoRawNs(10));
        let mut port = FixedPort::new(
            &clock,
            &[NativeStep {
                clock_override: Some(MonoRawNs(9)),
                ..transfer(0, 1)
            }],
        );
        let error = failure(
            write_all_until_bounded(
                &mut port,
                context(&clock, 100),
                Duration::from_nanos(50),
                &[1, 2],
            )
            .unwrap_err(),
        );
        assert_eq!(
            error.reason,
            R::ClockRegressed {
                previous_at: MonoRawNs(10)
            }
        );
        assert_eq!(error.got, 1);
        for (start, source, budget) in [
            (0, 100, Duration::from_secs(u64::MAX)),
            (u64::MAX - 2, u64::MAX, Duration::from_nanos(5)),
        ] {
            let clock = MockClock::new(MonoRawNs(start));
            let mut port = FixedPort::new(&clock, &[]);
            let error = failure(
                write_all_until_bounded(&mut port, context(&clock, source), budget, &[1])
                    .unwrap_err(),
            );
            assert_eq!(error.reason, R::BudgetOverflow);
            assert_eq!(
                (port.calls, port.timeout_calls, error.call_deadline),
                (0, 0, None)
            );
        }
    }

    #[test]
    fn absolute_supported_empty_is_freshness_checked_without_io() {
        let clock = MockClock::default();
        let mut port = FixedPort::new(&clock, &[]);
        write_all_until_bounded(
            &mut port,
            context(&clock, 100),
            Duration::from_nanos(1),
            &[],
        )
        .unwrap();
        for (source, budget, reason) in [(0, 1, R::SourceExpired), (100, 0, R::CallBudgetExpired)] {
            let error = failure(
                write_all_until_bounded(
                    &mut port,
                    context(&clock, source),
                    Duration::from_nanos(budget),
                    &[],
                )
                .unwrap_err(),
            );
            assert_eq!(error.reason, reason);
            assert_eq!(
                (error.wanted, error.got, error.write_attempted),
                (0, 0, false)
            );
        }
        assert_eq!((port.calls, port.timeout_calls), (0, 0));
    }

    #[test]
    fn absolute_invalid_native_write_count_does_not_wrap_progress() {
        let clock = MockClock::default();
        let mut port = FixedPort::new(&clock, &[transfer(1, 2), transfer(1, usize::MAX)]);
        let error = failure(
            write_all_until_bounded(
                &mut port,
                context(&clock, 100),
                Duration::from_nanos(100),
                &[1, 2, 3, 4],
            )
            .unwrap_err(),
        );
        assert_eq!(
            error.reason,
            R::InvalidWriteCount {
                reported: usize::MAX
            }
        );
        assert_eq!(error.got, 2);
        assert_eq!(port.calls, 2);
        assert_eq!(&port.written[..port.written_len], [1, 2]);
    }

    #[test]
    fn deadline_bus_forbidden_registers_keep_prelease_priority() {
        let clock = MockClock::default();
        let (mut bus, probe, port) = bus_fixture(&clock, &[], 0);
        bus.set_op_class(BusOpClass::Safety);
        for name in [
            "Phase",
            "CW_Dead_Zone",
            "CCW_Dead_Zone",
            "Firmware_Major_Version",
            "Firmware_Minor_Version",
        ] {
            let error = bus
                .sync_write_by_id_into_until(
                    name,
                    &[(0, i32::MIN)],
                    &mut [],
                    context(&NoReadClock, 0),
                )
                .unwrap_err();
            match name {
                "Firmware_Major_Version" | "Firmware_Minor_Version" => assert!(
                    matches!(error, FeetechError::ReadOnlyRegisterWriteForbidden(value) if value == name)
                ),
                _ => assert!(
                    matches!(error, FeetechError::DiagnosticRegisterWriteForbidden(value) if value == name)
                ),
            }
        }
        probe.assert_trace(&[]);
        assert_eq!(port.lock().unwrap().calls, 0);
    }

    #[test]
    fn deadline_bus_shape_value_and_frame_errors_never_acquire_lease() {
        let clock = MockClock::default();
        let (mut bus, probe, port) = bus_fixture(&clock, &[], 0);
        let huge = [(8, 0); robot_core::adapter::MAX_JOINTS + 1];
        assert!(
            matches!(bus.sync_write_by_id_into_until("Goal_Velocity", &huge, &mut [0; 256], context(&NoReadClock, 0)), Err(FeetechError::ByteLengthMismatch { expected: robot_core::adapter::MAX_JOINTS, actual }) if actual == huge.len())
        );
        assert!(matches!(
            bus.sync_write_by_id_into_until(
                "Goal_Velocity",
                &WHEELS,
                &mut [0; 16],
                context(&NoReadClock, 0)
            ),
            Err(FeetechError::ByteLengthMismatch {
                expected: 17,
                actual: 16
            })
        ));
        assert!(matches!(
            bus.sync_write_by_id_into_until(
                "Goal_Velocity",
                &[(8, i32::MIN)],
                &mut [0; 256],
                context(&NoReadClock, 0)
            ),
            Err(FeetechError::MagnitudeOutOfRange { .. })
        ));
        assert!(matches!(
            bus.sync_write_by_id_into_until(
                "not_registered",
                &[],
                &mut [],
                context(&NoReadClock, 0)
            ),
            Err(FeetechError::UnknownRegister("not_registered"))
        ));
        probe.assert_trace(&[]);
        assert_eq!(port.lock().unwrap().calls, 0);
    }

    #[test]
    fn deadline_bus_preexpired_frame_has_exact_wanted_and_no_lease() {
        let clock = MockClock::new(MonoRawNs(10));
        let (mut bus, probe, port) = bus_fixture(&clock, &[], 0);
        let mut frame = [0; 256];
        let error = failure(
            bus.sync_write_by_id_into_until(
                "Goal_Velocity",
                &WHEELS,
                &mut frame,
                context(&clock, 10),
            )
            .unwrap_err(),
        );
        assert_eq!(error.reason, R::SourceExpired);
        assert_eq!(
            (error.wanted, error.got, error.write_attempted),
            (17, 0, false)
        );
        assert_eq!(&frame[..17], GOLDEN);
        probe.assert_trace(&[]);
        assert_eq!(port.lock().unwrap().calls, 0);
    }

    #[test]
    fn deadline_bus_lease_across_success_expiry_and_unsupported() {
        for (elapsed, succeeds) in [(1, true), (10, false)] {
            for class in [BusOpClass::NormalEffectful, BusOpClass::PeriodicWrite] {
                let clock = MockClock::default();
                let (mut bus, probe, port) = bus_fixture(&clock, &[transfer(elapsed, 17)], 0);
                bus.set_op_class(class);
                let result = bus.sync_write_by_id_into_until(
                    "Goal_Velocity",
                    &WHEELS,
                    &mut [0; 256],
                    context(&clock, 10),
                );
                assert_eq!(result.is_ok(), succeeds);
                if let Err(error) = result {
                    assert_eq!(failure(error).got, 17);
                }
                probe.assert_trace(&[
                    Trace::Begin,
                    Trace::Enter,
                    Trace::Return,
                    Trace::Checkpoint,
                    Trace::Finish,
                ]);
                let port = port.lock().unwrap();
                assert_eq!(&port.written[..port.written_len], GOLDEN);
                assert_eq!(port.legacy_calls, 0);
            }
        }
        let clock = MockClock::default();
        let probe = Arc::new(LeaseProbe::new(&clock, 0));
        let mut bus = FeetechBus::new(DefaultReject, HashMap::new()).unwrap();
        bus.set_services(Some(probe.clone()), None);
        let error = failure(
            bus.sync_write_by_id_into_until(
                "Goal_Velocity",
                &WHEELS,
                &mut [0; 256],
                context(&clock, 10),
            )
            .unwrap_err(),
        );
        assert_eq!(error.reason, R::Unsupported);
        assert_eq!((error.wanted, error.got), (17, 0));
        probe.assert_trace(&[Trace::Begin, Trace::Checkpoint, Trace::Finish]);
    }

    #[test]
    fn deadline_bus_lease_revocation_preserves_original_abort() {
        for (raw, reason) in [(1, AbortReason::Emergency), (2, AbortReason::Revoked)] {
            let clock = MockClock::default();
            let (mut bus, probe, port) = bus_fixture(&clock, &[], 0);
            probe.abort.store(raw, Ordering::Release);
            assert!(
                matches!(bus.sync_write_by_id_into_until("Goal_Velocity", &WHEELS, &mut [0; 256], context(&clock, 10)), Err(FeetechError::Aborted(value)) if value == reason)
            );
            probe.assert_trace(&[Trace::Rejected]);
            assert_eq!(port.lock().unwrap().calls, 0);
        }
    }

    #[test]
    fn deadline_bus_source_expiring_while_acquiring_lease_has_no_tx() {
        let clock = MockClock::default();
        let (mut bus, probe, port) = bus_fixture(&clock, &[], 10);
        let error = failure(
            bus.sync_write_by_id_into_until(
                "Goal_Velocity",
                &WHEELS,
                &mut [0; 256],
                context(&clock, 10),
            )
            .unwrap_err(),
        );
        assert_eq!(error.reason, R::SourceExpired);
        assert_eq!((error.got, error.write_attempted), (0, false));
        probe.assert_trace(&[
            Trace::Begin,
            Trace::Enter,
            Trace::Return,
            Trace::Checkpoint,
            Trace::Finish,
        ]);
        assert_eq!(port.lock().unwrap().calls, 0);
    }

    #[test]
    fn deadline_bus_invalid_class_refuses_without_changing_legacy_safety() {
        let clock = MockClock::default();
        let (mut bus, probe, port) = bus_fixture(&clock, &[], 0);
        for class in [
            BusOpClass::Safety,
            BusOpClass::Confirm,
            BusOpClass::Connect,
            BusOpClass::PeriodicRead,
        ] {
            bus.set_op_class(class);
            let error = failure(
                bus.sync_write_by_id_into_until(
                    "Goal_Velocity",
                    &WHEELS,
                    &mut [0; 256],
                    context(&NoReadClock, 0),
                )
                .unwrap_err(),
            );
            assert_eq!(error.reason, R::InvalidOperationClass { class });
            assert_eq!((error.wanted, error.got), (17, 0));
        }
        probe.assert_trace(&[]);
        bus.set_services(None, None);
        bus.set_op_class(BusOpClass::Safety);
        bus.sync_write_by_id_into("Torque_Enable", &[(8, 0)], &mut [0; 256])
            .unwrap();
        let port = port.lock().unwrap();
        assert_eq!(
            &port.written[..port.written_len],
            [0xff, 0xff, 0xfe, 6, 0x83, 40, 1, 8, 0, 0x47]
        );
        assert_eq!((port.calls, port.legacy_calls), (0, 1));
    }

    #[test]
    fn deadline_bus_missing_probe_refuses_before_transport() {
        let mut bus = FeetechBus::new(DefaultReject, HashMap::new()).unwrap();
        for source in [0, 100] {
            let error = failure(
                bus.sync_write_by_id_into_until(
                    "Goal_Velocity",
                    &WHEELS,
                    &mut [0; 256],
                    context(&NoReadClock, source),
                )
                .unwrap_err(),
            );
            assert_eq!(error.reason, R::NoAuthorityProbe);
            assert_eq!(
                (
                    error.wanted,
                    error.got,
                    error.write_attempted,
                    error.observed_at
                ),
                (17, 0, false, None)
            );
            assert_eq!(
                (
                    9,
                    error.reason.diagnostic_code(),
                    error.diagnostic_progress()
                ),
                (9, 11, 0)
            );
        }
    }

    #[test]
    fn absolute_legacy_write_position_response_and_fast_stop_are_unchanged() {
        let clock = MockClock::default();
        let (mut bus, probe, port) = bus_fixture(&clock, &[], 0);
        bus.set_services(None, None);
        for class in [BusOpClass::Safety, BusOpClass::Confirm] {
            bus.set_op_class(class);
            bus.sync_write_by_id_into("Goal_Position", &[(8, 1234)], &mut [0; 256])
                .unwrap();
        }
        probe.assert_trace(&[]);
        let port = port.lock().unwrap();
        assert_eq!(port.calls, 0);
        assert_eq!(port.legacy_calls, 2);
        let position = [0xff, 0xff, 0xfe, 7, 0x83, 42, 2, 8, 0xd2, 4, 0x6d];
        assert_eq!(&port.written[..11], position);
        assert_eq!(&port.written[11..22], position);
        // 原响应/fast-stop与旧短IO的完整既有方法由完整lib及integration原suite复验。
    }

    #[test]
    fn absolute_production_hot_core_and_bus_paths_run_under_real_allocator_guard() {
        const NEGATIVE: &str = "FEETECH_ABSOLUTE_ALLOC_NEGATIVE";
        if std::env::var_os(NEGATIVE).is_some() {
            assert_no_alloc::assert_no_alloc(|| {
                std::hint::black_box(Box::new(7_u64));
            });
            panic!("真实分配负控未被allocator阻止");
        }
        let clock = MockClock::default();
        let mut success = FixedPort::new(&clock, &[transfer(1, 1), transfer(1, 1)]);
        let mut partial = FixedPort::new(&clock, &[transfer(10, 1)]);
        let mut expired = FixedPort::new(&clock, &[]);
        let mut unknown = DefaultReject;
        let (mut bus, probe, _) = bus_fixture(&clock, &[transfer(1, 17)], 0);
        let mut frame = [0; 256];
        assert_no_alloc::assert_no_alloc(|| {
            write_all_until_bounded(
                &mut success,
                context(&clock, 100),
                Duration::from_nanos(100),
                &[1, 2],
            )
            .unwrap();

            let err = failure(
                write_all_until_bounded(
                    &mut partial,
                    context(&clock, 12),
                    Duration::from_nanos(100),
                    &[1, 2],
                )
                .unwrap_err(),
            );
            assert_eq!(err.got, 1);

            let err = failure(
                write_all_until_bounded(
                    &mut expired,
                    context(&clock, 12),
                    Duration::from_nanos(100),
                    &[1],
                )
                .unwrap_err(),
            );
            assert_eq!(err.got, 0);

            let err = unknown
                .write_all_until(&[1], context(&NoReadClock, 0))
                .unwrap_err();
            assert_eq!(
                (
                    9,
                    failure_ref(&err).reason.diagnostic_code(),
                    failure_ref(&err).diagnostic_progress()
                ),
                (9, 1, 0)
            );
            assert_eq!(
                RobotError::from(err),
                RobotError::InvalidParameter("绝对期限写入不受支持")
            );

            bus.sync_write_by_id_into_until(
                "Goal_Velocity",
                &WHEELS,
                &mut frame,
                context(&clock, 100),
            )
            .unwrap();
        });
        probe.assert_trace(&[
            Trace::Begin,
            Trace::Enter,
            Trace::Return,
            Trace::Checkpoint,
            Trace::Finish,
        ]);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "transport::post_e_acceptance::absolute::absolute_production_hot_core_and_bus_paths_run_under_real_allocator_guard", "--nocapture"])
            .env(NEGATIVE, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn().unwrap();
        let pid = child.id();
        let child = child.wait_with_output().unwrap();
        // 原件日志记录此自有负控的真实完成；这里只计算一个Rust方法。
        eprintln!(
            "分配负控：pid={pid}，status={}，已自然wait/reap，stdout={}B，stderr={}B：{}",
            child.status,
            child.stdout.len(),
            child.stderr.len(),
            String::from_utf8_lossy(&child.stderr)
        );
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(child.status.signal(), Some(unix_abort_signal()));
        }
        assert!(!child.status.success());
        assert!(
            String::from_utf8_lossy(&child.stderr).contains("memory allocation of 8 bytes failed")
        );
    }

    #[cfg(unix)]
    const fn unix_abort_signal() -> i32 {
        6
    }

    #[test]
    fn absolute_write_zero_at_deadline_keeps_expiry_priority() {
        for (source, budget, reason) in
            [(10, 100, R::SourceExpired), (100, 10, R::CallBudgetExpired)]
        {
            for elapsed in [10, 11] {
                let clock = MockClock::default();
                let mut port = FixedPort::new(&clock, &[transfer(elapsed, 0)]);
                let error = failure(
                    write_all_until_bounded(
                        &mut port,
                        context(&clock, source),
                        Duration::from_nanos(budget),
                        &[1],
                    )
                    .unwrap_err(),
                );
                assert_eq!(error.reason, reason);
                assert_eq!(
                    (
                        error.wanted,
                        error.got,
                        error.write_attempted,
                        error.last_io_error
                    ),
                    (1, 0, true, None)
                );
                assert_eq!(
                    (
                        error.source_deadline,
                        error.call_deadline,
                        error.previous_at,
                        error.observed_at
                    ),
                    (
                        Deadline(MonoRawNs(source)),
                        Some(Deadline(MonoRawNs(budget))),
                        Some(MonoRawNs(0)),
                        Some(MonoRawNs(elapsed))
                    )
                );
                assert_eq!(error.comm_kind(), Some(CommErrorKind::IoWrite));
                assert_eq!(port.calls, 1);
            }
        }
    }

    #[test]
    fn absolute_terminal_io_at_deadline_retains_original_io_kind() {
        for (source, budget, reason) in
            [(10, 100, R::SourceExpired), (100, 10, R::CallBudgetExpired)]
        {
            for elapsed in [10, 11] {
                let clock = MockClock::default();
                let mut port =
                    FixedPort::new(&clock, &[io_error(elapsed, io::ErrorKind::BrokenPipe)]);
                let error = failure(
                    write_all_until_bounded(
                        &mut port,
                        context(&clock, source),
                        Duration::from_nanos(budget),
                        &[1],
                    )
                    .unwrap_err(),
                );
                assert_eq!(error.reason, reason);
                assert_eq!(
                    (
                        error.wanted,
                        error.got,
                        error.write_attempted,
                        error.last_io_error
                    ),
                    (1, 0, true, Some(io::ErrorKind::BrokenPipe))
                );
                assert_eq!(
                    (
                        error.source_deadline,
                        error.call_deadline,
                        error.previous_at,
                        error.observed_at
                    ),
                    (
                        Deadline(MonoRawNs(source)),
                        Some(Deadline(MonoRawNs(budget))),
                        Some(MonoRawNs(0)),
                        Some(MonoRawNs(elapsed))
                    )
                );
                assert_eq!(error.comm_kind(), Some(CommErrorKind::IoWrite));
                assert_eq!(port.calls, 1);
            }
        }
    }

    #[test]
    fn absolute_partial_return_with_regression_preserves_all_known_progress() {
        let clock = MockClock::default();
        let mut port = FixedPort::new(
            &clock,
            &[
                transfer(10, 2),
                NativeStep {
                    clock_override: Some(MonoRawNs(9)),
                    ..transfer(0, 2)
                },
            ],
        );
        let error = failure(
            write_all_until_bounded(
                &mut port,
                context(&clock, 100),
                Duration::from_nanos(100),
                &[1, 2, 3, 4, 5],
            )
            .unwrap_err(),
        );
        assert_eq!(
            error.reason,
            R::ClockRegressed {
                previous_at: MonoRawNs(10)
            }
        );
        assert_eq!(
            (error.got, error.previous_at, error.observed_at),
            (4, Some(MonoRawNs(10)), Some(MonoRawNs(9)))
        );
        assert_eq!(&port.written[..port.written_len], [1, 2, 3, 4]);
    }

    #[test]
    fn absolute_invalid_count_precedes_expiry_and_regression() {
        for (start, elapsed, clock_override, observed) in
            [(0, 10, None, 10), (5, 0, Some(MonoRawNs(4)), 4)]
        {
            let clock = MockClock::new(MonoRawNs(start));
            let mut port = FixedPort::new(
                &clock,
                &[NativeStep {
                    clock_override,
                    ..transfer(elapsed, 2)
                }],
            );
            let error = failure(
                write_all_until_bounded(
                    &mut port,
                    context(&clock, 10),
                    Duration::from_nanos(100),
                    &[1],
                )
                .unwrap_err(),
            );
            assert_eq!(error.reason, R::InvalidWriteCount { reported: 2 });
            assert_eq!(
                (error.got, error.previous_at, error.observed_at),
                (0, Some(MonoRawNs(start)), Some(MonoRawNs(observed)))
            );
        }
    }
}
