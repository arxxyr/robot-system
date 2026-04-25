//! Task-30 v9 / M1：`ExecutorCtrlClient` trait + 共享类型 + atomic 编码常量。
//!
//! 对应 `docs/designs/task-30_state_machine_adapter_link.md` §4 / §5。
//!
//! ## 角色
//!
//! `ipc_thread` 持 `Box<dyn ExecutorCtrlClient>`，在 state_machine transition
//! 时调 5 个方法（set_policy / safety_stop / request_normal /
//! poll_last_normal_resp / clear_safety_latch）；具体实现两份：
//!
//! - `HardRtCtrlClient`（M5，`robot-s0::rt_thread`）：单 packed `AtomicU64` slot
//!   + `RtSafeCtrl` 路径，rt_thread 主循环零 alloc / 零 Arc refcount。
//! - `SerialIoCtrlClient`（M6，`robot-s0::serial_io_thread`）：
//!   `crossbeam_channel` + `Arc<AtomicBool>` latch + confirm_torque_off RTT。
//!
//! 本模块**只**给类型 + 编码常量，不带任何具体后端实现 —— 保持 robot-core
//! 零 runtime 依赖（§4.2 依赖铁律）。
//!
//! ## RT 约束
//!
//! 模块内任何 helper 必须 `no-alloc / no-lock`；所有方法 inline 友好，可在
//! `assert_no_alloc!` 包裹下不 panic。`AdapterNormalReq` / `AdapterResp` /
//! `NormalReqStatus` 均 `Copy`（无 String / Vec / Box）；任何"复杂参数"语义
//! 走 `RobotError::Unsupported`。

// 本模块大量 doc-comment 用中文叙述引用本 crate 类型 / variant 名（如
// HardRT / OutputPolicy / safety_stop）。clippy doc_markdown 把所有 PascalCase
// 标识符一律视作"应反引号"，但中文上下文反而会让节奏 broken；这里整模块
// allow，每个公共项的类型引用按需用反引号即可。
#![allow(clippy::doc_markdown)]

use crate::adapter::{ControlMode, MAX_JOINTS};
use crate::error::RobotError;

// ============================================================================
// OutputPolicy（§5.1）
// ============================================================================

/// `ipc_thread` 通过 `set_policy` 下发的输出策略。`HardRT` 与 `SerialRT` 各自的
/// rt-loop / io-loop 在 hot path 读 atomic 决定**这一 tick 是否真往 motor 写**。
///
/// | 主态 | OutputPolicy | 行为 |
/// |---|---|---|
/// | Safe | `Disabled` | torque off / 无输出 |
/// | Ready | `ReadyHold` | torque on，但只读 / 不下发 goal |
/// | Enabled.Active | `ActiveForward` | 正常下发 latest_cmd |
/// | Enabled.CtrlLostHold | `HoldCurrent` | goal = 当前 present |
/// | Enabled.CtrlLostZero | `ZeroCommand` | mode=Stop |
/// | Enabled.CtrlLostStop | `StopTorque` | torque off（软停） |
/// | Fault / Stopping | `Disabled` 或 `StopTorque` | 由具体 transition 决定 |
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum OutputPolicy {
    #[default]
    Disabled = 0,
    ReadyHold = 1,
    ActiveForward = 2,
    HoldCurrent = 3,
    ZeroCommand = 4,
    StopTorque = 5,
}

impl OutputPolicy {
    /// 从 atomic u8 解码。**6 个有效值（0-5），其它 → `Disabled` 兜底**。
    /// 兜底语义：corrupted memory 或未来扩展时保守表现 = 不写 motor。
    #[must_use]
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::ReadyHold,
            2 => Self::ActiveForward,
            3 => Self::HoldCurrent,
            4 => Self::ZeroCommand,
            5 => Self::StopTorque,
            _ => Self::Disabled, // 0 / 6+ 均归 Disabled（保守）
        }
    }

    /// 编码到 atomic u8。
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        self as u8
    }
}

// ============================================================================
// OutputSafetyState（§5.3 + v6/v7 加 `StopCommanded` / v7 加 `AdapterUncertain`）
// ============================================================================

