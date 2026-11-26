//! S1 协调逻辑：配置、启动锚定、保持命令、动作状态机与 tick 循环。
//!
//! `robot-s1` 负责端口、工作线程与 OS 信号，向本模块注入 [`RobotClock`]、
//! [`Publisher`] 与 [`LatestSubscriber`]。生产循环使用 [`run_action_tick_loop`]；
//! [`tick_once`] / [`run_tick_loop`] 保留基础保持接口，便于独立验证。
//!
//! ## 模块入口
//!
//! - [`CoordinatorConfig`]：B1 行为参数（默认值 + TOML `[s1]` section
//!   反序列化 + validate）。
//! - [`parse_s1_section_from_toml`]：把同一份 daemon `--config` TOML
//!   text 解出 `[s1]` 子段 → [`CoordinatorConfig`]；缺失 `[s1]` 走全套
//!   默认（B1 outline §3.1）；非法值返 [`ConfigError`] 报错。
//! - [`build_hold_command`]：给定 `bias` (启动锚定的 anchor 位置) +
//!   `now_ns` 构造一帧 hold `JointCommand` body bytes（target = bias,
//!   `ControlMode::Position`, `on_timeout = cfg.on_timeout`，
//!   `deadline_ns = now_ns + cfg.deadline_ms * 1e6`）。
//! - [`parse_anchor_from_state_bytes`]：校验解码、配置布局、新鲜度、位置数量及
//!   有限值；与 [`crate::state_cache`] 的 `latest_fresh` 共用
//!   `check_state_frame`，不跳过布局校验。
//! - [`read_anchor_from_s0_state`]：泛型 over `LatestSubscriber<JointState>`
//!   的 polling loop；测试用 `MockSubscriber`，binary 用
//!   `Iceoryx2Subscriber`。坏帧立即失败，不等待后续帧或回退到零位。
//!
//! ## 执行边界
//!
//! - 本模块不创建线程或安装信号处理器；循环通过注入的时钟读取时间与等待。
//! - tick 循环通过借入的 publisher 发布，端口所有权仍由调用方持有；
//!   心跳构造复用 `robot_msg::build_helpers::build_heartbeat`。
//! - 配置解析不依赖 `robot-s0::config_loader`，保留 S0/S1 隔离。

// 中文 doc 块大量 identifier-like 字串（mock_s1 / position_rad / tokio /
// freeze 等），coordinator 模块走宽松 doc-markdown，跟 robot-s0 /
// robot-rpc 现有惯例对齐。
#![allow(clippy::doc_markdown)]

use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use flatbuffers::root;
use robot_core::time::{DurationNs, MonoRawNs, RobotClock};
use robot_msg::build_helpers::{
    build_heartbeat, build_joint_command, build_mixed_joint_command_from_iter,
};
use robot_msg::markers;
use robot_msg::msg::{ControlMode, JointState as FbJointState, StaleAction};
use robot_transport::error::TransportError;
use robot_transport::traits::{LatestSubscriber, Publisher};
use serde::Deserialize;
use thiserror::Error;

// ============================================================================
// CoordinatorConfig
// ============================================================================

/// B1 coordinator 行为参数。
///
/// 字段对应 B1 outline §3.1 + §4.1 + §5：
///
/// - `heartbeat_hz`：S1 → S0 心跳频率（design §6.2 默认 50）。
/// - `command_hz`：`JointCommand` publish 频率（默认 50；与 mock_s1 对齐）。
/// - `on_timeout`：`JointCommand.on_timeout` wire 字段；S0 看到 cmd
///   过期 / stale 时的降级动作。B1 默认 `Hold`（保持最后一次位置）。
///   `Stop` 留给 B2 profile matrix 选择。
/// - `deadline_ms`：`JointCommand.deadline_ns = now_ns + deadline_ms * 1e6`。
///   B1 默认 500（10 个 50 Hz cycle 的 cushion，跟 `mock_s1` 对齐）。
/// - `anchor_timeout_ms`：启动期读 S0 state subscriber 的超时；超时 = 退
///   出非 0（fail-fast，§9 决策 #2）。
/// - `max_stale_ms`：S0 state sample stamp 比 `now` 旧超过此值视为 stale，
///   anchor 阶段 fail-fast。
/// - `source_id`：`Heartbeat.source_id` / `FrameHeader.source_id`；S1
///   约定 = 2（design §3.5）。
/// - `priority`：`JointCommand.priority`。B1 默认 10（mock_s1 对齐）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CoordinatorConfig {
    pub heartbeat_hz: f64,
    pub command_hz: f64,
    pub on_timeout: StaleAction,
    pub deadline_ms: u64,
    pub anchor_timeout_ms: u64,
    pub max_stale_ms: u64,
    pub source_id: u32,
    pub priority: u8,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            heartbeat_hz: 50.0,
            command_hz: 50.0,
            on_timeout: StaleAction::Hold,
            deadline_ms: 500,
            anchor_timeout_ms: 1000,
            max_stale_ms: 200,
            source_id: 2,
            priority: 10,
        }
    }
}

impl CoordinatorConfig {
    /// `JointCommand.deadline_ns` 与 `now_ns` 的偏移（饱和加，绝不 wrap）。
    #[inline]
    #[must_use]
    pub fn deadline_offset_ns(&self) -> u64 {
        self.deadline_ms.saturating_mul(1_000_000)
    }

    /// `Heartbeat` 周期（饱和换算；`heartbeat_hz` 已 validate > 0）。
    #[inline]
    #[must_use]
    pub fn heartbeat_period(&self) -> Duration {
        let secs = 1.0_f64 / self.heartbeat_hz;
        Duration::from_secs_f64(secs)
    }

    /// `JointCommand` 周期（饱和换算；`command_hz` 已 validate > 0）。
    #[inline]
    #[must_use]
    pub fn command_period(&self) -> Duration {
        let secs = 1.0_f64 / self.command_hz;
        Duration::from_secs_f64(secs)
    }
}

// ============================================================================
// ConfigError + TOML 反序列化
// ============================================================================

/// `[s1]` section 解析 / validate 失败时的错误类型。
#[derive(Debug, Error)]
pub enum ConfigError {
    /// TOML 文本解析失败（语法 / unknown field in `[s1]` 等）。
    #[error("TOML parse failed: {0}")]
    Toml(String),
    /// `heartbeat_hz` ≤ 0 或 NaN / inf。
    #[error("heartbeat_hz must be > 0 and finite (got {0})")]
    InvalidHeartbeatHz(f64),
    /// `command_hz` ≤ 0 或 NaN / inf。
    #[error("command_hz must be > 0 and finite (got {0})")]
    InvalidCommandHz(f64),
    /// `deadline_ms == 0`。
    #[error("deadline_ms must be > 0 (got {0})")]
    ZeroDeadlineMs(u64),
    /// `anchor_timeout_ms == 0`。
    #[error("anchor_timeout_ms must be > 0 (got {0})")]
    ZeroAnchorTimeoutMs(u64),
    /// `max_stale_ms == 0`。
    #[error("max_stale_ms must be > 0 (got {0})")]
    ZeroMaxStaleMs(u64),
    /// `on_timeout` 字面值不在 \[hold, zero, stop\]。
    #[error("on_timeout must be one of [hold, zero, stop] (got {0:?})")]
    InvalidOnTimeout(String),
    /// `[s1].hold_strategy` 字面值不是 `"anchor"`（B1 唯一支持值；B2 加
    /// `"zero"` / `"neutral"` / `"from_config"`）。
    #[error(
        "hold_strategy must be \"anchor\" in B1 (got {0:?}); zero/neutral/from_config land in B2"
    )]
    InvalidHoldStrategy(String),
    #[error("invalid single_joint configuration: {0}")]
    InvalidSingleJoint(String),
    /// 配置里没有匹配 `--hardware-id` 的 `[[hardware]]` section。
    #[error("hardware section with id={0:?} not found in config")]
    HardwareIdNotFound(String),
    /// 匹配到的 `[[hardware]]` section 里 `joints = []`（B1 至少要 1 关节）。
    #[error("hardware section {0:?} has no joints (B1 requires >= 1 joint)")]
    EmptyJoints(String),
    /// v0.5 候选 E 段(2)：匹配段 `motor_ids = []`。
    #[error("hardware section {0:?} has empty motor_ids")]
    EmptyMotorIds(String),
    /// v0.5 候选 E 段(2)：`motor_ids` 与 `joints` 长度不等。
    #[error("hardware section {id:?}: len(motor_ids)={motor_ids} != len(joints)={joints}")]
    MotorIdsLenMismatch {
        id: String,
        joints: usize,
        motor_ids: usize,
    },
    /// v0.5 候选 E 段(2)：joint 名重复。
    #[error("hardware section {id:?}: duplicate joint name {name:?}")]
    DupJointName { id: String, name: String },
    /// v0.5 候选 E 段(2)：motor id 重复。
    #[error("hardware section {id:?}: duplicate motor_id {motor_id}")]
    DupMotorId { id: String, motor_id: u8 },
}

impl ConfigError {
    /// error → 退出码映射（AC-E3）：S1 config 加载/内容错误族 = **3**
    /// （`robot-s1` exit 码表；身份锁 flag/config 冲突走 2，不经本类型）。
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        3
    }
}

/// TOML 顶层 wrapper：daemon 同款 `--config` 文件含 `[robot]` /
/// `[[hardware]]` 等 section，本 wrapper **不**用 `deny_unknown_fields`，
/// 让 robot-s1 静默跳过非 `[s1]` 段（避免跟 daemon `config_loader` 解析
/// 路径耦合）。
#[derive(Debug, Default, Deserialize)]
struct Wrapper {
    s1: Option<S1Section>,
}

/// `[s1]` section schema（B1）。**所有字段都 optional**，缺失走
/// [`CoordinatorConfig::default`]；显式覆盖才校验。
///
/// `deny_unknown_fields` 防 `[s1]` 段内拼错字段名（`heartbeat_hz`
/// 写成 `heartbeatHz` 这种）静默走默认值导致部署事故。**例外**：
/// B2 (3/3) 加的 `[s1.profile]` 子段被 [`parse_profile_section_from_toml`]
/// 单独消费；S1 parser 必须把它当成"已知字段"放行（**不** propagate 进
/// `CoordinatorConfig`，因为 profile 不属于 B1 `[s1]` runtime 配置），
/// 否则 `[s1.profile]` 存在时 S1 parser 会因 `deny_unknown_fields` 报
/// `unknown field profile` —— 这是 v0.4 候选 1 (3/3) 启动期 scripted
/// backend 配置入口的硬约束。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct S1Section {
    heartbeat_hz: Option<f64>,
    command_hz: Option<f64>,
    on_timeout: Option<String>,
    deadline_ms: Option<u64>,
    anchor_timeout_ms: Option<u64>,
    max_stale_ms: Option<u64>,
    source_id: Option<u32>,
    priority: Option<u8>,
    /// B1 唯一支持值 `"anchor"`（启动期读一次 S0 state）。B2 加
    /// `"zero"` / `"neutral"` / `"from_config"`。
    hold_strategy: Option<String>,
    /// B2 (3/3) `[s1.profile]` 子段。本 parser **不**消费其内容；
    /// [`parse_profile_section_from_toml`] 单独消费同一份 TOML。在
    /// 这里显式列出仅为绕开 `deny_unknown_fields` ——
    /// `#[serde(default)]` + 接受 `toml::Value` 让 serde 允许它存在
    /// 但不施加任何字段级 schema。
    #[serde(default)]
    #[allow(dead_code)]
    profile: Option<toml::Value>,
    single_joint: Option<crate::single_joint::SingleJointConfig>,
}

/// 把 TOML 文本（daemon `--config` 同款）解析成 [`CoordinatorConfig`]。
///
/// **行为**：
/// - 文本里没有 `[s1]` section → 返回 [`CoordinatorConfig::default`]
///   （B1 outline §3.1：`[s1]` 完全 optional）。
/// - 有 `[s1]` 但字段缺失 → 缺失字段走默认。
/// - 有 `[s1]` 且字段非法（`heartbeat_hz <= 0` 等）→ 返 [`ConfigError`]。
/// - 文本是 invalid TOML / `[s1]` 段含 unknown field → 返
///   [`ConfigError::Toml`]。
/// - 文本里其它 section（`[robot]` / `[[hardware]]` 等）**忽略**（不
///   `deny_unknown_fields` 在 wrapper 层）。
pub fn parse_s1_section_from_toml(toml_text: &str) -> Result<CoordinatorConfig, ConfigError> {
    let wrap: Wrapper = toml::from_str(toml_text).map_err(|e| ConfigError::Toml(e.to_string()))?;
    let Some(section) = wrap.s1 else {
        return Ok(CoordinatorConfig::default());
    };

    let mut cfg = CoordinatorConfig::default();

    if let Some(hz) = section.heartbeat_hz {
        if !hz.is_finite() || hz <= 0.0 {
            return Err(ConfigError::InvalidHeartbeatHz(hz));
        }
        cfg.heartbeat_hz = hz;
    }
    if let Some(hz) = section.command_hz {
        if !hz.is_finite() || hz <= 0.0 {
            return Err(ConfigError::InvalidCommandHz(hz));
        }
        cfg.command_hz = hz;
    }
    if let Some(s) = section.on_timeout.as_deref() {
        cfg.on_timeout = match s.to_ascii_lowercase().as_str() {
            "hold" => StaleAction::Hold,
            "zero" => StaleAction::Zero,
            "stop" => StaleAction::Stop,
            _ => return Err(ConfigError::InvalidOnTimeout(s.to_string())),
        };
    }
    if let Some(ms) = section.deadline_ms {
        if ms == 0 {
            return Err(ConfigError::ZeroDeadlineMs(ms));
        }
        cfg.deadline_ms = ms;
    }
    if let Some(ms) = section.anchor_timeout_ms {
        if ms == 0 {
            return Err(ConfigError::ZeroAnchorTimeoutMs(ms));
        }
        cfg.anchor_timeout_ms = ms;
    }
    if let Some(ms) = section.max_stale_ms {
        if ms == 0 {
            return Err(ConfigError::ZeroMaxStaleMs(ms));
        }
        cfg.max_stale_ms = ms;
    }
    if let Some(sid) = section.source_id {
        cfg.source_id = sid;
    }
    if let Some(p) = section.priority {
        cfg.priority = p;
    }
    if let Some(hs) = section.hold_strategy.as_deref()
        && !hs.eq_ignore_ascii_case("anchor")
    {
        return Err(ConfigError::InvalidHoldStrategy(hs.to_string()));
    }

    Ok(cfg)
}

/// 单关节运动配置缺省关闭；显式配置必须完整匹配本实例硬件布局。
pub fn parse_single_joint_config_from_toml(
    text: &str,
    hardware: &HardwareSection,
) -> Result<Option<crate::single_joint::SingleJointConfig>, ConfigError> {
    let wrap: Wrapper = toml::from_str(text).map_err(|e| ConfigError::Toml(e.to_string()))?;
    let config = wrap.s1.and_then(|section| section.single_joint);
    if let Some(config) = &config {
        config
            .validate(hardware)
            .map_err(ConfigError::InvalidSingleJoint)?;
    }
    Ok(config)
}

// ============================================================================
// HardwareSection minimal parser（B1 outline §3 + §9 决策 #6）
// ============================================================================

/// `[[hardware]]` section 里 B1 coordinator 需要的最小子集。
///
/// 跟 `robot-s0::config_loader::HardwareSection` 共用同一份 TOML 文件
/// 格式（`adapter` / `address` / `extras` 等字段在 daemon 端被消费），
/// 但 robot-s1 **不**复用 robot-s0 的 schema struct，避免 s1 → s0 反向
/// 依赖（B1 outline §9 决策 #6）；多余字段 serde 默认静默忽略，所以
/// 同一份 TOML 给 daemon / coordinator 各自 parse 不会冲突。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HardwareSection {
    pub id: String,
    #[serde(default)]
    pub joints: Vec<String>,
    /// v0.5 候选 E 段(2)（PC1，与 S0 侧同步）：显式 motor id 表，与
    /// `joints` 按 config 顺序一一对应。**必填无 default**——缺失 = TOML
    /// parse 错误（S1 config 错误族 exit 3）。
    pub motor_ids: Vec<u8>,
    /// 与 S0 从同一 TOML 读取；混合总线保活只保持臂位置，三轮 raw 恒为零。
    #[serde(default)]
    pub feetech_bus2_mixed_raw: bool,
    /// 同一硬件 TOML 的原位保持边界容差；缺省为零，不扩展主动运动目标。
    #[serde(default)]
    pub calibration_range_tolerances_raw: Option<Vec<u16>>,
}

impl HardwareSection {
    pub(crate) fn range_tolerance_raw(&self, slot: usize) -> u16 {
        self.calibration_range_tolerances_raw
            .as_ref()
            .and_then(|values| values.get(slot))
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn range_tolerances_valid(&self) -> bool {
        self.calibration_range_tolerances_raw
            .as_ref()
            .is_none_or(|values| {
                values.len() == self.joints.len() && values.iter().all(|&value| value <= 4095)
            })
    }

    /// 轮槽只由同份混合总线配置的电机 ID 判定，不固定配置中的下标。
    #[must_use]
    pub fn is_wheel_slot(&self, slot: usize) -> bool {
        self.feetech_bus2_mixed_raw && self.motor_ids.get(slot).is_some_and(|&id| id >= 7)
    }

    /// 将已校验的完整反馈转换为保持目标；轮反馈不解释为位置或速度目标。
    pub fn clear_wheel_targets(&self, targets: &mut [f64]) {
        for (slot, target) in targets.iter_mut().enumerate() {
            if self.is_wheel_slot(slot) {
                *target = 0.0;
            }
        }
    }

    pub(crate) fn mixed_layout_is_valid(&self) -> bool {
        !self.feetech_bus2_mixed_raw
            || (self.id == "right_arm_full_with_base"
                && self.joints.len() == 9
                && self.motor_ids.len() == 9
                && (1..=9).all(|id| self.motor_ids.contains(&id)))
    }
}

/// `[[hardware]]` 段的 wrapper（同一 TOML 可有多个 hardware section）。
#[derive(Debug, Default, Deserialize)]
struct HardwareWrapper {
    #[serde(default)]
    hardware: Vec<HardwareSection>,
}

/// 从 daemon `--config` 同款 TOML 找匹配 `hardware_id` 的
/// `[[hardware]]` section。
///
/// **行为**：
/// - 找到 → `Ok(HardwareSection)`（含 `joints` 列表，长度 ≥ 1）。
/// - 没有匹配 → [`ConfigError::HardwareIdNotFound`]。
/// - 匹配到但 `joints == []` → [`ConfigError::EmptyJoints`]（B1 至少
///   1 关节才能锚定 + 发 hold command）。
/// - TOML 语法错 / 类型错 → [`ConfigError::Toml`]。
///
/// **不**校验 `adapter` / `address` / `extras` 等 daemon-only 字段
/// （robot-s1 用不到；daemon 启动时自己 parse 全套校验）。
pub fn parse_hardware_section_from_toml(
    toml_text: &str,
    hardware_id: &str,
) -> Result<HardwareSection, ConfigError> {
    let wrap: HardwareWrapper =
        toml::from_str(toml_text).map_err(|e| ConfigError::Toml(e.to_string()))?;
    let section = wrap
        .hardware
        .into_iter()
        .find(|h| h.id == hardware_id)
        .ok_or_else(|| ConfigError::HardwareIdNotFound(hardware_id.to_string()))?;
    if section.joints.is_empty() {
        return Err(ConfigError::EmptyJoints(section.id));
    }
    // v0.5 候选 E 段(2)：与 S0 `validate_hardware_section` 同判据（非空 /
    // 等长 / 唯一；u8 域由类型承担）——S1 侧独立实现，守 s1 → s0 0 依赖边
    // （B1 outline §9 决策 #6）。
    if section.motor_ids.is_empty() {
        return Err(ConfigError::EmptyMotorIds(section.id));
    }
    if section.motor_ids.len() != section.joints.len() {
        return Err(ConfigError::MotorIdsLenMismatch {
            id: section.id,
            joints: section.joints.len(),
            motor_ids: section.motor_ids.len(),
        });
    }
    let mut seen_names = std::collections::HashSet::with_capacity(section.joints.len());
    for name in &section.joints {
        if !seen_names.insert(name.as_str()) {
            return Err(ConfigError::DupJointName {
                id: section.id.clone(),
                name: name.clone(),
            });
        }
    }
    let mut seen_ids = [false; 256];
    for &motor_id in &section.motor_ids {
        if seen_ids[usize::from(motor_id)] {
            return Err(ConfigError::DupMotorId {
                id: section.id.clone(),
                motor_id,
            });
        }
        seen_ids[usize::from(motor_id)] = true;
    }
    if !section.mixed_layout_is_valid() {
        return Err(ConfigError::Toml(
            "混合总线需要完整右臂九槽及 ID1..9".into(),
        ));
    }
    if !section.range_tolerances_valid() {
        return Err(ConfigError::Toml(
            "校准边界容差必须与关节等长且在0..4095内".into(),
        ));
    }
    Ok(section)
}

// ============================================================================
// [robot].id minimal parser（v0.5 候选 E 段(2) 身份锁用）
// ============================================================================

/// `[robot]` 段 minimal 视图（只取 `id`；其余字段 serde 静默忽略——与
/// `HardwareSection` 同款"不复用 robot-s0 schema"边界）。
#[derive(Debug, Deserialize)]
struct RobotIdOnly {
    id: String,
}

#[derive(Debug, Deserialize)]
struct RobotIdWrapper {
    robot: RobotIdOnly,
}

/// 从 daemon `--config` 同款 TOML 取 `[robot].id`（身份锁比对用，
/// P.2.2；缺 `[robot]`/缺 `id`/语法错 → [`ConfigError::Toml`]）。
///
/// # Errors
///
/// [`ConfigError::Toml`]。
pub fn parse_robot_id_from_toml(toml_text: &str) -> Result<String, ConfigError> {
    let wrap: RobotIdWrapper =
        toml::from_str(toml_text).map_err(|e| ConfigError::Toml(e.to_string()))?;
    Ok(wrap.robot.id)
}

// ============================================================================
// build_hold_command
// ============================================================================

/// 构造一帧 hold `JointCommand` body bytes（B1 idle / hold 默认）。
///
/// **wire 字段语义**：
/// - `mode` = [`ControlMode::Position`]（B1 唯一支持模式）。
/// - `target` = `bias`（启动期读 S0 state 锚定的当前位置；每帧不变）。
/// - `deadline_ns` = `now_ns + cfg.deadline_offset_ns()`（绝对
///   `CLOCK_MONOTONIC_RAW` 时刻；§5.2）。
/// - `priority` = `cfg.priority`。
/// - `on_timeout` = `cfg.on_timeout`（B1 默认 `Hold`，§5.1）。
/// - `source_task_id`：调用方递增（B1 一般每条 cmd 一个 seq，与
///   `FrameHeader.seq` 同步）。
///
/// **`deadline_ns` 饱和算术**：用 `saturating_add` 防极端 `now_ns` →
/// 溢出 `u64::MAX`；`u64::MAX` 远超系统寿命，clip 到饱和值不损失语义。
#[must_use]
pub fn build_hold_command(
    cfg: &CoordinatorConfig,
    layout_id: u64,
    bias: &[f64],
    source_task_id: u64,
    now_ns: u64,
) -> Vec<u8> {
    let deadline_ns = now_ns.saturating_add(cfg.deadline_offset_ns());
    build_joint_command(
        layout_id,
        ControlMode::Position,
        bias,
        deadline_ns,
        cfg.priority,
        cfg.on_timeout,
        source_task_id,
    )
}

/// 按同份硬件配置构造保活/位置命令；混合模式只允许臂位置，三轮均停止。
/// 输入仍是完整配置槽位的位置向量；轮位置被清零，绝不解释为 raw 速度。
#[must_use]
pub fn build_hardware_hold_command(
    cfg: &CoordinatorConfig,
    hardware: &HardwareSection,
    layout_id: u64,
    positions: &[f64],
    source_task_id: u64,
    now_ns: u64,
) -> Vec<u8> {
    if !hardware.feetech_bus2_mixed_raw {
        return build_hold_command(cfg, layout_id, positions, source_task_id, now_ns);
    }
    // 保留输入快照，编码时清零轮位，不再为掩码创建第二份位置数组。
    let arm_positions = positions.iter().enumerate().map(|(slot, &position)| {
        if hardware.is_wheel_slot(slot) {
            0.0
        } else {
            position
        }
    });
    build_mixed_joint_command_from_iter(
        layout_id,
        arm_positions,
        &[0; 9],
        now_ns.saturating_add(cfg.deadline_offset_ns()),
        cfg.priority,
        source_task_id,
    )
}

// ============================================================================
// AnchorError + parse_anchor_from_state_bytes
// ============================================================================

/// `read_anchor_from_s0_state` / `parse_anchor_from_state_bytes` 失败原因。
///
/// 启动锚定失败时不建立保持目标；无帧有界等待，坏帧立即失败。
///
/// 解码、布局、新鲜度、位置字段、关节数量或有限值不满足时，启动入口均以
/// exit 4 退出；不会以零位或后续有效帧替代坏帧。
#[derive(Debug, Error)]
pub enum AnchorError {
    /// `anchor_timeout_ms` 内未收到任何 S0 state sample。
    #[error("anchor read timed out after {timeout_ms} ms (S0 daemon not publishing state)")]
    Timeout { timeout_ms: u64 },
    /// State sample 收到了但 `stamp_ns` 已过期。
    #[error(
        "S0 state sample is stale: stamp_ns={stamp_ns}, now_ns={now_ns}, max_stale_ns={max_stale_ns}"
    )]
    Stale {
        stamp_ns: u64,
        now_ns: u64,
        max_stale_ns: u64,
    },
    /// state.`position_rad` 长度跟 hardware section 配的 joint 数不一致。
    #[error("joint count mismatch: hardware section expects {expected}, S0 state has {got}")]
    JointCountMismatch { expected: usize, got: usize },
    /// `JointState.layout_id` 与 S1 配置派生值不等。
    #[error("layout_id mismatch: S1 config derives {expected:#018x}, S0 state carries {got:#018x}")]
    LayoutMismatch { expected: u64, got: u64 },
    /// 首个非有限位置的下标；不允许 NaN 或正负无穷进入保持目标。
    #[error("S0 position_rad[{index}] is not finite")]
    NonFinitePosition { index: usize },
    /// state body 完全没 `position_rad` 字段（publisher bug）。
    #[error("S0 state sample has no position_rad field")]
    MissingPositionField,
    /// Transport 层错误（subscriber recv failure）。
    #[error("transport error reading S0 state: {0}")]
    Transport(String),
    /// fbs verify 失败 / view 解码失败。
    #[error("S0 state body decode failed: {0}")]
    Decode(String),
}

impl From<TransportError> for AnchorError {
    fn from(e: TransportError) -> Self {
        Self::Transport(e.to_string())
    }
}

/// 从一帧 `JointState` 提取启动锚点；布局、数量、新鲜度和有限值必须全部通过。
/// 供单元测试与 `read_anchor_from_s0_state` 共用。
///
/// 注意 `bytes` 是 `JointState` body bytes（`build_joint_state`
/// 输出），**不是** wire sample（不含 `FrameHeader`）。
pub fn parse_anchor_from_state_bytes(
    bytes: &[u8],
    expected_joints: usize,
    expected_layout: u64,
    now_ns: u64,
    max_stale_ns: u64,
) -> Result<Vec<f64>, AnchorError> {
    check_state_frame(
        bytes,
        expected_joints,
        expected_layout,
        now_ns,
        max_stale_ns,
    )
    .map_err(AnchorError::from)
}

// ============================================================================
// check_state_frame（v0.5 候选 E 段(5)：锚定 / state cache 共享校验）
// ============================================================================

/// `check_state_frame` 失败原因——crate 私有的**单一校验 taxonomy**；
/// [`AnchorError`]（启动锚定）与 [`crate::state_cache::StaleOrMissing`]
/// （常驻 cache）各自 `From` 映射（exhaustive match：任一侧新增 variant
/// 编译期锁）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FrameCheckError {
    /// fbs verify / root 解码失败。
    Decode(String),
    /// `JointState.layout_id != expected_layout`。
    LayoutMismatch { expected: u64, got: u64 },
    /// `stamp_ns == 0` 或 `now_ns - stamp_ns > max_stale_ns`。
    Stale {
        stamp_ns: u64,
        now_ns: u64,
        max_stale_ns: u64,
    },
    /// body 无 `position_rad` 字段。
    MissingPositionField,
    /// `position_rad.len() != expected_joints`。
    JointCountMismatch { expected: usize, got: usize },
    /// `position_rad` 首个非有限数值的下标。
    NonFinitePosition { index: usize },
}

impl From<FrameCheckError> for AnchorError {
    fn from(e: FrameCheckError) -> Self {
        match e {
            FrameCheckError::Decode(s) => Self::Decode(s),
            FrameCheckError::LayoutMismatch { expected, got } => {
                Self::LayoutMismatch { expected, got }
            }
            FrameCheckError::Stale {
                stamp_ns,
                now_ns,
                max_stale_ns,
            } => Self::Stale {
                stamp_ns,
                now_ns,
                max_stale_ns,
            },
            FrameCheckError::MissingPositionField => Self::MissingPositionField,
            FrameCheckError::JointCountMismatch { expected, got } => {
                Self::JointCountMismatch { expected, got }
            }
            FrameCheckError::NonFinitePosition { index } => Self::NonFinitePosition { index },
        }
    }
}

