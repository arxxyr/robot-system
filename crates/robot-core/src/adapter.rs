//! Adapter trait 层 + 共享类型（`RtClass` / `AdapterKind` / `AdapterInfo` /
//! `JointStateBuf` / `JointCommandBuf` / `JointCommandView` / `JointLimits` /
//! `ControlMode` / `RtSnapshot`）。
//!
//! 对应 `docs/00_design.md` §4.4 Adapter trait、§4.5 静态注册、§4.6 RT 分级。
//!
//! ### 设计约束
//!
//! - **POD 强制**：`JointStateBuf` / `JointCommandBuf` / `RtSnapshot` / `JointLimits`
//!   全部 `#[derive(Copy)]`。编译期拒绝 `Vec` / `String` 等带 Drop 的字段进入
//!   snapshot / command-slot 载荷，保证 seqlock / `arc_swap` 侧 memcpy 安全（§4.9）。
//! - **`JointCommandView<'a>`** 是借用视图，不存数据：`rt_thread` 从 `LatestCommandReader`
//!   拿到 `Guard<Arc<JointCommandBuf>>` 后按需构造 View，交给 adapter `write_command`。
//! - trait 签名完全对照 `docs/00_design.md` §4.4（9 个方法），不简化。mock 实现把
//!   `connect` / `disconnect` / `enable` / `safety_stop` 做成 `Ok(())` 桩。
//!
//! Step 6 Part 1 只引入类型 + trait；`inventory::submit!` 注册走 §4.5 的宏由
//! 各 adapter crate 自己喂（Part 1 只 mock，不需要 inventory）。

use crate::error::RobotError;

// ============================================================================
// 分类枚举
// ============================================================================

/// Adapter 的 RT 分级。见 `docs/00_design.md` §4.6。
///
/// - `HardRT`：可挂 `s0_rt_thread`，bounded latency < 100 µs，无 malloc / lock，
///   走 `HardRtCtrlClient`（packed `AtomicU64` + RT-only safety 路径）。典型硬件：
///   FPGA 总线、内存映射 I/O、专用控制板。
/// - `SerialRT`：串口 / USB-RS485 / Modbus RTU 等**总线 latency 可证明但单帧 RTT
///   远超 100 µs**（毫秒量级）的硬件。挂 `s0_serial_io_thread`（普通优先级 +
///   `select!` 主循环），走 `SerialIoCtrlClient`（独立 channel + atomic latch）；
///   绝不允许挂 `s0_rt_thread`。典型硬件：Feetech STS3215 串口舵机、Dynamixel、
///   `Modbus` 串口设备。
/// - `SoftRT`：只能挂 `s0_io_thread`；禁挂 `s0_rt_thread` / `s0_pub_thread`。
///   典型硬件：USB camera、网络相机、点云传感器。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RtClass {
    HardRT,
    SerialRT,
    SoftRT,
}

/// Adapter 角色分类。`Manipulator` / `Base` / `Sensor` 三大类，与 §4.4 trait 三元组对齐。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AdapterKind {
    Manipulator,
    Base,
    Sensor,
}

/// 控制模式（§4.4 + §3.5 `FlatBuffers` `ControlMode` 枚举的 Rust 镜像）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ControlMode {
    Position,
    Velocity,
    Torque,
    ServoPosition,
    Stop,
}

// ============================================================================
// AdapterInfo（§4.6）
// ============================================================================

/// Adapter 静态元数据。启动时由 S0 读取，决定挂哪条线程（§4.6）。
///
/// `name` / `kind` / `rt_class` 在 `info()` 里返回；**同一 adapter 实例生命周期内
/// 不得变化**——S0 缓存这份 `AdapterInfo` 并基于它做线程分配决策。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdapterInfo {
    pub name: &'static str,
    pub kind: AdapterKind,
    pub rt_class: RtClass,
    /// 是否依赖厂家 C/C++ SDK（影响安全审计边界：`true` 建议挂 `SoftRT`）。
    pub vendor_sdk: bool,
}

// ============================================================================
// JointStateBuf / JointCommandBuf / JointCommandView
// ============================================================================

