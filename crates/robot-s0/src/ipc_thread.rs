//! S0 非 RT IPC 线程：订阅关节目标/心跳/急停，推进唯一业务状态与持责控制桥。
//!
//! 关节目标解析后写 latest slot。基础控制使用高优先级队列，dry-run 使用普通队列；
//! 每 tick 先处理原急停公告与 Lower，再推进原 pending 和 Restore，最后处理普通请求。
//! Clear 的文件事务由唯一 PersistClient 交 writer 执行，IPC 不同步执行文件 fsync。
//!
//! subscriber 与 Node 在工作线程内由 fallible factory 构造，OwnedIpcPorts 保持析构顺序。
//! 真实工厂与首次状态发布完成后才发 READY；spawn 立即交回拥有真实线程的 handle。
//! 正常线程退出返回 IpcExit，按值保留原 pending、coordinator、client、输入与 outbox。
//!
//! Guards 是业务观测：心跳按序回放，latch 来自同 ControlAuthority；Serial 恢复条件从
//! 原 abort/inhibit/Boot/RX/Off/L0 观察刷新。hw_enable_ok 仅由真实 Enable commit 更新，
//! 执行权限仍由同原子 authority 与不可复制许可决定。

#![forbid(unused_must_use)]
#![allow(clippy::doc_markdown)]

#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::event_ingress::ExternalEvents;
#[cfg(test)]
use crate::state_machine::FaultCode;
#[cfg(test)]
use crossbeam_channel::unbounded;
use crossbeam_channel::{Receiver, Sender};
use robot_core::adapter::{ControlModeRepr, JointCommandBuf, MAX_JOINTS};
use robot_core::time::{MonoRawNs, RobotClock};
use robot_msg::markers;
use robot_msg::msg::{
    ControlMode as FbControlMode, JointCommand as FbJointCommand, StaleAction as FbStaleAction,
};
use robot_transport::iceoryx2::Iceoryx2Subscriber;
use robot_transport::{LatestCommandWriter, LatestSubscriber, RtSnapshotPublisher};

use robot_core::executor_ctrl::{ExecutorCtrlClient, OutputSafetyState};

use crate::state_machine::{Event, Guards, S0State, S0StateSnapshot};

// ============================================================================
// RR control plane（A1-3 (1/3b) ClearEStop RR drain；reviewer 拍板）
// ============================================================================

/// RR 线程移交的唯一 Clear 信封，route 绑定原 peer/slot；request_id 只作关联标识。
/// dry_run 在普通通道只读评估；live 在高通道预约同域事务并等待真实文件与权限退休。
/// deadline 在真实源出队和执行前检查；已开始事务保留首因与真实结果，不由晚到期限改写。
#[derive(Debug)]
pub struct ClearEStopRrRequest {
    pub route: crate::rr_admission::SlotIdentity,
    /// 客户端 UUIDv7，用于 RR response routing；不用作幂等 key。
    pub request_id: String,
    /// `true` → 只读评估；`false` → 真实协调事务，文件完成与权限退休后回复。
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
#[derive(Debug)]
pub struct ClearEStopRrResponse {
    pub(crate) route: crate::rr_admission::SlotIdentity,
    pub(crate) effect: robot_msg::msg::SideEffect,
    pub(crate) superseded: bool,
    /// 复用 request 的 `request_id` 让 `rr_server_thread` 路由 reply。
    pub(crate) request_id: String,
    /// Domain status 枚举（详 [`ClearEStopRrPayload`]）。
    pub(crate) payload: ClearEStopRrPayload,
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
    /// W3 真实协调结果；原逻辑与物理退休事实由同一事务消费链产生。
    Coordination(crate::clear_coordination::ClearOutcome),
    QueueFull,
    ShuttingDown,
    /// **live 成功**：fsync(primary+bak) → `clear_safety_latch()` → 翻 guard
    /// 三步全过；`cleared_at_ns` = `clock.now()` mono raw 时间戳。
    /// gateway 映射到 `tonic::Code::Ok`（gRPC `Ack`）。
    Cleared {
        cleared_at_ns: u64,
    },

    /// **dry_run 成功**：gate 全过（state == Safe + safety_state ==
    /// TorqueOffConfirmed）；`would_clear_at_ns` = 评估瞬间的 mono raw 时间戳，
    /// **不**代表硬件真清的时刻——dry_run 不动硬件。
    /// gateway 映射到 `tonic::Code::Ok`（携带 dry_run 提示字符串）。
    WouldClear {
        would_clear_at_ns: u64,
    },

    /// **gate 拒**：`*state != S0State::Safe`。`actual` 携带具体 state
    /// 让 gateway 给客户端清楚的诊断。live 路径走 `plan_transition` 返
    /// `None`，dry_run 路径走 `evaluate_clear_estop_dry_run` 直接拒。
    /// gateway 映射到 `tonic::Code::FailedPrecondition`。
    StateNotSafe {
        actual: S0State,
    },

    /// **gate 拒**：state == Safe 但 `safety_state != TorqueOffConfirmed`
    /// （硬件断扭未确认）。**reviewer 拍板**：torque gate 看
    /// `executor_ctrl.safety_state()` 权威，不能靠 `guards.estop_latched`
    /// 副本绕过。
    /// gateway 映射到 `tonic::Code::FailedPrecondition`。
    NotTorqueOffConfirmed {
        actual: OutputSafetyState,
    },

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

    /// 原 live Restore 位于已领取 EStop 的准入前缀，尚未预约事务，按原 route 拒绝。
    PreemptedByEmergency,

    /// 源出队或执行前的严格 `now > deadline` 拒绝；不覆盖已经锁定的事务结果。
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
    pub normal_request_rx: Receiver<ClearEStopRrRequest>,
    pub response_tx: Sender<ClearEStopRrResponse>,
}

// ============================================================================
// SetEnabled RR control plane（v0.5 候选 C；第 2 个专用 rr_server，D5）
// ============================================================================
//
// Clear 与 SetEnabled 均保留原 RR route 跨 tick 推进。Clear 等待真实 writer
// 完成与事务退休；Enable 等待原执行器 start/finish/commit，Lower 等待原 Stop/
// Confirm 和旧序号退责。只有完整退休后才由原 outbox 交还唯一 RR 响应。

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
#[derive(Debug)]
pub struct SetEnabledRrRequest {
    pub route: crate::rr_admission::SlotIdentity,
    pub request_id: String,
    pub enabled: bool,
    pub dry_run: bool,
    pub deadline_ns: u64,
    pub audit_reason: String,
}

/// RR control plane: `ipc_thread` → `set_enabled_rr_server` 的 `SetEnabled`
/// response channel payload。`request_id` 复用让 rr_server 路由 reply。
#[derive(Debug)]
pub struct SetEnabledRrResponse {
    pub(crate) route: crate::rr_admission::SlotIdentity,
    pub(crate) effect: robot_msg::msg::SideEffect,
    pub(crate) request_id: String,
    pub(crate) payload: SetEnabledRrPayload,
}

/// `SetEnabled` RR domain status enum（v0.5 候选 C）。
///
/// gateway / rr_server **显式 match** 每 variant → `RpcStatus` + domain
/// `SetEnabledStatus`（**不**直接 cast，沿 `ClearEStopRrPayload` 约束）。
/// `Copy` 友好（仅 `u64` / `S0State` POD 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetEnabledRrPayload {
    SupersededByDisable,
    QueueFull,
    Superseded,
    /// **enable 成功**：state commit `Enabled.Active` + adapter `Enable(true)`
    /// ack（D8 / AC-2）。`completed_at_ns` = 观测到 target 的 mono raw 时刻。
    /// → `RpcStatus::Ok` + domain `Ok`。
    Enabled {
        completed_at_ns: u64,
    },
    /// **disable 成功**：state commit `Safe` + adapter `Enable(false)` ack
    /// （D3 / AC-3）。→ `RpcStatus::Ok` + domain `Ok`。
    Disabled {
        completed_at_ns: u64,
    },
    /// **dry_run precondition 通过**（D7）：**不**代表 adapter 会 ack / enable
    /// 真完成。→ `RpcStatus::Ok` + domain `WouldAccept`。
    WouldAccept {
        at_ns: u64,
    },
    /// **precondition 拒**：ready guard 不满足 / state 不允许（`Fault` 需先
    /// `ClearError`，AC-9）。`actual` 携当前 state。→ `FailedPrecondition` +
    /// domain `GuardRejected`。
    GuardRejected {
        actual: S0State,
    },
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
    pub normal_request_rx: Receiver<SetEnabledRrRequest>,
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
/// IPC 每 tick 在持责推进后读 `state`：到 `target` → `Enabled`/
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
    /// 仅验收读取真实 evaluator 入口计数，不参与生产行为。
    #[cfg(test)]
    pub clear_evaluations: AtomicU64,
    #[cfg(test)]
    pub set_evaluations: AtomicU64,
    /// 主循环 tick 数。
    pub tick_count: AtomicU64,
    /// 成功解析并写入 `LatestCommandWriter` 的 `JointCommand` 数。
    pub commands_applied: AtomicU64,
    /// **v0.5 候选 E 段(3)**：`layout_id` 不等于权威值被丢弃的
    /// `JointCommand` 帧数（`AtomicU64::fetch_add(1, Relaxed)`；丢弃 =
    /// 不写 `LatestCommandSlot`，防错位命令驱动错关节集）。
    pub unknown_layout_count: AtomicU64,
    /// `JointCommand` 解析失败及 Velocity 数值、超时策略、时效门拒绝次数。
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
    /// 唯一 IPC 业务循环完成次数（保留既有指标字段名）。
    pub ipc_tick_count: AtomicU64,
    /// state 变化次数（`new_state != prev_state`）。
    pub state_changes: AtomicU64,
    /// **S48 A3b**：`state_pub.publish` 返回错误（版本耗尽 / 写端损坏）的次数。
    /// 首次出错即结束本 worker，由 `s0_daemon` READY 循环的 `is_finished` 判错。
    pub snapshot_publish_errors: AtomicU64,
    /// **S48 A3b（test-only）**：真实 `publish` 调用之前取的 `RobotClock` 时刻，
    /// 仅该次返回 `VersionExhausted` 时原子记录（0 = 未发生）。
    #[cfg(feature = "test-comm-fault-hook")]
    pub snapshot_publish_failure_attempt_ns: AtomicU64,
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

/// subscriber 先于原 Node 析构；端口只在线程内创建和使用。
pub struct OwnedIpcPorts {
    subscribers: IpcSubscribers,
    _node: iceoryx2::prelude::Node<iceoryx2::service::ipc::Service>,
}
impl OwnedIpcPorts {
    #[must_use]
    pub fn new(
        subscribers: IpcSubscribers,
        node: iceoryx2::prelude::Node<iceoryx2::service::ipc::Service>,
    ) -> Self {
        Self {
            subscribers,
            _node: node,
        }
    }
}

#[derive(Debug, Clone)]
pub enum IpcInitError {
    Node(iceoryx2::node::NodeCreationFailure),
    Subscriber {
        topic: String,
        error: robot_transport::TransportError,
    },
    InitialPublish(robot_transport::SnapshotPublishError),
}

/// 非阻塞 READY 读取；先保缓冲结果，再报告断链，不能重复消费失败事实。
pub struct ReadyReader<E> {
    receiver: std::sync::mpsc::Receiver<Result<(), E>>,
    observed: Option<Result<(), E>>,
    disconnected: bool,
}
#[derive(Debug)]
pub enum ReadyState<'a, E> {
    Pending,
    Ready,
    Failed(&'a E),
    Disconnected,
}
impl<E> ReadyReader<E> {
    pub(crate) fn new(receiver: std::sync::mpsc::Receiver<Result<(), E>>) -> Self {
        Self {
            receiver,
            observed: None,
            disconnected: false,
        }
    }
    pub fn poll(&mut self) -> ReadyState<'_, E> {
        if self.observed.is_none() && !self.disconnected {
            match self.receiver.try_recv() {
                Ok(result) => self.observed = Some(result),
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.disconnected = true,
            }
        }
        match &self.observed {
            Some(Ok(())) => ReadyState::Ready,
            Some(Err(error)) => ReadyState::Failed(error),
            None if self.disconnected => ReadyState::Disconnected,
            None => ReadyState::Pending,
        }
    }
}

pub struct IpcSpawnInputs<F> {
    pub cfg: IpcThreadConfig,
    pub clock: Arc<dyn RobotClock>,
    pub subscribers_factory: F,
    pub cmd_writer: LatestCommandWriter<JointCommandBuf>,
    pub state_pub: RtSnapshotPublisher<S0StateSnapshot>,
    pub watchdog_rx: Receiver<Event>,
    pub ctrl: Arc<crate::hard_rt_ctrl::HardRtCtrlClient>,
    pub boot: Arc<crate::boot_supervisor::BootShared>,
    pub persist: crate::latch_writer::PersistClient,
    pub rr_channels: Option<RrChannels>,
    pub set_enabled_rr_channels: Option<SetEnabledRrChannels>,
    pub stop: Arc<AtomicBool>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcBindError {
    MissingBinding,
    AuthorityMismatch,
    StartDomainMismatch,
    WriterSharedMismatch,
}
#[allow(
    clippy::large_enum_variant,
    reason = "绑定拒绝内联返还全部原输入，避免为错误分支新增分配与拥有层级"
)]
pub enum IpcSpawnError<F> {
    Binding {
        inputs: IpcSpawnInputs<F>,
        reason: IpcBindError,
    },
    ThreadSpawn(std::io::Error),
}
impl<F> std::fmt::Debug for IpcSpawnError<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Binding { reason, .. } => f.debug_tuple("Binding").field(reason).finish(),
            Self::ThreadSpawn(error) => f.debug_tuple("ThreadSpawn").field(error).finish(),
        }
    }
}

