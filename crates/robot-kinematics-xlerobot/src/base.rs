//! `XLeRobot` 三轮全向底盘运动学与模拟面板方向。
//!
//! 机身坐标为前进 `vx`、左移 `vy`、逆时针 `wz`；数组顺序固定为前轮、左轮、右轮。
//! 设轮半径为 `r`，底盘中心到轮的半径为 `L`，`a = sqrt(3)/2`：
//!
//! ```text
//! wf = (vy + L*wz)/r
//! wl = (-a*vx - vy/2 + L*wz)/r
//! wr = (a*vx - vy/2 + L*wz)/r
//! vx = r*(wr-wl)/sqrt(3)
//! vy = r*(2*wf-wl-wr)/3
//! wz = r*(wf+wl+wr)/(3*L)
//! ```
//!
//! 以机身前方为 0°，朝左为正角度，左轮位于 120°，右轮位于 240°。
//! 从各轮外侧正对轮面，负轮速为逆时针。几何尺寸与轮速上限由调用方提供。
//! 模拟方向使用固定 raw 档位，档位只用于验证各轮转向组合。
//! 每次计算检查输入与中间值；中间溢出即拒绝，不借助后续除法把无穷大掩盖成零。

use std::fmt;

/// 机身平面速度；字段分别使用米/秒、米/秒、弧度/秒。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanarBodyVelocity {
    /// 前进速度，米/秒。
    pub vx_m_s: f64,
    /// 左移速度，米/秒。
    pub vy_m_s: f64,
    /// 逆时针角速度，弧度/秒。
    pub wz_rad_s: f64,
}

/// 以机器人自身为参照的四个底盘方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseDirection {
    /// 沿机身前方平移。
    Forward,
    /// 沿机身后方平移。
    Backward,
    /// 俯视时原地逆时针旋转。
    RotateLeft,
    /// 俯视时原地顺时针旋转。
    RotateRight,
}

impl BaseDirection {
    /// 模拟面板支持的全部方向。
    pub const ALL: [Self; 4] = [
        Self::Forward,
        Self::Backward,
        Self::RotateLeft,
        Self::RotateRight,
    ];

    const SIMULATED_RAW_SPEED: i16 = 500;

    /// 与 SI 变换共用轮切向投影，按前、左、右返回 −1、0 或 +1。
    #[must_use]
    pub fn wheel_signs(self) -> [i16; 3] {
        let (forward, rotation) = match self {
            Self::Forward => (1.0, 0.0),
            Self::Backward => (-1.0, 0.0),
            Self::RotateLeft => (0.0, 1.0),
            Self::RotateRight => (0.0, -1.0),
        };
        wheel_tangents(forward, 0.0, rotation)
            .map(|speed| i16::from(speed > 0.0) - i16::from(speed < 0.0))
    }

    /// 模拟面板的固定档位，按前、左、右返回各轮有符号目标。
    #[must_use]
    pub fn wheel_raw(self) -> [i16; 3] {
        self.wheel_signs()
            .map(|sign| sign * Self::SIMULATED_RAW_SPEED)
    }

    /// 兼容原模拟入口的各轮 0/+500；含负值的请求仅接受已核对的方向组合。
    #[must_use]
    pub fn accepts_simulated_raw(wheels: [i16; 3]) -> bool {
        wheels
            .iter()
            .all(|&raw| matches!(raw, 0 | Self::SIMULATED_RAW_SPEED))
            || Self::ALL
                .into_iter()
                .any(|direction| direction.wheel_raw() == wheels)
    }
}

// forward 是纵向速度在右轮切线上的有符号投影，rotation 是各轮共同的旋转分量。
fn wheel_tangents(forward: f64, lateral: f64, rotation: f64) -> [f64; 3] {
    let half_lateral = lateral / 2.0;
    [
        lateral + rotation,
        (-forward - half_lateral) + rotation,
        (forward - half_lateral) + rotation,
    ]
}

