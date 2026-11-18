//! 固定右总线维护动作；不接默认daemon，不修改模式、PID或EEPROM。

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use robot_adapter_feetech::adapter::FeetechAdapterConfig;
use robot_adapter_feetech::encoding::{decode_sign_magnitude, encode_sign_magnitude};
use robot_adapter_feetech::protocol::parse_status;
use robot_adapter_feetech::transport::{DrainReport, SerialTransport, SourceWriteDeadline};
use robot_adapter_feetech::velocity_profile::{
    FirmwareVersion, Sts3215RawWheelProfile, WheelObservation, WheelRole,
};
use robot_adapter_feetech::{
    DEFAULT_BAUDRATE, FeetechAdapter, FeetechBus, FeetechError, Motor, MotorCalibration, raw_to_rad,
};
use robot_core::adapter::{
    ControlModeRepr, JointCommandBuf, JointSelection, JointStateBuf, ManipulatorAdapter,
};
use robot_core::control_authority::{
    AuthorityInit, ControlAuthority, EnableExecutionView, EnableResult, InhibitSet, NormalIoIntent,
    SafetyOperation,
};
use robot_core::diag::{DiagEvent, DiagSink};
use robot_core::error::RobotError;
use robot_core::executor_ctrl::{OutputSafetyProbe, OutputSafetyState};
use robot_core::serial_services::{
    AbortReason, BusLease, BusOpClass, CommErrorKind, LeasedPreemptProbe, PreemptProbe,
};
use robot_core::time::{DurationNs, RobotClock, SystemClock};
use robot_kinematics_xlerobot::base::BaseDirection;
use serde_json::{Value, json};

