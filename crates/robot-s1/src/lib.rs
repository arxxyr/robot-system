//! # robot-s1
//!
//! S1 快反射层：协调动作执行、保持命令与心跳，提供任务控制和动作事件流。
//! 生产入口为 `robot-s1`，`mock_s1` 用于多关节正弦轨迹验证。
//!
//! ## 模块边界
//!
//! - [`coordinator`]：配置解析、启动锚定、动作状态机与 tick 循环；时钟和端口由调用方注入。
//! - [`action`] / [`action_backend`]：动作接口、后端注册与分发；具体后端由特性选择。
//! - [`state_cache`]：缓存 S0 状态，为 Scripted 提交提供当前位；关节数、布局或新鲜度
//!   校验失败时拒绝提交，不回退到启动锚点。
//! - [`rr_server`] / [`events_drain`]：通过 iceoryx2 接收任务控制请求并发布动作事件。
//! - [`liveness`]：工作线程存活观测；`freeze_control` 仅供测试冻结控制。
//!
//! S1 使用同步线程模型，不依赖 `robot-s0`、tokio 或 tonic；网关负责异步 RPC 适配。

#![forbid(unused_must_use)]
// 中文 doc 块大量 identifier-like 字串（tokio / tonic / mock_s1 等），
// 跟 coordinator 模块走相同的宽松 doc-markdown 口径。
#![allow(clippy::doc_markdown)]

pub mod action;
pub mod action_backend;
pub mod coordinator;
pub mod events_drain;
#[cfg(any(test, feature = "test-freeze-hook"))]
pub mod freeze_control;
pub mod liveness;
pub mod rr_server;
pub mod state_cache;
