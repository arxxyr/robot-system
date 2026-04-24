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
use std::thread::sleep;
use std::time::Duration;

use robot_core::adapter::{
    AdapterInfo, AdapterKind, ControlMode, JointCommandView, JointLimits, JointStateBuf,
    ManipulatorAdapter, RtClass,
};
use robot_core::error::RobotError;

use crate::bus::FeetechBus;
use crate::calibration_io::clamp_raw;
use crate::error::FeetechError;
use crate::registers::{STS3215_MAX_POSITION, STS3215_RESOLUTION, TorqueMode};
use crate::transport::SerialTransport;
use crate::types::MotorCalibration;

// ============================================================================
// dimos `_manipulator_base.py::write_enable(True)` 序列的常量
// ============================================================================

/// sync Goal=Present 之后、写 Torque_Enable 之前的消化延时。
///
/// 1:1 移植自 dimos `_manipulator_base.py:476` `time.sleep(0.05)`：让总线消化 sync_write
/// 指令，避免 Torque_Enable 紧接着写时电机内部状态跳 → 电流尖峰 → 抖一下。
const ENABLE_SYNC_SETTLE: Duration = Duration::from_millis(50);

/// Torque_Enable 写失败时的单次重试间隔。
///
/// 1:1 移植自 dimos `_manipulator_base.py:466` `retry_delay_s = 0.15`。
const TORQUE_ENABLE_RETRY_DELAY: Duration = Duration::from_millis(150);

/// Torque_Enable 每个 motor 的最多尝试次数。
///
/// 1:1 移植自 dimos `_manipulator_base.py:465` `max_attempts = 3`。
const TORQUE_ENABLE_MAX_ATTEMPTS: u32 = 3;

/// `sync_goal_to_present` 写 Goal 后、readback 验证前的消化延时。
///
/// 50 ms 给总线消化 sync_write，但不必和 `ENABLE_SYNC_SETTLE` 一样 —— 本 settle
/// 之后立刻读 Goal_Position 验证，20 ms 足够；过长拖慢启动。
const SYNC_GOAL_READBACK_WAIT: Duration = Duration::from_millis(20);

/// `sync_goal_to_present` 写 Goal 的最多尝试次数（每次失败 = readback 到的 Goal
/// 与 Present 不匹配）。task 29 real-hw 观察到 Torque_Enable=0 时 sync_write
/// Goal_Position 有时会被 motor 忽略（返回开机默认值 2047），导致 Torque 接上
/// 的瞬间电机追错 Goal → 启动"跳到一半"。本 retry + readback 兜底这个失效模式。
const SYNC_GOAL_MAX_ATTEMPTS: u32 = 3;

