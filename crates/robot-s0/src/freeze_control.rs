//! Test-only freeze control thread（v0.3 A2-2 / `docs/handoffs/v0.3_entry.md`
//! §4.3 选项 A）。订阅 test-only iceoryx2 topic，把测试驱动 publish 的 freeze
//! 命令路由到对应 worker 的 [`crate::freeze_hook::freeze_check`] 原子。
//!
//! ⚠️ **test-only**：整 module 由 `cfg(any(test, feature = "test-freeze-hook"))`
//! 守门，**不进生产 release**（reviewer 拍板硬边界，详
//! `crates/robot-s0/Cargo.toml` test-freeze-hook feature 注释 +
//! [`crate::freeze_hook`] 模块 doc）。
//!
//! ## 跟生产 schema / 拓扑完全隔离
//!
//! - **不**走 `robot_msg` / `robot_transport` 任何抽象（`BodyMarker` /
//!   `TopicClass` / schema_id verifier 全不用）。直接 iceoryx2 raw API。
//! - [`TestFreezeCommand`] 是 cfg-gated POD，**只**存在 test-only 编译。它**不**
//!   在 `docs/00_design.md` §2.4 九类 topic / §5 RPC schema 任何位置；它跟
//!   `robot_msg` 的 `markers::*` 无关。
//! - service name `<robot_id>/test/freeze` 在 test-only 命名空间，跟生产
//!   `<robot_id>/s0/...` / `<robot_id>/s1/...` / `<robot_id>/events/...` 完
//!   全 isolation。生产 daemon 不创建 / 不订阅 / 不 publish 这条 topic。
//!
//! ## 不进 watchdog observer
//!
//! 本 control thread 是测试注入器，**不**是 daemon worker：
//! - 不参与 [`crate::systemd_notify::LivenessObserver`] `tick_count` 报告。
//! - watchdog 主循环不查它（reviewer 硬边界："control thread 不进 watchdog
//!   observer；它只是测试注入器"）。
//! - daemon graceful_stop 时单独 `request_stop` + `join`（跟 4 worker 平行）。
//!
//! ## 控制路径
//!
//! 1. daemon 启动期 `s0_daemon` 主线程构造 [`FreezeControlContext`]（5 worker
//!    metrics 引用 + clock + robot_id）→ [`spawn_freeze_control_thread`]。
//! 2. control thread 启动 iceoryx2 Node → service `<robot_id>/test/freeze`
//!    `publish_subscribe::<TestFreezeCommand>()` open_or_create → subscriber。
//! 3. loop 内 `subscriber.receive()`：
//!    - `Ok(Some(sample))` → 解析 `TestFreezeCommand { kind, duration_ms }`
//!      → [`handle_command`] 路由：
//!      - **daemon 内 monotonic clock 计算**
//!        `freeze_until_ns = clock.now().0 + duration_ms * 1_000_000`
//!        （reviewer 硬边界：外部不传 absolute ns）。
//!      - 写到对应 worker 的 `freeze_until_ns: AtomicU64`。
//!    - `Ok(None)` → sleep 2ms 再轮询。
//!    - `Err(_)` → log warn + sleep 10ms。
//! 4. graceful_stop：`stop` 原子 store(true) + thread join。

#![cfg(any(test, feature = "test-freeze-hook"))]
// 整 module 是 test-only。中文 doc 块里大量出现 `Ipc` / `HardRT` /
// `monotonic` / `graceful_stop` / `s0_daemon` 等 identifier-like 字串，跟生产
// 模块对 doc backtick 严格化的标准比，这里牺牲 doc-markdown 严格性换可读性。
// 行为代码不影响。
#![allow(clippy::doc_markdown)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use iceoryx2::prelude::{NodeBuilder, ServiceName, ZeroCopySend};
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::RobotClock;

use crate::io_thread::IoThreadMetrics;
use crate::ipc_thread::IpcThreadMetrics;
use crate::pub_thread::PubThreadMetrics;
use crate::serial_io_thread::SerialIoThreadMetrics;

#[cfg(target_os = "linux")]
use crate::rt_thread::RtThreadMetrics;

