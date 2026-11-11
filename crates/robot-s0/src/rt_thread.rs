//! S0 RT 线程：`SCHED_FIFO` + `mlockall` + tick 闭环。
//!
//! 对应 `docs/00_design.md` §1.2 `s0_rt_thread` + §1.3 启动流程 + §4.9 RT 铁律。
//! 每条 `HardRT` 总线独立运行一个实例，保持唯一执行者。
//!
//! ### tick 闭环（每 tick 运行在 `rt_entry!` scope 内）
//!
//! ```text
//! clock.sleep_until(deadline)
//! deadline += tick_period
//! cmd_reader.try_load()                  // 固定一次；失败沿用完整缓存
//! adapter.write_command(&cmd.as_view())   // memcpy
//! adapter.read_state(&mut state_buf)      // memcpy
//! snapshot_pub.publish(&RtSnapshot { .. })// seqlock：2×atomic add + memcpy
//! ```
//!
//! 全程无 heap alloc、无 `Arc::clone/drop`、无 blocking syscall 除 `clock_nanosleep`。
//!
//! ### 初始化（在 RT scope 之外执行，允许 alloc）
//!
//! 1. `set_sched_fifo(cfg.sched_fifo_priority)`
//! 2. `mlockall()`
//! 3. optional `pin_to_cpu(cfg.cpu_affinity)`
//! 4. `adapter.connect()`；首个 tick 做真实安全停止，Enable 仅经原 A2 预约/执行。
//!
//! 三个 syscall 失败走 `io::Error::raw_os_error`，主线程根据策略决定是否 fail-fast
//! 或降级运行（在 CI / 无 rtprio 的开发机上允许降级）。
//!
//! ### Part 1 vs Part 2
//!
//! - Part 1：直接从 `LatestCommandReader<JointCommandBuf>` 读，不经 state machine
//!   过滤；adapter 内部根据 `enabled` 决定是否生效（保证 state machine 未进 ENABLED
//!   时写命令无害）。
//! - Part 2：`rt_thread` 读 state machine 的当前态，只在 `ENABLED.Active` 时调
//!   `write_command`；`CONTROL_LOST_*` 子态由 `rt_thread` 生成对应 holding command。

#![cfg(target_os = "linux")]
#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

use robot_core::adapter::{
    ControlMode, HardRtAdapter, JointCommandBuf, JointCommandView, JointSelection, JointStateBuf,
    RtSnapshot,
};
use robot_core::executor_ctrl::{
    AdapterErr, AdapterResp, OutputPolicy, OutputSafetyState, emergency_codes, normal_codes,
    pack_completed, unpack_pending_normal,
};
use robot_core::rt_entry;
use robot_core::thread as rt_thread_os;
use robot_core::time::{Deadline, DurationNs, RobotClock};
use robot_transport::{LatestCommandReader, RtSnapshotPublisher};

use crate::hard_rt_ctrl::{
    EnableCompensation, EnableFinishPending, HardRtCtrlInner, finish_enable_owned,
    start_enable_execution,
};
use robot_core::control_authority::{
    EnableResult, NonEnergizingControl, NormalIoIntent, SafetyOperation,
};
use robot_core::error::RobotError;
use robot_core::serial_services::AbortReason;

/// 部分 Position 本期只接入 SerialRT，不把内部掩码带入原 HardRT 合同。
fn write_full_bus<A: HardRtAdapter>(
    adapter: &mut A,
    command: &JointCommandView<'_>,
) -> Result<(), RobotError> {
    if command.selection != JointSelection::All {
        return Err(RobotError::Unsupported("HardRT暂不支持部分关节命令"));
    }
    adapter.write_command(command)
}

/// RT 线程配置。
#[derive(Clone, Copy, Debug)]
pub struct RtThreadConfig {
    /// tick 周期（纳秒）。默认 1 ms（1 kHz）。Part 1 用 20 ms（50 Hz）做 smoke 更省 CPU。
    pub tick_period_ns: u64,
    /// `SCHED_FIFO` 优先级。典型 70（§4.9）。
    pub sched_fifo_priority: i32,
    /// CPU 亲和性（None = 不绑核）。Part 1 可留空，Part 2 冲 iceoryx2 baseline 门槛时绑。
    pub cpu_affinity: Option<usize>,
    /// `SCHED_FIFO` / `mlockall` / `pin_to_cpu` 失败是否 panic（true）还是 warn-and-continue（false）。
    /// 默认 false（开发机友好）；生产 daemon 应设 true。
    pub strict_rt: bool,
}

impl Default for RtThreadConfig {
    fn default() -> Self {
        Self {
            tick_period_ns: 20_000_000, // 50 Hz
            sched_fifo_priority: 70,
            cpu_affinity: None,
            strict_rt: false,
        }
    }
}

/// RT 线程观测计数器（非 RT 测试 / 诊断路径读取）。
///
/// 所有字段 `AtomicU64`，RT 侧写 `Relaxed` 即可（单写，不需要跨 core fence）。
#[derive(Debug, Default)]
pub struct RtThreadMetrics {
    /// 成功完成的 tick 数（`sleep_until` + 一次完整 write/read/publish）。
    pub tick_count: AtomicU64,
    /// `adapter.read_state` 返回 `Err` 的次数。
    pub read_errors: AtomicU64,
    /// `adapter.write_command` 返回 `Err` 的次数。
    pub write_errors: AtomicU64,
    /// tick 超时次数（`sleep_until` 返回时 deadline 已过一段）。Part 2 作为 `RtOverrun` 事件来源。
    pub overruns: AtomicU64,
    /// task-30 v9 / M5：emergency `safety_stop` 下发次数。
    pub emergency_safety_stop_count: AtomicU64,
    /// task-30 v9 / M5：emergency drain 时 cancel 一条 in-flight pending 的次数
    /// （上层 ipc_thread 看见 `last_completed = PreemptedByEmergency`）。
    pub emergency_preempted_count: AtomicU64,
    /// task-30 v9 / M5：被 client/rt 端 latch 拒的 `Enable(true)` 计数。
    /// finding #3：拒绝是正常保护（不是 fault），但保留计数便于诊断。
    pub latch_rejected_count: AtomicU64,
    /// task-30 v9 / M5：normal action 完成（成功 + 失败）计数。
    pub normal_completed_count: AtomicU64,
    /// **S48 A3b**：`RtSnapshotPublisher::publish` 返回错误（版本耗尽 / 写端损坏）
    /// 的次数。首次出错即结束本 worker（guard 退出后结束外层循环），由
    /// `robot-s0` 的 `is_finished` / 进度停滞监督判错；不清 latch、不伪装 Ready。
    pub snapshot_publish_errors: AtomicU64,
    /// **S48 A3b（test-only）**：真实 `publish` 调用之前取的 `RobotClock` 时刻，
    /// 仅该次返回 `VersionExhausted` 时原子记录（0 = 未发生）；与 daemon 的真实
    /// READY 时刻比较可证明耗尽发生在 READY 之后。RT 内只做一次 `clock.now()`
    /// 与一次原子 store，无分配、无日志。
    #[cfg(feature = "test-comm-fault-hook")]
    pub snapshot_publish_failure_attempt_ns: AtomicU64,
    /// **A2 freeze hook**（v0.3 / test-only，cfg-gated）：tick loop 顶端 spin
    /// 到该单调时钟 ns 为止；0 = 不冻。详 [`crate::freeze_hook`] 模块 doc。
    /// 仅 `cfg(any(test, feature = "test-freeze-hook"))` 编译时存在，**不进
    /// 生产 release**。**注意**：reviewer 拍板硬边界，rt_thread hook 加，但
    /// 不在 Orin SCHED_FIFO busy-spin 冻 RT 系统级 smoke（避免调度饥饿）；
    /// rt_thread 单测 / 非 SCHED_FIFO 集成验证即可。
    #[cfg(any(test, feature = "test-freeze-hook"))]
    pub freeze_until_ns: AtomicU64,
}

/// RT 线程启动后返回的控制 handle。
pub struct RtThreadHandle {
    join: JoinHandle<crate::hard_rt_ctrl::ExecutorExit>,
    stop: Arc<AtomicBool>,
    metrics: Arc<RtThreadMetrics>,
}

impl RtThreadHandle {
    /// 请求停止。下次 tick 开始时检测到并退出。
    pub fn request_stop(&self) {
        self.stop.swap(true, Ordering::AcqRel);
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }

    /// 等待 RT 线程退出。`request_stop` 后调用。
    ///
    /// # Errors
    ///
    /// 返回 `Err` 当 RT 线程 panic（`JoinHandle::join` 的 `Result`）。
    pub fn join(self) -> thread::Result<crate::hard_rt_ctrl::ExecutorExit> {
        self.join.join()
    }

    #[must_use]
    pub fn metrics(&self) -> &Arc<RtThreadMetrics> {
        &self.metrics
    }
}

