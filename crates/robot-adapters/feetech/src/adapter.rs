//! [`FeetechAdapter`] —— 实现 `robot_core::adapter::ManipulatorAdapter` trait。
//!
//! ## v0.2 范围
//!
//! - 同步实现：`read_state` / `write_command` 在调用线程内完成 wire RTT
//!   （sync_read / sync_write）。
//! - **不可直接挂 `s0_rt_thread`**：串口 RTT ms 量级 → 违反 §4.9 RT 铁律的
//!   "< 100 µs / no blocking" 约束；adapter info 仍标 `RtClass::HardRT`，是按
//!   "目标分类"标的（task 28 上 IO 线程 + atomic snapshot 后才是真 HardRT）。
//! - **校准 / homing offset**：v0.2 不内置：raw ↔ rad 转换全部走线性 + 全圈
//!   `2 π` 假设；真 calibration 接入留 v0.3+（与 `robot-core::config`
//!   ConfigIdentity full hash 同步）。
//!
//! ## 单位约定
//!
//! - 关节状态 / 命令的 SI 单位是 **弧度**（trait 标准）。
//! - Feetech raw 单位是 12-bit tick：1 圈 = 4096 tick = 2 π rad。
//! - 转换：`rad = (raw - 2047) * (2π / 4096)`，`raw = round(rad / (2π / 4096) + 2047)`。
//!   2047 = `STS3215_MAX_POSITION` = 半转中点。
//! - velocity / torque：raw 单位定义随固件版本变（lerobot 也未做精确转换），
//!   v0.2 直接 raw 透传到 f32（精度足够 50 Hz observation）。

use std::f32::consts::TAU;

use robot_core::adapter::{
    AdapterInfo, AdapterKind, ControlMode, JointCommandView, JointLimits, JointStateBuf,
    ManipulatorAdapter, RtClass,
};
use robot_core::error::RobotError;

use crate::bus::FeetechBus;
use crate::error::FeetechError;
use crate::registers::{STS3215_MAX_POSITION, STS3215_RESOLUTION, TorqueMode};
use crate::transport::SerialTransport;

/// 12-bit tick → rad：`tick / 4096 * 2π`。
const RAD_PER_TICK: f32 = TAU / (STS3215_RESOLUTION as f32);

/// rad → 12-bit tick（绝对值，未加中点偏移）。
#[inline]
fn rad_to_tick_offset(rad: f32) -> f32 {
    rad / RAD_PER_TICK
}

/// raw_position（带符号 sign-magnitude 解码后的值，可为负）→ rad。
#[inline]
fn raw_to_rad(raw: i32) -> f32 {
    (raw - i32::from(STS3215_MAX_POSITION)) as f32 * RAD_PER_TICK
}

/// rad → raw_position（带符号；写到 sync_write 前由 bus 自动 sign-magnitude 编码）。
#[inline]
fn rad_to_raw(rad: f32) -> i32 {
    let tick = rad_to_tick_offset(rad).round() as i32;
    tick + i32::from(STS3215_MAX_POSITION)
}

// ============================================================================
// FeetechAdapter
// ============================================================================

/// 配置：构造 adapter 时必传。
pub struct FeetechAdapterConfig {
    pub adapter_name: &'static str,
    /// joint index 顺序对齐 `JointStateBuf.positions[..joint_count]`。每个 joint
    /// 名必须已经在 bus 的 motors 表里。
    pub joint_names: Vec<String>,
}

