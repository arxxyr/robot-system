//! 固定诊断验收：真实 adapter / bus / 权限租约，只有设备回应与请求超时来源由夹具提供。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use robot_core::adapter::{ControlModeRepr, JointCommandBuf, JointStateBuf, ManipulatorAdapter};
use robot_core::control_authority::{
    AuthorityInit, ControlAuthority, EnableOwner, EnableResult, InhibitSet, NonEnergizingControl,
    NormalIoIntent, Refusal,
};
use robot_core::diag::{DiagEvent, DiagSink};
use robot_core::error::RobotError;
use robot_core::executor_ctrl::OutputSafetyState;
use robot_core::serial_services::{
    AbortReason, BusLease, BusOpClass, CommErrorKind, LeasedPreemptProbe, PreemptProbe,
};
use robot_core::time::{DurationNs, MockClock, RobotClock};

use robot_adapter_feetech::adapter::{FeetechAdapter, FeetechAdapterConfig};
use robot_adapter_feetech::bus::FeetechBus;
use robot_adapter_feetech::error::FeetechError;
use robot_adapter_feetech::protocol::checksum;
use robot_adapter_feetech::transport::{DrainReport, MockHandle, MockTransport, SerialTransport};
use robot_adapter_feetech::types::Motor;

// 仅测试二进制启用，使下方 assert_no_alloc 真正检查生产 record_error 路径。
#[global_allocator]
static TEST_ALLOCATOR: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

#[derive(Default)]
struct CapturedDiagnostic {
    stamp: AtomicU64,
    fields: AtomicU64,
    count: AtomicUsize,
}

impl DiagSink for CapturedDiagnostic {
    fn record(&self, event: DiagEvent) {
        // 本夹具单线程生产并在调用返回后读取，避免测试 Mutex 首次访问分配。
        let detail = event.seq_lo.to_le_bytes();
        self.stamp.store(event.stamp_ns, Ordering::Relaxed);
        self.fields.store(
            u64::from_le_bytes([
                event.site, event.kind, event.a, event.b, detail[0], detail[1], detail[2],
                detail[3],
            ]),
            Ordering::Relaxed,
        );
        self.count.fetch_add(1, Ordering::Release);
    }
}

impl CapturedDiagnostic {
    fn take(&self, count: usize) -> DiagEvent {
        assert_eq!(self.count.swap(0, Ordering::AcqRel), count);
        let [site, kind, a, b, d0, d1, d2, d3] = self.fields.load(Ordering::Relaxed).to_le_bytes();
        DiagEvent {
            stamp_ns: self.stamp.load(Ordering::Relaxed),
            site,
            kind,
            a,
            b,
            seq_lo: u32::from_le_bytes([d0, d1, d2, d3]),
        }
    }
}

/// 不伪造许可结果：所有普通事务都向真实权限域申请并归还 `BusLease`。
/// 夹具仅选择本次调用意图和外部超时请求，不复制权限门的判断。
struct AuthorityProbe {
    authority: ControlAuthority,
    clock: Arc<MockClock>,
    context: Mutex<(BusOpClass, NormalIoIntent)>,
    timeout_requested: AtomicBool,
}

impl AuthorityProbe {
    fn context(&self, class: BusOpClass, intent: NormalIoIntent) {
        *self.context.lock().unwrap() = (class, intent);
    }
}

impl PreemptProbe for AuthorityProbe {
    fn should_abort(&self, class: BusOpClass) -> Option<AbortReason> {
        match (
            class.is_normal(),
            self.authority.snapshot().serial_estop_pending,
            class == BusOpClass::NormalEffectful && self.timeout_requested.load(Ordering::Relaxed),
        ) {
            (true, true, _) => Some(AbortReason::Emergency),
            (true, false, true) => Some(AbortReason::Timeout),
            _ => None,
        }
    }

    fn checkpoint(&self) {}

    fn sleep_or_abort(&self, class: BusOpClass, duration: DurationNs) -> Result<(), AbortReason> {
        if let Some(reason) = self.should_abort(class) {
            return Err(reason);
        }
        self.clock.advance(duration);
        Ok(())
    }

    fn operation_class(&self) -> BusOpClass {
        self.context.lock().unwrap().0
    }

    fn confirmation_trusted(&self) -> bool {
        true
    }

    fn report_comm_error(&self, _kind: CommErrorKind) {}
}

