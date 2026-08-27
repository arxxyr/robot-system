//! 冻结基线 12 维原始形态 → rad 导入 shim（**测试侧**；v0.5 候选 E 段(6)，
//! Spec D11 "基线导入 shim" + QA5 typed 化）。
//!
//! ## 输入形态（冻结 `executor.py:39-68` `ACTION_DIM_TO_JOINT` +
//!    `action_to_positions`）
//!
//! | idx | 含义 | 单位 |
//! |---|---|---|
//! | 0-4 | `left_arm_{shoulder_pan, shoulder_lift, elbow_flex, wrist_flex, wrist_roll}` | 度 |
//! | 5 | `left_arm_gripper` | `[0, 100]` |
//! | 6-10 | `right_arm_*` 同构 | 度 |
//! | 11 | `right_arm_gripper` | `[0, 100]` |
//!
//! ## 镜像换算（1:1 dimos，非等价宣称范围之外）
//!
//! - 臂：`rad = deg × π/180`。
//! - 夹爪：`v ∈ [0,100]` 越界 **clamp 到 [0,100]**（镜像 dimos `np.clip` +
//!   warn，本 shim 以 [`Shim12::clamp_count`] 计数）→ `u = v / 100` →
//!   `rad = rad_lo + u × (rad_hi − rad_lo)`。**左右夹爪标定范围不同**，两组
//!   `(rad_lo, rad_hi)` 必填（QA5）——由调用方用 feetech 公共换算 API
//!   （`robot_adapter_feetech::raw_to_rad`）从标定 JSON `range_min/max` 派生，
//!   **本 crate 不复制 raw↔rad 公式**（D11 换算权威唯一）。
//! - NaN / Inf 任一元素 → reject；`rad_lo >= rad_hi` 或非有限 → reject
//!   （range 反转 fail-fast；标定派生处 + 段(9) 真机侧再锁）。
//!
//! "度" / `[0,100]` **只存在于本层输入**，E 运行时（`client::trajectory` /
//! S1 / S0）全程 rad-native。

use thiserror::Error;

/// 冻结基线 action 维度。
pub const BASELINE_DIM: usize = 12;
/// 每臂维度（5 臂关节 + 1 夹爪 = D2(i) 6 维臂向量）。
pub const ARM_DIM: usize = 6;
/// 左夹爪索引（`[0,100]` 列）。
pub const LEFT_GRIPPER_IDX: usize = 5;
/// 右夹爪索引（`[0,100]` 列）。
pub const RIGHT_GRIPPER_IDX: usize = 11;
/// 夹爪原始量程 `[0, 100]`。
pub const GRIPPER_RAW_MAX: f64 = 100.0;

/// 夹爪侧别（错误定位用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GripperSide {
    Left,
    Right,
}

/// [`raw12_to_rad`] 输出：12 维 rad + 夹爪 clamp 计数。
#[derive(Debug, Clone, PartialEq)]
pub struct Shim12 {
    /// 全 12 维 rad（索引与输入同构）。
    pub values: [f64; BASELINE_DIM],
    /// 夹爪原始值越出 `[0,100]` 被 clamp 的次数（0 / 1 / 2）。
    pub clamp_count: u32,
}

impl Shim12 {
    /// 左臂 6 维（idx 0-5，含夹爪）——与 `configs/xlerobot_left_arm.toml`
    /// joints 顺序同构。
    #[must_use]
    pub fn left_arm(&self) -> [f64; ARM_DIM] {
        let mut out = [0.0; ARM_DIM];
        out.copy_from_slice(&self.values[..ARM_DIM]);
        out
    }

    /// 右臂 6 维（idx 6-11，含夹爪）——与 `configs/xlerobot_right_arm.toml`
    /// joints 顺序同构。
    #[must_use]
    pub fn right_arm(&self) -> [f64; ARM_DIM] {
        let mut out = [0.0; ARM_DIM];
        out.copy_from_slice(&self.values[ARM_DIM..]);
        out
    }
}

/// [`raw12_to_rad`] typed 拒绝原因。
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ShimError {
    #[error("raw[{idx}] is not finite ({value})")]
    NonFinite { idx: usize, value: f64 },
    #[error("{side:?} gripper rad range not finite: lo={lo}, hi={hi}")]
    RangeNonFinite { side: GripperSide, lo: f64, hi: f64 },
    #[error("{side:?} gripper rad range inverted or empty: lo={lo} >= hi={hi}")]
    RangeInverted { side: GripperSide, lo: f64, hi: f64 },
}

