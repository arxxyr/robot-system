//! `CommandMeta` 构造 + auth token 解析 + source 默认（AC-6）。
//!
//! round 2 amend #2：**不**引 `whoami` / `rand` / `getrandom`；`source` 走
//! `USER` / `USERNAME` env fallback；`idempotency_key` 自动生成复用本次
//! `UUIDv7`（`cli-<request_id>`）。

use robot_rpc::proto::{AuthContext, CommandMeta};

use crate::uuid_v7::new_request_id;

/// `--token` 缺失时的统一 stderr 文案（AC-3 / AC-6）。token 解析失败
/// （[`TokenError`]）+ clear-estop 四道闸 + 通用 write-RPC gate 三处复用
/// 同一句，避免文案漂移。
pub const TOKEN_REQUIRED_MSG: &str = "--token (or env ROBOT_TOKEN) is required";

/// token 解析失败（`--token` 与 env `ROBOT_TOKEN` 都缺/空）。
#[derive(Debug, PartialEq, Eq)]
pub enum TokenError {
    /// 既无 `--token` 也无 env `ROBOT_TOKEN`（或都为空串）。
    Missing,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "{TOKEN_REQUIRED_MSG}"),
        }
    }
}

/// token 解析（Q-CLI3(a) / round 2 amend #1）：`--token` 优先 → 否则 env
/// `ROBOT_TOKEN` → 两者都缺/空 → `Err(Missing)`。
///
/// 纯函数（env 值由调用方读入传参），单测无需改全局 env。
pub fn resolve_token(
    flag: Option<&str>,
    env_robot_token: Option<&str>,
) -> Result<String, TokenError> {
    if let Some(t) = flag.filter(|t| !t.is_empty()) {
        return Ok(t.to_owned());
    }
    if let Some(t) = env_robot_token.filter(|t| !t.is_empty()) {
        return Ok(t.to_owned());
    }
    Err(TokenError::Missing)
}

/// `source` 默认值（round 2 amend #2；**不**引 `whoami`）：`cli.<user>`，
/// `user` = `USER`（Linux）→ `USERNAME`（Windows）→ `"unknown"`。
///
/// 纯函数（env 值由调用方读入传参）。
#[must_use]
pub fn default_cli_source(env_user: Option<&str>, env_username: Option<&str>) -> String {
    let user = env_user
        .filter(|u| !u.is_empty())
        .or_else(|| env_username.filter(|u| !u.is_empty()))
        .unwrap_or("unknown");
    format!("cli.{user}")
}

/// [`build_command_meta`] 输入（调用方已完成 token 解析 / source 默认 /
/// `audit_reason` 校验）。
pub struct MetaInputs<'a> {
    /// 已解析的 auth token（[`resolve_token`] 成功结果）。
    pub token: &'a str,
    /// 已确定的 source（`--source` 或 [`default_cli_source`]）。
    pub source: &'a str,
    /// `--idempotency-key`；`None`（或空串）= 自动生成 `cli-<request_id>`
    /// （Q-CLI5(b)）。
    pub idempotency_key: Option<&'a str>,
    /// client 超时（ms）；默认 2000。
    pub timeout_ms: u64,
    /// `--dry-run`。
    pub dry_run: bool,
    /// audit reason（写 RPC 非空、读 RPC 空串；调用方已校验）。
    pub audit_reason: &'a str,
}