impl LeasedPreemptProbe for AuthorityProbe {
    fn begin_leased(&self, class: BusOpClass) -> Result<BusLease, AbortReason> {
        if let Some(reason) = self.should_abort(class) {
            return Err(reason);
        }
        if let Some(lease) = BusLease::special(class) {
            return Ok(lease);
        }
        let intent = self.context.lock().unwrap().1;
        let lease = self
            .authority
            .acquire_normal_io(intent)
            .map_err(|reason| match reason {
                Refusal::SerialEStopPending => AbortReason::Emergency,
                _ => AbortReason::Revoked,
            })?;
        Ok(BusLease::with_normal(class, lease).expect("测试调用类别必须与意图匹配"))
    }

    fn finish_leased(&self, lease: BusLease) {
        lease.release(&self.authority).expect("真实租约必须归还");
    }
}

#[derive(Default)]
struct TransportObservation {
    write_calls: AtomicUsize,
    failure: AtomicU8,
}

struct ObservedTransport {
    inner: MockTransport,
    observation: Arc<TransportObservation>,
}

impl SerialTransport for ObservedTransport {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), FeetechError> {
        self.observation.write_calls.fetch_add(1, Ordering::Relaxed);
        match self.observation.failure.load(Ordering::Relaxed) {
            1 => Err(FeetechError::TransportWrite(
                std::io::ErrorKind::BrokenPipe.into(),
            )),
            2 => Err(FeetechError::TransportWriteTimeout {
                timeout_ms: 50,
                wanted: bytes.len(),
                got: 0,
            }),
            // 只检验公开 safety_stop 的固定诊断链，不声称真实 transport
            // 会在 Safety 域生成这些原因，也不测周期命令整条路径的分配。
            3 => Err(FeetechError::Aborted(AbortReason::Emergency)),
            4 => Err(FeetechError::Aborted(AbortReason::Timeout)),
            5 => Err(FeetechError::Aborted(AbortReason::Revoked)),
            _ => self.inner.write_all(bytes),
        }
    }

    fn read_exact(&mut self, bytes: &mut [u8]) -> Result<(), FeetechError> {
        self.inner.read_exact(bytes)
    }

    fn set_timeout(&mut self, timeout: Duration) -> Result<(), FeetechError> {
        self.inner.set_timeout(timeout)
    }

    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }

    fn discard_input(&mut self) -> Result<(), FeetechError> {
        self.inner.discard_input()
    }

    fn drain_until_quiet(
        &mut self,
        quiet_required: Duration,
        absolute_budget: Duration,
    ) -> Result<DrainReport, FeetechError> {
        self.inner
            .drain_until_quiet(quiet_required, absolute_budget)
    }
}

struct Fixture {
    adapter: FeetechAdapter<ObservedTransport>,
    probe: Arc<AuthorityProbe>,
    diagnostic: Arc<CapturedDiagnostic>,
    transport: Arc<TransportObservation>,
    handle: MockHandle,
}

impl Fixture {
    fn new() -> Self {
        let clock = Arc::new(MockClock::default());
        let probe = Arc::new(AuthorityProbe {
            authority: ControlAuthority::new(AuthorityInit::new(
                false,
                OutputSafetyState::TorqueOffConfirmed,
                InhibitSet::EMPTY,
            ))
            .unwrap(),
            clock: clock.clone(),
            context: Mutex::new((BusOpClass::Connect, NormalIoIntent::PeriodicRead)),
            timeout_requested: AtomicBool::new(false),
        });
        let diagnostic = Arc::new(CapturedDiagnostic::default());
        let transport = Arc::new(TransportObservation::default());
        let (inner, handle) = MockTransport::new();
        let bus = FeetechBus::new(
            ObservedTransport {
                inner,
                observation: transport.clone(),
            },
            HashMap::from([("joint".to_string(), Motor::new(7).unwrap())]),
        )
        .unwrap();
        let mut adapter = FeetechAdapter::new(
            FeetechAdapterConfig {
                clock,
                preempt: Some(probe.clone()),
                diag: Some(diagnostic.clone()),
                call_deadline: Duration::from_millis(50),
                adapter_name: "诊断验收",
                joint_names: vec!["joint".to_string()],
                calibrations: Vec::new(),
            },
            bus,
        )
        .unwrap();
        push_response(&handle, &[]);
        push_response(&handle, &[0x09, 0x03]);
        adapter.connect().unwrap();
        let fixture = Self {
            adapter,
            probe,
            diagnostic,
            transport,
            handle,
        };
        fixture.clear_transport();
        fixture.motion_context();
        fixture
    }

    fn motion_context(&self) {
        self.probe
            .context(BusOpClass::PeriodicWrite, NormalIoIntent::Motion);
    }

    fn clear_transport(&self) {
        self.transport.write_calls.store(0, Ordering::Relaxed);
        let _ = self.handle.drain_tx();
    }

