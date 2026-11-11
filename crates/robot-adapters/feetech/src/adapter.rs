//! [`FeetechAdapter`] —— 实现 `robot_core::adapter::ManipulatorAdapter` trait。
//!
//! ## v0.2 范围
//!
//! - 同步实现：`read_state` / `write_command` 在调用线程内完成 wire RTT
//!   （sync_read / sync_write）。
//! - **挂 `s0_serial_io_thread`，不挂 `s0_rt_thread`**（task-30 pre-1a/1b）：
//!   串口 RTT ms 量级（13-motor sync_read ≈ 6 ms）→ 违反 §4.9 RT 铁律的
//!   "< 100 µs / no blocking" 约束。RtClass 标 `RtClass::SerialRT` 让 robot-s0
//!   按 "总线 latency 可证明但单帧 RTT 远超 100 µs" 路由到 SerialIoCtrlClient
//!   + serial_io_thread（普通优先级 + select! 主循环）。
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

use std::sync::Arc;
use std::time::Duration;

use robot_core::adapter::{
    AdapterInfo, AdapterKind, ControlMode, JointCommandView, JointLimits, JointStateBuf,
    ManipulatorAdapter, RtClass,
};
use robot_core::diag::{DiagEvent, DiagSink};
use robot_core::error::RobotError;
use robot_core::executor_ctrl::OutputSafetyProbe;
use robot_core::serial_services::{BusOpClass, CommErrorKind, LeasedPreemptProbe};
use robot_core::time::{DurationNs, RobotClock};

use crate::bus::FeetechBus;
use crate::calibration_io::clamp_raw;
use crate::convert::{rad_to_raw, raw_to_rad};
use crate::error::FeetechError;
use crate::registers::TorqueMode;
use crate::transport::SerialTransport;
use crate::types::MotorCalibration;

#[path = "diagnostic.rs"]
mod diagnostic;
use diagnostic::FeetechDiagnostic;
#[path = "mixed_bus.rs"]
mod mixed_bus;
use mixed_bus::MixedBus2;
#[cfg(feature = "real-hw")]
pub(crate) use mixed_bus::{raw_profiles_for_config, validate_raw_mixed_calibrations};

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

// raw ↔ rad 换算：v0.5 候选 E 段(6) 起挪到 `crate::convert`（pub 纯函数，
// 公式单源；本 adapter 热路径与外部消费者〔真机验收 bin〕调同一份实现）。

// ============================================================================
// FeetechAdapter
// ============================================================================

/// 配置：构造 adapter 时必传。
pub struct FeetechAdapterConfig {
    /// 同一运行时的单调时钟与构造期服务；热路径不 clone 句柄。
    pub clock: Arc<dyn RobotClock>,
    pub preempt: Option<Arc<dyn LeasedPreemptProbe>>,
    pub diag: Option<Arc<dyn DiagSink>>,
    pub call_deadline: Duration,
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
    /// 与关节同序的校准边界容差，raw 单位；空数组等价全 0。
    /// 非空长度必须与关节数一致，每项限 0..=4095；不修改原标定。
    pub calibration_range_tolerances_raw: Vec<u16>,
}

/// 只扩展软件校准边界，保留 12-bit 位置硬界及原标定数据。
fn effective_calibration_range_raw(calibration: &MotorCalibration, tolerance: u16) -> (i32, i32) {
    (
        i32::from(calibration.range_min.saturating_sub(tolerance)),
        i32::from(calibration.range_max.saturating_add(tolerance).min(4095)),
    )
}

fn validate_calibrations_raw(
    calibrations: &[MotorCalibration],
    joint_count: usize,
) -> Result<(), FeetechError> {
    if !calibrations.is_empty() && calibrations.len() != joint_count {
        return Err(FeetechError::ByteLengthMismatch {
            expected: joint_count,
            actual: calibrations.len(),
        });
    }
    for calibration in calibrations {
        if calibration.range_max > 4095 {
            return Err(FeetechError::CalibrationMaxOutOfRange(i32::from(
                calibration.range_max,
            )));
        }
        if calibration.range_min > calibration.range_max {
            return Err(FeetechError::CalibrationRangeInverted {
                min: calibration.range_min,
                max: calibration.range_max,
            });
        }
    }
    Ok(())
}

/// factory 在打开串口前复用此校验；adapter 构造仍独立守住同一约束。
pub(crate) fn validate_calibration_range_tolerances_raw(
    joint_count: usize,
    tolerances: &[u16],
) -> Result<(), FeetechError> {
    if !tolerances.is_empty() && tolerances.len() != joint_count {
        return Err(FeetechError::ByteLengthMismatch {
            expected: joint_count,
            actual: tolerances.len(),
        });
    }
    if let Some(&invalid) = tolerances.iter().find(|&&value| value > 4095) {
        return Err(FeetechError::CalibrationRangeToleranceOutOfRange(invalid));
    }
    Ok(())
}

/// `FeetechAdapter` —— 包一层 `FeetechBus`，把 sync_read/write 跟
/// `JointStateBuf` / `JointCommandView` 适配起来。
pub struct FeetechAdapter<T: SerialTransport> {
    clock: Arc<dyn RobotClock>,
    preempt: Option<Arc<dyn LeasedPreemptProbe>>,
    diag: Option<Arc<dyn DiagSink>>,
    connected: bool,
    mixed_bus2: Option<MixedBus2>,
    mixed_arm_anchor: Option<([i32; 6], u64)>,
    /// 仅完整成功的 raw 混合零轮目标允许后续设备观测，不代表已停止。
    mixed_zero_target_confirmed: bool,
    /// 观测尝试的独立限频，不随重复零目标或非零目标重置。
    mixed_zero_observation_last_ns: Option<u64>,
    /// 周期状态中的原字观测限频，不增加串口事务。
    mixed_motion_observation_last_ns: Option<u64>,
    command_budget_ns: u64,
    info: AdapterInfo,
    bus: FeetechBus<T>,
    joint_names: Vec<String>,
    /// `joint_names` 翻译成 motor id（hot path 用，避开 HashMap String compare）。
    /// `new()` 内一次性填充；首次 Safety 不依赖后续握手是否完成。
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
    /// 构造期补齐的逐关节边界容差；轮速路径不消费此配置。
    calibration_range_tolerances_raw: Vec<u16>,
    /// `sync_goal_to_present` 里 readback 用的 Goal_Position 缓冲（预分配避免
    /// 热路径 alloc，虽然 enable 不在 RT scope 但保持一致风格）。task 29 commit
    /// 4 新增 —— 用于防飞序列的 write Goal 后 readback 验证。
    goal_raw: Vec<i32>,
    /// task-30 v9 / M3：`OutputSafetyProbe::read_torque_enable_mask` 路径的预分配
    /// 缓冲。serial_io_thread 在 emergency 后下 tick 调，必须不 alloc。
    /// 长度 = `joint_count`，sync_read Torque_Enable 回包后解码到这里再翻 bool。
    torque_enable_raw: Vec<i32>,
    /// task-30 v9 / M4：`safety_stop_fast` 路径的预分配 (id, 0) 列表。
    /// `new()` 时按 `joint_ids` 一次性填充；`safety_stop_fast` 调用时只读不
    /// 改，**emergency 路径不 alloc**。
    safety_stop_entries: Vec<(u8, i32)>,
    /// task-30 v9 / M4：`safety_stop_fast` 路径的 wire frame 预分配缓冲。
    /// `MAX_JOINTS=32 × length=4` → max frame = 168 byte，预分配 256 留余量。
    /// 公式：`6 (FF FF ID LEN INST CHKSUM) + 2 (addr+length) + N×(1+length_byte)`。
    safety_stop_frame_buf: Vec<u8>,
}

