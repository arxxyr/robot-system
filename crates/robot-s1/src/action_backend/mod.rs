//! v0.3 Phase B B2 (1/3) action backend infrastructure（B2 (2/3) land）。
//!
//! 本模块是 `robot-s1` 内部 backend 注册中心：
//!
//! - [`BackendRegistry`]：所有通过 `inventory::submit!` 注册过的
//!   [`crate::action::ActionBackend`] 静态实例的查询表。`coordinator`
//!   端在启动期 `validate_profile_section` + `dispatch` 时使用。
//! - [`MockDwellBackend`](mock_dwell::MockDwellBackend)：B2 唯一 backend 实现
//!   （feature `backend-mock-dwell` gate；默认 on）；其它 backend 留 B3+。
//!
//! ## 边界
//!
//! - 模块 `pub`（B2 (3/3) 起）：integration test 在
//!   `crates/robot-s1/tests/action_dispatch.rs` 是 separate crate，仅
//!   看 lib `pub` 表面；`pub` 升级仅服务 in-crate integration test，跟
//!   "对外部 crate 暴露"无关（workspace 内目前无外部 crate 把
//!   `robot-s1` 列依赖）。新增 backend 仍走 backend crate
//!   `inventory::submit!` 注册路径，不直接构造 [`BackendRegistry`]。
//! - **不**实现 [`crate::action::ActionContext::clock`] / `publisher` 等
//!   coordinator 端运行时 plumbing；本模块只负责 "知道有哪些 backend、
//!   按 (kind, schema_id) 路由"。
//! - **不**做 RT 路径（B2 全程在普通线程）；inventory iter 仅启动期一
//!   次性遍历，之后查 `HashMap`，无 alloc 无 IO。
//!
//! ## 静态注册模式（与 robot-core::factory 同款）
//!
//! 每个 backend crate（B2: `mock_dwell`；B3+: `ik-xlerobot` /
//! `rl-onnx` / `scripted`）按下面模式自注册：
//!
//! ```ignore
//! pub struct MyBackend;
//! impl ActionBackend for MyBackend { ... }
//! static MY_BACKEND: MyBackend = MyBackend;
//! inventory::submit! { &MY_BACKEND as &'static dyn ActionBackend }
//! ```
//!
//! 链接器把每个 `submit!` 项收到一个 array section；
//! `BackendRegistry::from_inventory` 走一次 `inventory::iter` 建查询表。

#![allow(clippy::doc_markdown)]

use std::collections::HashMap;

use crate::action::{ActionBackend, ActionKind, NO_PAYLOAD_SCHEMA_ID, SubmitError};

#[cfg(feature = "backend-mock-dwell")]
pub mod mock_dwell;

#[cfg(feature = "backend-hold-pose")]
pub mod hold_pose;

#[cfg(feature = "backend-scripted")]
pub mod scripted;

#[cfg(feature = "backend-ik")]
pub mod ik;

// v0.5 候选 E 段(7)（Spec QA6 + R3-B1）：test-only 执行期 failure 触发器。
// **永不进 default features**——生产 binary 不 link，profile 引用它会在
// validate_profile_section 启动期 UnknownBackend fail-fast。
#[cfg(feature = "test-fault-backend")]
pub mod fault_inject;