/// 构造 `CommandMeta`（AC-6）：`request_id` 一次生成；`idempotency_key`
/// 自动模式复用同一 `request_id`（`cli-<request_id>`，round 2 amend #2）。
#[must_use]
pub fn build_command_meta(inputs: &MetaInputs) -> CommandMeta {
    let request_id = new_request_id();
    let idempotency_key = inputs
        .idempotency_key
        .filter(|k| !k.is_empty())
        .map_or_else(|| format!("cli-{request_id}"), str::to_owned);
    CommandMeta {
        request_id,
        source: inputs.source.to_owned(),
        auth: Some(AuthContext {
            token: inputs.token.to_owned(),
            mtls_fingerprint: String::new(),
        }),
        timeout_ms: inputs.timeout_ms,
        idempotency_key,
        dry_run: inputs.dry_run,
        audit_reason: inputs.audit_reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{MetaInputs, TokenError, build_command_meta, default_cli_source, resolve_token};

    fn inputs(idempotency_key: Option<&str>) -> MetaInputs<'_> {
        MetaInputs {
            token: "tok",
            source: "cli.test",
            idempotency_key,
            timeout_ms: 2000,
            dry_run: false,
            audit_reason: "",
        }
    }

    #[test]
    fn resolve_token_prefers_flag() {
        assert_eq!(
            resolve_token(Some("flagtok"), Some("envtok")),
            Ok("flagtok".to_owned())
        );
    }

    #[test]
    fn resolve_token_falls_back_to_env() {
        assert_eq!(resolve_token(None, Some("envtok")), Ok("envtok".to_owned()));
        assert_eq!(
            resolve_token(Some(""), Some("envtok")),
            Ok("envtok".to_owned())
        );
    }

    #[test]
    fn resolve_token_missing_both() {
        assert_eq!(resolve_token(None, None), Err(TokenError::Missing));
        assert_eq!(resolve_token(Some(""), Some("")), Err(TokenError::Missing));
    }

    #[test]
    fn token_error_message_matches_ac() {
        assert_eq!(
            TokenError::Missing.to_string(),
            "--token (or env ROBOT_TOKEN) is required"
        );
    }

    #[test]
    fn default_source_prefers_user_then_username_then_unknown() {
        assert_eq!(default_cli_source(Some("alice"), Some("bob")), "cli.alice");
        assert_eq!(default_cli_source(None, Some("bob")), "cli.bob");
        assert_eq!(default_cli_source(Some(""), Some("bob")), "cli.bob");
        assert_eq!(default_cli_source(None, None), "cli.unknown");
        assert_eq!(default_cli_source(Some(""), Some("")), "cli.unknown");
    }

    #[test]
    fn idempotency_auto_reuses_request_id() {
        let meta = build_command_meta(&inputs(None));
        assert_eq!(meta.idempotency_key, format!("cli-{}", meta.request_id));
    }

    #[test]
    fn idempotency_empty_flag_falls_back_to_auto() {
        let meta = build_command_meta(&inputs(Some("")));
        assert_eq!(meta.idempotency_key, format!("cli-{}", meta.request_id));
    }

    #[test]
    fn idempotency_explicit_override() {
        let meta = build_command_meta(&inputs(Some("my-key")));
        assert_eq!(meta.idempotency_key, "my-key");
    }

    #[test]
    fn auth_token_propagates_with_empty_fingerprint() {
        let meta = build_command_meta(&inputs(None));
        let auth = meta.auth.expect("auth set");
        assert_eq!(auth.token, "tok");
        assert!(
            auth.mtls_fingerprint.is_empty(),
            "expected empty fingerprint, got {:?}",
            auth.mtls_fingerprint
        );
    }

    #[test]
    fn request_id_is_v7() {
        let meta = build_command_meta(&inputs(None));
        let parsed = uuid::Uuid::parse_str(&meta.request_id).expect("parse uuid");
        assert_eq!(parsed.get_version_num(), 7);
    }

    #[test]
    fn dry_run_and_audit_reason_propagate() {
        let meta = build_command_meta(&MetaInputs {
            token: "t",
            source: "s",
            idempotency_key: None,
            timeout_ms: 1500,
            dry_run: true,
            audit_reason: "maintenance window",
        });
        assert!(meta.dry_run);
        assert_eq!(meta.audit_reason, "maintenance window");
        assert_eq!(meta.timeout_ms, 1500);
    }
}