    fn write_motion(&mut self) -> Result<(), RobotError> {
        self.adapter.write_command(
            &JointCommandBuf {
                mode: ControlModeRepr::Position,
                joint_count: 1,
                ..JointCommandBuf::default()
            }
            .as_view(),
        )
    }

    fn assert_motion_refused(&mut self) {
        self.motion_context();
        self.clear_transport();
        assert_eq!(
            self.write_motion(),
            Err(RobotError::Aborted(AbortReason::Revoked))
        );
        assert_eq!(self.transport.write_calls.load(Ordering::Relaxed), 0);
        assert_eq!(self.handle.peek_tx(), [] as [u8; 0]);
        assert!(!self.probe.authority.snapshot().normal_io_active);
        assert_event(self.diagnostic.take(1), 5, 7, 3);
    }

    /// 完整上扭协议已执行且 finish，但故意保留提交前窗口供测试周期写。
    fn enable_until_commit(
        &mut self,
        owner: EnableOwner,
        view: robot_core::control_authority::EnableStartView,
    ) -> EnableOwner {
        let execution = self.probe.authority.start_enable(view).unwrap();
        self.assert_motion_refused();
        self.probe.context(
            BusOpClass::NormalEffectful,
            NormalIoIntent::EnableStep(execution.view()),
        );
        push_response(&self.handle, &[0xff, 0x07]);
        push_response(&self.handle, &[0xff, 0x07]);
        push_response(&self.handle, &[]);
        self.adapter.enable(true).unwrap();
        assert_eq!(self.transport.write_calls.load(Ordering::Relaxed), 4);
        assert!(!self.probe.authority.snapshot().normal_io_active);
        self.assert_motion_refused();
        self.probe
            .authority
            .finish_enable(execution, EnableResult::Succeeded)
            .unwrap();
        self.assert_motion_refused();
        owner
    }

    fn enable(&mut self) {
        let (owner, view) = self.probe.authority.reserve_enable().unwrap();
        let owner = self.enable_until_commit(owner, view);
        self.probe.authority.commit_enable(owner).unwrap();
        self.clear_transport();
        self.motion_context();
    }
}

fn push_response(handle: &MockHandle, params: &[u8]) {
    push_status(handle, 0, params);
}

fn push_status(handle: &MockHandle, status: u8, params: &[u8]) {
    let mut response = vec![
        0xff,
        0xff,
        7,
        u8::try_from(params.len() + 2).unwrap(),
        status,
    ];
    response.extend_from_slice(params);
    response.push(checksum(&response[2..]));
    handle.push_rx(&response);
}

fn assert_event(event: DiagEvent, site: u8, kind: u8, a: u8) {
    assert_eq!(
        (event.site, event.kind, event.a, event.b, event.seq_lo),
        (site, kind, a, 0, 0)
    );
}

#[test]
fn motion_requires_enable_commit_and_disable_revokes_before_transport_write() {
    let mut fixture = Fixture::new();
    // ReadyHold 即使仍尝试周期写，也不能取代 Motion 的真实权限。
    fixture.assert_motion_refused();
    let (owner, view) = fixture.probe.authority.reserve_enable().unwrap();
    fixture.assert_motion_refused();
    let owner = fixture.enable_until_commit(owner, view);
    fixture.probe.authority.commit_enable(owner).unwrap();
    fixture.write_motion().unwrap();
    assert_eq!(fixture.transport.write_calls.load(Ordering::Relaxed), 1);
    let written = fixture.handle.drain_tx();
    assert_eq!(&written[4..10], &[0x83, 42, 2, 7, 0xff, 0x07]);
    assert_eq!(fixture.diagnostic.count.load(Ordering::Relaxed), 0);
    assert!(!fixture.probe.authority.snapshot().normal_io_active);

    let _ = fixture.probe.authority.disable();
    fixture.assert_motion_refused();
    // Disable 自己仍走非上扭控制租约，不能被 Motion 拒绝误伤。
    fixture.probe.context(
        BusOpClass::NormalEffectful,
        NormalIoIntent::NonEnergizingControl(NonEnergizingControl::Disable),
    );
    push_response(&fixture.handle, &[]);
    fixture.adapter.enable(false).unwrap();
    assert_eq!(fixture.transport.write_calls.load(Ordering::Relaxed), 1);
    assert_eq!(&fixture.handle.drain_tx()[4..7], &[3, 40, 0]);
    assert!(fixture.probe.authority.snapshot().disabled);
    assert!(!fixture.probe.authority.snapshot().normal_io_active);
    assert_eq!(fixture.diagnostic.count.load(Ordering::Relaxed), 0);
}

