//! Task-30 v9 / M6：`s0_serial_io_thread` —— SerialRT 路径的 IO 主循环。
//!
//! 对应 `docs/designs/task-30_state_machine_adapter_link.md` §4.5 + §6
//! confirm_torque_off 路径 + `docs/00_design.md` §4.6 SerialRT 段（pre-1c）。
//!
//! ## 角色
//!
//! `RtClass::SerialRT` adapter（如 `FeetechAdapter`）的串口 RTT 是毫秒量级，
//! **绝不能挂 `s0_rt_thread`** 的 SCHED_FIFO 循环（违反 §4.9 RT 铁律）。
//! 本线程是 SerialRT adapter 的专属 server：
//!
//! - **普通优先级**（`pthread default`），允许 `read/write` 串口阻塞。
//! - 主循环走 [`crate::serial_io_ctrl::SerialIoCtrlInner`] 的 atomic 协议
//!   消费 emergency / normal action（与 `rt_thread` 同款 packed slot 设计，
//!   见 [`crate::hard_rt_ctrl`] 模块文档）。
//! - **confirm_torque_off**：emergency 后下一 tick 调
//!   [`OutputSafetyProbe::read_torque_enable_mask`] 真读 motor 寄存器确认
//!   断扭。
//!
//! ## 主循环（每 tick，普通线程，无 RT 铁律）
//!
//! ```text
//! 1) emergency drain：
//!    em = ctrl.emergency_action.swap(NONE)
//!    if em == SAFETY_STOP:
//!        adapter.safety_stop()        // 单 broadcast 帧；< 10ms
//!        ctrl.safety_state = StopCommanded   // 下 tick 才 confirm（finding #4）
//!        ctrl.safety_latched = true
//!        ctrl.policy = StopTorque             // 兜底
//!        drain pending → last_completed = PreemptedByEmergency
//!
//! 2) confirm_torque_off（state == StopCommanded 且**本 tick 没刚做过
//!    emergency** —— finding #4：让硬件有时间真断扭 + 避免单 tick 内
//!    连续 broadcast write + sync_read 抢串口）：
//!    probe.read_torque_enable_mask(out) →
//!       Ok + on_count == 0       → safety_state = TorqueOffConfirmed
//!       Ok + on_count >  0       → confirm_attempts++; 超阈值升 StopFailed
//!                                  （明确知道还有电机 torque-on，不是不确定）
//!       Err(DeadlineExceeded)    → safety_state = AdapterUncertain
//!                                  （部分回包/timeout，**真**"不可信"语义）
//!       Err(其它)                → safety_state = StopFailed
//!
//! 3) consume normal action：
//!    pending = ctrl.pending_normal.swap(0)
//!    if pending != 0:
//!        二次 latch check（拒 Enable(true) 写 SafetyLatched）
//!        execute → write last_completed
//!
//! 4) policy gate + read_state：
//!    if policy ∈ {ActiveForward, HoldCurrent, ZeroCommand, ReadyHold}:
//!        adapter.write_command(latest_cmd)
//!    adapter.read_state(state_buf)
//!    publish snapshot
//!
//! 5) sleep_until(deadline += tick_period)
//! ```
//!
//! ## 启动行为（finding #2 修订）
//!
//! `spawn_serial_io_thread` 只调 `adapter.connect()`，**不**调
//! `adapter.enable(true)` —— 让 state_machine 通过 `Safe → Ready → Enabled`
//! 两阶段 transition 显式排 `AdapterNormalReq::Enable(true)` 走 ipc_thread →
//! ctrl → serial_io_thread Step 3 真下使能。
//!
//! 初始状态：`safety_state = TorqueOffConfirmed`，`policy = Disabled`（默认）。
//!
//! ## RtClass 守门（finding #3 修订）
//!
//! `spawn_serial_io_thread` 入口运行期校验 `adapter.info().rt_class ==
//! RtClass::SerialRT`，否则返 Error 拒启动。`HardRtAdapter` mock 误传进来会
//! 立即被拒，不走主循环。
//!
//! M6 当前实现 **skeleton**：可启动 + 跑稳态 + emergency → confirm 链路走通；
//! M7 `s0_daemon dispatch` 后接 `ipc_thread` 走 ClearEStop 解 latch 的端到端
//! 测试 + Orin 真机闭环验证留 M7。

