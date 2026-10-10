//! 固定数组直接守生产原始读接口；所有资源在 allocation guard 外构造。

use super::*;
use crate::protocol::checksum;
use crate::registers::Register;
use crate::transport::DrainReport;
use robot_core::control_authority::{AuthorityInit, ControlAuthority, InhibitSet, NormalIoIntent};
use robot_core::executor_ctrl::OutputSafetyState;
use robot_core::serial_services::{AbortReason, CommErrorKind, PreemptProbe};
use robot_core::time::DurationNs;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const IDS: [u8; 3] = [8, 7, 9];
const UNCHANGED: u32 = 0xdead_beef;

struct ReadLeaseProbe {
    authority: ControlAuthority,
    abort: AtomicBool,
    begins: AtomicUsize,
    checkpoints: AtomicUsize,
    finishes: AtomicUsize,
}

impl ReadLeaseProbe {
    fn new() -> Self {
        Self {
            authority: ControlAuthority::new(AuthorityInit::new(
                false,
                OutputSafetyState::TorqueOffConfirmed,
                InhibitSet::EMPTY,
            ))
            .unwrap(),
            abort: AtomicBool::new(false),
            begins: AtomicUsize::new(0),
            checkpoints: AtomicUsize::new(0),
            finishes: AtomicUsize::new(0),
        }
    }

    fn assert_counts(&self, begins: usize, checkpoints: usize, finishes: usize) {
        assert_eq!(self.begins.load(Ordering::Relaxed), begins);
        assert_eq!(self.checkpoints.load(Ordering::Relaxed), checkpoints);
        assert_eq!(self.finishes.load(Ordering::Relaxed), finishes);
        assert!(!self.authority.snapshot().normal_io_active);
    }
}

impl PreemptProbe for ReadLeaseProbe {
    fn should_abort(&self, _class: BusOpClass) -> Option<AbortReason> {
        self.abort
            .load(Ordering::Relaxed)
            .then_some(AbortReason::Revoked)
    }

    fn checkpoint(&self) {
        assert!(self.authority.snapshot().normal_io_active);
        self.checkpoints.fetch_add(1, Ordering::Relaxed);
    }

    fn sleep_or_abort(&self, class: BusOpClass, _duration: DurationNs) -> Result<(), AbortReason> {
        self.should_abort(class).map_or(Ok(()), Err)
    }

    fn operation_class(&self) -> BusOpClass {
        BusOpClass::PeriodicRead
    }

    fn confirmation_trusted(&self) -> bool {
        true
    }

    fn report_comm_error(&self, _kind: CommErrorKind) {
        panic!("夹具不代替运行时故障路由")
    }
}

impl LeasedPreemptProbe for ReadLeaseProbe {
    fn begin_leased(&self, class: BusOpClass) -> Result<BusLease, AbortReason> {
        self.begins.fetch_add(1, Ordering::Relaxed);
        if let Some(reason) = self.should_abort(class) {
            return Err(reason);
        }
        let lease = self
            .authority
            .acquire_normal_io(NormalIoIntent::PeriodicRead)
            .map_err(|_| AbortReason::Revoked)?;
        Ok(BusLease::with_normal(class, lease).unwrap())
    }

    fn finish_leased(&self, lease: BusLease) {
        assert!(self.authority.snapshot().normal_io_active);
        lease.release(&self.authority).unwrap();
        self.finishes.fetch_add(1, Ordering::Relaxed);
    }
}

struct FixedTransport {
    probe: Arc<ReadLeaseProbe>,
    rx: [u8; 320],
    rx_len: usize,
    tx: [u8; 40],
    tx_len: usize,
    writes: usize,
    reads: usize,
    write_error: Option<io::ErrorKind>,
    read_error: Option<io::ErrorKind>,
    read_abort: bool,
}