/// 冻结 12 维原始向量（臂 度 + 夹爪 `[0,100]`）→ 12 维 rad（契约详模块 doc）。
///
/// # Errors
///
/// [`ShimError`]：range 非有限 / 反转（先于逐元素校验，fail-fast）；任一
/// 元素 NaN / Inf。
pub fn raw12_to_rad(
    raw: &[f64; BASELINE_DIM],
    left_gripper_rad: (f64, f64),
    right_gripper_rad: (f64, f64),
) -> Result<Shim12, ShimError> {
    validate_range(GripperSide::Left, left_gripper_rad)?;
    validate_range(GripperSide::Right, right_gripper_rad)?;
    if let Some((idx, &value)) = raw.iter().enumerate().find(|(_, v)| !v.is_finite()) {
        return Err(ShimError::NonFinite { idx, value });
    }

    let mut values = [0.0; BASELINE_DIM];
    let mut clamp_count = 0_u32;
    for (idx, &v) in raw.iter().enumerate() {
        values[idx] = match idx {
            LEFT_GRIPPER_IDX => gripper_to_rad(v, left_gripper_rad, &mut clamp_count),
            RIGHT_GRIPPER_IDX => gripper_to_rad(v, right_gripper_rad, &mut clamp_count),
            _ => v.to_radians(),
        };
    }
    Ok(Shim12 {
        values,
        clamp_count,
    })
}

fn validate_range(side: GripperSide, (lo, hi): (f64, f64)) -> Result<(), ShimError> {
    if !lo.is_finite() || !hi.is_finite() {
        return Err(ShimError::RangeNonFinite { side, lo, hi });
    }
    if lo >= hi {
        return Err(ShimError::RangeInverted { side, lo, hi });
    }
    Ok(())
}

/// `[0,100]`（越界 clamp + 计数）→ `[0,1]` → `rad_lo + u × (rad_hi − rad_lo)`。
fn gripper_to_rad(v: f64, (lo, hi): (f64, f64), clamp_count: &mut u32) -> f64 {
    let clamped = v.clamp(0.0, GRIPPER_RAW_MAX);
    if (clamped - v).abs() > 0.0 {
        *clamp_count += 1;
    }
    let u = clamped / GRIPPER_RAW_MAX;
    lo + u * (hi - lo)
}

