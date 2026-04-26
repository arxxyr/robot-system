//! S0 RT 线程：`SCHED_FIFO` + `mlockall` + tick 闭环。
//!
//! 对应 `docs/00_design.md` §1.2 `s0_rt_thread` + §1.3 启动流程 + §4.9 RT 铁律。
//! 对应 `docs/handoffs/2026-04-24.md` §2.3 方案 B（每 `HardRT` bus 一实例，v0.2
//! 单 bus 等价）。
//!
//! ### tick 闭环（每 tick 运行在 `rt_entry!` scope 内）
//!
//! ```text
//! clock.sleep_until(deadline)
//! deadline += tick_period
//! cmd = cmd_reader.load()                 // arc-swap Guard，零 refcount 触动
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
//! 4. `adapter.connect()` → `adapter.enable(true)`（Part 1 简化；Part 2 由 state machine 管）
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

use robot_core::adapter::{ControlMode, HardRtAdapter, JointCommandBuf, JointStateBuf, RtSnapshot};
use robot_core::executor_ctrl::{
    AdapterErr, AdapterResp, OutputPolicy, OutputSafetyState, emergency_codes, normal_codes,
    pack_completed, unpack_pending_normal,
};
use robot_core::rt_entry;
use robot_core::thread as rt_thread_os;
use robot_core::time::{Deadline, DurationNs, RobotClock};
use robot_transport::{LatestCommandReader, RtSnapshotPublisher};

use crate::hard_rt_ctrl::HardRtCtrlInner;

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
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    metrics: Arc<RtThreadMetrics>,
}

