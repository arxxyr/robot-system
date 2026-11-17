//! STS3215 `raw ↔ rad` 纯换算（**公式单源**；v0.5 候选 E 段(6)，Spec D11
//! "换算权威唯一" + P.2.6）。
//!
//! 原本是 `adapter.rs` 私有 fn（`raw_to_rad` / `rad_to_raw`）；段(6) 提升为
//! 本模块 `pub` 纯函数，adapter 热路径改调同一份实现，**其它 crate 不得复制
//! 公式**（robot-rpc client `baseline_shim` 只收调用方算好的 rad_lo/rad_hi；
//! 真机验收 bin 用本 API 从标定 JSON `range_min/max` 派生）。
//!
//! ## 物理常量（与 dimos `units.py:31-50` 同源）
//!
//! - 12-bit：`STS3215_RESOLUTION = 4096` tick / rev。
//! - 中点：`STS3215_MAX_POSITION = 2047` ↔ 0 rad。
//! - `rad = (raw − 2047) × 2π / 4096`；`raw = round(rad × 4096 / 2π) + 2047`。
//! - **不**带 per-joint `direction` / `zero_offset_rad`（dimos 有，本仓 S0
//!   标定层用 `homing_offset` 在 EEPROM 侧处理；adapter 层需要时再上层叠加）。
//!
//! 无 feature gate、无 IO、`f32` 精度（与 `JointStateBuf` / `JointCommandView`
//! 同型）；调用方需要 `f64` 时自行上转。

use std::f32::consts::TAU;

use crate::registers::{STS3215_MAX_POSITION, STS3215_RESOLUTION};

/// 12-bit tick → rad：`tick / 4096 × 2π`。
pub const RAD_PER_TICK: f32 = TAU / (STS3215_RESOLUTION as f32);

/// rad → 12-bit tick（**绝对值**，未加中点偏移；`f32`，未取整）。
#[inline]
#[must_use]
pub fn rad_to_tick_offset(rad: f32) -> f32 {
    rad / RAD_PER_TICK
}

/// `raw_position`（sign-magnitude 解码后的值，可为负）→ rad。
///
/// `raw == 2047` ↔ `0.0`；`raw == 2047 + 1024` ↔ `+π/2`。
#[inline]
#[must_use]
pub fn raw_to_rad(raw: i32) -> f32 {
    (raw - i32::from(STS3215_MAX_POSITION)) as f32 * RAD_PER_TICK
}

/// rad → `raw_position`（带符号；写到 sync_write 前由 bus 自动 sign-magnitude
/// 编码）。四舍五入到最近 tick（`f32::round`，半数远离零）。
/// 超出整数范围时饱和，交由既有校准夹紧或协议编码边界处理；调用方须先拒绝非有限值。
#[inline]
#[must_use]
pub fn rad_to_raw(rad: f32) -> i32 {
    let tick = rad_to_tick_offset(rad).round() as i32;
    tick.saturating_add(i32::from(STS3215_MAX_POSITION))
}

