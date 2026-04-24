//! `Motor` / `MotorCalibration` 强类型，源自 dimos `feetech_io/types.py` 全 49 行。

use serde::{Deserialize, Serialize};

use crate::error::FeetechError;
use crate::registers::STS3215_RESOLUTION;

/// 一个 motor 节点（在某条 bus 上）。
///
/// 仅含 `id` 字段；本 crate 只支持 STS3215，model 隐含 `MODEL_NUMBER_STS3215`。
/// 多型号场景留 v0.4+。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Motor {
    pub id: u8,
}

impl Motor {
    /// `id` ∈ \[1, 253\]（0 = broadcast，254/255 保留）。
    ///
    /// # Errors
    /// `id == 0` 或 `id >= 254` → [`FeetechError::InvalidMotorId`]。
    pub fn new(id: u8) -> Result<Self, FeetechError> {
        if !(1..=253).contains(&id) {
            return Err(FeetechError::InvalidMotorId(i32::from(id)));
        }
        Ok(Self { id })
    }
}

/// 单 motor 校准：写到 motor EEPROM 的三个字段。
///
/// 字段对应 dimos `MotorCalibration`：
/// - `homing_offset`：sign-magnitude 11-bit signed → 限制 \[-2047, +2047\]
/// - `range_min` / `range_max`：raw unsigned 12-bit → 限制 \[0, 4095\]
/// - `drive_mode`：Feetech 原生方向反转，0 / 1（v0.2 暂未消费，留向后兼容）
///
/// ## JSON 反序列化
///
/// 通过 `#[serde(try_from = "MotorCalibrationRaw")]` 接入 serde：读到的字段
/// 会先填到内部 [`MotorCalibrationRaw`]，再调 [`Self::new`] 做范围校验 —— 与
/// 直接 `new` 构造走的是同一条校验路径，保证 JSON / 代码两条入口等价。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "MotorCalibrationRaw")]
pub struct MotorCalibration {
    pub id: u8,
    pub drive_mode: u8,
    pub homing_offset: i32,
    pub range_min: u16,
    pub range_max: u16,
}

/// 反序列化用的 "unchecked" struct：字段布局与 dimos `scripts/calibrate.py`
/// 输出一致，`drive_mode` 可缺省（默认 0），对应 dimos `entry.get("drive_mode", 0)`。
#[derive(Debug, Clone, Copy, Deserialize)]
struct MotorCalibrationRaw {
    id: u8,
    #[serde(default)]
    drive_mode: u8,
    homing_offset: i32,
    range_min: u16,
    range_max: u16,
}

impl TryFrom<MotorCalibrationRaw> for MotorCalibration {
    type Error = FeetechError;

    fn try_from(raw: MotorCalibrationRaw) -> Result<Self, Self::Error> {
        Self::new(
            raw.id,
            raw.drive_mode,
            raw.homing_offset,
            raw.range_min,
            raw.range_max,
        )
    }
}

impl MotorCalibration {
    /// # Errors
    ///
    /// - `homing_offset.abs() > 2047` → [`FeetechError::CalibrationOffsetOutOfRange`]
    /// - `range_min > 4095` → [`FeetechError::CalibrationMinOutOfRange`]
    /// - `range_max > 4095` → [`FeetechError::CalibrationMaxOutOfRange`]
    pub fn new(
        id: u8,
        drive_mode: u8,
        homing_offset: i32,
        range_min: u16,
        range_max: u16,
    ) -> Result<Self, FeetechError> {
        // STS3215 sign-magnitude 11-bit (registers.rs STS3215_SIGN_BIT["Homing_Offset"]=11)
        // → magnitude 上限 2047
        if homing_offset.unsigned_abs() > 2047 {
            return Err(FeetechError::CalibrationOffsetOutOfRange(homing_offset));
        }
        // 12-bit range，上限 = 4096 - 1 = STS3215_RESOLUTION - 1
        let max = STS3215_RESOLUTION - 1;
        if range_min > max {
            return Err(FeetechError::CalibrationMinOutOfRange(i32::from(range_min)));
        }
        if range_max > max {
            return Err(FeetechError::CalibrationMaxOutOfRange(i32::from(range_max)));
        }
        Ok(Self {
            id,
            drive_mode,
            homing_offset,
            range_min,
            range_max,
        })
    }

    /// 全开范围 + 零 offset，单元测试 / 初始化常用。
    #[must_use]
    pub fn identity(id: u8) -> Self {
        Self {
            id,
            drive_mode: 0,
            homing_offset: 0,
            range_min: 0,
            range_max: STS3215_RESOLUTION - 1, // 4095
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motor_id_valid_range() {
        assert_eq!(Motor::new(1).unwrap().id, 1);
        assert_eq!(Motor::new(127).unwrap().id, 127);
        assert_eq!(Motor::new(253).unwrap().id, 253);
    }

    #[test]
    fn motor_id_zero_rejected() {
        assert!(matches!(
            Motor::new(0),
            Err(FeetechError::InvalidMotorId(0))
        ));
    }

    #[test]
    fn motor_id_254_or_255_rejected() {
        for bad in [254u8, 255u8] {
            match Motor::new(bad) {
                Err(FeetechError::InvalidMotorId(n)) => assert_eq!(n, i32::from(bad)),
                other => panic!("id={bad} should fail, got {other:?}"),
            }
        }
    }

    #[test]
    fn calibration_identity_is_full_range() {
        let cal = MotorCalibration::identity(7);
        assert_eq!(cal.id, 7);
        assert_eq!(cal.drive_mode, 0);
        assert_eq!(cal.homing_offset, 0);
        assert_eq!(cal.range_min, 0);
        assert_eq!(cal.range_max, 4095);
    }

    #[test]
    fn calibration_homing_offset_boundary() {
        // 边界值通过
        assert!(MotorCalibration::new(1, 0, 2047, 0, 4095).is_ok());
        assert!(MotorCalibration::new(1, 0, -2047, 0, 4095).is_ok());
        // 超界拒绝
        assert!(matches!(
            MotorCalibration::new(1, 0, 2048, 0, 4095),
            Err(FeetechError::CalibrationOffsetOutOfRange(2048))
        ));
        assert!(matches!(
            MotorCalibration::new(1, 0, -2048, 0, 4095),
            Err(FeetechError::CalibrationOffsetOutOfRange(-2048))
        ));
    }

    #[test]
    fn calibration_range_boundary() {
        // 4095 (= RESOLUTION - 1) 通过
        assert!(MotorCalibration::new(1, 0, 0, 0, 4095).is_ok());
        assert!(MotorCalibration::new(1, 0, 0, 4095, 4095).is_ok());
        // 4096 拒绝
        assert!(matches!(
            MotorCalibration::new(1, 0, 0, 4096, 4095),
            Err(FeetechError::CalibrationMinOutOfRange(4096))
        ));
        assert!(matches!(
            MotorCalibration::new(1, 0, 0, 0, 4096),
            Err(FeetechError::CalibrationMaxOutOfRange(4096))
        ));
    }
}
