//! ClearEStop validation + `RpcStatus` → `tonic::Code` 显式映射（A1-1
//! (3/3) 范围）。
//!
//! ## 安全边界（reviewer A1 + A1-1 part 1 拍板硬边界）
//!
//! - **`confirmation` 必填**：空串拒，返 `tonic::Code::InvalidArgument`。
//!   具体格式约束（如必须含 request_id 后缀 / 操作员姓名）v0.3 实现时定，
//!   本 module **不**强制格式（[`validate_clear_estop_command`] 仅检查非
//!   空 + 非 whitespace-only）。
//! - **dry_run 解析**：本 module 仅**解析** `meta.dry_run` flag；**不**短
//!   路返回 Ok（reviewer A1 第 2 个拍板：dry_run 必须进 S0 端 action layer
//!   实施，gateway **不**自己评估）。`validate_clear_estop_command` 输出
//!   含 `dry_run: bool`，A1-2 接 RR client 时透传给 daemon。
//! - **`RpcStatus` → `tonic::Code` 必须显式 `match`**：reviewer A1-1 part
//!   1 拍板硬边界，**禁止** `code as i32` cast；新增 `RpcStatus` variant
//!   时编译期 `match` 非穷举会报错强制更新映射 + 测试。

use robot_msg::msg::RpcStatus;
use thiserror::Error;
use tonic::Code;

use crate::auth::{RequestIdError, validate_request_id};

/// 验证 [`crate::proto::ClearEStopCommand`] 通过的 RPC 入参。
///
/// **A1-1 (3/3) scope**：本 struct 仅是验证结果的容器；A1-3 真起 server
/// 时 trait impl 把 [`crate::proto::ClearEStopCommand`] 喂进
/// [`validate_clear_estop_command`]，拿这里的 struct 转
/// `robot-msg::ClearEStopRequest` wire bytes 通过 RR client publish。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedClearEStop {
    pub request_id: String,
    pub idempotency_key: String,
    pub source: String,
    pub confirmation: String,
    pub audit_reason: String,
    pub dry_run: bool,
    pub timeout_ms: u64,
    pub auth_token: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("missing CommandMeta")]
    MissingMeta,
    #[error("request_id invalid: {0}")]
    RequestId(#[from] RequestIdError),
    #[error("idempotency_key 为空字符串（必填）")]
    EmptyIdempotencyKey,
    #[error("source 为空字符串（必填）")]
    EmptySource,
    #[error("confirmation 为空字符串（必填，design §5.2 ClearEStopCommand.confirmation）")]
    EmptyConfirmation,
    #[error("confirmation 仅含空白字符（视同空）")]
    WhitespaceOnlyConfirmation,
    #[error("auth.token 为空（v0.3 A1 接受 fake token 但必须非空）")]
    EmptyAuthToken,
    #[error("timeout_ms = 0（必须 > 0；客户端 monotonic 相对超时）")]
    ZeroTimeout,
}

impl ValidationError {
    /// 把 validation 错误转成 `tonic::Code`。**不走** `as i32` cast。
    #[must_use]
    pub fn to_tonic_code(&self) -> Code {
        // 全部 validation 错误都映射到 InvalidArgument；语义就是"客户端入
        // 参不对"。
        match self {
            Self::MissingMeta
            | Self::RequestId(_)
            | Self::EmptyIdempotencyKey
            | Self::EmptySource
            | Self::EmptyConfirmation
            | Self::WhitespaceOnlyConfirmation
            | Self::EmptyAuthToken
            | Self::ZeroTimeout => Code::InvalidArgument,
        }
    }
}

