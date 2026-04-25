//! Mock `HardRT` adapter —— S0 最小闭环的纯内存关节模型。
//!
//! 对应 `docs/00_design.md` §4.4 `ManipulatorAdapter` trait、§4.6 RT 分级
//! （声明 `HardRT`）+ `docs/01_implementation_steps.md` Step 6 交付清单。
//!
//! ### 物理模型（Part 1）
//!
//! **位置 echo**：`write_command` 立即把 `cmd.positions` 拷到 `state.positions`，
//! 不做任何物理动力学（惯性、阻尼、PID）。目的是**让数据流闭环可测**——
//! Part 1 Step 6 baseline 不需要真实物理，`rt_thread` tick 看到 publish 的 command
//! 就应该下一 tick 从 state 读出同值。Part 2 如果 baseline 逼近门槛时需要更真实
//! 的动态响应来暴露控制路径延迟，再加一阶低通或惯性扩展。
//!
//! ### RT 铁律合规
//!
//! - 所有 field 都是 `[f32; MAX_JOINTS]` / `u64` / `u8`（POD）。
//! - `read_state` / `write_command` 走 `copy_from_slice` + scalar 赋值，不分配堆。
//! - `connect` / `disconnect` / `enable` / `safety_stop` 在非 RT 启动 / 关机期调用，
//!   实现中只改 bool flag，不允许 `println!` 等 alloc 操作（保持 release profile 下
//!   启动期也干净，便于 Part 2 `assert_no_alloc` 整体覆盖）。

#![forbid(unused_must_use)]

use robot_core::adapter::{
    AdapterInfo, AdapterKind, ControlMode, ControlModeRepr, HardRtAdapter, JointCommandView,
    JointLimits, JointStateBuf, MAX_JOINTS, ManipulatorAdapter, RtClass, RtSafeCtrl,
    SerialRtAdapter,
};
use robot_core::error::RobotError;
use robot_core::executor_ctrl::OutputSafetyProbe;
use robot_core::factory::{AdapterBuildContext, AdapterFactory};

/// Mock `HardRT` 机械臂 adapter。默认 `joint_count = 0`，使用前通过
/// `MockHardRTAdapter::new(joint_count, limits)` 配置关节数 + 限位；也可以用
/// `MockHardRTAdapter::with_unlimited_limits(joint_count)` 快速拿一个
/// "几乎不限位" 的实例做 smoke test。
pub struct MockHardRTAdapter {
    state: JointStateBuf,
    limits: JointLimits,
    mode: ControlModeRepr,
    connected: bool,
    enabled: bool,
    safety_stopped: bool,
}

impl MockHardRTAdapter {
    /// 构造一个关节数为 `joint_count` 的 mock adapter。
    ///
    /// # Panics
    ///
    /// 当 `joint_count > MAX_JOINTS` 时 panic（调用方自查）。
    #[must_use]
    pub fn new(joint_count: u8, limits: JointLimits) -> Self {
        assert!(
            (joint_count as usize) <= MAX_JOINTS,
            "joint_count {joint_count} exceeds MAX_JOINTS={MAX_JOINTS}"
        );
        Self {
            state: JointStateBuf {
                joint_count,
                ..JointStateBuf::default()
            },
            limits,
            mode: ControlModeRepr::Stop,
            connected: false,
            enabled: false,
            safety_stopped: false,
        }
    }

    /// **task-30 v9 / `SerialRT` E2E 修订**：清 mock 的持久 `safety_stopped` flag。
    ///
    /// **仅供 test / `SerialRT` wrap 内部使用**：模拟真 `SerialRT` 硬件
    /// （Feetech）的 `ClearEStop` 语义 —— `safety_stop` 是 broadcast disable，
    /// 不持久 latch；操作员 `ClearEStop` 后再 `enable(true)` 应该成功。mock
    /// 内部为单测便利保留持久 latch；E2E 测试用 `MockSerialRTAdapter` wrap，
    /// 在 `enable(true)` 之前先调本 API 清 latch，行为更接近真硬件。
    pub fn reset_safety_latch_for_test(&mut self) {
        self.safety_stopped = false;
    }

