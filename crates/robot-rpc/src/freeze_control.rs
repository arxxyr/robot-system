//! Test-only freeze control thread（gateway；v0.5 候选 E 段(8)，Spec P.2.7
//! R5-5 / R6-5 wire 契约）。订阅 test-only iceoryx2 topic
//! `/<robot_id>/gateway/test/freeze`，把 smoke driver publish 的
//! [`TestFreezeCommandV2`] 路由到对应 worker 的 `freeze_until_ns` 原子。
//!
//! ⚠️ **test-only**：整 module 由 `cfg(any(test, feature = "test-freeze-hook"))`
//! 守门，**不进生产 release**。
//!
//! ## worker 槽位（R6-5）
//!
//! | kind | index | park 点 |
//! |---|---|---|
//! | `RrIo(i)` | `0..6`，顺序 = `run_gateway_with_rr_io` spawn 序：clear_estop / task_invoke / task_cancel / task_status / clear_error / set_enabled | `rr_io_thread::run_loop` 顶端 |
//! | `EstopPub` | — | `estop_pub_thread` loop 顶端 |
//! | `StatePool(i)` | `0..hardware 数`（**per-hardware 一线程**，无索引单变体冻不到指定线程） | `state_pool` 线程 loop 顶端 |
//! | `TonicProbe` | — | probe task 周期前（`tokio::time::sleep` 分片等待） |
//!
//! `target_pid` 精确匹配（history 重放防护）/ 非法 kind·index → warn + 忽略，
//! 与 S1 `freeze_control` 同款。

#![cfg(any(test, feature = "test-freeze-hook"))]
#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use iceoryx2::prelude::{NodeBuilder, ServiceName, ZeroCopySend};
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::RobotClock;

use crate::estop_pub_thread::EStopPubMetrics;
use crate::rr_io_thread::RrIoMetrics;
use crate::state_pool::StatePoolThreadMetrics;
use crate::systemd_probe::TonicProbeMetrics;

/// gateway rr_io 线程数（`RrIo(index)` 合法域 `0..RR_IO_COUNT`）。
pub const RR_IO_COUNT: usize = 6;

/// wire `kind` 编码（`#[repr(u8)]`；数值锁定）。
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum WorkerKindCode {
    RrIo = 0,
    EstopPub = 1,
    StatePool = 2,
    TonicProbe = 3,
}

/// 受 freeze hook 控制的 gateway worker。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum WorkerKind {
    /// 第 `index` 个 rr_io 线程（`0..RR_IO_COUNT`）。
    RrIo(u8),
    /// estop publish 线程。
    EstopPub,
    /// 第 `index` 个 state pool 线程（per-hardware；上界 = 配置 hardware 数，
    /// 由路由期按 context 校验）。
    StatePool(u8),
    /// tonic loopback probe task。
    TonicProbe,
}

impl WorkerKind {
    /// wire `(kind, index)` → enum；`RrIo` index ≥ 6 / 未知 kind 返 `None`。
    /// `StatePool` 上界需 context（见 [`handle_command`]）。
    #[must_use]
    pub fn from_wire(kind: u8, index: u8) -> Option<Self> {
        match kind {
            0 if usize::from(index) < RR_IO_COUNT => Some(Self::RrIo(index)),
            1 => Some(Self::EstopPub),
            2 => Some(Self::StatePool(index)),
            3 => Some(Self::TonicProbe),
            _ => None,
        }
    }

    /// enum → wire `(kind, index)`。
    #[must_use]
    pub fn to_wire(self) -> (u8, u8) {
        match self {
            Self::RrIo(i) => (WorkerKindCode::RrIo as u8, i),
            Self::EstopPub => (WorkerKindCode::EstopPub as u8, 0),
            Self::StatePool(i) => (WorkerKindCode::StatePool as u8, i),
            Self::TonicProbe => (WorkerKindCode::TonicProbe as u8, 0),
        }
    }
}

