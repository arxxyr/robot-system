//! AuthContext 解析 + role → capability 查表（A1-1 (3/3) 范围）。
//!
//! ## 安全边界（reviewer A1-1 拍板硬边界）
//!
//! - **wire `role` 字段（[`robot_msg::msg::RpcRole`]）仅 audit**：daemon 端
//!   **不**做授权决策。授权决策**全部**在本 module 完成。
//! - **viewer / operator 调 `clear_estop` 必须 `PermissionDenied`**；
//!   maintainer / admin 通过。任何放宽都视为 §6.7 #3 "EStop 不可清为失忆
//!   态" 阻断。
//! - **fake token 接受**（v0.3 A1）：gateway 用 [`load_role_table`] 加载的
//!   `tokens` 表查 token → role。mTLS / OIDC 推 v0.5+；本 module **不**实
//!   现 token 加密 / 签名校验。
//! - **request_id 必须 UUIDv7**：[`validate_request_id`] 拒绝空串 / 非 v7
//!   格式（防客户端用 v4 让 audit trace 时序错乱）。

use serde::Deserialize;
use std::collections::HashMap;
use thiserror::Error;
use uuid::Uuid;

/// 4 roles。数值不映射到 wire；wire 走 [`robot_msg::msg::RpcRole`] 单独
/// audit 字段。本 enum 是 robot-rpc 内部授权决策类型。
///
/// 数值升序 = 权限升级（跟 wire `RpcRole` 顺序对齐方便审计 cross-check，
/// 但**禁止** `as u8` cast 跨类型）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    Viewer,
    Operator,
    Maintainer,
    Admin,
}

impl Role {
    /// docs/00_design.md §5.4 角色名（snake_case）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Operator => "operator",
            Self::Maintainer => "maintainer",
            Self::Admin => "admin",
        }
    }

    /// 反向解析（configs/rpc_roles.toml 加载用）。
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "viewer" => Some(Self::Viewer),
            "operator" => Some(Self::Operator),
            "maintainer" => Some(Self::Maintainer),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }
}

/// 9 capabilities（docs/00_design.md §5.4）。
///
/// **A1-1 (3/3) scope**：robot-rpc 真正用的只有 [`Capability::ClearEStop`]；
/// 其它 capability 占位枚举（让 v0.5+ Phase B B3 完整 gateway 落地时不
/// 用改 enum）。
///
/// `as_str` 跟 configs/rpc_roles.toml capability 字串严格对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    ReadState,
    InvokeTask,
    SetEnabled,
    ClearError,
    ClearEStop,
    EStop,
    ReloadCalibration,
    SetPolicy,
    Shutdown,
}

impl Capability {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadState => "read_state",
            Self::InvokeTask => "invoke_task",
            Self::SetEnabled => "set_enabled",
            Self::ClearError => "clear_error",
            Self::ClearEStop => "clear_estop",
            Self::EStop => "estop",
            Self::ReloadCalibration => "reload_calibration",
            Self::SetPolicy => "set_policy",
            Self::Shutdown => "shutdown",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "read_state" => Some(Self::ReadState),
            "invoke_task" => Some(Self::InvokeTask),
            "set_enabled" => Some(Self::SetEnabled),
            "clear_error" => Some(Self::ClearError),
            "clear_estop" => Some(Self::ClearEStop),
            "estop" => Some(Self::EStop),
            "reload_calibration" => Some(Self::ReloadCalibration),
            "set_policy" => Some(Self::SetPolicy),
            "shutdown" => Some(Self::Shutdown),
            _ => None,
        }
    }
}

/// 加载完的 role 表（fake token → role + role → capabilities）。
#[derive(Debug, Clone, Default)]
pub struct RoleTable {
    tokens: HashMap<String, Role>,
    roles: HashMap<Role, Vec<Capability>>,
}