    /// 便利构造：`joint_count` 关节，位置限位均为 `[-pi, pi]`，速度 / 力矩限位
    /// 都用 `1e6`（即"几乎不限"），适合 smoke test。
    #[must_use]
    pub fn with_unlimited_limits(joint_count: u8) -> Self {
        const PI: f32 = std::f32::consts::PI;
        let mut limits = JointLimits::default();
        for i in 0..MAX_JOINTS {
            limits.position_min[i] = -PI;
            limits.position_max[i] = PI;
            limits.velocity_max[i] = 1.0e6;
            limits.torque_max[i] = 1.0e6;
        }
        Self::new(joint_count, limits)
    }
}

impl ManipulatorAdapter for MockHardRTAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            // 与 [`MockFactory::name`] 字面对齐 + `configs/<robot>.toml` 里
            // `[[hardware]] adapter = "mock_hard_rt"` 一致。
            name: "mock_hard_rt",
            kind: AdapterKind::Manipulator,
            rt_class: RtClass::HardRT,
            vendor_sdk: false,
        }
    }

    fn connect(&mut self) -> Result<(), RobotError> {
        if self.safety_stopped {
            return Err(RobotError::HardwareFault("safety_stop latched"));
        }
        self.connected = true;
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), RobotError> {
        self.connected = false;
        self.enabled = false;
        Ok(())
    }

    fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError> {
        if !self.connected {
            return Err(RobotError::NotConnected);
        }
        // POD memcpy；`JointStateBuf: Copy` 保证零分配。
        *out = self.state;
        Ok(())
    }

    fn write_command(&mut self, cmd: &JointCommandView) -> Result<(), RobotError> {
        if !self.connected {
            return Err(RobotError::NotConnected);
        }
        if !self.enabled {
            // 未 enable 时静默吞掉命令（不报错，state 不更新）——对应 S0 state machine
            // SAFE/READY 下 rt_thread 仍在跑但命令不应生效。Part 2 rt_thread 会根据
            // state machine 决定是否调 write_command。
            return Ok(());
        }
        // 位置 echo：直接拷到 state；只拷 `joint_count` 前缀。
        let n = cmd.positions.len().min(self.state.joint_count as usize);
        self.state.positions[..n].copy_from_slice(&cmd.positions[..n]);
        // 速度 / 力矩同样 echo（长度可能不等，按各自长度取 min）。
        let nv = cmd.velocities.len().min(self.state.joint_count as usize);
        self.state.velocities[..nv].copy_from_slice(&cmd.velocities[..nv]);
        let nt = cmd.torques.len().min(self.state.joint_count as usize);
        self.state.torques[..nt].copy_from_slice(&cmd.torques[..nt]);
        self.state.stamp_ns = cmd.stamp_ns;
        Ok(())
    }

    fn set_mode(&mut self, mode: ControlMode) -> Result<(), RobotError> {
        self.mode = mode.into();
        Ok(())
    }

    fn enable(&mut self, on: bool) -> Result<(), RobotError> {
        if !self.connected {
            return Err(RobotError::NotConnected);
        }
        if self.safety_stopped && on {
            return Err(RobotError::HardwareFault("safety_stop latched"));
        }
        self.enabled = on;
        Ok(())
    }

    fn safety_stop(&mut self) -> Result<(), RobotError> {
        self.safety_stopped = true;
        self.enabled = false;
        // 所有字段归零 —— 物理上等价"输出切断 + 保持 0 位置"。
        self.state.positions = [0.0; MAX_JOINTS];
        self.state.velocities = [0.0; MAX_JOINTS];
        self.state.torques = [0.0; MAX_JOINTS];
        Ok(())
    }

    fn limits(&self) -> &JointLimits {
        &self.limits
    }
}

// ============================================================================
// RtSafeCtrl（task-30 v9 / M2）
// ============================================================================
//
// 三方法都是改 bool / enum / 数组 = 0，**无堆 alloc / 无 syscall / 无 lock**。
// 测试通过 `assert_no_alloc::assert_no_alloc(|| ...)` 锁死这条契约。
//
// `write_command(mode=Stop)` 合约（M2 doc）：
// rt_thread 在 OutputPolicy=StopTorque 下会下发 ControlMode::Stop 的命令。
// MockHardRTAdapter::write_command 当前实现忽略 mode 字段，照搬 positions/
// velocities/torques。这意味着上游必须**自己**清零向量；M5 rt_thread 会做这件
// 事（在 cmd 上把 ControlMode::Stop 投影成全 0 命令再 write）。本 trait 不
// 改变 write_command 行为。