impl RtThreadHandle {
    /// 请求停止。下次 tick 开始时检测到并退出。
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// 等待 RT 线程退出。`request_stop` 后调用。
    ///
    /// # Errors
    ///
    /// 返回 `Err` 当 RT 线程 panic（`JoinHandle::join` 的 `Result`）。
    pub fn join(self) -> thread::Result<()> {
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
///   对端配对）。spawn 时 `safety_state` 被初始化为 `TorqueOffConfirmed`；
///   `policy` 留默认 `Disabled`（rt_thread 不 write_command 直到上层 set_policy）。
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
) -> std::io::Result<RtThreadHandle>
where
    // task-30 v9 finding #3：bound 改成 `HardRtAdapter`（综合 ManipulatorAdapter
    // + RtSafeCtrl），**编译期**拒绝未实现 RtSafeCtrl 的 adapter 进 RT 路径。
    A: HardRtAdapter + Send + 'static,
{
    // 在**主线程**完成可能分配的准备（thread name 字符串、Arc<AtomicBool> 等）。
    // RT scope 只在 spawn 之后 + 每 tick rt_entry! 内才算进入。
    let stop = Arc::new(AtomicBool::new(false));
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
    // 初始 safety_state = `TorqueOffConfirmed`（adapter 默认 torque off；
    // mock connect 后 enabled=false，feetech connect 不动 Torque_Enable）。
    // 初始 policy = `Disabled`（ctrl 默认值）→ Step 3 do_write 白名单不含
    // Disabled，所以不会调 write_command。
    adapter
        .connect()
        .map_err(|e| std::io::Error::other(format!("adapter connect failed: {e}")))?;
    ctrl.safety_state.store(
        OutputSafetyState::TorqueOffConfirmed.to_u8(),
        Ordering::Release,
    );

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
            );
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
    snapshot_pub: RtSnapshotPublisher<RtSnapshot>,
    stop: &Arc<AtomicBool>,
    metrics: &Arc<RtThreadMetrics>,
    ctrl: &Arc<HardRtCtrlInner>,
) {
    // ---- 初始化阶段（尚未进 rt_entry!，允许 alloc）----
    apply_rt_prio(&cfg);
    apply_mlockall(&cfg);
    apply_cpu_pin(&cfg);

    let mut state_buf = JointStateBuf::default();
    let mut tick_seq: u64 = 0;
    let mut deadline = Deadline(clock.now());

    // ---- arc-swap / seqlock thread_local warmup（RT-critical）----
    //
    // arc-swap 的 hazard-pointer slot 是 *per-thread thread_local*，**第一次** load
    // 在新线程上会 alloc ~128 bytes（hazard pointer 单 slot + retired list 初始 cap）。
    // 同理 RtSnapshotPublisher 内部 seqlock writer 在首次 publish 时初始化 thread_local
    // 计数。如果不在这里 warmup，第一个 rt_entry! 就 alloc → assert_no_alloc abort
    // （Step 7 task 26 `rt_guard_soak` 集成测试发现）。
    //
    // warmup 在 rt_entry! 之前 + 在主循环之前，alloc 允许。完成后 thread_local 全
    // 已初始化，主循环稳态 0 alloc。
    drop(cmd_reader.load());
    snapshot_pub.publish(&RtSnapshot {
        state: state_buf,
        tick_seq: 0,
        tick_deadline_ns: 0,
    });

    // ---- 主循环（task-30 v9 / M5 改造） ----
    while !stop.load(Ordering::Acquire) {
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

        rt_entry!({
            clock.sleep_until(deadline);

            // ─── 1) emergency drain ────────────────────────────────────────
            //
            // **优先**消费 emergency_action：任何 in-flight pending 都被 cancel。
            // safety_stop_rt 是 RT-safe（HardRT mmap 写寄存器即时生效），<1µs。
            //
            // **finding #4 修订**：client `safety_stop()` 已经在 ipc_thread 调用
            // 那一刻**同步**置 `safety_latched=true` 并 drain pending → 写
            // `last_completed = PreemptedByEmergency`。本 step 仍然要消费
            // `emergency_action`：把硬件 fail-safe 真正下发并把
            // `safety_state` 升到 `StopCommanded` / `TorqueOffConfirmed` /
            // `StopFailed`。
            //
            // **finding #1 修订**：safety_stop() 返 Err **不能**伪装成
            // `TorqueOffConfirmed`。区分：
            //   Ok  → `TorqueOffConfirmed`（HardRT mmap 即时生效；M7 + 真硬件
            //                              adapter 可改成 `StopCommanded` +
            //                              `OutputSafetyProbe` 异步 confirm）
            //   Err → `StopFailed`（M7 ipc_tick 升 `Fault(SafetyStopFailed)`）
            let em = ctrl
                .emergency_action
                .swap(emergency_codes::NONE, Ordering::AcqRel);
            if em == emergency_codes::SAFETY_STOP {
                // task-30 v9 / M5：HardRT adapter `safety_stop()` 必须 RT-safe
                // （见 `RtSafeCtrl` trait + 各 adapter 的 assert_no_alloc 单测）。
                // SerialRT adapter 不会进 rt_thread（s0_daemon RtClass 守门）。
                let result = adapter.safety_stop_rt();
                ctrl.safety_latched.store(true, Ordering::Release);
                // **finding #1 兜底**：把 policy 兜底切到 StopTorque。client
                // safety_stop() 已经先 store 过；这里再写一次是防"emergency
                // 由测试 / 其它路径直接置 emergency_action 没经 client"的场景。
                //
                // **task-30 v9 / SerialRT E2E #4 调用规范**：本 tick 在
                // emergency_action 被消费时**会**重新 store(latch, true)。这
                // 意味着 ipc_tick 调 `clear_safety_latch` 前必须等
                // `safety_state == TorqueOffConfirmed`（SerialRT 端 confirm 完
                // 成 = emergency_action 已 swap 完 = rt 端兜底 store latch 已
                // 落地）。否则 clear 之后会被 rt 端兜底重写。这是工程上合理
                // 的：操作员/RPC 调 ClearEStop 时本就该已经 confirm hardware
                // torque off。
                ctrl.policy
                    .store(OutputPolicy::StopTorque.to_u8(), Ordering::Release);
                let new_safety_state = match result {
                    Ok(()) => OutputSafetyState::TorqueOffConfirmed,
                    Err(_) => OutputSafetyState::StopFailed,
                };
                ctrl.safety_state
                    .store(new_safety_state.to_u8(), Ordering::Release);
                metrics
                    .emergency_safety_stop_count
                    .fetch_add(1, Ordering::Relaxed);

                // **drain pending（兜底）**：client 端 safety_stop() 已 drain，
                // 但若 rt_thread 看到 emergency_action 时 pending 仍非零（极小
                // race 窗口：client safety_stop 还没跑完 pending swap，或
                // emergency_action 是其它路径塞的），这里再兜一次。
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

            // ─── 2) 消费 normal action（含二次 latch check） ────────────────
            let pending = ctrl.pending_normal.swap(0, Ordering::AcqRel);
            if pending != 0 {
                let (action, arg, seq) = unpack_pending_normal(pending);
                if action != normal_codes::NONE {
                    // **二次 latch check**：client 端检查后到 rt_thread swap 之间
                    // 可能有 race 窗口（emergency 在两次之间触发 latch）。
                    let latched = ctrl.safety_latched.load(Ordering::Acquire);
                    let resp = if latched && action == normal_codes::ENABLE && arg == 1 {
                        metrics.latch_rejected_count.fetch_add(1, Ordering::Relaxed);
                        AdapterResp::Err(AdapterErr::SafetyLatched)
                    } else {
                        execute_normal(&mut adapter, action, arg, ctrl)
                    };
                    ctrl.last_completed
                        .store(pack_completed(seq, resp), Ordering::Release);
                    metrics
                        .normal_completed_count
                        .fetch_add(1, Ordering::Relaxed);
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
                let cmd_guard = cmd_reader.load();
                let cmd = cmd_guard.as_view();
                if adapter.write_command(&cmd).is_err() {
                    metrics.write_errors.fetch_add(1, Ordering::Relaxed);
                }
            }

            if adapter.read_state(&mut state_buf).is_err() {
                metrics.read_errors.fetch_add(1, Ordering::Relaxed);
            }

            tick_seq = tick_seq.wrapping_add(1);
            let snap = RtSnapshot {
                state: state_buf,
                tick_seq,
                tick_deadline_ns: deadline.0.0,
            };
            snapshot_pub.publish(&snap);

            metrics.tick_count.fetch_add(1, Ordering::Relaxed);
        });
    }

    // ---- 退出清理 ----
    // Part 1：这里不主动 disable / disconnect —— 由主线程（s0_daemon）在 join 后
    // 负责关停 adapter，保持 RT 线程职责纯净。
    // Part 2 可能在这里做 "safe output" 兜底。
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
) -> AdapterResp {
    match action {
        normal_codes::ENABLE => {
            let on = arg != 0;
            match adapter.enable_rt(on) {
                Ok(()) => {
                    ctrl.safety_state.store(
                        if on {
                            OutputSafetyState::TorqueOn.to_u8()
                        } else {
                            OutputSafetyState::TorqueOffConfirmed.to_u8()
                        },
                        Ordering::Release,
                    );
                    AdapterResp::Ok
                }
                Err(_) => AdapterResp::Err(AdapterErr::HardwareFault),
            }
        }
        normal_codes::SET_MODE => {
            let mode = match arg {
                0 => ControlMode::Stop,
                1 => ControlMode::Position,
                2 => ControlMode::Velocity,
                3 => ControlMode::Torque,
                4 => ControlMode::ServoPosition,
                _ => return AdapterResp::Err(AdapterErr::Unsupported),
            };
            match adapter.set_mode_rt(mode) {
                Ok(()) => AdapterResp::Ok,
                Err(_) => AdapterResp::Err(AdapterErr::Unsupported),
            }
        }
        // HardRT 路径不支持的复杂 action：Connect / Disconnect / HoldCurrent
        // 由 SerialRT executor 处理。HardRT 拿到这些 → Unsupported。
        normal_codes::CONNECT | normal_codes::DISCONNECT | normal_codes::HOLD_CURRENT => {
            AdapterResp::Err(AdapterErr::Unsupported)
        }
        _ => AdapterResp::Err(AdapterErr::Unsupported),
    }
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
    use robot_adapter_mock::MockHardRTAdapter;
    use robot_core::adapter::{ControlModeRepr, JointCommandBuf, MAX_JOINTS};
    use robot_core::time::{MockClock, MonoRawNs};
    use robot_transport::LatestCommandSlot;
    use std::time::Duration;

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
        cmd_writer.store(cmd);

        let cfg = RtThreadConfig {
            tick_period_ns: 1_000, // 1 µs
            strict_rt: false,
            ..RtThreadConfig::default()
        };

        let ctrl = Arc::new(HardRtCtrlInner::new());
        // task-30 v9 / M5：默认 OutputPolicy=Disabled 时 rt_thread 不调
        // write_command。测试要求 mock adapter echo cmd → 必须先把 policy
        // 切到 ActiveForward。M7 由 state_machine 切。
        ctrl.policy
            .store(OutputPolicy::ActiveForward.to_u8(), Ordering::Release);
        let handle = spawn_rt_thread(cfg, clock, adapter, cmd_reader, snap_pub, ctrl).unwrap();
        let metrics = Arc::clone(handle.metrics());

        // 等至少 1 tick（确定性轮询替代固定 50ms sleep）
        assert!(crate::test_util::wait_until(
            Duration::from_millis(500),
            || metrics.tick_count.load(Ordering::Relaxed) > 0
        ));
        handle.request_stop();
        handle.join().unwrap();
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
        let ctrl = Arc::new(HardRtCtrlInner::new());
        // task-30 v9 / M5：默认 OutputPolicy=Disabled 时 rt_thread 不调
        // write_command。测试要求 mock adapter echo cmd → 必须先把 policy
        // 切到 ActiveForward。M7 由 state_machine 切。
        ctrl.policy
            .store(OutputPolicy::ActiveForward.to_u8(), Ordering::Release);
        let handle = spawn_rt_thread(cfg, clock, adapter, cmd_reader, snap_pub, ctrl).unwrap();
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
        let ctrl = Arc::new(HardRtCtrlInner::new());
        // task-30 v9 / M5：默认 OutputPolicy=Disabled 时 rt_thread 不调
        // write_command。测试要求 mock adapter echo cmd → 必须先把 policy
        // 切到 ActiveForward。M7 由 state_machine 切。
        ctrl.policy
            .store(OutputPolicy::ActiveForward.to_u8(), Ordering::Release);
        let handle = spawn_rt_thread(cfg, clock, adapter, cmd_reader, snap_pub, ctrl).unwrap();
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
        RtThreadHandle,
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
        let ctrl = Arc::new(HardRtCtrlInner::new());
        ctrl.policy
            .store(OutputPolicy::ActiveForward.to_u8(), Ordering::Release);
        let handle =
            spawn_rt_thread(cfg, clock, adapter, cmd_reader, snap_pub, Arc::clone(&ctrl)).unwrap();
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
        // 触发 safety_stop
        ctrl.emergency_action
            .store(emergency_codes::SAFETY_STOP, Ordering::Release);
        // 等 emergency 被消费 + safety_state 升 TorqueOffConfirmed
        assert!(
            wait_until(Duration::from_millis(500), || {
                metrics.emergency_safety_stop_count.load(Ordering::Relaxed) >= 1
                    && ctrl.safety_state.load(Ordering::Acquire)
                        == OutputSafetyState::TorqueOffConfirmed.to_u8()
            }),
            "emergency 500ms 内未升 TorqueOffConfirmed"
        );
        handle.request_stop();
        handle.join().unwrap();

        assert!(ctrl.safety_latched.load(Ordering::Acquire), "should latch");
        assert_eq!(
            OutputSafetyState::from_u8(ctrl.safety_state.load(Ordering::Acquire)),
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

        // 提交 Enable(true)
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
        handle.request_stop();
        handle.join().unwrap();

        assert!(metrics.normal_completed_count.load(Ordering::Relaxed) >= 1);
    }

    /// 二次 latch check：emergency 之后**直接**注入 Enable(true) packed slot
    /// （绕过 client 端检查模拟 race），rt_thread 应在 latch check 写
    /// `Err(SafetyLatched)` + `latch_rejected_count >= 1`。
    #[test]
    fn rt_thread_secondary_latch_check_rejects_enable_true() {
        use crate::test_util::wait_until;
        use robot_core::executor_ctrl::{AdapterErr, AdapterResp, pack_pending_normal};

        let (handle, ctrl, _) = setup(1);
        let metrics = Arc::clone(handle.metrics());

        // 1. emergency 触发 + 等 rt 消费 → safety_latched=true
        ctrl.emergency_action
            .store(emergency_codes::SAFETY_STOP, Ordering::Release);
        assert!(
            wait_until(Duration::from_millis(500), || {
                ctrl.safety_latched.load(Ordering::Acquire)
            }),
            "emergency 500ms 内未 latch"
        );

        // 2. **绕过 client 端 latch check**直接注入 Enable(true) slot
        //    （模拟 client 端检查后 latch 才被置位的 race 窗口）。
        //    seq 用一个新值（避免与已有 last_completed 冲突）。
        let raw_seq = 99u64;
        let packed = pack_pending_normal(normal_codes::ENABLE, 1, raw_seq);
        ctrl.pending_normal.store(packed, Ordering::Release);

        // 3. 等 rt_thread 处理 → 写 last_completed = (raw_seq, Err(SafetyLatched))
        assert!(
            wait_until(Duration::from_millis(500), || {
                let last = ctrl.last_completed.load(Ordering::Acquire);
                let (last_seq, _) = robot_core::executor_ctrl::unpack_completed(last);
                last_seq == raw_seq
            }),
            "secondary latch check 500ms 内未触发"
        );
        handle.request_stop();
        handle.join().unwrap();

        // 4. last_completed 应是 (seq=99, Err(SafetyLatched))
        let last = ctrl.last_completed.load(Ordering::Acquire);
        let (last_seq, resp) = robot_core::executor_ctrl::unpack_completed(last);
        assert_eq!(last_seq, raw_seq);
        assert_eq!(resp, AdapterResp::Err(AdapterErr::SafetyLatched));
        assert!(metrics.latch_rejected_count.load(Ordering::Relaxed) >= 1);
    }

    /// emergency drain pending：rt tick 头部消费 emergency 时，**同 tick 内**
    /// in-flight pending 被 drain → 写 `last_completed = Err(PreemptedByEmergency)`。
    #[test]
    fn rt_thread_emergency_preempts_in_flight_pending() {
        use crate::test_util::wait_until;
        use robot_core::executor_ctrl::{AdapterErr, AdapterResp, pack_pending_normal};

        let (handle, ctrl, _) = setup(1);
        let metrics = Arc::clone(handle.metrics());

        // 同时设 emergency + pending（rt_thread 同 tick 头部消费 emergency 应
        // 把 pending 也 drain 掉）。
        let packed = pack_pending_normal(normal_codes::ENABLE, 1, 42);
        ctrl.pending_normal.store(packed, Ordering::Release);
        ctrl.emergency_action
            .store(emergency_codes::SAFETY_STOP, Ordering::Release);

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
        let ctrl = Arc::new(HardRtCtrlInner::new());
        let client = HardRtCtrlClient::new(Arc::clone(&ctrl));

        let handle =
            spawn_rt_thread(cfg, clock, adapter, cmd_reader, snap_pub, Arc::clone(&ctrl)).unwrap();

        // **task-30 v9 / M7**：spawn-time 不再 enable；通过 client 排
        // Enable(true) 让 rt_thread 真上扭，等 ack 完成再切 policy。
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
        cmd_writer.store(cmd);
        // 等 mock echo（snap_rdr 反映 cmd.positions[0]=1.0）
        assert!(
            crate::test_util::wait_until(Duration::from_millis(500), || {
                let s = snap_rdr.load();
                (s.state.positions[0] - 1.0).abs() < 1e-3
            }),
            "ActiveForward 500ms 内 mock 未 echo cmd[0]=1.0"
        );

        // 2) emergency
        client.safety_stop();

        // 3) 等 emergency 被 rt 消费（state.positions[0] 被清零）
        let metrics = Arc::clone(handle.metrics());
        assert!(
            crate::test_util::wait_until(Duration::from_millis(500), || {
                metrics.emergency_safety_stop_count.load(Ordering::Relaxed) >= 1
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
        cmd_writer.store(cmd2);
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
        let ctrl = Arc::new(HardRtCtrlInner::new());
        ctrl.policy
            .store(OutputPolicy::ActiveForward.to_u8(), Ordering::Release);
        let handle =
            spawn_rt_thread(cfg, clock, adapter, cmd_reader, snap_pub, Arc::clone(&ctrl)).unwrap();
        let metrics = Arc::clone(handle.metrics());

        // 等 rt warmup
        assert!(crate::test_util::wait_until(
            Duration::from_millis(500),
            || metrics.tick_count.load(Ordering::Relaxed) > 0
        ));
        ctrl.emergency_action
            .store(emergency_codes::SAFETY_STOP, Ordering::Release);
        // 等 emergency 被消费 + safety_state 升 StopFailed
        assert!(
            crate::test_util::wait_until(Duration::from_millis(500), || {
                ctrl.safety_state.load(Ordering::Acquire) == OutputSafetyState::StopFailed.to_u8()
            }),
            "safety_stop_rt Err 后 500ms 内未升 StopFailed"
        );
        handle.request_stop();
        handle.join().unwrap();

        // **关键断言**：safety_state = StopFailed，**不是** TorqueOffConfirmed
        assert_eq!(
            OutputSafetyState::from_u8(ctrl.safety_state.load(Ordering::Acquire)),
            OutputSafetyState::StopFailed,
            "safety_stop_rt Err 必须升 StopFailed，不能伪装 TorqueOffConfirmed"
        );
        // safety_latched 仍要 latch（防后续 Enable(true)）
        assert!(ctrl.safety_latched.load(Ordering::Acquire));
    }

    /// **finding #4 修复 lock 测试**：`client.safety_stop()` **同步**置 latch
    /// + drain pending。调用方拿回控制权时 latch 已生效（不依赖 rt_thread tick）。
    #[test]
    fn client_safety_stop_synchronously_latches_and_drains_pending() {
        use crate::hard_rt_ctrl::HardRtCtrlClient;
        use robot_core::executor_ctrl::{
            AdapterErr, AdapterNormalReq, AdapterResp, ExecutorCtrlClient, NormalReqStatus,
            unpack_completed,
        };

        // 不 spawn rt_thread；直接验 client 端 atomic 行为。
        let inner = Arc::new(HardRtCtrlInner::new());
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
        //    - pending 已被 drain（=0）
        //    - last_completed 反映 PreemptedByEmergency 给 seq
        //    - emergency_action=SAFETY_STOP
        assert!(inner.safety_latched.load(Ordering::Acquire));
        assert_eq!(inner.pending_normal.load(Ordering::Acquire), 0);
        let (last_seq, resp) = unpack_completed(inner.last_completed.load(Ordering::Acquire));
        assert_eq!(last_seq, seq);
        assert_eq!(resp, AdapterResp::Err(AdapterErr::PreemptedByEmergency));
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
        let ctrl = Arc::new(HardRtCtrlInner::new());
        // **policy=Disabled (default)** —— 不切到 ActiveForward。
        let handle = spawn_rt_thread(cfg, clock, adapter, cmd_reader, snap_pub, ctrl).unwrap();
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
        let ctrl = Arc::new(HardRtCtrlInner::new());
        ctrl.policy
            .store(OutputPolicy::ActiveForward.to_u8(), Ordering::Release);
        let handle = spawn_rt_thread(cfg, clock, adapter, cmd_reader, snap_pub, ctrl).unwrap();
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
