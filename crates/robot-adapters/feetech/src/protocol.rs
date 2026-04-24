//! Wire packet build / parse —— Feetech protocol 0（与 Dynamixel protocol 1 兼容）。
//!
//! ## 帧格式
//!
//! **Instruction packet**（host → motor）：
//!
//! ```text
//!  byte:  0    1    2    3      4      5..(LEN+1)   (LEN+2)
//!         FF   FF   ID   LEN    INST   PARAMS...    CHECKSUM
//! ```
//!
//! **Status packet**（motor → host）：
//!
//! ```text
//!  byte:  0    1    2    3      4      5..(LEN+1)   (LEN+2)
//!         FF   FF   ID   LEN    ERROR  PARAMS...    CHECKSUM
//! ```
//!
//! - `LEN = N + 2`（N = 参数字节数；`+2` = INST/ERROR + CHECKSUM 各 1 字节）。
//! - 总包长 = `4 + LEN` = `6 + N`。
//! - `CHECKSUM = ~(ID + LEN + INST/ERROR + sum(PARAMS)) & 0xFF`。
//!
//! ## 指令枚举（[`Instruction`]）
//!
//! - `PING = 0x01` —— host 发 ping，motor 回带 model number 的 status。
//! - `READ_DATA = 0x02` params = `[addr, length]` —— 单 motor 单寄存器读。
//! - `WRITE_DATA = 0x03` params = `[addr, byte0, byte1, ...]` —— 单 motor 单寄存器写。
//! - `SYNC_READ = 0x82` 发到 `BROADCAST_ID = 0xFE`，params = `[addr, length, id1,
//!   id2, ...]`，每个 motor 按 id 顺序回 status。
//! - `SYNC_WRITE = 0x83` 发到 `BROADCAST_ID = 0xFE`，params = `[addr, length,
//!   id1, byte0_1, byte1_1, ..., id2, byte0_2, ...]`，**不**等 ACK。
//!
//! ## v0.2 实现策略
//!
//! - **PING 的 status payload 处理**：scservo_sdk 的 `ping` 实际上是 "PING + 隐式
//!   READ_DATA(addr=3, len=2) Model_Number"。本 crate 把这两步**显式拆开**：
//!   `bus::ping` 调 [`build_ping`] + [`parse_status`]，再单独 [`build_read`]
//!   `Model_Number`。这样协议层每个 packet 单一职责。
//! - **PARAMS 上限**：`LEN = u8`，则 `N <= 253`。`SYNC_WRITE` 单包最多 `(253 -
//!   2) / (1 + length)` 个 motor（length=2 时 ~83 个）。XLeRobot 双臂 + 三轮 ≤ 13
//!   个，远低于上限，本 crate 不实现分包。

use crate::error::FeetechError;

// ============================================================================
// 常量
// ============================================================================

/// Packet 头 magic：连续两字节 `0xFF`。
pub const PACKET_HEADER: [u8; 2] = [0xFF, 0xFF];

/// 广播地址，用于 `SYNC_READ` / `SYNC_WRITE` 指令的 ID 字段。
pub const BROADCAST_ID: u8 = 0xFE;

/// 一个 packet 的最小总长（FF FF + ID + LEN + INST + CHK = 6 字节，N=0）。
pub const MIN_PACKET_LEN: usize = 6;

/// `LEN = u8`，PARAMS 段 N 最大 = 253（保留 INST + CHK 共 2 字节给 LEN）。
pub const MAX_PARAMS: usize = 253;

/// Feetech protocol 0 指令字节。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instruction {
    Ping = 0x01,
    ReadData = 0x02,
    WriteData = 0x03,
    SyncRead = 0x82,
    SyncWrite = 0x83,
}

// ============================================================================
// Build
// ============================================================================

/// 计算 `~(sum) & 0xFF`，Feetech / Dynamixel protocol 1 标准 checksum。
#[inline]
#[must_use]
pub fn checksum(bytes: &[u8]) -> u8 {
    let sum: u32 = bytes.iter().copied().map(u32::from).sum();
    !(sum as u8)
}