// task-30 v9 / M2：`RtSafeCtrl` 是独立 marker trait，方法体与 `ManipulatorAdapter`
// 的 `enable`/`safety_stop`/`set_mode` 等价（mock 全是 in-memory 操作 → 都 RT-safe）。
// 单独写一遍而不是转发，是因为 RtSafeCtrl 是"承诺 RT 合规"的契约：
// HardRT adapter 实现这个 trait = 编译期声明；assert_no_alloc 单测验证之。
impl RtSafeCtrl for MockHardRTAdapter {
    fn safety_stop_rt(&mut self) -> Result<(), RobotError> {
        self.safety_stopped = true;
        self.enabled = false;
        self.state.positions = [0.0; MAX_JOINTS];
        self.state.velocities = [0.0; MAX_JOINTS];
        self.state.torques = [0.0; MAX_JOINTS];
        Ok(())
    }

    fn enable_rt(&mut self, on: bool) -> Result<(), RobotError> {
        if !self.connected {
            return Err(RobotError::NotConnected);
        }
        if self.safety_stopped && on {
            return Err(RobotError::HardwareFault("safety_stop latched"));
        }
        self.enabled = on;
        Ok(())
    }

    fn set_mode_rt(&mut self, mode: ControlMode) -> Result<(), RobotError> {
        self.mode = mode.into();
        Ok(())
    }
}

// ============================================================================
// OutputSafetyProbe（task-30 v9 / M3）—— mock 实现 = 镜像 enabled flag
// ============================================================================
//
// HardRT runtime 不调本 trait（rt_thread 用 mmap 写寄存器即时生效）；mock 仍
// 实现以便单测把 SerialRT 路径"假装"在 HardRT 上跑。

impl OutputSafetyProbe for MockHardRTAdapter {
    fn read_torque_enable_mask(&mut self, out: &mut [bool]) -> Result<(), RobotError> {
        let n = self.state.joint_count as usize;
        if out.len() < n {
            return Err(RobotError::InvalidParameter(
                "OutputSafetyProbe out len < joint_count",
            ));
        }
        // Mock 只跟踪整体 enabled flag，每个 motor 回同一 bool。真硬件每 motor
        // 独立读 Torque_Enable 寄存器才真正反映"部分 motor 没断电"的失败模式。
        let val = self.enabled && !self.safety_stopped;
        for slot in &mut out[..n] {
            *slot = val;
        }
        Ok(())
    }

    fn joint_count(&self) -> usize {
        self.state.joint_count as usize
    }
}

// ============================================================================
// Factory（Step 7 task 27b inventory 自注册）
// ============================================================================

/// `MockHardRTAdapter` 的 inventory 工厂。
///
/// `[[hardware]] adapter = "mock_hard_rt"` 时被 `s0_daemon` 命中。
/// build 出的 adapter 用 `JointLimits::default()`（全 0），调用方按需 reset。
pub struct MockFactory;

impl AdapterFactory for MockFactory {
    fn name(&self) -> &'static str {
        "mock_hard_rt"
    }

    fn build(
        &self,
        ctx: &AdapterBuildContext<'_>,
    ) -> Result<Box<dyn ManipulatorAdapter + Send>, RobotError> {
        Ok(build_concrete(ctx)?)
    }

    /// task-30 v9 finding #3：`HardRT` 专用入口，返回综合 trait object，
    /// **编译期保证** `RtSafeCtrl` 已实现。`s0_daemon` `HardRT` 分支调本方法
    /// 而不是 `build`。
    fn build_hard_rt(
        &self,
        ctx: &AdapterBuildContext<'_>,
    ) -> Result<Box<dyn HardRtAdapter + Send>, RobotError> {
        let mock = build_concrete(ctx)?;
        Ok(mock as Box<dyn HardRtAdapter + Send>)
    }
}

