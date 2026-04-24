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

/// Adapter 的 RT 分级。见 §4.6。
///
/// - `HardRT`：可挂 `s0_rt_thread`，bounded latency < 100 µs，无 malloc / lock。
/// - `SoftRT`：只能挂 `s0_io_thread`；禁挂 `s0_rt_thread` / `s0_pub_thread`。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RtClass {
    HardRT,
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
    fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError>;

    /// RT hot-path。HardRT 实现禁 alloc / lock / blocking。
    fn write_command(&mut self, cmd: &JointCommandView) -> Result<(), RobotError>;

    fn set_mode(&mut self, mode: ControlMode) -> Result<(), RobotError>;
    fn enable(&mut self, on: bool) -> Result<(), RobotError>;
    fn safety_stop(&mut self) -> Result<(), RobotError>;

    fn limits(&self) -> &JointLimits;
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
}