// 链接器收集每个 backend crate 的 `inventory::submit!` 项。
inventory::collect!(&'static dyn ActionBackend);

// ============================================================================
// BackendRegistry
// ============================================================================

/// 所有通过 `inventory::submit!` 链接进当前 binary 的
/// [`ActionBackend`] 静态实例的查询表。
///
/// **构造**：[`BackendRegistry::from_inventory`] 走一次 `inventory::iter`
/// 建 `HashMap<&'static str, &'static dyn ActionBackend>`。启动期一
/// 次性，无 alloc 无 IO；之后查表是 O(1) hash lookup。
///
/// **dispatch 语义**：[`BackendRegistry::dispatch`] 接受 `enabled` 过滤
/// 列表（来自 `[s1.profile.backends]`）+ `(kind, schema_id)`，返第一个
/// 同时满足三条的 backend：
///
/// 1. `name() ∈ enabled`
/// 2. `kind ∈ supported_kinds()`
/// 3. `schema_id ∈ supported_schemas()`
///
/// 任一条不满足返对应 [`SubmitError`] —— `NoBackendForKind`（kind 没人
/// 支持）/ `UnsupportedSchema`（kind 有支持者但 schema_id 不在他的
/// `supported_schemas()`）。优先级：先看 kind，再看 schema。
///
/// **`enabled` 列表来源**：调用方（`coordinator::Coordinator`）持有
/// `ProfileSection.backends`；dispatch 时透传作 filter。registry 本身
/// 不存 enabled list，便于 binary 可重新解 profile 后用同一 registry
/// （B4+ 复用场景）。
///
/// **同 kind/schema 多 backend 命中**：B2 单 action 序列化 + 静态
/// inventory；任一 (kind, schema_id) 应仅一个 backend 命中。如多个命中，
/// dispatch 返**首个**（HashMap 遍历顺序未定，但同 binary 同 link 顺
/// 序下稳定）；B3+ scheduler 引入 priority 后再升级 deterministic 路由。
pub struct BackendRegistry {
    /// `name() → &'static dyn ActionBackend`。`name()` 借 backend 自
    /// 报；inventory 注册时 `Sync + 'static` trait bound 保证字节静态。
    by_name: HashMap<&'static str, &'static dyn ActionBackend>,
}

impl BackendRegistry {
    /// 从 `inventory::iter` 收集所有 link 进 binary 的
    /// [`ActionBackend`] 静态实例。**永不失败**（inventory iter 是
    /// 编译/链接期决定，runtime 仅遍历一个静态 array）。
    ///
    /// **重名处理**：HashMap insert 把后来者覆盖前者；inventory iter 顺
    /// 序由链接器决定。同 binary 重名 backend 是配置事故（应在 PR
    /// review 阶段拦），运行期不报错（无显式错误返）—— B3+ 可加
    /// debug-assertion；B2 不引入。
    #[must_use]
    pub fn from_inventory() -> Self {
        let mut by_name = HashMap::new();
        for backend in inventory::iter::<&'static dyn ActionBackend> {
            by_name.insert(backend.name(), *backend);
        }
        Self { by_name }
    }

    /// **测试 helper**：用显式 backend 列表构建 registry，不走 inventory。
    /// 让单测可以独立于全局 inventory 状态（避免测试间相互影响）。
    ///
    /// **可见性**（B2 (3/3) 升级）：原 `#[cfg(test)] pub(crate)` 改 `pub`
    /// 让 `crates/robot-s1/tests/action_dispatch.rs` integration test 能
    /// 用（integration test crate 不见 lib `#[cfg(test)]`）。生产路径
    /// 仍走 [`Self::from_inventory`]；本入口的语义是"测试桥接"而非"动
    /// 态注册"，仍**不**承诺向 binary 层暴露。
    #[must_use]
    pub fn from_explicit(backends: &[&'static dyn ActionBackend]) -> Self {
        let mut by_name = HashMap::new();
        for &b in backends {
            by_name.insert(b.name(), b);
        }
        Self { by_name }
    }

    /// 是否注册过名为 `name` 的 backend。
    /// 给 `validate_profile_section` 检查 `[s1.profile.backends]`
    /// L2 unknown-backend 路径用。
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// 注册过的 backend 名字迭代器（debug / 错误信息 / 测试断言）。
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.by_name.keys().copied()
    }

    /// 按 name 查 backend；未注册返 `None`。
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&'static dyn ActionBackend> {
        self.by_name.get(name).copied()
    }

    /// 按 `(kind, schema_id)` 在 `enabled` 子集里路由。
    ///
    /// **失败语义**：
    ///
    /// - `enabled` 中没有任何 backend `supported_kinds()` 含 `kind` →
    ///   [`SubmitError::NoBackendForKind`]。
    /// - `enabled` 中 N 个 backend 支持 `kind`，但其 `supported_schemas()`
    ///   全都不含 `schema_id` → [`SubmitError::UnsupportedSchema`]，
    ///   `name` 字段填首个支持 `kind` 的 backend 名（debug 友好）。
    pub fn dispatch(
        &self,
        enabled: &[String],
        kind: ActionKind,
        schema_id: u32,
    ) -> Result<&'static dyn ActionBackend, SubmitError> {
        let mut kind_match: Option<&'static dyn ActionBackend> = None;
        for name in enabled {
            let Some(backend) = self.by_name.get(name.as_str()).copied() else {
                // unknown-backend 应在 validate_profile_section 阶段 fail-fast；
                // 走到这里说明 coordinator state 不一致，跳过这个 name 让
                // 其它 enabled backend 有机会响应。
                continue;
            };
            if !backend.supported_kinds().contains(&kind) {
                continue;
            }
            if kind_match.is_none() {
                kind_match = Some(backend);
            }
            if backend.supported_schemas().contains(&schema_id) {
                return Ok(backend);
            }
        }

        if let Some(b) = kind_match {
            // kind 有支持者，但 schema_id 不在他的 supported_schemas()
            // 集合 —— 报 UnsupportedSchema（带 backend 名）。
            return Err(SubmitError::UnsupportedSchema(
                b.name().to_string(),
                schema_id,
            ));
        }
        Err(SubmitError::NoBackendForKind(kind))
    }
}