#[test]
fn abort_reasons_keep_explicit_diagnostic_codes_and_original_errors() {
    // 固定协议数字，不从 enum 声明顺序推导编码。
    for (reason, code, site, events) in [
        (AbortReason::Emergency, 1, 5, 1),
        (AbortReason::Timeout, 2, 3, 2),
        (AbortReason::Revoked, 3, 5, 1),
    ] {
        let mut fixture = Fixture::new();
        let response = match reason {
            AbortReason::Emergency => {
                let _ = fixture.probe.authority.estop();
                fixture.write_motion()
            }
            AbortReason::Timeout => {
                fixture.probe.context(
                    BusOpClass::NormalEffectful,
                    NormalIoIntent::NonEnergizingControl(NonEnergizingControl::Disable),
                );
                fixture
                    .probe
                    .timeout_requested
                    .store(true, Ordering::Relaxed);
                fixture.adapter.enable(false)
            }
            AbortReason::Revoked => fixture.write_motion(),
        };
        assert_eq!(response, Err(RobotError::Aborted(reason)));
        assert_event(fixture.diagnostic.take(events), site, 7, code);
        assert_eq!(fixture.transport.write_calls.load(Ordering::Relaxed), 0);
        assert_eq!(fixture.handle.peek_tx(), [] as [u8; 0]);
        assert!(!fixture.probe.authority.snapshot().normal_io_active);
    }
}

#[test]
fn transport_write_errors_keep_communication_diagnostics_and_release_real_lease() {
    for failure in [1, 2] {
        let mut fixture = Fixture::new();
        fixture.enable();
        fixture.transport.failure.store(failure, Ordering::Relaxed);
        assert_eq!(
            fixture.write_motion(),
            Err(RobotError::Comm(CommErrorKind::IoWrite))
        );
        assert_eq!(fixture.transport.write_calls.load(Ordering::Relaxed), 1);
        assert_event(fixture.diagnostic.take(1), 5, 0, 3);
        assert!(!fixture.probe.authority.snapshot().normal_io_active);
    }
}

#[test]
fn timeout_device_status_and_parameter_errors_keep_distinct_diagnostics() {
    let mut fixture = Fixture::new();
    fixture
        .probe
        .context(BusOpClass::PeriodicRead, NormalIoIntent::PeriodicRead);
    assert_eq!(
        fixture.adapter.read_state(&mut JointStateBuf::default()),
        Err(RobotError::Comm(CommErrorKind::Timeout))
    );
    assert_eq!(fixture.transport.write_calls.load(Ordering::Relaxed), 1);
    assert_event(fixture.diagnostic.take(1), 4, 0, 0);
    assert!(!fixture.probe.authority.snapshot().normal_io_active);

    let mut fixture = Fixture::new();
    fixture
        .probe
        .context(BusOpClass::PeriodicRead, NormalIoIntent::PeriodicRead);
    push_status(&fixture.handle, 0x20, &[0xff, 0x07]);
    assert_eq!(
        fixture.adapter.read_state(&mut JointStateBuf::default()),
        Err(RobotError::HardwareFault("feetech motor status"))
    );
    let status = fixture.diagnostic.take(1);
    assert_eq!(
        (status.site, status.kind, status.a, status.b, status.seq_lo),
        (4, 5, 7, 0x20, 0)
    );
    assert!(!fixture.probe.authority.snapshot().normal_io_active);

    let mut fixture = Fixture::new();
    let mut command = JointCommandBuf {
        mode: ControlModeRepr::Position,
        joint_count: 1,
        ..JointCommandBuf::default()
    };
    command.positions[0] = 1_000_000.0;
    assert_eq!(
        fixture.adapter.write_command(&command.as_view()),
        Err(RobotError::InvalidParameter(
            "feetech encoded value out of range"
        ))
    );
    assert_event(fixture.diagnostic.take(1), 5, 8, 2);
    assert_eq!(fixture.transport.write_calls.load(Ordering::Relaxed), 0);
}

#[test]
fn abort_diagnostic_recording_is_allocation_free() {
    for (reason, code, injected) in [
        (AbortReason::Emergency, 1, 3),
        (AbortReason::Timeout, 2, 4),
        (AbortReason::Revoked, 3, 5),
    ] {
        let mut fixture = Fixture::new();
        fixture.transport.failure.store(injected, Ordering::Relaxed);
        assert_no_alloc::assert_no_alloc(|| {
            assert_eq!(
                fixture.adapter.safety_stop(),
                Err(RobotError::Aborted(reason))
            );
        });
        let event = fixture.diagnostic.take(1);
        assert_event(event, 7, 7, code);
        assert_eq!(event.stamp_ns, fixture.probe.clock.now().0);
    }
}
