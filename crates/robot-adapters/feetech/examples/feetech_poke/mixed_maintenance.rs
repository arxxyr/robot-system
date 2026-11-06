//! 固定右总线维护动作；不接默认daemon，不修改模式、PID或EEPROM。

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use robot_adapter_feetech::adapter::FeetechAdapterConfig;
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
use serde_json::{Value, json};

const DEVICE: &str = "/dev/serial/by-id/usb-1a86_USB_Single_Serial_5A7C122340-if00";
const IDS: [u8; 9] = [1, 2, 3, 4, 5, 6, 8, 7, 9];
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
const ENABLE: u64 = 21_000_000_000;
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

/// Unix GO只在入口映射一次；所有内部期限使用同一RobotClock，不把父MONOTONIC当RAW。
struct Guard {
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
    fn new(clock: Arc<dyn RobotClock>, expires_unix: u64) -> Result<Self, String> {
        let unix = Self::unix_now()?;
        let remaining = expires_unix
            .checked_sub(unix)
            .filter(|r| *r > 0)
            .ok_or("GO已过期")?;
        let now = clock.now().0;
        let hard_deadline = now
            .checked_add(remaining.min(28_000_000_000))
            .ok_or("GO本地期限溢出")?;
        Ok(Self {
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
                && bytes[7..16] == [8, 0, 0, 7, 0, 0, 9, 0, 0];
            let off = bytes.len() == 26
                && bytes[2..7] == [254, 22, 131, 40, 1]
                && IDS
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
                    now.saturating_add(ENABLE - 100_000_000)
                        .min(self.guard.deadline.load(Ordering::SeqCst)),
                );
            }
        }
        if bytes.len() == 17
            && bytes[4..7] == [131, 46, 2]
            && bytes[7..16] == [8, 0, 0, 7, 0, 0, 9, 244, 1]
        {
            let _ = self.guard.nonzero_attempt.compare_exchange(
                0,
                self.guard.clock.now().0,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
        self.pv_response = bytes.len() == 17 && bytes[4..7] == [130, 58, 2] && bytes[7..16] == IDS;
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
                if status.id != IDS[index] || status.error != 0 || status.params.len() != 2 {
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

fn profile(role: WheelRole) -> Sts3215RawWheelProfile {
    Sts3215RawWheelProfile::try_bind(
        WheelObservation {
            motor_id: role.motor_id(),
            model: 777,
            mode: 1,
            phase: 12,
            firmware: Some(FirmwareVersion {
                major: 3,
                minor: 10,
            }),
        },
        role,
    )
    .unwrap()
}
fn read_raw<T: SerialTransport>(
    adapter: &mut FeetechAdapter<T>,
    guard: &Guard,
    register: &'static str,
) -> Result<u32, String> {
    guard.check().map_err(|e| format!("读取中止:{e:?}"))?;
    adapter.bus().set_op_class(BusOpClass::PeriodicRead);
    let mut raw = [0];
    adapter
        .bus()
        .sync_read_by_id_raw_into(register, &[9], &mut raw, &mut [0; 16], &mut [0; 8])
        .map_err(|e| e.to_string())?;
    guard.check().map_err(|e| format!("读取返回过期:{e:?}"))?;
    Ok(raw[0])
}
fn write_cycle<T: SerialTransport + 'static>(
    adapter: &mut FeetechAdapter<T>,
    guard: &Guard,
    anchors: [i32; 6],
    raw: i16,
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
    command.raw_velocities[8] = raw;
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
    report: &mut Value,
) -> Result<(), String> {
    let motors: HashMap<_, _> = NAMES
        .iter()
        .zip(IDS)
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
        adapter_name: "mixed-right9-maintenance",
        joint_names: NAMES.iter().map(ToString::to_string).collect(),
        calibrations: calibrations.to_owned(),
    };
    let mut adapter = FeetechAdapter::new_bus2_mixed_raw(
        config,
        bus,
        [
            profile(WheelRole::Front),
            profile(WheelRole::Left),
            profile(WheelRole::Right),
        ],
    )
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
            report["arm_anchors"].as_array_mut().unwrap().push(json!({"motor_id":IDS[index],"raw":anchors[index],"goal_raw":goals[index],"sampled_ns":sampled_ns,"calibration_min":calibrations[index].range_min,"calibration_max":calibrations[index].range_max}));
        }
        let mut wheel_anchors = [0; 2];
        let wheel_sampled = guard.clock.now().0;
        adapter.bus().set_op_class(BusOpClass::PeriodicRead);
        adapter
            .bus()
            .sync_read_by_id_into("Present_Position", &[8, 7], &mut wheel_anchors)
            .map_err(|e| e.to_string())?;
        for (id, raw) in [8, 7].into_iter().zip(wheel_anchors) {
            if !(0..=4095).contains(&raw) {
                return Err("未驱动轮基线超界".into());
            }
            report["wheel_anchors"]
                .as_array_mut()
                .unwrap()
                .push(json!({"motor_id":id,"raw":raw,"sampled_ns":wheel_sampled}));
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
        let mut previous: Option<i32> = None;
        let mut travel = 0;
        let mut positive_pv = false;
        for cycle in 0..400_u64 {
            let scheduled = first + cycle * PERIOD;
            let deadline = if cycle == 0 {
                (scheduled + PERIOD).min(sampled_ns + 200_000_000)
            } else {
                scheduled + PERIOD
            };
            guard.set_deadline(
                deadline.min(guard.enable_attempt.load(Ordering::SeqCst) + ENABLE - 100_000_000),
            );
            guard.wait(scheduled)?;
            let started = guard.clock.now().0;
            write_cycle(&mut adapter, guard, anchors, 500, started, deadline)?;
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
                if raw.iter().any(|r| !(0..=4095).contains(r)) || words[8] > 1000 {
                    return Err("运动反馈原值越界或方向异常".into());
                }
                for index in 0..2 {
                    if !matches!(words[index + 6], 0 | 50 | 0x8032)
                        || (raw[index + 6] - wheel_anchors[index]).abs() > 4
                    {
                        return Err("未驱动轮反馈超出原非目标范围".into());
                    }
                }
                positive_pv |= words[8] > 0;
                if let Some(last) = previous {
                    let step = (raw[8] - last).rem_euclid(4096);
                    if step > 512 {
                        return Err("右轮相邻位移越界或逆向".into());
                    }
                    travel += step;
                }
                previous = Some(raw[8]);
            }
            let finished = guard.clock.now().0;
            report["cycles"].as_array_mut().unwrap().push(
                json!({"started_ns":started,"finished_ns":finished,"source_deadline_ns":deadline}),
            );
            guard.check().map_err(|e| format!("周期完成过期:{e:?}"))?;
        }
        guard.set_deadline(
            (guard.enable_attempt.load(Ordering::SeqCst) + ENABLE - 100_000_000)
                .min(guard.hard_deadline),
        );
        guard.wait(first.checked_add(PULSE).ok_or("20秒期限溢出")?)?;
        if travel <= 0 || !positive_pv {
            return Err("右轮20秒未得到正向位置变化".into());
        }
        let zero_started = guard.clock.now().0;
        report["timing"]["zero_started_ns"] = json!(zero_started);
        let zero_deadline = (zero_started + 500_000_000)
            .min(guard.enable_attempt.load(Ordering::SeqCst) + ENABLE - 100_000_000);
        guard.set_deadline(zero_deadline);
        adapter.bus().set_op_class(BusOpClass::PeriodicWrite);
        adapter
            .bus()
            .sync_write_by_id("Goal_Velocity", &[(8, 0), (7, 0), (9, 0)])
            .map_err(|e| e.to_string())?;
        let zero_finished = guard.clock.now().0;
        report["timing"]["zero_finished_ns"] = json!(zero_finished);
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
            let te = read_raw(&mut adapter, guard, "Torque_Enable")?;
            let gv = read_raw(&mut adapter, guard, "Goal_Velocity")?;
            let pv = read_raw(&mut adapter, guard, "Present_Velocity")?;
            report["zero_samples"].as_array_mut().unwrap().push(json!({"started_ns":started,"finished_ns":guard.clock.now().0,"torque_enable":te,"goal_velocity_word":gv,"present_velocity_word":pv}));
            if te != 1
                || gv != 0
                || (index < 2 && pv > 1000 && pv != 0x8032)
                || (index == 2 && !matches!(pv, 0 | 50 | 0x8032))
            {
                return Err("TE1零速500ms读回不满足既有合同".into());
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
        let zero = adapter
            .bus()
            .sync_write_by_id("Goal_Velocity", &[(8, 0), (7, 0), (9, 0)]);
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
                .is_none_or(|elapsed| elapsed > ENABLE)
        {
            error.get_or_insert_with(|| "首次TE1至全九TE0超过21秒".into());
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
                    &IDS[..6],
                    &mut final_raw,
                ) {
                    Ok(()) => {
                        for (index, raw) in final_raw.into_iter().enumerate() {
                            report["arm_final"]
                                .as_array_mut()
                                .unwrap()
                                .push(json!({"motor_id":IDS[index],"raw":raw}));
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

fn empty_report(args: &[String]) -> Value {
    json!({"schema":"FEETECH_MIXED_RIGHT9_V1","operation":"mixed-hold-right9","result":"FAIL","error":null,"serial_device":args.get(1),"arm_calibration_path":args.get(5),"go_expires_at_unix_ns":null,"motor_ids":IDS,"wheel_ids":[8,7,9],"wheel_raw_goal":[0,0,500],"pulse_duration_ns":PULSE,"zero_window_ns":500_000_000_u64,"enable_budget_ns":ENABLE,"loaded_calibration":[],"arm_anchors":[],"arm_final":[],"cycle_count":0,"cycles":[],"wheel_anchors":[],"wheel_samples":[],"zero_samples":[],"timing":{"enable_attempt_ns":null,"first_nonzero_ns":null,"zero_started_ns":null,"zero_finished_ns":null,"disable_attempt_ns":null,"disable_finished_ns":null,"finished_ns":null},"cleanup":{"zero_attempted":false,"zero_ok":false,"disable_attempted":false,"disable_ok":false,"torque_enable_after":null}})
}

pub(super) fn main(args: &[String]) -> ExitCode {
    let mut report = empty_report(args);
    let result = (|| {
        if args.len() != 8
            || args[1] != DEVICE
            || args[2] != "9"
            || args[4] != "--arm-calibration"
            || args[6] != "--go-expires-at-ns"
        {
            return Err("固定mixed维护参数不匹配".into());
        }
        let expiry = args[7].parse::<u64>().map_err(|_| "GO Unix期限解析失败")?;
        report["go_expires_at_unix_ns"] = json!(expiry);
        let mut loaded =
            robot_adapter_feetech::load_file(Path::new(&args[5])).map_err(|e| e.to_string())?;
        if loaded.len() != 6 {
            return Err("右臂标定必须恰为固定六电机".into());
        }
        let mut calibrations = Vec::with_capacity(9);
        for (name, id) in NAMES[..6].iter().zip(&IDS[..6]) {
            let cal = loaded.remove(*name).ok_or("缺少固定右臂标定")?;
            if cal.id != *id {
                return Err("右臂标定ID不匹配".into());
            }
            report["loaded_calibration"].as_array_mut().unwrap().push(json!({"id":cal.id,"range_min":cal.range_min,"range_max":cal.range_max,"homing_offset":cal.homing_offset,"drive_mode":cal.drive_mode}));
            calibrations.push(cal);
        }
        for id in &IDS[6..] {
            calibrations
                .push(MotorCalibration::new(*id, 0, 0, 0, 4095).map_err(|e| e.to_string())?);
        }
        let _signals = Signals::install()?;
        let guard = Arc::new(Guard::new(Arc::new(SystemClock::new()), expiry)?);
        guard
            .check()
            .map_err(|e| format!("UART前授权检查拒绝:{e:?}"))?;
        let transport = robot_adapter_feetech::transport::real::RealSerialTransport::open(
            Path::new(DEVICE),
            DEFAULT_BAUDRATE,
        )
        .map_err(|e| e.to_string())?;
        run(transport, &guard, &calibrations, &mut report)
    })();
    match result {
        Ok(()) => {
            report["result"] = json!("PASS");
        }
        Err(error) => {
            report["error"] = json!(error);
        }
    }
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
        clock: Arc<MockClock>,
        rx: VecDeque<u8>,
        frames: Arc<Mutex<Vec<Vec<u8>>>>,
        goal: [u16; 10],
        torque: [u8; 10],
        position: [u16; 10],
        velocity: [u16; 10],
        scenario: u8,
        nonzero: bool,
        zero_samples: usize,
    }
    impl Sim {
        fn status(&mut self, id: u8, bytes: &[u8]) {
            let mut packet = vec![255, 255, id, u8::try_from(bytes.len() + 2).unwrap(), 0];
            packet.extend(bytes);
            packet.push(checksum(&packet[2..]));
            self.rx.extend(packet);
        }
        fn value(&mut self, id: u8, address: u8) -> u16 {
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
                        self.goal[usize::from(id)]
                    }
                }
                46 => self.velocity[usize::from(id)],
                56 => self.position[usize::from(id)],
                58 => {
                    if id == 9 && self.velocity[9] == 500 {
                        if self.scenario == 7 { 0 } else { 500 }
                    } else if id == 9 && self.nonzero {
                        self.zero_samples += 1;
                        if self.zero_samples == 1 {
                            250
                        } else if self.scenario == 5 {
                            250
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
            if self.scenario == 8
                && self.nonzero
                && bytes[4..7] == [131, 46, 2]
                && bytes[15] == 0
                && self.torque[9] == 1
            {
                return Err(FeetechError::TransportWrite(std::io::Error::other(
                    "固定零速失败",
                )));
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
                    if bytes[5] == 56
                        && bytes[7..bytes.len() - 1] == IDS
                        && self.nonzero
                        && self.velocity[9] == 500
                    {
                        self.position[9] = (self.position[9] + 40) % 4096;
                        if self.scenario == 6 {
                            self.position[8] = 2052;
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
                            42 => self.goal[usize::from(row[0])] = value,
                            46 => self.velocity[usize::from(row[0])] = value,
                            _ => panic!("未授权寄存器写"),
                        }
                    }
                    if bytes[5] == 46 && self.velocity[9] == 500 {
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
            if self.scenario == 10 && self.rx.len() == 48 && self.goal[1] == 2047 && !self.nonzero {
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

    #[test]
    fn mixed_failure_before_run_retains_zero_cycles_and_no_effects() {
        let error = Guard::new(Arc::new(MockClock::default()), 0)
            .err()
            .expect("过期GO在run和串口打开之前拒绝");
        let mut report = empty_report(&[]);
        report["error"] = json!(error);
        let encoded = serde_json::to_string(&report).unwrap();
        let decoded: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded["result"], "FAIL");
        assert_eq!(decoded["error"], "GO已过期");
        assert_eq!(decoded["cycle_count"], 0);
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
    fn fixed_mixed_maintenance_runs_full_clocked_path_and_failures_keep_independent_stop() {
        assert!(Guard::new(Arc::new(MockClock::default()), 0).is_err());
        for scenario in 0..=10 {
            INTERRUPTED.store(false, Ordering::SeqCst);
            let clock = Arc::new(MockClock::new(MonoRawNs(1_000_000)));
            let checked_clock: Arc<dyn RobotClock> = if scenario == 0 {
                Arc::new(AdvancingClock(clock.clone()))
            } else {
                clock.clone()
            };
            let guard = Arc::new(
                Guard::new(checked_clock, Guard::unix_now().unwrap() + 600_000_000_000).unwrap(),
            );
            let frames = Arc::new(Mutex::new(Vec::new()));
            let mut position = [2047; 10];
            if scenario == 3 {
                position[6] = 1371;
            }
            let sim = Sim {
                clock: clock.clone(),
                rx: VecDeque::new(),
                frames: frames.clone(),
                goal: [0; 10],
                torque: [0; 10],
                position,
                velocity: [0; 10],
                scenario,
                nonzero: false,
                zero_samples: 0,
            };
            let mut cals = IDS
                .into_iter()
                .map(|id| MotorCalibration::new(id, 0, 0, 0, 4095).unwrap())
                .collect::<Vec<_>>();
            if scenario == 3 {
                cals[5].range_min = 1384;
            }
            let mut report = empty_report(&[]);
            let result = run(sim, &guard, &cals, &mut report);
            assert_eq!(
                result.is_ok(),
                scenario == 0,
                "scenario={scenario} {result:?}"
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
                IDS.into_iter().flat_map(|id| [id, 0]).collect::<Vec<_>>()
            );
            if scenario == 0 {
                let actual_first = report["timing"]["first_nonzero_ns"].as_u64().unwrap();
                assert!(report["cycles"][0]["started_ns"].as_u64().unwrap() < actual_first);
                assert_eq!(
                    report["cycles"][1]["source_deadline_ns"],
                    actual_first + 2 * PERIOD
                );
                assert!(
                    report["timing"]["zero_started_ns"].as_u64().unwrap() >= actual_first + PULSE
                );
                if let Some(path) = std::env::var_os("ROBOT_MIXED_SOFTWARE_FIXTURE") {
                    report["result"] = json!("PASS");
                    report["serial_device"] = json!(DEVICE);
                    report["arm_calibration_path"] =
                        json!("/tmp/mixed-software-arm-calibration.json");
                    report["go_expires_at_unix_ns"] = json!(guard.expires_unix);
                    report["loaded_calibration"] = json!(IDS[..6].iter().map(|id| json!({"id":id,"range_min":0,"range_max":4095,"homing_offset":0,"drive_mode":0})).collect::<Vec<_>>());
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
                    report["timing"]["disable_finished_ns"].as_u64().unwrap() - enable <= ENABLE
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
}