/// 最大关节数。超过由应用层拒绝（非 Part 1 范畴）。
///
/// 选 32 的理由：当前 XLeRobot（双臂 ≈ 12 + 夹爪 + 三轮 ≤ 18）、ATOM01 阵
/// （双臂 + 腿 ≈ 24）都在此上限内；POD 大小也受控（3 × 32 × 4 = 384 B + 头部 ≈
/// 396 B，seqlock memcpy 成本 50-80 ns）。
pub const MAX_JOINTS: usize = 32;

/// 关节状态的 POD buffer —— adapter 把当前 state 写进这里，RT 线程读。
///
/// 全 `[f32; MAX_JOINTS]` 固定长度数组；`joint_count` 告诉调用方实际有效位数。
/// `#[derive(Copy)]` 强制无堆字段，seqlock 安全。字段 `stamp_ns` 用
/// `CLOCK_MONOTONIC_RAW` 纪元（见 `crate::time::MonoRawNs`）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct JointStateBuf {
    pub positions: [f32; MAX_JOINTS],
    pub velocities: [f32; MAX_JOINTS],
    pub torques: [f32; MAX_JOINTS],
    pub stamp_ns: u64,
    pub joint_count: u8,
}

/// 关节命令的 POD buffer —— `ipc_thread` 收到 `FlatBuffers` `JointCommand` 后解析填入，
/// 存进 `LatestCommandSlot<JointCommandBuf>` 供 RT 线程读取。
///
/// `rt_thread` 侧不得修改 buffer（`Guard<Arc<T>>` 只读），需要 `write_command` 时
/// 构造 `JointCommandView` 传给 adapter。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct JointCommandBuf {
    pub positions: [f32; MAX_JOINTS],
    pub velocities: [f32; MAX_JOINTS],
    pub torques: [f32; MAX_JOINTS],
    pub mode: ControlModeRepr,
    pub stamp_ns: u64,
    pub deadline_ns: u64,
    pub joint_count: u8,
    /// 命令 seq 号，单调递增；`rt_thread` 判 `CONTROL_LOST` → `ACTIVE` 恢复时用。
    pub seq: u64,
}

/// `ControlMode` 的 `Copy + Default` 版本，存进 POD buffer 用。
///
/// 独立类型而非给 `ControlMode` 加 `Default`，是因为"没有默认的控制模式"
/// 在语义上没意义；但 buffer 初始化需要 `Default`，所以用 u8 repr 带 0 = Stop
/// 的保守默认。
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum ControlModeRepr {
    #[default]
    Stop = 0,
    Position = 1,
    Velocity = 2,
    Torque = 3,
    ServoPosition = 4,
}

impl From<ControlMode> for ControlModeRepr {
    fn from(m: ControlMode) -> Self {
        match m {
            ControlMode::Position => Self::Position,
            ControlMode::Velocity => Self::Velocity,
            ControlMode::Torque => Self::Torque,
            ControlMode::ServoPosition => Self::ServoPosition,
            ControlMode::Stop => Self::Stop,
        }
    }
}

impl From<ControlModeRepr> for ControlMode {
    fn from(r: ControlModeRepr) -> Self {
        match r {
            ControlModeRepr::Position => Self::Position,
            ControlModeRepr::Velocity => Self::Velocity,
            ControlModeRepr::Torque => Self::Torque,
            ControlModeRepr::ServoPosition => Self::ServoPosition,
            ControlModeRepr::Stop => Self::Stop,
        }
    }
}

/// 命令的借用视图 —— adapter `write_command(cmd: &JointCommandView)` 的入参。
///
/// 从 `JointCommandBuf` 构造（零拷贝 slice 切片），也可以从 `FlatBuffers` 直接
/// 构造（Part 2 `ipc_thread` 解析场景）。
#[derive(Clone, Copy, Debug)]
pub struct JointCommandView<'a> {
    pub positions: &'a [f32],
    pub velocities: &'a [f32],
    pub torques: &'a [f32],
    pub mode: ControlMode,
    pub stamp_ns: u64,
    pub deadline_ns: u64,
}

