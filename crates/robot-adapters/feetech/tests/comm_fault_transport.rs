//! 独立验证总线实际事务边界：所有 IO 必须处于同一个已获准的 lease 内。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use robot_adapter_feetech::FeetechBus;
use robot_adapter_feetech::error::FeetechError;
use robot_adapter_feetech::transport::{DrainReport, MockHandle, MockTransport, SerialTransport};
use robot_adapter_feetech::types::Motor;
use robot_core::serial_services::{
    AbortReason, BusLease, BusOpClass, CommErrorKind, LeasedPreemptProbe, PreemptProbe,
};
use robot_core::time::DurationNs;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trace {
    Begin,
    Write,
    Checkpoint,
    Read,
    End,
    Rejected,
}

struct LeaseProbe {
    authority: robot_core::control_authority::ControlAuthority,
    active: AtomicBool,
    emergency: AtomicBool,
    emergency_after_write: AtomicBool,
    trace: Mutex<Vec<Trace>>,
    auto_reply: AtomicBool,
    untrusted: AtomicBool,
    hygiene_fail: AtomicBool,
    facts: Mutex<Vec<CommErrorKind>>,
}

impl Default for LeaseProbe {
    fn default() -> Self {
        use robot_core::control_authority::{
            AuthorityInit, ControlAuthority, EnableResult, InhibitSet,
        };
        let authority = ControlAuthority::new(AuthorityInit::new(
            false,
            robot_core::executor_ctrl::OutputSafetyState::TorqueOffConfirmed,
            InhibitSet::EMPTY,
        ))
        .unwrap();
        let (owner, view) = authority.reserve_enable().unwrap();
        let execution = authority.start_enable(view).unwrap();
        authority
            .finish_enable(execution, EnableResult::Succeeded)
            .unwrap();
        authority.commit_enable(owner).unwrap();
        Self {
            authority,
            active: AtomicBool::new(false),
            emergency: AtomicBool::new(false),
            emergency_after_write: AtomicBool::new(false),
            trace: Mutex::new(Vec::new()),
            auto_reply: AtomicBool::new(false),
            untrusted: AtomicBool::new(false),
            hygiene_fail: AtomicBool::new(false),
            facts: Mutex::new(Vec::new()),
        }
    }
}

impl LeasedPreemptProbe for LeaseProbe {
    fn begin_leased(&self, class: BusOpClass) -> Result<BusLease, AbortReason> {
        if let Some(reason) = self.should_abort(class) {
            self.record(Trace::Rejected);
            return Err(reason);
        }
        let lease = if let Some(lease) = BusLease::special(class) {
            lease
        } else {
            use robot_core::control_authority::NormalIoIntent;
            let intent = match class {
                BusOpClass::PeriodicRead => NormalIoIntent::PeriodicRead,
                _ => NormalIoIntent::Motion,
            };
            let raw = self
                .authority
                .acquire_normal_io(intent)
                .map_err(|_| AbortReason::Revoked)?;
            BusLease::with_normal(class, raw).unwrap()
        };
        assert!(
            !self.active.swap(true, Ordering::AcqRel),
            "不得嵌套占用事务执行权"
        );
        self.record(Trace::Begin);
        Ok(lease)
    }
    fn finish_leased(&self, lease: BusLease) {
        lease.release(&self.authority).unwrap();
        assert!(
            self.active.swap(false, Ordering::AcqRel),
            "每个成功 acquire 恰好释放一次"
        );
        self.record(Trace::End);
    }
}

impl LeaseProbe {
    fn record(&self, item: Trace) {
        self.trace.lock().unwrap().push(item);
    }

    fn take_trace(&self) -> Vec<Trace> {
        std::mem::take(&mut *self.trace.lock().unwrap())
    }
}

impl PreemptProbe for LeaseProbe {
    fn should_abort(&self, class: BusOpClass) -> Option<AbortReason> {
        (class.is_normal() && self.emergency.load(Ordering::Acquire))
            .then_some(AbortReason::Emergency)
    }

    fn checkpoint(&self) {
        // discard/drain 也报告进度；W/R 的执行权由 transport 入口和完整序列单独断言。
        self.record(Trace::Checkpoint);
    }