/// safety-stop 路径的"硬件实际状态"汇报。**与 `OutputPolicy` 正交**：policy
/// 是"上层意图"，safety_state 是"硬件实际"。emergency 后 ipc_thread 立即把
/// state 拍到 `Safe`（意图），但硬件是否真断扭由本枚举反映。
///
/// 由 `Arc<AtomicU8> safety_state_slot` 在 rt_thread / serial_io_thread 内部
/// 写、`pub_thread` 读到对外 `S0SafetyState` 字段。
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum OutputSafetyState {
    /// 启动期 / 断连后未恢复，硬件状态未知。
    #[default]
    TorqueUnknown = 0,
    /// 正常运行，torque on（adapter 已 `enable(true)` 且无 stop）。
    TorqueOn = 1,
    /// `safety_stop` 已**下发**（HardRT mmap 写完 / SerialRT broadcast 已发送），
    /// 但**未读回 Torque_Enable=0 确认**。SerialRT confirm_torque_off 路径会
    /// 停留在此状态最多若干 tick。HardRT 通常立即过渡到 `TorqueOffConfirmed`。
    StopCommanded = 2,
    /// `safety_stop` 完成 + 确认硬件已断扭（read Torque_Enable = 0 一致）。
    TorqueOffConfirmed = 3,
    /// `safety_stop` 调用返回 `Err` / 或 confirm 多次失败 → 硬件可能仍 torque on。
    /// state_machine 应升 `Fault(SafetyStopFailed)` 并通知人工现场确认。
    StopFailed = 4,
    /// **v7 新增**：confirm_torque_off 拿到部分 ack，部分 motor 没回包
    /// （比如串口 status timeout）。这里"硬件不可信"但又不是明确失败 → 介于
    /// `TorqueOffConfirmed` 与 `StopFailed` 之间。state_machine 把它视为
    /// `StopFailed` 等级处理（保守），但对外 `S0SafetyState::AdapterUncertain`
    /// 让运维知道这不是 hard-fail 而是 uncertain。
    AdapterUncertain = 5,
}

impl OutputSafetyState {
    /// 0-5 valid；其它 → `TorqueUnknown`（保守兜底）。
    #[must_use]
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::TorqueOn,
            2 => Self::StopCommanded,
            3 => Self::TorqueOffConfirmed,
            4 => Self::StopFailed,
            5 => Self::AdapterUncertain,
            _ => Self::TorqueUnknown,
        }
    }

    #[must_use]
    pub const fn to_u8(self) -> u8 {
        self as u8
    }
}

// ============================================================================
// AdapterNormalReq / AdapterEmergencyReq（§4 设计 + v6 atomic 化）
// ============================================================================

/// `ipc_thread` 通过 `request_normal` 排进 executor 的 "normal" 动作。
///
/// **必须 `Copy` + 可 atomic 编码**（u8 action code + u8 arg）—— rt_thread 不
/// 走 alloc / Box / Vec。复杂参数（如全 16 motor calibration override）必须
/// 改走非 RT 路径（如 `connect()` 重新走，由主线程做）；trait 收到则返
/// `NormalReqStatus::Unsupported`。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdapterNormalReq {
    /// adapter.connect()。仅 SerialRT 路径用，HardRT 通常 spawn 前已 connect。
    Connect,
    /// adapter.disconnect()。
    Disconnect,
    /// adapter.enable(on)。bool 编码到 arg byte。
    Enable(bool),
    /// adapter.set_mode(ControlMode)。ControlMode 编码到 arg byte。
    SetMode(ControlMode),
    /// 主动 `hold_current_position()`：sync_read Present → sync_write Goal。
    /// state_machine `Active → CtrlLostHold` transition 触发。
    HoldCurrent,
}

/// `ipc_thread` 通过 `safety_stop` 排进 executor 的 "emergency" 动作。当前
/// 只有 `SafetyStop` 一种；保留 enum 以便未来扩展（`HardEStop` / `BrakeOn` 等）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdapterEmergencyReq {
    SafetyStop,
}

// ============================================================================
// AdapterResp / AdapterErr（§4 + v9 加 SafetyLatched）
// ============================================================================