impl JointCommandBuf {
    /// 构造借用视图，`joint_count` 截取数组前缀。
    #[inline]
    #[must_use]
    pub fn as_view(&self) -> JointCommandView<'_> {
        let n = self.joint_count as usize;
        JointCommandView {
            positions: &self.positions[..n],
            velocities: &self.velocities[..n],
            torques: &self.torques[..n],
            mode: self.mode.into(),
            stamp_ns: self.stamp_ns,
            deadline_ns: self.deadline_ns,
        }
    }
}

// ============================================================================
// JointLimits
// ============================================================================

/// 关节硬/软限位。按 joint index 对齐 `JointStateBuf`。
#[derive(Clone, Copy, Debug, Default)]
pub struct JointLimits {
    pub position_min: [f32; MAX_JOINTS],
    pub position_max: [f32; MAX_JOINTS],
    pub velocity_max: [f32; MAX_JOINTS],
    pub torque_max: [f32; MAX_JOINTS],
}

// ============================================================================
// RtSnapshot
// ============================================================================

/// `rt_thread` 每 tick 发布给 `pub_thread` 的 POD 快照。seqlock 载荷。
///
/// ≈ `JointStateBuf` (≈ 396 B) + 16 B = ~412 B，A78AE memcpy 约 60 ns。
#[derive(Clone, Copy, Debug, Default)]
pub struct RtSnapshot {
    pub state: JointStateBuf,
    pub tick_seq: u64,
    pub tick_deadline_ns: u64,
}

// ============================================================================
// ManipulatorAdapter trait（§4.4）
// ============================================================================

/// 机械臂 adapter 合约。参照 `docs/00_design.md` §4.4 原签名（9 个方法）。
///
/// 语义约定：
/// - `connect` / `disconnect` 在**启动期**和**关机期**由主线程调用，不走 RT loop。
/// - `read_state` / `write_command` 是 RT loop 热路径：`HardRT` 实现必须满足
///   §4.9 RT 铁律（无 alloc、无 lock wait、无 blocking syscall、< 100 µs）。
/// - `set_mode` 允许较慢：通常在 state machine `READY → ENABLED` 一次性调用。
/// - `enable(on)` 切电机 torque；`safety_stop` 强制断电（硬件级）。
/// - `limits()` 返回只读引用，实现应在 `connect` 阶段解析 config 并缓存。
pub trait ManipulatorAdapter: Send {
    fn info(&self) -> AdapterInfo;

    fn connect(&mut self) -> Result<(), RobotError>;
    fn disconnect(&mut self) -> Result<(), RobotError>;

    /// RT hot-path。HardRT 实现禁 alloc / lock / blocking。
    ///
    /// # `stamp_ns` 归属（v0.5 候选 E 段(7) 固化）
    ///
    /// - **adapter 不负责 `out.stamp_ns`**（adapter 不持时钟）；实现方只填
    ///   `positions` / `velocities` / `torques` / `joint_count`。
    /// - **调用方（S0 tick 线程）只在读取成功后**才提交时间戳：读进临时副本
    ///   → `Ok(())` 时打 `clock.now()` 并整体提交；`Err` 时**保留上一次成功的
    ///   状态与时间戳**。
    /// - 因此**失败时的输出不得被当作新样本**：即使某实现在返回 `Err` 之前
    ///   已部分写入 `out`，调用方也不会把它并入正式快照。下游（S1 启动锚定 /
    ///   段(5) state cache）据 `stamp_ns` 判新鲜度，失败期必须能正常判 stale。
    fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError>;

    /// RT hot-path。HardRT 实现禁 alloc / lock / blocking。
    fn write_command(&mut self, cmd: &JointCommandView) -> Result<(), RobotError>;

    fn set_mode(&mut self, mode: ControlMode) -> Result<(), RobotError>;
    fn enable(&mut self, on: bool) -> Result<(), RobotError>;
    fn safety_stop(&mut self) -> Result<(), RobotError>;

    fn limits(&self) -> &JointLimits;
}

