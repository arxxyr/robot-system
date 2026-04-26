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

// ── adapter crate 强制 link（Step 7 task 27b）─────────────────────────
//
// 每个 adapter crate 通过 `inventory::submit!` 静态注册一个 factory，注册
// 项靠链接器收到 special section。但 Rust 的 dead-code stripping 会丢弃 *未被
// 任何代码引用*的 lib，导致 `submit!` item 被一起 strip。
//
// 在这里 `pub use ... as _` 强制 lib 被 link，inventory section 才会被合并；
// `as _` 不污染 robot_s0 公开命名空间。
#[cfg(feature = "mock")]
#[allow(unused_imports)]
pub use robot_adapter_mock as _;

#[cfg(feature = "feetech")]
#[allow(unused_imports)]
pub use robot_adapter_feetech as _;

pub mod config_loader;
pub mod estop_latch;
pub mod hard_rt_ctrl;
pub mod io_thread;
pub mod ipc_thread;
pub mod ipc_tick;
pub mod pub_thread;
pub mod serial_io_ctrl;
pub mod serial_io_thread;
pub mod state_machine;
pub mod systemd_notify;
pub mod watchdog;

#[cfg(test)]
mod test_util;

#[cfg(target_os = "linux")]
pub mod rt_thread;