    fn sleep_or_abort(&self, class: BusOpClass, _dur: DurationNs) -> Result<(), AbortReason> {
        self.should_abort(class).map_or(Ok(()), Err)
    }

    fn operation_class(&self) -> BusOpClass {
        BusOpClass::NormalEffectful
    }
    fn confirmation_trusted(&self) -> bool {
        !self.untrusted.load(Ordering::Acquire)
    }
    fn report_comm_error(&self, kind: CommErrorKind) {
        self.facts.lock().unwrap().push(kind);
    }
}

struct LeaseTransport {
    inner: MockTransport,
    probe: Arc<LeaseProbe>,
    handle: MockHandle,
}

impl LeaseTransport {
    /// 响应只在对应请求发出后出现，不能被 probe 之前的 hygiene 当作旧字节消费。
    fn answer_request(&self, bytes: &[u8]) {
        match bytes[4] {
            1 | 3 => self.handle.push_rx(&status(bytes[2], 0, &[])),
            2 => {
                let payload = match bytes[5] {
                    3 => vec![9, 3],
                    _ => vec![0; usize::from(bytes[6])],
                };
                self.handle.push_rx(&status(bytes[2], 0, &payload));
            }
            0x82 => {
                for id in &bytes[7..bytes.len() - 1] {
                    let payload = match bytes[5] {
                        56 | 42 => vec![0xff, 7],
                        _ => vec![0; usize::from(bytes[6])],
                    };
                    self.handle.push_rx(&status(*id, 0, &payload));
                }
            }
            0x83 => {}
            opcode => panic!("未声明的指令 {opcode}"),
        }
    }
}

impl SerialTransport for LeaseTransport {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), FeetechError> {
        assert!(
            self.probe.active.load(Ordering::Acquire),
            "发现未获得执行权的请求 IO"
        );
        self.probe.record(Trace::Write);
        let result = self.inner.write_all(bytes);
        if self.probe.auto_reply.load(Ordering::Acquire) {
            self.answer_request(bytes);
        }
        if self.probe.emergency_after_write.load(Ordering::Acquire) {
            self.probe.emergency.store(true, Ordering::Release);
        }
        result
    }

    fn read_exact(&mut self, bytes: &mut [u8]) -> Result<(), FeetechError> {
        assert!(
            self.probe.active.load(Ordering::Acquire),
            "请求与响应之间不得释放执行权"
        );
        self.probe.record(Trace::Read);
        self.inner.read_exact(bytes)
    }

    fn set_timeout(&mut self, timeout: Duration) -> Result<(), FeetechError> {
        self.inner.set_timeout(timeout)
    }

    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }

    fn discard_input(&mut self) -> Result<(), FeetechError> {
        if self.probe.hygiene_fail.load(Ordering::Acquire) {
            Err(FeetechError::DiscardFailed(
                std::io::ErrorKind::BrokenPipe.into(),
            ))
        } else {
            self.inner.discard_input()
        }
    }

    fn drain_until_quiet(
        &mut self,
        quiet: Duration,
        budget: Duration,
    ) -> Result<DrainReport, FeetechError> {
        self.inner.drain_until_quiet(quiet, budget)
    }
}

fn fixture() -> (FeetechBus<LeaseTransport>, MockHandle, Arc<LeaseProbe>) {
    fixture_for_motors(&[("joint", 1)])
}

fn fixture_for_motors(
    names: &[(&str, u8)],
) -> (FeetechBus<LeaseTransport>, MockHandle, Arc<LeaseProbe>) {
    let (transport, handle) = MockTransport::new();
    let probe = Arc::new(LeaseProbe::default());
    let motors: HashMap<_, _> = names
        .iter()
        .map(|(name, id)| ((*name).to_owned(), Motor::new(*id).unwrap()))
        .collect();
    let mut bus = FeetechBus::new(
        LeaseTransport {
            inner: transport,
            probe: Arc::clone(&probe),
            handle: handle.clone(),
        },
        motors,
    )
    .unwrap();
    bus.set_services(Some(probe.clone()), None);
    bus.set_op_class(BusOpClass::NormalEffectful);
    bus.connect(false).unwrap();
    (bus, handle, probe)
}