// ============================================================================
// RtSafeCtrl 子 trait（task-30 v9 §5.5 / M2）
// ============================================================================
//
// 本节大量 doc-comment 用中文描述 RT 路径约束，反引号代价高且可读性下降。
// trait + 三 method block 内 allow doc_markdown，每个公共 API 还是会用反引号
// 标 Rust 类型；实际 lint 由 clippy 在外围 catch。

/// **RT-safe 控制路径**：`rt_thread` 主循环 (SCHED_FIFO 70) 在 hot-path 直接
/// 调用的最小集合。**只有 `RtClass::HardRT` adapter 实现本 trait**；
/// `RtClass::SerialRT` / `RtClass::SoftRT` adapter **禁止实现**
/// (`spawn_rt_thread` 通过 trait bound 在编译期拒)。
///
/// ## 不变量（必须满足）
///
/// 1. **无堆 alloc**：实现里不许 `Vec::push` / `Box::new` / `format!` / 日志
///    格式化 / `Arc::clone/drop` / `FlatBufferBuilder`。
/// 2. **无 blocking syscall**：no read/write 串口、no select/poll、no
///    `std::thread::sleep`。HardRT 实现允许的 syscall 限于内存映射 IO
///    （mmap 页 / spinlock 上的 atomic 读写）。
/// 3. **bounded latency < 100 µs**（典型）：每个方法在最坏情况下完成时间可
///    证明上界。
/// 4. **`safety_stop_rt` 必须 fail-safe**：即使总线 hang / 寄存器写错，方法
///    本身可以返回 `Err`，但 motor torque 必须真正断（硬件级 fail-safe，比如
///    内存映射写 + watchdog）。
/// 5. **可重入安全**：rt_thread 同一 tick 内连续调
///    `safety_stop_rt` + `enable_rt(false)` 必须正确（不依赖外部状态机器）。
///
/// ## 与 [`ManipulatorAdapter`] 的关系
///
/// `ManipulatorAdapter::safety_stop` / `enable` / `set_mode` 是**普通**版本，
/// 允许阻塞 / 分配 / 走串口。`RtSafeCtrl::*_rt` 是**RT 版本**，必须满足上面 5
/// 条。两者通常实现有重合代码，但**不允许内部互相调用**（避免 RT 版本意外
/// 走到非 RT 路径）。
#[allow(clippy::doc_markdown)]
pub trait RtSafeCtrl {
    /// RT-safe 紧急停止。**fire-and-forget 语义**：rt_thread 调完即认为已下发，
    /// 实际硬件状态由 `OutputSafetyProbe`（M3）下 tick 确认。
    ///
    /// HardRT 实现：mmap 写 torque enable register = 0，bounded < 1 µs。
    /// Mock 实现：只改 bool flag，trivially RT-safe。
    ///
    /// # Errors
    ///
    /// 仅在 adapter 内部状态损坏（例如 connect 前调）时返回 `NotConnected` 等；
    /// **绝不**因为"硬件可能已经 stop"返回 Err（fail-safe 语义优先）。
    fn safety_stop_rt(&mut self) -> Result<(), RobotError>;

    /// RT-safe enable / disable torque。HardRT 实现 mmap 写寄存器；Mock 实现
    /// 改 bool flag。
    ///
    /// # Errors
    ///
    /// `NotConnected` / `HardwareFault("safety_stop latched")`（latched 时拒
    /// `enable(true)`）。
    fn enable_rt(&mut self, on: bool) -> Result<(), RobotError>;

    /// RT-safe 切换控制模式。HardRT 通常 mmap 写一个寄存器；Mock 直接改 enum。
    ///
    /// # Errors
    ///
    /// `NotConnected` / `Unsupported`（adapter 不支持该 mode）。
    fn set_mode_rt(&mut self, mode: ControlMode) -> Result<(), RobotError>;
}