impl SerialTransport for FixedTransport {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), FeetechError> {
        assert!(self.probe.authority.snapshot().normal_io_active);
        self.writes += 1;
        if let Some(kind) = self.write_error {
            return Err(FeetechError::TransportWrite(io::Error::from(kind)));
        }
        self.tx[..bytes.len()].copy_from_slice(bytes);
        self.tx_len = bytes.len();
        Ok(())
    }

    fn read_exact(&mut self, bytes: &mut [u8]) -> Result<(), FeetechError> {
        assert!(self.probe.authority.snapshot().normal_io_active);
        self.reads += 1;
        if self.read_abort {
            return Err(FeetechError::Aborted(AbortReason::Revoked));
        }
        if let Some(kind) = self.read_error {
            return Err(FeetechError::TransportRead(io::Error::from(kind)));
        }
        let got = bytes.len().min(self.rx_len);
        bytes[..got].copy_from_slice(&self.rx[..got]);
        if got < bytes.len() {
            return Err(FeetechError::TransportTimeout {
                timeout_ms: 50,
                wanted: bytes.len(),
                got,
            });
        }
        Ok(())
    }

    fn set_timeout(&mut self, _timeout: Duration) -> Result<(), FeetechError> {
        Ok(())
    }

    fn is_connected(&self) -> bool {
        true
    }

    fn discard_input(&mut self) -> Result<(), FeetechError> {
        panic!("原始读不能添加清缓冲或重发")
    }

    fn drain_until_quiet(
        &mut self,
        _quiet_required: Duration,
        _absolute_budget: Duration,
    ) -> Result<DrainReport, FeetechError> {
        panic!("原始读不改变原 transport 调用预算")
    }
}

fn fixture(
    ids: &[u8],
    width: usize,
    values: &[u32],
) -> (FeetechBus<FixedTransport>, Arc<ReadLeaseProbe>) {
    let probe = Arc::new(ReadLeaseProbe::new());
    let mut transport = FixedTransport {
        probe: Arc::clone(&probe),
        rx: [0; 320],
        rx_len: 0,
        tx: [0; 40],
        tx_len: 0,
        writes: 0,
        reads: 0,
        write_error: None,
        read_error: None,
        read_abort: false,
    };
    for (&id, &value) in ids.iter().zip(values) {
        let start = transport.rx_len;
        let end = start + 6 + width;
        let packet = &mut transport.rx[start..end];
        packet[..5].copy_from_slice(&[0xff, 0xff, id, u8::try_from(width + 2).unwrap(), 0]);
        packet[5..5 + width].copy_from_slice(&value.to_le_bytes()[..width]);
        packet[end - start - 1] = checksum(&packet[2..end - start - 1]);
        transport.rx_len = end;
    }
    let mut bus = FeetechBus::new(transport, HashMap::new()).unwrap();
    let service: Arc<dyn LeasedPreemptProbe> = probe.clone();
    bus.set_services(Some(service), None);
    bus.set_op_class(BusOpClass::PeriodicRead);
    (bus, probe)
}

fn assert_no_io(bus: &FeetechBus<FixedTransport>, probe: &ReadLeaseProbe, out: &[u32]) {
    assert_eq!(bus.transport.writes, 0);
    assert_eq!(bus.transport.reads, 0);
    assert!(out.iter().all(|&value| value == UNCHANGED));
    probe.assert_counts(0, 0, 0);
}

#[test]
fn two_byte_raw_words_preserve_high_bits_and_exact_request() {
    let values = [0x8000, 0x8001, 0xffff];
    for (name, address, checksum) in [("Present_Velocity", 58, 0x24), ("Present_Load", 60, 0x22)] {
        let (mut bus, probe) = fixture(&IDS, 2, &values);
        let mut out = [UNCHANGED; 5];
        let mut tx = [0xa5; 16];
        let mut rx = [0x5a; 32];
        let result = assert_no_alloc::assert_no_alloc(|| {
            bus.sync_read_by_id_raw_into(name, &IDS, &mut out, &mut tx, &mut rx)
        });
        result.unwrap();
        assert_eq!(out, [0x8000, 0x8001, 0xffff, UNCHANGED, UNCHANGED]);
        assert_eq!(
            &bus.transport.tx[..11],
            &[0xff, 0xff, 0xfe, 7, 0x82, address, 2, 8, 7, 9, checksum]
        );
        assert_eq!(&tx[11..], &[0xa5; 5]);
        assert_eq!(&rx[24..], &[0x5a; 8]);
        assert_eq!((bus.transport.writes, bus.transport.reads), (1, 1));
        probe.assert_counts(1, 2, 1);
    }
}

