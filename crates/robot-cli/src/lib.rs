//! # robot-cli
//!
//! `robotctl` —— 运维 CLI。
//!
//! ## 既有子命令（v0.2 Part 2；本 crate 既有 binary 行为 0 改动）
//!
//! - `robotctl status` / `watch` / `estop` —— iceoryx2 直连 S0
//!   （见 `bin/robotctl.rs`；手写 parser）。
//!
//! ## v0.5 候选 A（operator CLI）共享 client harness
//!
//! 候选 A 扩 7 个 tonic 子命令（`clear-estop` / `clear-error` /
//! `get-task-status` / `cancel-task` / `stream-events` / `invoke scripted` /
//! `invoke ik`）。本 lib 落**跨子命令共享**的基础设施，binary 按 RPC 家族
//! 分 commit 增量接入：
//!
//! - [`exit_code`] —— `tonic::Code` → CLI exit code 完整映射（AC-5）。
//! - [`uuid_v7`] —— `request_id` 生成（`UUIDv7`）。
//! - [`meta`] —— `CommandMeta` 构造 + token 解析 + source 默认（AC-6）。
//! - [`validate`] —— `action-id` `UUIDv7` 校验 + clear-estop 四道闸（AC-3 / AC-11）。
//! - [`gateway_client`] —— gateway 地址解析 + 连接。
//! - [`json_out`] —— 输出格式（human + `--json`，AC-7）。
//! - [`cli_args`] —— gateway 子命令手写参数 parser（不引 clap，P1-2）。
//! - [`dispatch`] —— async-lane 子命令 dispatch + 各 RPC handler。
//! - [`subcommand_lane`] —— 纯路由函数（既有 3 走 sync、新 gateway 子命令走 async）。
//! - [`block_on_async`] —— async 子命令的 tokio runtime 入口（Q-CLI2(b)）。
//!
//! ## 增量边界（Plan round 1 P1-3）
//!
//! 每个新子命令的 parser 识别 + dispatch + 实现随**各自 impl commit**
//! 落地，避免可达 stub：[`dispatch::is_dispatchable_async`] 是"已落地、
//! 入口分流会接管"的权威来源（随 commit 增长），`main()` 只把命中
//! 它的名字交给 [`block_on_async`]；未落地的 async-lane 名仍走
//! `bin/robotctl.rs` 手写 parser → "unknown subcommand" exit 2（不可达 /
//! 不 panic）。
//!
//! - (1/5)：共享 lib + 纯路由 + runtime 脚手架；bin **0 改动**。
//! - (2/5)：`clear-estop` + `clear-error` 落地（首次接入 async parser/dispatch
//!   + main 入口分流）。
//! - (3/5)：`get-task-status` / `cancel-task` / `stream-events` 落地（action-id
//!   校验复用 + stream SIGINT / 整流 timeout）。
//! - (4/5)：`payload`（`ScriptedPayload` / `IkPayload` fbs 构造）随 invoke
//!   落地（invoke-only，非共享）。

#![forbid(unused_must_use)]

pub mod cli_args;
pub mod dispatch;
pub mod exit_code;
pub mod gateway_client;
pub mod json_out;
pub mod meta;
pub mod uuid_v7;
pub mod validate;

/// 子命令路由 lane：既有 3 个走 sync 路径，新 gateway 子命令走 async (tokio)。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Lane {
    /// 同步路径（iceoryx2 直连 S0）：`status` / `watch` / `estop`。
    Sync,
    /// 异步路径（tokio + tonic）：候选 A 新 gateway 子命令。
    Async,
}

/// 纯路由函数（by-name，Q-CLI2(b)）：决定**顶层子命令**走 sync 还是 async。
///
/// `invoke` 是单顶层 token（`scripted` / `ik` 是其位置子类型，在 invoke
/// handler 内分流，候选 A (4/5)）。未知名返 `None`（binary 走既有
/// "unknown subcommand" → exit 2）。
///
/// **(1/5) 说明**：本函数即落 + 单测全表；但 `bin/robotctl.rs` 的手写
/// parser 在 (1/5) **仍只识别** `status` / `watch` / `estop`，async-lane
/// 名字此刻不可达（避免可达 stub，Plan round 1 P1-3）。
#[must_use]
pub fn subcommand_lane(name: &str) -> Option<Lane> {
    match name {
        "status" | "watch" | "estop" => Some(Lane::Sync),
        "clear-estop" | "clear-error" | "get-task-status" | "cancel-task" | "stream-events"
        | "invoke" => Some(Lane::Async),
        _ => None,
    }
}

/// async 子命令的 tokio runtime 入口（Q-CLI2(b)；AC-2 `new_multi_thread`）。
///
/// (1/5) 作为脚手架落 lib（已单测）；binary 从 (2/5) 第一个 async 子命令
/// 起调用。
#[must_use]
pub fn block_on_async<F: std::future::Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio multi-thread runtime");
    runtime.block_on(future)
}

#[cfg(test)]
mod tests {
    use super::{Lane, block_on_async, subcommand_lane};

    #[test]
    fn lane_routes_existing_three_to_sync() {
        for n in ["status", "watch", "estop"] {
            assert_eq!(subcommand_lane(n), Some(Lane::Sync), "{n}");
        }
    }

    #[test]
    fn lane_routes_new_gateway_subcommands_to_async() {
        for n in [
            "clear-estop",
            "clear-error",
            "get-task-status",
            "cancel-task",
            "stream-events",
            "invoke",
        ] {
            assert_eq!(subcommand_lane(n), Some(Lane::Async), "{n}");
        }
    }

    #[test]
    fn lane_unknown_returns_none() {
        for n in ["", "scripted", "ik", "bogus", "STATUS", "clear_estop"] {
            assert_eq!(subcommand_lane(n), None, "{n}");
        }
    }

    #[test]
    fn block_on_async_runs_future_to_completion() {
        let out = block_on_async(async { 40_u32 + 2 });
        assert_eq!(out, 42);
    }
}
