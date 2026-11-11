//! 当前 full-bus 配置的分配回归；初始化、排入回包和取出抓帧均在守卫外。

use super::*;
use crate::protocol::checksum;
use crate::transport::{MockHandle, MockTransport};
use crate::types::{Motor, MotorCalibration};
use crate::velocity_profile::{
    FirmwareVersion, Sts3215RawWheelProfile, WheelObservation, WheelRole,
};
use robot_core::adapter::{ControlModeRepr, JointCommandBuf, JointSelection};
use robot_core::control_authority::{
    AuthorityInit, ControlAuthority, EnableResult, InhibitSet, NormalIoIntent,
};
use robot_core::executor_ctrl::OutputSafetyState;
use robot_core::serial_services::{AbortReason, BusLease, PreemptProbe};
use robot_core::time::MockClock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Probe {
    authority: ControlAuthority,
    clock: Arc<MockClock>,
    abort: AtomicBool,
}
impl Probe {
    fn new(clock: Arc<MockClock>) -> Self {
        let authority = ControlAuthority::new(AuthorityInit::new(
            false,
            OutputSafetyState::TorqueOffConfirmed,
            InhibitSet::EMPTY,
        ))
        .unwrap();
        let (owner, view) = authority.reserve_enable().unwrap();
        let running = authority.start_enable(view).unwrap();
        authority
            .finish_enable(running, EnableResult::Succeeded)
            .unwrap();
        authority.commit_enable(owner).unwrap();
        Self {
            authority,
            clock,
            abort: AtomicBool::new(false),
        }
    }
}
impl PreemptProbe for Probe {
    fn should_abort(&self, class: BusOpClass) -> Option<AbortReason> {
        (class.is_normal() && self.abort.load(Ordering::Relaxed)).then_some(AbortReason::Emergency)
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
        BusOpClass::NormalEffectful
    }
    fn confirmation_trusted(&self) -> bool {
        true
    }
    fn report_comm_error(&self, _: CommErrorKind) {}
}
impl LeasedPreemptProbe for Probe {
    fn begin_leased(&self, class: BusOpClass) -> Result<BusLease, AbortReason> {
        if let Some(reason) = self.should_abort(class) {
            return Err(reason);
        }
        if let Some(lease) = BusLease::special(class) {
            return Ok(lease);
        }
        let intent = if class == BusOpClass::PeriodicRead {
            NormalIoIntent::PeriodicRead
        } else {
            NormalIoIntent::Motion
        };
        let lease = self
            .authority
            .acquire_normal_io(intent)
            .map_err(|_| AbortReason::Revoked)?;
        Ok(BusLease::with_normal(class, lease).unwrap())
    }
    fn finish_leased(&self, lease: BusLease) {
        lease.release(&self.authority).unwrap();
    }
}

