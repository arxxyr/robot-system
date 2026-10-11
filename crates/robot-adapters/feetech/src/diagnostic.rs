//! 固定大小的诊断分类；只解释错误，不参与返回值转换、权限或故障处置。

use robot_core::serial_services::{AbortReason, CommErrorKind};

use crate::error::FeetechError;

/// 与既有 RobotError 参数分组一致，明确区分永久参数及校准问题。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ParameterReason {
    Argument,
    EncodedValue,
    Calibration,
    CalibrationFile,
    MotorModel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FeetechDiagnostic {
    Communication(CommErrorKind),
    Aborted(AbortReason),
    DeviceStatus { motor_id: u8, status: u8 },
    InvalidParameter(ParameterReason),
    GoalReadbackMismatch,
    AlreadyConnected,
    AbsoluteWrite { reason: u8, progress: u8 },
}

impl FeetechDiagnostic {
    /// 穷尽原始错误，新增变体必须明确分类，不能默认为通信 Unknown。
    pub(super) fn classify(error: &FeetechError, status_motor: u8) -> Self {
        use CommErrorKind as C;
        use FeetechError as F;
        use ParameterReason as P;
        match error {
            F::AbsoluteWriteFailure(failure) => Self::AbsoluteWrite {
                reason: failure.reason.diagnostic_code(),
                progress: failure.diagnostic_progress(),
            },
            F::NotConnected => Self::Communication(C::Disconnected),
            F::TransportTimeout { .. } => Self::Communication(C::Timeout),
            F::TransportWriteTimeout { .. } | F::TransportWrite(_) => {
                Self::Communication(C::IoWrite)
            }
            F::TransportRead(_) => Self::Communication(C::IoRead),
            F::TransportOpen { .. } => Self::Communication(C::OpenFailed),
            F::BadPacketHeader { .. }
            | F::BadPacketLength { .. }
            | F::BadChecksum { .. }
            | F::PacketTooShort { .. } => Self::Communication(C::FrameCorrupt),
            F::ResponseIdMismatch { .. } => Self::Communication(C::ResponseIdMismatch),
            F::PingNoResponse(_) | F::HandshakeMissing { .. } => {
                Self::Communication(C::PeerNoResponse)
            }
            F::DiscardFailed(_) => Self::Communication(C::DiscardFailed),
            F::Aborted(reason) => Self::Aborted(*reason),
            F::StatusError(status) => Self::DeviceStatus {
                motor_id: status_motor,
                status: *status,
            },
            F::UnknownRegister(_)
            | F::DiagnosticRegisterWriteForbidden(_)
            | F::ReadOnlyRegisterWriteForbidden(_)
            | F::UnknownMotor(_)
            | F::InvalidMotorId(_)
            | F::DuplicateMotorId(_)
            | F::UnsupportedByteLength(_)
            | F::ByteLengthMismatch { .. } => Self::InvalidParameter(P::Argument),
            F::MagnitudeOutOfRange { .. } | F::ValueExceedsByteRange { .. } => {
                Self::InvalidParameter(P::EncodedValue)
            }
            F::CalibrationOffsetOutOfRange(_)
            | F::CalibrationMinOutOfRange(_)
            | F::CalibrationMaxOutOfRange(_)
            | F::CalibrationMotorIdMismatch { .. }
            | F::CalibrationEntryInvalid { .. }
            | F::CalibrationDuplicateMotor { .. } => Self::InvalidParameter(P::Calibration),
            F::CalibrationIo { .. } | F::CalibrationParse { .. } => {
                Self::InvalidParameter(P::CalibrationFile)
            }
            F::PingModelMismatch { .. } | F::HandshakeWrongModel { .. } => {
                Self::InvalidParameter(P::MotorModel)
            }
            F::SyncGoalReadbackMismatch { .. } => Self::GoalReadbackMismatch,
            F::AlreadyConnected => Self::AlreadyConnected,
        }
    }