/// 具名轮角速度，均为弧度/秒；不是电机原始寄存器值。
///
/// 字段允许直接构造，公开转换方法仍逐项拒绝 NaN/无穷大。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct XLeRobotWheelVelocities {
    /// 前轮。
    pub front_rad_s: f64,
    /// 左轮。
    pub left_rad_s: f64,
    /// 右轮。
    pub right_rad_s: f64,
}

impl XLeRobotWheelVelocities {
    /// 按前、左、右角色顺序构造；拒绝非有限轮速。
    pub fn from_role_order(velocities_rad_s: [f64; 3]) -> Result<Self, BaseKinematicsError> {
        let [front_rad_s, left_rad_s, right_rad_s] = velocities_rad_s;
        let wheels = Self {
            front_rad_s,
            left_rad_s,
            right_rad_s,
        };
        wheels.to_role_order()?;
        Ok(wheels)
    }

    /// 重检有限值后，按前、左、右角色顺序输出弧度/秒数组。
    pub fn to_role_order(self) -> Result<[f64; 3], BaseKinematicsError> {
        let values = [self.front_rad_s, self.left_rad_s, self.right_rad_s];
        if values.iter().any(|value| !value.is_finite()) {
            return Err(BaseKinematicsError::NonFiniteWheelVelocity);
        }
        Ok(values)
    }
}

/// 同比限速后的轮角速度与施加到全部三个轮的唯一比例。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LimitedWheelVelocities {
    /// 限速结果，弧度/秒。
    pub wheels: XLeRobotWheelVelocities,
    /// 无超限为1；超限时严格大于0且不大于1。
    pub scale: f64,
}

/// 参数、输入或数值表示错误；不分配堆内存。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseKinematicsError {
    /// 轮半径必须有限且严格为正。
    InvalidWheelRadius,
    /// 底盘半径必须有限且严格为正。
    InvalidBaseRadius,
    /// 最大轮角速度必须有限且严格为正。
    InvalidMaxWheelVelocity,
    /// 至少一个机身输入分量为 NaN/无穷大。
    NonFiniteBodyVelocity,
    /// 至少一个轮速输入分量为 NaN/无穷大。
    NonFiniteWheelVelocity,
    /// 中间/输出非有限、缩放下溢为零，或舍入后超过轮速上限。
    UnrepresentableCalculation,
}

impl fmt::Display for BaseKinematicsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidWheelRadius => formatter.write_str("轮半径必须有限且严格为正"),
            Self::InvalidBaseRadius => formatter.write_str("底盘半径必须有限且严格为正"),
            Self::InvalidMaxWheelVelocity => formatter.write_str("最大轮角速度必须有限且严格为正"),
            Self::NonFiniteBodyVelocity => formatter.write_str("机身速度含非有限值"),
            Self::NonFiniteWheelVelocity => formatter.write_str("轮角速度含非有限值"),
            Self::UnrepresentableCalculation => {
                formatter.write_str("计算结果无法有限表示或满足轮速上限")
            }
        }
    }
}

impl std::error::Error for BaseKinematicsError {}

/// 显式几何参数下的三轮全向运动学，无默认构造和运行时依赖。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct XLeRobotOmniKinematics {
    wheel_radius_m: f64,
    base_radius_m: f64,
    max_wheel_rad_s: f64,
}

impl XLeRobotOmniKinematics {
    /// 半径单位为米，轮角速度上限为弧度/秒；三个参数都必须有限且严格为正。
    ///
    /// 不预计算可能溢出的比值；实际变换时检查全部中间计算。
    pub fn new(
        wheel_radius_m: f64,
        base_radius_m: f64,
        max_wheel_rad_s: f64,
    ) -> Result<Self, BaseKinematicsError> {
        for (value, error) in [
            (wheel_radius_m, BaseKinematicsError::InvalidWheelRadius),
            (base_radius_m, BaseKinematicsError::InvalidBaseRadius),
            (
                max_wheel_rad_s,
                BaseKinematicsError::InvalidMaxWheelVelocity,
            ),
        ] {
            if !value.is_finite() || value <= 0.0 {
                return Err(error);
            }
        }
        Ok(Self {
            wheel_radius_m,
            base_radius_m,
            max_wheel_rad_s,
        })
    }