/// `Box<Inner>` 对 `RtSafeCtrl` 的透传 blanket impl，对称于下方 `ManipulatorAdapter`
/// 的同款实现。让 inventory factory 返回的 `Box<...>` 直接满足
/// `spawn_rt_thread<A: ManipulatorAdapter + RtSafeCtrl + Send>` 的 trait bound。
impl<Inner> RtSafeCtrl for Box<Inner>
where
    Inner: RtSafeCtrl + ?Sized,
{
    fn safety_stop_rt(&mut self) -> Result<(), RobotError> {
        (**self).safety_stop_rt()
    }
    fn enable_rt(&mut self, on: bool) -> Result<(), RobotError> {
        (**self).enable_rt(on)
    }
    fn set_mode_rt(&mut self, mode: ControlMode) -> Result<(), RobotError> {
        (**self).set_mode_rt(mode)
    }
}

// ============================================================================
// HardRtAdapter 综合 trait（task-30 v9 finding #3 修复）
// ============================================================================

#[allow(clippy::doc_markdown)]
/// **HardRT 路径的 adapter 综合契约**：必须同时实现 [`ManipulatorAdapter`]
/// 与 [`RtSafeCtrl`]。
///
/// `spawn_rt_thread<A: HardRtAdapter + Send + 'static>` 用本 trait 作 bound，
/// **编译期**拒绝未实现 `RtSafeCtrl` 的 adapter（finding #3：之前只靠
/// `RtClass::HardRT` 字符串分类做运行时守门，adapter 写错也能进 RT 路径）。
///
/// 自动 blanket impl：任何 `T: ManipulatorAdapter + RtSafeCtrl` 自动满足。
/// `Box<dyn HardRtAdapter + Send>` 作 trait object 时同样可调 RT 三方法
/// （via 上方两条 `Box<Inner>` 透传 blanket impl）。
///
/// **factory 入口**：`AdapterFactory::build_hard_rt` 返
/// `Box<dyn HardRtAdapter + Send>`；HardRT factory（如 `MockFactory`）必须
/// override 该方法。SerialRT / SoftRT factory 留 default 实现 = 拒绝。
pub trait HardRtAdapter: ManipulatorAdapter + RtSafeCtrl {}

impl<T: ManipulatorAdapter + RtSafeCtrl + ?Sized> HardRtAdapter for T {}

// ============================================================================
// SerialRtAdapter 综合 trait（task-30 v9 / M7 阶段 4 第二步）
// ============================================================================

#[allow(clippy::doc_markdown)]
/// **SerialRT 路径的 adapter 综合契约**：必须同时实现 [`ManipulatorAdapter`]
/// 与 [`crate::executor_ctrl::OutputSafetyProbe`]。
///
/// `spawn_serial_io_thread<A: SerialRtAdapter + Send + 'static>` 用本 trait
/// 作 bound，**编译期**拒绝缺 `OutputSafetyProbe` 的 adapter（serial_io_thread
/// 在 emergency 后必须下 tick 调用 `read_torque_enable_mask` 把
/// `OutputSafetyState` 推进到 `TorqueOffConfirmed` / `AdapterUncertain` /
/// `StopFailed`，缺 probe 则无法 confirm）。
///
/// 自动 blanket impl：任何 `T: ManipulatorAdapter + OutputSafetyProbe` 自动
/// 满足。`Box<dyn SerialRtAdapter + Send>` 作 trait object 时同样可调两组方法
/// （via 上方 `ManipulatorAdapter` blanket impl + `OutputSafetyProbe` 自身的
/// `Box<Inner>` 透传）。
///
/// **factory 入口**：[`crate::factory::AdapterFactory::build_serial_rt`] 返
/// `Box<dyn SerialRtAdapter + Send>`；SerialRT factory（如 `FeetechFactory`）
/// 必须 override 该方法。HardRT / SoftRT factory 留 default 实现 = 拒绝。
pub trait SerialRtAdapter: ManipulatorAdapter + crate::executor_ctrl::OutputSafetyProbe {}

impl<T: ManipulatorAdapter + crate::executor_ctrl::OutputSafetyProbe + ?Sized> SerialRtAdapter
    for T
{
}

