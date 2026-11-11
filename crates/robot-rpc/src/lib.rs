//! gRPC 网关、鉴权与 iceoryx2 控制面桥接。
//!
//! - `robot-gateway` 提供 tonic 服务；鉴权、capability、期限与幂等守门在网关
//!   完成，S0/S1 通过各自控制 action 执行状态变更，不直接修改保护文件。
//! - ClearEStop / SetEnabled 及任务控制走独立 RR；状态由常驻订阅池读取，任务事件
//!   经每个 ActionId 的 pub/sub 转为 gRPC stream。
//! - wire role 仅供审计，不能替代入口鉴权；dry-run 进入真实 action 评估层，不在
//!   网关短路。相同幂等 key 的不同 payload 必须拒绝，缓存只在当前进程内有效。
//! - RpcStatus 到 tonic::Code 使用全枚举显式映射；plain TCP / token 模式保持
//!   loopback bind 守门。
//! - EStop latch 缺失或损坏保持 strict fail-closed；解除仅走 maintainer+ 的
//!   ClearEStop 和既有持久化、代次守门。通信保护合同见 `docs/10_verification.md`。

// 中文 doc 块大量 identifier-like 字串（CommandMeta / RoleTable / RpcStatus
// / ClearEStop / TTL / UUIDv7 等），robot-rpc 全 crate 走宽松 doc-markdown。
#![allow(clippy::doc_markdown)]

pub mod auth;
pub mod class_cache;
pub mod clear_estop;
pub mod client;
pub mod estop;
pub mod estop_pub_thread;
pub mod events_subscribe;
pub mod forward_queue;
pub mod gateway;
pub(crate) mod gateway_ledger;
pub mod idempotency;
pub mod reply_classify;
pub mod rr_io_thread;
pub mod set_enabled;
pub mod state_convert;
pub mod state_pool;
pub mod wire_credit;
// v0.5 候选 E 段(8)（Spec D12 + P.2.7）：tonic loopback probe（READY 门槛 +
// watchdog observer）/ worker liveness 面 + test-only freeze hook / control。
#[cfg(any(test, feature = "test-freeze-hook"))]
pub mod freeze_control;
pub mod liveness;
pub mod systemd_probe;
pub mod task_handlers;

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
        clippy::must_use_candidate,
        clippy::needless_lifetimes,
        clippy::redundant_pub_crate,
        clippy::result_large_err,
        clippy::similar_names,
        clippy::too_many_lines,
        clippy::wildcard_imports,
        unreachable_pub
    )]
    // tonic-build 生成进 `$OUT_DIR/robot.control.v1.rs`；本 macro 把
    // 它 include 到当前 module。
    tonic::include_proto!("robot.control.v1");
}