struct IpcControlBinding {
    ctrl: Arc<crate::hard_rt_ctrl::HardRtCtrlClient>,
    boot: Arc<crate::boot_supervisor::BootShared>,
}
struct IpcChannels {
    watchdog: Receiver<Event>,
    external: Receiver<crate::event_ingress::ControlEvent>,
    clear: Option<RrChannels>,
    set: Option<SetEnabledRrChannels>,
    rr_sealed: [bool; 4],
}
#[derive(Default)]
pub(crate) struct RetiredOutbox {
    pub(crate) clear: std::collections::VecDeque<ClearEStopRrResponse>,
    pub(crate) set: std::collections::VecDeque<SetEnabledRrResponse>,
}
struct IpcOwned {
    loop_state: crate::ipc_runtime::IpcLoopState,
    binding: IpcControlBinding,
    persist: crate::latch_writer::PersistClient,
    channels: IpcChannels,
    outbox: RetiredOutbox,
}
impl IpcOwned {
    fn is_quiescent(&self) -> bool {
        self.channels.rr_sealed.into_iter().all(|closed| closed)
            && self.loop_state.is_quiescent()
            && self.persist.has_credit()
            && self.outbox.clear.is_empty()
            && self.outbox.set.is_empty()
    }
}

fn flush_retired_outbox(outbox: &mut RetiredOutbox, channels: &IpcChannels) {
    if let Some(channel) = &channels.clear {
        while let Some(response) = outbox.clear.pop_front() {
            match channel.response_tx.try_send(response) {
                Ok(()) => {}
                Err(
                    crossbeam_channel::TrySendError::Full(original)
                    | crossbeam_channel::TrySendError::Disconnected(original),
                ) => {
                    outbox.clear.push_front(original);
                    break;
                }
            }
        }
    }
    if let Some(channel) = &channels.set {
        while let Some(response) = outbox.set.pop_front() {
            match channel.response_tx.try_send(response) {
                Ok(()) => {}
                Err(
                    crossbeam_channel::TrySendError::Full(original)
                    | crossbeam_channel::TrySendError::Disconnected(original),
                ) => {
                    outbox.set.push_front(original);
                    break;
                }
            }
        }
    }
}

/// 只在本拥有模块核原责任后构造；借用期间不能重新推进同一个 IPC owner。
pub(crate) struct ClearClosureProof<'a> {
    boot: &'a crate::boot_supervisor::BootShared,
    _owned: &'a IpcOwned,
    g: std::num::NonZeroU64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClosureMismatch {
    Target,
    Generation,
    PublishedGeneration,
}
pub(crate) struct ClearClosurePublishRefused<'a> {
    pub(crate) proof: ClearClosureProof<'a>,
    pub(crate) reason: ClosureMismatch,
}
impl ClearClosureProof<'_> {
    pub(crate) fn checked_epoch_for(
        &self,
        target: &crate::boot_supervisor::BootShared,
    ) -> Result<std::num::NonZeroU64, ClosureMismatch> {
        if !std::ptr::eq(self.boot, target) {
            return Err(ClosureMismatch::Target);
        }
        if target.runtime_words().generation() != self.g.get() {
            return Err(ClosureMismatch::Generation);
        }
        Ok(self.g)
    }
}
fn prove_clear_closure<'a>(
    owned: &'a IpcOwned,
    boot: &'a crate::boot_supervisor::BootShared,
) -> Option<ClearClosureProof<'a>> {
    let g = std::num::NonZeroU64::new(boot.runtime_words().generation())?;
    let binding = boot.binding()?;
    if !std::ptr::eq(owned.binding.boot.as_ref(), boot)
        || !Arc::ptr_eq(binding.authority(), owned.binding.ctrl.authority())
        || !Arc::ptr_eq(binding.writer(), owned.persist.shared())
        || boot.runtime_words().shutdown_deadline().is_none()
        || !owned.is_quiescent()
    {
        return None;
    }
    Some(ClearClosureProof {
        boot,
        _owned: owned,
        g,
    })
}
#[derive(Debug)]
pub enum IpcExitReason {
    Stopped,
    Initialization(IpcInitError),
    SnapshotPublish(robot_transport::SnapshotPublishError),
    ReadyDisconnected,
}
/// 正常退出持原 IPC 责任；panic 仅由真实 join Err 表达，不伪造此值。
#[must_use]
pub struct IpcExit {
    reason: IpcExitReason,
    owned: IpcOwned,
}
impl std::fmt::Debug for IpcExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcExit")
            .field("reason", &self.reason)
            .field("quiescent", &self.is_quiescent())
            .finish_non_exhaustive()
    }
}
impl IpcExit {
    #[must_use]
    pub const fn reason(&self) -> &IpcExitReason {
        &self.reason
    }
    #[must_use]
    pub fn is_quiescent(&self) -> bool {
        self.owned.is_quiescent()
    }
}

pub struct IpcThreadHandle {
    join: JoinHandle<IpcExit>,
    stop: Arc<AtomicBool>,
    last_hb_ns: Arc<AtomicU64>,
    event_tx: ExternalEvents,
    metrics: Arc<IpcThreadMetrics>,
    ready: ReadyReader<IpcInitError>,
}
impl IpcThreadHandle {
    pub fn request_stop(&self) {
        self.stop.swap(true, Ordering::AcqRel);
    }
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }
    pub fn poll_ready(&mut self) -> ReadyState<'_, IpcInitError> {
        self.ready.poll()
    }
    #[must_use]
    pub fn last_hb_ns(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.last_hb_ns)
    }
    #[must_use]
    pub fn event_sender(&self) -> ExternalEvents {
        self.event_tx.clone()
    }
    #[must_use]
    pub fn metrics(&self) -> &Arc<IpcThreadMetrics> {
        &self.metrics
    }
    pub fn join(self) -> thread::Result<IpcExit> {
        self.join.join()
    }
}

/// 先核原绑定；成功 spawn 后立即交原句柄，不等待 factory/首次发布。
#[allow(
    clippy::result_large_err,
    reason = "绑定拒绝必须完整返原输入含唯一写端/client"
)]
#[allow(
    clippy::too_many_lines,
    reason = "同一函数完成原输入核验、线程启动与原句柄交接，避免拆散失败拥有出口"
)]
pub fn spawn_ipc_thread<F>(inputs: IpcSpawnInputs<F>) -> Result<IpcThreadHandle, IpcSpawnError<F>>
where
    F: FnOnce() -> Result<OwnedIpcPorts, IpcInitError> + Send + 'static,
{
    let reason = match inputs.boot.binding() {
        None => Some(IpcBindError::MissingBinding),
        Some(binding) if !Arc::ptr_eq(binding.authority(), inputs.ctrl.authority()) => {
            Some(IpcBindError::AuthorityMismatch)
        }
        Some(_) if inputs.ctrl.start_slot().domain() != inputs.ctrl.authority().domain() => {
            Some(IpcBindError::StartDomainMismatch)
        }
        Some(binding) if !Arc::ptr_eq(binding.writer(), inputs.persist.shared()) => {
            Some(IpcBindError::WriterSharedMismatch)
        }
        Some(_) => None,
    };
    if let Some(reason) = reason {
        return Err(IpcSpawnError::Binding { inputs, reason });
    }
    #[cfg(feature = "test-comm-fault-hook")]
    crate::comm_fault_hook::before_spawn("ipc").map_err(IpcSpawnError::ThreadSpawn)?;
    let IpcSpawnInputs {
        cfg,
        clock,
        subscribers_factory,
        cmd_writer,
        mut state_pub,
        watchdog_rx,
        ctrl,
        boot,
        persist,
        rr_channels,
        set_enabled_rr_channels,
        stop,
    } = inputs;
    let last_hb_ns = Arc::new(AtomicU64::new(clock.now().0));
    let metrics = Arc::new(IpcThreadMetrics::default());
    let legacy_notice = Arc::new(AtomicBool::new(false));
    let executor: Arc<dyn ExecutorCtrlClient> = ctrl.clone();
    let emergency: Arc<dyn robot_core::executor_ctrl::SerialEmergencyCtrl> = ctrl.clone();
    let (event_tx, event_rx) =
        ExternalEvents::channel(Arc::clone(&legacy_notice), executor, Some(emergency));
    let rr_sealed = [
        rr_channels.is_none(),
        rr_channels.is_none(),
        set_enabled_rr_channels.is_none(),
        set_enabled_rr_channels.is_none(),
    ];
    let normal_capacity = rr_channels
        .as_ref()
        .and_then(|ch| ch.normal_request_rx.capacity())
        .unwrap_or(0)
        .saturating_add(
            set_enabled_rr_channels
                .as_ref()
                .and_then(|ch| ch.normal_request_rx.capacity())
                .unwrap_or(0),
        );
    let owned = IpcOwned {
        loop_state: crate::ipc_runtime::IpcLoopState::new(
            cfg.initial_state,
            cfg.initial_guards,
            normal_capacity,
        ),
        binding: IpcControlBinding { ctrl, boot },
        persist,
        channels: IpcChannels {
            watchdog: watchdog_rx,
            external: event_rx,
            clear: rr_channels,
            set: set_enabled_rr_channels,
            rr_sealed,
        },
        outbox: RetiredOutbox::default(),
    };
    let join_stop = Arc::clone(&stop);
    let join_last_hb = Arc::clone(&last_hb_ns);
    let join_metrics = Arc::clone(&metrics);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let join = thread::Builder::new()
        .name("s0_ipc_thread".into())
        .spawn(move || {
            let ports = match subscribers_factory() {
                Ok(ports) => ports,
                Err(error) => {
                    let _ = ready_tx.try_send(Err(error.clone()));
                    return IpcExit {
                        reason: IpcExitReason::Initialization(error),
                        owned,
                    };
                }
            };
            run_loop(
                &cfg,
                clock.as_ref(),
                ports,
                &cmd_writer,
                &mut state_pub,
                &join_stop,
                &join_last_hb,
                &join_metrics,
                owned,
                ready_tx,
            )
        })
        .map_err(IpcSpawnError::ThreadSpawn)?;
    Ok(IpcThreadHandle {
        join,
        stop,
        last_hb_ns,
        event_tx,
        metrics,
        ready: ReadyReader::new(ready_rx),
    })
}