/// `sync_goal_to_present` readback 的 Goal 与期望值之间允许的最大偏差（raw tick）。
///
/// 真机 sync_read Present → sync_write Goal → sync_read Goal 这个 roundtrip
/// 之间，motor 的 Present 本身可能在微动（外力 / 重力 / 控制精度），实测一次
/// gripper pose hold ±10 tick (~0.9°) 属于正常噪声。容差设 20 tick (~1.8°)：
/// 比噪声宽一倍，远小于 "跳到 center" 的失效幅度（600+ tick）。
const SYNC_GOAL_TOLERANCE: i32 = 20;

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
    /// 可选 per-joint 标定 —— 长度等于 `joint_names.len()` 时按同序对齐；空 `Vec`
    /// 表示"无标定"（热路径不做 silent clamp，`limits` 保持 ±π 全开）。
    ///
    /// 非空长度不匹配 → `FeetechAdapter::new` 返 `FeetechError::ByteLengthMismatch`。
    /// 来源推荐：`calibration_io::load_and_merge(&[...])` → factory 按 joint_names
    /// 顺序取出对应 motor 的 `MotorCalibration`。
    pub calibrations: Vec<MotorCalibration>,
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
    /// 关节限位（无 calibration 时 ±π 全开；有 calibration 时按 `range_min/max`
    /// raw → rad 换算填充，由 [`FeetechAdapter::new`] 一次性算好）。
    limits: JointLimits,
    /// `joint_names` 长度，缓存避免热路径调 `len()`。
    joint_count: u8,
    /// Per-joint 标定。非空 Vec 长度等于 `joint_count`；空 Vec 表示无标定，
    /// `write_command` 热路径跳过 silent clamp。task 29 commit 2 新增。
    calibrations: Vec<MotorCalibration>,
    /// `sync_goal_to_present` 里 readback 用的 Goal_Position 缓冲（预分配避免
    /// 热路径 alloc，虽然 enable 不在 RT scope 但保持一致风格）。task 29 commit
    /// 4 新增 —— 用于防飞序列的 write Goal 后 readback 验证。
    goal_raw: Vec<i32>,
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

        // calibration 长度校验：允许空（无标定）或长度 == joint_count
        let calibrations = config.calibrations;
        if !calibrations.is_empty() && calibrations.len() != joint_count_usize {
            return Err(FeetechError::ByteLengthMismatch {
                expected: joint_count_usize,
                actual: calibrations.len(),
            });
        }

        // 关节限位：无 calibration 时保留原 ±π 全开默认；有 calibration 时按
        // range_min/max raw → rad 换算。limits 下发到 s1 后做 goal planning 软夹。
        let mut limits = JointLimits::default();
        for slot in &mut limits.velocity_max[..joint_count_usize] {
            *slot = 10.0_f32; // ~95 RPM，安全上限保守值
        }
        for slot in &mut limits.torque_max[..joint_count_usize] {
            *slot = 1.0_f32;
        }
        if calibrations.is_empty() {
            for slot in &mut limits.position_min[..joint_count_usize] {
                *slot = -std::f32::consts::PI;
            }
            for slot in &mut limits.position_max[..joint_count_usize] {
                *slot = std::f32::consts::PI;
            }
        } else {
            for (i, cal) in calibrations.iter().enumerate() {
                limits.position_min[i] = raw_to_rad(i32::from(cal.range_min));
                limits.position_max[i] = raw_to_rad(i32::from(cal.range_max));
            }
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
            calibrations,
            // task 29 commit 4：sync_goal_to_present readback 验证缓冲
            goal_raw: vec![0_i32; joint_count_usize],
        })
    }

    /// 借出 `&mut FeetechBus` 给上层做校准 / set_half_turn_homings 等不属于 trait 的操作。
    pub fn bus(&mut self) -> &mut FeetechBus<T> {
        &mut self.bus
    }

    /// 读当前 `Present_Position`，把它作为 `Goal_Position` 写回 —— target 追平
    /// current。**不动 `Torque_Enable`**。
    ///
    /// 两处用：
    /// - [`ManipulatorAdapter::enable(true)`] 里作为防飞序列第一步；
    /// - S0 在 `CONTROL_LOST` 子态恢复时 *主动* 调用（对齐 handoff §4.1 "S1
    ///   心跳断掉后再恢复，goal 重置成当时的 Present_Position"），避免 S1 带旧
    ///   goal 回来猛拉电机。
    ///
    /// ## task 29 commit 4：readback 验证 + retry
    ///
    /// 真机实测发现：`Torque_Enable=0` 状态下 `sync_write Goal_Position` 有时
    /// 会被 motor 忽略（写入未生效，motor 内部 Goal 保留开机默认值 2047）。
    /// 于是 `enable(true)` 刚接上 Torque 时电机从当前位置**跳向 center** ——
    /// "张开到一半"症状。
    ///
    /// 兜底策略：
    /// 1. 写完 Goal 后 `sleep SYNC_GOAL_READBACK_WAIT`（20 ms）消化总线
    /// 2. `sync_read Goal_Position` **验证** 写入生效
    /// 3. 若 readback 值与期望 Present 不匹配（任一 joint 差 > [`SYNC_GOAL_TOLERANCE`]
    ///    tick）→ 重写，至多 [`SYNC_GOAL_MAX_ATTEMPTS`] 次
    /// 4. 全部重试失败 → 返回 [`FeetechError::SyncGoalReadbackMismatch`]
    ///
    /// 每次失败 attempt 都 `eprintln!` 打印 write/readback 对比 —— 即便 fix
    /// 对个别 motor 型号无效，日志也留诊断线索。
    ///
    /// # Errors
    /// - 透传 `sync_read` / `sync_write` 的 [`FeetechError`]（总线通信失败）
    /// - Readback 连续不匹配 → [`FeetechError::SyncGoalReadbackMismatch`]
    pub fn sync_goal_to_present(&mut self) -> Result<(), FeetechError> {
        // 1. 读 Present_Position（target 目标值）
        self.bus
            .sync_read_by_id_into("Present_Position", &self.joint_ids, &mut self.pos_raw)?;

        // 构造 Goal entries = Present
        self.cmd_entries.clear();
        for i in 0..self.joint_ids.len() {
            self.cmd_entries.push((self.joint_ids[i], self.pos_raw[i]));
        }

        // 2. 写入 + readback + retry
        for attempt in 1..=SYNC_GOAL_MAX_ATTEMPTS {
            self.bus
                .sync_write_by_id("Goal_Position", &self.cmd_entries)?;
            sleep(SYNC_GOAL_READBACK_WAIT);
            self.bus
                .sync_read_by_id_into("Goal_Position", &self.joint_ids, &mut self.goal_raw)?;

            // 比较 Goal (readback) vs Present (期望值) —— 允许 ±SYNC_GOAL_TOLERANCE
            // tick 的微小偏差（motor 内部编码精度噪声）
            let mut mismatch: Option<(u8, i32, i32)> = None;
            for i in 0..self.joint_ids.len() {
                let wanted = self.pos_raw[i];
                let got = self.goal_raw[i];
                if (got - wanted).abs() > SYNC_GOAL_TOLERANCE {
                    mismatch = Some((self.joint_ids[i], wanted, got));
                    break;
                }
            }

            let max = SYNC_GOAL_MAX_ATTEMPTS;
            match mismatch {
                None => {
                    if attempt > 1 {
                        eprintln!(
                            "feetech sync_goal_to_present: attempt {attempt}/{max} succeeded"
                        );
                    }
                    return Ok(());
                }
                Some((id, wanted, got)) => {
                    let diff = got - wanted;
                    eprintln!(
                        "feetech sync_goal_to_present: attempt {attempt}/{max}: motor id={id} Goal readback mismatch (wrote={wanted}, read={got}, diff={diff})"
                    );
                    if attempt == max {
                        return Err(FeetechError::SyncGoalReadbackMismatch {
                            motor_id: id,
                            wrote: wanted,
                            got,
                            attempts: attempt,
                        });
                    }
                }
            }
        }
        // 所有 attempt 分支都走 return，这里不可达
        unreachable!("sync_goal_to_present: attempt loop should have returned")
    }

    /// S0 在 `CONTROL_LOST` 恢复时调 —— 把 Goal 拉回当前 Present，让后续的第
    /// 一个 S1 command 从当前位置再起动，不要从可能很旧的 Goal 猛拉。
    ///
    /// 等价于 dimos `_manipulator_base.py::write_stop` 的语义（*冻结* 当前姿态，
    /// **不** disable torque）—— 区别于 [`ManipulatorAdapter::safety_stop`]
    /// 的 "disable torque 断扭"。
    ///
    /// # Errors
    /// 透传 [`Self::sync_goal_to_present`] 的错误。
    pub fn hold_current_position(&mut self) -> Result<(), RobotError> {
        self.sync_goal_to_present().map_err(|e| {
            eprintln!("feetech hold_current_position: {e}");
            RobotError::from(e)
        })
    }

    /// Per-motor `Torque_Enable` 写（带 3 次重试 + 150 ms 间隔），任一 motor
    /// 全部重试都失败即返回第一失败 motor 的 error（`dimos _manipulator_base.py:
    /// 468-504` 语义 1:1）。
    fn write_torque_enable_with_retry(&mut self, on: bool) -> Result<(), RobotError> {
        let target = if on {
            i32::from(TorqueMode::Enabled as u8)
        } else {
            i32::from(TorqueMode::Disabled as u8)
        };
        let label = if on { "enable(true)" } else { "enable(false)" };
        let max_attempts = TORQUE_ENABLE_MAX_ATTEMPTS;

        let mut first_failure: Option<(String, FeetechError)> = None;
        for name in &self.joint_names {
            let mut last_err: Option<FeetechError> = None;
            for attempt in 1..=max_attempts {
                match self.bus.write("Torque_Enable", name, target) {
                    Ok(()) => {
                        last_err = None;
                        break;
                    }
                    Err(e) => {
                        eprintln!("feetech {label} {name} attempt {attempt}/{max_attempts}: {e}");
                        last_err = Some(e);
                        if attempt < max_attempts {
                            sleep(TORQUE_ENABLE_RETRY_DELAY);
                        }
                    }
                }
            }
            if let Some(e) = last_err
                && first_failure.is_none()
            {
                first_failure = Some((name.clone(), e));
            }
        }
        if let Some((name, e)) = first_failure {
            eprintln!("feetech {label}: first failing motor {name}: {e}");
            return Err(RobotError::from(e));
        }
        Ok(())
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
        // task 29 commit 2：若提供 calibration，校验 cal.id 与 bus 给出的 motor id
        // 对齐 —— 配错时 ID 与名字错配（比如 left_arm.json 被指给了 right_arm
        // joints）→ silent clamp 会用错 range，真转时撞机械限位
        if !self.calibrations.is_empty() {
            for (i, cal) in self.calibrations.iter().enumerate() {
                let bus_id = self.joint_ids[i];
                if cal.id != bus_id {
                    eprintln!(
                        "feetech connect: calibration[{i}] id={cal_id} != bus motor id={bus_id} for joint {name}",
                        cal_id = cal.id,
                        name = self.joint_names[i]
                    );
                    return Err(RobotError::InvalidParameter(
                        "feetech: calibration motor id mismatch",
                    ));
                }
            }
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
        //
        // task 29 commit 2：非空 calibration 时热路径走 silent clamp（dimos
        // `_manipulator_base.py:422-426` 1:1 对齐）；无 calibration 时保留原行为。
        let n = std::cmp::min(cmd.positions.len(), self.joint_ids.len());
        self.cmd_entries.clear();
        let use_clamp = !self.calibrations.is_empty();
        for i in 0..n {
            let mut raw = rad_to_raw(cmd.positions[i]);
            if use_clamp {
                let cal = &self.calibrations[i];
                raw = clamp_raw(raw, i32::from(cal.range_min), i32::from(cal.range_max));
            }
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
        if on {
            // task 29 commit 2：1:1 移植 dimos `_manipulator_base.py:468-504` 防飞序列
            //   1) sync_read Present_Position for all
            //   2) sync_write Goal_Position <- Present (让 target = current)
            //   3) sleep 50ms 让总线消化 sync_write
            //   4) per-motor 写 Torque_Enable=1 带最多 3 次重试
            // 目的：Torque 接上瞬间 target == current → 电机不动，避开 Goal 还
            // 是旧值导致的"飞向 0"风险。
            self.sync_goal_to_present().map_err(|e| {
                eprintln!("feetech enable(true): sync goal=present: {e}");
                RobotError::from(e)
            })?;
            sleep(ENABLE_SYNC_SETTLE);

            self.write_torque_enable_with_retry(true)
        } else {
            // disable：直接写 0，不需要 goal=present 序列；为一致性也加 3x 重试
            self.write_torque_enable_with_retry(false)
        }
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
        make_adapter_with_calibration(joints, Vec::new())
    }

    /// 带 calibration 的 adapter 构造。calibrations 长度必须匹配 joints（或空表示无标定）。
    fn make_adapter_with_calibration(
        joints: &[(&str, u8)],
        calibrations: Vec<MotorCalibration>,
    ) -> (FeetechAdapter<MockTransport>, MockHandle) {
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
                calibrations,
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
    fn enable_true_follows_dimos_sequence() {
        // task 29 commit 2 + commit 4：enable(true) 移植 dimos 序列 + 新加 readback 验证：
        //   1) sync_read Present_Position（每 motor 一个 status 回包）
        //   2) sync_write Goal_Position（broadcast，不等 ACK）
        //   3) sync_read Goal_Position 验证（每 motor 一个 status 回包，匹配 → 一次通过）
        //   4) sleep ENABLE_SYNC_SETTLE（50ms）
        //   5) per-motor WRITE Torque_Enable=1（每个 1 个 ACK）
        let (mut a, h) = make_adapter(&[("j1", 1), ("j2", 2)]);
        // step 1 sync_read Present_Position：每 motor 一个 status packet，raw=2048
        h.push_rx(&read_status(1, &[0x00, 0x08]));
        h.push_rx(&read_status(2, &[0x00, 0x08]));
        // step 3 sync_read Goal_Position readback：返回 2048 匹配 → 一次通过
        h.push_rx(&read_status(1, &[0x00, 0x08]));
        h.push_rx(&read_status(2, &[0x00, 0x08]));
        // step 5 Torque_Enable ACK：每 motor 一个
        h.push_rx(&ack_status(1));
        h.push_rx(&ack_status(2));
        a.enable(true).unwrap();
        let tx = h.drain_tx();
        // 两个 WRITE_DATA Torque_Enable=1：addr=40=0x28, data=[01]
        let count = tx.windows(3).filter(|w| *w == [0x03, 0x28, 0x01]).count();
        assert_eq!(count, 2);
    }

    #[test]
    fn enable_false_writes_torque_off_per_joint() {
        // enable(false) 无 sync_read / sync_write 前置步骤，直接写 Torque_Enable=0
        let (mut a, h) = make_adapter(&[("j1", 1), ("j2", 2)]);
        h.push_rx(&ack_status(1));
        h.push_rx(&ack_status(2));
        a.enable(false).unwrap();
        let tx = h.drain_tx();
        let count = tx.windows(3).filter(|w| *w == [0x03, 0x28, 0x00]).count();
        assert_eq!(count, 2);
    }

    #[test]
    fn hold_current_position_syncs_goal_to_present() {
        let (mut a, h) = make_adapter(&[("j1", 1)]);
        // 序列（task 29 commit 4 后）：
        //   1) sync_read Present_Position -> raw=1500 (LE [0xDC, 0x05])
        //   2) sync_write Goal_Position = 1500（不 ACK）
        //   3) sync_read Goal_Position 验证 -> 返回 1500 = 匹配，attempt 1 就通过
        h.push_rx(&read_status(1, &[0xDC, 0x05])); // Present
        h.push_rx(&read_status(1, &[0xDC, 0x05])); // Goal readback (match → 一次通过)
        a.hold_current_position().unwrap();
        let tx = h.drain_tx();
        // sync_write Goal_Position addr=42=0x2A, len=2, motor 1, raw 1500 LE = [0xDC, 0x05]
        let has_sync_write = tx.windows(5).any(|w| *w == [0x2A, 0x02, 0x01, 0xDC, 0x05]);
        assert!(has_sync_write, "应含 sync_write Goal=1500 frame");
    }

    #[test]
    fn sync_goal_to_present_retries_on_readback_mismatch() {
        let (mut a, h) = make_adapter(&[("j1", 1)]);
        // 模拟 Torque_Enable=0 导致第一次 write Goal 未生效的场景：
        //   1) sync_read Present_Position -> raw=1500
        //   2) sync_write Goal_Position = 1500（不 ACK）
        //   3) sync_read Goal_Position   -> raw=2047（motor 开机默认，失败）
        //   4) sync_write Goal_Position = 1500（重试）
        //   5) sync_read Goal_Position   -> raw=1500（匹配，attempt 2 通过）
        h.push_rx(&read_status(1, &[0xDC, 0x05])); // Present = 1500
        h.push_rx(&read_status(1, &[0xFF, 0x07])); // Goal readback = 2047 (mismatch)
        h.push_rx(&read_status(1, &[0xDC, 0x05])); // Goal readback 2 = 1500 (match)
        a.sync_goal_to_present().unwrap();
        // 两次 sync_write Goal，确认有重试行为
        let tx = h.drain_tx();
        let count = tx
            .windows(5)
            .filter(|w| *w == [0x2A, 0x02, 0x01, 0xDC, 0x05])
            .count();
        assert_eq!(
            count, 2,
            "应含 2 次 sync_write Goal=1500 (一次原始 + 一次 retry)"
        );
    }

    #[test]
    fn sync_goal_to_present_tolerance_accepts_small_drift() {
        let (mut a, h) = make_adapter(&[("j1", 1)]);
        // Present=1500, Goal readback=1510（差 10 tick，在 SYNC_GOAL_TOLERANCE=20 内）
        //   → 应一次通过，不触发 retry
        h.push_rx(&read_status(1, &[0xDC, 0x05])); // Present = 1500
        h.push_rx(&read_status(1, &[0xE6, 0x05])); // Goal readback = 1510
        a.sync_goal_to_present().unwrap();
        let tx = h.drain_tx();
        let count = tx
            .windows(5)
            .filter(|w| *w == [0x2A, 0x02, 0x01, 0xDC, 0x05])
            .count();
        assert_eq!(count, 1, "容差内 Goal readback 不应触发 retry");
    }

    #[test]
    fn sync_goal_to_present_returns_error_after_max_attempts() {
        let (mut a, h) = make_adapter(&[("j1", 1)]);
        // Present=1500, Goal readback=2047 (永远不匹配) × 3 次 → 报 SyncGoalReadbackMismatch
        h.push_rx(&read_status(1, &[0xDC, 0x05])); // Present
        for _ in 0..3 {
            h.push_rx(&read_status(1, &[0xFF, 0x07])); // Goal readback 总是 2047
        }
        match a.sync_goal_to_present() {
            Err(FeetechError::SyncGoalReadbackMismatch {
                motor_id,
                wrote,
                got,
                attempts,
            }) => {
                assert_eq!(motor_id, 1);
                assert_eq!(wrote, 1500);
                assert_eq!(got, 2047);
                assert_eq!(attempts, 3);
            }
            Ok(()) => panic!("expected SyncGoalReadbackMismatch"),
            Err(e) => panic!("expected SyncGoalReadbackMismatch, got {e:?}"),
        }
    }

    #[test]
    fn write_command_silent_clamp_when_calibration_present() {
        // calibration range_min=1000, range_max=3000；goal rad 对应 raw=3500（超上限）
        // 应被 silent clamp 到 3000
        let cal = MotorCalibration::new(1, 0, 0, 1000, 3000).unwrap();
        let (mut a, h) = make_adapter_with_calibration(&[("j1", 1)], vec![cal]);
        let mut positions = [0.0_f32; robot_core::adapter::MAX_JOINTS];
        // raw 3500 = center 2047 + 1453 → rad = 1453 * (2π/4096) ≈ 2.229 rad
        positions[0] = 1453.0 * TAU / 4096.0;
        let buf = robot_core::adapter::JointCommandBuf {
            joint_count: 1,
            mode: robot_core::adapter::ControlModeRepr::Position,
            positions,
            ..robot_core::adapter::JointCommandBuf::default()
        };
        let view = buf.as_view();
        a.write_command(&view).unwrap();

        let tx = h.drain_tx();
        // sync_write Goal_Position raw=3000 LE = [0xB8, 0x0B]
        let has_clamped = tx.windows(5).any(|w| *w == [0x2A, 0x02, 0x01, 0xB8, 0x0B]);
        assert!(has_clamped, "raw 应被 clamp 到 3000（0x0BB8 LE）");
        // 反证：原 raw 3500 = 0x0DAC LE = [0xAC, 0x0D] 不应出现
        let has_unclamped = tx.windows(2).any(|w| *w == [0xAC, 0x0D]);
        assert!(!has_unclamped, "未经 clamp 的 3500 不应出现在 tx");
    }

    #[test]
    fn write_command_no_calibration_no_clamp() {
        // 无 calibration → 不 clamp，原 raw 照写
        let (mut a, h) = make_adapter(&[("j1", 1)]);
        let mut positions = [0.0_f32; robot_core::adapter::MAX_JOINTS];
        positions[0] = 1453.0 * TAU / 4096.0; // raw = 3500
        let buf = robot_core::adapter::JointCommandBuf {
            joint_count: 1,
            mode: robot_core::adapter::ControlModeRepr::Position,
            positions,
            ..robot_core::adapter::JointCommandBuf::default()
        };
        let view = buf.as_view();
        a.write_command(&view).unwrap();
        let tx = h.drain_tx();
        // raw 3500 LE = [0xAC, 0x0D]
        let has_raw = tx.windows(5).any(|w| *w == [0x2A, 0x02, 0x01, 0xAC, 0x0D]);
        assert!(has_raw, "无 calibration 应直写 raw=3500");
    }

    #[test]
    fn calibration_derived_limits_override_pm_pi() {
        // range_min=1024（raw，对应 rad ≈ -π/2）/ range_max=3072（rad ≈ +π/2+ε）
        let cal = MotorCalibration::new(1, 0, 0, 1024, 3072).unwrap();
        let (a, _h) = make_adapter_with_calibration(&[("j1", 1)], vec![cal]);
        // raw 1024 - 2047 = -1023 → rad ≈ -1023 * 2π/4096 ≈ -1.570 rad (-π/2 + ε)
        assert!(
            (a.limits().position_min[0] - (-1023.0 * TAU / 4096.0)).abs() < 1e-4,
            "position_min 应从 calibration 推导"
        );
        // raw 3072 - 2047 = 1025 → rad ≈ +1.572 rad
        assert!(
            (a.limits().position_max[0] - (1025.0 * TAU / 4096.0)).abs() < 1e-4,
            "position_max 应从 calibration 推导"
        );
    }

    #[test]
    fn calibration_length_mismatch_rejected() {
        // joint 有 1 个，calibration 给 2 个 → new() 拒绝
        let (transport, _handle) = MockTransport::new();
        let mut motors = HashMap::new();
        motors.insert("j1".to_string(), Motor::new(1).unwrap());
        let bus = FeetechBus::new(transport, motors).unwrap();
        let result = FeetechAdapter::new(
            FeetechAdapterConfig {
                adapter_name: "test",
                joint_names: vec!["j1".into()],
                calibrations: vec![MotorCalibration::identity(1), MotorCalibration::identity(2)],
            },
            bus,
        );
        match result {
            Err(FeetechError::ByteLengthMismatch { .. }) => {}
            Ok(_) => panic!("expected ByteLengthMismatch, got Ok"),
            Err(other) => panic!("expected ByteLengthMismatch, got {other:?}"),
        }
    }

    #[test]
    fn calibration_id_mismatch_rejected_on_connect() {
        // calibration.id=99 与 bus motor id=1 不匹配 → connect() 拒绝
        let (transport, handle) = MockTransport::new();
        let mut motors = HashMap::new();
        motors.insert("j1".to_string(), Motor::new(1).unwrap());
        let bus = FeetechBus::new(transport, motors).unwrap();
        let mut adapter = FeetechAdapter::new(
            FeetechAdapterConfig {
                adapter_name: "test",
                joint_names: vec!["j1".into()],
                calibrations: vec![MotorCalibration::identity(99)],
            },
            bus,
        )
        .unwrap();
        // handshake 回包（连接先成功）
        handle.push_rx(&ack_status(1));
        handle.push_rx(&read_status(1, &[0x09, 0x03]));
        let err = adapter.connect().unwrap_err();
        match err {
            RobotError::InvalidParameter(msg) => {
                assert!(msg.contains("calibration motor id mismatch"));
            }
            other => panic!("expected InvalidParameter, got {other:?}"),
        }
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