#[test]
fn one_byte_raw_values_have_independent_golden_request() {
    let values = [0xff, 0x80, 2];
    for (name, address, checksum) in [
        ("Phase", 18, 0x4d),
        ("Minimum_Starting_Torque", 24, 0x47),
        ("Integral_Limit", 25, 0x46),
        ("CW_Dead_Zone", 26, 0x45),
        ("CCW_Dead_Zone", 27, 0x44),
        ("Angle_Resolution", 30, 0x41),
    ] {
        let (mut bus, probe) = fixture(&IDS, 1, &values);
        let mut out = [UNCHANGED; 4];
        assert_no_alloc::assert_no_alloc(|| {
            bus.sync_read_by_id_raw_into(name, &IDS, &mut out, &mut [0; 11], &mut [0; 21])
        })
        .unwrap();
        assert_eq!(&out[..3], &values);
        assert_eq!(out[3], UNCHANGED);
        assert_eq!(
            &bus.transport.tx[..11],
            &[0xff, 0xff, 0xfe, 7, 0x82, address, 1, 8, 7, 9, checksum]
        );
        probe.assert_counts(1, 2, 1);
    }
}

#[test]
fn four_byte_shared_production_core_preserves_unsigned_high_bits() {
    // 当前具名表没有4字节项；直接守公共入口复用的私有执行核心，不新增寄存器。
    let values = [0x8000_0000, 0x89ab_cdef, u32::MAX];
    let (mut bus, probe) = fixture(&IDS, 4, &values);
    let mut out = [UNCHANGED; 4];
    assert_no_alloc::assert_no_alloc(|| {
        bus.sync_read_raw_register_into(
            Register {
                addr: 48,
                length: 4,
            },
            &IDS,
            &mut out,
            &mut [0; 11],
            &mut [0; 30],
        )
    })
    .unwrap();
    assert_eq!(&out[..3], &values);
    assert_eq!(out[3], UNCHANGED);
    assert_eq!(
        &bus.transport.tx[..11],
        &[0xff, 0xff, 0xfe, 7, 0x82, 48, 4, 8, 7, 9, 0x2c]
    );
    probe.assert_counts(1, 2, 1);
}

#[test]
fn empty_and_excessive_id_counts_reject_before_lease_or_io() {
    for ids in [&[][..], &[1; 33][..]] {
        let (mut bus, probe) = fixture(&[], 2, &[]);
        let mut out = [UNCHANGED; 33];
        let error = assert_no_alloc::assert_no_alloc(|| {
            bus.sync_read_by_id_raw_into(
                "Present_Velocity",
                ids,
                &mut out,
                &mut [0; 40],
                &mut [0; 320],
            )
        })
        .unwrap_err();
        assert!(matches!(error, FeetechError::ByteLengthMismatch { .. }));
        assert_no_io(&bus, &probe, &out);
    }
}

#[test]
fn invalid_and_duplicate_ids_reject_before_lease_or_io() {
    for (ids, invalid) in [
        ([8, 0, 9], Some(0)),
        ([8, 254, 9], Some(254)),
        ([8, 255, 9], Some(255)),
        ([8, 7, 8], None),
    ] {
        let (mut bus, probe) = fixture(&[], 2, &[]);
        let mut out = [UNCHANGED; 3];
        let error = assert_no_alloc::assert_no_alloc(|| {
            bus.sync_read_by_id_raw_into(
                "Present_Velocity",
                &ids,
                &mut out,
                &mut [0; 11],
                &mut [0; 24],
            )
        })
        .unwrap_err();
        match invalid {
            Some(id) => assert!(matches!(error, FeetechError::InvalidMotorId(got) if got == id)),
            None => assert!(matches!(error, FeetechError::DuplicateMotorId(8))),
        }
        assert_no_io(&bus, &probe, &out);
    }
}

