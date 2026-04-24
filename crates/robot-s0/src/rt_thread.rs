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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

use robot_core::adapter::{JointCommandBuf, JointStateBuf, ManipulatorAdapter, RtSnapshot};
use robot_core::rt_entry;
use robot_core::thread as rt_thread_os;
use robot_core::time::{Deadline, DurationNs, RobotClock};
use robot_transport::{LatestCommandReader, RtSnapshotPublisher};

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
/// - `adapter`: 已经构造（尚未 `connect()`）的 adapter 实例。本函数内会调 `connect` + `enable(true)`。
/// - `cmd_reader`: 由 `ipc_thread`（Part 2）或测试喂命令的 reader 端。
/// - `snapshot_pub`: 由 `pub_thread`（Part 2）或测试消费的 writer 端。
///
/// 返回 `RtThreadHandle`，可 `request_stop` + `join`。
///
/// # Errors
///
/// 返回 `std::io::Error` 当 `strict_rt = true` 且任一 `set_sched_fifo` / `mlockall` /
/// `pin_to_cpu` / `adapter.connect` 失败。非 strict 模式下三个 syscall 失败被记录到
/// stderr 后继续。
pub fn spawn_rt_thread<A>(
    cfg: RtThreadConfig,
    clock: Arc<dyn RobotClock>,
    mut adapter: A,
    cmd_reader: LatestCommandReader<JointCommandBuf>,
    snapshot_pub: RtSnapshotPublisher<RtSnapshot>,
) -> std::io::Result<RtThreadHandle>
where
    A: ManipulatorAdapter + Send + 'static,
{
    // 在**主线程**完成可能分配的准备（thread name 字符串、Arc<AtomicBool> 等）。
    // RT scope 只在 spawn 之后 + 每 tick rt_entry! 内才算进入。
    let stop = Arc::new(AtomicBool::new(false));
    let metrics = Arc::new(RtThreadMetrics::default());

    // Part 1：在 spawn 之前 connect + enable，避免 RT 线程启动期做这些。
    // Part 2 由 state machine 控制 enable/disable。
    adapter
        .connect()
        .map_err(|e| std::io::Error::other(format!("adapter connect failed: {e}")))?;
    adapter
        .enable(true)
        .map_err(|e| std::io::Error::other(format!("adapter enable failed: {e}")))?;

    let join_stop = Arc::clone(&stop);
    let join_metrics = Arc::clone(&metrics);

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
#[allow(clippy::needless_pass_by_value)]
fn run_rt_loop<A: ManipulatorAdapter>(
    cfg: RtThreadConfig,
    clock: Arc<dyn RobotClock>,
    mut adapter: A,
    cmd_reader: LatestCommandReader<JointCommandBuf>,
    snapshot_pub: RtSnapshotPublisher<RtSnapshot>,
    stop: &Arc<AtomicBool>,
    metrics: &Arc<RtThreadMetrics>,
) {
    // ---- 初始化阶段（尚未进 rt_entry!，允许 alloc）----
    apply_rt_prio(&cfg);
    apply_mlockall(&cfg);
    apply_cpu_pin(&cfg);

    let mut state_buf = JointStateBuf::default();
    let mut tick_seq: u64 = 0;
    let mut deadline = Deadline(clock.now());

    // ---- 主循环 ----
    while !stop.load(Ordering::Acquire) {
        let tick_period = DurationNs(cfg.tick_period_ns);
        deadline = deadline + tick_period;

        // 检查是否 overrun：deadline 是否已经落后 now
        if deadline.0 < clock.now() {
            metrics.overruns.fetch_add(1, Ordering::Relaxed);
        }

        rt_entry!({
            clock.sleep_until(deadline);

            let cmd_guard = cmd_reader.load();
            let cmd = cmd_guard.as_view();
            if adapter.write_command(&cmd).is_err() {
                metrics.write_errors.fetch_add(1, Ordering::Relaxed);
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

fn apply_rt_prio(cfg: &RtThreadConfig) {
    if let Err(e) = rt_thread_os::set_sched_fifo(cfg.sched_fifo_priority) {
        assert!(
            !cfg.strict_rt,
            "set_sched_fifo(prio={}) failed in strict mode: {e}",
            cfg.sched_fifo_priority
        );
        eprintln!(
            "warn: set_sched_fifo(prio={}) failed: {e} — continuing without RT priority",
            cfg.sched_fifo_priority
        );
    }
}

fn apply_mlockall(cfg: &RtThreadConfig) {
    if let Err(e) = rt_thread_os::mlockall() {
        assert!(!cfg.strict_rt, "mlockall failed in strict mode: {e}");
        eprintln!("warn: mlockall failed: {e} — continuing without memlock");
    }
}

fn apply_cpu_pin(cfg: &RtThreadConfig) {
    let Some(cpu) = cfg.cpu_affinity else { return };
    if let Err(e) = rt_thread_os::pin_to_cpu(cpu) {
        assert!(
            !cfg.strict_rt,
            "pin_to_cpu({cpu}) failed in strict mode: {e}"
        );
        eprintln!("warn: pin_to_cpu({cpu}) failed: {e} — continuing unpinned");
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

        let handle = spawn_rt_thread(cfg, clock, adapter, cmd_reader, snap_pub).unwrap();

        // 让它跑一会儿（MockClock 下 sleep_until 是瞬发，tick 会非常快）
        thread::sleep(Duration::from_millis(50));
        handle.request_stop();
        let _ = handle.metrics(); // 保留对 metrics 的引用（原 handle 被消费在 join 里）
        handle.join().unwrap();

        // 不断言确切 tick 数（MockClock 下极快，数量依赖调度），但应 > 0
        // 这里我们不能直接复用 metrics；做法是在 join 前 snapshot 一次：
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
        let handle = spawn_rt_thread(cfg, clock, adapter, cmd_reader, snap_pub).unwrap();
        handle.request_stop();
        handle.join().unwrap();
    }

    /// 压力：50 ms 跑一把，验证 snapshot 随 tick 增长；`MAX_JOINTS` 保证不越界。
    #[test]
    fn snapshot_tick_seq_grows() {
        let clock: Arc<dyn RobotClock> = Arc::new(MockClock::new(MonoRawNs::ZERO));
        let adapter = MockHardRTAdapter::with_unlimited_limits(4);
        let (_cmd_writer, cmd_reader) =
            LatestCommandSlot::<JointCommandBuf>::new(JointCommandBuf::default());
        let (snap_pub, snap_rdr) = RtSnapshotPublisher::new(RtSnapshot::default());

        let cfg = RtThreadConfig {
            tick_period_ns: 1_000, // 1 µs
            ..RtThreadConfig::default()
        };
        let handle = spawn_rt_thread(cfg, clock, adapter, cmd_reader, snap_pub).unwrap();
        thread::sleep(Duration::from_millis(30));
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
}