const DEVICE: &str = "/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C122340-if00";
const NAMES: [&str; 9] = [
    "right_arm_shoulder_pan",
    "right_arm_shoulder_lift",
    "right_arm_elbow_flex",
    "right_arm_wrist_flex",
    "right_arm_wrist_roll",
    "right_arm_gripper",
    "base_front_wheel",
    "base_left_wheel",
    "base_right_wheel",
];
const PERIOD: u64 = 50_000_000;
const PULSE: u64 = 20_000_000_000;
const GROUND_PULSE: u64 = 1_000_000_000;
const ENABLE_MARGIN: u64 = 1_000_000_000;
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn interrupted(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

struct Signals {
    interrupt: libc::sighandler_t,
    terminate: libc::sighandler_t,
}
impl Signals {
    fn install() -> Result<Self, String> {
        // handler只写原子标志，不执行串口、分配或格式化。
        let interrupt =
            unsafe { libc::signal(libc::SIGINT, interrupted as *const () as libc::sighandler_t) };
        if interrupt == libc::SIG_ERR {
            return Err("SIGINT安装失败".into());
        }
        let terminate = unsafe {
            libc::signal(
                libc::SIGTERM,
                interrupted as *const () as libc::sighandler_t,
            )
        };
        if terminate == libc::SIG_ERR {
            unsafe {
                libc::signal(libc::SIGINT, interrupt);
            }
            return Err("SIGTERM安装失败".into());
        }
        Ok(Self {
            interrupt,
            terminate,
        })
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        unsafe {
            libc::signal(libc::SIGINT, self.interrupt);
            libc::signal(libc::SIGTERM, self.terminate);
        }
    }
}

#[derive(Clone, Copy)]
enum Motion {
    Single(u8),
    Direction(BaseDirection),
    GroundForward,
}

impl From<u8> for Motion {
    fn from(id: u8) -> Self {
        Self::Single(id)
    }
}

impl Motion {
    const fn pulse_ns(self) -> u64 {
        match self {
            Self::GroundForward => GROUND_PULSE,
            Self::Single(_) | Self::Direction(_) => PULSE,
        }
    }

    const fn enable_budget_ns(self) -> u64 {
        self.pulse_ns() + ENABLE_MARGIN
    }

    fn goals(self, wheel_ids: [u8; 3]) -> [i16; 3] {
        match self {
            Self::Single(id) => wheel_ids.map(|wheel| if wheel == id { 500 } else { 0 }),
            Self::Direction(direction) => direction.wheel_raw(),
            Self::GroundForward => BaseDirection::Forward.wheel_raw(),
        }
    }
}

fn velocity_matches_motion(word: u32, target: i16) -> bool {
    let speed = decode_sign_magnitude(word, 15);
    word == 0 || (speed.signum() == i32::from(target.signum()) && (1..=1000).contains(&speed.abs()))
}

/// Unix GO只在入口映射一次；所有内部期限使用同一RobotClock，不把父MONOTONIC当RAW。
struct Guard {
    ids: [u8; 9],
    motion: Motion,
    goals: [i16; 3],
    clock: Arc<dyn RobotClock>,
    authority: ControlAuthority,
    expires_unix: u64,
    hard_deadline: u64,
    deadline: AtomicU64,
    last: AtomicU64,
    last_unix: AtomicU64,
    failed: AtomicBool,
    cleanup: AtomicBool,
    class: AtomicU8,
    enable_view: Mutex<Option<EnableExecutionView>>,
    enable_attempt: AtomicU64,
    nonzero_attempt: AtomicU64,
    calibration_failure: AtomicU64,
    pv_words: Mutex<Option<[u16; 9]>>,
}
impl Guard {
    fn unix_now() -> Result<u64, String> {
        u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| "Unix时钟早于epoch")?
                .as_nanos(),
        )
        .map_err(|_| "Unix时钟溢出".into())
    }
    fn new(
        clock: Arc<dyn RobotClock>,
        expires_unix: u64,
        wheel_ids: [u8; 3],
        motion: impl Into<Motion>,
    ) -> Result<Self, String> {
        let ids = motor_ids(wheel_ids)?;
        let motion = motion.into();
        if let Motion::Single(id) = motion
            && !wheel_ids.contains(&id)
        {
            return Err("所选物理轮必须属于本次三轮ID".into());
        }
        let unix = Self::unix_now()?;
        let remaining = expires_unix
            .checked_sub(unix)
            .filter(|r| *r > 0)
            .ok_or("GO已过期")?;
        let now = clock.now().0;
        let hard_deadline = now
            .checked_add(remaining.min(motion.pulse_ns() + 8_000_000_000))
            .ok_or("GO本地期限溢出")?;
        Ok(Self {
            ids,
            motion,
            goals: motion.goals(wheel_ids),
            clock,
            authority: ControlAuthority::new(AuthorityInit::new(
                false,
                OutputSafetyState::TorqueUnknown,
                InhibitSet::EMPTY,
            ))
            .map_err(|e| format!("权限域构造失败:{e:?}"))?,
            expires_unix,
            hard_deadline,
            deadline: AtomicU64::new(hard_deadline),
            last: AtomicU64::new(now),
            last_unix: AtomicU64::new(unix),
            failed: AtomicBool::new(false),
            cleanup: AtomicBool::new(false),
            class: AtomicU8::new(0),
            enable_view: Mutex::new(None),
            enable_attempt: AtomicU64::new(0),
            nonzero_attempt: AtomicU64::new(0),
            calibration_failure: AtomicU64::new(0),
            pv_words: Mutex::new(None),
        })
    }
    fn check(&self) -> Result<(), AbortReason> {
        let now = self.clock.now().0;
        let previous = self.last.fetch_max(now, Ordering::SeqCst);
        let unix_invalid = Self::unix_now().map_or(true, |unix| {
            unix < self.last_unix.fetch_max(unix, Ordering::SeqCst) || unix >= self.expires_unix
        });
        let invalid = now < previous
            || now >= self.deadline.load(Ordering::SeqCst)
            || now >= self.hard_deadline
            || unix_invalid;
        if INTERRUPTED.load(Ordering::SeqCst) || self.failed.load(Ordering::SeqCst) || invalid {
            self.failed.store(true, Ordering::SeqCst);
            let _ = self.authority.disable();
            return Err(if INTERRUPTED.load(Ordering::SeqCst) {
                AbortReason::Emergency
            } else {
                AbortReason::Timeout
            });
        }
        Ok(())
    }
    fn set_deadline(&self, deadline: u64) {
        self.deadline
            .store(deadline.min(self.hard_deadline), Ordering::SeqCst);
    }
    fn wait(&self, target: u64) -> Result<(), String> {
        loop {
            self.check().map_err(|e| format!("等待中止:{e:?}"))?;
            let now = self.clock.now().0;
            if now >= target {
                return Ok(());
            }
            self.clock
                .sleep_for(DurationNs((target - now).min(2_000_000)));
        }
    }
    fn fail(&self, error: String) -> String {
        self.failed.store(true, Ordering::SeqCst);
        let _ = self.authority.disable();
        error
    }

    fn adapter_error(&self, error: RobotError) -> String {
        let diagnostic = self.calibration_failure.load(Ordering::Relaxed);
        if matches!(error, RobotError::InvalidParameter("feetech calibration")) && diagnostic != 0 {
            let bytes = diagnostic.to_le_bytes();
            let raw = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            return format!("{error}: motor={}, raw={raw}", bytes[4]);
        }
        error.to_string()
    }
}
impl DiagSink for Guard {
    fn record(&self, event: DiagEvent) {
        // 仅保存首次启用校准拒绝；诊断不参与授权、停止或重试决策。
        if (event.site, event.kind, event.a) == (3, 8, 3) && event.b != 0 {
            let diagnostic = (u64::from(event.b) << 32) | u64::from(event.seq_lo);
            let _ = self.calibration_failure.compare_exchange(
                0,
                diagnostic,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
    }
}
impl PreemptProbe for Guard {
    fn should_abort(&self, class: BusOpClass) -> Option<AbortReason> {
        if class == BusOpClass::Safety && self.cleanup.load(Ordering::SeqCst) {
            None
        } else {
            self.check().err()
        }
    }
    fn checkpoint(&self) {
        if !self.cleanup.load(Ordering::SeqCst) {
            let _ = self.check();
        }
    }
    fn sleep_or_abort(&self, class: BusOpClass, duration: DurationNs) -> Result<(), AbortReason> {
        let start = self.clock.now().0;
        loop {
            if let Some(error) = self.should_abort(class) {
                return Err(error);
            }
            let now = self.clock.now().0;
            let elapsed = now.checked_sub(start).ok_or(AbortReason::Timeout)?;
            if elapsed >= duration.0 {
                return Ok(());
            }
            self.clock
                .sleep_for(DurationNs((duration.0 - elapsed).min(2_000_000)));
        }
    }
    fn operation_class(&self) -> BusOpClass {
        if self.class.load(Ordering::SeqCst) == 0 {
            BusOpClass::Connect
        } else {
            BusOpClass::NormalEffectful
        }
    }
    fn confirmation_trusted(&self) -> bool {
        !self.failed.load(Ordering::SeqCst)
    }
    fn report_comm_error(&self, _: CommErrorKind) {
        self.failed.store(true, Ordering::SeqCst);
        let _ = self.authority.disable();
    }
}
impl LeasedPreemptProbe for Guard {
    fn begin_leased(&self, class: BusOpClass) -> Result<BusLease, AbortReason> {
        if let Some(error) = self.should_abort(class) {
            return Err(error);
        }
        if let Some(lease) = BusLease::special(class) {
            return Ok(lease);
        }
        let intent = if class == BusOpClass::PeriodicRead {
            NormalIoIntent::PeriodicRead
        } else if let Some(view) = *self.enable_view.lock().map_err(|_| AbortReason::Revoked)? {
            NormalIoIntent::EnableStep(view)
        } else {
            NormalIoIntent::Motion
        };
        let lease = self
            .authority
            .acquire_normal_io(intent)
            .map_err(|_| AbortReason::Revoked)?;
        match BusLease::with_normal(class, lease) {
            Ok(lease) => Ok(lease),
            Err(lease) => {
                let _ = self.authority.release_normal_io(lease);
                Err(AbortReason::Revoked)
            }
        }
    }
    fn finish_leased(&self, lease: BusLease) {
        if lease.release(&self.authority).is_err() {
            self.failed.store(true, Ordering::SeqCst);
        }
    }
}

/// 仅给本固定维护动作加真实IO前检查；串口收发、绝对写与协议仍用原实现。
struct Port<T> {
    inner: T,
    guard: Arc<Guard>,
    pv_response: bool,
}
impl<T: SerialTransport> Port<T> {
    fn budget(&mut self) -> Result<(), FeetechError> {
        if self.guard.cleanup.load(Ordering::SeqCst) {
            return self.inner.set_timeout(Duration::from_millis(50));
        }
        self.guard.check().map_err(FeetechError::Aborted)?;
        let remaining = self
            .guard
            .deadline
            .load(Ordering::SeqCst)
            .checked_sub(self.guard.clock.now().0)
            .filter(|left| *left > 0)
            .ok_or(FeetechError::Aborted(AbortReason::Timeout))?;
        self.inner
            .set_timeout(Duration::from_nanos(remaining.min(PERIOD)))
    }
    fn before_write(&mut self, bytes: &[u8]) -> Result<(), FeetechError> {
        self.budget()?;
        if self.guard.cleanup.load(Ordering::SeqCst) {
            // 安全收尾只接受已固定三轮GV0或完整九TE0，不能借cleanup写GP/TE1。
            let zero = bytes.len() == 17
                && bytes[2..7] == [254, 13, 131, 46, 2]
                && self.guard.ids[6..]
                    .iter()
                    .enumerate()
                    .all(|(i, id)| bytes[7 + i * 3..10 + i * 3] == [*id, 0, 0]);
            let off = bytes.len() == 26
                && bytes[2..7] == [254, 22, 131, 40, 1]
                && self
                    .guard
                    .ids
                    .iter()
                    .enumerate()
                    .all(|(i, id)| bytes[7 + i * 2] == *id && bytes[8 + i * 2] == 0);
            if !zero && !off {
                return Err(FeetechError::Aborted(AbortReason::Revoked));
            }
            return Ok(());
        }
        self.guard.check().map_err(FeetechError::Aborted)?;
        if bytes.len() == 8 && bytes[4..7] == [3, 40, 1] {
            let now = self.guard.clock.now().0;
            if self
                .guard
                .enable_attempt
                .compare_exchange(0, now, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                self.guard.set_deadline(
                    now.saturating_add(self.guard.motion.enable_budget_ns() - 100_000_000)
                        .min(self.guard.deadline.load(Ordering::SeqCst)),
                );
            }
        }
        if bytes.len() == 17 && bytes[4..7] == [131, 46, 2] {
            let mut all_zero = true;
            let mut fixed_goal = true;
            for (index, entry) in bytes[7..16].as_chunks::<3>().0.iter().enumerate() {
                if entry[0] != self.guard.ids[6 + index] {
                    return Err(FeetechError::Aborted(AbortReason::Revoked));
                }
                let word = u32::from(u16::from_le_bytes([entry[1], entry[2]]));
                all_zero &= word == 0;
                fixed_goal &=
                    word == encode_sign_magnitude(i32::from(self.guard.goals[index]), 15)?;
            }
            if !all_zero && !fixed_goal {
                return Err(FeetechError::Aborted(AbortReason::Revoked));
            }
            if fixed_goal {
                let _ = self.guard.nonzero_attempt.compare_exchange(
                    0,
                    self.guard.clock.now().0,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
            }
        }
        self.pv_response =
            bytes.len() == 17 && bytes[4..7] == [130, 58, 2] && bytes[7..16] == self.guard.ids;
        Ok(())
    }
    fn returned(&self) -> Result<(), FeetechError> {
        if self.guard.cleanup.load(Ordering::SeqCst) {
            Ok(())
        } else {
            self.guard.check().map_err(FeetechError::Aborted)
        }
    }
}
impl<T: SerialTransport> SerialTransport for Port<T> {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), FeetechError> {
        self.before_write(bytes)?;
        self.inner.write_all(bytes)?;
        self.returned()
    }
    fn write_all_until(
        &mut self,
        bytes: &[u8],
        deadline: SourceWriteDeadline<'_>,
    ) -> Result<(), FeetechError> {
        self.before_write(bytes)?;
        self.inner.write_all_until(bytes, deadline)?;
        self.returned()
    }
    fn read_exact(&mut self, bytes: &mut [u8]) -> Result<(), FeetechError> {
        self.guard.check().map_err(FeetechError::Aborted)?;
        self.budget()?;
        self.inner.read_exact(bytes)?;
        if self.pv_response && bytes.len() == 72 {
            let mut words = [0; 9];
            for (index, packet) in bytes.as_chunks::<8>().0.iter().enumerate() {
                let status = parse_status(packet)?;
                if status.id != self.guard.ids[index]
                    || status.error != 0
                    || status.params.len() != 2
                {
                    return Err(FeetechError::Aborted(AbortReason::Revoked));
                }
                words[index] = u16::from_le_bytes(status.params.try_into().unwrap());
            }
            *self
                .guard
                .pv_words
                .lock()
                .map_err(|_| FeetechError::Aborted(AbortReason::Revoked))? = Some(words);
            self.pv_response = false;
        }
        self.returned()
    }
    fn set_timeout(&mut self, duration: Duration) -> Result<(), FeetechError> {
        self.inner.set_timeout(duration)
    }
    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }
    fn discard_input(&mut self) -> Result<(), FeetechError> {
        self.inner.discard_input()
    }
    fn drain_until_quiet(
        &mut self,
        quiet: Duration,
        budget: Duration,
    ) -> Result<DrainReport, FeetechError> {
        self.inner.drain_until_quiet(quiet, budget)
    }
}

fn profile(role: WheelRole, motor_id: u8) -> Sts3215RawWheelProfile {
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
}
fn read_raw<T: SerialTransport>(
    adapter: &mut FeetechAdapter<T>,
    guard: &Guard,
    register: &'static str,
    ids: &[u8],
) -> Result<Vec<u32>, String> {
    guard.check().map_err(|e| format!("读取中止:{e:?}"))?;
    adapter.bus().set_op_class(BusOpClass::PeriodicRead);
    let mut raw = vec![0; ids.len()];
    adapter
        .bus()
        .sync_read_by_id_raw_into(register, ids, &mut raw, &mut [0; 16], &mut [0; 24])
        .map_err(|e| e.to_string())?;
    guard.check().map_err(|e| format!("读取返回过期:{e:?}"))?;
    Ok(raw)
}
fn write_cycle<T: SerialTransport + 'static>(
    adapter: &mut FeetechAdapter<T>,
    guard: &Guard,
    anchors: [i32; 6],
    source: u64,
    deadline: u64,
) -> Result<(), String> {
    guard.check().map_err(|e| format!("写入中止:{e:?}"))?;
    let mut command = JointCommandBuf {
        joint_count: 9,
        mode: ControlModeRepr::Position,
        selection: JointSelection::MixedPositionRawVelocity {
            position_mask: 63,
            velocity_mask: 448,
        },
        stamp_ns: source,
        deadline_ns: deadline.min(guard.deadline.load(Ordering::SeqCst)),
        ..Default::default()
    };
    for (index, anchor) in anchors.into_iter().enumerate() {
        command.positions[index] = raw_to_rad(anchor);
    }
    command.raw_velocities[6..9].copy_from_slice(&guard.goals);
    adapter
        .write_command(&command.as_view())
        .map_err(|e| e.to_string())
}