#[test]
fn all_caller_capacities_reject_before_lease_or_io() {
    for (out_len, tx_len, rx_len, expected, actual) in
        [(2, 11, 24, 3, 2), (3, 10, 24, 11, 10), (3, 11, 23, 24, 23)]
    {
        let (mut bus, probe) = fixture(&IDS, 2, &[1, 2, 3]);
        let mut out = [UNCHANGED; 3];
        let mut tx = [0xa5; 11];
        let mut rx = [0x5a; 24];
        let error = assert_no_alloc::assert_no_alloc(|| {
            bus.sync_read_by_id_raw_into(
                "Present_Velocity",
                &IDS,
                &mut out[..out_len],
                &mut tx[..tx_len],
                &mut rx[..rx_len],
            )
        })
        .unwrap_err();
        assert!(
            matches!(error, FeetechError::ByteLengthMismatch { expected: e, actual: a } if (e,a) == (expected,actual))
        );
        assert_eq!(tx, [0xa5; 11]);
        assert_eq!(rx, [0x5a; 24]);
        assert_no_io(&bus, &probe, &out);
    }
}

#[test]
fn unknown_register_and_unsupported_widths_reject_before_io() {
    let (mut bus, probe) = fixture(&IDS, 2, &[1, 2, 3]);
    let mut out = [UNCHANGED; 3];
    assert!(matches!(
        assert_no_alloc::assert_no_alloc(|| bus.sync_read_by_id_raw_into(
            "Unknown_Register",
            &IDS,
            &mut out,
            &mut [0; 40],
            &mut [0; 320]
        )),
        Err(FeetechError::UnknownRegister("Unknown_Register"))
    ));
    for width in [0, 3, 5, 255] {
        assert!(
            matches!(assert_no_alloc::assert_no_alloc(|| bus.sync_read_raw_register_into(Register { addr: 58, length: width }, &IDS, &mut out, &mut [0; 40], &mut [0; 320])), Err(FeetechError::UnsupportedByteLength(got)) if got == usize::from(width))
        );
    }
    assert_no_io(&bus, &probe, &out);
}

#[test]
fn maximum_count_four_byte_capacity_is_exact_without_truncation() {
    let ids: [u8; 32] = std::array::from_fn(|index| u8::try_from(index + 1).unwrap());
    let values: [u32; 32] = std::array::from_fn(|index| u32::MAX - u32::try_from(index).unwrap());
    let (mut bus, probe) = fixture(&ids, 4, &values);
    let mut out = [UNCHANGED; 33];
    assert_no_alloc::assert_no_alloc(|| {
        bus.sync_read_raw_register_into(
            Register {
                addr: 48,
                length: 4,
            },
            &ids,
            &mut out,
            &mut [0; 40],
            &mut [0; 320],
        )
    })
    .unwrap();
    assert_eq!(&out[..32], &values);
    assert_eq!(out[32], UNCHANGED);
    assert_eq!(bus.transport.tx_len, 40);
    probe.assert_counts(1, 2, 1);
}

#[test]
fn later_id_and_status_errors_never_commit_any_output() {
    for index in [1, 2] {
        for status_error in [false, true] {
            let (mut bus, probe) = fixture(&IDS, 2, &[1, 2, 3]);
            let packet = &mut bus.transport.rx[index * 8..index * 8 + 8];
            if status_error {
                packet[4] = 0x20;
            } else {
                packet[2] = 42;
            }
            packet[7] = checksum(&packet[2..7]);
            let mut out = [UNCHANGED; 4];
            let error = assert_no_alloc::assert_no_alloc(|| {
                bus.sync_read_by_id_raw_into(
                    "Present_Velocity",
                    &IDS,
                    &mut out,
                    &mut [0; 11],
                    &mut [0; 24],
                )
            })
            .unwrap_err();
            if status_error {
                assert!(matches!(error, FeetechError::StatusError(0x20)));
                assert_eq!(bus.device_status_motor(), IDS[index]);
            } else {
                assert!(
                    matches!(error, FeetechError::ResponseIdMismatch { expected, got: 42 } if expected == IDS[index])
                );
            }
            assert_eq!(out, [UNCHANGED; 4]);
            probe.assert_counts(1, 2, 1);
        }
    }
}