/// adapter 一条 normal action 的执行结果。`Copy` 强制无堆字段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdapterResp {
    Ok,
    Err(AdapterErr),
}

/// `AdapterResp::Err` 的细化分类，供 `commit_fault` 路径映射到 `FaultCode`。
///
/// **fault 映射规则**（finding #3 修订版，v9 §0.5 A3 收紧）：
/// - `SafetyLatched` → `FaultCode::AdapterErr`（**不是** `SafetyStopFailed`）。
///   语义：safety latch 拒了请求 = 正常保护行为；调用方编排错误（应先 ClearEStop
///   再 Enable）。`SafetyStopFailed` 留给"safety_stop / confirm_torque_off
///   下发或确认失败"这种**真硬件故障**场景。
/// - `Other` / `HardwareFault` / `Unsupported` / `NotConnected` /
///   `PreemptedByEmergency` → `FaultCode::AdapterErr`。
///
/// **`FaultCode::SafetyStopFailed` 来源**（finding #3 收紧）：
///
/// 1. `RtSafeCtrl::safety_stop_rt` / `ManipulatorAdapter::safety_stop` 返回 Err；
/// 2. `OutputSafetyProbe::read_torque_enable_mask` 持续显示 motor 仍 torque-on。
///
/// **不**来自 `AdapterErr::SafetyLatched`。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdapterErr {
    /// safety 已 latch，拒绝执行 Enable(true) / 其它 normal action。
    ///
    /// → 映射到 `FaultCode::AdapterErr`（v9 finding #3 修订）。这是**正常保护**：
    /// state_machine 应先 `Event::ClearEStop` → `clear_safety_latch()` 再排
    /// `Enable(true)`。如果在未清 latch 时收到 Enable 请求 = state_machine
    /// 编排错，需要排查；但**不**等同于"安全停止失败"。
    SafetyLatched,
    /// adapter 未 connect。
    NotConnected,
    /// 参数 / mode 不被本 adapter 支持。
    Unsupported,
    /// 硬件 fault（电机报错、总线错误）。
    HardwareFault,
    /// rt_thread 在执行新动作前被 emergency preempt（本 action 未跑）。
    PreemptedByEmergency,
    /// 其它（transport 层错误等）。映射到 `FaultCode::AdapterErr`。
    Other,
}

impl AdapterErr {
    /// 从 atomic u8 解码（rt_thread 写 last_completed.code，client 读取）。
    #[must_use]
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::NotConnected,
            2 => Self::Unsupported,
            3 => Self::HardwareFault,
            4 => Self::PreemptedByEmergency,
            5 => Self::SafetyLatched,
            _ => Self::Other,
        }
    }

    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Other => 0,
            Self::NotConnected => 1,
            Self::Unsupported => 2,
            Self::HardwareFault => 3,
            Self::PreemptedByEmergency => 4,
            Self::SafetyLatched => 5,
        }
    }
}

// ============================================================================
// NormalReqStatus（§4 + v9 §0.5 A3）
// ============================================================================

/// `request_normal` 的同步返回值。语义：**这次调用本身**的结果，不是 action
/// 在 adapter 侧的最终结果（后者要 `poll_last_normal_resp(seq)` 取）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NormalReqStatus {
    /// 已占据 slot 并分配 `seq`；调用方下 tick 用 `seq` 调
    /// `poll_last_normal_resp` 取最终结果。
    Submitted { seq: u64 },
    /// slot 被前一次未消费的请求占住；调用方应 retry（v9 errata 由 ipc_tick
    /// 1s 总超时兜底防永久 Busy）。
    Busy,
    /// 本 client 实现根本不支持本 action（HardRT 收到只能 SerialRT 才能跑的
    /// 复杂请求 / 编码失败）。
    ///
    /// **本 variant 的存在场景仅有一个**：state_machine 已经决定一个 effectful
    /// transition 并 `request_normal(...)` 排进 client，但 client 实现拒了。
    /// `ipc_tick` 看到本值 = 必须立即 `commit_fault(FaultCode::AdapterErr)` +
    /// `set_policy(Disabled)`（M7 errata §0.5 A3）；**不允许** retry / 降级 /
    /// 跳过。
    ///
    /// 与 [`crate::error::RobotError::Unsupported`] 的关系：
    /// - `RobotError::Unsupported` = 中性能力声明（"adapter 不支持本能力"），
    ///   触发 fault 与否由调用方决定。
    /// - `NormalReqStatus::Unsupported` = "中性能力声明 + 已经在 effectful
    ///   路径上"。在这个具体调用边界下，触发 fault 是强制的。
    /// - HardRT / SerialRT client 内部把 `RobotError::Unsupported` 包装成
    ///   `NormalReqStatus::Unsupported` 时同时承担"我现在在 effectful 路径"的
    ///   契约。
    ///
    /// 两者**不冲突**：抽象层不同，调用边界不同。
    Unsupported(&'static str),
}

