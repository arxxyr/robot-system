//! S48 W2b：纯关闭 / join 机制——线程句柄槽（三态、状态私有）、通用有界 join、门谓词注入的
//! 顺序停止（沿监督 r4：控制面先停 → 真实 ACK 才停 Executor → Clear 退休且 flush 才停
//! IPC / writer → 最后 Watchdog）与全部真实结果只进入的唯一 [`TeardownReport`]。
//!
//! W3 的 daemon 启动与运行期关闭使用这些原句柄和结果类型；运行期通过
//! [`TeardownGates`] 接入同一 authority、原 Shutdown 代次、ACK 与截止门。
//! 本模块保留真实停止请求与 join 结果，daemon 将原残件移入进程退出聚合；
//! 独立监督器负责原 D 的最终执法。
//!
//! | 项 | 内容 |
//! |---|---|
//! | [`HandleKind`] / [`HandleKindSet`] | 12 种句柄身份与 `required` 集合；`bit()` 仅供残余位图 |
//! | [`WorkerSlot`] | 真实句柄的显式枚举：`kind` / `request_stop` / `is_finished` / `join`；health 变体经 `try_join`，`Pending` 原样返还 |
//! | [`WriterControl`] / [`ManagedWriterHandle`] / [`spawn_managed_writer`] | writer 停止能力绑定：唯一 spawn 从同一 `Arc<C>` 产生停止读取源并保存请求控制器 |
//! | [`JoinedOutcome`] | 普通 worker、Executor、IPC 各自的真实 join 结果；writer 的 `Result<WriterExit>` + 唯一 [`WriterJoined`] 原样保留 |
//! | [`Slot`] / [`SlotView`] | 三态槽，状态私有；公开只能构造 `NotSpawned` / `Live`，`Joined` 只由本模块在真实 join 后写入 |
//! | [`HandleSlots`] | 12 槽集合；`spawned` 失败原样返还句柄 |
//! | [`join_with_deadline`] | 唯一能把槽推进到 `Joined` 的入口；`NotSpawned` / `AlreadyJoined` 绝不产生结果 |
//! | [`TeardownGates`] / [`teardown`] | 门谓词注入；按值单次消费 `HandleSlots`，门失败不对受保护句柄发 stop |
//! | [`TeardownReport`] / [`ResidualHandles`] / [`RunExit`] | 全部结果与残余的唯一归宿；残余从不 Drop |
//! | [`CleanInputs`] / [`exit_code`] | 批准真值表：全部满足才 0，否则 1；90 由独立 D 产生，不在此 |
//!
//! `LatchWriter` 槽：W2a 冻结的 [`WriterHandle`] 只有 `is_finished` / `join`，停止能力经
//! `spawn_writer` 另收的 `Arc<dyn WriterStop>` 表达。本模块不假定 `WriterHandle::request_stop`
//! 存在，也不提供把任意裸 `WriterHandle` 与外部停止器拼装的公开构造：[`spawn_managed_writer`]
//! 用同一个 `Arc<C>`（`C: WriterControl`）既作 writer 线程 `running()` 的唯一读取源，又作句柄
//! `request_stop()` 的唯一写入源。该绑定只保证请求转发给同一个控制对象，不保证任意实现或
//! 已阻塞的 writer 必定退出；是否结束与唯一 [`WriterJoined`] 仍以真实 join 判定。

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use robot_core::time::{DurationNs, RobotClock};
use robot_health::systemd::WatchdogHandle;
use robot_health::{JoinState, MonitorHandle};

#[cfg(feature = "test-comm-fault-hook")]
use crate::comm_fault_hook::CommFaultControlHandle;
#[cfg(any(test, feature = "test-freeze-hook"))]
use crate::freeze_control::FreezeControlHandle;
use crate::io_thread::IoThreadHandle;
use crate::ipc_thread::IpcThreadHandle;
use crate::latch_writer::{
    PersistEndpoint, WriterConfig, WriterExit, WriterHandle, WriterHook, WriterId, WriterJoined,
    WriterStop, spawn_writer,
};
use crate::pub_thread::PubThreadHandle;
use crate::rr_server_thread::RrServerThreadHandle;
#[cfg(target_os = "linux")]
use crate::rt_thread::RtThreadHandle;
use crate::serial_io_thread::SerialIoThreadHandle;
use crate::set_enabled_rr_server::SetEnabledRrServerHandle;

// ============================================================================
// 句柄身份
// ============================================================================

/// 句柄身份（与八 worker 观测位图无关；`bit()` 仅供 [`ResidualHandles::bitmap`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HandleKind {
    ClearRr = 0,
    SetRr = 1,
    Publisher = 2,
    Io = 3,
    /// `HardRt` 或 `SerialRt` 执行器（二选一）。
    Executor = 4,
    Ipc = 5,
    /// S1 心跳 watchdog worker（`JoinHandle<()>` + 停止原子，经 [`WorkerSlot::Raw`]）。
    Watchdog = 6,
    FreezeControl = 7,
    HookControl = 8,
    HealthMonitor = 9,
    SystemdWatchdog = 10,
    LatchWriter = 11,
}

impl HandleKind {
    pub const COUNT: usize = 12;
    pub const ALL: [Self; Self::COUNT] = [
        Self::ClearRr,
        Self::SetRr,
        Self::Publisher,
        Self::Io,
        Self::Executor,
        Self::Ipc,
        Self::Watchdog,
        Self::FreezeControl,
        Self::HookControl,
        Self::HealthMonitor,
        Self::SystemdWatchdog,
        Self::LatchWriter,
    ];
    /// 第 ② 步先停的辅助句柄（数组顺序即停止顺序）；Executor / Ipc / `LatchWriter` / Watchdog
    /// 受门保护，只在对应门为真时才发停止。
    const HELPERS: [Self; 8] = [
        Self::ClearRr,
        Self::SetRr,
        Self::Publisher,
        Self::Io,
        Self::FreezeControl,
        Self::HookControl,
        Self::HealthMonitor,
        Self::SystemdWatchdog,
    ];

    /// 残余位图位；与 `worker_stop::WorkerKind` 的 7 位图无关。
    #[must_use]
    pub const fn bit(self) -> u16 {
        1 << (self as u16)
    }

    const fn index(self) -> usize {
        self as usize
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ClearRr => "clear_rr",
            Self::SetRr => "set_rr",
            Self::Publisher => "publisher",
            Self::Io => "io",
            Self::Executor => "executor",
            Self::Ipc => "ipc",
            Self::Watchdog => "watchdog",
            Self::FreezeControl => "freeze_control",
            Self::HookControl => "hook_control",
            Self::HealthMonitor => "health_monitor",
            Self::SystemdWatchdog => "systemd_watchdog",
            Self::LatchWriter => "latch_writer",
        }
    }
}

/// 句柄身份集合（`required` 由调用方给定；W2b 只在测试构造）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HandleKindSet(u16);

impl HandleKindSet {
    pub const EMPTY: Self = Self(0);

    #[must_use]
    pub const fn with(self, kind: HandleKind) -> Self {
        Self(self.0 | kind.bit())
    }

    #[must_use]
    pub const fn contains(self, kind: HandleKind) -> bool {
        self.0 & kind.bit() != 0
    }

    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    pub fn kinds(self) -> impl Iterator<Item = HandleKind> {
        HandleKind::ALL
            .into_iter()
            .filter(move |kind| self.contains(*kind))
    }
}

// ============================================================================
// writer 停止能力绑定（root 增补合同）
// ============================================================================

/// 可请求停止的 writer 停止源；`running()` 语义沿 W2a [`WriterStop`]。生产实现留 W3
/// （包 `StopFlags` 的停止原子与 `worker_running`）；本包不定义第八 `WorkerKind`。
pub trait WriterControl: WriterStop {
    /// 协作停止请求：只改变 `running()` 的读数，不保证已阻塞的 writer 必定退出。
    fn request_stop(&self);
}

