//! `tonic::Code` → CLI exit code 完整映射（AC-5）。
//!
//! exhaustive match 不留 `_` 兜底：新增 `tonic::Code` variant 会编译 fail
//! 强制更新本表（reviewer 审查重点 3）。

use tonic::Code;

/// 把 gRPC 状态码映射成 CLI 进程退出码（AC-5 完整表）。
///
/// 退出码 `1`（内部 fatal）与 `2`（CLI 参数错 / safety contract 早拒）来自
/// 非 RPC 路径，不在本函数；本函数只覆盖 `tonic::Code` 来源的
/// `{0, 3..=10}`。
#[must_use]
pub fn map_tonic_code_to_exit(code: Code) -> u8 {
    match code {
        Code::Ok => 0,
        Code::Unauthenticated | Code::PermissionDenied => 3,
        Code::FailedPrecondition
        | Code::NotFound
        | Code::AlreadyExists
        | Code::InvalidArgument
        | Code::Unimplemented => 4,
        Code::Unavailable => 5,
        Code::DeadlineExceeded => 6,
        Code::Aborted | Code::Cancelled => 7,
        Code::Internal | Code::Unknown => 8,
        Code::ResourceExhausted | Code::OutOfRange => 9,
        Code::DataLoss => 10,
    }
}

/// `tonic::Code` 的 `ALL_CAPS` 名字（stderr 文案 `error: <CODE>: <message>`，
/// AC-5）。exhaustive match 锁住每个 variant。
#[must_use]
pub fn code_label(code: Code) -> &'static str {
    match code {
        Code::Ok => "OK",
        Code::Cancelled => "CANCELLED",
        Code::Unknown => "UNKNOWN",
        Code::InvalidArgument => "INVALID_ARGUMENT",
        Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
        Code::NotFound => "NOT_FOUND",
        Code::AlreadyExists => "ALREADY_EXISTS",
        Code::PermissionDenied => "PERMISSION_DENIED",
        Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
        Code::FailedPrecondition => "FAILED_PRECONDITION",
        Code::Aborted => "ABORTED",
        Code::OutOfRange => "OUT_OF_RANGE",
        Code::Unimplemented => "UNIMPLEMENTED",
        Code::Internal => "INTERNAL",
        Code::Unavailable => "UNAVAILABLE",
        Code::DataLoss => "DATA_LOSS",
        Code::Unauthenticated => "UNAUTHENTICATED",
    }
}

#[cfg(test)]
mod tests {
    use super::{code_label, map_tonic_code_to_exit};
    use tonic::Code;

    #[test]
    fn exit_code_map_covers_ac5_table() {
        assert_eq!(map_tonic_code_to_exit(Code::Ok), 0);
        assert_eq!(map_tonic_code_to_exit(Code::Unauthenticated), 3);
        assert_eq!(map_tonic_code_to_exit(Code::PermissionDenied), 3);
        assert_eq!(map_tonic_code_to_exit(Code::FailedPrecondition), 4);
        assert_eq!(map_tonic_code_to_exit(Code::NotFound), 4);
        assert_eq!(map_tonic_code_to_exit(Code::AlreadyExists), 4);
        assert_eq!(map_tonic_code_to_exit(Code::InvalidArgument), 4);
        assert_eq!(map_tonic_code_to_exit(Code::Unimplemented), 4);
        assert_eq!(map_tonic_code_to_exit(Code::Unavailable), 5);
        assert_eq!(map_tonic_code_to_exit(Code::DeadlineExceeded), 6);
        assert_eq!(map_tonic_code_to_exit(Code::Aborted), 7);
        assert_eq!(map_tonic_code_to_exit(Code::Cancelled), 7);
        assert_eq!(map_tonic_code_to_exit(Code::Internal), 8);
        assert_eq!(map_tonic_code_to_exit(Code::Unknown), 8);
        assert_eq!(map_tonic_code_to_exit(Code::ResourceExhausted), 9);
        assert_eq!(map_tonic_code_to_exit(Code::OutOfRange), 9);
        assert_eq!(map_tonic_code_to_exit(Code::DataLoss), 10);
    }

    #[test]
    fn code_label_is_all_caps() {
        assert_eq!(code_label(Code::PermissionDenied), "PERMISSION_DENIED");
        assert_eq!(code_label(Code::FailedPrecondition), "FAILED_PRECONDITION");
        assert_eq!(code_label(Code::DeadlineExceeded), "DEADLINE_EXCEEDED");
        assert_eq!(code_label(Code::Ok), "OK");
    }
}