impl core::fmt::Debug for BackendRegistry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // 不打印 trait object 内部 —— 仅列 name 集，足够日志 + 测试断言。
        let names: Vec<&'static str> = self.names().collect();
        f.debug_struct("BackendRegistry")
            .field("backends", &names)
            .finish()
    }
}

// `NO_PAYLOAD_SCHEMA_ID` re-export 以便 mock_dwell.rs 同 crate 引入。
// （已通过 `use crate::action::NO_PAYLOAD_SCHEMA_ID` 提供给 mod 内文件；
// pub(crate) re-export 不必要，删之。）
const _: u32 = NO_PAYLOAD_SCHEMA_ID;

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use crate::action::{
        ActionBackend, ActionContext, ActionInstance, ActionKind, ActionRequest, ActionStartError,
        NO_PAYLOAD_SCHEMA_ID,
    };

    // ----- 测试用 stub backend -----

    /// 单元测试 stub：声明 supported_kinds / schemas，start / tick /
    /// cancel 仅返占位值；让 dispatch 测试不依赖 mock_dwell。
    struct StubBackend {
        name: &'static str,
        kinds: &'static [ActionKind],
        schemas: &'static [u32],
    }

    impl ActionBackend for StubBackend {
        fn name(&self) -> &str {
            self.name
        }
        fn supported_kinds(&self) -> &[ActionKind] {
            self.kinds
        }
        fn supported_schemas(&self) -> &[u32] {
            self.schemas
        }
        fn start(
            &self,
            _req: &ActionRequest,
            _ctx: &ActionContext<'_>,
        ) -> Result<Box<dyn ActionInstance>, ActionStartError> {
            // dispatch 测试不会走 start；落实到 instance 路径 mock_dwell
            // 自己测。
            Err(ActionStartError::Rejected("stub".into()))
        }
    }

    // 给 stub 起 'static 实例供 from_explicit 用。
    static STUB_DWELL_NO_PAYLOAD: StubBackend = StubBackend {
        name: "stub_dwell",
        kinds: &[ActionKind::DwellMs],
        schemas: &[NO_PAYLOAD_SCHEMA_ID],
    };
    static STUB_HOLD_BIN_PAYLOAD: StubBackend = StubBackend {
        name: "stub_hold",
        kinds: &[ActionKind::HoldPose],
        schemas: &[0xCAFE_0001],
    };

    // ----- registry construction -----

    #[test]
    fn from_explicit_collects_named_backends() {
        let r = BackendRegistry::from_explicit(&[
            &STUB_DWELL_NO_PAYLOAD as &'static dyn ActionBackend,
            &STUB_HOLD_BIN_PAYLOAD as &'static dyn ActionBackend,
        ]);
        assert!(r.has("stub_dwell"));
        assert!(r.has("stub_hold"));
        assert!(!r.has("does_not_exist"));
        let names: Vec<_> = {
            let mut v: Vec<&'static str> = r.names().collect();
            v.sort_unstable();
            v
        };
        assert_eq!(names, vec!["stub_dwell", "stub_hold"]);
    }

    #[test]
    fn get_returns_some_for_registered_and_none_for_unknown() {
        let r =
            BackendRegistry::from_explicit(&[&STUB_DWELL_NO_PAYLOAD as &'static dyn ActionBackend]);
        let b = r.get("stub_dwell").expect("registered");
        assert_eq!(b.name(), "stub_dwell");
        assert!(r.get("missing").is_none());
    }

    #[test]
    fn debug_lists_backend_names_only() {
        let r =
            BackendRegistry::from_explicit(&[&STUB_DWELL_NO_PAYLOAD as &'static dyn ActionBackend]);
        let s = format!("{r:?}");
        assert!(s.contains("BackendRegistry"));
        assert!(s.contains("stub_dwell"));
    }

    // ----- dispatch -----

    #[test]
    fn dispatch_routes_by_kind_and_schema() {
        let r = BackendRegistry::from_explicit(&[
            &STUB_DWELL_NO_PAYLOAD as &'static dyn ActionBackend,
            &STUB_HOLD_BIN_PAYLOAD as &'static dyn ActionBackend,
        ]);
        let enabled = vec!["stub_dwell".to_string(), "stub_hold".to_string()];

        let b = r
            .dispatch(&enabled, ActionKind::DwellMs, NO_PAYLOAD_SCHEMA_ID)
            .expect("dwell + sentinel matches stub_dwell");
        assert_eq!(b.name(), "stub_dwell");

        let b = r
            .dispatch(&enabled, ActionKind::HoldPose, 0xCAFE_0001)
            .expect("hold + 0xCAFE_0001 matches stub_hold");
        assert_eq!(b.name(), "stub_hold");
    }

    #[test]
    fn dispatch_rejects_unsupported_kind_with_no_backend_for_kind() {
        // registry 里只有 DwellMs 支持者 → 提交 HoldPose 应返
        // NoBackendForKind（不报 UnsupportedSchema）。
        let r =
            BackendRegistry::from_explicit(&[&STUB_DWELL_NO_PAYLOAD as &'static dyn ActionBackend]);
        let enabled = vec!["stub_dwell".to_string()];

        // T = &'static dyn ActionBackend 不 impl Debug，不能 expect_err；
        // 走 let-else 把 Ok 排除。
        let Err(err) = r.dispatch(&enabled, ActionKind::HoldPose, NO_PAYLOAD_SCHEMA_ID) else {
            panic!("expected Err");
        };
        assert!(
            matches!(err, SubmitError::NoBackendForKind(ActionKind::HoldPose)),
            "got {err:?}"
        );
    }

    #[test]
    fn dispatch_rejects_unsupported_schema_with_backend_name() {
        // kind 有支持者 (stub_hold)，但 schema_id 不在他的 supported_schemas()
        // 集合 → 应返 UnsupportedSchema 带 backend 名。
        let r =
            BackendRegistry::from_explicit(&[&STUB_HOLD_BIN_PAYLOAD as &'static dyn ActionBackend]);
        let enabled = vec!["stub_hold".to_string()];

        let Err(err) = r.dispatch(&enabled, ActionKind::HoldPose, 0xDEAD_BEEF) else {
            panic!("expected Err");
        };
        match err {
            SubmitError::UnsupportedSchema(name, schema) => {
                assert_eq!(name, "stub_hold");
                assert_eq!(schema, 0xDEAD_BEEF);
            }
            other => panic!("expected UnsupportedSchema, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_skips_backends_not_in_enabled_list() {
        // registry 含 stub_dwell + stub_hold；enabled 仅 stub_hold；
        // 提交 DwellMs → NoBackendForKind（stub_dwell 虽 link 但不 enable）。
        let r = BackendRegistry::from_explicit(&[
            &STUB_DWELL_NO_PAYLOAD as &'static dyn ActionBackend,
            &STUB_HOLD_BIN_PAYLOAD as &'static dyn ActionBackend,
        ]);
        let enabled = vec!["stub_hold".to_string()];

        let Err(err) = r.dispatch(&enabled, ActionKind::DwellMs, NO_PAYLOAD_SCHEMA_ID) else {
            panic!("expected Err");
        };
        assert!(matches!(
            err,
            SubmitError::NoBackendForKind(ActionKind::DwellMs)
        ));
    }

    #[test]
    fn dispatch_skips_unknown_enabled_names_silently() {
        // enabled 含一个不在 registry 的 name（理论上 validate 已拦，
        // 但 coordinator state 失同步时降级处理）；其它 enabled name
        // 仍正常 dispatch。
        let r =
            BackendRegistry::from_explicit(&[&STUB_DWELL_NO_PAYLOAD as &'static dyn ActionBackend]);
        let enabled = vec!["ghost".to_string(), "stub_dwell".to_string()];

        let b = r
            .dispatch(&enabled, ActionKind::DwellMs, NO_PAYLOAD_SCHEMA_ID)
            .expect("stub_dwell still dispatches");
        assert_eq!(b.name(), "stub_dwell");
    }
}
