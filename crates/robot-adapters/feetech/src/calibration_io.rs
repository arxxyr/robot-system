//! 校准文件 loader + 软限位 clamp helper。
//!
//! **与 [`crate::calibration`] 的区别**：
//! - [`crate::calibration`] 里的 `write_calibration` / `set_half_turn_homings`
//!   是 *写 EEPROM* 的路径，真动 motor 硬件永久状态。
//! - 本 module 只做 *内存层* 的加载 + 校验 + 软限位 clamp，**不碰硬件**。
//!   典型用途：adapter 启动时从 JSON 读出 `range_min/max` 缓存到内存，`write_command`
//!   热路径做 silent clamp；EEPROM 已由 dimos 侧一次性写入，不二次覆盖。
//!
//! ## JSON 格式
//!
//! 直接复用 dimos `scripts/calibrate.py` / `common/calibration.py` 的输出格式：
//!
//! ```json
//! {
//!   "left_arm_shoulder_pan": {
//!     "id": 1,
//!     "drive_mode": 0,
//!     "homing_offset": -428,
//!     "range_min": 620,
//!     "range_max": 3301
//!   },
//!   "left_arm_shoulder_lift": { ... }
//! }
//! ```
//!
//! - 顶层是 `motor_name → MotorCalibration` map
//! - `drive_mode` 缺省默认 0（见 [`crate::types::MotorCalibration`] 反序列化实现）
//! - range 超界 / offset 超界 / id 非法 → 从 `MotorCalibration::new` 返回的错误
//!   会被包进 [`FeetechError::CalibrationEntryInvalid`]
//!
//! ## 多文件合并
//!
//! XLeRobot 一条总线跨两个 JSON（bus1 = left_arm + head；bus2 = right_arm + base）。
//! [`load_and_merge`] 接受任意文件列表，合并成单一 map；motor name 重复 → 报
//! [`FeetechError::CalibrationDuplicateMotor`]，不允许静默覆盖（dimos 侧遇到重名只打
//! warning，我们更严格 —— 重名通常意味着配置错配，静默覆盖会让用户以为生效的是 A
//! 其实是 B）。
//!
//! ## 单位换算接口
//!
//! 纯物理常量的 raw ↔ rad 换算（不带 `direction` / `zero_offset_rad`）已经住在
//! [`crate::adapter`] 的 `raw_to_rad` / `rad_to_raw`。task 29 暂不再新增 helper —
//! adapter 层需要 per-joint direction / offset 时再上层叠加。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::FeetechError;
use crate::types::MotorCalibration;

/// 读取单个 calibration JSON 文件，返回 `motor_name → MotorCalibration` map。
///
/// # Errors
/// - 文件打不开 → [`FeetechError::CalibrationIo`]
/// - JSON 不合法 / 字段缺失 → [`FeetechError::CalibrationParse`]
/// - 单条记录校验失败（id 之外的 range / offset 超界）→ [`FeetechError::CalibrationEntryInvalid`]
pub fn load_file(
    path: impl AsRef<Path>,
) -> Result<HashMap<String, MotorCalibration>, FeetechError> {
    let path = path.as_ref();
    let text = fs::read_to_string(path).map_err(|source| FeetechError::CalibrationIo {
        path: path.to_path_buf(),
        source,
    })?;
    parse_str(path, &text)
}

/// 合并多个 calibration JSON 文件到单一 map。motor name 重复 → 报错。
///
/// # Errors
/// - 透传 [`load_file`] 的错误（附带具体是哪个文件）
/// - 跨文件 motor name 重复 → [`FeetechError::CalibrationDuplicateMotor`]
pub fn load_and_merge<P: AsRef<Path>>(
    paths: &[P],
) -> Result<HashMap<String, MotorCalibration>, FeetechError> {
    let mut merged: HashMap<String, MotorCalibration> = HashMap::new();
    // 记录每个 motor name 首次出现的文件路径，用于重名报错定位
    let mut origin: HashMap<String, PathBuf> = HashMap::new();

    for p in paths {
        let path = p.as_ref();
        let part = load_file(path)?;
        for (name, cal) in part {
            if let Some(first) = origin.get(&name) {
                return Err(FeetechError::CalibrationDuplicateMotor {
                    name,
                    first: first.clone(),
                    second: path.to_path_buf(),
                });
            }
            origin.insert(name.clone(), path.to_path_buf());
            merged.insert(name, cal);
        }
    }
    Ok(merged)
}