#[allow(
    clippy::too_many_lines,
    reason = "固定一次维护动作与无条件独立断扭保持在同一顺序控制流，不新增会话或调度框架"
)]
fn run<T: SerialTransport + 'static>(
    transport: T,
    guard: &Arc<Guard>,
    calibrations: &[MotorCalibration],
    calibration_range_tolerances_raw: [u16; 6],
    report: &mut Value,
) -> Result<(), String> {
    let ids = guard.ids;
    let pulse = guard.motion.pulse_ns();
    let enable_budget = guard.motion.enable_budget_ns();
    let held_slots: Vec<_> = (0..3).filter(|&role| guard.goals[role] == 0).collect();
    let held_ids: Vec<_> = held_slots.iter().map(|&role| ids[6 + role]).collect();
    bind_report(report, &ids, guard.motion);
    report["calibration_range_tolerances_raw"] = json!(calibration_range_tolerances_raw);
    let motors: HashMap<_, _> = NAMES
        .iter()
        .zip(ids)
        .map(|(name, id)| (name.to_string(), Motor::new(id).unwrap()))
        .collect();
    let port = Port {
        inner: transport,
        guard: guard.clone(),
        pv_response: false,
    };
    let bus = FeetechBus::new(port, motors).map_err(|e| e.to_string())?;
    let config = FeetechAdapterConfig {
        clock: guard.clock.clone(),
        preempt: Some(guard.clone()),
        diag: Some(guard.clone()),
        call_deadline: Duration::from_millis(50),
        adapter_name: "mixed-wheel-maintenance",
        joint_names: NAMES.iter().map(ToString::to_string).collect(),
        calibrations: calibrations.to_owned(),
        calibration_range_tolerances_raw: calibration_range_tolerances_raw
            .into_iter()
            .chain([0; 3])
            .collect(),
    };
    let profiles = [
        profile(WheelRole::Front, ids[6]),
        profile(WheelRole::Left, ids[7]),
        profile(WheelRole::Right, ids[8]),
    ];
    let mut adapter = match guard.motion {
        Motion::Single(_) => FeetechAdapter::new_bus2_mixed_raw(config, bus, profiles),
        Motion::Direction(_) | Motion::GroundForward => {
            FeetechAdapter::new_bus2_direction_maintenance(config, bus, profiles, guard.goals)
        }
    }
    .map_err(|e| e.to_string())?;
    let outcome = (|| {
        adapter.connect().map_err(|e| e.to_string())?;
        // connect逐个真实确认TE0；这里只将本次全九TE读回提交为同权限域安全事实。
        let observation = guard
            .authority
            .begin_safety_observation(SafetyOperation::ConfirmOff);
        let mut torque = [false; 9];
        adapter
            .read_torque_enable_mask(&mut torque)
            .map_err(|e| e.to_string())?;
        if torque.iter().any(|enabled| *enabled) {
            return Err("初始九电机并非TE0".into());
        }
        guard
            .authority
            .complete_safety_observation(observation)
            .map_err(|e| format!("初始安全事实拒绝:{e:?}"))?;
        for name in &NAMES[6..] {
            adapter.bus().set_op_class(BusOpClass::PeriodicRead);
            if adapter
                .bus()
                .read("Goal_Velocity", name)
                .map_err(|e| e.to_string())?
                != 0
            {
                return Err("初始三轮GV并非0".into());
            }
        }
        let (owner, view) = guard
            .authority
            .reserve_enable()
            .map_err(|e| format!("Enable预留拒绝:{e:?}"))?;
        let execution = guard
            .authority
            .start_enable(view)
            .map_err(|e| format!("Enable启动拒绝:{e:?}"))?;
        *guard.enable_view.lock().map_err(|_| "权限锁异常")? = Some(execution.view());
        guard.class.store(1, Ordering::SeqCst);
        // 比实际PP采样更早的固定200ms上界，在真实TX处约束整个防飞/TE1阶段。
        guard.set_deadline(guard.clock.now().0.saturating_add(200_000_000));
        let enabled = adapter.enable(true);
        *guard.enable_view.lock().map_err(|_| "权限锁异常")? = None;
        guard
            .authority
            .finish_enable(
                execution,
                if enabled.is_ok() {
                    EnableResult::Succeeded
                } else {
                    EnableResult::Failed
                },
            )
            .map_err(|e| format!("Enable完成拒绝:{e:?}"))?;
        enabled.map_err(|e| guard.adapter_error(e))?;
        guard
            .authority
            .commit_enable(owner)
            .map_err(|e| format!("Enable提交拒绝:{e:?}"))?;
        let (anchors, sampled_ns) = adapter.mixed_arm_anchor().ok_or("缺少enable实际固定锚点")?;
        let goals = adapter.mixed_arm_goal_readback().ok_or("缺少实际GP读回")?;
        for index in 0..6 {
            report["arm_anchors"].as_array_mut().unwrap().push(json!({"motor_id":ids[index],"raw":anchors[index],"goal_raw":goals[index],"sampled_ns":sampled_ns,"calibration_min":calibrations[index].range_min,"calibration_max":calibrations[index].range_max}));
        }
        let mut wheel_anchors = [0; 3];
        if !held_ids.is_empty() {
            let wheel_sampled = guard.clock.now().0;
            let mut observations = vec![0; held_ids.len()];
            adapter.bus().set_op_class(BusOpClass::PeriodicRead);
            adapter
                .bus()
                .sync_read_by_id_into("Present_Position", &held_ids, &mut observations)
                .map_err(|e| e.to_string())?;
            for (&role, raw) in held_slots.iter().zip(observations) {
                if !(0..=4095).contains(&raw) {
                    return Err("未驱动轮基线超界".into());
                }
                wheel_anchors[role] = raw;
                report["wheel_anchors"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"motor_id":ids[6 + role],"raw":raw,"sampled_ns":wheel_sampled}));
            }
        }
        let mut first = guard.clock.now().0;
        if first
            .checked_sub(sampled_ns)
            .is_none_or(|age| age > 200_000_000)
        {
            return Err("首非零前锚点超过200ms或时钟倒退".into());
        }
        report["timing"]["first_nonzero_ns"] = json!(first);

        let mut state = JointStateBuf::default();
        let mut previous = [None; 3];
        let mut travel = [0; 3];
        let mut observed_motion = [false; 3];
        for cycle in 0..pulse / PERIOD {
            let scheduled = first + cycle * PERIOD;
            let deadline = if cycle == 0 {
                (scheduled + PERIOD).min(sampled_ns + 200_000_000)
            } else {
                scheduled + PERIOD
            };
            guard
                .set_deadline(deadline.min(
                    guard.enable_attempt.load(Ordering::SeqCst) + enable_budget - 100_000_000,
                ));
            guard.wait(scheduled)?;
            let started = guard.clock.now().0;
            write_cycle(&mut adapter, guard, anchors, started, deadline)?;
            if cycle == 0 {
                first = guard.nonzero_attempt.load(Ordering::SeqCst);
            }
            if cycle != 0 && cycle % 5 == 0 {
                let started_ns = guard.clock.now().0;
                adapter.read_state(&mut state).map_err(|e| e.to_string())?;
                let finished_ns = guard.clock.now().0;
                let raw = state.positions[..9]
                    .iter()
                    .map(|p| robot_adapter_feetech::rad_to_raw(*p))
                    .collect::<Vec<_>>();
                let words = guard
                    .pv_words
                    .lock()
                    .map_err(|_| "反馈原字锁异常")?
                    .take()
                    .ok_or("缺少本次完整PV原字")?;
                report["wheel_samples"].as_array_mut().unwrap().push(json!({"started_ns":started_ns,"finished_ns":finished_ns,"position_raw":&raw[6..9],"velocity_word":&words[6..9],"arm_position_raw":&raw[..6]}));
                for index in 0..6 {
                    if (raw[index] - anchors[index]).abs() > 4 {
                        return Err("六臂偏离固定锚点超过4计数".into());
                    }
                }
                if raw.iter().any(|r| !(0..=4095).contains(r)) {
                    return Err("运动位置反馈原值越界".into());
                }
                for (role, goal) in guard.goals.iter().copied().enumerate() {
                    let slot = role + 6;
                    let position = raw[slot];
                    let word = words[slot];
                    if goal == 0 {
                        if !matches!(word, 0 | 50 | 0x8032)
                            || (position - wheel_anchors[role]).abs() > 4
                        {
                            return Err("未驱动轮反馈超出原非目标范围".into());
                        }
                    } else {
                        if !velocity_matches_motion(word.into(), goal) {
                            return Err(format!("物理{}速度反馈越界或方向异常", ids[slot]));
                        }
                        observed_motion[role] |= word != 0;
                        if let Some(last) = previous[role] {
                            let signed_step: i32 = (position - last) * i32::from(goal.signum());
                            let step = signed_step.rem_euclid(4096);
                            if step > 512 {
                                return Err(format!("物理{}相邻位移越界或逆向", ids[slot]));
                            }
                            travel[role] += step;
                        }
                        previous[role] = Some(position);
                    }
                }
            }
            let finished = guard.clock.now().0;
            report["cycles"].as_array_mut().unwrap().push(
                json!({"started_ns":started,"finished_ns":finished,"source_deadline_ns":deadline}),
            );
            guard.check().map_err(|e| format!("周期完成过期:{e:?}"))?;
        }
        let pulse_end = first.checked_add(pulse).ok_or("运动期限溢出")?;
        let enable_deadline =
            guard.enable_attempt.load(Ordering::SeqCst) + enable_budget - 100_000_000;
        let zero_write_deadline = if matches!(guard.motion, Motion::GroundForward) {
            enable_deadline.min(pulse_end.checked_add(PERIOD).ok_or("落地发零期限溢出")?)
        } else {
            enable_deadline
        };
        guard.set_deadline(zero_write_deadline);
        guard.wait(pulse_end)?;
        for (role, goal) in guard.goals.iter().enumerate() {
            if *goal != 0 && (travel[role] == 0 || !observed_motion[role]) {
                return Err(format!(
                    "物理{}在运动窗口内未得到指定方向的位置或速度变化",
                    ids[6 + role]
                ));
            }
        }
        let zero_started = guard.clock.now().0;
        report["timing"]["zero_started_ns"] = json!(zero_started);
        let zero_deadline = (zero_started + 500_000_000).min(enable_deadline);
        guard.set_deadline(zero_deadline.min(zero_write_deadline));
        adapter.bus().set_op_class(BusOpClass::PeriodicWrite);
        adapter
            .bus()
            .sync_write_by_id(
                "Goal_Velocity",
                &ids[6..].iter().map(|id| (*id, 0)).collect::<Vec<_>>(),
            )
            .map_err(|e| e.to_string())?;
        let zero_finished = guard.clock.now().0;
        report["timing"]["zero_finished_ns"] = json!(zero_finished);
        if matches!(guard.motion, Motion::GroundForward) {
            guard.check().map_err(|e| format!("落地发零过期:{e:?}"))?;
        }
        for (index, offset) in [50_000_000, 250_000_000, 450_000_000]
            .into_iter()
            .enumerate()
        {
            let slot_deadline = if index < 2 {
                (zero_finished + [250_000_000, 450_000_000][index]).min(zero_deadline)
            } else {
                zero_deadline
            };
            guard.set_deadline(slot_deadline);
            guard.wait(zero_finished + offset)?;
            let started = guard.clock.now().0;
            let wheel_ids = &ids[6..9];
            let watch = match &guard.motion {
                Motion::Single(id) => std::slice::from_ref(id),
                Motion::Direction(_) | Motion::GroundForward => wheel_ids,
            };
            let te = read_raw(&mut adapter, guard, "Torque_Enable", watch)?;
            let gv = read_raw(&mut adapter, guard, "Goal_Velocity", watch)?;
            let pv = read_raw(&mut adapter, guard, "Present_Velocity", watch)?;
            let reading = |values: &[u32]| match guard.motion {
                Motion::Single(_) => json!(values[0]),
                Motion::Direction(_) | Motion::GroundForward => json!(values),
            };
            report["zero_samples"].as_array_mut().unwrap().push(json!({"started_ns":started,"finished_ns":guard.clock.now().0,"torque_enable":reading(&te),"goal_velocity_word":reading(&gv),"present_velocity_word":reading(&pv)}));
            for (sample, id) in watch.iter().enumerate() {
                let role = wheel_ids.iter().position(|wheel| wheel == id).unwrap();
                let settled = matches!(pv[sample], 0 | 50 | 0x8032);
                if te[sample] != 1
                    || gv[sample] != 0
                    || (!settled
                        && (index == 2 || !velocity_matches_motion(pv[sample], guard.goals[role])))
                {
                    return Err("TE1零速500ms读回不满足既有合同".into());
                }
            }
        }
        Ok(())
    })();
    let mut error = outcome.err().map(|error| guard.fail(error));
    let nonzero = guard.nonzero_attempt.load(Ordering::SeqCst);
    report["timing"]["first_nonzero_ns"] = if nonzero == 0 {
        Value::Null
    } else {
        json!(nonzero)
    };
    let attempted = guard.enable_attempt.load(Ordering::SeqCst);
    report["timing"]["enable_attempt_ns"] = if attempted == 0 {
        Value::Null
    } else {
        json!(attempted)
    };
    guard.cleanup.store(true, Ordering::SeqCst);
    let _ = guard.authority.disable();
    adapter.bus().set_op_class(BusOpClass::Safety);
    if attempted != 0 {
        report["cleanup"]["zero_attempted"] = json!(true);
        let zero = adapter.bus().sync_write_by_id(
            "Goal_Velocity",
            &ids[6..].iter().map(|id| (*id, 0)).collect::<Vec<_>>(),
        );
        report["cleanup"]["zero_ok"] = json!(zero.is_ok());
        if let Err(failed) = zero {
            error.get_or_insert_with(|| failed.to_string());
        }
    }
    {
        report["timing"]["disable_attempt_ns"] = json!(guard.clock.now().0);
        report["cleanup"]["disable_attempted"] = json!(true);
        let disabled = adapter.safety_stop();
        report["timing"]["disable_finished_ns"] = json!(guard.clock.now().0);
        report["cleanup"]["disable_ok"] = json!(disabled.is_ok());
        if let Err(failed) = disabled {
            error.get_or_insert_with(|| failed.to_string());
        }
        if attempted != 0
            && guard
                .clock
                .now()
                .0
                .checked_sub(attempted)
                .is_none_or(|elapsed| elapsed > enable_budget)
        {
            error.get_or_insert_with(|| "首次TE1至全九TE0超出本次启用预算".into());
        }
        guard.cleanup.store(false, Ordering::SeqCst);
        if error.is_none() {
            guard.set_deadline(guard.hard_deadline);
            let mut torque = [false; 9];
            let mask = adapter
                .read_torque_enable_mask(&mut torque)
                .map(|()| torque.iter().any(|enabled| *enabled))
                .map_err(|e| e.to_string());
            match mask {
                Ok(false) => {
                    report["cleanup"]["torque_enable_after"] = json!([0, 0, 0, 0, 0, 0, 0, 0, 0]);
                }
                Ok(_) => {
                    error = Some("全九TE0读回不通过".into());
                }
                Err(failed) => {
                    error = Some(failed);
                }
            }
            if error.is_none() {
                let mut final_raw = [0; 6];
                adapter.bus().set_op_class(BusOpClass::PeriodicRead);
                match adapter.bus().sync_read_by_id_into(
                    "Present_Position",
                    &ids[..6],
                    &mut final_raw,
                ) {
                    Ok(()) => {
                        for (index, raw) in final_raw.into_iter().enumerate() {
                            report["arm_final"]
                                .as_array_mut()
                                .unwrap()
                                .push(json!({"motor_id":ids[index],"raw":raw}));
                        }
                        if let Some((anchors, _)) = adapter.mixed_arm_anchor()
                            && final_raw
                                .iter()
                                .zip(anchors)
                                .any(|(raw, anchor)| (*raw - anchor).abs() > 4)
                        {
                            error = Some("六臂末置偏离固定锚点超过4计数".into());
                        }
                    }
                    Err(failed) => error = Some(failed.to_string()),
                }
            }
        }
    }
    report["cycle_count"] = json!(report["cycles"].as_array().unwrap().len());
    report["timing"]["finished_ns"] = json!(guard.clock.now().0);
    if let Some(error) = error {
        Err(error)
    } else {
        Ok(())
    }
}

