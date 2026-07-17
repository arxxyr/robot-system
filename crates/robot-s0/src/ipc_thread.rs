//! `s0_ipc_thread` —— S0 四线程模型的"非 RT IPC 枢纽"。
//!
//! 对应 `docs/00_design.md` §1.2：
//! - 订阅 `JointCommand` / `Heartbeat` / `EStopEvent` iceoryx2 topic
//! - 把 `FlatBuffers` `JointCommand` 解析 → [`JointCommandBuf`] → 写
//!   [`LatestCommandWriter`]（供 `rt_thread` 零拷贝读）
//! - **驱动 state machine**：每 tick 消费 command/heartbeat/estop/`watchdog`/外部事件，
//!   调 [`try_transition`]，结果写入 state seqlock（供 `pub_thread` 读）
//! - 写 `last_hb_ns: Arc<AtomicU64>`（供 `watchdog` 监测）
//!
//! ### 架构决策（Part 2 已 confirm，见 `docs/handoffs/2026-04-24_4.md` §2）
//!
//! **state machine 执行者 = `ipc_thread`**：[`S0State`] 生命周期跟 `ipc_thread` 绑定，
//! 不设独立的 `state_thread`；`ipc_thread` 非 RT，allocate / lock / iceoryx2 都允许。
//!
//! ### 为什么 iceoryx2 subscribers 走 factory closure 注入
//!
//! iceoryx2 0.8 的 `ipc::Service` 家族用 `SingleThreaded` `arc_sync_policy`，
//! `Iceoryx2Subscriber<T>` 因此 **`!Send`**（内部 `Rc`）。直接把 subscriber 作为
//! 参数 move 进线程会被借用检查器拒绝；唯一干净的办法是让调用方提供一个
//! `FnOnce() -> IpcSubscribers` 闭包，在 `ipc_thread` **内部**执行构造 —— 这样 `!Send`
//! 的 subscribers 从不跨线程，只在 spawn 后的线程里存在。
//!
//! 闭包里通常调用 [`robot_transport::iceoryx2::subscriber`] 三次。同进程测试和
//! 跨进程 `s0_daemon` 共用同一模式。
//!
//! ### 线程语义
//!
//! - **非 RT**（普通优先级）。轮询周期 500 µs 默认（2 kHz），足够把最新 command
//!   转发给 `rt_thread`（`rt_thread` 本身 1 kHz）。
//! - 用 `Arc<AtomicBool>` 作退出信号；`event_tx.send(Event::Stop)` 也会走
//!   state machine 变成 `STOPPING` 态然后外层 stop。
//! - 用 `crossbeam_channel` 接 `watchdog`（下行） + 外部事件（`SIGINT` / 测试驱动）。
//!
//! ### Guards 生命周期（Part 2 简化）
//!
//! Part 2 scope 内，`Guards` 大部分字段（`adapter_connected` / `calibration_loaded` /
//! `power_ok` / `limits_valid` / `hw_enable_ok` 等）在**启动期一次性由 caller 填入**
//! `IpcThreadConfig::initial_guards`，运行期 `ipc_thread` 只更新 3 个字段：
//! - `heartbeat_ok`：收到新 heartbeat 置 true，收到 `HeartbeatTimeout` 事件置 false
//! - `estop_latched`：收到 `EStop` topic / 外部 `Event::EStop` 置 true；`ClearEStop` 置 false
//! - `adapter_fault_cleared`：收到外部 `Event::ClearError` 置 true
//!
//! 其他字段由 `rt_thread` 产生的观测（Part 3 加）回填；Part 2 不做。

#![forbid(unused_must_use)]
#![allow(clippy::doc_markdown)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use robot_core::adapter::{ControlModeRepr, JointCommandBuf, MAX_JOINTS};
use robot_core::time::RobotClock;
use robot_msg::markers;
use robot_msg::msg::{ControlMode as FbControlMode, JointCommand as FbJointCommand};
use robot_transport::iceoryx2::Iceoryx2Subscriber;
use robot_transport::{LatestCommandWriter, LatestSubscriber, RtSnapshotPublisher};

use robot_core::executor_ctrl::{ExecutorCtrlClient, OutputSafetyState};

use crate::clear_estop_action::{self, ClearEStopDryRunOutcome};
use crate::estop_latch::save_estop_latch;
use crate::ipc_tick::{ClearEStopTickOutcome, IpcRetryState, ipc_tick};
use crate::set_enabled_action::{self, SetEnabledDryRunOutcome};
use crate::state_machine::{
    EnabledSub, Event, EventQueue, FaultCode, Guards, S0State, S0StateSnapshot,
};

// ============================================================================
// RR control plane（A1-3 (1/3b) ClearEStop RR drain；reviewer 拍板）
// ============================================================================

/// RR control plane: `rr_server_thread` → `ipc_thread` 的 `ClearEStop`
/// request channel payload（A1-3 (1/3b)）。
///
/// reviewer A1-3 拍板线程模型：
///
/// - `rr_server_thread`（A1-3 (1/3c) 落地）只做 iceoryx2 RR I/O：收 wire
///   request → decode → 容量限制 → 转发本结构到 `ipc_thread`。
/// - `ipc_thread` 是唯一 action executor：持 pending 元数据、决定 dry_run
///   / live 路径、通过 `ipc_tick` 执行 live ClearEStop 三步硬顺序。
/// - response 经 channel 回到 `rr_server_thread`，由它 reply 给 gateway。
///
/// 字段语义：
///
/// - `request_id`：客户端 UUIDv7（每次 retry 重新生成；correlation / audit
///   only；详 `docs/00_design.md` §6.6.2 errata）。S0 端**仅**用于 RR
///   response routing，**不**用作幂等 cache key（idempotency cache 在
///   gateway 不在 S0；详 reviewer A1-3 拍板 3）。
/// - `dry_run`：true → 走 [`clear_estop_action::evaluate_clear_estop_dry_run`]
///   read-only 评估（不进 events queue / 不进 `ipc_tick`）；false → 走
///   live 路径（live 单飞 + 进 events queue + `ipc_tick` 三步硬顺序）。
/// - `confirmation` / `audit_reason`：gateway 已校过 `confirmation` 非空
///   （`InvalidArgument`），但 S0 也保留这些字段做 audit trace
///   （`tracing::info!` 落 journald；A1-3 (3/3) 端到端时再加详细字段）。
/// - `deadline_ns`：CLOCK_MONOTONIC_RAW 截止时刻（mono raw ns）。daemon
///   端在 RR drain 入口 / preempt 段 / response 派发前**三处**显式 check
///   `clock.now() > deadline_ns`，过期返 `DeadlineExceeded`，不入队 / 不
///   继续执行（reviewer A1-3 (1/3b) round 2 拍板 + frozen schema
///   `clear_estop.fbs:112` 要求）。
#[derive(Debug, Clone)]
pub struct ClearEStopRrRequest {
    /// 客户端 UUIDv7，用于 RR response routing；不用作幂等 key。
    pub request_id: String,
    /// `true` → dry_run read-only 评估；`false` → live 三步硬顺序。
    pub dry_run: bool,
    /// 人工确认串（gateway 已校非空）。
    pub confirmation: String,
    /// 审计原因（"who、why"）。
    pub audit_reason: String,
    /// CLOCK_MONOTONIC_RAW 截止时刻（mono raw ns）。
    pub deadline_ns: u64,
}

/// RR control plane: `ipc_thread` → `rr_server_thread` 的 `ClearEStop`
/// response channel payload（A1-3 (1/3b)）。
///
/// reviewer 拍板：S0 返**domain status**（[`ClearEStopRrPayload`]），gateway
/// **必须显式 match** 到 `tonic::Code`，**不**直接 cast——这条沿用 A1-1
/// `RpcStatus` 的 enum 映射约束。
#[derive(Debug, Clone)]
pub struct ClearEStopRrResponse {
    /// 复用 request 的 `request_id` 让 `rr_server_thread` 路由 reply。
    pub request_id: String,
    /// Domain status 枚举（详 [`ClearEStopRrPayload`]）。
    pub payload: ClearEStopRrPayload,
}

/// `ClearEStop` RR domain status enum（A1-3 (1/3b) reviewer 拍板）。
///
/// **不**直接 cast `tonic::Code`；gateway 显式 match 每个 variant 到
/// `tonic::Status::failed_precondition` / `internal` / `resource_exhausted`
/// / 等。
///
/// `Copy` 友好（无 `String` / `io::Error` 字段）；具体 IO 错误细节走
/// `tracing::warn!` 落 journald，不进 enum。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearEStopRrPayload {
    /// **live 成功**：fsync(primary+bak) → `clear_safety_latch()` → 翻 guard
    /// 三步全过；`cleared_at_ns` = `clock.now()` mono raw 时间戳。
    /// gateway 映射到 `tonic::Code::Ok`（gRPC `Ack`）。
    Cleared { cleared_at_ns: u64 },

    /// **dry_run 成功**：gate 全过（state == Safe + safety_state ==
    /// TorqueOffConfirmed）；`would_clear_at_ns` = 评估瞬间的 mono raw 时间戳，
    /// **不**代表硬件真清的时刻——dry_run 不动硬件。
    /// gateway 映射到 `tonic::Code::Ok`（携带 dry_run 提示字符串）。
    WouldClear { would_clear_at_ns: u64 },

    /// **gate 拒**：`*state != S0State::Safe`。`actual` 携带具体 state
    /// 让 gateway 给客户端清楚的诊断。live 路径走 `plan_transition` 返
    /// `None`，dry_run 路径走 `evaluate_clear_estop_dry_run` 直接拒。
    /// gateway 映射到 `tonic::Code::FailedPrecondition`。
    StateNotSafe { actual: S0State },

    /// **gate 拒**：state == Safe 但 `safety_state != TorqueOffConfirmed`
    /// （硬件断扭未确认）。**reviewer 拍板**：torque gate 看
    /// `executor_ctrl.safety_state()` 权威，不能靠 `guards.estop_latched`
    /// 副本绕过。
    /// gateway 映射到 `tonic::Code::FailedPrecondition`。
    NotTorqueOffConfirmed { actual: OutputSafetyState },

    /// **live: gate 通过但 fsync 失败**。具体 `io::Error` 走 tracing；
    /// 磁盘 latch 仍 latched=true（`save_estop_latch_clear` backup-first
    /// 保证；详 `docs/CHANGELOG.md` 2026-04-27 round 2 登记）。
    /// gateway 映射到 `tonic::Code::Internal`。
    FsyncFailed,

    /// **live: 单飞冲突**——`in_flight_live` 已被另一 live request 占用，
    /// 本 request 直拒不入队。reviewer 拍板：第二个 live 不能排队无限等，
    /// 防止两个 request 共享一个 `TickOutcome` 搞错 Ack。客户端可在收到
    /// `Cleared` / 失败 variant 后重试。
    /// gateway 映射到 `tonic::Code::ResourceExhausted`（A1-3 (1/3b) round 3
    /// 拍板：fbs `RpcStatus::ResourceExhausted = 7`；详
    /// `crates/robot-rpc/src/clear_estop.rs::rpc_status_to_tonic_code`）。
    AlreadyInProgress,

    /// **live: 被 emergency preempt**——RR drain 把 live request 入
    /// `in_flight_live` + push `Event::ClearEStop` 后，本 tick 又收到
    /// `EStop` / `SafetyStopFailed` 进 events queue。`ipc_thread` 主循环
    /// §5.6) preempt 段取消 in-flight live：移除 events queue 里的
    /// `Event::ClearEStop`，reply 本 variant。客户端应重新评估硬件状态
    /// 后决定是否重发。
    ///
    /// reviewer A1-3 (1/3b) round 2 阻断 1 拍板：避免 RR live 滞留
    /// EventQueue 被下一 tick 误清掉新触发的 EStop。
    /// gateway 映射到 `tonic::Code::Aborted`。
    PreemptedByEmergency,

    /// **deadline 过期**——`clock.now() > req.deadline_ns`。两处 check 之一
    /// 命中：(a) §5.5) RR drain 入口（过期请求直接拒，不入 in_flight_live
    /// / 不 push event）；(b) §5.6) preempt 段（drain 入队后到 ipc_tick
    /// 之前的窗口；同 emergency preempt 共用 take + remove + reply 三件事）。
    /// 任一命中即 reply 本 variant。
    ///
    /// **A1-3 (1/3b) round 3** 拍板：派发前**不**再事后覆盖 outcome ——
    /// 一旦 action 在 deadline 前开始产出 `Cleared` / `FsyncFailed` /
    /// `StateNotSafe` / `NotTorqueOffConfirmed`，response 必须返真实 outcome；
    /// 否则跟 `clear_estop.fbs` "S0 端 action layer ordering" 段"deadline
    /// 时三个翻转都不发生" 硬契约冲突。长 fsync 跨 deadline 极端 window 留
    /// A1-3 (3/3) e2e 评估。
    ///
    /// reviewer A1-3 (1/3b) round 2 阻断 2 + round 3 阻断 2 + frozen
    /// schema `clear_estop.fbs:112` 要求。
    /// gateway 映射到 `tonic::Code::DeadlineExceeded`。
    DeadlineExceeded,
}

/// RR control plane channels (A1-3 (1/3b))。
///
/// `request_rx`：`rr_server_thread` 转发到 `ipc_thread` 的 RR 请求。
/// `response_tx`：`ipc_thread` 派发回 `rr_server_thread` 的 domain status
/// payload。
///
/// 两条 channel 的 buffer 容量 / 类型由 caller 决定（A1-3 (1/3c)
/// `rr_server_thread` 实施时确定；reviewer 建议 64 或更小，防 overload）。
/// `ipc_thread` 用 `try_recv` / `send` 不阻塞主 tick。
pub struct RrChannels {
    pub request_rx: Receiver<ClearEStopRrRequest>,
    pub response_tx: Sender<ClearEStopRrResponse>,
}

// ============================================================================
// SetEnabled RR control plane（v0.5 候选 C；第 2 个专用 rr_server，D5）
// ============================================================================
//
// 与 ClearEStop RR **结构同构**（沿 A1-3 范式），但语义关键差异（D8）：
// ClearEStop 是**单 tick** action（`try_apply_clear_estop` 即时三步），SetEnabled
// 是**多 tick** action —— `SetEnabled(true)` 要观测 heartbeat 驱动的
// `Ready→Enabled.Active`（effectful `Enable(true)`），`SetEnabled(false)` 要观测
// effectful `Enable(false)` 的 adapter ack。因此 `ipc_thread` 持 [`PendingSetEnabled`]
// **跨 tick 观测** state，到 target / fault / deadline / preempt 才产 RR response。

/// RR control plane: `set_enabled_rr_server` → `ipc_thread` 的 `SetEnabled`
/// request channel payload（v0.5 候选 C）。
///
/// 字段语义：
/// - `request_id`：客户端 UUIDv7；S0 端**仅**用于 RR response routing，**不**作
///   幂等 key（idempotency cache 在 gateway，沿 ClearEStop A1-3 拍板 3）。
/// - `enabled`：`true` = enable（→ `Event::Enable`，目标 `Enabled.Active`）；
///   `false` = disable（→ `Event::Disable`，目标 `Safe`）。
/// - `dry_run`：`true` → 走 [`set_enabled_action::evaluate_set_enabled_dry_run`]
///   read-only 评估，立即 reply，**不**入 pending / 不 push event；`false` → live
///   多 tick 观测路径。
/// - `deadline_ns`：`CLOCK_MONOTONIC_RAW` 截止时刻（gateway 由 `CommandMeta.
///   timeout_ms` 转 wire deadline）。drain 入口 check 过期即拒；live pending 每
///   tick check 过期 → `DeadlineExceeded`。
/// - `role` / `audit_reason`：仅 audit（S0 不授权，沿 ClearEStop）。
#[derive(Debug, Clone)]
pub struct SetEnabledRrRequest {
    pub request_id: String,
    pub enabled: bool,
    pub dry_run: bool,
    pub deadline_ns: u64,
    pub audit_reason: String,
}