/// 验证 [`crate::proto::ClearEStopCommand`] 入参 + 抽出 [`ValidatedClearEStop`]。
///
/// **不**做的事（reviewer 硬边界）：
/// - **不**做授权 / role 查表（那是 `auth::RoleTable` 的事，调用方 wrap）。
/// - **不**短路返回 dry_run（仅解析 flag；A1-2 接 daemon 时由 daemon 端
///   action layer 决定真路径还是评估路径）。
/// - **不**做 idempotency 查 cache（那是 `idempotency::IdempotencyCache`
///   的事，调用方 wrap）。
///
/// # Errors
///
/// 见 [`ValidationError`]。
pub fn validate_clear_estop_command(
    cmd: &crate::proto::ClearEStopCommand,
) -> Result<ValidatedClearEStop, ValidationError> {
    let meta = cmd.meta.as_ref().ok_or(ValidationError::MissingMeta)?;
    let auth = meta.auth.as_ref().ok_or(ValidationError::EmptyAuthToken)?;

    if auth.token.is_empty() {
        return Err(ValidationError::EmptyAuthToken);
    }
    let _request_uuid = validate_request_id(&meta.request_id)?;
    if meta.idempotency_key.is_empty() {
        return Err(ValidationError::EmptyIdempotencyKey);
    }
    if meta.source.is_empty() {
        return Err(ValidationError::EmptySource);
    }
    if meta.timeout_ms == 0 {
        return Err(ValidationError::ZeroTimeout);
    }
    if cmd.confirmation.is_empty() {
        return Err(ValidationError::EmptyConfirmation);
    }
    if cmd.confirmation.chars().all(char::is_whitespace) {
        return Err(ValidationError::WhitespaceOnlyConfirmation);
    }

    Ok(ValidatedClearEStop {
        request_id: meta.request_id.clone(),
        idempotency_key: meta.idempotency_key.clone(),
        source: meta.source.clone(),
        confirmation: cmd.confirmation.clone(),
        audit_reason: meta.audit_reason.clone(),
        dry_run: meta.dry_run,
        timeout_ms: meta.timeout_ms,
        auth_token: auth.token.clone(),
    })
}

// ---------------------------------------------------------------------------
// Idempotency fingerprint（reviewer A1-1 (3/3) round 2 阻断 1 修正）
// ---------------------------------------------------------------------------

/// 算 [`ValidatedClearEStop`] 的 idempotency fingerprint（32-byte blake3
/// hash）；**排除** `request_id` / `timeout_ms` / `idempotency_key`，保证
/// 同一 idempotency_key 的合法 retry（每次 retry 客户端生成新 UUIDv7
/// request_id + 可能调整 timeout）走 [`crate::idempotency::Decision::Replay`]
/// 而**不**误走 `Conflict`。
///
/// ## 字段分类
///
/// **进 fingerprint**（语义字段；retry 必须不变，否则视同新意图）：
/// - `source`（"cli.loosqk" / "web.ui" / 等；同一来源 retry）
/// - `confirmation`（人工确认串；改字串 = 不同操作员意图）
/// - `audit_reason`（"为什么调"；改 reason = 不同 audit 上下文）
/// - `dry_run`（dry_run 翻转 = 行为彻底改变）
/// - `auth_token`（攻击者切换 token 应被检测）
///
/// **不进 fingerprint**（audit-only / retry 可变）：
/// - `request_id`（reviewer A1-1 (3/3) round 2 明文：每次 retry 改变；audit
///   trace correlation 用，不进幂等键）
/// - `timeout_ms`（客户端 retry 调整 timeout 是合理的）
/// - `idempotency_key`（cache key 自身；不应该是 fingerprint 输入）
///
/// ## Wire format
///
/// length-prefixed (label, value) 序对，blake3-256 hash。length-prefix 防
/// "ab"+"c" vs "a"+"bc" hash 相同的 ambiguity。`label` 字符串硬编码在源
/// 码，避免外部输入污染 hash 输入。
#[must_use]
pub fn clear_estop_idempotency_fingerprint(v: &ValidatedClearEStop) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    fingerprint_field(&mut h, "source", v.source.as_bytes());
    fingerprint_field(&mut h, "confirmation", v.confirmation.as_bytes());
    fingerprint_field(&mut h, "audit_reason", v.audit_reason.as_bytes());
    fingerprint_field(&mut h, "dry_run", &[u8::from(v.dry_run)]);
    fingerprint_field(&mut h, "auth_token", v.auth_token.as_bytes());
    h.finalize().into()
}

