//! 显式右总线混合布局；由配置选择既有factory分支，不执行电机模式写入。

use super::{FeetechAdapter, FeetechAdapterConfig, effective_calibration_range_raw};
use crate::bus::FeetechBus;
use crate::calibration_io::clamp_raw;
use crate::convert::{rad_to_raw, rad_to_tick_offset, raw_to_rad};
use crate::transport::{SerialTransport, SourceWriteDeadline};
use crate::velocity_profile::{
    FirmwareVersion, Sts3215RawWheelProfile, Sts3215WheelProfile, WheelObservation, WheelRole,
};
use robot_core::adapter::{ControlMode, JointCommandView, JointSelection};
use robot_core::diag::DiagEvent;
use robot_core::error::RobotError;
use robot_core::serial_services::BusOpClass;
use robot_core::time::{Deadline, MonoRawNs};

const JOINTS: [&str; 9] = [
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

const ZERO_OBSERVATION_INTERVAL_NS: u64 = 20_000_000;

#[derive(Clone, Copy)]
enum Profiles {
    Nominal([Sts3215WheelProfile; 3]),
    ObservedRaw([Sts3215RawWheelProfile; 3]),
}
impl Profiles {
    fn role(self, index: usize) -> WheelRole {
        match self {
            Self::Nominal(p) => p[index].contract().role,
            Self::ObservedRaw(p) => p[index].role(),
        }
    }
    fn motor_id(self, index: usize) -> u8 {
        match self {
            Self::Nominal(p) => p[index].motor_id(),
            Self::ObservedRaw(p) => p[index].motor_id(),
        }
    }
    fn accepts(self, index: usize, raw: i16) -> bool {
        match self {
            Self::Nominal(p) => {
                raw != i16::MIN && raw.unsigned_abs() <= p[index].contract().raw_limit
            }
            Self::ObservedRaw(p) => p[index].accepts(raw),
        }
    }
    fn verify(self, index: usize, observation: WheelObservation) -> bool {
        match self {
            Self::Nominal(p) => {
                Sts3215WheelProfile::try_bind(observation, p[index].contract()).is_ok()
            }
            Self::ObservedRaw(p) => {
                Sts3215RawWheelProfile::try_bind(observation, p[index].role(), p[index].motor_id())
                    .is_ok()
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct MixedBus2 {
    pub(super) arm_ids: [u8; 6],
    pub(super) wheel_ids: [u8; 3],
    arm_slots: [usize; 6],
    wheel_slots: [usize; 3],
    position_mask: u32,
    velocity_mask: u32,
    profiles: Profiles,
}

impl MixedBus2 {
    fn bind(names: &[String], ids: &[u8], profiles: Profiles) -> Result<Self, RobotError> {
        if names.len() != 9 || ids.len() != 9 {
            return Err(RobotError::InvalidParameter("混合右总线需要完整九关节"));
        }
        let mut slots = [0; 9];
        let mut seen = 0_u16;
        let mut seen_ids = 0_u16;
        for (slot, (name, id)) in names.iter().zip(ids).enumerate() {
            let Some(logical) = JOINTS
                .iter()
                .position(|&expected_name| name == expected_name)
            else {
                return Err(RobotError::InvalidParameter("混合右总线名称与电机ID不匹配"));
            };
            if (logical < 6 && usize::from(*id) != logical + 1)
                || (logical >= 6 && !(7..=9).contains(id))
            {
                return Err(RobotError::InvalidParameter("混合右总线名称与电机ID不匹配"));
            }
            if seen & (1 << logical) != 0 || seen_ids & (1 << id) != 0 {
                return Err(RobotError::InvalidParameter("混合右总线存在重复关节"));
            }
            seen |= 1 << logical;
            seen_ids |= 1 << id;
            slots[logical] = slot;
        }
        let wheel_ids = [slots[6], slots[7], slots[8]].map(|slot| ids[slot]);
        for (index, role) in [WheelRole::Front, WheelRole::Left, WheelRole::Right]
            .into_iter()
            .enumerate()
        {
            if profiles.role(index) != role || profiles.motor_id(index) != wheel_ids[index] {
                return Err(RobotError::InvalidParameter(
                    "三轮profile必须按前、左、右角色绑定配置电机ID",
                ));
            }
        }
        Ok(Self {
            arm_ids: [1, 2, 3, 4, 5, 6],
            wheel_ids,
            arm_slots: slots[..6].try_into().unwrap(),
            wheel_slots: slots[6..].try_into().unwrap(),
            position_mask: slots[..6].iter().fold(0, |mask, slot| mask | (1 << slot)),
            velocity_mask: slots[6..].iter().fold(0, |mask, slot| mask | (1 << slot)),
            profiles,
        })
    }
}

/// 只按配置关节角色取ID；随后仍由connect核对型号、模式、TE、固件及Phase。
#[cfg(feature = "real-hw")]
pub(crate) fn raw_profiles_for_config(
    names: &[String],
    ids: &[u8],
) -> Result<[Sts3215RawWheelProfile; 3], RobotError> {
    let profile = |index: usize, role| {
        let motor_id = names
            .iter()
            .zip(ids)
            .find_map(|(name, &id)| (name == JOINTS[index + 6]).then_some(id))
            .ok_or(RobotError::InvalidParameter("混合右总线缺少轮关节"))?;
        Sts3215RawWheelProfile::expected(role, motor_id)
            .map_err(|_| RobotError::InvalidParameter("配置轮电机ID必须在7到9内"))
    };
    let profiles = [
        profile(0, WheelRole::Front)?,
        profile(1, WheelRole::Left)?,
        profile(2, WheelRole::Right)?,
    ];
    MixedBus2::bind(names, ids, Profiles::ObservedRaw(profiles))?;
    Ok(profiles)
}

/// factory在打开串口前复用；原显式构造仍独立校验同一前提。
pub(crate) fn validate_raw_mixed_calibrations(
    calibrations: &[crate::types::MotorCalibration],
) -> Result<(), RobotError> {
    if calibrations.len() != 9
        || calibrations.iter().any(|cal| {
            cal.range_min > cal.range_max
                || cal.range_max > 4095
                || cal.homing_offset.unsigned_abs() > 2047
        })
    {
        return Err(RobotError::InvalidParameter("实证raw混合总线需要完整标定"));
    }
    Ok(())
}

impl<T: SerialTransport> FeetechAdapter<T> {
    /// 显式软件入口；只复用一条总线连接，默认 factory 不调用。
    /// profile 顺序固定为前、左、右，电机ID取自配置；raw 目标不进行 SI 换算。
    /// # Errors
    /// 原构造错误、九关节映射或profile角色错误。Phase12无法构造合法profile。
    pub fn new_bus2_mixed(
        config: FeetechAdapterConfig,
        bus: FeetechBus<T>,
        profiles: [Sts3215WheelProfile; 3],
    ) -> Result<Self, RobotError> {
        let mut adapter = Self::new(config, bus).map_err(RobotError::from)?;
        adapter.mixed_bus2 = Some(MixedBus2::bind(
            &adapter.joint_names,
            &adapter.joint_ids,
            Profiles::Nominal(profiles),
        )?);
        Ok(adapter)
    }

    /// 仅现场实证的0/+500合同；不提供SI速度，factory须通过显式配置选择。
    /// # Errors
    /// 完整布局、标定、逐轮角色或原构造前提不符时拒绝。
    pub fn new_bus2_mixed_raw(
        config: FeetechAdapterConfig,
        bus: FeetechBus<T>,
        profiles: [Sts3215RawWheelProfile; 3],
    ) -> Result<Self, RobotError> {
        validate_raw_mixed_calibrations(&config.calibrations)?;
        let mut adapter = Self::new(config, bus).map_err(RobotError::from)?;
        adapter.mixed_bus2 = Some(MixedBus2::bind(
            &adapter.joint_names,
            &adapter.joint_ids,
            Profiles::ObservedRaw(profiles),
        )?);
        Ok(adapter)
    }

    /// 返回本次enable防飞同步真正采用的六臂原始锚点及采样时刻，不是持续反馈保持。
    #[must_use]
    pub const fn mixed_arm_anchor(&self) -> Option<([i32; 6], u64)> {
        self.mixed_arm_anchor
    }

    /// 防飞同步实际GP读回，仅本次enable成功后供维护摘要使用。
    #[must_use]
    pub fn mixed_arm_goal_readback(&self) -> Option<[i32; 6]> {
        self.mixed_arm_anchor
            .map(|_| self.goal_raw[..6].try_into().unwrap())
    }

    pub(super) fn validate_raw_mixed_anchor(
        &self,
        sampled_ns: u64,
    ) -> Result<Option<([i32; 6], u64)>, crate::FeetechError> {
        let Some(layout) = self
            .mixed_bus2
            .filter(|l| matches!(l.profiles, Profiles::ObservedRaw(_)))
        else {
            return Ok(None);
        };
        for (index, &slot) in layout.arm_slots.iter().enumerate() {
            let raw = self.pos_raw[index];
            let cal = &self.calibrations[slot];
            let (min, max) =
                effective_calibration_range_raw(cal, self.calibration_range_tolerances_raw[slot]);
            if cal.range_min > cal.range_max
                || cal.range_max > 4095
                || !(min..=max).contains(&raw)
                || !(0..=4095).contains(&raw)
                || clamp_raw(raw, min, max) != raw
                || rad_to_raw(raw_to_rad(raw)) != raw
            {
                return Err(crate::FeetechError::CalibrationAnchorChanged {
                    motor_id: layout.arm_ids[index],
                    raw,
                });
            }
        }
        Ok(Some((self.pos_raw[..6].try_into().unwrap(), sampled_ns)))
    }

    pub(super) fn raw_mixed(&self) -> bool {
        self.mixed_bus2
            .is_some_and(|layout| matches!(layout.profiles, Profiles::ObservedRaw(_)))
    }

    /// 只记录设备原字，不把零目标、记录成功或丢记录当作停止/权限事实。
    pub(super) fn observe_raw_mixed_zero(&mut self) -> Result<(), crate::FeetechError> {
        if !self.raw_mixed() || !self.mixed_zero_target_confirmed || self.diag.is_none() {
            return Ok(());
        }
        let started = self.clock.now().0;
        if self.mixed_zero_observation_last_ns.is_some_and(|last| {
            started
                .checked_sub(last)
                .is_none_or(|elapsed| elapsed < ZERO_OBSERVATION_INTERVAL_NS)
        }) {
            return Ok(());
        }
        self.mixed_zero_observation_last_ns = Some(started);
        let Some(layout) = self.mixed_bus2 else {
            return Ok(());
        };
        let ids = layout.wheel_ids;
        let mut tx = [0_u8; 11];
        let mut rx = [0_u8; 24];
        let mut words = [[0_u32; 3]; 3];
        // 沿调用方的 PeriodicRead 执行权与预算，不逐读续期。
        for (register, output) in ["Torque_Enable", "Goal_Velocity", "Present_Velocity"]
            .into_iter()
            .zip(&mut words)
        {
            self.bus
                .sync_read_by_id_raw_into(register, &ids, output, &mut tx, &mut rx)?;
        }
        let mut torque = [0_u8; 3];
        let mut goals = [0_u16; 3];
        let mut velocities = [0_u16; 3];
        for index in 0..3 {
            let range_error = |value, max_bytes, max| crate::FeetechError::ValueExceedsByteRange {
                value,
                max_bytes,
                max,
            };
            torque[index] = u8::try_from(words[0][index])
                .map_err(|_| range_error(words[0][index], 1, u32::from(u8::MAX)))?;
            goals[index] = u16::try_from(words[1][index])
                .map_err(|_| range_error(words[1][index], 2, u32::from(u16::MAX)))?;
            velocities[index] = u16::try_from(words[2][index])
                .map_err(|_| range_error(words[2][index], 2, u32::from(u16::MAX)))?;
        }
        if let Some(reason) = self
            .preempt
            .as_ref()
            .and_then(|probe| probe.should_abort(BusOpClass::PeriodicRead))
        {
            return Err(crate::FeetechError::Aborted(reason));
        }
        let finished = self.clock.now().0;
        if let Some(diag) = self.diag.as_ref() {
            diag.record(DiagEvent {
                stamp_ns: started,
                site: 31,
                kind: 0,
                a: 3,
                b: 0,
                seq_lo: 0,
            });
            for index in 0..3 {
                diag.record(DiagEvent {
                    stamp_ns: finished,
                    site: 31,
                    kind: 1,
                    a: ids[index],
                    b: torque[index],
                    seq_lo: u32::from(goals[index]) | (u32::from(velocities[index]) << 16),
                });
            }
        }
        Ok(())
    }

    pub(super) fn verify_mixed_bus2(&mut self) -> Result<(), RobotError> {
        let Some(layout) = self.mixed_bus2 else {
            return Ok(());
        };
        for (slot, name) in self.joint_names.iter().enumerate() {
            let expected = i32::from(JointSelection::Mask(layout.velocity_mask).contains(slot));
            if self
                .bus
                .read("Operating_Mode", name)
                .map_err(RobotError::from)?
                != expected
                || self
                    .bus
                    .read("Torque_Enable", name)
                    .map_err(RobotError::from)?
                    != 0
            {
                return Err(RobotError::InvalidParameter("混合总线模式或断扭状态不匹配"));
            }
        }
        for index in 0..3 {
            let name = &self.joint_names[layout.wheel_slots[index]];
            let mut read_byte = |register| {
                u8::try_from(self.bus.read(register, name).map_err(RobotError::from)?)
                    .map_err(|_| RobotError::InvalidParameter("轮固件原始字节越界"))
            };
            let observation = WheelObservation {
                motor_id: layout.wheel_ids[index],
                // connect(true) 已逐ID PING+型号读取并严格核对777。
                model: crate::registers::MODEL_NUMBER_STS3215,
                mode: 1,
                phase: read_byte("Phase")?,
                firmware: Some(FirmwareVersion {
                    major: read_byte("Firmware_Major_Version")?,
                    minor: read_byte("Firmware_Minor_Version")?,
                }),
            };
            if !layout.profiles.verify(index, observation) {
                return Err(RobotError::InvalidParameter(
                    "轮固件或Phase不符合显式profile",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn write_mixed_bus2(
        &mut self,
        command: &JointCommandView<'_>,
    ) -> Result<(), RobotError> {
        let layout = self.mixed_bus2.unwrap();
        if command.mode == ControlMode::Stop && command.selection == JointSelection::All {
            self.mixed_zero_target_confirmed = false;
            return Ok(());
        }
        command.validate_selection(9)?;
        if command.selection
            != (JointSelection::MixedPositionRawVelocity {
                position_mask: layout.position_mask,
                velocity_mask: layout.velocity_mask,
            })
        {
            return Err(RobotError::InvalidParameter(
                "混合目标必须匹配已绑定右总线布局",
            ));
        }
        let started = self.clock.now();
        command.validate_selection_time(started.0)?;
        let deadline = command.deadline_ns.min(
            started
                .0
                .checked_add(self.command_budget_ns)
                .ok_or(RobotError::InvalidParameter("混合写预算溢出"))?,
        );
        let mut positions = [(0, 0); 6];
        let mut velocities = [(0, 0); 3];
        // 全部参数与表示范围先验证；raw值只在原bus编码层转一次符号幅度word。
        for (index, &slot) in layout.arm_slots.iter().enumerate() {
            let raw_float = rad_to_tick_offset(command.positions[slot]).round() + 2047.0;
            if !raw_float.is_finite() || !(-32767.0..=32767.0).contains(&raw_float) {
                return Err(RobotError::InvalidParameter("混合位置超出寄存器表示范围"));
            }
            let mut raw = rad_to_raw(command.positions[slot]);
            if let Some(cal) = self.calibrations.get(slot) {
                if cal.range_min > cal.range_max || cal.range_max > 4095 {
                    return Err(RobotError::InvalidParameter("混合位置原标定范围无效"));
                }
                let (min, max) = effective_calibration_range_raw(
                    cal,
                    self.calibration_range_tolerances_raw[slot],
                );
                let clamped = clamp_raw(raw, min, max);
                if raw != clamped {
                    return Err(RobotError::InvalidParameter("混合目标不允许标定限幅"));
                }
                raw = clamped;
            }
            if !(-32767..=32767).contains(&raw) {
                return Err(RobotError::InvalidParameter(
                    "混合位置标定结果超出寄存器表示范围",
                ));
            }
            positions[index] = (layout.arm_ids[index], raw);
        }
        for (index, &slot) in layout.wheel_slots.iter().enumerate() {
            let raw = command.raw_velocities[slot];
            if !layout.profiles.accepts(index, raw) {
                return Err(RobotError::InvalidParameter("混合轮raw速度超过显式上限"));
            }
            velocities[index] = (layout.wheel_ids[index], i32::from(raw));
        }
        self.bus.set_op_class(BusOpClass::PeriodicWrite);
        let source = SourceWriteDeadline::new(self.clock.as_ref(), Deadline(MonoRawNs(deadline)));
        let mut frame = [0; 256];
        // 从首次可能副作用起旧零目标不再可信；部分写失败不得恢复。
        self.mixed_zero_target_confirmed = false;
        // 先轮速后位置：有界反馈保持时先写零轮速；两帧不是原子事务。
        self.bus
            .sync_write_by_id_into_until("Goal_Velocity", &velocities, &mut frame, source)
            .map_err(RobotError::from)?;
        self.bus
            .sync_write_by_id_into_until("Goal_Position", &positions, &mut frame, source)
            .map_err(RobotError::from)?;
        self.mixed_zero_target_confirmed =
            self.raw_mixed() && velocities.iter().all(|&(_, raw)| raw == 0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::tests::{ack_status, read_status};
    use crate::protocol::checksum;
    use crate::transport::{MockHandle, MockTransport};
    use crate::types::Motor;
    use crate::velocity_profile::WheelContract;
    use robot_core::adapter::{
        ControlModeRepr, JointCommandBuf, JointStateBuf, ManipulatorAdapter,
    };
    use robot_core::control_authority::{
        AuthorityInit, ControlAuthority, EnableResult, InhibitSet, NormalIoIntent,
    };
    use robot_core::executor_ctrl::{OutputSafetyProbe, OutputSafetyState};
    use robot_core::serial_services::{
        AbortReason, BusLease, CommErrorKind, LeasedPreemptProbe, PreemptProbe,
    };
    use robot_core::time::{DurationNs, MockClock, RobotClock};
    use std::collections::HashMap;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    };
    use std::time::Duration;

    struct Probe(ControlAuthority);
    impl Probe {
        fn new() -> Self {
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
            Self(authority)
        }
    }
    impl PreemptProbe for Probe {
        fn should_abort(&self, _: BusOpClass) -> Option<AbortReason> {
            None
        }
        fn checkpoint(&self) {}
        fn sleep_or_abort(&self, _: BusOpClass, _: DurationNs) -> Result<(), AbortReason> {
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
            if let Some(special) = BusLease::special(class) {
                return Ok(special);
            }
            let lease = self
                .0
                .acquire_normal_io(match class {
                    BusOpClass::PeriodicRead => NormalIoIntent::PeriodicRead,
                    _ => NormalIoIntent::Motion,
                })
                .map_err(|_| AbortReason::Revoked)?;
            Ok(BusLease::with_normal(class, lease).unwrap())
        }
        fn finish_leased(&self, lease: BusLease) {
            lease.release(&self.0).unwrap();
        }
    }

    /// 只在既有内存串口外注入两帧之间的故障；编码与实际字节均走原总线。
    struct Port {
        inner: MockTransport,
        clock: Arc<MockClock>,
        fail: u8,
        calls: Arc<AtomicUsize>,
        read_advance_ns: Arc<AtomicU64>,
    }
    impl SerialTransport for Port {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), crate::error::FeetechError> {
            self.inner.write_all(bytes)
        }
        fn write_all_until(
            &mut self,
            bytes: &[u8],
            deadline: SourceWriteDeadline<'_>,
        ) -> Result<(), crate::error::FeetechError> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail == 2 && call == 1 {
                return Err(crate::error::FeetechError::TransportWrite(
                    std::io::Error::other("第二帧确定性失败"),
                ));
            }
            self.inner.write_all_until(bytes, deadline)?;
            if self.fail == 1 && call == 0 {
                self.clock.set(deadline.source_deadline().as_mono_raw_ns());
            }
            Ok(())
        }
        fn read_exact(&mut self, out: &mut [u8]) -> Result<(), crate::error::FeetechError> {
            let result = self.inner.read_exact(out);
            self.clock
                .advance(DurationNs(self.read_advance_ns.load(Ordering::Relaxed)));
            result
        }
        fn set_timeout(&mut self, duration: Duration) -> Result<(), crate::error::FeetechError> {
            self.inner.set_timeout(duration)
        }
        fn is_connected(&self) -> bool {
            self.inner.is_connected()
        }
        fn discard_input(&mut self) -> Result<(), crate::error::FeetechError> {
            self.inner.discard_input()
        }
        fn drain_until_quiet(
            &mut self,
            quiet: Duration,
            budget: Duration,
        ) -> Result<crate::transport::DrainReport, crate::error::FeetechError> {
            self.inner.drain_until_quiet(quiet, budget)
        }
    }

    const FIXTURE_WHEEL_IDS: [u8; 3] = [8, 9, 7];

    fn profiles(wheel_ids: [u8; 3]) -> [Sts3215WheelProfile; 3] {
        std::array::from_fn(|index| {
            let role = [WheelRole::Front, WheelRole::Left, WheelRole::Right][index];
            let motor_id = wheel_ids[index];
            let firmware = FirmwareVersion {
                major: 3,
                minor: 10,
            };
            Sts3215WheelProfile::try_bind(
                WheelObservation {
                    motor_id,
                    model: 777,
                    mode: 1,
                    phase: 4,
                    firmware: Some(firmware),
                },
                WheelContract {
                    role,
                    motor_id,
                    firmware,
                    phase: 4,
                    raw_limit: 500,
                },
            )
            .unwrap()
        })
    }

    fn raw_profiles(wheel_ids: [u8; 3]) -> [Sts3215RawWheelProfile; 3] {
        std::array::from_fn(|index| {
            let role = [WheelRole::Front, WheelRole::Left, WheelRole::Right][index];
            let motor_id = wheel_ids[index];
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
        })
    }

    struct Fixture {
        adapter: FeetechAdapter<Port>,
        handle: MockHandle,
        clock: Arc<MockClock>,
        calls: Arc<AtomicUsize>,
        ids: [u8; 9],
        read_advance_ns: Arc<AtomicU64>,
    }
    impl Fixture {
        fn new(
            ids: [u8; 9],
            fail: u8,
            phase: u8,
            bad_mode: bool,
        ) -> (Self, Result<(), RobotError>) {
            Self::with_profile(ids, fail, phase, bad_mode, false)
        }
        fn with_profile(
            ids: [u8; 9],
            fail: u8,
            phase: u8,
            bad_mode: bool,
            raw: bool,
        ) -> (Self, Result<(), RobotError>) {
            Self::with_mapping(ids, fail, phase, bad_mode, raw, FIXTURE_WHEEL_IDS)
        }

        #[allow(
            clippy::too_many_lines,
            reason = "保持完整内存串口握手夹具，避免拆分连接时序"
        )]
        fn with_mapping(
            ids: [u8; 9],
            fail: u8,
            phase: u8,
            bad_mode: bool,
            raw: bool,
            wheel_ids: [u8; 3],
        ) -> (Self, Result<(), RobotError>) {
            let names: Vec<String> = ids
                .iter()
                .map(|id| {
                    let logical = if *id <= 6 {
                        usize::from(*id) - 1
                    } else {
                        6 + wheel_ids
                            .iter()
                            .position(|wheel_id| wheel_id == id)
                            .unwrap()
                    };
                    JOINTS[logical].to_owned()
                })
                .collect();
            let motors = names
                .iter()
                .zip(ids)
                .map(|(name, id)| (name.clone(), Motor::new(id).unwrap()))
                .collect::<HashMap<_, _>>();
            let (inner, handle) = MockTransport::new();
            let clock = Arc::new(MockClock::default());
            let calls = Arc::new(AtomicUsize::new(0));
            let read_advance_ns = Arc::new(AtomicU64::new(0));
            let bus = FeetechBus::new(
                Port {
                    inner,
                    clock: clock.clone(),
                    fail,
                    calls: calls.clone(),
                    read_advance_ns: read_advance_ns.clone(),
                },
                motors,
            )
            .unwrap();
            let config = FeetechAdapterConfig {
                clock: clock.clone(),
                preempt: None,
                diag: None,
                call_deadline: Duration::from_millis(50),
                adapter_name: "mixed_test",
                joint_names: names,
                calibration_range_tolerances_raw: Vec::new(),
                calibrations: if raw {
                    ids.into_iter()
                        .map(|id| crate::MotorCalibration::new(id, 0, 0, 0, 4095).unwrap())
                        .collect()
                } else {
                    Vec::new()
                },
            };
            let mut adapter = if raw {
                FeetechAdapter::new_bus2_mixed_raw(config, bus, raw_profiles(wheel_ids))
            } else {
                FeetechAdapter::new_bus2_mixed(config, bus, profiles(wheel_ids))
            }
            .unwrap();
            for id in 1..=9 {
                handle.push_rx(&ack_status(id));
                handle.push_rx(&read_status(id, &[9, 3]));
            }
            for id in ids {
                let mode = u8::from(id >= 7) ^ u8::from(bad_mode && id == 1);
                handle.push_rx(&read_status(id, &[mode]));
                handle.push_rx(&read_status(id, &[0]));
                if bad_mode && id == 1 {
                    break;
                }
            }
            if !bad_mode {
                for id in wheel_ids {
                    for byte in [phase, 3, 10] {
                        handle.push_rx(&read_status(id, &[byte]));
                    }
                }
            }
            let connected = adapter.connect();
            if connected.is_ok() {
                let probe = Arc::new(Probe::new());
                adapter.preempt = Some(probe.clone());
                adapter.bus.set_services(Some(probe), None);
            }
            (
                Self {
                    adapter,
                    handle,
                    clock,
                    calls,
                    ids,
                    read_advance_ns,
                },
                connected,
            )
        }
        fn command(&self) -> JointCommandBuf {
            let layout = self.adapter.mixed_bus2.unwrap();
            let mut command = JointCommandBuf {
                mode: ControlModeRepr::Position,
                joint_count: 9,
                stamp_ns: self.clock.now().0,
                deadline_ns: self.clock.now().0 + 1_000_000,
                selection: JointSelection::MixedPositionRawVelocity {
                    position_mask: layout.position_mask,
                    velocity_mask: layout.velocity_mask,
                },
                ..JointCommandBuf::default()
            };
            for (slot, raw) in layout.wheel_slots.into_iter().zip([500, -500, 0]) {
                command.raw_velocities[slot] = raw;
            }
            command
        }

        fn write_zero(&mut self) {
            let mut command = self.command();
            command.raw_velocities.fill(0);
            self.adapter.write_command(&command.as_view()).unwrap();
        }

        fn queue_state(&self) {
            for word in [2047_u16, 0] {
                for id in self.ids {
                    self.handle.push_rx(&read_status(id, &word.to_le_bytes()));
                }
            }
        }

        fn queue_observation(&self, words: [[u16; 3]; 3]) {
            let ids = self.adapter.mixed_bus2.unwrap().wheel_ids;
            for (register, row) in words.into_iter().enumerate() {
                for (id, word) in ids.into_iter().zip(row) {
                    let bytes = word.to_le_bytes();
                    self.handle.push_rx(&read_status(
                        id,
                        &bytes[..if register == 0 { 1 } else { 2 }],
                    ));
                }
            }
        }
    }

    #[derive(Default)]
    struct Diagnostics(Mutex<Vec<DiagEvent>>);

    impl robot_core::diag::DiagSink for Diagnostics {
        fn record(&self, event: DiagEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn attach_diagnostics(fixture: &mut Fixture) -> Arc<Diagnostics> {
        let diagnostics = Arc::new(Diagnostics::default());
        fixture.adapter.diag = Some(diagnostics.clone());
        diagnostics
    }

    struct ObservationProbe {
        inner: Probe,
        checkpoints: AtomicUsize,
        abort_at: usize,
    }

    impl PreemptProbe for ObservationProbe {
        fn should_abort(&self, class: BusOpClass) -> Option<AbortReason> {
            assert_eq!(class, BusOpClass::PeriodicRead);
            (self.checkpoints.load(Ordering::Relaxed) >= self.abort_at)
                .then_some(AbortReason::Revoked)
        }
        fn checkpoint(&self) {
            self.checkpoints.fetch_add(1, Ordering::Relaxed);
        }
        fn sleep_or_abort(&self, class: BusOpClass, _: DurationNs) -> Result<(), AbortReason> {
            self.should_abort(class).map_or(Ok(()), Err)
        }
        fn operation_class(&self) -> BusOpClass {
            BusOpClass::PeriodicRead
        }
        fn confirmation_trusted(&self) -> bool {
            true
        }
        fn report_comm_error(&self, _: CommErrorKind) {}
    }

    impl LeasedPreemptProbe for ObservationProbe {
        fn begin_leased(&self, class: BusOpClass) -> Result<BusLease, AbortReason> {
            self.should_abort(class)
                .map_or_else(|| self.inner.begin_leased(class), Err)
        }
        fn finish_leased(&self, lease: BusLease) {
            self.inner.finish_leased(lease);
        }
    }
    fn frames(bytes: &[u8]) -> Vec<&[u8]> {
        let mut remaining = bytes;
        let mut output = Vec::new();
        while !remaining.is_empty() {
            let size = usize::from(remaining[3]) + 4;
            let frame = &remaining[..size];
            assert_eq!(&frame[..2], &[0xff, 0xff]);
            assert_eq!(checksum(&frame[2..size - 1]), frame[size - 1]);
            output.push(frame);
            remaining = &remaining[size..];
        }
        output
    }

    #[test]
    fn raw_zero_observation_reads_configured_ids_in_order_and_preserves_unsigned_words() {
        for wheel_ids in [
            [7, 8, 9],
            [7, 9, 8],
            [8, 7, 9],
            [8, 9, 7],
            [9, 7, 8],
            [9, 8, 7],
        ] {
            let ids = [wheel_ids[2], 3, 6, 1, wheel_ids[0], 4, 2, wheel_ids[1], 5];
            let (mut f, connected) = Fixture::with_mapping(ids, 0, 12, false, true, wheel_ids);
            connected.unwrap();
            let diagnostics = attach_diagnostics(&mut f);
            let _ = f.handle.drain_tx();
            f.queue_state();
            let mut state = JointStateBuf::default();
            f.adapter.read_state(&mut state).unwrap();
            assert_eq!(frames(&f.handle.drain_tx()).len(), 2);
            assert!(diagnostics.0.lock().unwrap().is_empty());

            f.write_zero();
            let _ = f.handle.drain_tx();
            f.read_advance_ns.store(2, Ordering::Relaxed);
            f.queue_state();
            // TE 是原字节，GV/PV 包括负零和符号位，不解码为 signed。
            let words = [
                [1, 0x80, 0xff],
                [0x8000, 0x8032, 0xffff],
                [0xffff, 0x8000, 0x8001],
            ];
            f.queue_observation(words);
            f.adapter.read_state(&mut state).unwrap();
            let bytes = f.handle.drain_tx();
            let requests = frames(&bytes);
            assert_eq!(requests.len(), 5);
            assert_eq!((requests[0][5], requests[1][5]), (56, 58));
            for (request, (address, width)) in requests[2..].iter().zip([(40, 1), (46, 2), (58, 2)])
            {
                assert_eq!((request[4], request[5], request[6]), (0x82, address, width));
                assert_eq!(&request[7..10], wheel_ids);
            }
            let events = diagnostics.0.lock().unwrap();
            assert_eq!(events.len(), 4);
            assert_eq!(
                events[0],
                DiagEvent {
                    stamp_ns: 4,
                    site: 31,
                    kind: 0,
                    a: 3,
                    b: 0,
                    seq_lo: 0,
                }
            );
            for index in 0..3 {
                assert_eq!(
                    events[index + 1],
                    DiagEvent {
                        stamp_ns: 10,
                        site: 31,
                        kind: 1,
                        a: wheel_ids[index],
                        b: u8::try_from(words[0][index]).unwrap(),
                        seq_lo: u32::from(words[1][index]) | (u32::from(words[2][index]) << 16),
                    }
                );
            }
            assert_eq!(&state.positions[..9], &[0.0; 9]);
            assert!(state.velocities[..9].iter().all(|value| value.is_nan()));
            assert!(state.torques[..9].iter().all(|value| value.is_nan()));
        }
    }

    #[test]
    fn raw_zero_observation_rate_limit_survives_repeated_zero_and_nonzero_commands() {
        let (mut f, connected) =
            Fixture::with_profile([1, 2, 3, 4, 5, 6, 7, 8, 9], 0, 12, false, true);
        connected.unwrap();
        let diagnostics = attach_diagnostics(&mut f);
        f.write_zero();
        f.queue_state();
        f.queue_observation([[1; 3], [0; 3], [0; 3]]);
        f.adapter.read_state(&mut JointStateBuf::default()).unwrap();
        assert_eq!(f.adapter.mixed_zero_observation_last_ns, Some(0));
        assert_eq!(diagnostics.0.lock().unwrap().len(), 4);
        for elapsed in [1, ZERO_OBSERVATION_INTERVAL_NS - 1] {
            f.clock.set(MonoRawNs(elapsed));
            let mut nonzero = f.command();
            nonzero.raw_velocities.fill(0);
            nonzero.raw_velocities[f.adapter.mixed_bus2.unwrap().wheel_slots[0]] = 500;
            f.adapter.write_command(&nonzero.as_view()).unwrap();
            assert!(!f.adapter.mixed_zero_target_confirmed);
            f.write_zero();
            let _ = f.handle.drain_tx();
            f.queue_state();
            f.adapter.read_state(&mut JointStateBuf::default()).unwrap();
            assert_eq!(frames(&f.handle.drain_tx()).len(), 2);
            assert_eq!(f.adapter.mixed_zero_observation_last_ns, Some(0));
            assert_eq!(diagnostics.0.lock().unwrap().len(), 4);
        }
        f.clock.set(MonoRawNs(ZERO_OBSERVATION_INTERVAL_NS));
        f.write_zero();
        let _ = f.handle.drain_tx();
        f.queue_state();
        f.queue_observation([[1; 3], [0; 3], [0; 3]]);
        f.adapter.read_state(&mut JointStateBuf::default()).unwrap();
        assert_eq!(frames(&f.handle.drain_tx()).len(), 5);
        assert_eq!(diagnostics.0.lock().unwrap().len(), 8);
    }

    #[test]
    fn raw_zero_target_fact_is_invalidated_by_partial_writes_and_lifecycle_operations() {
        for fail in [1, 2] {
            let (mut f, connected) =
                Fixture::with_profile([1, 2, 3, 4, 5, 6, 7, 8, 9], fail, 12, false, true);
            connected.unwrap();
            // 先避开夹具的一次性故障，真实写成完整零目标。
            f.calls.store(2, Ordering::Relaxed);
            f.write_zero();
            assert!(f.adapter.mixed_zero_target_confirmed);
            let mut invalid = f.command();
            invalid.raw_velocities.fill(0);
            invalid.raw_velocities[f.adapter.mixed_bus2.unwrap().wheel_slots[0]] = 501;
            assert!(f.adapter.write_command(&invalid.as_view()).is_err());
            assert!(
                f.adapter.mixed_zero_target_confirmed,
                "无副作用拒绝不改最近成功目标"
            );
            f.calls.store(0, Ordering::Relaxed);
            let mut zero = f.command();
            zero.raw_velocities.fill(0);
            assert!(f.adapter.write_command(&zero.as_view()).is_err());
            assert!(!f.adapter.mixed_zero_target_confirmed);
        }
        for operation in 0..8 {
            let (mut f, connected) =
                Fixture::with_profile([1, 2, 3, 4, 5, 6, 7, 8, 9], 0, 12, false, true);
            connected.unwrap();
            f.write_zero();
            assert!(f.adapter.mixed_zero_target_confirmed);
            match operation {
                0 => {
                    f.adapter.disconnect().unwrap();
                }
                1 => {
                    assert!(f.adapter.connect().is_err());
                }
                2 => {
                    // 即使 Enable 准备读失败，也不能继承上一轮零目标事实。
                    assert!(f.adapter.enable(true).is_err());
                }
                3 => {
                    for id in f.ids {
                        f.handle.push_rx(&ack_status(id));
                    }
                    f.adapter.enable(false).unwrap();
                }
                4 => f.adapter.safety_stop_fast().unwrap(),
                5 => f.adapter.safety_stop().unwrap(),
                6 => {
                    f.adapter
                        .write_command(&JointCommandBuf::default().as_view())
                        .unwrap();
                }
                _ => {
                    f.adapter.safety_stop_entries.clear();
                    assert!(f.adapter.safety_stop_fast().is_err());
                }
            }
            assert!(!f.adapter.mixed_zero_target_confirmed);
        }
    }

    #[test]
    fn raw_observation_failures_and_preemption_never_emit_a_success_group() {
        for failed_register in 0..3 {
            let (mut f, connected) =
                Fixture::with_profile([1, 2, 3, 4, 5, 6, 7, 8, 9], 0, 12, false, true);
            connected.unwrap();
            let diagnostics = attach_diagnostics(&mut f);
            f.write_zero();
            let _ = f.handle.drain_tx();
            f.queue_state();
            let ids = f.adapter.mixed_bus2.unwrap().wheel_ids;
            for register in 0..=failed_register {
                for (index, id) in ids.into_iter().enumerate() {
                    let mut response = read_status(id, if register == 0 { &[1] } else { &[0, 0] });
                    if register == failed_register && index == 2 {
                        response[4] = 0x20;
                        let last = response.len() - 1;
                        response[last] = checksum(&response[2..last]);
                    }
                    f.handle.push_rx(&response);
                }
            }
            assert!(matches!(
                f.adapter.read_state(&mut JointStateBuf::default()),
                Err(RobotError::HardwareFault("feetech motor status"))
            ));
            assert_eq!(frames(&f.handle.drain_tx()).len(), 3 + failed_register);
            let events = diagnostics.0.lock().unwrap();
            assert!(events.iter().all(|event| event.site != 31));
            assert!(events.iter().any(|event| event.site == 4
                && event.kind == 5
                && event.a == ids[2]
                && event.b == 0x20));
        }
        // 两个原状态事务后及每个新增事务后抢占，末读完成也不得发布成功组。
        for abort_at in [4, 6, 8, 10] {
            let (mut f, connected) =
                Fixture::with_profile([1, 2, 3, 4, 5, 6, 7, 8, 9], 0, 12, false, true);
            connected.unwrap();
            let diagnostics = attach_diagnostics(&mut f);
            f.write_zero();
            f.queue_state();
            f.queue_observation([[1; 3], [0; 3], [0; 3]]);
            let probe = Arc::new(ObservationProbe {
                inner: Probe::new(),
                checkpoints: AtomicUsize::new(0),
                abort_at,
            });
            f.adapter.preempt = Some(probe.clone());
            f.adapter.bus.set_services(Some(probe), None);
            assert_eq!(
                f.adapter.read_state(&mut JointStateBuf::default()),
                Err(RobotError::Aborted(AbortReason::Revoked))
            );
            let events = diagnostics.0.lock().unwrap();
            assert!(events.iter().all(|event| event.site != 31));
            assert!(
                events
                    .iter()
                    .any(|event| event.site == 4 && event.kind == 7)
            );
        }
    }

    #[test]
    fn nominal_mixed_and_raw_without_diagnostics_keep_the_original_two_reads() {
        for raw in [false, true] {
            let (mut f, connected) = Fixture::with_profile(
                [1, 2, 3, 4, 5, 6, 7, 8, 9],
                0,
                if raw { 12 } else { 4 },
                false,
                raw,
            );
            connected.unwrap();
            let diagnostics = (!raw).then(|| attach_diagnostics(&mut f));
            f.write_zero();
            let _ = f.handle.drain_tx();
            f.queue_state();
            f.adapter.read_state(&mut JointStateBuf::default()).unwrap();
            let bytes = f.handle.drain_tx();
            let requests = frames(&bytes);
            assert_eq!(requests.len(), 2);
            assert_eq!((requests[0][5], requests[1][5]), (56, 58));
            assert!(diagnostics.is_none_or(|sink| sink.0.lock().unwrap().is_empty()));
            assert_eq!(f.adapter.mixed_zero_observation_last_ns, None);
        }
    }

    #[test]
    fn incomplete_raw_observation_keeps_timeout_routing_without_any_success_record() {
        for completed_reads in 0..3 {
            let (mut f, connected) =
                Fixture::with_profile([1, 2, 3, 4, 5, 6, 7, 8, 9], 0, 12, false, true);
            connected.unwrap();
            let diagnostics = attach_diagnostics(&mut f);
            f.write_zero();
            let _ = f.handle.drain_tx();
            f.queue_state();
            for register in 0..completed_reads {
                for id in f.adapter.mixed_bus2.unwrap().wheel_ids {
                    f.handle
                        .push_rx(&read_status(id, if register == 0 { &[1] } else { &[0, 0] }));
                }
            }
            assert_eq!(
                f.adapter.read_state(&mut JointStateBuf::default()),
                Err(RobotError::Comm(CommErrorKind::Timeout))
            );
            assert_eq!(frames(&f.handle.drain_tx()).len(), 3 + completed_reads);
            let events = diagnostics.0.lock().unwrap();
            assert!(events.iter().all(|event| event.site != 31));
            assert!(
                events
                    .iter()
                    .any(|event| event.site == 4 && event.kind == 0)
            );
        }
    }

    #[test]
    fn mixed_layout_requires_profiles_to_match_the_configured_role_ids() {
        let mut names = [
            "right_arm_shoulder_pan",
            "right_arm_shoulder_lift",
            "right_arm_elbow_flex",
            "right_arm_wrist_flex",
            "right_arm_wrist_roll",
            "right_arm_gripper",
            "base_right_wheel",
            "base_front_wheel",
            "base_left_wheel",
        ]
        .map(str::to_owned);
        let ids = [1, 2, 3, 4, 5, 6, 7, 8, 9];
        let layout =
            MixedBus2::bind(&names, &ids, Profiles::Nominal(profiles(FIXTURE_WHEEL_IDS))).unwrap();
        assert_eq!(layout.wheel_ids, [8, 9, 7]);
        assert_eq!(layout.wheel_slots, [7, 8, 6]);
        names.swap(6, 8);
        assert!(
            MixedBus2::bind(&names, &ids, Profiles::Nominal(profiles(FIXTURE_WHEEL_IDS))).is_err()
        );
    }

    #[test]
    fn mixed_config_remapping_changes_wire_ids_for_all_wheel_permutations() {
        for wheel_ids in [
            [7, 8, 9],
            [7, 9, 8],
            [8, 7, 9],
            [8, 9, 7],
            [9, 7, 8],
            [9, 8, 7],
        ] {
            // 名称槽位不变，只改配置表内逐槽对应的ID。
            let ids = [1, 2, 3, 4, 5, 6, wheel_ids[2], wheel_ids[0], wheel_ids[1]];
            for raw in [false, true] {
                let phase = if raw { 12 } else { 4 };
                let (mut f, connected) =
                    Fixture::with_mapping(ids, 0, phase, false, raw, wheel_ids);
                connected.unwrap();
                assert_eq!(
                    &f.adapter.joint_names[6..],
                    &["base_right_wheel", "base_front_wheel", "base_left_wheel"]
                );
                let _ = f.handle.drain_tx();
                let mut command = f.command();
                let layout = f.adapter.mixed_bus2.unwrap();
                let goals = if raw { [0, 500, 0] } else { [500, -500, 0] };
                for (slot, goal) in layout.wheel_slots.into_iter().zip(goals) {
                    command.raw_velocities[slot] = goal;
                }
                f.adapter.write_command(&command.as_view()).unwrap();
                let bytes = f.handle.drain_tx();
                let output = frames(&bytes);
                assert_eq!(output.len(), 2);
                let [front, left, right] = wheel_ids;
                let expected = if raw {
                    [0x83, 46, 2, front, 0, 0, left, 0xf4, 1, right, 0, 0]
                } else {
                    [0x83, 46, 2, front, 0xf4, 1, left, 0xf4, 0x81, right, 0, 0]
                };
                assert_eq!(&output[0][4..16], &expected);
                assert_eq!(&output[1][4..7], &[0x83, 42, 2]);
            }
        }
    }

    #[test]
    fn mixed_layout_rejects_duplicate_missing_and_misbound_wheel_profiles() {
        let names = JOINTS.map(str::to_owned);
        let ids = [1, 2, 3, 4, 5, 6, 8, 9, 7];
        for profiles in [
            Profiles::Nominal(profiles([8, 7, 9])),
            Profiles::ObservedRaw(raw_profiles([8, 7, 9])),
        ] {
            assert!(MixedBus2::bind(&names, &ids, profiles).is_err());
        }
        let mut nominal = profiles(FIXTURE_WHEEL_IDS);
        nominal.swap(0, 1);
        let mut raw = raw_profiles(FIXTURE_WHEEL_IDS);
        raw.swap(0, 1);
        for profiles in [Profiles::Nominal(nominal), Profiles::ObservedRaw(raw)] {
            assert!(MixedBus2::bind(&names, &ids, profiles).is_err());
        }
        for variant in 0..5 {
            let mut invalid_names = names.clone();
            let mut invalid_ids = ids;
            match variant {
                0 => invalid_names[8] = invalid_names[7].clone(),
                1 => invalid_names[8] = "unknown_wheel".to_owned(),
                2 => invalid_ids[8] = invalid_ids[7],
                3 => invalid_ids[8] = 10,
                _ => invalid_ids[0] = 2,
            }
            assert!(
                MixedBus2::bind(
                    &invalid_names,
                    &invalid_ids,
                    Profiles::Nominal(profiles(FIXTURE_WHEEL_IDS))
                )
                .is_err()
            );
        }
        assert!(
            MixedBus2::bind(
                &names[..8],
                &ids[..8],
                Profiles::Nominal(profiles(FIXTURE_WHEEL_IDS))
            )
            .is_err()
        );
    }

    #[test]
    fn observed_raw_rejects_unverified_values_and_clamp_before_first_motion_frame() {
        for phase in [4, 12] {
            let (fixture, result) =
                Fixture::with_profile([9, 3, 6, 1, 8, 4, 2, 7, 5], 0, phase, false, true);
            assert_eq!(result.is_ok(), phase == 12);
            assert!(
                frames(&fixture.handle.drain_tx())
                    .iter()
                    .all(|frame| matches!(frame[4], 1 | 2))
            );
        }
        for ids in [[1, 2, 3, 4, 5, 6, 7, 8, 9], [9, 3, 6, 1, 8, 4, 2, 7, 5]] {
            let (mut f, connected) = Fixture::new(ids, 0, 4, false);
            connected.unwrap();
            let layout = f.adapter.mixed_bus2.as_mut().unwrap();
            layout.profiles = Profiles::ObservedRaw(raw_profiles(FIXTURE_WHEEL_IDS));
            f.adapter.calibrations = ids
                .into_iter()
                .map(|id| crate::MotorCalibration::new(id, 0, 0, 0, 4095).unwrap())
                .collect();
            let _ = f.handle.drain_tx();
            let mut command = f.command();
            for slot in f.adapter.mixed_bus2.unwrap().wheel_slots {
                command.raw_velocities[slot] = 0;
            }
            let wheel = f.adapter.mixed_bus2.unwrap().wheel_slots[2];
            for raw in [-500, -1, 1, 499, 501, i16::MIN] {
                command.raw_velocities[wheel] = raw;
                assert!(f.adapter.write_command(&command.as_view()).is_err());
                assert_eq!(f.handle.drain_tx(), [] as [u8; 0]);
            }
            command.raw_velocities[wheel] = 500;
            let arm = f.adapter.mixed_bus2.unwrap().arm_slots[0];
            f.adapter.calibrations[arm].range_min = 2100;
            assert!(f.adapter.write_command(&command.as_view()).is_err());
            assert_eq!(f.handle.drain_tx(), [] as [u8; 0]);
            f.adapter.calibrations[arm].range_min = 0;
            for (index, expected) in [
                [8, 244, 1, 9, 0, 0, 7, 0, 0],
                [8, 0, 0, 9, 244, 1, 7, 0, 0],
                [8, 0, 0, 9, 0, 0, 7, 244, 1],
            ]
            .into_iter()
            .enumerate()
            {
                command.raw_velocities.fill(0);
                command.raw_velocities[f.adapter.mixed_bus2.unwrap().wheel_slots[index]] = 500;
                f.adapter.write_command(&command.as_view()).unwrap();
                assert_eq!(&frames(&f.handle.drain_tx())[0][7..16], &expected);
            }
            for (min, max, raw) in [
                (2000, 1000, 1000),
                (0, 4096, 2047),
                (2000, 3000, 1999),
                (2000, 3000, 3001),
            ] {
                f.adapter.calibrations[arm].range_min = min;
                f.adapter.calibrations[arm].range_max = max;
                f.adapter.pos_raw[..6].fill(2047);
                f.adapter.pos_raw[0] = raw;
                assert!(f.adapter.validate_raw_mixed_anchor(1).is_err());
                assert_eq!(f.handle.drain_tx(), [] as [u8; 0]);
            }
        }
    }

    #[test]
    fn mixed_wire_targets_exact_groups_raw_sign_and_legacy_stop_has_no_transaction() {
        for ids in [
            [1, 2, 3, 4, 5, 6, 7, 8, 9],
            [9, 3, 6, 1, 8, 4, 2, 7, 5],
            [9, 8, 7, 6, 5, 4, 3, 2, 1],
        ] {
            let (mut f, connected) = Fixture::new(ids, 0, 4, false);
            connected.unwrap();
            let _ = f.handle.drain_tx();
            f.adapter
                .write_command(&JointCommandBuf::default().as_view())
                .unwrap();
            assert_eq!(
                f.adapter.set_mode(ControlMode::Position),
                Err(RobotError::Unsupported("混合总线禁止整总线模式切换"))
            );
            assert_eq!(f.handle.drain_tx(), [] as [u8; 0]);
            assert_eq!(
                f.adapter.hold_current_position(),
                Err(RobotError::Unsupported(
                    "混合总线保持须走S0有界反馈与轮零速"
                ))
            );
            assert_eq!(f.handle.drain_tx(), [] as [u8; 0]);
            let command = f.command();
            f.adapter.write_command(&command.as_view()).unwrap();
            let bytes = f.handle.drain_tx();
            let output = frames(&bytes);
            assert_eq!(output.len(), 2);
            assert_eq!(
                &output[0][4..16],
                &[0x83, 46, 2, 8, 0xf4, 1, 9, 0xf4, 0x81, 7, 0, 0]
            );
            assert_eq!(&output[1][4..7], &[0x83, 42, 2]);
            assert_eq!(
                output[1][7..25]
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|entry| entry[0])
                    .collect::<Vec<_>>(),
                [1, 2, 3, 4, 5, 6]
            );
            assert_eq!(f.calls.load(Ordering::Relaxed), 2);
        }
    }

    #[test]
    fn observed_raw_uses_each_configured_slot_tolerance_without_changing_wire_words() {
        let ids = [9, 3, 6, 1, 8, 4, 2, 7, 5];
        let (mut fixture, connected) = Fixture::with_profile(ids, 0, 12, false, true);
        connected.unwrap();
        let _ = fixture.handle.drain_tx();
        let layout = fixture.adapter.mixed_bus2.unwrap();
        for (index, &slot) in layout.arm_slots.iter().enumerate() {
            fixture.adapter.calibrations[slot].range_min = 1000;
            fixture.adapter.calibrations[slot].range_max = 3000;
            fixture.adapter.calibration_range_tolerances_raw[slot] = [0, 20, 7, 0, 0, 0][index];
        }
        for (index, tolerance) in [0, 20, 7].into_iter().enumerate() {
            let slot = layout.arm_slots[index];
            for (raw, accepted) in [
                (1000 - tolerance, true),
                (3000 + tolerance, true),
                (999 - tolerance, false),
                (3001 + tolerance, false),
            ] {
                fixture.adapter.pos_raw[..6].fill(2047);
                fixture.adapter.pos_raw[index] = raw;
                assert_eq!(
                    fixture.adapter.validate_raw_mixed_anchor(1).is_ok(),
                    accepted
                );
                let mut command = fixture.command();
                command.positions[slot] = raw_to_rad(raw);
                command.raw_velocities.fill(0);
                assert_eq!(
                    fixture.adapter.write_command(&command.as_view()).is_ok(),
                    accepted
                );
                let tx = fixture.handle.drain_tx();
                if accepted {
                    let output = frames(&tx);
                    let entry = output[1][7..25]
                        .as_chunks::<3>()
                        .0
                        .iter()
                        .find(|entry| entry[0] == layout.arm_ids[index])
                        .unwrap();
                    assert_eq!(i32::from(u16::from_le_bytes([entry[1], entry[2]])), raw);
                } else {
                    assert_eq!(tx, [] as [u8; 0]);
                }
            }
        }
    }

    #[test]
    fn observed_raw_tolerance_keeps_hard_bounds_and_rejects_original_invalid_calibration() {
        let (mut fixture, connected) =
            Fixture::with_profile([1, 2, 3, 4, 5, 6, 7, 8, 9], 0, 12, false, true);
        connected.unwrap();
        let _ = fixture.handle.drain_tx();
        fixture.adapter.calibration_range_tolerances_raw[0] = 20;
        for (min, max, raw, accepted) in [
            (10, 4085, 0, true),
            (10, 4085, 4095, true),
            (10, 4085, -1, false),
            (10, 4085, 4096, false),
            (3000, 2990, 2992, false),
            (0, 4096, 2047, false),
        ] {
            fixture.adapter.calibrations[0].range_min = min;
            fixture.adapter.calibrations[0].range_max = max;
            fixture.adapter.pos_raw[..6].fill(2047);
            fixture.adapter.pos_raw[0] = raw;
            assert_eq!(
                fixture.adapter.validate_raw_mixed_anchor(1).is_ok(),
                accepted
            );
            let mut command = fixture.command();
            command.positions[0] = raw_to_rad(raw);
            command.raw_velocities.fill(0);
            assert_eq!(
                fixture.adapter.write_command(&command.as_view()).is_ok(),
                accepted
            );
            let tx = fixture.handle.drain_tx();
            assert_eq!(tx.is_empty(), !accepted);
        }
    }

    #[test]
    fn mixed_invalid_profile_mode_and_full_targets_cannot_write_motion() {
        for (phase, bad_mode) in [(12, false), (4, true)] {
            let (mut f, connected) = Fixture::new([1, 2, 3, 4, 5, 6, 7, 8, 9], 0, phase, bad_mode);
            assert!(connected.is_err());
            let bytes = f.handle.drain_tx();
            assert!(
                frames(&bytes)
                    .iter()
                    .all(|frame| frame[4] == 1 || frame[4] == 2)
            );
            // 模式或profile拒绝也不能阻断原全9快停。
            f.adapter.safety_stop().unwrap();
            let stop = f.handle.drain_tx();
            assert_eq!(
                frames(&stop)[0][7..25]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|entry| entry[0])
                    .collect::<Vec<_>>(),
                f.ids
            );
        }
        let (mut f, connected) = Fixture::new([1, 2, 3, 4, 5, 6, 7, 8, 9], 0, 4, false);
        connected.unwrap();
        let _ = f.handle.drain_tx();
        let valid = f.command();
        for variant in 0..6 {
            let mut invalid = valid;
            match variant {
                0 => invalid.raw_velocities[7] = 501,
                1 => invalid.raw_velocities[7] = i16::MIN,
                2 => invalid.positions[0] = f32::NAN,
                3 => invalid.selection = JointSelection::All,
                4 => invalid.deadline_ns = 0,
                _ => {
                    invalid.selection = JointSelection::MixedPositionRawVelocity {
                        position_mask: 0x7f,
                        velocity_mask: 0x180,
                    }
                }
            }
            assert!(f.adapter.write_command(&invalid.as_view()).is_err());
            assert_eq!(f.handle.drain_tx(), [] as [u8; 0]);
        }
        assert_eq!(f.calls.load(Ordering::Relaxed), 0);
        // 公开标定结构体可绕过构造器；最终clamp结果也须在首GV之前可编码。
        f.adapter.calibrations = f.ids.map(crate::types::MotorCalibration::identity).to_vec();
        f.adapter.calibrations[0].range_min = 40_000;
        f.adapter.calibrations[0].range_max = 40_000;
        assert!(f.adapter.write_command(&valid.as_view()).is_err());
        assert_eq!(f.handle.drain_tx(), [] as [u8; 0]);
        assert_eq!(f.calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn mixed_second_write_failure_or_source_expiry_does_not_retry_and_stop_still_covers_bus() {
        for failure in [1, 2] {
            let (mut f, connected) = Fixture::new([1, 2, 3, 4, 5, 6, 7, 8, 9], failure, 4, false);
            connected.unwrap();
            let _ = f.handle.drain_tx();
            let command = f.command();
            let error = f.adapter.write_command(&command.as_view()).unwrap_err();
            assert!(matches!(error, RobotError::Comm(_)));
            let bytes = f.handle.drain_tx();
            let output = frames(&bytes);
            assert_eq!(output.len(), 1);
            assert_eq!(output[0][5], 46);
            assert_eq!(
                f.calls.load(Ordering::Relaxed),
                if failure == 1 { 1 } else { 2 }
            );
            f.adapter.safety_stop().unwrap();
            let stop = f.handle.drain_tx();
            assert_eq!(
                frames(&stop)[0][7..25]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|entry| entry[0])
                    .collect::<Vec<_>>(),
                f.ids
            );
            for id in f.ids {
                f.handle.push_rx(&read_status(id, &[0]));
            }
            let mut enabled = [true; 9];
            f.adapter.read_torque_enable_mask(&mut enabled).unwrap();
            assert_eq!(enabled, [false; 9]);
        }
    }

    #[test]
    fn mixed_enable_syncs_arm_only_zeros_wheels_then_enables_all_and_disable_remains_full_bus() {
        let (mut f, connected) = Fixture::new([1, 2, 3, 4, 5, 6, 7, 8, 9], 0, 4, false);
        connected.unwrap();
        let _ = f.handle.drain_tx();
        for _ in 0..2 {
            for id in 1..=6 {
                f.handle.push_rx(&read_status(id, &[0xff, 7]));
            }
        }
        for id in f.ids {
            f.handle.push_rx(&ack_status(id));
        }
        f.adapter.enable(true).unwrap();
        let bytes = f.handle.drain_tx();
        let output = frames(&bytes);
        let gp = output
            .iter()
            .find(|frame| frame[4] == 0x83 && frame[5] == 42)
            .unwrap();
        assert_eq!(
            gp[7..25]
                .as_chunks::<3>()
                .0
                .iter()
                .map(|entry| entry[0])
                .collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6]
        );
        let zero = output
            .iter()
            .position(|frame| frame[4] == 0x83 && frame[5] == 46)
            .unwrap();
        assert_eq!(&output[zero][7..16], &[8, 0, 0, 9, 0, 0, 7, 0, 0]);
        let enables: Vec<_> = output
            .iter()
            .enumerate()
            .filter(|(_, frame)| frame[4] == 3 && frame[5] == 40)
            .collect();
        assert_eq!(
            enables
                .iter()
                .map(|(_, frame)| frame[2])
                .collect::<Vec<_>>(),
            f.ids
        );
        assert!(
            enables
                .iter()
                .all(|(index, frame)| *index > zero && frame[6] == 1)
        );
        for id in f.ids {
            f.handle.push_rx(&ack_status(id));
        }
        f.adapter.enable(false).unwrap();
        let bytes = f.handle.drain_tx();
        assert_eq!(
            frames(&bytes)
                .iter()
                .map(|frame| (frame[2], frame[4], frame[5], frame[6]))
                .collect::<Vec<_>>(),
            f.ids.map(|id| (id, 3, 40, 0))
        );
    }
}