/// `FeetechAdapter` —— 包一层 `FeetechBus`，把 sync_read/write 跟
/// `JointStateBuf` / `JointCommandView` 适配起来。
pub struct FeetechAdapter<T: SerialTransport> {
    info: AdapterInfo,
    bus: FeetechBus<T>,
    joint_names: Vec<String>,
    /// `joint_names` 翻译成 motor id（hot path 用，避开 HashMap String compare）。
    /// `connect()` 内一次性填充；`new()` 只能拿 `Vec::new()` placeholder。
    /// Step 7 task 27c follow-up 优化点（火焰图发现 `__memcmp_evex_movbe` 来自
    /// `FeetechBus::motor_id` HashMap lookup）。
    joint_ids: Vec<u8>,
    /// `read_state` 用的 position raw 缓冲（预分配 `joint_count` 长度，hot
    /// path 不 alloc）。
    pos_raw: Vec<i32>,
    /// `read_state` 用的 velocity raw 缓冲。
    vel_raw: Vec<i32>,
    /// `write_command` 用的 entries 缓冲（id, encoded raw）。
    cmd_entries: Vec<(u8, i32)>,
    /// 关节限位（v0.2 默认全开 ±π，task 28 真机阶段从 calibration JSON 读）。
    limits: JointLimits,
    /// `joint_names` 长度，缓存避免热路径调 `len()`。
    joint_count: u8,
}

impl<T: SerialTransport> FeetechAdapter<T> {
    /// # Errors
    /// `joint_names.len() > MAX_JOINTS` 由 `robot-core::adapter` 编译期保证不会发生
    /// （`JointStateBuf::positions` 是 `[f32; MAX_JOINTS]`），但 v0.2 这里 runtime
    /// 拒绝便于 fail-fast。
    ///
    /// # Panics
    /// `joint_names.len() > 255`（不可能 —— `MAX_JOINTS = 32`）。
    pub fn new(config: FeetechAdapterConfig, bus: FeetechBus<T>) -> Result<Self, FeetechError> {
        let joint_count_usize = config.joint_names.len();
        if joint_count_usize > robot_core::adapter::MAX_JOINTS {
            return Err(FeetechError::ByteLengthMismatch {
                expected: robot_core::adapter::MAX_JOINTS,
                actual: joint_count_usize,
            });
        }
        // 默认全开限位（v0.3 接 calibration 后真实化）
        let mut limits = JointLimits::default();
        for slot in &mut limits.position_min[..joint_count_usize] {
            *slot = -std::f32::consts::PI;
        }
        for slot in &mut limits.position_max[..joint_count_usize] {
            *slot = std::f32::consts::PI;
        }
        for slot in &mut limits.velocity_max[..joint_count_usize] {
            *slot = 10.0_f32; // ~95 RPM，安全上限保守值
        }
        for slot in &mut limits.torque_max[..joint_count_usize] {
            *slot = 1.0_f32;
        }

        Ok(Self {
            info: AdapterInfo {
                name: config.adapter_name,
                kind: AdapterKind::Manipulator,
                rt_class: RtClass::HardRT,
                vendor_sdk: false, // Rust 自实现 wire protocol，无 vendor SDK 链接
            },
            bus,
            joint_names: config.joint_names,
            // joint_ids 留空，connect() 时填充（避免 new 也要查 bus 的 HashMap）
            joint_ids: Vec::new(),
            // hot path 缓冲预分配到 joint_count，read_state / write_command 不 alloc
            pos_raw: vec![0_i32; joint_count_usize],
            vel_raw: vec![0_i32; joint_count_usize],
            cmd_entries: Vec::with_capacity(joint_count_usize),
            limits,
            joint_count: u8::try_from(joint_count_usize).expect("joint_count <= MAX_JOINTS=32"),
        })
    }

    /// 借出 `&mut FeetechBus` 给上层做校准 / set_half_turn_homings 等不属于 trait 的操作。
    pub fn bus(&mut self) -> &mut FeetechBus<T> {
        &mut self.bus
    }
}

