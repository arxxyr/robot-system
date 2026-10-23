//! STS3215 三轮恒速模式的纯值合同，不执行串口、启扭或配置写入。
//!
//! 厂家磁编码 STS 内存表公开版本字段181：Phase bit2选择0.732/0.0146RPM，
//! bit3为0时速度0停止；46/58地址使用bit15方向的符号幅度编码。
//! 细单位的厂家标称精度仍为0.732RPM，不当作精确编码器步数每秒。
//! 固件版本匹配只绑定输入身份，不证明厂家表适用于现场固件。
//! 调用方仍须独立确认总线/USB身份、观察新鲜度、供电会话及运行权限。

use std::fmt;

use crate::registers::MODEL_NUMBER_STS3215;

/// XLeRobot三轮具名角色；不改变历史轮名或ArmIK。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WheelRole {
    Front,
    Left,
    Right,
}

impl WheelRole {
    /// 当前XLeRobot三轮固定ID，调用方必须另绑正确总线。
    #[must_use]
    pub const fn motor_id(self) -> u8 {
        match self {
            Self::Front => 8,
            Self::Left => 7,
            Self::Right => 9,
        }
    }
}

/// 厂家地址0/1的固件版本原始字节，不等同于文档平台version字段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FirmwareVersion {
    pub major: u8,
    pub minor: u8,
}

/// 调用方提供的逐项只读观察；没有默认或已认证布尔值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WheelObservation {
    pub motor_id: u8,
    pub model: u16,
    pub mode: u8,
    pub phase: u8,
    pub firmware: Option<FirmwareVersion>,
}

/// 明确期望值与raw上限；构造profile时校验，不赋予运行权限。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WheelContract {
    pub role: WheelRole,
    pub firmware: FirmwareVersion,
    pub phase: u8,
    pub raw_limit: u16,
}

/// Phase bit2对应的两种厂家名义RPM单位。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VelocityUnit {
    Rpm0_732,
    Rpm0_0146,
}

impl VelocityUnit {
    #[must_use]
    pub const fn rpm_per_raw(self) -> f64 {
        match self {
            Self::Rpm0_732 => 732.0 / 1000.0,
            Self::Rpm0_0146 => 146.0 / 10000.0,
        }
    }

    #[must_use]
    pub const fn rad_s_per_raw(self) -> f64 {
        self.rpm_per_raw() * std::f64::consts::TAU / 60.0
    }
}

/// 输入或数值边界错误；没有永久Unverified占位。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WheelProfileError {
    WrongMotorId,
    WrongModel,
    WrongMode,
    FirmwareMissing,
    FirmwareMismatch,
    PhaseOutOfRange,
    PhaseMismatch,
    ZeroMeansMaximumSpeed,
    InvalidRawLimit,
    NonFiniteVelocity,
    ArithmeticOverflow,
    RawRepresentationOutOfRange,
    RawLimitExceeded { requested: i32, limit: u16 },
}

impl fmt::Display for WheelProfileError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongMotorId => out.write_str("轮角色与固定电机ID不匹配"),
            Self::WrongModel => out.write_str("轮电机型号不是STS3215/777"),
            Self::WrongMode => out.write_str("轮电机不是恒速模式1"),
            Self::FirmwareMissing => out.write_str("缺少固件版本只读观察"),
            Self::FirmwareMismatch => out.write_str("固件版本与明确期望值不匹配"),
            Self::PhaseOutOfRange => out.write_str("Phase超出厂家0到254范围"),
            Self::PhaseMismatch => out.write_str("完整Phase与明确期望值不匹配"),
            Self::ZeroMeansMaximumSpeed => out.write_str("Phase bit3为1，速度0表示最高速度"),
            Self::InvalidRawLimit => out.write_str("raw速度上限必须在1到32767内"),
            Self::NonFiniteVelocity => out.write_str("轮角速度必须为有限值"),
            Self::ArithmeticOverflow => out.write_str("轮角速度换算中间值不可表示"),
            Self::RawRepresentationOutOfRange => out.write_str("速度幅度超出15位表示范围"),
            Self::RawLimitExceeded { .. } => out.write_str("速度超出明确raw上限，不做单轮限幅"),
        }
    }
}

