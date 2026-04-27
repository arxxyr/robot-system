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

use robot_core::executor_ctrl::ExecutorCtrlClient;

use crate::estop_latch::save_estop_latch;
use crate::ipc_tick::{ClearEStopTickOutcome, IpcRetryState, ipc_tick};
use crate::state_machine::{Event, EventQueue, Guards, S0State, S0StateSnapshot};

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
    /// 新实现：本字段 `Some` 时，`ipc_thread` 主循环每 tick 比较
    /// `prev_estop_latched` vs `guards.estop_latched`：从 false→true 立即
    /// `save_estop_latch(latch_path, true)`（双文件 `fsync` rename，~ms 级）；
    /// 从 true→false（`ClearEStop` 处理）立即 `save_estop_latch(false)`。
    ///
    /// `None` 仅供单测：跳过 latch IO，保持测试纯类型路径。生产 daemon 必须
    /// 传 `Some(args.latch_path.clone())`。
    pub latch_path: Option<PathBuf>,
}

impl Default for IpcThreadConfig {
    fn default() -> Self {
        Self {
            poll_interval_ns: 500_000, // 500 µs
            initial_guards: Guards::default(),
            initial_state: S0State::Safe,
            latch_path: None,
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
#[allow(clippy::too_many_arguments)]
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
) {
    let mut state = cfg.initial_state;
    let mut guards = cfg.initial_guards;
    let mut events = EventQueue::new();
    let mut retry = IpcRetryState::new();

    // **task #15 阻断 1 修复**：初值落盘 baseline。`prev_estop_latched`
    // 跟 `guards.estop_latched` 同步，每 tick 比较；翻转时立即 fsync 写
    // latch 文件。`latch_path` 为 `None` 时（单测）整条路径 no-op。
    let mut prev_estop_latched = guards.estop_latched;

    // 启动即发一次初值快照。
    state_pub.publish(&S0StateSnapshot::new(state, guards, clock.now().0));

    while !stop.load(Ordering::Acquire) {
        // ─── A2 freeze hook（test-only，0 cost when freeze_until_ns == 0）───
        #[cfg(any(test, feature = "test-freeze-hook"))]
        crate::freeze_hook::freeze_check(&metrics.freeze_until_ns, clock, stop);

        // ─── 1) data plane: JointCommand → cmd_writer (不进 EventQueue) ───
        poll_command(&mut subs.command, cmd_writer, metrics);

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

        // ─── 6) ipc_tick 三步式：唯一改 state 的路径（硬要求 #2）───
        //
        // guards 现在是 &mut，ipc_tick 内部对每个 event 单独 apply 副作用。
        // **A1-2**：`latch_path` 透传给 ipc_tick，让 `ClearEStop` action
        // handler 在 ipc_tick 内完成 fsync + clear_safety_latch + 翻 guard
        // 三步硬顺序；返回 `TickOutcome` 让本主循环跳过通用 latch hook 的
        // 二次写盘。
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
        //   gateway / RR response 在 ipc_thread 后续阶段（A1-3 (1/3b)）
        //   match 这些 variant 生成 FailedPrecondition payload。
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
        let body = build_joint_command(
            0xCAFE_0001,
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
}