#[allow(clippy::too_many_arguments)]
fn run_loop(
    cfg: &IpcThreadConfig,
    clock: &dyn RobotClock,
    mut ports: OwnedIpcPorts,
    cmd_writer: &LatestCommandWriter<JointCommandBuf>,
    state_pub: &mut RtSnapshotPublisher<S0StateSnapshot>,
    stop: &AtomicBool,
    last_hb_ns: &AtomicU64,
    metrics: &IpcThreadMetrics,
    mut owned: IpcOwned,
    ready: std::sync::mpsc::SyncSender<Result<(), IpcInitError>>,
) -> IpcExit {
    use crate::ipc_runtime::{IpcRuntimeContext, IpcSources, TopicEvents};
    let initial_stamp_ns = clock.now().0;
    if let Err(error) = state_pub.publish(&S0StateSnapshot::new(
        cfg.initial_state,
        cfg.initial_guards,
        initial_stamp_ns,
    )) {
        metrics
            .snapshot_publish_errors
            .fetch_add(1, Ordering::Relaxed);
        #[cfg(feature = "test-comm-fault-hook")]
        metrics
            .snapshot_publish_failure_attempt_ns
            .store(initial_stamp_ns, Ordering::Relaxed);
        let error = IpcInitError::InitialPublish(error);
        let _ = ready.try_send(Err(error.clone()));
        return IpcExit {
            reason: IpcExitReason::Initialization(error),
            owned,
        };
    }
    #[cfg(feature = "test-comm-fault-hook")]
    crate::comm_fault_hook::boot_pause("ipc_ready");
    if ready.try_send(Ok(())).is_err() {
        return IpcExit {
            reason: IpcExitReason::ReadyDisconnected,
            owned,
        };
    }
    drop(ready);
    while crate::worker_stop::worker_running(crate::worker_stop::WorkerKind::Ipc, clock, stop) {
        #[cfg(any(test, feature = "test-freeze-hook"))]
        crate::freeze_hook::freeze_check(&metrics.freeze_until_ns, clock, stop);
        let ctrl = &owned.binding.ctrl;
        let context = IpcRuntimeContext {
            ctrl: ctrl.as_ref(),
            boot: &owned.binding.boot,
            clock,
            metrics,
        };
        let sources = IpcSources {
            watchdog: &owned.channels.watchdog,
            external: &owned.channels.external,
            clear_rr: owned.channels.clear.as_ref(),
            set_rr: owned.channels.set.as_ref(),
        };
        let snapshot = owned.loop_state.tick(
            &context,
            &sources,
            &mut owned.channels.rr_sealed,
            &mut owned.persist,
            &mut owned.outbox,
            || TopicEvents {
                heartbeat: poll_heartbeat(
                    &mut ports.subscribers.heartbeat,
                    clock,
                    last_hb_ns,
                    metrics,
                ),
                estop: poll_estop(&mut ports.subscribers.estop, metrics),
            },
        );
        // 高优先级收紧与 owned 推进已完成后，才更新普通关节 latest 槽。
        poll_command(
            &mut ports.subscribers.command,
            cmd_writer,
            cfg.authoritative_layout,
            clock,
            metrics,
        );
        #[cfg(feature = "test-comm-fault-hook")]
        let attempt_started_ns = clock.now().0;
        if let Err(error) = state_pub.publish(&snapshot) {
            metrics
                .snapshot_publish_errors
                .fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "test-comm-fault-hook")]
            metrics
                .snapshot_publish_failure_attempt_ns
                .store(attempt_started_ns, Ordering::Relaxed);
            return IpcExit {
                reason: IpcExitReason::SnapshotPublish(error),
                owned,
            };
        }
        flush_retired_outbox(&mut owned.outbox, &owned.channels);
        if let Some(proof) = prove_clear_closure(&owned, &owned.binding.boot)
            && let Err(refused) = owned.binding.boot.publish_clear_retired(proof)
        {
            // 原 proof 仍借原 owned，失败不清状态，也不追认封口。
            tracing::error!(reason = ?refused.reason, "IPC 封口观测发布拒绝");
            let _ = refused.proof;
            let _ = owned.binding.boot.arm_runtime_shutdown(clock.now());
        }
        thread::sleep(Duration::from_nanos(cfg.poll_interval_ns));
    }
    IpcExit {
        reason: IpcExitReason::Stopped,
        owned,
    }
}

// ============================================================================
// Topic poll helpers（把 iceoryx2 lease → typed body → 业务动作）
// ============================================================================