impl<T: SerialTransport> FeetechAdapter<T> {
    /// # Errors
    /// `joint_names.len() > MAX_JOINTS` 由 `robot-core::adapter` 编译期保证不会发生
    /// （`JointStateBuf::positions` 是 `[f32; MAX_JOINTS]`），但 v0.2 这里 runtime
    /// 拒绝便于 fail-fast。
    ///
    /// # Panics
    /// `joint_names.len() > 255`（不可能 —— `MAX_JOINTS = 32`）。
    pub fn new(config: FeetechAdapterConfig, mut bus: FeetechBus<T>) -> Result<Self, FeetechError> {
        // 构造任何出口之前安装调用预算；首个 Safety 不能等 connect 再配置。
        bus.set_timeout(config.call_deadline)?;
        bus.set_services(config.preempt.clone(), config.diag.clone());
        let joint_count_usize = config.joint_names.len();
        if joint_count_usize > robot_core::adapter::MAX_JOINTS {
            return Err(FeetechError::ByteLengthMismatch {
                expected: robot_core::adapter::MAX_JOINTS,
                actual: joint_count_usize,
            });
        }

        // calibration 长度校验：允许空（无标定）或长度 == joint_count
        let calibrations = config.calibrations;
        validate_calibrations_raw(&calibrations, joint_count_usize)?;

        validate_calibration_range_tolerances_raw(
            joint_count_usize,
            &config.calibration_range_tolerances_raw,
        )?;
        if calibrations.is_empty()
            && config
                .calibration_range_tolerances_raw
                .iter()
                .any(|&value| value != 0)
        {
            return Err(FeetechError::CalibrationToleranceWithoutCalibration);
        }
        let calibration_range_tolerances_raw = if config.calibration_range_tolerances_raw.is_empty()
        {
            vec![0; joint_count_usize]
        } else {
            config.calibration_range_tolerances_raw
        };

        let mut joint_ids = Vec::with_capacity(joint_count_usize);
        let mut safety_stop_entries = Vec::with_capacity(joint_count_usize);
        for (index, name) in config.joint_names.iter().enumerate() {
            let id = bus.motor_id(name)?;
            if let Some(calibration) = calibrations.get(index)
                && calibration.id != id
            {
                return Err(FeetechError::CalibrationMotorIdMismatch {
                    expected: id,
                    got: calibration.id,
                });
            }
            joint_ids.push(id);
            safety_stop_entries.push((id, 0));
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
                let (min, max) =
                    effective_calibration_range_raw(cal, calibration_range_tolerances_raw[i]);
                limits.position_min[i] = raw_to_rad(min);
                limits.position_max[i] = raw_to_rad(max);
            }
        }

