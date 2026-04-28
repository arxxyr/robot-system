//! v0.3 Phase B B1 (1/3) coordinator 纯逻辑层。
//!
//! 本模块只放**可单元测试的纯函数 + 配置类型**，不直接 link iceoryx2 /
//! tokio / OS 信号。Binary 层（B1 (2/3) `crates/robot-s1/src/bin/s1_daemon.rs`）
//! 把启动锚定 + tick loop 编排起来；订阅器通过 [`LatestSubscriber`] trait
//! object 注入，便于测试用 mock 后端替代真 iceoryx2。
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
//! - [`parse_anchor_from_state_bytes`]：纯字节解码 + fail-fast 断言
//!   （fbs verify 失败 / position_rad 缺失 / joint count 错位 / state
//!   stamp 过期）。
//! - [`read_anchor_from_s0_state`]：泛型 over `LatestSubscriber<JointState>`
//!   的 polling loop；测试用 `MockSubscriber`，binary 用
//!   `Iceoryx2Subscriber`。三条 fail-fast 错误路径：超时 / stale /
//!   joint count 错位（B1 outline §4.2 + §9 决策 #2 锁定 —— 不 fallback
//!   `bias = 0.0`）。
//!
//! ## 不做
//!
//! - **不**起线程 / 不轮询 OS clock；调用方传 [`RobotClock`] trait
//!   object，测试用 `MockClock` 推时间，binary 用 `SystemClock`。
//! - **不**真 publish；binary 层自己 own publishers，调本模块拿 bytes
//!   后调 `SampleWriter::set_body_bytes(&bytes)`。
//! - **不**做 heartbeat builder（直接 `robot_msg::build_helpers::build_heartbeat`
//!   够用，没有 coordinator-specific 行为）。
//! - **不**接 `robot-s0::config_loader`（避免 s1 → s0 反向依赖；B1
//!   outline §9 决策 #6）。

// 中文 doc 块大量 identifier-like 字串（mock_s1 / position_rad / tokio /
// freeze 等），coordinator 模块走宽松 doc-markdown，跟 robot-s0 /
// robot-rpc 现有惯例对齐。
#![allow(clippy::doc_markdown)]

use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use flatbuffers::root;
use robot_core::time::{DurationNs, MonoRawNs, RobotClock};
use robot_msg::build_helpers::{build_heartbeat, build_joint_command};
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
    /// 配置里没有匹配 `--hardware-id` 的 `[[hardware]]` section。
    #[error("hardware section with id={0:?} not found in config")]
    HardwareIdNotFound(String),
    /// 匹配到的 `[[hardware]]` section 里 `joints = []`（B1 至少要 1 关节）。
    #[error("hardware section {0:?} has no joints (B1 requires >= 1 joint)")]
    EmptyJoints(String),
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
/// 写成 `heartbeatHz` 这种）静默走默认值导致部署事故。
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
    Ok(section)
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

// ============================================================================
// AnchorError + parse_anchor_from_state_bytes
// ============================================================================

/// `read_anchor_from_s0_state` / `parse_anchor_from_state_bytes` 失败原因。
///
/// 三条 fail-fast 路径（B1 outline §7.1 / §9 决策 #2 锁定）：
/// - [`AnchorError::Timeout`]：`anchor_timeout_ms` 内没收到任何 sample。
/// - [`AnchorError::Stale`]：拿到 sample 但 `state.stamp_ns` 比 `now`
///   旧超过 `max_stale_ms`（疑似 S0 已 freeze / panic）。
/// - [`AnchorError::JointCountMismatch`]：state `position_rad` 长度与
///   `expected_joints` 不一致（hardware section 配置漂移）。
///
/// 另两条非 fail-fast 但仍 Err 的路径：
/// - [`AnchorError::MissingPositionField`]：state body 没有 `position_rad`
///   字段（schema bug 或 publisher 错误）。
/// - [`AnchorError::Decode`]：fbs verify 失败 / view error。
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