impl std::error::Error for WheelProfileError {}

/// 私有字段保证每个可构造实例都具有明确的零停止和单位数学前提。
///
/// 这不是Enable能力，也不证明现场供电会话仍有效；重新打开或复位后须新观察。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sts3215WheelProfile {
    contract: WheelContract,
    unit: VelocityUnit,
}

/// 现场已验证的原始值合同；没有 SI 单位或任意方向速度承诺。
/// 仅绑定型号777、固件3.10、模式1、Phase12及0/+500两个写入值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sts3215RawWheelProfile {
    role: WheelRole,
}

impl Sts3215RawWheelProfile {
    /// # Errors
    /// 任一现场身份不符即拒绝，不解除名义 SI profile 的 Phase 限制。
    pub fn try_bind(
        observed: WheelObservation,
        role: WheelRole,
    ) -> Result<Self, WheelProfileError> {
        if observed.motor_id != role.motor_id() {
            return Err(WheelProfileError::WrongMotorId);
        }
        if observed.model != MODEL_NUMBER_STS3215 {
            return Err(WheelProfileError::WrongModel);
        }
        if observed.mode != 1 {
            return Err(WheelProfileError::WrongMode);
        }
        match observed.firmware {
            None => return Err(WheelProfileError::FirmwareMissing),
            Some(FirmwareVersion {
                major: 3,
                minor: 10,
            }) => {}
            Some(_) => return Err(WheelProfileError::FirmwareMismatch),
        }
        if observed.phase != 12 {
            return Err(WheelProfileError::PhaseMismatch);
        }
        Ok(Self { role })
    }

    #[must_use]
    pub const fn role(self) -> WheelRole {
        self.role
    }

    /// 该实证合同只容许已测的零速和正向500，不把上限解释为连续范围。
    #[must_use]
    pub const fn accepts(self, raw: i16) -> bool {
        matches!(raw, 0 | 500)
    }
}

/// 名义反馈换算与原始wire字；负零反馈保留原件。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecodedWheelVelocity {
    pub word: u16,
    pub signed_raw: i32,
    pub nominal_rad_s: f64,
}

impl Sts3215WheelProfile {
    /// 按ID、型号、模式、版本、完整Phase、零义和上限顺序验证输入。
    ///
    /// # Errors
    /// 任一输入不匹配或不满足具体合同前提，返回对应具名错误。
    pub fn try_bind(
        observed: WheelObservation,
        expected: WheelContract,
    ) -> Result<Self, WheelProfileError> {
        if observed.motor_id != expected.role.motor_id() {
            return Err(WheelProfileError::WrongMotorId);
        }
        if observed.model != MODEL_NUMBER_STS3215 {
            return Err(WheelProfileError::WrongModel);
        }
        if observed.mode != 1 {
            return Err(WheelProfileError::WrongMode);
        }
        match observed.firmware {
            None => return Err(WheelProfileError::FirmwareMissing),
            Some(version) if version != expected.firmware => {
                return Err(WheelProfileError::FirmwareMismatch);
            }
            Some(_) => {}
        }
        if observed.phase == u8::MAX || expected.phase == u8::MAX {
            return Err(WheelProfileError::PhaseOutOfRange);
        }
        if observed.phase != expected.phase {
            return Err(WheelProfileError::PhaseMismatch);
        }
        if observed.phase & 8 != 0 {
            return Err(WheelProfileError::ZeroMeansMaximumSpeed);
        }
        if !(1..=32767).contains(&expected.raw_limit) {
            return Err(WheelProfileError::InvalidRawLimit);
        }
        let unit = match observed.phase & 4 {
            0 => VelocityUnit::Rpm0_732,
            _ => VelocityUnit::Rpm0_0146,
        };
        Ok(Self {
            contract: expected,
            unit,
        })
    }

