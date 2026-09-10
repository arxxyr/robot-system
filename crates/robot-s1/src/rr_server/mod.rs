//! `rr_server` —— v0.3 B4 (2/3-a) S1 task control plane RR server 真 handler。
//!
//! ## 角色
//!
//! 沿 A1-3 (1/3c-1) `crates/robot-s0/src/rr_server_thread.rs` 同款 RR 线程
//! 模型，B4 (2/3-a) 把 1/3-b skeleton "每条 wire request 同步 reply
//! `Unimplemented`" 替换为真 [`crate::coordinator::Coordinator`] in-process
//! 调用 + spawn `events_drain_thread` + register 入
//! [`crate::events_drain::EventsDrainRegistry`]。
//!
//! 4 个 RR topic（沿 1/3-b skeleton 同款）：
//!
//! - `/<robot>/control/task_invoke` → [`handler::handle_invoke`] →
//!   `Coordinator::submit` + `events_drain_thread::spawn`。
//! - `/<robot>/control/task_cancel` → [`handler::handle_cancel`] →
//!   `Coordinator::cancel`。
//! - `/<robot>/control/task_status` → [`handler::handle_status`] →
//!   `Coordinator::status`。
//! - `/<robot>/control/clear_error` → [`handler::handle_clear_error`] →
//!   `Coordinator::clear_error`。
//!
//! ## 模块 layout（B4 (2/3-a) 拆分）
//!
//! 1/3-b 期 `rr_server_thread.rs` 一文件 921 行，B4 (2/3-a) 接真 handler
//! 后预计 1300+ 行，拆 3 子文件让可读性 / 可移植性 / reviewer 审计成本可
//! 控（reviewer round 4 ACK 1/3-b 时已留备忘"2/3 开始两个 module 都按
//! request handler / wire codec / thread runtime 拆开"）：
//!
//! - [`wire`]：fbs decode/encode helpers + 错误码映射（`SubmitError` /
//!   `ActionCancelError` / `ClearError` → `ActionRpcStatus`）。**纯字节↔Rust
//!   struct 转换 + 枚举映射**，零 iceoryx2 / 零 [`crate::coordinator::Coordinator`] 依赖。
//! - [`handler`]：[`handler::RealHandlerCtx`] + 4 个 `handle_*` 业务函数 +
//!   handler closure 构造 fn（`invoke_handler` / `cancel_handler` /
//!   `status_handler` / `clear_error_handler`）。**调 coordinator API + spawn
//!   events_drain**。
//! - [`runtime`]：[`runtime::RrServerThreadConfig`] /
//!   [`runtime::RrServerMetrics`] / [`runtime::RrServerThreadHandle`] /
//!   [`runtime::S1RrServerHandles`] + 4 个 `spawn_*_server` + 通用
//!   `spawn_typed` / `run_handler_loop` / `handle_active`。**纯
//!   iceoryx2 RR I/O 线程生命周期**，零 coordinator 依赖。
//!
//! ## shutdown 顺序契约（reviewer round 5 阻断锁定）
//!
//! `s1_daemon` shutdown 路径**必须**按以下顺序，避免 RR thread stop-but-
//! not-join 期间 in-flight invoke 调
//! [`crate::events_drain_thread::EventsDrainRegistry::register`] 跟
//! `events_registry.stop_and_join_all` 撞上 late register：
//!
//! ```text
//! 1. coordinator.mark_shutting_down()        // 拒新 submit（→ Unavailable）
//! 2. rr_handles.request_stop_all()           // 通知 RR thread 不接新 request
//! 3. rr_handles.join_all()                   // 等所有 in-flight handler 跑完
//! 4. events_registry.stop_and_join_all()     // 等所有 drain thread 跑完
//! ```
//!
//! 关键：第 3 步 `rr.join_all` 完成后才进第 4 步 `events_registry.stop_*`，
//! 保证 events_registry stop 期间不再有 RR thread 调 `register()`。
//! `coordinator.mark_shutting_down`（第 1 步）让 in-flight invoke 走
//! [`crate::action::SubmitError::ShuttingDown`] →
//! [`robot_msg::msg::ActionRpcStatus::Unavailable`] reply 路径，而**不**走
//! `register()`，从源头消除 late register 路径。
//!
//! lib unit test
//! [`handler::tests::shutdown_in_flight_invoke_rejected_no_register`] 锁
//! 住 invoke handler 在 `mark_shutting_down` 后必返 `Unavailable` 且不
//! `register()` 的契约。
//!
//! ## 不在范围（B4 (2/3-a) 边界）
//!
//! - **不**接 robot-rpc gateway（→ B4 (2/3-b)：gateway 4 个 RR client +
//!   5 RPC handler + sync↔async adapter + 错误码映射）。
//! - **不**接 `StreamTaskEvents` 跨进程 pubsub adapter（→ B4 (2/3-c)）。
//! - **不**起 events_drain_thread module 拆分（B4 (2/3-c) 跟 gateway streaming
//!   一起拆）。
//! - **不**做 idempotency cache / auth / capability 校验（gateway 端做；
//!   outline §8.1）。
//! - **不**做 cross-process integration test（→ B4 (3/3) `crates/robot-rpc/tests/...`）。
//! - **不**改 `JointCommand.source_task_id` / `FrameHeader.seq` —— 不破坏
//!   B3 wire 契约 §3.7。

#![allow(clippy::doc_markdown)]

pub mod handler;
pub mod runtime;
pub mod wire;

// 主要外部 API 直接 re-export，让调用方（s1_daemon binary、未来 lib 调用
// 方）只需要 `use robot_s1::rr_server::*` 一次拿到所需类型。runtime 层
// 跟 handler 层公开 surface 较多（`RrServerThreadConfig` / 4 个
// `spawn_*_server` / `RealHandlerCtx` / 4 个 `handle_*`），逐项 re-export
// 而非整 `pub use ...::*` glob 提升 IDE 跳转 / rustdoc 链接清晰度。
pub use handler::{
    RealHandlerCtx, handle_cancel, handle_clear_error, handle_invoke, handle_status,
};
pub use runtime::{
    OwnedRrServer, RrServerInitError, RrServerMetrics, RrServerSpawnError, RrServerThreadConfig,
    RrServerThreadHandle, S1RrServerHandles, spawn_clear_error_server, spawn_task_cancel_server,
    spawn_task_invoke_server, spawn_task_status_server, try_spawn_clear_error_server,
    try_spawn_task_cancel_server, try_spawn_task_invoke_server, try_spawn_task_status_server,
};