#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

use robot_core::adapter::{
    ControlMode, JointCommandBuf, JointStateBuf, MAX_JOINTS, RtClass, RtSnapshot,
};
use robot_core::error::RobotError;
use robot_core::executor_ctrl::{
    AdapterErr, AdapterResp, OutputPolicy, OutputSafetyState, emergency_codes, normal_codes,
    pack_completed, unpack_pending_normal,
};
use robot_core::time::{Deadline, DurationNs, RobotClock};
use robot_transport::{LatestCommandReader, RtSnapshotPublisher};

use crate::serial_io_ctrl::SerialIoCtrlInner;

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
    /// `confirm_torque_off` 连续多少 tick 仍全 torque-on 才升 `StopFailed`。
    /// 默认 5（5 × 5ms = 25ms 上限给硬件断扭）。
    pub confirm_max_attempts: u32,
}

impl Default for SerialIoThreadConfig {
    fn default() -> Self {
        Self {
            tick_period_ns: 5_000_000, // 5 ms = 200 Hz
            confirm_max_attempts: 5,
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
    /// emergency `safety_stop` 下发次数。
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
}

/// `serial_io_thread` 启动后返回的控制 handle。
pub struct SerialIoThreadHandle {
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    metrics: Arc<SerialIoThreadMetrics>,
}

impl SerialIoThreadHandle {
    /// 请求停止。下 tick 检测后退出。
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
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
    pub fn join(self) -> thread::Result<()> {
        self.join.join()
    }
}

// ============================================================================
// 入口
// ============================================================================

/// 启动 `serial_io_thread`。
///
/// 与 `spawn_rt_thread` 对称的入口，但**不**走 SCHED_FIFO，**不**强制
/// RT 铁律：允许串口 read/write 阻塞 ms 级。
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
/// `std::io::Error` 当 `adapter.connect()` 失败 / `thread::spawn` 失败。
#[allow(clippy::needless_pass_by_value)]
pub fn spawn_serial_io_thread<A>(
    cfg: SerialIoThreadConfig,
    clock: Arc<dyn RobotClock>,
    mut adapter: A,
    cmd_reader: LatestCommandReader<JointCommandBuf>,
    snapshot_pub: RtSnapshotPublisher<RtSnapshot>,
    ctrl: Arc<SerialIoCtrlInner>,
) -> std::io::Result<SerialIoThreadHandle>
where
    A: SerialRtAdapter + Send + 'static,
{
    // **finding #3 修复**：运行期校验 adapter 自报 `RtClass::SerialRT`。
    // 上层 s0_daemon 已按 RtClass 分发，但本入口必须独立守门 —— 防止误用
    // （如测试中传 HardRT mock 进 SerialIo 路径）；编译期 `SerialRtAdapter`
    // bound 不能区分 rt_class 字符串，运行期 guard 是必要的二道闸。
    let info = adapter.info();
    if info.rt_class != RtClass::SerialRT {
        return Err(std::io::Error::other(format!(
            "spawn_serial_io_thread: adapter '{}' reports rt_class={:?}, \
             expected RtClass::SerialRT",
            info.name, info.rt_class,
        )));
    }

    let stop = Arc::new(AtomicBool::new(false));
    let metrics = Arc::new(SerialIoThreadMetrics::default());

    // **finding #2 修复**：spawn-time 只 connect，**不** enable(true)。
    // 旧实现"启动即上扭"会绕过 state_machine（Safe → Ready → Enabled
    // 两阶段 transition）；M7 接通 dispatch 后 ipc_thread 通过
    // `client.request_normal(Enable(true))` 驱动 enable，serial_io_thread
    // 在 Step 3 consume normal action 时调 adapter.enable(true)。
    //
    // 初始 safety_state = `TorqueOffConfirmed`（adapter 默认 torque off；
    // mock connect 后 enabled=false，feetech connect 不动 Torque_Enable）。
    // 初始 policy = `Disabled`（默认值）→ Step 4 `do_write` 白名单不含
    // Disabled，所以不会调 write_command。
    adapter
        .connect()
        .map_err(|e| std::io::Error::other(format!("serial adapter connect failed: {e}")))?;
    ctrl.safety_state.store(
        OutputSafetyState::TorqueOffConfirmed.to_u8(),
        Ordering::Release,
    );

    let join_stop = Arc::clone(&stop);
    let join_metrics = Arc::clone(&metrics);
    let join_ctrl = Arc::clone(&ctrl);

    let join = thread::Builder::new()
        .name("s0_serial_io_thread".into())
        .spawn(move || {
            run_serial_io_loop(
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

    Ok(SerialIoThreadHandle {
        join,
        stop,
        metrics,
    })
}

// ============================================================================
// 主循环
// ============================================================================

#[allow(
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::too_many_arguments
)]
fn run_serial_io_loop<A: SerialRtAdapter>(
    cfg: SerialIoThreadConfig,
    clock: Arc<dyn RobotClock>,
    mut adapter: A,
    cmd_reader: LatestCommandReader<JointCommandBuf>,
    snapshot_pub: RtSnapshotPublisher<RtSnapshot>,
    stop: &Arc<AtomicBool>,
    metrics: &Arc<SerialIoThreadMetrics>,
    ctrl: &Arc<SerialIoCtrlInner>,
) {
    let mut state_buf = JointStateBuf::default();
    let mut tick_seq: u64 = 0;
    let mut deadline = Deadline(clock.now());

    // confirm_torque_off 状态：连续多少 tick 仍 motor torque-on。
    let mut confirm_attempts: u32 = 0;
    // 栈分配的 torque-enable mask buffer（与 OutputSafetyProbe 契约一致）。
    let mut torque_mask = [false; MAX_JOINTS];

    while !stop.load(Ordering::Acquire) {
        let tick_period = DurationNs(cfg.tick_period_ns);
        deadline = deadline + tick_period;
        clock.sleep_until(deadline);

        // ─── 1) emergency drain ────────────────────────────────────────
        let em = ctrl
            .emergency_action
            .swap(emergency_codes::NONE, Ordering::AcqRel);
        if em == emergency_codes::SAFETY_STOP {
            // safety_stop 下发（FeetechAdapter::safety_stop = 单 broadcast
            // sync_write Torque_Enable=0，< 10ms）。
            let result = adapter.safety_stop();
            ctrl.safety_latched.store(true, Ordering::Release);
            // **finding #1 兜底**：把 policy 切到 StopTorque。client safety_stop()
            // 已先 store 过；这里再写一次防 emergency_action 由非 client 路径直置。
            ctrl.policy
                .store(OutputPolicy::StopTorque.to_u8(), Ordering::Release);
            // SerialRT 与 HardRT 不同：safety_stop 下发**不**等于 confirmed。
            // 升 `StopCommanded`，下 tick confirm_torque_off 才能升
            // `TorqueOffConfirmed` / `StopFailed`。
            let new_state = match result {
                Ok(()) => OutputSafetyState::StopCommanded,
                Err(_) => OutputSafetyState::StopFailed,
            };
            ctrl.safety_state
                .store(new_state.to_u8(), Ordering::Release);
            confirm_attempts = 0;
            metrics
                .emergency_safety_stop_count
                .fetch_add(1, Ordering::Relaxed);

            // drain pending（兜底，client 已 drain 过）
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

        // ─── 2) confirm_torque_off ────────────────────────────────────
        //
        // 仅在 safety_state == StopCommanded 时跑；其它状态不调 probe（节省
        // 串口 RTT）。
        // **finding #4**：emergency 后跳过本 tick 的 confirm 路径（让硬件有
        // 时间真断扭 + 与文档"下 tick confirm"一致 + 避免单 tick 内连续做
        // broadcast write + sync_read 抢串口）。
        let just_did_emergency = em == emergency_codes::SAFETY_STOP;

        if !just_did_emergency
            && ctrl.safety_state.load(Ordering::Acquire) == OutputSafetyState::StopCommanded.to_u8()
        {
            let n = adapter.joint_count();
            // mask buffer 的有效前缀长度 = n（n ≤ MAX_JOINTS）
            //
            // **finding #1 收紧版**：
            // - probe 成功 + on_count == 0 → `TorqueOffConfirmed`
            // - probe 成功 + on_count > 0 → 还有电机没断扭，**继续 retry**
            //   `confirm_attempts++`，超阈值升 `StopFailed`。**不再误**升级
            //   `AdapterUncertain` —— 那是"部分回包/不可信" 语义，不是"明确
            //   知道还在 torque-on"。
            // - probe 返 `DeadlineExceeded`（串口部分回包/timeout 但有部分
            //   可信信息）→ `AdapterUncertain`（保守）
            // - probe 返其它 Err → `StopFailed`
            match adapter.read_torque_enable_mask(&mut torque_mask[..]) {
                Ok(()) => {
                    let on_count = torque_mask[..n].iter().filter(|b| **b).count();
                    if on_count == 0 {
                        // 全部 torque-off → 升 confirmed
                        ctrl.safety_state.store(
                            OutputSafetyState::TorqueOffConfirmed.to_u8(),
                            Ordering::Release,
                        );
                        metrics.confirm_off_count.fetch_add(1, Ordering::Relaxed);
                        confirm_attempts = 0;
                    } else {
                        // **finding #1 修订**：on_count > 0（含 partial 与 全 on）
                        // 都是"明确还有 motor torque-on"，统一 retry 路径，
                        // 超阈值升 StopFailed。
                        confirm_attempts = confirm_attempts.saturating_add(1);
                        if confirm_attempts >= cfg.confirm_max_attempts {
                            ctrl.safety_state
                                .store(OutputSafetyState::StopFailed.to_u8(), Ordering::Release);
                            metrics.confirm_failed_count.fetch_add(1, Ordering::Relaxed);
                            confirm_attempts = 0;
                        }
                        // 否则保留 StopCommanded，下 tick 再 probe
                    }
                }
                Err(RobotError::DeadlineExceeded) => {
                    // 真"部分回包/timeout 但有部分可信信息"语义 → AdapterUncertain
                    // （保守：调用方应视为 fail-safe 没拿到证据，等同 StopFailed
                    // 等级处理；wire S0SafetyState=AdapterUncertain 让运维知道
                    // 这是 uncertain 而非 hard-fail）。
                    ctrl.safety_state.store(
                        OutputSafetyState::AdapterUncertain.to_u8(),
                        Ordering::Release,
                    );
                    metrics
                        .confirm_uncertain_count
                        .fetch_add(1, Ordering::Relaxed);
                    confirm_attempts = 0;
                }
                Err(_) => {
                    // 其它 Err（HardwareFault / NotConnected / ...）→ StopFailed
                    ctrl.safety_state
                        .store(OutputSafetyState::StopFailed.to_u8(), Ordering::Release);
                    metrics.confirm_failed_count.fetch_add(1, Ordering::Relaxed);
                    confirm_attempts = 0;
                }
            }
        }

        // ─── 3) consume normal action ─────────────────────────────────
        let pending = ctrl.pending_normal.swap(0, Ordering::AcqRel);
        if pending != 0 {
            let (action, arg, seq) = unpack_pending_normal(pending);
            if action != normal_codes::NONE {
                let latched = ctrl.safety_latched.load(Ordering::Acquire);
                let resp = if latched && action == normal_codes::ENABLE && arg == 1 {
                    metrics.latch_rejected_count.fetch_add(1, Ordering::Relaxed);
                    AdapterResp::Err(AdapterErr::SafetyLatched)
                } else {
                    execute_serial_normal(&mut adapter, action, arg, ctrl)
                };
                ctrl.last_completed
                    .store(pack_completed(seq, resp), Ordering::Release);
                metrics
                    .normal_completed_count
                    .fetch_add(1, Ordering::Relaxed);
            }
        }

        // ─── 4) policy gate + read_state ──────────────────────────────
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
    }
}

/// SerialRT normal action 执行。与 HardRT 不同的是 SerialRT 支持
/// `Connect / Disconnect / HoldCurrent`（非 RT 路径，串口 IO 允许）。
#[inline]
fn execute_serial_normal<A: SerialRtAdapter>(
    adapter: &mut A,
    action: u8,
    arg: u8,
    ctrl: &SerialIoCtrlInner,
) -> AdapterResp {
    match action {
        normal_codes::ENABLE => {
            let on = arg != 0;
            match adapter.enable(on) {
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
            match adapter.set_mode(mode) {
                Ok(()) => AdapterResp::Ok,
                Err(_) => AdapterResp::Err(AdapterErr::Unsupported),
            }
        }
        normal_codes::CONNECT => match adapter.connect() {
            Ok(()) => AdapterResp::Ok,
            Err(_) => AdapterResp::Err(AdapterErr::HardwareFault),
        },
        normal_codes::DISCONNECT => match adapter.disconnect() {
            Ok(()) => AdapterResp::Ok,
            Err(_) => AdapterResp::Err(AdapterErr::HardwareFault),
        },
        normal_codes::HOLD_CURRENT => {
            // SerialRT-specific：HoldCurrent 是 FeetechAdapter 的扩展能力
            // （见 `FeetechAdapter::hold_current_position`）。trait object
            // 路径上没有直接入口，先返 Unsupported；M7 + SerialRT-only trait
            // 扩展时再接通。
            AdapterResp::Err(AdapterErr::Unsupported)
        }
        _ => AdapterResp::Err(AdapterErr::Unsupported),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use robot_adapter_mock::MockHardRTAdapter;
    use robot_core::adapter::{
        AdapterInfo, ControlMode, JointCommandView, JointLimits, JointStateBuf, ManipulatorAdapter,
    };
    use robot_core::executor_ctrl::OutputSafetyProbe;
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
    }

    impl SerialMockWrapper {
        fn new(inner: MockHardRTAdapter) -> Self {
            Self {
                inner,
                probe_mode: Arc::new(std::sync::atomic::AtomicU8::new(ProbeMode::Normal as u8)),
            }
        }

        fn probe_handle(&self) -> Arc<std::sync::atomic::AtomicU8> {
            Arc::clone(&self.probe_mode)
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
            self.inner.connect()
        }
        fn disconnect(&mut self) -> Result<(), RobotError> {
            self.inner.disconnect()
        }
        fn read_state(&mut self, out: &mut JointStateBuf) -> Result<(), RobotError> {
            self.inner.read_state(out)
        }
        fn write_command(&mut self, cmd: &JointCommandView) -> Result<(), RobotError> {
            self.inner.write_command(cmd)
        }
        fn set_mode(&mut self, mode: ControlMode) -> Result<(), RobotError> {
            self.inner.set_mode(mode)
        }
        fn enable(&mut self, on: bool) -> Result<(), RobotError> {
            self.inner.enable(on)
        }
        fn safety_stop(&mut self) -> Result<(), RobotError> {
            self.inner.safety_stop()
        }
        fn limits(&self) -> &JointLimits {
            self.inner.limits()
        }
    }

    impl OutputSafetyProbe for SerialMockWrapper {
        fn read_torque_enable_mask(&mut self, out: &mut [bool]) -> Result<(), RobotError> {
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

    /// **finding #3 lock**：spawn 入口运行期校验 adapter 自报 SerialRT。
    /// HardRT mock 直接传入 → 拒启动；不让 HardRT adapter 误进 serial_io_thread。
    #[test]
    fn spawn_rejects_non_serial_rt_adapter() {
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let adapter = MockHardRTAdapter::with_unlimited_limits(1);
        let (_cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, _) = RtSnapshotPublisher::new(RtSnapshot::default());
        let ctrl = Arc::new(SerialIoCtrlInner::new());
        let res = spawn_serial_io_thread(
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
        _snap_rdr: robot_transport::RtSnapshotReader<RtSnapshot>,
        probe_mode: Arc<std::sync::atomic::AtomicU8>,
    }

    fn setup(joint_count: u8) -> SerialIoTestKit {
        setup_with_attempts(joint_count, 3)
    }

    fn setup_with_attempts(joint_count: u8, confirm_max_attempts: u32) -> SerialIoTestKit {
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        // 用 wrapper 让 rt_class=SerialRT，过 spawn 的 RtClass guard。
        let adapter = SerialMockWrapper::new(MockHardRTAdapter::with_unlimited_limits(joint_count));
        let probe_mode = adapter.probe_handle();
        let (_cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, snap_rdr) = RtSnapshotPublisher::new(RtSnapshot::default());
        let cfg = SerialIoThreadConfig {
            tick_period_ns: 1_000_000, // 1 ms
            confirm_max_attempts,
        };
        let ctrl = Arc::new(SerialIoCtrlInner::new());
        // **finding #2 修复后**：spawn-time **不**调 enable；mock 默认
        // enabled=false，read_torque_enable_mask 全 0 ≡ "torque off"。
        // 测试不依赖 ActiveForward 写命令；仍可手动 set_policy。
        ctrl.policy
            .store(OutputPolicy::Disabled.to_u8(), Ordering::Release);
        let handle =
            spawn_serial_io_thread(cfg, clock, adapter, cmd_reader, snap_pub, Arc::clone(&ctrl))
                .unwrap();
        SerialIoTestKit {
            handle,
            ctrl,
            _snap_rdr: snap_rdr,
            probe_mode,
        }
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
        kit.handle.join().unwrap();
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
        kit.ctrl
            .emergency_action
            .store(emergency_codes::SAFETY_STOP, Ordering::Release);
        // **关键**：初始 safety_state 已是 TorqueOffConfirmed（spawn-time 设的），
        // 直接等 TorqueOffConfirmed 会立即 true，假阳性。改成等
        // emergency_safety_stop_count >= 1 **且** confirm_off_count >= 1
        // —— 必须 emergency 跑过 + 至少一次 confirm 真把状态升回 TorqueOffConfirmed。
        assert!(
            wait_until(Duration::from_secs(2), || {
                metrics.emergency_safety_stop_count.load(Ordering::Relaxed) >= 1
                    && metrics.confirm_off_count.load(Ordering::Relaxed) >= 1
            }),
            "emergency + confirm 2s 内未升 TorqueOffConfirmed"
        );
        kit.handle.request_stop();
        kit.handle.join().unwrap();

        assert!(kit.ctrl.safety_latched.load(Ordering::Acquire));
        assert_eq!(
            OutputSafetyState::from_u8(kit.ctrl.safety_state.load(Ordering::Acquire)),
            OutputSafetyState::TorqueOffConfirmed
        );
        assert!(metrics.emergency_safety_stop_count.load(Ordering::Relaxed) >= 1);
        assert!(metrics.confirm_off_count.load(Ordering::Relaxed) >= 1);
        // policy 兜底切 StopTorque
        assert_eq!(
            OutputPolicy::from_u8(kit.ctrl.policy.load(Ordering::Acquire)),
            OutputPolicy::StopTorque
        );
    }

    /// 二次 latch check：emergency 之后直接注入 Enable(true) packed slot，
    /// serial_io_thread step 3 二次 check 拒，写 `Err(SafetyLatched)`。
    #[test]
    fn serial_io_thread_secondary_latch_check_rejects_enable_true() {
        use crate::test_util::wait_until;
        use robot_core::executor_ctrl::{AdapterErr, AdapterResp, pack_pending_normal};

        let kit = setup(1);
        let metrics = Arc::clone(kit.handle.metrics());

        kit.ctrl
            .emergency_action
            .store(emergency_codes::SAFETY_STOP, Ordering::Release);
        assert!(
            wait_until(Duration::from_secs(2), || {
                kit.ctrl.safety_latched.load(Ordering::Acquire)
            }),
            "emergency 2s 内未 latch"
        );

        // 直接注入 Enable(true) packed slot
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
            "secondary latch check 2s 内未触发"
        );
        kit.handle.request_stop();
        kit.handle.join().unwrap();

        let last = kit.ctrl.last_completed.load(Ordering::Acquire);
        let (last_seq, resp) = robot_core::executor_ctrl::unpack_completed(last);
        assert_eq!(last_seq, raw_seq);
        assert_eq!(resp, AdapterResp::Err(AdapterErr::SafetyLatched));
        assert!(metrics.latch_rejected_count.load(Ordering::Relaxed) >= 1);
    }

    /// confirm_torque_off 升 `TorqueOffConfirmed` 一致性：
    /// emergency 后 mock `read_torque_enable_mask` 返全 0 → safety_state =
    /// TorqueOffConfirmed（不是 StopCommanded）。验 §6 confirm 路径走通。
    #[test]
    fn serial_io_thread_confirm_off_advances_safety_state() {
        use crate::test_util::wait_until;
        let kit = setup(3);
        let metrics = Arc::clone(kit.handle.metrics());
        kit.ctrl
            .emergency_action
            .store(emergency_codes::SAFETY_STOP, Ordering::Release);
        // 等 emergency + 至少一次 confirm_off 真把状态升回 TorqueOffConfirmed。
        // 初始 safety_state 已是 TorqueOffConfirmed，必须看到 metrics 计数才能
        // 证明真跑完 emergency → confirm 链路。
        assert!(
            wait_until(Duration::from_secs(2), || {
                metrics.emergency_safety_stop_count.load(Ordering::Relaxed) >= 1
                    && metrics.confirm_off_count.load(Ordering::Relaxed) >= 1
            }),
            "emergency + confirm 2s 内未跑完链路"
        );
        kit.handle.request_stop();
        kit.handle.join().unwrap();
        // 状态进入 TorqueOffConfirmed（confirmed），不是停在 StopCommanded
        let s = OutputSafetyState::from_u8(kit.ctrl.safety_state.load(Ordering::Acquire));
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
        let kit = setup_with_attempts(3, 2);
        let metrics = Arc::clone(kit.handle.metrics());
        // 在 emergency 之前先把 probe 切到 ForceAllOn —— 这样 emergency 跑完后
        // 进入 confirm 路径会拿到全 1，触发 retry → StopFailed。
        set_probe_mode(&kit.probe_mode, ProbeMode::ForceAllOn);

        kit.ctrl
            .emergency_action
            .store(emergency_codes::SAFETY_STOP, Ordering::Release);
        assert!(
            wait_until(Duration::from_secs(2), || {
                kit.ctrl.safety_state.load(Ordering::Acquire)
                    == OutputSafetyState::StopFailed.to_u8()
            }),
            "持续 torque-on 2s 内未升 StopFailed"
        );
        kit.handle.request_stop();
        kit.handle.join().unwrap();

        let s = OutputSafetyState::from_u8(kit.ctrl.safety_state.load(Ordering::Acquire));
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

        kit.ctrl
            .emergency_action
            .store(emergency_codes::SAFETY_STOP, Ordering::Release);
        assert!(
            wait_until(Duration::from_secs(2), || {
                kit.ctrl.safety_state.load(Ordering::Acquire)
                    == OutputSafetyState::AdapterUncertain.to_u8()
            }),
            "DeadlineExceeded 2s 内未升 AdapterUncertain"
        );
        kit.handle.request_stop();
        kit.handle.join().unwrap();

        let s = OutputSafetyState::from_u8(kit.ctrl.safety_state.load(Ordering::Acquire));
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
}
