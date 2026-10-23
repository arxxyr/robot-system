//! `FeetechError` —— 本 crate 内 Result 的 Err 类型。
//!
//! 所有公开 fn 返回 `Result<T, FeetechError>`，经 `From<FeetechError> for RobotError`
//! 在 adapter trait 边界转成 `robot_core::error::RobotError`。

use std::path::PathBuf;

use robot_core::serial_services::{BusOpClass, CommErrorKind};
use robot_core::time::{Deadline, MonoRawNs};

/// 绝对期限写入的固定原因；不持有字符串或动态 IO 错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbsoluteWriteReason {
    Unsupported,
    SourceExpired,
    CallBudgetExpired,
    ClockRegressed { previous_at: MonoRawNs },
    BudgetOverflow,
    TimeoutConfiguration { kind: std::io::ErrorKind },
    WriteZero,
    WriteIo { kind: std::io::ErrorKind },
    InvalidOperationClass { class: BusOpClass },
    InvalidWriteCount { reported: usize },
    NoAuthorityProbe,
}

impl AbsoluteWriteReason {
    /// kind9 诊断的原因字节；旧诊断编码不变。
    #[must_use]
    pub const fn diagnostic_code(self) -> u8 {
        match self {
            Self::Unsupported => 1,
            Self::SourceExpired => 2,
            Self::CallBudgetExpired => 3,
            Self::ClockRegressed { .. } => 4,
            Self::BudgetOverflow => 5,
            Self::TimeoutConfiguration { .. } => 6,
            Self::WriteZero => 7,
            Self::WriteIo { .. } => 8,
            Self::InvalidOperationClass { .. } => 9,
            Self::InvalidWriteCount { .. } => 10,
            Self::NoAuthorityProbe => 11,
        }
    }
}

/// 保留此次调用的全量已知进度和原期限；转换为 RobotError 后这些细节不再存在。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AbsoluteWriteFailure {
    pub reason: AbsoluteWriteReason,
    pub wanted: usize,
    pub got: usize,
    pub source_deadline: Deadline,
    pub call_deadline: Option<Deadline>,
    pub observed_at: Option<MonoRawNs>,
    pub previous_at: Option<MonoRawNs>,
    /// 当次 native Write 返回的原 IO kind；主要原因可被更早的时限门覆盖。
    pub last_io_error: Option<std::io::ErrorKind>,
    pub write_attempted: bool,
}

impl AbsoluteWriteFailure {
    /// 前置拒绝不代表底层写入尝试，不伪造调用预算或时钟观测。
    #[must_use]
    pub const fn before_write(
        reason: AbsoluteWriteReason,
        wanted: usize,
        source_deadline: Deadline,
        observed_at: Option<MonoRawNs>,
    ) -> Self {
        Self {
            reason,
            wanted,
            got: 0,
            source_deadline,
            call_deadline: None,
            observed_at,
            previous_at: None,
            last_io_error: None,
            write_attempted: false,
        }
    }

    #[must_use]
    pub const fn comm_kind(self) -> Option<CommErrorKind> {
        match self.reason {
            AbsoluteWriteReason::Unsupported
            | AbsoluteWriteReason::BudgetOverflow
            | AbsoluteWriteReason::InvalidOperationClass { .. }
            | AbsoluteWriteReason::NoAuthorityProbe => None,
            AbsoluteWriteReason::SourceExpired | AbsoluteWriteReason::CallBudgetExpired => {
                if self.write_attempted || self.got > 0 {
                    Some(CommErrorKind::IoWrite)
                } else {
                    Some(CommErrorKind::PendingDeadline)
                }
            }
            AbsoluteWriteReason::ClockRegressed { .. }
            | AbsoluteWriteReason::TimeoutConfiguration { .. }
            | AbsoluteWriteReason::WriteZero
            | AbsoluteWriteReason::WriteIo { .. }
            | AbsoluteWriteReason::InvalidWriteCount { .. } => Some(CommErrorKind::IoWrite),
        }
    }