/// 直接对一帧已收到的 `JointState` body bytes 做 anchor 提取 + 三条
/// fail-fast 断言。供单元测试 / `read_anchor_from_s0_state` 内部复用。
///
/// 注意 `bytes` 是 `JointState` body bytes（`build_joint_state`
/// 输出），**不是** wire sample（不含 `FrameHeader`）。
pub fn parse_anchor_from_state_bytes(
    bytes: &[u8],
    expected_joints: usize,
    now_ns: u64,
    max_stale_ns: u64,
) -> Result<Vec<f64>, AnchorError> {
    let js = root::<FbJointState<'_>>(bytes).map_err(|e| AnchorError::Decode(e.to_string()))?;

    let stamp_ns = js.stamp_ns();
    if stamp_ns == 0 || now_ns.saturating_sub(stamp_ns) > max_stale_ns {
        return Err(AnchorError::Stale {
            stamp_ns,
            now_ns,
            max_stale_ns,
        });
    }

    let Some(pos_vec) = js.position_rad() else {
        return Err(AnchorError::MissingPositionField);
    };

    if pos_vec.len() != expected_joints {
        return Err(AnchorError::JointCountMismatch {
            expected: expected_joints,
            got: pos_vec.len(),
        });
    }

    let bias: Vec<f64> = pos_vec.iter().collect();
    Ok(bias)
}

// ============================================================================
// read_anchor_from_s0_state
// ============================================================================

/// 启动期 polling loop：从 `subscriber` 拉最新 `JointState` sample，提
/// 取 `position_rad[0..expected_joints]` 作 anchor bias。
///
/// **行为**（B1 outline §4.2 + §7.1）：
/// - 在 `timeout` 内反复 `subscriber.latest()`：
///   - `Ok(Some(lease))`：执行 [`parse_anchor_from_state_bytes`] 三条
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
    body: &[u8],
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
    writer.set_body_bytes(body);
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
            &body,
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
        publish_body::<markers::Heartbeat, _>(hb_pub, &body, state.hb_seq, cfg.source_id, now_ns)?;
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

/// Production tick loop wrapper：循环调 [`tick_once`]，按 `poll_interval`
/// sleep；`shutdown.load(Acquire)` 或 `duration` 到期则退出。
///
/// **退出条件**（按检查顺序）：
/// 1. `shutdown.load(Acquire) == true`（信号处理器置位）。
/// 2. `duration.is_some_and(|d| state.elapsed_ns(now) >= d.as_nanos())`。
///
/// **clock 抽象**：测试用 [`robot_core::time::MockClock`] 可立即推时间，
/// `tick_once` 走 due 路径；binary 用 [`robot_core::time::SystemClock`]。
///
/// 返回最终 [`TickState`] 给 binary 做退出 banner（`cmd_seq` / `hb_seq` /
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
id      = "smoke_arm"
adapter = "mock_hard_rt"
joints  = ["smoke_j0"]
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
            &bytes, /* expected_joints */ 3, /* now_ns */ 150,
            /* max_stale_ns */ 1_000,
        )
        .unwrap();
        assert_eq!(bias, vec![1.5, -2.5, 0.0]);
    }

    #[test]
    fn parse_anchor_rejects_stale_sample() {
        let bytes = fresh_state_bytes(&[1.0], /* stamp_ns */ 1_000);
        let err = parse_anchor_from_state_bytes(
            &bytes, 1, /* now_ns */ 5_000, /* max_stale_ns */ 1_000,
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
            &bytes, 1, /* now_ns */ 100, /* max_stale_ns */ 1_000,
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
            &bytes, /* expected_joints */ 3, /* now_ns */ 150,
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
        let err = parse_anchor_from_state_bytes(&bytes, 1, 100, 1_000).unwrap_err();
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
        let err = parse_anchor_from_state_bytes(&cmd_bytes, 1, 200, 1_000).unwrap_err();
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
        // s0_daemon 先 ready，s1_daemon `After=s0_daemon`）。
        let now_ns: u64 = 1_000_000;
        let body = fresh_state_bytes(&[0.5, -0.5], now_ns - 50_000);
        publish_state(&channel, &body, now_ns - 50_000);

        let mut sub = channel.subscriber();
        let clock = MockClock::new(MonoRawNs(now_ns));
        let bias = read_anchor_from_s0_state(
            &mut sub,
            /* expected_joints */ 2,
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

    // ----- parse_hardware_section_from_toml -----

    #[test]
    fn parse_hardware_finds_matching_section() {
        let toml = r#"
[robot]
id             = "rb"
config_version = "0.3.0"

[[hardware]]
id      = "left_arm"
adapter = "mock_hard_rt"
joints  = ["a", "b"]

[[hardware]]
id      = "right_arm"
adapter = "feetech_sts3215"
joints  = ["r1", "r2", "r3"]
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
id      = "left_arm"
adapter = "mock_hard_rt"
joints  = ["a"]
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
id      = "smoke_arm"
adapter = "mock_hard_rt"
joints  = []
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
baud        = 1_000_000
poll_hz     = 200
"#;
        let section = parse_hardware_section_from_toml(toml, "bus2").unwrap();
        assert_eq!(section.id, "bus2");
        assert_eq!(section.joints, vec!["j0".to_string()]);
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
}