/// 把 instruction packet 写入 `out`，返回写入的字节数。
///
/// `out.len()` 必须 ≥ `6 + params.len()`。
///
/// # Errors
///
/// - `params.len() > MAX_PARAMS` → [`FeetechError::BadPacketLength`]
/// - `out.len() < 6 + params.len()` → [`FeetechError::ByteLengthMismatch`]
pub fn build_instruction(
    id: u8,
    inst: Instruction,
    params: &[u8],
    out: &mut [u8],
) -> Result<usize, FeetechError> {
    if params.len() > MAX_PARAMS {
        return Err(FeetechError::BadPacketLength {
            len: 0,
            body_len: params.len(),
        });
    }
    let total = 6 + params.len();
    if out.len() < total {
        return Err(FeetechError::ByteLengthMismatch {
            expected: total,
            actual: out.len(),
        });
    }
    let len_field: u8 =
        u8::try_from(params.len() + 2).map_err(|_| FeetechError::BadPacketLength {
            len: 0,
            body_len: params.len(),
        })?;

    out[0] = PACKET_HEADER[0];
    out[1] = PACKET_HEADER[1];
    out[2] = id;
    out[3] = len_field;
    out[4] = inst as u8;
    out[5..5 + params.len()].copy_from_slice(params);

    // Checksum 跨 ID + LEN + INST + PARAMS（不包括 header 两字节 + checksum 自己）
    out[5 + params.len()] = checksum(&out[2..5 + params.len()]);

    Ok(total)
}

// ── 各指令的薄包装 ────────────────────────────────────────────────────

/// PING：`FF FF <ID> 02 01 <CHK>`（总 6 字节）。
///
/// # Errors
///
/// `out.len() < 6` → [`FeetechError::ByteLengthMismatch`]。
pub fn build_ping(id: u8, out: &mut [u8]) -> Result<usize, FeetechError> {
    build_instruction(id, Instruction::Ping, &[], out)
}

/// READ_DATA：`FF FF <ID> 04 02 <ADDR> <LEN> <CHK>`（总 8 字节）。
///
/// # Errors
///
/// 同 [`build_instruction`]。
pub fn build_read(id: u8, addr: u8, length: u8, out: &mut [u8]) -> Result<usize, FeetechError> {
    build_instruction(id, Instruction::ReadData, &[addr, length], out)
}

/// WRITE_DATA：`FF FF <ID> <LEN> 03 <ADDR> <DATA...> <CHK>`。
///
/// # Errors
///
/// `data` 太长（`data.len() + 1 > MAX_PARAMS`） → [`FeetechError::BadPacketLength`]，
/// 或 `out` 太短 → [`FeetechError::ByteLengthMismatch`]。
pub fn build_write(id: u8, addr: u8, data: &[u8], out: &mut [u8]) -> Result<usize, FeetechError> {
    if data.len() + 1 > MAX_PARAMS {
        return Err(FeetechError::BadPacketLength {
            len: 0,
            body_len: data.len() + 1,
        });
    }
    let mut params = [0u8; MAX_PARAMS];
    params[0] = addr;
    params[1..=data.len()].copy_from_slice(data);
    build_instruction(id, Instruction::WriteData, &params[..=data.len()], out)
}

/// SYNC_WRITE：`FF FF FE <LEN> 83 <ADDR> <PER_LEN> <ID1> <DATA1...> <ID2> <DATA2...> <CHK>`。
///
/// `per_motor_data` 每条数据**长度必须**等于 `length`（与寄存器字节数一致），调用方
/// 自行用 [`split_bytes_le`](crate::encoding::split_bytes_le) 准备。
///
/// # Errors
///
/// - 任一 `per_motor_data` 长度 ≠ `length` → [`FeetechError::ByteLengthMismatch`]
/// - 总 params 超 `MAX_PARAMS` → [`FeetechError::BadPacketLength`]
pub fn build_sync_write(
    addr: u8,
    length: u8,
    per_motor: &[(u8, &[u8])],
    out: &mut [u8],
) -> Result<usize, FeetechError> {
    let length_usize = usize::from(length);
    for (_, data) in per_motor {
        if data.len() != length_usize {
            return Err(FeetechError::ByteLengthMismatch {
                expected: length_usize,
                actual: data.len(),
            });
        }
    }
    let entry_size = 1 + length_usize; // [id] + [data]
    let total_params = 2 + per_motor.len() * entry_size; // [addr, length] + N entries
    if total_params > MAX_PARAMS {
        return Err(FeetechError::BadPacketLength {
            len: 0,
            body_len: total_params,
        });
    }
    let mut params = vec![0u8; total_params];
    params[0] = addr;
    params[1] = length;
    for (i, (id, data)) in per_motor.iter().enumerate() {
        let off = 2 + i * entry_size;
        params[off] = *id;
        params[off + 1..off + 1 + length_usize].copy_from_slice(data);
    }
    build_instruction(BROADCAST_ID, Instruction::SyncWrite, &params, out)
}