/// 由唯一的 [`spawn_managed_writer`] 构造：字段私有、无公开构造 / setter，故不存在
/// "A 句柄配 B 停止器" 的公开裸构造。
///
/// ```compile_fail,E0451
/// use robot_s0::shutdown_mechanism::ManagedWriterHandle;
/// fn forge(inner: robot_s0::latch_writer::WriterHandle,
///          control: std::sync::Arc<dyn robot_s0::shutdown_mechanism::WriterControl>)
///     -> ManagedWriterHandle {
///     ManagedWriterHandle { inner, control }
/// }
/// ```
///
/// `Arc<dyn WriterStop>` 不是控制器（没有 `request_stop`）：
///
/// ```compile_fail,E0599
/// use std::sync::Arc;
/// use robot_s0::latch_writer::WriterStop;
/// fn cannot_request(stop: Arc<dyn WriterStop>) { stop.request_stop(); }
/// ```
pub struct ManagedWriterHandle {
    inner: WriterHandle,
    control: Arc<dyn WriterControl>,
}

impl fmt::Debug for ManagedWriterHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManagedWriterHandle")
            .field("writer", &self.inner.writer())
            .finish_non_exhaustive()
    }
}

impl ManagedWriterHandle {
    /// 把停止请求转发给 spawn 时绑定的同一控制对象。
    pub fn request_stop(&self) {
        self.control.request_stop();
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.inner.is_finished()
    }

    #[must_use]
    pub const fn writer(&self) -> WriterId {
        self.inner.writer()
    }

    /// 原样保留 W2a 的 tuple：正常返回与 panic `Err` 两路都产生唯一 [`WriterJoined`]。
    pub fn join(self) -> (thread::Result<WriterExit>, WriterJoined) {
        self.inner.join()
    }
}

/// 唯一构造点：同一个 `Arc<C>` 既作为 writer 线程的停止读取源（先显式克隆 `Arc<C>` 再经
/// unsized coercion 擦除为 `Arc<dyn WriterStop>` 传入现有 [`spawn_writer`]，不依赖 trait
/// upcasting），又保留为本句柄的请求控制器。
///
/// # Errors
///
/// 沿 [`spawn_writer`] 语义：线程创建失败原样返回 `io::Error`；`slot` / `endpoint` 已按 W2a
/// 语义被消费，不产生 [`WriterJoined`]、不进入 [`HandleSlots`]；调用方仍持有自己的 `Arc<C>`。
pub fn spawn_managed_writer<C: WriterControl + 'static>(
    slot: crate::latch_writer::WriterSlot,
    endpoint: PersistEndpoint,
    config: WriterConfig,
    control: Arc<C>,
    clock: Arc<dyn RobotClock>,
    hook: Box<dyn WriterHook>,
) -> io::Result<ManagedWriterHandle> {
    let stop: Arc<dyn WriterStop> = Arc::<C>::clone(&control);
    let inner = spawn_writer(slot, endpoint, config, stop, clock, hook)?;
    Ok(ManagedWriterHandle { inner, control })
}

// ============================================================================
// 真实句柄枚举
// ============================================================================

/// 真实线程句柄的显式枚举；`kind()` 决定其在 [`HandleSlots`] 中的位置。
///
/// `LatchWriter` 只接受 [`ManagedWriterHandle`]，裸 [`WriterHandle`] 类型不匹配：
///
/// ```compile_fail,E0308
/// use robot_s0::shutdown_mechanism::WorkerSlot;
/// fn wrap(raw: robot_s0::latch_writer::WriterHandle) -> WorkerSlot { WorkerSlot::LatchWriter(raw) }
/// ```
pub enum WorkerSlot {
    Ipc(IpcThreadHandle),
    #[cfg(target_os = "linux")]
    HardRt(RtThreadHandle),
    SerialRt(SerialIoThreadHandle),
    Publisher(PubThreadHandle),
    Io(IoThreadHandle),
    ClearRr(RrServerThreadHandle),
    SetRr(SetEnabledRrServerHandle),
    /// 只有 `JoinHandle<()>` + 停止原子的线程（daemon 的 S1 心跳 watchdog 绑定
    /// `StopFlags::flag(Watchdog)` 同一原子；测试线程）。
    Raw {
        kind: HandleKind,
        join: JoinHandle<()>,
        stop: Arc<AtomicBool>,
    },
    #[cfg(any(test, feature = "test-freeze-hook"))]
    FreezeControl(FreezeControlHandle),
    #[cfg(feature = "test-comm-fault-hook")]
    HookControl(CommFaultControlHandle),
    HealthMonitor(MonitorHandle),
    SystemdWatchdog(WatchdogHandle),
    LatchWriter(ManagedWriterHandle),
}

impl fmt::Debug for WorkerSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("WorkerSlot").field(&self.kind()).finish()
    }
}

impl WorkerSlot {
    #[must_use]
    pub fn kind(&self) -> HandleKind {
        match self {
            Self::Ipc(_) => HandleKind::Ipc,
            #[cfg(target_os = "linux")]
            Self::HardRt(_) => HandleKind::Executor,
            Self::SerialRt(_) => HandleKind::Executor,
            Self::Publisher(_) => HandleKind::Publisher,
            Self::Io(_) => HandleKind::Io,
            Self::ClearRr(_) => HandleKind::ClearRr,
            Self::SetRr(_) => HandleKind::SetRr,
            Self::Raw { kind, .. } => *kind,
            #[cfg(any(test, feature = "test-freeze-hook"))]
            Self::FreezeControl(_) => HandleKind::FreezeControl,
            #[cfg(feature = "test-comm-fault-hook")]
            Self::HookControl(_) => HandleKind::HookControl,
            Self::HealthMonitor(_) => HandleKind::HealthMonitor,
            Self::SystemdWatchdog(_) => HandleKind::SystemdWatchdog,
            Self::LatchWriter(_) => HandleKind::LatchWriter,
        }
    }

    /// 只发协作停止请求，不 join。
    pub fn request_stop(&self) {
        match self {
            Self::Ipc(handle) => handle.request_stop(),
            #[cfg(target_os = "linux")]
            Self::HardRt(handle) => handle.request_stop(),
            Self::SerialRt(handle) => handle.request_stop(),
            Self::Publisher(handle) => handle.request_stop(),
            Self::Io(handle) => handle.request_stop(),
            Self::ClearRr(handle) => handle.request_stop(),
            Self::SetRr(handle) => handle.request_stop(),
            Self::Raw { stop, .. } => stop.store(true, Ordering::Release),
            #[cfg(any(test, feature = "test-freeze-hook"))]
            Self::FreezeControl(handle) => handle.request_stop(),
            #[cfg(feature = "test-comm-fault-hook")]
            Self::HookControl(handle) => handle.request_stop(),
            Self::HealthMonitor(handle) => handle.request_stop(),
            Self::SystemdWatchdog(handle) => handle.request_stop(),
            Self::LatchWriter(handle) => handle.request_stop(),
        }
    }

    /// 只读发现（Relaxed 语义）；happens-before 只由 [`Self::join`] 建立。
    #[must_use]
    pub fn is_finished(&self) -> bool {
        match self {
            Self::Ipc(handle) => handle.is_finished(),
            #[cfg(target_os = "linux")]
            Self::HardRt(handle) => handle.is_finished(),
            Self::SerialRt(handle) => handle.is_finished(),
            Self::Publisher(handle) => handle.is_finished(),
            Self::Io(handle) => handle.is_finished(),
            Self::ClearRr(handle) => handle.is_finished(),
            Self::SetRr(handle) => handle.is_finished(),
            Self::Raw { join, .. } => join.is_finished(),
            #[cfg(any(test, feature = "test-freeze-hook"))]
            Self::FreezeControl(handle) => handle.is_finished(),
            #[cfg(feature = "test-comm-fault-hook")]
            Self::HookControl(handle) => handle.is_finished(),
            Self::HealthMonitor(handle) => handle.is_finished(),
            Self::SystemdWatchdog(handle) => handle.is_finished(),
            Self::LatchWriter(handle) => handle.is_finished(),
        }
    }