/// 一帧 `JointState` body bytes 的共享校验 + 关节位置提取（v0.5 候选 E
/// 段(5)，Spec P.2.5 "锚定校验逻辑与 `read_anchor_from_s0_state` 共享
/// 私有 fn 抽取"）。
///
/// **校验顺序**（单因短路）：
///
/// 1. fbs `root::<JointState>` verify → [`FrameCheckError::Decode`]
/// 2. `layout_id != expected_layout` →
///    [`FrameCheckError::LayoutMismatch`]（D1 联动；配置级错误优先于
///    时序级错误）
/// 3. `stamp_ns == 0 || now_ns - stamp_ns > max_stale_ns` →
///    [`FrameCheckError::Stale`]
/// 4. 无 `position_rad` → [`FrameCheckError::MissingPositionField`]
/// 5. `position_rad.len() != expected_joints` →
///    [`FrameCheckError::JointCountMismatch`]
/// 6. 任一位置不是有限值 → [`FrameCheckError::NonFinitePosition`]
pub(crate) fn check_state_frame(
    bytes: &[u8],
    expected_joints: usize,
    expected_layout: u64,
    now_ns: u64,
    max_stale_ns: u64,
) -> Result<Vec<f64>, FrameCheckError> {
    let js = root::<FbJointState<'_>>(bytes).map_err(|e| FrameCheckError::Decode(e.to_string()))?;

    let got = js.layout_id();
    if got != expected_layout {
        return Err(FrameCheckError::LayoutMismatch {
            expected: expected_layout,
            got,
        });
    }

    let stamp_ns = js.stamp_ns();
    if stamp_ns == 0 || stamp_ns > now_ns || now_ns.saturating_sub(stamp_ns) > max_stale_ns {
        return Err(FrameCheckError::Stale {
            stamp_ns,
            now_ns,
            max_stale_ns,
        });
    }

    let Some(pos_vec) = js.position_rad() else {
        return Err(FrameCheckError::MissingPositionField);
    };

    if pos_vec.len() != expected_joints {
        return Err(FrameCheckError::JointCountMismatch {
            expected: expected_joints,
            got: pos_vec.len(),
        });
    }

    if let Some(index) = pos_vec.iter().position(|value| !value.is_finite()) {
        return Err(FrameCheckError::NonFinitePosition { index });
    }
    Ok(pos_vec.iter().collect())
}

// ============================================================================
// read_anchor_from_s0_state
// ============================================================================

/// 启动期 polling loop：从 `subscriber` 拉最新 `JointState` sample，提
/// 取 `position_rad[0..expected_joints]` 作 anchor bias。
///
/// **行为**（B1 outline §4.2 + §7.1）：
/// - 在 `timeout` 内反复 `subscriber.latest()`：
///   - `Ok(Some(lease))`：执行 [`parse_anchor_from_state_bytes`] 的全部
///     fail-fast 断言；通过 → 返 `Ok(bias)`；任一失败立即 `Err`。
///   - `Ok(None)`：还没 sample，sleep `poll_interval` 后重试。
///   - `Err`：transport 错误，立即 `Err(AnchorError::Transport)`（这
///     条不重试 —— iceoryx2 subscriber `latest()` 不会偶发失败，
///     发错误意味着 service 出错，重试只会刷屏）。
/// - 时间到 `start + timeout` 仍没拿到 sample → `Err(AnchorError::Timeout)`。
///
/// **`Stale` 判定**：`now_ns - state.stamp_ns > max_stale.as_nanos()`。
/// 设计上 anchor 阶段 S0 state 应 < 100 ms 旧（design §6.2 50 Hz publish），
/// `max_stale` 默认 200 ms。
///
/// **不**实现 `Ok(Some(lease)) → stale → 等下一帧` 重试 —— 单帧 stale
/// = S0 已挂或 freeze，重试浪费 anchor_timeout 预算；fail-fast 让
/// systemd / deploy 流程更早恢复。
///
/// **睡眠抽象**：内部调用 [`RobotClock::sleep_for`] 推进 `poll_interval`。
/// 测试用 [`robot_core::time::MockClock`] 会快进单调时钟，因此 timeout
/// 路径不需要真等；binary 层传 [`robot_core::time::SystemClock`] 时才会
/// 产生真实 OS sleep。
pub fn read_anchor_from_s0_state<S, C>(
    subscriber: &mut S,
    expected_joints: usize,
    expected_layout: u64,
    clock: &C,
    timeout: Duration,
    max_stale: Duration,
    poll_interval: Duration,
) -> Result<Vec<f64>, AnchorError>
where
    S: LatestSubscriber<markers::JointState>,
    C: RobotClock + ?Sized,
{
    let start = clock.now();
    let timeout_ns = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX);
    let max_stale_ns = u64::try_from(max_stale.as_nanos()).unwrap_or(u64::MAX);
    let deadline = MonoRawNs(start.0.saturating_add(timeout_ns));

    loop {
        if let Some(lease) = subscriber.latest()? {
            let view = lease
                .view()
                .map_err(|e| AnchorError::Decode(e.to_string()))?;
            let body_bytes = view.body.raw();
            let now_ns = clock.now().0;
            return parse_anchor_from_state_bytes(
                body_bytes,
                expected_joints,
                expected_layout,
                now_ns,
                max_stale_ns,
            );
        }
        if clock.now() >= deadline {
            // `as_millis` 返回 u128；上限 = `u64::MAX` ms ≈ 5.8 亿年，
            // 远超 anchor_timeout 实际取值（默认 1000 ms），用 try_from
            // 在不可达 overflow 时饱和到 u64::MAX。
            let elapsed_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
            return Err(AnchorError::Timeout {
                timeout_ms: elapsed_ms,
            });
        }
        clock.sleep_for(DurationNs::from_nanos(
            u64::try_from(poll_interval.as_nanos()).unwrap_or(u64::MAX),
        ));
    }
}

// ============================================================================
// Tick loop（B1 (2/3) 单 tick 原语 + production loop wrapper）
// ============================================================================

/// `tick_once` / `run_tick_loop` 跨 tick 持续状态。
///
/// 调用方持有，每次 tick 通过 `&mut TickState` 让本 helper 更新内部
/// 计时器与 seq counter。
///
/// `cmd_seq` / `hb_seq` 公开让调用方做日志 / 报告（不参与 tick 逻辑）；
/// 其它字段保私有，跨 tick 一致性由 helper 保证。
#[derive(Debug)]
pub struct TickState {
    start_ns: u64,
    last_cmd_ns: u64,
    last_hb_ns: u64,
    /// 已 publish 的 `JointCommand` 累计计数（`wrapping_add`，永不 panic）。
    pub cmd_seq: u64,
    /// 已 publish 的 `Heartbeat` 累计计数（`wrapping_add`，永不 panic）。
    pub hb_seq: u64,
}

impl TickState {
    /// 构造一个新 tick state，把 `last_cmd_ns` / `last_hb_ns` 都设成
    /// `now_ns`，使首次 publish 在 `now_ns + period` 之后发生（与
    /// `mock_s1` 现有行为对齐）。
    #[must_use]
    pub fn new(now_ns: u64) -> Self {
        Self {
            start_ns: now_ns,
            last_cmd_ns: now_ns,
            last_hb_ns: now_ns,
            cmd_seq: 0,
            hb_seq: 0,
        }
    }

    /// `now_ns - start_ns`（饱和减；调用方 `Some(d)` 内查 duration 限）。
    #[inline]
    #[must_use]
    pub fn elapsed_ns(&self, now_ns: u64) -> u64 {
        now_ns.saturating_sub(self.start_ns)
    }
}

/// `tick_once` 单 tick 出口标识：哪些 topic 在本次被 publish。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickPublished {
    /// 本 tick 是否 publish 了 `JointCommand`。
    pub cmd: bool,
    /// 本 tick 是否 publish 了 `Heartbeat`。
    pub hb: bool,
}

/// `tick_once` / `run_tick_loop` 失败原因（B1 仅 publish path）。
#[derive(Debug, Error)]
pub enum TickError {
    /// `cmd_pub.loan()` / `hb_pub.loan()` / `writer.publish()` 任一报错。
    #[error("publish failed: {0}")]
    Publish(String),
}

impl From<TransportError> for TickError {
    fn from(e: TransportError) -> Self {
        Self::Publish(e.to_string())
    }
}

/// 共享的 publish helper：一次 `loan` → set header / body → `publish`。
fn publish_body<T, P>(
    pub_: &mut P,
    body: Vec<u8>,
    seq: u64,
    source_id: u32,
    stamp_ns: u64,
) -> Result<(), TransportError>
where
    T: robot_msg::markers::BodyMarker,
    P: Publisher<T>,
{
    // 1024 byte 余量给 FrameHeader（128 byte）+ blob padding；JointCommand /
    // Heartbeat body 都 < 1 KiB，1024 + body_len 是宽松上限。
    let mut writer = pub_.loan(body.len() + 1024)?;
    writer.set_seq(seq);
    writer.set_source_id(source_id);
    writer.set_stamp_ns(stamp_ns);
    writer.set_body_owned(body);
    writer.publish()
}

/// 单 tick 原语：检查 cmd / hb 是否到周期，到则各 publish 一次。
///
/// **行为**：
/// - `now_ns - state.last_cmd_ns >= cmd_period_ns` → 调
///   [`build_hold_command`] 构造 body，通过 `cmd_pub` 发出，`cmd_seq +=
///   1`，`last_cmd_ns = now_ns`。
/// - `now_ns - state.last_hb_ns >= hb_period_ns` → 调
///   [`build_heartbeat`](robot_msg::build_helpers::build_heartbeat)
///   构造 body，通过 `hb_pub` 发出，`hb_seq += 1`，`last_hb_ns = now_ns`。
///
/// 两条独立计时器；可能两个都发、一个发、都不发。
///
/// **前提**：`cfg.heartbeat_hz > 0` 且 `cfg.command_hz > 0`（由
/// [`parse_s1_section_from_toml`] 保证；调用方手动构造 [`CoordinatorConfig`]
/// 时自负责）。本函数对 `<= 0` 行为未定义（通过 `Duration::from_secs_f64`
/// 可能 panic）。
///
/// **测试用法**：调用方持 `MockChannel<JointCommand>` /
/// `MockChannel<Heartbeat>`，每次 tick 后调 `cmd_sub.latest()?` 检查
/// 最新 body；通过 `MockChannel::publishes()` 观察累计 publish 数。
pub fn tick_once<HBPub, CmdPub>(
    cfg: &CoordinatorConfig,
    state: &mut TickState,
    layout_id: u64,
    bias: &[f64],
    now_ns: u64,
    hb_pub: &mut HBPub,
    cmd_pub: &mut CmdPub,
) -> Result<TickPublished, TickError>
where
    HBPub: Publisher<markers::Heartbeat>,
    CmdPub: Publisher<markers::JointCommand>,
{
    let cmd_period_ns = duration_to_ns_saturating(cfg.command_period());
    let hb_period_ns = duration_to_ns_saturating(cfg.heartbeat_period());

    let mut out = TickPublished::default();

    if now_ns.saturating_sub(state.last_cmd_ns) >= cmd_period_ns {
        let body = build_hold_command(cfg, layout_id, bias, state.cmd_seq, now_ns);
        publish_body::<markers::JointCommand, _>(
            cmd_pub,
            body,
            state.cmd_seq,
            cfg.source_id,
            now_ns,
        )?;
        state.cmd_seq = state.cmd_seq.wrapping_add(1);
        state.last_cmd_ns = now_ns;
        out.cmd = true;
    }

    if now_ns.saturating_sub(state.last_hb_ns) >= hb_period_ns {
        let body = build_heartbeat(cfg.source_id, state.hb_seq, cfg.heartbeat_hz, now_ns);
        publish_body::<markers::Heartbeat, _>(hb_pub, body, state.hb_seq, cfg.source_id, now_ns)?;
        state.hb_seq = state.hb_seq.wrapping_add(1);
        state.last_hb_ns = now_ns;
        out.hb = true;
    }

    Ok(out)
}

#[inline]
fn duration_to_ns_saturating(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// 基础 Position 保持循环：调用 [`tick_once`]，通过注入的时钟按 `poll_interval` 等待。
/// 不调度 action backend；生产 `robot-s1` 使用 [`run_action_tick_loop`]。
///
/// **退出条件**（按检查顺序）：
/// 1. `shutdown.load(Acquire) == true`（信号处理器置位）。
/// 2. `duration.is_some_and(|d| state.elapsed_ns(now) >= d.as_nanos())`。
///
/// **clock 抽象**：测试用 [`robot_core::time::MockClock`] 可立即推时间，
/// `tick_once` 走 due 路径；实际运行可用 [`robot_core::time::SystemClock`]。
///
/// 返回最终 [`TickState`] 给调用方读取进度（`cmd_seq` / `hb_seq` /
/// `elapsed_ns`）。
///
/// 9 个参数全部是必要的（cfg / layout_id / bias / 两个 publisher /
/// clock / poll_interval / duration / shutdown），抽 builder 反而绕，
/// 显式 `allow(too_many_arguments)`。
#[allow(clippy::too_many_arguments)]
pub fn run_tick_loop<HBPub, CmdPub, C>(
    cfg: &CoordinatorConfig,
    layout_id: u64,
    bias: &[f64],
    hb_pub: &mut HBPub,
    cmd_pub: &mut CmdPub,
    clock: &C,
    poll_interval: Duration,
    duration: Option<Duration>,
    shutdown: &AtomicBool,
) -> Result<TickState, TickError>
where
    HBPub: Publisher<markers::Heartbeat>,
    CmdPub: Publisher<markers::JointCommand>,
    C: RobotClock + ?Sized,
{
    let start_ns = clock.now().0;
    let mut state = TickState::new(start_ns);
    let duration_ns = duration.map(duration_to_ns_saturating);
    let poll_ns = duration_to_ns_saturating(poll_interval);

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let now_ns = clock.now().0;
        if let Some(d_ns) = duration_ns
            && state.elapsed_ns(now_ns) >= d_ns
        {
            break;
        }
        tick_once(cfg, &mut state, layout_id, bias, now_ns, hb_pub, cmd_pub)?;
        clock.sleep_for(DurationNs::from_nanos(poll_ns));
    }

    Ok(state)
}

/// `robot-s1` 使用的状态感知循环；直接按 [`CoordinatorState`] 发布保持命令
/// 或推进 action backend，不调用 [`run_tick_loop`]。
///
/// 心跳与命令分别按配置周期门控，发布失败向调用方返回错误：
///
/// - `Degraded` 仍发送心跳，不把 S1 内部降级当作 S0 控制链路失活。
/// - `Idle` 保持 [`Coordinator::idle_hold_target`]：初值为启动锚点；Scripted
///   成功后保持末目标，新单关节请求中断时保持最后成功发布目标。
///   旧载荷及其他后端的中断仍重置为启动锚点。
///   已配置单关节控制且 S0 新鲜反馈确认安全断扭时，先将保持目标对齐当前位置。
/// - `Degraded` 与 `Idle` 共用终态保持目标；旧后端失败仍重置启动锚点。
/// - 保持命令经 `Coordinator::hold_command` 构造；混合实例的轮 raw 固定为零。
/// - `Executing` 调用 [`Coordinator::tick_action`]，透传当前 `cmd_seq` 作为 backend
///   的命令序号来源；`Some(body)` 发布 backend 命令，`None` 不发保持替代命令，
///   也不递增 `cmd_seq`，由 S0 沿用原命令及其截止约束。
/// - `Booting` 跳过命令，防止发布未锚定的目标；无论是否发布都推进命令周期，
///   避免按 poll 间隔反复调用 backend。
///
/// **退出条件**（按检查顺序）：
/// 1. `shutdown.load(Acquire)`（信号处理器置位）。
/// 2. `duration` 到期。
/// 3. `coordinator.current_state() == ShuttingDown`（由 [`Coordinator::mark_shutting_down`] 设置）。
#[allow(clippy::too_many_arguments)]
pub fn run_action_tick_loop<HBPub, CmdPub, C>(
    coordinator: &Coordinator,
    hb_pub: &mut HBPub,
    cmd_pub: &mut CmdPub,
    clock: &C,
    poll_interval: Duration,
    duration: Option<Duration>,
    shutdown: &AtomicBool,
) -> Result<TickState, TickError>
where
    HBPub: Publisher<markers::Heartbeat>,
    CmdPub: Publisher<markers::JointCommand>,
    // C: Sized 要求来自 `coordinator.tick_action(clock, ...)` 内部要把
    // `&C` 转 `&dyn RobotClock`（trait object 需要 vtable，需 C: Sized）。
    // run_tick_loop 没这条约束因为不调 tick_action。
    C: RobotClock,
{
    let cfg = coordinator.config();
    let loop_metrics = coordinator.loop_metrics();

    let start_ns = clock.now().0;
    let mut state = TickState::new(start_ns);
    let duration_ns = duration.map(duration_to_ns_saturating);
    let poll_ns = duration_to_ns_saturating(poll_interval);
    let cmd_period_ns = duration_to_ns_saturating(cfg.command_period());
    let hb_period_ns = duration_to_ns_saturating(cfg.heartbeat_period());

    loop {
        // 退出 1：信号处理器置位。
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        // v0.5 候选 E 段(8)：liveness 计数（空闲亦推进）+ test-only freeze hook
        // （Tick 槽位：loop 顶端 park）。
        loop_metrics.loop_tick_count.fetch_add(1, Ordering::Relaxed);
        #[cfg(any(test, feature = "test-freeze-hook"))]
        crate::liveness::freeze_check(&loop_metrics.freeze_until_ns_tick, clock, shutdown);

        let now_ns = clock.now().0;

        // 退出 2：duration 到期（测试用）。
        if let Some(d_ns) = duration_ns
            && state.elapsed_ns(now_ns) >= d_ns
        {
            break;
        }

        // 退出 3：coordinator 已 mark_shutting_down（即使 shutdown
        // AtomicBool 没置；典型场景是测试直接调 coordinator.mark_shutting_down
        // 不走信号路径）。
        let current_state = coordinator.current_state();
        if matches!(current_state, CoordinatorState::ShuttingDown) {
            break;
        }

        // Heartbeat：永远按周期发，不分状态（reviewer 决策 #10d：Degraded
        // 也续发 hb，S0 watchdog 不会因 hb stale 触发 control-lost）。
        if now_ns.saturating_sub(state.last_hb_ns) >= hb_period_ns {
            // test-only freeze hook（HeartbeatPub 槽位：hb publish 前 park；与
            // Tick 同线程，仅 park 点不同）。
            #[cfg(any(test, feature = "test-freeze-hook"))]
            crate::liveness::freeze_check(&loop_metrics.freeze_until_ns_heartbeat, clock, shutdown);
            let body = build_heartbeat(cfg.source_id, state.hb_seq, cfg.heartbeat_hz, now_ns);
            publish_body::<markers::Heartbeat, _>(
                hb_pub,
                body,
                state.hb_seq,
                cfg.source_id,
                now_ns,
            )?;
            state.hb_seq = state.hb_seq.wrapping_add(1);
            state.last_hb_ns = now_ns;
            loop_metrics
                .heartbeat_published
                .fetch_add(1, Ordering::Relaxed);
        }

        // Command：按周期 + 按 state 分支。
        if now_ns.saturating_sub(state.last_cmd_ns) >= cmd_period_ns {
            // 重新读 state（hb publish 不会动 fsm，但小心起见）。
            let mut published = false;
            match coordinator.current_state() {
                CoordinatorState::Idle => {
                    // 仅已配置单关节控制、无任务且已确认断扭时对齐；
                    // 上扭后保持已有目标，不随反馈追踪或回写启动锚点。
                    let target = coordinator.idle_hold_target_for_tick(clock);
                    let body = coordinator.hold_command(&target, state.cmd_seq, now_ns);
                    publish_body::<markers::JointCommand, _>(
                        cmd_pub,
                        body,
                        state.cmd_seq,
                        cfg.source_id,
                        now_ns,
                    )?;
                    published = true;
                }
                CoordinatorState::Degraded { .. } => {
                    // 与Idle使用同一终态目标；旧后端失败仍显式恢复启动锚点。
                    let target = coordinator.idle_hold_target();
                    let body = coordinator.hold_command(&target, state.cmd_seq, now_ns);
                    publish_body::<markers::JointCommand, _>(
                        cmd_pub,
                        body,
                        state.cmd_seq,
                        cfg.source_id,
                        now_ns,
                    )?;
                    published = true;
                }
                CoordinatorState::Executing { .. } => {
                    // Override：tick_action 决定本帧是否产新 cmd。
                    // 透传 state.cmd_seq —— backend 产 body bytes 时
                    // 把它作为 source_task_id，跟下面 publish_body 的
                    // FrameHeader.seq 同源；S0 ipc_thread 把
                    // fb.source_task_id() 直接当 JointCommandBuf.seq，
                    // adapter.rs 要求单调递增（详 B3 outline §3.4 +
                    // §2.1 #4 round 2 阻断 1 修法）。
                    if let Some(body_struct) = coordinator.tick_action(clock, state.cmd_seq) {
                        publish_body::<markers::JointCommand, _>(
                            cmd_pub,
                            body_struct.into_bytes(),
                            state.cmd_seq,
                            cfg.source_id,
                            now_ns,
                        )?;
                        coordinator.command_published(clock.now().0);
                        published = true;
                    }
                    // None 路径：不 publish；cmd_seq 不递增 —— reviewer
                    // 评审口径"hold loop 不再发 B1 hold command"通过
                    // cmd_ch.publishes() 计数验证。
                }
                CoordinatorState::Booting => {
                    // 防御性：anchor 完应已 mark_idle；若仍 Booting，跳
                    // 过 cmd 一帧，等下次 cmd-period 重试。
                }
                CoordinatorState::ShuttingDown => {
                    // 上面已 break；这里防 race 条件下兜底。
                    break;
                }
            }

            if published {
                state.cmd_seq = state.cmd_seq.wrapping_add(1);
                loop_metrics
                    .command_published
                    .fetch_add(1, Ordering::Relaxed);
            }
            // 无论是否 publish，都推 last_cmd_ns 维持 50 Hz tick rate
            // （否则 1 ms poll 间会反复 retry，违反 backend tick 节奏）。
            state.last_cmd_ns = now_ns;
        }

        clock.sleep_for(DurationNs::from_nanos(poll_ns));
    }

    Ok(state)
}

// ============================================================================
// ProfileSection + parse / validate（B2 (2/3) profile matrix L1 + L2）
// ============================================================================

/// `[s1.profile]` runtime profile section（B2 outline §4.2）。
///
/// **B2 单 profile 字段集**（B3 起按 outline §4 扩 joint_delta /
/// normalized 等）：
///
/// - `backends`：编译期 `inventory::submit!` 注册过的 backend 名字，
///   runtime 选哪些进入
///   [`crate::action_backend::BackendRegistry`]。
/// - `command_mode`：B2 仅 `"joint_position"`；其它字面值由
///   [`validate_profile_section`] L2 拒绝。
///
/// **缺失 `[s1.profile]` 走默认**（reviewer 拍板，等价 B1 no-active-action
/// 路径，B2 (3/3) wiring 不打断现有 config）：[`ProfileSection::default_b2`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSection {
    pub backends: Vec<String>,
    pub command_mode: String,
}

impl ProfileSection {
    /// reviewer 拍板：缺失 `[s1.profile]` 时的 B2 默认值 ——
    /// `backends = ["mock_dwell"]` + `command_mode = "joint_position"`。
    /// 等价 B1 no-active-action 路径 + 默认 feature `backend-mock-dwell`。
    #[must_use]
    pub fn default_b2() -> Self {
        Self {
            backends: vec!["mock_dwell".to_string()],
            command_mode: "joint_position".to_string(),
        }
    }
}

/// `[s1.profile]` parse / validate 失败原因。
///
/// **L1（[`parse_profile_section_from_toml`]）**：仅 `Toml` —— TOML
/// 语法错 / unknown field / 类型错 / 默认值非法。
///
/// **L2（[`validate_profile_section`]）**：`UnknownBackend` /
/// `UnsupportedCommandMode` / `JointCountMismatch` —— 需要 registry +
/// hardware + anchor_bias 才能查的语义级一致性。
#[derive(Debug, Error)]
pub enum ProfileError {
    /// L1：TOML 语法错 / `[s1.profile]` 段含 unknown field / 类型错。
    #[error("[s1.profile] TOML parse failed: {0}")]
    Toml(String),
    /// L2：`profile.backends` 里的 name 不在 `BackendRegistry`
    /// （编译期没 link / feature 没启用 / 拼写错）。
    #[error(
        "[s1.profile.backends] unknown backend {name:?}; \
         not registered via inventory::submit! (check Cargo features)"
    )]
    UnknownBackend { name: String },
    /// L2：`profile.command_mode` 不在 B2 支持集合 `{joint_position}`。
    /// B3 起加 `joint_delta` / `normalized` 时一起扩集合 + 加 backend
    /// 自报 `supported_command_modes()`。
    #[error("[s1.profile.command_mode] {value:?} not supported in B2 (only \"joint_position\")")]
    UnsupportedCommandMode { value: String },
    /// L2：`hardware.joints.len()` 与启动锚定 `anchor_bias.len()` 不一致。
    /// 典型成因：S0 state schema 漂移 / TOML 配错 hardware id。
    #[error(
        "joint count mismatch: hardware section has {hardware_joints} joints, \
         anchor bias has {anchor_bias_len}"
    )]
    JointCountMismatch {
        hardware_joints: usize,
        anchor_bias_len: usize,
    },
    /// L2（reviewer 阻断 2 锁定）：`profile.backends` 列表为空 ——
    /// 调用方显式写了 `[s1.profile]` 但没配 `backends`，或 `backends = []`。
    /// 没有任何 enabled backend 时 coordinator 永远拿不到 dispatch 路由
    /// 命中，所有 `submit()` 都会返 `NoBackendForKind` —— 这条错误必须
    /// 在启动期 fail-fast，不能漏到 runtime。
    ///
    /// **不影响**缺失 `[s1.profile]` 默认路径：
    /// [`ProfileSection::default_b2`] 显式含 `backends = ["mock_dwell"]`，
    /// 不会触发本错误。仅"显式 `[s1.profile]` 段 + 空 backends"才进。
    #[error(
        "[s1.profile.backends] is empty; profile must enable at least one backend \
         (B2 default = [\"mock_dwell\"]; check feature flags + spelling)"
    )]
    NoEnabledBackends,
    /// L2（v0.4 候选 2 round 2 (3/4) Q-IK10b (i) ACK）：`profile.backends`
    /// 启用 `backend` 但 `Coordinator::new(solver = None)` —— 即编译进
    /// `backend-ik` feature + profile 写 `ik`，但 binary 漏注入 solver。
    /// 典型成因：feature 拼写错 / robot-s1 漏 feature-gated 构造路径 /
    /// 多臂场景 hardware id 不匹配。必须 startup 期 fail-fast，不能漏到
    /// runtime `submit()` 才报 `BackendStartFailed`。
    ///
    /// 检查由 [`validate_solver_for_profile`] 在 [`Coordinator::new`] 之
    /// 前调用；详同名函数 doc。
    #[error(
        "[s1.profile.backends] includes {backend:?} but Coordinator was constructed without \
         solver injection; check that the corresponding feature flag is enabled and \
         robot-s1 constructs the solver (Q-IK10b (i))"
    )]
    MissingSolver { backend: String },
    /// 混合总线尚未适配的 backend 在启动期拒绝，不能发回旧 Position wire。
    #[error("混合总线尚不支持 backend {backend:?}")]
    UnsupportedMixedBackend { backend: String },
}

/// 顶层 wrapper：daemon `--config` 文件内含 `[s1.profile]` 子段；
/// 跟 B1 [`parse_s1_section_from_toml`] / [`parse_hardware_section_from_toml`]
/// 同款"不 deny_unknown_fields 在 wrapper 层"以静默跳过非
/// `[s1.profile]` 段。
#[derive(Debug, Default, Deserialize)]
struct ProfileWrapper {
    s1: Option<ProfileS1Wrapper>,
}