/// RR control plane: `ipc_thread` → `set_enabled_rr_server` 的 `SetEnabled`
/// response channel payload。`request_id` 复用让 rr_server 路由 reply。
#[derive(Debug, Clone)]
pub struct SetEnabledRrResponse {
    pub request_id: String,
    pub payload: SetEnabledRrPayload,
}

/// `SetEnabled` RR domain status enum（v0.5 候选 C）。
///
/// gateway / rr_server **显式 match** 每 variant → `RpcStatus` + domain
/// `SetEnabledStatus`（**不**直接 cast，沿 `ClearEStopRrPayload` 约束）。
/// `Copy` 友好（仅 `u64` / `S0State` POD 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetEnabledRrPayload {
    /// **enable 成功**：state commit `Enabled.Active` + adapter `Enable(true)`
    /// ack（D8 / AC-2）。`completed_at_ns` = 观测到 target 的 mono raw 时刻。
    /// → `RpcStatus::Ok` + domain `Ok`。
    Enabled { completed_at_ns: u64 },
    /// **disable 成功**：state commit `Safe` + adapter `Enable(false)` ack
    /// （D3 / AC-3）。→ `RpcStatus::Ok` + domain `Ok`。
    Disabled { completed_at_ns: u64 },
    /// **dry_run precondition 通过**（D7）：**不**代表 adapter 会 ack / enable
    /// 真完成。→ `RpcStatus::Ok` + domain `WouldAccept`。
    WouldAccept { at_ns: u64 },
    /// **precondition 拒**：ready guard 不满足 / state 不允许（`Fault` 需先
    /// `ClearError`，AC-9）。`actual` 携当前 state。→ `FailedPrecondition` +
    /// domain `GuardRejected`。
    GuardRejected { actual: S0State },
    /// **enable 被 latch 拒**：`estop_latched=true`（§6.7 #3 / AC-4）。→
    /// `FailedPrecondition` + domain `LatchBlocked`。
    LatchBlocked,
    /// **adapter ack 失败**：effectful `Enable(true/false)` ack fail →
    /// `commit_fault(AdapterErr)`（D3d / D8 / AC-2c / AC-3b）。→
    /// `RpcStatus::Internal` + domain `AdapterError`（**非** Ack）。
    AdapterError,
    /// **单飞冲突**：已有 in-flight SetEnabled pending，第二个直拒不入 pending。
    /// → `RpcStatus::ResourceExhausted` + domain `AlreadyInProgress`。
    AlreadyInProgress,
    /// **被 EStop preempt**：pending 期间 `EStop` / `SafetyStopFailed` 进 queue。
    /// → `RpcStatus::Aborted` + domain `PreemptedByEStop`。
    PreemptedByEStop,
    /// **被 shutdown preempt**：pending 期间进 `Stopping`（`Event::Stop`）。
    /// → `RpcStatus::Aborted` + domain `PreemptedByShutdown`。
    PreemptedByShutdown,
    /// **deadline 过期**：`clock.now() > deadline_ns`（drain 入口或 pending
    /// 观测期）。**不**回滚已发副作用（D8 / AC-2b）。→ `RpcStatus::DeadlineExceeded`
    /// + domain `DeadlineExceeded`。
    DeadlineExceeded,
}

/// `SetEnabled` RR control plane channels（沿 [`RrChannels`]）。
pub struct SetEnabledRrChannels {
    pub request_rx: Receiver<SetEnabledRrRequest>,
    pub response_tx: Sender<SetEnabledRrResponse>,
}

/// live SetEnabled 的多 tick 观测目标态（D8）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingTarget {
    /// `SetEnabled(true)` 目标：`Enabled.Active`。
    EnabledActive,
    /// `SetEnabled(false)` 目标：`Safe`。
    Safe,
}

/// in-flight live SetEnabled 的观测状态（单飞；**非** ClearEStop 单 tick）。
///
/// `ipc_thread` 每 tick 在 `ipc_tick` 后读 `state`：到 `target` → `Enabled`/
/// `Disabled`；`Fault(AdapterErr)` → `AdapterError`；EStop preempt /
/// `Stopping` / deadline → 对应 payload。resolve 后清空（释放单飞）。
#[derive(Debug, Clone)]
pub struct PendingSetEnabled {
    /// resolve 时回填 [`SetEnabledRrResponse`]，rr_server 按此路由 reply。
    pub request_id: String,
    /// 观测截止（`CommandMeta.timeout_ms` → wire `deadline_ns`，D-P2）。
    pub deadline_ns: u64,
    /// 观测目标态（enable→`EnabledActive` / disable→`Safe`）。
    pub target: PendingTarget,
    /// 原 `SetEnabledRequest.enabled`（audit + 对称性）。
    pub requested_enabled: bool,
}

// ============================================================================
// Config / Metrics / Handle
// ============================================================================

/// `ipc_thread` 配置。
#[derive(Clone, Debug)]
pub struct IpcThreadConfig {
    /// 主循环轮询间隔（纳秒）。默认 500 µs（2 kHz）。
    pub poll_interval_ns: u64,
    /// 启动期 Guards 快照。大部分字段由 caller（`s0_daemon`）在 `adapter.connect()`
    /// 成功后填入 all-true；`estop_latched` 从 latch 文件读。
    pub initial_guards: Guards,
    /// 初始 state。生产 `S0State::Safe`；测试可注入任意初值。
    pub initial_state: S0State,
    /// **task #15 阻断 1 修复**：`EStop` latch 文件路径（`fsync` 双写）。
    ///
    /// 设计 `docs/00_design.md` §6.7 不变量 #4 + §1117 强制要求：
    /// > "`EStop` 来源进入 S0 → 立即 `fsync` 写 latch 文件，不等 graceful shutdown"
    ///
    /// 旧实现仅在 daemon 退出时一次性 save，**daemon 在 graceful 退出前崩溃
    /// 会让 latch 落不下来 → 重启读 false → 自动 `Enable` → 安全语义破坏**。
    ///
    /// 新实现：本字段 `Some` 时，`ipc_thread` 主循环每 tick 走 latch fsync
    /// hook 两条路径（A1-3 (1/3a) 之后）：
    ///
    /// - **`EStop` 翻转 (false → true 单调)**：通用 hook 比较
    ///   `prev_estop_latched` vs `guards.estop_latched`，翻转时立即
    ///   `save_estop_latch(latch_path, true)`（primary-first 双文件 `fsync`
    ///   rename，~ms 级）。
    /// - **`ClearEStop` 路径 (true → false)**：由 `ipc_tick` 内
    ///   [`crate::clear_estop_action::try_apply_clear_estop`] 调
    ///   [`crate::estop_latch::save_estop_latch_clear`]（**backup-first**，
    ///   保证 partial failure 时磁盘 latch 不动；详 `docs/CHANGELOG.md`
    ///   2026-04-27 `round 2` 阻断登记）；`ipc_thread` hook 通过
    ///   `tick_outcome.clear_estop` 的 `ClearEStopTickOutcome::Cleared` /
    ///   `FsyncFailed` 等 variant 决定计数 + 同步 prev，**不**再二次
    ///   `save_estop_latch`（防二次写）。
    ///
    /// `None` 仅供单测：跳过 latch IO，保持测试纯类型路径。生产 daemon 必须
    /// 传 `Some(args.latch_path.clone())`。
    pub latch_path: Option<PathBuf>,
    /// **v0.5 候选 E 段(3)（D1 command 侧 layout gate，§3.5.1 部分落地）**：
    /// 本 S0 实例的权威 `layout_id`（config 模式 = `compute_layout_id(
    /// robot_id, hardware_id, zip(joints, motor_ids))` 派生；CLI-only 沿
    /// 既有占位）。`poll_command` 对每帧 `JointCommand.layout_id` 比对：
    /// **不等 → 丢弃不写 slot** + `unknown_layout_count` +1 + 限频 warn。
    pub authoritative_layout: u64,
}

impl Default for IpcThreadConfig {
    fn default() -> Self {
        Self {
            poll_interval_ns: 500_000, // 500 µs
            initial_guards: Guards::default(),
            initial_state: S0State::Safe,
            latch_path: None,
            // 与 CLI-only 生产路径同占位（§3.5.1 部分落地边界）；单测显式
            // 覆盖为各自 fixture 的 layout。
            authoritative_layout: 0xCAFE_BABE_DEAD_BEEF,
        }
    }
}

/// 非 RT 路径的观测计数器（测试 / 诊断用）。
///
/// `Relaxed` ordering 足够 —— 这些是统计类 counter，不参与控制流同步。
#[derive(Debug, Default)]
pub struct IpcThreadMetrics {
    /// 主循环 tick 数。
    pub tick_count: AtomicU64,
    /// 成功解析并写入 `LatestCommandWriter` 的 `JointCommand` 数。
    pub commands_applied: AtomicU64,
    /// **v0.5 候选 E 段(3)**：`layout_id` 不等于权威值被丢弃的
    /// `JointCommand` 帧数（`AtomicU64::fetch_add(1, Relaxed)`；丢弃 =
    /// 不写 `LatestCommandSlot`，防错位命令驱动错关节集）。
    pub unknown_layout_count: AtomicU64,
    /// `JointCommand` 解析失败（schema mismatch / verify 失败）次数。
    pub command_parse_errors: AtomicU64,
    /// `Heartbeat` 成功接收数。
    pub heartbeats_received: AtomicU64,
    /// `Heartbeat` 解析失败次数。
    pub heartbeat_parse_errors: AtomicU64,
    /// `EStopEvent` topic 接收数。
    pub estops_received: AtomicU64,
    /// `EStopEvent` 解析失败次数。
    pub estop_parse_errors: AtomicU64,
    /// Watchdog 发来的事件数（主要是 `HeartbeatTimeout`）。
    pub watchdog_events: AtomicU64,
    /// 外部 channel 发来的事件数（`SIGINT` / 测试驱动）。
    pub external_events: AtomicU64,
    /// task-30 v9 / M7 阶段 4：`ipc_tick` 调用次数（每 tick 一次）。
    pub ipc_tick_count: AtomicU64,
    /// state 变化次数（`new_state != prev_state`）。
    pub state_changes: AtomicU64,
    /// **task #15 阻断 1 修复**：触发即写 latch 的累计成功次数（每次
    /// `guards.estop_latched` 翻转 + `save_estop_latch` 成功 → +1）。
    pub latch_writes: AtomicU64,
    /// 同上，写盘失败次数（IO error）；触发后即便失败也继续运行（state 已
    /// 进 Safe，硬件已断扭；下次 daemon 启动可能漏读 latch，但这是分级降级
    /// 不阻断当前 tick）。
    pub latch_write_errors: AtomicU64,
    /// **A2 freeze hook**（v0.3 / test-only，cfg-gated）：tick loop 顶端 spin
    /// 到该单调时钟 ns 为止；0 = 不冻。详 [`crate::freeze_hook`] 模块 doc。
    /// 仅 `cfg(any(test, feature = "test-freeze-hook"))` 编译时存在，**不进
    /// 生产 release**（reviewer 拍板硬边界）。
    #[cfg(any(test, feature = "test-freeze-hook"))]
    pub freeze_until_ns: AtomicU64,
}

/// `ipc_thread` 线程内部持有的 3 个 iceoryx2 subscriber。
///
/// `!Send`（iceoryx2 `ipc::Service` 是 `SingleThreaded` `arc_sync_policy`），所以只能
/// 在线程内部构造。外部 `spawn_ipc_thread` 通过 factory closure 交付。
pub struct IpcSubscribers {
    pub command: Iceoryx2Subscriber<markers::JointCommand>,
    pub heartbeat: Iceoryx2Subscriber<markers::Heartbeat>,
    pub estop: Iceoryx2Subscriber<markers::EStopEvent>,
}

/// `ipc_thread` 启动后返回的控制 handle。
pub struct IpcThreadHandle {
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    /// 给 `watchdog` 读：`ipc_thread` 每收到 heartbeat 就 `store(now, Release)`。
    last_hb_ns: Arc<AtomicU64>,
    /// 给外部（signal handler / `CLI` / 测试）发 Event（`EStop` / Stop / Clear`EStop` ...）。
    event_tx: Sender<Event>,
    metrics: Arc<IpcThreadMetrics>,
}

impl IpcThreadHandle {
    /// 请求停止。下次 tick 开始时检测到并退出。
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Watchdog 监测用的 `last_hb_ns` 原子。
    #[must_use]
    pub fn last_hb_ns(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.last_hb_ns)
    }

    /// 外部注入事件（signal handler / `CLI` / 测试）。
    #[must_use]
    pub fn event_sender(&self) -> Sender<Event> {
        self.event_tx.clone()
    }

    #[must_use]
    pub fn metrics(&self) -> &Arc<IpcThreadMetrics> {
        &self.metrics
    }

    /// 等待线程退出。
    ///
    /// # Errors
    ///
    /// 返回 `Err` 当 `ipc_thread` panic。
    pub fn join(self) -> thread::Result<()> {
        self.join.join()
    }
}

// ============================================================================
// 入口
// ============================================================================

/// 启动 `ipc_thread`。
///
/// - `cfg`：初值 + 轮询间隔。
/// - `clock`：测试用 `MockClock`，生产用 `SystemClock`。
/// - `subscribers_factory`：`FnOnce` 闭包，在 `ipc_thread` 线程内执行，返回 3 个
///   已 open 的 iceoryx2 subscriber。用闭包包装是因为 iceoryx2 `ipc::Service`
///   的 subscriber 是 `!Send`（见模块 doc "为什么 ..."）。闭包 panic / unwind
///   会让 `ipc_thread` 直接退出（stop signal 没机会走 state machine）—— 调用方应
///   保证 iceoryx2 open 成功再进入 `production daemon`，或者接受 `panic-abort`。
/// - `cmd_writer`：`LatestCommandWriter<JointCommandBuf>` 写端（单写者）。
/// - `state_pub`：`RtSnapshotPublisher<S0StateSnapshot>` 写端（单写者，每 tick 发布）。
/// - `watchdog_rx`：`watchdog` 发 `HeartbeatTimeout` 进来。
///
/// # Errors
///
/// 返回 `std::io::Error` 当 `thread::Builder::spawn` 失败（OS 资源）。
///
/// **task-30 v9 / M7 阶段 4**：新增必传参数 `executor_ctrl: Arc<dyn
/// ExecutorCtrlClient>`。`ipc_thread` 主循环现在用 [`ipc_tick`] 三步式驱动
/// `executor_ctrl`（`set_policy` / `safety_stop` / `request_normal` /
/// `poll_last_normal_resp` / `clear_safety_latch`），**不再绕过 `ipc_tick`
/// 直接改 state**（reviewer 硬要求 #2）。
///
/// `executor_ctrl` 由 `s0_daemon` 按 `RtClass` 分发后构造：`HardRT` 路径的
/// `Arc<HardRtCtrlClient>`，`SerialRT` 路径的 `Arc<SerialIoCtrlClient>`
/// （同份 `HardRtCtrlInner` 的两路命名 alias）。
#[allow(clippy::too_many_arguments)]
pub fn spawn_ipc_thread<F>(
    cfg: IpcThreadConfig,
    clock: Arc<dyn RobotClock>,
    subscribers_factory: F,
    cmd_writer: LatestCommandWriter<JointCommandBuf>,
    state_pub: RtSnapshotPublisher<S0StateSnapshot>,
    watchdog_rx: Receiver<Event>,
    executor_ctrl: Arc<dyn ExecutorCtrlClient>,
    rr_channels: Option<RrChannels>,
    set_enabled_rr_channels: Option<SetEnabledRrChannels>,
) -> std::io::Result<IpcThreadHandle>
where
    F: FnOnce() -> IpcSubscribers + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let last_hb_ns = Arc::new(AtomicU64::new(clock.now().0));
    let metrics = Arc::new(IpcThreadMetrics::default());

    // 外部事件 channel（SIGINT / CLI / 测试驱动）。unbounded 避免发送端阻塞；
    // 预期流量极低（信号级），不会内存失控。
    let (event_tx, event_rx) = unbounded::<Event>();

    let join_stop = Arc::clone(&stop);
    let join_last_hb = Arc::clone(&last_hb_ns);
    let join_metrics = Arc::clone(&metrics);

    // 同步信号：spawn 返回前，确保 subscribers_factory 已成功完成 —— 调用方一旦
    // 拿到 handle，就可以开始发 publish，subscribers 这时已经 subscribe 到 service。
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

    let join = thread::Builder::new()
        .name("s0_ipc_thread".into())
        .spawn(move || {
            let subs = subscribers_factory();
            // 通知外层 "subscribers 已 ready"；ready_rx.recv() 立刻解除阻塞。
            let _ = ready_tx.send(());
            drop(ready_tx);
            run_loop(
                &cfg,
                clock.as_ref(),
                subs,
                &cmd_writer,
                &state_pub,
                &watchdog_rx,
                &event_rx,
                &join_stop,
                &join_last_hb,
                &join_metrics,
                executor_ctrl.as_ref(),
                rr_channels.as_ref(),
                set_enabled_rr_channels.as_ref(),
            );
        })?;

    // 阻塞等 factory 完成。若 factory panic 则 ready_tx drop → recv 返回 Err，
    // 我们往上抛一个 io::Error（调用方 unwrap 会看到"ipc_thread factory panicked"）。
    if ready_rx.recv().is_err() {
        return Err(std::io::Error::other(
            "ipc_thread subscribers_factory panicked before signalling ready",
        ));
    }

    Ok(IpcThreadHandle {
        join,
        stop,
        last_hb_ns,
        event_tx,
        metrics,
    })
}

