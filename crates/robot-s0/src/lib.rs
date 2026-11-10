//! # robot-s0
//!
//! S0 在设备 adapter 之上组织硬件抽象与运行时，复用 `robot-core` 的契约。
//! 线程边界沿 `docs/00_design.md` §1.2：
//!
//! - [`ipc_thread`]：接收命令与控制请求，推进状态机和控制权，发布内部快照。
//! - `rt_thread` / [`serial_io_thread`]：分别执行 `HardRT` / `SerialRT` adapter；
//!   每条串口总线保留唯一所有者、命令流和停止域。
//! - [`io_thread`]：独立轮询 `SoftRT` 相机，不阻塞执行器。
//! - [`pub_thread`]：发布状态、心跳和图像，不执行 adapter I/O。
//! - [`boot_supervisor`]、[`watchdog`]、[`latch_writer`] 与 [`shutdown_mechanism`]：
//!   启动监督、心跳监视、持久化安全状态及有界关闭与回收。
//!
//! `s0_daemon` 负责这些组件的生产接线；当前实现及实机验收边界见
//! `docs/s0_hal_status.md`。

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

// P1C 纯有界输入合同；尚未接 IPC/runtime/writer，不构成真实控制授权。
pub mod base_ingress;
pub mod boot_supervisor;
pub mod camera;
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
pub mod grouped_position;
pub mod hard_rt_ctrl;
pub mod io_thread;
pub mod ipc_pending;
pub(crate) mod ipc_runtime;
pub mod ipc_thread;
// S48 A3d / W1：latch 目标稳定锁、共享名称校验与绑定目录 fd 的数据对象检查。
pub mod latch_lock;
// daemon 启动唯一磁盘 writer，保留持久化义务、进度与真实完成凭证。
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
// daemon 复用句柄槽与门谓词执行有界关闭，真实 join 后才生成完成报告。
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
