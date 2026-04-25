//! Task-30 v9 / M6：`SerialIoCtrlClient` —— `RtClass::SerialRT` 路径的
//! [`ExecutorCtrlClient`] 实现。
//!
//! 对应 `docs/designs/task-30_state_machine_adapter_link.md` §4.5 + v9 §0.5。
//!
//! ## 设计决策：与 HardRT 共用 atomic-packed 协议
//!
//! v8 设计原本让 SerialRT 用 `crossbeam_channel + ArcSwap<Option<...>>` 混合，
//! 区别于 HardRT 的纯 `AtomicU64 packed slot`。**v9 收敛后两者已共用**：
//!
//! - **`pending_normal: AtomicU64`** packed (8 action / 8 arg / 48 seq) ——
//!   server 端 `serial_io_thread` 主循环 swap 出消费，与 HardRT `rt_thread` 同款。
//! - **`safety_latched: AtomicBool`**（v9 §0 #2 修订）—— 同步 atomic store，
//!   client `clear_safety_latch` 立即 visible，与 HardRT 完全对齐。
//! - 其它字段（`policy / safety_state / emergency_action / seq_counter /
//!   last_completed`）也与 HardRT 同款。
//!
//! **结果**：`SerialIoCtrlInner` 在数据布局与 atomic 协议层面**与
//! [`HardRtCtrlInner`](crate::hard_rt_ctrl::HardRtCtrlInner) 一致**。本模块不
//! 重复声明，直接 `pub use` 语义清晰的 alias 名给 SerialRT 路径使用。
//!
//! ## SerialRT 与 HardRT 的实际区别
//!
//! 区别**全在 server 端的主循环行为**（`serial_io_thread` vs `rt_thread`）：
//!
//! | 维度 | HardRT (`rt_thread`) | SerialRT (`serial_io_thread`) |
//! |---|---|---|
//! | 线程优先级 | SCHED_FIFO 70 | 普通优先级 |
//! | tick 周期 | 100 µs - 1 ms | 5-20 ms（受串口 RTT 限） |
//! | safety_stop 立即 confirm | mmap 写完即视为 `TorqueOffConfirmed` | 升 `StopCommanded`，下 tick 调 `OutputSafetyProbe::read_torque_enable_mask` 异步 confirm |
//! | RT 铁律 | 必须满足（无 alloc / 无 lock / 无 blocking syscall） | 不要求（允许串口 read/write 阻塞） |
//! | adapter trait bound | [`HardRtAdapter`] | [`ManipulatorAdapter`] + [`OutputSafetyProbe`] |
//!
//! ## 调用方
//!
//! - **client 端**：`ipc_thread` 持 `Arc<dyn ExecutorCtrlClient>`，**M7 dispatch
//!   by RtClass** 时根据 adapter rt_class 决定指向 HardRT client 还是 SerialIo
//!   client。**当前阶段两者实现完全相同**，任一都可。
//! - **server 端**：[`crate::serial_io_thread::spawn_serial_io_thread`] 用
//!   `Arc<SerialIoCtrlInner>`（= `Arc<HardRtCtrlInner>`）作共享状态。

#![allow(clippy::doc_markdown)]

/// `SerialIoCtrlInner` —— SerialRT 路径的共享 atomic 状态。
///
/// **设计 v9 收敛**：与 [`crate::hard_rt_ctrl::HardRtCtrlInner`] 共用同款
/// atomic-packed 协议（packed `AtomicU64` slot + `safety_latched: AtomicBool`
/// 等）。本 alias 仅作命名层语义说明：M7 dispatch by RtClass 时用
/// `SerialIoCtrlInner` 让代码意图清晰。
///
/// 任何对 inner 字段的语义变更**同时**影响 HardRT 与 SerialRT。
pub type SerialIoCtrlInner = crate::hard_rt_ctrl::HardRtCtrlInner;

/// `SerialIoCtrlClient` —— SerialRT 路径的 [`crate::hard_rt_ctrl::ExecutorCtrlClient`]
/// 实现。同上 alias 模式：与 [`crate::hard_rt_ctrl::HardRtCtrlClient`] 完全一致
/// 的 client 端 atomic 行为；区别在 server 端的 `serial_io_thread` 主循环。
pub type SerialIoCtrlClient = crate::hard_rt_ctrl::HardRtCtrlClient;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use robot_core::executor_ctrl::{
        AdapterNormalReq, ExecutorCtrlClient, NormalReqStatus, OutputPolicy,
    };

    /// alias 验证：`SerialIoCtrlInner` 与 `HardRtCtrlInner` 是同一类型；
    /// `SerialIoCtrlClient` 与 `HardRtCtrlClient` 同一类型。任何 inner 行为
    /// 变更对两路径都生效，避免 server 端 dispatch 时类型分叉。
    #[test]
    fn alias_types_share_inner_with_hard_rt() {
        // 编译期断言：SerialIoCtrlInner 与 HardRtCtrlInner 可互转（同类型）
        let inner: Arc<SerialIoCtrlInner> = Arc::new(SerialIoCtrlInner::new());
        let hard_arc: Arc<crate::hard_rt_ctrl::HardRtCtrlInner> = Arc::clone(&inner);
        // inner + hard_arc → 2 strong refs
        assert_eq!(Arc::strong_count(&hard_arc), 2);
        // SerialIoCtrlClient → HardRtCtrlClient 也可互转（同类型）
        let client = SerialIoCtrlClient::new(inner);
        let hard_c: crate::hard_rt_ctrl::HardRtCtrlClient = client;
        // hard_arc 仍持有 1 个；hard_c.inner() 持有 1 个 → 2 strong refs
        assert_eq!(Arc::strong_count(hard_c.inner()), 2);
    }

    /// SerialRT client 在 client 层面行为与 HardRT 完全一致：
    /// `safety_stop()` 同步切 policy 到 `StopTorque` + latch + drain pending
    /// + 置 emergency_action（finding #1/#4 fix 在两路径都生效）。
    #[test]
    fn serial_io_safety_stop_matches_hard_rt_semantics() {
        let inner = Arc::new(SerialIoCtrlInner::new());
        let client = SerialIoCtrlClient::new(Arc::clone(&inner));

        client.set_policy(OutputPolicy::ActiveForward);
        let _ = client.request_normal(AdapterNormalReq::Enable(false));
        client.safety_stop();

        // 同步生效：policy / latch / pending / emergency_action 全部到位
        assert_eq!(
            OutputPolicy::from_u8(inner.policy.load(Ordering::Acquire)),
            OutputPolicy::StopTorque
        );
        assert!(inner.safety_latched.load(Ordering::Acquire));
        assert_eq!(inner.pending_normal.load(Ordering::Acquire), 0);
        assert_eq!(
            inner.emergency_action.load(Ordering::Acquire),
            robot_core::executor_ctrl::emergency_codes::SAFETY_STOP
        );
    }

    /// SerialRT client 也通过 client 端 latch 拒 `Enable(true)`（finding #3
    /// 双重防护层 1）。
    #[test]
    fn serial_io_client_latch_rejects_enable_true() {
        let inner = Arc::new(SerialIoCtrlInner::new());
        let client = SerialIoCtrlClient::new(Arc::clone(&inner));
        inner.safety_latched.store(true, Ordering::Release);
        match client.request_normal(AdapterNormalReq::Enable(true)) {
            NormalReqStatus::Unsupported(_) => {}
            other => panic!("Enable(true) should be Unsupported when latched, got {other:?}"),
        }
    }
}
