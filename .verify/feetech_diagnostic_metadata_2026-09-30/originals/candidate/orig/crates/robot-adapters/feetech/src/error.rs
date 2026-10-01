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

    #[error("标定电机 ID 与总线不匹配：expected={expected}, got={got}")]
    CalibrationMotorIdMismatch { expected: u8, got: u8 },

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

    // ── adapter 防飞序列诊断（task 29 commit 4） ─────────────────────
    #[error(
        "sync_goal_to_present: motor id={motor_id} readback mismatch after {attempts} attempts (wrote={wrote}, got={got})"
    )]
    SyncGoalReadbackMismatch {
        motor_id: u8,
        wrote: i32,
        got: i32,
        attempts: u32,
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

    #[error("握手电机无响应：id={id}")]
    HandshakeMissing { id: u8 },

    #[error("握手电机型号不匹配：id={id}, got={got}")]
    HandshakeWrongModel { id: u8, got: u16 },

    #[error("响应电机 ID 不匹配：expected={expected}, got={got}")]
    ResponseIdMismatch { expected: u8, got: u8 },

    #[error("普通事务已在边界中止：{0:?}")]
    Aborted(robot_core::serial_services::AbortReason),

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

    #[error("发送在 {timeout_ms} ms 截止时间内未完成：wanted={wanted}, got={got}")]
    TransportWriteTimeout {
        timeout_ms: u64,
        wanted: usize,
        got: usize,
    },

    #[error("接收缓冲清理失败：{0}")]
    DiscardFailed(#[source] std::io::Error),
}

impl FeetechError {
    /// 类型化通信原因；硬件状态码和永久参数问题不伪装成通信故障。
    #[must_use]
    pub fn comm_kind(&self) -> Option<robot_core::serial_services::CommErrorKind> {
        use robot_core::serial_services::CommErrorKind as C;
        match self {
            Self::NotConnected => Some(C::Disconnected),
            Self::TransportTimeout { .. } => Some(C::Timeout),
            Self::TransportWriteTimeout { .. } | Self::TransportWrite(_) => Some(C::IoWrite),
            Self::TransportRead(_) => Some(C::IoRead),
            Self::TransportOpen { .. } => Some(C::OpenFailed),
            Self::BadPacketHeader { .. }
            | Self::BadPacketLength { .. }
            | Self::BadChecksum { .. }
            | Self::PacketTooShort { .. } => Some(C::FrameCorrupt),
            Self::ResponseIdMismatch { .. } => Some(C::ResponseIdMismatch),
            Self::PingNoResponse(_) | Self::HandshakeMissing { .. } => Some(C::PeerNoResponse),
            Self::DiscardFailed(_) => Some(C::DiscardFailed),
            _ => None,
        }
    }
}

impl From<FeetechError> for robot_core::error::RobotError {
    /// 保留通信类别与中止原因；额外细节由固定诊断事件记录，执行线程不格式化日志。
    fn from(e: FeetechError) -> Self {
        use FeetechError as F;
        match e {
            F::NotConnected => Self::Comm(robot_core::serial_services::CommErrorKind::Disconnected),
            F::AlreadyConnected => Self::Busy,
            F::Aborted(reason) => Self::Aborted(reason),
            F::TransportTimeout { .. }
            | F::TransportWriteTimeout { .. }
            | F::PingNoResponse(_)
            | F::HandshakeMissing { .. }
            | F::ResponseIdMismatch { .. }
            | F::DiscardFailed(_)
            | F::TransportOpen { .. }
            | F::TransportRead(_)
            | F::TransportWrite(_)
            | F::BadPacketHeader { .. }
            | F::BadPacketLength { .. }
            | F::BadChecksum { .. }
            | F::PacketTooShort { .. } => Self::Comm(
                e.comm_kind()
                    .unwrap_or(robot_core::serial_services::CommErrorKind::Unknown),
            ),
            F::StatusError(_) => Self::HardwareFault("feetech motor status"),
            F::PingModelMismatch { .. } | F::HandshakeWrongModel { .. } => {
                Self::InvalidParameter("feetech motor model")
            }
            F::CalibrationOffsetOutOfRange(_)
            | F::CalibrationMinOutOfRange(_)
            | F::CalibrationMaxOutOfRange(_)
            | F::CalibrationMotorIdMismatch { .. }
            | F::CalibrationEntryInvalid { .. }
            | F::CalibrationDuplicateMotor { .. } => Self::InvalidParameter("feetech calibration"),
            F::SyncGoalReadbackMismatch { .. } => {
                Self::HardwareFault("feetech sync_goal_to_present readback mismatch")
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use robot_core::error::RobotError;
    use robot_core::serial_services::{AbortReason, CommErrorKind as C};

    #[test]
    fn transport_and_protocol_errors_keep_their_communication_classes() {
        for (error, expected) in [
            (FeetechError::NotConnected, C::Disconnected),
            (
                FeetechError::TransportTimeout {
                    timeout_ms: 50,
                    wanted: 8,
                    got: 2,
                },
                C::Timeout,
            ),
            (
                FeetechError::TransportWriteTimeout {
                    timeout_ms: 50,
                    wanted: 8,
                    got: 2,
                },
                C::IoWrite,
            ),
            (
                FeetechError::TransportRead(std::io::Error::other("read")),
                C::IoRead,
            ),
            (
                FeetechError::TransportWrite(std::io::Error::other("write")),
                C::IoWrite,
            ),
            (
                FeetechError::TransportOpen {
                    path: PathBuf::from("missing"),
                    source: std::io::Error::other("open"),
                },
                C::OpenFailed,
            ),
            (
                FeetechError::BadPacketHeader { got: [0, 0] },
                C::FrameCorrupt,
            ),
            (
                FeetechError::BadPacketLength {
                    len: 1,
                    body_len: 2,
                },
                C::FrameCorrupt,
            ),
            (
                FeetechError::BadChecksum {
                    expected: 1,
                    got: 2,
                },
                C::FrameCorrupt,
            ),
            (
                FeetechError::PacketTooShort { needed: 6, got: 2 },
                C::FrameCorrupt,
            ),
            (
                FeetechError::ResponseIdMismatch {
                    expected: 1,
                    got: 2,
                },
                C::ResponseIdMismatch,
            ),
            (FeetechError::PingNoResponse(1), C::PeerNoResponse),
            (FeetechError::HandshakeMissing { id: 1 }, C::PeerNoResponse),
            (
                FeetechError::DiscardFailed(std::io::Error::other("discard")),
                C::DiscardFailed,
            ),
        ] {
            assert_eq!(error.comm_kind(), Some(expected));
            assert_eq!(RobotError::from(error), RobotError::Comm(expected));
        }
    }

    #[test]
    fn device_faults_configuration_and_abort_do_not_become_unknown_communication() {
        for reason in [AbortReason::Emergency, AbortReason::Timeout] {
            assert_eq!(
                RobotError::from(FeetechError::Aborted(reason)),
                RobotError::Aborted(reason)
            );
        }
        assert_eq!(
            RobotError::from(FeetechError::AlreadyConnected),
            RobotError::Busy
        );
        for error in [
            FeetechError::StatusError(1),
            FeetechError::SyncGoalReadbackMismatch {
                motor_id: 1,
                wrote: 0,
                got: 1,
                attempts: 3,
            },
        ] {
            assert_eq!(error.comm_kind(), None);
            assert!(matches!(
                RobotError::from(error),
                RobotError::HardwareFault(_)
            ));
        }
        for error in [
            FeetechError::HandshakeWrongModel { id: 1, got: 2 },
            FeetechError::PingModelMismatch {
                id: 1,
                expected: 777,
                got: 2,
            },
            FeetechError::CalibrationMotorIdMismatch {
                expected: 1,
                got: 2,
            },
            FeetechError::InvalidMotorId(0),
            FeetechError::ByteLengthMismatch {
                expected: 1,
                actual: 0,
            },
        ] {
            assert_eq!(error.comm_kind(), None);
            assert!(matches!(
                RobotError::from(error),
                RobotError::InvalidParameter(_)
            ));
        }
    }
}
