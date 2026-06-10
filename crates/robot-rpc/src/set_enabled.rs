//! `SetEnabled` validation + idempotency fingerprint（v0.5 候选 C，沿
//! [`crate::clear_estop`] 范式）。
//!
//! ## 边界（沿 ClearEStop A1 拍板）
//!
//! - **`enabled` 无格式约束**：任意 `bool` 合法（区别 ClearEStop 的
//!   `confirmation` 必填非空）。
//! - **dry_run 仅解析**：本 module **不**短路返回 Ok；`meta.dry_run` 透传给
//!   S0 action layer 真评估 precondition（reviewer A1 第 2 拍板 / D7）。
//! - **授权 / idempotency 不在此**：role 查表走 [`crate::auth::RoleTable`]，
//!   cache 走 [`crate::idempotency`]，调用方（gateway handler）wrap。
//! - **`RpcStatus` → `tonic::Code` 复用** [`crate::clear_estop::
//!   rpc_status_to_tonic_code`]（**不**另造 mapper，Plan §P.2.4）。

use thiserror::Error;
use tonic::Code;

use crate::auth::{RequestIdError, validate_request_id};

/// 验证 [`crate::proto::SetEnabledCommand`] 通过的 RPC 入参。
///
/// 转 `robot-msg::SetEnabledRequest` wire bytes 通过 RR client publish 到
/// S0 `set_enabled` RR server。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSetEnabled {
    pub request_id: String,
    pub idempotency_key: String,
    pub source: String,
    /// `true` = enable / `false` = disable。
    pub enabled: bool,
    pub dry_run: bool,
    pub audit_reason: String,
    pub timeout_ms: u64,
    pub auth_token: String,
}

/// `SetEnabled` 入参验证错误。沿 ClearEStop [`crate::clear_estop::
/// ValidationError`]，**去掉** confirmation 相关（SetEnabled 无 confirmation）。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SetEnabledValidationError {
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
}

impl SetEnabledValidationError {
    /// 把 validation 错误转 `tonic::Code`。全部 → `InvalidArgument`（客户端入
    /// 参不对）。**不走** `as i32` cast。
    #[must_use]
    pub fn to_tonic_code(&self) -> Code {
        match self {
            Self::MissingMeta
            | Self::RequestId(_)
            | Self::EmptyIdempotencyKey
            | Self::EmptySource
            | Self::EmptyAuthToken
            | Self::ZeroTimeout => Code::InvalidArgument,
        }
    }
}

/// 验证 [`crate::proto::SetEnabledCommand`] 入参 + 抽出 [`ValidatedSetEnabled`]。
///
/// # Errors
///
/// 见 [`SetEnabledValidationError`]。
pub fn validate_set_enabled_command(
    cmd: &crate::proto::SetEnabledCommand,
) -> Result<ValidatedSetEnabled, SetEnabledValidationError> {
    let meta = cmd
        .meta
        .as_ref()
        .ok_or(SetEnabledValidationError::MissingMeta)?;
    let auth = meta
        .auth
        .as_ref()
        .ok_or(SetEnabledValidationError::EmptyAuthToken)?;

    if auth.token.is_empty() {
        return Err(SetEnabledValidationError::EmptyAuthToken);
    }
    let _request_uuid = validate_request_id(&meta.request_id)?;
    if meta.idempotency_key.is_empty() {
        return Err(SetEnabledValidationError::EmptyIdempotencyKey);
    }
    if meta.source.is_empty() {
        return Err(SetEnabledValidationError::EmptySource);
    }
    if meta.timeout_ms == 0 {
        return Err(SetEnabledValidationError::ZeroTimeout);
    }

    Ok(ValidatedSetEnabled {
        request_id: meta.request_id.clone(),
        idempotency_key: meta.idempotency_key.clone(),
        source: meta.source.clone(),
        enabled: cmd.enabled,
        dry_run: meta.dry_run,
        audit_reason: meta.audit_reason.clone(),
        timeout_ms: meta.timeout_ms,
        auth_token: auth.token.clone(),
    })
}

/// 算 [`ValidatedSetEnabled`] 的 idempotency fingerprint（32-byte blake3）。
///
/// **进** fingerprint（语义字段；retry 必须不变）：`source` / **`enabled`**
/// （enable↔disable 翻转 = 不同意图，**绝不**互相 replay）/ `dry_run`（翻转 =
/// 行为彻底改变）/ `audit_reason` / `auth_token`。
/// **不进**：`request_id` / `timeout_ms` / `idempotency_key`（沿 ClearEStop
/// [`crate::clear_estop::clear_estop_idempotency_fingerprint`] 字段分类）。
#[must_use]
pub fn set_enabled_idempotency_fingerprint(v: &ValidatedSetEnabled) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    fingerprint_field(&mut h, "source", v.source.as_bytes());
    fingerprint_field(&mut h, "enabled", &[u8::from(v.enabled)]);
    fingerprint_field(&mut h, "dry_run", &[u8::from(v.dry_run)]);
    fingerprint_field(&mut h, "audit_reason", v.audit_reason.as_bytes());
    fingerprint_field(&mut h, "auth_token", v.auth_token.as_bytes());
    h.finalize().into()
}