    /// 机身 SI 速度转换为三轮角速度；此方法不隐式限幅。
    pub fn body_to_wheels(
        &self,
        body: PlanarBodyVelocity,
    ) -> Result<XLeRobotWheelVelocities, BaseKinematicsError> {
        validate_body(body)?;
        let rotation = finite(self.base_radius_m * body.wz_rad_s)?;
        let forward = finite((3.0_f64.sqrt() / 2.0) * body.vx_m_s)?;
        let [front, left, right] = wheel_tangents(forward, body.vy_m_s, rotation);
        Ok(XLeRobotWheelVelocities {
            front_rad_s: finite(front / self.wheel_radius_m)?,
            left_rad_s: finite(left / self.wheel_radius_m)?,
            right_rad_s: finite(right / self.wheel_radius_m)?,
        })
    }

    /// 三轮角速度转换为具名机身 SI 速度；直构的轮类型也逐项重检。
    pub fn wheels_to_body(
        &self,
        wheels: XLeRobotWheelVelocities,
    ) -> Result<PlanarBodyVelocity, BaseKinematicsError> {
        let [front, left, right] = wheels.to_role_order()?;
        let difference = finite(right - left)?;
        let lateral = finite(finite(finite(2.0 * front)? - left)? - right)?;
        let sum = finite(finite(front + left)? + right)?;
        let rotation_denominator = finite(3.0 * self.base_radius_m)?;
        Ok(PlanarBodyVelocity {
            vx_m_s: finite(finite(self.wheel_radius_m * difference)? / 3.0_f64.sqrt())?,
            vy_m_s: finite(finite(self.wheel_radius_m * lateral)? / 3.0)?,
            wz_rad_s: finite(finite(self.wheel_radius_m * sum)? / rotation_denominator)?,
        })
    }

    /// 先变换再按 `max/peak` 统一缩放，保持三个轮使用同一比例。
    ///
    /// 不逐轮裁剪。零输入返回零轮速且比例为1；下溢到零的比例拒绝。
    /// 若首次舍入超限，统一比例向下调整一 ULP 后重算全部三轮；仍超限则拒绝。
    pub fn limit_body_velocity(
        &self,
        body: PlanarBodyVelocity,
    ) -> Result<LimitedWheelVelocities, BaseKinematicsError> {
        let wheels = self.body_to_wheels(body)?;
        let [front, left, right] = wheels.to_role_order()?;
        let peak = front.abs().max(left.abs()).max(right.abs());
        if peak <= self.max_wheel_rad_s {
            return Ok(LimitedWheelVelocities { wheels, scale: 1.0 });
        }
        let mut scale = finite(self.max_wheel_rad_s / peak)?;
        if scale <= 0.0 {
            return Err(BaseKinematicsError::UnrepresentableCalculation);
        }
        let scaled = |factor: f64| -> Result<XLeRobotWheelVelocities, BaseKinematicsError> {
            Ok(XLeRobotWheelVelocities {
                front_rad_s: finite(front * factor)?,
                left_rad_s: finite(left * factor)?,
                right_rad_s: finite(right * factor)?,
            })
        };
        let mut limited = scaled(scale)?;
        if limited
            .to_role_order()?
            .iter()
            .any(|value| value.abs() > self.max_wheel_rad_s)
        {
            // scale已知严格为正且有限，正数编码减1精确取下一较小的可表示值。
            scale = finite(f64::from_bits(scale.to_bits() - 1))?;
            if scale <= 0.0 {
                return Err(BaseKinematicsError::UnrepresentableCalculation);
            }
            limited = scaled(scale)?;
            if limited
                .to_role_order()?
                .iter()
                .any(|value| value.abs() > self.max_wheel_rad_s)
            {
                return Err(BaseKinematicsError::UnrepresentableCalculation);
            }
        }
        Ok(LimitedWheelVelocities {
            wheels: limited,
            scale,
        })
    }
}