// ============================================================================
// OutputSafetyProbe trait（§5.4 / M3）
// ============================================================================

/// SerialRT executor 在执行 `safety_stop` 后**下 tick** 调用，**独立**确认硬件
/// 真断扭。task-30 v9 §5.4。
///
/// ## 为什么独立路径而不是嵌入 `read_state`
///
/// - `read_state` 是 hot-path（每 rt tick 跑），加 Torque_Enable 字段会拖慢
///   sync_read packet（每 motor 多读 1 byte）。
/// - safety probe 是 cold-path（仅 EStop / safety_stop 后用），偶发 RTT OK。
/// - 类型隔离：观测（位置/速度）vs 安全状态（torque enable mask）。
///
/// ## 哪些 adapter 实现本 trait
///
/// - **必须实现**：`SerialRT` adapter（如 `FeetechAdapter`）—— serial_io_thread
///   会在 emergency 后下 tick 调用，把结果反映到 `OutputSafetyState::
///   {TorqueOffConfirmed, AdapterUncertain, StopFailed}`。
/// - **可选实现**：`HardRT` adapter（如 `MockHardRTAdapter`）—— rt_thread 不
///   调本 trait（mmap 写 torque-enable register 即时生效），但 mock 仍实现
///   方便单测把 SerialRT 路径"假装"在 HardRT 上跑。
/// - **不实现**：`SoftRT` adapter（无 motor torque 概念）。
///
/// ## RT 安全性
///
/// 调用方在 `serial_io_thread`（普通优先级，非 RT scope）。实现可走串口 RTT，
/// 但**不允许 alloc**：传入的 `out: &mut [bool]` 由调用方预分配，实现内部如需
/// 缓冲（如 Feetech sync_read 回包）也必须预分配（`connect()` 期 with_capacity）。
pub trait OutputSafetyProbe {
    /// 读所有 joint 的当前 `Torque_Enable` 寄存器到 `out[..joint_count]`。
    ///
    /// `out.len()` 必须 `>= joint_count`，否则返 [`RobotError::InvalidParameter`]。
    /// 每个 motor 回包 byte == 0 → `out[i] = false`（torque off），其它任何值
    /// → `out[i] = true`（torque on）。
    ///
    /// # Errors
    ///
    /// - 返回 [`RobotError::InvalidParameter`] 当 `out.len() < joint_count`；
    /// - 返回 [`RobotError::HardwareFault`] 当任一 motor 回包 status_error 非零；
    /// - 返回 [`RobotError::DeadlineExceeded`] 当串口 timeout（部分 motor 没回包）；
    ///   调用方据此判断 `OutputSafetyState::AdapterUncertain` vs `StopFailed`。
    fn read_torque_enable_mask(&mut self, out: &mut [bool]) -> Result<(), RobotError>;

    /// `out` slice 至少需要的长度（即 adapter 自身 `joint_count`），让调用方
    /// 可以预分配 `[bool; MAX_JOINTS]` 后用对应前缀。
    fn joint_count(&self) -> usize;
}

/// `Box<Inner>` 透传 blanket impl。
impl<Inner> OutputSafetyProbe for Box<Inner>
where
    Inner: OutputSafetyProbe + ?Sized,
{
    fn read_torque_enable_mask(&mut self, out: &mut [bool]) -> Result<(), RobotError> {
        (**self).read_torque_enable_mask(out)
    }
    fn joint_count(&self) -> usize {
        (**self).joint_count()
    }
}

