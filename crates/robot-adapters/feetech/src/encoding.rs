//! sign-magnitude 编解码 + LE byte 拆合，对应 dimos `feetech_io/encoding.py` 全 82 行。
//!
//! Feetech STS 系列寄存器值的编码规则（**非二补码**）：
//! - 最高位 (`sign_bit_index`) = 方向位（1 = 负）
//! - 余下位 = magnitude（绝对值）
//!
//! 例：`Homing_Offset` `sign_bit_index = 11`：
//! - 最大 magnitude = `(1 << 11) - 1 = 2047`
//! - `-1` 编码 = `0x801`（1000 0000 0001）— **不是** `0xFFFF`
//!
//! 字节序：little-endian。

use crate::error::FeetechError;

// ============================================================================
// sign-magnitude
// ============================================================================

/// 把 *带符号* 整数编码成 *无符号* sign-magnitude 表示。
///
/// `sign_bit_index` 给寄存器允许的最大 bit 位置（详见 `STS3215_SIGN_BIT` 表）。
///
/// # Errors
///
/// `value.abs() > (1 << sign_bit_index) - 1` → [`FeetechError::MagnitudeOutOfRange`]。
///
/// # Panics
///
/// `sign_bit_index >= 32`（实际不会发生：最大寄存器是 16-bit / sign_bit=15）。
pub fn encode_sign_magnitude(value: i32, sign_bit_index: u32) -> Result<u32, FeetechError> {
    assert!(
        sign_bit_index < 32,
        "sign_bit_index {sign_bit_index} >= 32 不支持"
    );
    let max_magnitude: u32 = (1u32 << sign_bit_index) - 1;
    let magnitude: u32 = value.unsigned_abs();
    if magnitude > max_magnitude {
        return Err(FeetechError::MagnitudeOutOfRange {
            magnitude: i64::from(value).abs(),
            max: max_magnitude,
            sign_bit_index,
        });
    }
    let direction_bit: u32 = u32::from(value < 0);
    Ok((direction_bit << sign_bit_index) | magnitude)
}

/// 反向解码：sign-magnitude 无符号 → 带符号。永不失败（任意 u32 都是合法编码）。
#[must_use]
pub fn decode_sign_magnitude(encoded: u32, sign_bit_index: u32) -> i32 {
    assert!(
        sign_bit_index < 32,
        "sign_bit_index {sign_bit_index} >= 32 不支持"
    );
    let direction_bit = (encoded >> sign_bit_index) & 1;
    let magnitude_mask = (1u32 << sign_bit_index) - 1;
    let magnitude = encoded & magnitude_mask;
    if direction_bit == 1 {
        // 用 i64 中转避免 magnitude 在边界值 (2^31 - 1) 时溢出
        -(i64::from(magnitude) as i32)
    } else {
        magnitude as i32
    }
}

// ============================================================================
// 字节序：LE byte 拆合
// ============================================================================

/// 把 *无符号* 整数拆成 little-endian 字节，写入 `out` 起始 `length` 字节。
///
/// 对带符号寄存器先用 [`encode_sign_magnitude`] 转 unsigned 再拆。
///
/// # Errors
///
/// - `value` 超出 `length` 字节能表达 → [`FeetechError::ValueExceedsByteRange`]
/// - `length` 不在 \{1, 2, 4\} → [`FeetechError::UnsupportedByteLength`]
/// - `out.len() < length` → [`FeetechError::ByteLengthMismatch`]
pub fn split_bytes_le(value: u32, length: usize, out: &mut [u8]) -> Result<(), FeetechError> {
    if out.len() < length {
        return Err(FeetechError::ByteLengthMismatch {
            expected: length,
            actual: out.len(),
        });
    }
    let max: u32 = match length {
        1 => 0xFF,
        2 => 0xFFFF,
        4 => u32::MAX,
        _ => return Err(FeetechError::UnsupportedByteLength(length)),
    };
    if length < 4 && value > max {
        return Err(FeetechError::ValueExceedsByteRange {
            value,
            max_bytes: length,
            max,
        });
    }
    for (i, byte) in out.iter_mut().take(length).enumerate() {
        *byte = u8::try_from((value >> (i * 8)) & 0xFF).unwrap_or(0);
    }
    Ok(())
}

