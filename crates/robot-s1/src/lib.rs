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
pub mod coordinator;
