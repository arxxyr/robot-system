//! 串口唯一 worker：八线程同源启动门后运行统一收敛与正常执行 tick。
//! 所有设备 IO、事务抢占和停止确认均由 serial 独占；日志交给诊断环消费者。

#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

use robot_core::adapter::{JointCommandBuf, RtClass, RtSnapshot};
use robot_core::time::{Deadline, DurationNs, MonoRawNs, RobotClock};
use robot_transport::{LatestCommandReader, RtSnapshotPublisher};

use crate::serial_io_ctrl::SerialIoCtrlInner;
pub(crate) use crate::serial_tick::{SerialTick, SerialTickContext};

// ============================================================================
// SerialRtAdapter 综合 trait
// ============================================================================
//
// **task-30 v9 / M7 阶段 4 第二步**：trait 定义统一在 `robot_core::adapter`
// （与 `HardRtAdapter` 并列），factory `build_serial_rt` 入口才能跨 crate 返回
// `Box<dyn SerialRtAdapter + Send>`。这里 re-export 保持调用方 import 路径
// 不变。

pub use robot_core::adapter::SerialRtAdapter;

// ============================================================================
// Config / Metrics / Handle
// ============================================================================

/// `serial_io_thread` 配置。
#[derive(Clone, Copy, Debug)]
pub struct SerialIoThreadConfig {
    /// tick 周期（纳秒）。SerialRT 受串口 RTT 限制，典型 5-20 ms（5 ms = 200 Hz）。
    pub tick_period_ns: u64,
    /// 显式 `SCHED_FIFO` 优先级；配置的严格SerialRT传入此值并同时要求mlockall。
    /// 无配置时保留CLI兼容行为；`None`不申请FIFO。
    pub sched_fifo_priority: Option<u8>,
    /// opt-in 绑核（CLI `--cpu`；配合 `isolcpus` 内核参数把本线程放上
    /// 隔离核）。`None` = 不绑。
    pub cpu_affinity: Option<usize>,
    /// RT setup（FIFO / mlockall / pin）失败时 fail-fast（true）还是
    /// warn-and-continue（false，默认；开发机无 rtprio 时可降级跑）。
    pub strict_rt: bool,
}

impl Default for SerialIoThreadConfig {
    fn default() -> Self {
        Self {
            tick_period_ns: 5_000_000, // 5 ms = 200 Hz
            sched_fifo_priority: None,
            cpu_affinity: None,
            strict_rt: false,
        }
    }
}

/// `serial_io_thread` 观测计数器。
#[derive(Debug, Default)]
pub struct SerialIoThreadMetrics {
    /// 完成的 tick 数。
    pub tick_count: AtomicU64,
    /// `read_state` Err 次数。
    pub read_errors: AtomicU64,
    /// `write_command` Err 次数。
    pub write_errors: AtomicU64,
    /// `safety_stop` 下发次数；新串口协议包含启动、Fault 和 Emergency 的统一停止写。
    pub emergency_safety_stop_count: AtomicU64,
    /// confirm_torque_off 升 `TorqueOffConfirmed` 的次数。
    pub confirm_off_count: AtomicU64,
    /// confirm_torque_off 升 `AdapterUncertain` 的次数 ——
    /// **仅** `read_torque_enable_mask` 返 `RobotError::DeadlineExceeded`
    /// （部分回包 / timeout / 不可信）时计数。
    ///
    /// **不**计入"明确知道还有 motor torque-on"（那是 `Ok` + on_count > 0 的
    /// 情况，走 retry → 超阈值 → `confirm_failed_count`）。
    pub confirm_uncertain_count: AtomicU64,
    /// confirm_torque_off 升 `StopFailed` 的次数（超 confirm_max_attempts）。
    pub confirm_failed_count: AtomicU64,
    /// 被 latch 拒的 `Enable(true)` 计数（rt 端二次 check）。
    pub latch_rejected_count: AtomicU64,
    /// normal action 完成（成功 + 失败）计数。
    pub normal_completed_count: AtomicU64,
    /// emergency drain 时 cancel 一条 in-flight pending 的次数。
    pub emergency_preempted_count: AtomicU64,
    /// **S48 A3b**：`RtSnapshotPublisher::publish` 返回错误（版本耗尽 / 写端损坏）
    /// 的次数。首次出错即结束本 worker：已完成的物理 tick 计数不回滚，失败后不再
    /// 发布也不执行下一 tick；由 `robot-s0` 的 `is_finished` / 进度停滞监督判错。
    pub snapshot_publish_errors: AtomicU64,
    /// **S48 A3b（test-only）**：真实 `publish` 调用之前取的 `RobotClock` 时刻，
    /// 仅该次返回 `VersionExhausted` 时原子记录（0 = 未发生）。
    #[cfg(feature = "test-comm-fault-hook")]
    pub snapshot_publish_failure_attempt_ns: AtomicU64,
    /// **A2 freeze hook**（v0.3 / test-only，cfg-gated）：tick loop 顶端 spin
    /// 到该单调时钟 ns 为止；0 = 不冻。详 [`crate::freeze_hook`] 模块 doc。
    /// 仅 `cfg(any(test, feature = "test-freeze-hook"))` 编译时存在，**不进
    /// 生产 release**。SerialRT 真跑路径，A2-3 systemd e2e 必测之一。
    #[cfg(any(test, feature = "test-freeze-hook"))]
    pub freeze_until_ns: AtomicU64,
}

/// `serial_io_thread` 启动后返回的控制 handle。
pub struct SerialIoThreadHandle {
    join: JoinHandle<crate::hard_rt_ctrl::ExecutorExit>,
    stop: Arc<AtomicBool>,
    metrics: Arc<SerialIoThreadMetrics>,
}

impl SerialIoThreadHandle {
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }
    /// 请求停止。下 tick 检测后退出。
    pub fn request_stop(&self) {
        self.stop.swap(true, Ordering::AcqRel);
    }

    #[must_use]
    pub fn metrics(&self) -> &Arc<SerialIoThreadMetrics> {
        &self.metrics
    }

    /// 等待线程退出。
    ///
    /// # Errors
    ///
    /// `JoinHandle::join` 的 `Err`（线程 panic）。
    pub fn join(self) -> thread::Result<crate::hard_rt_ctrl::ExecutorExit> {
        self.join.join()
    }
}

// ============================================================================
// 入口
// ============================================================================