    #[must_use]
    pub const fn contract(self) -> WheelContract {
        self.contract
    }

    #[must_use]
    pub const fn unit(self) -> VelocityUnit {
        self.unit
    }

    /// 将rad/s量化为46/47地址的符号幅度word，不执行写入或单轮限幅。
    ///
    /// 三轮同比限速由上层统一处理。精确半值远离0；先验表示范围再验配置上限。
    /// # Errors
    /// 非有限、换算溢出、幅度超32767或超明确raw_limit时拒绝。
    pub fn try_encode_rad_s(self, omega_rad_s: f64) -> Result<u16, WheelProfileError> {
        if !omega_rad_s.is_finite() {
            return Err(WheelProfileError::NonFiniteVelocity);
        }
        let raw_float = omega_rad_s * (60.0 / (std::f64::consts::TAU * self.unit.rpm_per_raw()));
        if !raw_float.is_finite() {
            return Err(WheelProfileError::ArithmeticOverflow);
        }
        let rounded = raw_float.round();
        if rounded.abs() > 32767.0 {
            return Err(WheelProfileError::RawRepresentationOutOfRange);
        }
        // 有限值已通过表示范围门，转换不会依赖Rust饱和cast来掩盖溢出。
        let signed_raw = rounded as i32;
        if signed_raw.unsigned_abs() > u32::from(self.contract.raw_limit) {
            return Err(WheelProfileError::RawLimitExceeded {
                requested: signed_raw,
                limit: self.contract.raw_limit,
            });
        }
        let magnitude = signed_raw.unsigned_abs() as u16;
        let direction = u16::from(signed_raw < 0) << 15;
        Ok(magnitude | direction)
    }