/// 按冻结 wire 格式构造独立响应，不调用生产 checksum/build helper。
fn status(id: u8, error: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![
        0xff,
        0xff,
        id,
        u8::try_from(payload.len() + 2).unwrap(),
        error,
    ];
    frame.extend_from_slice(payload);
    let checksum = !frame[2..]
        .iter()
        .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
    frame.push(checksum);
    frame
}

const REQUEST_RESPONSE: [Trace; 6] = [
    Trace::Begin,
    Trace::Write,
    Trace::Checkpoint,
    Trace::Read,
    Trace::Checkpoint,
    Trace::End,
];

#[test]
fn transaction_response_is_not_preempted_after_the_request_write() {
    let (mut bus, handle, probe) = fixture();
    handle.push_rx(&status(1, 0, &[0]));
    probe.emergency_after_write.store(true, Ordering::Release);
    assert_eq!(bus.read("Torque_Enable", "joint").unwrap(), 0);
    assert_eq!(probe.take_trace(), REQUEST_RESPONSE);
    assert!(!probe.active.load(Ordering::Acquire));
    let bytes_before = handle.peek_tx();
    assert!(matches!(
        bus.read("Torque_Enable", "joint"),
        Err(FeetechError::Aborted(AbortReason::Emergency))
    ));
    assert_eq!(probe.take_trace(), [Trace::Rejected]);
    assert_eq!(handle.peek_tx(), bytes_before);
}

#[test]
fn periodic_admission_refused_by_emergency_never_starts_io() {
    for class in [
        BusOpClass::PeriodicRead,
        BusOpClass::PeriodicWrite,
        BusOpClass::NormalEffectful,
    ] {
        let (mut bus, handle, probe) = fixture();
        bus.set_op_class(class);
        probe.emergency.store(true, Ordering::Release);
        assert!(matches!(
            bus.read("Torque_Enable", "joint"),
            Err(FeetechError::Aborted(AbortReason::Emergency))
        ));
        assert_eq!(probe.take_trace(), [Trace::Rejected]);
        assert_eq!(handle.peek_tx(), [] as [u8; 0]);
        assert!(!probe.active.load(Ordering::Acquire));
    }
}

#[test]
fn ping_uses_two_transactions_and_may_abort_only_between_them() {
    for abort in [false, true] {
        let (mut bus, handle, probe) = fixture();
        handle.push_rx(&status(1, 0, &[]));
        handle.push_rx(&status(1, 0, &[9, 3]));
        probe.emergency_after_write.store(abort, Ordering::Release);
        let result = bus.ping(1);
        let mut expected = REQUEST_RESPONSE.to_vec();
        if abort {
            assert!(matches!(
                result,
                Err(FeetechError::Aborted(AbortReason::Emergency))
            ));
            expected.push(Trace::Rejected);
        } else {
            assert_eq!(result.unwrap(), Some(777));
            expected.extend(REQUEST_RESPONSE);
        }
        assert_eq!(probe.take_trace(), expected);
        assert!(!probe.active.load(Ordering::Acquire));
    }
}

#[test]
fn wrong_reply_id_is_rejected_by_single_read_write_and_both_ping_steps() {
    for operation in 0..4 {
        let (mut bus, handle, probe) = fixture();
        let result = match operation {
            0 => {
                handle.push_rx(&status(2, 0, &[0]));
                bus.read("Torque_Enable", "joint").map(|_| ())
            }
            1 => {
                handle.push_rx(&status(2, 0, &[]));
                bus.write("Torque_Enable", "joint", 0)
            }
            2 => {
                handle.push_rx(&status(2, 0, &[]));
                bus.ping(1).map(|_| ())
            }
            _ => {
                handle.push_rx(&status(1, 0, &[]));
                handle.push_rx(&status(2, 0, &[9, 3]));
                bus.ping(1).map(|_| ())
            }
        };
        assert!(
            matches!(
                result,
                Err(FeetechError::ResponseIdMismatch {
                    expected: 1,
                    got: 2
                })
            ),
            "operation={operation} result={result:?}"
        );
        assert!(!probe.active.load(Ordering::Acquire));
        let trace = probe.take_trace();
        assert_eq!(trace.last(), Some(&Trace::End));
    }
}

