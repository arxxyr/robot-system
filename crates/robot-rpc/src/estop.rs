//! `EStop` 触发 RPC validation + idempotency fingerprint（v0.5 候选 D，沿
//! [`crate::set_enabled`] / [`crate::clear_estop`] 范式）。
//!
//! ## 边界（Spec D4/D6）
//!
//! - **`dry_run` 不支持**（D4）：EStop 无 precondition gate，无可评估物；
//!   `meta.dry_run=true` → [`EStopValidationError::DryRunUnsupported`]
//!   （`InvalidArgument`）——validate 步即拒，**先于** idempotency cache
//!   （不入 cache、不 publish）。比静默吞（候选 A "假 dry-run" 洞）和无意义
//!   的 would-publish 都诚实。
//! - **`estop_reason` 空串合法**（D6）：按钮触发也没 reason；**不**强制非空
//!   （区别 ClearEStop `confirmation` 必填）。
//! - 授权 / cache 不在此：role 查表走 [`crate::auth::RoleTable`]
//!   （`Capability::EStop`，operator+），cache 走 [`crate::idempotency`]，
//!   调用方（gateway handler）wrap。

use thiserror::Error;
use tonic::Code;

use crate::auth::{RequestIdError, validate_request_id};

/// 验证 [`crate::proto::EStopCommand`] 通过的 RPC 入参。
///
/// **无 `dry_run` 字段**：D4 在 [`validate_estop_command`] 即拒，验证通过的
/// 入参恒为 live。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedEStop {
    pub request_id: String,
    pub idempotency_key: String,
    pub source: String,
    /// 机器侧原因（透传 wire `EStopEvent.reason`）；**空串合法**。
    pub estop_reason: String,
    pub audit_reason: String,
    pub timeout_ms: u64,
    pub auth_token: String,
}

/// `EStop` 入参验证错误。沿 SetEnabled 集合 **+ `DryRunUnsupported`**（D4）。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EStopValidationError {
    #[error("missing CommandMeta")]
    MissingMeta,
    #[error("request_id invalid: {0}")]
    RequestId(#[from] RequestIdError),
    #[error("idempotency_key 为空字符串（必填）")]
    EmptyIdempotencyKey,
    #[error("source 为空字符串（必填）")]
    EmptySource,
    #[error("auth.token 为空（v0.5 接受 fake token 但必须非空）")]
    EmptyAuthToken,
    #[error("timeout_ms = 0（必须 > 0；客户端 monotonic 相对超时）")]
    ZeroTimeout,
    #[error("EStop does not support dry_run（无 precondition gate，无可评估物；D4）")]
    DryRunUnsupported,
}

impl EStopValidationError {
    /// 把 validation 错误转 `tonic::Code`。全部 → `InvalidArgument`。
    /// **不走** `as i32` cast。
    #[must_use]
    pub fn to_tonic_code(&self) -> Code {
        match self {
            Self::MissingMeta
            | Self::RequestId(_)
            | Self::EmptyIdempotencyKey
            | Self::EmptySource
            | Self::EmptyAuthToken
            | Self::ZeroTimeout
            | Self::DryRunUnsupported => Code::InvalidArgument,
        }
    }
}

/// 验证 [`crate::proto::EStopCommand`] 入参 + 抽出 [`ValidatedEStop`]。
///
/// # Errors
///
/// 见 [`EStopValidationError`]；`meta.dry_run=true` → `DryRunUnsupported`（D4）。
pub fn validate_estop_command(
    cmd: &crate::proto::EStopCommand,
) -> Result<ValidatedEStop, EStopValidationError> {
    let meta = cmd.meta.as_ref().ok_or(EStopValidationError::MissingMeta)?;
    let auth = meta
        .auth
        .as_ref()
        .ok_or(EStopValidationError::EmptyAuthToken)?;

    if auth.token.is_empty() {
        return Err(EStopValidationError::EmptyAuthToken);
    }
    let _request_uuid = validate_request_id(&meta.request_id)?;
    if meta.idempotency_key.is_empty() {
        return Err(EStopValidationError::EmptyIdempotencyKey);
    }
    if meta.source.is_empty() {
        return Err(EStopValidationError::EmptySource);
    }
    if meta.timeout_ms == 0 {
        return Err(EStopValidationError::ZeroTimeout);
    }
    if meta.dry_run {
        return Err(EStopValidationError::DryRunUnsupported);
    }

    Ok(ValidatedEStop {
        request_id: meta.request_id.clone(),
        idempotency_key: meta.idempotency_key.clone(),
        source: meta.source.clone(),
        estop_reason: cmd.estop_reason.clone(),
        audit_reason: meta.audit_reason.clone(),
        timeout_ms: meta.timeout_ms,
        auth_token: auth.token.clone(),
    })
}