/// 构造具体类型 `Box<MockHardRTAdapter>`，给 `build` / `build_hard_rt`
/// 共享。具体类型同时实现 `ManipulatorAdapter + RtSafeCtrl`，可向上
/// 转成两个 trait object 之一。
fn build_concrete(ctx: &AdapterBuildContext<'_>) -> Result<Box<MockHardRTAdapter>, RobotError> {
    let n = ctx.joint_names.len();
    if n > MAX_JOINTS {
        return Err(RobotError::InvalidParameter(
            "mock joint_count > MAX_JOINTS",
        ));
    }
    let joint_count =
        u8::try_from(n).map_err(|_| RobotError::InvalidParameter("mock joint_count > 255"))?;
    Ok(Box::new(MockHardRTAdapter::with_unlimited_limits(
        joint_count,
    )))
}

inventory::submit! { &MockFactory as &dyn AdapterFactory }

// ============================================================================
// MockSerialRTAdapter + MockSerialRTFactory（task-30 v9 / SerialRT E2E）
// ============================================================================
//
// SerialRT 路径的 mock：包一份 `MockHardRTAdapter`（复用 ManipulatorAdapter +
// OutputSafetyProbe 的实现）作为内部状态，但**不**实现 `RtSafeCtrl`，
// 编译期保证它**只能**走 `RtClass::SerialRT` 路径（spawn_serial_io_thread）。
// `info().rt_class` 报告 SerialRT；`info().name` 报告 `mock_serial_rt`。
//
// 用途：robot-s0 的 SerialRT 端到端集成测试 + s0_daemon dispatch 锁测，
// 不依赖真硬件即可验证 build_serial_rt → spawn_serial_io_thread 全链路。

/// `RtClass::SerialRT` 的 mock adapter。
pub struct MockSerialRTAdapter {
    inner: MockHardRTAdapter,
}

impl MockSerialRTAdapter {
    /// 便利构造：与 `MockHardRTAdapter::with_unlimited_limits` 一致，但
    /// 顶层 `info().rt_class = SerialRT`。
    #[must_use]
    pub fn with_unlimited_limits(joint_count: u8) -> Self {
        Self {
            inner: MockHardRTAdapter::with_unlimited_limits(joint_count),
        }
    }
}

impl ManipulatorAdapter for MockSerialRTAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "mock_serial_rt",
            kind: AdapterKind::Manipulator,
            rt_class: RtClass::SerialRT,
            vendor_sdk: false,
        }
    }
    fn connect(&mut self) -> Result<(), RobotError> {
        self.inner.connect()
    }
    fn disconnect(&mut self) -> Result<(), RobotError> {
        self.inner.disconnect()
    }
    fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError> {
        self.inner.read_state(out)
    }
    fn write_command(&mut self, cmd: &JointCommandView) -> Result<(), RobotError> {
        self.inner.write_command(cmd)
    }
    fn set_mode(&mut self, mode: ControlMode) -> Result<(), RobotError> {
        self.inner.set_mode(mode)
    }
    fn enable(&mut self, on: bool) -> Result<(), RobotError> {
        // **task-30 v9 / SerialRT E2E 修订**：模拟真 SerialRT (Feetech) 硬件
        // 的 ClearEStop 语义 —— `safety_stop` 只 broadcast disable
        // Torque_Enable，不持久 latch；下次 `enable(true)` 是新的 broadcast
        // Torque_Enable=1，应当成功（对应物理上操作员清完 EStop 后重新使能）。
        // inner（`MockHardRTAdapter`）为单测便利保留持久 `safety_stopped`
        // 粘性 latch；这里在 enable(true) 之前清掉，让 wrap 的对外行为更
        // 接近真硬件。
        if on {
            self.inner.reset_safety_latch_for_test();
        }
        self.inner.enable(on)
    }
    fn safety_stop(&mut self) -> Result<(), RobotError> {
        self.inner.safety_stop()
    }
    fn limits(&self) -> &JointLimits {
        self.inner.limits()
    }
}

impl OutputSafetyProbe for MockSerialRTAdapter {
    fn read_torque_enable_mask(&mut self, out: &mut [bool]) -> Result<(), RobotError> {
        self.inner.read_torque_enable_mask(out)
    }
    fn joint_count(&self) -> usize {
        self.inner.joint_count()
    }
}

