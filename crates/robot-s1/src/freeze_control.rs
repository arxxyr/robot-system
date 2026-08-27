//! Test-only freeze control thread（S1；v0.5 候选 E 段(8)，Spec P.2.7 R5-5 /
//! R6-5 wire 契约）。订阅 test-only iceoryx2 topic `/<robot_id>/s1/test/freeze`，
//! 把 smoke driver publish 的 [`TestFreezeCommandV2`] 路由到对应 worker 的
//! `freeze_until_ns` 原子（[`crate::liveness::freeze_check`] 在 loop 顶端 park）。
//!
//! ⚠️ **test-only**：整 module 由 `cfg(any(test, feature = "test-freeze-hook"))`
//! 守门，**不进生产 release**（沿 S0 `freeze_control.rs` 硬边界）。
//!
//! ## 与 S0 先例的差异（Spec R5-5 / R6-5 锁死）
//!
//! - **独立 topic 路径** `/<robot_id>/s1/test/freeze`（S0 = `/<robot_id>/test/
//!   freeze`、gateway = `/<robot_id>/gateway/test/freeze`）：S0 QoS
//!   `max_subscribers(1)` 下同 robot_id 三进程共用会使后两个 subscriber 启动
//!   失败，故各持独立 service；QoS 沿 S0 字面。
//! - **精确 POD V2**：`{ target_pid, kind, index, pad, duration_ms }`（Spec 写
//!   `_pad`；clippy `pub_underscore_fields` 禁 pub `_` 前缀，改 `pad`，wire 布局不变）——
//!   既有 `{kind, duration_ms}` 编码不了带槽位 worker（4 个 rr server）。
//! - **`target_pid` 精确匹配**：`history_size(1)` 会把 PID0 的命令重放给重启
//!   后 PID1 的 subscriber；`target_pid != std::process::id()` → warn
//!   `stale freeze ignored (pid mismatch)` + 忽略（不 freeze 不 panic）。
//! - **非法值**：kind 未知 / `RrServer` index ≥ 4 → warn + 忽略。
//!
//! ## 不进 watchdog observer
//!
//! 本 control thread 是测试注入器，**不**是 daemon worker：不挂
//! `LivenessObserver`；daemon shutdown 单独 `request_stop` + `join`。

#![cfg(any(test, feature = "test-freeze-hook"))]
#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use iceoryx2::prelude::{NodeBuilder, ServiceName, ZeroCopySend};
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::RobotClock;

use crate::liveness::TickLoopMetrics;
use crate::rr_server::RrServerMetrics;
use crate::state_cache::StateCacheLiveness;

/// S1 rr server 线程数（`RrServer(index)` 合法域 `0..RR_SERVER_COUNT`；顺序 =
/// `S1RrServerHandles` 字段序：task_invoke / task_cancel / task_status /
/// clear_error）。
pub const RR_SERVER_COUNT: usize = 4;

/// wire `kind` 编码（`#[repr(u8)]`；数值锁定，smoke driver 与 daemon 必须一致）。
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum WorkerKindCode {
    Tick = 0,
    HeartbeatPub = 1,
    RrServer = 2,
    StateCache = 3,
}

/// 受 freeze hook 控制的 S1 worker。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum WorkerKind {
    /// `run_action_tick_loop` 顶端。
    Tick,
    /// heartbeat publish 前（与 Tick 同线程，仅 park 点不同）。
    HeartbeatPub,
    /// 第 `index` 个 rr server 线程（`0..RR_SERVER_COUNT`）。
    RrServer(u8),
    /// state cache 线程。
    StateCache,
}

impl WorkerKind {
    /// wire `(kind, index)` → enum；非法返 `None`（调用方 warn + 忽略）。
    #[must_use]
    pub fn from_wire(kind: u8, index: u8) -> Option<Self> {
        match kind {
            0 => Some(Self::Tick),
            1 => Some(Self::HeartbeatPub),
            2 if usize::from(index) < RR_SERVER_COUNT => Some(Self::RrServer(index)),
            3 => Some(Self::StateCache),
            _ => None,
        }
    }

    /// enum → wire `(kind, index)`（非槽位 kind 的 index 固定 0）。
    #[must_use]
    pub fn to_wire(self) -> (u8, u8) {
        match self {
            Self::Tick => (WorkerKindCode::Tick as u8, 0),
            Self::HeartbeatPub => (WorkerKindCode::HeartbeatPub as u8, 0),
            Self::RrServer(i) => (WorkerKindCode::RrServer as u8, i),
            Self::StateCache => (WorkerKindCode::StateCache as u8, 0),
        }
    }
}

