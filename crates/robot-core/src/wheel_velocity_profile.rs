//! 显式轮速几何参数与上限的纯值合同。
//!
//! 三项输入分别使用米、米、弧度每秒，均须有限且严格为正。
//! 构造时只校验并保存原值，不计算比例、倒数或电机原始单位。
//! 构造成功不证明现场参数、固件语义或后续运动学计算可行。

use std::fmt;

/// 底盘运动命令从提交到到期的最长时间，单位为纳秒。
pub const COMMAND_TTL_MAX_NS: u64 = 100_000_000;

/// 已校验的显式轮半径、底盘半径和最大轮角速度。
///
/// 没有默认参数；后续计算仍须独立检查中间值和输出的可表示性。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WheelVelocityProfile {
    wheel_radius_m: f64,
    base_radius_m: f64,
    max_wheel_rad_s: f64,
}

/// 轮速参数的输入错误；按构造参数顺序报告首个非法字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelVelocityProfileError {
    /// 轮半径非有限或不严格为正。
    InvalidWheelRadius,
    /// 底盘半径非有限或不严格为正。
    InvalidBaseRadius,
    /// 最大轮角速度非有限或不严格为正。
    InvalidMaxWheelVelocity,
}

impl fmt::Display for WheelVelocityProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidWheelRadius => formatter.write_str("轮半径必须有限且严格为正（米）"),
            Self::InvalidBaseRadius => formatter.write_str("底盘半径必须有限且严格为正（米）"),
            Self::InvalidMaxWheelVelocity => {
                formatter.write_str("最大轮角速度必须有限且严格为正（弧度每秒）")
            }
        }
    }
}

impl std::error::Error for WheelVelocityProfileError {}

impl WheelVelocityProfile {
    /// 校验三个显式参数并逐值保存，不限幅或换算单位。
    ///
    /// 任一值非有限或不严格为正时，返回该字段的具体错误。
    /// 多个字段非法时，依次检查轮半径、底盘半径、最大轮角速度。
    pub fn new(
        wheel_radius_m: f64,
        base_radius_m: f64,
        max_wheel_rad_s: f64,
    ) -> Result<Self, WheelVelocityProfileError> {
        if !wheel_radius_m.is_finite() || wheel_radius_m <= 0.0 {
            return Err(WheelVelocityProfileError::InvalidWheelRadius);
        }
        if !base_radius_m.is_finite() || base_radius_m <= 0.0 {
            return Err(WheelVelocityProfileError::InvalidBaseRadius);
        }
        if !max_wheel_rad_s.is_finite() || max_wheel_rad_s <= 0.0 {
            return Err(WheelVelocityProfileError::InvalidMaxWheelVelocity);
        }
        Ok(Self {
            wheel_radius_m,
            base_radius_m,
            max_wheel_rad_s,
        })
    }

    /// 返回原轮半径，单位为米。
    #[must_use]
    pub const fn wheel_radius_m(&self) -> f64 {
        self.wheel_radius_m
    }

    /// 返回原底盘半径，单位为米。
    #[must_use]
    pub const fn base_radius_m(&self) -> f64 {
        self.base_radius_m
    }

    /// 返回原最大轮角速度，单位为弧度每秒。
    #[must_use]
    pub const fn max_wheel_rad_s(&self) -> f64 {
        self.max_wheel_rad_s
    }
}

#[cfg(test)]
mod tests {
    use super::{WheelVelocityProfile, WheelVelocityProfileError};

    fn assert_values(profile: WheelVelocityProfile, expected: [f64; 3]) {
        let actual = [
            profile.wheel_radius_m(),
            profile.base_radius_m(),
            profile.max_wheel_rad_s(),
        ];
        assert_eq!(actual.map(f64::to_bits), expected.map(f64::to_bits));
    }

    #[test]
    fn explicit_values_are_preserved_by_getters_and_copy() {
        let expected = [0.13, 0.43, 9.0];
        let profile = WheelVelocityProfile::new(expected[0], expected[1], expected[2]).unwrap();
        let copied = profile;
        assert_values(profile, expected);
        assert_values(copied, expected);
        assert_eq!(profile, copied);
    }

    #[test]
    fn invalid_values_report_the_matching_field() {
        let invalid = [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -0.0,
            -1.0,
            -f64::MIN_POSITIVE,
            -f64::MAX,
            -f64::from_bits(1),
        ];
        for value in invalid {
            assert_eq!(
                WheelVelocityProfile::new(value, 1.0, 1.0),
                Err(WheelVelocityProfileError::InvalidWheelRadius)
            );
            assert_eq!(
                WheelVelocityProfile::new(1.0, value, 1.0),
                Err(WheelVelocityProfileError::InvalidBaseRadius)
            );
            assert_eq!(
                WheelVelocityProfile::new(1.0, 1.0, value),
                Err(WheelVelocityProfileError::InvalidMaxWheelVelocity)
            );
        }
    }

    #[test]
    fn positive_finite_extremes_are_preserved() {
        let smallest = f64::from_bits(1);
        for value in [smallest, f64::MIN_POSITIVE, f64::MAX] {
            for expected in [[value, 1.0, 1.0], [1.0, value, 1.0], [1.0, 1.0, value]] {
                let profile =
                    WheelVelocityProfile::new(expected[0], expected[1], expected[2]).unwrap();
                assert_values(profile, expected);
            }
        }
        // 极端值可保存；此处不提前计算会溢出或下溢的几何比例。
        for expected in [
            [smallest, f64::MAX, f64::MAX],
            [f64::MAX, smallest, smallest],
        ] {
            let profile = WheelVelocityProfile::new(expected[0], expected[1], expected[2]).unwrap();
            assert_values(profile, expected);
        }
    }

    #[test]
    fn invalid_field_precedence_matches_argument_order() {
        assert_eq!(
            WheelVelocityProfile::new(f64::NAN, 0.0, f64::NEG_INFINITY),
            Err(WheelVelocityProfileError::InvalidWheelRadius)
        );
        assert_eq!(
            WheelVelocityProfile::new(1.0, f64::NAN, 0.0),
            Err(WheelVelocityProfileError::InvalidBaseRadius)
        );
    }
}