/// 启动 RT 线程。
///
/// - `clock`: 供 `sleep_until` / `now` 调用。生产 `Arc<SystemClock>`，测试 `Arc<MockClock>`。
/// - `adapter`: 已经构造（尚未 `connect()`）的 adapter 实例。本函数内只调
///   `adapter.connect()`，**不**调 `enable(true)` —— spawn-time 不再上扭
///   （task-30 v9 / M7 修订），由 `ipc_thread` 走 state_machine `Safe → Ready
///   → Enabled.Active` 两阶段 transition 通过 `client.request_normal(Enable
///   (true))` 排进 `ctrl`，rt_thread 主循环 Step 2 消费时调 `adapter.enable(true)`。
/// - `cmd_reader`: 由 `ipc_thread`（Part 2）或测试喂命令的 reader 端。
/// - `snapshot_pub`: 由 `pub_thread`（Part 2）或测试消费的 writer 端。
/// - `ctrl`: `HardRtCtrlInner` 共享 atomic 协议 slot（与 `HardRtCtrlClient`
///   对端配对）。spawn 不改写安全事实；首个 tick 完成真实安全停止后，
///   才可能提交 `TorqueOffConfirmed`，运动还须原 authority 的 Motion 租约。
///
/// 返回 `RtThreadHandle`，可 `request_stop` + `join`。
///
/// # Errors
///
/// 返回 `std::io::Error` 当 `strict_rt = true` 且任一 `set_sched_fifo` / `mlockall` /
/// `pin_to_cpu` / `adapter.connect` 失败。非 strict 模式下三个 syscall 失败被记录到
/// stderr 后继续。
#[allow(clippy::needless_pass_by_value)]
pub fn spawn_rt_thread<A>(
    cfg: RtThreadConfig,
    clock: Arc<dyn RobotClock>,
    mut adapter: A,
    cmd_reader: LatestCommandReader<JointCommandBuf>,
    snapshot_pub: RtSnapshotPublisher<RtSnapshot>,
    ctrl: Arc<HardRtCtrlInner>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<RtThreadHandle>
where
    // task-30 v9 finding #3：bound 改成 `HardRtAdapter`（综合 ManipulatorAdapter
    // + RtSafeCtrl），**编译期**拒绝未实现 RtSafeCtrl 的 adapter 进 RT 路径。
    A: HardRtAdapter + Send + 'static,
{
    // 在**主线程**完成可能分配的准备（thread name 字符串、Arc<AtomicBool> 等）。
    // RT scope 只在 spawn 之后 + 每 tick rt_entry! 内才算进入。
    #[cfg(feature = "test-comm-fault-hook")]
    crate::comm_fault_hook::before_spawn("executor")?;
    let metrics = Arc::new(RtThreadMetrics::default());

    // **task-30 v9 / M7 修订**：spawn-time 只 connect，**不**调 enable(true)。
    //
    // 旧 M5 实现"启动即上扭"（adapter.enable(true) + safety_state=TorqueOn）
    // 绕过 state_machine 的 Safe → Ready → Enabled 两阶段 transition。
    // M7 接通 dispatch 后，由 ipc_thread 通过 `client.request_normal(Enable(true))`
    // 走 ctrl 路径，rt_thread 主循环 Step 2 consume normal action 时调
    // `adapter.enable(true)` 真上扭。这样 HardRT / SerialRT 两路径**统一**
    // 由 state_machine 驱动 enable，避免 daemon 启动那一刻的"特权窗口"。
    //
    // connect 的返回不构成断扭证明；首个 tick 必须真实 safety_stop_rt，
    // 再提交该调用之前取得的同 authority 观测。
    // 初始 policy = `Disabled`（ctrl 默认值）→ Step 3 do_write 白名单不含
    // Disabled，所以不会调 write_command。
    adapter
        .connect()
        .map_err(|e| std::io::Error::other(format!("adapter connect failed: {e}")))?;
    // connect 不证明已断扭；首个真实 RT tick 做安全停止并提交其原观测。
    ctrl.emergency_action
        .store(emergency_codes::SAFETY_STOP, Ordering::Release);

    let join_stop = Arc::clone(&stop);
    let join_metrics = Arc::clone(&metrics);
    let join_ctrl = Arc::clone(&ctrl);

    let join = thread::Builder::new()
        .name("s0_rt_thread".into())
        .spawn(move || {
            run_rt_loop(
                cfg,
                clock,
                adapter,
                cmd_reader,
                snapshot_pub,
                &join_stop,
                &join_metrics,
                &join_ctrl,
            )
        })?;

    Ok(RtThreadHandle {
        join,
        stop,
        metrics,
    })
}

/// 实际 RT 循环体。从 `spawn_rt_thread` 调用，不对外 pub。
///
/// 入参 `Arc<dyn RobotClock>` / `LatestCommandReader` / `RtSnapshotPublisher`
/// 按值传入是为了明确**所有权被 RT 线程持有**（`move` 进 spawn 闭包），即使
/// 函数体内只通过方法借用；clippy `needless_pass_by_value` 在此 allow。
#[allow(
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::too_many_arguments
)]
fn run_rt_loop<A: HardRtAdapter>(
    cfg: RtThreadConfig,
    clock: Arc<dyn RobotClock>,
    mut adapter: A,
    cmd_reader: LatestCommandReader<JointCommandBuf>,
    mut snapshot_pub: RtSnapshotPublisher<RtSnapshot>,
    stop: &Arc<AtomicBool>,
    metrics: &Arc<RtThreadMetrics>,
    ctrl: &Arc<HardRtCtrlInner>,
) -> crate::hard_rt_ctrl::ExecutorExit {
    // ---- 初始化阶段（尚未进 rt_entry!，允许 alloc）----
    apply_rt_prio(&cfg);
    apply_mlockall(&cfg);
    apply_cpu_pin(&cfg);

    let mut state_buf = JointStateBuf::default();
    let mut tick_seq: u64 = 0;
    let mut active_enable: Option<EnableFinishPending> = None;
    let mut command = cmd_reader.initial_value();
    let mut deadline = Deadline(clock.now());

    // 固定值槽不含reader TLS；在调度循环前初始化分配守卫的线程局部状态。
    rt_entry!({});
    // 初始快照保留原发布/耗尽合同，不承担命令槽预热职责。
    // S48 A3b：首次发布同样是失败出口——版本耗尽即不进入主循环、结束 worker。
    #[cfg(feature = "test-comm-fault-hook")]
    let warmup_attempt_started_ns = clock.now().0;
    let warmup = snapshot_pub.publish(&RtSnapshot {
        state: state_buf,
        tick_seq: 0,
        tick_deadline_ns: 0,
    });
    #[cfg(feature = "test-comm-fault-hook")]
    if warmup == Err(robot_transport::SnapshotPublishError::VersionExhausted) {
        metrics
            .snapshot_publish_failure_attempt_ns
            .store(warmup_attempt_started_ns, Ordering::Relaxed);
    }
    if warmup.is_err() {
        metrics
            .snapshot_publish_errors
            .fetch_add(1, Ordering::Relaxed);
        return crate::hard_rt_ctrl::ExecutorExit::from_pending(active_enable);
    }

    // ---- 主循环（task-30 v9 / M5 改造） ----
    while crate::worker_stop::worker_running(
        crate::worker_stop::WorkerKind::Executor,
        clock.as_ref(),
        stop,
    ) {
        // ─── A2 freeze hook（test-only，0 cost when freeze_until_ns == 0）───
        //
        // 放在 rt_entry! **外**：spin 是 alloc-free / syscall-free，但放外面避
        // 免污染 RT-zone alloc-check 心智模型。freeze 期间整 tick 被 hold（含
        // deadline 计算 + sleep_until + I/O），tick_count 不增 →
        // LivenessObserver 看到停滞 → watchdog 不喂 sd_notify(WATCHDOG=1)。
        #[cfg(any(test, feature = "test-freeze-hook"))]
        crate::freeze_hook::freeze_check(&metrics.freeze_until_ns, clock.as_ref(), stop);

        let tick_period = DurationNs(cfg.tick_period_ns);
        deadline = deadline + tick_period;

        // 检查是否 overrun：deadline 是否已经落后 now
        if deadline.0 < clock.now() {
            metrics.overruns.fetch_add(1, Ordering::Relaxed);
        }

        // S48 A3b：rt_entry 块返回本 tick 的发布结果；错误在 guard 退出之后
        // 处理（计数 + 结束循环），RT scope 内不 format、不 panic、不 drop Arc。
        let published = rt_entry!({
            clock.sleep_until(deadline);

            // ─── 1) emergency drain ────────────────────────────────────────
            //
            // **优先**消费 emergency_action：任何 in-flight pending 都被 cancel。
            // safety_stop_rt 是 RT-safe（HardRT mmap 写寄存器即时生效），<1µs。
            //
            // client 已在原 authority 同步收紧；原 packed 请求留给本执行器
            // 精确消费并发布拒绝，不能由 client 丢弃原 seq 冒作真实执行结束。
            // **finding #1 修订**：safety_stop() 返 Err **不能**伪装成
            // `TorqueOffConfirmed`。区分：
            //   Ok  → `TorqueOffConfirmed`（HardRT mmap 即时生效；M7 + 真硬件
            //                              adapter 可改成 `StopCommanded` +
            //                              `OutputSafetyProbe` 异步 confirm）
            //   Err → `StopFailed`（M7 ipc_tick 升 `Fault(SafetyStopFailed)`）
            // 两个入口分别消费：IPC 的单调 EStop 只发布 A2 责任，不能要求它再写旧槽。
            // 即使本次 claim 成功，仍须交换 emergency 槽，避免同一请求在下 tick 重投。
            let claimed_estop = ctrl.claim_serial_estop();
            let em = ctrl
                .emergency_action
                .swap(emergency_codes::NONE, Ordering::AcqRel);
            if claimed_estop || em == emergency_codes::SAFETY_STOP {
                let observation = ctrl
                    .authority()
                    .begin_safety_observation(SafetyOperation::ConfirmOff);
                let result = adapter.safety_stop_rt();
                if let Some(active) = &mut active_enable {
                    // 这次真实 Stop 已在该 Enable adapter 返回之后执行，不能由 finish 重投。
                    active.record_hard_rt_stop_returned();
                }
                ctrl.policy
                    .store(OutputPolicy::StopTorque.to_u8(), Ordering::Release);
                match result {
                    Ok(()) => {
                        let _ = ctrl.authority().complete_safety_observation(observation);
                    }
                    Err(_) => ctrl.tighten_safety(OutputSafetyState::StopFailed),
                }
                metrics
                    .emergency_safety_stop_count
                    .fetch_add(1, Ordering::Relaxed);

                // 本执行器取走尚未开始的原请求后才发布该 seq 的终结。
                let pending = ctrl.pending_normal.swap(0, Ordering::AcqRel);
                if pending != 0 {
                    let (action, _arg, seq) = unpack_pending_normal(pending);
                    if action != normal_codes::NONE {
                        ctrl.last_completed.store(
                            pack_completed(seq, AdapterResp::Err(AdapterErr::PreemptedByEmergency)),
                            Ordering::Release,
                        );
                        metrics
                            .emergency_preempted_count
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }

            // 已开始的 Enable 先办真实 finish；拒绝时不取新 normal 或覆盖完成槽。
            if let Some(active) = active_enable.take() {
                active_enable = finish_hard_rt_enable(active, ctrl);
                if active_enable.is_none() {
                    metrics
                        .normal_completed_count
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            if active_enable.is_none() {
                let raw = ctrl.pending_normal.swap(0, Ordering::AcqRel);
                if let Ok(Some(pending)) = ctrl.decode_pending(raw) {
                    active_enable = execute_owned_normal(&mut adapter, pending, ctrl);
                    if active_enable.is_none() {
                        metrics
                            .normal_completed_count
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }

            // ─── 3) policy gate + 普通 read/write ──────────────────────────
            //
            // **不变量**：policy 决定 hot-path 是否 write_command。read_state
            // 始终调（observability 不关）。Mock adapter `write_command` 内部
            // 在 enabled=false 时本身是 noop；所以即使我们走 write_command 也
            // 安全。**显式 gate** 是为 M7 + SerialRT 一致语义。
            let policy = OutputPolicy::from_u8(ctrl.policy.load(Ordering::Acquire));
            let do_write = matches!(
                policy,
                OutputPolicy::ActiveForward
                    | OutputPolicy::HoldCurrent
                    | OutputPolicy::ZeroCommand
                    | OutputPolicy::ReadyHold
            );
            if do_write {
                if let Some(latest) = cmd_reader.try_load() {
                    command = latest;
                }
                let cmd = command.as_view();
                if with_normal_lease(ctrl, NormalIoIntent::Motion, || {
                    write_full_bus(&mut adapter, &cmd)
                })
                .is_err()
                {
                    metrics.write_errors.fetch_add(1, Ordering::Relaxed);
                }
            }

            // **采样时刻的事务式提交**（v0.5 候选 E 段(7) round 1 P0）：
            //
            // `JointStateBuf.stamp_ns` 的填写责任在**调用方**——adapter 不持
            // 时钟（`robot_core::adapter::ManipulatorAdapter::read_state` 契约
            // + feetech `adapter.rs` 同址注释）。此前本处**完全漏填**，S0 发布
            // 的 `JointState` body 采样时刻恒 0（mock 路径），真 S1 锚定 /
            // 段(5) state cache 一律判 stale（"三真同堂"六进程 e2e 首次暴露）。
            //
            // **失败路径不得刷新时间戳**：读失败时若仍打新戳，会把持续串口故障
            // 伪装成新鲜数据，消费方永远看不到 stale。故读进**临时副本**，仅
            // 成功时连同新戳整体提交；失败时保留 last-known-good 的状态**与
            // 旧戳**（即便某 adapter 在失败前部分写坏了 out，也污染不到正式
            // 快照）。时钟读复用既有 `RobotClock` 热路径，无分配 / 无锁等待 /
            // 无阻塞 IO。
            let sample_started_ns = clock.now().0;
            let mut next_state = state_buf;
            match with_normal_lease(ctrl, NormalIoIntent::PeriodicRead, || {
                adapter.read_state(&mut next_state)
            }) {
                Ok(()) => {
                    next_state.stamp_ns = sample_started_ns;
                    state_buf = next_state;
                }
                Err(_) => {
                    metrics.read_errors.fetch_add(1, Ordering::Relaxed);
                }
            }

            tick_seq = tick_seq.wrapping_add(1);
            let snap = RtSnapshot {
                state: state_buf,
                tick_seq,
                tick_deadline_ns: deadline.0.0,
            };
            #[cfg(feature = "test-comm-fault-hook")]
            let attempt_started_ns = clock.now().0;
            let published = snapshot_pub.publish(&snap);
            #[cfg(feature = "test-comm-fault-hook")]
            if published == Err(robot_transport::SnapshotPublishError::VersionExhausted) {
                metrics
                    .snapshot_publish_failure_attempt_ns
                    .store(attempt_started_ns, Ordering::Relaxed);
            }
            if published.is_ok() {
                metrics.tick_count.fetch_add(1, Ordering::Relaxed);
            }
            published
        });
        if published.is_err() {
            metrics
                .snapshot_publish_errors
                .fetch_add(1, Ordering::Relaxed);
            break;
        }
    }

    // 正常 stop 或快照失败均携原执行责任退出；不能通过析构伪造 finish 或原 seq 响应。
    // 设备停止仍由关闭路径的真实 Lower/Stop 证明，join 本身不证明已断扭。
    crate::hard_rt_ctrl::ExecutorExit::from_pending(active_enable)
}

/// 只消费执行器实际取到的 packed 请求；启动、调用、finish 走同一实现。
/// 返回值是尚未能完成的原 Execution，调用者必须保留并只重试 finish。
pub(crate) fn execute_owned_normal<A: HardRtAdapter>(
    adapter: &mut A,
    pending: crate::hard_rt_ctrl::SerialPending,
    ctrl: &HardRtCtrlInner,
) -> Option<EnableFinishPending> {
    let record = pending.action;
    let mut execution = None;
    let start_error = if pending.cancelled {
        Some(AdapterErr::PreemptedByEmergency)
    } else if record.action == normal_codes::ENABLE && record.arg == 1 {
        match start_enable_execution(ctrl) {
            Ok(token) => {
                let installed = ctrl.execution_slot().install(token.view());
                execution = Some(token);
                installed.err().map(|_| AdapterErr::AuthorityRevoked)
            }
            Err(error) => Some(error),
        }
    } else {
        None
    };
    let mut adapter_called = false;
    let response = match start_error {
        Some(error) => AdapterResp::Err(error),
        None => execute_normal(
            adapter,
            record.action,
            record.arg,
            ctrl,
            &mut adapter_called,
        ),
    };
    if execution.is_some() {
        ctrl.execution_slot().clear();
    }
    match execution {
        Some(execution) => {
            return finish_hard_rt_enable(
                EnableFinishPending::new(
                    execution,
                    record,
                    match response {
                        AdapterResp::Ok => EnableResult::Succeeded,
                        AdapterResp::Err(_) => EnableResult::Failed,
                    },
                    response,
                    adapter_called,
                ),
                ctrl,
            );
        }
        None => crate::serial_convergence::publish_completion(ctrl, record, response),
    }
    None
}

/// 真实 finish 只执行一次；补偿使用原紧急槽，不另造 seq 或 EStop。
fn finish_hard_rt_enable(
    active: EnableFinishPending,
    ctrl: &HardRtCtrlInner,
) -> Option<EnableFinishPending> {
    match finish_enable_owned(active, ctrl) {
        Ok(finished) => {
            finished.publish_after_compensation(ctrl, |_, compensation| {
                if compensation == EnableCompensation::Required {
                    ctrl.emergency_action
                        .store(emergency_codes::SAFETY_STOP, Ordering::Release);
                }
            });
            None
        }
        Err(active) => Some(active),
    }
}

/// rt_thread 内部：把 (action, arg) 翻译成 adapter 调用，返回 AdapterResp。
/// **只在已通过 latch check 之后调用**。
///
/// **finding #3 修复**：bound 是 [`HardRtAdapter`]（综合
/// `ManipulatorAdapter + RtSafeCtrl`），编译期保证 `*_rt` 三方法可用。
/// 这里调 `enable_rt` / `set_mode_rt` 而不是 `enable` / `set_mode`，明确走
/// RT-safe 路径。各 adapter 的 `assert_no_alloc` 单测验证 RT 合规。
#[inline]
fn execute_normal<A: HardRtAdapter>(
    adapter: &mut A,
    action: u8,
    arg: u8,
    ctrl: &HardRtCtrlInner,
    adapter_called: &mut bool,
) -> AdapterResp {
    let intent = match (action, arg) {
        (normal_codes::ENABLE, 1) => match ctrl.execution_slot().load() {
            Some(view) => NormalIoIntent::EnableStep(view),
            None => return AdapterResp::Err(AdapterErr::AuthorityRevoked),
        },
        (normal_codes::ENABLE, 0) => {
            NormalIoIntent::NonEnergizingControl(NonEnergizingControl::Disable)
        }
        (normal_codes::SET_MODE, _) => NormalIoIntent::Motion,
        _ => return AdapterResp::Err(AdapterErr::Unsupported),
    };
    let observation = match (action, arg) {
        (normal_codes::ENABLE, 0) => Some(
            ctrl.authority()
                .begin_safety_observation(SafetyOperation::Disable),
        ),
        _ => None,
    };
    let result = with_normal_lease(ctrl, intent, || match action {
        normal_codes::ENABLE => {
            *adapter_called = true;
            adapter.enable_rt(arg != 0)
        }
        normal_codes::SET_MODE => {
            let mode = match arg {
                0 => ControlMode::Stop,
                1 => ControlMode::Position,
                2 => ControlMode::Velocity,
                3 => ControlMode::Torque,
                4 => ControlMode::ServoPosition,
                _ => return Err(RobotError::Unsupported("不支持的控制模式")),
            };
            adapter.set_mode_rt(mode)
        }
        _ => Err(RobotError::Unsupported("HardRT 不支持该动作")),
    });
    match result {
        Ok(()) => {
            if let Some(observation) = observation {
                let _ = ctrl.authority().complete_safety_observation(observation);
            }
            AdapterResp::Ok
        }
        Err(RobotError::Aborted(AbortReason::Revoked)) => {
            AdapterResp::Err(AdapterErr::AuthorityRevoked)
        }
        Err(RobotError::Aborted(AbortReason::Emergency)) => {
            AdapterResp::Err(AdapterErr::PreemptedByEmergency)
        }
        Err(RobotError::Aborted(AbortReason::Timeout)) => {
            AdapterResp::Err(AdapterErr::AbortedByTimeout)
        }
        Err(RobotError::Unsupported(_) | RobotError::InvalidParameter(_)) => {
            AdapterResp::Err(AdapterErr::Unsupported)
        }
        Err(_) => AdapterResp::Err(AdapterErr::HardwareFault),
    }
}

/// 调用前取得真实 token；返回后只释放原 token，拒绝不清位、不重复调用 adapter。
fn with_normal_lease<T>(
    ctrl: &HardRtCtrlInner,
    intent: NormalIoIntent,
    call: impl FnOnce() -> Result<T, RobotError>,
) -> Result<T, RobotError> {
    #[cfg(feature = "test-comm-fault-hook")]
    let acquired = {
        let observed = ctrl.authority().acquire_normal_io_observed(intent);
        ctrl.trace_now(
            crate::comm_fault_trace::TraceKind::NormalAcquire,
            observed.failed_cas.unwrap_or(u64::MAX),
            u64::from(observed.outcome.is_ok()),
        );
        observed.outcome
    };
    #[cfg(not(feature = "test-comm-fault-hook"))]
    let acquired = ctrl.authority().acquire_normal_io(intent);
    let lease = acquired.map_err(|reason| {
        RobotError::Aborted(match reason {
            robot_core::control_authority::Refusal::SerialEStopPending => AbortReason::Emergency,
            _ => AbortReason::Revoked,
        })
    })?;
    let result = call();
    #[cfg(feature = "test-comm-fault-hook")]
    {
        let observed = ctrl.authority().release_normal_io_observed(lease);
        ctrl.trace_now(
            crate::comm_fault_trace::TraceKind::NormalRelease,
            observed.failed_cas.unwrap_or(u64::MAX),
            match observed.outcome {
                Ok(_) => 1,
                Err(_) => 3,
            },
        );
    }
    #[cfg(not(feature = "test-comm-fault-hook"))]
    let _ = ctrl.authority().release_normal_io(lease);
    result
}

// **RT 边界说明**（§0 / §4.9 RT 铁律）：以下三个 `apply_*` helper 在 RT
// thread **spawn 后、进入 tick 主循环之前**的 startup setup 阶段执行，
// 非 hot path。RT 铁律的 "禁 alloc / 禁 tracing" 针对 tick loop 每帧；
// startup setup 阶段允许一次性 alloc / log（fmt 字段、subscriber dispatch
// 都只跑一次）。tick loop 内仍**严禁**任何 `tracing::*!` / `eprintln!` /
// 带 format arg 的日志。

fn apply_rt_prio(cfg: &RtThreadConfig) {
    if let Err(e) = rt_thread_os::set_sched_fifo(cfg.sched_fifo_priority) {
        assert!(
            !cfg.strict_rt,
            "set_sched_fifo(prio={}) failed in strict mode: {e}",
            cfg.sched_fifo_priority
        );
        tracing::warn!(
            sched_fifo_priority = cfg.sched_fifo_priority,
            ?e,
            "set_sched_fifo failed — continuing without RT priority"
        );
    }
}

fn apply_mlockall(cfg: &RtThreadConfig) {
    if let Err(e) = rt_thread_os::mlockall() {
        assert!(!cfg.strict_rt, "mlockall failed in strict mode: {e}");
        tracing::warn!(?e, "mlockall failed — continuing without memlock");
    }
}

fn apply_cpu_pin(cfg: &RtThreadConfig) {
    let Some(cpu) = cfg.cpu_affinity else { return };
    if let Err(e) = rt_thread_os::pin_to_cpu(cpu) {
        assert!(
            !cfg.strict_rt,
            "pin_to_cpu({cpu}) failed in strict mode: {e}"
        );
        tracing::warn!(cpu, ?e, "pin_to_cpu failed — continuing unpinned");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hard_rt_rejects_partial_selection_before_adapter_write() {
        use robot_core::adapter::ManipulatorAdapter;
        let mut adapter = MockHardRTAdapter::with_unlimited_limits(8);
        adapter.connect().unwrap();
        adapter.enable(true).unwrap();
        let mut command = JointCommandBuf {
            mode: ControlModeRepr::Position,
            joint_count: 8,
            positions: [1.0; robot_core::adapter::MAX_JOINTS],
            stamp_ns: 100,
            deadline_ns: 200,
            ..JointCommandBuf::default()
        };
        write_full_bus(&mut adapter, &command.as_view()).unwrap();
        command.selection = JointSelection::Mask(0xc0);
        command.positions.fill(2.0);
        assert_eq!(
            write_full_bus(&mut adapter, &command.as_view()),
            Err(RobotError::Unsupported("HardRT暂不支持部分关节命令"))
        );
        let mut state = JointStateBuf::default();
        adapter.read_state(&mut state).unwrap();
        assert_eq!(&state.positions[..8], &[1.0; 8]);
        command.selection = JointSelection::MixedPositionRawVelocity {
            position_mask: 0x3f,
            velocity_mask: 0x1c0,
        };
        command.joint_count = 9;
        command.raw_velocities[7] = 500;
        assert_eq!(
            write_full_bus(&mut adapter, &command.as_view()),
            Err(RobotError::Unsupported("HardRT暂不支持部分关节命令"))
        );
        adapter.read_state(&mut state).unwrap();
        assert_eq!(&state.positions[..8], &[1.0; 8]);
    }
    use robot_adapter_mock::MockHardRTAdapter;
    use robot_core::adapter::{ControlModeRepr, JointCommandBuf, MAX_JOINTS};
    use robot_core::executor_ctrl::ExecutorCtrlClient;
    use robot_core::time::{MockClock, MonoRawNs};
    use robot_transport::LatestCommandSlot;
    use std::time::Duration;

    /// 失败路径仍持真实句柄：先 stop，再有界等真正结束，最后 join。
    /// adapter 若违约阻塞，终止测试进程，不 detach 活 worker 后继续其它测试。
    struct TestRtHandle(Option<RtThreadHandle>);
    impl TestRtHandle {
        fn is_finished(&self) -> bool {
            self.0.as_ref().unwrap().is_finished()
        }
        fn request_stop(&self) {
            self.0.as_ref().unwrap().request_stop();
        }
        fn metrics(&self) -> &Arc<RtThreadMetrics> {
            self.0.as_ref().unwrap().metrics()
        }
        fn wait_finished(&self) {
            self.request_stop();
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !self.is_finished() {
                if std::time::Instant::now() >= deadline {
                    std::process::abort();
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        fn join(mut self) -> thread::Result<()> {
            self.wait_finished();
            let exit = self.0.take().unwrap().join()?;
            if exit.has_pending_enable() {
                // 旧健康用例必须真实为空；失败时把原件保到进程退出。
                std::process::abort();
            }
            Ok(())
        }
    }
    impl Drop for TestRtHandle {
        fn drop(&mut self) {
            if self.0.is_some() {
                self.wait_finished();
                if let Ok(exit) = self.0.take().unwrap().join()
                    && exit.has_pending_enable()
                {
                    // 测试失败收尾不能先析构原执行凭证后继续运行。
                    std::process::abort();
                }
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn spawn_rt_thread<A: HardRtAdapter + Send + 'static>(
        cfg: RtThreadConfig,
        clock: Arc<dyn RobotClock>,
        adapter: A,
        commands: LatestCommandReader<JointCommandBuf>,
        publisher: RtSnapshotPublisher<RtSnapshot>,
        ctrl: Arc<HardRtCtrlInner>,
        stop: Arc<AtomicBool>,
    ) -> std::io::Result<TestRtHandle> {
        super::spawn_rt_thread(cfg, clock, adapter, commands, publisher, ctrl, stop)
            .map(|handle| TestRtHandle(Some(handle)))
    }

    // **task-30 v9 / M7 配套**：spawn_rt_thread 不再 spawn-time enable。
    // 需要 enabled adapter 的测试通过 client 排 `Enable(true)` 让 rt_thread
    // 主循环 Step 2 消费；见 `rt_thread_after_emergency_skips_write_command`
    // / `rt_thread_consumes_normal_action` 内联用法。

    /// 启动 `rt_thread` + 跑若干 tick（用 `MockClock` 自我推进），验证 metrics 增长。
    #[test]
    fn spawn_smoke_with_mock_clock() {
        let clock: Arc<dyn RobotClock> = Arc::new(MockClock::new(MonoRawNs::ZERO));
        let adapter = MockHardRTAdapter::with_unlimited_limits(3);
        let (cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, _snap_rdr) = RtSnapshotPublisher::new(RtSnapshot::default());

        // 注入一条命令
        let mut cmd = JointCommandBuf {
            joint_count: 3,
            mode: ControlModeRepr::Position,
            stamp_ns: 1,
            deadline_ns: 1_000_000_000,
            ..JointCommandBuf::default()
        };
        cmd.positions[0] = 1.23;
        cmd.positions[1] = 2.34;
        cmd.positions[2] = 3.45;
        cmd_writer.store(cmd).unwrap();

        let cfg = RtThreadConfig {
            tick_period_ns: 1_000, // 1 µs
            strict_rt: false,
            ..RtThreadConfig::default()
        };

        let ctrl = Arc::new(HardRtCtrlInner::new(crate::hard_rt_ctrl::test_authority(
            false,
        )));
        // task-30 v9 / M5：默认 OutputPolicy=Disabled 时 rt_thread 不调
        // write_command。测试要求 mock adapter echo cmd → 必须先把 policy
        // 切到 ActiveForward。M7 由 state_machine 切。
        ctrl.policy
            .store(OutputPolicy::ActiveForward.to_u8(), Ordering::Release);
        let handle = spawn_rt_thread(
            cfg,
            clock,
            adapter,
            cmd_reader,
            snap_pub,
            ctrl,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        let metrics = Arc::clone(handle.metrics());

        // 等至少 1 tick（确定性轮询替代固定 50ms sleep）
        assert!(crate::test_util::wait_until(
            Duration::from_millis(500),
            || metrics.tick_count.load(Ordering::Relaxed) > 0
        ));
        handle.request_stop();
        handle.join().unwrap();
    }

    // ------------------------------------------------------------------
    // round 1 P0 回归锁（v0.5 候选 E 段(7)）：读失败不得刷新采样时间戳
    // ------------------------------------------------------------------

    /// `read_fail` 模式下写入 `positions[0]` 的哨兵值。
    const READ_FAIL_POISON: f32 = 999.0;

    /// 两份快照的 positions 是否逐位相同（clippy 禁 f32 数组整体 `==`）。
    fn positions_identical(
        a: &robot_core::adapter::JointStateBuf,
        b: &robot_core::adapter::JointStateBuf,
    ) -> bool {
        a.positions
            .iter()
            .zip(b.positions.iter())
            .all(|(x, y)| x.to_bits() == y.to_bits())
    }

    /// 失败前**部分写坏** `out` 的 adapter（trait 未禁止该行为，故调用方
    /// 必须整体丢弃这次读）。
    struct FlakyReadAdapter {
        inner: MockHardRTAdapter,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    impl robot_core::adapter::ManipulatorAdapter for FlakyReadAdapter {
        fn info(&self) -> robot_core::adapter::AdapterInfo {
            robot_core::adapter::AdapterInfo {
                name: "flaky_read",
                kind: robot_core::adapter::AdapterKind::Manipulator,
                rt_class: robot_core::adapter::RtClass::HardRT,
                vendor_sdk: false,
            }
        }
        fn connect(&mut self) -> Result<(), robot_core::error::RobotError> {
            self.inner.connect()
        }
        fn disconnect(&mut self) -> Result<(), robot_core::error::RobotError> {
            self.inner.disconnect()
        }
        fn read_state(
            &mut self,
            out: &mut robot_core::adapter::JointStateBuf,
        ) -> Result<(), robot_core::error::RobotError> {
            if self.fail.load(Ordering::Acquire) {
                out.positions[0] = READ_FAIL_POISON;
                out.stamp_ns = 0;
                return Err(robot_core::error::RobotError::HardwareFault(
                    "simulated read failure",
                ));
            }
            self.inner.read_state(out)
        }
        fn write_command(
            &mut self,
            cmd: &robot_core::adapter::JointCommandView,
        ) -> Result<(), robot_core::error::RobotError> {
            self.inner.write_command(cmd)
        }
        fn set_mode(
            &mut self,
            mode: robot_core::adapter::ControlMode,
        ) -> Result<(), robot_core::error::RobotError> {
            self.inner.set_mode(mode)
        }
        fn enable(&mut self, on: bool) -> Result<(), robot_core::error::RobotError> {
            self.inner.enable(on)
        }
        fn safety_stop(&mut self) -> Result<(), robot_core::error::RobotError> {
            self.inner.safety_stop()
        }
        fn limits(&self) -> &robot_core::adapter::JointLimits {
            self.inner.limits()
        }
    }

    impl robot_core::adapter::RtSafeCtrl for FlakyReadAdapter {
        fn safety_stop_rt(&mut self) -> Result<(), robot_core::error::RobotError> {
            self.inner.safety_stop_rt()
        }
        fn enable_rt(&mut self, on: bool) -> Result<(), robot_core::error::RobotError> {
            self.inner.enable_rt(on)
        }
        fn set_mode_rt(
            &mut self,
            mode: robot_core::adapter::ControlMode,
        ) -> Result<(), robot_core::error::RobotError> {
            self.inner.set_mode_rt(mode)
        }
    }

    /// `read_state` 失败时**不得刷新采样时间戳**，也不得把失败前的部分写入
    /// 并进正式快照。
    ///
    /// 序列：① 正常读若干 tick（stamp 推进）→ ② adapter 转失败模式**并故意
    /// 部分改写 out** → 断言快照**冻结**在最后一次成功值、`read_errors`
    /// 增长 → ③ 恢复成功 → 断言 stamp 再次推进。
    #[test]
    fn read_failure_freezes_state_and_stamp() {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let adapter = FlakyReadAdapter {
            inner: MockHardRTAdapter::with_unlimited_limits(3),
            fail: Arc::clone(&fail),
        };
        // SystemClock + 100 µs tick（沿 FailingSafetyAdapter 同款：MockClock
        // 在多线程下时序不稳）。
        let clock: Arc<dyn RobotClock> = Arc::new(robot_core::time::SystemClock::new());
        let (_cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, snap_rdr) = RtSnapshotPublisher::new(RtSnapshot::default());
        let cfg = RtThreadConfig {
            tick_period_ns: 100_000,
            strict_rt: false,
            ..RtThreadConfig::default()
        };
        let ctrl = Arc::new(HardRtCtrlInner::new(crate::hard_rt_ctrl::test_authority(
            false,
        )));
        let handle = spawn_rt_thread(
            cfg,
            clock,
            adapter,
            cmd_reader,
            snap_pub,
            ctrl,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        let metrics = Arc::clone(handle.metrics());

        // ① 正常读：等 stamp 真正推进。
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            snap_rdr.load().state.stamp_ns > 0 && metrics.tick_count.load(Ordering::Relaxed) > 2
        }));
        let errors_before = metrics.read_errors.load(Ordering::Relaxed);

        // ② 失败模式：等错误累积后取两次快照，断言**完全冻结**。
        fail.store(true, Ordering::Release);
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            metrics.read_errors.load(Ordering::Relaxed) >= errors_before + 3
        }));
        let frozen_a = snap_rdr.load().state;
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            metrics.read_errors.load(Ordering::Relaxed) >= errors_before + 20
        }));
        let frozen_b = snap_rdr.load().state;
        assert_eq!(
            frozen_a.stamp_ns, frozen_b.stamp_ns,
            "读失败期间采样时间戳必须冻结（否则持续故障会被伪装成新鲜数据）"
        );
        assert!(
            positions_identical(&frozen_a, &frozen_b),
            "读失败期间状态必须冻结在 last-known-good"
        );
        assert!(
            (frozen_b.positions[0] - READ_FAIL_POISON).abs() > f32::EPSILON,
            "失败前的部分写入不得进入正式快照：positions[0] = {}",
            frozen_b.positions[0]
        );
        assert!(frozen_b.stamp_ns > 0, "冻结的是**旧戳**，不是 0");

        // ③ 恢复：stamp 必须再次推进。
        fail.store(false, Ordering::Release);
        assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
            snap_rdr.load().state.stamp_ns > frozen_b.stamp_ns
        }));

        handle.request_stop();
        handle.join().unwrap();
    }

    /// S48 A3b：快照版本已耗尽 → warmup（首次）发布失败 → worker 不进入主循环即
    /// 结束；错误计数可观察，reader 保留构造时的快照与版本，不回绕。
    #[cfg(feature = "test-comm-fault-hook")]
    #[test]
    fn snapshot_version_exhaustion_on_first_publish_ends_the_worker() {
        use robot_transport::MAX_PUBLISHED_VERSION;
        let clock: Arc<dyn RobotClock> = Arc::new(robot_core::time::SystemClock::new());
        let adapter = MockHardRTAdapter::with_unlimited_limits(2);
        let (_cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, snap_rdr) =
            RtSnapshotPublisher::new_with_version(RtSnapshot::default(), MAX_PUBLISHED_VERSION)
                .unwrap();
        let cfg = RtThreadConfig {
            tick_period_ns: 100_000,
            strict_rt: false,
            ..RtThreadConfig::default()
        };
        let handle = spawn_rt_thread(
            cfg,
            clock,
            adapter,
            cmd_reader,
            snap_pub,
            Arc::new(HardRtCtrlInner::new(crate::hard_rt_ctrl::test_authority(
                false,
            ))),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        assert!(crate::test_util::wait_until(Duration::from_secs(3), || {
            handle.is_finished()
        }));
        let metrics = Arc::clone(handle.metrics());
        handle.join().unwrap();
        assert_eq!(metrics.snapshot_publish_errors.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.tick_count.load(Ordering::Relaxed), 0);
        assert_eq!(snap_rdr.version(), MAX_PUBLISHED_VERSION, "不回绕");
        assert_eq!(snap_rdr.load().tick_seq, 0, "保留构造时的快照");
    }

    /// S48 A3b：warmup 成功后第一个 tick 的发布耗尽 → 该 tick 不计数，错误在 guard
    /// 退出后处理并结束循环；reader 保留 warmup 快照（tick_seq = 0）。
    #[cfg(feature = "test-comm-fault-hook")]
    #[test]
    fn snapshot_version_exhaustion_inside_the_loop_ends_the_worker_after_the_guard() {
        use robot_transport::MAX_PUBLISHED_VERSION;
        let clock: Arc<dyn RobotClock> = Arc::new(robot_core::time::SystemClock::new());
        let adapter = MockHardRTAdapter::with_unlimited_limits(2);
        let (_cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, snap_rdr) =
            RtSnapshotPublisher::new_with_version(RtSnapshot::default(), MAX_PUBLISHED_VERSION - 2)
                .unwrap();
        let cfg = RtThreadConfig {
            tick_period_ns: 100_000,
            strict_rt: false,
            ..RtThreadConfig::default()
        };
        let handle = spawn_rt_thread(
            cfg,
            clock,
            adapter,
            cmd_reader,
            snap_pub,
            Arc::new(HardRtCtrlInner::new(crate::hard_rt_ctrl::test_authority(
                false,
            ))),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        assert!(crate::test_util::wait_until(Duration::from_secs(3), || {
            handle.is_finished()
        }));
        let metrics = Arc::clone(handle.metrics());
        handle.join().unwrap();
        assert_eq!(metrics.snapshot_publish_errors.load(Ordering::Relaxed), 1);
        assert_eq!(
            metrics.tick_count.load(Ordering::Relaxed),
            0,
            "发布失败的 tick 不计成功"
        );
        assert_eq!(snap_rdr.version(), MAX_PUBLISHED_VERSION);
        assert_eq!(snap_rdr.load().tick_seq, 0, "保留 warmup 快照");
    }

    /// 构造即 destroy（不跑 tick）—— 验证 handle 生命周期 + adapter connect 成功。
    #[test]
    fn spawn_then_immediately_stop() {
        let clock: Arc<dyn RobotClock> = Arc::new(MockClock::new(MonoRawNs::ZERO));
        let adapter = MockHardRTAdapter::with_unlimited_limits(1);
        let (_cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, _snap_rdr) = RtSnapshotPublisher::new(RtSnapshot::default());

        let cfg = RtThreadConfig {
            tick_period_ns: 1_000_000, // 1 ms
            ..RtThreadConfig::default()
        };
        let ctrl = Arc::new(HardRtCtrlInner::new(crate::hard_rt_ctrl::test_authority(
            false,
        )));
        // task-30 v9 / M5：默认 OutputPolicy=Disabled 时 rt_thread 不调
        // write_command。测试要求 mock adapter echo cmd → 必须先把 policy
        // 切到 ActiveForward。M7 由 state_machine 切。
        ctrl.policy
            .store(OutputPolicy::ActiveForward.to_u8(), Ordering::Release);
        let handle = spawn_rt_thread(
            cfg,
            clock,
            adapter,
            cmd_reader,
            snap_pub,
            ctrl,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        handle.request_stop();
        handle.join().unwrap();
    }

    /// 压力：50 ms 跑一把，验证 snapshot 随 tick 增长；`MAX_JOINTS` 保证不越界。
    #[test]
    fn snapshot_tick_seq_grows() {
        let clock: Arc<dyn RobotClock> = Arc::new(robot_core::time::SystemClock::new());
        let adapter = MockHardRTAdapter::with_unlimited_limits(4);
        let (_cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, snap_rdr) = RtSnapshotPublisher::new(RtSnapshot::default());

        let cfg = RtThreadConfig {
            tick_period_ns: 100_000, // 100 µs
            ..RtThreadConfig::default()
        };
        let ctrl = Arc::new(HardRtCtrlInner::new(crate::hard_rt_ctrl::test_authority(
            false,
        )));
        // task-30 v9 / M5：默认 OutputPolicy=Disabled 时 rt_thread 不调
        // write_command。测试要求 mock adapter echo cmd → 必须先把 policy
        // 切到 ActiveForward。M7 由 state_machine 切。
        ctrl.policy
            .store(OutputPolicy::ActiveForward.to_u8(), Ordering::Release);
        let handle = spawn_rt_thread(
            cfg,
            clock,
            adapter,
            cmd_reader,
            snap_pub,
            ctrl,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        let metrics = Arc::clone(handle.metrics());
        // 等 tick_seq 超 0（确定性轮询替代 sleep 30ms）
        assert!(
            crate::test_util::wait_until(Duration::from_millis(500), || {
                metrics.tick_count.load(Ordering::Relaxed) > 0
            }),
            "rt_thread 500ms 内未产出 tick"
        );
        handle.request_stop();
        handle.join().unwrap();

        let snap = snap_rdr.load();
        assert!(
            snap.tick_seq > 0,
            "rt_thread should have produced at least one tick"
        );
        // joint_count 跟着 adapter 走（默认 0，因为 mock 没执行 write_command 输出位置 —— 没 cmd）
        assert!(snap.state.joint_count == 4 || snap.state.joint_count == 0);
        let _ = MAX_JOINTS; // 只是引用一下保证 import 不 unused
    }

    // ============================================================================
    // task-30 v9 / M5：rt_thread 主循环新行为集成测试
    // ============================================================================

    /// 工具：准备一个 `(rt_thread, ctrl, snap_rdr)`，OutputPolicy=ActiveForward。
    fn setup(
        joint_count: u8,
    ) -> (
        TestRtHandle,
        Arc<HardRtCtrlInner>,
        robot_transport::RtSnapshotReader<RtSnapshot>,
    ) {
        // **task-30 v9 / M5**：用 SystemClock + 100µs tick。
        // MockClock + thread::sleep 在 workspace 并行 cargo test 下时序不稳定
        // （rt_thread 在 MockClock 上轮询过快导致 main thread sleep 拿不到 CPU）。
        let clock: Arc<dyn RobotClock> = Arc::new(robot_core::time::SystemClock::new());
        let adapter = MockHardRTAdapter::with_unlimited_limits(joint_count);
        let (_cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, snap_rdr) = RtSnapshotPublisher::new(RtSnapshot::default());

        let cfg = RtThreadConfig {
            tick_period_ns: 100_000, // 100 µs = 10 kHz
            ..RtThreadConfig::default()
        };
        let ctrl = Arc::new(HardRtCtrlInner::new(crate::hard_rt_ctrl::test_authority(
            false,
        )));
        ctrl.policy
            .store(OutputPolicy::ActiveForward.to_u8(), Ordering::Release);
        let handle = spawn_rt_thread(
            cfg,
            clock,
            adapter,
            cmd_reader,
            snap_pub,
            Arc::clone(&ctrl),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        (handle, ctrl, snap_rdr)
    }

    /// emergency drain：`ctrl.safety_stop()` → 下个 tick 后 `safety_latched=true`、
    /// `safety_state=TorqueOffConfirmed`，`emergency_safety_stop_count >= 1`。
    #[test]
    fn rt_thread_consumes_emergency_action() {
        use crate::test_util::wait_until;
        let (handle, ctrl, _) = setup(2);
        let metrics = Arc::clone(handle.metrics());
        // 等 rt 至少跑 1 tick 起步（确定性轮询替代固定 sleep）
        assert!(
            wait_until(Duration::from_millis(500), || {
                metrics.tick_count.load(Ordering::Relaxed) > 0
            }),
            "rt_thread 500ms 内未跑出第一 tick"
        );
        let stops_before = metrics.emergency_safety_stop_count.load(Ordering::Relaxed);
        crate::hard_rt_ctrl::HardRtCtrlClient::new(ctrl.clone()).safety_stop();
        // 等 emergency 被消费 + safety_state 升 TorqueOffConfirmed
        assert!(
            wait_until(Duration::from_millis(500), || {
                metrics.emergency_safety_stop_count.load(Ordering::Relaxed) > stops_before
                    && ctrl.safety_state().to_u8() == OutputSafetyState::TorqueOffConfirmed.to_u8()
            }),
            "emergency 500ms 内未升 TorqueOffConfirmed"
        );
        handle.request_stop();
        handle.join().unwrap();

        assert!(ctrl.safety_latched(), "should latch");
        assert_eq!(
            OutputSafetyState::from_u8(ctrl.safety_state().to_u8()),
            OutputSafetyState::TorqueOffConfirmed
        );
        assert!(metrics.emergency_safety_stop_count.load(Ordering::Relaxed) >= 1);
    }

    /// normal action：client 端 `request_normal(Enable(true))` → 下 tick 后
    /// `last_completed = (seq=1, Ok)`，`normal_completed_count >= 1`。
    #[test]
    fn rt_thread_consumes_normal_action() {
        use crate::hard_rt_ctrl::HardRtCtrlClient;
        use robot_core::executor_ctrl::{AdapterNormalReq, ExecutorCtrlClient, NormalReqStatus};

        use crate::test_util::wait_until;
        let (handle, ctrl, _) = setup(1);
        let client = HardRtCtrlClient::new(Arc::clone(&ctrl));
        let metrics = Arc::clone(handle.metrics());

        // 等 rt warmup（至少 1 tick）
        assert!(wait_until(Duration::from_millis(500), || {
            metrics.tick_count.load(Ordering::Relaxed) > 0
        }));

        // 同一真实 owner 派生公告，裸请求不能取得执行权限。
        let (owner, view) = ctrl.authority().reserve_enable().unwrap();
        ctrl.start_slot().post(view).unwrap();
        let Ok(seq) = (match client.request_normal(AdapterNormalReq::Enable(true)) {
            NormalReqStatus::Submitted { seq } => Ok(seq),
            other => Err(other),
        }) else {
            panic!("expected Submitted")
        };

        // 等 rt_thread 消费 → poll 拿到 Ok
        assert!(
            wait_until(Duration::from_millis(500), || {
                client.poll_last_normal_resp(seq)
                    == Some(robot_core::executor_ctrl::AdapterResp::Ok)
            }),
            "Enable(true) 500ms 内未被 rt 消费"
        );
        ctrl.authority().commit_enable(owner).unwrap();
        handle.request_stop();
        handle.join().unwrap();

        assert!(metrics.normal_completed_count.load(Ordering::Relaxed) >= 1);
    }

    /// 无视图启动拒绝：emergency 之后**直接**注入 Enable(true) packed slot
    /// （绕过 client 端检查模拟 race），rt_thread 应在 A2 start 写
    /// `Err(AuthorityRevoked)` + 真实完成计数。
    #[test]
    fn rt_thread_secondary_latch_check_rejects_enable_true() {
        use crate::test_util::wait_until;
        use robot_core::executor_ctrl::{AdapterErr, AdapterResp, pack_pending_normal};

        let (handle, ctrl, snap_rdr) = setup(1);
        let metrics = Arc::clone(handle.metrics());

        assert!(wait_until(Duration::from_millis(500), || {
            metrics.tick_count.load(Ordering::Relaxed) > 0
        }));
        let stops_before = metrics.emergency_safety_stop_count.load(Ordering::Relaxed);
        crate::hard_rt_ctrl::HardRtCtrlClient::new(ctrl.clone()).safety_stop();
        assert!(
            wait_until(Duration::from_millis(500), || {
                ctrl.safety_latched()
                    && metrics.emergency_safety_stop_count.load(Ordering::Relaxed) > stops_before
                    && ctrl.emergency_action.load(Ordering::Acquire) == emergency_codes::NONE
                    && !ctrl.authority().snapshot().serial_estop_pending
            }),
            "emergency 500ms 内未 latch"
        );

        // 两个急停入口均已消费后，再等 tick 尾发布，避免把计数更新误作 pending 已排空。
        let stopped_tick = snap_rdr.load().tick_seq;
        assert!(
            wait_until(Duration::from_millis(500), || {
                snap_rdr.load().tick_seq > stopped_tick
            }),
            "emergency 500ms 内未完成 tick 尾发布"
        );

        // 2. **绕过 client 端 A2 start**直接注入 Enable(true) slot
        //    （模拟 client 端检查后 latch 才被置位的 race 窗口）。
        //    seq 用一个新值（避免与已有 last_completed 冲突）。
        let raw_seq = 99u64;
        let packed = pack_pending_normal(normal_codes::ENABLE, 1, raw_seq);
        ctrl.pending_normal.store(packed, Ordering::Release);

        // 3. 等 rt_thread 处理 → 写 last_completed = (raw_seq, Err(AuthorityRevoked))
        assert!(
            wait_until(Duration::from_millis(500), || {
                let last = ctrl.last_completed.load(Ordering::Acquire);
                let (last_seq, _) = robot_core::executor_ctrl::unpack_completed(last);
                last_seq == raw_seq
            }),
            "secondary A2 start 500ms 内未触发"
        );
        handle.request_stop();
        handle.join().unwrap();

        // 4. last_completed 应是 (seq=99, Err(AuthorityRevoked))
        let last = ctrl.last_completed.load(Ordering::Acquire);
        let (last_seq, resp) = robot_core::executor_ctrl::unpack_completed(last);
        assert_eq!(last_seq, raw_seq);
        assert_eq!(resp, AdapterResp::Err(AdapterErr::AuthorityRevoked));
        assert!(metrics.normal_completed_count.load(Ordering::Relaxed) >= 1);
    }

    /// emergency drain pending：rt tick 头部消费 emergency 时，**同 tick 内**
    /// in-flight pending 被 drain → 写 `last_completed = Err(PreemptedByEmergency)`。
    #[test]
    fn rt_thread_emergency_preempts_in_flight_pending() {
        use crate::test_util::wait_until;
        use robot_core::executor_ctrl::{AdapterErr, AdapterResp, pack_pending_normal};

        let ctrl = Arc::new(HardRtCtrlInner::new(crate::hard_rt_ctrl::test_authority(
            false,
        )));
        ctrl.pending_normal.store(
            pack_pending_normal(normal_codes::ENABLE, 1, 42),
            Ordering::Release,
        );
        crate::hard_rt_ctrl::HardRtCtrlClient::new(ctrl.clone()).safety_stop();
        let (_commands, reader) = LatestCommandSlot::new(JointCommandBuf::default());
        let (publisher, _snapshot) = RtSnapshotPublisher::new(RtSnapshot::default());
        let handle = spawn_rt_thread(
            RtThreadConfig {
                tick_period_ns: 100_000,
                ..RtThreadConfig::default()
            },
            Arc::new(robot_core::time::SystemClock::new()),
            MockHardRTAdapter::with_unlimited_limits(1),
            reader,
            publisher,
            ctrl.clone(),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let metrics = Arc::clone(handle.metrics());
        // 请求与真实收紧均在 spawn 前完成，第一次真实安全停止必取走原请求。

        // 等 rt 写 last_completed (seq=42, PreemptedByEmergency)
        assert!(
            wait_until(Duration::from_millis(500), || {
                let last = ctrl.last_completed.load(Ordering::Acquire);
                let (last_seq, _) = robot_core::executor_ctrl::unpack_completed(last);
                last_seq == 42
            }),
            "emergency drain pending 500ms 内未触发"
        );
        handle.request_stop();
        handle.join().unwrap();

        let last = ctrl.last_completed.load(Ordering::Acquire);
        let (last_seq, resp) = robot_core::executor_ctrl::unpack_completed(last);
        assert_eq!(last_seq, 42);
        assert_eq!(resp, AdapterResp::Err(AdapterErr::PreemptedByEmergency));
        assert!(metrics.emergency_preempted_count.load(Ordering::Relaxed) >= 1);
    }

    /// **finding #1 lock**：emergency 后 rt_thread 必须**不再调 write_command**。
    /// 通过 `client.safety_stop()` → 喂一条带新 position 的 cmd → 等若干 tick →
    /// 读 mock adapter 的 state；如果 adapter 仍 write_command，state.positions
    /// 会被 echo 成 cmd 的值；如果**正确不写**，state.positions 留旧值。
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "同一真实 worker 先授权写入再急停，比较原快照并完成真实 join"
    )]
    fn rt_thread_after_emergency_skips_write_command() {
        use crate::hard_rt_ctrl::HardRtCtrlClient;
        use robot_core::adapter::ControlModeRepr;
        use robot_core::executor_ctrl::ExecutorCtrlClient;

        let clock: Arc<dyn RobotClock> = Arc::new(robot_core::time::SystemClock::new());
        let adapter = MockHardRTAdapter::with_unlimited_limits(2);
        let (cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, snap_rdr) = RtSnapshotPublisher::new(RtSnapshot::default());

        let cfg = RtThreadConfig {
            tick_period_ns: 100_000,
            ..RtThreadConfig::default()
        };
        let ctrl = Arc::new(HardRtCtrlInner::new(crate::hard_rt_ctrl::test_authority(
            false,
        )));
        let client = HardRtCtrlClient::new(Arc::clone(&ctrl));

        let handle = spawn_rt_thread(
            cfg,
            clock,
            adapter,
            cmd_reader,
            snap_pub,
            Arc::clone(&ctrl),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();

        assert!(crate::test_util::wait_until(
            Duration::from_millis(500),
            || { handle.metrics().tick_count.load(Ordering::Relaxed) > 0 }
        ));
        let (owner, view) = ctrl.authority().reserve_enable().unwrap();
        ctrl.start_slot().post(view).unwrap();
        // 原 owner 保留到真实执行完成；只有 commit 成功才允许 Motion。
        let status =
            client.request_normal(robot_core::executor_ctrl::AdapterNormalReq::Enable(true));
        let robot_core::executor_ctrl::NormalReqStatus::Submitted { seq: enable_seq } = status
        else {
            panic!("Enable(true) should be Submitted, got {status:?}")
        };
        assert!(
            crate::test_util::wait_until(Duration::from_millis(500), || client
                .poll_last_normal_resp(enable_seq)
                == Some(robot_core::executor_ctrl::AdapterResp::Ok)),
            "Enable(true) 500ms 内未被 rt 消费"
        );
        ctrl.authority().commit_enable(owner).unwrap();
        client.set_policy(OutputPolicy::ActiveForward);

        // 1) 正常路径：写一条 cmd → rt_thread 应 echo 到 state
        let mut cmd = JointCommandBuf {
            joint_count: 2,
            mode: ControlModeRepr::Position,
            stamp_ns: 1,
            deadline_ns: 1_000_000_000,
            ..JointCommandBuf::default()
        };
        cmd.positions[0] = 1.0;
        cmd.positions[1] = 2.0;
        cmd_writer.store(cmd).unwrap();
        // 等 mock echo（snap_rdr 反映 cmd.positions[0]=1.0）
        assert!(
            crate::test_util::wait_until(Duration::from_millis(500), || {
                let s = snap_rdr.load();
                (s.state.positions[0] - 1.0).abs() < 1e-3
            }),
            "ActiveForward 500ms 内 mock 未 echo cmd[0]=1.0"
        );

        // 2) emergency，不能把启动期已经完成的 Stop 计为这次收尾。
        let stops_before = handle
            .metrics()
            .emergency_safety_stop_count
            .load(Ordering::Relaxed);
        client.safety_stop();

        // 3) 等 emergency 被 rt 消费（state.positions[0] 被清零）
        let metrics = Arc::clone(handle.metrics());
        assert!(
            crate::test_util::wait_until(Duration::from_millis(500), || {
                metrics.emergency_safety_stop_count.load(Ordering::Relaxed) > stops_before
            }),
            "emergency 500ms 内未被 rt 消费"
        );

        // 4) 立即 store 新 cmd（5.0 / 6.0）；emergency 后**不应**被 echo 到 state
        let mut cmd2 = JointCommandBuf {
            joint_count: 2,
            mode: ControlModeRepr::Position,
            stamp_ns: 2,
            deadline_ns: 2_000_000_000,
            ..JointCommandBuf::default()
        };
        cmd2.positions[0] = 5.0;
        cmd2.positions[1] = 6.0;
        cmd_writer.store(cmd2).unwrap();
        // 让 rt 跑几 tick 验证 cmd2 不被 echo
        let baseline_tick = metrics.tick_count.load(Ordering::Relaxed);
        assert!(crate::test_util::wait_until(
            Duration::from_millis(500),
            || metrics.tick_count.load(Ordering::Relaxed) >= baseline_tick + 5
        ));

        // 4) emergency drain 之后 mock.safety_stop_rt() 把 state.positions 清零
        //    （mock 的 safety_stop_rt 实现是 [0; MAX_JOINTS]）。新 cmd 没被
        //    write_command 应用 → state.positions 仍是 0，**不是** 5.0 / 6.0。
        handle.request_stop();
        handle.join().unwrap();
        let snap_after = snap_rdr.load();
        assert!(
            snap_after.state.positions[0].abs() < 1e-3,
            "emergency 后 rt_thread 不该再调 write_command；state.positions[0] \
             应被 mock.safety_stop_rt 清 0，实际 {}",
            snap_after.state.positions[0]
        );
        assert!(
            (snap_after.state.positions[0] - 5.0).abs() > 1.0,
            "emergency 后 cmd[0]=5.0 不该被 echo 到 state；实际 state[0]={}",
            snap_after.state.positions[0]
        );
        // policy 现在是 StopTorque（client + rt 都写过）
        assert_eq!(
            OutputPolicy::from_u8(ctrl.policy.load(Ordering::Acquire)),
            OutputPolicy::StopTorque
        );
    }

    /// **finding #1 修复 lock 测试**：safety_stop_rt 返 Err → `safety_state`
    /// 必须是 `StopFailed`，**不是** `TorqueOffConfirmed`。
    /// 用一个 mock wrapper adapter 强制 `safety_stop_rt` 返 Err 验证。
    #[test]
    fn rt_thread_safety_stop_err_sets_stop_failed() {
        use robot_core::adapter::{
            AdapterInfo, AdapterKind, ControlMode, JointCommandView, JointLimits, JointStateBuf,
            ManipulatorAdapter, RtClass, RtSafeCtrl,
        };
        use robot_core::error::RobotError;

        /// adapter wrapper：`safety_stop` / `safety_stop_rt` 强制返 Err；其它
        /// 全部转发到 inner mock。
        struct FailingSafetyAdapter(MockHardRTAdapter);
        impl ManipulatorAdapter for FailingSafetyAdapter {
            fn info(&self) -> AdapterInfo {
                AdapterInfo {
                    name: "failing_safety",
                    kind: AdapterKind::Manipulator,
                    rt_class: RtClass::HardRT,
                    vendor_sdk: false,
                }
            }
            fn connect(&mut self) -> Result<(), RobotError> {
                self.0.connect()
            }
            fn disconnect(&mut self) -> Result<(), RobotError> {
                self.0.disconnect()
            }
            fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError> {
                self.0.read_state(out)
            }
            fn write_command(&mut self, cmd: &JointCommandView) -> Result<(), RobotError> {
                self.0.write_command(cmd)
            }
            fn set_mode(&mut self, mode: ControlMode) -> Result<(), RobotError> {
                self.0.set_mode(mode)
            }
            fn enable(&mut self, on: bool) -> Result<(), RobotError> {
                self.0.enable(on)
            }
            fn safety_stop(&mut self) -> Result<(), RobotError> {
                Err(RobotError::HardwareFault("simulated safety_stop fail"))
            }
            fn limits(&self) -> &JointLimits {
                self.0.limits()
            }
        }
        impl RtSafeCtrl for FailingSafetyAdapter {
            fn safety_stop_rt(&mut self) -> Result<(), RobotError> {
                Err(RobotError::HardwareFault("simulated safety_stop_rt fail"))
            }
            fn enable_rt(&mut self, on: bool) -> Result<(), RobotError> {
                self.0.enable_rt(on)
            }
            fn set_mode_rt(&mut self, mode: ControlMode) -> Result<(), RobotError> {
                self.0.set_mode_rt(mode)
            }
        }

        // 用 SystemClock + 100µs tick：避免 MockClock 在多线程下时序不稳定
        let clock: Arc<dyn RobotClock> = Arc::new(robot_core::time::SystemClock::new());
        let adapter = FailingSafetyAdapter(MockHardRTAdapter::with_unlimited_limits(2));
        let (_cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, _) = RtSnapshotPublisher::new(RtSnapshot::default());
        let cfg = RtThreadConfig {
            tick_period_ns: 100_000, // 100 µs = 10 kHz tick
            ..RtThreadConfig::default()
        };
        let ctrl = Arc::new(HardRtCtrlInner::new(crate::hard_rt_ctrl::test_authority(
            false,
        )));
        ctrl.policy
            .store(OutputPolicy::ActiveForward.to_u8(), Ordering::Release);
        let handle = spawn_rt_thread(
            cfg,
            clock,
            adapter,
            cmd_reader,
            snap_pub,
            Arc::clone(&ctrl),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        let metrics = Arc::clone(handle.metrics());

        // 等 rt warmup
        assert!(crate::test_util::wait_until(
            Duration::from_millis(500),
            || metrics.tick_count.load(Ordering::Relaxed) > 0
        ));
        crate::hard_rt_ctrl::HardRtCtrlClient::new(ctrl.clone()).safety_stop();
        // 等 emergency 被消费 + safety_state 升 StopFailed
        assert!(
            crate::test_util::wait_until(Duration::from_millis(500), || {
                ctrl.safety_state().to_u8() == OutputSafetyState::StopFailed.to_u8()
            }),
            "safety_stop_rt Err 后 500ms 内未升 StopFailed"
        );
        handle.request_stop();
        handle.join().unwrap();

        // **关键断言**：safety_state = StopFailed，**不是** TorqueOffConfirmed
        assert_eq!(
            OutputSafetyState::from_u8(ctrl.safety_state().to_u8()),
            OutputSafetyState::StopFailed,
            "safety_stop_rt Err 必须升 StopFailed，不能伪装 TorqueOffConfirmed"
        );
        // safety_latched 仍要 latch（防后续 Enable(true)）
        assert!(ctrl.safety_latched());
    }

    /// 同步收紧立即可见，但原请求和序号保持到真实执行器消费后才终结。
    #[test]
    fn client_safety_stop_synchronously_latches_and_retains_pending() {
        use crate::hard_rt_ctrl::HardRtCtrlClient;
        use robot_core::executor_ctrl::{AdapterNormalReq, ExecutorCtrlClient, NormalReqStatus};

        // 不 spawn rt_thread；直接验 client 端 atomic 行为。
        let inner = Arc::new(HardRtCtrlInner::new(crate::hard_rt_ctrl::test_authority(
            false,
        )));
        let client = HardRtCtrlClient::new(Arc::clone(&inner));

        // 1) 先排一个 normal pending（Enable(false) 不被 latch 拒）
        let seq = match client.request_normal(AdapterNormalReq::Enable(false)) {
            NormalReqStatus::Submitted { seq } => seq,
            other => panic!("expected Submitted, got {other:?}"),
        };
        assert_ne!(inner.pending_normal.load(Ordering::Acquire), 0);

        // 2) 调 client.safety_stop()
        client.safety_stop();

        // 3) **同步**断言（**不**等 rt tick）：
        //    - safety_latched=true
        //    - pending 原样保留，尚无 last_completed
        //    - 原 seq 的责任未因同步收紧而退休
        //    - emergency_action=SAFETY_STOP
        assert!(inner.safety_latched());
        assert_ne!(inner.pending_normal.load(Ordering::Acquire), 0);
        assert_eq!(
            client.poll_last_normal_resp(seq),
            None,
            "原序号保留到真实执行器处理"
        );
        assert_eq!(
            inner.emergency_action.load(Ordering::Acquire),
            emergency_codes::SAFETY_STOP
        );

        // 4) 后续 client 立即拒 Enable(true)（latch 同步可见）
        match client.request_normal(AdapterNormalReq::Enable(true)) {
            NormalReqStatus::Unsupported(_) => {}
            other => panic!("Enable(true) should be Unsupported after safety_stop, got {other:?}"),
        }
    }

    /// OutputPolicy=Disabled 时 rt_thread 跳过 write_command（adapter 不会被
    /// 调到 write_command；通过 metrics.write_errors == 0 + read_state 仍跑 验证）。
    #[test]
    fn rt_thread_policy_disabled_skips_write() {
        // 自己 setup，policy 留默认 Disabled。SystemClock 防 workspace 并行 race。
        let clock: Arc<dyn RobotClock> = Arc::new(robot_core::time::SystemClock::new());
        let adapter = MockHardRTAdapter::with_unlimited_limits(2);
        let (_cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, snap_rdr) = RtSnapshotPublisher::new(RtSnapshot::default());
        let cfg = RtThreadConfig {
            tick_period_ns: 100_000,
            ..RtThreadConfig::default()
        };
        let ctrl = Arc::new(HardRtCtrlInner::new(crate::hard_rt_ctrl::test_authority(
            false,
        )));
        // **policy=Disabled (default)** —— 不切到 ActiveForward。
        let handle = spawn_rt_thread(
            cfg,
            clock,
            adapter,
            cmd_reader,
            snap_pub,
            ctrl,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        let metrics = Arc::clone(handle.metrics());
        // 等至少 1 tick
        assert!(crate::test_util::wait_until(
            Duration::from_millis(500),
            || metrics.tick_count.load(Ordering::Relaxed) > 0
        ));
        handle.request_stop();
        handle.join().unwrap();

        // tick 应已跑（read_state 总在跑），但 write_errors 应为 0（mock
        // write_command 在 Disabled 时根本没被调）。这里没法直接验"未调"；
        // 但能验 tick_count > 0 + write_errors == 0 间接说明。
        assert!(metrics.tick_count.load(Ordering::Relaxed) > 0);
        assert_eq!(metrics.write_errors.load(Ordering::Relaxed), 0);
        // snapshot 应有 joint_count 来自 read_state
        let snap = snap_rdr.load();
        assert!(snap.tick_seq > 0);
    }

    /// **A2 freeze hook 单测**：设 `freeze_until_ns` 后，rt_thread 的
    /// `tick_count` 在 spin 期间不增长；过期后恢复。**SystemClock + 非
    /// SCHED_FIFO**：reviewer 拍板硬要求 rt_thread 验证走单测 / 非
    /// SCHED_FIFO 集成，避免 Orin 上 SCHED_FIFO busy-spin 让 RT 线程饥饿。
    /// 单测里 `apply_rt_prio` 没 CAP_SYS_NICE 会 fallback 到 SCHED_OTHER。
    #[test]
    fn freeze_hook_holds_tick_count() {
        use robot_core::time::SystemClock;
        let sys_clock = SystemClock::new();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let adapter = MockHardRTAdapter::with_unlimited_limits(2);
        let (_cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, _snap_rdr) = RtSnapshotPublisher::new(RtSnapshot::default());
        let cfg = RtThreadConfig {
            tick_period_ns: 1_000_000, // 1ms（单测节奏）
            strict_rt: false,
            ..RtThreadConfig::default()
        };
        let ctrl = Arc::new(HardRtCtrlInner::new(crate::hard_rt_ctrl::test_authority(
            false,
        )));
        ctrl.policy
            .store(OutputPolicy::ActiveForward.to_u8(), Ordering::Release);
        let handle = spawn_rt_thread(
            cfg,
            clock,
            adapter,
            cmd_reader,
            snap_pub,
            ctrl,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        let metrics = Arc::clone(handle.metrics());

        // 等 worker 跑起来
        assert!(crate::test_util::wait_until(
            Duration::from_secs(2),
            || metrics.tick_count.load(Ordering::Relaxed) >= 5
        ));
        let pre_freeze = metrics.tick_count.load(Ordering::Relaxed);

        // 设 freeze 80ms
        let until = sys_clock.now().0 + 80_000_000;
        metrics.freeze_until_ns.store(until, Ordering::Release);

        // freeze 中段
        std::thread::sleep(Duration::from_millis(40));
        let mid_freeze = metrics.tick_count.load(Ordering::Relaxed);
        let mid_grew = mid_freeze.saturating_sub(pre_freeze);
        assert!(
            mid_grew <= 1,
            "freeze 期间 tick_count 不应继续增长：pre={pre_freeze} mid={mid_freeze} 增 {mid_grew}",
        );

        // freeze 过期后恢复
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