fn motor_ids(wheel_ids: [u8; 3]) -> Result<[u8; 9], String> {
    let mut sorted = wheel_ids;
    sorted.sort_unstable();
    if sorted != [7, 8, 9] {
        return Err("三轮ID必须唯一且集合恰为7/8/9".into());
    }
    Ok([1, 2, 3, 4, 5, 6, wheel_ids[0], wheel_ids[1], wheel_ids[2]])
}

fn parse_wheel_ids(value: &str) -> Result<[u8; 3], String> {
    let parsed = value
        .split(',')
        .map(str::parse::<u8>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "三轮ID解析失败")?;
    let wheel_ids: [u8; 3] = parsed
        .try_into()
        .map_err(|_| "必须显式提供前/左/右三个ID")?;
    motor_ids(wheel_ids)?;
    Ok(wheel_ids)
}

/// 先于标定读取、信号安装与UART打开拒绝无效物理轮及旧入口不匹配。
fn parse_selected_motor_id(args: &[String], wheel_ids: [u8; 3]) -> Result<u8, String> {
    let selected_motor_id = args
        .get(2)
        .ok_or("缺少所选物理轮ID")?
        .parse::<u8>()
        .map_err(|_| "所选物理轮ID解析失败")?;
    if !wheel_ids.contains(&selected_motor_id) {
        return Err("所选物理轮必须属于本次三轮ID".into());
    }
    match args.get(3).map(String::as_str) {
        Some("mixed-hold-wheel") => Ok(selected_motor_id),
        Some("mixed-hold-wheel9") if selected_motor_id == 9 => Ok(selected_motor_id),
        Some("mixed-hold-wheel9") => Err("mixed-hold-wheel9仅允许物理9".into()),
        _ => Err("mixed维护操作不匹配".into()),
    }
}

fn parse_calibration_range_tolerances_raw(args: &[String]) -> Result<[u16; 6], String> {
    let value = match args {
        [] => return Ok([0; 6]),
        [flag, value] if flag == "--calibration-range-tolerances-raw" => value,
        _ => return Err("六臂校准容差参数不匹配".into()),
    };
    let parsed = value
        .split(',')
        .map(str::parse::<u16>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "六臂校准容差解析失败")?;
    if parsed.iter().any(|tolerance| *tolerance > 4095) {
        return Err("六臂校准容差必须在0..4095内".into());
    }
    parsed
        .try_into()
        .map_err(|_| "六臂校准容差必须恰为六个整数".into())
}

fn bind_report(report: &mut Value, ids: &[u8; 9], motion: impl Into<Motion>) {
    let motion = motion.into();
    let wheel_ids: [u8; 3] = ids[6..].try_into().unwrap();
    report["motor_ids"] = json!(ids);
    report["wheel_ids"] = json!(wheel_ids);
    if report["operation"] == "mixed-hold-wheel"
        && let Motion::Single(id) = motion
    {
        report["selected_motor_id"] = json!(id);
    }
    report["wheel_raw_goal"] = json!(motion.goals(wheel_ids));
}