impl<T: SerialTransport + 'static> ManipulatorAdapter for FeetechAdapter<T> {
    fn info(&self) -> AdapterInfo {
        self.info
    }

    fn connect(&mut self) -> Result<(), RobotError> {
        self.bus.connect(true).map_err(RobotError::from)?;
        // Step 7 task 27c follow-up：一次性翻译 joint_names → joint_ids，
        // 让 read_state / write_command hot path 走 by-id API 跳过 HashMap lookup。
        self.joint_ids.clear();
        self.joint_ids.reserve(self.joint_names.len());
        for name in &self.joint_names {
            let id = self.bus.motor_id(name).map_err(|e| {
                eprintln!("feetech connect: motor_id({name}): {e}");
                RobotError::from(e)
            })?;
            self.joint_ids.push(id);
        }
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), RobotError> {
        self.bus.disconnect();
        Ok(())
    }

    fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError> {
        // Step 7 task 27c follow-up：走 by-id API + 复用预分配 raw 缓冲，
        // 完全跳过 HashMap String compare（火焰图 hot path 优化）。
        // pos_raw / vel_raw 在 new() 已 vec![0; joint_count]，size 与 joint_count
        // 对齐，sync_read_by_id_into 写满 joint_ids.len() 个槽位即可。
        self.bus
            .sync_read_by_id_into("Present_Position", &self.joint_ids, &mut self.pos_raw)
            .map_err(|e| {
                eprintln!("feetech read_state Present_Position: {e}");
                RobotError::from(e)
            })?;
        self.bus
            .sync_read_by_id_into("Present_Velocity", &self.joint_ids, &mut self.vel_raw)
            .map_err(|e| {
                eprintln!("feetech read_state Present_Velocity: {e}");
                RobotError::from(e)
            })?;

        let n = self.joint_ids.len();
        for i in 0..n {
            out.positions[i] = raw_to_rad(self.pos_raw[i]);
            // velocity raw 单位是 0.01 rev/s 还是 RPM 取决于固件 setting；v0.2 直接
            // 透传 raw 到 f32，等 task 28 实测用示波器 / known 命令对照定单位换算。
            out.velocities[i] = self.vel_raw[i] as f32;
            out.torques[i] = 0.0; // STS3215 无 torque sensor
        }
        out.joint_count = self.joint_count;
        // stamp_ns 由调用方填（trait 文档约定：adapter 不持时钟）
        Ok(())
    }

    fn write_command(&mut self, cmd: &JointCommandView) -> Result<(), RobotError> {
        // v0.2 仅支持 Position 模式；其他模式静默忽略 + 警告
        if !matches!(cmd.mode, ControlMode::Position) {
            // 不报错，因为 state machine 的 holding command 也走这条路；只在
            // 非 stop 模式给警告
            if !matches!(cmd.mode, ControlMode::Stop) {
                eprintln!(
                    "feetech write_command: ControlMode {:?} not yet implemented (v0.2 only Position)",
                    cmd.mode
                );
            }
            return Ok(());
        }

        // Step 7 task 27c follow-up：复用预分配 cmd_entries 缓冲 + by-id API。
        // joint_ids 已在 connect() 缓存好，零 HashMap lookup。
        let n = std::cmp::min(cmd.positions.len(), self.joint_ids.len());
        self.cmd_entries.clear();
        for i in 0..n {
            let raw = rad_to_raw(cmd.positions[i]);
            self.cmd_entries.push((self.joint_ids[i], raw));
        }
        self.bus
            .sync_write_by_id("Goal_Position", &self.cmd_entries)
            .map_err(|e| {
                eprintln!("feetech write_command sync_write Goal_Position: {e}");
                RobotError::from(e)
            })
    }

    fn set_mode(&mut self, mode: ControlMode) -> Result<(), RobotError> {
        let op_mode_value: i32 = match mode {
            ControlMode::Position | ControlMode::ServoPosition => 0,
            ControlMode::Velocity => 1,
            ControlMode::Torque => {
                // STS3215 无独立 torque mode（PWM 模式可近似），v0.2 拒绝
                return Err(RobotError::InvalidParameter(
                    "feetech: ControlMode::Torque not supported on STS3215",
                ));
            }
            ControlMode::Stop => {
                // Stop 通过 enable(false) 实现，不动 Operating_Mode
                return self.enable(false);
            }
        };
        for name in &self.joint_names {
            self.bus
                .write("Operating_Mode", name, op_mode_value)
                .map_err(|e| {
                    eprintln!("feetech set_mode {name}: {e}");
                    RobotError::from(e)
                })?;
        }
        Ok(())
    }

    fn enable(&mut self, on: bool) -> Result<(), RobotError> {
        let v = if on {
            i32::from(TorqueMode::Enabled as u8)
        } else {
            i32::from(TorqueMode::Disabled as u8)
        };
        for name in &self.joint_names {
            self.bus.write("Torque_Enable", name, v).map_err(|e| {
                eprintln!("feetech enable({on}) {name}: {e}");
                RobotError::from(e)
            })?;
        }
        Ok(())
    }

    fn safety_stop(&mut self) -> Result<(), RobotError> {
        // STS3215 没"硬件级 ESTOP"指令；最强 software stop = 全部 Torque_Enable=0
        // 真硬件级安全停（断电）由上层 GPIO / 继电器控制，v0.2 不在 adapter 内 wire。
        self.enable(false)
    }

    fn limits(&self) -> &JointLimits {
        &self.limits
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::checksum;
    use crate::transport::{MockHandle, MockTransport};
    use crate::types::Motor;
    use std::collections::HashMap;

    /// 构造 + connected 状态的 adapter。喂 handshake 回包（每 motor 2 包：
    /// PING ack + Model_Number 777）让 `connect()` 内部 handshake + populate
    /// `joint_ids` 全部跑完，drain 掉 handshake tx，返回干净 handle。
    ///
    /// task 27c follow-up 之后 read_state / write_command 必须在 `joint_ids` 已
    /// populate 后才能正确工作，所以测试一律从 connected 状态出发。
    fn make_adapter(joints: &[(&str, u8)]) -> (FeetechAdapter<MockTransport>, MockHandle) {
        let (transport, handle) = MockTransport::new();
        let mut motors = HashMap::new();
        for (name, id) in joints {
            motors.insert((*name).to_string(), Motor::new(*id).unwrap());
        }
        let bus = FeetechBus::new(transport, motors).unwrap();
        let mut adapter = FeetechAdapter::new(
            FeetechAdapterConfig {
                adapter_name: "feetech_sts3215_test",
                joint_names: joints.iter().map(|(n, _)| (*n).to_string()).collect(),
            },
            bus,
        )
        .unwrap();
        // handshake：connect() 对每个 motor 调 ping（PING ack + READ Model_Number）
        for &(_, id) in joints {
            handle.push_rx(&ack_status(id));
            handle.push_rx(&read_status(id, &[0x09, 0x03])); // 777 LE
        }
        adapter.connect().unwrap();
        let _ = handle.drain_tx();
        (adapter, handle)
    }

    fn ack_status(id: u8) -> Vec<u8> {
        let mut out = vec![0xFF, 0xFF, id, 0x02, 0x00];
        out.push(checksum(&out[2..]));
        out
    }

    fn read_status(id: u8, params: &[u8]) -> Vec<u8> {
        let len = u8::try_from(params.len() + 2).unwrap();
        let mut out = vec![0xFF, 0xFF, id, len, 0x00];
        out.extend_from_slice(params);
        out.push(checksum(&out[2..]));
        out
    }

    #[test]
    fn raw_rad_round_trip_at_center() {
        // 2047 = 中点 → 0 rad
        assert!((raw_to_rad(2047) - 0.0).abs() < 1e-6);
        assert_eq!(rad_to_raw(0.0), 2047);
    }

    #[test]
    fn raw_rad_round_trip_quarter_turn() {
        // 1024 tick away from center = π/2 rad
        let r = raw_to_rad(2047 + 1024);
        assert!((r - std::f32::consts::FRAC_PI_2).abs() < 1e-3);
        assert_eq!(rad_to_raw(std::f32::consts::FRAC_PI_2), 2047 + 1024);
    }

    #[test]
    fn info_reports_hard_rt_manipulator() {
        let (a, _h) = make_adapter(&[("j1", 1)]);
        let info = a.info();
        assert_eq!(info.kind, AdapterKind::Manipulator);
        assert_eq!(info.rt_class, RtClass::HardRT);
        assert!(!info.vendor_sdk);
        assert_eq!(info.name, "feetech_sts3215_test");
    }

    #[test]
    fn limits_default_to_pm_pi() {
        let (a, _h) = make_adapter(&[("j1", 1), ("j2", 2)]);
        assert!((a.limits().position_min[0] + std::f32::consts::PI).abs() < 1e-6);
        assert!((a.limits().position_max[1] - std::f32::consts::PI).abs() < 1e-6);
    }

    #[test]
    fn enable_writes_torque_enable_per_joint() {
        let (mut a, h) = make_adapter(&[("j1", 1), ("j2", 2)]);
        h.push_rx(&ack_status(1));
        h.push_rx(&ack_status(2));
        a.enable(true).unwrap();
        let tx = h.drain_tx();
        // 两个 WRITE_DATA Torque_Enable=1：addr=40=0x28, data=[01]
        let count = tx.windows(3).filter(|w| *w == [0x03, 0x28, 0x01]).count();
        assert_eq!(count, 2);
    }

    #[test]
    fn read_state_decodes_positions_to_rad() {
        let (mut a, h) = make_adapter(&[("j1", 1)]);
        // sync_read Present_Position：1 motor，raw=2047+1024 = 3071 (positive)
        // raw 3071 LE = [0xFF, 0x0B]
        h.push_rx(&read_status(1, &[0xFF, 0x0B]));
        // sync_read Present_Velocity：raw=0
        h.push_rx(&read_status(1, &[0x00, 0x00]));

        let mut state = JointStateBuf::default();
        a.read_state(&mut state).unwrap();
        assert_eq!(state.joint_count, 1);
        assert!((state.positions[0] - std::f32::consts::FRAC_PI_2).abs() < 1e-3);
        assert_eq!(state.velocities[0], 0.0);
    }

    #[test]
    fn write_command_converts_rad_to_raw_sync_write() {
        let (mut a, h) = make_adapter(&[("j1", 1)]);
        // sync_write 不 ACK，无 push_rx
        let mut positions = [0.0_f32; robot_core::adapter::MAX_JOINTS];
        positions[0] = std::f32::consts::FRAC_PI_2;
        let buf = robot_core::adapter::JointCommandBuf {
            joint_count: 1,
            mode: robot_core::adapter::ControlModeRepr::Position,
            positions,
            ..robot_core::adapter::JointCommandBuf::default()
        };
        let view = buf.as_view();
        a.write_command(&view).unwrap();

        let tx = h.drain_tx();
        // SYNC_WRITE addr=42=0x2A, length=2, motor 1: 3071 LE = [FF, 0B]
        // 包: FF FF FE LEN 83 2A 02 01 FF 0B CHK
        // PARAMS = [2A, 02, 01, FF, 0B] = 5 bytes, LEN = 7
        assert_eq!(tx[0..5], [0xFF, 0xFF, 0xFE, 0x07, 0x83]);
        assert_eq!(tx[5..10], [0x2A, 0x02, 0x01, 0xFF, 0x0B]);
    }

    #[test]
    fn write_command_non_position_mode_is_noop_with_warning() {
        let (mut a, h) = make_adapter(&[("j1", 1)]);
        let buf = robot_core::adapter::JointCommandBuf {
            joint_count: 1,
            mode: robot_core::adapter::ControlModeRepr::Stop,
            ..robot_core::adapter::JointCommandBuf::default()
        };
        let view = buf.as_view();
        a.write_command(&view).unwrap();
        // tx 应空（Stop 模式不发包）
        let tx = h.drain_tx();
        assert!(tx.is_empty(), "Stop mode 不应发 sync_write");
    }

    #[test]
    fn set_mode_torque_rejected() {
        let (mut a, _h) = make_adapter(&[("j1", 1)]);
        let err = a.set_mode(ControlMode::Torque).unwrap_err();
        assert!(matches!(err, RobotError::InvalidParameter(_)));
    }
}