/// length-prefixed `(label, value)` 更新 hasher（防 `"ab"+"c"` vs `"a"+"bc"`
/// 歧义）。沿 ClearEStop 同款；`label` 硬编码源码不受外部输入污染。
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
    use crate::proto::{AuthContext, CommandMeta, SetEnabledCommand};
    use uuid::Uuid;

    fn fresh_cmd(enabled: bool) -> SetEnabledCommand {
        SetEnabledCommand {
            meta: Some(CommandMeta {
                request_id: Uuid::now_v7().to_string(),
                source: "test".into(),
                auth: Some(AuthContext {
                    token: "operator-fake-token-001".into(),
                    mtls_fingerprint: String::new(),
                }),
                timeout_ms: 5_000,
                idempotency_key: "test-idem-se-001".into(),
                dry_run: false,
                audit_reason: "unit test".into(),
            }),
            enabled,
        }
    }

    #[test]
    fn validates_happy_path() {
        let v = validate_set_enabled_command(&fresh_cmd(true)).unwrap();
        assert_eq!(v.idempotency_key, "test-idem-se-001");
        assert!(v.enabled);
        assert!(!v.dry_run);
        assert_eq!(v.auth_token, "operator-fake-token-001");
    }

    #[test]
    fn rejects_missing_meta() {
        let mut cmd = fresh_cmd(false);
        cmd.meta = None;
        assert_eq!(
            validate_set_enabled_command(&cmd),
            Err(SetEnabledValidationError::MissingMeta)
        );
    }

    #[test]
    fn rejects_empty_idempotency_key_source_token_zero_timeout() {
        let mut c = fresh_cmd(true);
        c.meta.as_mut().unwrap().idempotency_key = String::new();
        assert_eq!(
            validate_set_enabled_command(&c),
            Err(SetEnabledValidationError::EmptyIdempotencyKey)
        );

        let mut c = fresh_cmd(true);
        c.meta.as_mut().unwrap().source = String::new();
        assert_eq!(
            validate_set_enabled_command(&c),
            Err(SetEnabledValidationError::EmptySource)
        );

        let mut c = fresh_cmd(true);
        c.meta.as_mut().unwrap().auth.as_mut().unwrap().token = String::new();
        assert_eq!(
            validate_set_enabled_command(&c),
            Err(SetEnabledValidationError::EmptyAuthToken)
        );

        let mut c = fresh_cmd(true);
        c.meta.as_mut().unwrap().timeout_ms = 0;
        assert_eq!(
            validate_set_enabled_command(&c),
            Err(SetEnabledValidationError::ZeroTimeout)
        );
    }

    #[test]
    fn rejects_non_uuidv7_request_id() {
        let mut c = fresh_cmd(true);
        c.meta.as_mut().unwrap().request_id = "not-a-uuid".into();
        assert!(matches!(
            validate_set_enabled_command(&c),
            Err(SetEnabledValidationError::RequestId(_))
        ));
    }

    #[test]
    fn fingerprint_excludes_request_id_and_timeout() {
        // 同语义字段、不同 request_id + timeout → 同 fingerprint（合法 retry 走 Replay）。
        let a = validate_set_enabled_command(&fresh_cmd(true)).unwrap();
        let mut c = fresh_cmd(true);
        c.meta.as_mut().unwrap().request_id = Uuid::now_v7().to_string();
        c.meta.as_mut().unwrap().timeout_ms = 9_999;
        let b = validate_set_enabled_command(&c).unwrap();
        assert_eq!(
            set_enabled_idempotency_fingerprint(&a),
            set_enabled_idempotency_fingerprint(&b),
            "request_id / timeout 不应进 fingerprint"
        );
    }

    #[test]
    fn fingerprint_distinguishes_enable_from_disable() {
        // enable ↔ disable 同 idempotency_key **绝不**互相 replay。
        let enable = validate_set_enabled_command(&fresh_cmd(true)).unwrap();
        let disable = validate_set_enabled_command(&fresh_cmd(false)).unwrap();
        assert_ne!(
            set_enabled_idempotency_fingerprint(&enable),
            set_enabled_idempotency_fingerprint(&disable),
            "enabled 翻转必须改 fingerprint"
        );
    }

    #[test]
    fn fingerprint_distinguishes_dry_run() {
        let live = validate_set_enabled_command(&fresh_cmd(true)).unwrap();
        let mut c = fresh_cmd(true);
        c.meta.as_mut().unwrap().dry_run = true;
        let dry = validate_set_enabled_command(&c).unwrap();
        assert_ne!(
            set_enabled_idempotency_fingerprint(&live),
            set_enabled_idempotency_fingerprint(&dry),
            "dry_run 翻转必须改 fingerprint"
        );
    }
}
