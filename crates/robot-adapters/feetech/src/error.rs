//! `FeetechError` —— 本 crate 内 Result 的 Err 类型。
//!
//! 所有公开 fn 返回 `Result<T, FeetechError>`，经 `From<FeetechError> for RobotError`
//! 在 adapter trait 边界转成 `robot_core::error::RobotError`。

use std::path::PathBuf;

/// Feetech 协议层 / bus 层错误。
#[derive(Debug, thiserror::Error)]
pub enum FeetechError {
    // ── 配置 / 参数错误 ────────────────────────────────────────────────
    #[error("unknown register: {0}")]
    UnknownRegister(&'static str),

    #[error("unknown motor name: {0}")]
    UnknownMotor(String),

    #[error("motor id out of range [1, 253]: got {0}")]
    InvalidMotorId(i32),

    #[error("duplicate motor id: {0}")]
    DuplicateMotorId(u8),

    #[error("sign-magnitude magnitude {magnitude} exceeds {max} (sign_bit_index={sign_bit_index})")]
    MagnitudeOutOfRange {
        magnitude: i64,
        max: u32,
        sign_bit_index: u32,
    },

    #[error("calibration homing_offset {0} out of [-2047, +2047] (STS3215 12-bit sign-magnitude)")]
    CalibrationOffsetOutOfRange(i32),

    #[error("calibration range_min {0} out of [0, 4095]")]
    CalibrationMinOutOfRange(i32),

    #[error("calibration range_max {0} out of [0, 4095]")]
    CalibrationMaxOutOfRange(i32),

    // ── calibration JSON loader (task 29) ──────────────────────────────
    #[error("calibration file {path:?}: {source}")]
    CalibrationIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("calibration file {path:?}: parse error: {source}")]
    CalibrationParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("calibration file {path:?}: motor {name} field invalid: {reason}")]
    CalibrationEntryInvalid {
        path: PathBuf,
        name: String,
        /// 来自 `serde_json::from_value` 的字段错误文本 —— 含 "unknown field foo" /
        /// "value 2048 out of [-2047, 2047]" 等 Display 消息；细粒度分类留给
        /// serde_json::Error / `MotorCalibration::new` 自己的 error 类。
        reason: String,
    },

    #[error("calibration merge: motor {name} duplicated between {first:?} and {second:?}")]
    CalibrationDuplicateMotor {
        name: String,
        first: PathBuf,
        second: PathBuf,
    },

    #[error("byte length {0} unsupported (only 1 / 2 / 4 allowed)")]
    UnsupportedByteLength(usize),

    #[error("value {value} exceeds {max_bytes}-byte unsigned range (max {max})")]
    ValueExceedsByteRange {
        value: u32,
        max_bytes: usize,
        max: u32,
    },

    #[error("expected {expected} bytes, got {actual}")]
    ByteLengthMismatch { expected: usize, actual: usize },

    // ── 协议 / wire 错误 ───────────────────────────────────────────────
    #[error("malformed packet header: expected 0xFF 0xFF, got {got:02X?}")]
    BadPacketHeader { got: [u8; 2] },

    #[error("packet length field {len} inconsistent with body bytes {body_len}")]
    BadPacketLength { len: u8, body_len: usize },

    #[error("packet checksum mismatch: expected {expected:#04x}, got {got:#04x}")]
    BadChecksum { expected: u8, got: u8 },

    #[error("status packet error byte = {0:#04x}")]
    StatusError(u8),

    #[error("packet too short: need at least {needed} bytes, got {got}")]
    PacketTooShort { needed: usize, got: usize },

    #[error("ping motor id={0}: no response")]
    PingNoResponse(u8),

    #[error("ping motor id={id}: model mismatch (expected {expected}, got {got})")]
    PingModelMismatch { id: u8, expected: u16, got: u16 },

    #[error("handshake failed: {0}")]
    Handshake(String),

    // ── transport 层 ──────────────────────────────────────────────────
    #[error("transport not connected")]
    NotConnected,

    #[error("transport already connected")]
    AlreadyConnected,

    #[error("transport open {path:?}: {source}")]
    TransportOpen {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("transport read: {0}")]
    TransportRead(#[source] std::io::Error),

    #[error("transport write: {0}")]
    TransportWrite(#[source] std::io::Error),

    #[error("transport timeout after {timeout_ms} ms (waiting for {wanted} bytes, got {got})")]
    TransportTimeout {
        timeout_ms: u64,
        wanted: usize,
        got: usize,
    },
}

impl From<FeetechError> for robot_core::error::RobotError {
    /// 把富信息 [`FeetechError`] 映射成 `&'static str` based [`robot_core::error::RobotError`]。
    ///
    /// `RobotError` v0.2 设计为零堆分配（`robot-core/src/error.rs` 头部约束），
    /// 所有变体只能持 `&'static str`。映射在此**故意丢失细节**，富信息应在
    /// adapter 层 `eprintln!` / `tracing::error!(?e)` 打印 `FeetechError` 原文。
    fn from(e: FeetechError) -> Self {
        use FeetechError as F;
        match e {
            F::NotConnected => Self::NotConnected,
            F::AlreadyConnected => Self::Busy,
            F::TransportTimeout { .. } => Self::DeadlineExceeded,
            F::PingNoResponse(_)
            | F::PingModelMismatch { .. }
            | F::Handshake(_)
            | F::TransportOpen { .. }
            | F::TransportRead(_)
            | F::TransportWrite(_) => Self::HardwareFault("feetech transport / handshake"),
            F::BadPacketHeader { .. }
            | F::BadPacketLength { .. }
            | F::BadChecksum { .. }
            | F::PacketTooShort { .. }
            | F::StatusError(_) => Self::HardwareFault("feetech wire protocol"),
            F::CalibrationOffsetOutOfRange(_)
            | F::CalibrationMinOutOfRange(_)
            | F::CalibrationMaxOutOfRange(_)
            | F::CalibrationEntryInvalid { .. }
            | F::CalibrationDuplicateMotor { .. } => Self::InvalidParameter("feetech calibration"),
            F::CalibrationIo { .. } | F::CalibrationParse { .. } => {
                Self::InvalidParameter("feetech calibration file")
            }
            F::MagnitudeOutOfRange { .. } | F::ValueExceedsByteRange { .. } => {
                Self::InvalidParameter("feetech encoded value out of range")
            }
            F::UnknownRegister(_)
            | F::UnknownMotor(_)
            | F::InvalidMotorId(_)
            | F::DuplicateMotorId(_)
            | F::UnsupportedByteLength(_)
            | F::ByteLengthMismatch { .. } => Self::InvalidParameter("feetech invalid argument"),
        }
    }
}
