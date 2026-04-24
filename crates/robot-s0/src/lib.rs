//! # robot-s0
//!
//! S0 daemon：`docs/00_design.md` §1.2 四线程模型的 Rust 落地。
//!
//! **Part 1 Scope（本 crate 当前内容）**：
//! - [`state_machine`]：5 主态 + 4 子态 + 12 事件 + 转换表 + 单测
//! - `rt_thread`（Task 7）：`SCHED_FIFO` 70 + `mlockall` + tick 闭环（mock adapter 接入）
//! - `bin/s0_daemon`（Task 8）：最小 main，只拉起 `rt_thread`，smoke duration 后退出
//!
//! **Part 2 将新增**（留 TODO 不实现）：
//! - `ipc_thread`：iceoryx2 订阅 command / heartbeat → 写 `LatestCommandSlot`
//! - `pub_thread`：读 snapshot → iceoryx2 发布 state / heartbeat
//! - `io_thread`：poll `SoftRT` adapter
//! - `watchdog`：S1 heartbeat 超时检测 + `CONTROL_LOST_*` 事件发生
//! - `estop_latch`：持久化 `EStop` latch
//!
//! 详见 `docs/handoffs/2026-04-24_2.md` §6。

#![forbid(unused_must_use)]

pub mod state_machine;

#[cfg(target_os = "linux")]
pub mod rt_thread;

// Task 8 将加入 `bin/s0_daemon.rs`。