        Ok(Self {
            clock: config.clock,
            preempt: config.preempt,
            diag: config.diag,
            connected: false,
            mixed_bus2: None,
            mixed_arm_anchor: None,
            mixed_zero_target_confirmed: false,
            mixed_zero_observation_last_ns: None,
            mixed_motion_observation_last_ns: None,
            command_budget_ns: u64::try_from(config.call_deadline.as_nanos()).unwrap_or(u64::MAX),
            info: AdapterInfo {
                name: config.adapter_name,
                kind: AdapterKind::Manipulator,
                // task-30 pre-1b：feetech rt_class HardRT → SerialRT。串口 RTT
                // 毫秒量级，绝不能挂 s0_rt_thread。robot-s0 按 SerialRT 路由到
                // s0_serial_io_thread（普通优先级 + select! 主循环）。
                rt_class: RtClass::SerialRT,
                vendor_sdk: false, // Rust 自实现 wire protocol，无 vendor SDK 链接
            },
            bus,
            joint_names: config.joint_names,
            joint_ids,
            // hot path 缓冲预分配到 joint_count，read_state / write_command 不 alloc
            pos_raw: vec![0_i32; joint_count_usize],
            vel_raw: vec![0_i32; joint_count_usize],
            cmd_entries: Vec::with_capacity(joint_count_usize),
            limits,
            joint_count: u8::try_from(joint_count_usize).expect("joint_count <= MAX_JOINTS=32"),
            calibrations,
            calibration_range_tolerances_raw,
            // task 29 commit 4：sync_goal_to_present readback 验证缓冲
            goal_raw: vec![0_i32; joint_count_usize],
            // task-30 v9 / M3：OutputSafetyProbe 路径预分配缓冲
            torque_enable_raw: vec![0_i32; joint_count_usize],
            // 首次 Safety 已有完整 motor 列表，不发空广播冒充成功。
            safety_stop_entries,
            safety_stop_frame_buf: vec![0u8; 256],
        })
    }

    /// 借出 `&mut FeetechBus` 给上层做校准 / set_half_turn_homings 等不属于 trait 的操作。
    pub fn bus(&mut self) -> &mut FeetechBus<T> {
        &mut self.bus
    }

    fn require_connected(&self) -> Result<(), FeetechError> {
        if !self.connected || !self.bus.is_connected() {
            return Err(FeetechError::NotConnected);
        }
        Ok(())
    }

    /// site 标记入口；kind：0错误、1重试、2恢复、3不支持模式、4接收清理、
    /// 5设备状态（a=motor、b=状态字节）、6扭矩使能写失败（a=motor、b=通信分类）、
    /// 7事务中止（a：1急停、2超时、3权限撤销）、8非通信错误（a 见诊断分类）；
    /// 校准锚点错误使用 kind8/a3/b=motor/detail=raw 位模式，其余 7/8 的 b/detail=0。
    /// 诊断分类不改变原始错误或安全处理。
    fn record(&self, site: u8, kind: u8, a: u8, b: u8, detail: u32) {
        if let Some(diag) = self.diag.as_ref() {
            diag.record(DiagEvent {
                stamp_ns: self.clock.now().0,
                site,
                kind,
                a,
                b,
                seq_lo: detail,
            });
        }
    }

    fn record_error(&self, site: u8, error: &FeetechError) {
        let diagnostic = FeetechDiagnostic::classify(error, self.bus.device_status_motor());
        let (kind, a, b) = diagnostic.encode();
        self.record(site, kind, a, b, diagnostic.detail());
    }

    fn sleep_or_abort(&self, duration: Duration) -> Result<(), FeetechError> {
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        if let Some(preempt) = self.preempt.as_ref() {
            return preempt
                .sleep_or_abort(BusOpClass::NormalEffectful, DurationNs(nanos))
                .map_err(FeetechError::Aborted);
        }
        let deadline = self.clock.now().0.saturating_add(nanos);
        loop {
            let remaining = deadline.saturating_sub(self.clock.now().0);
            if remaining == 0 {
                return Ok(());
            }
            self.clock.sleep_for(DurationNs(remaining.min(5_000_000)));
        }
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
    /// 每次失败与恢复均记录固定诊断事件；串口执行线程不格式化日志。
    ///
    /// # Errors
    /// - 透传 `sync_read` / `sync_write` 的 [`FeetechError`]（总线通信失败）
    /// - Readback 连续不匹配 → [`FeetechError::SyncGoalReadbackMismatch`]
    ///
    /// 显式 mixed 实例只同步六个臂关节，这是 Enable 准备原语，不代表全总线停止。
    pub fn sync_goal_to_present(&mut self) -> Result<(), FeetechError> {
        self.require_connected()?;
        self.bus.set_op_class(BusOpClass::NormalEffectful);
        // 1. 读 Present_Position（target 目标值）
        let mixed_ids = self.mixed_bus2.map(|layout| layout.arm_ids);
        let ids = mixed_ids
            .as_ref()
            .map_or(self.joint_ids.as_slice(), |ids| ids.as_slice());
        if self.raw_mixed() {
            self.mixed_arm_anchor = None;
        }
        let sampled_ns = if self.raw_mixed() {
            self.clock.now().0
        } else {
            0
        };
        self.bus
            .sync_read_by_id_into("Present_Position", ids, &mut self.pos_raw[..ids.len()])?;
        let mut candidate_anchor = None;
        if self.raw_mixed() {
            if self
                .clock
                .now()
                .0
                .checked_sub(sampled_ns)
                .is_none_or(|age| age > 200_000_000)
            {
                return Err(FeetechError::Aborted(
                    robot_core::serial_services::AbortReason::Timeout,
                ));
            }
            candidate_anchor = self.validate_raw_mixed_anchor(sampled_ns)?;
        }

        // 构造 Goal entries = Present
        self.cmd_entries.clear();
        for (i, &id) in ids.iter().enumerate() {
            self.cmd_entries.push((id, self.pos_raw[i]));
        }

        // 2. 写入 + readback + retry
        for attempt in 1..=SYNC_GOAL_MAX_ATTEMPTS {
            self.bus
                .sync_write_by_id("Goal_Position", &self.cmd_entries)?;
            self.sleep_or_abort(SYNC_GOAL_READBACK_WAIT)?;
            self.bus
                .sync_read_by_id_into("Goal_Position", ids, &mut self.goal_raw[..ids.len()])?;

            // 比较 Goal (readback) vs Present (期望值) —— 允许 ±SYNC_GOAL_TOLERANCE
            // tick 的微小偏差（motor 内部编码精度噪声）
            let mut mismatch: Option<(u8, i32, i32)> = None;
            for (i, &id) in ids.iter().enumerate() {
                let wanted = self.pos_raw[i];
                let got = self.goal_raw[i];
                if (self.raw_mixed() && got != wanted) || (got - wanted).abs() > SYNC_GOAL_TOLERANCE
                {
                    mismatch = Some((id, wanted, got));
                    break;
                }
            }

            let max = SYNC_GOAL_MAX_ATTEMPTS;
            match mismatch {
                None => {
                    if self.raw_mixed() {
                        self.mixed_arm_anchor = candidate_anchor;
                    }
                    if attempt > 1 {
                        self.record(2, 2, 0, attempt as u8, 0);
                    }
                    return Ok(());
                }
                Some((id, wanted, got)) => {
                    let diff = got - wanted;
                    self.record(2, 1, id, attempt as u8, diff.unsigned_abs());
                    if attempt == max || self.raw_mixed() {
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
        if self.mixed_bus2.is_some() {
            return Err(RobotError::Unsupported(
                "混合总线保持须走S0有界反馈与轮零速",
            ));
        }
        self.sync_goal_to_present().map_err(|e| {
            self.record_error(2, &e);
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
        let mut first_failure = None;
        for index in 0..self.joint_names.len() {
            for attempt in 1..=TORQUE_ENABLE_MAX_ATTEMPTS {
                if on && self.raw_mixed() {
                    let fresh = self
                        .mixed_arm_anchor
                        .and_then(|(_, stamp)| self.clock.now().0.checked_sub(stamp))
                        .is_some_and(|age| age <= 200_000_000);
                    if !fresh {
                        return Err(RobotError::Aborted(
                            robot_core::serial_services::AbortReason::Timeout,
                        ));
                    }
                }
                match self
                    .bus
                    .write("Torque_Enable", &self.joint_names[index], target)
                {
                    Ok(()) => {
                        if attempt > 1 {
                            self.record(3, 2, self.joint_ids[index], attempt as u8, 0);
                        }
                        break;
                    }
                    Err(error) => {
                        self.record(
                            3,
                            6,
                            self.joint_ids[index],
                            error.comm_kind().unwrap_or(CommErrorKind::Unknown).to_u8(),
                            0,
                        );
                        self.record_error(3, &error);
                        // 只有设备 NACK 可重试；通信、抢占和永久参数问题立即定级返回。
                        if (on && self.raw_mixed())
                            || !matches!(error, FeetechError::StatusError(_))
                        {
                            return Err(error.into());
                        }
                        self.record(3, 1, self.joint_ids[index], attempt as u8, 0);
                        if attempt == TORQUE_ENABLE_MAX_ATTEMPTS {
                            if first_failure.is_none() {
                                first_failure = Some(error);
                            }
                        } else {
                            self.sleep_or_abort(TORQUE_ENABLE_RETRY_DELAY)
                                .map_err(RobotError::from)?;
                        }
                    }
                }
            }
        }
        match first_failure {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }

    /// **task-30 v9 / M4**：emergency 路径专用快速断扭。
    ///
    /// 一次 broadcast `sync_write Torque_Enable=0`（**不**等 ACK，**不**重试）
    /// + 预分配 `safety_stop_entries` / `safety_stop_frame_buf`。13 motor 一次
    ///   wire 帧 ≈ 4-6 ms RTT，远低于 SLA `< 10ms`（pre-1d baseline）。
    ///
    /// 与 `enable(false)` 的对比：
    /// - `enable(false)`：per-motor `Torque_Enable=0` 写 × 3 次重试 × 150 ms =
    ///   13 × 3 × 150 ms ≈ 5.85s。**不能**做 emergency。
    /// - `safety_stop_fast`：单 broadcast → < 10 ms。**emergency 路径专用**，
    ///   失败也不重试（再失败由 confirm_torque_off 升 `StopFailed`）。
    ///
    /// 构造结束即拥有完整停止列表，允许在 `connect()` 握手之前发送首次 Safety。
    /// 路径**不分配堆**（`assert_no_alloc::assert_no_alloc(...)` 包裹下不 panic）。
    ///
    /// # Errors
    ///
    /// - 透传 `sync_write_by_id_into` 的协议 / transport 错误。
    /// - transport 未打开或停止目标为空 → `NotConnected`，绝不发空广播冒充成功。
    pub fn safety_stop_fast(&mut self) -> Result<(), FeetechError> {
        self.mixed_zero_target_confirmed = false;
        self.bus.set_op_class(BusOpClass::Safety);
        // 构造期已填完整目标；首次广播只要求物理 transport 已打开。
        if !self.bus.is_connected()
            || self.safety_stop_entries.is_empty()
            || self.safety_stop_entries.len() != self.joint_ids.len()
        {
            return Err(FeetechError::NotConnected);
        }
        // 非 RT scope（serial_io_thread），但路径整体不分配堆。**`borrow_mut`
        // 双重借用避坑**：先把字段切出来用临时 mutable 引用 + transport，
        // 让 sync_write_by_id_into 同时借用 self.bus 与 self.safety_stop_frame_buf。
        // Rust 借用检查接受这个 pattern：两个字段独立 mut borrow。
        let entries = &self.safety_stop_entries;
        let frame_buf = &mut self.safety_stop_frame_buf;
        self.bus
            .sync_write_by_id_into("Torque_Enable", entries, frame_buf)
    }
}

impl<T: SerialTransport + 'static> ManipulatorAdapter for FeetechAdapter<T> {
    fn info(&self) -> AdapterInfo {
        self.info
    }

    fn connect(&mut self) -> Result<(), RobotError> {
        self.mixed_zero_target_confirmed = false;
        self.mixed_zero_observation_last_ns = None;
        self.mixed_motion_observation_last_ns = None;
        let class = self
            .preempt
            .as_ref()
            .map_or(BusOpClass::NormalEffectful, |probe| probe.operation_class());
        self.bus.set_op_class(class);
        self.connected = false;
        self.bus.connect(true).map_err(|error| {
            self.record_error(1, &error);
            RobotError::from(error)
        })?;
        self.verify_mixed_bus2()?;
        self.connected = true;
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), RobotError> {
        self.mixed_zero_target_confirmed = false;
        self.mixed_zero_observation_last_ns = None;
        self.mixed_motion_observation_last_ns = None;
        self.bus.set_op_class(BusOpClass::NormalEffectful);
        self.connected = false;
        self.bus.disconnect();
        Ok(())
    }

    fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError> {
        self.require_connected().map_err(RobotError::from)?;
        self.bus.set_op_class(BusOpClass::PeriodicRead);
        if self.raw_mixed() && self.diag.is_some() {
            self.read_raw_mixed_state().map_err(|error| {
                self.record_error(4, &error);
                RobotError::from(error)
            })?;
        } else {
            self.bus
                .sync_read_by_id_into("Present_Position", &self.joint_ids, &mut self.pos_raw)
                .map_err(|e| {
                    self.record_error(4, &e);
                    RobotError::from(e)
                })?;
            self.bus
                .sync_read_by_id_into("Present_Velocity", &self.joint_ids, &mut self.vel_raw)
                .map_err(|e| {
                    self.record_error(4, &e);
                    RobotError::from(e)
                })?;
        }

        let n = self.joint_ids.len();
        for i in 0..n {
            out.positions[i] = raw_to_rad(self.pos_raw[i]);
            if self.mixed_bus2.is_some() {
                // 混合实例尚未标定SI速度，且没有力矩传感；不能发布raw或伪零。
                // 原始寄存器缓冲仍保留，停止确认和诊断继续使用各自的真实读回。
                out.velocities[i] = f32::NAN;
                out.torques[i] = f32::NAN;
            } else {
                // 旧位置实例保留历史反馈行为，本次不扩大修改范围。
                out.velocities[i] = self.vel_raw[i] as f32;
                out.torques[i] = 0.0;
            }
        }
        out.joint_count = self.joint_count;
        // stamp_ns 由调用方填（trait 文档约定：adapter 不持时钟）
        self.observe_raw_mixed_zero().map_err(|error| {
            self.record_error(4, &error);
            RobotError::from(error)
        })?;
        Ok(())
    }

    fn write_command(&mut self, cmd: &JointCommandView) -> Result<(), RobotError> {
        self.require_connected().map_err(RobotError::from)?;
        if self.mixed_bus2.is_some() {
            return self.write_mixed_bus2(cmd);
        }
        if matches!(
            cmd.selection,
            robot_core::adapter::JointSelection::MixedPositionRawVelocity { .. }
        ) {
            return Err(RobotError::Unsupported("混合目标需要显式混合总线构造"));
        }
        cmd.validate_selection(self.joint_ids.len())?;
        if cmd.selection != robot_core::adapter::JointSelection::All {
            cmd.validate_selection_time(self.clock.now().0)?;
        }
        self.bus.set_op_class(BusOpClass::PeriodicWrite);
        match cmd.mode {
            ControlMode::Position => {}
            // Stop 的真正断扭沿现有 safety_stop 路径执行，这里保持零事务。
            ControlMode::Stop => return Ok(()),
            ControlMode::Velocity | ControlMode::Torque | ControlMode::ServoPosition => {
                self.record(5, 3, 0, 0, 0);
                return Err(RobotError::Unsupported(
                    "feetech: write_command 不支持此控制模式",
                ));
            }
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
            if !cmd.selection.contains(i) {
                continue;
            }
            let mut raw = rad_to_raw(cmd.positions[i]);
            if use_clamp {
                let (min, max) = effective_calibration_range_raw(
                    &self.calibrations[i],
                    self.calibration_range_tolerances_raw[i],
                );
                raw = clamp_raw(raw, min, max);
            }
            self.cmd_entries.push((self.joint_ids[i], raw));
        }
        self.bus
            .sync_write_by_id("Goal_Position", &self.cmd_entries)
            .map_err(|e| {
                self.record_error(5, &e);
                RobotError::from(e)
            })
    }

    fn set_mode(&mut self, mode: ControlMode) -> Result<(), RobotError> {
        self.require_connected().map_err(RobotError::from)?;
        if self.mixed_bus2.is_some() && mode != ControlMode::Stop {
            return Err(RobotError::Unsupported("混合总线禁止整总线模式切换"));
        }
        self.bus.set_op_class(BusOpClass::NormalEffectful);
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
                    self.record_error(6, &e);
                    RobotError::from(e)
                })?;
        }
        Ok(())
    }

    fn enable(&mut self, on: bool) -> Result<(), RobotError> {
        self.mixed_zero_target_confirmed = false;
        self.require_connected().map_err(RobotError::from)?;
        self.bus.set_op_class(BusOpClass::NormalEffectful);
        if on {
            // task 29 commit 2：1:1 移植 dimos `_manipulator_base.py:468-504` 防飞序列
            //   1) sync_read Present_Position for all
            //   2) sync_write Goal_Position <- Present (让 target = current)
            //   3) sleep 50ms 让总线消化 sync_write
            //   4) per-motor 写 Torque_Enable=1 带最多 3 次重试
            // 目的：Torque 接上瞬间 target == current → 电机不动，避开 Goal 还
            // 是旧值导致的"飞向 0"风险。
            self.sync_goal_to_present().map_err(|e| {
                self.record_error(3, &e);
                RobotError::from(e)
            })?;
            if let Some(layout) = self.mixed_bus2 {
                let zeros = layout.wheel_ids.map(|id| (id, 0));
                self.bus
                    .sync_write_by_id("Goal_Velocity", &zeros)
                    .map_err(RobotError::from)?;
            }
            self.sleep_or_abort(ENABLE_SYNC_SETTLE)
                .map_err(RobotError::from)?;

            self.write_torque_enable_with_retry(true)
        } else {
            // disable：直接写 0，不需要 goal=present 序列；为一致性也加 3x 重试
            self.write_torque_enable_with_retry(false)
        }
    }

    fn safety_stop(&mut self) -> Result<(), RobotError> {
        // task-30 v9 / M4：emergency 入口必须走 fast path（一次 broadcast，
        // < 10ms RTT），**不**能落到 enable(false) 的 per-motor 3× 重试 ×
        // 150ms = ~5.85s 慢路径。SerialRT executor (M6) 在 emergency 路径
        // 唯一调用本方法。
        //
        // STS3215 没"硬件级 ESTOP"指令；最强 software stop = 全部
        // Torque_Enable=0（broadcast 单帧）。真硬件级安全停（断电）由上层
        // GPIO / 继电器控制，v0.2 不在 adapter 内 wire。
        //
        // confirm_torque_off 由 SerialIoCtrlClient 下 tick 调
        // `OutputSafetyProbe::read_torque_enable_mask` 验证；本方法只负责下发。
        self.safety_stop_fast().map_err(|e| {
            self.record_error(7, &e);
            RobotError::from(e)
        })
    }

    fn limits(&self) -> &JointLimits {
        &self.limits
    }
}

// ============================================================================
// OutputSafetyProbe（task-30 v9 / M3）
// ============================================================================
//
// SerialRT 路径：emergency safety_stop 后下 tick 由 serial_io_thread 调本 trait
// 确认硬件真断扭。读 Torque_Enable 寄存器（addr 40, 1 byte）；==0 则 motor
// torque off 已确认。13 motor sync_read 一次 RTT，预期 < 50ms（pre-1d
// baseline SLA）。

impl<T: SerialTransport + 'static> OutputSafetyProbe for FeetechAdapter<T> {
    fn read_torque_enable_mask(&mut self, out: &mut [bool]) -> Result<(), RobotError> {
        let n = self.joint_count as usize;
        if out.len() < n {
            return Err(RobotError::InvalidParameter(
                "OutputSafetyProbe out len < joint_count",
            ));
        }
        if self.joint_ids.len() != n || n == 0 || !self.bus.is_connected() {
            return Err(FeetechError::NotConnected.into());
        }
        self.bus.set_op_class(BusOpClass::Confirm);
        if self
            .preempt
            .as_ref()
            .is_some_and(|probe| !probe.confirmation_trusted())
        {
            match self.bus.confirm_hygiene() {
                Ok(report) => self.record(
                    8,
                    4,
                    u8::from(report.deadline_hit),
                    0,
                    u32::try_from(report.bytes).unwrap_or(u32::MAX),
                ),
                Err(error) => {
                    // 先发布安全事实，仍继续本轮 probe；probe 成功不能抹掉 hygiene 错误。
                    if let Some(probe) = self.preempt.as_ref() {
                        probe.report_comm_error(CommErrorKind::DiscardFailed);
                    }
                    self.record_error(8, &error);
                }
            }
        }
        // sync_read Torque_Enable 一次 wire RTT；torque_enable_raw 已在 new()
        // 预分配 vec![0; joint_count]，本路径不 alloc。
        self.bus
            .sync_read_by_id_into(
                "Torque_Enable",
                &self.joint_ids,
                &mut self.torque_enable_raw,
            )
            .map_err(|e| {
                self.record_error(8, &e);
                RobotError::from(e)
            })?;
        // 解码：== 0 → torque off (false)；其它（== 1 通常）→ torque on (true)。
        for (i, raw) in self.torque_enable_raw[..n].iter().enumerate() {
            out[i] = *raw != 0;
        }
        Ok(())
    }

    fn joint_count(&self) -> usize {
        self.joint_count as usize
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    // 换算常量已挪 `crate::convert`；本模块测试仍以 TAU/4096 手算 ground truth。
    use crate::protocol::checksum;
    use crate::transport::{MockHandle, MockTransport};
    use crate::types::Motor;
    use std::collections::HashMap;
    use std::f32::consts::TAU;
    use std::sync::Mutex;

    #[derive(Default)]
    struct CapturedDiagnostics(Mutex<Vec<DiagEvent>>);

    impl DiagSink for CapturedDiagnostics {
        fn record(&self, event: DiagEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    /// 命令分派拒绝和 Stop 空操作不得进入总线事务或权限服务。
    struct NoTransactionProbe;

    impl robot_core::serial_services::PreemptProbe for NoTransactionProbe {
        fn should_abort(
            &self,
            _class: BusOpClass,
        ) -> Option<robot_core::serial_services::AbortReason> {
            panic!("零事务命令不得查询中止状态")
        }

        fn checkpoint(&self) {
            panic!("零事务命令不得进入 IO 检查点")
        }

        fn sleep_or_abort(
            &self,
            _class: BusOpClass,
            _dur: DurationNs,
        ) -> Result<(), robot_core::serial_services::AbortReason> {
            panic!("零事务命令不得等待")
        }

        fn operation_class(&self) -> BusOpClass {
            panic!("零事务命令不得查询操作域")
        }

        fn confirmation_trusted(&self) -> bool {
            panic!("零事务命令不得查询确认信任")
        }

        fn report_comm_error(&self, _kind: CommErrorKind) {
            panic!("命令拒绝不得伪装成通信错误")
        }
    }

    impl LeasedPreemptProbe for NoTransactionProbe {
        fn begin_leased(
            &self,
            _class: BusOpClass,
        ) -> Result<robot_core::serial_services::BusLease, robot_core::serial_services::AbortReason>
        {
            panic!("零事务命令不得申请租约")
        }

        fn finish_leased(&self, _lease: robot_core::serial_services::BusLease) {
            panic!("零事务命令没有租约可归还")
        }
    }

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
        make_adapter_with_calibration_tolerances(joints, calibrations, Vec::new())
    }

    fn make_adapter_with_calibration_tolerances(
        joints: &[(&str, u8)],
        calibrations: Vec<MotorCalibration>,
        calibration_range_tolerances_raw: Vec<u16>,
    ) -> (FeetechAdapter<MockTransport>, MockHandle) {
        let (transport, handle) = MockTransport::new();
        let mut motors = HashMap::new();
        for (name, id) in joints {
            motors.insert((*name).to_string(), Motor::new(*id).unwrap());
        }
        let bus = FeetechBus::new(transport, motors).unwrap();
        let mut adapter = FeetechAdapter::new(
            FeetechAdapterConfig {
                clock: Arc::new(robot_core::time::MockClock::default()),
                preempt: None,
                diag: None,
                call_deadline: Duration::from_millis(50),
                adapter_name: "feetech_sts3215_test",
                joint_names: joints.iter().map(|(n, _)| (*n).to_string()).collect(),
                calibrations,
                calibration_range_tolerances_raw,
            },
            bus,
        )
        .unwrap();
        // handshake：connect() 对每个 motor 调 ping（PING ack + READ Model_Number）
        // 总线握手按 ID 排序，配置槽位可独立重排。
        let mut handshake_ids: Vec<_> = joints.iter().map(|&(_, id)| id).collect();
        handshake_ids.sort_unstable();
        for id in handshake_ids {
            handle.push_rx(&ack_status(id));
            handle.push_rx(&read_status(id, &[0x09, 0x03])); // 777 LE
        }
        adapter.connect().unwrap();
        let _ = handle.drain_tx();
        (adapter, handle)
    }

    pub(super) fn ack_status(id: u8) -> Vec<u8> {
        let mut out = vec![0xFF, 0xFF, id, 0x02, 0x00];
        out.push(checksum(&out[2..]));
        out
    }

    pub(super) fn read_status(id: u8, params: &[u8]) -> Vec<u8> {
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
    fn info_reports_serial_rt_manipulator() {
        // task-30 pre-1b：feetech rt_class 已从 HardRT 改成 SerialRT。
        let (a, _h) = make_adapter(&[("j1", 1)]);
        let info = a.info();
        assert_eq!(info.kind, AdapterKind::Manipulator);
        assert_eq!(info.rt_class, RtClass::SerialRT);
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
    fn enable_false_stops_on_first_communication_error_without_retry_or_next_motor() {
        let (mut adapter, handle) = make_adapter(&[("j1", 1), ("j2", 2)]);
        assert_eq!(
            adapter.enable(false),
            Err(RobotError::Comm(CommErrorKind::Timeout))
        );
        let mut expected = [0; 16];
        let size = crate::protocol::build_write(1, 40, &[0], &mut expected).unwrap();
        assert_eq!(handle.drain_tx(), expected[..size]);
    }

    #[test]
    fn enable_true_does_not_retry_a_failed_initial_present_read() {
        let (mut adapter, handle) = make_adapter(&[("j1", 1)]);
        assert_eq!(
            adapter.enable(true),
            Err(RobotError::Comm(CommErrorKind::Timeout))
        );
        let mut expected = [0; 16];
        let size = crate::protocol::build_sync_read(56, 2, &[1], &mut expected).unwrap();
        assert_eq!(handle.drain_tx(), expected[..size]);
    }

    #[test]
    fn torque_nack_retry_has_three_attempts_and_preserves_first_device_failure() {
        let (mut adapter, handle) = make_adapter(&[("j1", 1), ("j2", 2)]);
        for _ in 0..3 {
            handle.push_rx(&read_status_with_error(1, 1));
        }
        handle.push_rx(&ack_status(2));
        assert!(matches!(
            adapter.enable(false),
            Err(RobotError::HardwareFault(_))
        ));
        let transmitted = handle.drain_tx();
        // 每个 write Torque_Enable 为八字节；设备 NACK 封顶后仍处理下一个 motor。
        assert_eq!(transmitted.len(), 4 * 8);
        assert_eq!(
            transmitted
                .as_chunks::<8>()
                .0
                .iter()
                .map(|frame| frame[2])
                .collect::<Vec<_>>(),
            [1, 1, 1, 2]
        );
    }

    #[test]
    fn torque_communication_failure_diagnostic_retains_motor_and_error_kind() {
        let (mut adapter, handle) = make_adapter(&[("j1", 7), ("j2", 42)]);
        let diag = Arc::new(CapturedDiagnostics::default());
        adapter.diag = Some(diag.clone());
        handle.push_rx(&ack_status(7));
        assert_eq!(
            adapter.enable(false),
            Err(RobotError::Comm(CommErrorKind::Timeout))
        );
        let events = diag.0.lock().unwrap();
        let failures: Vec<_> = events.iter().filter(|event| event.kind == 6).collect();
        assert_eq!(failures.len(), 1);
        assert_eq!(
            (
                failures[0].site,
                failures[0].a,
                failures[0].b,
                failures[0].seq_lo
            ),
            (3, 42, 0, 0)
        );
    }

    #[test]
    fn torque_device_status_diagnostic_retains_motor_status_and_retry_count() {
        let (mut adapter, handle) = make_adapter(&[("j1", 19), ("j2", 42)]);
        let diag = Arc::new(CapturedDiagnostics::default());
        adapter.diag = Some(diag.clone());
        for _ in 0..3 {
            handle.push_rx(&read_status_with_error(19, 0x24));
        }
        for _ in 0..3 {
            handle.push_rx(&read_status_with_error(42, 0x08));
        }
        assert!(matches!(
            adapter.enable(false),
            Err(RobotError::HardwareFault(_))
        ));
        let events = diag.0.lock().unwrap();
        let statuses: Vec<_> = events.iter().filter(|event| event.kind == 5).collect();
        assert_eq!(statuses.len(), 6);
        for (status, (motor, byte)) in statuses.into_iter().zip([
            (19, 0x24),
            (19, 0x24),
            (19, 0x24),
            (42, 0x08),
            (42, 0x08),
            (42, 0x08),
        ]) {
            assert_eq!(
                (status.site, status.a, status.b, status.seq_lo),
                (3, motor, byte, 0)
            );
        }
        let failures: Vec<_> = events.iter().filter(|event| event.kind == 6).collect();
        assert_eq!(failures.len(), 6);
        for (failure, motor) in failures.into_iter().zip([19, 19, 19, 42, 42, 42]) {
            assert_eq!((failure.site, failure.a, failure.b), (3, motor, 63));
        }
        assert_eq!(events.iter().filter(|event| event.kind == 1).count(), 6);
    }

    #[test]
    fn sync_read_device_status_diagnostic_identifies_the_failed_response_motor() {
        let (mut adapter, handle) = make_adapter(&[("j1", 7), ("j2", 42)]);
        let diag = Arc::new(CapturedDiagnostics::default());
        adapter.diag = Some(diag.clone());
        handle.push_rx(&read_status(7, &[0x00, 0x08]));
        let mut failed_response = read_status(42, &[0x00, 0x08]);
        failed_response[4] = 0x20;
        let checksum_offset = failed_response.len() - 1;
        failed_response[checksum_offset] = checksum(&failed_response[2..checksum_offset]);
        handle.push_rx(&failed_response);
        assert!(matches!(
            adapter.read_state(&mut JointStateBuf::default()),
            Err(RobotError::HardwareFault(_))
        ));
        let events = diag.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        let event = events[0];
        assert_eq!(
            (event.site, event.kind, event.a, event.b, event.seq_lo),
            (4, 5, 42, 0x20, 0)
        );
    }

    fn read_status_with_error(id: u8, error: u8) -> Vec<u8> {
        let mut response = vec![0xff, 0xff, id, 2, error];
        response.push(checksum(&response[2..]));
        response
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
    fn all_position_nonfinite_targets_are_rejected_without_writes() {
        use robot_core::adapter::{ControlModeRepr, JointCommandBuf, JointSelection};
        for calibrated in [false, true] {
            let calibrations = if calibrated {
                vec![MotorCalibration::new(1, 0, 0, 1000, 3000).unwrap()]
            } else {
                Vec::new()
            };
            let (mut adapter, handle) = make_adapter_with_calibration(&[("j1", 1)], calibrations);
            let mut command = JointCommandBuf {
                joint_count: 1,
                mode: ControlModeRepr::Position,
                selection: JointSelection::All,
                ..JointCommandBuf::default()
            };
            for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                command.positions[0] = invalid;
                assert!(matches!(
                    adapter.write_command(&command.as_view()),
                    Err(RobotError::InvalidParameter(_))
                ));
                assert_eq!(handle.drain_tx(), [0_u8; 0]);
            }
        }
    }

    #[test]
    fn all_position_large_finite_targets_keep_calibration_clamp() {
        use robot_core::adapter::{ControlModeRepr, JointCommandBuf, JointSelection};
        let cal = MotorCalibration::new(1, 0, 0, 1000, 3000).unwrap();
        let (mut adapter, handle) = make_adapter_with_calibration(&[("j1", 1)], vec![cal]);
        let mut command = JointCommandBuf {
            joint_count: 1,
            mode: ControlModeRepr::Position,
            selection: JointSelection::All,
            ..JointCommandBuf::default()
        };
        for (target, raw) in [(f32::MAX, 3000_u16), (-f32::MAX, 1000)] {
            command.positions[0] = target;
            adapter.write_command(&command.as_view()).unwrap();
            let [low, high] = raw.to_le_bytes();
            let mut expected = vec![0xff, 0xff, 0xfe, 0x07, 0x83, 0x2a, 0x02, 0x01, low, high];
            expected.push(checksum(&expected[2..]));
            assert_eq!(handle.drain_tx(), expected);
        }
        let (mut uncalibrated, handle) = make_adapter(&[("j1", 1)]);
        for target in [f32::MAX, -f32::MAX] {
            command.positions[0] = target;
            assert!(matches!(
                uncalibrated.write_command(&command.as_view()),
                Err(RobotError::InvalidParameter(_))
            ));
            assert_eq!(handle.drain_tx(), [0_u8; 0]);
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
    fn partial_position_wire_selects_only_head_ids_and_keeps_calibration_slot_order() {
        use robot_core::adapter::{ControlModeRepr, JointCommandBuf, JointSelection};
        for ids in [[1, 2, 3, 4, 5, 6, 7, 8], [8, 3, 6, 1, 7, 4, 2, 5]] {
            let names: Vec<_> = ids.iter().map(|id| format!("joint_{id}")).collect();
            let joints: Vec<_> = names
                .iter()
                .zip(ids)
                .map(|(name, id)| (name.as_str(), id))
                .collect();
            let calibrations = ids
                .iter()
                .map(|&id| {
                    MotorCalibration::new(id, 0, 0, 1000 + u16::from(id), 3000 + u16::from(id))
                        .unwrap()
                })
                .collect();
            let (mut adapter, handle) = make_adapter_with_calibration(&joints, calibrations);
            let mut command = JointCommandBuf {
                positions: [f32::NAN; robot_core::adapter::MAX_JOINTS],
                mode: ControlModeRepr::Position,
                joint_count: 8,
                deadline_ns: 1_000_000,
                ..JointCommandBuf::default()
            };
            let mut mask = 0;
            let mut expected = vec![0xff, 0xff, 0xfe, 10, 0x83, 42, 2];
            for (slot, &id) in ids.iter().enumerate() {
                if id == 7 || id == 8 {
                    mask |= 1 << slot;
                    command.positions[slot] = raw_to_rad(if id == 7 { 3500 } else { 0 });
                    let clamped = if id == 7 { 3007_u16 } else { 1008_u16 };
                    expected.push(id);
                    expected.extend_from_slice(&clamped.to_le_bytes());
                }
            }
            command.selection = JointSelection::Mask(mask);
            expected.push(checksum(&expected[2..]));
            adapter.write_command(&command.as_view()).unwrap();
            // 真 FeetechAdapter→FeetechBus→MockTransport；整份帧只有 ID7/8。
            assert_eq!(handle.drain_tx(), expected);

            // 部分位置写不改变全总线 safety_stop 和确认的目标集合。
            adapter.safety_stop().unwrap();
            let stop = handle.drain_tx();
            assert_eq!(&stop[..7], &[0xff, 0xff, 0xfe, 20, 0x83, 40, 1]);
            assert_eq!(
                stop[7..23]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|entry| (entry[0], entry[1]))
                    .collect::<Vec<_>>(),
                ids.map(|id| (id, 0))
            );
            for id in ids {
                handle.push_rx(&read_status(id, &[0]));
            }
            let mut enabled = [true; 8];
            adapter.read_torque_enable_mask(&mut enabled).unwrap();
            assert_eq!(enabled, [false; 8]);
            let confirm = handle.drain_tx();
            assert_eq!(&confirm[7..15], &ids);
        }
    }

    #[test]
    fn invalid_partial_commands_never_start_a_bus_transaction() {
        use robot_core::adapter::{ControlModeRepr, JointCommandBuf, JointSelection};
        let (mut adapter, handle) = make_adapter(&[("arm", 1), ("head", 7)]);
        adapter
            .bus
            .set_services(Some(Arc::new(NoTransactionProbe)), None);
        let valid = JointCommandBuf {
            mode: ControlModeRepr::Position,
            selection: JointSelection::Mask(2),
            joint_count: 2,
            deadline_ns: 1_000,
            ..JointCommandBuf::default()
        };
        let mut nonfinite = valid;
        nonfinite.positions[1] = f32::NAN;
        for invalid in [
            JointCommandBuf {
                selection: JointSelection::Mask(0),
                ..valid
            },
            JointCommandBuf {
                selection: JointSelection::Mask(4),
                ..valid
            },
            JointCommandBuf {
                joint_count: 1,
                ..valid
            },
            JointCommandBuf {
                stamp_ns: 1,
                ..valid
            },
            JointCommandBuf {
                deadline_ns: 0,
                ..valid
            },
            nonfinite,
        ]
        .into_iter()
        .chain(
            [
                ControlModeRepr::Velocity,
                ControlModeRepr::Torque,
                ControlModeRepr::ServoPosition,
                ControlModeRepr::Stop,
            ]
            .map(|mode| JointCommandBuf { mode, ..valid }),
        ) {
            assert!(adapter.write_command(&invalid.as_view()).is_err());
            assert_eq!(handle.drain_tx(), [] as [u8; 0]);
        }
        // 形状和窗口本身合法，但真实 adapter 入口时已到来源截止；仍不得申请事务。
        adapter.clock = Arc::new(robot_core::time::MockClock::new(
            robot_core::time::MonoRawNs(1_000),
        ));
        assert_eq!(
            adapter.write_command(&valid.as_view()),
            Err(RobotError::DeadlineExceeded)
        );
        assert_eq!(handle.drain_tx(), [] as [u8; 0]);
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
    fn per_joint_tolerances_drive_limits_and_position_clamp_without_changing_calibration() {
        let calibrations = (1..=3)
            .map(|id| MotorCalibration::new(id, 0, 0, 1000, 3000).unwrap())
            .collect::<Vec<_>>();
        let (mut adapter, handle) = make_adapter_with_calibration_tolerances(
            &[("head", 1), ("arm", 2), ("gripper", 3)],
            calibrations.clone(),
            vec![0, 20, 7],
        );
        for (index, (min, max)) in [(1000, 3000), (980, 3020), (993, 3007)]
            .into_iter()
            .enumerate()
        {
            assert_eq!(rad_to_raw(adapter.limits().position_min[index]), min);
            assert_eq!(rad_to_raw(adapter.limits().position_max[index]), max);
        }
        let mut command = robot_core::adapter::JointCommandBuf {
            joint_count: 3,
            mode: robot_core::adapter::ControlModeRepr::Position,
            ..robot_core::adapter::JointCommandBuf::default()
        };
        for (target, expected) in [(3021, [3000, 3020, 3007]), (979, [1000, 980, 993])] {
            command.positions[..3].fill(raw_to_rad(target));
            adapter.write_command(&command.as_view()).unwrap();
            let tx = handle.drain_tx();
            let actual = tx[7..tx.len() - 1]
                .as_chunks::<3>()
                .0
                .iter()
                .map(|entry| u16::from_le_bytes([entry[1], entry[2]]))
                .collect::<Vec<_>>();
            assert_eq!(actual, expected);
        }
        assert_eq!(adapter.calibrations, calibrations);
    }

    #[test]
    fn calibration_tolerances_keep_position_hard_bounds() {
        let (mut adapter, handle) = make_adapter_with_calibration_tolerances(
            &[("j1", 1)],
            vec![MotorCalibration::new(1, 0, 0, 10, 4085).unwrap()],
            vec![4095],
        );
        assert_eq!(rad_to_raw(adapter.limits().position_min[0]), 0);
        assert_eq!(rad_to_raw(adapter.limits().position_max[0]), 4095);
        let mut command = robot_core::adapter::JointCommandBuf {
            joint_count: 1,
            mode: robot_core::adapter::ControlModeRepr::Position,
            ..robot_core::adapter::JointCommandBuf::default()
        };
        for (target, expected) in [(-1, 0), (4096, 4095)] {
            command.positions[0] = raw_to_rad(target);
            adapter.write_command(&command.as_view()).unwrap();
            let tx = handle.drain_tx();
            assert_eq!(u16::from_le_bytes([tx[8], tx[9]]), expected);
        }
    }

    #[test]
    fn calibration_tolerance_configuration_is_validated_at_construction() {
        for tolerances in [Vec::new(), vec![4095], vec![0, 20], vec![4096]] {
            let (transport, handle) = MockTransport::new();
            let motors = HashMap::from([("j1".into(), Motor::new(1).unwrap())]);
            let bus = FeetechBus::new(transport, motors).unwrap();
            let result = FeetechAdapter::new(
                FeetechAdapterConfig {
                    clock: Arc::new(robot_core::time::MockClock::default()),
                    preempt: None,
                    diag: None,
                    call_deadline: Duration::from_millis(50),
                    adapter_name: "test",
                    joint_names: vec!["j1".into()],
                    calibrations: vec![MotorCalibration::identity(1)],
                    calibration_range_tolerances_raw: tolerances.clone(),
                },
                bus,
            );
            match tolerances.as_slice() {
                [] => assert_eq!(result.unwrap().calibration_range_tolerances_raw, [0]),
                [4095] => assert!(result.is_ok()),
                [0, 20] => assert!(matches!(
                    result,
                    Err(FeetechError::ByteLengthMismatch {
                        expected: 1,
                        actual: 2
                    })
                )),
                [4096] => assert!(matches!(
                    result,
                    Err(FeetechError::CalibrationRangeToleranceOutOfRange(4096))
                )),
                _ => unreachable!(),
            }
            assert_eq!(handle.drain_tx(), [] as [u8; 0]);
        }
        let (transport, handle) = MockTransport::new();
        let bus = FeetechBus::new(
            transport,
            HashMap::from([("j1".into(), Motor::new(1).unwrap())]),
        )
        .unwrap();
        let result = FeetechAdapter::new(
            FeetechAdapterConfig {
                clock: Arc::new(robot_core::time::MockClock::default()),
                preempt: None,
                diag: None,
                call_deadline: Duration::from_millis(50),
                adapter_name: "test",
                joint_names: vec!["j1".into()],
                calibrations: Vec::new(),
                calibration_range_tolerances_raw: vec![1],
            },
            bus,
        );
        assert!(matches!(
            result,
            Err(FeetechError::CalibrationToleranceWithoutCalibration)
        ));
        assert_eq!(handle.drain_tx(), [] as [u8; 0]);
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
                clock: Arc::new(robot_core::time::MockClock::default()),
                preempt: None,
                diag: None,
                call_deadline: Duration::from_millis(50),
                adapter_name: "test",
                joint_names: vec!["j1".into()],
                calibrations: vec![MotorCalibration::identity(1), MotorCalibration::identity(2)],
                calibration_range_tolerances_raw: Vec::new(),
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
    fn original_invalid_calibration_is_rejected_before_tolerance_or_io() {
        for tolerance in [0, 20] {
            for (min, max) in [(3000, 2990), (0, 4096)] {
                let (transport, handle) = MockTransport::new();
                let bus = FeetechBus::new(
                    transport,
                    HashMap::from([("j1".into(), Motor::new(1).unwrap())]),
                )
                .unwrap();
                let mut calibration = MotorCalibration::identity(1);
                calibration.range_min = min;
                calibration.range_max = max;
                let result = FeetechAdapter::new(
                    FeetechAdapterConfig {
                        clock: Arc::new(robot_core::time::MockClock::default()),
                        preempt: None,
                        diag: None,
                        call_deadline: Duration::from_millis(50),
                        adapter_name: "test",
                        joint_names: vec!["j1".into()],
                        calibrations: vec![calibration],
                        calibration_range_tolerances_raw: vec![tolerance],
                    },
                    bus,
                );
                assert!(matches!(
                    result,
                    Err(FeetechError::CalibrationRangeInverted { .. }
                        | FeetechError::CalibrationMaxOutOfRange(_))
                ));
                assert_eq!(handle.drain_tx(), [] as [u8; 0]);
            }
        }
    }

    #[test]
    fn calibration_id_mismatch_rejected_at_construction() {
        let (transport, handle) = MockTransport::new();
        let motors = HashMap::from([("j1".into(), Motor::new(1).unwrap())]);
        let bus = FeetechBus::new(transport, motors).unwrap();
        let calibration = MotorCalibration::new(99, 0, 0, 0, 4095).unwrap();
        let result = FeetechAdapter::new(
            FeetechAdapterConfig {
                clock: Arc::new(robot_core::time::MockClock::default()),
                preempt: None,
                diag: None,
                call_deadline: Duration::from_millis(50),
                adapter_name: "test",
                joint_names: vec!["j1".into()],
                calibrations: vec![calibration],
                calibration_range_tolerances_raw: Vec::new(),
            },
            bus,
        );
        assert!(matches!(
            result,
            Err(FeetechError::CalibrationMotorIdMismatch {
                expected: 1,
                got: 99
            })
        ));
        assert_eq!(handle.drain_tx(), [] as [u8; 0]);
    }

    #[test]
    fn read_state_decodes_positions_to_rad() {
        let (mut a, h) = make_adapter(&[("j1", 1)]);
        let diagnostics = Arc::new(CapturedDiagnostics::default());
        a.diag = Some(diagnostics.clone());
        let _ = h.drain_tx();
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
        let requests = h.drain_tx();
        assert_eq!(requests.len(), 18, "位置实例仍只发两个单电机 SYNC_READ");
        assert_eq!((requests[5], requests[14]), (56, 58));
        assert!(diagnostics.0.lock().unwrap().is_empty());
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
    fn write_command_stop_remains_noop_without_transaction_or_diagnostic() {
        let (mut a, h) = make_adapter(&[("j1", 1)]);
        let diag = Arc::new(CapturedDiagnostics::default());
        a.diag = Some(diag.clone());
        a.bus.set_services(Some(Arc::new(NoTransactionProbe)), None);
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
        assert!(diag.0.lock().unwrap().is_empty());
    }

    #[test]
    fn write_command_unsupported_modes_rejected_without_transaction() {
        for mode in [
            ControlMode::Velocity,
            ControlMode::Torque,
            ControlMode::ServoPosition,
        ] {
            let (mut adapter, handle) = make_adapter(&[("j1", 1)]);
            let diag = Arc::new(CapturedDiagnostics::default());
            adapter.diag = Some(diag.clone());
            adapter
                .bus
                .set_services(Some(Arc::new(NoTransactionProbe)), None);
            let command = robot_core::adapter::JointCommandBuf {
                joint_count: 1,
                mode: mode.into(),
                ..robot_core::adapter::JointCommandBuf::default()
            };
            assert_eq!(
                adapter.write_command(&command.as_view()),
                Err(RobotError::Unsupported(
                    "feetech: write_command 不支持此控制模式"
                ))
            );
            assert_eq!(handle.drain_tx(), [] as [u8; 0]);
            let events = diag.0.lock().unwrap();
            assert_eq!(events.len(), 1);
            let event = events[0];
            assert_eq!(
                (event.site, event.kind, event.a, event.b, event.seq_lo),
                (5, 3, 0, 0, 0)
            );
        }
    }

    #[test]
    fn write_command_disconnected_takes_priority_over_mode_dispatch() {
        let (mut adapter, handle) = make_adapter(&[("j1", 1)]);
        adapter.disconnect().unwrap();
        let diag = Arc::new(CapturedDiagnostics::default());
        adapter.diag = Some(diag.clone());
        adapter
            .bus
            .set_services(Some(Arc::new(NoTransactionProbe)), None);
        for mode in [
            ControlMode::Position,
            ControlMode::Velocity,
            ControlMode::Torque,
            ControlMode::ServoPosition,
            ControlMode::Stop,
        ] {
            let command = robot_core::adapter::JointCommandBuf {
                joint_count: 1,
                mode: mode.into(),
                ..robot_core::adapter::JointCommandBuf::default()
            };
            assert_eq!(
                adapter.write_command(&command.as_view()),
                Err(RobotError::Comm(CommErrorKind::Disconnected))
            );
        }
        assert_eq!(handle.drain_tx(), [] as [u8; 0]);
        assert!(diag.0.lock().unwrap().is_empty());
    }

    #[test]
    fn set_mode_torque_rejected() {
        let (mut a, _h) = make_adapter(&[("j1", 1)]);
        let err = a.set_mode(ControlMode::Torque).unwrap_err();
        assert!(matches!(err, RobotError::InvalidParameter(_)));
    }

    // ---- task-30 v9 / M3：OutputSafetyProbe ----

    /// safety_stop 后 confirm_torque_off 路径：sync_read Torque_Enable 全部 0
    /// → mask 全 false。
    #[test]
    fn output_safety_probe_all_off_confirmed() {
        let (mut a, h) = make_adapter(&[("j1", 1), ("j2", 2)]);
        // sync_read Torque_Enable：每 motor 1 byte = 0x00
        h.push_rx(&read_status(1, &[0x00]));
        h.push_rx(&read_status(2, &[0x00]));

        let mut mask = [true; 4];
        a.read_torque_enable_mask(&mut mask).unwrap();
        assert_eq!(&mask[..2], &[false, false]);

        // wire bytes 检查：READ_DATA 走 sync_read API（INST=0x82 SYNC_READ）
        let tx = h.drain_tx();
        // SYNC_READ pkt: FF FF FE LEN 82 ADDR LEN_BYTE id1 id2 CHK
        // ADDR = 40 = 0x28, LEN_BYTE = 1
        assert_eq!(tx[0..5], [0xFF, 0xFF, 0xFE, 0x06, 0x82]);
        assert_eq!(tx[5..6], [0x28]);
        assert_eq!(tx[6..7], [0x01]);
        assert_eq!(tx[7..9], [0x01, 0x02]);
    }

    /// 部分 motor 仍 torque on（混合状态）→ mask 反映实际：[true, false]。
    /// 这是 confirm_torque_off 升级 `OutputSafetyState::AdapterUncertain`
    /// 或 `StopFailed` 的判别基础（serial_io_thread 在上层做）。
    #[test]
    fn output_safety_probe_mixed_state() {
        let (mut a, h) = make_adapter(&[("j1", 1), ("j2", 2)]);
        h.push_rx(&read_status(1, &[0x01])); // motor 1: torque on
        h.push_rx(&read_status(2, &[0x00])); // motor 2: torque off
        let mut mask = [false; 2];
        a.read_torque_enable_mask(&mut mask).unwrap();
        assert_eq!(mask, [true, false]);
    }

    #[test]
    fn output_safety_probe_short_out_rejected() {
        let (mut a, _h) = make_adapter(&[("j1", 1), ("j2", 2)]);
        let mut mask = [false; 1]; // 太短
        let err = a.read_torque_enable_mask(&mut mask).unwrap_err();
        assert_eq!(
            err,
            RobotError::InvalidParameter("OutputSafetyProbe out len < joint_count")
        );
    }

    #[test]
    fn output_safety_probe_joint_count_reports() {
        let (a, _h) = make_adapter(&[("j1", 1), ("j2", 2), ("j3", 3)]);
        assert_eq!(a.joint_count(), 3);
    }

    // ---- task-30 v9 / M4：safety_stop_fast + sync_write_by_id_into ----

    /// `safety_stop_fast` 一次 broadcast `sync_write Torque_Enable=0`（13 motor
    /// 一次 wire 帧）。本单测验证 wire 字节正确：broadcast id (0xFE) + addr 40 +
    /// length 1 + 全 0 data。
    #[test]
    fn safety_stop_fast_broadcasts_torque_disable() {
        let (mut a, h) = make_adapter(&[
            ("j1", 1),
            ("j2", 2),
            ("j3", 3),
            ("j4", 4),
            ("j5", 5),
            ("j6", 6),
            ("j7", 7),
            ("j8", 8),
            ("j9", 9),
            ("j10", 10),
            ("j11", 11),
            ("j12", 12),
            ("j13", 13),
        ]);
        a.safety_stop_fast().expect("safety_stop_fast");
        let tx = h.drain_tx();
        // SYNC_WRITE：FF FF FE LEN 83 ADDR LEN_BYTE [id1 data1 id2 data2 ...] CHK
        assert_eq!(tx[0..3], [0xFF, 0xFF, 0xFE], "broadcast preamble");
        assert_eq!(tx[4], 0x83, "instruction = SYNC_WRITE");
        assert_eq!(tx[5], 40, "addr = Torque_Enable (40)");
        assert_eq!(tx[6], 1, "length = 1 byte");
        // 13 entries：id 1..=13，每个 data byte = 0
        for i in 0..13 {
            let off = 7 + i * 2;
            assert_eq!(tx[off], (i + 1) as u8, "entry {i} id");
            assert_eq!(tx[off + 1], 0x00, "entry {i} data byte = 0");
        }
        // LEN = 2 (addr+length) + 13×(1+1) + 2 (inst+chksum) = 30
        // 由 build_instruction 写入 tx[3]
        // total params = 28；total tx len = 6 + 28 = 34；checksum 在末位
        assert_eq!(tx.len(), 34, "13-motor sync_write frame length");
    }

    /// `safety_stop_fast` 路径**不分配堆**。本单测把它包进 assert_no_alloc，
    /// 触发任何 alloc 即 panic。serial_io_thread emergency 路径必须满足。
    #[test]
    fn safety_stop_fast_is_alloc_free() {
        use assert_no_alloc::assert_no_alloc;

        let (mut a, _h) = make_adapter(&[
            ("j1", 1),
            ("j2", 2),
            ("j3", 3),
            ("j4", 4),
            ("j5", 5),
            ("j6", 6),
            ("j7", 7),
            ("j8", 8),
            ("j9", 9),
            ("j10", 10),
            ("j11", 11),
            ("j12", 12),
            ("j13", 13),
        ]);

        assert_no_alloc(|| {
            let _ = a.safety_stop_fast();
        });
    }

    /// `sync_write_by_id_into` 直接在 bus 层调；验证两阶段 borrow + 栈数组
    /// 路径**不分配堆**。
    #[test]
    fn sync_write_by_id_into_is_alloc_free() {
        use assert_no_alloc::assert_no_alloc;

        let (mut a, _h) = make_adapter(&[("j1", 1), ("j2", 2), ("j3", 3)]);
        let entries = [(1u8, 0i32), (2u8, 0i32), (3u8, 0i32)];
        let mut frame_buf = vec![0u8; 256];

        assert_no_alloc(|| {
            let _ = a
                .bus()
                .sync_write_by_id_into("Torque_Enable", &entries, &mut frame_buf);
        });
    }

    /// 长度超 MAX_JOINTS 拒绝（safety_stop_fast 应永远不命中此分支，但防御性
    /// 检查存在）。
    #[test]
    fn sync_write_by_id_into_rejects_too_many_motors() {
        let (mut a, _h) = make_adapter(&[("j1", 1)]);
        let huge = vec![(1u8, 0i32); robot_core::adapter::MAX_JOINTS + 1];
        let mut frame_buf = vec![0u8; 256];
        let err = a
            .bus()
            .sync_write_by_id_into("Torque_Enable", &huge, &mut frame_buf)
            .unwrap_err();
        assert!(matches!(err, FeetechError::ByteLengthMismatch { .. }));
    }

    /// **finding #5 lock 测试**：`frame_buf` 太小时返回的是
    /// `ByteLengthMismatch`（由 `build_instruction` 抛），**不是**
    /// `BadPacketLength`（后者只在协议层 `params.len() > MAX_PARAMS` 时出现）。
    /// 锁住这个错误类型，未来谁动了底层错误传播路径会立刻 fail。
    #[test]
    fn sync_write_by_id_into_short_frame_buf_returns_byte_length_mismatch() {
        let (mut a, _h) = make_adapter(&[("j1", 1), ("j2", 2)]);
        let entries = [(1u8, 0i32), (2u8, 0i32)];
        let mut tiny = vec![0u8; 4]; // 远不够装 sync_write 帧
        let err = a
            .bus()
            .sync_write_by_id_into("Torque_Enable", &entries, &mut tiny)
            .unwrap_err();
        match err {
            FeetechError::ByteLengthMismatch { expected, actual } => {
                assert!(expected > actual, "expected={expected} actual={actual}");
                assert_eq!(actual, 4);
            }
            other => panic!("expected ByteLengthMismatch, got {other:?}"),
        }
    }

    /// **finding #1 + #2 lock 测试**：`safety_stop()` 必须走快路径（一帧
    /// broadcast wire），**不**走 enable(false) 的 per-motor 重试。
    /// 验证：调一次 safety_stop()，wire 帧数 = 1（broadcast SYNC_WRITE）。
    #[test]
    fn safety_stop_uses_fast_path_one_broadcast_frame() {
        let (mut a, h) = make_adapter(&[("j1", 1), ("j2", 2), ("j3", 3)]);
        a.safety_stop().expect("safety_stop");
        let tx = h.drain_tx();
        // 慢路径会发 3 motor × 重试 多帧；快路径就 1 帧 broadcast。
        // 一帧 = 6 + 2 + 3×(1+1) = 14 byte。
        assert_eq!(tx.len(), 14, "fast path frame length");
        assert_eq!(tx[0..3], [0xFF, 0xFF, 0xFE], "broadcast preamble");
        assert_eq!(tx[4], 0x83, "INST = SYNC_WRITE");
        // 慢路径会有多个 INST = WriteData (0x03) 的帧。
    }

    /// 首次 Safety 在握手前也必须向全部构造期目标发送实际广播。
    #[test]
    fn safety_stop_fast_before_connect_sends_all_configured_motors() {
        let (transport, handle) = MockTransport::new();
        let motors = HashMap::from([
            ("j1".into(), Motor::new(1).unwrap()),
            ("j2".into(), Motor::new(2).unwrap()),
        ]);
        let bus = FeetechBus::new(transport, motors).unwrap();
        let mut adapter = FeetechAdapter::new(
            FeetechAdapterConfig {
                clock: Arc::new(robot_core::time::MockClock::default()),
                preempt: None,
                diag: None,
                call_deadline: Duration::from_millis(50),
                adapter_name: "test_unconnected",
                joint_names: vec!["j1".into(), "j2".into()],
                calibrations: Vec::new(),
                calibration_range_tolerances_raw: Vec::new(),
            },
            bus,
        )
        .unwrap();
        assert!(matches!(
            adapter.enable(true),
            Err(RobotError::Comm(CommErrorKind::Disconnected))
        ));
        assert_eq!(handle.drain_tx(), [] as [u8; 0]);
        adapter.safety_stop_fast().unwrap();
        let mut expected = [0; 32];
        let size = crate::protocol::build_sync_write(40, 1, &[(1, &[0]), (2, &[0])], &mut expected)
            .unwrap();
        assert_eq!(handle.drain_tx(), expected[..size]);
    }
}