// ManipulatorAdapter + OutputSafetyProbe → blanket impl 自动满足 SerialRtAdapter。

/// `MockSerialRTAdapter` 的 inventory 工厂。
///
/// `[[hardware]] adapter = "mock_serial_rt"` 时被 `s0_daemon` 命中。
/// 与 `MockFactory` 的关键区别：**不** override `build_hard_rt`（保持
/// default = `Unsupported`），改 override `build_serial_rt`。`s0_daemon`
/// 的三段 dispatch 自然落到 `SerialRT` 路径。
pub struct MockSerialRTFactory;

impl AdapterFactory for MockSerialRTFactory {
    fn name(&self) -> &'static str {
        "mock_serial_rt"
    }

    fn build(
        &self,
        ctx: &AdapterBuildContext<'_>,
    ) -> Result<Box<dyn ManipulatorAdapter + Send>, RobotError> {
        Ok(build_serial_concrete(ctx)?)
    }

    fn build_serial_rt(
        &self,
        ctx: &AdapterBuildContext<'_>,
    ) -> Result<Box<dyn SerialRtAdapter + Send>, RobotError> {
        let adapter = build_serial_concrete(ctx)?;
        Ok(adapter as Box<dyn SerialRtAdapter + Send>)
    }
}

fn build_serial_concrete(
    ctx: &AdapterBuildContext<'_>,
) -> Result<Box<MockSerialRTAdapter>, RobotError> {
    let n = ctx.joint_names.len();
    if n > MAX_JOINTS {
        return Err(RobotError::InvalidParameter(
            "mock_serial_rt joint_count > MAX_JOINTS",
        ));
    }
    let joint_count = u8::try_from(n)
        .map_err(|_| RobotError::InvalidParameter("mock_serial_rt joint_count > 255"))?;
    Ok(Box::new(MockSerialRTAdapter::with_unlimited_limits(
        joint_count,
    )))
}