/// LE byte → unsigned。`length ∈ {1, 2, 4}`。
///
/// # Errors
///
/// - `data.len() != length` → [`FeetechError::ByteLengthMismatch`]
/// - `length` 不在 \{1, 2, 4\} → [`FeetechError::UnsupportedByteLength`]
pub fn combine_bytes_le(data: &[u8], length: usize) -> Result<u32, FeetechError> {
    if data.len() != length {
        return Err(FeetechError::ByteLengthMismatch {
            expected: length,
            actual: data.len(),
        });
    }
    if !matches!(length, 1 | 2 | 4) {
        return Err(FeetechError::UnsupportedByteLength(length));
    }
    let mut result: u32 = 0;
    for (i, &byte) in data.iter().enumerate() {
        result |= u32::from(byte) << (i * 8);
    }
    Ok(result)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── sign-magnitude ────────────────────────────────────────────────

    #[test]
    fn sign_magnitude_zero_no_direction_bit() {
        // 0 编码 = 0；任何 sign_bit_index 都成立
        assert_eq!(encode_sign_magnitude(0, 11).unwrap(), 0);
        assert_eq!(encode_sign_magnitude(0, 15).unwrap(), 0);
        assert_eq!(decode_sign_magnitude(0, 11), 0);
        assert_eq!(decode_sign_magnitude(0, 15), 0);
    }

    #[test]
    fn sign_magnitude_positive_uses_lower_bits() {
        // sign_bit=11: 100 编码 = 100（direction_bit=0）
        assert_eq!(encode_sign_magnitude(100, 11).unwrap(), 100);
        assert_eq!(decode_sign_magnitude(100, 11), 100);
    }

    #[test]
    fn sign_magnitude_negative_one_is_0x801_for_sign_bit_11() {
        // 教科书例：-1 编码 = (1 << 11) | 1 = 0x801（**不是** 0xFFFF）
        assert_eq!(encode_sign_magnitude(-1, 11).unwrap(), 0x801);
        assert_eq!(decode_sign_magnitude(0x801, 11), -1);
    }

    #[test]
    fn sign_magnitude_negative_one_is_0x8001_for_sign_bit_15() {
        assert_eq!(encode_sign_magnitude(-1, 15).unwrap(), 0x8001);
        assert_eq!(decode_sign_magnitude(0x8001, 15), -1);
    }

    #[test]
    fn sign_magnitude_boundary_values() {
        // sign_bit=11: magnitude max = 2047
        assert_eq!(encode_sign_magnitude(2047, 11).unwrap(), 2047);
        assert_eq!(encode_sign_magnitude(-2047, 11).unwrap(), 0x801 + 2046); // = 0xFFF
        assert_eq!(decode_sign_magnitude(0xFFF, 11), -2047);
        // 越界
        assert!(matches!(
            encode_sign_magnitude(2048, 11),
            Err(FeetechError::MagnitudeOutOfRange { .. })
        ));
        assert!(matches!(
            encode_sign_magnitude(-2048, 11),
            Err(FeetechError::MagnitudeOutOfRange { .. })
        ));
    }

    #[test]
    fn sign_magnitude_roundtrip_random_set() {
        // 把 sign_bit=15 范围内若干典型值 encode-decode 验回
        for v in [
            0_i32, 1, -1, 100, -100, 4095, -4095, 16384, -16384, 32767, -32767,
        ] {
            let enc = encode_sign_magnitude(v, 15).unwrap();
            let dec = decode_sign_magnitude(enc, 15);
            assert_eq!(dec, v, "roundtrip for {v}");
        }
    }

    #[test]
    fn sign_magnitude_present_position_use_case() {
        // dimos 06_lerobot_replace_spec.md §1.5 例：Present_Position 在 0 边界跨越
        // 时取负值。Present = Actual - Homing_Offset → 100 - 500 = -400
        let raw = encode_sign_magnitude(-400, 15).unwrap();
        assert_eq!(raw, 0x8000 | 400);
        assert_eq!(decode_sign_magnitude(raw, 15), -400);
    }

    // ── LE byte split / combine ───────────────────────────────────────

    #[test]
    fn split_one_byte() {
        let mut out = [0u8; 1];
        split_bytes_le(0xAB, 1, &mut out).unwrap();
        assert_eq!(out, [0xAB]);
    }

    #[test]
    fn split_two_byte_little_endian() {
        let mut out = [0u8; 2];
        split_bytes_le(0xABCD, 2, &mut out).unwrap();
        assert_eq!(out, [0xCD, 0xAB]); // LE: low byte first
    }

    #[test]
    fn split_four_byte_little_endian() {
        let mut out = [0u8; 4];
        split_bytes_le(0xDEAD_BEEF, 4, &mut out).unwrap();
        assert_eq!(out, [0xEF, 0xBE, 0xAD, 0xDE]);
    }

    #[test]
    fn split_one_byte_overflow_rejected() {
        let mut out = [0u8; 1];
        assert!(matches!(
            split_bytes_le(0x100, 1, &mut out),
            Err(FeetechError::ValueExceedsByteRange { .. })
        ));
    }

    #[test]
    fn split_two_byte_overflow_rejected() {
        let mut out = [0u8; 2];
        assert!(matches!(
            split_bytes_le(0x10000, 2, &mut out),
            Err(FeetechError::ValueExceedsByteRange { .. })
        ));
    }

    #[test]
    fn split_unsupported_length_rejected() {
        let mut out = [0u8; 3];
        assert!(matches!(
            split_bytes_le(0x10, 3, &mut out),
            Err(FeetechError::UnsupportedByteLength(3))
        ));
    }

    #[test]
    fn combine_one_byte() {
        assert_eq!(combine_bytes_le(&[0xAB], 1).unwrap(), 0xAB);
    }

    #[test]
    fn combine_two_byte_little_endian() {
        assert_eq!(combine_bytes_le(&[0xCD, 0xAB], 2).unwrap(), 0xABCD);
    }

    #[test]
    fn combine_four_byte_little_endian() {
        assert_eq!(
            combine_bytes_le(&[0xEF, 0xBE, 0xAD, 0xDE], 4).unwrap(),
            0xDEAD_BEEF
        );
    }

    #[test]
    fn combine_length_mismatch_rejected() {
        assert!(matches!(
            combine_bytes_le(&[0xAB], 2),
            Err(FeetechError::ByteLengthMismatch {
                expected: 2,
                actual: 1
            })
        ));
    }

    #[test]
    fn split_combine_roundtrip() {
        let cases: &[(u32, usize)] = &[
            (0, 1),
            (0xFF, 1),
            (0, 2),
            (0xFFFF, 2),
            (0xABCD, 2),
            (0x1234_5678, 4),
            (0xDEAD_BEEF, 4),
            (0, 4),
            (u32::MAX, 4),
        ];
        for &(v, len) in cases {
            let mut buf = [0u8; 4];
            split_bytes_le(v, len, &mut buf[..len]).unwrap();
            let back = combine_bytes_le(&buf[..len], len).unwrap();
            assert_eq!(back, v, "split→combine for value {v:#x} len {len}");
        }
    }
}