/// `MAX_JOINTS` 上限的 torque-enable bitmask（栈分配）。serial_io_thread 在
/// confirm_torque_off 路径预分配一个，避免 cold-path 也分配堆。
pub type TorqueEnableMask = [bool; MAX_JOINTS];

// ============================================================================
// ExecutorCtrlClient trait（§4.1）
// ============================================================================

/// state_machine（`ipc_thread`）→ executor（`rt_thread` / `serial_io_thread`）
/// 的统一控制入口。**5 个方法**（v8/v9 对齐）；具体实现两份。
///
/// ## 线程模型
///
/// trait `Send + Sync`：`ipc_thread` 持 `Arc<dyn ExecutorCtrlClient>`，executor
/// 内部线程持 `Arc<...Inner>` 直接读 atomic / 收 channel；client 端无锁，
/// O(1) atomic 操作。
///
/// ## 不变量
///
/// 1. `set_policy` / `safety_stop` 永远成功（无返回值）—— 真正失败由后续
///    `poll_last_normal_resp` / `OutputSafetyProbe` 反映。
/// 2. `request_normal` 的 `Busy` **不**触发 retry 自动化；由 ipc_tick 自身的
///    重试策略 + 1s 总超时（v9 §0.5 A2）。
/// 3. `clear_safety_latch` 必须**同步可见**（atomic store）—— 不允许走
///    channel 异步动作（v9 §0 #2 修正）。
pub trait ExecutorCtrlClient: Send + Sync {
    /// 切换 OutputPolicy；rt-loop / io-loop 下个 tick 即生效。
    fn set_policy(&self, policy: OutputPolicy);

    /// 紧急 safety_stop。**fire-and-forget**：调用方不等结果，
    /// 由 `OutputSafetyState` 异步反映硬件状态。
    fn safety_stop(&self);

    /// 排进一条 normal action。返回值见 [`NormalReqStatus`]。
    fn request_normal(&self, req: AdapterNormalReq) -> NormalReqStatus;

    /// 查询给定 `seq` 的 normal action 结果：
    /// - `None`：还没执行完
    /// - `Some(AdapterResp::Ok)`：成功
    /// - `Some(AdapterResp::Err(_))`：失败（细化由 `AdapterErr`）
    ///
    /// **`seq` 与 `request_normal::Submitted{seq}` 一一对应**。`seq` 错配
    /// （比如 `seq < last_completed_seq` 表示已被覆盖）→ 实现可返回 `None` 或
    /// 旧结果，调用方应保证 `seq` 来源于最近一次 Submitted。
    fn poll_last_normal_resp(&self, seq: u64) -> Option<AdapterResp>;

    /// 清除 safety latch（由 `Event::ClearEStop` transition 调用）。
    ///
    /// **同步语义**：本方法返回后，下一次 `request_normal(Enable(true))` 必须
    /// 不再被 latch 拒。HardRT / SerialRT 实现都是 `safety_latched.store(false,
    /// Release)`（v9 §0 #2 修正）。
    fn clear_safety_latch(&self);
}

// ============================================================================
// Atomic 编码常量（HardRT slot 用 + last_completed 兼容）
// ============================================================================

/// `emergency_action: AtomicU8` 编码。
pub mod emergency_codes {
    pub const NONE: u8 = 0;
    pub const SAFETY_STOP: u8 = 1;
}

/// `pending_normal` packed AtomicU64 内 action 字段编码（u8）。
///
/// v9 设计：单 packed `AtomicU64` slot 装 (action: u8, arg: u8, seq: u48)。
/// 见 [`pack_pending_normal`] / [`unpack_pending_normal`].
pub mod normal_codes {
    pub const NONE: u8 = 0;
    pub const CONNECT: u8 = 1;
    pub const DISCONNECT: u8 = 2;
    pub const ENABLE: u8 = 3;
    pub const SET_MODE: u8 = 4;
    pub const HOLD_CURRENT: u8 = 5;
}

/// `last_completed` packed AtomicU64 内 result 字段编码（u8）。0 = Ok，
/// 其它见 [`AdapterErr::to_u8`]。
pub mod resp_codes {
    pub const OK: u8 = 0;
    /// 占位：Err 路径用 [`super::AdapterErr::to_u8`] + bias 一字节。
    /// `last_completed` 第 8 位 = `is_err` flag；下 8 位 = `AdapterErr::to_u8`。
    pub const ERR_FLAG: u64 = 1 << 8;
}

