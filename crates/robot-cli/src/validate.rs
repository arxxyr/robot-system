//! 客户端早拒校验（AC-3 clear-estop 四道闸 + AC-11 action-id）。

use std::fmt;

use robot_rpc::auth::{RequestIdError, validate_request_id};

use crate::meta::TOKEN_REQUIRED_MSG;

/// 校验 `<action-id>` 是合法 `UUIDv7`（复用
/// `robot_rpc::auth::validate_request_id`）。
///
/// 失败返回人类可读消息（CLI 打 stderr + exit 2；AC-11）。exhaustive match
/// 锁住 [`RequestIdError`] 三 variant。
pub fn validate_action_id(s: &str) -> Result<(), String> {
    match validate_request_id(s) {
        Ok(_) => Ok(()),
        Err(RequestIdError::Empty) => Err("<action-id> must not be empty".to_owned()),
        Err(RequestIdError::Malformed(v)) => Err(format!("<action-id> is not a valid UUID: {v}")),
        Err(RequestIdError::WrongVersion { got }) => {
            Err(format!("<action-id> must be UUIDv7, got version={got}"))
        }
    }
}

/// clear-estop `--confirmation` 必须精确等于的字面值（AC-3：全大写 ASCII，
/// **不**接受任何变体 / 前后空白）。
pub const CLEAR_ESTOP_CONFIRMATION: &str = "CONFIRM_CLEAR_ESTOP";

/// clear-estop 四道闸（AC-3）校验失败。`Display` = AC-3 精确 stderr 文案
/// （**不**含 `error: ` 前缀；binary 打印时统一加 `error: `），便于 lib
/// unit 锁字面。
#[derive(Debug, PartialEq, Eq)]
pub enum ClearEstopGateError {
    /// token 既无 `--token` 也无 env `ROBOT_TOKEN`。
    TokenMissing,
    /// 缺 `--confirmation`。
    ConfirmationMissing,
    /// `--confirmation` 不精确等于 [`CLEAR_ESTOP_CONFIRMATION`]。
    ConfirmationMismatch {
        /// 用户实际传入值（原样，含可能的前后空白）。
        got: String,
    },
    /// `--audit-reason` 缺失 / 空串 / 纯空白。
    AuditReasonMissing,
}

impl fmt::Display for ClearEstopGateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TokenMissing => write!(f, "{TOKEN_REQUIRED_MSG}"),
            Self::ConfirmationMissing => write!(
                f,
                "--confirmation is required for clear-estop (must be literal \"CONFIRM_CLEAR_ESTOP\")"
            ),
            Self::ConfirmationMismatch { got } => write!(
                f,
                "--confirmation must be exactly \"CONFIRM_CLEAR_ESTOP\", got: \"{got}\""
            ),
            Self::AuditReasonMissing => write!(
                f,
                "--audit-reason is required for clear-estop and must not be empty"
            ),
        }
    }
}

/// clear-estop CLI 早拒四道闸（AC-3）：顺序短路 **token → confirmation →
/// audit-reason**，任一失败返对应 [`ClearEstopGateError`]（binary 打 stderr
/// `error: <msg>` + exit 2 + **不**发 RPC，避免污染 gateway audit log）。
///
/// 纯函数：token 由调用方先经 [`crate::meta::resolve_token`] 解析为
/// `Option<&str>`（`None` / 空串 = 不可解析）；confirmation / audit-reason
/// 为 CLI 原始 `Option<&str>`。
///
/// # Errors
///
/// token 不可解析 / confirmation 缺失或不匹配 / audit-reason 缺失或纯空白。
pub fn check_clear_estop_gates(
    resolved_token: Option<&str>,
    confirmation: Option<&str>,
    audit_reason: Option<&str>,
) -> Result<(), ClearEstopGateError> {
    match resolved_token {
        Some(t) if !t.is_empty() => {}
        _ => return Err(ClearEstopGateError::TokenMissing),
    }
    match confirmation {
        None => return Err(ClearEstopGateError::ConfirmationMissing),
        Some(c) if c == CLEAR_ESTOP_CONFIRMATION => {}
        Some(c) => return Err(ClearEstopGateError::ConfirmationMismatch { got: c.to_owned() }),
    }
    match audit_reason {
        Some(r) if !r.trim().is_empty() => Ok(()),
        _ => Err(ClearEstopGateError::AuditReasonMissing),
    }
}

