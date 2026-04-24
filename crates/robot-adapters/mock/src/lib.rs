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
    AdapterInfo, AdapterKind, ControlMode, ControlModeRepr, JointCommandView, JointLimits,
    JointStateBuf, MAX_JOINTS, ManipulatorAdapter, RtClass,
};
use robot_core::error::RobotError;
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
}

inventory::submit! { &MockFactory as &dyn AdapterFactory }

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
}