// ============================================================================
// pack/unpack helpers（v9 packed AtomicU64 layout）
// ============================================================================

/// 把 `(action, arg, seq)` 编码进单一 u64：
///
/// | bit | 字段 | 范围 |
/// |---|---|---|
/// | 0..=7 | action u8 | normal_codes::* |
/// | 8..=15 | arg u8 | enum 值（如 ControlMode） |
/// | 16..=63 | seq u48 | 单调递增请求号 |
///
/// **seq 实际上是 u48 上限 = 2^48 ≈ 2.8e14**；按每秒 1k req 算 = 8900 年，
/// 实用上不溢出。溢出由调用方自行决策（推荐：assert 或 wrap）。
#[must_use]
#[inline]
pub const fn pack_pending_normal(action: u8, arg: u8, seq: u64) -> u64 {
    let action_part = action as u64;
    let arg_part = (arg as u64) << 8;
    // 截断 seq 到 48 bits；高 16 位 = action+arg，低 48 位 = seq。
    let seq_part = (seq & 0x0000_FFFF_FFFF_FFFF) << 16;
    action_part | arg_part | seq_part
}

/// 解码 [`pack_pending_normal`] 的 inverse。
#[must_use]
#[inline]
pub const fn unpack_pending_normal(packed: u64) -> (u8, u8, u64) {
    let action = (packed & 0xFF) as u8;
    let arg = ((packed >> 8) & 0xFF) as u8;
    let seq = (packed >> 16) & 0x0000_FFFF_FFFF_FFFF;
    (action, arg, seq)
}

/// 把 `(seq, AdapterResp)` 编码进单一 u64（last_completed slot）：
///
/// | bit | 字段 |
/// |---|---|
/// | 0..=7 | err_code u8（0=Ok / `AdapterErr::to_u8`） |
/// | 8 | is_err flag |
/// | 16..=63 | seq u48 |
#[must_use]
#[inline]
pub const fn pack_completed(seq: u64, resp: AdapterResp) -> u64 {
    let (err_code, is_err) = match resp {
        AdapterResp::Ok => (0u64, 0u64),
        AdapterResp::Err(e) => (e.to_u8() as u64, 1u64),
    };
    let seq_part = (seq & 0x0000_FFFF_FFFF_FFFF) << 16;
    err_code | (is_err << 8) | seq_part
}

#[must_use]
#[inline]
pub const fn unpack_completed(packed: u64) -> (u64, AdapterResp) {
    let err_code = (packed & 0xFF) as u8;
    let is_err = (packed >> 8) & 0x1;
    let seq = (packed >> 16) & 0x0000_FFFF_FFFF_FFFF;
    let resp = if is_err == 0 {
        AdapterResp::Ok
    } else {
        AdapterResp::Err(AdapterErr::from_u8(err_code))
    };
    (seq, resp)
}

// ============================================================================
// AdapterNormalReq encode/decode（u8 action + u8 arg）
// ============================================================================

/// 把 `AdapterNormalReq` 编码成 (action_code, arg) 对，可塞进 packed AtomicU64。
/// 复杂请求或编码失败返回 `None` —— 上层映射到 `NormalReqStatus::Unsupported`。
#[must_use]
pub fn encode_normal_req(req: AdapterNormalReq) -> Option<(u8, u8)> {
    use normal_codes::{CONNECT, DISCONNECT, ENABLE, HOLD_CURRENT, SET_MODE};
    match req {
        AdapterNormalReq::Connect => Some((CONNECT, 0)),
        AdapterNormalReq::Disconnect => Some((DISCONNECT, 0)),
        AdapterNormalReq::Enable(on) => Some((ENABLE, u8::from(on))),
        AdapterNormalReq::SetMode(mode) => {
            // ControlMode 是 #[repr(...)] 但没 to_u8 method；手动映射。
            let arg = match mode {
                ControlMode::Stop => 0,
                ControlMode::Position => 1,
                ControlMode::Velocity => 2,
                ControlMode::Torque => 3,
                ControlMode::ServoPosition => 4,
            };
            Some((SET_MODE, arg))
        }
        AdapterNormalReq::HoldCurrent => Some((HOLD_CURRENT, 0)),
    }
}

