//! # robot-s1
//!
//! S1 快反射层。
//!
//! ## v0.2 → v0.3 演进
//!
//! - **v0.2 Part 2**：仅 [`mock_s1`](crate::bin::mock_s1) 二进制 —— 发
//!   `JointCommand` + `Heartbeat` 给 S0 做最小闭环验证。
//! - **v0.3 Phase B**：B1 落地 [`coordinator`] lib 层 + `s1_daemon` binary
//!   （`hold + heartbeat` 最小闭环 + 启动锚定 fail-fast）。`mock_s1` 仍保留
//!   作 ballet 工具（multi-joint 正弦轨迹手测）。完整 coordinator
//!   （trajectory 调度、IK 分发、`CommandMeta` 幂等分流）留 v0.3 Phase B
//!   后续工作项 + v0.4。
//!
//! ## 模块边界
//!
//! - [`coordinator`]：纯逻辑 + 可测试核心 —— [`coordinator::CoordinatorConfig`]
//!   / [`coordinator::parse_s1_section_from_toml`] /
//!   [`coordinator::build_hold_command`] / [`coordinator::read_anchor_from_s0_state`]。
//!   零 iceoryx2 / 零 OS 依赖（除测试时透明的 [`robot_core::time`]）；
//!   binary 层通过 `LatestSubscriber<JointState>` trait object 注入真订阅。
//! - [`state_cache`]（v0.5 候选 E 段(5)）：S1 常驻 S0 state cache 线程
//!   —— Scripted `scripted_start_pose`（提交时当前位）唯一来源；
//!   `latest_fresh` 三重校验（joint_count / layout_id / 新鲜度）失败即拒
//!   Scripted submit（`FailedPrecondition`），不 fallback 到启动 anchor。
//! - [`action`]：v0.3 Phase B B2 (1/3) 起 land 的 action interface 类型
//!   骨架（[`action::ActionRequest`] / [`action::ActionKind`] /
//!   [`action::ActionBackend`] trait / [`action::JointCommandBody`]
//!   newtype / 错误枚举）。**B2 (1/3) 仅 lib types**；`BackendRegistry`
//!   / coordinator state machine / mock_dwell backend 留 B2 (2/3)，
//!   binary wiring 留 B2 (3/3)。详
//!   [`docs/handoffs/v0.3_b2_outline.md`](../../../docs/handoffs/v0.3_b2_outline.md)。
//! - **不**依赖 `robot-s0`（避免 s1 → s0 层级反向依赖；TOML 同格式但
//!   robot-s1 自反序列化最小子集 [`coordinator::S1Section`]）。
//! - **不**链接 tokio / tonic（Phase B 全程同步线程模型，对齐 mock_s1）。
//!   B2 outline §10 决策 #10 + 阻断 1 锁定：B4 跨 sync ↔ async 边界在
//!   robot-rpc-gateway 端做 adapter thread，不污染 robot-s1。

#![forbid(unused_must_use)]
// 中文 doc 块大量 identifier-like 字串（tokio / tonic / mock_s1 等），
// 跟 coordinator 模块走相同的宽松 doc-markdown 口径。
#![allow(clippy::doc_markdown)]

pub mod action;
// B2 (3/3) 升级：模块从 `pub(crate)` 改 `pub`，让 `crates/robot-s1/tests/`
// integration test 能访问 `BackendRegistry` / `MockDwellBackend` /
// `Coordinator` 等 wiring 必需 API（integration test 是 separate crate
// from lib's POV，仅看 `pub` 表面）。
//
// 与 B2 (2/3) reviewer 拍板"优先 pub(crate) mod action_backend;"的契
// 约对齐：先前 pub(crate) 是为防"暴露给下游 crate"，但 B2 (3/3) 落
// integration test 路径需要 lib 显式 pub 才能跨 test crate 引用（B1
// 同款 precedent：`run_tick_loop` / `read_anchor_from_s0_state` 也是
// pub，被 `tests/tick_loop_in_process.rs` 用）。可见性升级理由仅源于
// integration test 工程需要，不增加任何"对外部 crate 暴露"语义 ——
// workspace 内目前无任何外部 crate 把 robot-s1 列依赖。
pub mod action_backend;
pub mod coordinator;
// v0.5 候选 E 段(5)（Spec D3 第 1 条 + P.2.5）：S1 常驻 S0 state cache 线程
// （`scripted_start_pose` 来源；stop + join + 正常 Drop，禁 mem::forget）。
// 详 [`state_cache`] 顶部 module doc。
pub mod state_cache;
// v0.5 候选 E 段(8)（Spec D12 + P.2.7）：worker liveness 计数（生产）+ test-only
// freeze hook / control（cfg 守门，永不进 default features）。
#[cfg(any(test, feature = "test-freeze-hook"))]
pub mod freeze_control;
pub mod liveness;
// B4 (2/3-c) 拆 1/3-c 的 `events_drain_thread.rs` (1001L) 成 wire/runtime
// 两段子模块（同 (2/3-a) `rr_server` 拆分模式）。详 [`events_drain`] 顶部
// module doc。
pub mod events_drain;
// B4 (2/3-a) 拆 1/3-b 的 `rr_server_thread.rs` (921L) 成 wire/handler/runtime
// 三段子模块，并同时把 1/3-b skeleton handler 升级为真 Coordinator forwarding。
// 详 [`rr_server`] 顶部 module doc。
pub mod rr_server;