    /// 真实 join：`JoinHandle` 类变体阻塞到线程结束并必定 `Ok`；health 变体经非消费式
    /// `try_join`，尚未结束则 `Err(self)` 原样退回句柄（不伪造 `Ok`、不掉入阻塞 `Drop`）；
    /// `LatchWriter` 原样透传 W2a 的结果与唯一凭证。
    ///
    /// # Errors
    ///
    /// health 变体尚未结束时返回原句柄。
    #[allow(
        clippy::result_large_err,
        reason = "尚不能回收时完整返还原线程句柄，不新增分配或丢弃停止责任"
    )]
    pub fn join(self) -> Result<JoinedOutcome, Self> {
        match self {
            Self::Ipc(handle) => Ok(JoinedOutcome::Ipc(handle.join())),
            #[cfg(target_os = "linux")]
            Self::HardRt(handle) => Ok(JoinedOutcome::Executor(handle.join())),
            Self::SerialRt(handle) => Ok(JoinedOutcome::Executor(handle.join())),
            Self::Publisher(handle) => Ok(JoinedOutcome::Worker(handle.join())),
            Self::Io(handle) => Ok(JoinedOutcome::Worker(handle.join())),
            Self::ClearRr(handle) => Ok(JoinedOutcome::Worker(handle.join())),
            Self::SetRr(handle) => Ok(JoinedOutcome::Worker(handle.join())),
            Self::Raw { join, .. } => Ok(JoinedOutcome::Worker(join.join())),
            #[cfg(any(test, feature = "test-freeze-hook"))]
            Self::FreezeControl(handle) => Ok(JoinedOutcome::Worker(handle.join())),
            #[cfg(feature = "test-comm-fault-hook")]
            Self::HookControl(handle) => Ok(JoinedOutcome::Worker(handle.join())),
            Self::HealthMonitor(mut handle) => match handle.try_join() {
                JoinState::Joined(result) => Ok(JoinedOutcome::Worker(result)),
                JoinState::Pending => Err(Self::HealthMonitor(handle)),
            },
            Self::SystemdWatchdog(mut handle) => match handle.try_join() {
                JoinState::Joined(result) => Ok(JoinedOutcome::Worker(result)),
                JoinState::Pending => Err(Self::SystemdWatchdog(handle)),
            },
            Self::LatchWriter(handle) => {
                let (result, joined) = handle.join();
                Ok(JoinedOutcome::Writer { result, joined })
            }
        }
    }
}

/// 真实 join 的结果：worker 的 `thread::Result<()>`（`Err` = panic）；writer 的
/// `Result<WriterExit>` 与唯一 [`WriterJoined`] 原样保留（panic 亦带凭证，不压成 `Result<()>`）。
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "真实线程返回的 IPC 与执行器原残件内联保留至最终退出，不引入新的分配层"
)]
pub enum JoinedOutcome {
    Worker(thread::Result<()>),
    Executor(thread::Result<crate::hard_rt_ctrl::ExecutorExit>),
    Ipc(thread::Result<crate::ipc_thread::IpcExit>),
    Writer {
        result: thread::Result<WriterExit>,
        joined: WriterJoined,
    },
}

// ============================================================================
// 三态槽（状态私有）
// ============================================================================

/// 三态槽：外部只能表达"未创建"与"活着"两种状态；`Joined` 只由本模块在真实 take + join
/// 后写入，没有任何把 `Joined` 写回的公开方法或构造。
///
/// ```compile_fail,E0616
/// use robot_s0::shutdown_mechanism::Slot;
/// fn peek(slot: &Slot) -> bool { matches!(slot.0, _) }
/// ```
#[derive(Debug)]
pub struct Slot(SlotState);

#[derive(Debug)]
enum SlotState {
    NotSpawned,
    Live(WorkerSlot),
    Joined(HandleKind),
}

/// 无所有权的观察视图。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotView {
    NotSpawned,
    Live(HandleKind),
    Joined(HandleKind),
}

impl Slot {
    #[must_use]
    pub const fn not_spawned() -> Self {
        Self(SlotState::NotSpawned)
    }

    #[must_use]
    pub fn live(handle: WorkerSlot) -> Self {
        Self(SlotState::Live(handle))
    }

    #[must_use]
    pub fn view(&self) -> SlotView {
        match &self.0 {
            SlotState::NotSpawned => SlotView::NotSpawned,
            SlotState::Live(handle) => SlotView::Live(handle.kind()),
            SlotState::Joined(kind) => SlotView::Joined(*kind),
        }
    }

    /// 取走活句柄（槽暂置 `NotSpawned`，由调用方决定放回 `Live` 或转 `Joined`）。
    fn take_live(&mut self) -> Option<WorkerSlot> {
        match std::mem::replace(&mut self.0, SlotState::NotSpawned) {
            SlotState::Live(handle) => Some(handle),
            other => {
                self.0 = other;
                None
            }
        }
    }
}

/// 12 槽集合；W3 的 `run()` 唯一拥有。
///
/// `teardown` 按值单次消费，随后不可再用：
///
/// ```compile_fail,E0382
/// use robot_core::time::{DurationNs, MockClock, MonoRawNs};
/// use robot_s0::shutdown_mechanism::{HandleKindSet, HandleSlots, TeardownGates, teardown};
/// struct Open;
/// impl TeardownGates for Open {
///     fn control_stop_and_notify(&mut self) {}
///     fn ack_reached(&mut self, _now_ns: u64) -> bool { true }
///     fn clear_retired_and_flushed(&mut self, _now_ns: u64) -> bool { true }
/// }
/// let clock = MockClock::new(MonoRawNs(0));
/// let slots = HandleSlots::new(HandleKindSet::EMPTY);
/// let mut gates = Open;
/// let _first = teardown(slots, &mut gates, &clock, 0, DurationNs(1));
/// let _second = teardown(slots, &mut gates, &clock, 0, DurationNs(1));
/// ```
#[derive(Debug)]
pub struct HandleSlots {
    slots: [Slot; HandleKind::COUNT],
    required: HandleKindSet,
}

/// [`HandleSlots::spawned`] 的拒绝原因；句柄随错误原样返还。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertError {
    AlreadyPresent,
    AlreadyJoined,
}

impl HandleSlots {
    #[must_use]
    pub fn new(required: HandleKindSet) -> Self {
        Self {
            slots: std::array::from_fn(|_| Slot::not_spawned()),
            required,
        }
    }