// ============================================================================
// Tests（AC-E10：镜像换算 + clamp 计数 + range 反转 fail-fast + 左右分范围）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    const LEFT: (f64, f64) = (-1.0, 1.5);
    const RIGHT: (f64, f64) = (-1.02, 1.23);

    fn raw_with(left_g: f64, right_g: f64) -> [f64; BASELINE_DIM] {
        let mut raw = [0.0; BASELINE_DIM];
        for (i, v) in raw.iter_mut().enumerate() {
            *v = f64::from(u8::try_from(i).unwrap()) * 10.0; // 0,10,20,… 度
        }
        raw[LEFT_GRIPPER_IDX] = left_g;
        raw[RIGHT_GRIPPER_IDX] = right_g;
        raw
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn arm_degrees_mirror_pi_over_180() {
        let shim = raw12_to_rad(&raw_with(0.0, 0.0), LEFT, RIGHT).unwrap();
        for idx in (0..5).chain(6..11) {
            let deg = f64::from(u8::try_from(idx).unwrap()) * 10.0;
            assert!(approx(shim.values[idx], deg * PI / 180.0), "idx {idx}");
        }
        assert_eq!(shim.clamp_count, 0);
    }

    #[test]
    fn gripper_endpoints_and_midpoint_use_side_specific_ranges() {
        let s = raw12_to_rad(&raw_with(0.0, 100.0), LEFT, RIGHT).unwrap();
        assert!(
            approx(s.values[LEFT_GRIPPER_IDX], LEFT.0),
            "0 → rad_lo（左）"
        );
        assert!(
            approx(s.values[RIGHT_GRIPPER_IDX], RIGHT.1),
            "100 → rad_hi（右）"
        );
        let s = raw12_to_rad(&raw_with(50.0, 25.0), LEFT, RIGHT).unwrap();
        assert!(approx(
            s.values[LEFT_GRIPPER_IDX],
            LEFT.0 + 0.5 * (LEFT.1 - LEFT.0)
        ));
        assert!(approx(
            s.values[RIGHT_GRIPPER_IDX],
            RIGHT.0 + 0.25 * (RIGHT.1 - RIGHT.0)
        ));
        assert_eq!(s.clamp_count, 0);
    }

    #[test]
    fn gripper_out_of_range_is_clamped_and_counted() {
        let s = raw12_to_rad(&raw_with(-5.0, 250.0), LEFT, RIGHT).unwrap();
        assert!(
            approx(s.values[LEFT_GRIPPER_IDX], LEFT.0),
            "-5 → clamp 0 → rad_lo"
        );
        assert!(
            approx(s.values[RIGHT_GRIPPER_IDX], RIGHT.1),
            "250 → clamp 100 → rad_hi"
        );
        assert_eq!(s.clamp_count, 2);
        let s = raw12_to_rad(&raw_with(100.0001, 40.0), LEFT, RIGHT).unwrap();
        assert_eq!(s.clamp_count, 1, "仅左越界");
        let s = raw12_to_rad(&raw_with(0.0, 100.0), LEFT, RIGHT).unwrap();
        assert_eq!(s.clamp_count, 0, "边界值不计 clamp");
    }

    #[test]
    fn arm_degrees_are_not_clamped() {
        // D11：臂不 clamp（越界由 S0 calibration clamp 兜底）；shim 只镜像换算。
        let mut raw = raw_with(0.0, 0.0);
        raw[0] = 720.0;
        let s = raw12_to_rad(&raw, LEFT, RIGHT).unwrap();
        assert!(approx(s.values[0], 4.0 * PI));
        assert_eq!(s.clamp_count, 0);
    }

    #[test]
    fn non_finite_element_rejected_arm_and_gripper() {
        for (idx, bad) in [
            (0, f64::NAN),
            (7, f64::INFINITY),
            (5, f64::NEG_INFINITY),
            (11, f64::NAN),
        ] {
            let mut raw = raw_with(0.0, 0.0);
            raw[idx] = bad;
            assert!(matches!(
                raw12_to_rad(&raw, LEFT, RIGHT).unwrap_err(),
                ShimError::NonFinite { idx: i, .. } if i == idx
            ));
        }
    }

    #[test]
    fn range_inverted_or_empty_fails_fast_per_side() {
        let raw = raw_with(0.0, 0.0);
        assert!(matches!(
            raw12_to_rad(&raw, (1.0, 1.0), RIGHT).unwrap_err(),
            ShimError::RangeInverted {
                side: GripperSide::Left,
                ..
            }
        ));
        assert!(matches!(
            raw12_to_rad(&raw, LEFT, (0.5, -0.5)).unwrap_err(),
            ShimError::RangeInverted {
                side: GripperSide::Right,
                ..
            }
        ));
        // range 校验先于元素校验（fail-fast）：NaN 元素 + 反转 range → 报 range。
        let mut bad = raw;
        bad[0] = f64::NAN;
        assert!(matches!(
            raw12_to_rad(&bad, (2.0, 1.0), RIGHT).unwrap_err(),
            ShimError::RangeInverted { .. }
        ));
    }

    #[test]
    fn range_non_finite_rejected() {
        let raw = raw_with(0.0, 0.0);
        assert!(matches!(
            raw12_to_rad(&raw, (f64::NAN, 1.0), RIGHT).unwrap_err(),
            ShimError::RangeNonFinite {
                side: GripperSide::Left,
                ..
            }
        ));
        assert!(matches!(
            raw12_to_rad(&raw, LEFT, (0.0, f64::INFINITY)).unwrap_err(),
            ShimError::RangeNonFinite {
                side: GripperSide::Right,
                ..
            }
        ));
    }

    #[test]
    fn split_arms_matches_config_order() {
        let s = raw12_to_rad(&raw_with(0.0, 100.0), LEFT, RIGHT).unwrap();
        let left = s.left_arm();
        let right = s.right_arm();
        assert_eq!(left.len(), ARM_DIM);
        assert_eq!(&left[..], &s.values[..6]);
        assert_eq!(&right[..], &s.values[6..]);
        assert!(approx(left[5], LEFT.0));
        assert!(approx(right[5], RIGHT.1));
    }

    #[test]
    fn dimension_constants_lock_frozen_layout() {
        assert_eq!(BASELINE_DIM, 12);
        assert_eq!(ARM_DIM * 2, BASELINE_DIM);
        assert_eq!(LEFT_GRIPPER_IDX, ARM_DIM - 1);
        assert_eq!(RIGHT_GRIPPER_IDX, BASELINE_DIM - 1);
    }
}