#[test]
fn parse_failure_and_transport_timeout_release_exactly_one_lease() {
    for malformed in [false, true] {
        let (mut bus, handle, probe) = fixture();
        if malformed {
            let mut frame = status(1, 0, &[0]);
            *frame.last_mut().unwrap() ^= 1;
            handle.push_rx(&frame);
        }
        assert!(bus.read("Torque_Enable", "joint").is_err());
        assert_eq!(probe.take_trace(), REQUEST_RESPONSE);
        assert!(!probe.active.load(Ordering::Acquire));
    }
}

#[test]
fn invalid_arguments_are_rejected_before_acquiring_the_transaction() {
    let (mut bus, handle, probe) = fixture();
    assert!(bus.read("Missing_Register", "joint").is_err());
    assert!(bus.write("Torque_Enable", "missing_motor", 0).is_err());
    assert!(
        bus.sync_read_by_id_into("Torque_Enable", &[1], &mut [])
            .is_err()
    );
    assert!(
        bus.sync_write_by_id_into("Torque_Enable", &[(1, 0)], &mut [])
            .is_err()
    );
    assert_eq!(probe.take_trace(), [] as [Trace; 0]);
    assert_eq!(handle.peek_tx(), [] as [u8; 0]);
}

#[test]
fn named_and_by_id_bulk_reads_share_the_complete_transaction_contract() {
    for by_id in [false, true] {
        let (mut bus, handle, probe) = fixture();
        handle.push_rx(&status(1, 0, &[1]));
        if by_id {
            let mut output = [0];
            bus.sync_read_by_id_into("Torque_Enable", &[1], &mut output)
                .unwrap();
            assert_eq!(output, [1]);
        } else {
            assert_eq!(
                bus.sync_read("Torque_Enable", &["joint"]).unwrap()["joint"],
                1
            );
        }
        assert_eq!(probe.take_trace(), REQUEST_RESPONSE);
    }
}

#[test]
fn every_broadcast_variant_holds_one_lease_without_reading_an_ack() {
    for variant in 0..3 {
        let (mut bus, handle, probe) = fixture();
        match variant {
            0 => bus.sync_write("Torque_Enable", &[("joint", 0)]).unwrap(),
            1 => bus.sync_write_by_id("Torque_Enable", &[(1, 0)]).unwrap(),
            _ => bus
                .sync_write_by_id_into("Torque_Enable", &[(1, 0)], &mut [0_u8; 256])
                .unwrap(),
        }
        assert_eq!(
            probe.take_trace(),
            [Trace::Begin, Trace::Write, Trace::Checkpoint, Trace::End]
        );
        let bytes = handle.peek_tx();
        assert_eq!(&bytes[..5], &[0xff, 0xff, 0xfe, 6, 0x83]);
        assert!(!probe.active.load(Ordering::Acquire));
    }
}

#[test]
fn ping_status_error_is_not_silently_accepted_as_a_healthy_motor() {
    let (mut bus, handle, probe) = fixture();
    handle.push_rx(&status(1, 0x20, &[]));
    assert!(matches!(bus.ping(1), Err(FeetechError::StatusError(0x20))));
    assert_eq!(probe.take_trace(), REQUEST_RESPONSE);
    assert!(!probe.active.load(Ordering::Acquire));
}

fn adapter_fixture() -> (
    robot_adapter_feetech::FeetechAdapter<LeaseTransport>,
    MockHandle,
    Arc<LeaseProbe>,
) {
    use robot_adapter_feetech::adapter::{FeetechAdapter, FeetechAdapterConfig};
    let (bus, handle, probe) = fixture_for_motors(&[("first", 2), ("second", 7)]);
    let adapter = FeetechAdapter::new(
        FeetechAdapterConfig {
            adapter_name: "独立通信验收",
            joint_names: vec!["first".to_owned(), "second".to_owned()],
            calibrations: Vec::new(),
            calibration_range_tolerances_raw: Vec::new(),
            clock: Arc::new(robot_core::time::MockClock::default()),
            preempt: Some(probe.clone()),
            diag: None,
            call_deadline: Duration::from_millis(50),
        },
        bus,
    )
    .unwrap();
    (adapter, handle, probe)
}