    /// 成功才转移所有权；失败原样返还 `WorkerSlot`（不丢责任、不触发阻塞 `Drop`）。
    ///
    /// # Errors
    ///
    /// 同一身份已有活句柄 → `AlreadyPresent`；该槽已 `Joined` → `AlreadyJoined`。
    #[allow(
        clippy::result_large_err,
        reason = "安装拒绝必须完整返还原句柄和错误，原线程责任不经替身转移"
    )]
    pub fn spawned(&mut self, handle: WorkerSlot) -> Result<(), (InsertError, WorkerSlot)> {
        let entry = &mut self.slots[handle.kind().index()];
        match entry.view() {
            SlotView::NotSpawned => {
                entry.0 = SlotState::Live(handle);
                Ok(())
            }
            SlotView::Live(_) => Err((InsertError::AlreadyPresent, handle)),
            SlotView::Joined(_) => Err((InsertError::AlreadyJoined, handle)),
        }
    }

    #[must_use]
    pub fn view(&self, kind: HandleKind) -> SlotView {
        self.slots[kind.index()].view()
    }

    #[must_use]
    pub fn live_is_finished(&self, kind: HandleKind) -> Option<bool> {
        match &self.slots[kind.index()].0 {
            SlotState::Live(handle) => Some(handle.is_finished()),
            SlotState::NotSpawned | SlotState::Joined(_) => None,
        }
    }

    /// 只借用真实 Ready 结果；结果与原句柄继续留在同一个槽。
    pub fn poll_ready(&mut self, kind: HandleKind) -> ReadyProbe<'_> {
        match &mut self.slots[kind.index()].0 {
            SlotState::Live(WorkerSlot::Ipc(handle)) => match handle.poll_ready() {
                crate::ipc_thread::ReadyState::Pending => ReadyProbe::Pending,
                crate::ipc_thread::ReadyState::Ready => ReadyProbe::Ready,
                crate::ipc_thread::ReadyState::Failed(error) => ReadyProbe::IpcFailed(error),
                crate::ipc_thread::ReadyState::Disconnected => ReadyProbe::Disconnected,
            },
            SlotState::Live(WorkerSlot::Publisher(handle)) => match handle.poll_ready() {
                crate::ipc_thread::ReadyState::Pending => ReadyProbe::Pending,
                crate::ipc_thread::ReadyState::Ready => ReadyProbe::Ready,
                crate::ipc_thread::ReadyState::Failed(error) => ReadyProbe::PublisherFailed(error),
                crate::ipc_thread::ReadyState::Disconnected => ReadyProbe::Disconnected,
            },
            #[cfg(feature = "test-comm-fault-hook")]
            SlotState::Live(WorkerSlot::HookControl(handle)) => match handle.poll_ready() {
                crate::ipc_thread::ReadyState::Pending => ReadyProbe::Pending,
                crate::ipc_thread::ReadyState::Ready => ReadyProbe::Ready,
                crate::ipc_thread::ReadyState::Failed(error) => ReadyProbe::HookFailed(error),
                crate::ipc_thread::ReadyState::Disconnected => ReadyProbe::Disconnected,
            },
            SlotState::Live(WorkerSlot::ClearRr(handle)) => rr_ready(&handle.poll_ready()),
            SlotState::Live(WorkerSlot::SetRr(handle)) => rr_ready(&handle.poll_ready()),
            SlotState::Live(_) => ReadyProbe::NotReadyWorker,
            SlotState::NotSpawned => ReadyProbe::NotSpawned,
            SlotState::Joined(_) => ReadyProbe::Joined,
        }
    }

    /// 启动集合仍未 join 时按值拆出全部原句柄；任何 Joined 都完整返原集合。
    #[allow(
        clippy::result_large_err,
        reason = "拆分失败完整返还全部原槽，禁止丢弃或另行分配原退出责任"
    )]
    pub fn try_into_unjoined(
        self,
    ) -> Result<(HandleKindSet, [Option<WorkerSlot>; HandleKind::COUNT]), Self> {
        if self
            .slots
            .iter()
            .any(|slot| matches!(slot.0, SlotState::Joined(_)))
        {
            return Err(self);
        }
        let Self { slots, required } = self;
        Ok((
            required,
            slots.map(|slot| match slot.0 {
                SlotState::Live(handle) => Some(handle),
                SlotState::NotSpawned => None,
                SlotState::Joined(_) => unreachable!("已核没有 Joined"),
            }),
        ))
    }

    #[must_use]
    pub const fn required(&self) -> HandleKindSet {
        self.required
    }
}

#[derive(Debug)]
pub enum ReadyProbe<'a> {
    Pending,
    Ready,
    Disconnected,
    NotSpawned,
    Joined,
    NotReadyWorker,
    IpcFailed(&'a crate::ipc_thread::IpcInitError),
    PublisherFailed(&'a robot_transport::TransportError),
    RrFailed(&'a crate::rr_server_thread::RrInitError),
    #[cfg(feature = "test-comm-fault-hook")]
    HookFailed(&'a std::io::Error),
}
fn rr_ready<'a>(
    state: &crate::ipc_thread::ReadyState<'a, crate::rr_server_thread::RrInitError>,
) -> ReadyProbe<'a> {
    match state {
        crate::ipc_thread::ReadyState::Pending => ReadyProbe::Pending,
        crate::ipc_thread::ReadyState::Ready => ReadyProbe::Ready,
        crate::ipc_thread::ReadyState::Failed(error) => ReadyProbe::RrFailed(error),
        crate::ipc_thread::ReadyState::Disconnected => ReadyProbe::Disconnected,
    }
}

// ============================================================================
// 通用有界 join
// ============================================================================

/// [`join_with_deadline`] 的结果；`NotSpawned` / `AlreadyJoined` 绝不携带 [`JoinedOutcome`]。
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "单次回收按值移交真实结果及原残件，保持最终退出的唯一拥有链"
)]
pub enum SlotJoin {
    NotSpawned,
    AlreadyJoined,
    Pending,
    Joined(JoinedOutcome),
}

/// 唯一能把槽推进到 `Joined` 的入口。`Live`：轮询 `is_finished` 直到为真或
/// `now ≥ deadline_ns`；为真则 take + 真实 join——`Ok(outcome)` → 槽转 `Joined(kind)`，
/// `Err(handle)`（health 尚未结束）→ 放回 `Live`、按 `Pending` 处理；否则 `Pending` 且 `Live`
/// 保留。`NotSpawned` / `Joined` 各返回对应变体。`poll` 必须为正（假时钟依赖它推进时间）。
pub fn join_with_deadline(
    slot: &mut Slot,
    clock: &dyn RobotClock,
    deadline_ns: u64,
    poll: DurationNs,
) -> SlotJoin {
    assert!(poll.0 > 0, "join 轮询间隔必须为正");
    let finished = match &slot.0 {
        SlotState::NotSpawned => return SlotJoin::NotSpawned,
        SlotState::Joined(_) => return SlotJoin::AlreadyJoined,
        SlotState::Live(handle) => wait_finished(handle, clock, deadline_ns, poll),
    };
    if !finished {
        return SlotJoin::Pending;
    }
    let Some(handle) = slot.take_live() else {
        return SlotJoin::Pending;
    };
    let kind = handle.kind();
    match handle.join() {
        Ok(outcome) => {
            slot.0 = SlotState::Joined(kind);
            SlotJoin::Joined(outcome)
        }
        Err(handle) => {
            slot.0 = SlotState::Live(handle);
            SlotJoin::Pending
        }
    }
}

fn wait_finished(
    handle: &WorkerSlot,
    clock: &dyn RobotClock,
    deadline_ns: u64,
    poll: DurationNs,
) -> bool {
    loop {
        if handle.is_finished() {
            return true;
        }
        let now = clock.now().0;
        if now >= deadline_ns {
            return false;
        }
        clock.sleep_for(DurationNs(poll.0.min(deadline_ns - now)));
    }
}

// ============================================================================
// 门谓词注入的顺序停止
// ============================================================================

/// 门谓词由调用方注入；W3 接真实 ACK / Clear 退休 / `flushed`。机制不定义 ACK 来源、不持
/// authority。
pub trait TeardownGates {
    /// ①：控制面停止并通知（如 `STOPPING=1`）。
    fn control_stop_and_notify(&mut self);
    /// ③：真实 ACK（Lower 确认）是否已到达；为真才停 Executor。
    fn ack_reached(&mut self, now_ns: u64) -> bool;
    /// ④：Clear 已退休且 flush 完成；为真才停 IPC 与 writer。
    fn clear_retired_and_flushed(&mut self, now_ns: u64) -> bool;
}