// ============================================================================
// 主循环
// ============================================================================

/// **task-30 v9 / M7 阶段 4**：主循环改造为 [`ipc_tick`] 驱动。
///
/// 与旧 `run_loop` 的差别：
/// - **删除** `apply_event` + `try_transition` 直接改 state 的旧路径
///   （reviewer 硬要求 #2）；
/// - [`EventQueue`] 收集所有来源（`heartbeat` / `estop` topic / `watchdog` /
///   外部 channel），由 `ipc_tick` 三步式（`drain_emergency` → 推进 pending →
///   plan）统一消费；
/// - **`ClearEStop` / `EStop` / `ClearError` 等事件的 `guards` 副作用必须在
///   `events.push` 之前更新**（reviewer 硬要求 #1）—— 否则 `ipc_tick` 内部
///   `plan_transition` 会因 stale guard 拒绝 transition。
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_loop(
    cfg: &IpcThreadConfig,
    clock: &dyn RobotClock,
    mut subs: IpcSubscribers,
    cmd_writer: &LatestCommandWriter<JointCommandBuf>,
    state_pub: &RtSnapshotPublisher<S0StateSnapshot>,
    watchdog_rx: &Receiver<Event>,
    event_rx: &Receiver<Event>,
    stop: &AtomicBool,
    last_hb_ns: &AtomicU64,
    metrics: &IpcThreadMetrics,
    executor_ctrl: &dyn ExecutorCtrlClient,
    rr_channels: Option<&RrChannels>,
    set_enabled_rr_channels: Option<&SetEnabledRrChannels>,
) {
    let mut state = cfg.initial_state;
    let mut guards = cfg.initial_guards;
    let mut events = EventQueue::new();
    let mut retry = IpcRetryState::new();

    // **task #15 阻断 1 修复**：初值落盘 baseline。`prev_estop_latched`
    // 跟 `guards.estop_latched` 同步，每 tick 比较；翻转时立即 fsync 写
    // latch 文件。`latch_path` 为 `None` 时（单测）整条路径 no-op。
    let mut prev_estop_latched = guards.estop_latched;

    // **A1-3 (1/3b)**：live ClearEStop 单飞插槽。reviewer 拍板：第二个
    // live request 不能排队无限等，第二个直接拒 `AlreadyInProgress`。
    // dry_run 不入此槽（同步 read-only 评估，立刻 reply）。
    let mut in_flight_live: Option<ClearEStopRrRequest> = None;

    // **v0.5 候选 C**：live SetEnabled 单飞 + 多 tick 观测槽（D8）。区别
    // `in_flight_live`（ClearEStop 单 tick）：SetEnabled 跨 tick 观测 state 到
    // target / fault / deadline / preempt 才 resolve。dry_run 不入此槽。
    let mut pending_set_enabled: Option<PendingSetEnabled> = None;

    // 启动即发一次初值快照。
    state_pub.publish(&S0StateSnapshot::new(state, guards, clock.now().0));

    while !stop.load(Ordering::Acquire) {
        // ─── A2 freeze hook（test-only，0 cost when freeze_until_ns == 0）───
        #[cfg(any(test, feature = "test-freeze-hook"))]
        crate::freeze_hook::freeze_check(&metrics.freeze_until_ns, clock, stop);

        // ─── 1) data plane: JointCommand → cmd_writer (不进 EventQueue) ───
        poll_command(
            &mut subs.command,
            cmd_writer,
            cfg.authoritative_layout,
            metrics,
        );

        // ─── 2) Heartbeat：本 event 没有"反向 guard 副作用"（只在收到时
        //       置 heartbeat_ok=true，未收到时由 watchdog HeartbeatTimeout
        //       走 ipc_tick 内置 false），可以在 push 前安全置位 ───
        if poll_heartbeat(&mut subs.heartbeat, clock, last_hb_ns, metrics) {
            guards.heartbeat_ok = true;
            events.push(Event::Heartbeat);
        }

        // ─── 3) EStop topic / 4) Watchdog channel / 5) 外部 channel ───
        //
        // **第 5 条门槛修订（SerialRT E2E）**：本 ipc_thread::run_loop **不再**
        // 在 channel drain 阶段对 ClearEStop / EStop / ClearError /
        // HeartbeatTimeout 调 apply_event_guard_side_effect（旧逻辑会让同
        // tick 多事件批量进入 ipc_tick 时，前到事件被后到事件的 guard 改写
        // 污染 plan）。改成 ipc_tick 内每个 event pop 后逐条 apply。详
        // ipc_tick 模块 doc + 锁测
        // `events_in_same_tick_use_per_event_guard_view`。
        if poll_estop(&mut subs.estop, metrics) {
            events.push(Event::EStop);
        }
        while let Ok(ev) = watchdog_rx.try_recv() {
            metrics.watchdog_events.fetch_add(1, Ordering::Relaxed);
            events.push(ev);
        }
        while let Ok(ev) = event_rx.try_recv() {
            metrics.external_events.fetch_add(1, Ordering::Relaxed);
            events.push(ev);
        }

        // ─── 5.5) RR control plane drain (A1-3 (1/3b)) ───
        //
        // reviewer 拍板线程模型：rr_server_thread 做 iceoryx2 RR I/O，
        // ipc_thread 是唯一 action executor。本步：
        //
        // - **deadline check**（drain 入口；A1-3 (1/3b) round 2 阻断 2）：
        //   `clock.now() > req.deadline_ns` → 直接 reply
        //   `DeadlineExceeded`，**不**入 in_flight_live / **不** push event。
        // - **dry_run** request：直接同步调
        //   `clear_estop_action::evaluate_clear_estop_dry_run` (read-only)
        //   评估，**不**进 events queue / 不进 ipc_tick；立即把 domain
        //   status 通过 response_tx 回 rr_server_thread。
        // - **live** request：检查 `in_flight_live` 单飞槽：
        //     - 槽空 → 占用 + push `Event::ClearEStop` 进 events queue，
        //       让本 tick 后续 ipc_tick 走三步硬顺序消费
        //     - 槽已占 → 直拒 `AlreadyInProgress`（reviewer 拍板：第二个
        //       live 不能排队无限等，防止两个 request 共享一个 TickOutcome
        //       搞错 Ack）
        if let Some(rr) = rr_channels {
            while let Ok(req) = rr.request_rx.try_recv() {
                if clock.now().0 > req.deadline_ns {
                    let _ = rr.response_tx.send(ClearEStopRrResponse {
                        request_id: req.request_id,
                        payload: ClearEStopRrPayload::DeadlineExceeded,
                    });
                    continue;
                }
                if req.dry_run {
                    let payload = match clear_estop_action::evaluate_clear_estop_dry_run(
                        state,
                        &guards,
                        executor_ctrl,
                        clock,
                    ) {
                        ClearEStopDryRunOutcome::WouldClear { would_clear_at_ns } => {
                            ClearEStopRrPayload::WouldClear { would_clear_at_ns }
                        }
                        ClearEStopDryRunOutcome::StateNotSafe { actual } => {
                            ClearEStopRrPayload::StateNotSafe { actual }
                        }
                        ClearEStopDryRunOutcome::NotTorqueOffConfirmed { actual } => {
                            ClearEStopRrPayload::NotTorqueOffConfirmed { actual }
                        }
                    };
                    let _ = rr.response_tx.send(ClearEStopRrResponse {
                        request_id: req.request_id,
                        payload,
                    });
                } else if in_flight_live.is_some() {
                    let _ = rr.response_tx.send(ClearEStopRrResponse {
                        request_id: req.request_id,
                        payload: ClearEStopRrPayload::AlreadyInProgress,
                    });
                } else {
                    in_flight_live = Some(req);
                    events.push(Event::ClearEStop);
                }
            }
        }

        // ─── 5.5b) SetEnabled RR drain (v0.5 候选 C，D5/D7/D8) ───
        //
        // 与 §5.5 ClearEStop drain 同构，差异（D8 多 tick）：
        // - **deadline check**（drain 入口）：过期直接 reply `DeadlineExceeded`，
        //   不入 pending / 不 push event。
        // - **dry_run**：调 `set_enabled_action::evaluate_set_enabled_dry_run`
        //   read-only 评估 → 立即 reply `WouldAccept` / `LatchBlocked` /
        //   `GuardRejected`；不进 events queue / 不进 ipc_tick（D7）。
        // - **live**：先单飞（`pending_set_enabled.is_some()` → `AlreadyInProgress`，
        //   不入 pending）；否则跑**即时 precondition gate**（同 evaluator）：
        //   `LatchBlocked` / `GuardRejected` 即时拒不入 pending（AC-4/AC-9）；
        //   `WouldAccept` 才 push `Event::Enable`/`Event::Disable` + 起 pending
        //   多 tick 观测（§6.6b resolve，D8）。dry_run 与 live 共用同一 gate，
        //   杜绝"dry-run 说能、live 却走不同判断"漂移。
        if let Some(se) = set_enabled_rr_channels {
            while let Ok(req) = se.request_rx.try_recv() {
                if clock.now().0 > req.deadline_ns {
                    let _ = se.response_tx.send(SetEnabledRrResponse {
                        request_id: req.request_id,
                        payload: SetEnabledRrPayload::DeadlineExceeded,
                    });
                    continue;
                }
                let outcome =
                    set_enabled_action::evaluate_set_enabled_dry_run(state, &guards, req.enabled);
                if req.dry_run {
                    let payload = match outcome {
                        SetEnabledDryRunOutcome::WouldAccept => SetEnabledRrPayload::WouldAccept {
                            at_ns: clock.now().0,
                        },
                        SetEnabledDryRunOutcome::LatchBlocked => SetEnabledRrPayload::LatchBlocked,
                        SetEnabledDryRunOutcome::GuardRejected { actual } => {
                            SetEnabledRrPayload::GuardRejected { actual }
                        }
                    };
                    let _ = se.response_tx.send(SetEnabledRrResponse {
                        request_id: req.request_id,
                        payload,
                    });
                } else if pending_set_enabled.is_some() {
                    let _ = se.response_tx.send(SetEnabledRrResponse {
                        request_id: req.request_id,
                        payload: SetEnabledRrPayload::AlreadyInProgress,
                    });
                } else {
                    match outcome {
                        SetEnabledDryRunOutcome::LatchBlocked => {
                            let _ = se.response_tx.send(SetEnabledRrResponse {
                                request_id: req.request_id,
                                payload: SetEnabledRrPayload::LatchBlocked,
                            });
                        }
                        SetEnabledDryRunOutcome::GuardRejected { actual } => {
                            let _ = se.response_tx.send(SetEnabledRrResponse {
                                request_id: req.request_id,
                                payload: SetEnabledRrPayload::GuardRejected { actual },
                            });
                        }
                        SetEnabledDryRunOutcome::WouldAccept
                            if !req.enabled
                                && matches!(state, S0State::Safe | S0State::Fault(_)) =>
                        {
                            // **Reverse Sync（Fault+disable 语义 A）**：disable 时
                            // 当前 state 已是 torque-off 终态（Safe / Fault(_)）→
                            // no-op 幂等 → **即时** reply `Disabled`(Ok)，**不** push
                            // `Event::Disable`、**不**建 pending、**不**清 Fault /
                            // latch、**不**动 adapter（沿 D3 (Safe/Fault,Disable)=no-op
                            // 幂等 + AC-5 disable 安全方向）。区别 Enabled→disable
                            // （effectful 真 torque-off）/ Ready→disable（ReadyHold
                            // 仍带扭，须观测到 Safe）走下方多 tick pending。
                            // `Fault(AdapterErr)` 也走本路（请求**前**就在 Fault =
                            // 幂等，非本次 ack fail；后者只可能在 Enabled→disable 的
                            // pending 观测期产生 → §6.6b 才报 AdapterError）。
                            let _ = se.response_tx.send(SetEnabledRrResponse {
                                request_id: req.request_id,
                                payload: SetEnabledRrPayload::Disabled {
                                    completed_at_ns: clock.now().0,
                                },
                            });
                        }
                        SetEnabledDryRunOutcome::WouldAccept => {
                            let (event, target) = if req.enabled {
                                (Event::Enable, PendingTarget::EnabledActive)
                            } else {
                                (Event::Disable, PendingTarget::Safe)
                            };
                            events.push(event);
                            pending_set_enabled = Some(PendingSetEnabled {
                                request_id: req.request_id,
                                deadline_ns: req.deadline_ns,
                                target,
                                requested_enabled: req.enabled,
                            });
                        }
                    }
                }
            }
        }

        // ─── 5.6) RR live preempt (A1-3 (1/3b) round 2 阻断 1+2) ───
        //
        // 在 §5.5 drain 入队后、§6 ipc_tick 之前，检查 in-flight live 是否
        // 应被 emergency preempt 或 deadline 过期取消：
        //
        // - **emergency preempt**：events queue 含 `Event::EStop` /
        //   `Event::AdapterErr(SafetyStopFailed)` → 旧 RR live 不应继续
        //   执行（emergency 之后硬件可能新 latch；客户端的 ClearEStop
        //   request 是基于 emergency 之前的状态发的）。
        // - **deadline preempt**：drain 入队后到 ipc_tick 之间窗口（典型：
        //   上一 tick 没消费 ClearEStop event，本 tick 才消费，中间隔
        //   500µs sleep）期间 deadline 过期。
        //
        // 两种情况都：(a) `EventQueue::remove_clear_estop_events()` 从
        // queue 摘掉 ClearEStop event 防 ipc_tick Step 3 误处理；
        // (b) take in_flight_live + reply `PreemptedByEmergency` /
        // `DeadlineExceeded`。
        if let Some(rr) = rr_channels
            && let Some(req) = in_flight_live.as_ref()
        {
            let now = clock.now().0;
            let preempt_payload = if events.has_emergency() {
                Some(ClearEStopRrPayload::PreemptedByEmergency)
            } else if now > req.deadline_ns {
                Some(ClearEStopRrPayload::DeadlineExceeded)
            } else {
                None
            };
            if let Some(payload) = preempt_payload {
                let removed = events.remove_clear_estop_events();
                debug_assert!(
                    removed >= 1,
                    "in_flight_live = Some 时 events queue 应至少有 1 个 ClearEStop event"
                );
                let req = in_flight_live.take().expect("just checked is Some");
                let _ = rr.response_tx.send(ClearEStopRrResponse {
                    request_id: req.request_id,
                    payload,
                });
            }
        }

        // ─── 6) ipc_tick 三步式：唯一改 state 的路径（硬要求 #2）───
        //
        // guards 现在是 &mut，ipc_tick 内部对每个 event 单独 apply 副作用。
        // **A1-2**：`latch_path` 透传给 ipc_tick，让 `ClearEStop` action
        // handler 在 ipc_tick 内完成 fsync + clear_safety_latch + 翻 guard
        // 三步硬顺序；返回 `TickOutcome` 让本主循环跳过通用 latch hook 的
        // 二次写盘。
        // **v0.5 候选 C**：SetEnabled pending 的 EStop preempt 判据须在 ipc_tick
        // drain events **之前**抓（tick 后 queue 已空）。仅 pending 在飞时看；
        // §6.6b 用它优先于 target 匹配（emergency 覆盖 disable 也落 Safe 的歧义）。
        let set_enabled_emergency_pre = pending_set_enabled.is_some() && events.has_emergency();

        let prev_state = state;
        let tick_outcome = ipc_tick(
            &mut retry,
            &mut state,
            &mut guards,
            executor_ctrl,
            clock,
            &mut events,
            cfg.latch_path.as_deref(),
        );
        if state != prev_state {
            metrics.state_changes.fetch_add(1, Ordering::Relaxed);
        }
        metrics.ipc_tick_count.fetch_add(1, Ordering::Relaxed);

        // ─── 6.5) latch fsync hook ───
        //
        // **A1-3 (1/3a)**：ClearEStop 路径已在 ipc_tick 内由
        // `clear_estop_action::try_apply_clear_estop` 完成 fsync + clear +
        // 翻 guard 三步硬顺序；本 hook 通过 `tick_outcome.clear_estop` domain
        // status enum 决定是否要补 fsync / 计数 / 同步 prev：
        //
        // - `Some(Cleared { .. })`：成功 → 计 latch_writes + 同步 prev +
        //   **跳过**通用 `prev != now` hook 的二次 `save_estop_latch`。
        // - `Some(FsyncFailed)`：gate 通过但 fsync 失败 → 计
        //   latch_write_errors；guard 没翻 → prev 不动；通用 hook 也不会
        //   触发（`guards.estop_latched` 仍为 true）。
        // - `Some(StateNotSafe { .. } | NotTorqueOffConfirmed { .. })`：
        //   gate 拒 → 不计数 / 不动盘；guard / executor / latch 全不动。
        //   RR response 在 §6.6) 段从 `in_flight_live` 取出原 request 派发
        //   `FailedPrecondition` payload。
        // - `None`：本 tick 没消费 ClearEStop → 走通用 EStop 翻转 hook
        //   （false → true 单调；保留 task #15 阻断 1 修复）。
        //
        // 设计 §6.7 不变量 #4：S0 重启不失忆，EStop / fault / 最后 safe 输出
        // 都 fsync 双写。本 hook 是 EStop / ClearEStop 两条的实施点。
        match tick_outcome.clear_estop {
            Some(ClearEStopTickOutcome::Cleared { .. }) => {
                metrics.latch_writes.fetch_add(1, Ordering::Relaxed);
                prev_estop_latched = guards.estop_latched; // = false
            }
            Some(ClearEStopTickOutcome::FsyncFailed) => {
                metrics.latch_write_errors.fetch_add(1, Ordering::Relaxed);
                // guard 没翻 → prev 不动；下方通用 hook 走不到
            }
            Some(
                ClearEStopTickOutcome::StateNotSafe { .. }
                | ClearEStopTickOutcome::NotTorqueOffConfirmed { .. },
            ) => {
                // gate 拒：guard / executor / latch 全不动；不计数
                // RR response 由 A1-3 (1/3b) 的 ipc_thread RR drain 处理
            }
            None => {
                if guards.estop_latched != prev_estop_latched {
                    // 通用 EStop 翻转 hook（false→true 单调；ClearEStop 路径
                    // 已被 outcome 接管）。`save_estop_latch` 内部双文件
                    // atomic rename + fsync，几 ms 级阻塞；EStop 是低频事件，
                    // 对 500us tick 节奏影响仅在事件发生瞬间，不持续。
                    if let Some(path) = cfg.latch_path.as_ref() {
                        match save_estop_latch(path, guards.estop_latched) {
                            Ok(()) => {
                                metrics.latch_writes.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(_e) => {
                                metrics.latch_write_errors.fetch_add(1, Ordering::Relaxed);
                                // **不**中断主循环：即便 IO 失败，state 已进
                                // Safe，executor 已 safety_stop（client 同步
                                // 置 latch + emergency_action）。重启读不到
                                // latch 是分级降级，不阻断 emergency 响应路径。
                            }
                        }
                    }
                    prev_estop_latched = guards.estop_latched;
                }
            }
        }

        // ─── 6.6) RR live response 派发 (A1-3 (1/3b)) ───
        //
        // 当本 tick 消费了 `Event::ClearEStop` 时（`tick_outcome.clear_estop
        // == Some(_)`），从 `in_flight_live` 取出原 request，把 4 variant
        // `ClearEStopTickOutcome` 显式 match 成对应 `ClearEStopRrPayload`
        // domain status，通过 response_tx 回 rr_server_thread。
        //
        // **A1-3 (1/3b) round 3 阻断 2**：派发前**不**再 check deadline。
        // deadline 只在两处拦截：
        // - §5.5) drain 入口：过期请求不入队（写盘 / clear / 翻 guard 都
        //   未发生）
        // - §5.6) preempt 段：drain 入队后到 ipc_tick 之前的窗口（同
        //   emergency preempt 共用 take + remove + reply 三件事）
        //
        // 一旦 action 在 deadline 前开始并产出 `Cleared` / `FsyncFailed` /
        // `StateNotSafe` / `NotTorqueOffConfirmed`，response 必须返**真实**
        // outcome——不再"事后用 DeadlineExceeded 覆盖已成功 / 已失败的真
        // 实结果"，避免"硬件已 clear 但客户端收 DeadlineExceeded 以为没
        // 成功" 跟 `clear_estop.fbs` "S0 端 action layer ordering" 段
        // "deadline 时三个翻转都不发生" 硬契约冲突。
        //
        // 长 fsync 跨 deadline 的极端 window 留 A1-3 (3/3) e2e 评估是否
        // 把 deadline 传进 `try_apply_clear_estop` action handler 在不可
        // 逆步骤前再 check（reviewer 拍板本轮不强求；ipc_tick 同步执行
        // ms 级，single tick 内 deadline 过期的 window 极小）。
        //
        // **invariant**：`in_flight_live.is_some()` ↔ 上一轮 RR drain 把
        // `Event::ClearEStop` push 进 events queue（live 单飞）。如果
        // `tick_outcome.clear_estop == Some(_)` 但 `in_flight_live == None`，
        // 说明事件来源不是 RR（理论上不会发生：ClearEStop 当前只通过 RR /
        // 外部 channel 入队，其它入队点都没了；这里 debug_assert 兜底）。
        if let (Some(rr), Some(status)) = (rr_channels, tick_outcome.clear_estop) {
            if let Some(req) = in_flight_live.take() {
                let payload = match status {
                    ClearEStopTickOutcome::Cleared { cleared_at_ns } => {
                        ClearEStopRrPayload::Cleared { cleared_at_ns }
                    }
                    ClearEStopTickOutcome::StateNotSafe { actual } => {
                        ClearEStopRrPayload::StateNotSafe { actual }
                    }
                    ClearEStopTickOutcome::NotTorqueOffConfirmed { actual } => {
                        ClearEStopRrPayload::NotTorqueOffConfirmed { actual }
                    }
                    ClearEStopTickOutcome::FsyncFailed => ClearEStopRrPayload::FsyncFailed,
                };
                let _ = rr.response_tx.send(ClearEStopRrResponse {
                    request_id: req.request_id,
                    payload,
                });
            } else {
                debug_assert!(
                    false,
                    "tick_outcome.clear_estop = Some(_) but in_flight_live = None; \
                     ClearEStop event 入队来源应只有 RR drain"
                );
            }
        }

        // ─── 6.6b) SetEnabled pending resolve (v0.5 候选 C，D8) ───
        //
        // ipc_tick 后读 `state` 决定 pending 是否 resolve（多 tick 观测）。优先级：
        // 1. **EStop preempt**（`set_enabled_emergency_pre`，tick 前抓）→
        //    `PreemptedByEStop`（优先于 target：disable 也落 Safe，emergency 须先赢）。
        // 2. **Stopping**（`Event::Stop`）→ `PreemptedByShutdown`。
        // 3. **`Fault(AdapterErr)`**（effectful `Enable(*)` ack fail / 1s timeout，
        //    `ipc_tick.rs`）→ `AdapterError`（AC-2c/AC-3b）。enable 路径上唯一可能
        //    的 fault 即 AdapterErr（Ready 态无其它 →Fault 边）；非 AdapterErr 的
        //    病态 fault 由 deadline 兜底。
        // 4. **到 target**（`EnabledActive` / `Safe`）→ `Enabled` / `Disabled`
        //    （`completed_at_ns = now`，AC-2/AC-3）。
        // 5. **deadline 过期** → `DeadlineExceeded`（**不**回滚已发副作用，D8/AC-2b）。
        // 6. 否则继续观测（下一 tick）。
        // resolve 后 `pending_set_enabled = None`（释放单飞）。
        let se_resolve: Option<(String, SetEnabledRrPayload)> =
            if let Some(pending) = pending_set_enabled.as_ref() {
                let now = clock.now().0;
                let reached_target = match pending.target {
                    PendingTarget::EnabledActive => matches!(
                        state,
                        S0State::Enabled {
                            sub: EnabledSub::Active,
                            ..
                        }
                    ),
                    PendingTarget::Safe => state == S0State::Safe,
                };
                let payload = if set_enabled_emergency_pre {
                    Some(SetEnabledRrPayload::PreemptedByEStop)
                } else if state == S0State::Stopping {
                    Some(SetEnabledRrPayload::PreemptedByShutdown)
                } else if state == S0State::Fault(FaultCode::AdapterErr) {
                    Some(SetEnabledRrPayload::AdapterError)
                } else if reached_target {
                    Some(match pending.target {
                        PendingTarget::EnabledActive => SetEnabledRrPayload::Enabled {
                            completed_at_ns: now,
                        },
                        PendingTarget::Safe => SetEnabledRrPayload::Disabled {
                            completed_at_ns: now,
                        },
                    })
                } else if now > pending.deadline_ns {
                    Some(SetEnabledRrPayload::DeadlineExceeded)
                } else {
                    None
                };
                payload.map(|p| (pending.request_id.clone(), p))
            } else {
                None
            };
        if let Some((request_id, payload)) = se_resolve {
            pending_set_enabled = None;
            if let Some(se) = set_enabled_rr_channels {
                let _ = se.response_tx.send(SetEnabledRrResponse {
                    request_id,
                    payload,
                });
            }
        }

        // ─── 7) 发布 snapshot ───
        state_pub.publish(&S0StateSnapshot::new(state, guards, clock.now().0));
        metrics.tick_count.fetch_add(1, Ordering::Relaxed);

        // 非 RT 线程：std::thread::sleep 够用。
        thread::sleep(Duration::from_nanos(cfg.poll_interval_ns));
    }
}

// ============================================================================
// Topic poll helpers（把 iceoryx2 lease → typed body → 业务动作）
// ============================================================================

fn poll_command(
    cmd_sub: &mut Iceoryx2Subscriber<markers::JointCommand>,
    cmd_writer: &LatestCommandWriter<JointCommandBuf>,
    authoritative_layout: u64,
    metrics: &IpcThreadMetrics,
) {
    let Ok(Some(lease)) = cmd_sub.latest() else {
        return;
    };
    let Ok(view) = lease.view() else {
        metrics.command_parse_errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let fb_cmd: FbJointCommand<'_> = view.body.get();
    // v0.5 候选 E 段(3)：command 侧 layout gate（D1，§3.5.1 部分落地）——
    // 错 layout 帧**丢弃不写 slot**（防错位命令驱动错关节集）。warn 限频 =
    // 首条必报 + 每 64 条一报（无既有 every-N 先例，本处定此规则）；计数
    // 不限频。
    if fb_cmd.layout_id() != authoritative_layout {
        let n = metrics.unknown_layout_count.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n.is_multiple_of(64) {
            tracing::warn!(
                got = format_args!("{:#018x}", fb_cmd.layout_id()),
                want = format_args!("{authoritative_layout:#018x}"),
                dropped_total = n,
                "JointCommand layout_id mismatch; dropped (not written to slot)"
            );
        }
        return;
    }
    let stamp_ns = view.header.stamp_ns();
    let buf = parse_joint_command(&fb_cmd, stamp_ns);
    cmd_writer.store(buf);
    metrics.commands_applied.fetch_add(1, Ordering::Relaxed);
}

fn poll_heartbeat(
    hb_sub: &mut Iceoryx2Subscriber<markers::Heartbeat>,
    clock: &dyn RobotClock,
    last_hb_ns: &AtomicU64,
    metrics: &IpcThreadMetrics,
) -> bool {
    let Ok(Some(lease)) = hb_sub.latest() else {
        return false;
    };
    // Heartbeat body 解析失败也算"收到过"—— payload 解码失败是网关 / schema 层问题，
    // 不影响 heartbeat 存活信号。但计到 parse_errors 方便诊断。
    if lease.view().is_ok() {
        metrics.heartbeats_received.fetch_add(1, Ordering::Relaxed);
        last_hb_ns.store(clock.now().0, Ordering::Release);
        true
    } else {
        metrics
            .heartbeat_parse_errors
            .fetch_add(1, Ordering::Relaxed);
        false
    }
}

fn poll_estop(
    estop_sub: &mut Iceoryx2Subscriber<markers::EStopEvent>,
    metrics: &IpcThreadMetrics,
) -> bool {
    let Ok(Some(lease)) = estop_sub.latest() else {
        return false;
    };
    if lease.view().is_ok() {
        metrics.estops_received.fetch_add(1, Ordering::Relaxed);
        true
    } else {
        metrics.estop_parse_errors.fetch_add(1, Ordering::Relaxed);
        false
    }
}

// ============================================================================
// FlatBuffers JointCommand → JointCommandBuf POD
// ============================================================================

/// `FlatBuffers` `JointCommand` → POD [`JointCommandBuf`]。
///
/// f64 → f32 的 narrowing cast：`docs/00_design.md` §3.5 明确底层 adapter 用
/// f32（HardRT 寄存器精度），网关 schema 用 f64 统一公开接口。精度损失对关节
/// 控制不影响（位置误差 < 1e-7 rad）。
fn parse_joint_command(fb: &FbJointCommand<'_>, stamp_ns: u64) -> JointCommandBuf {
    let mut buf = JointCommandBuf::default();

    let target = fb.target();
    let n = target.map_or(0, |v| v.len().min(MAX_JOINTS));
    #[allow(clippy::cast_possible_truncation)]
    {
        buf.joint_count = n as u8;
    }
    if let Some(v) = target {
        for i in 0..n {
            #[allow(clippy::cast_possible_truncation)]
            {
                buf.positions[i] = v.get(i) as f32;
            }
        }
    }
    buf.mode = fb_control_mode_to_repr(fb.mode());
    buf.stamp_ns = stamp_ns;
    buf.deadline_ns = fb.deadline_ns();
    buf.seq = fb.source_task_id();
    buf
}

/// FB `ControlMode` → `ControlModeRepr`。FB 生成的 enum 是独立类型，需要显式映射。
fn fb_control_mode_to_repr(m: FbControlMode) -> ControlModeRepr {
    match m {
        FbControlMode::Position => ControlModeRepr::Position,
        FbControlMode::Velocity => ControlModeRepr::Velocity,
        FbControlMode::Torque => ControlModeRepr::Torque,
        FbControlMode::ServoPosition => ControlModeRepr::ServoPosition,
        // FB 的 enum 允许未来扩展；未识别值保守 Stop（S0 在 Stop 模式下不驱动电机）。
        _ => ControlModeRepr::Stop,
    }
}

// ============================================================================
// Tests（同进程 iceoryx2 pub/sub，所有跨线程限制通过 factory closure 绕开）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use iceoryx2::prelude::NodeBuilder;
    use iceoryx2::service::ipc::Service as IxService;
    use robot_core::adapter::ControlMode as AdapterControlMode;
    use robot_core::time::{MockClock, MonoRawNs, SystemClock};
    use robot_msg::build_helpers::{build_estop_event, build_heartbeat, build_joint_command};
    use robot_msg::msg::StaleAction;
    use robot_transport::iceoryx2::{TopicClass, publisher, subscriber};
    use robot_transport::traits::Publisher;
    use robot_transport::{LatestCommandSlot, RtSnapshotReader};
    use std::sync::atomic::AtomicU32;

    // 每个测试用独立 topic 前缀，避免 cargo test 并行执行时 iceoryx2 service 名冲突。
    static TEST_SEQ: AtomicU32 = AtomicU32::new(0);

    struct Topics {
        command: String,
        heartbeat: String,
        estop: String,
    }

    fn fresh_topics() -> Topics {
        let id = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let base = format!("/test_ipc_{}_{}", std::process::id(), id);
        Topics {
            command: format!("{base}/cmd"),
            heartbeat: format!("{base}/hb"),
            estop: format!("{base}/estop"),
        }
    }

    fn healthy_guards() -> Guards {
        Guards {
            estop_latched: false,
            adapter_connected: true,
            calibration_loaded: true,
            joint_state_valid: true,
            power_ok: true,
            heartbeat_ok: false, // 等 ipc 收到 heartbeat 再变 true
            cmd_deadline_valid: true,
            limits_valid: true,
            hw_enable_ok: true,
            adapter_fault_cleared: false,
            command_mode: AdapterControlMode::Position,
            on_timeout_hint: crate::state_machine::EnabledSub::CtrlLostHold,
        }
    }

    fn enabled_active() -> S0State {
        S0State::Enabled {
            sub: crate::state_machine::EnabledSub::Active,
            control_link: crate::state_machine::ControlLink::Ok,
            output_mode: crate::state_machine::OutputMode::Active,
        }
    }

    /// 等待闭包返回 true，最长 `max`；否则 panic。用于等异步 tick 产生观察结果。
    fn wait_until<F: FnMut() -> bool>(mut cond: F, max: Duration) {
        let start = std::time::Instant::now();
        while start.elapsed() < max {
            if cond() {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        panic!("wait_until timed out after {max:?}");
    }

    /// task-30 v9 / M7 阶段 4 测试辅助：把 `bring_up` 返回的 `Arc<HardRtCtrlInner>`
    /// 包成一个**自动应答**线程 —— 把 `pending_normal` swap 出来后立即
    /// store `last_completed = (seq, AdapterResp::Ok)`。
    ///
    /// 用途：测试只关心 `ipc_thread` 通路，不想真起 `rt_thread`；但
    /// `Ready → Enabled.Active` 是 Effectful `Enable(true)`，必须 executor 端
    /// ack 才能 commit。本 helper 提供 ack。
    fn spawn_auto_ack(
        inner: Arc<crate::hard_rt_ctrl::HardRtCtrlInner>,
    ) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        use robot_core::executor_ctrl::{AdapterResp, pack_completed, unpack_pending_normal};
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let h = std::thread::spawn(move || {
            while !stop_clone.load(Ordering::Acquire) {
                let p = inner.pending_normal.swap(0, Ordering::AcqRel);
                if p != 0 {
                    let (_, _, seq) = unpack_pending_normal(p);
                    inner
                        .last_completed
                        .store(pack_completed(seq, AdapterResp::Ok), Ordering::Release);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        (stop, h)
    }

    /// 构造 `ipc_thread` + 配套的 iceoryx2 publishers（在调用方线程里存活，用于发数据）。
    ///
    /// 关键点：subscribers 在 `ipc_thread` 内部 open；publishers 在测试主线程 open。
    /// iceoryx2 service 首次调用 create，后续 open。两端 Node 不同但 service 共享。
    #[allow(clippy::type_complexity)]
    fn bring_up(
        initial_state: S0State,
        initial_guards: Guards,
        watchdog_rx: Receiver<Event>,
        clock: Arc<dyn RobotClock>,
    ) -> (
        IpcThreadHandle,
        robot_transport::LatestCommandReader<JointCommandBuf>,
        RtSnapshotReader<S0StateSnapshot>,
        robot_transport::iceoryx2::Iceoryx2Publisher<markers::JointCommand>,
        robot_transport::iceoryx2::Iceoryx2Publisher<markers::Heartbeat>,
        robot_transport::iceoryx2::Iceoryx2Publisher<markers::EStopEvent>,
        Arc<crate::hard_rt_ctrl::HardRtCtrlInner>,
    ) {
        bring_up_with_latch(initial_state, initial_guards, watchdog_rx, clock, None)
    }

    /// `bring_up` 的扩展形态：可注入 `latch_path`，让单测覆盖"`EStop` 触发即
    /// `fsync` 落盘"链路。daemon 内嵌 `ipc_thread` 用这条 hook（`Some(path)`）；
    /// 其它纯通路单测用 `None` 跳过 IO。
    #[allow(clippy::type_complexity)]
    fn bring_up_with_latch(
        initial_state: S0State,
        initial_guards: Guards,
        watchdog_rx: Receiver<Event>,
        clock: Arc<dyn RobotClock>,
        latch_path: Option<std::path::PathBuf>,
    ) -> (
        IpcThreadHandle,
        robot_transport::LatestCommandReader<JointCommandBuf>,
        RtSnapshotReader<S0StateSnapshot>,
        robot_transport::iceoryx2::Iceoryx2Publisher<markers::JointCommand>,
        robot_transport::iceoryx2::Iceoryx2Publisher<markers::Heartbeat>,
        robot_transport::iceoryx2::Iceoryx2Publisher<markers::EStopEvent>,
        Arc<crate::hard_rt_ctrl::HardRtCtrlInner>,
    ) {
        let topics = fresh_topics();
        // publisher-side Node，测试线程持有。leak 到 'static 避免 drop 后 service
        // 被 iceoryx2 回收（ipc_thread 还要继续 open service）；测试进程 staci 可接受。
        let pub_node: &'static _ =
            Box::leak(Box::new(NodeBuilder::new().create::<IxService>().unwrap()));
        // 先 open publisher 端（会 create service）。
        let cmd_pub =
            publisher::<markers::JointCommand>(pub_node, &topics.command, TopicClass::S0Command)
                .unwrap();
        let hb_pub =
            publisher::<markers::Heartbeat>(pub_node, &topics.heartbeat, TopicClass::Heartbeat)
                .unwrap();
        let estop_pub =
            publisher::<markers::EStopEvent>(pub_node, &topics.estop, TopicClass::SafetyEvent)
                .unwrap();

        let (cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let reader_clone = cmd_reader.clone();
        let (state_pub, state_rdr) =
            RtSnapshotPublisher::<S0StateSnapshot>::new(S0StateSnapshot::default());

        let cfg = IpcThreadConfig {
            poll_interval_ns: 500_000,
            initial_guards,
            initial_state,
            latch_path,
            // 段(3)：与 publish_command fixture layout（0xCAFE_0001）对齐。
            authoritative_layout: 0xCAFE_0001,
        };

        let topics_for_factory = Topics {
            command: topics.command.clone(),
            heartbeat: topics.heartbeat.clone(),
            estop: topics.estop.clone(),
        };
        let factory = move || {
            // ipc_thread 内部的 Node —— iceoryx2 ipc_policy::SingleThreaded 要求
            // 每线程独立 Node。两端的 Node 指向同一个 iceoryx2 service（topic +
            // schema attribute 匹配时 open_or_create 合并到同一 service）。
            //
            // sub_node 需要活得跟 subscribers 一样久（subscriber 持有 Node 内的
            // shm handle 句柄）。factory 结束后 node 会 drop，因此 leak 到 'static。
            // 生产 s0_daemon 里 Node 由 main 持有，无需 leak。
            let sub_node = NodeBuilder::new().create::<IxService>().unwrap();
            let command = subscriber::<markers::JointCommand>(
                &sub_node,
                &topics_for_factory.command,
                TopicClass::S0Command,
            )
            .unwrap();
            let heartbeat = subscriber::<markers::Heartbeat>(
                &sub_node,
                &topics_for_factory.heartbeat,
                TopicClass::Heartbeat,
            )
            .unwrap();
            let estop = subscriber::<markers::EStopEvent>(
                &sub_node,
                &topics_for_factory.estop,
                TopicClass::SafetyEvent,
            )
            .unwrap();
            Box::leak(Box::new(sub_node));
            IpcSubscribers {
                command,
                heartbeat,
                estop,
            }
        };

        // task-30 v9 / M7 阶段 4 测试桩：注入一份默认 HardRtCtrlClient（Inner
        // 默认值即可）。本模块单测重点是 ipc_thread 的"事件 → guards → 入队 →
        // ipc_tick → snapshot 发布"通路；executor_ctrl 端的具体语义由
        // hard_rt_ctrl.rs / ipc_tick.rs 自己的单测覆盖。inner 暴露给测试，
        // 测试侧按需 spawn_auto_ack 模拟 rt_thread ack。
        let inner = Arc::new(crate::hard_rt_ctrl::HardRtCtrlInner::new());
        let inner_clone = Arc::clone(&inner);
        let executor_ctrl: Arc<dyn ExecutorCtrlClient> =
            Arc::new(crate::hard_rt_ctrl::HardRtCtrlClient::new(inner_clone));

        let handle = spawn_ipc_thread(
            cfg,
            clock,
            factory,
            cmd_writer,
            state_pub,
            watchdog_rx,
            executor_ctrl,
            // 默认 mod tests 不接 RR control plane；RR 单测自己 spawn 时
            // 显式传 `Some(...)`。
            None,
            // SetEnabled RR control plane（v0.5 候选 C）：默认 helper 不接。
            None,
        )
        .unwrap();

        (
            handle,
            reader_clone,
            state_rdr,
            cmd_pub,
            hb_pub,
            estop_pub,
            inner,
        )
    }

    fn publish_heartbeat(
        pub_: &mut robot_transport::iceoryx2::Iceoryx2Publisher<markers::Heartbeat>,
        seq: u64,
    ) {
        let body = build_heartbeat(2, seq, 50.0, 1_000_000);
        let mut w = pub_.loan(1024 + body.len()).unwrap();
        w.set_seq(seq);
        w.set_source_id(2);
        w.set_stamp_ns(1_000_000);
        w.set_body_bytes(&body);
        w.publish().unwrap();
    }

    fn publish_command(
        pub_: &mut robot_transport::iceoryx2::Iceoryx2Publisher<markers::JointCommand>,
        seq: u64,
        target: &[f64],
    ) {
        publish_command_with_layout(pub_, seq, target, 0xCAFE_0001);
    }

    /// 段(3) layout gate 测试用：显式 layout 变体。
    fn publish_command_with_layout(
        pub_: &mut robot_transport::iceoryx2::Iceoryx2Publisher<markers::JointCommand>,
        seq: u64,
        target: &[f64],
        layout_id: u64,
    ) {
        let body = build_joint_command(
            layout_id,
            robot_msg::msg::ControlMode::Position,
            target,
            999_999_999_999,
            10,
            StaleAction::Hold,
            seq,
        );
        let mut w = pub_.loan(1024 + body.len()).unwrap();
        w.set_seq(seq);
        w.set_source_id(2);
        w.set_stamp_ns(seq * 1_000_000);
        w.set_body_bytes(&body);
        w.publish().unwrap();
    }

    fn publish_estop(
        pub_: &mut robot_transport::iceoryx2::Iceoryx2Publisher<markers::EStopEvent>,
        stamp_ns: u64,
    ) {
        let body = build_estop_event("test-driver", stamp_ns, "rpc", true);
        let mut w = pub_.loan(1024 + body.len()).unwrap();
        w.set_stamp_ns(stamp_ns);
        w.set_body_bytes(&body);
        w.publish().unwrap();
    }

    /// 最小冒烟：spawn + 立即 stop，验证不 panic、线程干净退出。
    #[test]
    fn spawn_then_stop_cleanly() {
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let (_wd_tx, wd_rx) = unbounded();
        let (handle, _cmd_reader, _state_rdr, _cp, _hp, _ep, _inner) =
            bring_up(S0State::Safe, healthy_guards(), wd_rx, clock);
        thread::sleep(Duration::from_millis(30));
        handle.request_stop();
        handle.join().unwrap();
    }

    /// 收到 `JointCommand` → `LatestCommandWriter` 里出现对应 POD。
    #[test]
    fn command_plumbing_writes_pod_buffer() {
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let (_wd_tx, wd_rx) = unbounded();
        let (handle, cmd_reader, _state_rdr, mut cmd_pub, _hp, _ep, _inner) =
            bring_up(S0State::Safe, healthy_guards(), wd_rx, clock);

        publish_command(&mut cmd_pub, 7, &[0.1, 0.2, 0.3]);

        wait_until(
            || {
                let g = cmd_reader.load();
                g.joint_count == 3 && (g.positions[0] - 0.1).abs() < 1e-3
            },
            Duration::from_secs(2),
        );

        let loaded = cmd_reader.load();
        assert_eq!(loaded.joint_count, 3);
        assert!((loaded.positions[0] - 0.1).abs() < 1e-3);
        assert!((loaded.positions[1] - 0.2).abs() < 1e-3);
        assert!((loaded.positions[2] - 0.3).abs() < 1e-3);
        assert_eq!(loaded.mode, ControlModeRepr::Position);
        assert_eq!(loaded.seq, 7);

        handle.request_stop();
        handle.join().unwrap();
    }

    /// 段(3) AC-E2 unit：mismatch 帧 → **slot 不变** + `unknown_layout_count`
    /// 恰 +1（计数断言仅本 unit，R3-B4）。
    #[test]
    fn command_layout_mismatch_dropped_not_written() {
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let (_wd_tx, wd_rx) = unbounded();
        let (handle, cmd_reader, _state_rdr, mut cmd_pub, _hp, _ep, _inner) =
            bring_up(S0State::Safe, healthy_guards(), wd_rx, clock);

        publish_command_with_layout(&mut cmd_pub, 9, &[0.9], 0xDEAD_0002);

        wait_until(
            || {
                handle
                    .metrics()
                    .unknown_layout_count
                    .load(Ordering::Relaxed)
                    == 1
            },
            Duration::from_secs(2),
        );

        let loaded = cmd_reader.load();
        assert_eq!(
            loaded.seq, 0,
            "mismatch 帧不得写 slot（应保持初值 default）"
        );
        assert_eq!(loaded.joint_count, 0);
        assert_eq!(
            handle
                .metrics()
                .unknown_layout_count
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(handle.metrics().commands_applied.load(Ordering::Relaxed), 0);

        handle.request_stop();
        handle.join().unwrap();
    }

    /// 段(3) 对照：match 帧照常写 slot（gate 放行路径回归）。
    #[test]
    fn command_layout_match_written() {
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let (_wd_tx, wd_rx) = unbounded();
        let (handle, cmd_reader, _state_rdr, mut cmd_pub, _hp, _ep, _inner) =
            bring_up(S0State::Safe, healthy_guards(), wd_rx, clock);

        publish_command_with_layout(&mut cmd_pub, 11, &[0.5, 0.6], 0xCAFE_0001);

        wait_until(|| cmd_reader.load().seq == 11, Duration::from_secs(2));

        let loaded = cmd_reader.load();
        assert_eq!(loaded.joint_count, 2);
        assert_eq!(
            handle
                .metrics()
                .unknown_layout_count
                .load(Ordering::Relaxed),
            0
        );

        handle.request_stop();
        handle.join().unwrap();
    }

    /// heartbeat 到达 → state 从 `READY` 推进到 `ENABLED.Active`；`last_hb_ns` 被更新。
    ///
    /// task-30 v9：READY → ENABLED.Active 是 Effectful Enable(true)，需要
    /// executor ack。本测试用 [`spawn_auto_ack`] 模拟 `rt_thread` ack 通路。
    #[test]
    fn heartbeat_transitions_state_and_updates_watchdog_atom() {
        let clock = Arc::new(MockClock::new(MonoRawNs(100_000_000)));
        let clock_dyn: Arc<dyn RobotClock> = Arc::clone(&clock) as Arc<dyn RobotClock>;
        let (_wd_tx, wd_rx) = unbounded();
        let (handle, _cmd_reader, state_rdr, _cp, mut hb_pub, _ep, inner) =
            bring_up(S0State::Ready, healthy_guards(), wd_rx, clock_dyn);
        let (ack_stop, ack_join) = spawn_auto_ack(Arc::clone(&inner));

        publish_heartbeat(&mut hb_pub, 1);

        let last_hb = handle.last_hb_ns();
        wait_until(
            || state_rdr.load().state == enabled_active() && last_hb.load(Ordering::Acquire) > 0,
            Duration::from_secs(2),
        );

        let snap = state_rdr.load();
        assert_eq!(snap.state, enabled_active());
        assert!(snap.guards_summary.heartbeat_ok);
        assert_eq!(last_hb.load(Ordering::Acquire), clock.now().0);

        handle.request_stop();
        handle.join().unwrap();
        ack_stop.store(true, Ordering::Release);
        ack_join.join().unwrap();
    }

    /// `watchdog` 发 `HeartbeatTimeout` → `Active` 态退到 `CONTROL_LOST_*`（hold）。
    #[test]
    fn watchdog_timeout_drops_to_control_lost() {
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let (wd_tx, wd_rx) = unbounded();
        let mut initial = healthy_guards();
        initial.heartbeat_ok = true;
        let (handle, _cmd_reader, state_rdr, _cp, _hp, _ep, _inner) =
            bring_up(enabled_active(), initial, wd_rx, clock);

        wd_tx.send(Event::HeartbeatTimeout).unwrap();

        wait_until(
            || {
                matches!(
                    state_rdr.load().state,
                    S0State::Enabled {
                        control_link: crate::state_machine::ControlLink::Lost,
                        ..
                    }
                )
            },
            Duration::from_secs(2),
        );

        let snap = state_rdr.load();
        assert!(matches!(
            snap.state,
            S0State::Enabled {
                sub: crate::state_machine::EnabledSub::CtrlLostHold,
                control_link: crate::state_machine::ControlLink::Lost,
                output_mode: crate::state_machine::OutputMode::Hold,
            }
        ));
        assert!(!snap.guards_summary.heartbeat_ok);

        handle.request_stop();
        handle.join().unwrap();
    }

    /// `EStop` topic → 任意态 → `SAFE`；guards.`estop_latched` 置位。
    #[test]
    fn estop_topic_drops_to_safe() {
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let (_wd_tx, wd_rx) = unbounded();
        let mut initial = healthy_guards();
        initial.heartbeat_ok = true;
        let (handle, _cmd_reader, state_rdr, _cp, _hp, mut estop_pub, _inner) =
            bring_up(enabled_active(), initial, wd_rx, clock);

        publish_estop(&mut estop_pub, 42_000_000);

        wait_until(
            || state_rdr.load().state == S0State::Safe,
            Duration::from_secs(2),
        );

        let snap = state_rdr.load();
        assert_eq!(snap.state, S0State::Safe);
        assert!(snap.guards_summary.estop_latched);

        handle.request_stop();
        handle.join().unwrap();
    }

    /// **task #15 阻断 1 锁测**：`EStop` topic 进来后，**daemon 仍在运行
    /// 时**（不依赖 graceful shutdown）latch 文件已为 `1` 落盘双写。
    ///
    /// 旧实现 latch 仅在 `s0_daemon` graceful shutdown 路径写一次；daemon
    /// 在 graceful 退出前崩溃时 latch 落不下来，重启读到 false → 自动
    /// `Event::Enable` → 安全语义破坏。本测试钉死 "`EStop` 触发即 `fsync`
    /// 落盘" 不变量：handle 仍 active 时文件已存在 + 内容 = `1\n`，bak 同。
    ///
    /// 关键时序：
    /// 1. publish `EStop` topic
    /// 2. `wait_until` daemon `state=Safe` + `guards_summary.estop_latched=true`
    ///    （也即 `ipc_tick` emergency drain 已处理）
    /// 3. **不**调用 `handle.request_stop` / `handle.join`
    /// 4. 直接读 `latch_path` / `latch_path.bak` 文件 → 必须存在 + 内容 = `1`
    /// 5. 然后才 stop daemon（验证关闭无副作用）
    #[test]
    fn estop_topic_writes_latch_to_disk_immediately() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");
        let bak_path = tmp.path().join("estop_latch.bak");

        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let (_wd_tx, wd_rx) = unbounded();
        let mut initial = healthy_guards();
        initial.heartbeat_ok = true;
        // 关键：把 latch_path 注入 ipc_thread —— daemon 现实场景同款。
        let (handle, _cmd_reader, state_rdr, _cp, _hp, mut estop_pub, _inner) = bring_up_with_latch(
            enabled_active(),
            initial,
            wd_rx,
            clock,
            Some(latch_path.clone()),
        );

        // 触发前确认：latch 文件**不存在**（baseline；ipc_thread 启动期初值
        // estop_latched=false，prev_estop_latched=false，没有 false→? 翻转，
        // 所以不写）。
        assert!(
            !latch_path.exists(),
            "baseline: latch file should not exist before EStop"
        );

        publish_estop(&mut estop_pub, 42_000_000);

        // 等 ipc_tick emergency drain 完成（state=Safe + guard 翻转）
        wait_until(
            || {
                let snap = state_rdr.load();
                snap.state == S0State::Safe && snap.guards_summary.estop_latched
            },
            Duration::from_secs(2),
        );

        // 等 ipc_thread 主循环 §6.5 hook 把 latch 写盘。`save_estop_latch`
        // 双文件 atomic rename + fsync，几 ms 级；用 wait_until 防 IO 抖动。
        wait_until(
            || latch_path.exists() && bak_path.exists(),
            Duration::from_secs(2),
        );

        // **关键断言（daemon 仍 active 时）**：两份文件都已落盘 + 内容是 `1`。
        let content = fs::read_to_string(&latch_path).expect("read latch_path");
        assert_eq!(
            content.trim_end(),
            "1",
            "latch_path content (daemon still alive): {content:?}"
        );
        let bak_content = fs::read_to_string(&bak_path).expect("read latch_path.bak");
        assert_eq!(
            bak_content.trim_end(),
            "1",
            "latch_path.bak content (daemon still alive): {bak_content:?}"
        );

        // 同时验 metrics 计数：至少 1 次成功写盘，0 次错误
        let m = handle.metrics();
        assert!(
            m.latch_writes.load(Ordering::Relaxed) >= 1,
            "metrics.latch_writes should be >= 1 after EStop"
        );
        assert_eq!(
            m.latch_write_errors.load(Ordering::Relaxed),
            0,
            "metrics.latch_write_errors should be 0 (tmpdir always writable)"
        );

        // 之后才停 daemon —— 验证 stop 路径无副作用
        handle.request_stop();
        handle.join().unwrap();
    }

    /// 外部事件 channel：注入 `Event::Stop` → 进 `STOPPING`。
    #[test]
    fn external_stop_event_transitions_to_stopping() {
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let (_wd_tx, wd_rx) = unbounded();
        let (handle, _cmd_reader, state_rdr, _cp, _hp, _ep, _inner) =
            bring_up(S0State::Ready, healthy_guards(), wd_rx, clock);

        handle.event_sender().send(Event::Stop).unwrap();

        wait_until(
            || state_rdr.load().state == S0State::Stopping,
            Duration::from_secs(2),
        );

        assert_eq!(state_rdr.load().state, S0State::Stopping);

        handle.request_stop();
        handle.join().unwrap();
    }

    /// **A2 freeze hook 单测**：设 `freeze_until_ns` 后，`ipc_thread` 的
    /// `tick_count` 在 spin 期间不增长；过期后恢复 →
    /// [`crate::systemd_notify::LivenessObserver`] 看到停滞，watchdog 主循环
    /// 跳 `sd_notify(WATCHDOG=1)`。详 [`crate::freeze_hook`] 模块 doc。
    #[test]
    fn freeze_hook_holds_tick_count() {
        let sys_clock = SystemClock::new();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let (_wd_tx, wd_rx) = unbounded();
        let (handle, _cmd_reader, _state_rdr, _cp, _hp, _ep, _inner) =
            bring_up(S0State::Safe, healthy_guards(), wd_rx, clock);

        let metrics = Arc::clone(handle.metrics());
        // 等 worker 跑起来
        wait_until(
            || metrics.tick_count.load(Ordering::Relaxed) >= 5,
            Duration::from_secs(2),
        );
        let pre_freeze = metrics.tick_count.load(Ordering::Relaxed);
        assert!(pre_freeze >= 5, "warmup 失败 tick={pre_freeze}");

        // 设 freeze 80ms
        let until = sys_clock.now().0 + 80_000_000;
        metrics.freeze_until_ns.store(until, Ordering::Release);

        // 等 freeze 中段（40ms）
        std::thread::sleep(Duration::from_millis(40));
        let mid_freeze = metrics.tick_count.load(Ordering::Relaxed);
        let mid_grew = mid_freeze.saturating_sub(pre_freeze);
        assert!(
            mid_grew <= 1,
            "freeze 期间 tick_count 不应继续增长：pre={pre_freeze} mid={mid_freeze} 增 {mid_grew}",
        );

        // 等 freeze 过期 + tick 恢复
        std::thread::sleep(Duration::from_millis(100));
        let post_freeze = metrics.tick_count.load(Ordering::Relaxed);
        assert!(
            post_freeze > mid_freeze + 5,
            "freeze 过期后 tick_count 应当恢复增长：mid={mid_freeze} post={post_freeze}",
        );

        handle.request_stop();
        handle.join().unwrap();
    }

    // ========================================================================
    // A1-3 (1/3b) RR control plane 单测
    //
    // 端到端：测试线程通过 rr_request_tx 发 ClearEStopRrRequest →
    // ipc_thread RR drain → (dry_run 同步评估 / live 入 events queue +
    // ipc_tick 三步硬顺序) → response_tx → 测试线程 rr_response_rx 收
    // ClearEStopRrResponse。
    // ========================================================================

    /// 起 ipc_thread + 配套 RR control plane channels，预设
    /// `safety_state` / `safety_latched` 让单测覆盖 dry_run / live / gate
    /// 拒等多种路径。
    ///
    /// 返回值含 `clock` + `metrics` 让 deadline / freeze hook 单测精确
    /// 控制时序。
    #[allow(clippy::type_complexity)]
    fn bring_up_with_rr(
        initial_state: S0State,
        initial_guards: Guards,
        latch_path: Option<std::path::PathBuf>,
        safety_state: OutputSafetyState,
        safety_latched: bool,
    ) -> (
        IpcThreadHandle,
        Arc<crate::hard_rt_ctrl::HardRtCtrlInner>,
        crossbeam_channel::Sender<ClearEStopRrRequest>,
        crossbeam_channel::Receiver<ClearEStopRrResponse>,
        Arc<dyn RobotClock>,
    ) {
        use crossbeam_channel::bounded;

        let topics = fresh_topics();
        let pub_node: &'static _ =
            Box::leak(Box::new(NodeBuilder::new().create::<IxService>().unwrap()));
        let _cmd_pub =
            publisher::<markers::JointCommand>(pub_node, &topics.command, TopicClass::S0Command)
                .expect("open command publisher");
        let _hb_pub =
            publisher::<markers::Heartbeat>(pub_node, &topics.heartbeat, TopicClass::Heartbeat)
                .expect("open heartbeat publisher");
        let _estop_pub =
            publisher::<markers::EStopEvent>(pub_node, &topics.estop, TopicClass::SafetyEvent)
                .expect("open estop publisher");

        let (cmd_writer, _cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (state_pub, _state_rdr) =
            RtSnapshotPublisher::<S0StateSnapshot>::new(S0StateSnapshot::default());
        let (_wd_tx, wd_rx) = unbounded::<Event>();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let clock_for_test = Arc::clone(&clock);

        let topics_for_factory = topics;
        let factory = move || {
            let sub_node = NodeBuilder::new().create::<IxService>().unwrap();
            let command = subscriber::<markers::JointCommand>(
                &sub_node,
                &topics_for_factory.command,
                TopicClass::S0Command,
            )
            .unwrap();
            let heartbeat = subscriber::<markers::Heartbeat>(
                &sub_node,
                &topics_for_factory.heartbeat,
                TopicClass::Heartbeat,
            )
            .unwrap();
            let estop = subscriber::<markers::EStopEvent>(
                &sub_node,
                &topics_for_factory.estop,
                TopicClass::SafetyEvent,
            )
            .unwrap();
            Box::leak(Box::new(sub_node));
            IpcSubscribers {
                command,
                heartbeat,
                estop,
            }
        };

        let inner = Arc::new(crate::hard_rt_ctrl::HardRtCtrlInner::new());
        inner
            .safety_state
            .store(safety_state.to_u8(), Ordering::Release);
        inner
            .safety_latched
            .store(safety_latched, Ordering::Release);
        let executor_ctrl: Arc<dyn ExecutorCtrlClient> = Arc::new(
            crate::hard_rt_ctrl::HardRtCtrlClient::new(Arc::clone(&inner)),
        );

        let cfg = IpcThreadConfig {
            poll_interval_ns: 500_000,
            initial_guards,
            initial_state,
            latch_path,
            // 段(3)：与 publish_command fixture layout（0xCAFE_0001）对齐。
            authoritative_layout: 0xCAFE_0001,
        };

        // RR control plane channels：bounded(64) 跟 reviewer 拍板的容量一致。
        let (req_tx, req_rx) = bounded::<ClearEStopRrRequest>(64);
        let (resp_tx, resp_rx) = bounded::<ClearEStopRrResponse>(64);

        let handle = spawn_ipc_thread(
            cfg,
            clock,
            factory,
            cmd_writer,
            state_pub,
            wd_rx,
            executor_ctrl,
            Some(RrChannels {
                request_rx: req_rx,
                response_tx: resp_tx,
            }),
            // ClearEStop RR 单测不接 SetEnabled RR（v0.5 候选 C）。
            None,
        )
        .unwrap();

        (handle, inner, req_tx, resp_rx, clock_for_test)
    }

    /// 测试 helper：发 RR request → 等 response（最多 1s）→ 返回 payload。
    /// `deadline_ns` 默认设 `u64::MAX`（永不过期），跑过 RR 通路不被
    /// deadline 拦截；deadline 单测用 `send_and_recv_rr_with_deadline`。
    fn send_and_recv_rr(
        req_tx: &crossbeam_channel::Sender<ClearEStopRrRequest>,
        resp_rx: &crossbeam_channel::Receiver<ClearEStopRrResponse>,
        request_id: &str,
        dry_run: bool,
    ) -> ClearEStopRrPayload {
        send_and_recv_rr_with_deadline(req_tx, resp_rx, request_id, dry_run, u64::MAX)
    }

    /// 测试 helper：发 RR request 带显式 `deadline_ns`，等 response（最多 1s）。
    fn send_and_recv_rr_with_deadline(
        req_tx: &crossbeam_channel::Sender<ClearEStopRrRequest>,
        resp_rx: &crossbeam_channel::Receiver<ClearEStopRrResponse>,
        request_id: &str,
        dry_run: bool,
        deadline_ns: u64,
    ) -> ClearEStopRrPayload {
        req_tx
            .send(ClearEStopRrRequest {
                request_id: request_id.to_string(),
                dry_run,
                confirmation: format!("CONFIRM-{request_id}"),
                audit_reason: "unit-test".to_string(),
                deadline_ns,
            })
            .unwrap();
        let resp = resp_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("response within 1s");
        assert_eq!(resp.request_id, request_id, "RR response correlation 错位");
        resp.payload
    }

    /// **A1-3 (1/3b)**：dry_run 在 ipc_thread RR drain 阶段直接同步评估，
    /// **不**进 events queue / **不**进 ipc_tick。返 `WouldClear` /
    /// `StateNotSafe` / `NotTorqueOffConfirmed` domain status。
    #[test]
    fn rr_dry_run_short_circuits_with_would_clear_when_gate_passes() {
        // state=Safe + safety_state=TorqueOffConfirmed → gate 全过
        let (handle, inner, req_tx, resp_rx, _clock) = bring_up_with_rr(
            S0State::Safe,
            healthy_guards(),
            None,
            OutputSafetyState::TorqueOffConfirmed,
            true,
        );
        let payload = send_and_recv_rr(&req_tx, &resp_rx, "req-dry-1", true);
        match payload {
            ClearEStopRrPayload::WouldClear { would_clear_at_ns } => {
                assert!(
                    would_clear_at_ns > 0,
                    "would_clear_at_ns 应来自 clock.now()"
                );
            }
            other => panic!("expected WouldClear, got {other:?}"),
        }
        // 副作用全不动
        assert!(
            inner.safety_latched.load(Ordering::Acquire),
            "dry_run 不得动 executor.safety_latched"
        );

        handle.request_stop();
        handle.join().unwrap();
    }

    /// **A1-3 (1/3b)**：dry_run state != Safe → 返 `StateNotSafe`，
    /// 不进 events queue / ipc_tick。
    #[test]
    fn rr_dry_run_returns_state_not_safe_when_state_is_ready() {
        let (handle, _inner, req_tx, resp_rx, _clock) = bring_up_with_rr(
            S0State::Ready,
            healthy_guards(),
            None,
            OutputSafetyState::TorqueOffConfirmed,
            false,
        );
        let payload = send_and_recv_rr(&req_tx, &resp_rx, "req-dry-2", true);
        match payload {
            ClearEStopRrPayload::StateNotSafe { actual } => {
                assert_eq!(actual, S0State::Ready);
            }
            other => panic!("expected StateNotSafe, got {other:?}"),
        }
        handle.request_stop();
        handle.join().unwrap();
    }

    /// **A1-3 (1/3b)**：live 端到端成功路径。state=Safe + safety_state=
    /// TorqueOffConfirmed + 真实 latch_path → ipc_tick 三步硬顺序成功 →
    /// outcome.clear_estop=Cleared → response 派发 Cleared。
    #[test]
    fn rr_live_success_routes_cleared_response() {
        use crate::estop_latch::save_estop_latch;

        let tmp = tempfile::tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");
        save_estop_latch(&latch_path, true).unwrap(); // 预 provision latched=true

        let mut guards = healthy_guards();
        guards.estop_latched = true;

        let (handle, inner, req_tx, resp_rx, _clock) = bring_up_with_rr(
            S0State::Safe,
            guards,
            Some(latch_path.clone()),
            OutputSafetyState::TorqueOffConfirmed,
            true,
        );

        let payload = send_and_recv_rr(&req_tx, &resp_rx, "req-live-1", false);
        match payload {
            ClearEStopRrPayload::Cleared { cleared_at_ns } => {
                assert!(cleared_at_ns > 0);
            }
            other => panic!("expected Cleared, got {other:?}"),
        }

        // 端到端副作用：executor.safety_latched 已清；latch 文件 = "0\n"
        assert!(!inner.safety_latched.load(Ordering::Acquire));
        assert_eq!(std::fs::read_to_string(&latch_path).unwrap(), "0\n");

        handle.request_stop();
        handle.join().unwrap();
    }

    /// **A1-3 (1/3b)**：live state != Safe → ipc_tick 抛
    /// `ClearEStopTickOutcome::StateNotSafe` → response 派发
    /// `ClearEStopRrPayload::StateNotSafe`。
    #[test]
    fn rr_live_state_not_safe_routes_state_not_safe_response() {
        let (handle, _inner, req_tx, resp_rx, _clock) = bring_up_with_rr(
            S0State::Ready,
            healthy_guards(),
            None,
            OutputSafetyState::TorqueOffConfirmed,
            false,
        );
        let payload = send_and_recv_rr(&req_tx, &resp_rx, "req-live-2", false);
        match payload {
            ClearEStopRrPayload::StateNotSafe { actual } => {
                assert_eq!(actual, S0State::Ready);
            }
            other => panic!("expected StateNotSafe, got {other:?}"),
        }
        handle.request_stop();
        handle.join().unwrap();
    }

    /// **A1-3 (1/3b)**：live state=Safe 但 safety_state=TorqueOn → ipc_tick
    /// 走 try_apply_clear_estop gate 拒 → 抛 `NotTorqueOffConfirmed`。
    #[test]
    fn rr_live_not_torque_off_confirmed_routes_failed_precondition() {
        use crate::estop_latch::save_estop_latch;

        let tmp = tempfile::tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");
        save_estop_latch(&latch_path, true).unwrap();

        let mut guards = healthy_guards();
        guards.estop_latched = true;

        let (handle, _inner, req_tx, resp_rx, _clock) = bring_up_with_rr(
            S0State::Safe,
            guards,
            Some(latch_path),
            OutputSafetyState::TorqueOn,
            true,
        );
        let payload = send_and_recv_rr(&req_tx, &resp_rx, "req-live-3", false);
        match payload {
            ClearEStopRrPayload::NotTorqueOffConfirmed { actual } => {
                assert_eq!(actual, OutputSafetyState::TorqueOn);
            }
            other => panic!("expected NotTorqueOffConfirmed, got {other:?}"),
        }
        handle.request_stop();
        handle.join().unwrap();
    }

    /// **A1-3 (1/3b)**：live fsync 失败 → ipc_tick 抛 `FsyncFailed` →
    /// response 派发 `FsyncFailed` payload。
    #[test]
    fn rr_live_fsync_failed_routes_fsync_failed_response() {
        use crate::estop_latch::{backup_path, save_estop_latch};

        let tmp = tempfile::tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");
        let bak_path = backup_path(&latch_path);
        save_estop_latch(&latch_path, true).unwrap();
        // bak 替换成非空目录：让 backup-first save_estop_latch_clear 第一步失败
        std::fs::remove_file(&bak_path).unwrap();
        std::fs::create_dir(&bak_path).unwrap();
        std::fs::write(bak_path.join("nonempty"), b"x").unwrap();

        let mut guards = healthy_guards();
        guards.estop_latched = true;

        let (handle, inner, req_tx, resp_rx, _clock) = bring_up_with_rr(
            S0State::Safe,
            guards,
            Some(latch_path.clone()),
            OutputSafetyState::TorqueOffConfirmed,
            true,
        );
        let payload = send_and_recv_rr(&req_tx, &resp_rx, "req-live-4", false);
        assert_eq!(payload, ClearEStopRrPayload::FsyncFailed);

        // 副作用：executor.safety_latched 不动；primary 仍 "1\n"
        assert!(inner.safety_latched.load(Ordering::Acquire));
        assert_eq!(std::fs::read(&latch_path).unwrap(), b"1\n");

        handle.request_stop();
        handle.join().unwrap();
    }

    /// **A1-3 (1/3b) reviewer 加的硬边界**：active live ClearEStop 单飞——
    /// 第二个 live 来时 `in_flight_live` 已被占用，必须直接返
    /// `AlreadyInProgress`，不能排队等。这条挡两个 request 共享一个
    /// TickOutcome 搞错 Ack 的 race。
    #[test]
    fn rr_live_single_flight_second_request_returns_already_in_progress() {
        use crate::estop_latch::save_estop_latch;

        let tmp = tempfile::tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");
        save_estop_latch(&latch_path, true).unwrap();

        let mut guards = healthy_guards();
        guards.estop_latched = true;

        let (handle, _inner, req_tx, resp_rx, _clock) = bring_up_with_rr(
            S0State::Safe,
            guards,
            Some(latch_path),
            // safety_state=TorqueOn → live 进 try_apply_clear_estop 后 gate
            // 拒；这样 in_flight_live 在 ipc_tick 内就被 take，但本测试
            // 关心的是 "第二个 live 在 in_flight_live 没被 take 之前到达"
            // 的 race 窗口。为了制造稳定窗口，用 TorqueOn 让 live 走进
            // ipc_tick 也会 reject，但事件 push + ipc_tick 处理之间有时
            // 序窗口；并发 send 两条 live 至少一条会撞上 in_flight_live=Some
            // 窗口。
            //
            // 更稳的做法：两条 live 同时挤入 RR drain，第一条占 in_flight，
            // 第二条立刻拒。crossbeam bounded(64) channel 足以承载并发 64 条
            // 顺序消息；ipc_thread RR drain 一 tick 内 try_recv 多条循环，
            // 第一条 push event 后第二条立刻被拒。
            OutputSafetyState::TorqueOn,
            true,
        );

        // 先发两条 live request（一次性塞进 channel；ipc_thread 同 tick
        // drain 时第一条占 in_flight_live，第二条直拒 AlreadyInProgress）。
        req_tx
            .send(ClearEStopRrRequest {
                request_id: "req-A".to_string(),
                dry_run: false,
                confirmation: "CONFIRM-A".to_string(),
                audit_reason: "unit-test-A".to_string(),
                deadline_ns: u64::MAX,
            })
            .unwrap();
        req_tx
            .send(ClearEStopRrRequest {
                request_id: "req-B".to_string(),
                dry_run: false,
                confirmation: "CONFIRM-B".to_string(),
                audit_reason: "unit-test-B".to_string(),
                deadline_ns: u64::MAX,
            })
            .unwrap();

        // 收两条响应（顺序由 ipc_thread 决定）。
        let mut got_a: Option<ClearEStopRrPayload> = None;
        let mut got_b: Option<ClearEStopRrPayload> = None;
        for _ in 0..2 {
            let resp = resp_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("response within 2s");
            match resp.request_id.as_str() {
                "req-A" => got_a = Some(resp.payload),
                "req-B" => got_b = Some(resp.payload),
                other => panic!("unexpected request_id: {other}"),
            }
        }
        let pa = got_a.expect("req-A 应有响应");
        let pb = got_b.expect("req-B 应有响应");

        // **核心断言**：req-A / req-B 至少一条是 AlreadyInProgress（单飞拒），
        // 另一条是 ipc_tick 处理结果（NotTorqueOffConfirmed，因为 safety_state=TorqueOn）。
        // 不预设哪条先到（ipc_thread 端 try_recv 顺序虽通常 FIFO，但不依赖）。
        let (ipc_processed, single_flight_rejected) = match (&pa, &pb) {
            (ClearEStopRrPayload::AlreadyInProgress, p) => (*p, pa),
            (p, ClearEStopRrPayload::AlreadyInProgress) => (*p, pb),
            (a, b) => panic!(
                "至少一条 live 必须返 AlreadyInProgress（单飞）；\
                 实际 req-A = {a:?}, req-B = {b:?}"
            ),
        };
        assert!(
            matches!(
                ipc_processed,
                ClearEStopRrPayload::NotTorqueOffConfirmed { .. }
            ),
            "ipc_tick 处理的那一条应返 NotTorqueOffConfirmed（safety_state=TorqueOn 时 gate 拒），\
             实际 = {ipc_processed:?}"
        );
        assert_eq!(
            single_flight_rejected,
            ClearEStopRrPayload::AlreadyInProgress
        );

        handle.request_stop();
        handle.join().unwrap();
    }

    // ========================================================================
    // A1-3 (1/3b) round 2 新加：deadline + emergency preempt 锁测
    // ========================================================================

    /// **A1-3 (1/3b) round 2 阻断 2**：drain 入口 deadline check 拦截过期
    /// live request，**不**入 in_flight_live、**不**push event。
    #[test]
    fn rr_live_deadline_exceeded_at_drain_time_returns_deadline_exceeded() {
        let (handle, _inner, req_tx, resp_rx, _clock) = bring_up_with_rr(
            S0State::Safe,
            healthy_guards(),
            None,
            OutputSafetyState::TorqueOffConfirmed,
            true,
        );
        // deadline_ns=1（远小于 SystemClock::now()，drain 入口必命中）
        let payload = send_and_recv_rr_with_deadline(&req_tx, &resp_rx, "req-deadline-1", false, 1);
        assert_eq!(payload, ClearEStopRrPayload::DeadlineExceeded);
        handle.request_stop();
        handle.join().unwrap();
    }

    /// **A1-3 (1/3b) round 2 阻断 2**：dry_run 路径同样必须走 deadline check
    /// （drain 入口）；过期 dry_run 不调 evaluate_clear_estop_dry_run，直接
    /// reply `DeadlineExceeded`。
    #[test]
    fn rr_dry_run_deadline_exceeded_at_drain_time() {
        let (handle, _inner, req_tx, resp_rx, _clock) = bring_up_with_rr(
            S0State::Safe,
            healthy_guards(),
            None,
            OutputSafetyState::TorqueOffConfirmed,
            true,
        );
        let payload = send_and_recv_rr_with_deadline(&req_tx, &resp_rx, "req-deadline-2", true, 1);
        assert_eq!(payload, ClearEStopRrPayload::DeadlineExceeded);
        handle.request_stop();
        handle.join().unwrap();
    }

    /// **A1-3 (1/3b) round 2 阻断 1**：emergency preempt 取消 in-flight live。
    ///
    /// 场景：RR live 入 in_flight_live + push `Event::ClearEStop`，但同 tick
    /// 内 EStop event 也进 events queue。`ipc_thread` §5.6) preempt 段
    /// 检测 `events.has_emergency() = true` → 移除 events queue 里的
    /// `Event::ClearEStop` + take in_flight_live + reply
    /// `PreemptedByEmergency`。
    ///
    /// 实施：用 freeze hook 暂停 ipc_thread tick body，期间塞 EStop external
    /// event + RR live 到 channels；unfreeze 后 ipc_thread 同 tick drain
    /// 两条 message → §5.6 preempt 命中。
    ///
    /// 这条单测端到端覆盖 §5.6 段 "take in_flight_live + remove ClearEStop
    /// event + reply" 三件事；deadline 路径只是替换 payload variant
    /// 一行（共用 §5.6 take/remove/reply 代码）。
    #[test]
    fn rr_live_emergency_preempt_aborts_in_flight_live() {
        use crate::estop_latch::save_estop_latch;

        let tmp = tempfile::tempdir().unwrap();
        let latch_path = tmp.path().join("estop_latch");
        save_estop_latch(&latch_path, true).unwrap();

        let mut guards = healthy_guards();
        guards.estop_latched = true;

        let (handle, inner, req_tx, resp_rx, clock) = bring_up_with_rr(
            S0State::Safe,
            guards,
            Some(latch_path.clone()),
            OutputSafetyState::TorqueOffConfirmed,
            true,
        );

        // 1) freeze 200ms：ipc_thread tick body 不跑，channels 不被 drain
        let metrics = Arc::clone(handle.metrics());
        let freeze_until = clock.now().0 + 200_000_000;
        metrics
            .freeze_until_ns
            .store(freeze_until, Ordering::Release);
        // 等 freeze 稳定生效
        std::thread::sleep(Duration::from_millis(40));

        // 2) freeze 期间塞两条 message：EStop external + RR live
        handle.event_sender().send(Event::EStop).unwrap();
        req_tx
            .send(ClearEStopRrRequest {
                request_id: "req-emerg-1".to_string(),
                dry_run: false,
                confirmation: "CONFIRM-emerg-1".to_string(),
                audit_reason: "unit-test-preempt".to_string(),
                deadline_ns: u64::MAX,
            })
            .unwrap();

        // 3) unfreeze
        metrics.freeze_until_ns.store(0, Ordering::Release);

        // 4) recv RR response → 必须是 PreemptedByEmergency
        let resp = resp_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("response within 2s");
        assert_eq!(resp.request_id, "req-emerg-1");
        assert_eq!(
            resp.payload,
            ClearEStopRrPayload::PreemptedByEmergency,
            "in-flight live RR ClearEStop 必须被 §5.6 preempt 取消"
        );

        // 5) 副作用：EStop 路径继续执行 → executor.safety_latched 升 true
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            inner.safety_latched.load(Ordering::Acquire),
            "EStop 路径应继续执行，executor.safety_latched 应升 true"
        );

        handle.request_stop();
        handle.join().unwrap();
    }

    // ========================================================================
    // v0.5 候选 C：SetEnabled RR drain + 多 tick pending 观测（D8）
    //
    // 端到端：测试线程通过 se_req_tx 发 SetEnabledRrRequest → ipc_thread §5.5b
    // drain →（dry_run 同步评估 / live 即时 precondition gate + 起 pending）→
    // §6.6b 每 tick 观测 state → se_resp_tx → 测试线程 se_resp_rx 收
    // SetEnabledRrResponse。
    // ========================================================================

    /// 起 ipc_thread + SetEnabled RR control plane channels；返回 hb/estop
    /// publisher（驱动 Ready→Enabled.Active / EStop preempt）+ inner（spawn
    /// auto-ack/nack）+ clock（算 deadline）。
    #[allow(clippy::type_complexity)]
    fn bring_up_with_set_enabled_rr(
        initial_state: S0State,
        initial_guards: Guards,
        safety_latched: bool,
    ) -> (
        IpcThreadHandle,
        Arc<crate::hard_rt_ctrl::HardRtCtrlInner>,
        robot_transport::iceoryx2::Iceoryx2Publisher<markers::Heartbeat>,
        robot_transport::iceoryx2::Iceoryx2Publisher<markers::EStopEvent>,
        crossbeam_channel::Sender<SetEnabledRrRequest>,
        crossbeam_channel::Receiver<SetEnabledRrResponse>,
        Arc<dyn RobotClock>,
        RtSnapshotReader<S0StateSnapshot>,
    ) {
        use crossbeam_channel::bounded;

        let topics = fresh_topics();
        let pub_node: &'static _ =
            Box::leak(Box::new(NodeBuilder::new().create::<IxService>().unwrap()));
        let _cmd_pub =
            publisher::<markers::JointCommand>(pub_node, &topics.command, TopicClass::S0Command)
                .expect("open command publisher");
        let hb_pub =
            publisher::<markers::Heartbeat>(pub_node, &topics.heartbeat, TopicClass::Heartbeat)
                .expect("open heartbeat publisher");
        let estop_pub =
            publisher::<markers::EStopEvent>(pub_node, &topics.estop, TopicClass::SafetyEvent)
                .expect("open estop publisher");

        let (cmd_writer, _cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (state_pub, state_rdr) =
            RtSnapshotPublisher::<S0StateSnapshot>::new(S0StateSnapshot::default());
        let (_wd_tx, wd_rx) = unbounded::<Event>();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let clock_for_test = Arc::clone(&clock);

        let topics_for_factory = topics;
        let factory = move || {
            let sub_node = NodeBuilder::new().create::<IxService>().unwrap();
            let command = subscriber::<markers::JointCommand>(
                &sub_node,
                &topics_for_factory.command,
                TopicClass::S0Command,
            )
            .unwrap();
            let heartbeat = subscriber::<markers::Heartbeat>(
                &sub_node,
                &topics_for_factory.heartbeat,
                TopicClass::Heartbeat,
            )
            .unwrap();
            let estop = subscriber::<markers::EStopEvent>(
                &sub_node,
                &topics_for_factory.estop,
                TopicClass::SafetyEvent,
            )
            .unwrap();
            Box::leak(Box::new(sub_node));
            IpcSubscribers {
                command,
                heartbeat,
                estop,
            }
        };

        let inner = Arc::new(crate::hard_rt_ctrl::HardRtCtrlInner::new());
        inner.safety_state.store(
            OutputSafetyState::TorqueOffConfirmed.to_u8(),
            Ordering::Release,
        );
        inner
            .safety_latched
            .store(safety_latched, Ordering::Release);
        let executor_ctrl: Arc<dyn ExecutorCtrlClient> = Arc::new(
            crate::hard_rt_ctrl::HardRtCtrlClient::new(Arc::clone(&inner)),
        );

        let cfg = IpcThreadConfig {
            poll_interval_ns: 500_000,
            initial_guards,
            initial_state,
            latch_path: None,
            authoritative_layout: 0xCAFE_0001,
        };

        let (se_req_tx, se_req_rx) = bounded::<SetEnabledRrRequest>(64);
        let (se_resp_tx, se_resp_rx) = bounded::<SetEnabledRrResponse>(64);

        let handle = spawn_ipc_thread(
            cfg,
            clock,
            factory,
            cmd_writer,
            state_pub,
            wd_rx,
            executor_ctrl,
            // 本 harness 只接 SetEnabled RR（不接 ClearEStop）。
            None,
            Some(SetEnabledRrChannels {
                request_rx: se_req_rx,
                response_tx: se_resp_tx,
            }),
        )
        .unwrap();

        (
            handle,
            inner,
            hb_pub,
            estop_pub,
            se_req_tx,
            se_resp_rx,
            clock_for_test,
            state_rdr,
        )
    }

    /// `spawn_auto_ack` 的失败孪生：把 `pending_normal` swap 出来后 store
    /// `last_completed = (seq, AdapterResp::Err(HardwareFault))` → ipc_tick
    /// 走 `commit_fault(AdapterErr)`（PB10 fault-inject）。
    fn spawn_auto_nack(
        inner: Arc<crate::hard_rt_ctrl::HardRtCtrlInner>,
    ) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        use robot_core::executor_ctrl::{
            AdapterErr, AdapterResp, pack_completed, unpack_pending_normal,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let h = std::thread::spawn(move || {
            while !stop_clone.load(Ordering::Acquire) {
                let p = inner.pending_normal.swap(0, Ordering::AcqRel);
                if p != 0 {
                    let (_, _, seq) = unpack_pending_normal(p);
                    inner.last_completed.store(
                        pack_completed(seq, AdapterResp::Err(AdapterErr::HardwareFault)),
                        Ordering::Release,
                    );
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        (stop, h)
    }

    /// 发 live / dry_run SetEnabled RR request。
    fn send_se(
        req_tx: &crossbeam_channel::Sender<SetEnabledRrRequest>,
        request_id: &str,
        enabled: bool,
        dry_run: bool,
        deadline_ns: u64,
    ) {
        req_tx
            .send(SetEnabledRrRequest {
                request_id: request_id.to_string(),
                enabled,
                dry_run,
                deadline_ns,
                audit_reason: "unit-test".to_string(),
            })
            .unwrap();
    }

    /// 持续发 heartbeat（驱动 Ready→Enabled.Active）直到收到 RR response。
    fn pump_hb_until_se_resp(
        hb_pub: &mut robot_transport::iceoryx2::Iceoryx2Publisher<markers::Heartbeat>,
        resp_rx: &crossbeam_channel::Receiver<SetEnabledRrResponse>,
        timeout: Duration,
    ) -> SetEnabledRrResponse {
        let start = std::time::Instant::now();
        let mut seq = 1u64;
        while start.elapsed() < timeout {
            publish_heartbeat(hb_pub, seq);
            seq += 1;
            if let Ok(resp) = resp_rx.recv_timeout(Duration::from_millis(15)) {
                return resp;
            }
        }
        panic!("no SetEnabled response within {timeout:?}");
    }

    /// **AC-2 / D8**：enable happy —— Safe + ready guard + heartbeat 源 →
    /// 观测 Enabled.Active → `Enabled`（Ok）。
    #[test]
    fn pending_resolves_on_enabled_active() {
        let (handle, inner, mut hb_pub, _estop_pub, req_tx, resp_rx, _clock, _state_rdr) =
            bring_up_with_set_enabled_rr(S0State::Safe, healthy_guards(), false);
        let (ack_stop, ack_join) = spawn_auto_ack(Arc::clone(&inner));

        send_se(&req_tx, "enable-1", true, false, u64::MAX);
        let resp = pump_hb_until_se_resp(&mut hb_pub, &resp_rx, Duration::from_secs(3));

        assert_eq!(resp.request_id, "enable-1");
        assert!(
            matches!(resp.payload, SetEnabledRrPayload::Enabled { .. }),
            "enable happy 应返 Enabled，实际 {:?}",
            resp.payload
        );

        handle.request_stop();
        handle.join().unwrap();
        ack_stop.store(true, Ordering::Release);
        ack_join.join().unwrap();
    }

    /// **AC-3 / D3**：disable happy —— Enabled.Active → `Event::Disable` →
    /// adapter `Enable(false)` ack → Safe → `Disabled`（Ok）。无 heartbeat 依赖。
    #[test]
    fn disable_pending_resolves_on_safe() {
        let (handle, inner, _hb_pub, _estop_pub, req_tx, resp_rx, _clock, _state_rdr) =
            bring_up_with_set_enabled_rr(enabled_active(), healthy_guards(), false);
        let (ack_stop, ack_join) = spawn_auto_ack(Arc::clone(&inner));

        send_se(&req_tx, "disable-1", false, false, u64::MAX);
        let resp = resp_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("disable response within 3s");

        assert_eq!(resp.request_id, "disable-1");
        assert!(
            matches!(resp.payload, SetEnabledRrPayload::Disabled { .. }),
            "disable happy 应返 Disabled，实际 {:?}",
            resp.payload
        );

        handle.request_stop();
        handle.join().unwrap();
        ack_stop.store(true, Ordering::Release);
        ack_join.join().unwrap();
    }

    /// **单飞**：pending 在飞时第二个 live request 直拒 `AlreadyInProgress`，
    /// **不**入 pending（不 pump heartbeat → 第一个滞留 Ready 不 resolve）。
    #[test]
    fn pending_single_flight_rejects_second() {
        let (handle, _inner, _hb_pub, _estop_pub, req_tx, resp_rx, _clock, _state_rdr) =
            bring_up_with_set_enabled_rr(S0State::Safe, healthy_guards(), false);

        // A：far deadline + 无 heartbeat → 滞留 Ready 不 resolve（占单飞槽）。
        send_se(&req_tx, "A", true, false, u64::MAX);
        // B：在 A 的 pending 期间进 → AlreadyInProgress。
        send_se(&req_tx, "B", true, false, u64::MAX);

        let resp = resp_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("single-flight reject within 2s");
        assert_eq!(resp.request_id, "B", "应是第二个 request 被拒");
        assert_eq!(resp.payload, SetEnabledRrPayload::AlreadyInProgress);

        handle.request_stop();
        handle.join().unwrap();
    }

    /// **AC-2b / D8**：到 Ready 但 deadline 内无 heartbeat/Enabled.Active →
    /// `DeadlineExceeded`（pending 观测期 deadline，**非** drain 入口）。
    #[test]
    fn pending_deadline_resolves() {
        let (handle, _inner, _hb_pub, _estop_pub, req_tx, resp_rx, clock, _state_rdr) =
            bring_up_with_set_enabled_rr(S0State::Safe, healthy_guards(), false);

        // deadline 在未来 60ms（drain 入口 now < deadline → 起 pending）；不 pump
        // heartbeat → 滞留 Ready → 60ms 后 §6.6b deadline 过期 → DeadlineExceeded。
        let deadline_ns = clock.now().0 + 60_000_000;
        send_se(&req_tx, "deadline-1", true, false, deadline_ns);

        let resp = resp_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("deadline response within 2s");
        assert_eq!(resp.request_id, "deadline-1");
        assert_eq!(resp.payload, SetEnabledRrPayload::DeadlineExceeded);

        handle.request_stop();
        handle.join().unwrap();
    }

    /// **EStop preempt**：enable pending（Ready）期间 EStop 进 queue →
    /// `PreemptedByEStop`（emergency 优先于 target / deadline）。
    #[test]
    fn pending_preempt_estop_resolves_aborted() {
        let (handle, _inner, _hb_pub, mut estop_pub, req_tx, resp_rx, _clock, _state_rdr) =
            bring_up_with_set_enabled_rr(S0State::Safe, healthy_guards(), false);

        // enable far deadline + 无 heartbeat → 滞留 Ready（pending 在飞）。
        send_se(&req_tx, "estop-1", true, false, u64::MAX);
        // 让 pending 稳定在 Ready 再触发 EStop。
        std::thread::sleep(Duration::from_millis(30));
        publish_estop(&mut estop_pub, 1);

        let resp = resp_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("estop preempt response within 2s");
        assert_eq!(resp.request_id, "estop-1");
        assert_eq!(resp.payload, SetEnabledRrPayload::PreemptedByEStop);

        handle.request_stop();
        handle.join().unwrap();
    }

    /// **AC-2c / AC-3b / PB10**：adapter `Enable(false)` ack 失败 →
    /// `commit_fault(AdapterErr)` → §6.6b resolve `AdapterError`（wire Internal）。
    /// fault-inject = `spawn_auto_nack`（非 cross-process e2e；mock_hard_rt 无可控
    /// ack-fail 开关）。
    #[test]
    fn pending_adapter_fault_resolves_internal_adapter_error() {
        let (handle, inner, _hb_pub, _estop_pub, req_tx, resp_rx, _clock, _state_rdr) =
            bring_up_with_set_enabled_rr(enabled_active(), healthy_guards(), false);
        // 注入 adapter ack 失败（Enable(false) → Err(HardwareFault)）。
        let (nack_stop, nack_join) = spawn_auto_nack(Arc::clone(&inner));

        send_se(&req_tx, "fault-1", false, false, u64::MAX);
        let resp = resp_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("adapter-fault response within 3s");

        assert_eq!(resp.request_id, "fault-1");
        assert_eq!(resp.payload, SetEnabledRrPayload::AdapterError);

        handle.request_stop();
        handle.join().unwrap();
        nack_stop.store(true, Ordering::Release);
        nack_join.join().unwrap();
    }

    /// **Reverse Sync（Fault+disable 语义 A）**：`Fault(AdapterErr)` + disable →
    /// 幂等成功 `Disabled`(Ok)；**不**清 Fault（state 仍 `Fault(AdapterErr)`）、
    /// **不**进 pending、**不**误报 `AdapterError`（区别 Enabled→disable 中途 ack
    /// fail 才报 AdapterError）。
    #[test]
    fn disable_from_fault_adapter_err_is_idempotent_ok() {
        let fault = S0State::Fault(FaultCode::AdapterErr);
        let (handle, _inner, _hb_pub, _estop_pub, req_tx, resp_rx, _clock, state_rdr) =
            bring_up_with_set_enabled_rr(fault, healthy_guards(), false);

        send_se(&req_tx, "idem-adaptererr", false, false, u64::MAX);
        let resp = resp_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("idempotent disable response within 2s");

        assert_eq!(resp.request_id, "idem-adaptererr");
        assert!(
            matches!(resp.payload, SetEnabledRrPayload::Disabled { .. }),
            "Fault(AdapterErr)+disable 应幂等 Disabled(Ok)，实际 {:?}",
            resp.payload
        );
        // Ok **不**清 Fault：state 仍原 Fault(AdapterErr)（避免 Ok 被误读成清 Fault）。
        assert_eq!(
            state_rdr.load().state,
            fault,
            "disable 幂等成功不得清 Fault"
        );

        handle.request_stop();
        handle.join().unwrap();
    }

    /// **Reverse Sync（语义 A）**：非-AdapterErr `Fault`（如 `LimitViolation`）+
    /// disable → 幂等成功 `Disabled`(Ok) **即时**；state 仍原 Fault。
    /// `deadline = u64::MAX`：若误走 pending（target Safe，Fault 永不到 Safe），会
    /// 一直滞留 → recv 超时 → 本测 fail（即捕获回归）。
    #[test]
    fn disable_from_fault_other_is_idempotent_ok() {
        let fault = S0State::Fault(FaultCode::LimitViolation);
        let (handle, _inner, _hb_pub, _estop_pub, req_tx, resp_rx, _clock, state_rdr) =
            bring_up_with_set_enabled_rr(fault, healthy_guards(), false);

        send_se(&req_tx, "idem-limit", false, false, u64::MAX);
        let resp = resp_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("idempotent disable response within 2s");

        assert_eq!(resp.request_id, "idem-limit");
        assert!(
            matches!(resp.payload, SetEnabledRrPayload::Disabled { .. }),
            "非-AdapterErr Fault+disable 应幂等 Disabled(Ok)（非 DeadlineExceeded），实际 {:?}",
            resp.payload
        );
        assert_eq!(
            state_rdr.load().state,
            fault,
            "disable 幂等成功不得清 Fault"
        );

        handle.request_stop();
        handle.join().unwrap();
    }
}