fn poll_command(
    cmd_sub: &mut Iceoryx2Subscriber<markers::JointCommand>,
    cmd_writer: &LatestCommandWriter<JointCommandBuf>,
    authoritative_layout: u64,
    clock: &dyn RobotClock,
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
    let Ok(buf) = parse_joint_command(&fb_cmd, stamp_ns, clock.now()) else {
        metrics.command_parse_errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
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
    #[cfg(feature = "test-comm-fault-hook")]
    crate::comm_fault_trace::emit(
        crate::comm_fault_trace::TraceKind::TopicReceived,
        crate::comm_fault_trace::now(),
        crate::comm_fault_trace::TraceContext::default(),
        0,
        0,
    );
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

/// 仅 Velocity 的新增接收校验错误；其他模式保留既有解码合同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JointCommandParseError {
    EmptyVelocityTargets,
    TooManyVelocityTargets,
    NonFiniteVelocityTarget,
    VelocityTargetOutOfRange,
    UnsupportedVelocityTimeout,
    InvalidVelocityWindow,
}

/// `FlatBuffers` `JointCommand` → POD [`JointCommandBuf`]。
///
/// Velocity 必须有非空有界目标、Zero 超时策略及接收时仍有效的绝对期限；
/// f64 目标在窄化前校验有限性与 f32 范围，失败时不交出可写槽的命令。
/// Position、ServoPosition、Torque 与未知模式沿用原 positions 映射与截断规则。
fn parse_joint_command(
    fb: &FbJointCommand<'_>,
    stamp_ns: u64,
    now: MonoRawNs,
) -> Result<JointCommandBuf, JointCommandParseError> {
    let mut buf = JointCommandBuf::default();

    let target = fb.target();
    let n = match fb.mode() {
        FbControlMode::Velocity => {
            if fb.on_timeout() != FbStaleAction::Zero {
                return Err(JointCommandParseError::UnsupportedVelocityTimeout);
            }
            let deadline_ns = fb.deadline_ns();
            if stamp_ns > now.0 || now.0 >= deadline_ns || deadline_ns <= stamp_ns {
                return Err(JointCommandParseError::InvalidVelocityWindow);
            }
            let n = target.map_or(0, |values| values.len());
            if n == 0 {
                return Err(JointCommandParseError::EmptyVelocityTargets);
            }
            if n > MAX_JOINTS {
                return Err(JointCommandParseError::TooManyVelocityTargets);
            }
            n
        }
        _ => target.map_or(0, |values| values.len().min(MAX_JOINTS)),
    };
    #[allow(clippy::cast_possible_truncation)]
    {
        buf.joint_count = n as u8;
    }
    if let Some(v) = target {
        let slots = match fb.mode() {
            FbControlMode::Velocity => &mut buf.velocities,
            _ => &mut buf.positions,
        };
        for (i, slot) in slots.iter_mut().enumerate().take(n) {
            let value = v.get(i);
            if fb.mode() == FbControlMode::Velocity {
                if !value.is_finite() {
                    return Err(JointCommandParseError::NonFiniteVelocityTarget);
                }
                if value.abs() > f64::from(f32::MAX) {
                    return Err(JointCommandParseError::VelocityTargetOutOfRange);
                }
            }
            #[allow(clippy::cast_possible_truncation)]
            {
                let narrowed = value as f32;
                if fb.mode() == FbControlMode::Velocity && !narrowed.is_finite() {
                    return Err(JointCommandParseError::VelocityTargetOutOfRange);
                }
                *slot = narrowed;
            }
        }
    }
    buf.mode = fb_control_mode_to_repr(fb.mode());
    buf.stamp_ns = stamp_ns;
    buf.deadline_ns = fb.deadline_ns();
    buf.seq = fb.source_task_id();
    Ok(buf)
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
    use crate::clear_coordination::ClearOutcome;
    use crate::clear_estop_transaction::{ClearTxnStage, ClearTxnStore};
    use crate::latch_writer::{
        PersistShared, WriterConfig, WriterHandle, WriterHook, WriterPairing, WriterStop,
    };
    use crate::rr_admission::{
        CheckedLimits, ClassCaps, PeerLedger, RrRequestClass, Slot, SlotIdentity,
    };
    use iceoryx2::prelude::NodeBuilder;
    use iceoryx2::service::ipc::Service as IxService;
    use robot_core::adapter::{
        AdapterInfo, AdapterKind, ControlMode as AdapterControlMode, HardRtAdapter,
        JointCommandView, JointLimits, JointStateBuf, ManipulatorAdapter, RtClass, RtSafeCtrl,
    };
    use robot_core::control_authority::{
        AuthorityInit, ControlAuthority, EnableResult, InhibitSet, SafetyOperation,
    };
    use robot_core::error::RobotError;
    use robot_core::time::{MockClock, MonoRawNs, SystemClock};
    use robot_msg::build_helpers::{build_estop_event, build_heartbeat, build_joint_command};
    use robot_msg::msg::{SideEffect, StaleAction};
    use robot_transport::iceoryx2::{RrProfile, TopicClass, publisher, subscriber};
    use robot_transport::{LatestCommandSlot, Publisher, RtSnapshotReader};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU8, AtomicU32};

    static TEST_SEQ: AtomicU32 = AtomicU32::new(0);
    const WAIT: Duration = Duration::from_secs(3);
    const LAYOUT: u64 = 0xCAFE_0001;

    fn command_body(
        mode: FbControlMode,
        targets: Option<&[f64]>,
        deadline_ns: u64,
        on_timeout: StaleAction,
    ) -> Vec<u8> {
        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let target = targets.map(|values| builder.create_vector(values));
        let command = FbJointCommand::create(
            &mut builder,
            &robot_msg::msg::JointCommandArgs {
                layout_id: LAYOUT,
                mode,
                target,
                deadline_ns,
                priority: 10,
                source_task_id: 7,
                on_timeout,
            },
        );
        builder.finish(command, None);
        builder.finished_data().to_vec()
    }

    fn parse_command_body(
        body: &[u8],
        stamp_ns: u64,
        now: MonoRawNs,
    ) -> Result<JointCommandBuf, JointCommandParseError> {
        let command = flatbuffers::root::<FbJointCommand<'_>>(body).unwrap();
        parse_joint_command(&command, stamp_ns, now)
    }

    #[test]
    fn velocity_command_decodes_signed_and_zero_targets_into_only_velocities() {
        let body = command_body(
            FbControlMode::Velocity,
            Some(&[1.5, -2.0, 0.0, -0.0]),
            200,
            StaleAction::Zero,
        );
        let command = parse_command_body(&body, 100, MonoRawNs(150)).unwrap();
        let mut expected = [0.0_f32; MAX_JOINTS];
        expected[..4].copy_from_slice(&[1.5, -2.0, 0.0, -0.0]);
        assert_eq!(
            command.velocities.map(f32::to_bits),
            expected.map(f32::to_bits)
        );
        assert_eq!(command.positions.map(f32::to_bits), [0_u32; MAX_JOINTS]);
        assert_eq!(command.torques.map(f32::to_bits), [0_u32; MAX_JOINTS]);
        assert_eq!(command.joint_count, 4);
        assert_eq!(command.mode, ControlModeRepr::Velocity);
        assert_eq!(
            (command.stamp_ns, command.deadline_ns, command.seq),
            (100, 200, 7)
        );
    }

    #[test]
    fn velocity_command_accepts_f32_endpoints_and_finite_underflow() {
        let limit = f64::from(f32::MAX);
        let body = command_body(
            FbControlMode::Velocity,
            Some(&[limit, -limit, f64::MIN_POSITIVE]),
            1,
            StaleAction::Zero,
        );
        let command = parse_command_body(&body, 0, MonoRawNs(0)).unwrap();
        assert_eq!(command.velocities[0].to_bits(), f32::MAX.to_bits());
        assert_eq!(command.velocities[1].to_bits(), (-f32::MAX).to_bits());
        assert_eq!(command.velocities[2].to_bits(), 0.0_f32.to_bits());
        assert!(command.velocities.iter().all(|value| value.is_finite()));
    }

    #[test]
    fn velocity_command_rejects_nonfinite_and_out_of_f32_range_targets() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let body = command_body(
                FbControlMode::Velocity,
                Some(&[1.0, value]),
                200,
                StaleAction::Zero,
            );
            assert_eq!(
                parse_command_body(&body, 100, MonoRawNs(150)),
                Err(JointCommandParseError::NonFiniteVelocityTarget),
            );
        }
        let just_outside = f64::from(f32::MAX) * (1.0 + f64::EPSILON);
        for value in [just_outside, -just_outside, f64::MAX, f64::MIN] {
            let body = command_body(
                FbControlMode::Velocity,
                Some(&[1.0, value]),
                200,
                StaleAction::Zero,
            );
            assert_eq!(
                parse_command_body(&body, 100, MonoRawNs(150)),
                Err(JointCommandParseError::VelocityTargetOutOfRange),
            );
        }
    }

    #[test]
    fn velocity_command_requires_nonempty_targets_without_truncating() {
        for targets in [None, Some(&[] as &[f64])] {
            let body = command_body(FbControlMode::Velocity, targets, 200, StaleAction::Zero);
            assert_eq!(
                parse_command_body(&body, 100, MonoRawNs(150)),
                Err(JointCommandParseError::EmptyVelocityTargets),
            );
        }
        let values = [1.0_f64; MAX_JOINTS + 1];
        let body = command_body(
            FbControlMode::Velocity,
            Some(&values),
            200,
            StaleAction::Zero,
        );
        assert_eq!(
            parse_command_body(&body, 100, MonoRawNs(150)),
            Err(JointCommandParseError::TooManyVelocityTargets),
        );
        let body = command_body(
            FbControlMode::Velocity,
            Some(&values[..MAX_JOINTS]),
            200,
            StaleAction::Zero,
        );
        let command = parse_command_body(&body, 100, MonoRawNs(150)).unwrap();
        assert_eq!(usize::from(command.joint_count), MAX_JOINTS);
        assert_eq!(
            command.velocities.map(f32::to_bits),
            [1.0_f32.to_bits(); MAX_JOINTS]
        );
    }

    #[test]
    fn velocity_command_enforces_absolute_window_with_zero_clock_origin() {
        for (stamp_ns, now_ns, deadline_ns) in [
            (0, 0, 1),
            (100, 100, 101),
            (100, 199, 200),
            (0, 0, u64::MAX),
            (u64::MAX - 2, u64::MAX - 1, u64::MAX),
        ] {
            let body = command_body(
                FbControlMode::Velocity,
                Some(&[0.0]),
                deadline_ns,
                StaleAction::Zero,
            );
            assert!(parse_command_body(&body, stamp_ns, MonoRawNs(now_ns)).is_ok());
        }
        // 绝对期限不推断发送端历史；包括回绕后不成立的区间，均按同一接收门拒绝。
        for (stamp_ns, now_ns, deadline_ns) in [
            (0, 0, 0),
            (100, 200, 200),
            (100, 201, 200),
            (151, 150, 200),
            (100, 100, 100),
            (100, 100, 99),
            (u64::MAX - 1, u64::MAX, 1),
        ] {
            let body = command_body(
                FbControlMode::Velocity,
                Some(&[0.0]),
                deadline_ns,
                StaleAction::Zero,
            );
            assert_eq!(
                parse_command_body(&body, stamp_ns, MonoRawNs(now_ns)),
                Err(JointCommandParseError::InvalidVelocityWindow),
            );
        }
    }

    #[test]
    fn velocity_command_requires_zero_timeout_policy() {
        for on_timeout in [StaleAction::Hold, StaleAction::Stop, StaleAction(99)] {
            let body = command_body(FbControlMode::Velocity, Some(&[0.0]), 200, on_timeout);
            assert_eq!(
                parse_command_body(&body, 100, MonoRawNs(150)),
                Err(JointCommandParseError::UnsupportedVelocityTimeout),
            );
        }
    }

    #[test]
    fn velocity_validation_preserves_other_modes_original_decode_contract() {
        let values = [1.5_f64; MAX_JOINTS + 1];
        for (mode, expected) in [
            (FbControlMode::Position, ControlModeRepr::Position),
            (FbControlMode::ServoPosition, ControlModeRepr::ServoPosition),
            (FbControlMode::Torque, ControlModeRepr::Torque),
            (FbControlMode::Stop, ControlModeRepr::Stop),
            (FbControlMode(99), ControlModeRepr::Stop),
        ] {
            let body = command_body(mode, Some(&values), 0, StaleAction::Hold);
            let command = parse_command_body(&body, 100, MonoRawNs(0)).unwrap();
            assert_eq!(command.mode, expected);
            assert_eq!(usize::from(command.joint_count), MAX_JOINTS);
            assert_eq!(
                command.positions.map(f32::to_bits),
                [1.5_f32.to_bits(); MAX_JOINTS]
            );
            assert_eq!(command.velocities.map(f32::to_bits), [0_u32; MAX_JOINTS]);
            assert_eq!(command.torques.map(f32::to_bits), [0_u32; MAX_JOINTS]);
            let body = command_body(mode, None, 0, StaleAction::Hold);
            assert_eq!(
                parse_command_body(&body, 100, MonoRawNs(0))
                    .unwrap()
                    .joint_count,
                0
            );
        }
    }

    #[test]
    fn poll_command_velocity_rejections_preserve_slot_and_counters_over_real_pubsub() {
        let id = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let topic = format!("/b1_velocity_{}_{id}/cmd", std::process::id());
        let node = NodeBuilder::new().create::<IxService>().unwrap();
        let mut receiver =
            subscriber::<markers::JointCommand>(&node, &topic, TopicClass::S0Command).unwrap();
        let mut transmitter =
            publisher::<markers::JointCommand>(&node, &topic, TopicClass::S0Command).unwrap();
        // 首次空读取同步刷新接收连接；后续计数与读槽断言验证实际投递生效。
        assert!(receiver.latest().unwrap().is_none());
        transmitter.update_connections().unwrap();
        let initial_body =
            command_body(FbControlMode::Position, Some(&[3.0]), 0, StaleAction::Hold);
        let initial = parse_command_body(&initial_body, 50, MonoRawNs(0)).unwrap();
        let (writer, reader) = LatestCommandSlot::new(initial);
        let clock = MockClock::new(MonoRawNs(150));
        let metrics = IpcThreadMetrics::default();
        let overlong = [1.0_f64; MAX_JOINTS + 1];
        let cases: [(Option<&[f64]>, u64, u64, StaleAction); 8] = [
            (Some(&[1.0, f64::NAN]), 100, 200, StaleAction::Zero),
            (Some(&[f64::MAX]), 100, 200, StaleAction::Zero),
            (None, 100, 200, StaleAction::Zero),
            (Some(&overlong), 100, 200, StaleAction::Zero),
            (Some(&[1.0]), 100, 150, StaleAction::Zero),
            (Some(&[1.0]), 151, 200, StaleAction::Zero),
            (Some(&[1.0]), 100, 0, StaleAction::Zero),
            (Some(&[1.0]), 100, 200, StaleAction::Hold),
        ];
        let mut rejected = 0;
        for (targets, stamp_ns, deadline_ns, on_timeout) in cases {
            let body = command_body(FbControlMode::Velocity, targets, deadline_ns, on_timeout);
            let mut sample = transmitter.loan(1024 + body.len()).unwrap();
            sample.set_stamp_ns(stamp_ns);
            sample.set_body_bytes(&body);
            sample.publish().unwrap();
            poll_command(&mut receiver, &writer, LAYOUT, &clock, &metrics);
            rejected += 1;
            assert_eq!(**reader.load(), initial);
            assert_eq!(
                metrics.command_parse_errors.load(Ordering::Relaxed),
                rejected
            );
            assert_eq!(metrics.commands_applied.load(Ordering::Relaxed), 0);
            assert_eq!(metrics.unknown_layout_count.load(Ordering::Relaxed), 0);
        }
        let body = command_body(
            FbControlMode::Velocity,
            Some(&[-2.0, 0.0]),
            200,
            StaleAction::Zero,
        );
        let mut sample = transmitter.loan(1024 + body.len()).unwrap();
        sample.set_stamp_ns(100);
        sample.set_body_bytes(&body);
        sample.publish().unwrap();
        poll_command(&mut receiver, &writer, LAYOUT, &clock, &metrics);
        let expected = parse_command_body(&body, 100, clock.now()).unwrap();
        assert_eq!(**reader.load(), expected);
        assert_eq!(metrics.commands_applied.load(Ordering::Relaxed), 1);
        assert_eq!(
            metrics.command_parse_errors.load(Ordering::Relaxed),
            rejected
        );
        assert!(receiver.latest().unwrap().is_none());
    }

    fn wait_until(mut condition: impl FnMut() -> bool, budget: Duration) {
        let end = std::time::Instant::now() + budget;
        while !condition() {
            assert!(
                std::time::Instant::now() < end,
                "等待真实 IPC/设备/文件事实超时"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn healthy_guards() -> Guards {
        Guards {
            rt_serial: false,
            estop_latched: false,
            adapter_connected: true,
            calibration_loaded: true,
            joint_state_valid: true,
            power_ok: true,
            heartbeat_ok: false,
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

    struct WriterRunning(AtomicBool);
    impl WriterStop for WriterRunning {
        fn running(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
    }
    struct FileHook {
        pause: AtomicBool,
        reached: AtomicBool,
        release: AtomicBool,
        fail_backup_once: AtomicU8,
        backup: PathBuf,
    }
    impl WriterHook for Arc<FileHook> {
        fn before_step(&mut self, stage: ClearTxnStage) {
            if stage == ClearTxnStage::CreateMarker && self.pause.swap(false, Ordering::AcqRel) {
                self.reached.store(true, Ordering::Release);
                wait_until(|| self.release.load(Ordering::Acquire), WAIT);
            }
            // 只替换文件系统夹具：真正 RenameBackup 系统调用失败，后续真实 repair 可重试。
            if stage == ClearTxnStage::RenameBackup
                && self
                    .fail_backup_once
                    .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                std::fs::remove_file(&self.backup).unwrap();
                std::fs::create_dir(&self.backup).unwrap();
                std::fs::write(self.backup.join("nonempty"), b"x").unwrap();
            } else if self
                .fail_backup_once
                .compare_exchange(2, 3, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                std::fs::remove_file(self.backup.join("nonempty")).unwrap();
                std::fs::remove_dir(&self.backup).unwrap();
                std::fs::write(&self.backup, b"1\n").unwrap();
            }
        }
    }

    /// 纯软件设备模型；错误来自实际 enable_rt 调用，不直接伪写 completed/权限位。
    struct TestAdapter {
        limits: JointLimits,
        enabled: bool,
        fail: bool,
    }
    impl ManipulatorAdapter for TestAdapter {
        fn info(&self) -> AdapterInfo {
            AdapterInfo {
                name: "ipc_test",
                kind: AdapterKind::Manipulator,
                rt_class: RtClass::HardRT,
                vendor_sdk: false,
            }
        }
        fn connect(&mut self) -> Result<(), RobotError> {
            Ok(())
        }
        fn disconnect(&mut self) -> Result<(), RobotError> {
            self.enabled = false;
            Ok(())
        }
        fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError> {
            out.joint_count = 1;
            Ok(())
        }
        fn write_command(&mut self, _: &JointCommandView<'_>) -> Result<(), RobotError> {
            Ok(())
        }
        fn set_mode(&mut self, _: AdapterControlMode) -> Result<(), RobotError> {
            Ok(())
        }
        fn enable(&mut self, on: bool) -> Result<(), RobotError> {
            self.enabled = on;
            Ok(())
        }
        fn safety_stop(&mut self) -> Result<(), RobotError> {
            self.enabled = false;
            Ok(())
        }
        fn limits(&self) -> &JointLimits {
            &self.limits
        }
    }
    impl RtSafeCtrl for TestAdapter {
        fn safety_stop_rt(&mut self) -> Result<(), RobotError> {
            self.enabled = false;
            Ok(())
        }
        fn enable_rt(&mut self, on: bool) -> Result<(), RobotError> {
            if self.fail {
                return Err(RobotError::HardwareFault("IPC 设备调用失败夹具"));
            }
            self.enabled = on;
            Ok(())
        }
        fn set_mode_rt(&mut self, _: AdapterControlMode) -> Result<(), RobotError> {
            Ok(())
        }
    }
    #[allow(
        clippy::needless_pass_by_value,
        reason = "测试线程按值持有原控制端和停止标记，直到真实线程返回"
    )]
    fn executor_loop(
        mut adapter: impl HardRtAdapter,
        ctrl: Arc<crate::hard_rt_ctrl::HardRtCtrlInner>,
        mode: Arc<AtomicU8>,
        stop: Arc<AtomicBool>,
    ) {
        let mut active: Option<crate::hard_rt_ctrl::EnableFinishPending> = None;
        let mut unstarted = None;
        while !stop.load(Ordering::Acquire) {
            // 与真实 HardRT 相同：先领取 A2 公告，仍独立取旧停止槽，两来源只做一次真实 Stop。
            let claimed_estop = ctrl.claim_serial_estop();
            let emergency = ctrl.emergency_action.swap(0, Ordering::AcqRel);
            if claimed_estop || emergency == robot_core::executor_ctrl::emergency_codes::SAFETY_STOP
            {
                let observation = ctrl
                    .authority()
                    .begin_safety_observation(SafetyOperation::ConfirmOff);
                adapter.safety_stop_rt().unwrap();
                if let Some(active) = &mut active {
                    active.record_hard_rt_stop_returned();
                }
                let _ = ctrl.authority().complete_safety_observation(observation);
            }
            if let Some(pending) = active.take() {
                match crate::hard_rt_ctrl::finish_enable_owned(pending, &ctrl) {
                    Ok(finished) => {
                        finished.publish_after_compensation(&ctrl, |_, compensation| {
                            if compensation == crate::hard_rt_ctrl::EnableCompensation::Required {
                                ctrl.emergency_action.store(
                                    robot_core::executor_ctrl::emergency_codes::SAFETY_STOP,
                                    Ordering::Release,
                                );
                            }
                        });
                    }
                    Err(pending) => active = Some(pending),
                }
            }
            if active.is_none() {
                match mode.load(Ordering::Acquire) {
                    1 => {
                        let pending = match unstarted.take() {
                            Some(pending) => Some(pending),
                            None => ctrl
                                .decode_pending(ctrl.pending_normal.swap(0, Ordering::AcqRel))
                                .unwrap(),
                        };
                        if let Some(pending) = pending {
                            active = crate::rt_thread::execute_owned_normal(
                                &mut adapter,
                                pending,
                                &ctrl,
                            );
                        }
                    }
                    2 => {
                        // 已真实从 pending 槽取走 seq，暂不取启动 view。原件由此线程持有。
                        let raw = ctrl.pending_normal.swap(0, Ordering::AcqRel);
                        if let Some(pending) = ctrl.decode_pending(raw).unwrap() {
                            unstarted = Some(pending);
                            mode.store(3, Ordering::Release);
                        }
                    }
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
        // 若真实 finish 尚被拒，原 Execution 不因测试线程退出丢失。
        let unfinished = active.is_some() || unstarted.is_some();
        let _held_until_process_exit = std::mem::ManuallyDrop::new((active, unstarted));
        assert!(!unfinished, "执行器退出时原 Enable 尚未真 finish");
    }

    struct Routes {
        ledger: PeerLedger<u32>,
        slots: HashMap<SlotIdentity, Slot<u32>>,
        next_peer: u32,
    }
    impl Routes {
        fn new(limits: &CheckedLimits) -> Self {
            Self {
                ledger: PeerLedger::new(limits).unwrap(),
                slots: HashMap::new(),
                next_peer: 0,
            }
        }
        fn admit(&mut self, class: RrRequestClass) -> SlotIdentity {
            self.next_peer += 1;
            let slot = self.ledger.admit(self.next_peer, class).unwrap();
            let identity = slot.identity();
            assert!(self.slots.insert(identity, slot).is_none());
            identity
        }
        fn retired(&mut self, identity: SlotIdentity) {
            self.ledger
                .release(self.slots.remove(&identity).expect("回复匹配原已准入 Slot"))
                .unwrap();
        }
    }

    /// 一个 fixture 同时拥有 Node、真实唯一 writer/配对锁、IPC 与软件执行器。
    /// 普通动作调用生产 execute_owned_normal；不声称本夹具覆盖完整 RT 调度线程。
    struct Rig {
        ipc: Option<IpcThreadHandle>,
        writer: Option<WriterHandle>,
        executor: Option<JoinHandle<()>>,
        executor_mode: Arc<AtomicU8>,
        executor_stop: Arc<AtomicBool>,
        writer_stop: Arc<WriterRunning>,
        inner: Arc<crate::hard_rt_ctrl::HardRtCtrlInner>,
        boot: Arc<crate::boot_supervisor::BootShared>,
        shared: Arc<PersistShared>,
        clock: Arc<dyn RobotClock>,
        hook: Arc<FileHook>,
        cmd_reader: robot_transport::LatestCommandReader<JointCommandBuf>,
        state: RtSnapshotReader<S0StateSnapshot>,
        command: robot_transport::iceoryx2::Iceoryx2Publisher<markers::JointCommand>,
        heartbeat: robot_transport::iceoryx2::Iceoryx2Publisher<markers::Heartbeat>,
        estop: robot_transport::iceoryx2::Iceoryx2Publisher<markers::EStopEvent>,
        watchdog: Sender<Event>,
        clear_tx: Option<[Sender<ClearEStopRrRequest>; 2]>,
        clear_rx: Receiver<ClearEStopRrResponse>,
        clear_routes: Option<Routes>,
        set_tx: Option<[Sender<SetEnabledRrRequest>; 2]>,
        set_rx: Receiver<SetEnabledRrResponse>,
        set_routes: Option<Routes>,
        latch: PathBuf,
        standby: Option<crate::latch_writer::StandbyStore>,
        lock: Option<crate::latch_lock::LatchTargetLock>,
        expected_snapshot_exhaustion: bool,
        directory: Option<tempfile::TempDir>,
        _node: iceoryx2::prelude::Node<IxService>,
    }
    #[derive(Clone, Copy)]
    enum RigProbe {
        Binding(IpcBindError),
        NarrowResponses,
    }

    impl Rig {
        fn new(state: S0State, guards: Guards, safety: OutputSafetyState, latched: bool) -> Self {
            Self::with_clock(
                state,
                guards,
                safety,
                latched,
                Arc::new(SystemClock::new()),
                None,
                false,
            )
        }
        #[allow(clippy::too_many_arguments)]
        fn with_clock(
            initial_state: S0State,
            guards: Guards,
            safety: OutputSafetyState,
            latched: bool,
            clock: Arc<dyn RobotClock>,
            injected: Option<(
                RtSnapshotPublisher<S0StateSnapshot>,
                RtSnapshotReader<S0StateSnapshot>,
            )>,
            fail_device: bool,
        ) -> Self {
            Self::with_probe(
                initial_state,
                guards,
                safety,
                latched,
                clock,
                injected,
                fail_device,
                None,
            )
        }
        #[allow(clippy::too_many_arguments)]
        #[allow(
            clippy::too_many_lines,
            reason = "测试夹具在一个拥有出口安装真实配对、线程与关闭责任，完整保留失败收尾"
        )]
        fn with_probe(
            initial_state: S0State,
            mut guards: Guards,
            safety: OutputSafetyState,
            latched: bool,
            clock: Arc<dyn RobotClock>,
            injected: Option<(
                RtSnapshotPublisher<S0StateSnapshot>,
                RtSnapshotReader<S0StateSnapshot>,
            )>,
            fail_device: bool,
            probe: Option<RigProbe>,
        ) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let latch = directory.path().join("estop_latch");
            let initial = if latched { b"1\n" } else { b"0\n" };
            std::fs::write(&latch, initial).unwrap();
            std::fs::write(latch.with_file_name("estop_latch.bak"), initial).unwrap();
            let lock = crate::latch_lock::acquire_latch_lock(&latch).unwrap();
            let base = clock.now();
            let open = || ClearTxnStore::open(&latch, "ipc_test", "bus").unwrap();
            let (slot, standby) = WriterPairing::new(open(), open(), &lock, base)
                .unwrap()
                .into_parts();
            let shared = Arc::clone(slot.shared());
            let (persist, endpoint) = crate::latch_writer::mailbox(Arc::clone(&shared));
            let writer_stop = Arc::new(WriterRunning(AtomicBool::new(true)));
            let hook = Arc::new(FileHook {
                pause: AtomicBool::new(false),
                reached: AtomicBool::new(false),
                release: AtomicBool::new(false),
                fail_backup_once: AtomicU8::new(0),
                backup: latch.with_file_name("estop_latch.bak"),
            });
            let writer = crate::latch_writer::spawn_writer(
                slot,
                endpoint,
                WriterConfig::default(),
                writer_stop.clone(),
                Arc::clone(&clock),
                Box::new(Arc::clone(&hook)),
            )
            .unwrap();
            let authority = Arc::new(
                ControlAuthority::new(AuthorityInit::new(latched, safety, InhibitSet::EMPTY))
                    .unwrap(),
            );
            let mut adapter = TestAdapter {
                limits: JointLimits::default(),
                enabled: false,
                fail: false,
            };
            if matches!(initial_state, S0State::Enabled { .. }) {
                // 运行态前提也经真实 A2，不能直接改 disabled/phase/safety 原子位。
                let (owner, view) = authority.reserve_enable().unwrap();
                let execution = authority.start_enable(view).unwrap();
                adapter.enable_rt(true).unwrap();
                authority
                    .finish_enable(execution, EnableResult::Succeeded)
                    .unwrap();
                authority.commit_enable(owner).unwrap();
            }
            adapter.fail = fail_device;
            guards.estop_latched = latched;
            let inner = Arc::new(crate::hard_rt_ctrl::HardRtCtrlInner::new(Arc::clone(
                &authority,
            )));
            let ctrl = Arc::new(crate::hard_rt_ctrl::HardRtCtrlClient::new(Arc::clone(
                &inner,
            )));
            let boot = Arc::new(crate::boot_supervisor::BootShared::new(base));
            let permit = boot
                .install_runtime_for_test(authority, Arc::clone(&shared))
                .unwrap_or_else(|refused| panic!("同对运行绑定拒绝: {:?}", refused.reason));
            drop(permit); // 本 fixture 不启动 READY 监督线程；只安装同一对真实实例。
            let id = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
            let prefix = format!("/w3_ipc_{}_{}", std::process::id(), id);
            let topics = [
                format!("{prefix}/cmd"),
                format!("{prefix}/hb"),
                format!("{prefix}/estop"),
            ];
            let node = NodeBuilder::new().create::<IxService>().unwrap();
            let command = publisher(&node, &topics[0], TopicClass::S0Command).unwrap();
            let heartbeat = publisher(&node, &topics[1], TopicClass::Heartbeat).unwrap();
            let estop = publisher(&node, &topics[2], TopicClass::SafetyEvent).unwrap();
            let factory_calls = Arc::new(AtomicU32::new(0));
            let called = Arc::clone(&factory_calls);
            let factory = move || {
                called.fetch_add(1, Ordering::AcqRel);
                let node = NodeBuilder::new()
                    .create::<IxService>()
                    .map_err(IpcInitError::Node)?;
                let command =
                    subscriber(&node, &topics[0], TopicClass::S0Command).map_err(|error| {
                        IpcInitError::Subscriber {
                            topic: topics[0].clone(),
                            error,
                        }
                    })?;
                let heartbeat =
                    subscriber(&node, &topics[1], TopicClass::Heartbeat).map_err(|error| {
                        IpcInitError::Subscriber {
                            topic: topics[1].clone(),
                            error,
                        }
                    })?;
                let estop =
                    subscriber(&node, &topics[2], TopicClass::SafetyEvent).map_err(|error| {
                        IpcInitError::Subscriber {
                            topic: topics[2].clone(),
                            error,
                        }
                    })?;
                Ok(OwnedIpcPorts::new(
                    IpcSubscribers {
                        command,
                        heartbeat,
                        estop,
                    },
                    node,
                ))
            };
            let (cmd_writer, cmd_reader) = LatestCommandSlot::new(JointCommandBuf::default());
            let injected_snapshot = injected.is_some();
            let (state_pub, state) =
                injected.unwrap_or_else(|| RtSnapshotPublisher::new(S0StateSnapshot::default()));
            let (watchdog, watchdog_rx) = unbounded();
            let limits =
                CheckedLimits::try_from_limits(&RrProfile::S0_CONTROL.validate().unwrap()).unwrap();
            let caps = ClassCaps::from_checked(&limits);
            let (clear_high, clear_high_rx) = crossbeam_channel::bounded(caps.restore + caps.lower);
            let (clear_normal, clear_normal_rx) = crossbeam_channel::bounded(caps.normal);
            // 窄1容量仅用于内部 outbox Full 边界；生产 Profile 与原请求信用仍为 C8/L4。
            let response_capacity = if matches!(probe, Some(RigProbe::NarrowResponses)) {
                1
            } else {
                caps.total
            };
            let (clear_response, clear_rx) = crossbeam_channel::bounded(response_capacity);
            let (set_high, set_high_rx) = crossbeam_channel::bounded(caps.restore + caps.lower);
            let (set_normal, set_normal_rx) = crossbeam_channel::bounded(caps.normal);
            let (set_response, set_rx) = crossbeam_channel::bounded(response_capacity);
            let mut inputs = IpcSpawnInputs {
                cfg: IpcThreadConfig {
                    poll_interval_ns: 500_000,
                    initial_guards: guards,
                    initial_state,
                    authoritative_layout: LAYOUT,
                },
                clock: Arc::clone(&clock),
                subscribers_factory: factory,
                cmd_writer,
                state_pub,
                watchdog_rx,
                ctrl,
                boot: Arc::clone(&boot),
                persist,
                rr_channels: Some(RrChannels {
                    request_rx: clear_high_rx,
                    normal_request_rx: clear_normal_rx,
                    response_tx: clear_response,
                }),
                set_enabled_rr_channels: Some(SetEnabledRrChannels {
                    request_rx: set_high_rx,
                    normal_request_rx: set_normal_rx,
                    response_tx: set_response,
                }),
                stop: Arc::new(AtomicBool::new(false)),
            };
            if let Some(RigProbe::Binding(expected)) = probe {
                let bad_boot = Arc::new(crate::boot_supervisor::BootShared::new(base));
                match expected {
                    IpcBindError::MissingBinding => {}
                    IpcBindError::AuthorityMismatch => {
                        let other = Arc::new(
                            ControlAuthority::new(AuthorityInit::new(
                                latched,
                                safety,
                                InhibitSet::EMPTY,
                            ))
                            .unwrap(),
                        );
                        drop(
                            bad_boot
                                .install_runtime_for_test(other, Arc::clone(&shared))
                                .unwrap_or_else(|refused| {
                                    panic!("夹具绑定失败: {:?}", refused.reason)
                                }),
                        );
                    }
                    IpcBindError::WriterSharedMismatch => {
                        let other = Arc::new(PersistShared::new(base).unwrap());
                        drop(
                            bad_boot
                                .install_runtime_for_test(
                                    Arc::clone(inputs.ctrl.authority()),
                                    other,
                                )
                                .unwrap_or_else(|refused| {
                                    panic!("夹具绑定失败: {:?}", refused.reason)
                                }),
                        );
                    }
                    IpcBindError::StartDomainMismatch => panic!("合法 ctrl 构造不能外造错域 slot"),
                }
                inputs.boot = Arc::clone(&bad_boot);
                let original_ctrl = Arc::clone(&inputs.ctrl);
                let original_stop = Arc::clone(&inputs.stop);
                let original_watchdog = inputs.watchdog_rx.clone();
                let original_clear = inputs.rr_channels.as_ref().unwrap();
                let clear_receivers = [
                    original_clear.request_rx.clone(),
                    original_clear.normal_request_rx.clone(),
                ];
                let original_set = inputs.set_enabled_rr_channels.as_ref().unwrap();
                let set_receivers = [
                    original_set.request_rx.clone(),
                    original_set.normal_request_rx.clone(),
                ];
                inputs = match spawn_ipc_thread(inputs) {
                    Err(IpcSpawnError::Binding { inputs, reason }) => {
                        assert_eq!(reason, expected);
                        inputs
                    }
                    Ok(handle) => {
                        handle.request_stop();
                        let end = std::time::Instant::now() + WAIT;
                        while !handle.is_finished() {
                            if std::time::Instant::now() >= end {
                                std::process::abort();
                            }
                            thread::sleep(Duration::from_millis(1));
                        }
                        let _original_exit = std::mem::ManuallyDrop::new(handle.join());
                        panic!("错误绑定竟然真实启动 IPC，已回收并保原退出");
                    }
                    Err(error) => panic!("预期绑定拒绝，实际 {error:?}"),
                };
                assert_eq!(factory_calls.load(Ordering::Acquire), 0);
                assert!(Arc::ptr_eq(&inputs.boot, &bad_boot));
                assert!(Arc::ptr_eq(&inputs.ctrl, &original_ctrl));
                assert!(Arc::ptr_eq(&inputs.clock, &clock));
                assert!(Arc::ptr_eq(&inputs.stop, &original_stop));
                assert!(Arc::ptr_eq(inputs.persist.shared(), &shared));
                assert!(inputs.watchdog_rx.same_channel(&original_watchdog));
                let clear = inputs.rr_channels.as_ref().unwrap();
                let set = inputs.set_enabled_rr_channels.as_ref().unwrap();
                assert!(
                    clear.request_rx.same_channel(&clear_receivers[0])
                        && clear.normal_request_rx.same_channel(&clear_receivers[1])
                );
                assert!(
                    set.request_rx.same_channel(&set_receivers[0])
                        && set.normal_request_rx.same_channel(&set_receivers[1])
                );
                assert_eq!(inputs.cfg.authoritative_layout, LAYOUT);
                // 返原唯一写端仍驱动原 reader；不以新建替身证明归还。
                inputs.cmd_writer.store(JointCommandBuf {
                    seq: 91,
                    ..JointCommandBuf::default()
                });
                assert_eq!(cmd_reader.load().seq, 91);
                inputs
                    .state_pub
                    .publish(&S0StateSnapshot::new(initial_state, guards, 123))
                    .unwrap();
                assert_eq!(state.load().stamp_ns, 123);
                assert_eq!(
                    inputs.rr_channels.as_ref().unwrap().response_tx.capacity(),
                    clear_rx.capacity()
                );
                assert_eq!(
                    inputs
                        .set_enabled_rr_channels
                        .as_ref()
                        .unwrap()
                        .response_tx
                        .capacity(),
                    set_rx.capacity()
                );
                // 只修正拒绝的绑定；同 factory/client/通道/写端继续交实际 spawn。
                inputs.boot = Arc::clone(&boot);
            }
            let ipc = spawn_ipc_thread(inputs).unwrap();
            let executor_mode = Arc::new(AtomicU8::new(0));
            let executor_stop = Arc::new(AtomicBool::new(false));
            let executor = {
                let ctrl = Arc::clone(&inner);
                let mode = Arc::clone(&executor_mode);
                let stop = Arc::clone(&executor_stop);
                thread::spawn(move || executor_loop(adapter, ctrl, mode, stop))
            };
            let mut rig = Self {
                ipc: Some(ipc),
                writer: Some(writer),
                executor: Some(executor),
                executor_mode,
                executor_stop,
                writer_stop,
                inner,
                boot,
                shared,
                clock,
                hook,
                cmd_reader,
                state,
                command,
                heartbeat,
                estop,
                watchdog,
                clear_tx: Some([clear_high, clear_normal]),
                clear_rx,
                clear_routes: Some(Routes::new(&limits)),
                set_tx: Some([set_high, set_normal]),
                set_rx,
                set_routes: Some(Routes::new(&limits)),
                latch,
                standby: Some(standby),
                lock: Some(lock),
                directory: Some(directory),
                expected_snapshot_exhaustion: injected_snapshot,
                _node: node,
            };
            wait_until(
                || !matches!(rig.ipc.as_mut().unwrap().poll_ready(), ReadyState::Pending),
                WAIT,
            );
            match rig.ipc.as_mut().unwrap().poll_ready() {
                ReadyState::Ready => {}
                ReadyState::Failed(IpcInitError::InitialPublish(_)) if injected_snapshot => {}
                other => panic!("IPC 实际初始化失败: {other:?}"),
            }
            assert_eq!(factory_calls.load(Ordering::Acquire), 1);
            rig
        }
        fn handle(&self) -> &IpcThreadHandle {
            self.ipc.as_ref().unwrap()
        }
        fn execute(&self) {
            self.executor_mode.store(1, Ordering::Release);
        }
        fn claim_without_start(&self) {
            self.executor_mode.store(2, Ordering::Release);
            wait_until(|| self.executor_mode.load(Ordering::Acquire) == 3, WAIT);
        }
        fn heartbeat(&mut self) {
            let body = build_heartbeat(2, 1, 50.0, 1_000_000);
            let mut writer = self.heartbeat.loan(1024 + body.len()).unwrap();
            writer.set_body_bytes(&body);
            writer.publish().unwrap();
        }
        fn estop(&mut self) {
            let now = self.clock.now().0;
            let body = build_estop_event("ipc_test", now, "rpc", true);
            let mut writer = self.estop.loan(1024 + body.len()).unwrap();
            writer.set_stamp_ns(now);
            writer.set_body_bytes(&body);
            writer.publish().unwrap();
        }
        fn command(&mut self, seq: u64, layout: u64, positions: &[f64]) {
            let body = build_joint_command(
                layout,
                robot_msg::msg::ControlMode::Position,
                positions,
                u64::MAX,
                10,
                StaleAction::Hold,
                seq,
            );
            let mut writer = self.command.loan(1024 + body.len()).unwrap();
            writer.set_seq(seq);
            writer.set_body_bytes(&body);
            writer.publish().unwrap();
        }
        fn clear(&mut self, id: &str, dry_run: bool, deadline_ns: u64) {
            let route = self
                .clear_routes
                .as_mut()
                .unwrap()
                .admit(RrRequestClass::of_clear(dry_run));
            self.clear_tx.as_ref().unwrap()[usize::from(dry_run)]
                .send(ClearEStopRrRequest {
                    route,
                    request_id: id.into(),
                    dry_run,
                    confirmation: "CONFIRM".into(),
                    audit_reason: "IPC owned test".into(),
                    deadline_ns,
                })
                .unwrap();
        }
        fn clear_reply(&mut self) -> ClearEStopRrResponse {
            let response = self
                .clear_rx
                .recv_timeout(WAIT)
                .expect("真实 Clear 退休回复");
            self.clear_routes.as_mut().unwrap().retired(response.route);
            response
        }
        fn set(&mut self, id: &str, enabled: bool, deadline_ns: u64) {
            let route = self
                .set_routes
                .as_mut()
                .unwrap()
                .admit(RrRequestClass::of_set_enabled(false, enabled));
            self.set_tx.as_ref().unwrap()[0]
                .send(SetEnabledRrRequest {
                    route,
                    request_id: id.into(),
                    enabled,
                    dry_run: false,
                    deadline_ns,
                    audit_reason: "IPC owned test".into(),
                })
                .unwrap();
        }
        fn set_reply(&mut self) -> SetEnabledRrResponse {
            let response = self
                .set_rx
                .recv_timeout(WAIT)
                .expect("真实 Enable/Lower 退休回复");
            self.set_routes.as_mut().unwrap().retired(response.route);
            response
        }
        fn drain_replies(&mut self) {
            while let Ok(response) = self.clear_rx.try_recv() {
                self.clear_routes.as_mut().unwrap().retired(response.route);
            }
            while let Ok(response) = self.set_rx.try_recv() {
                self.set_routes.as_mut().unwrap().retired(response.route);
            }
        }
        fn close(&mut self) {
            if self.ipc.is_none() {
                return;
            }
            self.hook.release.store(true, Ordering::Release);
            self.handle()
                .metrics()
                .freeze_until_ns
                .store(0, Ordering::Release);
            self.execute();
            drop(self.clear_tx.take());
            drop(self.set_tx.take());
            if !self.handle().is_finished() {
                // 同实例真实 shutdown，停止输入后继续消费原退休责任；不是将测试槽清零。
                let _ = self.boot.arm_runtime_shutdown(self.clock.now());
                let end = std::time::Instant::now() + WAIT;
                while std::time::Instant::now() < end
                    && !self
                        .boot
                        .clear_retired_for(self.boot.runtime_words().generation())
                {
                    self.drain_replies();
                    thread::sleep(Duration::from_millis(1));
                }
            }
            self.handle().request_stop();
            self.writer_stop.0.store(false, Ordering::Release);
            self.executor_stop.store(true, Ordering::Release);
            let end = std::time::Instant::now() + WAIT;
            while !self.handle().is_finished()
                || !self.writer.as_ref().unwrap().is_finished()
                || !self.executor.as_ref().unwrap().is_finished()
            {
                if std::time::Instant::now() >= end {
                    eprintln!("IPC fixture 原 worker 未在测试退出预算内结束");
                    std::process::abort();
                }
                thread::sleep(Duration::from_millis(1));
            }
            let ipc = self.ipc.take().unwrap().join();
            let writer = self.writer.take().unwrap().join();
            let executor = self.executor.take().unwrap().join();
            self.drain_replies();
            let clean = ipc.as_ref().is_ok_and(IpcExit::is_quiescent)
                && writer.0.is_ok()
                && executor.is_ok()
                && self.clear_routes.as_ref().unwrap().slots.is_empty()
                && self.set_routes.as_ref().unwrap().slots.is_empty();
            let workers_ok = ipc.is_ok() && writer.0.is_ok() && executor.is_ok();
            let expected_exit = self.expected_snapshot_exhaustion
                && ipc.as_ref().is_ok_and(|exit| {
                    matches!(
                        exit.reason(),
                        IpcExitReason::Initialization(IpcInitError::InitialPublish(
                            robot_transport::SnapshotPublishError::VersionExhausted
                        )) | IpcExitReason::SnapshotPublish(
                            robot_transport::SnapshotPublishError::VersionExhausted
                        )
                    )
                });

            if !clean {
                // 初始化失败/故意未退休场景保原 exit、配对与锁，直到真进程退出。
                let _held_until_process_exit = std::mem::ManuallyDrop::new((
                    ipc,
                    writer,
                    executor,
                    self.standby.take(),
                    self.lock.take(),
                    self.directory.take(),
                    self.clear_routes.take(),
                    self.set_routes.take(),
                ));
            }
            assert!(
                workers_ok || thread::panicking(),
                "IPC 夹具真实 worker join 返回 panic"
            );
            assert!(
                clean || expected_exit || thread::panicking(),
                "IPC 夹具意外退出时仍有原退休责任，已保原件"
            );
        }
    }
    impl Drop for Rig {
        fn drop(&mut self) {
            self.close();
        }
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "测试按值消费唯一原回复，只提取其真实协调结果"
    )]
    fn clear_outcome(response: ClearEStopRrResponse) -> ClearOutcome {
        match response.payload {
            ClearEStopRrPayload::Coordination(outcome) => outcome,
            other => panic!("生产协调 typed 结果未抵达: {other:?}"),
        }
    }

    fn safe_rig(latched: bool) -> Rig {
        Rig::new(
            S0State::Safe,
            healthy_guards(),
            OutputSafetyState::TorqueOffConfirmed,
            latched,
        )
    }

    #[cfg(feature = "test-comm-fault-hook")]
    #[test]
    fn state_snapshot_version_exhaustion_on_first_publish_ends_the_ipc_worker() {
        let injected = RtSnapshotPublisher::new_with_version(
            S0StateSnapshot::default(),
            robot_transport::MAX_PUBLISHED_VERSION,
        )
        .unwrap();
        let rig = Rig::with_clock(
            S0State::Safe,
            healthy_guards(),
            OutputSafetyState::TorqueOffConfirmed,
            false,
            Arc::new(SystemClock::new()),
            Some(injected),
            false,
        );
        wait_until(|| rig.handle().is_finished(), WAIT);
        assert_eq!(
            rig.handle()
                .metrics()
                .snapshot_publish_errors
                .load(Ordering::Acquire),
            1
        );
        assert_eq!(
            rig.handle()
                .metrics()
                .ipc_tick_count
                .load(Ordering::Acquire),
            0
        );
        assert_eq!(rig.state.version(), robot_transport::MAX_PUBLISHED_VERSION);
        assert_eq!(rig.state.load(), S0StateSnapshot::default());
    }

    #[cfg(feature = "test-comm-fault-hook")]
    #[test]
    fn state_snapshot_version_exhaustion_inside_the_loop_ends_the_ipc_worker() {
        let injected = RtSnapshotPublisher::new_with_version(
            S0StateSnapshot::default(),
            robot_transport::MAX_PUBLISHED_VERSION - 2,
        )
        .unwrap();
        let rig = Rig::with_clock(
            S0State::Safe,
            healthy_guards(),
            OutputSafetyState::TorqueOffConfirmed,
            false,
            Arc::new(SystemClock::new()),
            Some(injected),
            false,
        );
        wait_until(|| rig.handle().is_finished(), WAIT);
        assert_eq!(
            rig.handle()
                .metrics()
                .snapshot_publish_errors
                .load(Ordering::Acquire),
            1
        );
        assert_eq!(
            rig.handle()
                .metrics()
                .ipc_tick_count
                .load(Ordering::Acquire),
            1
        );
        assert_eq!(rig.state.version(), robot_transport::MAX_PUBLISHED_VERSION);
        assert_eq!(rig.state.load().state, S0State::Safe);
    }

    #[test]
    fn spawn_binding_rejections_return_original_inputs_before_factory_call() {
        for reason in [
            IpcBindError::MissingBinding,
            IpcBindError::AuthorityMismatch,
            IpcBindError::WriterSharedMismatch,
        ] {
            let mut rig = Rig::with_probe(
                S0State::Safe,
                healthy_guards(),
                OutputSafetyState::TorqueOffConfirmed,
                true,
                Arc::new(SystemClock::new()),
                None,
                false,
                Some(RigProbe::Binding(reason)),
            );
            rig.clear("returned-original-clear", false, u64::MAX);
            let reply = rig.clear_reply();
            assert_eq!(reply.request_id, "returned-original-clear");
            assert!(matches!(clear_outcome(reply), ClearOutcome::Cleared { .. }));
            rig.set("returned-original-set", false, u64::MAX);
            rig.execute();
            assert_eq!(rig.set_reply().request_id, "returned-original-set");
            rig.close();
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "同一真实四路通道按序断链和送达，连续验证封口前后的原责任"
    )]
    fn four_rr_disconnects_and_actual_outbox_delivery_are_required_for_closure() {
        let mut rig = Rig::with_probe(
            S0State::Safe,
            healthy_guards(),
            OutputSafetyState::TorqueOffConfirmed,
            false,
            Arc::new(SystemClock::new()),
            None,
            false,
            Some(RigProbe::NarrowResponses),
        );
        rig.execute();
        let _ = rig.boot.arm_runtime_shutdown(rig.clock.now());
        let generation = rig.boot.runtime_words().generation();
        assert_ne!(generation, 0);
        let tick = rig
            .handle()
            .metrics()
            .ipc_tick_count
            .load(Ordering::Acquire);
        wait_until(
            || {
                rig.handle()
                    .metrics()
                    .ipc_tick_count
                    .load(Ordering::Acquire)
                    >= tick + 3
            },
            WAIT,
        );
        assert!(
            !rig.boot.clear_retired_for(generation),
            "四原 sender 仍在，Empty 不能变成 Disconnected"
        );
        rig.clear("close-high", false, u64::MAX);
        rig.clear("close-normal", true, u64::MAX);
        rig.set("set-high", false, u64::MAX);
        let route = rig
            .set_routes
            .as_mut()
            .unwrap()
            .admit(RrRequestClass::of_set_enabled(true, false));
        rig.set_tx.as_ref().unwrap()[1]
            .send(SetEnabledRrRequest {
                route,
                request_id: "set-normal".into(),
                enabled: false,
                dry_run: true,
                deadline_ns: u64::MAX,
                audit_reason: "原关闭边界".into(),
            })
            .unwrap();
        wait_until(
            || {
                rig.clear_tx.as_ref().unwrap().iter().all(Sender::is_empty)
                    && rig.set_tx.as_ref().unwrap().iter().all(Sender::is_empty)
                    && rig.clear_rx.len() == 1
                    && rig.set_rx.len() == 1
            },
            WAIT,
        );
        let [clear_high, clear_normal] = rig.clear_tx.take().unwrap();
        let [set_high, set_normal] = rig.set_tx.take().unwrap();
        drop((clear_high, clear_normal, set_high));
        let tick = rig
            .handle()
            .metrics()
            .ipc_tick_count
            .load(Ordering::Acquire);
        wait_until(
            || {
                rig.handle()
                    .metrics()
                    .ipc_tick_count
                    .load(Ordering::Acquire)
                    >= tick + 3
            },
            WAIT,
        );
        assert!(!rig.boot.clear_retired_for(generation), "只断三路不能封口");
        drop(set_normal);
        let tick = rig
            .handle()
            .metrics()
            .ipc_tick_count
            .load(Ordering::Acquire);
        wait_until(
            || {
                rig.handle()
                    .metrics()
                    .ipc_tick_count
                    .load(Ordering::Acquire)
                    >= tick + 3
            },
            WAIT,
        );
        assert!(
            !rig.boot.clear_retired_for(generation),
            "四路断链后原 outbox Full 仍须持有未送出的退休回复"
        );
        let mut clear_ids = Vec::new();
        let mut set_ids = Vec::new();
        for _ in 0..2 {
            let clear = rig.clear_reply();
            assert!(matches!(clear.payload, ClearEStopRrPayload::ShuttingDown));
            assert_eq!(clear.effect, SideEffect::None);
            clear_ids.push(clear.request_id);
            let set = rig.set_reply();
            assert!(matches!(
                set.payload,
                SetEnabledRrPayload::PreemptedByShutdown
            ));
            assert_eq!(set.effect, SideEffect::None);
            set_ids.push(set.request_id);
        }
        clear_ids.sort();
        set_ids.sort();
        assert_eq!(clear_ids, ["close-high", "close-normal"]);
        assert_eq!(set_ids, ["set-high", "set-normal"]);
        wait_until(|| rig.boot.clear_retired_for(generation), WAIT);
        assert!(rig.clear_rx.is_empty() && rig.set_rx.is_empty());
        assert!(
            rig.clear_routes.as_ref().unwrap().slots.is_empty()
                && rig.set_routes.as_ref().unwrap().slots.is_empty()
        );
        rig.close();
    }

    #[test]
    fn spawn_then_stop_cleanly() {
        let mut rig = safe_rig(false);
        wait_until(
            || {
                rig.handle()
                    .metrics()
                    .ipc_tick_count
                    .load(Ordering::Acquire)
                    >= 2
            },
            WAIT,
        );
        rig.close();
        assert!(rig.ipc.is_none() && rig.writer.is_none() && rig.executor.is_none());
    }

    #[test]
    fn command_plumbing_writes_pod_buffer() {
        let mut rig = safe_rig(false);
        rig.command(7, LAYOUT, &[0.1, 0.2, 0.3]);
        wait_until(|| rig.cmd_reader.load().seq == 7, WAIT);
        let command = rig.cmd_reader.load();
        assert_eq!(command.joint_count, 3);
        for (actual, expected) in command.positions.iter().zip([0.1, 0.2, 0.3]) {
            assert!((actual - expected).abs() < 1e-3);
        }
        assert_eq!(command.mode, ControlModeRepr::Position);
    }

    #[test]
    fn command_layout_mismatch_dropped_not_written() {
        let mut rig = safe_rig(false);
        rig.command(9, 0xDEAD_0002, &[0.9]);
        wait_until(
            || {
                rig.handle()
                    .metrics()
                    .unknown_layout_count
                    .load(Ordering::Acquire)
                    == 1
            },
            WAIT,
        );
        assert_eq!(rig.cmd_reader.load().seq, 0);
        assert_eq!(rig.cmd_reader.load().joint_count, 0);
        assert_eq!(
            rig.handle()
                .metrics()
                .commands_applied
                .load(Ordering::Acquire),
            0
        );
    }

    #[test]
    fn command_layout_match_written() {
        let mut rig = safe_rig(false);
        rig.command(11, LAYOUT, &[0.5, 0.6]);
        wait_until(|| rig.cmd_reader.load().seq == 11, WAIT);
        assert_eq!(rig.cmd_reader.load().joint_count, 2);
        assert_eq!(
            rig.handle()
                .metrics()
                .unknown_layout_count
                .load(Ordering::Acquire),
            0
        );
    }

    #[test]
    fn heartbeat_transitions_state_and_updates_watchdog_atom() {
        let clock = Arc::new(MockClock::new(MonoRawNs(100_000_000)));
        let mut rig = Rig::with_clock(
            S0State::Ready,
            healthy_guards(),
            OutputSafetyState::TorqueOffConfirmed,
            false,
            clock.clone(),
            None,
            false,
        );
        rig.execute();
        rig.heartbeat();
        wait_until(|| rig.state.load().state == enabled_active(), WAIT);
        assert!(rig.state.load().guards_summary.heartbeat_ok);
        assert_eq!(
            rig.handle().last_hb_ns().load(Ordering::Acquire),
            clock.now().0
        );
        assert!(!rig.inner.authority().snapshot().disabled);
    }

    #[test]
    fn watchdog_timeout_drops_to_control_lost() {
        let mut guards = healthy_guards();
        guards.heartbeat_ok = true;
        let rig = Rig::new(
            enabled_active(),
            guards,
            OutputSafetyState::TorqueOffConfirmed,
            false,
        );
        rig.watchdog.send(Event::HeartbeatTimeout).unwrap();
        wait_until(
            || {
                matches!(
                    rig.state.load().state,
                    S0State::Enabled {
                        control_link: crate::state_machine::ControlLink::Lost,
                        ..
                    }
                )
            },
            WAIT,
        );
        assert_eq!(
            rig.state.load().state,
            S0State::Enabled {
                sub: crate::state_machine::EnabledSub::CtrlLostHold,
                control_link: crate::state_machine::ControlLink::Lost,
                output_mode: crate::state_machine::OutputMode::Hold
            }
        );
        assert!(!rig.state.load().guards_summary.heartbeat_ok);
    }

    #[test]
    fn estop_topic_drops_to_safe() {
        let mut rig = Rig::new(
            enabled_active(),
            healthy_guards(),
            OutputSafetyState::TorqueOffConfirmed,
            false,
        );
        rig.estop();
        wait_until(
            || {
                rig.state.load().state == S0State::Safe
                    && rig.state.load().guards_summary.estop_latched
            },
            WAIT,
        );
        assert!(rig.inner.authority().snapshot().latch);
    }

    #[test]
    fn estop_topic_writes_latch_to_disk_immediately() {
        let mut rig = safe_rig(false);
        assert_eq!(std::fs::read(&rig.latch).unwrap(), b"0\n");
        rig.estop();
        wait_until(
            || {
                rig.inner.authority().snapshot().latch
                    && rig.shared.obligation.is_settled()
                    && std::fs::read(&rig.latch).is_ok_and(|bytes| bytes == b"1\n")
                    && std::fs::read(rig.hook.backup.as_path()).is_ok_and(|bytes| bytes == b"1\n")
            },
            WAIT,
        );
        assert!(!rig.handle().is_finished(), "持久化在 IPC 仍运行时完成");
        assert!(!rig.writer.as_ref().unwrap().is_finished());
    }

    #[test]
    fn external_stop_event_transitions_to_stopping() {
        let rig = Rig::new(
            S0State::Ready,
            healthy_guards(),
            OutputSafetyState::TorqueOffConfirmed,
            false,
        );
        rig.handle()
            .event_sender()
            .control(crate::event_ingress::ControlEvent::Stop)
            .unwrap();
        wait_until(|| rig.state.load().state == S0State::Stopping, WAIT);
        assert!(rig.inner.authority().snapshot().disabled);
    }

    #[test]
    fn freeze_hook_holds_tick_count() {
        let rig = safe_rig(false);
        let metrics = rig.handle().metrics();
        wait_until(|| metrics.tick_count.load(Ordering::Acquire) >= 5, WAIT);
        let before = metrics.tick_count.load(Ordering::Acquire);
        metrics
            .freeze_until_ns
            .store(rig.clock.now().0 + 80_000_000, Ordering::Release);
        thread::sleep(Duration::from_millis(40));
        let middle = metrics.tick_count.load(Ordering::Acquire);
        assert!(
            middle.saturating_sub(before) <= 1,
            "freeze 内仍推进 tick: {before}→{middle}"
        );
        wait_until(
            || metrics.tick_count.load(Ordering::Acquire) > middle + 2,
            WAIT,
        );
    }

    #[test]
    fn rr_dry_run_short_circuits_with_would_clear_when_gate_passes() {
        let mut rig = safe_rig(true);
        rig.clear("dry", true, u64::MAX);
        let reply = rig.clear_reply();
        assert_eq!(reply.request_id, "dry");
        assert_eq!(reply.effect, SideEffect::None);
        assert!(matches!(
            clear_outcome(reply),
            ClearOutcome::WouldClear { .. }
        ));
        assert!(rig.inner.authority().snapshot().latch);
        assert_eq!(std::fs::read(&rig.latch).unwrap(), b"1\n");
    }

    #[test]
    fn rr_dry_run_returns_state_not_safe_when_state_is_ready() {
        let mut rig = Rig::new(
            S0State::Ready,
            healthy_guards(),
            OutputSafetyState::TorqueOffConfirmed,
            false,
        );
        rig.clear("dry-state", true, u64::MAX);
        assert!(matches!(
            clear_outcome(rig.clear_reply()),
            ClearOutcome::StateNotSafe
        ));
    }

    #[test]
    fn rr_live_success_routes_cleared_response() {
        let mut rig = safe_rig(true);
        rig.clear("clear", false, u64::MAX);
        let response = rig.clear_reply();
        assert_eq!(response.request_id, "clear");
        assert_eq!(response.effect, SideEffect::Committed);
        assert!(matches!(
            clear_outcome(response),
            ClearOutcome::Cleared {
                superseded: false,
                ..
            }
        ));
        assert!(!rig.inner.authority().snapshot().latch);
        assert_eq!(std::fs::read(&rig.latch).unwrap(), b"0\n");
        assert_eq!(std::fs::read(&rig.hook.backup).unwrap(), b"0\n");
        assert!(
            !rig.latch
                .with_file_name("estop_latch.clear_pending")
                .exists()
        );
        assert!(rig.clear_rx.try_recv().is_err(), "同一 RR 不得回复两次");
    }

    #[test]
    fn rr_live_state_not_safe_routes_state_not_safe_response() {
        let mut rig = Rig::new(
            S0State::Ready,
            healthy_guards(),
            OutputSafetyState::TorqueOffConfirmed,
            false,
        );
        rig.clear("state", false, u64::MAX);
        assert!(matches!(
            clear_outcome(rig.clear_reply()),
            ClearOutcome::StateNotSafe
        ));
    }

    #[test]
    fn rr_live_not_torque_off_confirmed_routes_failed_precondition() {
        let mut rig = Rig::new(
            S0State::Safe,
            healthy_guards(),
            OutputSafetyState::TorqueOn,
            true,
        );
        rig.clear("not-off", false, u64::MAX);
        let response = rig.clear_reply();
        assert_eq!(response.effect, SideEffect::None);
        assert!(matches!(
            clear_outcome(response),
            ClearOutcome::NotTorqueOffConfirmed {
                safety: OutputSafetyState::TorqueOn
            }
        ));
        assert_eq!(std::fs::read(&rig.latch).unwrap(), b"1\n");
    }

    #[test]
    fn rr_live_fsync_failed_routes_fsync_failed_response() {
        let mut rig = safe_rig(true);
        rig.hook.fail_backup_once.store(1, Ordering::Release);
        rig.clear("filesystem", false, u64::MAX);
        assert!(matches!(
            clear_outcome(rig.clear_reply()),
            ClearOutcome::FsyncFailed {
                stage: ClearTxnStage::RenameBackup
            }
        ));
        assert_eq!(rig.hook.fail_backup_once.load(Ordering::Acquire), 3);
        assert_eq!(std::fs::read(&rig.latch).unwrap(), b"1\n");
        assert_eq!(std::fs::read(&rig.hook.backup).unwrap(), b"1\n");
    }

    #[test]
    fn rr_live_single_flight_second_request_returns_already_in_progress() {
        let mut rig = safe_rig(true);
        rig.hook.pause.store(true, Ordering::Release);
        rig.clear("first-clear", false, u64::MAX);
        wait_until(|| rig.hook.reached.load(Ordering::Acquire), WAIT);
        rig.clear("second-clear", false, u64::MAX);
        let response = rig.clear_reply();
        assert_eq!(response.request_id, "second-clear");
        assert!(matches!(
            clear_outcome(response),
            ClearOutcome::AlreadyInProgress
        ));
        assert!(
            rig.clear_rx.try_recv().is_err(),
            "原 Clear 尚在真实文件步骤"
        );
        rig.hook.release.store(true, Ordering::Release);
        let response = rig.clear_reply();
        assert_eq!(response.request_id, "first-clear");
        assert!(matches!(
            clear_outcome(response),
            ClearOutcome::Cleared { .. }
        ));
    }

    #[test]
    fn rr_live_deadline_exceeded_at_drain_time_returns_deadline_exceeded() {
        let mut rig = safe_rig(true);
        rig.clear("expired", false, 0);
        let response = rig.clear_reply();
        assert_eq!(response.effect, SideEffect::None);
        assert_eq!(response.payload, ClearEStopRrPayload::DeadlineExceeded);
        assert_eq!(std::fs::read(&rig.latch).unwrap(), b"1\n");
    }

    #[test]
    fn rr_dry_run_deadline_exceeded_at_drain_time() {
        let mut rig = safe_rig(true);
        rig.clear("expired-dry", true, 0);
        let response = rig.clear_reply();
        assert_eq!(response.payload, ClearEStopRrPayload::DeadlineExceeded);
        assert_eq!(response.effect, SideEffect::None);
    }

    #[test]
    fn rr_live_emergency_during_prepare_preserves_first_guard_failure_and_repairs() {
        let mut rig = safe_rig(true);
        rig.hook.pause.store(true, Ordering::Release);
        rig.clear("estop-clear", false, u64::MAX);
        let original_route = *rig
            .clear_routes
            .as_ref()
            .unwrap()
            .slots
            .keys()
            .next()
            .unwrap();
        wait_until(|| rig.hook.reached.load(Ordering::Acquire), WAIT);
        let epoch = rig.inner.authority().snapshot().epoch;
        rig.estop();
        wait_until(|| rig.inner.authority().snapshot().epoch != epoch, WAIT);
        assert!(
            rig.clear_rx.try_recv().is_err(),
            "真实 writer 归还/退休前不得回复"
        );
        rig.hook.release.store(true, Ordering::Release);
        let response = rig.clear_reply();
        assert_eq!(response.route, original_route);
        assert_eq!(response.request_id, "estop-clear");
        assert_eq!(response.effect, SideEffect::Started);
        // 新义务使原 writer 在首个改值步骤拒绝；W3a 保留这条真实最早失败，再 repair/退休。
        assert_eq!(
            clear_outcome(response),
            ClearOutcome::FsyncFailed {
                stage: ClearTxnStage::CreateBackup
            }
        );
        assert_eq!(std::fs::read(&rig.latch).unwrap(), b"1\n");
        assert_eq!(std::fs::read(&rig.hook.backup).unwrap(), b"1\n");
        assert!(
            !rig.latch
                .with_file_name("estop_latch.clear_pending")
                .exists()
        );
        assert!(rig.inner.authority().snapshot().latch);
        assert!(
            rig.clear_rx.try_recv().is_err(),
            "原 Clear route 只能真实退休回复一次"
        );
    }

    #[test]
    fn pending_resolves_on_enabled_active() {
        let mut rig = safe_rig(false);
        rig.set("enable", true, u64::MAX);
        rig.heartbeat();
        wait_until(
            || rig.inner.pending_normal.load(Ordering::Acquire) != 0,
            WAIT,
        );
        assert!(rig.set_rx.try_recv().is_err());
        rig.execute();
        let response = rig.set_reply();
        assert_eq!(response.request_id, "enable");
        assert!(matches!(
            response.payload,
            SetEnabledRrPayload::Enabled { .. }
        ));
        assert_eq!(response.effect, SideEffect::Committed);
        assert_eq!(rig.state.load().state, enabled_active());
        assert_eq!(
            rig.inner.authority().snapshot().safety,
            OutputSafetyState::TorqueOn
        );
    }

    #[test]
    fn disable_pending_resolves_on_safe() {
        let mut rig = Rig::new(
            enabled_active(),
            healthy_guards(),
            OutputSafetyState::TorqueOffConfirmed,
            false,
        );
        rig.set("disable", false, u64::MAX);
        wait_until(
            || rig.inner.pending_normal.load(Ordering::Acquire) != 0,
            WAIT,
        );
        assert!(rig.set_rx.try_recv().is_err());
        rig.execute();
        assert!(matches!(
            rig.set_reply().payload,
            SetEnabledRrPayload::Disabled { .. }
        ));
        assert_eq!(rig.state.load().state, S0State::Safe);
        assert_eq!(
            rig.inner.authority().snapshot().safety,
            OutputSafetyState::TorqueOffConfirmed
        );
    }

    // 旧“第二条单飞拒绝”改为已批 Restore 排队：首个原 seq 未退休前不消费第二条。
    #[test]
    fn pending_single_flight_preserves_second_restore_until_first_retires() {
        let mut rig = safe_rig(false);
        rig.set("first", true, u64::MAX);
        rig.heartbeat();
        wait_until(
            || rig.inner.pending_normal.load(Ordering::Acquire) != 0,
            WAIT,
        );
        let seq = rig.inner.pending_normal.load(Ordering::Acquire);
        let ticks = rig.handle().metrics().tick_count.load(Ordering::Acquire);
        rig.set("second", true, u64::MAX);
        wait_until(
            || rig.handle().metrics().tick_count.load(Ordering::Acquire) >= ticks + 4,
            WAIT,
        );
        assert!(rig.set_rx.try_recv().is_err());
        assert_eq!(rig.inner.pending_normal.load(Ordering::Acquire), seq);
        rig.execute();
        let first = rig.set_reply();
        let second = rig.set_reply();
        assert_eq!(first.request_id, "first");
        assert_eq!(second.request_id, "second");
        assert!(matches!(first.payload, SetEnabledRrPayload::Enabled { .. }));
        assert!(matches!(
            second.payload,
            SetEnabledRrPayload::Enabled { .. }
        ));
        assert_eq!(first.effect, SideEffect::Committed);
        assert_eq!(second.effect, SideEffect::None);
    }

    #[test]
    fn pending_deadline_resolves() {
        let clock = Arc::new(MockClock::new(MonoRawNs(100_000_000)));
        let mut rig = Rig::with_clock(
            S0State::Safe,
            healthy_guards(),
            OutputSafetyState::TorqueOffConfirmed,
            false,
            clock.clone(),
            None,
            false,
        );
        rig.set("deadline", true, 200_000_000);
        rig.heartbeat();
        wait_until(
            || rig.inner.pending_normal.load(Ordering::Acquire) != 0,
            WAIT,
        );
        rig.claim_without_start();
        clock.set(MonoRawNs(300_000_000));
        let ticks = rig.handle().metrics().tick_count.load(Ordering::Acquire);
        wait_until(
            || rig.handle().metrics().tick_count.load(Ordering::Acquire) >= ticks + 3,
            WAIT,
        );
        assert!(
            rig.set_rx.try_recv().is_err(),
            "过期不替代原 seq 的真实物理退休"
        );
        rig.execute();
        assert_eq!(
            rig.set_reply().payload,
            SetEnabledRrPayload::DeadlineExceeded
        );
    }

    #[test]
    fn pending_preempt_estop_resolves_aborted() {
        let mut rig = safe_rig(false);
        rig.set("preempt", true, u64::MAX);
        rig.heartbeat();
        wait_until(
            || rig.inner.pending_normal.load(Ordering::Acquire) != 0,
            WAIT,
        );
        rig.claim_without_start();
        rig.estop();
        wait_until(|| rig.inner.authority().snapshot().latch, WAIT);
        assert!(rig.set_rx.try_recv().is_err(), "EStop 不伪造原 seq 退休");
        rig.execute();
        assert_eq!(
            rig.set_reply().payload,
            SetEnabledRrPayload::PreemptedByEStop
        );
    }

    #[test]
    fn pending_adapter_fault_resolves_internal_adapter_error() {
        let mut rig = Rig::with_clock(
            S0State::Safe,
            healthy_guards(),
            OutputSafetyState::TorqueOffConfirmed,
            false,
            Arc::new(SystemClock::new()),
            None,
            true,
        );
        // 初始化就绪不替代首次订阅轮询；先观察实际 tick，再发布一次心跳。
        wait_until(
            || rig.handle().metrics().tick_count.load(Ordering::Acquire) > 0,
            WAIT,
        );
        rig.heartbeat();
        wait_until(
            || {
                rig.handle()
                    .metrics()
                    .heartbeats_received
                    .load(Ordering::Acquire)
                    > 0
            },
            WAIT,
        );
        rig.set("adapter-error", true, u64::MAX);
        wait_until(
            || rig.inner.pending_normal.load(Ordering::Acquire) != 0,
            WAIT,
        );
        rig.execute();
        assert_eq!(rig.set_reply().payload, SetEnabledRrPayload::AdapterError);
        assert!(rig.inner.authority().snapshot().disabled);
    }

    fn assert_fault_disable_preserves_fault(code: FaultCode) {
        let fault = S0State::Fault(code);
        let mut rig = Rig::new(
            fault,
            healthy_guards(),
            OutputSafetyState::TorqueOffConfirmed,
            false,
        );
        rig.set("idempotent", false, u64::MAX);
        let response = rig.set_reply();
        assert_eq!(response.request_id, "idempotent");
        // 原 Lower 的结果先锁存业务故障，物理退休成功不会覆盖这条首因。
        match code {
            FaultCode::AdapterErr => {
                assert_eq!(response.payload, SetEnabledRrPayload::AdapterError);
            }
            _ => assert!(matches!(
                response.payload,
                SetEnabledRrPayload::Disabled { .. }
            )),
        }
        assert_eq!(response.effect, SideEffect::Committed);
        assert_eq!(
            rig.inner.authority().snapshot().safety,
            OutputSafetyState::TorqueOffConfirmed
        );
        assert_eq!(rig.state.load().state, fault);
        assert_eq!(rig.inner.pending_normal.load(Ordering::Acquire), 0);
    }

    #[test]
    fn disable_from_fault_adapter_err_preserves_error_after_retirement() {
        assert_fault_disable_preserves_fault(FaultCode::AdapterErr);
    }

    #[test]
    fn disable_from_fault_other_is_idempotent_ok() {
        assert_fault_disable_preserves_fault(FaultCode::LimitViolation);
    }
}