/// 关闭失败原因（全部只进入唯一 [`TeardownReport`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownFailure {
    /// `required` 中的句柄未创建（可选未创建不计）。
    RequiredMissing(HandleKind),
    /// 截止 L 前 ACK 未到：Executor / Ipc / `LatchWriter` / Watchdog 均未发停止。
    LoweringUnconfirmed,
    /// ACK 已到但 Clear 退休 / flush 未在 L 前完成：Ipc / `LatchWriter` / Watchdog 未发停止。
    ClearOrFlushIncomplete,
    /// 截止 L 时仍未结束：句柄移入残余。
    JoinTimeout(HandleKind),
    /// 已真实 join 且线程 panic（不进残余）。
    Panicked(HandleKind),
}

/// 残余句柄：非 `Clone`、无公开取出；只随 [`RunExit`] 活到 `process::exit`，从不 `Drop`。
///
/// ```compile_fail,E0599
/// use robot_s0::shutdown_mechanism::ResidualHandles;
/// fn duplicate(residual: ResidualHandles) { let _ = residual.clone(); }
/// ```
#[derive(Debug)]
pub struct ResidualHandles(Vec<WorkerSlot>, Option<HandleSlots>);

impl ResidualHandles {
    /// 收入启动数组或无法拆出的原集合；拒绝安装的句柄独立保管，不按 kind 覆盖。
    #[must_use]
    pub fn from_startup(
        accepted: Result<[Option<WorkerSlot>; HandleKind::COUNT], HandleSlots>,
        rejected: Option<WorkerSlot>,
    ) -> Self {
        let (mut handles, opaque) = match accepted {
            Ok(array) => (array.into_iter().flatten().collect::<Vec<_>>(), None),
            Err(slots) => (Vec::new(), Some(slots)),
        };
        if let Some(handle) = rejected {
            handles.push(handle);
        }
        Self(handles, opaque)
    }
    pub fn retain_extra(&mut self, handle: WorkerSlot) {
        self.0.push(handle);
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.kinds().next().is_none()
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.kinds().count()
    }
    #[must_use]
    pub fn bitmap(&self) -> u16 {
        self.kinds().fold(0, |bits, kind| bits | kind.bit())
    }
    pub fn kinds(&self) -> impl Iterator<Item = HandleKind> + '_ {
        self.0
            .iter()
            .map(WorkerSlot::kind)
            .chain(self.1.iter().flat_map(|slots| {
                HandleKind::ALL
                    .into_iter()
                    .filter(|kind| matches!(slots.view(*kind), SlotView::Live(_)))
            }))
    }
}

/// 全部真实结果的唯一归宿。
#[derive(Debug)]
pub struct TeardownReport {
    /// 真实 join 过的 worker（含 panic 的 `Err`）。
    pub joined: Vec<(HandleKind, thread::Result<()>)>,
    writer: Option<(thread::Result<WriterExit>, WriterJoined)>,
    ipc: Option<thread::Result<crate::ipc_thread::IpcExit>>,
    executor: Option<thread::Result<crate::hard_rt_ctrl::ExecutorExit>>,
    pub residual: ResidualHandles,
    pub failures: Vec<ShutdownFailure>,
    /// 仅在 Executor 槽 `Live → Joined` 时为 true（空槽无同步含义）。
    pub executor_joined: bool,
    /// 仅在 Ipc 槽 `Live → Joined` 时为 true。
    pub ipc_joined: bool,
    /// 每次真实发出停止请求的 `(kind, now_ns)`，按发出顺序。
    pub stop_trace: Vec<(HandleKind, u64)>,
}

impl TeardownReport {
    fn new() -> Self {
        Self {
            joined: Vec::new(),
            writer: None,
            ipc: None,
            executor: None,
            residual: ResidualHandles(Vec::new(), None),
            failures: Vec::new(),
            executor_joined: false,
            ipc_joined: false,
            stop_trace: Vec::new(),
        }
    }

    /// 按值一次取出 writer 的真实结果与唯一凭证（供 W3 `StandbyStore::after_join`）；第二次为
    /// `None`。
    pub fn take_writer(&mut self) -> Option<(thread::Result<WriterExit>, WriterJoined)> {
        self.writer.take()
    }

    /// 仅真实 IPC join 的原值，包含尚未退休的原 owned 责任；第二次取为 None。
    pub fn take_ipc(&mut self) -> Option<thread::Result<crate::ipc_thread::IpcExit>> {
        self.ipc.take()
    }

    /// 一次按值移交真实执行器退出结果；非空 Enable 残件仍完整持有。
    pub fn take_executor(&mut self) -> Option<thread::Result<crate::hard_rt_ctrl::ExecutorExit>> {
        self.executor.take()
    }
}

/// 按值单次消费 [`HandleSlots`]：随后该集合不可再用，不存在第二次收尾；全部 `Joined` /
/// panic / [`WriterJoined`] / 残余只进入返回的报告。
///
/// 顺序（沿监督 r4）：先对 `required` 中 `NotSpawned` 记 `RequiredMissing`（可选未创建跳过）；
/// ① `control_stop_and_notify`；② 对活着的辅助句柄发停止；③ 轮询 `ack_reached` 至 L：真 →
/// 停 Executor，假 → `LoweringUnconfirmed`（Executor / Ipc / `LatchWriter` / Watchdog 均不发）；
/// ④ 仅③成功后轮询 `clear_retired_and_flushed`：真 → 停 Ipc + LatchWriter，假 →
/// `ClearOrFlushIncomplete`；⑤ 仅④成功后停 Watchdog；⑥ 对所有活槽 [`join_with_deadline`]
/// 至全部 `Joined` 或 L。无 `stop_all`。`poll` 必须为正。
pub fn teardown(
    slots: HandleSlots,
    gates: &mut dyn TeardownGates,
    clock: &dyn RobotClock,
    deadline_l_ns: u64,
    poll: DurationNs,
) -> TeardownReport {
    assert!(poll.0 > 0, "关闭轮询间隔必须为正");
    let HandleSlots {
        mut slots,
        required,
    } = slots;
    let mut report = TeardownReport::new();
    for kind in required.kinds() {
        if slots[kind.index()].view() == SlotView::NotSpawned {
            report.failures.push(ShutdownFailure::RequiredMissing(kind));
        }
    }

    gates.control_stop_and_notify();
    for kind in HandleKind::HELPERS {
        stop_if_live(&slots, kind, clock, &mut report.stop_trace);
    }

    let ack = poll_gate(clock, deadline_l_ns, poll, |now| gates.ack_reached(now));
    if ack {
        stop_if_live(&slots, HandleKind::Executor, clock, &mut report.stop_trace);
    } else {
        report.failures.push(ShutdownFailure::LoweringUnconfirmed);
    }

    let cleared = ack
        && poll_gate(clock, deadline_l_ns, poll, |now| {
            gates.clear_retired_and_flushed(now)
        });
    if ack {
        if cleared {
            stop_if_live(&slots, HandleKind::Ipc, clock, &mut report.stop_trace);
            stop_if_live(
                &slots,
                HandleKind::LatchWriter,
                clock,
                &mut report.stop_trace,
            );
        } else {
            report
                .failures
                .push(ShutdownFailure::ClearOrFlushIncomplete);
        }
    }
    if cleared {
        stop_if_live(&slots, HandleKind::Watchdog, clock, &mut report.stop_trace);
    }

    for kind in HandleKind::ALL {
        collect_join(
            &mut slots[kind.index()],
            kind,
            clock,
            deadline_l_ns,
            poll,
            &mut report,
        );
    }
    report
}