    /// 诊断只编码进度类别，不截断原错误里的 usize 长度。
    #[must_use]
    pub const fn diagnostic_progress(self) -> u8 {
        match (self.write_attempted, self.got) {
            (false, 0) => 0,
            (_, 0) => 1,
            (_, got) if got < self.wanted => 2,
            _ => 3,
        }
    }
}

/// Feetech 协议层 / bus 层错误。
#[derive(Debug, thiserror::Error)]
pub enum FeetechError {
    // ── 配置 / 参数错误 ────────────────────────────────────────────────
    #[error("unknown register: {0}")]
    UnknownRegister(&'static str),

    #[error("本项目诊断专用寄存器禁止写入：{0}")]
    DiagnosticRegisterWriteForbidden(&'static str),

    #[error("固件版本只读寄存器禁止写入：{0}")]
    ReadOnlyRegisterWriteForbidden(&'static str),

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

    #[error("固定位置锚点不满足标定范围或raw/rad往返：motor={motor_id}, raw={raw}")]
    CalibrationAnchorChanged { motor_id: u8, raw: i32 },

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

    #[error("绝对期限写入失败：{0:?}")]
    AbsoluteWriteFailure(AbsoluteWriteFailure),

    #[error("接收缓冲清理失败：{0}")]
    DiscardFailed(#[source] std::io::Error),
}

impl FeetechError {
    /// 类型化通信原因；硬件状态码和永久参数问题不伪装成通信故障。
    #[must_use]
    pub fn comm_kind(&self) -> Option<robot_core::serial_services::CommErrorKind> {
        use robot_core::serial_services::CommErrorKind as C;
        match self {
            Self::AbsoluteWriteFailure(failure) => failure.comm_kind(),
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
            F::AbsoluteWriteFailure(failure) => {
                use AbsoluteWriteReason as R;
                match failure.reason {
                    R::Unsupported => Self::InvalidParameter("绝对期限写入不受支持"),
                    R::BudgetOverflow => Self::InvalidParameter("写入预算不可表示"),
                    R::InvalidOperationClass { .. } => {
                        Self::InvalidParameter("来源期限仅用于普通写入")
                    }
                    R::NoAuthorityProbe => Self::InvalidParameter("来源期限写入缺少执行权探针"),
                    R::SourceExpired | R::CallBudgetExpired => {
                        Self::Comm(if failure.write_attempted || failure.got > 0 {
                            CommErrorKind::IoWrite
                        } else {
                            CommErrorKind::PendingDeadline
                        })
                    }
                    R::ClockRegressed { .. }
                    | R::TimeoutConfiguration { .. }
                    | R::WriteZero
                    | R::WriteIo { .. }
                    | R::InvalidWriteCount { .. } => Self::Comm(CommErrorKind::IoWrite),
                }
            }
            F::DiagnosticRegisterWriteForbidden(_) => {
                Self::InvalidParameter("诊断专用寄存器禁止写入")
            }
            F::ReadOnlyRegisterWriteForbidden(_) => {
                Self::InvalidParameter("固件版本只读寄存器禁止写入")
            }
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
            | F::CalibrationAnchorChanged { .. }
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
    fn diagnostic_write_denial_is_a_parameter_error_without_communication_kind() {
        for name in ["Phase", "CW_Dead_Zone", "CCW_Dead_Zone"] {
            let error = FeetechError::DiagnosticRegisterWriteForbidden(name);
            assert_eq!(error.comm_kind(), None);
            assert_eq!(
                RobotError::from(error),
                RobotError::InvalidParameter("诊断专用寄存器禁止写入")
            );
        }
    }

    #[test]
    fn firmware_readonly_denial_keeps_specific_parameter_error() {
        for name in ["Firmware_Major_Version", "Firmware_Minor_Version"] {
            let error = FeetechError::ReadOnlyRegisterWriteForbidden(name);
            assert_eq!(error.comm_kind(), None);
            assert_eq!(
                RobotError::from(error),
                RobotError::InvalidParameter("固件版本只读寄存器禁止写入")
            );
        }
    }

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
