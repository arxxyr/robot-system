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
#[inline]
#[must_use]
pub fn rad_to_raw(rad: f32) -> i32 {
    let tick = rad_to_tick_offset(rad).round() as i32;
    tick + i32::from(STS3215_MAX_POSITION)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::{FRAC_PI_2, PI};

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
}