/// SYNC_READ：`FF FF FE <LEN> 82 <ADDR> <PER_LEN> <ID1> <ID2> ... <CHK>`。
///
/// 每个 motor 按 id 顺序回独立 status packet，由调用方循环 [`parse_status`]
/// 收集。
///
/// # Errors
///
/// `motor_ids.len() + 2 > MAX_PARAMS` → [`FeetechError::BadPacketLength`]。
pub fn build_sync_read(
    addr: u8,
    length: u8,
    motor_ids: &[u8],
    out: &mut [u8],
) -> Result<usize, FeetechError> {
    let total_params = 2 + motor_ids.len();
    if total_params > MAX_PARAMS {
        return Err(FeetechError::BadPacketLength {
            len: 0,
            body_len: total_params,
        });
    }
    let mut params = vec![0u8; total_params];
    params[0] = addr;
    params[1] = length;
    params[2..].copy_from_slice(motor_ids);
    build_instruction(BROADCAST_ID, Instruction::SyncRead, &params, out)
}

// ============================================================================
// Parse
// ============================================================================

/// Parse 出来的一个 status packet。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusPacket<'a> {
    pub id: u8,
    pub error: u8,
    pub params: &'a [u8],
    /// 本 packet 在原 `buf` 中占的总字节数（含 header / checksum），调用方据此
    /// 推进 cursor 找下一个 packet。
    pub consumed: usize,
}

/// 在 `buf` 起始位置 parse 一个 status packet。
///
/// # Errors
///
/// - `buf.len() < 6` → [`FeetechError::PacketTooShort`]
/// - `buf[0..2] != [0xFF, 0xFF]` → [`FeetechError::BadPacketHeader`]
/// - `LEN < 2` 或 `buf.len() < 4 + LEN` → [`FeetechError::BadPacketLength`] /
///   [`FeetechError::PacketTooShort`]
/// - checksum 不匹配 → [`FeetechError::BadChecksum`]
pub fn parse_status(buf: &[u8]) -> Result<StatusPacket<'_>, FeetechError> {
    if buf.len() < MIN_PACKET_LEN {
        return Err(FeetechError::PacketTooShort {
            needed: MIN_PACKET_LEN,
            got: buf.len(),
        });
    }
    if buf[0..2] != PACKET_HEADER {
        return Err(FeetechError::BadPacketHeader {
            got: [buf[0], buf[1]],
        });
    }
    let id = buf[2];
    let len_field = buf[3];
    if len_field < 2 {
        return Err(FeetechError::BadPacketLength {
            len: len_field,
            body_len: 0,
        });
    }
    let total = 4 + usize::from(len_field);
    if buf.len() < total {
        return Err(FeetechError::PacketTooShort {
            needed: total,
            got: buf.len(),
        });
    }
    let error = buf[4];
    let params = &buf[5..total - 1];
    let chk_expected = checksum(&buf[2..total - 1]);
    let chk_got = buf[total - 1];
    if chk_expected != chk_got {
        return Err(FeetechError::BadChecksum {
            expected: chk_expected,
            got: chk_got,
        });
    }
    Ok(StatusPacket {
        id,
        error,
        params,
        consumed: total,
    })
}