/// 从原校准派生的运动范围计算原位保持允许范围，不改变主动目标限位。
/// 零容差保留原范围；非零容差要求端点就是可往返的 12-bit f32 校准值。
/// 只扩展范围并在 raw 硬边界饱和，不裁剪实际位置或目标。
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    reason = "先核有限值，再验证 f32 与 raw 精确往返"
)]
pub fn calibrated_hold_range(min_rad: f64, max_rad: f64, tolerance_raw: u16) -> Option<(f64, f64)> {
    if !min_rad.is_finite() || !max_rad.is_finite() || min_rad >= max_rad || tolerance_raw > 4095 {
        return None;
    }
    if tolerance_raw == 0 {
        return Some((min_rad, max_rad));
    }
    let endpoint = |position: f64| {
        let raw = rad_to_raw(position as f32);
        ((0..=4095).contains(&raw) && f64::from(raw_to_rad(raw)) == position).then_some(raw)
    };
    let lower_ticks = endpoint(min_rad)?;
    let upper_ticks = endpoint(max_rad)?;
    let tolerance = i32::from(tolerance_raw);
    Some((
        f64::from(raw_to_rad((lower_ticks - tolerance).max(0))),
        f64::from(raw_to_rad((upper_ticks + tolerance).min(4095))),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::{FRAC_PI_2, PI};

    #[test]
    fn calibrated_hold_bounds_preserve_f32_endpoints_and_raw_saturation() {
        let rad = |raw| f64::from(raw_to_rad(raw));
        let (min, max) = calibrated_hold_range(rad(812), rad(2991), 20).unwrap();
        assert_eq!((min, max), (rad(792), rad(3011)));
        for raw in [792, 812, 2991, 2998, 3011] {
            assert!((min..=max).contains(&rad(raw)));
        }
        for raw in [791, 3012] {
            assert!(!(min..=max).contains(&rad(raw)));
        }
        assert_eq!(
            calibrated_hold_range(rad(1), rad(4094), 20),
            Some((rad(0), rad(4095)))
        );
        assert_eq!(calibrated_hold_range(-0.1, 0.1, 0), Some((-0.1, 0.1)));
        assert_eq!(calibrated_hold_range(-0.1, 0.1, 20), None);
        assert_eq!(calibrated_hold_range(rad(-1), rad(4095), 20), None);
        assert_eq!(calibrated_hold_range(rad(0), rad(4096), 20), None);
        assert_eq!(calibrated_hold_range(rad(0), rad(4095), 4096), None);
        assert_eq!(calibrated_hold_range(f64::NAN, rad(4095), 0), None);
    }

    #[test]
    fn rad_per_tick_matches_resolution() {
        assert!((RAD_PER_TICK - TAU / 4096.0).abs() < 1e-9);
        assert!((rad_to_tick_offset(TAU) - 4096.0).abs() < 1e-3);
    }

    #[test]
    fn center_is_zero_rad() {
        assert!((raw_to_rad(2047) - 0.0).abs() < 1e-6);
        assert_eq!(rad_to_raw(0.0), 2047);
    }

    #[test]
    fn quarter_turn_is_1024_ticks() {
        assert!((raw_to_rad(2047 + 1024) - FRAC_PI_2).abs() < 1e-3);
        assert_eq!(rad_to_raw(FRAC_PI_2), 2047 + 1024);
        assert!((raw_to_rad(2047 - 1024) + FRAC_PI_2).abs() < 1e-3);
        assert_eq!(rad_to_raw(-FRAC_PI_2), 2047 - 1024);
    }

    #[test]
    fn full_12bit_range_round_trips_exactly() {
        // 标定 range_min/max ∈ [0, 4095]：raw → rad → raw 逐 tick 恒等
        // （f32 在 ±2048 tick 量级下每 tick 间距远大于舍入误差）。
        for raw in 0..=4095_i32 {
            assert_eq!(rad_to_raw(raw_to_rad(raw)), raw, "raw={raw}");
        }
    }

    #[test]
    fn negative_raw_after_sign_magnitude_decode() {
        // sign-magnitude 解码后可为负（bus 层语义）；公式对负值同样线性。
        let r = raw_to_rad(-1);
        assert!((r - (-2048.0 * RAD_PER_TICK)).abs() < 1e-6);
        assert_eq!(rad_to_raw(r), -1);
    }

    #[test]
    fn rounding_is_nearest_tick() {
        // 0.4 tick → 向下；0.6 tick → 向上。
        assert_eq!(rad_to_raw(0.4 * RAD_PER_TICK), 2047);
        assert_eq!(rad_to_raw(0.6 * RAD_PER_TICK), 2048);
        assert_eq!(rad_to_raw(-0.6 * RAD_PER_TICK), 2046);
    }

    #[test]
    fn half_turn_maps_to_full_scale_edges() {
        // +π → 2047 + 2048 = 4095（12-bit 顶）；−π → −1（越过 0，由标定 clamp 兜）。
        assert_eq!(rad_to_raw(PI), 4095);
        assert_eq!(rad_to_raw(-PI), -1);
    }

    #[test]
    fn finite_extreme_targets_do_not_overflow_before_calibration_clamp() {
        assert_eq!(rad_to_raw(f32::MAX), i32::MAX);
        assert_eq!(rad_to_raw(-f32::MAX), i32::MIN + 2047);
        // 原先正向整数转换饱和后再加中点会溢出；超过边界前后保持单调。
        let near_limit = (i32::MAX as f32 - 4096.0) * RAD_PER_TICK;
        assert!(rad_to_raw(near_limit) <= rad_to_raw(near_limit + TAU));
    }
}