    /// 解码58/59反馈，保留原word，返回厂家名义rad/s；不自行判负零非法。
    #[must_use]
    pub fn decode_velocity_word(self, word: u16) -> DecodedWheelVelocity {
        let magnitude = i32::from(word & 0x7fff);
        let signed_raw = match word & 0x8000 {
            0 => magnitude,
            _ => -magnitude,
        };
        DecodedWheelVelocity {
            word,
            signed_raw,
            nominal_rad_s: f64::from(signed_raw) * self.unit.rad_s_per_raw(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_FIRMWARE: FirmwareVersion = FirmwareVersion { major: 1, minor: 2 };

    fn inputs(role: WheelRole, phase: u8, raw_limit: u16) -> (WheelObservation, WheelContract) {
        (
            WheelObservation {
                motor_id: role.motor_id(),
                model: 777,
                mode: 1,
                phase,
                firmware: Some(FIXTURE_FIRMWARE),
            },
            WheelContract {
                role,
                firmware: FIXTURE_FIRMWARE,
                phase,
                raw_limit,
            },
        )
    }

    fn profile(phase: u8, limit: u16) -> Sts3215WheelProfile {
        let (observation, contract) = inputs(WheelRole::Front, phase, limit);
        Sts3215WheelProfile::try_bind(observation, contract).unwrap()
    }

    #[test]
    fn observed_raw_contract_is_exact_and_does_not_relax_nominal_phase() {
        for role in [WheelRole::Front, WheelRole::Left, WheelRole::Right] {
            let observation = WheelObservation {
                motor_id: role.motor_id(),
                model: 777,
                mode: 1,
                phase: 12,
                firmware: Some(FirmwareVersion {
                    major: 3,
                    minor: 10,
                }),
            };
            let profile = Sts3215RawWheelProfile::try_bind(observation, role).unwrap();
            for raw in i16::MIN..=i16::MAX {
                assert_eq!(profile.accepts(raw), matches!(raw, 0 | 500));
            }
            for bad in [
                WheelObservation {
                    motor_id: 1,
                    ..observation
                },
                WheelObservation {
                    model: 778,
                    ..observation
                },
                WheelObservation {
                    mode: 0,
                    ..observation
                },
                WheelObservation {
                    phase: 4,
                    ..observation
                },
                WheelObservation {
                    firmware: None,
                    ..observation
                },
                WheelObservation {
                    firmware: Some(FirmwareVersion {
                        major: 3,
                        minor: 11,
                    }),
                    ..observation
                },
            ] {
                assert!(Sts3215RawWheelProfile::try_bind(bad, role).is_err());
            }
            assert_eq!(
                Sts3215WheelProfile::try_bind(
                    observation,
                    WheelContract {
                        role,
                        firmware: observation.firmware.unwrap(),
                        phase: 12,
                        raw_limit: 500
                    }
                ),
                Err(WheelProfileError::ZeroMeansMaximumSpeed)
            );
        }
    }

    #[test]
    fn three_roles_and_two_nominal_units_have_explicit_positive_paths() {
        for role in [WheelRole::Front, WheelRole::Left, WheelRole::Right] {
            for (phase, unit) in [(0, VelocityUnit::Rpm0_732), (4, VelocityUnit::Rpm0_0146)] {
                let (observed, expected) = inputs(role, phase, 32767);
                let profile = Sts3215WheelProfile::try_bind(observed, expected).unwrap();
                assert_eq!(profile.contract(), expected);
                assert_eq!(profile.unit(), unit);
                assert_eq!(profile.try_encode_rad_s(0.0), Ok(0));
                assert_eq!(profile.try_encode_rad_s(-0.0), Ok(0));
            }
        }
        assert_eq!(VelocityUnit::Rpm0_732.rpm_per_raw(), 0.732);
        assert_eq!(VelocityUnit::Rpm0_0146.rpm_per_raw(), 0.0146);
        assert_ne!(
            VelocityUnit::Rpm0_0146.rad_s_per_raw(),
            std::f64::consts::TAU / 4096.0
        );
    }

    #[test]
    fn identity_firmware_mode_and_complete_phase_failures_are_specific() {
        let (observed, expected) = inputs(WheelRole::Front, 4, 32767);
        for (observed, expected, error) in [
            (
                WheelObservation {
                    motor_id: 7,
                    ..observed
                },
                expected,
                WheelProfileError::WrongMotorId,
            ),
            (
                WheelObservation {
                    model: 778,
                    ..observed
                },
                expected,
                WheelProfileError::WrongModel,
            ),
            (
                WheelObservation {
                    mode: 0,
                    ..observed
                },
                expected,
                WheelProfileError::WrongMode,
            ),
            (
                WheelObservation {
                    firmware: None,
                    ..observed
                },
                expected,
                WheelProfileError::FirmwareMissing,
            ),
            (
                WheelObservation {
                    firmware: Some(FirmwareVersion { major: 1, minor: 3 }),
                    ..observed
                },
                expected,
                WheelProfileError::FirmwareMismatch,
            ),
            (
                WheelObservation {
                    phase: 255,
                    ..observed
                },
                expected,
                WheelProfileError::PhaseOutOfRange,
            ),
            (
                observed,
                WheelContract {
                    phase: 255,
                    ..expected
                },
                WheelProfileError::PhaseOutOfRange,
            ),
            (
                WheelObservation {
                    phase: 0,
                    ..observed
                },
                expected,
                WheelProfileError::PhaseMismatch,
            ),
        ] {
            assert_eq!(
                Sts3215WheelProfile::try_bind(observed, expected),
                Err(error)
            );
        }
        for raw_limit in [0, 32768, u16::MAX] {
            assert_eq!(
                Sts3215WheelProfile::try_bind(
                    observed,
                    WheelContract {
                        raw_limit,
                        ..expected
                    }
                ),
                Err(WheelProfileError::InvalidRawLimit)
            );
        }
    }

    #[test]
    fn phase12_and_every_bit3_set_value_reject_zero_contract() {
        for phase in 0..=254_u8 {
            let (observed, expected) = inputs(WheelRole::Right, phase, 32767);
            let result = Sts3215WheelProfile::try_bind(observed, expected);
            match phase & 8 {
                0 => assert_eq!(result.unwrap().contract().phase, phase),
                _ => assert_eq!(result, Err(WheelProfileError::ZeroMeansMaximumSpeed)),
            }
        }
    }

    #[test]
    fn all_wire_words_roundtrip_in_both_units_and_preserve_negative_zero() {
        for phase in [0, 4] {
            let profile = profile(phase, 32767);
            for word in 0..=u16::MAX {
                let decoded = profile.decode_velocity_word(word);
                assert_eq!(decoded.word, word);
                assert_eq!(
                    profile.try_encode_rad_s(decoded.nominal_rad_s),
                    Ok(match word {
                        0x8000 => 0,
                        _ => word,
                    })
                );
            }
            let negative_zero = profile.decode_velocity_word(0x8000);
            assert_eq!(negative_zero.signed_raw, 0);
            assert_eq!(negative_zero.nominal_rad_s, 0.0);
        }
    }

    #[test]
    fn sign_magnitude_and_exact_half_rounding_are_not_twos_complement() {
        for phase in [0, 4] {
            let profile = profile(phase, 32767);
            for (raw, expected) in [
                (60, 0x003c),
                (-60, 0x803c),
                (32767, 0x7fff),
                (-32767, 0xffff),
            ] {
                let omega = f64::from(raw) * profile.unit().rad_s_per_raw();
                let word = profile.try_encode_rad_s(omega).unwrap();
                assert_eq!(word, expected);
            }
            let factor = 60.0 / (std::f64::consts::TAU * profile.unit().rpm_per_raw());
            let mut exact_half_checks = 0;
            for (raw, expected) in [(0.5_f64, 1_u16), (-0.5, 0x8001), (1.5, 2), (-1.5, 0x8002)] {
                let omega = raw / factor;
                // 只有换算回精确half的输入，才断言该half舍入方向。
                if omega * factor == raw {
                    exact_half_checks += 1;
                    assert_eq!(profile.try_encode_rad_s(omega), Ok(expected));
                }
            }
            assert!(exact_half_checks >= 2, "每个单位必须实际验证精确正负half");
        }
    }

    #[test]
    fn nonfinite_overflow_and_rawlimit_are_rejected_without_clamp() {
        for phase in [0, 4] {
            let profile = profile(phase, 20);
            for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                assert_eq!(
                    profile.try_encode_rad_s(value),
                    Err(WheelProfileError::NonFiniteVelocity)
                );
            }
            for value in [f64::MAX, -f64::MAX] {
                assert_eq!(
                    profile.try_encode_rad_s(value),
                    Err(WheelProfileError::ArithmeticOverflow)
                );
            }
            for raw in [32768, -32768] {
                assert_eq!(
                    profile.try_encode_rad_s(f64::from(raw) * profile.unit().rad_s_per_raw()),
                    Err(WheelProfileError::RawRepresentationOutOfRange)
                );
            }
            for raw in [21, -21, 60, -60] {
                assert_eq!(
                    profile.try_encode_rad_s(f64::from(raw) * profile.unit().rad_s_per_raw()),
                    Err(WheelProfileError::RawLimitExceeded {
                        requested: raw,
                        limit: 20
                    })
                );
            }
            for raw in [20, -20] {
                let word = profile
                    .try_encode_rad_s(f64::from(raw) * profile.unit().rad_s_per_raw())
                    .unwrap();
                assert_eq!(profile.decode_velocity_word(word).signed_raw, raw);
            }
        }
    }

    #[test]
    fn pure_profile_math_does_not_allocate() {
        assert_no_alloc::assert_no_alloc(|| {
            let profile = profile(4, 100);
            let word = profile.try_encode_rad_s(0.05).unwrap();
            let decoded = profile.decode_velocity_word(word);
            assert!(decoded.nominal_rad_s.is_finite());
            assert_eq!(profile.try_encode_rad_s(-0.0), Ok(0));
        });
    }
}