#[test]
fn later_header_checksum_and_length_errors_never_commit_output() {
    for index in [1, 2] {
        for fault in 0..4 {
            let (mut bus, probe) = fixture(&IDS, 2, &[1, 2, 3]);
            let packet = &mut bus.transport.rx[index * 8..index * 8 + 8];
            match fault {
                0 => packet[0] = 0,
                1 => packet[7] ^= 1,
                2 => packet[3] = 5,
                _ => {
                    packet[3] = 3;
                    packet[6] = checksum(&packet[2..6]);
                }
            }
            let mut out = [UNCHANGED; 4];
            let error = assert_no_alloc::assert_no_alloc(|| {
                bus.sync_read_by_id_raw_into(
                    "Present_Velocity",
                    &IDS,
                    &mut out,
                    &mut [0; 11],
                    &mut [0; 24],
                )
            })
            .unwrap_err();
            match fault {
                0 => assert!(matches!(error, FeetechError::BadPacketHeader { .. })),
                1 => assert!(matches!(error, FeetechError::BadChecksum { .. })),
                2 => assert!(matches!(error, FeetechError::PacketTooShort { .. })),
                _ => assert!(matches!(
                    error,
                    FeetechError::ByteLengthMismatch {
                        expected: 2,
                        actual: 1
                    }
                )),
            }
            assert_eq!(out, [UNCHANGED; 4]);
            probe.assert_counts(1, 2, 1);
        }
    }
}

#[test]
fn rejected_lease_has_no_io_and_no_false_release() {
    let (mut bus, probe) = fixture(&IDS, 2, &[1, 2, 3]);
    probe.abort.store(true, Ordering::Relaxed);
    let mut out = [UNCHANGED; 4];
    assert!(matches!(
        assert_no_alloc::assert_no_alloc(|| bus.sync_read_by_id_raw_into(
            "Present_Velocity",
            &IDS,
            &mut out,
            &mut [0; 11],
            &mut [0; 24]
        )),
        Err(FeetechError::Aborted(AbortReason::Revoked))
    ));
    assert_eq!(out, [UNCHANGED; 4]);
    assert_eq!((bus.transport.writes, bus.transport.reads), (0, 0));
    probe.assert_counts(1, 0, 0);
}

#[test]
fn write_read_and_inflight_abort_failures_release_once_without_replay() {
    for fault in 0..3 {
        let (mut bus, probe) = fixture(&IDS, 2, &[1, 2, 3]);
        match fault {
            0 => bus.transport.write_error = Some(io::ErrorKind::BrokenPipe),
            1 => bus.transport.read_error = Some(io::ErrorKind::BrokenPipe),
            _ => bus.transport.read_abort = true,
        }
        let mut out = [UNCHANGED; 4];
        let error = assert_no_alloc::assert_no_alloc(|| {
            bus.sync_read_by_id_raw_into(
                "Present_Velocity",
                &IDS,
                &mut out,
                &mut [0; 11],
                &mut [0; 24],
            )
        })
        .unwrap_err();
        match fault {
            0 => assert!(matches!(error, FeetechError::TransportWrite(_))),
            1 => assert!(matches!(error, FeetechError::TransportRead(_))),
            _ => assert!(matches!(error, FeetechError::Aborted(AbortReason::Revoked))),
        }
        assert_eq!(out, [UNCHANGED; 4]);
        assert_eq!(bus.transport.writes, 1);
        let reads = usize::from(fault != 0);
        assert_eq!(bus.transport.reads, reads);
        probe.assert_counts(1, 1 + reads, 1);
    }
}

#[test]
fn short_second_or_third_response_preserves_complete_output() {
    for got in [15, 23] {
        let (mut bus, probe) = fixture(&IDS, 2, &[1, 2, 3]);
        bus.transport.rx_len = got;
        let mut out = [UNCHANGED; 4];
        assert!(
            matches!(assert_no_alloc::assert_no_alloc(|| bus.sync_read_by_id_raw_into("Present_Velocity", &IDS, &mut out, &mut [0; 11], &mut [0; 24])), Err(FeetechError::TransportTimeout { timeout_ms: 50, wanted: 24, got: count }) if count == got)
        );
        assert_eq!(out, [UNCHANGED; 4]);
        probe.assert_counts(1, 2, 1);
    }
}