/// 受 freeze hook 控制的 worker 标识。**不**进 frozen schema；只用于
/// [`TestFreezeCommand`] wire format。
///
/// 数值（`#[repr(u8)]`）跟 `TestFreezeCommand::kind` 字段对应：
/// - `0` = `Ipc`、`1` = `Rt`、`2` = `SerialIo`、`3` = `Pub`、`4` = `Io`
///
/// 数值不可在 v0.3 内随意调整（测试驱动 binary 跟 daemon 必须一致）；A2 完成
/// 后下个 release 才能改。
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum WorkerKind {
    Ipc = 0,
    Rt = 1,
    SerialIo = 2,
    Pub = 3,
    Io = 4,
}

impl WorkerKind {
    /// 反序列化 wire 上 `u8` → enum。返 `None` 表非法 kind 数值；调用方应丢弃
    /// 非法 cmd 并 log warn。
    #[must_use]
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Ipc),
            1 => Some(Self::Rt),
            2 => Some(Self::SerialIo),
            3 => Some(Self::Pub),
            4 => Some(Self::Io),
            _ => None,
        }
    }
}

/// Test-only freeze 命令 wire format。POD + `ZeroCopySend` over iceoryx2。
///
/// **不**进 `robot_msg` / `robot_transport` / frozen schema（详 module doc）。
///
/// `kind`：[`WorkerKind::from_u8`] 解析。
/// `duration_ms`：freeze 时长（毫秒）。daemon 内 monotonic clock 计算 absolute
/// `freeze_until_ns`，外部 publisher **不传** absolute ns。
#[repr(C)]
#[derive(Debug, Copy, Clone, ZeroCopySend)]
pub struct TestFreezeCommand {
    /// `WorkerKind` 编码值（u8）。
    pub kind: u8,
    /// 对齐 padding（POD wire 跨进程必须显式声明，physical 4-byte 对齐）。
    /// 不读 / 不写，保留供未来扩展。
    #[allow(dead_code)]
    pub pad: [u8; 3],
    /// freeze 时长（毫秒）。0 = no-op（daemon 内仍计算，但写出去
    /// `until_ns == clock.now()`，下次 worker tick 看到 `now >= until` 立即
    /// 不 spin）。
    pub duration_ms: u32,
}

/// 5 worker metrics 引用 + clock + robot_id 的容器；spawn 时一次性传给
/// control thread。
///
/// `rt` / `serial_io` 二选一（按 daemon dispatch 路径）：
/// - HardRT 路径：`rt = Some(...)`、`serial_io = None`
/// - SerialRT 路径：`rt = None`、`serial_io = Some(...)`
///
/// 其它三个（`ipc` / `pub_thr` / `io`）是必填，跟 dispatch 路径无关。
pub struct FreezeControlContext {
    /// `<robot_id>/test/freeze` 的 robot_id 部分；跟生产 topic 命名空间共享。
    pub robot_id: String,
    /// daemon 主时钟；control thread 用它计算 `freeze_until_ns`。
    pub clock: Arc<dyn RobotClock>,
    /// `ipc_thread` metrics 引用（必填）。
    pub ipc: Arc<IpcThreadMetrics>,
    /// `rt_thread` metrics 引用（HardRT dispatch 路径）。`SerialRT` 路径置 `None`。
    #[cfg(target_os = "linux")]
    pub rt: Option<Arc<RtThreadMetrics>>,
    /// `serial_io_thread` metrics 引用（SerialRT dispatch 路径）。`HardRT` 路径
    /// 置 `None`。
    pub serial_io: Option<Arc<SerialIoThreadMetrics>>,
    /// `pub_thread` metrics 引用（必填）。
    pub pub_thr: Arc<PubThreadMetrics>,
    /// `io_thread` metrics 引用（必填）。
    pub io: Arc<IoThreadMetrics>,
}

/// freeze control thread 的控制 handle。daemon graceful_stop 时
/// [`Self::request_stop`] → [`Self::join`]。
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
    /// `JoinHandle::join` 的 `Err`（control thread panic）。
    pub fn join(self) -> thread::Result<()> {
        self.join.join()
    }
}

/// 启动 test-only freeze control thread。整 module cfg-gated，**生产 release
/// 不编译**。
///
/// control thread 内 iceoryx2 service 创建失败时**只 log error 退出**，**不**
/// 杀 daemon（test-only 路径不应影响生产）。
///
/// # Errors
///
/// `std::io::Error` 当 `thread::Builder::spawn` 失败（OS 资源耗尽）。
pub fn spawn_freeze_control_thread(
    ctx: FreezeControlContext,
) -> std::io::Result<FreezeControlHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let join_stop = Arc::clone(&stop);

    let join = thread::Builder::new()
        .name("s0_freeze_control".into())
        .spawn(move || {
            run_loop(&ctx, &join_stop);
        })?;

    Ok(FreezeControlHandle { join, stop })
}