/// 在 `buf` 中扫描下一个 `0xFF 0xFF` 出现的偏移。给 stream-based reader 用，遇到
/// 半包 / 噪声字节时 resync。
#[must_use]
pub fn find_header(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == PACKET_HEADER)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── checksum ──────────────────────────────────────────────────────

    #[test]
    fn checksum_known_values() {
        // PING ID=1: ID(01) + LEN(02) + INST(01) = 04 → ~04 = FB
        assert_eq!(checksum(&[0x01, 0x02, 0x01]), 0xFB);
        // ID=0x01 + LEN=0x05 + INST=0x03 + ADDR=0x2A + 0x00 + 0x08 = 0x3B → ~3B = C4
        assert_eq!(checksum(&[0x01, 0x05, 0x03, 0x2A, 0x00, 0x08]), 0xC4);
    }

    // ── build ─────────────────────────────────────────────────────────

    #[test]
    fn build_ping_id1_matches_known_bytes() {
        let mut buf = [0u8; 6];
        let n = build_ping(1, &mut buf).unwrap();
        assert_eq!(n, 6);
        assert_eq!(buf, [0xFF, 0xFF, 0x01, 0x02, 0x01, 0xFB]);
    }

    #[test]
    fn build_read_present_position_id5() {
        // READ_DATA ID=5, addr=56 (Present_Position), length=2
        // payload: [56, 2], LEN = 4
        let mut buf = [0u8; 8];
        let n = build_read(5, 56, 2, &mut buf).unwrap();
        assert_eq!(n, 8);
        // CHK = ~(05 + 04 + 02 + 38 + 02) = ~45 = BA
        assert_eq!(buf, [0xFF, 0xFF, 0x05, 0x04, 0x02, 0x38, 0x02, 0xBA]);
    }

    #[test]
    fn build_write_goal_position_2048_id1() {
        // WRITE_DATA ID=1, addr=42 (Goal_Position), data = [00, 08]（2048 LE）
        // payload: [42, 00, 08], LEN = 5, CHK = C4（手工算）
        let mut buf = [0u8; 9];
        let n = build_write(1, 42, &[0x00, 0x08], &mut buf).unwrap();
        assert_eq!(n, 9);
        assert_eq!(buf, [0xFF, 0xFF, 0x01, 0x05, 0x03, 0x2A, 0x00, 0x08, 0xC4]);
    }

    #[test]
    fn build_sync_write_two_motors() {
        // SYNC_WRITE addr=42 (Goal_Position), length=2, motors {1: [00,08], 2: [FF,07]}
        // 包: FF FF FE LEN 83 2A 02 01 00 08 02 FF 07 CHK
        // PARAMS = [2A, 02, 01, 00, 08, 02, FF, 07] = 8 bytes
        // LEN = N + 2 = 10
        let mut buf = [0u8; 14];
        let n =
            build_sync_write(42, 2, &[(1, &[0x00, 0x08]), (2, &[0xFF, 0x07])], &mut buf).unwrap();
        assert_eq!(n, 14);
        assert_eq!(buf[0..5], [0xFF, 0xFF, 0xFE, 0x0A, 0x83]);
        assert_eq!(buf[5..13], [0x2A, 0x02, 0x01, 0x00, 0x08, 0x02, 0xFF, 0x07]);
        // 校验 checksum
        let chk = checksum(&buf[2..13]);
        assert_eq!(buf[13], chk);
    }

    #[test]
    fn build_sync_read_three_motors() {
        // SYNC_READ addr=56 (Present_Position), length=2, ids = [1, 2, 3]
        // PARAMS = [56, 2, 1, 2, 3] = 5 bytes, LEN = 7
        let mut buf = [0u8; 11];
        let n = build_sync_read(56, 2, &[1, 2, 3], &mut buf).unwrap();
        assert_eq!(n, 11);
        assert_eq!(buf[0..5], [0xFF, 0xFF, 0xFE, 0x07, 0x82]);
        assert_eq!(buf[5..10], [0x38, 0x02, 0x01, 0x02, 0x03]);
        let chk = checksum(&buf[2..10]);
        assert_eq!(buf[10], chk);
    }

    #[test]
    fn build_sync_write_data_length_mismatch_rejected() {
        let mut buf = [0u8; 32];
        let err = build_sync_write(42, 2, &[(1, &[0x00])], &mut buf).unwrap_err();
        assert!(matches!(
            err,
            FeetechError::ByteLengthMismatch {
                expected: 2,
                actual: 1
            }
        ));
    }

    #[test]
    fn build_too_long_params_rejected() {
        let huge = vec![0u8; MAX_PARAMS + 1];
        let mut buf = vec![0u8; 6 + huge.len()];
        assert!(matches!(
            build_instruction(1, Instruction::WriteData, &huge, &mut buf),
            Err(FeetechError::BadPacketLength { .. })
        ));
    }

    #[test]
    fn build_out_buffer_too_small_rejected() {
        let mut buf = [0u8; 5];
        assert!(matches!(
            build_ping(1, &mut buf),
            Err(FeetechError::ByteLengthMismatch {
                expected: 6,
                actual: 5
            })
        ));
    }

    // ── parse ─────────────────────────────────────────────────────────

    #[test]
    fn parse_status_ping_response() {
        // 模拟一个 ping 后的 status 回包：ID=1, ERROR=0, no params
        // FF FF 01 02 00 CHK; CHK = ~(01 + 02 + 00) = ~03 = FC
        let buf = [0xFF, 0xFF, 0x01, 0x02, 0x00, 0xFC];
        let pkt = parse_status(&buf).unwrap();
        assert_eq!(pkt.id, 1);
        assert_eq!(pkt.error, 0);
        assert_eq!(pkt.params, &[] as &[u8]);
        assert_eq!(pkt.consumed, 6);
    }

    #[test]
    fn parse_status_with_two_byte_payload() {
        // ID=5, ERROR=0, 2 字节 payload 0x09 0x03（= Model_Number 777）
        // LEN = N+2 = 4, CHK = ~(05 + 04 + 00 + 09 + 03) = ~15 = EA
        let buf = [0xFF, 0xFF, 0x05, 0x04, 0x00, 0x09, 0x03, 0xEA];
        let pkt = parse_status(&buf).unwrap();
        assert_eq!(pkt.id, 5);
        assert_eq!(pkt.error, 0);
        assert_eq!(pkt.params, &[0x09, 0x03]);
        assert_eq!(pkt.consumed, 8);
    }

    #[test]
    fn parse_status_bad_header_rejected() {
        let buf = [0x00, 0xFF, 0x01, 0x02, 0x00, 0xFC];
        assert!(matches!(
            parse_status(&buf),
            Err(FeetechError::BadPacketHeader { got: [0x00, 0xFF] })
        ));
    }

    #[test]
    fn parse_status_too_short_rejected() {
        let buf = [0xFF, 0xFF, 0x01];
        assert!(matches!(
            parse_status(&buf),
            Err(FeetechError::PacketTooShort { needed: 6, got: 3 })
        ));
    }

    #[test]
    fn parse_status_truncated_payload_rejected() {
        // LEN field 说有 4 byte 但 buf 不够
        let buf = [0xFF, 0xFF, 0x05, 0x04, 0x00, 0x09]; // 缺 2 字节
        match parse_status(&buf) {
            Err(FeetechError::PacketTooShort { needed: 8, got: 6 }) => {}
            other => panic!("expected PacketTooShort{{8,6}}, got {other:?}"),
        }
    }

    #[test]
    fn parse_status_bad_checksum_rejected() {
        let buf = [0xFF, 0xFF, 0x01, 0x02, 0x00, 0x00]; // 正确应是 0xFC
        match parse_status(&buf) {
            Err(FeetechError::BadChecksum {
                expected: 0xFC,
                got: 0x00,
            }) => {}
            other => panic!("expected BadChecksum, got {other:?}"),
        }
    }

    #[test]
    fn parse_status_zero_length_rejected() {
        let buf = [0xFF, 0xFF, 0x01, 0x01, 0x00, 0x00];
        // LEN < 2 不合法
        assert!(matches!(
            parse_status(&buf),
            Err(FeetechError::BadPacketLength { len: 1, .. })
        ));
    }

    // ── header search ─────────────────────────────────────────────────

    #[test]
    fn find_header_in_clean_buffer() {
        let buf = [0xFF, 0xFF, 0x01, 0x02];
        assert_eq!(find_header(&buf), Some(0));
    }

    #[test]
    fn find_header_skips_noise() {
        let buf = [0xAA, 0xBB, 0xFF, 0xFF, 0x01];
        assert_eq!(find_header(&buf), Some(2));
    }

    #[test]
    fn find_header_partial_returns_none() {
        let buf = [0xAA, 0xBB, 0xFF];
        assert_eq!(find_header(&buf), None);
    }

    // ── 集成：build → parse 自洽 ──────────────────────────────────────

    #[test]
    fn build_then_locally_parse_as_status() {
        // build_ping 出来的 packet 与 status packet 同 frame 格式（INST 字段
        // 在 status 里被解释为 ERROR），所以可以直接 parse_status，且
        // ERROR==0x01 (PING instruction byte)。
        let mut buf = [0u8; 6];
        build_ping(7, &mut buf).unwrap();
        let pkt = parse_status(&buf).unwrap();
        assert_eq!(pkt.id, 7);
        assert_eq!(pkt.error, Instruction::Ping as u8);
        assert!(pkt.params.is_empty());
        assert_eq!(pkt.consumed, 6);
    }
}