#[derive(Default)]
struct Diagnostic(AtomicUsize);
impl DiagSink for Diagnostic {
    fn record(&self, _: DiagEvent) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

struct Fixture {
    adapter: FeetechAdapter<MockTransport>,
    handle: MockHandle,
    probe: Arc<Probe>,
    ids: Vec<u8>,
}
impl Fixture {
    fn new(bus: u8) -> Self {
        let source = match bus {
            1 => include_str!("../../../../configs/xlerobot-bus1.toml"),
            2 => include_str!("../../../../configs/xlerobot-bus2.toml"),
            _ => panic!("只测当前两条总线"),
        };
        let config: toml::Value = toml::from_str(source).unwrap();
        let hardware = &config["hardware"][0];
        let names: Vec<String> = hardware["joints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect();
        let ids: Vec<u8> = hardware["motor_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| u8::try_from(value.as_integer().unwrap()).unwrap())
            .collect();
        assert_eq!(names.len(), if bus == 1 { 8 } else { 9 });
        let (mut transport, handle) = MockTransport::new();
        // 日志是测试观测器，不得把其 Vec 增长误判为生产分配；预留后每次取出保留原容量。
        transport.write_all(&[0; 4096]).unwrap();
        drop(handle.drain_tx());
        let motors = names
            .iter()
            .cloned()
            .zip(ids.iter().map(|&id| Motor::new(id).unwrap()))
            .collect::<HashMap<_, _>>();
        let bus_driver = FeetechBus::new(transport, motors).unwrap();
        let clock = Arc::new(MockClock::default());
        let probe = Arc::new(Probe::new(clock.clone()));
        let adapter_config = FeetechAdapterConfig {
            clock,
            preempt: Some(probe.clone()),
            diag: Some(Arc::new(Diagnostic::default())),
            call_deadline: Duration::from_millis(50),
            adapter_name: "alloc-regression",
            joint_names: names,
            calibrations: ids
                .iter()
                .map(|&id| MotorCalibration::identity(id))
                .collect(),
            calibration_range_tolerances_raw: Vec::new(),
        };
        let adapter = if bus == 2 {
            let names = &adapter_config.joint_names;
            let profiles = [WheelRole::Front, WheelRole::Left, WheelRole::Right].map(|role| {
                let name = match role {
                    WheelRole::Front => "base_front_wheel",
                    WheelRole::Left => "base_left_wheel",
                    WheelRole::Right => "base_right_wheel",
                };
                let slot = names.iter().position(|value| value == name).unwrap();
                let motor_id = ids[slot];
                Sts3215RawWheelProfile::try_bind(
                    WheelObservation {
                        motor_id,
                        model: 777,
                        mode: 1,
                        phase: 12,
                        firmware: Some(FirmwareVersion {
                            major: 3,
                            minor: 10,
                        }),
                    },
                    role,
                    motor_id,
                )
                .unwrap()
            });
            FeetechAdapter::new_bus2_mixed_raw(adapter_config, bus_driver, profiles).unwrap()
        } else {
            FeetechAdapter::new(adapter_config, bus_driver).unwrap()
        };
        Self {
            adapter,
            handle,
            probe,
            ids,
        }
    }

    fn response(&self, id: u8, value: u16, width: usize, error: u8) {
        let mut bytes = [0; 8];
        let length = 6 + width;
        bytes[..5].copy_from_slice(&[0xff, 0xff, id, u8::try_from(width + 2).unwrap(), error]);
        bytes[5..5 + width].copy_from_slice(&value.to_le_bytes()[..width]);
        bytes[length - 1] = checksum(&bytes[2..length - 1]);
        self.handle.push_rx(&bytes[..length]);
    }
    fn group(&self, ids: &[u8], value: u16, width: usize) {
        for &id in ids {
            self.response(id, value, width, 0);
        }
    }
    fn connect(&mut self) {
        let mut sorted = self.ids.clone();
        sorted.sort_unstable();
        for id in sorted {
            self.response(id, 0, 0, 0);
            self.response(id, 777, 2, 0);
        }
        if let Some(layout) = self.adapter.mixed_bus2 {
            for &id in &self.ids {
                self.response(id, u16::from(layout.wheel_ids.contains(&id)), 1, 0);
                self.response(id, 0, 1, 0);
            }
            for id in layout.wheel_ids {
                for value in [12, 3, 10] {
                    self.response(id, value, 1, 0);
                }
            }
        }
        assert_no_alloc::assert_no_alloc(|| self.adapter.connect()).unwrap();
        drop(self.handle.drain_tx());
    }
    fn queue_enable(&self) {
        let arm_ids = self.adapter.mixed_bus2.map(|layout| layout.arm_ids);
        let ids = arm_ids
            .as_ref()
            .map_or(self.ids.as_slice(), |ids| ids.as_slice());
        self.group(ids, 2047, 2);
        self.group(ids, 2047, 2);
        self.group(&self.ids, 0, 0);
    }
    fn command(&self) -> JointCommandBuf {
        let mut command = JointCommandBuf {
            mode: ControlModeRepr::Position,
            joint_count: self.adapter.joint_count,
            stamp_ns: self.probe.clock.now().0,
            deadline_ns: self.probe.clock.now().0 + 200_000_000,
            ..JointCommandBuf::default()
        };
        if let Some(layout) = self.adapter.mixed_bus2 {
            let velocity_mask = self
                .ids
                .iter()
                .enumerate()
                .filter(|(_, id)| layout.wheel_ids.contains(id))
                .fold(0, |mask, (slot, _)| mask | (1 << slot));
            let position_mask = ((1 << self.ids.len()) - 1) & !velocity_mask;
            command.selection = JointSelection::MixedPositionRawVelocity {
                position_mask,
                velocity_mask,
            };
        }
        command
    }
    fn assert_stop(&mut self) {
        assert_no_alloc::assert_no_alloc(|| self.adapter.safety_stop()).unwrap();
        let bytes = self.handle.drain_tx();
        assert_eq!(
            &bytes[..7],
            &[
                0xff,
                0xff,
                0xfe,
                u8::try_from(4 + self.ids.len() * 2).unwrap(),
                0x83,
                40,
                1
            ]
        );
        for (entry, &id) in bytes[7..bytes.len() - 1]
            .as_chunks::<2>()
            .0
            .iter()
            .zip(&self.ids)
        {
            assert_eq!(*entry, [id, 0]);
        }
        assert_eq!(bytes[bytes.len() - 1], checksum(&bytes[2..bytes.len() - 1]));
        self.group(&self.ids, 0, 1);
        let mut enabled = [true; robot_core::adapter::MAX_JOINTS];
        assert_no_alloc::assert_no_alloc(|| self.adapter.read_torque_enable_mask(&mut enabled))
            .unwrap();
        assert!(enabled[..self.ids.len()].iter().all(|value| !value));
        drop(self.handle.drain_tx());
    }
}

#[test]
fn both_current_full_buses_complete_serial_lifecycle_without_allocation() {
    for bus in [1, 2] {
        let mut f = Fixture::new(bus);
        // 首次 stop 不依赖 adapter.connect 已完成。
        f.assert_stop();
        f.connect();
        f.queue_enable();
        assert_no_alloc::assert_no_alloc(|| f.adapter.enable(true)).unwrap();
        drop(f.handle.drain_tx());
        let command = f.command();
        assert_no_alloc::assert_no_alloc(|| f.adapter.write_command(&command.as_view())).unwrap();
        let write = f.handle.drain_tx();
        assert_eq!((write[4], write[5]), (0x83, if bus == 1 { 42 } else { 46 }));
        f.group(&f.ids, 2047, 2);
        f.group(&f.ids, 0, 2);
        if let Some(layout) = f.adapter.mixed_bus2 {
            for width in [1, 2, 2] {
                f.group(&layout.wheel_ids, 0, width);
            }
        }
        let mut state = JointStateBuf::default();
        assert_no_alloc::assert_no_alloc(|| f.adapter.read_state(&mut state)).unwrap();
        assert_eq!(usize::from(state.joint_count), f.ids.len());
        drop(f.handle.drain_tx());
        f.assert_stop();
        f.group(&f.ids, 0, 0);
        assert_no_alloc::assert_no_alloc(|| f.adapter.enable(false)).unwrap();
        assert_no_alloc::assert_no_alloc(|| f.adapter.disconnect()).unwrap();
    }
}

#[test]
fn full_bus_timeout_preemption_and_device_errors_do_not_allocate() {
    for bus in [1, 2] {
        let mut f = Fixture::new(bus);
        f.connect();
        let mut state = JointStateBuf::default();
        assert!(matches!(
            assert_no_alloc::assert_no_alloc(|| f.adapter.read_state(&mut state)),
            Err(RobotError::Comm(CommErrorKind::Timeout))
        ));
        drop(f.handle.drain_tx());
        for (index, &id) in f.ids.iter().enumerate() {
            f.response(id, 2047, 2, if index == 0 { 0x20 } else { 0 });
        }
        assert!(matches!(
            assert_no_alloc::assert_no_alloc(|| f.adapter.read_state(&mut state)),
            Err(RobotError::HardwareFault(_))
        ));
        drop(f.handle.drain_tx());
        f.probe.abort.store(true, Ordering::Relaxed);
        assert_eq!(
            assert_no_alloc::assert_no_alloc(|| f.adapter.read_state(&mut state)),
            Err(RobotError::Aborted(AbortReason::Emergency))
        );
        f.assert_stop();
    }
}

#[test]
fn full_bus_hold_and_bounded_retries_do_not_allocate() {
    let mut f = Fixture::new(1);
    f.connect();
    f.group(&f.ids, 2047, 2);
    f.group(&f.ids, 1000, 2);
    f.group(&f.ids, 2047, 2);
    assert_no_alloc::assert_no_alloc(|| f.adapter.hold_current_position()).unwrap();
    drop(f.handle.drain_tx());

    f.group(&f.ids, 2047, 2);
    for _ in 0..SYNC_GOAL_MAX_ATTEMPTS {
        f.group(&f.ids, 1000, 2);
    }
    let failure = assert_no_alloc::assert_no_alloc(|| f.adapter.sync_goal_to_present());
    assert!(matches!(
        failure,
        Err(FeetechError::SyncGoalReadbackMismatch {
            attempts: SYNC_GOAL_MAX_ATTEMPTS,
            ..
        })
    ));
    drop(f.handle.drain_tx());

    // 仅设备 NACK 沿原最多三次重试；两个 full-bus 的 disable 路径相同。
    for bus in [1, 2] {
        let mut f = Fixture::new(bus);
        f.connect();
        for &id in &f.ids {
            for _ in 0..TORQUE_ENABLE_MAX_ATTEMPTS {
                f.response(id, 0, 0, 0x20);
            }
        }
        let failure = assert_no_alloc::assert_no_alloc(|| f.adapter.enable(false));
        assert!(matches!(failure, Err(RobotError::HardwareFault(_))));
        drop(f.handle.drain_tx());
        f.assert_stop();
    }
}

#[test]
fn allocator_guard_catches_real_heap_allocation() {
    const CHILD: &str = "FEETECH_ALLOC_GUARD_NEGATIVE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        #[cfg(unix)]
        {
            let limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY：仅禁止本测试子进程生成 core dump。
            assert_eq!(
                unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const limit) },
                0
            );
        }
        assert_no_alloc::assert_no_alloc(|| {
            let layout = std::alloc::Layout::new::<u64>();
            // SAFETY：仅用于证明真实 allocator 守卫会中止；若守卫失效则正常归还。
            unsafe {
                let value = std::alloc::alloc(layout);
                std::hint::black_box(value);
                std::alloc::dealloc(value, layout);
            }
        });
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "adapter::alloc_tests::allocator_guard_catches_real_heap_allocation",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGABRT),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(!output.status.success());
}