/// Test-only freeze 命令 wire format V2（与 S1 同布局；各 daemon crate 各自
/// cfg-gated 导出，smoke driver 分别 import）。16 字节、8 对齐。
#[repr(C)]
#[derive(Debug, Copy, Clone, ZeroCopySend)]
pub struct TestFreezeCommandV2 {
    /// 目标 daemon pid；不等于 `std::process::id()` → 忽略。
    pub target_pid: u32,
    /// [`WorkerKindCode`] 编码。
    pub kind: u8,
    /// 槽位（`RrIo` / `StatePool`）。
    pub index: u8,
    /// 对齐 padding；不读不写。
    #[allow(dead_code)]
    pub pad: [u8; 2],
    /// freeze 时长（毫秒）；0 = 清除。
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

/// gateway test-only freeze topic 路径。
#[must_use]
pub fn freeze_topic(robot_id: &str) -> String {
    format!("/{robot_id}/gateway/test/freeze")
}

/// worker metrics 引用 + clock + robot_id 的容器。
pub struct FreezeControlContext {
    pub robot_id: String,
    pub clock: Arc<dyn RobotClock>,
    /// 顺序 = spawn 序（长度 == [`RR_IO_COUNT`]）。
    pub rr_io: Vec<Arc<RrIoMetrics>>,
    pub estop_pub: Arc<EStopPubMetrics>,
    /// 顺序 = config `[[hardware]]` 序（`StatePool(i)` 上界）。
    pub state_pool: Vec<Arc<StatePoolThreadMetrics>>,
    pub tonic_probe: Arc<TonicProbeMetrics>,
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

/// 起 test-only freeze control thread；iceoryx2 建服务失败**只 log 退出**。
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
        .name("gw_freeze_control".into())
        .spawn(move || run_loop(&ctx, &join_stop))?;
    Ok(FreezeControlHandle { join, stop })
}

fn run_loop(ctx: &FreezeControlContext, stop: &AtomicBool) {
    let service_path = freeze_topic(&ctx.robot_id);
    let node = match NodeBuilder::new().create::<IxService>() {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(?e, %service_path, "gw freeze_control: NodeBuilder::create failed; thread exits");
            return;
        }
    };
    let service_name = match ServiceName::new(&service_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(?e, %service_path, "gw freeze_control: invalid ServiceName; thread exits");
            return;
        }
    };
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
            tracing::error!(?e, %service_path, "gw freeze_control: open_or_create failed; thread exits");
            return;
        }
    };
    let subscriber = match service.subscriber_builder().create() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(?e, %service_path, "gw freeze_control: subscriber create failed; thread exits");
            return;
        }
    };
    tracing::info!(%service_path, pid = std::process::id(), "gw freeze_control thread ready");

    while !stop.load(Ordering::Acquire) {
        match subscriber.receive() {
            Ok(Some(sample)) => handle_command(ctx, *sample.payload()),
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(e) => {
                tracing::warn!(?e, "gw freeze_control: receive failed; backing off 10ms");
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
    tracing::info!(%service_path, "gw freeze_control thread stopped");
}

/// 解析 + 路由：pid 精确匹配 → kind/index 合法（`StatePool` 按 context 上界）
/// → 计算 absolute ns → 写对应 worker 原子。
fn handle_command(ctx: &FreezeControlContext, cmd: TestFreezeCommandV2) {
    let my_pid = std::process::id();
    if cmd.target_pid != my_pid {
        tracing::warn!(
            target_pid = cmd.target_pid,
            my_pid,
            kind = cmd.kind,
            index = cmd.index,
            "gw freeze_control: stale freeze ignored (pid mismatch)"
        );
        return;
    }
    let Some(worker) = WorkerKind::from_wire(cmd.kind, cmd.index) else {
        tracing::warn!(
            kind = cmd.kind,
            index = cmd.index,
            "gw freeze_control: invalid worker kind/index; dropping cmd"
        );
        return;
    };
    let until_ns = if cmd.duration_ms == 0 {
        0
    } else {
        ctx.clock
            .now()
            .0
            .saturating_add(cmd.duration_ms.saturating_mul(1_000_000))
    };
    let target = match worker {
        WorkerKind::RrIo(i) => ctx.rr_io.get(usize::from(i)).map(|m| &m.freeze_until_ns),
        WorkerKind::EstopPub => Some(&ctx.estop_pub.freeze_until_ns),
        WorkerKind::StatePool(i) => ctx
            .state_pool
            .get(usize::from(i))
            .map(|m| &m.freeze_until_ns),
        WorkerKind::TonicProbe => Some(&ctx.tonic_probe.freeze_until_ns),
    };
    let Some(slot) = target else {
        tracing::warn!(
            ?worker,
            rr_io = ctx.rr_io.len(),
            state_pool = ctx.state_pool.len(),
            "gw freeze_control: worker slot out of range; dropping cmd"
        );
        return;
    };
    slot.store(until_ns, Ordering::Release);
    tracing::info!(
        ?worker,
        duration_ms = cmd.duration_ms,
        until_ns,
        "gw freeze_control: applied"
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
            "gw_freeze_ctl_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn make_ctx(robot_id: String, clock: Arc<dyn RobotClock>) -> FreezeControlContext {
        FreezeControlContext {
            robot_id,
            clock,
            rr_io: (0..RR_IO_COUNT)
                .map(|_| Arc::new(RrIoMetrics::default()))
                .collect(),
            estop_pub: Arc::new(EStopPubMetrics::default()),
            state_pool: vec![
                Arc::new(StatePoolThreadMetrics::new("arm0")),
                Arc::new(StatePoolThreadMetrics::new("arm1")),
            ],
            tonic_probe: Arc::new(TonicProbeMetrics::default()),
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
        publisher
            .loan_uninit()
            .unwrap()
            .write_payload(cmd)
            .send()
            .unwrap();
    }

    #[test]
    fn wire_round_trip_and_bounds() {
        for w in [
            WorkerKind::RrIo(0),
            WorkerKind::RrIo(5),
            WorkerKind::EstopPub,
            WorkerKind::StatePool(1),
            WorkerKind::TonicProbe,
        ] {
            let (k, i) = w.to_wire();
            assert_eq!(WorkerKind::from_wire(k, i), Some(w));
        }
        assert_eq!(WorkerKind::from_wire(0, 6), None, "RrIo index ≥ 6 非法");
        assert_eq!(WorkerKind::from_wire(7, 0), None);
        assert_eq!(std::mem::size_of::<TestFreezeCommandV2>(), 16);
    }

    #[test]
    fn routes_each_worker_kind_including_state_pool_slot() {
        let robot_id = fresh_robot_id();
        let mock = Arc::new(MockClock::new(MonoRawNs(10_000_000)));
        let clock: Arc<dyn RobotClock> = Arc::clone(&mock) as Arc<dyn RobotClock>;
        let ctx = make_ctx(robot_id.clone(), clock);
        let rr: Vec<Arc<RrIoMetrics>> = ctx.rr_io.iter().map(Arc::clone).collect();
        let estop = Arc::clone(&ctx.estop_pub);
        let pool: Vec<Arc<StatePoolThreadMetrics>> =
            ctx.state_pool.iter().map(Arc::clone).collect();
        let probe = Arc::clone(&ctx.tonic_probe);
        let handle = spawn_freeze_control_thread(ctx).unwrap();
        thread::sleep(Duration::from_millis(50));
        let publisher = open_publisher(&robot_id);
        let pid = std::process::id();

        publish(
            &publisher,
            TestFreezeCommandV2::new(pid, WorkerKind::RrIo(5), 50),
        );
        assert!(wait_for(Duration::from_secs(2), || {
            rr[5].freeze_until_ns.load(Ordering::Acquire) == 60_000_000
        }));
        assert_eq!(rr[0].freeze_until_ns.load(Ordering::Acquire), 0);
        publish(
            &publisher,
            TestFreezeCommandV2::new(pid, WorkerKind::EstopPub, 20),
        );
        assert!(wait_for(Duration::from_secs(2), || {
            estop.freeze_until_ns.load(Ordering::Acquire) == 30_000_000
        }));
        // R6-5：StatePool(1) 只冻第 1 条线程，StatePool(0) 不动。
        publish(
            &publisher,
            TestFreezeCommandV2::new(pid, WorkerKind::StatePool(1), 40),
        );
        assert!(wait_for(Duration::from_secs(2), || {
            pool[1].freeze_until_ns.load(Ordering::Acquire) == 50_000_000
        }));
        assert_eq!(pool[0].freeze_until_ns.load(Ordering::Acquire), 0);
        publish(
            &publisher,
            TestFreezeCommandV2::new(pid, WorkerKind::TonicProbe, 60),
        );
        assert!(wait_for(Duration::from_secs(2), || {
            probe.freeze_until_ns.load(Ordering::Acquire) == 70_000_000
        }));
        // StatePool index ≥ 配置数 → 忽略；duration 0 → 清除。
        publish(
            &publisher,
            TestFreezeCommandV2::new(pid, WorkerKind::StatePool(2), 99),
        );
        publish(
            &publisher,
            TestFreezeCommandV2::new(pid, WorkerKind::RrIo(5), 0),
        );
        assert!(wait_for(Duration::from_secs(2), || {
            rr[5].freeze_until_ns.load(Ordering::Acquire) == 0
        }));
        assert!(
            pool.iter()
                .all(|m| m.freeze_until_ns.load(Ordering::Acquire) != 99)
        );

        handle.request_stop();
        handle.join().unwrap();
    }

    #[test]
    fn pid_mismatch_and_invalid_kind_are_ignored() {
        let robot_id = fresh_robot_id();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let ctx = make_ctx(robot_id.clone(), clock);
        let rr: Vec<Arc<RrIoMetrics>> = ctx.rr_io.iter().map(Arc::clone).collect();
        let estop = Arc::clone(&ctx.estop_pub);
        let probe = Arc::clone(&ctx.tonic_probe);
        let handle = spawn_freeze_control_thread(ctx).unwrap();
        thread::sleep(Duration::from_millis(50));
        let publisher = open_publisher(&robot_id);

        publish(
            &publisher,
            TestFreezeCommandV2::new(
                std::process::id().wrapping_add(1),
                WorkerKind::EstopPub,
                500,
            ),
        );
        publish(
            &publisher,
            TestFreezeCommandV2 {
                target_pid: std::process::id(),
                kind: 42,
                index: 0,
                pad: [0; 2],
                duration_ms: 500,
            },
        );
        thread::sleep(Duration::from_millis(100));
        assert!(
            rr.iter()
                .all(|m| m.freeze_until_ns.load(Ordering::Acquire) == 0)
        );
        assert_eq!(estop.freeze_until_ns.load(Ordering::Acquire), 0);
        assert_eq!(probe.freeze_until_ns.load(Ordering::Acquire), 0);

        handle.request_stop();
        handle.join().unwrap();
    }
}