/// Test-only freeze 命令 wire format V2（POD + `ZeroCopySend`；**不**进
/// `robot_msg` / frozen schema）。16 字节、8 对齐。
#[repr(C)]
#[derive(Debug, Copy, Clone, ZeroCopySend)]
pub struct TestFreezeCommandV2 {
    /// 目标 daemon pid；不等于 `std::process::id()` → 忽略（history 重放防护）。
    pub target_pid: u32,
    /// [`WorkerKindCode`] 编码。
    pub kind: u8,
    /// 槽位（仅 `RrServer`）。
    pub index: u8,
    /// 对齐 padding；不读不写（POD wire 跨进程显式声明）。
    #[allow(dead_code)]
    pub pad: [u8; 2],
    /// freeze 时长（毫秒）；0 = 清除（store 0，hot path 恢复单次 load）。
    pub duration_ms: u64,
}

impl TestFreezeCommandV2 {
    /// 构造（driver 侧便捷）。
    #[must_use]
    pub fn new(target_pid: u32, worker: WorkerKind, duration_ms: u64) -> Self {
        let (kind, index) = worker.to_wire();
        Self {
            target_pid,
            kind,
            index,
            pad: [0; 2],
            duration_ms,
        }
    }
}

/// S1 test-only freeze topic 路径。
#[must_use]
pub fn freeze_topic(robot_id: &str) -> String {
    format!("/{robot_id}/s1/test/freeze")
}

/// worker metrics 引用 + clock + robot_id 的容器。
pub struct FreezeControlContext {
    pub robot_id: String,
    pub clock: Arc<dyn RobotClock>,
    pub tick_loop: Arc<TickLoopMetrics>,
    /// 顺序 = `S1RrServerHandles` 字段序（长度必须 == [`RR_SERVER_COUNT`]）。
    pub rr_servers: Vec<Arc<RrServerMetrics>>,
    pub state_cache: Arc<StateCacheLiveness>,
}

/// control thread 句柄：`request_stop` → `join`。
pub struct FreezeControlHandle {
    join: JoinHandle<()>,
    stop: Arc<AtomicBool>,
}

impl FreezeControlHandle {
    /// 通知 control thread 退出 receive loop。
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// 等待 control thread 退出。
    ///
    /// # Errors
    ///
    /// control thread panic → `Err(payload)`。
    pub fn join(self) -> thread::Result<()> {
        self.join.join()
    }
}

/// 起 test-only freeze control thread；iceoryx2 建服务失败**只 log 退出**，
/// 不杀 daemon。
///
/// # Errors
///
/// `thread::Builder::spawn` 失败。
pub fn spawn_freeze_control_thread(
    ctx: FreezeControlContext,
) -> std::io::Result<FreezeControlHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let join_stop = Arc::clone(&stop);
    let join = thread::Builder::new()
        .name("s1_freeze_control".into())
        .spawn(move || run_loop(&ctx, &join_stop))?;
    Ok(FreezeControlHandle { join, stop })
}