fn stop_if_live(
    slots: &[Slot; HandleKind::COUNT],
    kind: HandleKind,
    clock: &dyn RobotClock,
    trace: &mut Vec<(HandleKind, u64)>,
) {
    if let SlotState::Live(handle) = &slots[kind.index()].0 {
        handle.request_stop();
        trace.push((kind, clock.now().0));
    }
}

/// 只在截止之前评估门；已到截止不再接纳新确认，也不主动睡过剩余预算。
fn poll_gate(
    clock: &dyn RobotClock,
    deadline_ns: u64,
    poll: DurationNs,
    mut gate: impl FnMut(u64) -> bool,
) -> bool {
    loop {
        let now = clock.now().0;
        if now >= deadline_ns {
            return false;
        }
        if gate(now) {
            // 谓词自身也可能消耗剩余预算；返回后到达截止的确认不能补记成功。
            return clock.now().0 < deadline_ns;
        }
        let remaining = deadline_ns.saturating_sub(clock.now().0);
        if remaining == 0 {
            return false;
        }
        clock.sleep_for(DurationNs(poll.0.min(remaining)));
    }
}

fn collect_join(
    entry: &mut Slot,
    kind: HandleKind,
    clock: &dyn RobotClock,
    deadline_ns: u64,
    poll: DurationNs,
    report: &mut TeardownReport,
) {
    match join_with_deadline(entry, clock, deadline_ns, poll) {
        SlotJoin::NotSpawned | SlotJoin::AlreadyJoined => {}
        SlotJoin::Joined(JoinedOutcome::Worker(result)) => {
            if result.is_err() {
                report.failures.push(ShutdownFailure::Panicked(kind));
            }
            match kind {
                HandleKind::Executor => report.executor_joined = true,
                HandleKind::Ipc => report.ipc_joined = true,
                _ => {}
            }
            report.joined.push((kind, result));
        }
        SlotJoin::Joined(JoinedOutcome::Writer { result, joined }) => {
            if result.is_err() {
                report.failures.push(ShutdownFailure::Panicked(kind));
            }
            report.writer = Some((result, joined));
        }
        SlotJoin::Joined(JoinedOutcome::Ipc(result)) => {
            if result.is_err() {
                report.failures.push(ShutdownFailure::Panicked(kind));
            }
            report.ipc_joined = true;
            report.ipc = Some(result);
        }
        SlotJoin::Joined(JoinedOutcome::Executor(result)) => {
            if result.is_err() {
                report.failures.push(ShutdownFailure::Panicked(kind));
            }
            report.executor_joined = true;
            report.executor = Some(result);
        }
        SlotJoin::Pending => {
            report.failures.push(ShutdownFailure::JoinTimeout(kind));
            if let Some(handle) = entry.take_live() {
                report.residual.0.push(handle);
            }
        }
    }
}

// ============================================================================
// 退出
// ============================================================================

/// `run()` 的出口：退出码与残余句柄；W3 的 `main` 唯一调用 [`Self::exit`]。
#[derive(Debug)]
pub struct RunExit {
    code: i32,
    residual: ResidualHandles,
}

impl RunExit {
    #[must_use]
    pub fn new(code: i32, residual: ResidualHandles) -> Self {
        Self { code, residual }
    }

    #[must_use]
    pub const fn code(&self) -> i32 {
        self.code
    }

    /// 测试 / 诊断：拆出退出码与残余（残余仍无公开取出）。
    #[must_use]
    pub fn into_parts(self) -> (i32, ResidualHandles) {
        (self.code, self.residual)
    }

    /// 进程出口：残余句柄从不 `Drop`（health 句柄的 `Drop` 会阻塞 join），随进程一起消失。
    /// 验收 hook 下先在 `before_process_exit` 阶段冻结（W3 进程门）。
    pub fn exit(self) -> ! {
        #[cfg(feature = "test-comm-fault-hook")]
        crate::comm_fault_hook::record_process_exit_boundary(self.residual.bitmap());
        #[cfg(feature = "test-comm-fault-hook")]
        crate::comm_fault_hook::boot_pause("before_process_exit");
        let Self { code, residual } = self;
        std::mem::forget(residual);
        std::process::exit(code)
    }
}

/// 退出码真值表输入（90 由独立 D 产生，不在此函数）。
#[derive(Debug, Clone, Copy)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "真值表各输入语义独立，均为已批准的布尔判据"
)]
pub struct CleanInputs<'a> {
    pub runtime_ok: bool,
    /// ACK 必须早于 Executor 停止（晚于则不清洁）。
    pub ack_before_executor_stop: bool,
    pub clear_retired: bool,
    /// `1` = flush 完成；`0` / `2` / `3` 均不清洁。
    pub flushed: u8,
    pub failures: &'a [ShutdownFailure],
    pub residual_empty: bool,
}