/// 算 [`ValidatedEStop`] 的 idempotency fingerprint（32-byte blake3）。
///
/// **进** fingerprint（语义字段；retry 必须不变）：`source` / `estop_reason` /
/// `audit_reason` / `auth_token`。**不进**：`request_id` / `timeout_ms` /
/// `idempotency_key`（沿 [`crate::clear_estop::clear_estop_idempotency_
/// fingerprint`] 字段分类）。**无 dry_run**（D4 validate 即拒，永不到本步）。
#[must_use]
pub fn estop_idempotency_fingerprint(v: &ValidatedEStop) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    fingerprint_field(&mut h, "source", v.source.as_bytes());
    fingerprint_field(&mut h, "estop_reason", v.estop_reason.as_bytes());
    fingerprint_field(&mut h, "audit_reason", v.audit_reason.as_bytes());
    fingerprint_field(&mut h, "auth_token", v.auth_token.as_bytes());
    h.finalize().into()
}

/// length-prefixed `(label, value)` 更新 hasher（防拼接歧义；沿 ClearEStop /
/// SetEnabled 同款，label 硬编码源码不受外部输入污染）。
fn fingerprint_field(h: &mut blake3::Hasher, label: &str, value: &[u8]) {
    let label_bytes = label.as_bytes();
    h.update(
        &u32::try_from(label_bytes.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    h.update(label_bytes);
    h.update(&u32::try_from(value.len()).unwrap_or(u32::MAX).to_be_bytes());
    h.update(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{AuthContext, CommandMeta, EStopCommand};
    use uuid::Uuid;

    fn fresh_cmd(estop_reason: &str) -> EStopCommand {
        EStopCommand {
            meta: Some(CommandMeta {
                request_id: Uuid::now_v7().to_string(),
                source: "test.unit".into(),
                auth: Some(AuthContext {
                    token: "operator-fake-token-001".into(),
                    mtls_fingerprint: String::new(),
                }),
                timeout_ms: 2_000,
                idempotency_key: "test-idem-estop-001".into(),
                dry_run: false,
                audit_reason: "unit test".into(),
            }),
            estop_reason: estop_reason.into(),
        }
    }

    #[test]
    fn validates_happy_path() {
        let v = validate_estop_command(&fresh_cmd("collision imminent")).unwrap();
        assert_eq!(v.estop_reason, "collision imminent");
        assert_eq!(v.idempotency_key, "test-idem-estop-001");
        assert_eq!(v.auth_token, "operator-fake-token-001");
    }

    /// **D4**：`dry_run=true` → `DryRunUnsupported`（InvalidArgument），
    /// validate 步即拒（不入 cache / 不 publish 的前置保证）。
    #[test]
    fn rejects_dry_run_unsupported() {
        let mut cmd = fresh_cmd("r");
        cmd.meta.as_mut().unwrap().dry_run = true;
        let err = validate_estop_command(&cmd).unwrap_err();
        assert_eq!(err, EStopValidationError::DryRunUnsupported);
        assert_eq!(err.to_tonic_code(), Code::InvalidArgument);
    }

    /// **D6**：`estop_reason` 空串合法（按钮也没 reason；区别 ClearEStop
    /// confirmation 必填）。
    #[test]
    fn empty_estop_reason_is_legal() {
        let v = validate_estop_command(&fresh_cmd("")).unwrap();
        assert_eq!(v.estop_reason, "");
    }

    #[test]
    fn fingerprint_excludes_request_id_and_timeout() {
        let a = validate_estop_command(&fresh_cmd("r1")).unwrap();
        let mut c = fresh_cmd("r1");
        c.meta.as_mut().unwrap().request_id = Uuid::now_v7().to_string();
        c.meta.as_mut().unwrap().timeout_ms = 9_999;
        let b = validate_estop_command(&c).unwrap();
        assert_eq!(
            estop_idempotency_fingerprint(&a),
            estop_idempotency_fingerprint(&b),
            "request_id / timeout 不应进 fingerprint"
        );
    }

    #[test]
    fn fingerprint_distinguishes_estop_reason() {
        let a = validate_estop_command(&fresh_cmd("reason-A")).unwrap();
        let b = validate_estop_command(&fresh_cmd("reason-B")).unwrap();
        assert_ne!(
            estop_idempotency_fingerprint(&a),
            estop_idempotency_fingerprint(&b),
            "estop_reason 变 = 不同意图，必须改 fingerprint"
        );
    }

    #[test]
    fn rejects_missing_required_fields() {
        let mut c = fresh_cmd("r");
        c.meta = None;
        assert_eq!(
            validate_estop_command(&c),
            Err(EStopValidationError::MissingMeta)
        );

        let mut c = fresh_cmd("r");
        c.meta.as_mut().unwrap().idempotency_key = String::new();
        assert_eq!(
            validate_estop_command(&c),
            Err(EStopValidationError::EmptyIdempotencyKey)
        );

        let mut c = fresh_cmd("r");
        c.meta.as_mut().unwrap().timeout_ms = 0;
        assert_eq!(
            validate_estop_command(&c),
            Err(EStopValidationError::ZeroTimeout)
        );
    }
}