fn run_loop(ctx: &FreezeControlContext, stop: &AtomicBool) {
    let service_path = freeze_topic(&ctx.robot_id);
    let node = match NodeBuilder::new().create::<IxService>() {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(?e, %service_path, "s1 freeze_control: NodeBuilder::create failed; thread exits");
            return;
        }
    };
    let service_name = match ServiceName::new(&service_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(?e, %service_path, "s1 freeze_control: invalid ServiceName; thread exits");
            return;
        }
    };
    // QoS 沿 S0 字面：1 pub / 1 sub / history 1 / buffer 2 / safe_overflow。
    let service = match node
        .service_builder(&service_name)
        .publish_subscribe::<TestFreezeCommandV2>()
        .max_publishers(1)
        .max_subscribers(1)
        .history_size(1)
        .subscriber_max_buffer_size(2)
        .enable_safe_overflow(true)
        .open_or_create()
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(?e, %service_path, "s1 freeze_control: open_or_create failed; thread exits");
            return;
        }
    };
    let subscriber = match service.subscriber_builder().create() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(?e, %service_path, "s1 freeze_control: subscriber create failed; thread exits");
            return;
        }
    };
    tracing::info!(%service_path, pid = std::process::id(), "s1 freeze_control thread ready");

    while !stop.load(Ordering::Acquire) {
        match subscriber.receive() {
            Ok(Some(sample)) => handle_command(ctx, *sample.payload()),
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(e) => {
                tracing::warn!(?e, "s1 freeze_control: receive failed; backing off 10ms");
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
    tracing::info!(%service_path, "s1 freeze_control thread stopped");
}

/// 解析 + 路由：pid 精确匹配 → kind/index 合法 → 计算 absolute ns（daemon 内
/// clock）→ 写对应 worker 原子。
fn handle_command(ctx: &FreezeControlContext, cmd: TestFreezeCommandV2) {
    let my_pid = std::process::id();
    if cmd.target_pid != my_pid {
        tracing::warn!(
            target_pid = cmd.target_pid,
            my_pid,
            kind = cmd.kind,
            index = cmd.index,
            "s1 freeze_control: stale freeze ignored (pid mismatch)"
        );
        return;
    }
    let Some(worker) = WorkerKind::from_wire(cmd.kind, cmd.index) else {
        tracing::warn!(
            kind = cmd.kind,
            index = cmd.index,
            "s1 freeze_control: invalid worker kind/index; dropping cmd"
        );
        return;
    };
    let until_ns = if cmd.duration_ms == 0 {
        0
    } else {
        let now_ns = ctx.clock.now().0;
        now_ns.saturating_add(cmd.duration_ms.saturating_mul(1_000_000))
    };
    match worker {
        WorkerKind::Tick => ctx
            .tick_loop
            .freeze_until_ns_tick
            .store(until_ns, Ordering::Release),
        WorkerKind::HeartbeatPub => ctx
            .tick_loop
            .freeze_until_ns_heartbeat
            .store(until_ns, Ordering::Release),
        WorkerKind::RrServer(i) => {
            let Some(m) = ctx.rr_servers.get(usize::from(i)) else {
                tracing::warn!(
                    index = i,
                    have = ctx.rr_servers.len(),
                    "s1 freeze_control: rr server slot not wired; dropping cmd"
                );
                return;
            };
            m.freeze_until_ns.store(until_ns, Ordering::Release);
        }
        WorkerKind::StateCache => ctx
            .state_cache
            .freeze_until_ns
            .store(until_ns, Ordering::Release),
    }
    tracing::info!(
        ?worker,
        duration_ms = cmd.duration_ms,
        until_ns,
        "s1 freeze_control: applied"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use robot_core::time::{MockClock, MonoRawNs, SystemClock};
    use std::sync::atomic::AtomicU32;

    fn fresh_robot_id() -> String {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        format!(
            "s1_freeze_ctl_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn make_ctx(robot_id: String, clock: Arc<dyn RobotClock>) -> FreezeControlContext {
        FreezeControlContext {
            robot_id,
            clock,
            tick_loop: Arc::new(TickLoopMetrics::default()),
            rr_servers: (0..RR_SERVER_COUNT)
                .map(|_| Arc::new(RrServerMetrics::default()))
                .collect(),
            state_cache: Arc::new(StateCacheLiveness::default()),
        }
    }

    fn wait_for<F: FnMut() -> bool>(max: Duration, mut pred: F) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < max {
            if pred() {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        false
    }

    /// node leak 必须在 service / publisher 创建之前（沿 S0 测试注释）。
    fn open_publisher(
        robot_id: &str,
    ) -> iceoryx2::port::publisher::Publisher<IxService, TestFreezeCommandV2, ()> {
        let node: &'static iceoryx2::prelude::Node<IxService> =
            Box::leak(Box::new(NodeBuilder::new().create::<IxService>().unwrap()));
        let service_name = ServiceName::new(&freeze_topic(robot_id)).unwrap();
        let service = node
            .service_builder(&service_name)
            .publish_subscribe::<TestFreezeCommandV2>()
            .max_publishers(1)
            .max_subscribers(1)
            .history_size(1)
            .subscriber_max_buffer_size(2)
            .enable_safe_overflow(true)
            .open_or_create()
            .unwrap();
        service.publisher_builder().create().unwrap()
    }

    fn publish(
        publisher: &iceoryx2::port::publisher::Publisher<IxService, TestFreezeCommandV2, ()>,
        cmd: TestFreezeCommandV2,
    ) {
        let sample = publisher.loan_uninit().unwrap();
        sample.write_payload(cmd).send().unwrap();
    }

    #[test]
    fn wire_round_trip_and_bounds() {
        for w in [
            WorkerKind::Tick,
            WorkerKind::HeartbeatPub,
            WorkerKind::RrServer(0),
            WorkerKind::RrServer(3),
            WorkerKind::StateCache,
        ] {
            let (k, i) = w.to_wire();
            assert_eq!(WorkerKind::from_wire(k, i), Some(w));
        }
        assert_eq!(WorkerKind::from_wire(2, 4), None, "RrServer index ≥ 4 非法");
        assert_eq!(WorkerKind::from_wire(9, 0), None);
        assert_eq!(std::mem::size_of::<TestFreezeCommandV2>(), 16);
        assert_eq!(std::mem::align_of::<TestFreezeCommandV2>(), 8);
    }

    #[test]
    fn routes_each_worker_kind_with_matching_pid() {
        let robot_id = fresh_robot_id();
        let mock = Arc::new(MockClock::new(MonoRawNs(10_000_000)));
        let clock: Arc<dyn RobotClock> = Arc::clone(&mock) as Arc<dyn RobotClock>;
        let ctx = make_ctx(robot_id.clone(), clock);
        let tick = Arc::clone(&ctx.tick_loop);
        let rr: Vec<Arc<RrServerMetrics>> = ctx.rr_servers.iter().map(Arc::clone).collect();
        let cache = Arc::clone(&ctx.state_cache);
        let handle = spawn_freeze_control_thread(ctx).unwrap();
        thread::sleep(Duration::from_millis(50));
        let publisher = open_publisher(&robot_id);
        let pid = std::process::id();

        publish(
            &publisher,
            TestFreezeCommandV2::new(pid, WorkerKind::Tick, 50),
        );
        assert!(wait_for(Duration::from_secs(2), || {
            tick.freeze_until_ns_tick.load(Ordering::Acquire) == 60_000_000
        }));
        publish(
            &publisher,
            TestFreezeCommandV2::new(pid, WorkerKind::HeartbeatPub, 70),
        );
        assert!(wait_for(Duration::from_secs(2), || {
            tick.freeze_until_ns_heartbeat.load(Ordering::Acquire) == 80_000_000
        }));
        publish(
            &publisher,
            TestFreezeCommandV2::new(pid, WorkerKind::RrServer(2), 30),
        );
        assert!(wait_for(Duration::from_secs(2), || {
            rr[2].freeze_until_ns.load(Ordering::Acquire) == 40_000_000
        }));
        assert_eq!(rr[0].freeze_until_ns.load(Ordering::Acquire), 0);
        publish(
            &publisher,
            TestFreezeCommandV2::new(pid, WorkerKind::StateCache, 90),
        );
        assert!(wait_for(Duration::from_secs(2), || {
            cache.freeze_until_ns.load(Ordering::Acquire) == 100_000_000
        }));
        // duration 0 → store 0（清除）。
        publish(
            &publisher,
            TestFreezeCommandV2::new(pid, WorkerKind::Tick, 0),
        );
        assert!(wait_for(Duration::from_secs(2), || {
            tick.freeze_until_ns_tick.load(Ordering::Acquire) == 0
        }));

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn pid_mismatch_and_invalid_kind_are_ignored() {
        let robot_id = fresh_robot_id();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let ctx = make_ctx(robot_id.clone(), clock);
        let tick = Arc::clone(&ctx.tick_loop);
        let rr: Vec<Arc<RrServerMetrics>> = ctx.rr_servers.iter().map(Arc::clone).collect();
        let cache = Arc::clone(&ctx.state_cache);
        let handle = spawn_freeze_control_thread(ctx).unwrap();
        thread::sleep(Duration::from_millis(50));
        let publisher = open_publisher(&robot_id);

        // 错 pid（已死 PID0 语义）→ 忽略。
        publish(
            &publisher,
            TestFreezeCommandV2::new(std::process::id().wrapping_add(1), WorkerKind::Tick, 500),
        );
        // 非法 kind / index → 忽略。
        publish(
            &publisher,
            TestFreezeCommandV2 {
                target_pid: std::process::id(),
                kind: 99,
                index: 0,
                pad: [0; 2],
                duration_ms: 500,
            },
        );
        publish(
            &publisher,
            TestFreezeCommandV2 {
                target_pid: std::process::id(),
                kind: WorkerKindCode::RrServer as u8,
                index: 4,
                pad: [0; 2],
                duration_ms: 500,
            },
        );
        thread::sleep(Duration::from_millis(100));
        assert_eq!(tick.freeze_until_ns_tick.load(Ordering::Acquire), 0);
        assert_eq!(tick.freeze_until_ns_heartbeat.load(Ordering::Acquire), 0);
        assert!(
            rr.iter()
                .all(|m| m.freeze_until_ns.load(Ordering::Acquire) == 0)
        );
        assert_eq!(cache.freeze_until_ns.load(Ordering::Acquire), 0);

        handle.request_stop();
        handle.join().unwrap();
    }
}