impl RoleTable {
    /// 查 fake token 对应的 role。
    #[must_use]
    pub fn lookup_token(&self, token: &str) -> Option<Role> {
        self.tokens.get(token).copied()
    }

    /// 检查给定 role 是否拥有指定 capability。
    #[must_use]
    pub fn role_has_capability(&self, role: Role, cap: Capability) -> bool {
        self.roles
            .get(&role)
            .is_some_and(|caps| caps.contains(&cap))
    }

    /// 拒绝授权决策的硬不变量：**`Capability::ClearEStop` 必须只属于
    /// maintainer 或 admin**（reviewer A1-1 拍板硬边界）。任何 role 表配
    /// 置违反此不变量都视为 §6.7 #3 阻断。
    ///
    /// `load_role_table` 加载完即调用本检查；测试可单独调验证 toml。
    ///
    /// # Errors
    ///
    /// `AuthError::ClearEstopGrantedToLowRole`：viewer / operator 拿到
    /// `clear_estop` capability。
    pub fn check_clear_estop_invariant(&self) -> Result<(), AuthError> {
        for (role, caps) in &self.roles {
            if caps.contains(&Capability::ClearEStop)
                && !matches!(role, Role::Maintainer | Role::Admin)
            {
                return Err(AuthError::ClearEstopGrantedToLowRole(*role));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("rpc_roles.toml 解析失败：{0}")]
    Toml(#[from] toml::de::Error),
    #[error("rpc_roles.toml 含未知 role 名：{0:?}")]
    UnknownRole(String),
    #[error("rpc_roles.toml 含未知 capability：{role:?} -> {capability:?}")]
    UnknownCapability { role: String, capability: String },
    #[error(
        "role 表违反硬不变量：{0:?} 不应拥有 `clear_estop` capability \
         （reviewer A1-1 拍板：仅 maintainer / admin 可调 ClearEStop）"
    )]
    ClearEstopGrantedToLowRole(Role),
    #[error("rpc_roles.toml 没有任何 role 定义")]
    EmptyRoleTable,
}

/// configs/rpc_roles.toml 的 serde 反序列化中间表示。
#[derive(Debug, Deserialize)]
struct RoleTableToml {
    #[serde(default)]
    tokens: HashMap<String, String>,
    #[serde(default)]
    roles: HashMap<String, RoleEntryToml>,
}

#[derive(Debug, Deserialize)]
struct RoleEntryToml {
    #[serde(default)]
    capabilities: Vec<String>,
    #[allow(dead_code)] // 仅文档用，本 module 不读。
    #[serde(default)]
    description: String,
}

/// 从 TOML 字符串加载 role 表 + 自检 ClearEStop 硬不变量。
///
/// # Errors
///
/// 见 [`AuthError`]。
pub fn load_role_table(toml_text: &str) -> Result<RoleTable, AuthError> {
    let raw: RoleTableToml = toml::from_str(toml_text)?;
    if raw.roles.is_empty() {
        return Err(AuthError::EmptyRoleTable);
    }

    let mut tokens = HashMap::with_capacity(raw.tokens.len());
    for (token, role_str) in raw.tokens {
        let role = Role::from_str(&role_str).ok_or(AuthError::UnknownRole(role_str.clone()))?;
        tokens.insert(token, role);
    }

    let mut roles = HashMap::with_capacity(raw.roles.len());
    for (role_str, entry) in raw.roles {
        let role = Role::from_str(&role_str).ok_or(AuthError::UnknownRole(role_str.clone()))?;
        let mut caps: Vec<Capability> = Vec::with_capacity(entry.capabilities.len());
        for cap_str in entry.capabilities {
            let cap = Capability::from_str(&cap_str).ok_or(AuthError::UnknownCapability {
                role: role_str.clone(),
                capability: cap_str.clone(),
            })?;
            caps.push(cap);
        }
        roles.insert(role, caps);
    }

    let table = RoleTable { tokens, roles };
    table.check_clear_estop_invariant()?;
    Ok(table)
}

// ---------------------------------------------------------------------------
// request_id 校验
// ---------------------------------------------------------------------------

/// `CommandMeta.request_id` 必须是 UUIDv7。空串 / 非 v7 / 非 UUID 都拒。
///
/// **为什么强制 v7**：v7 是时间排序的 UUID（`unix_ts_ms` 占头 48 bit），
/// audit trace 跨 request 直接按字典序就是时序，方便 grep journal /
/// recorder。v4 是随机的，audit 难按时序复盘。
///
/// # Errors
///
/// `RequestIdError::Empty` / `Malformed` / `WrongVersion`。
pub fn validate_request_id(s: &str) -> Result<Uuid, RequestIdError> {
    if s.is_empty() {
        return Err(RequestIdError::Empty);
    }
    let uuid = Uuid::parse_str(s).map_err(|_| RequestIdError::Malformed(s.to_string()))?;
    if uuid.get_version_num() != 7 {
        return Err(RequestIdError::WrongVersion {
            got: uuid.get_version_num(),
        });
    }
    Ok(uuid)
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RequestIdError {
    #[error("request_id 为空字符串（必须是 UUIDv7）")]
    Empty,
    #[error("request_id 不是合法 UUID：{0:?}")]
    Malformed(String),
    #[error("request_id 必须是 UUIDv7（time-ordered），实际版本：{got}")]
    WrongVersion { got: usize },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_TOML: &str = r#"
[tokens]
"viewer-tok"     = "viewer"
"operator-tok"   = "operator"
"maintainer-tok" = "maintainer"
"admin-tok"      = "admin"

[roles.viewer]
capabilities = ["read_state"]

[roles.operator]
# B4 (2/3-b) 收紧：operator 删 clear_error；ClearError 改 maintainer+。
capabilities = ["read_state", "invoke_task", "set_enabled", "estop"]

[roles.maintainer]
# B4 (2/3-b)：maintainer / admin 保留 clear_error（错误恢复 = maintainer+）；
# invoke_task 跟 production rpc_roles.toml 对齐（maintainer 也能提交任务）。
capabilities = ["read_state", "invoke_task", "clear_estop", "clear_error", "reload_calibration"]

[roles.admin]
capabilities = ["read_state", "invoke_task", "clear_estop", "clear_error", "set_policy", "shutdown"]
"#;

    #[test]
    fn role_str_roundtrip() {
        for r in [Role::Viewer, Role::Operator, Role::Maintainer, Role::Admin] {
            assert_eq!(Role::from_str(r.as_str()), Some(r));
        }
        assert_eq!(Role::from_str(""), None);
        assert_eq!(Role::from_str("ADMIN"), None);
        assert_eq!(Role::from_str("super_admin"), None);
    }

    #[test]
    fn capability_str_roundtrip() {
        for c in [
            Capability::ReadState,
            Capability::InvokeTask,
            Capability::SetEnabled,
            Capability::ClearError,
            Capability::ClearEStop,
            Capability::EStop,
            Capability::ReloadCalibration,
            Capability::SetPolicy,
            Capability::Shutdown,
        ] {
            assert_eq!(Capability::from_str(c.as_str()), Some(c));
        }
        assert_eq!(Capability::from_str(""), None);
        assert_eq!(Capability::from_str("CLEAR_ESTOP"), None);
    }

    #[test]
    fn role_ord_ascends_with_privilege() {
        assert!(Role::Viewer < Role::Operator);
        assert!(Role::Operator < Role::Maintainer);
        assert!(Role::Maintainer < Role::Admin);
    }

    #[test]
    fn load_role_table_happy_path() {
        let t = load_role_table(VALID_TOML).unwrap();
        assert_eq!(t.lookup_token("viewer-tok"), Some(Role::Viewer));
        assert_eq!(t.lookup_token("admin-tok"), Some(Role::Admin));
        assert_eq!(t.lookup_token("nonexistent-token"), None);
    }

    #[test]
    fn maintainer_has_clear_estop() {
        let t = load_role_table(VALID_TOML).unwrap();
        assert!(t.role_has_capability(Role::Maintainer, Capability::ClearEStop));
    }

    #[test]
    fn admin_has_clear_estop() {
        let t = load_role_table(VALID_TOML).unwrap();
        assert!(t.role_has_capability(Role::Admin, Capability::ClearEStop));
    }

    #[test]
    fn viewer_does_not_have_clear_estop() {
        let t = load_role_table(VALID_TOML).unwrap();
        assert!(!t.role_has_capability(Role::Viewer, Capability::ClearEStop));
    }

    #[test]
    fn operator_does_not_have_clear_estop() {
        let t = load_role_table(VALID_TOML).unwrap();
        assert!(!t.role_has_capability(Role::Operator, Capability::ClearEStop));
    }

    /// B4 (2/3-b) 收紧锁测：operator 不再拥有 `clear_error`（outline §8.1
    /// #7c）。viewer 同样不拥有；maintainer / admin 保留。
    ///
    /// 跟 `clear_estop` 分级语义不同：`clear_estop` 清磁盘 latch
    /// (safety-critical)，**不可** operator；`clear_error` 仅清内存
    /// Degraded 态（不碰 §6.7 invariant），但同款收紧 = `(maintainer+)`，
    /// 让"错误恢复"权限上限齐到 maintainer。
    #[test]
    fn b4_clear_error_tightened_to_maintainer_plus() {
        let t = load_role_table(VALID_TOML).unwrap();
        assert!(
            !t.role_has_capability(Role::Viewer, Capability::ClearError),
            "viewer 不应拥有 clear_error"
        );
        assert!(
            !t.role_has_capability(Role::Operator, Capability::ClearError),
            "B4 (2/3-b) 收紧（outline §8.1 #7c）：operator 不再拥有 clear_error"
        );
        assert!(
            t.role_has_capability(Role::Maintainer, Capability::ClearError),
            "maintainer 必须保留 clear_error（错误恢复入口）"
        );
        assert!(
            t.role_has_capability(Role::Admin, Capability::ClearError),
            "admin 必须保留 clear_error"
        );
    }

    /// B4 (2/3-b) capability 复用锁测：4 个 task control plane RPC 均
    /// 复用现有 capability，**不**扩 enum（outline §8.1 #7a 拍板 (a)）。
    ///
    /// - `InvokeTask` / `CancelTask` → `Capability::InvokeTask` (operator+)
    /// - `GetTaskStatus` / `StreamTaskEvents` → `Capability::ReadState` (viewer+)
    /// - `ClearError` → `Capability::ClearError` (maintainer+；上一条锁测)
    #[test]
    fn b4_task_rpc_capabilities_reuse_existing_enum() {
        let t = load_role_table(VALID_TOML).unwrap();

        // InvokeTask / CancelTask = operator+ (复用 InvokeTask capability)
        assert!(
            !t.role_has_capability(Role::Viewer, Capability::InvokeTask),
            "viewer 不能 InvokeTask"
        );
        for r in [Role::Operator, Role::Maintainer, Role::Admin] {
            assert!(
                t.role_has_capability(r, Capability::InvokeTask),
                "{r:?} 必须能 InvokeTask"
            );
        }

        // GetTaskStatus / StreamTaskEvents = viewer+ (复用 ReadState capability)
        for r in [Role::Viewer, Role::Operator, Role::Maintainer, Role::Admin] {
            assert!(
                t.role_has_capability(r, Capability::ReadState),
                "{r:?} 必须能 ReadState（含 GetTaskStatus / StreamTaskEvents）"
            );
        }
    }

    /// 硬不变量回归锁：viewer 拿到 clear_estop → load 必须拒绝。
    #[test]
    fn check_clear_estop_invariant_rejects_viewer() {
        let bad_toml = r#"
[roles.viewer]
capabilities = ["read_state", "clear_estop"]
[roles.maintainer]
capabilities = ["clear_estop"]
"#;
        let err = load_role_table(bad_toml).unwrap_err();
        assert!(matches!(
            err,
            AuthError::ClearEstopGrantedToLowRole(Role::Viewer)
        ));
    }

    /// 硬不变量回归锁：operator 拿到 clear_estop → load 必须拒绝。
    #[test]
    fn check_clear_estop_invariant_rejects_operator() {
        let bad_toml = r#"
[roles.operator]
capabilities = ["clear_estop"]
[roles.maintainer]
capabilities = ["clear_estop"]
"#;
        let err = load_role_table(bad_toml).unwrap_err();
        assert!(matches!(
            err,
            AuthError::ClearEstopGrantedToLowRole(Role::Operator)
        ));
    }

    #[test]
    fn unknown_role_rejected() {
        let bad = r#"
[roles.super_admin]
capabilities = ["clear_estop"]
"#;
        let err = load_role_table(bad).unwrap_err();
        assert!(matches!(err, AuthError::UnknownRole(s) if s == "super_admin"));
    }

    #[test]
    fn unknown_capability_rejected() {
        let bad = r#"
[roles.maintainer]
capabilities = ["delete_universe"]
"#;
        let err = load_role_table(bad).unwrap_err();
        match err {
            AuthError::UnknownCapability { role, capability } => {
                assert_eq!(role, "maintainer");
                assert_eq!(capability, "delete_universe");
            }
            other => panic!("expected UnknownCapability, got {other:?}"),
        }
    }

    #[test]
    fn empty_role_table_rejected() {
        let bad = r#"[tokens]
"foo" = "viewer"
"#;
        let err = load_role_table(bad).unwrap_err();
        assert!(matches!(err, AuthError::EmptyRoleTable));
    }

    /// **回归锁**：仓库的 `configs/rpc_roles.toml` 必须 load 成功 +
    /// 通过 ClearEStop 硬不变量校验。任何 PR 改这个 toml 让 viewer /
    /// operator 拿到 clear_estop 都会让本测试失败。
    #[test]
    fn repo_rpc_roles_toml_passes_invariants() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs/rpc_roles.toml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let table = load_role_table(&text).unwrap_or_else(|e| {
            panic!("configs/rpc_roles.toml load failed: {e}");
        });
        assert!(table.role_has_capability(Role::Maintainer, Capability::ClearEStop));
        assert!(table.role_has_capability(Role::Admin, Capability::ClearEStop));
        assert!(!table.role_has_capability(Role::Viewer, Capability::ClearEStop));
        assert!(!table.role_has_capability(Role::Operator, Capability::ClearEStop));
    }

    // ---- request_id (UUIDv7) validation ----

    #[test]
    fn validate_request_id_accepts_v7() {
        let v7 = Uuid::now_v7();
        let s = v7.to_string();
        let parsed = validate_request_id(&s).unwrap();
        assert_eq!(parsed, v7);
    }

    #[test]
    fn validate_request_id_rejects_empty() {
        assert_eq!(validate_request_id(""), Err(RequestIdError::Empty));
    }

    #[test]
    fn validate_request_id_rejects_malformed() {
        let err = validate_request_id("not-a-uuid").unwrap_err();
        assert!(matches!(err, RequestIdError::Malformed(s) if s == "not-a-uuid"));
    }

    #[test]
    fn validate_request_id_rejects_v4() {
        // hardcoded v4 UUID
        let v4 = "550e8400-e29b-41d4-a716-446655440000";
        let err = validate_request_id(v4).unwrap_err();
        assert!(matches!(err, RequestIdError::WrongVersion { got: 4 }));
    }
}
