//! `events_drain` —— S1 action event drain pubsub（B4 (1/3-c) 起 land；
//! B4 (2/3-c) 拆 wire codec / runtime 两段子模块）。
//!
//! ## 角色
//!
//! [`crate::action::ActionTicket::events`] 是 `Coordinator::submit` 成功后
//! 返给调用方的 `std::sync::mpsc::Receiver<ActionEvent>`（in-process
//! sync channel）。`StreamTaskEvents` RPC 跨进程拓扑（详
//! [`docs/handoffs/v0.3_b4_outline.md`](../../../docs/handoffs/v0.3_b4_outline.md)
//! §5.2 reviewer round 2 拍板）：
//!
//! ```text
//! s1_daemon process                       gateway process
//! ─────────────────                       ───────────────────────────
//!
//! Coordinator::submit
//!   → CurrentAction.events_tx
//!
//!   ↓ ActionEvent (in-process mpsc)
//!
//! events_drain (本模块)                   events_subscribe (B4 (2/3-c))
//!   - 持 Receiver<ActionEvent>             - 订阅 iceoryx2 pubsub:
//!   - loop recv() / recv_timeout            /<robot>/s1/action_events/<action_id>
//!   - encode action_event.fbs body         - poll latest sample
//!   - publish iceoryx2 pubsub               ↓ ActionEvent (decoded)
//!     /<robot>/s1/action_events/<action_id>
//!                                          tokio::sync::mpsc::Sender +
//!                                          tonic stream forwards
//! ```
//!
//! ## 模块 layout（B4 (2/3-c) 拆分）
//!
//! 1/3-c 期 `events_drain_thread.rs` 一文件 1001 行，B4 (2/3-c) 拆 2 子文件
//! 让可读性 / 跟 [`crate::rr_server`] 模式对齐 / reviewer 审计成本可控
//! （reviewer ACK B4 (2/3-a) 时备忘"events_drain_thread.rs 也按
//! request handler / wire codec / thread runtime 拆开"，本模块仅 wire +
//! runtime 两段，无独立 handler 层）：
//!
//! - [`wire`]：[`wire::encode_envelope`] (`ActionEvent` → fbs body bytes) +
//!   [`wire::is_terminal`] + 内部 `map_outcome` / `map_failure` helpers。
//!   纯 enum 映射 + FlatBuffers builder，零 iceoryx2 / 零线程依赖。
//! - [`runtime`]：[`runtime::EventsDrainThreadConfig`] /
//!   [`runtime::EventsDrainMetrics`] / [`runtime::EventsDrainThreadHandle`] /
//!   [`runtime::EventsDrainRegistry`] +
//!   [`runtime::spawn_events_drain_thread`] + 内部 `run_drain_loop` /
//!   `publish_event`。线程生命周期 + iceoryx2 publisher I/O，调
//!   [`wire::encode_envelope`] / [`wire::is_terminal`] 但不依赖内部 layout。
//!
//! ## 终止条件 / 时间戳来源 / B4 (2/3-c) 边界
//!
//! 详 [`runtime`] 顶部 module doc。
//!
//! ## 不在范围（B4 (2/3-c) 边界）
//!
//! - **不**接 robot-rpc gateway pubsub adapter（→
//!   `crates/robot-rpc/src/events_subscribe.rs`，本 commit 同步 land）。
//! - **不**做 cross-process integration test（→ B4 (3/3) `crates/robot-rpc/tests/...`）。
//! - **不**改 coordinator 状态机决策（drain 是 read-only consumer）。
//! - **不**碰 `JointCommand.source_task_id` / `FrameHeader.seq` —— drain
//!   thread 走独立 per-ActionId pubsub topic，跟 wire 协议契约 §3.7 完全
//!   分离。

#![allow(clippy::doc_markdown)]

pub mod runtime;
pub mod wire;

// 主要外部 API 直接 re-export，让调用方（s1_daemon binary、
// rr_server::handler 等）只需 `use robot_s1::events_drain::*` 一次拿到所
// 需类型。逐项 re-export 而非 glob，让 IDE 跳转 / rustdoc 链接更清晰。
pub use runtime::{
    EventsDrainMetrics, EventsDrainRegistry, EventsDrainThreadConfig, EventsDrainThreadHandle,
    spawn_events_drain_thread,
};
pub(crate) use runtime::{EventsPublisherGuard, spawn_owned_events_drain_thread};
pub use wire::{encode_envelope, is_terminal};
