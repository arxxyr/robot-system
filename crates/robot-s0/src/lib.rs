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

pub mod boot_supervisor;
pub mod clear_coordination;
mod clear_estop_action;
pub mod clear_estop_transaction;
#[cfg(feature = "test-comm-fault-hook")]
pub mod comm_fault_adapter;
#[cfg(feature = "test-comm-fault-hook")]
pub mod comm_fault_hook;
#[cfg(feature = "test-comm-fault-hook")]
pub mod comm_fault_observation;
#[cfg(feature = "test-comm-fault-hook")]
pub mod comm_fault_trace;
pub mod config_loader;
// S48 A3a：权限原语本体已归位 robot-core；此处直接重导出同一模块，不留复制实现。
pub use robot_core::control_authority;
// v0.5 候选 E 段(9)：真机部署件（guard / preflight / provision）。纯逻辑不加
// feature 门——AC-E16/E21/E22 的 unit 判据必须在默认 gate 内跑到；真串口实现
// 与 `xlerobot_deploy` bin 才由 `xlerobot-real` 守门（详 deploy/mod.rs doc）。
pub mod deploy;
pub mod diag;
pub mod estop_latch;
pub mod event_ingress;
#[cfg(any(test, feature = "test-freeze-hook"))]
pub mod freeze_control;
#[cfg(any(test, feature = "test-freeze-hook"))]
pub mod freeze_hook;
pub mod hard_rt_ctrl;
pub mod io_thread;
pub mod ipc_pending;
pub(crate) mod ipc_runtime;
pub mod ipc_thread;
// S48 A3d / W1：latch 目标稳定锁、共享名称校验与绑定目录 fd 的数据对象检查。
pub mod latch_lock;
// S48 A3e / W2a：唯一磁盘 writer 纯模块（义务 / 进度 / 公告 / 协议 / 本体 / 配对 / 监督）；无生产调用方。
pub mod latch_writer;
pub mod persist_health;
pub mod pub_thread;
pub mod rr_admission;
pub mod rr_server_thread;
pub mod rx_trust;
pub(crate) mod serial_convergence;
pub mod serial_io_ctrl;
pub mod serial_io_thread;
pub mod serial_preempt;
mod serial_tick;
mod set_enabled_action;
pub mod set_enabled_rr_server;
// S48 W2b：纯关闭 / join 机制（句柄槽、有界 join、门谓词注入的顺序停止与唯一报告）；无生产调用方。
pub mod shutdown_mechanism;
pub mod state_machine;
// S48 A3b：`S0StateSnapshot` 的显式字段 codec（快照编码与 state_machine 分文件）。
mod state_snapshot_codec;
// v0.5 候选 E 段(8)：`systemd_notify` 迁入 `robot_health::systemd`（单一实现），
// 本 crate 不再持副本；s0_daemon 直接调 robot_health。
pub mod watchdog;
pub mod worker_stop;

/// S48 A3a：唯一依赖本 crate 内部 `hard_rt_ctrl::inhibit` 常量的位兼容断言。
/// 这些常量是 `pub(crate)`，外部公开验收无法引用，故保留在本 crate 的单测内。
#[cfg(test)]
mod control_authority_inhibit_compat {
    use crate::hard_rt_ctrl::inhibit;
    use robot_core::control_authority::{InhibitReason, InhibitSet};

    #[test]
    fn inhibit_bits_match_the_existing_serial_reasons() {
        assert_eq!(InhibitReason::CommFault.bit(), inhibit::COMM_FAULT);
        assert_eq!(InhibitReason::AdapterFault.bit(), inhibit::ADAPTER_FAULT);
        assert_eq!(InhibitReason::StopFailed.bit(), inhibit::STOP_FAILED);
        assert_eq!(InhibitReason::Converging.bit(), inhibit::CONVERGING);
        assert_eq!(InhibitReason::FaultPending.bit(), inhibit::FAULT_PENDING);
        assert_eq!(InhibitReason::Boot.bit(), inhibit::BOOT);
        let set = InhibitSet::from_bits(inhibit::BOOT | inhibit::COMM_FAULT).unwrap();
        assert_eq!(
            set.reasons().collect::<Vec<_>>(),
            vec![InhibitReason::CommFault, InhibitReason::Boot]
        );
        for reason in InhibitReason::ALL {
            assert_eq!(
                reason.is_releasable(),
                matches!(
                    reason,
                    InhibitReason::Boot | InhibitReason::Converging | InhibitReason::FaultPending
                )
            );
        }
    }
}

#[cfg(test)]
mod post_e_acceptance;
#[cfg(test)]
mod post_e_convergence_acceptance;
#[cfg(test)]
mod post_e_diag_acceptance;
#[cfg(test)]
mod test_util;

#[cfg(target_os = "linux")]
pub mod rt_thread;