fn fingerprint_field(h: &mut blake3::Hasher, label: &str, value: &[u8]) {
    let label_bytes = label.as_bytes();
    // u32 大端长度前缀；硬编码 label 长度永远不会超 u32::MAX。
    h.update(
        &u32::try_from(label_bytes.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    h.update(label_bytes);
    h.update(&u32::try_from(value.len()).unwrap_or(u32::MAX).to_be_bytes());
    h.update(value);
}

// ---------------------------------------------------------------------------
// RpcStatus → tonic::Code 显式映射
// ---------------------------------------------------------------------------

/// 把 [`robot_msg::msg::RpcStatus`]（A1-1 part 1 wire enum）转 tonic
/// `Code`。
///
/// **必须**用显式 `match` 全枚举（reviewer A1-1 part 1 拍板硬边界）。原因：
/// `RpcStatus` 数值（A1-1 part 1 fbs 注释明文）跟 gRPC numeric code **不**
/// 对齐；任何 `code as i32` cast 都会把错误映射错。
///
/// **flatc 生成的 enum 结构**：`RpcStatus` 是 `pub struct RpcStatus(pub u8)`
/// 不是真 Rust enum；`match` 的非穷举无法在编译期 catch 新 variant。本
/// helper 走两道防线：
/// 1. **数值 wildcard 兜底**：未知 u8 值（旧 reader 收到新 writer 写的
///    新 variant，或 wire 损坏）→ `Code::Internal`。
/// 2. **测试期变量计数锁**：`rpc_status_variant_count_matches_explicit_mapping`
///    单测把 [`RpcStatus::ENUM_VALUES`] 长度跟本 helper 显式映射数硬绑；
///    新增 variant 会让 length 对不上 → 测试失败 → 强制同步本 helper +
///    单测。
#[must_use]
// 显式列每个 RpcStatus variant，**禁止** `as i32` cast。Unknown / Internal
// 都映射到 Code::Internal（语义来源不同：前者兼容旧 writer，后者显式内部
// 错误），但 tonic 端表现一致；clippy `match_same_arms` 在此 allow 而**不**
// 合并 arm，让两个语义来源在源码 + 注释里保持显式可读。
#[allow(clippy::match_same_arms)]
pub fn rpc_status_to_tonic_code(status: RpcStatus) -> Code {
    match status {
        // Unknown 是 wire 兼容旧 writer 用；gateway 视同内部错误（不应
        // 出现在 daemon 真返的 wire 上；出现 = wire layer 没对齐 schema）。
        RpcStatus::Unknown => Code::Internal,
        RpcStatus::Ok => Code::Ok,
        RpcStatus::FailedPrecondition => Code::FailedPrecondition,
        RpcStatus::PermissionDenied => Code::PermissionDenied,
        RpcStatus::InvalidArgument => Code::InvalidArgument,
        // 显式 Internal 错误（daemon 端 fsync 失败 / 内部 panic 等）。
        RpcStatus::Internal => Code::Internal,
        RpcStatus::DeadlineExceeded => Code::DeadlineExceeded,
        // wildcard：flatc enum 是 newtype，理论 u8 0..=255 都合法。未知值
        // = wire 协议 mismatch / 旧 reader + 新 writer 的过渡期；视同内部
        // 错误。**注意**：新增 RpcStatus variant 时**不**该走到这里，
        // `rpc_status_variant_count_matches_explicit_mapping` 单测会先报错
        // 强制更新本 match。
        _ => Code::Internal,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{AuthContext, ClearEStopCommand, CommandMeta};
    use uuid::Uuid;

    fn fresh_cmd() -> ClearEStopCommand {
        ClearEStopCommand {
            meta: Some(CommandMeta {
                request_id: Uuid::now_v7().to_string(),
                source: "test".into(),
                auth: Some(AuthContext {
                    token: "maintainer-fake-token-001".into(),
                    mtls_fingerprint: String::new(),
                }),
                timeout_ms: 5_000,
                idempotency_key: "test-idem-001".into(),
                dry_run: false,
                audit_reason: "unit test".into(),
            }),
            confirmation: "operator-loosqk-confirm-001".into(),
        }
    }

    #[test]
    fn validates_happy_path() {
        let cmd = fresh_cmd();
        let v = validate_clear_estop_command(&cmd).unwrap();
        assert_eq!(v.idempotency_key, "test-idem-001");
        assert_eq!(v.source, "test");
        assert!(!v.dry_run);
        assert_eq!(v.timeout_ms, 5_000);
        assert_eq!(v.confirmation, "operator-loosqk-confirm-001");
        assert_eq!(v.auth_token, "maintainer-fake-token-001");
    }

    #[test]
    fn rejects_missing_meta() {
        let cmd = ClearEStopCommand {
            meta: None,
            confirmation: "x".into(),
        };
        let err = validate_clear_estop_command(&cmd).unwrap_err();
        assert_eq!(err, ValidationError::MissingMeta);
        assert_eq!(err.to_tonic_code(), Code::InvalidArgument);
    }

    #[test]
    fn rejects_empty_confirmation() {
        let mut cmd = fresh_cmd();
        cmd.confirmation = String::new();
        let err = validate_clear_estop_command(&cmd).unwrap_err();
        assert_eq!(err, ValidationError::EmptyConfirmation);
        assert_eq!(err.to_tonic_code(), Code::InvalidArgument);
    }

    #[test]
    fn rejects_whitespace_only_confirmation() {
        let mut cmd = fresh_cmd();
        cmd.confirmation = "   \t\n  ".into();
        let err = validate_clear_estop_command(&cmd).unwrap_err();
        assert_eq!(err, ValidationError::WhitespaceOnlyConfirmation);
        assert_eq!(err.to_tonic_code(), Code::InvalidArgument);
    }

    #[test]
    fn rejects_empty_auth_token() {
        let mut cmd = fresh_cmd();
        cmd.meta.as_mut().unwrap().auth.as_mut().unwrap().token = String::new();
        let err = validate_clear_estop_command(&cmd).unwrap_err();
        assert_eq!(err, ValidationError::EmptyAuthToken);
    }

    #[test]
    fn rejects_missing_auth() {
        let mut cmd = fresh_cmd();
        cmd.meta.as_mut().unwrap().auth = None;
        let err = validate_clear_estop_command(&cmd).unwrap_err();
        assert_eq!(err, ValidationError::EmptyAuthToken);
    }

    #[test]
    fn rejects_empty_idempotency_key() {
        let mut cmd = fresh_cmd();
        cmd.meta.as_mut().unwrap().idempotency_key = String::new();
        let err = validate_clear_estop_command(&cmd).unwrap_err();
        assert_eq!(err, ValidationError::EmptyIdempotencyKey);
    }

    #[test]
    fn rejects_empty_source() {
        let mut cmd = fresh_cmd();
        cmd.meta.as_mut().unwrap().source = String::new();
        let err = validate_clear_estop_command(&cmd).unwrap_err();
        assert_eq!(err, ValidationError::EmptySource);
    }

    #[test]
    fn rejects_zero_timeout() {
        let mut cmd = fresh_cmd();
        cmd.meta.as_mut().unwrap().timeout_ms = 0;
        let err = validate_clear_estop_command(&cmd).unwrap_err();
        assert_eq!(err, ValidationError::ZeroTimeout);
    }

    #[test]
    fn rejects_v4_request_id() {
        let mut cmd = fresh_cmd();
        cmd.meta.as_mut().unwrap().request_id = "550e8400-e29b-41d4-a716-446655440000".into();
        let err = validate_clear_estop_command(&cmd).unwrap_err();
        match err {
            ValidationError::RequestId(RequestIdError::WrongVersion { got: 4 }) => {}
            other => panic!("expected RequestId WrongVersion, got {other:?}"),
        }
    }

    /// **dry_run 解析路径**：reviewer A1 第 2 个拍板硬边界 — RPC 端不短路
    /// 返，仅解析 flag 透传给 S0 端 action layer。本测试锁住 validate 把
    /// `dry_run = true` 完整透传到 ValidatedClearEStop（A1-3 真起 server
    /// 时也必须保留这条契约）。
    #[test]
    fn dry_run_flag_propagates_through_validation() {
        let mut cmd = fresh_cmd();
        cmd.meta.as_mut().unwrap().dry_run = true;
        let v = validate_clear_estop_command(&cmd).unwrap();
        assert!(
            v.dry_run,
            "dry_run must propagate untouched through validate"
        );
    }

    // ---- RpcStatus -> tonic::Code 映射（reviewer A1-1 part 1 拍板必测）----

    #[test]
    fn rpc_status_unknown_maps_to_internal() {
        assert_eq!(rpc_status_to_tonic_code(RpcStatus::Unknown), Code::Internal);
    }

    #[test]
    fn rpc_status_ok_maps_to_ok() {
        assert_eq!(rpc_status_to_tonic_code(RpcStatus::Ok), Code::Ok);
    }

    #[test]
    fn rpc_status_failed_precondition_maps_to_failed_precondition() {
        assert_eq!(
            rpc_status_to_tonic_code(RpcStatus::FailedPrecondition),
            Code::FailedPrecondition
        );
    }

    #[test]
    fn rpc_status_permission_denied_maps_to_permission_denied() {
        assert_eq!(
            rpc_status_to_tonic_code(RpcStatus::PermissionDenied),
            Code::PermissionDenied
        );
    }

    #[test]
    fn rpc_status_invalid_argument_maps_to_invalid_argument() {
        assert_eq!(
            rpc_status_to_tonic_code(RpcStatus::InvalidArgument),
            Code::InvalidArgument
        );
    }

    #[test]
    fn rpc_status_internal_maps_to_internal() {
        assert_eq!(
            rpc_status_to_tonic_code(RpcStatus::Internal),
            Code::Internal
        );
    }

    #[test]
    fn rpc_status_deadline_exceeded_maps_to_deadline_exceeded() {
        assert_eq!(
            rpc_status_to_tonic_code(RpcStatus::DeadlineExceeded),
            Code::DeadlineExceeded
        );
    }

    /// **回归锁**：禁止 `RpcStatus as i32 -> Code` 数值 cast。RpcStatus
    /// 数值（A1-1 part 1 wire enum，FailedPrecondition=2 / Internal=5）跟
    /// gRPC numeric code（FailedPrecondition=9 / Internal=13）**不**对齐。
    /// 任何走 cast 路径的实现会把映射错；本测试通过对比"按数值 cast"vs
    /// "显式 match"两个结果不相等来锁住实现。
    #[test]
    fn fails_on_numeric_cast_drift() {
        // 模拟"按数值 cast"的错误路径：RpcStatus::FailedPrecondition (=2)
        // → Code::Cancelled (=2 in tonic)。这跟正确映射 (Code::FailedPrecondition,
        // tonic numeric=9) 不等。
        let wrong = Code::from_i32(i32::from(RpcStatus::FailedPrecondition.0));
        let right = rpc_status_to_tonic_code(RpcStatus::FailedPrecondition);
        assert_ne!(
            wrong, right,
            "numeric cast must NOT match explicit mapping; if equal, RpcStatus / \
             gRPC code 数值意外对齐了，需要重新审计 fbs comment + 本回归锁"
        );
    }

    // ---- idempotency fingerprint（reviewer A1-1 (3/3) round 2 阻断 1）----

    /// 把 ValidatedClearEStop 通过 fresh_cmd 路径构造（共享 fresh_cmd
    /// 的语义字段），然后允许 mutator 改 audit-only / 重试可变字段。
    fn validated_with(mutator: impl FnOnce(&mut ClearEStopCommand)) -> ValidatedClearEStop {
        let mut cmd = fresh_cmd();
        mutator(&mut cmd);
        validate_clear_estop_command(&cmd).unwrap()
    }

    /// **回归锁**：合法 retry — 同 idempotency_key + 新 request_id + 新
    /// timeout_ms + 其它语义字段不变 → fingerprint **必须**相同（不能让
    /// cache 误走 Conflict 杀合法 retry，reviewer A1-1 (3/3) round 2 阻
    /// 断 1）。
    #[test]
    fn fingerprint_stable_across_request_id_and_timeout_changes() {
        let v1 = validated_with(|c| {
            // attempt 1：默认 fresh_cmd（UUIDv7 #1, timeout 5000）
            let _ = c;
        });
        let v2 = validated_with(|c| {
            // attempt 2：retry，新 UUIDv7 + 新 timeout
            let m = c.meta.as_mut().unwrap();
            m.request_id = Uuid::now_v7().to_string();
            m.timeout_ms = 10_000;
        });
        assert_ne!(
            v1.request_id, v2.request_id,
            "测试前提：retry 必须用新 request_id"
        );
        assert_ne!(
            v1.timeout_ms, v2.timeout_ms,
            "测试前提：retry 必须用新 timeout_ms"
        );
        assert_eq!(
            clear_estop_idempotency_fingerprint(&v1),
            clear_estop_idempotency_fingerprint(&v2),
            "REGRESSION: fingerprint 跟 request_id / timeout_ms 耦合，合法 \
             retry 会被 cache 误走 Conflict（reviewer A1-1 (3/3) round 2 \
             阻断 1）"
        );
    }

    #[test]
    fn fingerprint_changes_when_confirmation_changes() {
        let v1 = validated_with(|_| {});
        let v2 = validated_with(|c| {
            c.confirmation = "different-operator-different-confirmation".into();
        });
        assert_ne!(
            clear_estop_idempotency_fingerprint(&v1),
            clear_estop_idempotency_fingerprint(&v2),
            "confirmation 是语义字段，改它应当让 fingerprint 不同 → 同 \
             idempotency_key + 不同 confirmation 走 Conflict"
        );
    }

    #[test]
    fn fingerprint_changes_when_dry_run_flips() {
        let v1 = validated_with(|_| {});
        let v2 = validated_with(|c| {
            c.meta.as_mut().unwrap().dry_run = true;
        });
        assert_ne!(
            clear_estop_idempotency_fingerprint(&v1),
            clear_estop_idempotency_fingerprint(&v2),
            "dry_run 翻转 = 行为彻底改变；同 idempotency_key + 不同 dry_run \
             必须走 Conflict（防 client 用 dry_run cache 替真清 latch 路径）"
        );
    }

    #[test]
    fn fingerprint_changes_when_auth_token_changes() {
        let v1 = validated_with(|_| {});
        let v2 = validated_with(|c| {
            c.meta.as_mut().unwrap().auth.as_mut().unwrap().token = "admin-fake-token-001".into();
        });
        assert_ne!(
            clear_estop_idempotency_fingerprint(&v1),
            clear_estop_idempotency_fingerprint(&v2),
            "auth_token 切换 = 不同身份调用；必须走 Conflict 防止 token 切换 \
             复用其它身份的 cached ack"
        );
    }

    #[test]
    fn fingerprint_changes_when_source_changes() {
        let v1 = validated_with(|_| {});
        let v2 = validated_with(|c| {
            c.meta.as_mut().unwrap().source = "web.ui".into();
        });
        assert_ne!(
            clear_estop_idempotency_fingerprint(&v1),
            clear_estop_idempotency_fingerprint(&v2),
            "source 切换 = 不同入口；audit 视角同 idempotency_key 跨 source \
             复用 cached ack 会丢 audit 轨迹，必须走 Conflict"
        );
    }

    #[test]
    fn fingerprint_changes_when_audit_reason_changes() {
        let v1 = validated_with(|_| {});
        let v2 = validated_with(|c| {
            c.meta.as_mut().unwrap().audit_reason = "different reason".into();
        });
        assert_ne!(
            clear_estop_idempotency_fingerprint(&v1),
            clear_estop_idempotency_fingerprint(&v2),
            "audit_reason 改变 = 不同 audit 上下文意图；必须走 Conflict"
        );
    }

    /// length-prefix 防 ambiguity 锁：不带 length-prefix 的 naive 拼接
    /// 会让 ("ab", "c") 跟 ("a", "bc") 同 hash。本测试用不同 audit_reason
    /// + source 长度组合验证 fingerprint 不会假等。
    #[test]
    fn fingerprint_length_prefix_prevents_ambiguity() {
        let v1 = validated_with(|c| {
            let m = c.meta.as_mut().unwrap();
            m.source = "ab".into();
            m.audit_reason = "c".into();
        });
        let v2 = validated_with(|c| {
            let m = c.meta.as_mut().unwrap();
            m.source = "a".into();
            m.audit_reason = "bc".into();
        });
        assert_ne!(
            clear_estop_idempotency_fingerprint(&v1),
            clear_estop_idempotency_fingerprint(&v2),
            "length-prefix 失效 → naive 拼接的 ('ab','c') 跟 ('a','bc') 会假 \
             等。fingerprint helper 必须 length-prefix 各字段"
        );
    }

    // ---- RpcStatus -> tonic::Code 映射继续 ----

    /// **变体数计数锁**（reviewer A1-1 part 1 拍板硬边界 round 2）：flatc
    /// 把 RpcStatus 生成成 `pub struct RpcStatus(pub u8)`，`match` 非穷举
    /// 不会编译期报错。本测试把 wire variant 数（`RpcStatus::ENUM_VALUES`
    /// 长度）跟本 module 显式映射的 variant 数硬绑：新增 RpcStatus variant
    /// 必须同步：
    ///
    /// 1. 在 [`rpc_status_to_tonic_code`] match 表加 arm（不能依赖 wildcard
    ///    `_ => Code::Internal` 兜底）。
    /// 2. 在本 module 加单测验证新 variant 映射。
    /// 3. 把本测试的 `EXPECTED_VARIANTS` 改成新数字。
    ///
    /// 任何漏第 3 步 → 测试失败强制 reviewer 关注。
    #[test]
    fn rpc_status_variant_count_matches_explicit_mapping() {
        const EXPECTED_VARIANTS: usize = 7; // Unknown / Ok / FailedPrecondition /
        // PermissionDenied / InvalidArgument / Internal / DeadlineExceeded
        let actual = RpcStatus::ENUM_VALUES.len();
        assert_eq!(
            actual, EXPECTED_VARIANTS,
            "RpcStatus 增删 variant 必须同步更新 rpc_status_to_tonic_code \
             match 表 + 各单测 + 本计数锁。旧 EXPECTED_VARIANTS={EXPECTED_VARIANTS}, \
             新 ENUM_VALUES.len()={actual}",
        );
        // 顺手验：每个已知 variant 真的能映射出某个 tonic Code（不 panic）。
        for v in RpcStatus::ENUM_VALUES {
            let _ = rpc_status_to_tonic_code(*v);
        }
    }
}