#[derive(Debug, Default, Deserialize)]
struct ProfileS1Wrapper {
    /// `[s1.profile]` 子段；缺失走 [`ProfileSection::default_b2`]。
    /// `serde(default)` 不报错；`flatten` 让顶层 `[s1]` 字段（B1 配
    /// 项）落到 `_other` 里被忽略。
    profile: Option<ProfileSectionRaw>,
    /// 容纳 B1 已有的 `[s1]` 字段（heartbeat_hz / 等）。**不**
    /// 限制内容（不 deny_unknown_fields），跟 B1 路径解耦。
    #[serde(flatten)]
    _other: toml::Table,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileSectionRaw {
    /// **缺失** = `[]`；validate 见 `backends` 必须非空（at least 1
    /// backend 才能跑 action dispatch；B2 默认 mock_dwell）。
    #[serde(default)]
    backends: Vec<String>,
    /// **缺失** = `"joint_position"`（B2 唯一合法值，跟 default_b2 对齐）。
    #[serde(default = "default_command_mode")]
    command_mode: String,
}

fn default_command_mode() -> String {
    "joint_position".to_string()
}

/// 把 daemon `--config` TOML 文本中的 `[s1.profile]` 段解出
/// [`ProfileSection`]（**仅 L1**：TOML 语法 / 字段类型 / unknown field）。
///
/// **L2（unknown backend / unsupported command mode / joint count
/// mismatch）由 [`validate_profile_section`] 单独跑**（reviewer 拍板拆两
/// 层）。L1 通过不代表 backends 名字一定在 registry 里。
///
/// **行为**：
///
/// - 文本里没有 `[s1]` section → [`ProfileSection::default_b2`]。
/// - 有 `[s1]` 但没 `[s1.profile]` → 同上（reviewer 拍板：缺失等价 B1
///   no-active-action 路径，wiring 不打断现有 config）。
/// - 有 `[s1.profile]` 且字段缺失 → 各字段走 default（`backends = []` /
///   `command_mode = "joint_position"`）；B2 默认 `backends = []` 在
///   validate 阶段会被 "registry 找不到任何匹配" 拒（NoBackendForKind），
///   但**不**在 L1 报错（保持纯 L1 语义）。
/// - 有 `[s1.profile]` 含 unknown field → [`ProfileError::Toml`]。
/// - 文本是 invalid TOML → 同上。
pub fn parse_profile_section_from_toml(toml_text: &str) -> Result<ProfileSection, ProfileError> {
    let wrap: ProfileWrapper =
        toml::from_str(toml_text).map_err(|e| ProfileError::Toml(e.to_string()))?;

    let Some(s1) = wrap.s1 else {
        return Ok(ProfileSection::default_b2());
    };
    let Some(raw) = s1.profile else {
        return Ok(ProfileSection::default_b2());
    };

    Ok(ProfileSection {
        backends: raw.backends,
        command_mode: raw.command_mode,
    })
}

/// L2 语义校验：profile + registry + hardware + anchor_bias 一致性。
///
/// **检查项**（reviewer 拍板验收契约 round 1 #6 + 阻断 2 补强）：
///
/// 1. `profile.backends` 非空 —— 否则 [`ProfileError::NoEnabledBackends`]
///    （reviewer 阻断 2 锁定：空 backends 每次 `submit` 都会返
///    `NoBackendForKind`，必须启动期 fail-fast）。
/// 2. `profile.backends` 中每个 name 必须在 `registry.has(name)`，否则
///    [`ProfileError::UnknownBackend`]。
/// 3. `profile.command_mode` 必须 `== "joint_position"`，否则
///    [`ProfileError::UnsupportedCommandMode`]。reviewer 拍板：B2 仅
///    一个合法值，**不**给 [`crate::action::ActionBackend`] trait 加
///    `supported_command_modes()` 方法（B3 引入新 mode 时再扩）。
/// 4. `hardware.joints.len() == anchor_bias_len`，否则
///    [`ProfileError::JointCountMismatch`]。
///
/// 失败按检查顺序短路返第一个错；多错并存的情况只报第一个 —— 客户端
/// 修一个再跑就能看到下一个。
///
/// **可见性**（B2 (3/3) 升级）：原 `pub(crate)` 改 `pub` —— `BackendRegistry`
/// 在 B2 (3/3) 同步升级为 `pub`，`robot-s1` binary 调用本函数；
/// `crates/robot-s1/tests/action_dispatch.rs` integration test 也跨 test
/// crate 用本函数验 profile 错配 fail-fast 路径。
pub fn validate_profile_section(
    profile: &ProfileSection,
    registry: &crate::action_backend::BackendRegistry,
    hardware: &HardwareSection,
    anchor_bias_len: usize,
) -> Result<(), ProfileError> {
    // 第一项检查（reviewer 阻断 2 锁定）：profile.backends 必须非空。
    // 顺序在 unknown-backend 之前 —— 空列表本质是"profile 错配"，跟
    // 单条 name 拼写错的"配置漂移"是不同语义；让客户端先看到这条更
    // 直观的错。
    if profile.backends.is_empty() {
        return Err(ProfileError::NoEnabledBackends);
    }
    for name in &profile.backends {
        if !registry.has(name) {
            return Err(ProfileError::UnknownBackend { name: name.clone() });
        }
        let supports_mixed = match name.as_str() {
            "hold_pose" | "scripted" | "mock_dwell" => true,
            "ik" => cfg!(all(feature = "backend-ik", feature = "backend-scripted")),
            "simulated_base_command" => cfg!(feature = "test-base-command"),
            _ => false,
        };
        if hardware.feetech_bus2_mixed_raw && !supports_mixed {
            return Err(ProfileError::UnsupportedMixedBackend {
                backend: name.clone(),
            });
        }
    }
    if profile.command_mode != "joint_position" {
        return Err(ProfileError::UnsupportedCommandMode {
            value: profile.command_mode.clone(),
        });
    }
    if hardware.joints.len() != anchor_bias_len {
        return Err(ProfileError::JointCountMismatch {
            hardware_joints: hardware.joints.len(),
            anchor_bias_len,
        });
    }
    Ok(())
}

/// v0.4 候选 2 round 2 (3/4) Q-IK10b (i) startup 期校验：profile 启用的
/// 每个 backend 都必须有对应资源（目前仅 `"ik"` 需要 solver 注入）。
///
/// **何时调用**：robot-s1 binary 在 `Coordinator::new` 之前调（接着
/// [`validate_profile_section`]）；任一错都 exit 3 fail-fast。
///
/// **检查项**：
///
/// - `profile.backends.contains("ik")` 且 `solver_present == false` →
///   [`ProfileError::MissingSolver { backend: "ik" }`]。
///
/// **不检查**：
///
/// - 其它 backend（`hold_pose` / `mock_dwell` / `scripted` 都无 solver
///   依赖）—— 留空集合，未来加新 backend 资源时扩本函数。
/// - feature on + profile 未启用 `"ik"` + 注了 solver —— 允许（多余资
///   源不报错，只防漏注入）。
///
/// **测试形态**（Plan §P.3.2 Q-IK10 (a-i) 路径）：用
/// `BackendRegistry::from_explicit(&[ik_stub])` 模拟 feature on 场景，
/// 然后 `solver_present = false` → MissingSolver；feature off 路径由
/// [`validate_profile_section`] 自身 `UnknownBackend` 拦下，不进本函数。
pub fn validate_solver_for_profile(
    profile: &ProfileSection,
    solver_present: bool,
) -> Result<(), ProfileError> {
    if profile.backends.iter().any(|b| b == "ik") && !solver_present {
        return Err(ProfileError::MissingSolver {
            backend: "ik".to_string(),
        });
    }
    Ok(())
}

// ============================================================================
// CoordinatorState 5-state 机
// ============================================================================
//
// B2 (2/3) 引入的 CoordinatorState / Coordinator / FINISHED_RING_CAPACITY
// / InnerState / CurrentAction 等由 B2 (3/3) `robot-s1` binary wiring
// 接入。B2 (3/3) land 后所有 `#[allow(dead_code)]` 已批量移除（原 wiring
// 接入条件已成立）。

/// reviewer 拍板：B2 coordinator state 5 显式 variant，**不**吃成隐式 flag。
///
/// 转移图（详 outline §5.2）：
///
/// ```text
/// Booting ──[mark_idle / anchor success]──> Idle ──[submit OK]──> Executing
///                                            │                       │
///                                            │      ┌────────────────┼─────────────────┐
///                                            ▼      ▼                ▼                 ▼
///                              [shutdown]           Completed/      Failed(Pre        Failed(Safety/
///                                  │              cancelled normally /Deadline)         Adapter/Internal)
///                                  ▼              │                  │                  │
///                              ShuttingDown       ▼                  ▼                  ▼
///                                                Idle               Idle             Degraded
///                                                                                       │
///                                                                                       ▼
///                                                                  [clear_error]      Idle
/// ```
///
/// **不变量**：
///
/// - `Executing` 唯一持有 `current` 实例（见 [`InnerState`]）；其它 4 状态 `current = None`。
/// - `Degraded` 携带 reason 字符串（来自 [`ActionFailure::SafetyAbort`] /
///   [`ActionFailure::AdapterError`] / [`ActionFailure::InternalError`] 的
///   message）；唯一出口是 [`Coordinator::clear_error`] → `Idle`。
/// - `ShuttingDown` 是终态（B2 不复活）；任何状态收 SIGINT/SIGTERM 后都
///   走 [`Coordinator::mark_shutting_down`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinatorState {
    /// 启动期：构造完毕，anchor read 未完。`submit` 返
    /// [`SubmitError::NotReady`]。
    Booting,
    /// 闲态：B1 hold loop 正在跑（hb + hold cmd 50 Hz）；接受 `submit`。
    Idle,
    /// 单 action 执行中（B2 单 action 序列化）。
    Executing {
        action_id: ActionId,
        started_at_ns: u64,
    },
    /// S1 内部 degraded（reviewer 阻断 2 + 决策 #10 锁定）：B1 hold loop
    /// **继续**（hb + hold cmd 50 Hz 不变），拒新 `submit`。出口 =
    /// `clear_error`。**不**触 S0 watchdog / 不宣称 CONTROL_LOST_*。
    Degraded { reason: String },
    /// 收到 SIGINT/SIGTERM 后；coordinator 不接受新 submit / 不再走
    /// tick 推进。[`run_action_tick_loop`] 检测到此状态后退出。
    ShuttingDown,
}

impl CoordinatorState {
    /// 给错误信息 / debug log 用的人类可读 tag（不暴露具体字段值）。
    fn human_label(&self) -> &'static str {
        match self {
            Self::Booting => "Booting",
            Self::Idle => "Idle",
            Self::Executing { .. } => "Executing",
            Self::Degraded { .. } => "Degraded",
            Self::ShuttingDown => "ShuttingDown",
        }
    }
}

// ============================================================================
// Coordinator 内部状态 + 4 API + tick_action
// ============================================================================