/// 启动 `serial_io_thread`。
///
/// 串口read/write允许原协议的毫秒级阻塞；周期执行仍禁止动态分配。
/// FIFO、内存锁定与绑核由显式配置决定，严格模式任一失败即拒绝运行。
///
/// - `cfg`：tick_period + confirm 阈值。
/// - `clock`：`SystemClock` 生产 / `MockClock` 测试。
/// - `adapter`：必须满足 [`SerialRtAdapter`]（`ManipulatorAdapter` +
///   `OutputSafetyProbe`）。**编译期**保证 confirm_torque_off 可用。
/// - `cmd_reader`：`ipc_thread` 写入端的对端。
/// - `snapshot_pub`：`pub_thread` 消费 joint state 的发布端。
/// - `ctrl`：[`SerialIoCtrlInner`]（与 `HardRtCtrlInner` 共享类型）共享 atomic 协议。
///
/// # Errors
///
/// `std::io::Error` 当周期为零、adapter/控制块类型不匹配或真实线程创建失败。
/// 设备 Connect 由启动门后的串口收敛机执行，结果通过原启动进度发布。
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
pub fn spawn_serial_io_thread<A>(
    cfg: SerialIoThreadConfig,
    clock: Arc<dyn RobotClock>,
    adapter: A,
    cmd_reader: LatestCommandReader<JointCommandBuf>,
    snapshot_pub: RtSnapshotPublisher<RtSnapshot>,
    ctrl: Arc<SerialIoCtrlInner>,
    probe: Arc<crate::serial_preempt::SerialPreemptProbe>,
    diag: Arc<crate::diag::SerialDiagRing>,
    authorized: bool,
    boot: Arc<crate::boot_supervisor::BootShared>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<SerialIoThreadHandle>
where
    A: SerialRtAdapter + Send + 'static,
{
    if cfg.tick_period_ns == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "SerialRT tick 周期必须大于零",
        ));
    }
    #[cfg(feature = "test-comm-fault-hook")]
    let adapter = crate::comm_fault_adapter::CommFaultAdapter::new(
        adapter,
        Arc::clone(&clock),
        probe.clone(),
        boot.observation_arc(),
        ctrl.clone(),
    );
    let info = robot_core::adapter::ManipulatorAdapter::info(&adapter);
    if info.rt_class != RtClass::SerialRT {
        return Err(std::io::Error::other(format!(
            "adapter '{}' reports {:?}, expected SerialRT",
            info.name, info.rt_class,
        )));
    }
    if ctrl.ctrl_kind != crate::hard_rt_ctrl::CtrlKind::SerialRt {
        return Err(std::io::Error::other("SerialRT 必须使用 new_serial 控制块"));
    }
    #[cfg(feature = "test-comm-fault-hook")]
    crate::comm_fault_hook::before_spawn("executor")?;
    let metrics = Arc::new(SerialIoThreadMetrics::default());
    let running_metrics = Arc::clone(&metrics);
    let running_stop = Arc::clone(&stop);
    let join = thread::Builder::new()
        .name("s0_serial_io_thread".into())
        .spawn(move || {
            // 在全部八个 worker（含唯一 writer）的 spawn 提交之前，任何 adapter 方法都不得调用。
            while !boot.spawned_reached.load(Ordering::Acquire) {
                if !crate::worker_stop::worker_running(
                    crate::worker_stop::WorkerKind::Executor,
                    clock.as_ref(),
                    &running_stop,
                ) {
                    return crate::hard_rt_ctrl::ExecutorExit::from_pending(None);
                }
                thread::sleep(std::time::Duration::from_millis(1));
            }
            let spawned_at =
                robot_core::time::MonoRawNs(boot.spawned_at_ns.load(Ordering::Relaxed));
            run_serial_hardened_io_loop(
                cfg,
                clock,
                adapter,
                cmd_reader,
                snapshot_pub,
                ctrl,
                probe,
                diag,
                authorized,
                spawned_at,
                running_stop,
                running_metrics,
                boot,
            )
        })?;
    Ok(SerialIoThreadHandle {
        join,
        stop,
        metrics,
    })
}

/// 调度设置也只发固定诊断，不从串口线程调用 tracing。
fn apply_serial_rt_setup(
    cfg: &SerialIoThreadConfig,
    diag: &dyn robot_core::diag::DiagSink,
    clock: &dyn RobotClock,
) -> bool {
    let mut okay = true;
    let mut observe = |kind, result: std::io::Result<()>| {
        if result.is_err() {
            okay = false;
        }
        diag.record(robot_core::diag::DiagEvent {
            stamp_ns: clock.now().0,
            site: 4,
            kind,
            a: u8::from(result.is_ok()),
            b: 0,
            seq_lo: 0,
        });
    };
    if let Some(cpu) = cfg.cpu_affinity {
        observe(3, robot_core::thread::pin_to_cpu(cpu));
    }
    if let Some(priority) = cfg.sched_fifo_priority {
        observe(2, robot_core::thread::mlockall());
        observe(1, robot_core::thread::set_sched_fifo(i32::from(priority)));
    }
    okay || !cfg.strict_rt
}

/// 初始化期预触碰后续调用栈工作区，不在周期路径按需分配。
#[inline(never)]
#[allow(
    clippy::large_stack_arrays,
    reason = "初始化期刻意预触碰线程栈页；移到堆上无法预触后续RT调用栈"
)]
fn prefault_serial_stack() {
    let mut pages = [0_u8; 128 * 1024];
    for page in pages.chunks_mut(4096) {
        // SAFETY: page非空且属于本函数栈对象；volatile保留实际页面写入。
        unsafe { std::ptr::write_volatile(page.as_mut_ptr(), 0) };
    }
    std::hint::black_box(&pages);
}

/// 保持原相位，丢弃错失周期，选择严格晚于当前时刻的首个周期点。
/// 零周期或未来时刻无法表示时返回 None，不饱和后连续执行过期 tick。
fn next_serial_deadline(
    previous: Deadline,
    now: MonoRawNs,
    period: DurationNs,
) -> Option<Deadline> {
    let elapsed = now.0.saturating_sub(previous.0.0);
    let periods = elapsed.checked_div(period.0)?.checked_add(1)?;
    let advance = periods.checked_mul(period.0)?;
    previous
        .0
        .0
        .checked_add(advance)
        .map(|next| Deadline(MonoRawNs(next)))
}