/// 反向：把 (action_code, arg) 解回 `AdapterNormalReq`。任何无效组合返回
/// `Err(RobotError::Unsupported)`，rt_thread 应升 `last_completed = Err(Unsupported)`。
///
/// # Errors
///
/// 当 `action_code` 不在 `normal_codes` 已知集合 / `arg` 越界时返回。
pub fn decode_normal_req(action_code: u8, arg: u8) -> Result<AdapterNormalReq, RobotError> {
    use normal_codes::{CONNECT, DISCONNECT, ENABLE, HOLD_CURRENT, NONE, SET_MODE};
    match action_code {
        CONNECT => Ok(AdapterNormalReq::Connect),
        DISCONNECT => Ok(AdapterNormalReq::Disconnect),
        ENABLE => Ok(AdapterNormalReq::Enable(arg != 0)),
        SET_MODE => {
            let mode = match arg {
                0 => ControlMode::Stop,
                1 => ControlMode::Position,
                2 => ControlMode::Velocity,
                3 => ControlMode::Torque,
                4 => ControlMode::ServoPosition,
                _ => {
                    return Err(RobotError::Unsupported(
                        "decode_normal_req: bad ControlMode arg",
                    ));
                }
            };
            Ok(AdapterNormalReq::SetMode(mode))
        }
        HOLD_CURRENT => Ok(AdapterNormalReq::HoldCurrent),
        NONE => Err(RobotError::Unsupported("decode_normal_req: NONE")),
        _ => Err(RobotError::Unsupported(
            "decode_normal_req: unknown action_code",
        )),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_policy_round_trip_full_range() {
        for v in 0u8..=10 {
            let p = OutputPolicy::from_u8(v);
            // 0-5 valid，其它 → Disabled
            if v <= 5 {
                assert_eq!(p.to_u8(), v, "valid u8 round-trips: {v}");
            } else {
                assert_eq!(p, OutputPolicy::Disabled, "u8={v} falls back to Disabled");
            }
        }
    }

    #[test]
    fn output_policy_default_is_disabled() {
        assert_eq!(OutputPolicy::default(), OutputPolicy::Disabled);
    }

    #[test]
    fn output_safety_state_round_trip_full_range() {
        for v in 0u8..=10 {
            let s = OutputSafetyState::from_u8(v);
            if v <= 5 {
                assert_eq!(s.to_u8(), v);
            } else {
                assert_eq!(s, OutputSafetyState::TorqueUnknown);
            }
        }
    }

    #[test]
    fn pack_unpack_pending_normal_round_trip() {
        for action in 0u8..=8 {
            for arg in [0u8, 1, 2, 200, 255] {
                for seq in [0u64, 1, 0xFFFF, 0xFFFF_FFFF, 0x0000_FFFF_FFFF_FFFF] {
                    let packed = pack_pending_normal(action, arg, seq);
                    let (a, g, s) = unpack_pending_normal(packed);
                    assert_eq!(a, action, "action mismatch");
                    assert_eq!(g, arg, "arg mismatch");
                    assert_eq!(s, seq, "seq mismatch (input seq={seq})");
                }
            }
        }
    }

    #[test]
    fn pack_unpack_completed_round_trip_ok() {
        for seq in [0u64, 1, 12345, 0x0000_FFFF_FFFF_FFFF] {
            let packed = pack_completed(seq, AdapterResp::Ok);
            let (s, r) = unpack_completed(packed);
            assert_eq!(s, seq);
            assert_eq!(r, AdapterResp::Ok);
        }
    }

    #[test]
    fn pack_unpack_completed_round_trip_err() {
        for err in [
            AdapterErr::SafetyLatched,
            AdapterErr::NotConnected,
            AdapterErr::Unsupported,
            AdapterErr::HardwareFault,
            AdapterErr::PreemptedByEmergency,
            AdapterErr::Other,
        ] {
            for seq in [0u64, 1, 99, 0x123_4567_89AB] {
                let packed = pack_completed(seq, AdapterResp::Err(err));
                let (s, r) = unpack_completed(packed);
                assert_eq!(s, seq, "seq round-trip with err {err:?}");
                assert_eq!(r, AdapterResp::Err(err), "err round-trip seq={seq}");
            }
        }
    }

    #[test]
    fn encode_decode_normal_req_round_trip() {
        let cases = [
            AdapterNormalReq::Connect,
            AdapterNormalReq::Disconnect,
            AdapterNormalReq::Enable(true),
            AdapterNormalReq::Enable(false),
            AdapterNormalReq::SetMode(ControlMode::Position),
            AdapterNormalReq::SetMode(ControlMode::Velocity),
            AdapterNormalReq::SetMode(ControlMode::Torque),
            AdapterNormalReq::SetMode(ControlMode::ServoPosition),
            AdapterNormalReq::SetMode(ControlMode::Stop),
            AdapterNormalReq::HoldCurrent,
        ];
        for req in cases {
            let (action, arg) = encode_normal_req(req).expect("encode should succeed");
            let decoded = decode_normal_req(action, arg).expect("decode should succeed");
            assert_eq!(decoded, req, "req round-trip");
        }
    }

    #[test]
    fn decode_normal_req_unknown_returns_unsupported() {
        let r = decode_normal_req(99, 0);
        assert!(matches!(r, Err(RobotError::Unsupported(_))));
        let r2 = decode_normal_req(normal_codes::SET_MODE, 99);
        assert!(matches!(r2, Err(RobotError::Unsupported(_))));
        let r3 = decode_normal_req(normal_codes::NONE, 0);
        assert!(matches!(r3, Err(RobotError::Unsupported(_))));
    }

    #[test]
    fn adapter_err_round_trip() {
        for e in [
            AdapterErr::SafetyLatched,
            AdapterErr::NotConnected,
            AdapterErr::Unsupported,
            AdapterErr::HardwareFault,
            AdapterErr::PreemptedByEmergency,
            AdapterErr::Other,
        ] {
            let v = e.to_u8();
            assert_eq!(AdapterErr::from_u8(v), e, "round-trip {e:?}");
        }
    }

    #[test]
    fn pack_pending_normal_seq_u48_truncation() {
        // 高 16 位 seq 被截断（设计如此：seq 是 u48）。
        let packed = pack_pending_normal(1, 2, 0xFFFF_FFFF_FFFF_FFFF);
        let (_, _, seq_decoded) = unpack_pending_normal(packed);
        assert_eq!(seq_decoded, 0x0000_FFFF_FFFF_FFFF, "u48 truncation");
    }

    #[test]
    fn pack_pending_normal_disjoint_fields() {
        // action / arg / seq 完全独立 —— 改 action 不影响 arg 与 seq。
        let p1 = pack_pending_normal(1, 2, 100);
        let p2 = pack_pending_normal(5, 2, 100);
        assert_ne!(p1, p2);
        let (a1, g1, s1) = unpack_pending_normal(p1);
        let (a2, g2, s2) = unpack_pending_normal(p2);
        assert_eq!(g1, g2);
        assert_eq!(s1, s2);
        assert_ne!(a1, a2);
    }

    /// 编译期确认 `Box<dyn ExecutorCtrlClient>` 是 `Send + Sync`，可以放进
    /// `Arc<...>` 给 `ipc_thread` 持有 + `pub_thread` 读 metric 用。
    #[test]
    fn executor_ctrl_client_is_send_sync_via_dyn() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<Box<dyn ExecutorCtrlClient>>();
        assert_sync::<Box<dyn ExecutorCtrlClient>>();
    }

    /// AdapterResp / NormalReqStatus 必须是 Copy（HardRT 路径 zero-alloc）。
    #[test]
    fn types_are_copy() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<OutputPolicy>();
        assert_copy::<OutputSafetyState>();
        assert_copy::<AdapterNormalReq>();
        assert_copy::<AdapterEmergencyReq>();
        assert_copy::<AdapterResp>();
        assert_copy::<AdapterErr>();
        assert_copy::<NormalReqStatus>();
    }
}