use crate::action::{
    ActionCancelError, ActionContext, ActionEvent, ActionFailure, ActionId, ActionInstance,
    ActionKind, ActionOutcome, ActionRequest, ActionStartError, ActionStatus, ActionTickOutput,
    ActionTicket, ClearError, JointCommandBody, SubmitError,
};
use crate::action_backend::BackendRegistry;
use crate::liveness::TickLoopMetrics;
use crate::state_cache::{
    JointFeedbackSnapshot, StaleOrMissing, StateCacheHandle, StateCacheLiveness,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// `Coordinator::status` 查询的 finished ring 容量。
///
/// reviewer 拍板：terminal 事件发出后保留 cap=128 的 `(ActionId,
/// ActionStatus)` 环；`status()` 先查 EXECUTING 再查 ring，找不到才
/// `None`。**不**做无界 map（防 long-running daemon 慢漏内存）；超 128
/// 淘汰最旧。
pub(crate) const FINISHED_RING_CAPACITY: usize = 128;

/// EXECUTING 时 coordinator 持有的"当前 action"上下文。
///
/// `instance` + `events_tx` 是 backend 实例 + 事件发送端；`pending_cancel`
/// 跟踪 `cancel()` 是否被调过；`last_progress` 给 `status()` 查
/// EXECUTING 时返 [`ActionStatus::Executing { progress }`]。
struct CurrentAction {
    action_id: ActionId,
    instance: Box<dyn ActionInstance>,
    events_tx: std::sync::mpsc::Sender<ActionEvent>,
    /// `cancel()` 调用后置 `true`；下次 `tick_action` 看 backend 收敛
    /// （`Completed` / `Err`）时决定走 [`ActionEvent::Cancelled`] 还是
    /// [`ActionEvent::Failed`]。reviewer 拍板：cancel 不是 terminal，
    /// 收敛 = `Cancelled`，backend 收敛报错 = `Failed`，**不**双发。
    pending_cancel: bool,
    last_progress: f32,
    needs_feedback: bool,
}

/// Coordinator 内部状态（state machine + current action + finished ring）。
///
/// 单 [`Mutex`] 守门 = 简单 + 无 ABBA 风险；tick_action 持锁调
/// `instance.tick()` 是同步的，B2 全程普通线程，`tick()` 不阻塞 IO。
struct InnerState {
    fsm: CoordinatorState,
    current: Option<CurrentAction>,
    finished: VecDeque<(ActionId, ActionStatus)>,
    /// 保持命令目标：初值为启动锚点，任务终态更新或重置。
    /// 配置单关节控制时，仅 Idle 且确认安全断扭才对齐新鲜当前位置。
    /// `Degraded` 也读取本字段，使单关节任务失败后不会回拉启动锚点。
    idle_hold_target: Vec<f64>,
    /// 模拟底盘输入序号高水位；取消、任务结束或清错均不回退。
    #[cfg(feature = "test-base-command")]
    simulated_base_sequence: u64,
}

impl InnerState {
    fn new_booting(anchor_bias: &[f64]) -> Self {
        Self {
            fsm: CoordinatorState::Booting,
            current: None,
            finished: VecDeque::with_capacity(FINISHED_RING_CAPACITY),
            idle_hold_target: anchor_bias.to_vec(),
            #[cfg(feature = "test-base-command")]
            simulated_base_sequence: 0,
        }
    }

    /// 有明确终态目标时保持该完整向量；None沿旧后端语义重置启动锚点。
    /// 单关节中断目标来自最后成功发布的命令，或首次发布前的提交快照。
    fn apply_terminal_hold_target(&mut self, hold_target: Option<Vec<f64>>, anchor_bias: &[f64]) {
        match hold_target {
            Some(t) if t.len() == anchor_bias.len() => {
                self.idle_hold_target = t;
            }
            Some(t) => {
                tracing::warn!(
                    target: "robot_s1::coordinator",
                    got = t.len(),
                    expected = anchor_bias.len(),
                    "successful_hold_target length mismatch; resetting idle_hold_target to anchor_bias"
                );
                self.reset_idle_hold_target(anchor_bias);
            }
            None => self.reset_idle_hold_target(anchor_bias),
        }
    }

    /// 显式 reset `idle_hold_target = anchor_bias`（复用容量，不重分配）。
    fn reset_idle_hold_target(&mut self, anchor_bias: &[f64]) {
        self.idle_hold_target.clear();
        self.idle_hold_target.extend_from_slice(anchor_bias);
    }

    /// 把 `(id, status)` 写入 ring；超 128 淘汰最旧。
    fn push_finished(&mut self, id: ActionId, status: ActionStatus) {
        if self.finished.len() >= FINISHED_RING_CAPACITY {
            self.finished.pop_front();
        }
        self.finished.push_back((id, status));
    }

    /// `status(action_id)` 查询：先 EXECUTING，再 finished ring，否则 None。
    fn lookup_status(&self, action_id: ActionId) -> Option<ActionStatus> {
        if let Some(ref ca) = self.current
            && ca.action_id == action_id
        {
            return Some(ActionStatus::Executing {
                progress: ca.last_progress,
            });
        }
        self.finished
            .iter()
            .find(|(fid, _)| *fid == action_id)
            .map(|(_, s)| s.clone())
    }
}

/// S1 coordinator：state machine + action dispatch + finished status ring。
///
/// **构造**：[`Coordinator::new`] 初始 [`CoordinatorState::Booting`]；
/// binary 在 anchor read 完成后调 [`Coordinator::mark_idle`] 转 Idle，之
/// 后接 RPC `InvokeTask` 等。B2 (2/3) 落 lib API + 状态机；binary wiring
/// 留 B2 (3/3)。
///
/// **线程安全**：所有 4 API + `tick_action` 通过 [`Mutex`] 同步；多线程
/// 并发调用安全（B4 时 robot-rpc-gateway 的 tonic worker 多线程会用得到）。
///
/// **不**owns 时钟：调用方传 [`RobotClock`] 给 [`Coordinator::submit`] /
/// [`Coordinator::tick_action`]。本设计跟 B1 [`tick_once`] 一致，让
/// binary 决定 system vs mock clock。
///
/// **可见性**（B2 (3/3) 升级）：原 `pub(crate)` 改 `pub` —— `robot-s1`
/// binary 接 wiring + integration test 在
/// `crates/robot-s1/tests/action_dispatch.rs` 跨 test crate 用；
/// `BackendRegistry` 在 B2 (3/3) 同步 pub，private-in-public 限制解除。
/// 升级仅服务 in-crate binary + integration test，跟"对外部 crate
/// 暴露"无关（workspace 内目前无外部 crate 把 `robot-s1` 列依赖）。
pub struct Coordinator {
    inner: Mutex<InnerState>,
    /// `[s1.profile.backends]` 启用列表；dispatch filter。
    enabled_backends: Vec<String>,
    /// 全部 link 进 binary 的 backend；dispatch 路由表。
    registry: BackendRegistry,
    /// Backend `start` / `tick` 的 [`ActionContext`] 字段集（非 RT，
    /// 启动期 + tick 期间复用）。
    config: CoordinatorConfig,
    single_joint_config: Option<crate::single_joint::SingleJointConfig>,
    hardware: HardwareSection,
    layout_id: u64,
    anchor_bias: Vec<f64>,
    /// IK solver instance（v0.4 候选 2 round 2 (3/4) 加；Q-IK3 (a) ACK
    /// `Option<Box<dyn ArmIkSolver>>` 持有所有权，**不** `Arc`）。
    ///
    /// **构造期注入** ：由 [`Self::new`] 接 `solver` 参数。robot-s1
    /// binary 按 `backend-ik` feature 决定是否构造 `XLeRobotArmIkSolver`：
    ///
    /// - feature off → solver 类型不在 binary，传 `None`。
    /// - feature on → 启动期构造 `Box::new(XLeRobotArmIkSolver::new(Side::Right))`
    ///   传 `Some(_)`。
    ///
    /// **startup 期校验**：profile 启用 `"ik"` + solver `None` →
    /// [`validate_solver_for_profile`] fail-fast（Q-IK10b (i)）。
    /// validate 由 binary 在 `Coordinator::new` 之前调用。
    ///
    /// **借出语义**：[`Self::submit`] / [`Self::cancel`] / [`Self::tick_action`]
    /// 构 [`ActionContext`] 时用 `.as_deref()` 借为 `Option<&dyn ArmIkSolver>`；
    /// `dyn ArmIkSolver` 自带 `Send + Sync`，跨线程持 `Coordinator` 锁
    /// 安全。
    solver: Option<Box<dyn robot_kinematics::ArmIkSolver>>,
    /// S1 常驻 state cache handle（v0.5 候选 E 段(5)，Spec D3 第 1 条 +
    /// P.2.5）：`submit` 的 Scripted 路径经 [`StateCacheHandle::latest_fresh`]
    /// 取 `scripted_start_pose`（joint_count + layout + 新鲜度校验；失败
    /// → `ActionStartError::Rejected` → wire `FailedPrecondition`，**不**
    /// fallback）。
    ///
    /// **注入**：[`Self::with_state_cache`]（robot-s1 在 cache spawn 成功
    /// 后注入；缺省 `None` = 未接 cache，一切 Scripted submit 被拒——测试 /
    /// 无 cache 场景显式可见，非静默降级）。
    ///
    /// **线程卫生**：[`Self::take_state_cache`] 让 daemon shutdown 序列显式
    /// `stop + join`；未 take 时随 `Coordinator` Drop 走
    /// `StateCacheHandle::drop` best-effort stop+join（AC-E20，禁
    /// mem::forget）。`Mutex` 仅为 `take` 的内部可变性；锁序 = `inner` →
    /// `state_cache`（submit 持 inner 时短暂取 state_cache），`take` /
    /// `probe` 只取 `state_cache`，无 ABBA。
    state_cache: Mutex<Option<StateCacheHandle>>,
    /// tick loop 进度计数（v0.5 候选 E 段(8)）：[`run_action_tick_loop`] 每次
    /// 迭代 / 每帧 hb 推进；robot-s1 把它挂 systemd watchdog observer，
    /// test-only freeze control 写其 `freeze_until_ns_*`。
    loop_metrics: Arc<TickLoopMetrics>,
}

impl Coordinator {
    /// 构造一个新的 coordinator，初始 `Booting`；binary 在 anchor read
    /// 完后调 [`Self::mark_idle`] 转 Idle。
    ///
    /// `enabled_backends` / `registry` / `config` / `hardware` /
    /// `layout_id` / `anchor_bias` 已在调用方完成 `validate_profile_section`
    /// L2 校验（unknown backend / unsupported command mode / joint count
    /// mismatch 都已通过）；本构造不再二次校验，避免重复昂贵的 registry
    /// 查表。
    #[must_use]
    pub fn new(
        config: CoordinatorConfig,
        hardware: HardwareSection,
        layout_id: u64,
        mut anchor_bias: Vec<f64>,
        registry: BackendRegistry,
        enabled_backends: Vec<String>,
        solver: Option<Box<dyn robot_kinematics::ArmIkSolver>>,
    ) -> Self {
        hardware.clear_wheel_targets(&mut anchor_bias);
        Self {
            inner: Mutex::new(InnerState::new_booting(&anchor_bias)),
            enabled_backends,
            registry,
            config,
            single_joint_config: None,
            hardware,
            layout_id,
            anchor_bias,
            solver,
            state_cache: Mutex::new(None),
            loop_metrics: Arc::new(TickLoopMetrics::default()),
        }
    }

    /// tick loop 进度计数 `Arc`（watchdog observer / freeze control / 诊断）。
    #[must_use]
    pub fn loop_metrics(&self) -> Arc<TickLoopMetrics> {
        Arc::clone(&self.loop_metrics)
    }

    /// state cache 线程 liveness 面（`None` = 未注入 cache）。
    #[must_use]
    pub fn state_cache_liveness(&self) -> Option<Arc<StateCacheLiveness>> {
        self.lock_state_cache()
            .as_ref()
            .map(StateCacheHandle::liveness)
    }

    /// 注入 S1 常驻 state cache handle（v0.5 候选 E 段(5)；builder 形态，
    /// 保持 [`Self::new`] 7 参签名 0 改）。robot-s1 在
    /// `spawn_state_cache_thread` 成功后调；注入后 `submit` 的 Scripted
    /// 路径才可能通过（详 [`Self::submit`]）。
    #[must_use]
    pub fn with_state_cache(self, cache: StateCacheHandle) -> Self {
        *self.lock_state_cache() = Some(cache);
        self
    }

    /// 注入已经按硬件布局校验的运动配置；缺省保持单关节入口关闭。
    #[must_use]
    pub fn with_single_joint_config(
        mut self,
        config: Option<crate::single_joint::SingleJointConfig>,
    ) -> Self {
        self.single_joint_config = config;
        self
    }

    /// 发布成功后才推进中断保持目标，不把生成但未发送的命令当作已执行。
    pub fn command_published(&self, published_at_ns: u64) {
        if let Some(action) = self.lock_inner().current.as_mut() {
            action.instance.command_published(published_at_ns);
        }
    }

    /// 是否已注入 state cache（诊断 / 测试）。
    #[must_use]
    pub fn has_state_cache(&self) -> bool {
        self.lock_state_cache().is_some()
    }

    /// 取走 state cache handle（一次性；之后 `has_state_cache() == false`，
    /// Scripted submit 全拒）。robot-s1 shutdown 序列用：tick loop 退出 +
    /// RR / drain 线程全 join 后 `take` → `stop()` → `join()` 显式收线程，
    /// 不依赖 `Coordinator` Drop 时机（AC-E20 线程卫生）。
    pub fn take_state_cache(&self) -> Option<StateCacheHandle> {
        self.lock_state_cache().take()
    }

    /// 只读探针：以 `now` 评估 cache 当前帧（与 `submit` Scripted 路径
    /// **同一**校验）。`None` = 未注入 cache。给 in-process 测试等待
    /// cache 线程收到首帧 / 诊断用；**不**改变任何状态。
    pub fn probe_state_cache(&self, now: MonoRawNs) -> Option<Result<Vec<f64>, StaleOrMissing>> {
        self.lock_state_cache()
            .as_ref()
            .map(|cache| cache.latest_fresh(now))
    }

    /// 当前 `Idle` hold target 快照（v0.5 候选 E 段(5) D3 第 3 条）。
    /// [`run_action_tick_loop`] `Idle` 分支每 cmd 周期读；初值 = 启动
    /// `anchor_bias`；单关节中断后也保持其已确认的终态目标。
    #[must_use]
    pub fn idle_hold_target(&self) -> Vec<f64> {
        self.lock_inner().idle_hold_target.clone()
    }

    /// 使能前对齐复用同一状态缓存和命令流；坏帧、越界和上扭状态均不刷新。
    /// Web 仍须核对实际发布的命令与新鲜反馈，不以本地目标更新作为使能确认。
    fn idle_hold_target_for_tick(&self, clock: &dyn RobotClock) -> Vec<f64> {
        let mut inner = self.lock_inner();
        if let Some(config) = self.single_joint_config.as_ref()
            && matches!(inner.fsm, CoordinatorState::Idle)
            && inner.current.is_none()
            && let Some(mut positions) = self
                .lock_state_cache()
                .as_ref()
                .and_then(|cache| cache.latest_torque_off_positions(clock).ok().flatten())
            && config
                .validate_hold_positions(&self.hardware, &positions)
                .is_ok()
        {
            self.hardware.clear_wheel_targets(&mut positions);
            inner.idle_hold_target = positions;
        }
        inner.idle_hold_target.clone()
    }

    fn hold_command(&self, positions: &[f64], seq: u64, now_ns: u64) -> Vec<u8> {
        build_hardware_hold_command(
            &self.config,
            &self.hardware,
            self.layout_id,
            positions,
            seq,
            now_ns,
        )
    }

    /// 借用本 coordinator 的 [`CoordinatorConfig`]（构造期固化，不再变）。
    /// 给 [`run_action_tick_loop`] 等需要 cfg.heartbeat_period / command_period
    /// 的路径用。
    #[must_use]
    pub fn config(&self) -> &CoordinatorConfig {
        &self.config
    }

    /// `JointCommand.layout_id`（构造期传入，不再变）。
    #[must_use]
    pub fn layout_id(&self) -> u64 {
        self.layout_id
    }

    /// 借用启动锚定的 `anchor_bias`（构造期传入，不再变）。
    /// 旧后端终态重置保持目标时仍使用本值，使能前对齐不回写启动锚点。
    #[must_use]
    pub fn anchor_bias(&self) -> &[f64] {
        &self.anchor_bias
    }

    /// `Booting → Idle`（binary 在 anchor read 完成后调用）。
    ///
    /// 仅 `Booting` 状态合法；其它状态返
    /// [`ClearError::NotDegraded`]（复用枚举；B3+ 若需要单独的
    /// transition 错误再拆出）。
    pub fn mark_idle(&self) -> Result<(), ClearError> {
        let mut inner = self.lock_inner();
        match inner.fsm {
            CoordinatorState::Booting => {
                inner.fsm = CoordinatorState::Idle;
                Ok(())
            }
            ref other => Err(ClearError::NotDegraded(other.human_label().to_string())),
        }
    }

    /// 任意状态 → `ShuttingDown`（binary 收 SIGINT/SIGTERM 后调用）。
    /// 幂等 + 不可逆；已 `ShuttingDown` 时无副作用。
    pub fn mark_shutting_down(&self) {
        let mut inner = self.lock_inner();
        // 若当前 EXECUTING，drop current 让 backend instance 析构 + sender
        // drop（持票方 recv 会拿到 RecvError 表示 channel 关闭）。
        inner.current = None;
        inner.fsm = CoordinatorState::ShuttingDown;
    }

    /// 提交一次 action 请求；`Idle` 时启动 backend 转 `Executing`，其它
    /// 4 状态返对应 [`SubmitError`]。
    ///
    /// **失败映射**（reviewer 验收契约表）：
    ///
    /// - `Booting` → [`SubmitError::NotReady`]
    /// - `Executing(id)` → [`SubmitError::BusyExecuting(id)`]
    /// - `Degraded` → [`SubmitError::CoordinatorDegraded`]
    /// - `ShuttingDown` → [`SubmitError::ShuttingDown`]
    ///
    /// 路由 / 校验失败：
    /// - [`ActionRequest::validate`] → [`SubmitError::InvalidRequest`]
    /// - [`BackendRegistry::dispatch`] →
    ///   [`SubmitError::NoBackendForKind`] / [`SubmitError::UnsupportedSchema`]
    /// - **Scripted 起点（v0.5 候选 E 段(5)，Spec D3 第 1 条）**：
    ///   `kind == Scripted` 时先取 `scripted_start_pose =
    ///   state_cache.latest_fresh(now)`——未注入 cache / 无帧 / stale /
    ///   layout 不匹配 / joint count 错位 →
    ///   [`SubmitError::BackendStartFailed`]`(`[`ActionStartError::Rejected`]`)`
    ///   （既有 wire 映射 → `FailedPrecondition`），**绝不** fallback 到
    ///   `anchor_bias`；状态保持 Idle。
    /// - [`ActionBackend::start`] → [`SubmitError::BackendStartFailed`]
    ///
    /// **状态优先**（reviewer 阻断 1 锁定）：`Booting` / `Executing` /
    /// `Degraded` / `ShuttingDown` 永远先返对应状态错误，**不**让坏请求 /
    /// unknown backend 抢先返 [`SubmitError::InvalidRequest`] /
    /// [`SubmitError::NoBackendForKind`] 等掩盖真实状态语义。只有
    /// `Idle` 才进入 [`ActionRequest::validate`] / [`BackendRegistry::dispatch`]
    /// / [`ActionBackend::start`] 路径。
    ///
    /// `req` 按值传：API 语义上"submit 接管请求所有权"对齐 RPC InvokeTask；
    /// 实际函数体内仅借用引用 + Copy 字段，clippy `needless_pass_by_value`
    /// 误报，allow 掉。
    #[allow(clippy::needless_pass_by_value)]
    pub fn submit(
        &self,
        req: ActionRequest,
        clock: &dyn RobotClock,
    ) -> Result<ActionTicket, SubmitError> {
        // 一把锁守住整个临界区：state 检查 → validate → dispatch → start
        // → 状态转移 + 写 current + 发 Started 事件，全在锁内一气呵成
        // （reviewer 阻断锁定：为保证单 action 原子转移，start 必须跟状态
        // 转移在同一临界区。如要锁外 start 需引入额外 reservation 状态，
        // B2 没必要）。
        //
        // **backend.start 不得阻塞 IO**：本路径在普通线程（非 RT），持锁
        // 期间若有 backend 做磁盘 / 网络等阻塞调用会拖慢整个 coordinator
        // 入队。B2 的 MockDwell 仅算 `now + duration_ms` + Vec 分配，O(1)
        // 非阻塞；B3+ 真 backend 落地时同样契约 —— 阻塞资源初始化必须
        // 提前到 backend 注册期（`inventory::submit!` 静态实例 ctor）或
        // 显式异步启动路径，**不**进 start hot path。
        let mut inner = self.lock_inner();

        // 状态优先（reviewer 阻断 1）：非 Idle 状态永远先返状态错误，不
        // 看 req / dispatch 是否合法。
        match &inner.fsm {
            CoordinatorState::Booting => return Err(SubmitError::NotReady),
            CoordinatorState::Executing { action_id, .. } => {
                return Err(SubmitError::BusyExecuting(*action_id));
            }
            CoordinatorState::Degraded { .. } => return Err(SubmitError::CoordinatorDegraded),
            CoordinatorState::ShuttingDown => return Err(SubmitError::ShuttingDown),
            CoordinatorState::Idle => {}
        }

        // Idle 路径：依次校验 / 路由 / 启动 backend；任一失败 early-return，
        // 状态保持 Idle 不变。
        let now_ns = clock.now().0;
        req.validate(now_ns)?;
        let backend =
            self.registry
                .dispatch(&self.enabled_backends, req.kind, req.payload_schema_id)?;

        #[cfg(feature = "test-base-command")]
        let base_sequence = if backend.name() == "simulated_base_command" {
            let sequence = crate::action_backend::simulated_base_command::request_sequence(&req)?;
            if sequence <= inner.simulated_base_sequence {
                return Err(
                    ActionStartError::Rejected("模拟底盘命令序号已消费，禁止重放".into()).into(),
                );
            }
            Some(sequence)
        } else {
            None
        };

        // 混合 IK 与 Scripted 同取提交时的总线快照，保留夹爪位置。
        let mixed_ik = req.kind == ActionKind::Ik && self.hardware.feetech_bus2_mixed_raw;
        let feedback = match req.kind {
            ActionKind::Ik if mixed_ik => Some(self.fetch_scripted_start_snapshot(clock)?),
            ActionKind::Scripted => Some(self.fetch_scripted_start_snapshot(clock)?),
            ActionKind::DwellMs | ActionKind::HoldPose | ActionKind::Ik => None,
        };

        let ctx = ActionContext {
            clock,
            layout_id: self.layout_id,
            hardware: &self.hardware,
            config: &self.config,
            anchor_bias: &self.anchor_bias,
            scripted_start_pose: feedback
                .as_ref()
                .map(|snapshot| snapshot.positions.as_slice()),
            single_joint_config: self.single_joint_config.as_ref(),
            feedback: feedback.as_ref(),
            // submit 路径不在 publish frame seq 流上；backend.start 通常
            // 不产 body bytes（仅返 ActionInstance），用 0 哨兵安全（详
            // ActionContext::command_seq doc）。
            command_seq: 0,
            // v0.4 候选 2 round 2 (3/4) IK solver borrow plumb
            // （Q-IK3 (a) ACK `Option<&dyn ArmIkSolver>`，**不** Arc /
            // **不** clone；详 ActionContext::solver doc）。
            solver: self.solver.as_deref(),
        };
        // start 失败：状态不转移，仍 Idle。
        let instance = backend.start(&req, &ctx)?;
        let needs_feedback = mixed_ik
            || matches!(
                req.payload_schema_id,
                robot_msg::schema_id::SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD
                    | robot_msg::schema_id::SCHEMA_RIGHT_ARM_HOME_PAYLOAD
            );
        #[cfg(feature = "test-base-command")]
        let needs_feedback = {
            if let Some(sequence) = base_sequence {
                inner.simulated_base_sequence = sequence;
            }
            needs_feedback || base_sequence.is_some()
        };

        let action_id = req.action_id;
        let (tx, rx) = std::sync::mpsc::channel::<ActionEvent>();

        // 第一帧 Started 事件：`send().ok()` 吞 SendError —— ticket 已被
        // drop 时 sender 仍能 send（mpsc::channel 是 unbuffered 但 sender
        // 不会因 receiver drop 而 panic；返 Err 即可），coordinator
        // **不**把 send 失败当 action failure（reviewer 拍板）。
        let _ = tx.send(ActionEvent::Started { at_ns: now_ns });

        inner.fsm = CoordinatorState::Executing {
            action_id,
            started_at_ns: now_ns,
        };
        inner.current = Some(CurrentAction {
            action_id,
            instance,
            events_tx: tx,
            pending_cancel: false,
            last_progress: 0.0,
            needs_feedback,
        });

        Ok(ActionTicket {
            action_id,
            events: rx,
        })
    }

    /// 网页的下一条按住输入只更新当前模拟动作，绝不重新创建任务或事件线程。
    #[cfg(feature = "test-base-command")]
    pub(crate) fn renew_simulated_base(
        &self,
        req: &ActionRequest,
        clock: &dyn RobotClock,
    ) -> Result<(), SubmitError> {
        let mut inner = self.lock_inner();
        let now = clock.now().0;
        req.validate(now)?;
        let sequence = crate::action_backend::simulated_base_command::request_sequence(req)?;
        if sequence <= inner.simulated_base_sequence {
            return Err(
                ActionStartError::Rejected("模拟底盘命令序号已消费，禁止重放".into()).into(),
            );
        }
        let current = inner
            .current
            .as_mut()
            .filter(|c| c.action_id == req.action_id && !c.pending_cancel)
            .ok_or_else(|| ActionStartError::Rejected("底盘按住动作已失效".into()))?;
        current
            .instance
            .renew_simulated_base(req, self.layout_id, now)?;
        inner.simulated_base_sequence = sequence;
        Ok(())
    }

    /// Cooperative cancel：set backend `cancel()` flag + 标记
    /// `pending_cancel`；下次 [`Self::tick_action`] 收敛 backend 输出。
    ///
    /// **失败路径**：
    ///
    /// - 当前不在 EXECUTING / `action_id` 不匹配 →
    ///   [`ActionCancelError::NotExecuting`]。
    /// - Backend `cancel()` 自身报错 → [`ActionCancelError::BackendError`]。
    ///
    /// **reviewer 拍板**：cancel **不**是 terminal —— 即使返 `Ok` 也仍
    /// `Executing`；下次 tick 收敛后 emit `Cancelled`（正常）或 `Failed`
    /// （backend 报错）；**不**双发 `Completed + Cancelled`。
    pub fn cancel(
        &self,
        action_id: ActionId,
        clock: &dyn RobotClock,
    ) -> Result<(), ActionCancelError> {
        let mut inner = self.lock_inner();
        let Some(current) = inner.current.as_mut() else {
            return Err(ActionCancelError::NotExecuting);
        };
        if current.action_id != action_id {
            return Err(ActionCancelError::NotExecuting);
        }

        let mut ctx = ActionContext {
            clock,
            layout_id: self.layout_id,
            hardware: &self.hardware,
            config: &self.config,
            anchor_bias: &self.anchor_bias,
            // cancel 路径不取起点（仅 submit Scripted 路径填 Some）。
            scripted_start_pose: None,
            single_joint_config: self.single_joint_config.as_ref(),
            feedback: None,
            // cancel 路径不在 publish frame seq 流上；backend.cancel 通
            // 常仅 set flag 不产 body bytes，用 0 哨兵安全。
            command_seq: 0,
            // IK solver borrow（v0.4 候选 2 round 2 (3/4)；详 submit 同款）。
            solver: self.solver.as_deref(),
        };
        current.instance.cancel(&mut ctx)?;
        current.pending_cancel = true;
        Ok(())
    }

    /// 查询 action 状态：先 EXECUTING（返 `Executing { progress }`），
    /// 再 finished ring（cap 128）；都没找到返 `None`。
    ///
    /// reviewer 拍板：**不**做无界 map；超 128 终态淘汰最旧。
    #[must_use]
    pub fn status(&self, action_id: ActionId) -> Option<ActionStatus> {
        let inner = self.lock_inner();
        inner.lookup_status(action_id)
    }

    /// `Degraded → Idle`（B2 (2/3) stub 内部 API；B4 接 RPC `ClearError`）。
    ///
    /// 仅 `Degraded` 合法；其它 4 状态返
    /// [`ClearError::NotDegraded`] 携当前状态 label。
    pub fn clear_error(&self) -> Result<(), ClearError> {
        let mut inner = self.lock_inner();
        match inner.fsm {
            CoordinatorState::Degraded { .. } => {
                inner.fsm = CoordinatorState::Idle;
                Ok(())
            }
            ref other => Err(ClearError::NotDegraded(other.human_label().to_string())),
        }
    }

    /// 当前状态快照（debug / 测试用 / B2 (3/3) binary tick loop 分支判断）。
    #[must_use]
    pub fn current_state(&self) -> CoordinatorState {
        self.lock_inner().fsm.clone()
    }

    /// [`run_action_tick_loop`] 调用的单次动作推进：
    ///
    /// - 状态 `Executing` → 调 backend.`tick()`；按返回值更新 state /
    ///   推 [`ActionEvent`] / 写 finished ring；返 `Some(body)` 让调用方
    ///   发布这帧 `JointCommand`。
    /// - 其它 4 状态（`Booting` / `Idle` / `Degraded` / `ShuttingDown`）
    ///   → 不动 state，返 `None`。调用方按状态处理保持命令与心跳。
    ///
    /// **失败转移**（outline §6.5 + reviewer 拍板）：
    ///
    /// | failure | new state |
    /// |---|---|
    /// | `PreconditionFailed` | `Idle` |
    /// | `DeadlineExceeded` | `Idle` |
    /// | `SafetyAbort(reason)` | `Degraded { reason }` |
    /// | `AdapterError(reason)` | `Degraded { reason }` |
    /// | `InternalError(reason)` | `Degraded { reason }` |
    ///
    /// 成功使用successful_hold_target；取消、PartialSuccess及失败使用
    /// interrupted_hold_target。新单关节请求在中断后保持最后已发送目标，
    /// 其他后端默认None，沿旧语义重置启动锚点；取消不表示已断扭。
    pub fn tick_action(
        &self,
        clock: &dyn RobotClock,
        command_seq: u64,
    ) -> Option<JointCommandBody> {
        let mut inner = self.lock_inner();
        if !matches!(inner.fsm, CoordinatorState::Executing { .. }) {
            return None;
        }

        // 借出 current 让 instance.tick 调用；其它字段保持 inner 锁内。
        // ActionContext::clock 透传调用方传的 clock；command_seq 透传调
        // 用方（典型 run_action_tick_loop）传的本帧 cmd_seq —— backend
        // 产 body bytes 时把它作为 source_task_id（详 ActionContext doc
        // + B3 outline §3.4 round 2 阻断 1 修法）。
        let feedback = inner
            .current
            .as_ref()
            .filter(|action| action.needs_feedback)
            .and_then(|_| {
                self.lock_state_cache()
                    .as_ref()
                    .and_then(|cache| cache.latest_fresh_snapshot(clock).ok())
            });
        let now_ns = clock.now().0;
        let mut ctx = ActionContext {
            clock,
            layout_id: self.layout_id,
            hardware: &self.hardware,
            config: &self.config,
            anchor_bias: &self.anchor_bias,
            // tick 路径不取起点（Scripted 已在 start() 期 snapshot）。
            scripted_start_pose: None,
            single_joint_config: self.single_joint_config.as_ref(),
            feedback: feedback.as_ref(),
            command_seq,
            // IK solver borrow（v0.4 候选 2 round 2 (3/4)；详 submit 同款；
            // tick_action 路径上 IkBackend 自己用 ctx.solver 调 solve()，
            // hold_pose / scripted backend 不读本字段）。
            solver: self.solver.as_deref(),
        };

        let Some(current) = inner.current.as_mut() else {
            // state 是 Executing 但 current 为空：不一致；防御性回 Idle
            // （非 Success 路径 → idle_hold_target reset = anchor_bias）。
            inner.fsm = CoordinatorState::Idle;
            inner.reset_idle_hold_target(&self.anchor_bias);
            return None;
        };

        let result = current.instance.tick(&mut ctx);
        let action_id = current.action_id;
        let pending_cancel = current.pending_cancel;

        match result {
            Ok(ActionTickOutput::Continue { progress, command }) => {
                current.last_progress = progress;
                // Continue 期间不发 terminal；emit Progress（持续推让 audit
                // 能看到流式 stream，B3+ scheduler 可加 delta threshold 优化）。
                let _ = current.events_tx.send(ActionEvent::Progress {
                    progress,
                    at_ns: now_ns,
                });
                command
            }
            Ok(ActionTickOutput::Completed(outcome)) => {
                // 终态：转 Idle；按 pending_cancel 决定 Cancelled vs Completed；
                // ring buffer 写一条；drop current（events_tx drop → ticket
                // 端 RecvError = 流关闭）。
                //
                // 成功与中断目标分开，取消不能被末帧成功覆盖。
                let hold_target: Option<Vec<f64>> = if pending_cancel {
                    current.instance.interrupted_hold_target()
                } else {
                    match &outcome {
                        ActionOutcome::Success => current.instance.successful_hold_target(),
                        ActionOutcome::PartialSuccess { .. } => {
                            current.instance.interrupted_hold_target()
                        }
                    }
                };
                if pending_cancel {
                    let _ = current
                        .events_tx
                        .send(ActionEvent::Cancelled { at_ns: now_ns });
                    inner.push_finished(action_id, ActionStatus::Cancelled);
                } else {
                    let _ = current.events_tx.send(ActionEvent::Completed {
                        outcome: outcome.clone(),
                        at_ns: now_ns,
                    });
                    inner.push_finished(action_id, ActionStatus::Completed(outcome));
                }
                inner.current = None;
                inner.fsm = CoordinatorState::Idle;
                inner.apply_terminal_hold_target(hold_target, &self.anchor_bias);
                None
            }
            Err(failure) => {
                // Failed仍按原错误类型转移；单关节中断保持目标同时用于Idle/Degraded。
                let hold_target = current.instance.interrupted_hold_target();
                let _ = current.events_tx.send(ActionEvent::Failed {
                    failure: failure.clone(),
                    at_ns: now_ns,
                });
                inner.push_finished(action_id, ActionStatus::Failed(failure.clone()));
                inner.current = None;
                inner.fsm = match failure {
                    ActionFailure::PreconditionFailed(_) | ActionFailure::DeadlineExceeded => {
                        CoordinatorState::Idle
                    }
                    ActionFailure::SafetyAbort(reason)
                    | ActionFailure::AdapterError(reason)
                    | ActionFailure::InternalError(reason) => CoordinatorState::Degraded { reason },
                };
                inner.apply_terminal_hold_target(hold_target, &self.anchor_bias);
                None
            }
        }
    }

    /// `Mutex` 拿锁；poison 时降级用 `into_inner()` 继续，记一行 debug log。
    /// reviewer 拍板：B2 不为 mutex poison 单独引入新错误类型，按"前
    /// holder panic 是 bug 但不影响 B2 后续 invariant 守门"处理。
    fn lock_inner(&self) -> std::sync::MutexGuard<'_, InnerState> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!(
                    target: "robot_s1::coordinator",
                    "Coordinator inner mutex poisoned; recovering with into_inner"
                );
                poisoned.into_inner()
            }
        }
    }

    /// `state_cache` 锁；poison 语义同 [`Self::lock_inner`]。
    fn lock_state_cache(&self) -> std::sync::MutexGuard<'_, Option<StateCacheHandle>> {
        match self.state_cache.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!(
                    target: "robot_s1::coordinator",
                    "Coordinator state_cache mutex poisoned; recovering with into_inner"
                );
                poisoned.into_inner()
            }
        }
    }

    /// Scripted 起点取值（v0.5 候选 E 段(5)，Spec D3 第 1 条失败语义）：
    /// 未注入 cache / `latest_fresh` 任一校验失败 →
    /// `SubmitError::BackendStartFailed(ActionStartError::Rejected(..))`
    /// （既有 wire 映射 → `FailedPrecondition`），**不** fallback。
    fn fetch_scripted_start_snapshot(
        &self,
        clock: &dyn RobotClock,
    ) -> Result<JointFeedbackSnapshot, SubmitError> {
        let guard = self.lock_state_cache();
        let Some(cache) = guard.as_ref() else {
            return Err(SubmitError::BackendStartFailed(ActionStartError::Rejected(
                "scripted_start_pose unavailable: no S1 state cache attached \
                 (D3: no fallback to anchor_bias)"
                    .into(),
            )));
        };
        cache.latest_fresh_snapshot(clock).map_err(|e| {
            SubmitError::BackendStartFailed(ActionStartError::Rejected(format!(
                "scripted_start_pose unavailable: {e} (D3: no fallback to anchor_bias)"
            )))
        })
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use flatbuffers::root;
    use robot_core::time::MockClock;
    use robot_msg::build_helpers::{build_joint_command, build_joint_state};
    use robot_msg::msg::JointCommand as FbJointCommand;
    use robot_transport::mock::MockChannel;

    // ----- CoordinatorConfig defaults / parse -----

    #[test]
    fn config_default_values() {
        let cfg = CoordinatorConfig::default();
        assert!((cfg.heartbeat_hz - 50.0).abs() < 1e-9);
        assert!((cfg.command_hz - 50.0).abs() < 1e-9);
        assert_eq!(cfg.on_timeout, StaleAction::Hold);
        assert_eq!(cfg.deadline_ms, 500);
        assert_eq!(cfg.anchor_timeout_ms, 1000);
        assert_eq!(cfg.max_stale_ms, 200);
        assert_eq!(cfg.source_id, 2);
        assert_eq!(cfg.priority, 10);
        assert_eq!(cfg.deadline_offset_ns(), 500_000_000);
    }

    #[test]
    fn parse_empty_toml_returns_defaults() {
        let cfg = parse_s1_section_from_toml("").unwrap();
        assert_eq!(cfg, CoordinatorConfig::default());
    }

    #[test]
    fn parse_toml_without_s1_section_returns_defaults() {
        // 模拟 daemon `--config` 共享路径：[robot] + [[hardware]] 段都
        // 不带 [s1] 时走全套默认。
        let toml = r#"
[robot]
id             = "smoke"
config_version = "0.3.0"

[[hardware]]
id        = "smoke_arm"
adapter   = "mock_hard_rt"
joints    = ["smoke_j0"]
motor_ids = [1]
"#;
        let cfg = parse_s1_section_from_toml(toml).unwrap();
        assert_eq!(cfg, CoordinatorConfig::default());
    }

    #[test]
    fn parse_toml_partial_override() {
        let toml = r#"
[s1]
heartbeat_hz = 100.0
deadline_ms  = 250
on_timeout   = "stop"
"#;
        let cfg = parse_s1_section_from_toml(toml).unwrap();
        assert!((cfg.heartbeat_hz - 100.0).abs() < 1e-9);
        assert_eq!(cfg.deadline_ms, 250);
        assert_eq!(cfg.on_timeout, StaleAction::Stop);
        // 其它字段仍走默认。
        assert!((cfg.command_hz - 50.0).abs() < 1e-9);
        assert_eq!(cfg.anchor_timeout_ms, 1000);
        assert_eq!(cfg.priority, 10);
    }

    #[test]
    fn parse_toml_full_override() {
        let toml = r#"
[s1]
heartbeat_hz       = 25.0
command_hz         = 30.0
on_timeout         = "zero"
deadline_ms        = 750
anchor_timeout_ms  = 2000
max_stale_ms       = 50
source_id          = 99
priority           = 200
hold_strategy      = "anchor"
"#;
        let cfg = parse_s1_section_from_toml(toml).unwrap();
        assert!((cfg.heartbeat_hz - 25.0).abs() < 1e-9);
        assert!((cfg.command_hz - 30.0).abs() < 1e-9);
        assert_eq!(cfg.on_timeout, StaleAction::Zero);
        assert_eq!(cfg.deadline_ms, 750);
        assert_eq!(cfg.anchor_timeout_ms, 2000);
        assert_eq!(cfg.max_stale_ms, 50);
        assert_eq!(cfg.source_id, 99);
        assert_eq!(cfg.priority, 200);
    }

    #[test]
    fn parse_toml_rejects_zero_heartbeat_hz() {
        let toml = "[s1]\nheartbeat_hz = 0.0\n";
        let err = parse_s1_section_from_toml(toml).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidHeartbeatHz(v) if v.abs() < 1e-9));
    }

    #[test]
    fn parse_toml_rejects_negative_command_hz() {
        let toml = "[s1]\ncommand_hz = -10.0\n";
        let err = parse_s1_section_from_toml(toml).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidCommandHz(v) if (v - -10.0).abs() < 1e-9));
    }

    #[test]
    fn parse_toml_rejects_nan_heartbeat_hz() {
        let toml = "[s1]\nheartbeat_hz = nan\n";
        let err = parse_s1_section_from_toml(toml).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidHeartbeatHz(v) if v.is_nan()));
    }

    #[test]
    fn parse_toml_rejects_zero_deadline_ms() {
        let toml = "[s1]\ndeadline_ms = 0\n";
        let err = parse_s1_section_from_toml(toml).unwrap_err();
        assert!(matches!(err, ConfigError::ZeroDeadlineMs(0)));
    }

    #[test]
    fn parse_toml_rejects_zero_anchor_timeout_ms() {
        let toml = "[s1]\nanchor_timeout_ms = 0\n";
        let err = parse_s1_section_from_toml(toml).unwrap_err();
        assert!(matches!(err, ConfigError::ZeroAnchorTimeoutMs(0)));
    }

    #[test]
    fn parse_toml_rejects_zero_max_stale_ms() {
        let toml = "[s1]\nmax_stale_ms = 0\n";
        let err = parse_s1_section_from_toml(toml).unwrap_err();
        assert!(matches!(err, ConfigError::ZeroMaxStaleMs(0)));
    }

    #[test]
    fn parse_toml_rejects_unknown_on_timeout() {
        let toml = "[s1]\non_timeout = \"foo\"\n";
        let err = parse_s1_section_from_toml(toml).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidOnTimeout(s) if s == "foo"));
    }

    #[test]
    fn parse_toml_rejects_non_anchor_hold_strategy() {
        let toml = "[s1]\nhold_strategy = \"zero\"\n";
        let err = parse_s1_section_from_toml(toml).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidHoldStrategy(s) if s == "zero"));
    }

    #[test]
    fn parse_toml_rejects_unknown_field_in_s1() {
        let toml = "[s1]\nheartbeatHz = 50.0\n"; // typo: camelCase 写错
        let err = parse_s1_section_from_toml(toml).unwrap_err();
        assert!(
            matches!(err, ConfigError::Toml(_)),
            "expected TOML parse error for unknown field, got {err:?}"
        );
    }

    #[test]
    fn parse_toml_rejects_invalid_syntax() {
        let toml = "[s1\nheartbeat_hz = 50.0\n";
        let err = parse_s1_section_from_toml(toml).unwrap_err();
        assert!(matches!(err, ConfigError::Toml(_)));
    }

    // ----- build_hold_command -----

    fn mixed_test_hardware() -> HardwareSection {
        let text = include_str!("../../../configs/xlerobot-bus2.toml")
            .lines()
            .filter(|line| !line.trim_start().starts_with("feetech_bus2_mixed_raw"))
            .collect::<Vec<_>>()
            .join("\n")
            .replace(
                "[[hardware]]",
                "[[hardware]]\nfeetech_bus2_mixed_raw = true",
            );
        parse_hardware_section_from_toml(&text, "right_arm_full_with_base").unwrap()
    }

    #[test]
    fn mixed_hardware_flag_defaults_false_and_requires_full_bus() {
        let text = "[[hardware]]\nid='arm'\njoints=['j']\nmotor_ids=[1]";
        let hardware = parse_hardware_section_from_toml(text, "arm").unwrap();
        assert!(!hardware.feetech_bus2_mixed_raw);
        assert!(mixed_test_hardware().feetech_bus2_mixed_raw);
        let invalid = "[[hardware]]\nid='right_arm_full_with_base'\njoints=['j']\nmotor_ids=[1]\nfeetech_bus2_mixed_raw=true";
        assert!(parse_hardware_section_from_toml(invalid, "right_arm_full_with_base").is_err());
    }

    #[test]
    fn mixed_hardware_hold_zeroes_wheel_targets_without_reinterpreting_them_as_raw() {
        let mut hardware = mixed_test_hardware();
        hardware.joints.reverse();
        hardware.motor_ids.reverse();
        let positions: Vec<_> = hardware
            .motor_ids
            .iter()
            .map(|&id| if id <= 6 { f64::from(id) / 10.0 } else { 500.0 })
            .collect();
        let cfg = CoordinatorConfig {
            on_timeout: StaleAction::Zero,
            ..CoordinatorConfig::default()
        };
        let bytes = build_hardware_hold_command(&cfg, &hardware, 0xCAFE, &positions, 42, 100);
        let command = root::<FbJointCommand>(&bytes).unwrap();
        assert_eq!(command.mode(), ControlMode::MixedPositionRawVelocity);
        assert_eq!(command.on_timeout(), StaleAction::Hold);
        assert_eq!(command.target().unwrap().len(), 9);
        assert_eq!(
            command.raw_velocities().unwrap().iter().collect::<Vec<_>>(),
            [0; 9]
        );
        for (slot, &id) in hardware.motor_ids.iter().enumerate() {
            assert_eq!(
                command.target().unwrap().get(slot),
                if id <= 6 { positions[slot] } else { 0.0 }
            );
        }
        assert_eq!(command.source_task_id(), 42);
        assert_eq!(command.deadline_ns(), 500_000_100);
        hardware.feetech_bus2_mixed_raw = false;
        assert_eq!(
            build_hardware_hold_command(&cfg, &hardware, 0xCAFE, &positions, 42, 100),
            build_hold_command(&cfg, 0xCAFE, &positions, 42, 100),
        );
    }

    #[test]
    fn mixed_hold_mask_preserves_input_and_original_zip_semantics() {
        let mut hardware = mixed_test_hardware();
        let cfg = CoordinatorConfig::default();
        let values = [
            0.1,
            -0.2,
            -0.0,
            f64::NAN,
            f64::INFINITY,
            -3.0,
            f64::NAN,
            1234.0,
            -99.0,
            42.0,
        ];
        for reverse in [false, true] {
            if reverse {
                hardware.motor_ids.reverse();
            }
            for count in [0, 3, 6, 9, 10] {
                let positions = &values[..count];
                let before: Vec<_> = positions.iter().map(|v| v.to_bits()).collect();
                let mut expected = positions.to_vec();
                for (position, &id) in expected.iter_mut().zip(&hardware.motor_ids) {
                    if id >= 7 {
                        *position = 0.0;
                    }
                }
                let bytes =
                    build_hardware_hold_command(&cfg, &hardware, 42, positions, 123, u64::MAX);
                assert_eq!(
                    bytes,
                    robot_msg::build_helpers::build_mixed_joint_command(
                        42,
                        &expected,
                        &[0; 9],
                        u64::MAX,
                        cfg.priority,
                        123,
                    )
                );
                assert_eq!(
                    before,
                    positions.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn build_hold_command_field_alignment() {
        let cfg = CoordinatorConfig::default();
        let bias = [0.1, -0.2, 0.3];
        let now_ns = 1_000_000_000;
        let bytes = build_hold_command(&cfg, 0xCAFE, &bias, /* source_task_id */ 7, now_ns);

        let cmd = root::<FbJointCommand<'_>>(&bytes).unwrap();
        assert_eq!(cmd.layout_id(), 0xCAFE);
        assert_eq!(cmd.mode(), ControlMode::Position);
        let target: Vec<f64> = cmd.target().unwrap().iter().collect();
        assert_eq!(target, vec![0.1, -0.2, 0.3]);
        assert_eq!(cmd.deadline_ns(), now_ns + 500_000_000);
        assert!(
            cmd.deadline_ns() > now_ns,
            "deadline must be strictly > now"
        );
        assert_eq!(cmd.priority(), 10);
        assert_eq!(cmd.on_timeout(), StaleAction::Hold);
        assert_eq!(cmd.source_task_id(), 7);
    }

    #[test]
    fn build_hold_command_respects_config_overrides() {
        let cfg = CoordinatorConfig {
            on_timeout: StaleAction::Stop,
            deadline_ms: 100,
            priority: 200,
            ..CoordinatorConfig::default()
        };
        let bias = [1.0];
        let bytes = build_hold_command(&cfg, 1, &bias, 0, /* now_ns */ 500_000);
        let cmd = root::<FbJointCommand<'_>>(&bytes).unwrap();
        assert_eq!(cmd.on_timeout(), StaleAction::Stop);
        assert_eq!(cmd.priority(), 200);
        assert_eq!(cmd.deadline_ns(), 500_000 + 100_000_000);
    }

    #[test]
    fn build_hold_command_deadline_saturates_on_overflow() {
        let cfg = CoordinatorConfig::default();
        let bias = [0.0];
        let bytes = build_hold_command(&cfg, 0, &bias, 0, u64::MAX);
        let cmd = root::<FbJointCommand<'_>>(&bytes).unwrap();
        assert_eq!(
            cmd.deadline_ns(),
            u64::MAX,
            "saturating add must not wrap to small value"
        );
    }

    // ----- parse_anchor_from_state_bytes -----

    fn fresh_state_bytes(positions: &[f64], stamp_ns: u64) -> Vec<u8> {
        build_joint_state(0xBEEF, stamp_ns, positions, &[], &[], "")
    }

    #[test]
    fn parse_anchor_success() {
        let positions = [1.5, -2.5, 0.0];
        let bytes = fresh_state_bytes(&positions, 100);
        let bias = parse_anchor_from_state_bytes(
            &bytes, /* expected_joints */ 3, 0xBEEF, /* now_ns */ 150,
            /* max_stale_ns */ 1_000,
        )
        .unwrap();
        assert_eq!(bias, vec![1.5, -2.5, 0.0]);
    }

    #[test]
    fn parse_anchor_rejects_future_source_timestamp() {
        let body = fresh_state_bytes(&[0.25], 1_000_000_001);
        assert!(matches!(
            parse_anchor_from_state_bytes(&body, 1, 0xBEEF, 1_000_000_000, 200_000_000),
            Err(AnchorError::Stale { .. })
        ));
    }

    #[test]
    fn parse_anchor_rejects_wrong_layout_before_stale_or_positions() {
        let bytes = fresh_state_bytes(&[f64::NAN], 0);
        let err = parse_anchor_from_state_bytes(&bytes, 1, 0xCAFE, 100, 1_000).unwrap_err();
        assert!(matches!(
            err,
            AnchorError::LayoutMismatch {
                expected: 0xCAFE,
                got: 0xBEEF,
            }
        ));
    }

    #[test]
    fn parse_anchor_rejects_each_nonfinite_position_with_its_index() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for index in 0..8 {
                let mut positions = [0.0; 8];
                positions[index] = value;
                let bytes = fresh_state_bytes(&positions, 100);
                let err = parse_anchor_from_state_bytes(&bytes, 8, 0xBEEF, 150, 1_000).unwrap_err();
                assert!(
                    matches!(err, AnchorError::NonFinitePosition { index: got } if got == index)
                );
            }
        }
    }

    #[test]
    fn parse_anchor_rejects_stale_sample() {
        let bytes = fresh_state_bytes(&[1.0], /* stamp_ns */ 1_000);
        let err = parse_anchor_from_state_bytes(
            &bytes, 1, 0xBEEF, /* now_ns */ 5_000, /* max_stale_ns */ 1_000,
        )
        .unwrap_err();
        assert!(
            matches!(err, AnchorError::Stale { stamp_ns, now_ns, max_stale_ns }
                if stamp_ns == 1_000 && now_ns == 5_000 && max_stale_ns == 1_000),
            "expected Stale, got {err:?}"
        );
    }

    #[test]
    fn parse_anchor_rejects_zero_stamp() {
        // stamp_ns == 0 视为 stale（FlatBuffers 默认值；publisher 没显式 set）。
        let bytes = fresh_state_bytes(&[1.0], /* stamp_ns */ 0);
        let err = parse_anchor_from_state_bytes(
            &bytes, 1, 0xBEEF, /* now_ns */ 100, /* max_stale_ns */ 1_000,
        )
        .unwrap_err();
        assert!(
            matches!(err, AnchorError::Stale { stamp_ns: 0, .. }),
            "expected Stale (stamp=0), got {err:?}"
        );
    }

    #[test]
    fn parse_anchor_rejects_joint_count_mismatch() {
        let bytes = fresh_state_bytes(&[1.0, 2.0], 100);
        let err = parse_anchor_from_state_bytes(
            &bytes, /* expected_joints */ 3, 0xBEEF, /* now_ns */ 150,
            /* max_stale_ns */ 1_000,
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                AnchorError::JointCountMismatch {
                    expected: 3,
                    got: 2
                }
            ),
            "expected JointCountMismatch, got {err:?}"
        );
    }

    #[test]
    fn parse_anchor_rejects_garbage_bytes() {
        // 真正不合法的 fbs：长度 < 4 byte（root offset 都读不到）→
        // verify 必挂 Decode。4 字节全 0 反而会被 verify 当作"空表"
        // 接受（root offset = 0 → 全字段走默认）；那条路径走的是
        // stamp_ns = 0 触发 Stale，已被 parse_anchor_rejects_zero_stamp
        // 覆盖。
        let bytes = vec![0u8; 1];
        let err = parse_anchor_from_state_bytes(&bytes, 1, 0xBEEF, 100, 1_000).unwrap_err();
        assert!(matches!(err, AnchorError::Decode(_)), "got {err:?}");
    }

    #[test]
    fn parse_anchor_rejects_wrong_schema_bytes() {
        // 把一帧 JointCommand bytes 当 JointState 解 → fbs verify 应该挂。
        let cmd_bytes = build_joint_command(
            0xCAFE,
            ControlMode::Position,
            &[1.0],
            100,
            0,
            StaleAction::Hold,
            0,
        );
        let err = parse_anchor_from_state_bytes(&cmd_bytes, 1, 0xCAFE, 200, 1_000).unwrap_err();
        // 不强制必须是 Decode；JointCommand bytes 解成 JointState 时
        // 可能 verify 通过（结构兼容）但语义不对（position_rad 缺失），
        // 也可能 Decode；两条路径都接受。
        assert!(
            matches!(
                err,
                AnchorError::Decode(_)
                    | AnchorError::MissingPositionField
                    | AnchorError::Stale { .. }
            ),
            "expected decode-like error, got {err:?}"
        );
    }

    // ----- read_anchor_from_s0_state -----

    /// 把一帧 `JointState` body bytes publish 到 mock channel。
    fn publish_state(channel: &MockChannel<markers::JointState>, body: &[u8], stamp_ns: u64) {
        use robot_transport::traits::Publisher;
        let mut pub_ = channel.publisher();
        let mut writer = pub_.loan(8 * 1024).expect("loan");
        writer.set_body_bytes(body);
        writer.set_stamp_ns(stamp_ns);
        writer.set_seq(1);
        writer.set_source_id(/* S0 */ 1);
        writer.publish().expect("publish");
    }

    #[test]
    fn read_anchor_success_from_mock_subscriber() {
        let channel = MockChannel::<markers::JointState>::new();
        // S0 在 anchor 调用前已 publish 过一帧 state（典型 deploy 序：
        // robot-s0 先 ready，robot-s1 `After=robot-s0`）。
        let now_ns: u64 = 1_000_000;
        let body = fresh_state_bytes(&[0.5, -0.5], now_ns - 50_000);
        publish_state(&channel, &body, now_ns - 50_000);

        let mut sub = channel.subscriber();
        let clock = MockClock::new(MonoRawNs(now_ns));
        let bias = read_anchor_from_s0_state(
            &mut sub,
            /* expected_joints */ 2,
            0xBEEF,
            &clock,
            /* timeout */ Duration::from_secs(1),
            /* max_stale */ Duration::from_millis(200),
            /* poll_interval */ Duration::from_millis(10),
        )
        .unwrap();
        assert_eq!(bias, vec![0.5, -0.5]);
    }

    #[test]
    fn read_anchor_timeout_when_no_publisher() {
        let channel = MockChannel::<markers::JointState>::new();
        // 无 publisher 推 sample —— anchor 应在 timeout 内 fail-fast。
        let mut sub = channel.subscriber();
        let now_ns: u64 = 1_000_000_000;
        let clock = MockClock::new(MonoRawNs(now_ns));
        let err = read_anchor_from_s0_state(
            &mut sub,
            /* expected_joints */ 1,
            0xBEEF,
            &clock,
            /* timeout */ Duration::from_millis(50),
            /* max_stale */ Duration::from_millis(200),
            /* poll_interval */ Duration::from_millis(5),
        )
        .unwrap_err();
        assert!(
            matches!(err, AnchorError::Timeout { timeout_ms: 50 }),
            "expected Timeout(50), got {err:?}"
        );
    }

    #[test]
    fn read_anchor_stale_sample_fails_fast() {
        let channel = MockChannel::<markers::JointState>::new();
        let now_ns: u64 = 10_000_000_000;
        // state.stamp = now - 500ms，max_stale = 200ms → stale。
        let stale_stamp = now_ns - 500_000_000;
        let body = fresh_state_bytes(&[1.0], stale_stamp);
        publish_state(&channel, &body, stale_stamp);

        let mut sub = channel.subscriber();
        let clock = MockClock::new(MonoRawNs(now_ns));
        let err = read_anchor_from_s0_state(
            &mut sub,
            1,
            0xBEEF,
            &clock,
            Duration::from_secs(1),
            Duration::from_millis(200),
            Duration::from_millis(10),
        )
        .unwrap_err();
        assert!(
            matches!(err, AnchorError::Stale { .. }),
            "expected Stale, got {err:?}"
        );
    }

    #[test]
    fn read_anchor_joint_count_mismatch_fails_fast() {
        let channel = MockChannel::<markers::JointState>::new();
        let now_ns: u64 = 10_000_000_000;
        let body = fresh_state_bytes(&[1.0, 2.0], now_ns - 10_000);
        publish_state(&channel, &body, now_ns - 10_000);

        let mut sub = channel.subscriber();
        let clock = MockClock::new(MonoRawNs(now_ns));
        let err = read_anchor_from_s0_state(
            &mut sub,
            /* expected_joints */ 3,
            0xBEEF,
            &clock,
            Duration::from_secs(1),
            Duration::from_millis(200),
            Duration::from_millis(10),
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                AnchorError::JointCountMismatch {
                    expected: 3,
                    got: 2
                }
            ),
            "expected JointCountMismatch, got {err:?}"
        );
    }

    #[test]
    fn read_anchor_invalid_first_frame_does_not_retry_with_valid_frame() {
        use robot_transport::sample::SampleLease;

        struct SequenceSubscriber {
            frames: std::vec::IntoIter<SampleLease<markers::JointState>>,
            calls: usize,
        }

        impl LatestSubscriber<markers::JointState> for SequenceSubscriber {
            fn latest(
                &mut self,
            ) -> Result<Option<SampleLease<markers::JointState>>, TransportError> {
                self.calls += 1;
                Ok(self.frames.next())
            }
        }

        let now = 1_000_000;
        for (layout, value) in [
            (0xCAFE, 0.0),
            (0xBEEF, f64::NAN),
            (0xBEEF, f64::INFINITY),
            (0xBEEF, f64::NEG_INFINITY),
        ] {
            let channel = MockChannel::<markers::JointState>::new();
            let mut subscriber = channel.subscriber();
            let mut positions = [0.0; 8];
            positions[6] = value;
            let bad = build_joint_state(layout, now, &positions, &[], &[], "");
            publish_state(&channel, &bad, now);
            let first = subscriber.latest().unwrap().unwrap();
            publish_state(&channel, &fresh_state_bytes(&[0.0; 8], now), now);
            let second = subscriber.latest().unwrap().unwrap();
            let mut sequence = SequenceSubscriber {
                frames: vec![first, second].into_iter(),
                calls: 0,
            };
            let clock = MockClock::new(MonoRawNs(now));
            let result = read_anchor_from_s0_state(
                &mut sequence,
                8,
                0xBEEF,
                &clock,
                Duration::from_secs(1),
                Duration::from_millis(200),
                Duration::from_millis(10),
            );
            match layout {
                0xCAFE => assert!(matches!(result, Err(AnchorError::LayoutMismatch { .. }))),
                _ => assert!(matches!(
                    result,
                    Err(AnchorError::NonFinitePosition { index: 6 })
                )),
            }
            assert_eq!(sequence.calls, 1, "坏帧不得等待后续有效帧");
            assert_eq!(sequence.frames.len(), 1);
            assert_eq!(clock.now(), MonoRawNs(now), "坏帧失败不得消耗等待预算");
        }
    }

    // ----- parse_hardware_section_from_toml -----

    #[test]
    fn parse_hardware_finds_matching_section() {
        let toml = r#"
[robot]
id             = "rb"
config_version = "0.3.0"

[[hardware]]
id        = "left_arm"
adapter   = "mock_hard_rt"
joints    = ["a", "b"]
motor_ids = [1, 2]

[[hardware]]
id        = "right_arm"
adapter   = "feetech_sts3215"
joints    = ["r1", "r2", "r3"]
motor_ids = [1, 2, 3]
"#;
        let section = parse_hardware_section_from_toml(toml, "right_arm").unwrap();
        assert_eq!(section.id, "right_arm");
        assert_eq!(
            section.joints,
            vec!["r1".to_string(), "r2".to_string(), "r3".to_string()]
        );
    }

    #[test]
    fn parse_hardware_returns_not_found_for_missing_id() {
        let toml = r#"
[[hardware]]
id        = "left_arm"
adapter   = "mock_hard_rt"
joints    = ["a"]
motor_ids = [1]
"#;
        let err = parse_hardware_section_from_toml(toml, "right_arm").unwrap_err();
        assert!(
            matches!(err, ConfigError::HardwareIdNotFound(ref s) if s == "right_arm"),
            "got {err:?}"
        );
    }

    #[test]
    fn parse_hardware_rejects_empty_joints() {
        let toml = r#"
[[hardware]]
id        = "smoke_arm"
adapter   = "mock_hard_rt"
joints    = []
motor_ids = []
"#;
        let err = parse_hardware_section_from_toml(toml, "smoke_arm").unwrap_err();
        assert!(
            matches!(err, ConfigError::EmptyJoints(ref s) if s == "smoke_arm"),
            "got {err:?}"
        );
    }

    #[test]
    fn parse_hardware_ignores_extra_fields() {
        // daemon-only 字段（adapter / address / extras / calibration）应
        // 被静默吃掉，不引起 parse 失败。
        let toml = r#"
[[hardware]]
id          = "bus2"
adapter     = "feetech_sts3215"
address     = "/dev/ttyUSB0"
calibration = "configs/calib.json"
joints      = ["j0"]
motor_ids   = [1]
baud        = 1_000_000
poll_hz     = 200
"#;
        let section = parse_hardware_section_from_toml(toml, "bus2").unwrap();
        assert_eq!(section.id, "bus2");
        assert_eq!(section.joints, vec!["j0".to_string()]);
        assert_eq!(section.motor_ids, vec![1]);
    }

    // ----- v0.5 候选 E 段(2)：motor_ids typed + 身份锁 parser（AC-E3）-----

    #[test]
    fn parse_hardware_missing_motor_ids_is_toml_error() {
        // 必填无 default：缺失 = TOML parse 错误（S1 config 错误族 exit 3）。
        let toml = r#"
[[hardware]]
id      = "arm"
adapter = "mock_hard_rt"
joints  = ["j0"]
"#;
        let err = parse_hardware_section_from_toml(toml, "arm").unwrap_err();
        assert!(matches!(err, ConfigError::Toml(_)), "got {err:?}");
        assert_eq!(err.exit_code(), 3);
    }

    #[test]
    fn parse_hardware_rejects_empty_motor_ids() {
        let toml = "[[hardware]]\nid = \"arm\"\nadapter = \"m\"\n\
                    joints = [\"j0\"]\nmotor_ids = []\n";
        let err = parse_hardware_section_from_toml(toml, "arm").unwrap_err();
        assert!(
            matches!(err, ConfigError::EmptyMotorIds(ref s) if s == "arm"),
            "got {err:?}"
        );
    }

    #[test]
    fn parse_hardware_rejects_len_mismatch() {
        let toml = "[[hardware]]\nid = \"arm\"\nadapter = \"m\"\n\
                    joints = [\"j0\", \"j1\"]\nmotor_ids = [1]\n";
        let err = parse_hardware_section_from_toml(toml, "arm").unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::MotorIdsLenMismatch {
                    joints: 2,
                    motor_ids: 1,
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn parse_hardware_rejects_dup_joint_name() {
        let toml = "[[hardware]]\nid = \"arm\"\nadapter = \"m\"\n\
                    joints = [\"j0\", \"j0\"]\nmotor_ids = [1, 2]\n";
        let err = parse_hardware_section_from_toml(toml, "arm").unwrap_err();
        assert!(
            matches!(err, ConfigError::DupJointName { ref name, .. } if name == "j0"),
            "got {err:?}"
        );
    }

    #[test]
    fn parse_hardware_rejects_dup_motor_id() {
        let toml = "[[hardware]]\nid = \"arm\"\nadapter = \"m\"\n\
                    joints = [\"j0\", \"j1\"]\nmotor_ids = [1, 1]\n";
        let err = parse_hardware_section_from_toml(toml, "arm").unwrap_err();
        assert!(
            matches!(err, ConfigError::DupMotorId { motor_id: 1, .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn parse_robot_id_ok_and_missing_section_err() {
        let ok =
            parse_robot_id_from_toml("[robot]\nid = \"xlerobot_left\"\nconfig_version = \"0.5\"\n")
                .unwrap();
        assert_eq!(ok, "xlerobot_left");
        let err = parse_robot_id_from_toml("[s1]\nheartbeat_hz = 50.0\n").unwrap_err();
        assert!(matches!(err, ConfigError::Toml(_)), "got {err:?}");
    }

    /// PX-3（段(2) 复核 P0 回修）：仓库两份**权威 arm config** 必须启用
    /// Scripted backend——缺省 profile 只有 mock_dwell
    /// （[`ProfileSection::default_b2`]），Scripted 请求会落
    /// NoBackendForKind，候选 E 真机工作流直接失败。顺带锁 pair 的 D2
    /// 身份与 hardware 段可解析（motor_ids 全套校验路径）。
    #[test]
    fn authoritative_arm_configs_enable_scripted() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../configs");
        for (file, robot_id, hardware_id) in [
            ("xlerobot-left-arm.toml", "xlerobot_left", "left_arm"),
            ("xlerobot-right-arm.toml", "xlerobot_right", "right_arm"),
        ] {
            let text = std::fs::read_to_string(format!("{root}/{file}")).unwrap();
            let profile = parse_profile_section_from_toml(&text).unwrap();
            assert!(
                profile.backends.iter().any(|b| b == "scripted"),
                "{file}: profile 缺 scripted，backends={:?}",
                profile.backends
            );
            assert_eq!(profile.command_mode, "joint_position", "{file}");
            assert_eq!(parse_robot_id_from_toml(&text).unwrap(), robot_id, "{file}");
            let hw = parse_hardware_section_from_toml(&text, hardware_id).unwrap();
            assert_eq!(hw.joints.len(), 6, "{file}");
            assert_eq!(hw.motor_ids, vec![1, 2, 3, 4, 5, 6], "{file}");
        }
    }

    // ----- TickState / tick_once -----

    #[test]
    fn tick_state_new_seqs_are_zero() {
        let s = TickState::new(123);
        assert_eq!(s.cmd_seq, 0);
        assert_eq!(s.hb_seq, 0);
        assert_eq!(s.elapsed_ns(123), 0);
        assert_eq!(s.elapsed_ns(456), 333);
    }

    #[test]
    fn tick_once_publishes_cmd_and_hb_when_due() {
        let cfg = CoordinatorConfig::default(); // 50 Hz / 50 Hz
        let cmd_ch = MockChannel::<markers::JointCommand>::new();
        let hb_ch = MockChannel::<markers::Heartbeat>::new();
        let mut cmd_pub = cmd_ch.publisher();
        let mut hb_pub = hb_ch.publisher();

        let now_ns: u64 = 1_000_000_000;
        let mut state = TickState::new(now_ns);

        // 第一次 tick @ now_ns → 计时器=0，未到 period，啥都不发。
        let r0 = tick_once(
            &cfg,
            &mut state,
            0xCAFE,
            &[0.5],
            now_ns,
            &mut hb_pub,
            &mut cmd_pub,
        )
        .unwrap();
        assert_eq!(r0, TickPublished::default());

        // 第二次 tick @ now_ns + 25ms → 都过 20ms 周期，都发。
        let later = now_ns + 25_000_000;
        let r1 = tick_once(
            &cfg,
            &mut state,
            0xCAFE,
            &[0.5],
            later,
            &mut hb_pub,
            &mut cmd_pub,
        )
        .unwrap();
        assert!(r1.cmd && r1.hb);
        assert_eq!(state.cmd_seq, 1);
        assert_eq!(state.hb_seq, 1);
        assert_eq!(cmd_ch.publishes(), 1);
        assert_eq!(hb_ch.publishes(), 1);
    }

    #[test]
    fn tick_once_skips_when_not_due() {
        let cfg = CoordinatorConfig::default();
        let cmd_ch = MockChannel::<markers::JointCommand>::new();
        let hb_ch = MockChannel::<markers::Heartbeat>::new();
        let mut cmd_pub = cmd_ch.publisher();
        let mut hb_pub = hb_ch.publisher();

        let now_ns: u64 = 1_000_000_000;
        let mut state = TickState::new(now_ns);
        // 第一次推 25ms → publish。
        let _ = tick_once(
            &cfg,
            &mut state,
            0,
            &[0.0],
            now_ns + 25_000_000,
            &mut hb_pub,
            &mut cmd_pub,
        )
        .unwrap();
        // 紧接着 +1ms 再 tick → 不到 20ms period，不发。
        let r = tick_once(
            &cfg,
            &mut state,
            0,
            &[0.0],
            now_ns + 26_000_000,
            &mut hb_pub,
            &mut cmd_pub,
        )
        .unwrap();
        assert_eq!(r, TickPublished::default());
        assert_eq!(cmd_ch.publishes(), 1);
        assert_eq!(hb_ch.publishes(), 1);
    }

    // ----- ProfileSection parse + validate（B2 (2/3) reviewer 验收契约
    //        round 1 #6：profile 双层校验必须 fail-fast，3 路径有测试）-----

    use crate::action::{
        ActionBackend, ActionContext, ActionInstance, ActionKind, ActionRequest, ActionStartError,
        NO_PAYLOAD_SCHEMA_ID,
    };
    use crate::action_backend::BackendRegistry;

    /// 测试用 stub backend：声明 `name` / `supported_kinds` /
    /// `supported_schemas`；start 返 stub error，本测试不走 instance 路径。
    struct ProfileTestBackend {
        name: &'static str,
    }

    impl ActionBackend for ProfileTestBackend {
        fn name(&self) -> &str {
            self.name
        }
        fn supported_kinds(&self) -> &[ActionKind] {
            const KINDS: &[ActionKind] = &[ActionKind::DwellMs];
            KINDS
        }
        fn supported_schemas(&self) -> &[u32] {
            const SCHEMAS: &[u32] = &[NO_PAYLOAD_SCHEMA_ID];
            SCHEMAS
        }
        fn start(
            &self,
            _req: &ActionRequest,
            _ctx: &ActionContext<'_>,
        ) -> Result<Box<dyn ActionInstance>, ActionStartError> {
            Err(ActionStartError::Rejected("stub".into()))
        }
    }

    static PROFILE_STUB_MOCK: ProfileTestBackend = ProfileTestBackend { name: "mock_dwell" };

    #[test]
    fn mixed_profile_rejects_unadapted_backend_at_startup() {
        let registry = BackendRegistry::from_explicit(&[&PROFILE_STUB_OTHER]);
        let profile = ProfileSection {
            backends: vec!["other".into()],
            command_mode: "joint_position".into(),
        };
        assert!(
            matches!(validate_profile_section(&profile, &registry, &mixed_test_hardware(), 9), Err(ProfileError::UnsupportedMixedBackend { backend }) if backend == "other")
        );
    }
    static PROFILE_STUB_OTHER: ProfileTestBackend = ProfileTestBackend { name: "other" };

    fn registry_with_mock_dwell() -> BackendRegistry {
        BackendRegistry::from_explicit(&[&PROFILE_STUB_MOCK as &'static dyn ActionBackend])
    }

    fn registry_with_two() -> BackendRegistry {
        BackendRegistry::from_explicit(&[
            &PROFILE_STUB_MOCK as &'static dyn ActionBackend,
            &PROFILE_STUB_OTHER as &'static dyn ActionBackend,
        ])
    }

    fn hardware_n(joint_count: usize) -> HardwareSection {
        HardwareSection {
            id: "test_arm".into(),
            joints: (0..joint_count).map(|i| format!("j{i}")).collect(),
            motor_ids: (1..=u8::try_from(joint_count).expect("test joint_count fits u8")).collect(),
            feetech_bus2_mixed_raw: false,
            calibration_range_tolerances_raw: None,
        }
    }

    // ----- L1 parse_profile_section_from_toml -----

    #[test]
    fn parse_profile_returns_default_b2_when_section_missing() {
        // reviewer 拍板：缺失 [s1.profile] 等价 B1 no-active-action 路径，
        // backends = ["mock_dwell"]，command_mode = "joint_position"。
        let toml = r#"
[robot]
id = "x"

[[hardware]]
id     = "arm"
joints = ["j0"]
"#;
        let p = parse_profile_section_from_toml(toml).expect("L1 ok");
        assert_eq!(p, ProfileSection::default_b2());
        assert_eq!(p.backends, vec!["mock_dwell".to_string()]);
        assert_eq!(p.command_mode, "joint_position");
    }

    #[test]
    fn parse_profile_returns_default_b2_when_s1_present_but_profile_missing() {
        // [s1] 段在但没 [s1.profile] 子段也走 default_b2（reviewer 拍板）。
        let toml = "
[s1]
heartbeat_hz = 50.0
";
        let p = parse_profile_section_from_toml(toml).expect("L1 ok");
        assert_eq!(p, ProfileSection::default_b2());
    }

    #[test]
    fn parse_profile_full_override() {
        let toml = r#"
[s1.profile]
backends     = ["mock_dwell", "other"]
command_mode = "joint_position"
"#;
        let p = parse_profile_section_from_toml(toml).expect("L1 ok");
        assert_eq!(
            p.backends,
            vec!["mock_dwell".to_string(), "other".to_string()]
        );
        assert_eq!(p.command_mode, "joint_position");
    }

    #[test]
    fn parse_profile_partial_keeps_defaults_for_missing_field() {
        // 只填 backends，command_mode 走 default。
        let toml = r#"
[s1.profile]
backends = ["mock_dwell"]
"#;
        let p = parse_profile_section_from_toml(toml).expect("L1 ok");
        assert_eq!(p.backends, vec!["mock_dwell".to_string()]);
        assert_eq!(p.command_mode, "joint_position");
    }

    #[test]
    fn parse_profile_rejects_unknown_field_in_profile_section() {
        let toml = r#"
[s1.profile]
backends     = ["mock_dwell"]
typo_extra   = "boom"
"#;
        let err = parse_profile_section_from_toml(toml).expect_err("expected Err");
        assert!(matches!(err, ProfileError::Toml(_)), "got {err:?}");
    }

    #[test]
    fn parse_profile_rejects_invalid_toml_syntax() {
        let toml = "[s1.profile\nbackends = []\n";
        let err = parse_profile_section_from_toml(toml).expect_err("expected Err");
        assert!(matches!(err, ProfileError::Toml(_)), "got {err:?}");
    }

    #[test]
    fn parse_profile_rejects_wrong_field_type() {
        // backends 该是 array of string，给个 number 应 L1 类型错。
        let toml = "
[s1.profile]
backends = 42
";
        let err = parse_profile_section_from_toml(toml).expect_err("expected Err");
        assert!(matches!(err, ProfileError::Toml(_)), "got {err:?}");
    }

    // ----- L2 validate_profile_section -----

    #[test]
    fn validate_profile_happy_path() {
        let p = ProfileSection::default_b2();
        let r = registry_with_mock_dwell();
        let hw = hardware_n(2);
        validate_profile_section(&p, &r, &hw, /* anchor_bias_len */ 2).expect("validate ok");
    }

    #[test]
    fn validate_profile_rejects_unknown_backend() {
        // backends 含 registry 没有的 name → UnknownBackend。
        let p = ProfileSection {
            backends: vec!["does_not_exist".into()],
            command_mode: "joint_position".into(),
        };
        let r = registry_with_mock_dwell();
        let hw = hardware_n(1);
        let err = validate_profile_section(&p, &r, &hw, 1).expect_err("expected Err");
        match err {
            ProfileError::UnknownBackend { name } => {
                assert_eq!(name, "does_not_exist");
            }
            other => panic!("expected UnknownBackend, got {other:?}"),
        }
    }

    #[test]
    fn validate_profile_unknown_backend_short_circuits_first_match() {
        // 多个 unknown 时只报第一个（短路）。
        let p = ProfileSection {
            backends: vec!["ghost_a".into(), "ghost_b".into()],
            command_mode: "joint_position".into(),
        };
        let r = registry_with_mock_dwell();
        let hw = hardware_n(1);
        let err = validate_profile_section(&p, &r, &hw, 1).expect_err("expected Err");
        match err {
            ProfileError::UnknownBackend { name } => {
                assert_eq!(name, "ghost_a");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn validate_profile_rejects_unsupported_command_mode() {
        // B2 仅 "joint_position"；其它字面值（含 B3 "joint_delta"）拒。
        let p = ProfileSection {
            backends: vec!["mock_dwell".into()],
            command_mode: "joint_delta".into(),
        };
        let r = registry_with_mock_dwell();
        let hw = hardware_n(1);
        let err = validate_profile_section(&p, &r, &hw, 1).expect_err("expected Err");
        match err {
            ProfileError::UnsupportedCommandMode { value } => {
                assert_eq!(value, "joint_delta");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn validate_profile_rejects_joint_count_mismatch() {
        // hardware.joints = 3，anchor_bias_len = 2 → JointCountMismatch。
        let p = ProfileSection::default_b2();
        let r = registry_with_mock_dwell();
        let hw = hardware_n(3);
        let err = validate_profile_section(&p, &r, &hw, 2).expect_err("expected Err");
        match err {
            ProfileError::JointCountMismatch {
                hardware_joints,
                anchor_bias_len,
            } => {
                assert_eq!(hardware_joints, 3);
                assert_eq!(anchor_bias_len, 2);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn validate_profile_with_two_enabled_backends_all_in_registry_passes() {
        let p = ProfileSection {
            backends: vec!["mock_dwell".into(), "other".into()],
            command_mode: "joint_position".into(),
        };
        let r = registry_with_two();
        let hw = hardware_n(1);
        validate_profile_section(&p, &r, &hw, 1).expect("validate ok");
    }

    // ----- reviewer 阻断 2：[s1.profile.backends] 空列表必须 fail-fast -----

    #[test]
    fn parse_profile_empty_section_yields_empty_backends_then_validate_rejects() {
        // [s1.profile] 显式存在但 backends 字段缺失 → parse 走
        // `serde(default)` 给 backends = []；validate 必须 NoEnabledBackends
        // 拦下，不能漏到 runtime submit 阶段才报 NoBackendForKind。
        let toml = "[s1.profile]\n";
        let p = parse_profile_section_from_toml(toml).expect("L1 ok");
        assert!(p.backends.is_empty(), "missing field defaults to []");
        assert_eq!(p.command_mode, "joint_position");

        let r = registry_with_mock_dwell();
        let hw = hardware_n(1);
        let err = validate_profile_section(&p, &r, &hw, 1).expect_err("expected Err");
        assert!(
            matches!(err, ProfileError::NoEnabledBackends),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_profile_rejects_explicit_empty_backends_with_no_enabled_backends() {
        // backends = [] 显式空列表（区别于"字段缺失"）→ 同样 NoEnabledBackends。
        let p = ProfileSection {
            backends: vec![],
            command_mode: "joint_position".into(),
        };
        let r = registry_with_mock_dwell();
        let hw = hardware_n(1);
        let err = validate_profile_section(&p, &r, &hw, 1).expect_err("expected Err");
        assert!(
            matches!(err, ProfileError::NoEnabledBackends),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_profile_default_b2_still_passes_after_no_enabled_backends_check() {
        // 锁住"缺失 [s1.profile] → default_b2"路径不被本检查误伤
        // —— default_b2 显式含 ["mock_dwell"]，应继续通过。
        let p = ProfileSection::default_b2();
        assert_eq!(p.backends, vec!["mock_dwell".to_string()]);
        let r = registry_with_mock_dwell();
        let hw = hardware_n(1);
        validate_profile_section(&p, &r, &hw, 1).expect("default_b2 must pass");
    }

    #[test]
    fn no_enabled_backends_display_renders_human_readable_message() {
        // 锁住 Display 文案 —— B4 RPC 错误码映射 / audit log 都靠这条字
        // 符串识别。
        let err = ProfileError::NoEnabledBackends;
        let s = err.to_string();
        assert!(s.contains("[s1.profile.backends]"));
        assert!(s.contains("empty"));
        assert!(s.contains("at least one backend"));
    }

    // ----- v0.4 候选 2 round 2 (3/4) Q-IK10 双层 fail-fast -----

    #[test]
    fn validate_profile_rejects_ik_when_backend_feature_off() {
        // **Q-IK10 (a-i) 测试形态**：用 `from_explicit(&[])` 模拟 "整个
        // binary 没有 link 任何 backend"（**不**走 `--no-default-features`，
        // 因 default features 已 include backend-ik）。profile 写
        // `["ik"]` → registry 无 "ik" → validate_profile_section 报
        // `UnknownBackend { name: "ik" }`，跟 feature off 路径行为一致。
        let p = ProfileSection {
            backends: vec!["ik".into()],
            command_mode: "joint_position".into(),
        };
        let r = BackendRegistry::from_explicit(&[]);
        let hw = hardware_n(5);
        let err = validate_profile_section(&p, &r, &hw, /* anchor_bias_len */ 5)
            .expect_err("expected Err");
        match err {
            ProfileError::UnknownBackend { name } => {
                assert_eq!(name, "ik", "feature off 路径应报 UnknownBackend(\"ik\")");
            }
            other => panic!("expected UnknownBackend(\"ik\"), got {other:?}"),
        }
    }

    #[test]
    fn validate_solver_for_profile_rejects_missing_solver_when_ik_enabled() {
        // **Q-IK10b (i) 测试形态**：profile 启用 `"ik"` 但 solver_present
        // = false（典型 wiring bug：feature on + robot-s1 漏构造 solver）
        // → `MissingSolver { backend: "ik" }`。
        let p = ProfileSection {
            backends: vec!["ik".into()],
            command_mode: "joint_position".into(),
        };
        let err =
            validate_solver_for_profile(&p, /* solver_present */ false).expect_err("expected Err");
        match err {
            ProfileError::MissingSolver { backend } => {
                assert_eq!(backend, "ik");
            }
            other => panic!("expected MissingSolver(\"ik\"), got {other:?}"),
        }
    }

    #[test]
    fn validate_solver_for_profile_accepts_ik_with_solver_present() {
        // profile 启用 `"ik"` + solver 注入 OK → Ok(()) 通过。
        let p = ProfileSection {
            backends: vec!["ik".into()],
            command_mode: "joint_position".into(),
        };
        validate_solver_for_profile(&p, /* solver_present */ true).expect("validate ok");
    }

    #[test]
    fn validate_solver_for_profile_accepts_no_ik_in_profile() {
        // profile 未启用 `"ik"`（如默认 default_b2 = ["mock_dwell"]）→
        // solver 可注可不注，恒 Ok(())（不允许"多余资源"报错）。
        let p = ProfileSection::default_b2();
        validate_solver_for_profile(&p, /* solver_present */ false).expect("ok without solver");
        validate_solver_for_profile(&p, /* solver_present */ true)
            .expect("ok with redundant solver");
    }

    #[test]
    fn missing_solver_display_renders_human_readable_message() {
        let err = ProfileError::MissingSolver {
            backend: "ik".into(),
        };
        let s = err.to_string();
        assert!(s.contains("\"ik\""));
        assert!(s.contains("solver injection"));
        assert!(s.contains("Q-IK10b"));
    }

    // ====================================================================
    // Coordinator state machine + 4 API + tick_action（B2 (2/3) reviewer
    // 验收契约：4 条 submit 拒绝 + 5 条 clear_error 转移 + cancel 收敛
    // 三路 + status ring）
    // ====================================================================

    use crate::action::{
        ActionEvent, ActionFailure, ActionId, ActionOutcome, ActionSource, ActionTickOutput,
        ClearError,
    };
    use robot_core::time::SystemClock;
    use std::sync::Mutex as StdMutex;

    // ----- 测试用可脚本化 backend：每次 tick 返回 script 中下一项 -----

    /// Tick 脚本：N 个预设 [`ActionTickOutput`]/`ActionFailure` 顺序消费。
    enum TickScriptItem {
        Continue { progress: f32 },
        Completed(ActionOutcome),
        Failed(ActionFailure),
    }

    struct ScriptedBackend {
        name: &'static str,
        kinds: &'static [ActionKind],
        schemas: &'static [u32],
    }

    impl ActionBackend for ScriptedBackend {
        fn name(&self) -> &str {
            self.name
        }
        fn supported_kinds(&self) -> &[ActionKind] {
            self.kinds
        }
        fn supported_schemas(&self) -> &[u32] {
            self.schemas
        }
        fn start(
            &self,
            _req: &ActionRequest,
            _ctx: &ActionContext<'_>,
        ) -> Result<Box<dyn ActionInstance>, ActionStartError> {
            Ok(Box::new(ScriptedInstance {
                script: StdMutex::new(Vec::new()),
                cancelled: false,
                hold_target: None,
            }))
        }
    }

    impl ScriptedBackend {
        /// 用预置 script 起一个 instance（绕过 backend.start，避免每条
        /// 测试重新塞 script 进 backend 状态）。
        fn make_instance(script: Vec<TickScriptItem>) -> Box<dyn ActionInstance> {
            Box::new(ScriptedInstance {
                script: StdMutex::new(script),
                cancelled: false,
                hold_target: None,
            })
        }

        /// 同上，但 `successful_hold_target()` 返 `Some(target)`（模拟真
        /// Scripted 末帧已发布；v0.5 候选 E 段(5) D3 第 3 条矩阵单机维）。
        fn make_instance_with_hold(
            script: Vec<TickScriptItem>,
            target: Vec<f64>,
        ) -> Box<dyn ActionInstance> {
            Box::new(ScriptedInstance {
                script: StdMutex::new(script),
                cancelled: false,
                hold_target: Some(target),
            })
        }
    }

    struct ScriptedInstance {
        script: StdMutex<Vec<TickScriptItem>>,
        cancelled: bool,
        /// `successful_hold_target()` 返值（默认 None = 既有 backend 行为）。
        hold_target: Option<Vec<f64>>,
    }

    impl ActionInstance for ScriptedInstance {
        fn tick(
            &mut self,
            _ctx: &mut ActionContext<'_>,
        ) -> Result<ActionTickOutput, ActionFailure> {
            // cancel 优先：直接返 Completed(PartialSuccess { warning })，
            // 跟 MockDwell 同款 cancel 收敛。
            if self.cancelled {
                return Ok(ActionTickOutput::Completed(ActionOutcome::PartialSuccess {
                    warning: "scripted cancelled".into(),
                }));
            }
            let mut sc = self.script.lock().expect("lock");
            if sc.is_empty() {
                // script 用完了：停在最后一帧 Continue 0.99 让 cancel
                // 测试有"还在跑"的窗口。
                return Ok(ActionTickOutput::Continue {
                    progress: 0.99,
                    command: None,
                });
            }
            match sc.remove(0) {
                TickScriptItem::Continue { progress } => Ok(ActionTickOutput::Continue {
                    progress,
                    command: None,
                }),
                TickScriptItem::Completed(o) => Ok(ActionTickOutput::Completed(o)),
                TickScriptItem::Failed(f) => Err(f),
            }
        }
        fn cancel(&mut self, _ctx: &mut ActionContext<'_>) -> Result<(), ActionCancelError> {
            self.cancelled = true;
            Ok(())
        }
        fn successful_hold_target(&self) -> Option<Vec<f64>> {
            self.hold_target.clone()
        }
    }

    static SCRIPTED_BACKEND: ScriptedBackend = ScriptedBackend {
        name: "scripted",
        kinds: &[ActionKind::DwellMs],
        schemas: &[NO_PAYLOAD_SCHEMA_ID],
    };

    /// 跟 [`ScriptedInstance`] 同款但 `cancel()` **不**置 cancelled flag —— 给
    /// `cancel_then_tick_failed_emits_failed_not_cancelled` 测"backend 报错
    /// 覆盖 cancel 意图"路径用：cancel 是协作取消，但 backend 在下个 tick
    /// 自身故障 → coordinator 应 emit Failed（而不是 Cancelled）。
    struct ScriptedNoCancelInstance {
        script: StdMutex<Vec<TickScriptItem>>,
        /// `successful_hold_target()` 返值（cancel 收敛路径应被 coordinator 忽略）。
        hold_target: Option<Vec<f64>>,
    }

    impl ActionInstance for ScriptedNoCancelInstance {
        fn tick(
            &mut self,
            _ctx: &mut ActionContext<'_>,
        ) -> Result<ActionTickOutput, ActionFailure> {
            let mut sc = self.script.lock().expect("lock");
            match sc.remove(0) {
                TickScriptItem::Continue { progress } => Ok(ActionTickOutput::Continue {
                    progress,
                    command: None,
                }),
                TickScriptItem::Completed(o) => Ok(ActionTickOutput::Completed(o)),
                TickScriptItem::Failed(f) => Err(f),
            }
        }
        fn cancel(&mut self, _ctx: &mut ActionContext<'_>) -> Result<(), ActionCancelError> {
            Ok(())
        }
        fn successful_hold_target(&self) -> Option<Vec<f64>> {
            self.hold_target.clone()
        }
    }

    // ----- helpers -----

    /// 构造一个 Coordinator + 强制移到指定状态。
    fn make_coordinator() -> Coordinator {
        let cfg = CoordinatorConfig::default();
        let hardware = HardwareSection {
            id: "test_arm".into(),
            joints: vec!["j0".into()],
            motor_ids: vec![1],
            feetech_bus2_mixed_raw: false,
            calibration_range_tolerances_raw: None,
        };
        let registry =
            BackendRegistry::from_explicit(&[&SCRIPTED_BACKEND as &'static dyn ActionBackend]);
        Coordinator::new(
            cfg,
            hardware,
            /* layout_id */ 0xCAFE,
            /* anchor_bias */ vec![0.0],
            registry,
            /* enabled_backends */ vec!["scripted".into()],
            /* solver */ None,
        )
    }

    fn coordinator_in_idle() -> Coordinator {
        let c = make_coordinator();
        c.mark_idle().expect("Booting → Idle ok");
        c
    }

    fn sample_request() -> ActionRequest {
        ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::DwellMs,
            deadline_ns: u64::MAX, // 远期 deadline，不会被 validate 拒
            priority: 10,
            payload_schema_id: NO_PAYLOAD_SCHEMA_ID,
            payload: Vec::new(),
            idempotency_key: "test-key".into(),
            source: ActionSource::Internal,
        }
    }

    /// 直接把 coordinator 推到 EXECUTING 状态 + 用注入的 instance（绕
    /// 开 backend.start，让单测能给任意 script）。返 ticket 给调用方查
    /// 事件流。
    fn force_executing(c: &Coordinator, instance: Box<dyn ActionInstance>) -> ActionTicket {
        let action_id = ActionId::new_v7();
        let (tx, rx) = std::sync::mpsc::channel::<ActionEvent>();
        let _ = tx.send(ActionEvent::Started { at_ns: 0 });
        let mut inner = c.lock_inner();
        inner.fsm = CoordinatorState::Executing {
            action_id,
            started_at_ns: 0,
        };
        inner.current = Some(CurrentAction {
            action_id,
            instance,
            events_tx: tx,
            pending_cancel: false,
            last_progress: 0.0,
            needs_feedback: false,
        });
        ActionTicket {
            action_id,
            events: rx,
        }
    }

    fn force_degraded(c: &Coordinator, reason: &str) {
        let mut inner = c.lock_inner();
        inner.fsm = CoordinatorState::Degraded {
            reason: reason.into(),
        };
    }

    // ----- mark_idle -----

    #[test]
    fn mark_idle_from_booting_succeeds() {
        let c = make_coordinator();
        assert!(matches!(c.current_state(), CoordinatorState::Booting));
        c.mark_idle().expect("Booting → Idle ok");
        assert!(matches!(c.current_state(), CoordinatorState::Idle));
    }

    #[test]
    fn mark_idle_from_idle_rejects() {
        let c = coordinator_in_idle();
        let err = c.mark_idle().expect_err("expected Err");
        assert!(matches!(err, ClearError::NotDegraded(s) if s == "Idle"));
    }

    // ----- submit 4 条拒绝路径（reviewer 验收契约必测）-----

    #[test]
    fn submit_rejects_when_booting_returns_not_ready() {
        let c = make_coordinator();
        let clock = SystemClock;
        let err = c
            .submit(sample_request(), &clock)
            .expect_err("expected Err");
        assert!(matches!(err, SubmitError::NotReady), "got {err:?}");
    }

    #[test]
    fn submit_rejects_when_executing_returns_busy_with_action_id() {
        let c = coordinator_in_idle();
        let inst = ScriptedBackend::make_instance(vec![TickScriptItem::Continue { progress: 0.1 }]);
        let ticket = force_executing(&c, inst);

        let clock = SystemClock;
        let err = c
            .submit(sample_request(), &clock)
            .expect_err("expected Err");
        match err {
            SubmitError::BusyExecuting(id) => {
                assert_eq!(id, ticket.action_id, "BusyExecuting 应携当前 action_id");
            }
            other => panic!("expected BusyExecuting, got {other:?}"),
        }
    }

    #[test]
    fn submit_rejects_when_degraded() {
        let c = coordinator_in_idle();
        force_degraded(&c, "test reason");
        let clock = SystemClock;
        let err = c
            .submit(sample_request(), &clock)
            .expect_err("expected Err");
        assert!(
            matches!(err, SubmitError::CoordinatorDegraded),
            "got {err:?}"
        );
    }

    #[test]
    fn submit_rejects_when_shutting_down() {
        let c = coordinator_in_idle();
        c.mark_shutting_down();
        let clock = SystemClock;
        let err = c
            .submit(sample_request(), &clock)
            .expect_err("expected Err");
        assert!(matches!(err, SubmitError::ShuttingDown), "got {err:?}");
    }

    // ----- 状态优先（reviewer 阻断 1）：非 Idle 状态遇 invalid request
    //        仍必须先返状态错误，**不**让 InvalidRequest /
    //        NoBackendForKind 等抢占 -----

    /// 构造一个**明显非法**的请求（多条 validate 规则同时违反 + 哨兵
    /// schema_id 配非空 payload），让 Idle 路径下 validate 必拒；用于测
    /// 非 Idle 状态优先 —— state 错误要先于 InvalidRequest 返回。
    fn obviously_invalid_request() -> ActionRequest {
        ActionRequest {
            action_id: ActionId::new_v7(),
            kind: ActionKind::DwellMs,
            deadline_ns: 0, // < now_ns，违反 deadline > now
            priority: 10,
            payload_schema_id: NO_PAYLOAD_SCHEMA_ID,
            payload: vec![1, 2, 3], // 哨兵 schema_id 配非空 payload，违反守门
            idempotency_key: String::new(), // 空 idempotency_key，违反守门
            source: ActionSource::Internal,
        }
    }

    #[test]
    fn submit_state_priority_booting_returns_not_ready_for_invalid_request() {
        // Booting 下传明显坏请求：仍必须先返 NotReady，不是 InvalidRequest。
        let c = make_coordinator();
        let clock = SystemClock;
        let err = c
            .submit(obviously_invalid_request(), &clock)
            .expect_err("expected Err");
        assert!(matches!(err, SubmitError::NotReady), "got {err:?}");
    }

    #[test]
    fn submit_state_priority_degraded_returns_degraded_for_invalid_request() {
        let c = coordinator_in_idle();
        force_degraded(&c, "test");
        let clock = SystemClock;
        let err = c
            .submit(obviously_invalid_request(), &clock)
            .expect_err("expected Err");
        assert!(
            matches!(err, SubmitError::CoordinatorDegraded),
            "got {err:?}"
        );
    }

    #[test]
    fn submit_state_priority_shutting_down_returns_shutting_down_for_invalid_request() {
        let c = coordinator_in_idle();
        c.mark_shutting_down();
        let clock = SystemClock;
        let err = c
            .submit(obviously_invalid_request(), &clock)
            .expect_err("expected Err");
        assert!(matches!(err, SubmitError::ShuttingDown), "got {err:?}");
    }

    // ----- clear_error 5 条转移（reviewer 验收契约必测）-----

    #[test]
    fn clear_error_succeeds_only_from_degraded_to_idle() {
        let c = coordinator_in_idle();
        force_degraded(&c, "test");
        assert!(matches!(
            c.current_state(),
            CoordinatorState::Degraded { .. }
        ));
        c.clear_error().expect("Degraded → Idle ok");
        assert!(matches!(c.current_state(), CoordinatorState::Idle));
    }

    #[test]
    fn clear_error_rejects_from_booting_returns_not_degraded() {
        let c = make_coordinator();
        let err = c.clear_error().expect_err("expected Err");
        assert!(matches!(err, ClearError::NotDegraded(s) if s == "Booting"));
    }

    #[test]
    fn clear_error_rejects_from_idle_returns_not_degraded() {
        let c = coordinator_in_idle();
        let err = c.clear_error().expect_err("expected Err");
        assert!(matches!(err, ClearError::NotDegraded(s) if s == "Idle"));
    }

    #[test]
    fn clear_error_rejects_from_executing_returns_not_degraded() {
        let c = coordinator_in_idle();
        let inst = ScriptedBackend::make_instance(vec![TickScriptItem::Continue { progress: 0.1 }]);
        let _ticket = force_executing(&c, inst);
        let err = c.clear_error().expect_err("expected Err");
        assert!(matches!(err, ClearError::NotDegraded(s) if s == "Executing"));
    }

    #[test]
    fn clear_error_rejects_from_shutting_down_returns_not_degraded() {
        let c = coordinator_in_idle();
        c.mark_shutting_down();
        let err = c.clear_error().expect_err("expected Err");
        assert!(matches!(err, ClearError::NotDegraded(s) if s == "ShuttingDown"));
    }

    // ----- cancel 收敛三路（reviewer 拍板：cancel 不是 terminal）-----

    #[test]
    fn cancel_unknown_id_returns_not_executing() {
        let c = coordinator_in_idle();
        let clock = SystemClock;
        let err = c
            .cancel(ActionId::new_v7(), &clock)
            .expect_err("expected Err");
        assert!(
            matches!(err, ActionCancelError::NotExecuting),
            "got {err:?}"
        );
    }

    #[test]
    fn cancel_then_tick_completed_emits_cancelled_not_completed() {
        // 脚本：第一帧 Continue，cancel 后 ScriptedInstance 自查 cancelled
        // 标志返 Completed(PartialSuccess) → coordinator 应 emit Cancelled，
        // 不双发 Completed + Cancelled。
        let c = coordinator_in_idle();
        let inst = ScriptedBackend::make_instance(vec![TickScriptItem::Continue { progress: 0.5 }]);
        let ticket = force_executing(&c, inst);

        let clock = SystemClock;
        c.cancel(ticket.action_id, &clock).expect("cancel ok");

        // 第一次 tick：cancelled 标志置位，instance 直接返 Completed(PartialSuccess)。
        let _ = c.tick_action(&clock, 0);

        // 收事件流，跳过 Started + Progress（如有），找 terminal。
        let mut saw_cancelled = false;
        let mut saw_completed = false;
        for ev in ticket.events.try_iter() {
            match ev {
                ActionEvent::Cancelled { .. } => saw_cancelled = true,
                ActionEvent::Completed { .. } => saw_completed = true,
                _ => {}
            }
        }
        assert!(saw_cancelled, "应 emit Cancelled");
        assert!(!saw_completed, "**绝不**双发 Completed");

        // 状态 → Idle；finished ring 含 (id, Cancelled)。
        assert!(matches!(c.current_state(), CoordinatorState::Idle));
        assert!(matches!(
            c.status(ticket.action_id),
            Some(ActionStatus::Cancelled)
        ));
    }

    #[test]
    fn cancel_then_tick_failed_emits_failed_not_cancelled() {
        // 脚本：cancel 后 backend 报 Err（覆盖 cancel 意图）→ coordinator
        // 应 emit Failed，不发 Cancelled（reviewer 拍板：backend 失败信
        // 息更重要）。
        let c = coordinator_in_idle();
        // 用 `ScriptedNoCancelInstance`（cancel 不置 flag）让下次 tick
        // 走 script Failed 路径 —— 验 "cancel 后 backend 报错 → emit
        // Failed（不发 Cancelled）"。
        let inst = Box::new(ScriptedNoCancelInstance {
            script: StdMutex::new(vec![TickScriptItem::Failed(ActionFailure::AdapterError(
                "post-cancel adapter blew".into(),
            ))]),
            hold_target: None,
        }) as Box<dyn ActionInstance>;

        let ticket = force_executing(&c, inst);

        let clock = SystemClock;
        c.cancel(ticket.action_id, &clock).expect("cancel ok");
        let _ = c.tick_action(&clock, 0);

        let mut saw_failed = false;
        let mut saw_cancelled = false;
        for ev in ticket.events.try_iter() {
            match ev {
                ActionEvent::Failed { .. } => saw_failed = true,
                ActionEvent::Cancelled { .. } => saw_cancelled = true,
                _ => {}
            }
        }
        assert!(saw_failed, "应 emit Failed（backend 报错覆盖 cancel）");
        assert!(!saw_cancelled, "**不**双发 Cancelled");

        // AdapterError → Degraded。
        assert!(matches!(
            c.current_state(),
            CoordinatorState::Degraded { .. }
        ));
    }

    // ----- status() 查询 + finished ring 容量 -----

    #[test]
    fn status_returns_executing_for_active_action() {
        let c = coordinator_in_idle();
        let inst = ScriptedBackend::make_instance(vec![TickScriptItem::Continue { progress: 0.5 }]);
        let ticket = force_executing(&c, inst);

        // 推一次 tick 让 last_progress 从 0.0 更新到 0.5。
        let clock = SystemClock;
        let _ = c.tick_action(&clock, 0);

        let st = c.status(ticket.action_id).expect("Some Executing");
        match st {
            ActionStatus::Executing { progress } => {
                assert!((progress - 0.5).abs() < 1e-6);
            }
            other => panic!("expected Executing, got {other:?}"),
        }
    }

    #[test]
    fn status_returns_finished_for_completed_action() {
        let c = coordinator_in_idle();
        let inst =
            ScriptedBackend::make_instance(vec![TickScriptItem::Completed(ActionOutcome::Success)]);
        let ticket = force_executing(&c, inst);

        let clock = SystemClock;
        let _ = c.tick_action(&clock, 0);

        // EXECUTING 完，状态应 Idle，status 应从 finished ring 拿到。
        assert!(matches!(c.current_state(), CoordinatorState::Idle));
        let st = c.status(ticket.action_id).expect("Some Completed");
        assert!(matches!(
            st,
            ActionStatus::Completed(ActionOutcome::Success)
        ));
    }

    #[test]
    fn status_returns_none_for_unknown_id() {
        let c = coordinator_in_idle();
        let unknown = ActionId::new_v7();
        assert!(c.status(unknown).is_none());
    }

    #[test]
    fn status_finished_ring_evicts_oldest_after_capacity() {
        // 写 cap+1 项 (id_i, Cancelled)，验最旧那条被淘汰。
        let c = coordinator_in_idle();
        let mut ids: Vec<ActionId> = Vec::with_capacity(FINISHED_RING_CAPACITY + 1);
        {
            let mut inner = c.lock_inner();
            for i in 0..=FINISHED_RING_CAPACITY {
                let id = ActionId::new_v7();
                ids.push(id);
                inner.push_finished(
                    id,
                    ActionStatus::Cancelled, // 任意终态都行
                );
                let _ = i;
            }
        }
        // 最早写入的（ids[0]）应被淘汰。
        assert!(c.status(ids[0]).is_none(), "oldest should be evicted");
        // 后续 cap 项都该还在 ring 里。
        for id in &ids[1..] {
            assert!(
                c.status(*id).is_some(),
                "id {id} should still be in finished ring"
            );
        }
    }

    // ----- tick_action transitions -----

    #[test]
    fn tick_action_safety_abort_transitions_to_degraded() {
        let c = coordinator_in_idle();
        let inst = ScriptedBackend::make_instance(vec![TickScriptItem::Failed(
            ActionFailure::SafetyAbort("limits exceeded".into()),
        )]);
        let _ticket = force_executing(&c, inst);

        let clock = SystemClock;
        let _ = c.tick_action(&clock, 0);
        match c.current_state() {
            CoordinatorState::Degraded { reason } => {
                assert_eq!(reason, "limits exceeded");
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    #[test]
    fn tick_action_precondition_failed_transitions_to_idle() {
        // outline §6.5：Precondition / DeadlineExceeded → Idle，**不**进
        // Degraded（hold loop 自动恢复，无需 clear_error）。
        let c = coordinator_in_idle();
        let inst = ScriptedBackend::make_instance(vec![TickScriptItem::Failed(
            ActionFailure::PreconditionFailed("limit too tight".into()),
        )]);
        let _ticket = force_executing(&c, inst);

        let clock = SystemClock;
        let _ = c.tick_action(&clock, 0);
        assert!(matches!(c.current_state(), CoordinatorState::Idle));
    }

    #[test]
    fn tick_action_returns_none_when_not_executing() {
        // Booting / Idle / Degraded / ShuttingDown 都不该 tick backend。
        let cs = vec![
            (make_coordinator(), "Booting"),
            (coordinator_in_idle(), "Idle"),
            (
                {
                    let c = coordinator_in_idle();
                    force_degraded(&c, "x");
                    c
                },
                "Degraded",
            ),
            (
                {
                    let c = coordinator_in_idle();
                    c.mark_shutting_down();
                    c
                },
                "ShuttingDown",
            ),
        ];
        let clock = SystemClock;
        for (c, label) in cs {
            assert!(
                c.tick_action(&clock, 0).is_none(),
                "{label}: tick_action should return None"
            );
        }
    }

    // ----- mark_shutting_down 副作用 -----

    #[test]
    fn mark_shutting_down_drops_current_action() {
        let c = coordinator_in_idle();
        let inst = ScriptedBackend::make_instance(vec![TickScriptItem::Continue { progress: 0.1 }]);
        let ticket = force_executing(&c, inst);

        c.mark_shutting_down();
        assert!(matches!(c.current_state(), CoordinatorState::ShuttingDown));

        // ticket.events 端 sender drop → recv 应 Disconnected。
        match ticket.events.try_recv() {
            // Started 还在 buffer 里就 ok（drain 之后 disconnected）。
            Ok(ActionEvent::Started { .. }) | Err(_) => {}
            other => panic!("unexpected first event: {other:?}"),
        }
    }

    #[test]
    fn mark_shutting_down_is_idempotent() {
        let c = coordinator_in_idle();
        c.mark_shutting_down();
        c.mark_shutting_down(); // 不 panic / 不副作用。
        assert!(matches!(c.current_state(), CoordinatorState::ShuttingDown));
    }

    // ----- ticket drop send 失败不影响状态 -----

    #[test]
    fn ticket_drop_does_not_break_state_transitions() {
        let c = coordinator_in_idle();
        let inst =
            ScriptedBackend::make_instance(vec![TickScriptItem::Completed(ActionOutcome::Success)]);
        let ticket = force_executing(&c, inst);
        // 模拟客户端 drop ticket（reviewer 拍板：events_tx.send 失败时
        // coordinator 不当 action failure，状态机正常推进到终态）。
        drop(ticket.events);

        let clock = SystemClock;
        let _ = c.tick_action(&clock, 0);
        assert!(
            matches!(c.current_state(), CoordinatorState::Idle),
            "ticket events drop 不影响状态机收敛"
        );
    }

    // ====================================================================
    // v0.5 候选 E 段(5) D3 第 3 条：idle_hold_target 穷尽式单一规则（矩阵
    // 单机维；RB5：各分支独立 case，先 Success 再分别 ...；跨进程
    // AC-E6/E7/E18 落段(7)）
    // ====================================================================

    /// 前置：一次"Scripted 式" Success（instance 报 Some(末点)）→
    /// `idle_hold_target = 末点`（≠ anchor_bias）。每个分支 case 从本前置出发。
    fn success_with_hold_target(c: &Coordinator) -> Vec<f64> {
        let target = vec![0.75];
        let inst = ScriptedBackend::make_instance_with_hold(
            vec![TickScriptItem::Completed(ActionOutcome::Success)],
            target.clone(),
        );
        let _ticket = force_executing(c, inst);
        let _ = c.tick_action(&SystemClock, 0);
        assert!(matches!(c.current_state(), CoordinatorState::Idle));
        assert_eq!(
            c.idle_hold_target(),
            target,
            "前置：Success + Some(末点) 后 idle_hold_target = 末点"
        );
        assert_ne!(target, c.anchor_bias().to_vec());
        target
    }

    #[test]
    fn idle_hold_target_initial_equals_anchor_bias() {
        let c = coordinator_in_idle();
        assert_eq!(c.idle_hold_target(), c.anchor_bias().to_vec());
    }

    #[test]
    fn success_with_hold_target_updates_idle_hold_target() {
        let c = coordinator_in_idle();
        let t = success_with_hold_target(&c);
        assert_eq!(c.idle_hold_target(), t);
    }

    #[test]
    fn success_without_hold_target_resets_idle_hold_target() {
        // 先 Success(Some) 再 Success(None)（= HoldPose / DwellMs / IK 缺省
        // successful_hold_target）→ 显式 reset = anchor_bias。
        let c = coordinator_in_idle();
        success_with_hold_target(&c);
        let inst =
            ScriptedBackend::make_instance(vec![TickScriptItem::Completed(ActionOutcome::Success)]);
        let _t = force_executing(&c, inst);
        let _ = c.tick_action(&SystemClock, 0);
        assert!(matches!(c.current_state(), CoordinatorState::Idle));
        assert_eq!(c.idle_hold_target(), c.anchor_bias().to_vec());
    }

    #[test]
    fn partial_success_resets_even_if_instance_reports_hold_target() {
        let c = coordinator_in_idle();
        success_with_hold_target(&c);
        let inst = ScriptedBackend::make_instance_with_hold(
            vec![TickScriptItem::Completed(ActionOutcome::PartialSuccess {
                warning: "clamped".into(),
            })],
            vec![0.5],
        );
        let _t = force_executing(&c, inst);
        let _ = c.tick_action(&SystemClock, 0);
        assert!(matches!(c.current_state(), CoordinatorState::Idle));
        assert_eq!(c.idle_hold_target(), c.anchor_bias().to_vec());
    }

    #[test]
    fn cancel_convergence_resets_even_if_instance_reports_hold_target() {
        // cancel 后 instance 收敛 Completed(Success)（模拟末帧之后 cancel），
        // pending_cancel → Cancelled → reset；successful_hold_target 不问。
        let c = coordinator_in_idle();
        success_with_hold_target(&c);
        let inst = Box::new(ScriptedNoCancelInstance {
            script: StdMutex::new(vec![TickScriptItem::Completed(ActionOutcome::Success)]),
            hold_target: Some(vec![0.5]),
        }) as Box<dyn ActionInstance>;
        let ticket = force_executing(&c, inst);
        c.cancel(ticket.action_id, &SystemClock).expect("cancel ok");
        let _ = c.tick_action(&SystemClock, 0);
        assert!(matches!(c.current_state(), CoordinatorState::Idle));
        assert!(
            ticket
                .events
                .try_iter()
                .any(|ev| matches!(ev, ActionEvent::Cancelled { .. })),
            "应 emit Cancelled"
        );
        assert_eq!(c.idle_hold_target(), c.anchor_bias().to_vec());
    }

    #[test]
    fn failure_precondition_resets_idle_hold_target() {
        let c = coordinator_in_idle();
        success_with_hold_target(&c);
        let inst = ScriptedBackend::make_instance_with_hold(
            vec![TickScriptItem::Failed(ActionFailure::PreconditionFailed(
                "limit".into(),
            ))],
            vec![0.5],
        );
        let _t = force_executing(&c, inst);
        let _ = c.tick_action(&SystemClock, 0);
        assert!(matches!(c.current_state(), CoordinatorState::Idle));
        assert_eq!(c.idle_hold_target(), c.anchor_bias().to_vec());
    }

    #[test]
    fn failure_safety_abort_resets_idle_hold_target_and_survives_clear_error() {
        // failure → Degraded：Degraded hold 本就读 anchor_bias（不解锁），
        // 且 idle_hold_target 已 reset；clear_error 回 Idle 后仍 = anchor。
        let c = coordinator_in_idle();
        success_with_hold_target(&c);
        let inst = ScriptedBackend::make_instance_with_hold(
            vec![TickScriptItem::Failed(ActionFailure::SafetyAbort(
                "abort".into(),
            ))],
            vec![0.5],
        );
        let _t = force_executing(&c, inst);
        let _ = c.tick_action(&SystemClock, 0);
        assert!(matches!(
            c.current_state(),
            CoordinatorState::Degraded { .. }
        ));
        assert_eq!(c.idle_hold_target(), c.anchor_bias().to_vec());
        c.clear_error().expect("Degraded → Idle");
        assert_eq!(c.idle_hold_target(), c.anchor_bias().to_vec());
    }

    #[test]
    fn hold_target_length_mismatch_resets_defensively() {
        // coordinator 1 joint；instance 报 2 维 target → 契约违反 → reset。
        let c = coordinator_in_idle();
        success_with_hold_target(&c);
        let inst = ScriptedBackend::make_instance_with_hold(
            vec![TickScriptItem::Completed(ActionOutcome::Success)],
            vec![1.0, 2.0],
        );
        let _t = force_executing(&c, inst);
        let _ = c.tick_action(&SystemClock, 0);
        assert_eq!(c.idle_hold_target(), c.anchor_bias().to_vec());
    }

    #[test]
    fn executing_without_current_resets_idle_hold_target() {
        // 防御分支：fsm Executing 但 current None → Idle + reset。
        let c = coordinator_in_idle();
        success_with_hold_target(&c);
        {
            let mut inner = c.lock_inner();
            inner.fsm = CoordinatorState::Executing {
                action_id: ActionId::new_v7(),
                started_at_ns: 0,
            };
            inner.current = None;
        }
        assert!(c.tick_action(&SystemClock, 0).is_none());
        assert!(matches!(c.current_state(), CoordinatorState::Idle));
        assert_eq!(c.idle_hold_target(), c.anchor_bias().to_vec());
    }

    #[test]
    fn state_cache_absent_by_default_and_take_returns_none() {
        let c = coordinator_in_idle();
        assert!(!c.has_state_cache());
        assert!(c.take_state_cache().is_none());
        assert!(c.probe_state_cache(MonoRawNs(0)).is_none());
    }

    // ====================================================================
    // submit Scripted 路径（真 ScriptedBackend + mock transport state cache）
    // ====================================================================

    #[cfg(feature = "backend-scripted")]
    mod scripted_submit {
        use super::*;

        use crate::action::ActionStartError;
        use crate::action_backend::scripted::ScriptedBackend as RealScriptedBackend;
        use crate::action_backend::scripted::test_support::build_scripted_payload_bytes;
        use crate::state_cache::{READY_TIMEOUT, StateCacheConfig, spawn_state_cache_thread_with};
        use robot_core::time::MockClock;
        use robot_msg::build_helpers::build_joint_state;
        use robot_msg::msg::JointCommand as FbJointCommand;
        use robot_msg::schema_id::SCHEMA_SCRIPTED_PAYLOAD;
        use robot_transport::mock::MockChannel;
        use robot_transport::traits::Publisher;
        use std::sync::Arc;

        #[cfg(feature = "backend-ik")]
        mod bus2_ik {
            include!("bus2_ik_tests.rs");
        }

        static REAL_SCRIPTED: RealScriptedBackend = RealScriptedBackend;
        const LAYOUT: u64 = 0x5EED_0000_0000_0001;
        const JOINTS: usize = 2;
        const ANCHOR: [f64; JOINTS] = [9.0, 9.0];

        fn scripted_coordinator() -> Coordinator {
            let c = Coordinator::new(
                CoordinatorConfig::default(),
                HardwareSection {
                    id: "arm".into(),
                    joints: vec!["j0".into(), "j1".into()],
                    motor_ids: vec![1, 2],
                    feetech_bus2_mixed_raw: false,
                    calibration_range_tolerances_raw: None,
                },
                LAYOUT,
                ANCHOR.to_vec(),
                BackendRegistry::from_explicit(&[&REAL_SCRIPTED as &'static dyn ActionBackend]),
                vec!["scripted".into()],
                None,
            );
            c.mark_idle().expect("Booting → Idle");
            c
        }

        fn scripted_request(waypoints: &[Vec<f64>]) -> ActionRequest {
            let wps: Vec<(Vec<f64>, u32, u32)> =
                waypoints.iter().map(|p| (p.clone(), 0, 100)).collect();
            ActionRequest {
                action_id: ActionId::new_v7(),
                kind: ActionKind::Scripted,
                deadline_ns: u64::MAX,
                priority: 10,
                payload_schema_id: SCHEMA_SCRIPTED_PAYLOAD,
                payload: build_scripted_payload_bytes(&wps),
                idempotency_key: "scripted-submit".into(),
                source: ActionSource::Internal,
            }
        }

        fn publish_state(
            ch: &MockChannel<markers::JointState>,
            layout: u64,
            positions: &[f64],
            stamp_ns: u64,
        ) {
            let body = build_joint_state(layout, stamp_ns, positions, &[], &[], "");
            let mut pub_ = ch.publisher();
            let mut w = pub_.loan(body.len() + 1024).expect("loan");
            w.set_body_bytes(&body);
            w.set_stamp_ns(stamp_ns);
            w.set_seq(0);
            w.set_source_id(1);
            w.publish().expect("publish");
        }

        fn attach_cache(c: Coordinator, ch: &MockChannel<markers::JointState>) -> Coordinator {
            let ch2 = ch.clone();
            let handle = spawn_state_cache_thread_with(
                StateCacheConfig {
                    topic: "/t/s0/arm/state".into(),
                    expected_joints: c.hardware.joints.len(),
                    expected_layout: LAYOUT,
                    max_stale_ms: 200,
                    poll_interval: Duration::from_millis(1),
                },
                Arc::new(SystemClock::new()),
                READY_TIMEOUT,
                move || Ok(ch2.subscriber()),
            )
            .expect("spawn cache");
            c.with_state_cache(handle)
        }

        /// 等 cache 线程（真线程 1 ms poll）观测到 `want`（同判据探针）。
        fn wait_probe(c: &Coordinator, now: MonoRawNs, want: &[f64]) {
            for _ in 0..2000 {
                if let Some(Ok(got)) = c.probe_state_cache(now)
                    && got == want
                {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!(
                "state cache never observed {want:?}: {:?}",
                c.probe_state_cache(now)
            );
        }

        fn rejected_reason(err: &SubmitError) -> &str {
            match err {
                SubmitError::BackendStartFailed(ActionStartError::Rejected(s)) => s.as_str(),
                other => panic!("expected BackendStartFailed(Rejected), got {other:?}"),
            }
        }

        fn decode_target(body: &JointCommandBody) -> Vec<f64> {
            let cmd = flatbuffers::root::<FbJointCommand<'_>>(body.as_bytes()).expect("verify");
            cmd.target().expect("target").iter().collect()
        }

        #[test]
        fn scripted_without_cache_is_rejected_not_fallback() {
            let c = scripted_coordinator();
            assert!(!c.has_state_cache());
            let clock = MockClock::new(MonoRawNs(1_000_000_000));
            let err = c
                .submit(scripted_request(&[vec![1.0, 1.0]]), &clock)
                .expect_err("must reject");
            let reason = rejected_reason(&err);
            assert!(reason.contains("no S1 state cache"), "{reason}");
            assert!(reason.contains("no fallback"), "{reason}");
            assert!(matches!(c.current_state(), CoordinatorState::Idle));
            assert_eq!(c.idle_hold_target(), ANCHOR.to_vec());
        }

        #[test]
        fn scripted_no_frame_is_rejected() {
            let ch = MockChannel::<markers::JointState>::new();
            let c = attach_cache(scripted_coordinator(), &ch);
            assert!(c.has_state_cache());
            let clock = MockClock::new(MonoRawNs(1_000_000_000));
            let err = c
                .submit(scripted_request(&[vec![1.0, 1.0]]), &clock)
                .expect_err("must reject");
            assert!(rejected_reason(&err).contains("no frame"), "{err}");
            assert!(matches!(c.current_state(), CoordinatorState::Idle));
        }

        #[test]
        fn scripted_fresh_frame_becomes_start_pose() {
            let ch = MockChannel::<markers::JointState>::new();
            let c = attach_cache(scripted_coordinator(), &ch);
            let clock = MockClock::new(MonoRawNs(1_000_000_000));
            let now = clock.now();
            let pose = vec![0.25, -0.25];
            publish_state(&ch, LAYOUT, &pose, now.0);
            wait_probe(&c, now, &pose);

            c.submit(scripted_request(&[vec![1.0, 1.0]]), &clock)
                .expect("submit ok with fresh frame");
            assert!(matches!(
                c.current_state(),
                CoordinatorState::Executing { .. }
            ));
            // α=0 首帧 target == cache pose（≠ anchor [9,9]）。
            let body = c
                .tick_action(&clock, 7)
                .expect("scripted first tick emits body");
            assert_eq!(decode_target(&body), pose);
            let cmd = flatbuffers::root::<FbJointCommand<'_>>(body.as_bytes()).expect("verify");
            assert_eq!(cmd.source_task_id(), 7);
        }

        #[test]
        fn scripted_invalid_positions_reject_before_execution_and_preserve_hold() {
            let ch = MockChannel::<markers::JointState>::new();
            let c = attach_cache(scripted_coordinator(), &ch);
            let clock = MockClock::new(MonoRawNs(1_000_000_000));
            let pose = vec![0.0, 0.0];
            publish_state(&ch, LAYOUT, &pose, clock.now().0);
            wait_probe(&c, clock.now(), &pose);

            // 先建立合法末点保持，拒绝后必须保留该目标而非非法输入或启动锚点。
            let hold = vec![0.25, -0.25];
            let _ticket = c
                .submit(scripted_request(std::slice::from_ref(&hold)), &clock)
                .expect("合法目标可提交");
            assert_eq!(decode_target(&c.tick_action(&clock, 1).unwrap()), pose);
            clock.advance(DurationNs::from_millis(100));
            assert_eq!(decode_target(&c.tick_action(&clock, 2).unwrap()), hold);
            clock.advance(DurationNs::from_millis(20));
            assert!(c.tick_action(&clock, 3).is_none());
            assert_eq!(c.idle_hold_target(), hold);

            for invalid in [
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::MAX,
                -f64::MAX,
            ] {
                let req = scripted_request(&[vec![invalid, 0.0]]);
                let action_id = req.action_id;
                let error = c.submit(req, &clock).expect_err("非法目标必须在启动前拒绝");
                assert!(rejected_reason(&error).contains("waypoints[0].positions[0]"));
                assert!(matches!(c.current_state(), CoordinatorState::Idle));
                assert!(
                    c.status(action_id).is_none(),
                    "未执行不能记录成完成或失败终态"
                );
                assert!(c.tick_action(&clock, 4).is_none(), "拒绝后不能生成动作命令");
                assert_eq!(c.idle_hold_target(), hold, "非法目标不能污染保持位置");
            }
            c.take_state_cache().unwrap().join().expect("回收采集线程");
        }

        #[test]
        fn scripted_stale_frame_is_rejected_without_fallback() {
            let ch = MockChannel::<markers::JointState>::new();
            let c = attach_cache(scripted_coordinator(), &ch);
            let clock = MockClock::new(MonoRawNs(1_000_000_000));
            let now = clock.now();
            let pose = vec![0.25, -0.25];
            publish_state(&ch, LAYOUT, &pose, now.0);
            wait_probe(&c, now, &pose);
            // 推 201 ms > max_stale 200 ms → stale。
            clock.advance(DurationNs::from_nanos(201_000_000));
            let err = c
                .submit(scripted_request(&[vec![1.0, 1.0]]), &clock)
                .expect_err("must reject stale");
            let reason = rejected_reason(&err);
            assert!(reason.contains("stale"), "{reason}");
            assert!(matches!(c.current_state(), CoordinatorState::Idle));
            assert_eq!(c.idle_hold_target(), ANCHOR.to_vec());
        }

        #[test]
        fn scripted_layout_mismatch_is_rejected() {
            let ch = MockChannel::<markers::JointState>::new();
            let c = attach_cache(scripted_coordinator(), &ch);
            let clock = MockClock::new(MonoRawNs(1_000_000_000));
            let now = clock.now();
            publish_state(&ch, LAYOUT ^ 0x1, &[0.0, 0.0], now.0);
            // 等线程收到（探针会因 layout 错返 Err；用 stats 等首帧）。
            for _ in 0..2000 {
                if matches!(
                    c.probe_state_cache(now),
                    Some(Err(
                        crate::state_cache::StaleOrMissing::LayoutMismatch { .. }
                    ))
                ) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let err = c
                .submit(scripted_request(&[vec![1.0, 1.0]]), &clock)
                .expect_err("must reject layout mismatch");
            assert!(
                rejected_reason(&err).contains("layout_id mismatch"),
                "{err}"
            );
            assert!(matches!(c.current_state(), CoordinatorState::Idle));
        }

        #[test]
        fn take_state_cache_then_scripted_rejected() {
            let ch = MockChannel::<markers::JointState>::new();
            let c = attach_cache(scripted_coordinator(), &ch);
            let cache = c.take_state_cache().expect("Some");
            assert!(!c.has_state_cache());
            cache.join().expect("join ok");
            let clock = MockClock::new(MonoRawNs(1_000_000_000));
            let err = c
                .submit(scripted_request(&[vec![1.0, 1.0]]), &clock)
                .expect_err("must reject");
            assert!(rejected_reason(&err).contains("no S1 state cache"));
        }

        fn single_joint_config() -> crate::single_joint::SingleJointConfig {
            let text = include_str!("../../../configs/single-joint-smoke.toml");
            let hardware = parse_hardware_section_from_toml(text, "arm").unwrap();
            parse_single_joint_config_from_toml(text, &hardware)
                .unwrap()
                .unwrap()
        }

        fn single_joint_request() -> ActionRequest {
            single_joint_request_for("arm", "j0", 0.5, 100)
        }

        fn single_joint_request_for(
            hardware: &str,
            joint: &str,
            target: f64,
            duration_ms: u32,
        ) -> ActionRequest {
            use robot_msg::msg::{SingleJointPositionPayload, SingleJointPositionPayloadArgs};
            let mut builder = flatbuffers::FlatBufferBuilder::new();
            let hardware_id = builder.create_string(hardware);
            let joint_name = builder.create_string(joint);
            let payload = SingleJointPositionPayload::create(
                &mut builder,
                &SingleJointPositionPayloadArgs {
                    hardware_id: Some(hardware_id),
                    joint_name: Some(joint_name),
                    target_position_rad: target,
                    duration_ms,
                },
            );
            builder.finish(payload, None);
            ActionRequest {
                payload_schema_id: robot_msg::schema_id::SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD,
                payload: builder.finished_data().to_vec(),
                ..scripted_request(&[vec![0.5, 0.25]])
            }
        }

        fn publish_motion_feedback(
            c: &Coordinator,
            channel: &MockChannel<markers::JointState>,
            clock: &MockClock,
            positions: &[f64],
            ready: bool,
        ) {
            use robot_msg::msg::{S0ControlLink, S0FaultCode, S0Mode, S0OutputMode, S0SafetyState};
            let stamp = clock.now().0;
            let body = robot_msg::build_helpers::build_joint_state_with_s0(
                LAYOUT,
                stamp,
                positions,
                &[],
                &[],
                "",
                if ready { S0Mode::Enabled } else { S0Mode::Safe },
                S0ControlLink::Ok,
                S0OutputMode::Active,
                S0SafetyState::TorqueOn,
                S0FaultCode::Unknown,
            );
            let mut publisher = channel.publisher();
            let mut writer = publisher.loan(body.len() + 1024).unwrap();
            writer.set_body_bytes(&body);
            writer.set_stamp_ns(stamp);
            writer.publish().unwrap();
            for _ in 0..2000 {
                let observed = c
                    .lock_state_cache()
                    .as_ref()
                    .unwrap()
                    .latest_fresh_snapshot(clock)
                    .ok();
                if observed.as_ref().is_some_and(|snapshot| {
                    snapshot.stamp_ns == stamp
                        && snapshot.positions == positions
                        && snapshot.motion_ready == ready
                }) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("模拟反馈未进入缓存");
        }

        fn single_joint_fixture() -> (Coordinator, MockChannel<markers::JointState>, MockClock) {
            let channel = MockChannel::new();
            let coordinator = attach_cache(scripted_coordinator(), &channel)
                .with_single_joint_config(Some(single_joint_config()));
            let clock = MockClock::new(MonoRawNs(1_000_000_000));
            publish_motion_feedback(&coordinator, &channel, &clock, &[0.0, 0.25], true);
            (coordinator, channel, clock)
        }

        fn publish_alignment_frame(
            coordinator: &Coordinator,
            channel: &MockChannel<markers::JointState>,
            layout: u64,
            stamp_ns: u64,
            positions: &[f64],
        ) {
            use robot_msg::msg::{S0ControlLink, S0FaultCode, S0Mode, S0OutputMode, S0SafetyState};
            let body = robot_msg::build_helpers::build_joint_state_with_s0(
                layout,
                stamp_ns,
                positions,
                &[],
                &[],
                "",
                S0Mode::Safe,
                S0ControlLink::Unknown,
                S0OutputMode::Unknown,
                S0SafetyState::TorqueOffConfirmed,
                S0FaultCode::Unknown,
            );
            let mut publisher = channel.publisher();
            let mut writer = publisher.loan(body.len() + 1024).unwrap();
            writer.set_body_bytes(&body);
            writer.set_stamp_ns(stamp_ns);
            writer.publish().unwrap();
            // keep-last 可重复缓存旧帧并递增计数；必须等本次完整body，坏帧也不能跳过。
            for _ in 0..2000 {
                if coordinator
                    .lock_state_cache()
                    .as_ref()
                    .unwrap()
                    .contains_test_frame(&body)
                {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("使能前对齐帧未进入既有缓存");
        }

        #[test]
        fn idle_alignment_tracks_only_confirmed_torque_off_and_keeps_startup_anchor() {
            let (coordinator, channel, clock) = single_joint_fixture();
            let anchor = coordinator.anchor_bias().to_vec();
            // 初始反馈为上扭状态，即使当前位置已变化也不刷新旧保持目标。
            assert_eq!(coordinator.idle_hold_target_for_tick(&clock), anchor);
            publish_alignment_frame(
                &coordinator,
                &channel,
                LAYOUT,
                clock.now().0,
                &[0.125, 0.25],
            );
            assert_eq!(coordinator.idle_hold_target_for_tick(&clock), [0.125, 0.25]);
            assert_eq!(coordinator.anchor_bias(), anchor);
            publish_motion_feedback(&coordinator, &channel, &clock, &[0.5, 0.75], true);
            assert_eq!(coordinator.idle_hold_target_for_tick(&clock), [0.125, 0.25]);
        }

        #[test]
        fn idle_alignment_never_overwrites_active_or_degraded_hold() {
            let (coordinator, channel, clock) = single_joint_fixture();
            coordinator.submit(single_joint_request(), &clock).unwrap();
            let hold = coordinator.idle_hold_target();
            publish_alignment_frame(
                &coordinator,
                &channel,
                LAYOUT,
                clock.now().0,
                &[0.125, 0.25],
            );
            assert_eq!(coordinator.idle_hold_target_for_tick(&clock), hold);
            // 即使状态与任务不一致，也不得覆盖仍存活任务的目标。
            coordinator.lock_inner().fsm = CoordinatorState::Idle;
            assert_eq!(coordinator.idle_hold_target_for_tick(&clock), hold);
            {
                let mut inner = coordinator.lock_inner();
                inner.current = None;
                inner.fsm = CoordinatorState::Degraded {
                    reason: "测试故障".into(),
                };
            }
            assert_eq!(coordinator.idle_hold_target_for_tick(&clock), hold);
        }

        #[test]
        fn idle_alignment_requires_opt_in_and_a_valid_fresh_pose_without_clamping() {
            let (mut coordinator, channel, clock) = single_joint_fixture();
            let config = coordinator.single_joint_config.take();
            let hold = coordinator.idle_hold_target();
            publish_alignment_frame(
                &coordinator,
                &channel,
                LAYOUT,
                clock.now().0,
                &[0.125, 0.25],
            );
            assert_eq!(coordinator.idle_hold_target_for_tick(&clock), hold);
            coordinator.single_joint_config = config;
            for (layout, stamp, positions) in [
                (LAYOUT ^ 1, clock.now().0, vec![0.125, 0.25]),
                (LAYOUT, clock.now().0 - 201_000_000, vec![0.125, 0.25]),
                (LAYOUT, clock.now().0 + 1, vec![0.125, 0.25]),
                (LAYOUT, clock.now().0, vec![0.125]),
                (LAYOUT, clock.now().0, vec![f64::NAN, 0.25]),
                (LAYOUT, clock.now().0, vec![f64::INFINITY, 0.25]),
                (LAYOUT, clock.now().0, vec![1.01, 0.25]),
                (LAYOUT, clock.now().0, vec![-1.01, 0.25]),
            ] {
                publish_alignment_frame(&coordinator, &channel, layout, stamp, &positions);
                assert_eq!(coordinator.idle_hold_target_for_tick(&clock), hold);
            }
            // 原值在边界内，但沿 S0 f32 编码后越界也不能作为原位保持。
            coordinator.single_joint_config.as_mut().unwrap().joints[0].max_position_rad = 0.1;
            publish_alignment_frame(&coordinator, &channel, LAYOUT, clock.now().0, &[0.1, 0.25]);
            assert_eq!(coordinator.idle_hold_target_for_tick(&clock), hold);
            let cache = coordinator.take_state_cache().unwrap();
            cache.join().unwrap();
            assert_eq!(coordinator.idle_hold_target_for_tick(&clock), hold);
        }

        fn tick_and_confirm_publish(c: &Coordinator, clock: &MockClock, seq: u64) -> Vec<f64> {
            let command = c.tick_action(clock, seq).expect("应发布完整目标");
            let target = decode_target(&command);
            c.command_published(clock.now().0);
            target
        }

        fn mixed_single_joint_fixture() -> (
            Coordinator,
            MockChannel<markers::JointState>,
            MockClock,
            Vec<f64>,
        ) {
            let mut hardware = mixed_test_hardware();
            hardware.calibration_range_tolerances_raw = None;
            hardware.joints.rotate_left(4);
            hardware.motor_ids.rotate_left(4);
            let feedback: Vec<_> = hardware
                .motor_ids
                .iter()
                .map(|&id| {
                    if id <= 6 {
                        f64::from(id) / 16.0
                    } else {
                        4000.0
                    }
                })
                .collect();
            let mut limits = single_joint_config();
            limits.max_duration_ms = 20_000;
            limits.joints = hardware
                .joints
                .iter()
                .enumerate()
                .filter(|(slot, _)| !hardware.is_wheel_slot(*slot))
                .map(|(_, name)| crate::single_joint::JointMotionLimits {
                    name: name.clone(),
                    ..limits.joints[0].clone()
                })
                .collect();
            limits.joints.reverse();
            let coordinator = Coordinator::new(
                CoordinatorConfig::default(),
                hardware,
                LAYOUT,
                feedback.clone(),
                BackendRegistry::from_explicit(&[&REAL_SCRIPTED as &'static dyn ActionBackend]),
                vec!["scripted".into()],
                None,
            )
            .with_single_joint_config(Some(limits));
            coordinator.mark_idle().unwrap();
            let channel = MockChannel::new();
            let coordinator = attach_cache(coordinator, &channel);
            let clock = MockClock::new(MonoRawNs(1_000_000_000));
            publish_motion_feedback(&coordinator, &channel, &clock, &feedback, true);
            (coordinator, channel, clock, feedback)
        }

        #[cfg(feature = "test-base-command")]
        fn simulated_base_fixture() -> (
            Coordinator,
            MockChannel<markers::JointState>,
            MockClock,
            Vec<f64>,
        ) {
            static BASE:
                crate::action_backend::simulated_base_command::SimulatedBaseCommandBackend =
                crate::action_backend::simulated_base_command::SimulatedBaseCommandBackend;
            let (mut coordinator, channel, clock, feedback) = mixed_single_joint_fixture();
            coordinator.registry = BackendRegistry::from_explicit(&[&REAL_SCRIPTED, &BASE]);
            coordinator
                .enabled_backends
                .push("simulated_base_command".into());
            (coordinator, channel, clock, feedback)
        }

        #[cfg(feature = "test-base-command")]
        fn simulated_base_request(
            c: &Coordinator,
            clock: &MockClock,
            feedback: &[f64],
            sequence: u64,
            raw: i16,
        ) -> ActionRequest {
            let mut held = feedback.to_vec();
            c.hardware.clear_wheel_targets(&mut held);
            let velocities: Vec<i16> = (0..held.len())
                .map(|slot| {
                    if c.hardware.is_wheel_slot(slot) {
                        raw
                    } else {
                        0
                    }
                })
                .collect();
            ActionRequest {
                payload_schema_id: robot_msg::schema_id::SCHEMA_JOINT_COMMAND,
                payload: robot_msg::build_helpers::build_mixed_joint_command(
                    LAYOUT,
                    &held,
                    &velocities,
                    clock.now().0 + 100_000_000,
                    10,
                    sequence,
                ),
                // RPC预算比运动租期长，不用它刷新原payload的短租期。
                deadline_ns: clock.now().0 + 10_000_000_000,
                ..scripted_request(&[held])
            }
        }

        #[cfg(feature = "test-base-command")]
        fn assert_simulated_base_body(
            c: &Coordinator,
            body: &[u8],
            feedback: &[f64],
            raw: i16,
            sequence: u64,
        ) -> u64 {
            let command = flatbuffers::root::<FbJointCommand<'_>>(body).unwrap();
            assert_eq!(command.mode(), ControlMode::MixedPositionRawVelocity);
            assert_eq!(command.on_timeout(), StaleAction::Hold);
            assert_eq!(command.source_task_id(), sequence);
            for (slot, &position) in feedback.iter().enumerate() {
                let wheel = c.hardware.is_wheel_slot(slot);
                assert_eq!(
                    command.target().unwrap().get(slot),
                    if wheel {
                        0.0
                    } else {
                        crate::single_joint::position_for_s0(position)
                    }
                );
                assert_eq!(
                    command.raw_velocities().unwrap().get(slot),
                    if wheel { raw } else { 0 }
                );
            }
            command.deadline_ns()
        }

        #[cfg(feature = "test-base-command")]
        #[test]
        fn simulated_base_three_wheels_fixed_deadline_and_original_publisher_return_to_zero() {
            let (c, _, clock, feedback) = simulated_base_fixture();
            let mut request = simulated_base_request(&c, &clock, &feedback, 1, 500);
            let mut held = feedback.clone();
            c.hardware.clear_wheel_targets(&mut held);
            let mut velocities = [0; 9];
            let wheels: Vec<_> = (0..9)
                .filter(|&slot| c.hardware.is_wheel_slot(slot))
                .collect();
            for (&slot, raw) in wheels.iter().zip([500, 0, -500]) {
                velocities[slot] = raw;
            }
            let fixed_deadline = clock.now().0 + 100_000_000;
            request.payload = robot_msg::build_helpers::build_mixed_joint_command(
                LAYOUT,
                &held,
                &velocities,
                fixed_deadline,
                10,
                1,
            );
            let check_nonzero = |body: &[u8], sequence| {
                let command = flatbuffers::root::<FbJointCommand<'_>>(body).unwrap();
                assert_eq!(command.mode(), ControlMode::MixedPositionRawVelocity);
                assert_eq!(command.on_timeout(), StaleAction::Hold);
                assert_eq!(command.source_task_id(), sequence);
                assert_eq!(command.deadline_ns(), fixed_deadline);
                for (slot, &raw) in velocities.iter().enumerate() {
                    assert_eq!(command.raw_velocities().unwrap().get(slot), raw);
                    assert_eq!(
                        command.target().unwrap().get(slot),
                        crate::single_joint::position_for_s0(held[slot])
                    );
                }
            };
            let ticket = c.submit(request, &clock).unwrap();
            let initial = c.tick_action(&clock, 0).unwrap();
            check_nonzero(initial.as_bytes(), 0);
            clock.advance(DurationNs::from_nanos(20_000_000));
            let next = c.tick_action(&clock, 1).unwrap();
            check_nonzero(next.as_bytes(), 1);
            let commands = MockChannel::<markers::JointCommand>::new();
            let heartbeats = MockChannel::<markers::Heartbeat>::new();
            let mut publisher = commands.publisher();
            let mut heartbeat = heartbeats.publisher();
            let tick = run_action_tick_loop(
                &c,
                &mut heartbeat,
                &mut publisher,
                &clock,
                Duration::from_millis(1),
                Some(Duration::from_millis(140)),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(commands.publishes(), tick.cmd_seq);
            assert!(tick.cmd_seq >= 5);
            let mut subscriber = commands.subscriber();
            let sample = subscriber.latest().unwrap().unwrap();
            assert_simulated_base_body(
                &c,
                sample.view().unwrap().body.raw(),
                &feedback,
                0,
                tick.cmd_seq - 1,
            );
            assert!(matches!(
                c.status(ticket.action_id),
                Some(ActionStatus::Completed(
                    ActionOutcome::PartialSuccess { .. }
                ))
            ));
            assert!(matches!(c.current_state(), CoordinatorState::Idle));
            c.take_state_cache().unwrap().join().unwrap();
        }

        #[cfg(feature = "test-base-command")]
        #[test]
        fn simulated_base_renew_keeps_original_target_and_cannot_revive() {
            let (c, _, clock, feedback) = simulated_base_fixture();
            let mut first = simulated_base_request(&c, &clock, &feedback, 1, 500);
            first.source = crate::action::ActionSource::RpcGateway {
                actor: "web-base-start".into(),
            };
            let ticket = c.submit(first, &clock).unwrap();
            for sequence in 2..=5 {
                clock.advance(DurationNs::from_millis(30));
                let request = |raw| ActionRequest {
                    action_id: ticket.action_id,
                    ..simulated_base_request(&c, &clock, &feedback, sequence, raw)
                };
                assert!(c.renew_simulated_base(&request(0), &clock).is_err());
                let renew = request(500);
                c.renew_simulated_base(&renew, &clock).unwrap();
                assert!(c.renew_simulated_base(&renew, &clock).is_err());
                assert_simulated_base_body(
                    &c,
                    c.tick_action(&clock, sequence).unwrap().as_bytes(),
                    &feedback,
                    500,
                    sequence,
                );
                c.command_published(clock.now().0);
            }
            clock.advance(DurationNs::from_millis(100));
            let late = ActionRequest {
                action_id: ticket.action_id,
                ..simulated_base_request(&c, &clock, &feedback, 6, 500)
            };
            assert!(c.renew_simulated_base(&late, &clock).is_err());
            assert_simulated_base_body(
                &c,
                c.tick_action(&clock, 6).unwrap().as_bytes(),
                &feedback,
                0,
                6,
            );
            c.command_published(clock.now().0);
            assert!(c.tick_action(&clock, 7).is_none());
            assert!(c.renew_simulated_base(&late, &clock).is_err());
            c.take_state_cache().unwrap().join().unwrap();
        }

        #[cfg(feature = "test-base-command")]
        #[test]
        fn simulated_base_cancel_requires_zero_publish_and_replay_stays_rejected() {
            let (c, _, clock, feedback) = simulated_base_fixture();
            let request = simulated_base_request(&c, &clock, &feedback, 7, 500);
            let replay = request.clone();
            let ticket = c.submit(request, &clock).unwrap();
            assert!(matches!(
                c.submit(replay.clone(), &clock),
                Err(SubmitError::BusyExecuting(_))
            ));
            let nonzero = c.tick_action(&clock, 41).unwrap();
            assert_simulated_base_body(&c, nonzero.as_bytes(), &feedback, 500, 41);
            c.command_published(clock.now().0);
            c.cancel(ticket.action_id, &clock).unwrap();
            for seq in 42..44 {
                let zero = c.tick_action(&clock, seq).unwrap();
                assert_simulated_base_body(&c, zero.as_bytes(), &feedback, 0, seq);
                assert!(matches!(
                    c.status(ticket.action_id),
                    Some(ActionStatus::Executing { .. })
                ));
            }
            // 未收到publisher成功回调时，不以发送意图报告停止完成。
            c.command_published(clock.now().0);
            assert!(c.tick_action(&clock, 44).is_none());
            assert!(matches!(
                c.status(ticket.action_id),
                Some(ActionStatus::Cancelled)
            ));
            assert!(
                c.submit(replay, &clock)
                    .unwrap_err()
                    .to_string()
                    .contains("禁止重放")
            );
            assert_mixed_target(
                &c.hold_command(&c.idle_hold_target(), 45, clock.now().0),
                &c.hardware,
                &c.idle_hold_target(),
            );
            let zero = simulated_base_request(&c, &clock, &feedback, 8, 0);
            let zero_ticket = c.submit(zero, &clock).unwrap();
            assert_simulated_base_body(
                &c,
                c.tick_action(&clock, 46).unwrap().as_bytes(),
                &feedback,
                0,
                46,
            );
            c.command_published(clock.now().0);
            assert!(c.tick_action(&clock, 47).is_none());
            assert!(matches!(
                c.status(zero_ticket.action_id),
                Some(ActionStatus::Completed(ActionOutcome::Success))
            ));
            c.take_state_cache().unwrap().join().unwrap();
        }

        #[cfg(feature = "test-base-command")]
        #[test]
        fn simulated_base_rejects_unapproved_raw_arm_target_and_oversized_lease() {
            let (c, _, clock, feedback) = simulated_base_fixture();
            for raw in [i16::MIN, -501, -499, 1, 499, 501, i16::MAX] {
                assert!(
                    c.submit(
                        simulated_base_request(&c, &clock, &feedback, 1, raw),
                        &clock
                    )
                    .is_err()
                );
                assert!(matches!(c.current_state(), CoordinatorState::Idle));
            }
            let mut held = feedback.clone();
            c.hardware.clear_wheel_targets(&mut held);
            let raw: Vec<_> = (0..held.len())
                .map(|slot| {
                    if c.hardware.is_wheel_slot(slot) {
                        500
                    } else {
                        0
                    }
                })
                .collect();
            let mut invalid_combo = raw.clone();
            let front = c
                .hardware
                .joints
                .iter()
                .position(|name| name == "base_front_wheel")
                .unwrap();
            invalid_combo[front] = -500;
            let mut request = simulated_base_request(&c, &clock, &feedback, 1, 500);
            request.payload = robot_msg::build_helpers::build_mixed_joint_command(
                LAYOUT,
                &held,
                &invalid_combo,
                clock.now().0 + 100_000_000,
                10,
                1,
            );
            assert!(c.submit(request, &clock).is_err());
            for (layout, deadline, changed_arm) in [
                (LAYOUT + 1, clock.now().0 + 100_000_000, false),
                (LAYOUT, clock.now().0 + 100_000_001, false),
                (LAYOUT, clock.now().0, false),
                (LAYOUT, clock.now().0 + 100_000_000, true),
            ] {
                let mut target = held.clone();
                if changed_arm {
                    target[0] += 0.001;
                }
                let mut request = simulated_base_request(&c, &clock, &feedback, 1, 500);
                request.payload = robot_msg::build_helpers::build_mixed_joint_command(
                    layout, &target, &raw, deadline, 10, 1,
                );
                assert!(c.submit(request, &clock).is_err());
            }
            // 全部提交拒绝均不消费序号，不产生动作实例。
            assert!(
                c.submit(
                    simulated_base_request(&c, &clock, &feedback, 1, 500),
                    &clock
                )
                .is_ok()
            );
            c.take_state_cache().unwrap().join().unwrap();
        }

        #[cfg(feature = "test-base-command")]
        #[test]
        fn simulated_base_fault_stops_before_failed_and_never_reuses_nonzero_hold() {
            let (c, channel, clock, feedback) = simulated_base_fixture();
            let ticket = c
                .submit(
                    simulated_base_request(&c, &clock, &feedback, 1, 500),
                    &clock,
                )
                .unwrap();
            c.tick_action(&clock, 1).unwrap();
            c.command_published(clock.now().0);
            clock.advance(DurationNs::from_nanos(20_000_000));
            publish_motion_feedback(&c, &channel, &clock, &feedback, false);
            c.cancel(ticket.action_id, &clock).unwrap();
            assert_simulated_base_body(
                &c,
                c.tick_action(&clock, 2).unwrap().as_bytes(),
                &feedback,
                0,
                2,
            );
            c.command_published(clock.now().0);
            assert!(c.tick_action(&clock, 3).is_none());
            assert!(matches!(
                c.status(ticket.action_id),
                Some(ActionStatus::Failed(ActionFailure::SafetyAbort(_)))
            ));
            assert!(matches!(
                c.current_state(),
                CoordinatorState::Degraded { .. }
            ));
            assert_mixed_target(
                &c.hold_command(&c.idle_hold_target(), 4, clock.now().0),
                &c.hardware,
                &c.idle_hold_target(),
            );
            c.clear_error().unwrap();
            assert!(c.tick_action(&clock, 5).is_none());
            c.take_state_cache().unwrap().join().unwrap();
        }

        #[cfg(feature = "test-base-command")]
        #[test]
        fn simulated_base_expired_before_first_tick_reports_no_nonzero_publish() {
            let (c, _, clock, feedback) = simulated_base_fixture();
            let ticket = c
                .submit(
                    simulated_base_request(&c, &clock, &feedback, 1, 500),
                    &clock,
                )
                .unwrap();
            clock.advance(DurationNs::from_nanos(100_000_000));
            assert_simulated_base_body(
                &c,
                c.tick_action(&clock, 1).unwrap().as_bytes(),
                &feedback,
                0,
                1,
            );
            assert!(matches!(
                c.status(ticket.action_id),
                Some(ActionStatus::Executing { .. })
            ));
            c.command_published(clock.now().0);
            assert!(c.tick_action(&clock, 2).is_none());
            assert!(
                matches!(c.status(ticket.action_id), Some(ActionStatus::Completed(
                ActionOutcome::PartialSuccess { warning }
            )) if warning.contains("未发布非零命令"))
            );
            c.take_state_cache().unwrap().join().unwrap();
        }

        #[cfg(feature = "test-base-command")]
        #[test]
        fn simulated_base_first_tick_rejects_feedback_older_than_start_snapshot() {
            let (c, channel, clock, feedback) = simulated_base_fixture();
            let ticket = c
                .submit(
                    simulated_base_request(&c, &clock, &feedback, 1, 500),
                    &clock,
                )
                .unwrap();
            let previous_clock = MockClock::new(MonoRawNs(clock.now().0 - 1));
            publish_motion_feedback(&c, &channel, &previous_clock, &feedback, true);
            assert_simulated_base_body(
                &c,
                c.tick_action(&clock, 1).unwrap().as_bytes(),
                &feedback,
                0,
                1,
            );
            c.command_published(clock.now().0);
            assert!(c.tick_action(&clock, 2).is_none());
            assert!(matches!(
                c.status(ticket.action_id),
                Some(ActionStatus::Failed(ActionFailure::SafetyAbort(_)))
            ));
            c.take_state_cache().unwrap().join().unwrap();
        }

        fn mixed_request(c: &Coordinator, duration_ms: u32) -> ActionRequest {
            let selected = c.hardware.motor_ids.iter().position(|&id| id == 5).unwrap();
            single_joint_request_for(
                &c.hardware.id,
                &c.hardware.joints[selected],
                0.75,
                duration_ms,
            )
        }

        #[test]
        fn idle_alignment_keeps_mixed_wheels_raw_zero_in_configuration_slot_order() {
            let (coordinator, channel, clock, mut positions) = mixed_single_joint_fixture();
            let anchor = coordinator.anchor_bias().to_vec();
            for (slot, position) in positions.iter_mut().enumerate() {
                if !coordinator.hardware.is_wheel_slot(slot) {
                    *position += 0.25;
                }
            }
            publish_alignment_frame(&coordinator, &channel, LAYOUT, clock.now().0, &positions);
            coordinator.hardware.clear_wheel_targets(&mut positions);
            let aligned = coordinator.idle_hold_target_for_tick(&clock);
            assert_eq!(aligned, positions);
            assert_eq!(coordinator.anchor_bias(), anchor);
            let command = coordinator.hold_command(&aligned, 11, clock.now().0);
            assert_mixed_target(&command, &coordinator.hardware, &positions);
        }

        fn assert_mixed_target(body: &[u8], hardware: &HardwareSection, expected: &[f64]) {
            let command = flatbuffers::root::<FbJointCommand<'_>>(body).unwrap();
            assert_eq!(command.mode(), ControlMode::MixedPositionRawVelocity);
            assert_eq!(command.on_timeout(), StaleAction::Hold);
            assert_eq!(
                command.raw_velocities().unwrap().iter().collect::<Vec<_>>(),
                [0; 9]
            );
            let target: Vec<_> = command.target().unwrap().iter().collect();
            assert_eq!(target, expected);
            for (slot, value) in target.iter().enumerate() {
                if hardware.is_wheel_slot(slot) {
                    assert_eq!(value.to_bits(), 0.0_f64.to_bits());
                }
            }
        }

        #[test]
        fn mixed_single_joint_cancel_before_publish_keeps_position_snapshot_and_zero_wheels() {
            let (c, _, clock, feedback) = mixed_single_joint_fixture();
            let mut expected = feedback.clone();
            c.hardware.clear_wheel_targets(&mut expected);
            assert_eq!(c.anchor_bias(), expected);
            assert_eq!(c.idle_hold_target(), expected);
            assert_eq!(c.probe_state_cache(clock.now()).unwrap().unwrap(), feedback);
            let ticket = c.submit(mixed_request(&c, 100), &clock).unwrap();
            c.cancel(ticket.action_id, &clock).unwrap();
            assert!(c.tick_action(&clock, 1).is_none());
            assert!(matches!(
                c.status(ticket.action_id),
                Some(ActionStatus::Cancelled)
            ));
            assert_eq!(c.idle_hold_target(), expected);
            assert_mixed_target(
                &c.hold_command(&c.idle_hold_target(), 2, clock.now().0),
                &c.hardware,
                &expected,
            );
            c.take_state_cache().unwrap().join().unwrap();
        }

        #[test]
        fn mixed_single_joint_cancel_fault_and_deadline_keep_last_published_target() {
            for termination in ["cancel", "fault", "deadline"] {
                let (c, channel, clock, feedback) = mixed_single_joint_fixture();
                let mut request = mixed_request(&c, 100);
                request.deadline_ns = clock.now().0 + 400_000_000;
                let ticket = c.submit(request, &clock).unwrap();
                tick_and_confirm_publish(&c, &clock, 1);
                clock.advance(DurationNs::from_nanos(50_000_000));
                let committed = tick_and_confirm_publish(&c, &clock, 2);
                clock.advance(DurationNs::from_nanos(25_000_000));
                let unsent = c.tick_action(&clock, 3).unwrap();
                assert_ne!(decode_target(&unsent), committed);
                match termination {
                    "cancel" => c.cancel(ticket.action_id, &clock).unwrap(),
                    "fault" => publish_motion_feedback(&c, &channel, &clock, &feedback, false),
                    "deadline" => clock.advance(DurationNs::from_nanos(325_000_000)),
                    _ => unreachable!(),
                }
                assert!(c.tick_action(&clock, 4).is_none());
                if termination == "cancel" {
                    assert!(matches!(
                        c.status(ticket.action_id),
                        Some(ActionStatus::Cancelled)
                    ));
                } else {
                    assert!(matches!(
                        c.status(ticket.action_id),
                        Some(ActionStatus::Failed(_))
                    ));
                    if termination == "fault" {
                        assert!(matches!(
                            c.current_state(),
                            CoordinatorState::Degraded { .. }
                        ));
                    } else {
                        assert!(matches!(c.current_state(), CoordinatorState::Idle));
                    }
                }
                assert_eq!(c.idle_hold_target(), committed);
                assert_mixed_target(
                    &c.hold_command(&c.idle_hold_target(), 5, clock.now().0),
                    &c.hardware,
                    &committed,
                );
                if termination == "fault" {
                    c.clear_error().unwrap();
                    assert_eq!(c.idle_hold_target(), committed);
                    assert_mixed_target(
                        &c.hold_command(&c.idle_hold_target(), 6, clock.now().0),
                        &c.hardware,
                        &committed,
                    );
                }
                c.take_state_cache().unwrap().join().unwrap();
            }
        }

        #[test]
        fn mixed_single_joint_20s_50hz_success_holds_position_target_and_zero_wheels() {
            let (c, channel, clock, mut feedback) = mixed_single_joint_fixture();
            let ticket = c.submit(mixed_request(&c, 20_000), &clock).unwrap();
            let mut last = Vec::new();
            // 真实 mixed 编码经解码后回灌六个位置；轮反馈始终保留非零值。
            for tick in 0..=1_000 {
                if tick != 0 {
                    clock.advance(DurationNs::from_nanos(20_000_000));
                }
                last = mixed_tick_with_position_feedback(&c, &channel, &clock, &mut feedback, tick);
            }
            assert!(matches!(
                c.current_state(),
                CoordinatorState::Executing { .. }
            ));
            clock.advance(DurationNs::from_nanos(20_000_000));
            publish_motion_feedback(&c, &channel, &clock, &feedback, true);
            assert_eq!(tick_and_confirm_publish(&c, &clock, 1_001), last);
            clock.advance(DurationNs::from_nanos(40_000_000));
            publish_motion_feedback(&c, &channel, &clock, &feedback, true);
            assert!(c.tick_action(&clock, 1_002).is_none());
            assert!(matches!(
                c.status(ticket.action_id),
                Some(ActionStatus::Completed(ActionOutcome::Success))
            ));
            assert_eq!(c.idle_hold_target(), last);
            assert_mixed_target(
                &c.hold_command(&c.idle_hold_target(), 1_003, clock.now().0),
                &c.hardware,
                &last,
            );
            c.take_state_cache().unwrap().join().unwrap();
        }

        fn mixed_tick_with_position_feedback(
            c: &Coordinator,
            channel: &MockChannel<markers::JointState>,
            clock: &MockClock,
            feedback: &mut [f64],
            seq: u64,
        ) -> Vec<f64> {
            publish_motion_feedback(c, channel, clock, feedback, true);
            let body = c.tick_action(clock, seq).expect("完整混合目标");
            let target = decode_target(&body);
            assert_mixed_target(body.as_bytes(), &c.hardware, &target);
            for (slot, position) in feedback.iter_mut().enumerate() {
                if c.hardware.is_wheel_slot(slot) {
                    assert_eq!(*position, 4000.0);
                } else {
                    if c.hardware.motor_ids[slot] != 5 {
                        assert_eq!(target[slot], *position);
                    }
                    *position = target[slot];
                }
            }
            c.command_published(clock.now().0);
            target
        }

        #[test]
        fn mixed_single_joint_20s_50hz_cancel_at_10s_freezes_last_published_target() {
            let (c, channel, clock, mut feedback) = mixed_single_joint_fixture();
            let started_at = clock.now().0;
            let ticket = c.submit(mixed_request(&c, 20_000), &clock).unwrap();
            let mut last = Vec::new();
            for tick in 0..500 {
                if tick != 0 {
                    clock.advance(DurationNs::from_nanos(20_000_000));
                }
                last = mixed_tick_with_position_feedback(&c, &channel, &clock, &mut feedback, tick);
            }
            clock.advance(DurationNs::from_nanos(20_000_000));
            assert_eq!(clock.now().0 - started_at, 10_000_000_000);
            publish_motion_feedback(&c, &channel, &clock, &feedback, true);
            let unsent = c.tick_action(&clock, 500).unwrap();
            assert_ne!(decode_target(&unsent), last);
            c.cancel(ticket.action_id, &clock).unwrap();
            assert!(matches!(
                c.current_state(),
                CoordinatorState::Executing { .. }
            ));
            assert!(c.tick_action(&clock, 501).is_none());
            assert!(matches!(
                c.status(ticket.action_id),
                Some(ActionStatus::Cancelled)
            ));
            assert_eq!(
                ticket
                    .events
                    .try_iter()
                    .filter(|event| matches!(event, ActionEvent::Cancelled { .. }))
                    .count(),
                1
            );
            for seq in 502..=504 {
                clock.advance(DurationNs::from_nanos(20_000_000));
                assert_eq!(c.idle_hold_target(), last);
                assert_mixed_target(
                    &c.hold_command(&c.idle_hold_target(), seq, clock.now().0),
                    &c.hardware,
                    &last,
                );
            }
            c.take_state_cache().unwrap().join().unwrap();
        }

        #[test]
        fn single_joint_configuration_is_explicit_and_production_configs_remain_disabled() {
            for (text, hardware_id) in [
                (
                    include_str!("../../../configs/xlerobot-bus1.toml"),
                    "left_arm_full_with_head",
                ),
                (
                    include_str!("../../../configs/xlerobot-bus2.toml"),
                    "right_arm_full_with_base",
                ),
            ] {
                let hardware = parse_hardware_section_from_toml(text, hardware_id).unwrap();
                assert!(
                    parse_single_joint_config_from_toml(text, &hardware)
                        .unwrap()
                        .is_none()
                );
            }
            let text = include_str!("../../../configs/single-joint-smoke.toml");
            assert!(parse_s1_section_from_toml(text).is_ok());
            assert_eq!(
                parse_profile_section_from_toml(text).unwrap().backends,
                ["scripted"]
            );
            let hardware = parse_hardware_section_from_toml(text, "arm").unwrap();
            let bad = text.replace("stable_window_ms = 40", "stable_window_ms = 201");
            assert!(parse_single_joint_config_from_toml(&bad, &hardware).is_err());
        }

        #[test]
        fn single_joint_uses_one_submit_snapshot_and_rejects_busy() {
            let (c, channel, clock) = single_joint_fixture();
            c.submit(single_joint_request(), &clock).unwrap();
            assert!(matches!(
                c.submit(single_joint_request(), &clock),
                Err(SubmitError::BusyExecuting(_))
            ));
            clock.advance(DurationNs::from_nanos(1_000_000));
            publish_motion_feedback(&c, &channel, &clock, &[0.125, 0.375], true);
            assert_eq!(tick_and_confirm_publish(&c, &clock, 1), [0.0, 0.25]);
            clock.advance(DurationNs::from_nanos(50_000_000));
            let halfway = tick_and_confirm_publish(&c, &clock, 2);
            assert!((halfway[0] - 0.25).abs() < 1e-12);
            assert_eq!(halfway[1], 0.25);
        }

        #[test]
        fn single_joint_cancel_before_first_publish_keeps_submit_pose() {
            let (c, _, clock) = single_joint_fixture();
            let ticket = c.submit(single_joint_request(), &clock).unwrap();
            c.cancel(ticket.action_id, &clock).unwrap();
            assert!(c.tick_action(&clock, 1).is_none());
            assert_eq!(c.idle_hold_target(), [0.0, 0.25]);
            assert!(matches!(
                c.status(ticket.action_id),
                Some(ActionStatus::Cancelled)
            ));
            assert_eq!(
                ticket
                    .events
                    .try_iter()
                    .filter(|event| matches!(event, ActionEvent::Cancelled { .. }))
                    .count(),
                1
            );
        }

        #[test]
        fn single_joint_cancel_never_commits_generated_but_unpublished_target() {
            let (c, _, clock) = single_joint_fixture();
            let ticket = c.submit(single_joint_request(), &clock).unwrap();
            tick_and_confirm_publish(&c, &clock, 1);
            clock.advance(DurationNs::from_nanos(50_000_000));
            let committed = tick_and_confirm_publish(&c, &clock, 2);
            clock.advance(DurationNs::from_nanos(25_000_000));
            let unsent = c.tick_action(&clock, 3).unwrap();
            assert_ne!(decode_target(&unsent), committed);
            c.cancel(ticket.action_id, &clock).unwrap();
            assert!(c.tick_action(&clock, 3).is_none());
            assert_eq!(c.idle_hold_target(), committed);
        }

        #[test]
        fn single_joint_deadline_keeps_last_published_target() {
            let (c, _, clock) = single_joint_fixture();
            let mut request = single_joint_request();
            request.deadline_ns = clock.now().0 + 400_000_000;
            let ticket = c.submit(request, &clock).unwrap();
            tick_and_confirm_publish(&c, &clock, 1);
            clock.advance(DurationNs::from_nanos(50_000_000));
            let last = tick_and_confirm_publish(&c, &clock, 2);
            clock.advance(DurationNs::from_nanos(350_000_000));
            assert!(c.tick_action(&clock, 3).is_none());
            assert!(matches!(
                c.status(ticket.action_id),
                Some(ActionStatus::Failed(ActionFailure::DeadlineExceeded))
            ));
            assert_eq!(c.idle_hold_target(), last);
        }

        #[test]
        fn single_joint_fault_then_clear_error_preserves_target_without_reenable() {
            let (c, channel, clock) = single_joint_fixture();
            let ticket = c.submit(single_joint_request(), &clock).unwrap();
            tick_and_confirm_publish(&c, &clock, 1);
            clock.advance(DurationNs::from_nanos(50_000_000));
            let last = tick_and_confirm_publish(&c, &clock, 2);
            clock.advance(DurationNs::from_nanos(10_000_000));
            publish_motion_feedback(&c, &channel, &clock, &last, false);
            assert!(c.tick_action(&clock, 3).is_none());
            assert!(matches!(
                c.status(ticket.action_id),
                Some(ActionStatus::Failed(_))
            ));
            assert!(matches!(
                c.current_state(),
                CoordinatorState::Degraded { .. }
            ));
            assert_eq!(c.idle_hold_target(), last);
            c.clear_error().unwrap();
            assert_eq!(c.idle_hold_target(), last);
            assert!(c.submit(single_joint_request(), &clock).is_err());
        }

        #[test]
        fn single_joint_success_waits_for_advancing_all_joint_feedback() {
            let (c, channel, clock) = single_joint_fixture();
            let ticket = c.submit(single_joint_request(), &clock).unwrap();
            tick_and_confirm_publish(&c, &clock, 1);
            clock.advance(DurationNs::from_nanos(100_000_000));
            assert_eq!(tick_and_confirm_publish(&c, &clock, 2), [0.5, 0.25]);
            clock.advance(DurationNs::from_nanos(20_000_000));
            publish_motion_feedback(&c, &channel, &clock, &[0.5, 0.25], true);
            tick_and_confirm_publish(&c, &clock, 3);
            clock.advance(DurationNs::from_nanos(40_000_000));
            tick_and_confirm_publish(&c, &clock, 4);
            assert!(matches!(
                c.current_state(),
                CoordinatorState::Executing { .. }
            ));
            publish_motion_feedback(&c, &channel, &clock, &[0.5, 0.375], true);
            tick_and_confirm_publish(&c, &clock, 5);
            for (delta, seq) in [(20_000_000, 6), (20_000_000, 7)] {
                clock.advance(DurationNs::from_nanos(delta));
                publish_motion_feedback(&c, &channel, &clock, &[0.5, 0.25], true);
                tick_and_confirm_publish(&c, &clock, seq);
            }
            clock.advance(DurationNs::from_nanos(20_000_000));
            publish_motion_feedback(&c, &channel, &clock, &[0.5, 0.25], true);
            assert!(c.tick_action(&clock, 8).is_none());
            assert!(matches!(
                c.status(ticket.action_id),
                Some(ActionStatus::Completed(ActionOutcome::Success))
            ));
            assert_eq!(c.idle_hold_target(), [0.5, 0.25]);
        }

        #[test]
        fn ac_e17_final_frame_then_completed_then_idle_hold_is_last_waypoint() {
            // coordinator 层末点发布保证：末帧 Continue(Some) 时仍 Executing；
            // 下一 tick Completed → Idle + idle_hold_target = 末 waypoint。
            let ch = MockChannel::<markers::JointState>::new();
            let c = attach_cache(scripted_coordinator(), &ch);
            let clock = MockClock::new(MonoRawNs(1_000_000_000));
            let now = clock.now();
            let pose = vec![0.0, 0.0];
            publish_state(&ch, LAYOUT, &pose, now.0);
            wait_probe(&c, now, &pose);
            let last = vec![1.0, -1.0];
            c.submit(scripted_request(std::slice::from_ref(&last)), &clock)
                .expect("submit ok");

            let first = c.tick_action(&clock, 1).expect("α=0 帧");
            assert_eq!(decode_target(&first), pose);

            clock.advance(DurationNs::from_nanos(1_000_000_000));
            let fin = c
                .tick_action(&clock, 2)
                .expect("末帧必须是 Some(body)（先发布再 Completed）");
            assert_eq!(decode_target(&fin), last);
            assert!(
                matches!(c.current_state(), CoordinatorState::Executing { .. }),
                "末帧 tick 后仍 Executing（Completed 推迟到下一 tick）"
            );
            assert_eq!(
                c.idle_hold_target(),
                ANCHOR.to_vec(),
                "末帧 tick 后 idle_hold_target 尚未提交"
            );

            clock.advance(DurationNs::from_nanos(20_000_000));
            assert!(c.tick_action(&clock, 3).is_none(), "Completed 帧不产 cmd");
            assert!(matches!(c.current_state(), CoordinatorState::Idle));
            assert_eq!(c.idle_hold_target(), last);
        }
    }
}