/// 把 raw 值夹到 `[lower, upper]`（软限位）。1:1 移植自 dimos `units.py::clamp_raw`。
///
/// `write_joint_positions` 热路径用：即使 EEPROM `Min/Max_Position_Limit` 已是双保险，
/// 软件层也默默夹一下，避免越界写多半 round-trip（dimos `_manipulator_base.py:422-426`）。
#[inline]
#[must_use]
pub fn clamp_raw(raw: i32, lower: i32, upper: i32) -> i32 {
    raw.max(lower).min(upper)
}

// ============================================================================
// 内部实现
// ============================================================================

/// 把 JSON 文本解析为 `motor_name → MotorCalibration`，走 `serde_json::Value`
/// 中转拿 per-entry 的精确错误位置 —— 让上层看到"是哪个 motor 出的问题"，比
/// serde 原生 derive 的 "unknown field foo at line X" 更有价值。
fn parse_str(path: &Path, text: &str) -> Result<HashMap<String, MotorCalibration>, FeetechError> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|source| FeetechError::CalibrationParse {
            path: path.to_path_buf(),
            source,
        })?;

    let map = value
        .as_object()
        .ok_or_else(|| FeetechError::CalibrationParse {
            path: path.to_path_buf(),
            source: serde::de::Error::custom("top-level JSON must be an object"),
        })?;

    let mut out = HashMap::with_capacity(map.len());
    for (name, entry) in map {
        // MotorCalibration 的 try_from = MotorCalibrationRaw 会自动触发校验
        // serde 把 TryFrom 返回的 FeetechError 用 `Display` 压成字符串塞进
        // `serde_json::Error`，拿不回原结构化 error。统一包成
        // `CalibrationEntryInvalid { reason: String }`，保留 serde 原文 —— 越界
        // 信息 / 缺字段信息都是人读得懂的 Display。
        let cal: MotorCalibration = serde_json::from_value(entry.clone()).map_err(|source| {
            FeetechError::CalibrationEntryInvalid {
                path: path.to_path_buf(),
                name: name.clone(),
                reason: source.to_string(),
            }
        })?;
        out.insert(name.clone(), cal);
    }
    Ok(out)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// 与 dimos `left_arm.json` 顶部两项同格式（不同取值）。
    const LEFT_ARM_FIXTURE: &str = r#"{
        "left_arm_shoulder_pan": {
            "id": 1,
            "drive_mode": 0,
            "homing_offset": -428,
            "range_min": 620,
            "range_max": 3301
        },
        "left_arm_shoulder_lift": {
            "id": 2,
            "drive_mode": 0,
            "homing_offset": 290,
            "range_min": 1086,
            "range_max": 3410
        }
    }"#;

    /// 与 dimos `base.json` 风格一致：无 homing_offset / range_min / range_max 要求（实际上必须有，
    /// base motor 都是 0 / 4095）。drive_mode 字段缺省测试。
    const BASE_FIXTURE: &str = r#"{
        "base_left_wheel": {
            "id": 7,
            "homing_offset": 0,
            "range_min": 0,
            "range_max": 4095
        },
        "base_back_wheel": {
            "id": 8,
            "homing_offset": 0,
            "range_min": 0,
            "range_max": 4095
        }
    }"#;

    fn write_tempfile(body: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn load_file_parses_left_arm_fixture() {
        let f = write_tempfile(LEFT_ARM_FIXTURE);
        let cals = load_file(f.path()).unwrap();
        assert_eq!(cals.len(), 2);

        let pan = cals.get("left_arm_shoulder_pan").unwrap();
        assert_eq!(pan.id, 1);
        assert_eq!(pan.drive_mode, 0);
        assert_eq!(pan.homing_offset, -428);
        assert_eq!(pan.range_min, 620);
        assert_eq!(pan.range_max, 3301);

        let lift = cals.get("left_arm_shoulder_lift").unwrap();
        assert_eq!(lift.id, 2);
        assert_eq!(lift.homing_offset, 290);
    }

    #[test]
    fn load_file_drive_mode_defaults_to_zero() {
        let f = write_tempfile(BASE_FIXTURE);
        let cals = load_file(f.path()).unwrap();
        assert_eq!(cals.get("base_left_wheel").unwrap().drive_mode, 0);
        assert_eq!(cals.get("base_back_wheel").unwrap().drive_mode, 0);
    }

    #[test]
    fn load_file_missing_returns_io_error() {
        let missing = Path::new("/nonexistent/does_not_exist.json");
        match load_file(missing) {
            Err(FeetechError::CalibrationIo { path, .. }) => {
                assert_eq!(path, missing);
            }
            other => panic!("expected CalibrationIo, got {other:?}"),
        }
    }

    #[test]
    fn load_file_malformed_json_returns_parse_error() {
        let f = write_tempfile("{not valid json");
        match load_file(f.path()) {
            Err(FeetechError::CalibrationParse { path, .. }) => {
                assert_eq!(path, f.path());
            }
            other => panic!("expected CalibrationParse, got {other:?}"),
        }
    }

    #[test]
    fn load_file_non_object_top_level_rejected() {
        let f = write_tempfile("[1, 2, 3]");
        match load_file(f.path()) {
            Err(FeetechError::CalibrationParse { .. }) => {}
            other => panic!("expected CalibrationParse, got {other:?}"),
        }
    }

    #[test]
    fn load_file_out_of_range_homing_offset_rejected() {
        // 2048 超 [-2047, +2047]
        let bad = r#"{
            "m": {"id": 1, "drive_mode": 0, "homing_offset": 2048, "range_min": 0, "range_max": 4095}
        }"#;
        let f = write_tempfile(bad);
        match load_file(f.path()) {
            Err(FeetechError::CalibrationEntryInvalid { name, .. }) => {
                assert_eq!(name, "m");
            }
            other => panic!("expected CalibrationEntryInvalid, got {other:?}"),
        }
    }

    #[test]
    fn load_file_range_min_over_4095_rejected() {
        let bad = r#"{
            "m": {"id": 1, "drive_mode": 0, "homing_offset": 0, "range_min": 5000, "range_max": 4095}
        }"#;
        let f = write_tempfile(bad);
        match load_file(f.path()) {
            // u16 上限是 65535，解析过；new() 里的 4095 上限会触发。
            Err(FeetechError::CalibrationEntryInvalid { name, .. }) => assert_eq!(name, "m"),
            other => panic!("expected CalibrationEntryInvalid, got {other:?}"),
        }
    }

    #[test]
    fn load_file_range_over_u16_max_is_parse_error() {
        // 99999 > u16::MAX (65535) → serde 解析阶段就拒
        let bad = r#"{
            "m": {"id": 1, "drive_mode": 0, "homing_offset": 0, "range_min": 99999, "range_max": 4095}
        }"#;
        let f = write_tempfile(bad);
        match load_file(f.path()) {
            Err(FeetechError::CalibrationEntryInvalid { name, .. }) => assert_eq!(name, "m"),
            other => panic!("expected CalibrationEntryInvalid, got {other:?}"),
        }
    }

    #[test]
    fn load_and_merge_combines_bus1_files() {
        let left = write_tempfile(LEFT_ARM_FIXTURE);
        let base = write_tempfile(BASE_FIXTURE);
        let merged = load_and_merge(&[left.path(), base.path()]).unwrap();
        assert_eq!(merged.len(), 4);
        // 来自 left_arm
        assert!(merged.contains_key("left_arm_shoulder_pan"));
        // 来自 base
        assert!(merged.contains_key("base_left_wheel"));
    }

    #[test]
    fn load_and_merge_duplicate_motor_name_rejected() {
        // 两文件都给了 "same_motor"
        let dup =
            r#"{"same_motor": {"id": 1, "homing_offset": 0, "range_min": 0, "range_max": 4095}}"#;
        let a = write_tempfile(dup);
        let b = write_tempfile(dup);
        match load_and_merge(&[a.path(), b.path()]) {
            Err(FeetechError::CalibrationDuplicateMotor {
                name,
                first,
                second,
                ..
            }) => {
                assert_eq!(name, "same_motor");
                assert_eq!(first, a.path());
                assert_eq!(second, b.path());
            }
            other => panic!("expected CalibrationDuplicateMotor, got {other:?}"),
        }
    }

    #[test]
    fn clamp_raw_inside_range_returns_value() {
        assert_eq!(clamp_raw(2000, 1000, 3000), 2000);
    }

    #[test]
    fn clamp_raw_below_lower_bound_clamps_up() {
        assert_eq!(clamp_raw(500, 1000, 3000), 1000);
    }

    #[test]
    fn clamp_raw_above_upper_bound_clamps_down() {
        assert_eq!(clamp_raw(5000, 1000, 3000), 3000);
    }

    #[test]
    fn clamp_raw_negative_bounds_work() {
        // sign-magnitude 解码后的 raw 可以是负数（homing_offset 应用后）
        assert_eq!(clamp_raw(-500, -1000, 1000), -500);
        assert_eq!(clamp_raw(-1500, -1000, 1000), -1000);
    }

    #[test]
    fn clamp_raw_equal_bounds_pins_value() {
        // 退化情况：range_min == range_max（theoretical "锁死"），始终返回该值
        assert_eq!(clamp_raw(999, 2047, 2047), 2047);
        assert_eq!(clamp_raw(3000, 2047, 2047), 2047);
    }
}