/// 通用 write-RPC CLI 早拒 gate（clear-error；后续 cancel-task / invoke 复用）：
/// token 可解析 + `--audit-reason` 非空非纯空白。clear-estop 走更严的
/// [`check_clear_estop_gates`]（多 confirmation 闸）。
#[derive(Debug, PartialEq, Eq)]
pub enum WriteRpcGateError {
    /// token 既无 `--token` 也无 env `ROBOT_TOKEN`。
    TokenMissing,
    /// `--audit-reason` 缺失 / 空串 / 纯空白。
    AuditReasonMissing,
}

impl fmt::Display for WriteRpcGateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TokenMissing => write!(f, "{TOKEN_REQUIRED_MSG}"),
            Self::AuditReasonMissing => {
                write!(f, "--audit-reason is required and must not be empty")
            }
        }
    }
}

/// 通用 write-RPC 早拒 gate（见 [`WriteRpcGateError`]）：顺序短路 token →
/// audit-reason。
///
/// # Errors
///
/// token 不可解析 / audit-reason 缺失或纯空白。
pub fn check_write_rpc_gates(
    resolved_token: Option<&str>,
    audit_reason: Option<&str>,
) -> Result<(), WriteRpcGateError> {
    match resolved_token {
        Some(t) if !t.is_empty() => {}
        _ => return Err(WriteRpcGateError::TokenMissing),
    }
    match audit_reason {
        Some(r) if !r.trim().is_empty() => Ok(()),
        _ => Err(WriteRpcGateError::AuditReasonMissing),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CLEAR_ESTOP_CONFIRMATION, ClearEstopGateError, WriteRpcGateError, check_clear_estop_gates,
        check_write_rpc_gates, validate_action_id,
    };

    #[test]
    fn accepts_uuid_v7() {
        let id = crate::uuid_v7::new_request_id();
        assert!(validate_action_id(&id).is_ok(), "{id}");
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(
            validate_action_id("").unwrap_err(),
            "<action-id> must not be empty"
        );
    }

    #[test]
    fn rejects_malformed() {
        let err = validate_action_id("not-a-uuid").unwrap_err();
        assert!(err.starts_with("<action-id> is not a valid UUID:"), "{err}");
    }

    #[test]
    fn rejects_v4() {
        // 版本 nibble = 4（第三组首字符）。
        let err = validate_action_id("550e8400-e29b-41d4-a716-446655440000").unwrap_err();
        assert_eq!(err, "<action-id> must be UUIDv7, got version=4");
    }

    // ====================================================================
    // clear-estop 四道闸（AC-3；reviewer 审查重点 2 —— lib unit 先于 e2e）
    // ====================================================================

    const GOOD_TOKEN: Option<&str> = Some("maintainer-fake-token-001");
    const GOOD_CONF: Option<&str> = Some(CLEAR_ESTOP_CONFIRMATION);
    const GOOD_AUDIT: Option<&str> = Some("e2e maintenance");

    #[test]
    fn clear_estop_gate_happy() {
        assert_eq!(
            check_clear_estop_gates(GOOD_TOKEN, GOOD_CONF, GOOD_AUDIT),
            Ok(())
        );
    }

    #[test]
    fn clear_estop_gate_token_missing_or_empty() {
        assert_eq!(
            check_clear_estop_gates(None, GOOD_CONF, GOOD_AUDIT),
            Err(ClearEstopGateError::TokenMissing)
        );
        assert_eq!(
            check_clear_estop_gates(Some(""), GOOD_CONF, GOOD_AUDIT),
            Err(ClearEstopGateError::TokenMissing)
        );
    }

    /// fail-path：缺 `--confirmation`（token / audit 良好以隔离）。
    #[test]
    fn clear_estop_gate_confirmation_missing() {
        assert_eq!(
            check_clear_estop_gates(GOOD_TOKEN, None, GOOD_AUDIT),
            Err(ClearEstopGateError::ConfirmationMissing)
        );
    }

    /// fail-path：`--confirmation` 不匹配（大小写 / 连字符 / 混合变体）。
    #[test]
    fn clear_estop_gate_confirmation_mismatch_variants() {
        for bad in [
            "confirm_clear_estop",
            "CONFIRM-CLEAR-ESTOP",
            "Confirm_Clear_Estop",
            "CONFIRM_CLEAR_ESTOPX",
        ] {
            let err = check_clear_estop_gates(GOOD_TOKEN, Some(bad), GOOD_AUDIT).unwrap_err();
            assert_eq!(
                err,
                ClearEstopGateError::ConfirmationMismatch {
                    got: bad.to_owned()
                },
                "{bad}"
            );
        }
    }

    /// fail-path：confirmation 含前后空白（裸 literal 不接受任何前后空白）。
    #[test]
    fn clear_estop_gate_confirmation_surrounding_whitespace_rejected() {
        let bad = " CONFIRM_CLEAR_ESTOP ";
        assert_eq!(
            check_clear_estop_gates(GOOD_TOKEN, Some(bad), GOOD_AUDIT),
            Err(ClearEstopGateError::ConfirmationMismatch {
                got: bad.to_owned()
            })
        );
    }

    /// fail-path：`--audit-reason` 缺 / 空 / 纯空白（confirmation 良好以隔离）。
    #[test]
    fn clear_estop_gate_audit_reason_missing_empty_whitespace() {
        for bad in [None, Some(""), Some("   "), Some("\t\n")] {
            assert_eq!(
                check_clear_estop_gates(GOOD_TOKEN, GOOD_CONF, bad),
                Err(ClearEstopGateError::AuditReasonMissing),
                "{bad:?}"
            );
        }
    }

    /// 顺序短路：token 先于 confirmation 先于 audit-reason。
    #[test]
    fn clear_estop_gate_short_circuits_in_order() {
        // token 缺 + confirmation 也错 + audit 也缺 → 先报 token。
        assert_eq!(
            check_clear_estop_gates(None, Some("wrong"), None),
            Err(ClearEstopGateError::TokenMissing)
        );
        // token 好 + confirmation 错 + audit 也缺 → 先报 confirmation。
        assert_eq!(
            check_clear_estop_gates(GOOD_TOKEN, Some("wrong"), None),
            Err(ClearEstopGateError::ConfirmationMismatch {
                got: "wrong".to_owned()
            })
        );
    }

    /// AC-3 精确 stderr 文案（不含 `error: ` 前缀；binary 打印时统一加）。
    #[test]
    fn clear_estop_gate_error_messages_match_ac3() {
        assert_eq!(
            ClearEstopGateError::TokenMissing.to_string(),
            "--token (or env ROBOT_TOKEN) is required"
        );
        assert_eq!(
            ClearEstopGateError::ConfirmationMissing.to_string(),
            "--confirmation is required for clear-estop (must be literal \"CONFIRM_CLEAR_ESTOP\")"
        );
        assert_eq!(
            ClearEstopGateError::ConfirmationMismatch {
                got: "confirm_clear_estop".to_owned()
            }
            .to_string(),
            "--confirmation must be exactly \"CONFIRM_CLEAR_ESTOP\", got: \"confirm_clear_estop\""
        );
        assert_eq!(
            ClearEstopGateError::AuditReasonMissing.to_string(),
            "--audit-reason is required for clear-estop and must not be empty"
        );
    }

    // ====================================================================
    // clear-error 通用 write-RPC gate（AC-4：token + audit-reason）
    // ====================================================================

    #[test]
    fn write_rpc_gate_happy() {
        assert_eq!(check_write_rpc_gates(Some("tok"), Some("reason")), Ok(()));
    }

    #[test]
    fn write_rpc_gate_token_missing() {
        assert_eq!(
            check_write_rpc_gates(None, Some("reason")),
            Err(WriteRpcGateError::TokenMissing)
        );
        assert_eq!(
            check_write_rpc_gates(Some(""), Some("reason")),
            Err(WriteRpcGateError::TokenMissing)
        );
    }

    #[test]
    fn write_rpc_gate_audit_missing_empty_whitespace() {
        for bad in [None, Some(""), Some("  ")] {
            assert_eq!(
                check_write_rpc_gates(Some("tok"), bad),
                Err(WriteRpcGateError::AuditReasonMissing),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn write_rpc_gate_error_messages() {
        assert_eq!(
            WriteRpcGateError::TokenMissing.to_string(),
            "--token (or env ROBOT_TOKEN) is required"
        );
        assert_eq!(
            WriteRpcGateError::AuditReasonMissing.to_string(),
            "--audit-reason is required and must not be empty"
        );
    }
}