/// `Box<dyn ManipulatorAdapter>` 透传 blanket impl —— Step 7 task 27b 加。
///
/// 让 inventory factory 返回的 `Box<dyn ManipulatorAdapter + Send>` 直接满足
/// `spawn_rt_thread<A: ManipulatorAdapter + Send + 'static>` 的泛型约束，无需在
/// 调用方再包一层 newtype。零运行时开销（每方法多一次 vtable 间接 + Box deref，
/// 与显式 dyn 调用等价）。
impl<Inner> ManipulatorAdapter for Box<Inner>
where
    Inner: ManipulatorAdapter + ?Sized,
{
    fn info(&self) -> AdapterInfo {
        (**self).info()
    }
    fn connect(&mut self) -> Result<(), RobotError> {
        (**self).connect()
    }
    fn disconnect(&mut self) -> Result<(), RobotError> {
        (**self).disconnect()
    }
    fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError> {
        (**self).read_state(out)
    }
    fn write_command(&mut self, cmd: &JointCommandView) -> Result<(), RobotError> {
        (**self).write_command(cmd)
    }
    fn set_mode(&mut self, mode: ControlMode) -> Result<(), RobotError> {
        (**self).set_mode(mode)
    }
    fn enable(&mut self, on: bool) -> Result<(), RobotError> {
        (**self).enable(on)
    }
    fn safety_stop(&mut self) -> Result<(), RobotError> {
        (**self).safety_stop()
    }
    fn limits(&self) -> &JointLimits {
        (**self).limits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_structs_are_copy_and_default() {
        // 编译期校验 Copy / Default 能同时 derive —— 若引入 Vec/String 这行会失败。
        fn assert_copy<T: Copy>() {}
        fn assert_default<T: Default>() {}
        assert_copy::<JointStateBuf>();
        assert_copy::<JointCommandBuf>();
        assert_copy::<JointLimits>();
        assert_copy::<RtSnapshot>();
        assert_default::<JointStateBuf>();
        assert_default::<JointCommandBuf>();
        assert_default::<JointLimits>();
        assert_default::<RtSnapshot>();
    }

    #[test]
    fn joint_command_buf_view_truncates_by_joint_count() {
        let mut positions = [0.0_f32; MAX_JOINTS];
        positions[0] = 1.0;
        positions[1] = 2.0;
        positions[2] = 3.0;
        positions[3] = 999.0; // 应被 joint_count 截断
        let buf = JointCommandBuf {
            joint_count: 3,
            positions,
            ..JointCommandBuf::default()
        };
        let view = buf.as_view();
        assert_eq!(view.positions, &[1.0, 2.0, 3.0]);
        assert_eq!(view.velocities.len(), 3);
    }

    #[test]
    fn control_mode_round_trip() {
        for m in [
            ControlMode::Position,
            ControlMode::Velocity,
            ControlMode::Torque,
            ControlMode::ServoPosition,
            ControlMode::Stop,
        ] {
            let r: ControlModeRepr = m.into();
            let m2: ControlMode = r.into();
            assert_eq!(m, m2);
        }
    }

    #[test]
    fn rt_snapshot_size_under_1kb() {
        // §4.9 约束：seqlock 载荷要尽量小。留一点余量给未来加 u32/u16 字段。
        assert!(
            std::mem::size_of::<RtSnapshot>() < 1024,
            "RtSnapshot too large: {}",
            std::mem::size_of::<RtSnapshot>()
        );
    }

    #[test]
    fn adapter_info_fields_are_copy() {
        let info = AdapterInfo {
            name: "mock",
            kind: AdapterKind::Manipulator,
            rt_class: RtClass::HardRT,
            vendor_sdk: false,
        };
        let info2 = info;
        assert_eq!(info, info2);
    }

    #[test]
    fn rt_class_three_variants_are_distinct() {
        // pre-1a：SerialRT 与 HardRT / SoftRT 在 PartialEq / Hash / Debug 维度
        // 完全独立。后续 s0_daemon dispatch_by_rt_class 会基于 PartialEq 路由。
        let v = [RtClass::HardRT, RtClass::SerialRT, RtClass::SoftRT];
        for (i, a) in v.iter().enumerate() {
            for (j, b) in v.iter().enumerate() {
                assert_eq!(a == b, i == j, "{a:?} vs {b:?}");
            }
        }
        // 编译期 Copy 校验
        let a = RtClass::SerialRT;
        let b = a;
        assert_eq!(a, b);
    }
}