fn run_loop(ctx: &FreezeControlContext, stop: &AtomicBool) {
    let service_path = format!("/{}/test/freeze", ctx.robot_id);

    let node = match NodeBuilder::new().create::<IxService>() {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(?e, %service_path, "freeze_control: NodeBuilder::create failed; thread exits");
            return;
        }
    };

    let service_name = match ServiceName::new(&service_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(?e, %service_path, "freeze_control: invalid ServiceName; thread exits");
            return;
        }
    };

    // test-only QoS：1 publisher + 1 subscriber + small buffer + safe_overflow
    // = 测试驱动 publish 永不阻塞，daemon 控制路由永远拿最新 cmd。**不**复用
    // 任何生产 TopicClass profile；本 service 完全独立。
    let service = match node
        .service_builder(&service_name)
        .publish_subscribe::<TestFreezeCommand>()
        .max_publishers(1)
        .max_subscribers(1)
        .history_size(1)
        .subscriber_max_buffer_size(2)
        .enable_safe_overflow(true)
        .open_or_create()
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(?e, %service_path, "freeze_control: open_or_create failed; thread exits");
            return;
        }
    };

    let subscriber = match service.subscriber_builder().create() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(?e, %service_path, "freeze_control: subscriber create failed; thread exits");
            return;
        }
    };

    tracing::info!(%service_path, "freeze_control thread ready");

    while !stop.load(Ordering::Acquire) {
        match subscriber.receive() {
            Ok(Some(sample)) => {
                handle_command(ctx, *sample.payload());
            }
            Ok(None) => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(e) => {
                tracing::warn!(?e, "freeze_control: receive failed; backing off 10ms");
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    tracing::info!(%service_path, "freeze_control thread stopped");
}

/// 解析 `TestFreezeCommand` + 路由到对应 worker 的 `freeze_until_ns`。
///
/// 用 `ctx.clock` 计算 absolute ns（reviewer 硬边界：daemon 内 clock，不接受
/// 外部 absolute ns）。
///
/// `cmd` 8 字节 POD，by-value（clippy::trivially_copy_pass_by_ref：8B 阈值
/// 内 by-value 比 by-ref 更便宜）。
fn handle_command(ctx: &FreezeControlContext, cmd: TestFreezeCommand) {
    let Some(kind) = WorkerKind::from_u8(cmd.kind) else {
        tracing::warn!(
            kind = cmd.kind,
            duration_ms = cmd.duration_ms,
            "freeze_control: invalid worker kind; dropping cmd"
        );
        return;
    };
    // **duration_ms == 0 ≡ no-op**：直接 store(0)，跟 [`crate::freeze_hook`]
    // "未冻结 hot path == 0" 口径完全对齐。否则 store(clock.now()) 虽然语义
    // 上不会 spin（now >= until 立即退出），但会让 worker tick loop 每次都
    // 多走一次 clock.now() 比较，跟"未冻结 hot path 仅 1 次 atomic load"的
    // 不变量背离。reviewer A2-2 非阻断备注（commit 17ce63c 复核）。
    let until_ns = if cmd.duration_ms == 0 {
        0
    } else {
        let now_ns = ctx.clock.now().0;
        let duration_ns = u64::from(cmd.duration_ms).saturating_mul(1_000_000);
        now_ns.saturating_add(duration_ns)
    };

    match kind {
        WorkerKind::Ipc => {
            ctx.ipc.freeze_until_ns.store(until_ns, Ordering::Release);
            tracing::info!(
                ?kind,
                duration_ms = cmd.duration_ms,
                until_ns,
                "freeze_control: applied"
            );
        }
        WorkerKind::Rt => {
            #[cfg(target_os = "linux")]
            if let Some(rt) = &ctx.rt {
                rt.freeze_until_ns.store(until_ns, Ordering::Release);
                tracing::info!(
                    ?kind,
                    duration_ms = cmd.duration_ms,
                    until_ns,
                    "freeze_control: applied"
                );
            } else {
                tracing::warn!(
                    ?kind,
                    "freeze_control: rt_thread not running on this dispatch path (likely SerialRT); dropping cmd"
                );
            }
            #[cfg(not(target_os = "linux"))]
            tracing::warn!(
                ?kind,
                "freeze_control: rt_thread cfg-gated to linux; dropping cmd"
            );
        }
        WorkerKind::SerialIo => {
            if let Some(serial) = &ctx.serial_io {
                serial.freeze_until_ns.store(until_ns, Ordering::Release);
                tracing::info!(
                    ?kind,
                    duration_ms = cmd.duration_ms,
                    until_ns,
                    "freeze_control: applied"
                );
            } else {
                tracing::warn!(
                    ?kind,
                    "freeze_control: serial_io_thread not running on this dispatch path (likely HardRT); dropping cmd"
                );
            }
        }
        WorkerKind::Pub => {
            ctx.pub_thr
                .freeze_until_ns
                .store(until_ns, Ordering::Release);
            tracing::info!(
                ?kind,
                duration_ms = cmd.duration_ms,
                until_ns,
                "freeze_control: applied"
            );
        }
        WorkerKind::Io => {
            ctx.io.freeze_until_ns.store(until_ns, Ordering::Release);
            tracing::info!(
                ?kind,
                duration_ms = cmd.duration_ms,
                until_ns,
                "freeze_control: applied"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use robot_core::time::SystemClock;

    /// 生成测试用 robot_id（带 PID + 单元化序列号），避免并行单测 service name 撞。
    fn fresh_robot_id() -> String {
        use std::sync::atomic::AtomicU32;
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        format!("freeze_ctl_{}_{}", std::process::id(), seq)
    }

    fn make_ctx(robot_id: String, clock: Arc<dyn RobotClock>) -> FreezeControlContext {
        FreezeControlContext {
            robot_id,
            clock,
            ipc: Arc::new(IpcThreadMetrics::default()),
            #[cfg(target_os = "linux")]
            rt: Some(Arc::new(RtThreadMetrics::default())),
            serial_io: Some(Arc::new(SerialIoThreadMetrics::default())),
            pub_thr: Arc::new(PubThreadMetrics::default()),
            io: Arc::new(IoThreadMetrics::default()),
        }
    }

    /// 等条件成真，最长 `max`；超时返 false（不 panic，方便组合断言信息）。
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

    /// publisher 端 helper：起一个 iceoryx2 publisher 给测试发 cmd。
    ///
    /// **node leak 必须在 service / publisher 创建之前**：iceoryx2 service 内
    /// 部持节点 ref，publisher 由 service 派生。如果在 publisher 已经创建之后
    /// move node 到 `Box::leak`，publisher 内部 ref 可能错位（runtime 没
    /// alloc，但 publish 端 receive 端配对不上 → subscriber 永远收不到）。
    fn open_publisher(
        robot_id: &str,
    ) -> iceoryx2::port::publisher::Publisher<IxService, TestFreezeCommand, ()> {
        let service_path = format!("/{robot_id}/test/freeze");
        let node: &'static iceoryx2::prelude::Node<IxService> =
            Box::leak(Box::new(NodeBuilder::new().create::<IxService>().unwrap()));
        let service_name = ServiceName::new(&service_path).unwrap();
        let service = node
            .service_builder(&service_name)
            .publish_subscribe::<TestFreezeCommand>()
            .max_publishers(1)
            .max_subscribers(1)
            .history_size(1)
            .subscriber_max_buffer_size(2)
            .enable_safe_overflow(true)
            .open_or_create()
            .unwrap();
        service.publisher_builder().create().unwrap()
    }

    fn publish_cmd(
        publisher: &iceoryx2::port::publisher::Publisher<IxService, TestFreezeCommand, ()>,
        kind: WorkerKind,
        duration_ms: u32,
    ) {
        let sample = publisher.loan_uninit().unwrap();
        let sample = sample.write_payload(TestFreezeCommand {
            kind: kind as u8,
            pad: [0; 3],
            duration_ms,
        });
        sample.send().unwrap();
    }

    /// 5 个 [`WorkerKind`] 都能被路由到对应 worker 的 `freeze_until_ns`
    /// （reviewer 硬要求："pub_thread / io_thread 至少要有控制路由测试"）。
    #[test]
    fn routes_to_each_worker_kind() {
        let robot_id = fresh_robot_id();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let ctx = make_ctx(robot_id.clone(), Arc::clone(&clock));
        let ipc = Arc::clone(&ctx.ipc);
        #[cfg(target_os = "linux")]
        let rt = ctx.rt.as_ref().map(Arc::clone).unwrap();
        let serial = ctx.serial_io.as_ref().map(Arc::clone).unwrap();
        let pub_thr = Arc::clone(&ctx.pub_thr);
        let io = Arc::clone(&ctx.io);

        let handle = spawn_freeze_control_thread(ctx).unwrap();
        // 给 control thread 一点时间真正建好 iceoryx2 subscriber（起 Node +
        // open_or_create service + subscriber_builder 需要 ~10ms）；publish
        // 早于 subscriber ready 时 sample 不会进 history（test-only QoS
        // history_size=1 但要 subscriber 存在才拿到）。
        thread::sleep(Duration::from_millis(50));
        let publisher = open_publisher(&robot_id);

        // Ipc kind=0 / 50ms
        let before = clock.now().0;
        publish_cmd(&publisher, WorkerKind::Ipc, 50);
        assert!(
            wait_for(Duration::from_secs(2), || {
                ipc.freeze_until_ns.load(Ordering::Acquire) > before
            }),
            "ipc.freeze_until_ns 未在 2s 内被写入"
        );
        let until = ipc.freeze_until_ns.load(Ordering::Acquire);
        assert!(
            (before + 49_000_000..=before + 200_000_000).contains(&until),
            "ipc until_ns 偏离预期：before={before} until={until}",
        );

        // Rt kind=1 / 60ms
        #[cfg(target_os = "linux")]
        {
            let before = clock.now().0;
            publish_cmd(&publisher, WorkerKind::Rt, 60);
            assert!(
                wait_for(Duration::from_secs(2), || {
                    rt.freeze_until_ns.load(Ordering::Acquire) > before
                }),
                "rt.freeze_until_ns 未写入"
            );
        }

        // SerialIo kind=2 / 70ms
        let before = clock.now().0;
        publish_cmd(&publisher, WorkerKind::SerialIo, 70);
        assert!(
            wait_for(Duration::from_secs(2), || {
                serial.freeze_until_ns.load(Ordering::Acquire) > before
            }),
            "serial.freeze_until_ns 未写入"
        );

        // Pub kind=3 / 80ms
        let before = clock.now().0;
        publish_cmd(&publisher, WorkerKind::Pub, 80);
        assert!(
            wait_for(Duration::from_secs(2), || {
                pub_thr.freeze_until_ns.load(Ordering::Acquire) > before
            }),
            "pub_thr.freeze_until_ns 未写入"
        );

        // Io kind=4 / 90ms
        let before = clock.now().0;
        publish_cmd(&publisher, WorkerKind::Io, 90);
        assert!(
            wait_for(Duration::from_secs(2), || {
                io.freeze_until_ns.load(Ordering::Acquire) > before
            }),
            "io.freeze_until_ns 未写入"
        );

        handle.request_stop();
        handle.join().unwrap();
    }

    /// invalid kind（如 99）→ 不写任何 `freeze_until_ns`，不 panic。
    #[test]
    fn rejects_invalid_worker_kind() {
        let robot_id = fresh_robot_id();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let ctx = make_ctx(robot_id.clone(), Arc::clone(&clock));
        let ipc = Arc::clone(&ctx.ipc);
        #[cfg(target_os = "linux")]
        let rt = ctx.rt.as_ref().map(Arc::clone).unwrap();
        let serial = ctx.serial_io.as_ref().map(Arc::clone).unwrap();
        let pub_thr = Arc::clone(&ctx.pub_thr);
        let io = Arc::clone(&ctx.io);

        let handle = spawn_freeze_control_thread(ctx).unwrap();
        thread::sleep(Duration::from_millis(50));
        let publisher = open_publisher(&robot_id);

        // 直接用 raw struct 发 kind=99（绕过 WorkerKind 类型）
        let sample = publisher.loan_uninit().unwrap();
        let sample = sample.write_payload(TestFreezeCommand {
            kind: 99,
            pad: [0; 3],
            duration_ms: 100,
        });
        sample.send().unwrap();

        // 给 control thread 时间处理（且确认它没 panic）
        thread::sleep(Duration::from_millis(50));

        assert_eq!(ipc.freeze_until_ns.load(Ordering::Acquire), 0);
        #[cfg(target_os = "linux")]
        assert_eq!(rt.freeze_until_ns.load(Ordering::Acquire), 0);
        assert_eq!(serial.freeze_until_ns.load(Ordering::Acquire), 0);
        assert_eq!(pub_thr.freeze_until_ns.load(Ordering::Acquire), 0);
        assert_eq!(io.freeze_until_ns.load(Ordering::Acquire), 0);

        handle.request_stop();
        handle.join().unwrap();
    }

    /// daemon 内 monotonic clock 计算 absolute ns 的语义锁：MockClock 冻在
    /// t=10_000_000，发 duration=50ms → freeze_until_ns 必须 = 10_000_000 +
    /// 50_000_000 = 60_000_000，**不**取系统真实时间。
    ///
    /// reviewer 硬边界："daemon 内用自己的 monotonic clock 计算
    /// freeze_until_ns，不要从测试进程传 absolute ns"。
    #[test]
    fn computes_absolute_ns_from_daemon_clock_only() {
        use robot_core::time::{MockClock, MonoRawNs};
        let robot_id = fresh_robot_id();
        let mock = Arc::new(MockClock::new(MonoRawNs(10_000_000)));
        let clock: Arc<dyn RobotClock> = Arc::clone(&mock) as Arc<dyn RobotClock>;
        let ctx = make_ctx(robot_id.clone(), clock);
        let ipc = Arc::clone(&ctx.ipc);

        let handle = spawn_freeze_control_thread(ctx).unwrap();
        thread::sleep(Duration::from_millis(50));
        let publisher = open_publisher(&robot_id);

        publish_cmd(&publisher, WorkerKind::Ipc, 50);

        assert!(
            wait_for(Duration::from_secs(2), || {
                ipc.freeze_until_ns.load(Ordering::Acquire) != 0
            }),
            "ipc.freeze_until_ns 未在 2s 内被写"
        );

        let until = ipc.freeze_until_ns.load(Ordering::Acquire);
        assert_eq!(
            until, 60_000_000,
            "MockClock(10_000_000) + 50ms 应当严格 = 60_000_000，实际 {until}（说明 control thread 取了系统时钟而不是 daemon clock）"
        );

        handle.request_stop();
        handle.join().unwrap();
    }

    /// `duration_ms = 0` ≡ no-op：必须 `store(0)`，跟 freeze_hook
    /// "未冻结 hot path == 0" 口径对齐（reviewer A2-2 非阻断备注收紧）。
    /// **不能** store `clock.now()`：那样 worker tick 每次都多 1 次
    /// `clock.now()` 比较，违反"hot path 仅 1 次 atomic load"。
    ///
    /// 测试方法：先发非零 cmd 让 `freeze_until_ns` 变成非 0，再发
    /// `duration_ms = 0` 把它清回 0。
    #[test]
    fn duration_ms_zero_is_noop() {
        let robot_id = fresh_robot_id();
        let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
        let ctx = make_ctx(robot_id.clone(), Arc::clone(&clock));
        let ipc = Arc::clone(&ctx.ipc);

        let handle = spawn_freeze_control_thread(ctx).unwrap();
        thread::sleep(Duration::from_millis(50));
        let publisher = open_publisher(&robot_id);

        // 1) 先 dirty：发 100ms freeze 让 freeze_until_ns 变 ≠ 0
        publish_cmd(&publisher, WorkerKind::Ipc, 100);
        assert!(
            wait_for(Duration::from_secs(2), || {
                ipc.freeze_until_ns.load(Ordering::Acquire) != 0
            }),
            "warmup 100ms freeze 未在 2s 内 store"
        );

        // 2) 发 duration_ms=0 → 期望严格 store(0)
        publish_cmd(&publisher, WorkerKind::Ipc, 0);
        assert!(
            wait_for(Duration::from_secs(2), || {
                ipc.freeze_until_ns.load(Ordering::Acquire) == 0
            }),
            "duration_ms=0 应当 store(0)，2s 内未观察到清零；实际 {}",
            ipc.freeze_until_ns.load(Ordering::Acquire),
        );

        handle.request_stop();
        handle.join().unwrap();
    }
}