    /// 固定日志编码 (kind, a, b)；序号字段由调用方继续置零。
    /// kind8 的 a：1参数、2编码值、3校准、4校准文件、5型号、6目标读回、7重复连接。
    pub(super) const fn encode(self) -> (u8, u8, u8) {
        match self {
            Self::Communication(kind) => (0, kind.to_u8(), 0),
            Self::DeviceStatus { motor_id, status } => (5, motor_id, status),
            Self::Aborted(reason) => (
                7,
                match reason {
                    AbortReason::Emergency => 1,
                    AbortReason::Timeout => 2,
                    AbortReason::Revoked => 3,
                },
                0,
            ),
            Self::InvalidParameter(reason) => (
                8,
                match reason {
                    ParameterReason::Argument => 1,
                    ParameterReason::EncodedValue => 2,
                    ParameterReason::Calibration => 3,
                    ParameterReason::CalibrationFile => 4,
                    ParameterReason::MotorModel => 5,
                },
                0,
            ),
            Self::GoalReadbackMismatch => (8, 6, 0),
            Self::AlreadyConnected => (8, 7, 0),
            Self::AbsoluteWrite { reason, progress } => (9, reason, progress),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FeetechDiagnostic as D, ParameterReason as P};
    use crate::error::FeetechError as F;
    use crate::error::{AbsoluteWriteFailure, AbsoluteWriteReason as R};
    use robot_core::error::RobotError;
    use robot_core::serial_services::BusOpClass;
    use robot_core::serial_services::{AbortReason, CommErrorKind as C};
    use robot_core::time::{Deadline, MonoRawNs};
    use std::io;

    #[test]
    fn typed_classes_keep_literal_diagnostic_codes() {
        for (error, expected, encoded) in [
            (
                F::ReadOnlyRegisterWriteForbidden("Firmware_Major_Version"),
                D::InvalidParameter(P::Argument),
                (8, 1, 0),
            ),
            (
                F::ReadOnlyRegisterWriteForbidden("Firmware_Minor_Version"),
                D::InvalidParameter(P::Argument),
                (8, 1, 0),
            ),
            (
                F::UnknownRegister("unknown"),
                D::InvalidParameter(P::Argument),
                (8, 1, 0),
            ),
            (
                F::ValueExceedsByteRange {
                    value: 256,
                    max_bytes: 1,
                    max: 255,
                },
                D::InvalidParameter(P::EncodedValue),
                (8, 2, 0),
            ),
            (
                F::CalibrationMinOutOfRange(-1),
                D::InvalidParameter(P::Calibration),
                (8, 3, 0),
            ),
            (
                F::CalibrationIo {
                    path: "calibration.json".into(),
                    source: std::io::ErrorKind::NotFound.into(),
                },
                D::InvalidParameter(P::CalibrationFile),
                (8, 4, 0),
            ),
            (
                F::HandshakeWrongModel { id: 7, got: 1 },
                D::InvalidParameter(P::MotorModel),
                (8, 5, 0),
            ),
            (
                F::SyncGoalReadbackMismatch {
                    motor_id: 7,
                    wrote: 2047,
                    got: 0,
                    attempts: 3,
                },
                D::GoalReadbackMismatch,
                (8, 6, 0),
            ),
            (F::AlreadyConnected, D::AlreadyConnected, (8, 7, 0)),
            (
                F::StatusError(0x20),
                D::DeviceStatus {
                    motor_id: 7,
                    status: 0x20,
                },
                (5, 7, 0x20),
            ),
            (
                F::Aborted(AbortReason::Emergency),
                D::Aborted(AbortReason::Emergency),
                (7, 1, 0),
            ),
            (
                F::Aborted(AbortReason::Timeout),
                D::Aborted(AbortReason::Timeout),
                (7, 2, 0),
            ),
            (
                F::Aborted(AbortReason::Revoked),
                D::Aborted(AbortReason::Revoked),
                (7, 3, 0),
            ),
        ] {
            assert_eq!(error.comm_kind(), None);
            let diagnostic = D::classify(&error, 7);
            assert_eq!(diagnostic, expected);
            assert_eq!(diagnostic.encode(), encoded);
        }
        for name in [
            "Phase",
            "Minimum_Starting_Torque",
            "Integral_Limit",
            "CW_Dead_Zone",
            "CCW_Dead_Zone",
            "Angle_Resolution",
            "Acceleration",
            "Torque_Limit",
            "Present_Load",
        ] {
            let error = F::DiagnosticRegisterWriteForbidden(name);
            assert_eq!(error.comm_kind(), None);
            let diagnostic = D::classify(&error, 7);
            assert_eq!(diagnostic, D::InvalidParameter(P::Argument));
            assert_eq!(diagnostic.encode(), (8, 1, 0));
        }
    }

    #[test]
    fn communication_classes_match_existing_error_mapping() {
        for (error, kind, code) in [
            (F::NotConnected, C::Disconnected, 1),
            (
                F::TransportTimeout {
                    timeout_ms: 50,
                    wanted: 8,
                    got: 0,
                },
                C::Timeout,
                0,
            ),
            (
                F::TransportWriteTimeout {
                    timeout_ms: 50,
                    wanted: 8,
                    got: 0,
                },
                C::IoWrite,
                3,
            ),
            (
                F::TransportWrite(std::io::ErrorKind::BrokenPipe.into()),
                C::IoWrite,
                3,
            ),
            (
                F::TransportRead(std::io::ErrorKind::BrokenPipe.into()),
                C::IoRead,
                2,
            ),
            (
                F::TransportOpen {
                    path: "serial".into(),
                    source: std::io::ErrorKind::NotFound.into(),
                },
                C::OpenFailed,
                4,
            ),
            (F::BadPacketHeader { got: [0, 0] }, C::FrameCorrupt, 5),
            (
                F::BadPacketLength {
                    len: 1,
                    body_len: 2,
                },
                C::FrameCorrupt,
                5,
            ),
            (
                F::BadChecksum {
                    expected: 1,
                    got: 2,
                },
                C::FrameCorrupt,
                5,
            ),
            (F::PacketTooShort { needed: 8, got: 1 }, C::FrameCorrupt, 5),
            (
                F::ResponseIdMismatch {
                    expected: 7,
                    got: 8,
                },
                C::ResponseIdMismatch,
                6,
            ),
            (F::PingNoResponse(7), C::PeerNoResponse, 7),
            (F::HandshakeMissing { id: 7 }, C::PeerNoResponse, 7),
            (
                F::DiscardFailed(std::io::ErrorKind::BrokenPipe.into()),
                C::DiscardFailed,
                8,
            ),
        ] {
            assert_eq!(error.comm_kind(), Some(kind));
            let diagnostic = D::classify(&error, 7);
            assert_eq!(diagnostic, D::Communication(kind));
            assert_eq!(diagnostic.encode(), (0, code, 0));
        }
    }
    #[test]
    fn absolute_error_conversion_and_diagnostic_reason_are_exhaustive() {
        let reasons = [
            R::Unsupported,
            R::SourceExpired,
            R::CallBudgetExpired,
            R::ClockRegressed {
                previous_at: MonoRawNs(8),
            },
            R::BudgetOverflow,
            R::TimeoutConfiguration {
                kind: io::ErrorKind::PermissionDenied,
            },
            R::WriteZero,
            R::WriteIo {
                kind: io::ErrorKind::BrokenPipe,
            },
            R::InvalidOperationClass {
                class: BusOpClass::Safety,
            },
            R::InvalidWriteCount {
                reported: usize::MAX,
            },
            R::NoAuthorityProbe,
        ];
        for (index, reason) in reasons.into_iter().enumerate() {
            let capability = matches!(
                reason,
                R::Unsupported
                    | R::BudgetOverflow
                    | R::InvalidOperationClass { .. }
                    | R::NoAuthorityProbe
            );
            for (attempt, got, progress) in [
                (false, 0, 0),
                (true, 0, 1),
                (true, usize::MAX - 1, 2),
                (true, usize::MAX, 3),
            ] {
                if capability && attempt {
                    continue;
                }
                let value = AbsoluteWriteFailure {
                    reason,
                    wanted: usize::MAX,
                    got,
                    source_deadline: Deadline(MonoRawNs(100)),
                    call_deadline: Some(Deadline(MonoRawNs(50))),
                    observed_at: Some(MonoRawNs(10)),
                    previous_at: Some(MonoRawNs(9)),
                    last_io_error: Some(io::ErrorKind::BrokenPipe),
                    write_attempted: attempt,
                };
                assert_no_alloc::assert_no_alloc(|| {
                    let error = F::AbsoluteWriteFailure(value);
                    assert_eq!(
                        D::classify(&error, 0).encode(),
                        (9, u8::try_from(index + 1).unwrap(), progress)
                    );
                    let expected_comm = match reason {
                        R::Unsupported
                        | R::BudgetOverflow
                        | R::InvalidOperationClass { .. }
                        | R::NoAuthorityProbe => None,
                        R::SourceExpired | R::CallBudgetExpired if !attempt => {
                            Some(C::PendingDeadline)
                        }
                        _ => Some(C::IoWrite),
                    };
                    assert_eq!(error.comm_kind(), expected_comm);
                    let converted = RobotError::from(error);
                    let expected = match reason {
                        R::Unsupported => RobotError::InvalidParameter("绝对期限写入不受支持"),
                        R::BudgetOverflow => RobotError::InvalidParameter("写入预算不可表示"),
                        R::InvalidOperationClass { .. } => {
                            RobotError::InvalidParameter("来源期限仅用于普通写入")
                        }
                        R::NoAuthorityProbe => {
                            RobotError::InvalidParameter("来源期限写入缺少执行权探针")
                        }
                        _ => RobotError::Comm(expected_comm.unwrap()),
                    };
                    assert_eq!(converted, expected);
                    assert_eq!(
                        (
                            value.wanted,
                            value.got,
                            value.source_deadline,
                            value.last_io_error
                        ),
                        (
                            usize::MAX,
                            got,
                            Deadline(MonoRawNs(100)),
                            Some(io::ErrorKind::BrokenPipe)
                        )
                    );
                });
            }
        }
    }
}