fn parse_motion(args: &[String], wheel_ids: [u8; 3]) -> Result<Motion, String> {
    if args.get(3).is_some_and(|op| op == "mixed-ground-forward") {
        return match args.get(2).map(String::as_str) {
            Some("forward") => Ok(Motion::GroundForward),
            _ => Err("本次落地短距维护仅允许前进".into()),
        };
    }
    if args.get(3).map(String::as_str) == Some("mixed-hold-direction") {
        let direction = match args.get(2).map(String::as_str) {
            Some("forward") => BaseDirection::Forward,
            Some("backward") => BaseDirection::Backward,
            Some("turn_left") => BaseDirection::RotateLeft,
            Some("turn_right") => BaseDirection::RotateRight,
            _ => return Err("联动维护仅接受四种具名方向".into()),
        };
        Ok(Motion::Direction(direction))
    } else {
        parse_selected_motor_id(args, wheel_ids).map(Motion::Single)
    }
}

fn empty_report(args: &[String]) -> Value {
    let (schema, operation) = match args.get(3).map(String::as_str) {
        Some("mixed-ground-forward") => ("FEETECH_MIXED_DIRECTION_V1", "mixed-ground-forward"),
        Some("mixed-hold-direction") => ("FEETECH_MIXED_DIRECTION_V1", "mixed-hold-direction"),
        Some("mixed-hold-wheel9") => ("FEETECH_MIXED_WHEEL9_V1", "mixed-hold-wheel9"),
        _ => ("FEETECH_MIXED_WHEEL_V1", "mixed-hold-wheel"),
    };
    let pulse = if operation == "mixed-ground-forward" {
        GROUND_PULSE
    } else {
        PULSE
    };
    let mut report = json!({"schema":schema,"operation":operation,"result":"FAIL","error":null,"serial_device":args.get(1),"arm_calibration_path":args.get(5),"go_expires_at_unix_ns":null,"motor_ids":[],"wheel_ids":[],"wheel_raw_goal":[],"calibration_range_tolerances_raw":[0,0,0,0,0,0],"pulse_duration_ns":pulse,"zero_window_ns":500_000_000_u64,"enable_budget_ns":pulse + ENABLE_MARGIN,"loaded_calibration":[],"arm_anchors":[],"arm_final":[],"cycle_count":0,"cycles":[],"wheel_anchors":[],"wheel_samples":[],"zero_samples":[],"timing":{"enable_attempt_ns":null,"first_nonzero_ns":null,"zero_started_ns":null,"zero_finished_ns":null,"disable_attempt_ns":null,"disable_finished_ns":null,"finished_ns":null},"cleanup":{"zero_attempted":false,"zero_ok":false,"disable_attempted":false,"disable_ok":false,"torque_enable_after":null}});
    if schema == "FEETECH_MIXED_DIRECTION_V1" {
        report["direction"] = json!(args.get(2));
    }
    if operation == "mixed-hold-wheel" {
        report["selected_motor_id"] = Value::Null;
    }
    report
}

fn execute(args: &[String]) -> Value {
    let mut report = empty_report(args);
    let result = (|| {
        if !matches!(args.len(), 10 | 12)
            || args[1] != DEVICE
            || args[4] != "--arm-calibration"
            || args[6] != "--go-expires-at-ns"
            || args[8] != "--wheel-ids"
        {
            return Err("固定mixed维护参数不匹配".into());
        }
        let wheel_ids = parse_wheel_ids(&args[9])?;
        let motion = parse_motion(args, wheel_ids)?;
        let calibration_range_tolerances_raw = parse_calibration_range_tolerances_raw(&args[10..])?;
        report["calibration_range_tolerances_raw"] = json!(calibration_range_tolerances_raw);
        let ids = motor_ids(wheel_ids)?;
        bind_report(&mut report, &ids, motion);
        let expiry = args[7].parse::<u64>().map_err(|_| "GO Unix期限解析失败")?;
        report["go_expires_at_unix_ns"] = json!(expiry);
        let mut loaded =
            robot_adapter_feetech::load_file(Path::new(&args[5])).map_err(|e| e.to_string())?;
        if loaded.len() != 6 {
            return Err("右臂标定必须恰为固定六电机".into());
        }
        let mut calibrations = Vec::with_capacity(9);
        for (name, id) in NAMES[..6].iter().zip(&ids[..6]) {
            let cal = loaded.remove(*name).ok_or("缺少固定右臂标定")?;
            if cal.id != *id {
                return Err("右臂标定ID不匹配".into());
            }
            report["loaded_calibration"].as_array_mut().unwrap().push(json!({"id":cal.id,"range_min":cal.range_min,"range_max":cal.range_max,"homing_offset":cal.homing_offset,"drive_mode":cal.drive_mode}));
            calibrations.push(cal);
        }
        for id in &ids[6..] {
            calibrations
                .push(MotorCalibration::new(*id, 0, 0, 0, 4095).map_err(|e| e.to_string())?);
        }
        let _signals = Signals::install()?;
        let guard = Arc::new(Guard::new(
            Arc::new(SystemClock::new()),
            expiry,
            wheel_ids,
            motion,
        )?);
        guard
            .check()
            .map_err(|e| format!("UART前授权检查拒绝:{e:?}"))?;
        let transport = robot_adapter_feetech::transport::real::RealSerialTransport::open(
            Path::new(DEVICE),
            DEFAULT_BAUDRATE,
        )
        .map_err(|e| e.to_string())?;
        run(
            transport,
            &guard,
            &calibrations,
            calibration_range_tolerances_raw,
            &mut report,
        )
    })();
    match result {
        Ok(()) => {
            report["result"] = json!("PASS");
        }
        Err(error) => {
            report["error"] = json!(error);
        }
    }
    report
}