fn validate_body(body: PlanarBodyVelocity) -> Result<(), BaseKinematicsError> {
    if [body.vx_m_s, body.vy_m_s, body.wz_rad_s]
        .iter()
        .any(|value| !value.is_finite())
    {
        return Err(BaseKinematicsError::NonFiniteBodyVelocity);
    }
    Ok(())
}

fn finite(value: f64) -> Result<f64, BaseKinematicsError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(BaseKinematicsError::UnrepresentableCalculation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOLERANCE: f64 = 1e-12;

    fn body(values: [f64; 3]) -> PlanarBodyVelocity {
        let [forward_m_s, lateral_m_s, angular_rad_s] = values;
        PlanarBodyVelocity {
            vx_m_s: forward_m_s,
            vy_m_s: lateral_m_s,
            wz_rad_s: angular_rad_s,
        }
    }

    fn wheels(values: [f64; 3]) -> XLeRobotWheelVelocities {
        let [front_rad_s, left_rad_s, right_rad_s] = values;
        XLeRobotWheelVelocities {
            front_rad_s,
            left_rad_s,
            right_rad_s,
        }
    }

    fn model(max_wheel_rad_s: f64) -> XLeRobotOmniKinematics {
        XLeRobotOmniKinematics::new(0.2, 0.5, max_wheel_rad_s).unwrap()
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(actual.is_finite() && expected.is_finite());
        assert!(
            (actual - expected).abs() <= TOLERANCE,
            "实际{actual:?}，期望{expected:?}"
        );
    }

    fn assert_wheels(actual: XLeRobotWheelVelocities, expected: [f64; 3]) {
        for (actual, expected) in actual.to_role_order().unwrap().into_iter().zip(expected) {
            assert_close(actual, expected);
        }
    }

    fn assert_body(actual: PlanarBodyVelocity, expected: [f64; 3]) {
        for (actual, expected) in [actual.vx_m_s, actual.vy_m_s, actual.wz_rad_s]
            .into_iter()
            .zip(expected)
        {
            assert_close(actual, expected);
        }
    }

    #[test]
    fn role_order_and_named_array_mapping() {
        let value = XLeRobotWheelVelocities::from_role_order([1.0, 2.0, 3.0]).unwrap();
        assert_close(value.front_rad_s, 1.0);
        assert_close(value.left_rad_s, 2.0);
        assert_close(value.right_rad_s, 3.0);
        assert_eq!(value, wheels([1.0, 2.0, 3.0]));
        assert_wheels(value, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn zero_body_and_wheels_have_scale_one() {
        let model = model(2.5);
        assert_wheels(model.body_to_wheels(body([0.0; 3])).unwrap(), [0.0; 3]);
        assert_body(model.wheels_to_body(wheels([0.0; 3])).unwrap(), [0.0; 3]);
        let limited = model.limit_body_velocity(body([0.0; 3])).unwrap();
        assert_wheels(limited.wheels, [0.0; 3]);
        assert_eq!(limited.scale.to_bits(), 1.0_f64.to_bits());
    }

    #[test]
    fn forward_basis_absolute_golden_with_nonunit_geometry() {
        // r=0.2米、L=0.5米，vx=1米/秒；前轮为零，左轮负、右轮正。
        assert_wheels(
            model(20.0).body_to_wheels(body([1.0, 0.0, 0.0])).unwrap(),
            [0.0, -4.330_127_018_922_193, 4.330_127_018_922_193],
        );
    }

    #[test]
    fn left_basis_absolute_golden_with_nonunit_geometry() {
        assert_wheels(
            model(20.0).body_to_wheels(body([0.0, 1.0, 0.0])).unwrap(),
            [5.0, -2.5, -2.5],
        );
    }

    #[test]
    fn counterclockwise_basis_absolute_golden_with_nonunit_geometry() {
        assert_wheels(
            model(20.0).body_to_wheels(body([0.0, 0.0, 1.0])).unwrap(),
            [2.5, 2.5, 2.5],
        );
    }

    #[test]
    fn four_directions_match_physical_wheel_roles() {
        for (direction, expected) in [
            (BaseDirection::Forward, [0, -1, 1]),
            (BaseDirection::Backward, [0, 1, -1]),
            (BaseDirection::RotateLeft, [1, 1, 1]),
            (BaseDirection::RotateRight, [-1, -1, -1]),
        ] {
            assert_eq!(direction.wheel_signs(), expected);
        }
    }

    #[test]
    fn simulated_raw_accepts_original_positive_combinations_and_only_known_directions() {
        for front in [-500, 0, 500] {
            for left in [-500, 0, 500] {
                for right in [-500, 0, 500] {
                    let wheels = [front, left, right];
                    let original = wheels.iter().all(|&speed| speed >= 0);
                    let reverse =
                        matches!(wheels, [0, -500, 500] | [0, 500, -500] | [-500, -500, -500]);
                    assert_eq!(
                        BaseDirection::accepts_simulated_raw(wheels),
                        original || reverse
                    );
                }
            }
        }
        for wheels in [[0, -499, 499], [0, -501, 501], [501; 3], [i16::MIN; 3]] {
            assert!(!BaseDirection::accepts_simulated_raw(wheels));
        }
    }

    #[test]
    fn mixed_body_absolute_golden_then_round_trip() {
        let model = model(20.0);
        let actual = model.body_to_wheels(body([0.4, -0.3, 0.2])).unwrap();
        assert_wheels(
            actual,
            [-1.0, -0.482_050_807_568_877_2, 2.982_050_807_568_877],
        );
        assert_body(model.wheels_to_body(actual).unwrap(), [0.4, -0.3, 0.2]);
    }

    #[test]
    fn inverse_absolute_golden_independent_of_forward_output() {
        // 指定独立轮速[2,4,-2]，手算vx=-1.2/sqrt(3)、vy=0.4/3、wz=0.8/1.5。
        assert_body(
            model(20.0)
                .wheels_to_body(wheels([2.0, 4.0, -2.0]))
                .unwrap(),
            [
                -0.692_820_323_027_550_9,
                0.133_333_333_333_333_33,
                0.533_333_333_333_333_3,
            ],
        );
    }

    #[test]
    fn deterministic_grid_round_trips_for_another_geometry() {
        let model = XLeRobotOmniKinematics::new(0.13, 0.43, 9.0).unwrap();
        for vx in [-0.75, 0.0, 0.375] {
            for vy in [-0.2, 0.0, 0.4] {
                for wz in [-0.6, 0.0, 0.8] {
                    let actual = model.body_to_wheels(body([vx, vy, wz])).unwrap();
                    assert_body(model.wheels_to_body(actual).unwrap(), [vx, vy, wz]);
                }
            }
        }
    }

    #[test]
    fn forward_and_inverse_are_sign_symmetric() {
        let model = model(20.0);
        let positive = model.body_to_wheels(body([0.4, -0.3, 0.2])).unwrap();
        let negative = model.body_to_wheels(body([-0.4, 0.3, -0.2])).unwrap();
        assert_wheels(
            negative,
            positive.to_role_order().unwrap().map(|value| -value),
        );
        assert_body(model.wheels_to_body(negative).unwrap(), [-0.4, 0.3, -0.2]);
        assert_body(
            model.wheels_to_body(wheels([-2.0, -4.0, 2.0])).unwrap(),
            [
                0.692_820_323_027_550_9,
                -0.133_333_333_333_333_33,
                -0.533_333_333_333_333_3,
            ],
        );
    }

    #[test]
    fn no_limit_preserves_original_wheels_and_scale_one() {
        let model = model(20.0);
        let input = body([0.4, -0.3, 0.2]);
        let original = model.body_to_wheels(input).unwrap();
        let limited = model.limit_body_velocity(input).unwrap();
        assert_eq!(limited.wheels, original);
        assert_eq!(limited.scale.to_bits(), 1.0_f64.to_bits());
    }

    #[test]
    fn one_over_limit_wheel_scales_all_wheels() {
        let model = model(2.5);
        // 未限制为[5,-2.5,-2.5]，只有前轮超限，另两轮也须同比缩至-1.25。
        let limited = model.limit_body_velocity(body([0.0, 1.0, 0.0])).unwrap();
        assert_close(limited.scale, 0.5);
        assert_wheels(limited.wheels, [2.5, -1.25, -1.25]);
        assert_body(
            model.wheels_to_body(limited.wheels).unwrap(),
            [0.0, 0.5, 0.0],
        );
    }

    #[test]
    fn multiple_over_limit_wheels_share_one_scale() {
        let model = model(2.5);
        let limited = model.limit_body_velocity(body([0.0, 0.0, 2.0])).unwrap();
        assert_close(limited.scale, 0.5);
        assert_wheels(limited.wheels, [2.5, 2.5, 2.5]);
        assert_body(
            model.wheels_to_body(limited.wheels).unwrap(),
            [0.0, 0.0, 1.0],
        );
    }

    #[test]
    fn mixed_limit_keeps_actual_body_direction() {
        let model = model(1.0);
        let input = body([0.4, -0.3, 0.2]);
        let limited = model.limit_body_velocity(input).unwrap();
        assert_close(limited.scale, 1.0 / 2.982_050_807_568_877);
        assert!(
            limited
                .wheels
                .to_role_order()
                .unwrap()
                .iter()
                .all(|value| value.abs() <= 1.0)
        );
        assert_body(
            model.wheels_to_body(limited.wheels).unwrap(),
            [
                input.vx_m_s * limited.scale,
                input.vy_m_s * limited.scale,
                input.wz_rad_s * limited.scale,
            ],
        );
    }

    #[test]
    fn one_ulp_rounding_overshoot_reduces_the_shared_scale() {
        let model = XLeRobotOmniKinematics::new(1.0, 1.0, 0.1).unwrap();
        let input = body([0.0, 20.0 / 123.0, 0.0]);
        let original = model.body_to_wheels(input).unwrap();
        let initial_scale = 0.1 / original.front_rad_s;
        assert!(original.front_rad_s * initial_scale > 0.1);
        let limited = model.limit_body_velocity(input).unwrap();
        assert_eq!(limited.scale.to_bits(), initial_scale.to_bits() - 1);
        assert!(
            limited
                .wheels
                .to_role_order()
                .unwrap()
                .iter()
                .all(|value| value.abs() <= 0.1)
        );
        assert_eq!(
            limited.wheels,
            wheels(
                original
                    .to_role_order()
                    .unwrap()
                    .map(|value| value * limited.scale)
            )
        );
        assert_body(
            model.wheels_to_body(limited.wheels).unwrap(),
            [0.0, input.vy_m_s * limited.scale, 0.0],
        );
    }

    #[test]
    fn every_geometry_parameter_rejects_nonpositive_or_nonfinite() {
        for value in [0.0, -0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                XLeRobotOmniKinematics::new(value, 0.5, 2.5),
                Err(BaseKinematicsError::InvalidWheelRadius)
            );
            assert_eq!(
                XLeRobotOmniKinematics::new(0.2, value, 2.5),
                Err(BaseKinematicsError::InvalidBaseRadius)
            );
            assert_eq!(
                XLeRobotOmniKinematics::new(0.2, 0.5, value),
                Err(BaseKinematicsError::InvalidMaxWheelVelocity)
            );
        }
    }

    #[test]
    fn each_body_component_rejects_nan_and_both_infinities() {
        let model = model(20.0);
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for index in 0..3 {
                let mut input = [0.0; 3];
                input[index] = value;
                assert_eq!(
                    model.body_to_wheels(body(input)),
                    Err(BaseKinematicsError::NonFiniteBodyVelocity)
                );
                assert_eq!(
                    model.limit_body_velocity(body(input)),
                    Err(BaseKinematicsError::NonFiniteBodyVelocity)
                );
            }
        }
    }

    #[test]
    fn direct_and_ordered_wheels_recheck_every_nonfinite_component() {
        let model = model(20.0);
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for index in 0..3 {
                let mut input = [0.0; 3];
                input[index] = value;
                assert_eq!(
                    XLeRobotWheelVelocities::from_role_order(input),
                    Err(BaseKinematicsError::NonFiniteWheelVelocity)
                );
                assert_eq!(
                    wheels(input).to_role_order(),
                    Err(BaseKinematicsError::NonFiniteWheelVelocity)
                );
                assert_eq!(
                    model.wheels_to_body(wheels(input)),
                    Err(BaseKinematicsError::NonFiniteWheelVelocity)
                );
            }
        }
    }

    #[test]
    fn finite_forward_inputs_reject_intermediate_and_output_overflow() {
        for (model, input) in [
            (
                XLeRobotOmniKinematics::new(1.0, f64::MAX, 1.0).unwrap(),
                [0.0, 0.0, 2.0],
            ),
            (
                XLeRobotOmniKinematics::new(1.0, 1.0, 1.0).unwrap(),
                [0.0, f64::MAX, f64::MAX],
            ),
            (
                XLeRobotOmniKinematics::new(f64::MIN_POSITIVE, 1.0, 1.0).unwrap(),
                [0.0, f64::MAX, 0.0],
            ),
            (
                XLeRobotOmniKinematics::new(1.0, 1.0, 1.0).unwrap(),
                [f64::MAX, -f64::MAX, -f64::MAX / 2.0],
            ),
        ] {
            assert_eq!(
                model.body_to_wheels(body(input)),
                Err(BaseKinematicsError::UnrepresentableCalculation)
            );
            assert_eq!(
                model.limit_body_velocity(body(input)),
                Err(BaseKinematicsError::UnrepresentableCalculation)
            );
        }
    }

    #[test]
    fn inverse_rejects_overflow_instead_of_false_zero_from_infinite_denominator() {
        for (model, input) in [
            // 3*L溢出；不能把有限分子除无穷大当合法零角速度。
            (
                XLeRobotOmniKinematics::new(1.0, f64::MAX, 1.0).unwrap(),
                [1.0, 1.0, 1.0],
            ),
            // 差值、2*前轮、三轮和与乘半径分别触发中间溢出。
            (model(20.0), [0.0, f64::MAX, -f64::MAX]),
            (model(20.0), [f64::MAX, 0.0, 0.0]),
            (
                XLeRobotOmniKinematics::new(1.0, 1.0, 1.0).unwrap(),
                [f64::MAX / 2.0; 3],
            ),
            (
                XLeRobotOmniKinematics::new(f64::MAX, 1.0, 1.0).unwrap(),
                [1.0, 1.0, 1.0],
            ),
            (
                XLeRobotOmniKinematics::new(f64::MAX, 1.0, 1.0).unwrap(),
                [1.0, 0.0, 0.0],
            ),
        ] {
            assert_eq!(
                model.wheels_to_body(wheels(input)),
                Err(BaseKinematicsError::UnrepresentableCalculation)
            );
        }
    }

    #[test]
    fn zero_underflow_scale_is_rejected() {
        let model = XLeRobotOmniKinematics::new(1.0, 1.0, f64::from_bits(1)).unwrap();
        assert_eq!(
            model.limit_body_velocity(body([0.0, f64::MAX / 2.0, 0.0])),
            Err(BaseKinematicsError::UnrepresentableCalculation)
        );
    }

    #[test]
    fn positive_subnormal_scale_still_uses_one_ratio() {
        let model = XLeRobotOmniKinematics::new(1.0, 1.0, 1.0).unwrap();
        let limited = model
            .limit_body_velocity(body([0.0, f64::MAX / 2.0, 0.0]))
            .unwrap();
        assert!(limited.scale > 0.0 && limited.scale < f64::MIN_POSITIVE);
        assert_wheels(limited.wheels, [1.0, -0.5, -0.5]);
        assert_body(
            model.wheels_to_body(limited.wheels).unwrap(),
            [0.0, 1.0, 0.0],
        );
    }
}
