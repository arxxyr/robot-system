//! `FrameHeader.capability_flags` 位域常量。
//!
//! 对应 `docs/00_design.md` §3.4。位号一旦分配**永久不得复用**；deprecate
//! 老位请保留常量并在注释标记 "deprecated since vX.Y"，**不从枚举里删除**。
//!
//! ```text
//! bit  0: HAS_BLOB
//! bit  1: BLOB_COMPRESSED_LZ4
//! bit  2: BLOB_COMPRESSED_ZSTD
//! bit  3: GPU_MEMORY_HANDLE
//! bit  4: LITTLE_ENDIAN
//! bit  5: HEADER_CRC_VALID
//! bit  6: IS_REPLAY
//! bit  7: IS_SIMULATION
//! bit  8: IS_DEGRADED
//! bit  9: IS_SYNTHETIC
//! bit 10: BODY_CRC_VALID
//! bit 11: BLOB_CRC_VALID
//! bits 12-63: reserved
//! ```
//!
//! 使用：
//!
//! ```ignore
//! use robot_msg::capability::{CAP_HAS_BLOB, CAP_LITTLE_ENDIAN};
//! let flags = CAP_HAS_BLOB | CAP_LITTLE_ENDIAN;
//! ```

/// sample 携带 raw blob 区域（`FrameHeader.blob_offset` / `blob_size` 有效）。
pub const CAP_HAS_BLOB: u64 = 1 << 0;
/// blob 载荷使用 LZ4 压缩。
pub const CAP_BLOB_COMPRESSED_LZ4: u64 = 1 << 1;
/// blob 载荷使用 Zstandard 压缩。
pub const CAP_BLOB_COMPRESSED_ZSTD: u64 = 1 << 2;
/// blob 载荷是 GPU memory handle（跨进程共享），而非主机 CPU 字节。
pub const CAP_GPU_MEMORY_HANDLE: u64 = 1 << 3;
/// sample 以小端写入；`0` 意味着大端（目标平台全是小端，置 1 是"显式"标注）。
pub const CAP_LITTLE_ENDIAN: u64 = 1 << 4;
/// `FrameHeader.header_crc32` 字段有效（§3.4 CRC 策略）。
pub const CAP_HEADER_CRC_VALID: u64 = 1 << 5;
/// 这帧是 `robot-recorder` 回放的产物，不是实时采集。
pub const CAP_IS_REPLAY: u64 = 1 << 6;
/// 这帧源自仿真（MuJoCo / Isaac / Gazebo …），不是真硬件。
pub const CAP_IS_SIMULATION: u64 = 1 << 7;
/// 发布者声明自己处于降级状态（§1.5 S1 DEGRADED / §6.3 `CONTROL_LOST` 等）。
pub const CAP_IS_DEGRADED: u64 = 1 << 8;
/// 这帧是合成 / 插值 / 预测值，而非直接采样。
pub const CAP_IS_SYNTHETIC: u64 = 1 << 9;
/// body 负载 CRC 有效（需额外 body-level CRC 字段，默认关闭）。
pub const CAP_BODY_CRC_VALID: u64 = 1 << 10;
/// blob 负载 CRC 有效（对应 `BlobDesc.crc32`；默认仅对 record / 跨机开启）。
pub const CAP_BLOB_CRC_VALID: u64 = 1 << 11;

/// 已分配位的位掩码；`flags & !CAP_RESERVED_MASK == 0` 可用来检测 "发布者
/// 是否用了未来版本才有的位"。保留位（12-63）当前全为 0，新增 cap 必须
/// 追加到本常量。
pub const CAP_ALLOCATED_MASK: u64 = CAP_HAS_BLOB
    | CAP_BLOB_COMPRESSED_LZ4
    | CAP_BLOB_COMPRESSED_ZSTD
    | CAP_GPU_MEMORY_HANDLE
    | CAP_LITTLE_ENDIAN
    | CAP_HEADER_CRC_VALID
    | CAP_IS_REPLAY
    | CAP_IS_SIMULATION
    | CAP_IS_DEGRADED
    | CAP_IS_SYNTHETIC
    | CAP_BODY_CRC_VALID
    | CAP_BLOB_CRC_VALID;

#[inline]
#[must_use]
pub const fn has(flags: u64, cap: u64) -> bool {
    flags & cap != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_are_distinct() {
        // 每个 cap 是 2 的幂且互不碰撞
        let caps = [
            CAP_HAS_BLOB,
            CAP_BLOB_COMPRESSED_LZ4,
            CAP_BLOB_COMPRESSED_ZSTD,
            CAP_GPU_MEMORY_HANDLE,
            CAP_LITTLE_ENDIAN,
            CAP_HEADER_CRC_VALID,
            CAP_IS_REPLAY,
            CAP_IS_SIMULATION,
            CAP_IS_DEGRADED,
            CAP_IS_SYNTHETIC,
            CAP_BODY_CRC_VALID,
            CAP_BLOB_CRC_VALID,
        ];
        for (i, a) in caps.iter().enumerate() {
            assert!(a.is_power_of_two(), "{a:#x} is not a single bit");
            for b in &caps[i + 1..] {
                assert_eq!(a & b, 0, "bits {a:#x} and {b:#x} overlap");
            }
        }
    }

    #[test]
    fn allocated_mask_covers_all_constants() {
        let expected = CAP_HAS_BLOB
            | CAP_BLOB_COMPRESSED_LZ4
            | CAP_BLOB_COMPRESSED_ZSTD
            | CAP_GPU_MEMORY_HANDLE
            | CAP_LITTLE_ENDIAN
            | CAP_HEADER_CRC_VALID
            | CAP_IS_REPLAY
            | CAP_IS_SIMULATION
            | CAP_IS_DEGRADED
            | CAP_IS_SYNTHETIC
            | CAP_BODY_CRC_VALID
            | CAP_BLOB_CRC_VALID;
        assert_eq!(CAP_ALLOCATED_MASK, expected);
        // 未分配位全为 0
        assert_eq!(CAP_ALLOCATED_MASK >> 12, 0);
    }

    #[test]
    fn has_helper() {
        let flags = CAP_HAS_BLOB | CAP_LITTLE_ENDIAN;
        assert!(has(flags, CAP_HAS_BLOB));
        assert!(has(flags, CAP_LITTLE_ENDIAN));
        assert!(!has(flags, CAP_HEADER_CRC_VALID));
    }
}