#[test]
fn first_safety_covers_nonconsecutive_ids_before_connect_but_normal_io_is_denied() {
    use robot_core::adapter::{
        ControlModeRepr, JointCommandBuf, JointStateBuf, ManipulatorAdapter,
    };
    let (mut adapter, handle, probe) = adapter_fixture();
    assert!(adapter.read_state(&mut JointStateBuf::default()).is_err());
    assert!(adapter.enable(true).is_err());
    let command = JointCommandBuf {
        joint_count: 2,
        mode: ControlModeRepr::Position,
        ..JointCommandBuf::default()
    };
    assert!(adapter.write_command(&command.as_view()).is_err());
    assert_eq!(handle.peek_tx(), [] as [u8; 0]);
    assert_eq!(probe.take_trace(), [] as [Trace; 0]);

    adapter.safety_stop().unwrap();
    let bytes = handle.drain_tx();
    assert_eq!(
        &bytes[..11],
        &[0xff, 0xff, 0xfe, 8, 0x83, 40, 1, 2, 0, 7, 0]
    );
    assert_eq!(
        probe.take_trace(),
        [Trace::Begin, Trace::Write, Trace::Checkpoint, Trace::End]
    );
}

#[test]
fn read_state_aborted_between_position_and_velocity_preserves_the_whole_snapshot() {
    use robot_core::adapter::{JointStateBuf, ManipulatorAdapter};
    let (mut adapter, handle, probe) = adapter_fixture();
    probe.auto_reply.store(true, Ordering::Release);
    adapter.connect().unwrap();
    let _ = handle.drain_tx();
    let _ = probe.take_trace();
    probe.emergency_after_write.store(true, Ordering::Release);
    let mut out = JointStateBuf {
        positions: [3.0; 32],
        velocities: [4.0; 32],
        torques: [5.0; 32],
        stamp_ns: 991,
        joint_count: 2,
    };
    let old = out;
    assert!(adapter.read_state(&mut out).is_err());
    assert_eq!(
        out.positions.map(f32::to_bits),
        old.positions.map(f32::to_bits)
    );
    assert_eq!(
        out.velocities.map(f32::to_bits),
        old.velocities.map(f32::to_bits)
    );
    assert_eq!(out.torques.map(f32::to_bits), old.torques.map(f32::to_bits));
    assert_eq!(
        (out.stamp_ns, out.joint_count),
        (old.stamp_ns, old.joint_count)
    );
    let mut expected = REQUEST_RESPONSE.to_vec();
    expected.push(Trace::Rejected);
    assert_eq!(probe.take_trace(), expected);
    let tx = handle.peek_tx();
    assert_eq!((tx[4], tx[5]), (0x82, 56));
    assert_eq!(tx.len(), 10, "只能有一帧 Position 请求");
}

#[test]
fn failed_connect_does_not_prevent_safety_or_confirm_and_hygiene_error_survives_success() {
    use robot_core::adapter::ManipulatorAdapter;
    use robot_core::executor_ctrl::OutputSafetyProbe;
    let (mut adapter, handle, probe) = adapter_fixture();
    assert!(adapter.connect().is_err());
    let _ = handle.drain_tx();
    let _ = probe.take_trace();
    adapter.safety_stop().unwrap();
    let _ = handle.drain_tx();
    let _ = probe.take_trace();

    probe.auto_reply.store(true, Ordering::Release);
    probe.untrusted.store(true, Ordering::Release);
    probe.hygiene_fail.store(true, Ordering::Release);
    let mut mask = [true; 2];
    adapter.read_torque_enable_mask(&mut mask).unwrap();
    assert_eq!(mask, [false, false]);
    assert_eq!(*probe.facts.lock().unwrap(), [CommErrorKind::DiscardFailed]);
    let mut expected = vec![Trace::Checkpoint]; // discard 返回错误仍报告进度。
    expected.extend(REQUEST_RESPONSE);
    assert_eq!(probe.take_trace(), expected);
    assert!(!handle.peek_tx().is_empty(), "hygiene 失败后仍须 probe");
}