pub(super) fn main(args: &[String]) -> ExitCode {
    let report = execute(args);
    println!("{report}");
    if report["result"] == "PASS" {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use robot_adapter_feetech::protocol::checksum;
    use robot_core::time::{MockClock, MonoRawNs};
    use std::collections::VecDeque;

    // 模拟信号场景共享入口原子标志，禁止并行修改。
    static SIM_LOCK: Mutex<()> = Mutex::new(());

    struct AdvancingClock(Arc<MockClock>);
    impl RobotClock for AdvancingClock {
        fn now(&self) -> MonoRawNs {
            self.0.advance(DurationNs(1_000));
            self.0.now()
        }
        fn sleep_until(&self, deadline: robot_core::time::Deadline) {
            self.0.sleep_until(deadline);
        }
        fn sleep_for(&self, duration: DurationNs) {
            self.0.sleep_for(duration);
        }
    }

    struct Sim {
        ids: [u8; 9],
        selected_motor_id: u8,
        clock: Arc<MockClock>,
        rx: VecDeque<u8>,
        frames: Arc<Mutex<Vec<Vec<u8>>>>,
        goal: Arc<Mutex<[u16; 10]>>,
        torque: [u8; 10],
        position: [u16; 10],
        velocity: [u16; 10],
        scenario: u8,
        nonzero: bool,
        previous_goal: [u16; 10],
        zero_samples: [usize; 10],
    }
    impl Sim {
        fn status(&mut self, id: u8, bytes: &[u8]) {
            let mut packet = vec![255, 255, id, u8::try_from(bytes.len() + 2).unwrap(), 0];
            packet.extend(bytes);
            packet.push(checksum(&packet[2..]));
            self.rx.extend(packet);
        }
        fn value(&mut self, id: u8, address: u8) -> u16 {
            let slot = usize::from(id);
            match address {
                0 => 3,
                1 => {
                    if self.scenario == 9 {
                        11
                    } else {
                        10
                    }
                }
                3 => 777,
                18 => 12,
                33 => u16::from(id >= 7),
                40 => u16::from(self.torque[usize::from(id)]),
                42 => {
                    if self.scenario == 4 {
                        0
                    } else {
                        self.goal.lock().unwrap()[usize::from(id)]
                    }
                }
                46 => self.velocity[usize::from(id)],
                56 => self.position[usize::from(id)],
                58 => {
                    let goal = self.velocity[slot];
                    if goal != 0 {
                        if self.scenario == 7 {
                            0
                        } else if self.scenario == 11 && id == self.ids[8] {
                            goal ^ 0x8000
                        } else {
                            goal
                        }
                    } else if self.previous_goal[slot] != 0 {
                        self.zero_samples[slot] += 1;
                        if self.zero_samples[slot] == 1
                            || self.scenario == 5
                            || (self.scenario == 14 && id == self.ids[8])
                        {
                            (self.previous_goal[slot] & 0x8000) | 250
                        } else {
                            0x8032
                        }
                    } else {
                        0
                    }
                }
                _ => 0,
            }
        }
    }
    impl SerialTransport for Sim {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), FeetechError> {
            self.clock.advance(DurationNs(20_000));
            let selected = usize::from(self.selected_motor_id);
            if self.scenario == 8
                && self.nonzero
                && bytes[4..7] == [131, 46, 2]
                && bytes[7..16].chunks_exact(3).all(|row| row[1..] == [0, 0])
                && self.torque[selected] == 1
            {
                return Err(FeetechError::TransportWrite(std::io::Error::other(
                    "固定零速失败",
                )));
            }
            if self.scenario == 16 && self.nonzero && bytes.get(4..7) == Some(&[131, 40, 1]) {
                self.clock.advance(DurationNs(600_000_000));
            }
            if self.scenario == 17
                && self.nonzero
                && bytes.len() == 17
                && bytes[4..7] == [131, 46, 2]
                && bytes[7..16].chunks_exact(3).all(|row| row[1..] == [0, 0])
            {
                self.clock.advance(DurationNs(60_000_000));
            }
            self.frames.lock().unwrap().push(bytes.to_vec());
            match bytes[4] {
                1 => self.status(bytes[2], &[]),
                2 => {
                    let value = self.value(bytes[2], bytes[5]).to_le_bytes();
                    self.status(bytes[2], &value[..usize::from(bytes[6])]);
                }
                3 => {
                    if bytes[5] == 40 {
                        self.torque[usize::from(bytes[2])] = bytes[6];
                    }
                    self.status(bytes[2], &[]);
                }
                130 => {
                    if bytes[5] == 56 && bytes[7..bytes.len() - 1] == self.ids && self.nonzero {
                        for &wheel in &self.ids[6..9] {
                            let slot = usize::from(wheel);
                            let mut sign =
                                decode_sign_magnitude(self.velocity[slot].into(), 15).signum();
                            if self.scenario == 12 && wheel == self.ids[8] {
                                sign = -sign;
                            }
                            if self.scenario == 13 && wheel == self.ids[8] {
                                sign = 0;
                            }
                            self.position[slot] = u16::try_from(
                                (i32::from(self.position[slot]) + sign * 40).rem_euclid(4096),
                            )
                            .unwrap();
                        }
                        if self.scenario == 15 {
                            self.position[1] = 2100;
                        }
                        if self.scenario == 6 {
                            let held = self.ids[6..]
                                .iter()
                                .copied()
                                .find(|id| *id != self.selected_motor_id)
                                .unwrap();
                            self.position[usize::from(held)] = 2052;
                        }
                    }
                    for id in &bytes[7..bytes.len() - 1] {
                        let value = self.value(*id, bytes[5]).to_le_bytes();
                        self.status(*id, &value[..usize::from(bytes[6])]);
                    }
                }
                131 => {
                    let width = usize::from(bytes[6]);
                    for row in bytes[7..bytes.len() - 1].chunks_exact(width + 1) {
                        let value = if width == 1 {
                            u16::from(row[1])
                        } else {
                            u16::from_le_bytes([row[1], row[2]])
                        };
                        match bytes[5] {
                            40 => self.torque[usize::from(row[0])] = u8::try_from(value).unwrap(),
                            42 => self.goal.lock().unwrap()[usize::from(row[0])] = value,
                            46 => {
                                let slot = usize::from(row[0]);
                                self.velocity[slot] = value;
                                if value != 0 {
                                    self.previous_goal[slot] = value;
                                }
                            }
                            _ => panic!("未授权寄存器写"),
                        }
                    }
                    if bytes[5] == 46 && self.velocity.iter().any(|speed| *speed != 0) {
                        self.nonzero = true;
                        if self.scenario == 1 {
                            interrupted(libc::SIGINT);
                        }
                        if self.scenario == 2 {
                            self.clock.advance(DurationNs(1_000_000_000));
                        }
                    }
                }
                _ => panic!("未支持指令"),
            }
            Ok(())
        }
        fn write_all_until(
            &mut self,
            bytes: &[u8],
            deadline: SourceWriteDeadline<'_>,
        ) -> Result<(), FeetechError> {
            if deadline.clock().now() >= deadline.source_deadline().as_mono_raw_ns() {
                return Err(FeetechError::Aborted(AbortReason::Timeout));
            }
            self.write_all(bytes)
        }
        fn read_exact(&mut self, out: &mut [u8]) -> Result<(), FeetechError> {
            self.clock.advance(DurationNs(20_000));
            if self.scenario == 10
                && self.rx.len() == 48
                && self.goal.lock().unwrap()[1] == 2047
                && !self.nonzero
            {
                self.clock.advance(DurationNs(201_000_000));
            }
            for byte in out {
                *byte = self.rx.pop_front().ok_or(FeetechError::TransportTimeout {
                    timeout_ms: 50,
                    wanted: 1,
                    got: 0,
                })?;
            }
            Ok(())
        }
        fn set_timeout(&mut self, _: Duration) -> Result<(), FeetechError> {
            Ok(())
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn discard_input(&mut self) -> Result<(), FeetechError> {
            Ok(())
        }
        fn drain_until_quiet(
            &mut self,
            _: Duration,
            _: Duration,
        ) -> Result<DrainReport, FeetechError> {
            unimplemented!("本固定动作不做恢复")
        }
    }

    fn arguments(selected_motor_id: &str, operation: &str) -> Vec<String> {
        [
            "feetech_poke",
            DEVICE,
            selected_motor_id,
            operation,
            "--arm-calibration",
            "/tmp/never-open-arm-calibration.json",
            "--go-expires-at-ns",
            "0",
            "--wheel-ids",
            "8,9,7",
        ]
        .map(str::to_string)
        .to_vec()
    }

    #[test]
    fn selected_wheel_and_legacy_alias_are_checked_before_uart() {
        for selected_motor_id in [7, 8, 9] {
            let args = arguments(&selected_motor_id.to_string(), "mixed-hold-wheel");
            assert_eq!(
                parse_selected_motor_id(&args, [8, 9, 7]).unwrap(),
                selected_motor_id
            );
            let mut report = empty_report(&args);
            bind_report(
                &mut report,
                &motor_ids([8, 9, 7]).unwrap(),
                selected_motor_id,
            );
            assert_eq!(report["schema"], "FEETECH_MIXED_WHEEL_V1");
            assert_eq!(report["operation"], "mixed-hold-wheel");
            assert_eq!(report["selected_motor_id"], selected_motor_id);
        }
        let args = arguments("9", "mixed-hold-wheel9");
        assert_eq!(parse_selected_motor_id(&args, [8, 9, 7]).unwrap(), 9);
        let mut report = empty_report(&args);
        bind_report(&mut report, &motor_ids([8, 9, 7]).unwrap(), 9);
        assert_eq!(report["schema"], "FEETECH_MIXED_WHEEL9_V1");
        assert_eq!(report["operation"], "mixed-hold-wheel9");
        assert!(report.get("selected_motor_id").is_none());
        for (selected_motor_id, operation) in [
            ("0", "mixed-hold-wheel"),
            ("6", "mixed-hold-wheel"),
            ("10", "mixed-hold-wheel"),
            ("254", "mixed-hold-wheel"),
            ("256", "mixed-hold-wheel"),
            ("-1", "mixed-hold-wheel"),
            ("x", "mixed-hold-wheel"),
            ("7", "mixed-hold-wheel9"),
            ("8", "mixed-hold-wheel9"),
            ("9", "mixed-hold-front8"),
        ] {
            let args = arguments(selected_motor_id, operation);
            let expected_error = parse_selected_motor_id(&args, [8, 9, 7])
                .expect_err("无效ID与旧alias不匹配必须拒绝");
            // 执行真实入口；标定路径不存在，必须先得到参数错误而非文件或UART错误。
            let report = execute(&args);
            assert_eq!(report["result"], "FAIL");
            assert_eq!(report["error"], expected_error);
            assert_eq!(report["cycle_count"], 0);
            assert_eq!(report["cleanup"]["zero_attempted"], false);
            assert_eq!(report["cleanup"]["disable_attempted"], false);
        }
        assert!(Guard::new(Arc::new(MockClock::default()), 0, [8, 9, 7], 6).is_err());
    }

    #[test]
    fn wheel_mapping_requires_three_unique_explicit_ids() {
        for value in ["", "7,8", "7,8,9,10", "7,7,9", "6,8,9", "7,8,x"] {
            assert!(parse_wheel_ids(value).is_err(), "{value}");
        }
        for wheel_ids in [[7, 8, 9], [9, 7, 8], [8, 9, 7]] {
            let text = wheel_ids.map(|id| id.to_string()).join(",");
            assert_eq!(parse_wheel_ids(&text).unwrap(), wheel_ids);
        }
    }

    #[test]
    fn calibration_tolerances_require_six_bounded_raw_values_or_default_to_zero() {
        assert_eq!(parse_calibration_range_tolerances_raw(&[]).unwrap(), [0; 6]);
        for values in ["0,20,7,4095,1,0", "4095,4095,4095,4095,4095,4095"] {
            let args = ["--calibration-range-tolerances-raw".into(), values.into()];
            let expected = values
                .split(',')
                .map(|value| value.parse::<u16>().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                parse_calibration_range_tolerances_raw(&args)
                    .unwrap()
                    .as_slice(),
                expected
            );
        }
        for values in [
            "",
            "0,0,0,0,0",
            "0,0,0,0,0,0,0",
            "0,0,0,0,0,4096",
            "0,0,0,0,0,65536",
            "0,0,0,0,0,-1",
            "0,0,0,0,0,x",
            "0,0,0,0,0,1.0",
        ] {
            let args = ["--calibration-range-tolerances-raw".into(), values.into()];
            assert!(
                parse_calibration_range_tolerances_raw(&args).is_err(),
                "{values}"
            );
        }
        for args in [
            vec!["--calibration-range-tolerances-raw".into()],
            vec![
                "--calibration-range-tolerance-raw".into(),
                "0,0,0,0,0,0".into(),
            ],
            vec![
                "--calibration-range-tolerances-raw".into(),
                "0,0,0,0,0,0".into(),
                "extra".into(),
            ],
        ] {
            assert!(parse_calibration_range_tolerances_raw(&args).is_err());
        }
    }

    #[test]
    fn mixed_failure_before_run_retains_zero_cycles_and_no_effects() {
        let error = Guard::new(Arc::new(MockClock::default()), 0, [8, 9, 7], 9)
            .err()
            .expect("过期GO在run和串口打开之前拒绝");
        let mut report = empty_report(&[]);
        report["error"] = json!(error);
        let encoded = serde_json::to_string(&report).unwrap();
        let decoded: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded["result"], "FAIL");
        assert_eq!(decoded["error"], "GO已过期");
        assert_eq!(decoded["cycle_count"], 0);
        assert_eq!(
            decoded["calibration_range_tolerances_raw"],
            json!([0, 0, 0, 0, 0, 0])
        );
        assert!(decoded["cycles"].as_array().unwrap().is_empty());
        assert!(
            decoded["timing"]
                .as_object()
                .unwrap()
                .values()
                .all(Value::is_null)
        );
        assert_eq!(
            decoded["cleanup"],
            json!({"zero_attempted":false,"zero_ok":false,
            "disable_attempted":false,"disable_ok":false,"torque_enable_after":null})
        );
    }

    #[test]
    fn mixed_directions_verify_wire_feedback_and_stops() {
        let _sim_lock = SIM_LOCK.lock().unwrap();
        for (operation, name, goals, cycles) in [
            ("mixed-hold-direction", "forward", [0, -500, 500], 400),
            ("mixed-hold-direction", "backward", [0, 500, -500], 400),
            ("mixed-hold-direction", "turn_left", [500, 500, 500], 400),
            (
                "mixed-hold-direction",
                "turn_right",
                [-500, -500, -500],
                400,
            ),
            ("mixed-ground-forward", "forward", [0, -500, 500], 20),
        ] {
            for scenario in [0, 2, 5, 11, 12, 13, 14, 15, 16, 17] {
                if scenario == 17 && operation != "mixed-ground-forward" {
                    continue;
                }
                INTERRUPTED.store(false, Ordering::SeqCst);
                let wheel_ids = [8, 9, 7];
                let ids = motor_ids(wheel_ids).unwrap();
                let args = arguments(name, operation);
                let motion = parse_motion(&args, wheel_ids).unwrap();
                let clock = Arc::new(MockClock::new(MonoRawNs(1_000_000)));
                let guard = Arc::new(
                    Guard::new(
                        Arc::new(AdvancingClock(clock.clone())),
                        Guard::unix_now().unwrap() + 600_000_000_000,
                        wheel_ids,
                        motion,
                    )
                    .unwrap(),
                );
                let frames = Arc::new(Mutex::new(Vec::new()));
                let sim = Sim {
                    ids,
                    selected_motor_id: wheel_ids[2],
                    clock,
                    rx: VecDeque::new(),
                    frames: frames.clone(),
                    goal: Arc::new(Mutex::new([0; 10])),
                    torque: [0; 10],
                    position: [2047; 10],
                    velocity: [0; 10],
                    scenario,
                    nonzero: false,
                    previous_goal: [0; 10],
                    zero_samples: [0; 10],
                };
                let calibrations: Vec<_> = ids
                    .into_iter()
                    .map(|id| MotorCalibration::new(id, 0, 0, 0, 4095).unwrap())
                    .collect();
                let mut report = empty_report(&args);
                let result = run(sim, &guard, &calibrations, [20; 6], &mut report);
                assert_eq!(
                    result.is_ok(),
                    scenario == 0,
                    "{operation} {name} {scenario}: {result:?}"
                );
                assert_eq!(report["cleanup"]["zero_ok"], true);
                assert_eq!(report["cleanup"]["disable_ok"], true);
                let frames = frames.lock().unwrap();
                let last_write = frames.iter().rfind(|f| matches!(f[4], 3 | 131)).unwrap();
                assert_eq!(last_write[4..7], [131, 40, 1]);
                assert_eq!(
                    last_write[7..25],
                    ids.into_iter().flat_map(|id| [id, 0]).collect::<Vec<_>>()
                );
                if scenario == 16 {
                    assert_eq!(
                        result.err().as_deref(),
                        Some("首次TE1至全九TE0超出本次启用预算")
                    );
                }
                if scenario == 17 {
                    assert!(report["timing"]["zero_started_ns"].is_u64());
                    assert_eq!(report["zero_samples"], json!([]));
                    assert_eq!(report["cycles"].as_array().unwrap().len(), 20);
                }
                if scenario != 0 {
                    assert_eq!(frames.last(), Some(last_write));
                    continue;
                }
                let pulse = u64::try_from(cycles).unwrap() * PERIOD;
                let budget = pulse + 1_000_000_000;
                assert_eq!(report["pulse_duration_ns"], pulse);
                assert_eq!(report["enable_budget_ns"], budget);
                assert_eq!(report["operation"], operation);
                assert_eq!(report["cycle_count"], cycles);
                let timing = &report["timing"];
                let actual_pulse = timing["zero_started_ns"].as_u64().unwrap()
                    - timing["first_nonzero_ns"].as_u64().unwrap();
                assert!((pulse..pulse + PERIOD).contains(&actual_pulse));
                assert!(
                    timing["disable_finished_ns"].as_u64().unwrap()
                        - timing["enable_attempt_ns"].as_u64().unwrap()
                        <= budget
                );
                assert_eq!(report["wheel_raw_goal"], json!(goals));
                assert_eq!(
                    report["wheel_samples"].as_array().unwrap().len(),
                    cycles / 5 - 1
                );
                assert_eq!(report["zero_samples"].as_array().unwrap().len(), 3);
                assert_eq!(
                    report["zero_samples"][2]["present_velocity_word"],
                    json!(goals.map(|g| if g == 0 { 0 } else { 0x8032 }))
                );
                let goal_frame: Vec<_> = wheel_ids
                    .iter()
                    .zip(goals)
                    .flat_map(|(&id, raw)| {
                        let word = u16::try_from(encode_sign_magnitude(raw, 15).unwrap())
                            .unwrap()
                            .to_le_bytes();
                        [id, word[0], word[1]]
                    })
                    .collect();
                assert_eq!(
                    frames
                        .iter()
                        .filter(|frame| frame.len() == 17
                            && frame[4..7] == [131, 46, 2]
                            && frame[7..16] == goal_frame)
                        .count(),
                    cycles
                );
                if let Some(directory) = std::env::var_os("ROBOT_MIXED_DIRECTION_FIXTURE_DIR") {
                    report["result"] = json!("PASS");
                    report["arm_calibration_path"] =
                        json!("/tmp/mixed-software-arm-calibration.json");
                    report["go_expires_at_unix_ns"] = json!(guard.expires_unix);
                    report["loaded_calibration"] = json!(ids[..6].iter().map(|id| json!({"id":id,"range_min":0,"range_max":4095,"homing_offset":0,"drive_mode":0})).collect::<Vec<_>>());
                    let fixture = if operation == "mixed-ground-forward" {
                        "ground_forward"
                    } else {
                        name
                    };
                    std::fs::write(
                        Path::new(&directory).join(format!("{fixture}.json")),
                        serde_json::to_vec_pretty(&report).unwrap(),
                    )
                    .unwrap();
                }
            }
        }
        assert!(parse_motion(&arguments("unknown", "mixed-hold-direction"), [8, 9, 7]).is_err());
        for invalid in ["backward", "turn_left", "turn_right", "9"] {
            let report = execute(&arguments(invalid, "mixed-ground-forward"));
            assert_eq!(report["error"], "本次落地短距维护仅允许前进");
            assert!(report["timing"]["enable_attempt_ns"].is_null());
        }
    }

    #[test]
    fn fixed_mixed_maintenance_runs_full_clocked_path_and_failures_keep_independent_stop() {
        let _sim_lock = SIM_LOCK.lock().unwrap();
        assert!(Guard::new(Arc::new(MockClock::default()), 0, [8, 9, 7], 9).is_err());
        for (wheel_ids, selected_motor_id, scenario) in [
            [8, 9, 7],
            [8, 7, 9],
            [9, 8, 7],
            [9, 7, 8],
            [7, 8, 9],
            [7, 9, 8],
        ]
        .into_iter()
        .flat_map(|ids| {
            (7..=9)
                .flat_map(move |selected| (0..=10).map(move |scenario| (ids, selected, scenario)))
        }) {
            let ids = motor_ids(wheel_ids).unwrap();
            INTERRUPTED.store(false, Ordering::SeqCst);
            let clock = Arc::new(MockClock::new(MonoRawNs(1_000_000)));
            let checked_clock: Arc<dyn RobotClock> = if scenario == 0 {
                Arc::new(AdvancingClock(clock.clone()))
            } else {
                clock.clone()
            };
            let guard = Arc::new(
                Guard::new(
                    checked_clock,
                    Guard::unix_now().unwrap() + 600_000_000_000,
                    wheel_ids,
                    selected_motor_id,
                )
                .unwrap(),
            );
            let frames = Arc::new(Mutex::new(Vec::new()));
            let mut position = [2047; 10];
            if scenario == 3 {
                position[6] = 1371;
            }
            let sim = Sim {
                ids,
                selected_motor_id,
                clock: clock.clone(),
                rx: VecDeque::new(),
                frames: frames.clone(),
                goal: Arc::new(Mutex::new([0; 10])),
                torque: [0; 10],
                position,
                velocity: [0; 10],
                scenario,
                nonzero: false,
                previous_goal: [0; 10],
                zero_samples: [0; 10],
            };
            let mut cals = ids
                .into_iter()
                .map(|id| MotorCalibration::new(id, 0, 0, 0, 4095).unwrap())
                .collect::<Vec<_>>();
            if scenario == 3 {
                cals[5].range_min = 1384;
            }
            let args = arguments(
                &selected_motor_id.to_string(),
                if selected_motor_id == 9 {
                    "mixed-hold-wheel9"
                } else {
                    "mixed-hold-wheel"
                },
            );
            let mut report = empty_report(&args);
            let result = run(sim, &guard, &cals, [0; 6], &mut report);
            assert_eq!(
                report["calibration_range_tolerances_raw"],
                json!([0, 0, 0, 0, 0, 0])
            );
            assert_eq!(
                result.is_ok(),
                scenario == 0,
                "selected={selected_motor_id} scenario={scenario} {result:?}"
            );
            if scenario == 3 {
                assert_eq!(
                    result.as_ref().err().map(String::as_str),
                    Some("invalid parameter: feetech calibration: motor=6, raw=1371")
                );
                assert_eq!(report["cycle_count"], 0);
                assert!(report["timing"]["enable_attempt_ns"].is_null());
                assert!(report["cleanup"]["torque_enable_after"].is_null());
            }
            let frames = frames.lock().unwrap();
            let stop = frames
                .iter()
                .rposition(|f| f.len() == 26 && f[4..7] == [131, 40, 1])
                .expect("独立完整九TE0");
            assert_eq!(
                frames[stop][7..25],
                ids.into_iter().flat_map(|id| [id, 0]).collect::<Vec<_>>()
            );
            if scenario == 0 {
                assert_eq!(report["wheel_ids"], json!(wheel_ids));
                if selected_motor_id == 9 {
                    assert!(report.get("selected_motor_id").is_none());
                    assert_eq!(report["schema"], "FEETECH_MIXED_WHEEL9_V1");
                } else {
                    assert_eq!(report["selected_motor_id"], selected_motor_id);
                    assert_eq!(report["schema"], "FEETECH_MIXED_WHEEL_V1");
                }
                assert_eq!(
                    report["wheel_raw_goal"],
                    json!(wheel_ids.map(|id| if id == selected_motor_id { 500 } else { 0 }))
                );
                let velocity_frames = frames
                    .iter()
                    .filter(|frame| frame.len() == 17 && frame[4..7] == [131, 46, 2])
                    .collect::<Vec<_>>();
                assert!(velocity_frames.iter().any(|frame| {
                    frame[7..16]
                        == wheel_ids
                            .into_iter()
                            .flat_map(|id| {
                                [
                                    id,
                                    if id == selected_motor_id { 244 } else { 0 },
                                    u8::from(id == selected_motor_id),
                                ]
                            })
                            .collect::<Vec<_>>()
                }));
                assert!(velocity_frames.iter().all(|frame| {
                    frame[7..16]
                        == wheel_ids
                            .into_iter()
                            .flat_map(|id| {
                                [
                                    id,
                                    if id == selected_motor_id { 244 } else { 0 },
                                    u8::from(id == selected_motor_id),
                                ]
                            })
                            .collect::<Vec<_>>()
                        || frame[7..16]
                            == wheel_ids
                                .into_iter()
                                .flat_map(|id| [id, 0, 0])
                                .collect::<Vec<_>>()
                }));
                let actual_first = report["timing"]["first_nonzero_ns"].as_u64().unwrap();
                assert!(report["cycles"][0]["started_ns"].as_u64().unwrap() < actual_first);
                assert_eq!(
                    report["cycles"][1]["source_deadline_ns"],
                    actual_first + 2 * PERIOD
                );
                assert!(
                    report["timing"]["zero_started_ns"].as_u64().unwrap() >= actual_first + PULSE
                );
                let fixture_variable = match selected_motor_id {
                    8 => "ROBOT_MIXED_FRONT_SOFTWARE_FIXTURE",
                    9 => "ROBOT_MIXED_SOFTWARE_FIXTURE",
                    _ => "",
                };
                if let Some(path) =
                    std::env::var_os(fixture_variable).filter(|_| wheel_ids == [8, 9, 7])
                {
                    report["result"] = json!("PASS");
                    report["serial_device"] = json!(DEVICE);
                    report["arm_calibration_path"] =
                        json!("/tmp/mixed-software-arm-calibration.json");
                    report["go_expires_at_unix_ns"] = json!(guard.expires_unix);
                    report["loaded_calibration"] = json!(ids[..6].iter().map(|id| json!({"id":id,"range_min":0,"range_max":4095,"homing_offset":0,"drive_mode":0})).collect::<Vec<_>>());
                    std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
                }
                assert_eq!(report["cycles"].as_array().unwrap().len(), 400);
                assert_eq!(report["wheel_samples"].as_array().unwrap().len(), 79);
                assert_eq!(report["zero_samples"][2]["present_velocity_word"], 0x8032);
                assert_eq!(report["arm_anchors"][5]["raw"], 2047);
                assert_eq!(report["arm_anchors"][5]["goal_raw"], 2047);
                assert_eq!(report["cleanup"]["disable_ok"], true);
                let enable = report["timing"]["enable_attempt_ns"].as_u64().unwrap();
                assert!(
                    report["timing"]["disable_finished_ns"].as_u64().unwrap() - enable
                        <= PULSE + ENABLE_MARGIN
                );
            } else {
                assert!(report["arm_final"].as_array().unwrap().is_empty());
                assert_eq!(stop, frames.len() - 1, "失败后不得补读或重试:{scenario}");
                if matches!(scenario, 3 | 4 | 9 | 10) {
                    assert!(!frames.iter().any(|f| f.len() == 8 && f[4..7] == [3, 40, 1]));
                    if !matches!(scenario, 4 | 10) {
                        assert!(frames.iter().filter(|f| f[4] == 131).all(|f| f[5] == 40));
                    }
                }
            }
        }
        INTERRUPTED.store(false, Ordering::SeqCst);
    }

    #[test]
    fn mixed_calibration_tolerances_preserve_actual_anchor_and_goal_raw() {
        let _sim_lock = SIM_LOCK.lock().unwrap();
        for (tolerances, second_max, rejected_motor) in [
            ([0; 6], 4095, Some(3)),
            ([20; 6], 4095, None),
            ([0, 20, 7, 0, 0, 0], 2030, None),
            ([0, 7, 20, 0, 0, 0], 2030, Some(2)),
        ] {
            INTERRUPTED.store(false, Ordering::SeqCst);
            let wheel_ids = [8, 9, 7];
            let ids = motor_ids(wheel_ids).unwrap();
            let clock = Arc::new(MockClock::new(MonoRawNs(1_000_000)));
            let guard = Arc::new(
                Guard::new(
                    Arc::new(AdvancingClock(clock.clone())),
                    Guard::unix_now().unwrap() + 600_000_000_000,
                    wheel_ids,
                    9,
                )
                .unwrap(),
            );
            let frames = Arc::new(Mutex::new(Vec::new()));
            let goals = Arc::new(Mutex::new([0; 10]));
            let mut position = [2047; 10];
            position[3] = 2992;
            let sim = Sim {
                ids,
                selected_motor_id: 9,
                clock,
                rx: VecDeque::new(),
                frames: frames.clone(),
                goal: goals.clone(),
                torque: [0; 10],
                position,
                velocity: [0; 10],
                scenario: 0,
                nonzero: false,
                previous_goal: [0; 10],
                zero_samples: [0; 10],
            };
            let mut calibrations = ids
                .into_iter()
                .map(|id| MotorCalibration::new(id, 0, 0, 0, 4095).unwrap())
                .collect::<Vec<_>>();
            calibrations[1].range_max = second_max;
            calibrations[2].range_max = 2991;
            let mut report = empty_report(&arguments("9", "mixed-hold-wheel9"));
            let result = run(sim, &guard, &calibrations, tolerances, &mut report);
            assert_eq!(
                report["calibration_range_tolerances_raw"],
                json!(tolerances)
            );
            assert_eq!(
                result.is_ok(),
                rejected_motor.is_none(),
                "{tolerances:?}: {result:?}"
            );
            let frames = frames.lock().unwrap();
            if let Some(motor) = rejected_motor {
                let raw = if motor == 3 { 2992 } else { 2047 };
                assert_eq!(
                    result.unwrap_err(),
                    format!("invalid parameter: feetech calibration: motor={motor}, raw={raw}")
                );
                assert_eq!(report["cycle_count"], 0);
                assert!(report["arm_anchors"].as_array().unwrap().is_empty());
                assert!(
                    frames
                        .iter()
                        .filter(|frame| frame[4] == 131)
                        .all(|frame| frame[5] == 40)
                );
                continue;
            }
            assert_eq!(goals.lock().unwrap()[3], 2992);
            assert_eq!(report["arm_anchors"][2]["raw"], 2992);
            assert_eq!(report["arm_anchors"][2]["goal_raw"], 2992);
            assert_eq!(report["arm_anchors"][2]["calibration_min"], 0);
            assert_eq!(report["arm_anchors"][2]["calibration_max"], 2991);
            assert_eq!(report["arm_anchors"][2].as_object().unwrap().len(), 6);
            assert_eq!(report["arm_final"][2]["raw"], 2992);
            assert_eq!(report["cycle_count"], 400);
            assert_eq!(report["cleanup"]["disable_ok"], true);
            assert_eq!(
                report["cleanup"]["torque_enable_after"],
                json!([0, 0, 0, 0, 0, 0, 0, 0, 0])
            );
            assert!(
                frames
                    .iter()
                    .filter(|frame| frame.len() >= 8 && frame[4..7] == [131, 42, 2])
                    .all(|frame| {
                        frame[7..frame.len() - 1]
                            .chunks_exact(3)
                            .any(|row| row[0] == 3 && u16::from_le_bytes([row[1], row[2]]) == 2992)
                    })
            );
            if let Some(path) = std::env::var_os("ROBOT_MIXED_TOLERANCE_SOFTWARE_FIXTURE")
                .filter(|_| tolerances == [20; 6])
            {
                report["result"] = json!("PASS");
                report["serial_device"] = json!(DEVICE);
                report["arm_calibration_path"] = json!("/tmp/mixed-software-arm-calibration.json");
                report["go_expires_at_unix_ns"] = json!(guard.expires_unix);
                report["loaded_calibration"] = json!(calibrations[..6].iter().map(|cal| json!({"id":cal.id,"range_min":cal.range_min,"range_max":cal.range_max,"homing_offset":cal.homing_offset,"drive_mode":cal.drive_mode})).collect::<Vec<_>>());
                std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
            }
        }
        INTERRUPTED.store(false, Ordering::SeqCst);
    }
}