/// 八 worker 同源启动门打开后的唯一串口运行入口。
/// 本函数不执行调用方 connect，首个设备动作由 SerialTick 的 StopIssue 决定。
/// 长事务/调度迟到错失的周期直接丢弃，保留 spawned_at 相位，不补跑旧 tick。
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
pub(crate) fn run_serial_hardened_io_loop<A: SerialRtAdapter>(
    cfg: SerialIoThreadConfig,
    clock: Arc<dyn RobotClock>,
    mut adapter: A,
    cmd_reader: LatestCommandReader<JointCommandBuf>,
    mut snapshot_pub: RtSnapshotPublisher<RtSnapshot>,
    ctrl: Arc<SerialIoCtrlInner>,
    probe: Arc<crate::serial_preempt::SerialPreemptProbe>,
    diag: Arc<crate::diag::SerialDiagRing>,
    authorized: bool,
    spawned_at: robot_core::time::MonoRawNs,
    stop: Arc<AtomicBool>,
    metrics: Arc<SerialIoThreadMetrics>,
    boot: Arc<crate::boot_supervisor::BootShared>,
) -> crate::hard_rt_ctrl::ExecutorExit {
    let mut tick = SerialTick::new(authorized, adapter.joint_count(), spawned_at);
    let mut command = cmd_reader.initial_value();
    prefault_serial_stack();
    // 一次性TLS初始化在调度setup之前完成；不豁免首轮停止/连接设备调用。
    robot_core::rt_entry!({});
    if !apply_serial_rt_setup(&cfg, diag.as_ref(), clock.as_ref()) {
        return crate::hard_rt_ctrl::ExecutorExit::from_pending(None);
    }
    let context = SerialTickContext {
        ctrl: ctrl.as_ref(),
        probe: probe.as_ref(),
        diag: diag.as_ref(),
        clock: clock.as_ref(),
        metrics: metrics.as_ref(),
    };
    let mut deadline = Deadline(spawned_at);
    robot_core::rt_entry!({
        while crate::worker_stop::worker_running(
            crate::worker_stop::WorkerKind::Executor,
            clock.as_ref(),
            &stop,
        ) {
            #[cfg(any(test, feature = "test-freeze-hook"))]
            crate::freeze_hook::freeze_check(&metrics.freeze_until_ns, clock.as_ref(), &stop);
            let Some(next_deadline) =
                next_serial_deadline(deadline, clock.now(), DurationNs(cfg.tick_period_ns))
            else {
                // 仍走统一 into_exit，保留可能存在的原 Pending execution。
                break;
            };
            deadline = next_deadline;
            clock.sleep_until(deadline);
            if !crate::worker_stop::worker_running(
                crate::worker_stop::WorkerKind::Executor,
                clock.as_ref(),
                &stop,
            ) {
                break;
            }
            if let Some(latest) = cmd_reader.try_load() {
                command = latest;
            }
            let snapshot = tick.tick(&mut adapter, &command.as_view(), &context, deadline.0);
            #[cfg(feature = "test-comm-fault-hook")]
            if let Some(page) = boot.observation() {
                use crate::comm_fault_observation::ObservationField as F;
                if let Some(exit) = ctrl.serial_boot_progress().first_exit {
                    page.store_first(F::FirstExit, exit.0);
                }
                if ctrl.policy.load(Ordering::Acquire)
                    == robot_core::executor_ctrl::OutputPolicy::Disabled.to_u8()
                {
                    page.store_first(F::Disabled, clock.now().0);
                }
            }
            #[cfg(not(feature = "test-comm-fault-hook"))]
            let _ = &boot;
            // S48 A3b：发布失败即结束 worker；本 tick 已完成的计数不回滚。
            #[cfg(feature = "test-comm-fault-hook")]
            let attempt_started_ns = clock.now().0;
            let published = snapshot_pub.publish(&snapshot);
            #[cfg(feature = "test-comm-fault-hook")]
            if published == Err(robot_transport::SnapshotPublishError::VersionExhausted) {
                metrics
                    .snapshot_publish_failure_attempt_ns
                    .store(attempt_started_ns, Ordering::Relaxed);
            }
            if published.is_err() {
                metrics
                    .snapshot_publish_errors
                    .fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
        tick.into_exit()
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use robot_adapter_mock::MockHardRTAdapter;

    fn join_clean(handle: SerialIoThreadHandle) {
        let exit = handle.join().unwrap();
        if exit.has_pending_enable() {
            // 旧健康场景不得静默丢失 Execution；异常保原件直到测试进程退出。
            std::process::abort();
        }
    }
    use robot_core::adapter::{
        AdapterInfo, ControlMode, JointCommandView, JointLimits, JointStateBuf, ManipulatorAdapter,
    };
    use robot_core::error::RobotError;
    use robot_core::executor_ctrl::OutputSafetyProbe;
    use robot_core::executor_ctrl::{
        OutputPolicy, OutputSafetyState, SerialEmergencyCtrl, normal_codes,
    };
    use robot_core::time::SystemClock;
    use robot_transport::{LatestCommandSlot, RtSnapshotPublisher};
    use std::time::Duration;

    /// 测试 probe 行为开关（让 `SerialMockWrapper::read_torque_enable_mask`
    /// 注入可配置失败 / 部分回包，覆盖 finding #1 的两条新分支）。
    #[derive(Clone, Copy, Debug, Default)]
    enum ProbeMode {
        /// 默认：转发 inner mock（mirror enabled flag）。
        #[default]
        Normal,
        /// 强制返"全部仍 torque-on"：覆盖 `confirm_failed_count` 升级路径。
        ForceAllOn,
        /// 强制返 `Err(DeadlineExceeded)`：覆盖 `confirm_uncertain_count` 升级路径。
        ForceDeadlineExceeded,
        /// 强制返其它 Err（HardwareFault）：覆盖 `confirm_failed_count` Err 路径。
        ForceHardwareFault,
    }

    /// **测试专用 wrapper**（finding #3 配套）：把 mock 的 `rt_class` 从
    /// `HardRT` 改为 `SerialRT`，让 spawn_serial_io_thread 的 RtClass guard
    /// 通过。`probe_mode: Arc<AtomicU8>` 让测试在运行期切换 probe 行为。
    /// 除 `info().rt_class` 与 `read_torque_enable_mask` 外，所有方法直接
    /// 转发 inner mock。
    struct SerialMockWrapper {
        inner: MockHardRTAdapter,
        probe_mode: Arc<std::sync::atomic::AtomicU8>,
        /// 置位后 `read_state` **失败前故意部分写坏 out**（段(7) round 1 P0
        /// 回归锁用；trait 未禁止该行为，调用方必须防住）。
        read_fail: Arc<std::sync::atomic::AtomicBool>,
        read_calls: Arc<AtomicU64>,
        after_read: Option<Box<dyn FnMut() + Send>>,
        after_read_fired: bool,
        after_enable: Option<Box<dyn FnMut() + Send>>,
        after_enable_fired: bool,
        enable_true_calls: Arc<std::sync::atomic::AtomicU64>,
    }

    /// `read_fail` 模式下写入 `positions[0]` 的哨兵值。
    const READ_FAIL_POISON: f32 = 999.0;

    /// 两份快照的 positions 是否逐位相同（clippy 禁 f32 数组整体 `==`）。
    fn positions_identical(a: &JointStateBuf, b: &JointStateBuf) -> bool {
        a.positions
            .iter()
            .zip(b.positions.iter())
            .all(|(x, y)| x.to_bits() == y.to_bits())
    }

    impl SerialMockWrapper {
        fn new(inner: MockHardRTAdapter) -> Self {
            Self {
                inner,
                probe_mode: Arc::new(std::sync::atomic::AtomicU8::new(ProbeMode::Normal as u8)),
                read_fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                read_calls: Arc::new(AtomicU64::new(0)),
                after_read: None,
                after_read_fired: false,
                after_enable: None,
                after_enable_fired: false,
                enable_true_calls: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            }
        }

        fn probe_handle(&self) -> Arc<std::sync::atomic::AtomicU8> {
            Arc::clone(&self.probe_mode)
        }

        fn read_fail_handle(&self) -> Arc<std::sync::atomic::AtomicBool> {
            Arc::clone(&self.read_fail)
        }
    }

    fn set_probe_mode(handle: &std::sync::atomic::AtomicU8, mode: ProbeMode) {
        handle.store(mode as u8, Ordering::Release);
    }

    impl ManipulatorAdapter for SerialMockWrapper {
        fn info(&self) -> AdapterInfo {
            let inner = self.inner.info();
            AdapterInfo {
                name: "serial_mock_wrapper",
                kind: inner.kind,
                rt_class: RtClass::SerialRT, // ← 关键 override
                vendor_sdk: inner.vendor_sdk,
            }
        }
        fn connect(&mut self) -> Result<(), RobotError> {
            assert!(robot_core::rt_guard::in_rt_scope());
            self.inner.connect()
        }
        fn disconnect(&mut self) -> Result<(), RobotError> {
            assert!(robot_core::rt_guard::in_rt_scope());
            self.inner.disconnect()
        }
        fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError> {
            assert!(robot_core::rt_guard::in_rt_scope());
            self.read_calls.fetch_add(1, Ordering::Relaxed);
            if self.read_fail.load(Ordering::Acquire) {
                // 失败前污染输出：调用方必须整体丢弃这次读。
                out.positions[0] = READ_FAIL_POISON;
                out.stamp_ns = 0;
                return Err(RobotError::HardwareFault("simulated serial read failure"));
            }
            self.inner.read_state(out)?;
            if !self.after_read_fired {
                self.after_read_fired = true;
                if let Some(hook) = self.after_read.as_mut() {
                    hook();
                }
            }
            Ok(())
        }
        fn write_command(&mut self, cmd: &JointCommandView) -> Result<(), RobotError> {
            assert!(robot_core::rt_guard::in_rt_scope());
            self.inner.write_command(cmd)
        }
        fn set_mode(&mut self, mode: ControlMode) -> Result<(), RobotError> {
            assert!(robot_core::rt_guard::in_rt_scope());
            self.inner.set_mode(mode)
        }
        fn enable(&mut self, on: bool) -> Result<(), RobotError> {
            assert!(robot_core::rt_guard::in_rt_scope());
            if on {
                self.enable_true_calls.fetch_add(1, Ordering::Relaxed);
            }
            self.inner.enable(on)?;
            if !self.after_enable_fired {
                self.after_enable_fired = true;
                if let Some(hook) = self.after_enable.as_mut() {
                    hook();
                }
            }
            Ok(())
        }
        fn safety_stop(&mut self) -> Result<(), RobotError> {
            assert!(robot_core::rt_guard::in_rt_scope());
            self.inner.safety_stop()
        }
        fn limits(&self) -> &JointLimits {
            self.inner.limits()
        }
    }

    impl OutputSafetyProbe for SerialMockWrapper {
        fn read_torque_enable_mask(&mut self, out: &mut [bool]) -> Result<(), RobotError> {
            assert!(robot_core::rt_guard::in_rt_scope());
            let mode = self.probe_mode.load(Ordering::Acquire);
            if mode == ProbeMode::ForceAllOn as u8 {
                let n = self.inner.joint_count();
                if out.len() < n {
                    return Err(RobotError::InvalidParameter(
                        "OutputSafetyProbe out len < joint_count",
                    ));
                }
                for slot in &mut out[..n] {
                    *slot = true;
                }
                Ok(())
            } else if mode == ProbeMode::ForceDeadlineExceeded as u8 {
                Err(RobotError::DeadlineExceeded)
            } else if mode == ProbeMode::ForceHardwareFault as u8 {
                Err(RobotError::HardwareFault("simulated probe hw fault"))
            } else {
                self.inner.read_torque_enable_mask(out)
            }
        }
        fn joint_count(&self) -> usize {
            self.inner.joint_count()
        }
    }

    /// SerialRtAdapter blanket impl：mock 同时实现 ManipulatorAdapter +
    /// OutputSafetyProbe → 自动满足 SerialRtAdapter（编译期断言）。
    #[test]
    fn mock_satisfies_serial_rt_adapter_bound() {
        fn assert_serial_rt<T: SerialRtAdapter>() {}
        assert_serial_rt::<MockHardRTAdapter>();
        assert_serial_rt::<SerialMockWrapper>();
    }

    #[test]
    fn invalid_cpu_setup_rejects_strict_mode_and_preserves_legacy_diagnostics() {
        let clock = robot_core::time::MockClock::new(MonoRawNs(123));
        for strict_rt in [true, false] {
            let diag = crate::diag::SerialDiagRing::new();
            let cfg = SerialIoThreadConfig {
                cpu_affinity: Some(usize::MAX),
                sched_fifo_priority: None,
                strict_rt,
                ..SerialIoThreadConfig::default()
            };
            // CPU容量校验在syscall前拒绝；此测试不修改亲和性、FIFO或锁页状态。
            assert_eq!(apply_serial_rt_setup(&cfg, &diag, &clock), !strict_rt);
            assert_eq!(
                diag.pop(),
                Some(robot_core::diag::DiagEvent {
                    stamp_ns: 123,
                    site: 4,
                    kind: 3,
                    a: 0,
                    b: 0,
                    seq_lo: 0,
                })
            );
            assert_eq!(diag.pop(), None);
            assert_eq!(diag.dropped(), 0);
        }
    }

    #[test]
    fn periodic_schedule_deadline_math_handles_zero_and_overflow() {
        for (previous, now, period, expected) in [
            (0, 0, 0, None),
            (0, 0, 5, Some(5)),
            (7, 20, 5, Some(22)),
            (7, 22, 5, Some(27)),
            (20, 7, 5, Some(25)),
            (u64::MAX - 10, u64::MAX - 9, 5, Some(u64::MAX - 5)),
            (u64::MAX - 10, u64::MAX - 5, 5, Some(u64::MAX)),
            (u64::MAX - 10, u64::MAX, 5, None),
            (0, u64::MAX, 1, None),
            (u64::MAX - 1, u64::MAX - 1, 2, None),
        ] {
            assert_eq!(
                next_serial_deadline(
                    Deadline(MonoRawNs(previous)),
                    MonoRawNs(now),
                    DurationNs(period)
                )
                .map(|deadline| deadline.0.0),
                expected,
                "previous={previous}, now={now}, period={period}"
            );
        }
    }

    /// 只记录真实外层 loop 的 sleep 目标；最后一次 sleep 内置 stop 锁住睡后停止门。
    struct ScheduleClock {
        inner: robot_core::time::MockClock,
        deadlines: std::sync::Mutex<Vec<u64>>,
        stop: Arc<AtomicBool>,
    }

    impl RobotClock for ScheduleClock {
        fn now(&self) -> robot_core::time::MonoRawNs {
            self.inner.now()
        }

        fn sleep_until(&self, deadline: Deadline) {
            let mut deadlines = self.deadlines.lock().unwrap();
            deadlines.push(deadline.0.0);
            self.inner.sleep_until(deadline);
            if deadlines.len() == 6 {
                self.stop.store(true, Ordering::Release);
            }
        }

        fn sleep_for(&self, duration: DurationNs) {
            self.inner.sleep_for(duration);
        }
    }

    struct ScheduleObservation {
        deadlines: Vec<u64>,
        tick_count: u64,
        read_calls: u64,
        snapshot: RtSnapshot,
    }

    fn observe_periodic_schedule(
        read_duration: DurationNs,
        stop_after_read: bool,
    ) -> ScheduleObservation {
        let stop = Arc::new(AtomicBool::new(false));
        let clock = Arc::new(ScheduleClock {
            inner: robot_core::time::MockClock::new(robot_core::time::MonoRawNs(1_000_000)),
            deadlines: std::sync::Mutex::new(Vec::with_capacity(6)),
            stop: stop.clone(),
        });
        let mut adapter = SerialMockWrapper::new(MockHardRTAdapter::with_unlimited_limits(2));
        let read_calls = adapter.read_calls.clone();
        let read_clock = clock.clone();
        let read_stop = stop.clone();
        // 真实 Stop / Connect / Confirm 后，第 4 tick 的首次周期事务消耗指定时间。
        adapter.after_read = Some(Box::new(move || {
            read_clock.inner.advance(read_duration);
            if stop_after_read {
                read_stop.store(true, Ordering::Release);
            }
        }));
        let ctrl = Arc::new(SerialIoCtrlInner::new_serial(
            crate::hard_rt_ctrl::test_authority(true),
        ));
        let worker_clock: Arc<dyn RobotClock> = clock.clone();
        let probe = Arc::new(crate::serial_preempt::SerialPreemptProbe::new(
            worker_clock.clone(),
            ctrl.clone(),
        ));
        let boot = Arc::new(crate::boot_supervisor::BootShared::new(clock.now()));
        let (_commands, reader) = LatestCommandSlot::new(JointCommandBuf {
            joint_count: 2,
            ..JointCommandBuf::default()
        });
        let (publisher, snapshot) = RtSnapshotPublisher::new(RtSnapshot::default());
        let metrics = Arc::new(SerialIoThreadMetrics::default());
        let exit = run_serial_hardened_io_loop(
            SerialIoThreadConfig::default(),
            worker_clock,
            adapter,
            reader,
            publisher,
            ctrl,
            probe,
            Arc::new(crate::diag::SerialDiagRing::new()),
            true,
            clock.now(),
            stop,
            metrics.clone(),
            boot,
        );
        assert!(!exit.has_pending_enable());
        let deadlines = clock.deadlines.lock().unwrap().clone();
        ScheduleObservation {
            deadlines,
            tick_count: metrics.tick_count.load(Ordering::Relaxed),
            read_calls: read_calls.load(Ordering::Relaxed),
            snapshot: snapshot.load(),
        }
    }

    fn assert_periodic_schedule(read_duration: DurationNs, expected: [u64; 6]) {
        let observed = observe_periodic_schedule(read_duration, false);
        assert_eq!(observed.deadlines, expected);
        assert_eq!(
            observed.tick_count, 5,
            "跳过的周期和 stop 后都不得补记 tick"
        );
        assert_eq!(
            observed.read_calls, 2,
            "睡后 stop 必须阻止下一次 adapter 调用"
        );
        assert_eq!(observed.snapshot.tick_seq, 5);
        assert_eq!(observed.snapshot.tick_deadline_ns, expected[4]);
    }

    #[test]
    fn periodic_schedule_overrun_skips_expired_ticks_and_preserves_phase() {
        assert_periodic_schedule(
            DurationNs(17_000_000),
            [
                6_000_000, 11_000_000, 16_000_000, 21_000_000, 41_000_000, 46_000_000,
            ],
        );
    }

    #[test]
    fn periodic_schedule_equal_boundary_selects_strictly_future_tick() {
        assert_periodic_schedule(
            DurationNs(5_000_000),
            [
                6_000_000, 11_000_000, 16_000_000, 21_000_000, 31_000_000, 36_000_000,
            ],
        );
    }

    #[test]
    fn periodic_schedule_normal_cadence_keeps_original_phase() {
        assert_periodic_schedule(
            DurationNs::ZERO,
            [
                6_000_000, 11_000_000, 16_000_000, 21_000_000, 26_000_000, 31_000_000,
            ],
        );
    }

    #[test]
    fn periodic_schedule_stop_after_read_does_not_sleep_or_call_adapter_again() {
        let observed = observe_periodic_schedule(DurationNs(17_000_000), true);
        assert_eq!(
            observed.deadlines,
            [6_000_000, 11_000_000, 16_000_000, 21_000_000]
        );
        assert_eq!(observed.tick_count, 4);
        assert_eq!(observed.read_calls, 1);
        assert_eq!(observed.snapshot.tick_seq, 4);
        assert_eq!(observed.snapshot.tick_deadline_ns, 21_000_000);
    }

    #[test]
    fn periodic_schedule_zero_period_is_rejected_before_spawn() {
        let clock: Arc<dyn RobotClock> = Arc::new(robot_core::time::MockClock::default());
        let (_commands, reader) = LatestCommandSlot::new(JointCommandBuf::default());
        let (publisher, _snapshot) = RtSnapshotPublisher::new(RtSnapshot::default());
        let result = spawn_test_serial(
            SerialIoThreadConfig {
                tick_period_ns: 0,
                ..SerialIoThreadConfig::default()
            },
            clock,
            SerialMockWrapper::new(MockHardRTAdapter::with_unlimited_limits(2)),
            reader,
            publisher,
            Arc::new(SerialIoCtrlInner::new_serial(
                crate::hard_rt_ctrl::test_authority(true),
            )),
        );
        match result {
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput),
            Ok(handle) => {
                handle.request_stop();
                join_clean(handle);
                panic!("零周期必须在创建 worker 前拒绝");
            }
        }
    }

    /// 父进程监督 stop / 真版本耗尽；孩子保原 Exit/owner/lease 到进程退出。
    #[cfg(feature = "test-comm-fault-hook")]
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "同一父进程监督停止与版本耗尽两条真实线程出口，原残件保留到子进程退出"
    )]
    fn stopped_or_snapshot_failed_worker_returns_its_original_pending_execution() {
        use robot_core::executor_ctrl::ExecutorCtrlClient;

        const CHILD: &str = "ROBOT_W3_EXECUTOR_EXIT_CASE";
        if let Some(case) = std::env::var_os(CHILD) {
            let owner_thread = std::thread::current().id();
            std::panic::set_hook(Box::new(move |info| {
                eprintln!("执行器原件验收失败：{info}");
                if std::thread::current().id() == owner_thread {
                    std::process::exit(101);
                }
            }));
            let snapshot_failure = case == "snapshot";
            let ctrl = Arc::new(SerialIoCtrlInner::new_serial(
                crate::hard_rt_ctrl::test_authority(true),
            ));
            let clock: Arc<dyn RobotClock> = Arc::new(robot_core::time::MockClock::default());
            let stop = Arc::new(AtomicBool::new(false));
            let owner = Arc::new(std::sync::Mutex::new(None));
            let lease = Arc::new(std::sync::Mutex::new(None));
            let seq = Arc::new(AtomicU64::new(0));
            let mut adapter = SerialMockWrapper::new(MockHardRTAdapter::with_unlimited_limits(2));
            let (submitted_owner, submitted_seq, submit_ctrl) =
                (owner.clone(), seq.clone(), ctrl.clone());
            let submit_client = crate::hard_rt_ctrl::HardRtCtrlClient::new(submit_ctrl.clone());
            // 首次周期读发生在真实启动 Stop/Connect/Confirm 完成之后。
            adapter.after_read = Some(Box::new(move || {
                use robot_core::executor_ctrl::{
                    AdapterNormalReq, ExecutorCtrlClient, NormalReqStatus,
                };
                let (owner, view) = submit_ctrl.authority().reserve_enable().unwrap();
                submit_ctrl.start_slot().post(view).unwrap();
                let NormalReqStatus::Submitted { seq } =
                    submit_client.request_normal(AdapterNormalReq::Enable(true))
                else {
                    panic!("真实 Enable 通道未接纳原 owner");
                };
                *submitted_owner.lock().unwrap() = Some(owner);
                submitted_seq.store(seq, Ordering::Release);
            }));
            let (held_lease, active_ctrl, stopped) = (lease.clone(), ctrl.clone(), stop.clone());
            adapter.after_enable = Some(Box::new(move || {
                let view = active_ctrl.execution_slot().load().unwrap();
                let original = active_ctrl
                    .authority()
                    .acquire_normal_io(robot_core::control_authority::NormalIoIntent::EnableStep(
                        view,
                    ))
                    .unwrap();
                *held_lease.lock().unwrap() = Some(original);
                if !snapshot_failure {
                    stopped.store(true, Ordering::Release);
                }
            }));
            let (_commands, reader) = LatestCommandSlot::new(JointCommandBuf {
                joint_count: 2,
                ..JointCommandBuf::default()
            });
            // Stop / Connect / Confirm 各占一 tick；第 4 tick 的周期读提交 Enable。
            // 容许这四次真实发布，第 5 tick 执行 Enable 并保留原 Pending 后才耗尽。
            let version = if snapshot_failure {
                robot_transport::MAX_PUBLISHED_VERSION - 8
            } else {
                0
            };
            let (publisher, snapshot) =
                RtSnapshotPublisher::new_with_version(RtSnapshot::default(), version).unwrap();
            let boot = Arc::new(crate::boot_supervisor::BootShared::new(clock.now()));
            boot.spawned_at_ns.store(clock.now().0, Ordering::Relaxed);
            boot.spawned_reached.store(true, Ordering::Release);
            let probe = Arc::new(crate::serial_preempt::SerialPreemptProbe::new(
                clock.clone(),
                ctrl.clone(),
            ));
            let handle = spawn_serial_io_thread(
                SerialIoThreadConfig::default(),
                clock,
                adapter,
                reader,
                publisher,
                ctrl.clone(),
                probe,
                Arc::new(crate::diag::SerialDiagRing::new()),
                true,
                boot,
                stop,
            )
            .unwrap();
            assert!(crate::test_util::wait_until(Duration::from_secs(2), || {
                handle.is_finished()
            }));
            let metrics = handle.metrics().clone();
            let exit = handle.join().unwrap();
            assert!(exit.has_pending_enable());
            let sequence = seq.load(Ordering::Acquire);
            assert_ne!(sequence, 0);
            let client = crate::hard_rt_ctrl::HardRtCtrlClient::new(ctrl.clone());
            assert_eq!(client.poll_last_normal_resp(sequence), None);
            let refused = ctrl
                .authority()
                .retire_enable(owner.lock().unwrap().take().unwrap())
                .unwrap_err();
            assert_eq!(
                refused.reason,
                robot_core::control_authority::Refusal::WrongPhase(
                    robot_core::control_authority::Phase::EnableExecuting
                )
            );
            assert!(ctrl.authority().snapshot().normal_io_active);
            assert!(lease.lock().unwrap().is_some());
            assert_eq!(
                metrics.snapshot_publish_errors.load(Ordering::Relaxed),
                u64::from(snapshot_failure)
            );
            if snapshot_failure {
                assert_eq!(snapshot.version(), robot_transport::MAX_PUBLISHED_VERSION);
                assert_eq!(metrics.tick_count.load(Ordering::Relaxed), 5);
            }
            eprintln!("W3_EXECUTOR_EXIT_PRESERVED case={}", case.to_string_lossy());
            // 原 Exit、原 owner 及 lease 全保留，不造额外恢复入口为夹具收尾。
            std::process::exit(0);
        }
        for case in ["stop", "snapshot"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("exit.log");
            let log = std::fs::File::create(&path).unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "serial_io_thread::tests::stopped_or_snapshot_failed_worker_returns_its_original_pending_execution", "--nocapture"])
                .env(CHILD, case).stdout(log.try_clone().unwrap()).stderr(log).spawn().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    result => {
                        let _ = child.kill();
                        let reap = std::time::Instant::now() + Duration::from_secs(2);
                        loop {
                            if matches!(child.try_wait(), Ok(Some(_))) {
                                break;
                            }
                            if std::time::Instant::now() >= reap {
                                std::process::abort();
                            }
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        panic!("执行器验收子进程已 kill/reap：{result:?}");
                    }
                }
            };
            let raw = std::fs::read_to_string(path).unwrap();
            assert!(
                status.success()
                    && raw.contains(&format!("W3_EXECUTOR_EXIT_PRESERVED case={case}")),
                "{raw}"
            );
            eprintln!("W3_EXECUTOR_EXIT_CHILD_OK case={case}");
        }
    }

    /// **finding #3 lock**：spawn 入口运行期校验 adapter 自报 SerialRT。
    /// S48 A3b：快照版本耗尽 → 已完成的物理 tick 计数不回滚，失败后不再发布也不
    /// 执行下一 tick，worker 结束且错误计数可观察；reader 保留上一完整快照。
    #[cfg(feature = "test-comm-fault-hook")]
    #[test]
    fn snapshot_version_exhaustion_ends_the_serial_worker_without_rolling_back_ticks() {
        use robot_transport::MAX_PUBLISHED_VERSION;
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let adapter = SerialMockWrapper::new(MockHardRTAdapter::with_unlimited_limits(2));
        let (_cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, snap_rdr) =
            RtSnapshotPublisher::new_with_version(RtSnapshot::default(), MAX_PUBLISHED_VERSION - 2)
                .unwrap();
        let cfg = SerialIoThreadConfig {
            tick_period_ns: 1_000_000,
            ..SerialIoThreadConfig::default()
        };
        let ctrl = Arc::new(SerialIoCtrlInner::new_serial(
            crate::hard_rt_ctrl::test_authority(true),
        ));
        ctrl.policy
            .store(OutputPolicy::Disabled.to_u8(), Ordering::Release);
        let handle =
            spawn_test_serial(cfg, clock, adapter, cmd_reader, snap_pub, Arc::clone(&ctrl))
                .unwrap();
        assert!(crate::test_util::wait_until(Duration::from_secs(3), || {
            handle.is_finished()
        }));
        let metrics = Arc::clone(handle.metrics());
        join_clean(handle);
        assert_eq!(metrics.snapshot_publish_errors.load(Ordering::Relaxed), 1);
        assert_eq!(
            metrics.tick_count.load(Ordering::Relaxed),
            2,
            "第一 tick 发布成功（到 MAX）、第二 tick 完成后发布失败：两次物理 tick 均计数"
        );
        assert_eq!(snap_rdr.version(), MAX_PUBLISHED_VERSION, "不回绕");
    }

    /// HardRT mock 直接传入 → 拒启动；不让 HardRT adapter 误进 serial_io_thread。
    #[test]
    fn spawn_rejects_non_serial_rt_adapter() {
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let adapter = MockHardRTAdapter::with_unlimited_limits(1);
        let (_cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, _) = RtSnapshotPublisher::new(RtSnapshot::default());
        let ctrl = Arc::new(SerialIoCtrlInner::new_serial(
            crate::hard_rt_ctrl::test_authority(true),
        ));
        let res = spawn_test_serial(
            SerialIoThreadConfig::default(),
            clock,
            adapter,
            cmd_reader,
            snap_pub,
            ctrl,
        );
        let Err(err) = res else {
            panic!("HardRT adapter should be rejected by RtClass guard");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("HardRT") && msg.contains("SerialRT"),
            "expected RtClass guard rejection, got: {msg}"
        );
    }

    /// 一组共享的测试套件输出：句柄、ctrl、snapshot reader、probe-mode handle。
    struct SerialIoTestKit {
        handle: SerialIoThreadHandle,
        ctrl: Arc<SerialIoCtrlInner>,
        snap_rdr: robot_transport::RtSnapshotReader<RtSnapshot>,
        probe_mode: Arc<std::sync::atomic::AtomicU8>,
        read_fail: Arc<std::sync::atomic::AtomicBool>,
        enable_true_calls: Arc<std::sync::atomic::AtomicU64>,
    }

    fn setup(joint_count: u8) -> SerialIoTestKit {
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        // 用 wrapper 让 rt_class=SerialRT，过 spawn 的 RtClass guard。
        let adapter = SerialMockWrapper::new(MockHardRTAdapter::with_unlimited_limits(joint_count));
        let probe_mode = adapter.probe_handle();
        let read_fail = adapter.read_fail_handle();
        let enable_true_calls = Arc::clone(&adapter.enable_true_calls);
        let (_cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, snap_rdr) = RtSnapshotPublisher::new(RtSnapshot::default());
        let cfg = SerialIoThreadConfig {
            tick_period_ns: 1_000_000, // 1 ms
            ..SerialIoThreadConfig::default()
        };
        let ctrl = Arc::new(SerialIoCtrlInner::new_serial(
            crate::hard_rt_ctrl::test_authority(true),
        ));
        // **finding #2 修复后**：spawn-time **不**调 enable；mock 默认
        // enabled=false，read_torque_enable_mask 全 0 ≡ "torque off"。
        // 测试不依赖 ActiveForward 写命令；仍可手动 set_policy。
        ctrl.policy
            .store(OutputPolicy::Disabled.to_u8(), Ordering::Release);
        let handle =
            spawn_test_serial(cfg, clock, adapter, cmd_reader, snap_pub, Arc::clone(&ctrl))
                .unwrap();
        assert!(crate::test_util::wait_until(
            Duration::from_secs(2),
            || ctrl.boot_outcome.load(Ordering::Acquire) == 1
        ));
        SerialIoTestKit {
            handle,
            ctrl,
            snap_rdr,
            probe_mode,
            read_fail,
            enable_true_calls,
        }
    }

    fn spawn_test_serial<A: SerialRtAdapter + 'static>(
        cfg: SerialIoThreadConfig,
        clock: Arc<dyn RobotClock>,
        adapter: A,
        reader: LatestCommandReader<JointCommandBuf>,
        publisher: RtSnapshotPublisher<RtSnapshot>,
        ctrl: Arc<SerialIoCtrlInner>,
    ) -> std::io::Result<SerialIoThreadHandle> {
        let boot = Arc::new(crate::boot_supervisor::BootShared::new(clock.now()));
        boot.spawned_at_ns.store(clock.now().0, Ordering::Relaxed);
        boot.spawned_reached.store(true, Ordering::Release);
        let probe = Arc::new(crate::serial_preempt::SerialPreemptProbe::new(
            clock.clone(),
            ctrl.clone(),
        ));
        spawn_serial_io_thread(
            cfg,
            clock,
            adapter,
            reader,
            publisher,
            ctrl,
            probe,
            Arc::new(crate::diag::SerialDiagRing::new()),
            true,
            boot,
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// **round 1 P0 回归锁**（v0.5 候选 E 段(7)，与 `rt_thread` 同款）：
    /// `read_state` 失败时**不得刷新采样时间戳**，也不得把失败前的部分写入
    /// 并进正式快照。
    #[test]
    fn read_failure_freezes_state_and_stamp() {
        use crate::test_util::wait_until;
        let kit = setup(3);
        let metrics = Arc::clone(kit.handle.metrics());
        let snap_rdr = &kit.snap_rdr;

        // ① 正常读：等 stamp 推进。
        assert!(wait_until(Duration::from_secs(2), || {
            snap_rdr.load().state.stamp_ns > 0 && metrics.tick_count.load(Ordering::Relaxed) > 2
        }));
        let errors_before = metrics.read_errors.load(Ordering::Relaxed);

        // ② 失败模式：快照必须完全冻结。
        kit.read_fail.store(true, Ordering::Release);
        assert!(wait_until(Duration::from_secs(3), || {
            metrics.read_errors.load(Ordering::Relaxed) > errors_before
        }));
        let frozen_a = snap_rdr.load().state;
        let frozen_tick = metrics.tick_count.load(Ordering::Relaxed);
        assert!(wait_until(Duration::from_secs(3), || {
            metrics.tick_count.load(Ordering::Relaxed) >= frozen_tick + 10
        }));
        let frozen_b = snap_rdr.load().state;
        assert_eq!(
            frozen_a.stamp_ns, frozen_b.stamp_ns,
            "读失败期间采样时间戳必须冻结"
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

        // ③ 错误源消失且正确 ACK 仍不得自动解除 ADAPTER_FAULT 或恢复普通采样。
        kit.read_fail.store(false, Ordering::Release);
        if let Some(fault) = kit.ctrl.mailbox_snapshot() {
            kit.ctrl
                .policy
                .store(OutputPolicy::Disabled.to_u8(), Ordering::Release);
            kit.ctrl.ack_fault(fault.generation);
        }
        let acknowledged_tick = metrics.tick_count.load(Ordering::Relaxed);
        assert!(wait_until(Duration::from_secs(3), || metrics
            .tick_count
            .load(Ordering::Relaxed)
            >= acknowledged_tick + 10));
        assert_eq!(snap_rdr.load().state.stamp_ns, frozen_b.stamp_ns);
        assert_ne!(
            kit.ctrl.inhibit_bits() & crate::hard_rt_ctrl::inhibit::ADAPTER_FAULT,
            0
        );

        kit.handle.request_stop();
        join_clean(kit.handle);
    }

    /// 启动 + 跑稳态 + tick_count > 0 + clean shutdown。
    #[test]
    fn serial_io_thread_spawn_and_runs() {
        use crate::test_util::wait_until;
        let kit = setup(2);
        let metrics = Arc::clone(kit.handle.metrics());
        // 等至少 1 tick（不依赖固定 sleep）
        assert!(wait_until(Duration::from_secs(2), || {
            metrics.tick_count.load(Ordering::Relaxed) > 0
        }));
        kit.handle.request_stop();
        join_clean(kit.handle);
    }

    /// emergency drain：触发 `safety_stop` → safety_state 升 `StopCommanded`
    /// → 下个 tick confirm_torque_off 升 `TorqueOffConfirmed`（mock
    /// `read_torque_enable_mask` 返 enabled flag，emergency 后 enabled=false →
    /// 全 0 → confirmed）。
    #[test]
    fn serial_io_thread_emergency_then_confirm_torque_off() {
        use crate::test_util::wait_until;
        let kit = setup(2);
        let metrics = Arc::clone(kit.handle.metrics());
        // warmup
        assert!(wait_until(Duration::from_secs(2), || {
            metrics.tick_count.load(Ordering::Relaxed) > 0
        }));

        // emergency
        crate::hard_rt_ctrl::HardRtCtrlClient::new(kit.ctrl.clone()).estop_monotonic();
        // **关键**：初始 safety_state 已是 TorqueOffConfirmed（spawn-time 设的），
        // 直接等 TorqueOffConfirmed 会立即 true，假阳性。改成等
        // emergency_safety_stop_count >= 1 **且** confirm_off_count >= 1
        // —— 必须 emergency 跑过 + 至少一次 confirm 真把状态升回 TorqueOffConfirmed。
        assert!(
            wait_until(Duration::from_secs(2), || {
                metrics.emergency_safety_stop_count.load(Ordering::Relaxed) >= 2
                    && metrics.confirm_off_count.load(Ordering::Relaxed) >= 2
            }),
            "emergency + confirm 2s 内未升 TorqueOffConfirmed"
        );
        kit.handle.request_stop();
        join_clean(kit.handle);

        assert!(kit.ctrl.safety_latched());
        assert_eq!(
            kit.ctrl.safety_state(),
            OutputSafetyState::TorqueOffConfirmed
        );
        assert!(metrics.emergency_safety_stop_count.load(Ordering::Relaxed) >= 2);
        assert!(metrics.confirm_off_count.load(Ordering::Relaxed) >= 2);
        // 已禁用输出不得被后续急停放宽。
        assert_eq!(
            OutputPolicy::from_u8(kit.ctrl.policy.load(Ordering::Acquire)),
            OutputPolicy::Disabled
        );
    }

    /// 保留旧具名：原真实预约/公告经 EStop 撤销后，执行器必须拒绝迟到请求。
    /// 拒绝来自同一 authority 的旧 StartView，不能依赖已移除的第二份 latch。
    #[test]
    fn serial_io_thread_secondary_latch_check_rejects_enable_true() {
        use crate::test_util::wait_until;
        use robot_core::executor_ctrl::{AdapterErr, AdapterResp, pack_pending_normal};

        let kit = setup(1);
        let metrics = Arc::clone(kit.handle.metrics());
        let (owner, view) = kit.ctrl.authority().reserve_enable().unwrap();
        kit.ctrl.start_slot().post(view).unwrap();
        assert!(!kit.ctrl.start_slot().is_empty());

        crate::hard_rt_ctrl::HardRtCtrlClient::new(kit.ctrl.clone()).estop_monotonic();
        assert!(
            wait_until(Duration::from_secs(2), || {
                kit.ctrl.safety_latched() && metrics.confirm_off_count.load(Ordering::Relaxed) >= 2
            }),
            "emergency 2s 内未 latch"
        );

        // 原视图已公告；延迟到真实 EStop/Confirm 后才让原请求到达执行器。
        let raw_seq = 99u64;
        let packed = pack_pending_normal(normal_codes::ENABLE, 1, raw_seq);
        kit.ctrl.pending_normal.store(packed, Ordering::Release);

        // 等 last_completed 写入 (raw_seq, ...)
        assert!(
            wait_until(Duration::from_secs(2), || {
                let last = kit.ctrl.last_completed.load(Ordering::Acquire);
                let (last_seq, _) = robot_core::executor_ctrl::unpack_completed(last);
                last_seq == raw_seq
            }),
            "真实撤销视图的执行拒绝 2s 内未发布"
        );
        assert!(kit.ctrl.start_slot().is_empty(), "执行器必须实际消费原视图");
        let retired = kit.ctrl.authority().retire_enable(owner).unwrap();
        assert_eq!(
            retired.retired_from,
            robot_core::control_authority::Phase::EnableQueued
        );
        assert!(retired.superseded);
        kit.handle.request_stop();
        join_clean(kit.handle);

        let last = kit.ctrl.last_completed.load(Ordering::Acquire);
        let (last_seq, resp) = robot_core::executor_ctrl::unpack_completed(last);
        assert_eq!(last_seq, raw_seq);
        assert_eq!(resp, AdapterResp::Err(AdapterErr::AuthorityRevoked));
        assert_eq!(kit.enable_true_calls.load(Ordering::Relaxed), 0);
        assert!(kit.ctrl.safety_latched());
        assert_eq!(
            kit.ctrl.safety_state(),
            OutputSafetyState::TorqueOffConfirmed
        );
        assert_eq!(
            kit.ctrl.authority().snapshot().phase,
            Some(robot_core::control_authority::Phase::Idle)
        );
    }

    /// confirm_torque_off 升 `TorqueOffConfirmed` 一致性：
    /// emergency 后 mock `read_torque_enable_mask` 返全 0 → safety_state =
    /// TorqueOffConfirmed（不是 StopCommanded）。验 §6 confirm 路径走通。
    #[test]
    fn serial_io_thread_confirm_off_advances_safety_state() {
        use crate::test_util::wait_until;
        let kit = setup(3);
        let metrics = Arc::clone(kit.handle.metrics());
        crate::hard_rt_ctrl::HardRtCtrlClient::new(kit.ctrl.clone()).estop_monotonic();
        // 等 emergency + 至少一次 confirm_off 真把状态升回 TorqueOffConfirmed。
        // 初始 safety_state 已是 TorqueOffConfirmed，必须看到 metrics 计数才能
        // 证明真跑完 emergency → confirm 链路。
        assert!(
            wait_until(Duration::from_secs(2), || {
                metrics.emergency_safety_stop_count.load(Ordering::Relaxed) >= 2
                    && metrics.confirm_off_count.load(Ordering::Relaxed) >= 2
            }),
            "emergency + confirm 2s 内未跑完链路"
        );
        kit.handle.request_stop();
        join_clean(kit.handle);
        // 状态进入 TorqueOffConfirmed（confirmed），不是停在 StopCommanded
        let s = kit.ctrl.safety_state();
        assert_eq!(
            s,
            OutputSafetyState::TorqueOffConfirmed,
            "emergency 后 confirm 应升 TorqueOffConfirmed，实际 {s:?}"
        );
    }

    /// **finding #1 lock**：probe 持续返"全部 torque-on"（明确知道还没断扭）→
    /// 连续 `confirm_max_attempts` tick 后升 `StopFailed`，**不**走
    /// `AdapterUncertain`。证明 v9 修订后"明确状态" vs "不可信" 的语义分叉。
    #[test]
    fn serial_io_thread_persistent_torque_on_yields_stop_failed() {
        use crate::test_util::wait_until;
        // confirm_max_attempts = 2 让测试更快收敛
        let kit = setup(3);
        let metrics = Arc::clone(kit.handle.metrics());
        // 在 emergency 之前先把 probe 切到 ForceAllOn —— 这样 emergency 跑完后
        // 进入 confirm 路径会拿到全 1，触发 retry → StopFailed。
        set_probe_mode(&kit.probe_mode, ProbeMode::ForceAllOn);

        crate::hard_rt_ctrl::HardRtCtrlClient::new(kit.ctrl.clone()).estop_monotonic();
        assert!(
            wait_until(Duration::from_secs(2), || {
                kit.ctrl.safety_state() == OutputSafetyState::StopFailed
            }),
            "持续 torque-on 2s 内未升 StopFailed"
        );
        kit.handle.request_stop();
        join_clean(kit.handle);

        let s = kit.ctrl.safety_state();
        assert_eq!(s, OutputSafetyState::StopFailed);
        // **不**应升 AdapterUncertain
        assert_eq!(
            metrics.confirm_uncertain_count.load(Ordering::Relaxed),
            0,
            "明确知道还有 motor torque-on 不应误升 AdapterUncertain"
        );
        assert!(metrics.confirm_failed_count.load(Ordering::Relaxed) >= 1);
    }

    /// **finding #1 lock**：probe 返 `Err(DeadlineExceeded)`（部分回包/timeout）
    /// → safety_state 升 `AdapterUncertain`，**不是** `StopFailed`。
    /// 这是 finding #1 真正"不可信"语义的入口。
    #[test]
    fn serial_io_thread_deadline_exceeded_yields_adapter_uncertain() {
        use crate::test_util::wait_until;
        let kit = setup(2);
        let metrics = Arc::clone(kit.handle.metrics());
        set_probe_mode(&kit.probe_mode, ProbeMode::ForceDeadlineExceeded);

        crate::hard_rt_ctrl::HardRtCtrlClient::new(kit.ctrl.clone()).estop_monotonic();
        assert!(
            wait_until(Duration::from_secs(2), || {
                kit.ctrl.safety_state() == OutputSafetyState::AdapterUncertain
            }),
            "DeadlineExceeded 2s 内未升 AdapterUncertain"
        );
        kit.handle.request_stop();
        join_clean(kit.handle);

        let s = kit.ctrl.safety_state();
        assert_eq!(
            s,
            OutputSafetyState::AdapterUncertain,
            "DeadlineExceeded 应升 AdapterUncertain（部分回包不可信），实际 {s:?}"
        );
        assert!(metrics.confirm_uncertain_count.load(Ordering::Relaxed) >= 1);
        assert_eq!(
            metrics.confirm_failed_count.load(Ordering::Relaxed),
            0,
            "DeadlineExceeded 不该走 StopFailed 路径"
        );
    }

    /// **A2 freeze hook 单测**：设 `freeze_until_ns` 后，serial_io_thread 的
    /// `tick_count` 在 spin 期间不增长；过期后恢复。SerialRT 路径是 v0.2 task
    /// #15 真跑的实路径，A2-3 systemd e2e 必测之一（reviewer 拍板硬要求）。
    #[test]
    fn freeze_hook_holds_tick_count() {
        use crate::test_util::wait_until;
        let sys_clock = SystemClock::new();
        let kit = setup(2);
        let metrics = Arc::clone(kit.handle.metrics());

        // 等 worker 跑起来
        assert!(wait_until(Duration::from_secs(2), || {
            metrics.tick_count.load(Ordering::Relaxed) >= 5
        }));
        let pre_freeze = metrics.tick_count.load(Ordering::Relaxed);

        // 设 freeze 80ms
        let until = sys_clock.now().0 + 80_000_000;
        metrics.freeze_until_ns.store(until, Ordering::Release);

        // freeze 中段（40ms）
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

        kit.handle.request_stop();
        join_clean(kit.handle);
    }
}