inventory::submit! { &MockSerialRTFactory as &dyn AdapterFactory }

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_view<'a>(
        positions: &'a [f32],
        velocities: &'a [f32],
        torques: &'a [f32],
    ) -> JointCommandView<'a> {
        JointCommandView {
            positions,
            velocities,
            torques,
            mode: ControlMode::Position,
            stamp_ns: 1_000_000,
            deadline_ns: 2_000_000,
        }
    }

    #[test]
    fn info_reports_hardrt_manipulator() {
        let a = MockHardRTAdapter::with_unlimited_limits(3);
        let info = a.info();
        assert_eq!(info.rt_class, RtClass::HardRT);
        assert_eq!(info.kind, AdapterKind::Manipulator);
        assert_eq!(info.name, "mock_hard_rt");
        assert!(!info.vendor_sdk);
    }

    #[test]
    fn read_state_before_connect_errors() {
        let mut a = MockHardRTAdapter::with_unlimited_limits(3);
        let mut buf = JointStateBuf::default();
        assert_eq!(
            a.read_state(&mut buf).unwrap_err(),
            RobotError::NotConnected
        );
    }

    #[test]
    fn write_command_position_echo() {
        let mut a = MockHardRTAdapter::with_unlimited_limits(3);
        a.connect().unwrap();
        a.enable(true).unwrap();
        let cmd = sample_view(&[1.0, 2.0, 3.0], &[0.1, 0.2, 0.3], &[]);
        a.write_command(&cmd).unwrap();
        let mut out = JointStateBuf::default();
        a.read_state(&mut out).unwrap();
        assert_eq!(&out.positions[..3], &[1.0, 2.0, 3.0]);
        assert_eq!(&out.velocities[..3], &[0.1, 0.2, 0.3]);
        assert_eq!(out.stamp_ns, 1_000_000);
    }

    #[test]
    fn write_command_disabled_is_noop() {
        let mut a = MockHardRTAdapter::with_unlimited_limits(2);
        a.connect().unwrap();
        // 不 enable
        let cmd = sample_view(&[9.0, 9.0], &[], &[]);
        a.write_command(&cmd).unwrap();
        let mut out = JointStateBuf::default();
        a.read_state(&mut out).unwrap();
        assert_eq!(
            &out.positions[..2],
            &[0.0, 0.0],
            "disabled adapter should not update state"
        );
    }

    #[test]
    fn safety_stop_latches_and_zeros_state() {
        let mut a = MockHardRTAdapter::with_unlimited_limits(2);
        a.connect().unwrap();
        a.enable(true).unwrap();
        let cmd = sample_view(&[5.0, 5.0], &[], &[]);
        a.write_command(&cmd).unwrap();
        a.safety_stop().unwrap();
        let mut out = JointStateBuf::default();
        a.read_state(&mut out).unwrap();
        assert_eq!(&out.positions[..2], &[0.0, 0.0]);
        // 尝试再 enable 会失败（latched）
        assert_eq!(
            a.enable(true).unwrap_err(),
            RobotError::HardwareFault("safety_stop latched")
        );
    }

    #[test]
    fn set_mode_stores_repr() {
        let mut a = MockHardRTAdapter::with_unlimited_limits(1);
        a.set_mode(ControlMode::Velocity).unwrap();
        assert_eq!(a.mode, ControlModeRepr::Velocity);
    }

    #[test]
    fn disconnect_clears_enabled() {
        let mut a = MockHardRTAdapter::with_unlimited_limits(1);
        a.connect().unwrap();
        a.enable(true).unwrap();
        a.disconnect().unwrap();
        // 再 enable 应返回 NotConnected（必须先 connect）
        assert_eq!(a.enable(true).unwrap_err(), RobotError::NotConnected);
    }

    #[test]
    #[should_panic(expected = "exceeds MAX_JOINTS")]
    fn constructor_rejects_overflow() {
        let _ = MockHardRTAdapter::new(200, JointLimits::default());
    }

    // ---- task-30 v9 / M2：RtSafeCtrl 行为 + RT 安全性 ----

    #[test]
    fn rt_safety_stop_latches_and_zeros_state() {
        let mut a = MockHardRTAdapter::with_unlimited_limits(2);
        a.connect().unwrap();
        a.enable(true).unwrap();
        let cmd = sample_view(&[5.0, 5.0], &[], &[]);
        a.write_command(&cmd).unwrap();
        a.safety_stop_rt().unwrap();
        let mut out = JointStateBuf::default();
        a.read_state(&mut out).unwrap();
        assert_eq!(&out.positions[..2], &[0.0, 0.0]);
        // safety_stopped 已 latch
        assert_eq!(
            a.enable_rt(true).unwrap_err(),
            RobotError::HardwareFault("safety_stop latched")
        );
    }

    #[test]
    fn rt_enable_requires_connected() {
        let mut a = MockHardRTAdapter::with_unlimited_limits(1);
        // 未 connect → enable_rt 失败
        assert_eq!(a.enable_rt(true).unwrap_err(), RobotError::NotConnected);
        a.connect().unwrap();
        a.enable_rt(true).unwrap();
        a.enable_rt(false).unwrap();
    }

    #[test]
    fn rt_set_mode_supports_all_five_modes() {
        let mut a = MockHardRTAdapter::with_unlimited_limits(1);
        for m in [
            ControlMode::Position,
            ControlMode::Velocity,
            ControlMode::Torque,
            ControlMode::ServoPosition,
            ControlMode::Stop,
        ] {
            a.set_mode_rt(m).unwrap();
            let r: ControlMode = a.mode.into();
            assert_eq!(r, m);
        }
    }

    /// **task-30 v9 / M2 关键测试**：把 `RtSafeCtrl` 三方法塞进
    /// `assert_no_alloc::assert_no_alloc(|| ...)` scope；scope 内任何 alloc =
    /// panic。证明 Mock 实现**真满足"RT 路径 0 alloc"契约**。
    ///
    /// `HardRT` adapter 加进 `spawn_rt_thread` 前必须先过这一关；后续真硬件
    /// adapter（v0.4+ FPGA / EtherCAT）实现 `RtSafeCtrl` 时**必须**复制本
    /// pattern 加同款单测。
    #[test]
    fn rt_safe_ctrl_methods_are_alloc_free() {
        use assert_no_alloc::assert_no_alloc;

        let mut a = MockHardRTAdapter::with_unlimited_limits(u8::try_from(MAX_JOINTS).unwrap());
        a.connect().unwrap();

        // safety_stop_rt
        assert_no_alloc(|| {
            let _ = a.safety_stop_rt();
        });

        // 清 latch（safety_stop 之后 enable_rt(true) 会被拒；做一遍 reset 让后续
        // enable 能跑）。Mock 没有公开 clear_latch，直接重置 latched 标志测试用。
        a.safety_stopped = false;

        // enable_rt(false) / enable_rt(true)
        assert_no_alloc(|| {
            let _ = a.enable_rt(false);
        });
        assert_no_alloc(|| {
            let _ = a.enable_rt(true);
        });

        // set_mode_rt 全 5 mode
        for m in [
            ControlMode::Position,
            ControlMode::Velocity,
            ControlMode::Torque,
            ControlMode::ServoPosition,
            ControlMode::Stop,
        ] {
            assert_no_alloc(|| {
                let _ = a.set_mode_rt(m);
            });
        }
    }

    // ---- task-30 v9 / M3：OutputSafetyProbe ----

    #[test]
    fn output_safety_probe_mirrors_enabled_flag() {
        let mut a = MockHardRTAdapter::with_unlimited_limits(3);
        a.connect().unwrap();
        let mut mask = [true; 8];
        // 未 enable → 全 false
        a.read_torque_enable_mask(&mut mask).unwrap();
        assert_eq!(&mask[..3], &[false, false, false]);
        // 后缀（>= joint_count）调用方不关心，无契约
        a.enable_rt(true).unwrap();
        a.read_torque_enable_mask(&mut mask).unwrap();
        assert_eq!(&mask[..3], &[true, true, true]);
        // safety_stop 后 → 全 false
        a.safety_stop_rt().unwrap();
        a.read_torque_enable_mask(&mut mask).unwrap();
        assert_eq!(&mask[..3], &[false, false, false]);
    }

    #[test]
    fn output_safety_probe_short_out_rejected() {
        let mut a = MockHardRTAdapter::with_unlimited_limits(5);
        a.connect().unwrap();
        let mut mask = [false; 3]; // 太短
        let r = a.read_torque_enable_mask(&mut mask);
        assert_eq!(
            r.unwrap_err(),
            RobotError::InvalidParameter("OutputSafetyProbe out len < joint_count")
        );
    }

    #[test]
    fn output_safety_probe_joint_count_reports() {
        let a = MockHardRTAdapter::with_unlimited_limits(7);
        assert_eq!(a.joint_count(), 7);
    }

    /// `confirm_torque_off` 路径在 `serial_io_thread` 跑（普通优先级，非 RT），但
    /// 仍要求**不分配堆**：传入预分配 mask，实现内部不应再 alloc。
    #[test]
    fn output_safety_probe_is_alloc_free() {
        use assert_no_alloc::assert_no_alloc;

        let mut a = MockHardRTAdapter::with_unlimited_limits(u8::try_from(MAX_JOINTS).unwrap());
        a.connect().unwrap();
        a.enable_rt(true).unwrap();
        let mut mask = [false; MAX_JOINTS];

        assert_no_alloc(|| {
            let _ = a.read_torque_enable_mask(&mut mask);
        });
    }

    /// `read_state` / `write_command` 也是 RT 热路径；同标准 alloc-free。
    /// （这是 `ManipulatorAdapter` trait 早就规定的，本测试 lock 住 mock 的
    /// 实现不漏 alloc，未来改 mock 时也会 fail-fast。）
    #[test]
    fn read_state_and_write_command_are_alloc_free() {
        use assert_no_alloc::assert_no_alloc;

        let mut a = MockHardRTAdapter::with_unlimited_limits(8);
        a.connect().unwrap();
        a.enable_rt(true).unwrap();
        let pos = [0.1_f32; 8];
        let vel = [0.0_f32; 8];
        let tau = [0.0_f32; 8];
        let cmd = sample_view(&pos, &vel, &tau);
        let mut buf = JointStateBuf::default();

        assert_no_alloc(|| {
            let _ = a.write_command(&cmd);
            let _ = a.read_state(&mut buf);
        });
    }
}
