//! # robot-rpc
//!
//! v0.3 A1 ClearEStop control-plane slice gateway。
//!
//! ## 当前 scope（A1-1 (3/3) → A1-3 (2/3-a) 落地后）
//!
//! - **lib**：[`proto`] 模块（tonic codegen）+ [`auth`] /
//!   [`idempotency`] / [`clear_estop`] 纯函数 + [`gateway`] 模块（A1-3
//!   (2/3-a) 起加；含 [`gateway::ClearEStopHandler`] +
//!   [`gateway::run_gateway`] + [`gateway::GatewayConfig`] +
//!   [`gateway::validate_loopback_bind`]）。
//! - **bin**：`robot_rpc_gateway` 独立进程（A1-3 (2/3-a) 起加；loopback-only
//!   tonic gRPC server + A1-3 (2/3-b) 起接 iceoryx2 RR client →
//!   `s0_daemon`）。
//! - **不**接 S0 mutation；**不**删 `--allow-empty-latch` /
//!   `MissingLatchPolicy::AllowEmpty`；**不**宣称 `docs/00_design.md` §6.7
//!   #3 真锁住（A1 全 land 后由 A1-5 升级真宣称）。
//! - 依赖白名单登记走 `docs/CHANGELOG.md` 2026-04-27 "A1-1 (3/3) robot-rpc
//!   skeleton + tonic 引入" + 2026-04-28 "A1-3 (2/3) `robot_rpc_gateway`
//!   binary 依赖" 偏离登记。
//!
//! ## ClearEStop control action 安全边界（reviewer 拍板硬边界）
//!
//! 跟 `docs/CHANGELOG.md` 2026-04-26 "v0.3 entry：ClearEStop RPC 文案的控
//! 制动作解释" 解释登记一致：
//!
//! - **wire `role` 字段仅 audit**：S0 端**不**做授权决策；RPC 端**必须**
//!   完成 auth / role 查表 / capability 校验，viewer / operator 在 publish
//!   之前直接 PermissionDenied。
//! - **dry_run 进 S0 端 action layer**：RPC 端**不**短路返；只做 auth /
//!   confirmation / idempotency 预检。
//! - **idempotency**：robot-rpc 进程内 TTL cache，**不**持久化；cache key =
//!   `meta.idempotency_key`，**不**用 `request_id`；同 key 不同 payload 必
//!   须拒绝（**不**复用错误请求的 cached Ack）。
//! - **`RpcStatus` → `tonic::Code` 转译**：**禁止** `code as i32` cast；
//!   **必须**用显式 `match` 全枚举（[`clear_estop::rpc_status_to_tonic_code`]
//!   + 单测覆盖每条映射）。
//! - **gateway loopback bind 守门**（A1-3 (2/3-a) reviewer 拍板硬边界）：
//!   plain TCP + fake token 阶段必须 loopback；非 loopback bind 直接拒
//!   启动（[`gateway::validate_loopback_bind`]）。TLS / mTLS 留 v0.4+。
//!
//! ## A1-3 (2/3-b) / (2/3-c) / (3/3) 留作业
//!
//! - **A1-3 (2/3-b)**：[`gateway::ClearEStopHandler`] 接 RR I/O 独立
//!   `std::thread` + `Iceoryx2RrClient` 跟 daemon 端 `rr_server_thread`
//!   通信；集成 [`idempotency::IdempotencyCache`] +
//!   [`clear_estop::validate_clear_estop_command`] + [`auth`] role 查表 +
//!   deadline 计算 + `rpc_status_to_tonic_code` 显式 match 映射；handler
//!   通过 `tokio::sync::oneshot` async await wire response。
//! - **A1-3 (2/3-c)**：完整 graceful shutdown — 已进 handler 的请求继续
//!   等 RR response 或自身 `timeout_ms + rr_timeout_grace_ms`；尚未 forward
//!   才返 `tonic::Code::Unavailable`。
//! - **A1-3 (3/3)**：起两进程跑真 gRPC client → gateway → daemon 端到端
//!   集成测；CHANGELOG / handoff 升级 §6.7 #3 真宣称登记由 A1-5 闭环。
//! - **v0.4+**：TLS / mTLS / OIDC（A1 阶段接受 fake token）；response
//!   schema_id attribute 验证（A1-1 part 2 reviewer 非阻断提醒）。

// 中文 doc 块大量 identifier-like 字串（CommandMeta / RoleTable / RpcStatus
// / ClearEStop / TTL / UUIDv7 等），robot-rpc 全 crate 走宽松 doc-markdown。
#![allow(clippy::doc_markdown)]

pub mod auth;
pub mod clear_estop;
pub mod gateway;
pub mod idempotency;

/// gRPC tonic 生成代码（`robot.control.v1` namespace）。包含
/// `AuthContext` / `CommandMeta` / `ClearEStopCommand` / `Ack` /
/// `RobotControl` server trait（A1-3 真起 server 时 impl）。
pub mod proto {
    // tonic-build 生成代码不走 robot-system 的 lint 风格；显式 allow 各项
    // generated-code clippy 噪音（doc 格式 / 风格性 lint），保留**真错误**
    // 覆盖（如 unsafe / unused_must_use 等仍 deny）。
    #![allow(
        clippy::doc_markdown,
        clippy::doc_lazy_continuation,
        clippy::doc_overindented_list_items,
        clippy::default_trait_access,
        clippy::derive_partial_eq_without_eq,
        clippy::missing_const_for_fn,
        clippy::module_name_repetitions,
        clippy::needless_lifetimes,
        clippy::redundant_pub_crate,
        clippy::similar_names,
        clippy::wildcard_imports,
        unreachable_pub
    )]
    // tonic-build 生成进 `$OUT_DIR/robot.control.v1.rs`；本 macro 把
    // 它 include 到当前 module。
    tonic::include_proto!("robot.control.v1");
}