/// 全部满足且 `flushed == 1` 且 `failures` 空 → 0，否则 1。
#[must_use]
pub fn exit_code(inputs: &CleanInputs<'_>) -> i32 {
    let clean = inputs.runtime_ok
        && inputs.ack_before_executor_stop
        && inputs.clear_retired
        && inputs.flushed == 1
        && inputs.failures.is_empty()
        && inputs.residual_empty;
    i32::from(!clean)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use robot_core::time::{MockClock, MonoRawNs, SystemClock};
    use std::time::{Duration, Instant};

    const L: u64 = 1_000_000;
    const POLL: DurationNs = DurationNs(1_000);

    /// 记录门调用顺序与评估时刻的脚本化门。
    struct ScriptedGates {
        ack: bool,
        clear: bool,
        calls: Vec<&'static str>,
        ack_nows: Vec<u64>,
        clear_nows: Vec<u64>,
    }

    impl ScriptedGates {
        fn new(ack: bool, clear: bool) -> Self {
            Self {
                ack,
                clear,
                calls: Vec::new(),
                ack_nows: Vec::new(),
                clear_nows: Vec::new(),
            }
        }
    }

    impl TeardownGates for ScriptedGates {
        fn control_stop_and_notify(&mut self) {
            self.calls.push("control");
        }
        fn ack_reached(&mut self, now_ns: u64) -> bool {
            self.calls.push("ack");
            self.ack_nows.push(now_ns);
            self.ack
        }
        fn clear_retired_and_flushed(&mut self, now_ns: u64) -> bool {
            self.calls.push("clear");
            self.clear_nows.push(now_ns);
            self.clear
        }
    }

    /// 私有测试域持有槽到真实 join；断言失败时也不能分离活线程。
    struct SlotReclaimer(Slot);

    impl Drop for SlotReclaimer {
        fn drop(&mut self) {
            if let Some(handle) = self.0.take_live() {
                reclaim_handle(handle);
            }
        }
    }

    struct SetReclaimer(HandleSlots);

    impl SetReclaimer {
        fn take(&mut self) -> HandleSlots {
            std::mem::replace(&mut self.0, HandleSlots::new(HandleKindSet::EMPTY))
        }
    }

    impl Drop for SetReclaimer {
        fn drop(&mut self) {
            for slot in &mut self.0.slots {
                if let Some(handle) = slot.take_live() {
                    reclaim_handle(handle);
                }
            }
        }
    }

    struct ReportReclaimer(TeardownReport);

    impl Drop for ReportReclaimer {
        fn drop(&mut self) {
            while let Some(handle) = self.0.residual.0.pop() {
                reclaim_handle(handle);
            }
        }
    }

    fn reclaim_handle(mut handle: WorkerSlot) {
        handle.request_stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if Instant::now() >= deadline {
                std::process::abort();
            }
            while !handle.is_finished() {
                if Instant::now() >= deadline {
                    // 测试线程不可强杀；保留句柄直至本测试进程失败终止。
                    std::process::abort();
                }
                thread::sleep(Duration::from_millis(1));
            }
            match handle.join() {
                Ok(_) => return,
                Err(returned) => handle = returned,
            }
        }
    }

    /// 以 `stop` 为退出条件的真实线程，spawn 后立即由槽守卫持有。
    fn raw(kind: HandleKind) -> (SlotReclaimer, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let join = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
        });
        (
            SlotReclaimer(Slot::live(WorkerSlot::Raw {
                kind,
                join,
                stop: Arc::clone(&stop),
            })),
            stop,
        )
    }

    fn wait_until(condition: impl Fn() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "{what}");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn kinds_have_distinct_bits_names_and_indices() {
        let mut bits = 0_u16;
        for (position, kind) in HandleKind::ALL.iter().enumerate() {
            assert_eq!(kind.index(), position);
            assert_eq!(bits & kind.bit(), 0, "{kind:?} 位重复");
            bits |= kind.bit();
        }
        assert_eq!(bits, 0b1111_1111_1111);
        let names: std::collections::HashSet<&str> =
            HandleKind::ALL.iter().map(|kind| kind.name()).collect();
        assert_eq!(names.len(), HandleKind::COUNT);
        assert!(!HandleKind::HELPERS.contains(&HandleKind::Executor));
        assert!(!HandleKind::HELPERS.contains(&HandleKind::Ipc));
        assert!(!HandleKind::HELPERS.contains(&HandleKind::LatchWriter));
        assert!(!HandleKind::HELPERS.contains(&HandleKind::Watchdog));
    }

    #[test]
    fn kind_set_membership_is_explicit() {
        let set = HandleKindSet::EMPTY
            .with(HandleKind::Executor)
            .with(HandleKind::Ipc);
        assert!(set.contains(HandleKind::Executor) && set.contains(HandleKind::Ipc));
        assert!(!set.contains(HandleKind::Watchdog));
        assert_eq!(
            set.kinds().collect::<Vec<_>>(),
            vec![HandleKind::Executor, HandleKind::Ipc]
        );
        assert_eq!(
            set.bits(),
            HandleKind::Executor.bit() | HandleKind::Ipc.bit()
        );
        assert_eq!(HandleKindSet::default(), HandleKindSet::EMPTY);
    }

    #[test]
    fn exit_code_truth_table() {
        let clean = CleanInputs {
            runtime_ok: true,
            ack_before_executor_stop: true,
            clear_retired: true,
            flushed: 1,
            failures: &[],
            residual_empty: true,
        };
        assert_eq!(exit_code(&clean), 0);
        for broken in [
            CleanInputs {
                runtime_ok: false,
                ..clean
            },
            CleanInputs {
                ack_before_executor_stop: false,
                ..clean
            },
            CleanInputs {
                clear_retired: false,
                ..clean
            },
            CleanInputs {
                flushed: 0,
                ..clean
            },
            CleanInputs {
                flushed: 2,
                ..clean
            },
            CleanInputs {
                flushed: 3,
                ..clean
            },
            CleanInputs {
                residual_empty: false,
                ..clean
            },
            CleanInputs {
                failures: &[ShutdownFailure::RequiredMissing(HandleKind::Executor)],
                ..clean
            },
            CleanInputs {
                failures: &[ShutdownFailure::JoinTimeout(HandleKind::Io)],
                ..clean
            },
        ] {
            assert_eq!(exit_code(&broken), 1, "{broken:?}");
        }
    }

    #[test]
    fn join_with_deadline_never_forges_joined_for_empty_or_joined_slots() {
        let clock = MockClock::new(MonoRawNs(0));
        let mut empty = Slot::not_spawned();
        assert!(matches!(
            join_with_deadline(&mut empty, &clock, L, POLL),
            SlotJoin::NotSpawned
        ));
        assert_eq!(empty.view(), SlotView::NotSpawned);

        let (mut live, stop) = raw(HandleKind::Io);
        stop.store(true, Ordering::Release);
        assert_eq!(live.0.view(), SlotView::Live(HandleKind::Io));
        let real = SystemClock;
        let far = real.now().0 + 5_000_000_000;
        assert!(matches!(
            join_with_deadline(&mut live.0, &real, far, DurationNs::from_millis(1)),
            SlotJoin::Joined(JoinedOutcome::Worker(Ok(())))
        ));
        assert_eq!(live.0.view(), SlotView::Joined(HandleKind::Io));
        assert!(matches!(
            join_with_deadline(&mut live.0, &clock, L, POLL),
            SlotJoin::AlreadyJoined
        ));
    }

    #[test]
    fn join_with_deadline_keeps_live_when_thread_still_runs_past_deadline() {
        let (mut live, stop) = raw(HandleKind::Publisher);
        let real = SystemClock;
        let soon = real.now().0 + 20_000_000;
        assert!(matches!(
            join_with_deadline(&mut live.0, &real, soon, DurationNs::from_millis(1)),
            SlotJoin::Pending
        ));
        assert_eq!(live.0.view(), SlotView::Live(HandleKind::Publisher));
        stop.store(true, Ordering::Release);
        let far = real.now().0 + 5_000_000_000;
        assert!(matches!(
            join_with_deadline(&mut live.0, &real, far, DurationNs::from_millis(1)),
            SlotJoin::Joined(JoinedOutcome::Worker(Ok(())))
        ));
    }

    #[test]
    fn spawned_rejects_duplicate_kind_and_returns_the_handle() {
        let mut slots = SetReclaimer(HandleSlots::new(HandleKindSet::EMPTY));
        let (mut first, first_stop) = raw(HandleKind::Publisher);
        let (mut second, second_stop) = raw(HandleKind::Publisher);
        if let Err((error, handle)) = slots.0.spawned(first.0.take_live().unwrap()) {
            reclaim_handle(handle);
            panic!("空集合首次插入失败：{error:?}");
        }
        let Err((error, returned)) = slots.0.spawned(second.0.take_live().unwrap()) else {
            panic!("同 kind 二次插入必须被拒绝");
        };
        let mut returned = SlotReclaimer(Slot::live(returned));
        assert_eq!(error, InsertError::AlreadyPresent);
        assert_eq!(returned.0.view(), SlotView::Live(HandleKind::Publisher));
        assert_eq!(
            slots.0.view(HandleKind::Publisher),
            SlotView::Live(HandleKind::Publisher)
        );
        // 返还的原句柄仍可显式停止并真实 join。
        second_stop.store(true, Ordering::Release);
        let real = SystemClock;
        assert!(matches!(
            join_with_deadline(
                &mut returned.0,
                &real,
                real.now().0 + 5_000_000_000,
                DurationNs::from_millis(1)
            ),
            SlotJoin::Joined(JoinedOutcome::Worker(Ok(())))
        ));
        assert!(second_stop.load(Ordering::Acquire));
        // 集合内的活句柄由 teardown 真实收尾。
        let mut gates = ScriptedGates::new(true, true);
        let report = ReportReclaimer(teardown(
            slots.take(),
            &mut gates,
            &real,
            real.now().0 + 5_000_000_000,
            DurationNs::from_millis(1),
        ));
        assert!(first_stop.load(Ordering::Acquire));
        assert!(report.0.failures.is_empty(), "{:?}", report.0.failures);
        assert_eq!(report.0.joined.len(), 1);
    }

    #[test]
    fn teardown_without_live_slots_reports_required_missing_and_gate_failures() {
        let clock = MockClock::new(MonoRawNs(10));
        let slots = HandleSlots::new(
            HandleKindSet::EMPTY
                .with(HandleKind::Executor)
                .with(HandleKind::Ipc),
        );
        let mut gates = ScriptedGates::new(false, true);
        let mut report = teardown(slots, &mut gates, &clock, 10 + L, POLL);
        assert_eq!(
            report.failures,
            vec![
                ShutdownFailure::RequiredMissing(HandleKind::Executor),
                ShutdownFailure::RequiredMissing(HandleKind::Ipc),
                ShutdownFailure::LoweringUnconfirmed,
            ]
        );
        assert!(report.stop_trace.is_empty(), "空槽不发停止");
        assert!(report.residual.is_empty() && report.joined.is_empty());
        assert!(!report.executor_joined && !report.ipc_joined);
        assert!(report.take_writer().is_none());
        assert_eq!(gates.calls.first(), Some(&"control"));
        assert!(!gates.calls.contains(&"clear"), "ACK 失败不评估 Clear 门");
        // 只在截止之前评估 ACK；睡眠恰好用完预算后不再调用谓词。
        assert!(gates.ack_nows.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(*gates.ack_nows.last().unwrap() < 10 + L);
        assert_eq!(clock.now().0, 10 + L);
        assert_eq!(gates.ack_nows.first(), Some(&10));
    }

    #[test]
    fn teardown_clear_failure_after_ack_is_reported_without_evaluating_watchdog_stop() {
        let clock = MockClock::new(MonoRawNs(0));
        let slots = HandleSlots::new(HandleKindSet::EMPTY);
        let mut gates = ScriptedGates::new(true, false);
        let report = teardown(slots, &mut gates, &clock, L, POLL);
        assert_eq!(
            report.failures,
            vec![ShutdownFailure::ClearOrFlushIncomplete]
        );
        assert_eq!(gates.ack_nows.len(), 1, "ACK 首次评估即真");
        assert!(gates.clear_nows.len() > 1 && *gates.clear_nows.last().unwrap() < L);
        assert_eq!(clock.now().0, L);
    }

    #[test]
    fn gate_deadline_refuses_late_first_success_and_never_evaluates_after_expiry() {
        // 同一轮询入口同时服务 ACK 与 Clear，不能因第一次为真而越过截止。
        for start in [0, 5, 6] {
            for true_at in [5, 10] {
                let clock = MockClock::new(MonoRawNs(start));
                let mut calls = Vec::new();
                assert!(!poll_gate(&clock, 5, DurationNs(10), |now| {
                    calls.push(now);
                    now >= true_at
                }));
                assert_eq!(calls, if start == 0 { vec![0] } else { vec![] });
                assert_eq!(clock.now().0, start.max(5), "sleep 不超过剩余预算");
            }
        }
        for start in [5, 6] {
            let clock = MockClock::new(MonoRawNs(start));
            let mut gates = ScriptedGates::new(true, true);
            let report = teardown(
                HandleSlots::new(HandleKindSet::EMPTY),
                &mut gates,
                &clock,
                5,
                DurationNs(10),
            );
            assert_eq!(report.failures, vec![ShutdownFailure::LoweringUnconfirmed]);
            assert!(gates.ack_nows.is_empty() && gates.clear_nows.is_empty());
        }
    }

    #[test]
    fn gate_success_that_spends_the_remaining_budget_is_rejected() {
        for returned_at in [5, 6] {
            let clock = MockClock::new(MonoRawNs(0));
            let mut calls = 0;
            assert!(!poll_gate(&clock, 5, DurationNs(10), |_| {
                calls += 1;
                clock.set(MonoRawNs(returned_at));
                true
            }));
            assert_eq!(calls, 1);
            assert_eq!(clock.now().0, returned_at);
        }
    }

    /// ACK 的两次取时仍在预算内，第三次（进入 Clear 门）推进至指定边界。
    struct ClearEntryClock {
        inner: MockClock,
        reads: std::sync::atomic::AtomicUsize,
        clear_entry: u64,
    }

    impl RobotClock for ClearEntryClock {
        fn now(&self) -> MonoRawNs {
            if self.reads.fetch_add(1, Ordering::Relaxed) == 2 {
                self.inner.set(MonoRawNs(self.clear_entry));
            }
            self.inner.now()
        }

        fn sleep_until(&self, deadline: robot_core::time::Deadline) {
            self.inner.sleep_until(deadline);
        }

        fn sleep_for(&self, duration: DurationNs) {
            self.inner.sleep_for(duration);
        }
    }

    #[test]
    fn clear_gate_is_not_called_when_ack_spent_its_entry_budget() {
        for clear_entry in [5, 6] {
            let clock = ClearEntryClock {
                inner: MockClock::new(MonoRawNs(0)),
                reads: std::sync::atomic::AtomicUsize::new(0),
                clear_entry,
            };
            let mut gates = ScriptedGates::new(true, true);
            let report = teardown(
                HandleSlots::new(HandleKindSet::EMPTY),
                &mut gates,
                &clock,
                5,
                DurationNs(10),
            );
            assert_eq!(
                report.failures,
                vec![ShutdownFailure::ClearOrFlushIncomplete]
            );
            assert_eq!(gates.ack_nows, [0]);
            assert!(gates.clear_nows.is_empty(), "截止后不得调用 Clear 谓词");
        }
    }

    #[test]
    fn clear_first_success_after_deadline_does_not_pass_teardown() {
        struct LateClear(Vec<u64>);
        impl TeardownGates for LateClear {
            fn control_stop_and_notify(&mut self) {}
            fn ack_reached(&mut self, _: u64) -> bool {
                true
            }
            fn clear_retired_and_flushed(&mut self, now: u64) -> bool {
                self.0.push(now);
                now >= 10
            }
        }
        let clock = MockClock::new(MonoRawNs(0));
        let mut gates = LateClear(Vec::new());
        let report = teardown(
            HandleSlots::new(HandleKindSet::EMPTY),
            &mut gates,
            &clock,
            5,
            DurationNs(10),
        );
        assert_eq!(
            report.failures,
            vec![ShutdownFailure::ClearOrFlushIncomplete]
        );
        assert_eq!(gates.0, [0]);
        assert_eq!(clock.now().0, 5);
    }

    #[test]
    fn wait_finished_caps_sleep_and_joins_already_finished_after_deadline() {
        let clock = MockClock::new(MonoRawNs(0));
        let (mut slot, stop) = raw(HandleKind::Publisher);
        assert!(matches!(
            join_with_deadline(&mut slot.0, &clock, 5, DurationNs(10)),
            SlotJoin::Pending
        ));
        assert_eq!(clock.now().0, 5);
        stop.store(true, Ordering::Release);
        wait_until(
            || match &slot.0.0 {
                SlotState::Live(handle) => handle.is_finished(),
                _ => false,
            },
            "真实线程应结束",
        );
        clock.set(MonoRawNs(6));
        assert!(matches!(
            join_with_deadline(&mut slot.0, &clock, 5, DurationNs(10)),
            SlotJoin::Joined(JoinedOutcome::Worker(Ok(())))
        ));
        assert_eq!(slot.0.view(), SlotView::Joined(HandleKind::Publisher));
    }

    #[test]
    #[should_panic(expected = "关闭轮询间隔必须为正")]
    fn teardown_rejects_zero_poll() {
        let clock = MockClock::new(MonoRawNs(0));
        let mut gates = ScriptedGates::new(true, true);
        let _ = teardown(
            HandleSlots::new(HandleKindSet::EMPTY),
            &mut gates,
            &clock,
            L,
            DurationNs(0),
        );
    }

    #[test]
    fn run_exit_keeps_code_and_residual() {
        let clock = MockClock::new(MonoRawNs(0));
        let mut gates = ScriptedGates::new(true, true);
        let report = teardown(
            HandleSlots::new(HandleKindSet::EMPTY),
            &mut gates,
            &clock,
            L,
            POLL,
        );
        let run = RunExit::new(1, report.residual);
        assert_eq!(run.code(), 1);
        let (code, residual) = run.into_parts();
        assert_eq!(code, 1);
        assert!(residual.is_empty());
        assert_eq!(residual.bitmap(), 0);
        assert_eq!(residual.len(), 0);
    }
}
