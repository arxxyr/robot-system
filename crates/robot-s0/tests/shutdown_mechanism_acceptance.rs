//! S48 W2b：纯关闭 / join 机制的公开 API 验收——真实线程（`Raw` / health monitor / systemd
//! watchdog / 真实 latch writer）、真实单调时钟、脚本化门谓词；不 spawn 生产 worker、不接
//! daemon、不定义第八 `WorkerKind`。
//!
//! 每个用例由父进程有界监督一个真实测试子进程；子进程持有未 join 的报告直到进程退出，
//! 父进程在失败或超期时终止并回收它。正常成功路径仍逐一验证真实 join，不把退出标志当凭证。

#![cfg(target_os = "linux")]

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use robot_core::time::{Deadline, DurationNs, MonoRawNs, RobotClock, SystemClock};
use robot_health::systemd::{WatchdogConfig, spawn_watchdog_with_usec};
use robot_health::{MonitorConfig, spawn_monitor};
use robot_s0::clear_estop_transaction::{ClearTxnStage, ClearTxnStore};
use robot_s0::latch_lock::{LatchTargetLock, acquire_latch_lock};
use robot_s0::latch_writer::{
    NoHook, PersistClient, PersistShared, Phase, RequestOutcome, StandbyStore, TokenRefused,
    WriterConfig, WriterExit, WriterHook, WriterId, WriterPairing, WriterStop, mailbox,
};
use robot_s0::shutdown_mechanism::{
    CleanInputs, HandleKind, HandleKindSet, HandleSlots, InsertError, JoinedOutcome,
    ManagedWriterHandle, RunExit, ShutdownFailure, Slot, SlotJoin, SlotView, TeardownGates,
    TeardownReport, WorkerSlot, WriterControl, exit_code, join_with_deadline, spawn_managed_writer,
    teardown,
};

const WAIT: Duration = Duration::from_secs(10);
const POLL: DurationNs = DurationNs::from_millis(1);
const BASE: MonoRawNs = MonoRawNs(1_000_000_000);
const NS_PER_MS: u64 = 1_000_000;
const CASE_WAIT: Duration = Duration::from_secs(20);
const REAP_WAIT: Duration = Duration::from_secs(5);
const CHILD_CASE: &str = "W2B_ACCEPTANCE_CHILD_CASE";
const CHILD_ROOT: &str = "W2B_ACCEPTANCE_CHILD_ROOT";

// ============================================================================
// 夹具
// ============================================================================

/// 父进程唯一拥有子进程；早期断言失败也先终止、回收，不依赖 `Child::drop`。
struct ReapedChild {
    child: Child,
    reaped: bool,
}

impl ReapedChild {
    fn wait_for(&mut self, duration: Duration) -> io::Result<Option<ExitStatus>> {
        let deadline = Instant::now() + duration;
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.reaped = true;
                return Ok(Some(status));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            thread::sleep(remaining.min(Duration::from_millis(5)));
        }
    }

    fn kill_and_reap(&mut self) {
        if self.reaped {
            return;
        }
        // kill 与自然退出可竞争；实际 try_wait 回收成功才算完成。
        let kill_result = self.child.kill();
        match self.wait_for(REAP_WAIT) {
            Ok(Some(_)) => {}
            other => {
                eprintln!("测试子进程未能有界回收：kill={kill_result:?}, wait={other:?}");
                std::process::abort();
            }
        }
    }
}

impl Drop for ReapedChild {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

/// 子进程只跑具名用例；完成标记与成功退出必须同时成立，避免空过滤器假阳性。
struct TestCase {
    name: &'static str,
}

impl TestCase {
    fn enter(name: &'static str) -> Option<Self> {
        if std::env::var(CHILD_CASE).ok().as_deref() == Some(name) {
            let test_thread = thread::current().id();
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                previous(info);
                // 主测试线程失败时保留所有权到进程终止，避免先展开阻塞 Drop。
                // worker 的预期 panic 必须正常 unwind，才能验证真实 JoinErr。
                if thread::current().id() == test_thread {
                    std::process::exit(101);
                }
            }));
            return Some(Self { name });
        }

        let root = tempfile::TempDir::new().expect("创建父进程持有的夹具目录");
        let log_path = root.path().join("child.log");
        let output = std::fs::File::create(&log_path).expect("创建子进程日志");
        let errors = output.try_clone().expect("复制子进程日志句柄");
        let mut child = ReapedChild {
            child: Command::new(std::env::current_exe().expect("定位当前测试二进制"))
                .args(["--exact", name, "--nocapture", "--test-threads=1"])
                .env(CHILD_CASE, name)
                .env(CHILD_ROOT, root.path())
                .stdin(Stdio::null())
                .stdout(output)
                .stderr(errors)
                .spawn()
                .expect("启动受监督用例"),
            reaped: false,
        };
        let Some(status) = child.wait_for(CASE_WAIT).expect("轮询子进程") else {
            child.kill_and_reap();
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!("用例 {name} 超期，子进程已终止并回收：\n{log}");
        };
        let log = std::fs::read_to_string(&log_path).expect("读取子进程日志");
        assert!(status.success(), "用例 {name} 子进程失败 {status}：\n{log}");
        let marker = format!("W2B_ACCEPTANCE_COMPLETED={name}");
        assert_eq!(log.matches(&marker).count(), 1, "缺少唯一完成标记：\n{log}");
        eprintln!(
            "w2b_child_verified case={name} pid={} exit={status}",
            child.child.id()
        );
        None
    }

    /// 不展开测试局部变量；未 join 的 residual/report 仍持有活句柄直到进程终止。
    fn complete(self) -> ! {
        println!("W2B_ACCEPTANCE_COMPLETED={}", self.name);
        io::stdout().flush().expect("刷新完成标记");
        std::process::exit(0);
    }
}

/// 真实单调时钟，但 `now()` 严格递增（同一 ns 内的连续读数也可区分先后）。
struct StrictClock {
    inner: SystemClock,
    last: AtomicU64,
}

impl StrictClock {
    fn new() -> Self {
        Self {
            inner: SystemClock,
            last: AtomicU64::new(0),
        }
    }
}

impl RobotClock for StrictClock {
    fn now(&self) -> MonoRawNs {
        let system = self.inner.now().0;
        let mut prev = self.last.load(Ordering::Acquire);
        loop {
            let next = system.max(prev + 1);
            match self
                .last
                .compare_exchange_weak(prev, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return MonoRawNs(next),
                Err(actual) => prev = actual,
            }
        }
    }

    fn sleep_until(&self, deadline: Deadline) {
        self.inner.sleep_until(deadline);
    }

    fn sleep_for(&self, d: DurationNs) {
        self.inner.sleep_for(d);
    }
}

fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "{what}");
        thread::sleep(Duration::from_millis(1));
    }
}

/// join 拒绝时保留返回的活句柄，主测试线程的 panic hook 随后终止整个子进程。
fn assert_worker_joined(slot: WorkerSlot) {
    match slot.join() {
        Ok(JoinedOutcome::Worker(Ok(()))) => {}
        Ok(other) => panic!("预期普通 worker 成功 join，实际为 {other:?}"),
        Err(live) => panic!("worker 未结束，仍持有返回句柄：{live:?}"),
    }
}

/// 以自己的停止原子为退出条件的真实线程；`exited` 记录退出路径观测，真实 join 另由报告确认。
struct RawThread {
    stop: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
}

fn spawn_raw(
    kind: HandleKind,
    clock: &Arc<StrictClock>,
    log: &Arc<Mutex<Vec<(HandleKind, u64)>>>,
) -> (WorkerSlot, RawThread) {
    let stop = Arc::new(AtomicBool::new(false));
    let exited = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread_exited = Arc::clone(&exited);
    let thread_clock = Arc::clone(clock);
    let thread_log = Arc::clone(log);
    let join = thread::spawn(move || {
        while !thread_stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
        thread_log
            .lock()
            .unwrap()
            .push((kind, thread_clock.now().0));
        thread_exited.store(true, Ordering::Release);
    });
    (
        WorkerSlot::Raw {
            kind,
            join,
            stop: Arc::clone(&stop),
        },
        RawThread { stop, exited },
    )
}

/// 脚本化门：记录调用顺序与评估时刻；`ack` / `clear` 可绑定"辅助 / 执行器已真实退出"。
struct ScriptedGates {
    ack: Box<dyn FnMut() -> bool>,
    clear: Box<dyn FnMut() -> bool>,
    calls: Vec<(&'static str, u64)>,
}

impl ScriptedGates {
    fn fixed(ack: bool, clear: bool) -> Self {
        Self {
            ack: Box::new(move || ack),
            clear: Box::new(move || clear),
            calls: Vec::new(),
        }
    }

    fn first_call(&self, name: &str) -> Option<u64> {
        self.calls
            .iter()
            .find(|(called, _)| *called == name)
            .map(|(_, at)| *at)
    }
}

impl TeardownGates for ScriptedGates {
    fn control_stop_and_notify(&mut self) {
        self.calls.push(("control", 0));
    }
    fn ack_reached(&mut self, now_ns: u64) -> bool {
        self.calls.push(("ack", now_ns));
        (self.ack)()
    }
    fn clear_retired_and_flushed(&mut self, now_ns: u64) -> bool {
        self.calls.push(("clear", now_ns));
        (self.clear)()
    }
}

fn all_exited(threads: &[&RawThread]) -> impl Fn() -> bool + 'static {
    let flags: Vec<Arc<AtomicBool>> = threads.iter().map(|t| Arc::clone(&t.exited)).collect();
    move || flags.iter().all(|flag| flag.load(Ordering::Acquire))
}

fn deadline_after(clock: &StrictClock, ms: u64) -> u64 {
    clock.now().0 + ms * NS_PER_MS
}

fn kinds_of(trace: &[(HandleKind, u64)]) -> Vec<HandleKind> {
    trace.iter().map(|(kind, _)| *kind).collect()
}

fn strictly_increasing(trace: &[(HandleKind, u64)]) -> bool {
    trace.windows(2).all(|pair| pair[0].1 < pair[1].1)
}

// ---- 真实 latch writer 夹具（W2a 同款：tempdir + 真实 W0 store + W1 锁配对）----

/// 测试专用原子控制器：`running` = load(Acquire)，`request_stop` = store(false, Release)。
struct AtomicWriterControl(AtomicBool);

impl WriterStop for AtomicWriterControl {
    fn running(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl WriterControl for AtomicWriterControl {
    fn request_stop(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// 在指定文件步骤前 panic（证明 panic 的 `Err` 仍附带唯一凭证）。
struct PanicAt(ClearTxnStage);

impl WriterHook for PanicAt {
    fn before_step(&mut self, stage: ClearTxnStage) {
        assert!(self.0 != stage, "测试注入：writer 在 {stage:?} 前 panic");
    }
}

fn open(latch: &Path) -> ClearTxnStore {
    ClearTxnStore::open(latch, "xlerobot_left", "left_arm").unwrap()
}

/// 子进程持有句柄、共享字和锁；成功路径显式 join，受保护报告持有活句柄直到子进程结束。
/// 目录归父进程所有，子进程早失败也在真实回收后清理；`Exited` 只作观测，不充当 join。
struct WriterRig {
    handle: Option<ManagedWriterHandle>,
    control: Arc<AtomicWriterControl>,
    standby: Option<StandbyStore>,
    shared: Arc<PersistShared>,
    _client: PersistClient,
    writer_id: WriterId,
    _lock: LatchTargetLock,
    _dir: tempfile::TempDir,
}

impl WriterRig {
    fn spawn(hook: Box<dyn WriterHook>) -> Self {
        let root = std::env::var_os(CHILD_ROOT).expect("writer 夹具必须在受监督子进程内");
        let dir = tempfile::TempDir::new_in(root).unwrap();
        let latch: PathBuf = dir.path().join("estop_latch");
        std::fs::write(&latch, b"1\n").unwrap();
        std::fs::write(latch.with_file_name("estop_latch.bak"), b"1\n").unwrap();
        let lock = acquire_latch_lock(&latch).unwrap();
        let (slot, standby) = WriterPairing::new(open(&latch), open(&latch), &lock, BASE)
            .unwrap()
            .into_parts();
        let shared = Arc::clone(slot.shared());
        let (client, endpoint) = mailbox(Arc::clone(&shared));
        let control = Arc::new(AtomicWriterControl(AtomicBool::new(true)));
        let handle = spawn_managed_writer(
            slot,
            endpoint,
            WriterConfig::default(),
            Arc::clone(&control),
            Arc::new(SystemClock),
            hook,
        )
        .unwrap();
        let writer_id = handle.writer();
        Self {
            handle: Some(handle),
            control,
            standby: Some(standby),
            shared,
            _client: client,
            writer_id,
            _lock: lock,
            _dir: dir,
        }
    }

    fn take_handle(&mut self) -> ManagedWriterHandle {
        self.handle.take().expect("句柄只取一次")
    }

    fn take_standby(&mut self) -> StandbyStore {
        self.standby.take().expect("standby 只取一次")
    }

    fn exited(&self) -> bool {
        self.shared.progress.load().unwrap().phase == Phase::Exited
    }
}

// ============================================================================
// 通用 join / 三态槽
// ============================================================================

/// B3：同一 kind 的三个槽经 `view()` 与 `join_with_deadline` 区分 `NotSpawned` / `Joined(Ok)` /
/// `Joined(Err)`；`Joined` 只能由真实 join 产生。
#[test]
fn slot_views_distinguish_not_spawned_joined_ok_and_joined_err() {
    let Some(case) = TestCase::enter("slot_views_distinguish_not_spawned_joined_ok_and_joined_err")
    else {
        return;
    };
    let clock = Arc::new(StrictClock::new());
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut empty = Slot::not_spawned();
    assert_eq!(empty.view(), SlotView::NotSpawned);
    assert!(matches!(
        join_with_deadline(&mut empty, clock.as_ref(), deadline_after(&clock, 10), POLL),
        SlotJoin::NotSpawned
    ));
    assert_eq!(empty.view(), SlotView::NotSpawned, "空槽绝不变成 Joined");

    let (ok_slot, raw) = spawn_raw(HandleKind::Io, &clock, &log);
    raw.stop.store(true, Ordering::Release);
    let mut ok = Slot::live(ok_slot);
    assert_eq!(ok.view(), SlotView::Live(HandleKind::Io));
    assert!(matches!(
        join_with_deadline(&mut ok, clock.as_ref(), deadline_after(&clock, 5_000), POLL),
        SlotJoin::Joined(JoinedOutcome::Worker(Ok(())))
    ));
    assert_eq!(ok.view(), SlotView::Joined(HandleKind::Io));
    assert!(matches!(
        join_with_deadline(&mut ok, clock.as_ref(), deadline_after(&clock, 10), POLL),
        SlotJoin::AlreadyJoined
    ));

    let mut failed = Slot::live(WorkerSlot::Raw {
        kind: HandleKind::Io,
        join: thread::spawn(|| panic!("测试注入：Io 线程 panic")),
        stop: Arc::new(AtomicBool::new(false)),
    });
    assert!(matches!(
        join_with_deadline(
            &mut failed,
            clock.as_ref(),
            deadline_after(&clock, 5_000),
            POLL
        ),
        SlotJoin::Joined(JoinedOutcome::Worker(Err(_)))
    ));
    assert_eq!(failed.view(), SlotView::Joined(HandleKind::Io));
    case.complete();
}

/// 拒停线程：截止时 `Pending` 且 `Live` 保留（不伪造 Joined、不分离）；放行后真实 join。
#[test]
fn refusing_thread_stays_live_and_pending_at_deadline() {
    let Some(case) = TestCase::enter("refusing_thread_stays_live_and_pending_at_deadline") else {
        return;
    };
    let clock = Arc::new(StrictClock::new());
    let stop = Arc::new(AtomicBool::new(false));
    let observed_stop = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread_observed = Arc::clone(&observed_stop);
    let thread_release = Arc::clone(&release);
    let handle = WorkerSlot::Raw {
        kind: HandleKind::Publisher,
        stop,
        join: thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            thread_observed.store(true, Ordering::Release);
            while !thread_release.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
        }),
    };
    handle.request_stop();
    wait_until(
        || observed_stop.load(Ordering::Acquire),
        "线程实际观察停止请求",
    );
    assert!(!handle.is_finished(), "已收停止请求但尚未独立放行");
    let mut live = Slot::live(handle);
    let started = Instant::now();
    assert!(matches!(
        join_with_deadline(&mut live, clock.as_ref(), deadline_after(&clock, 50), POLL),
        SlotJoin::Pending
    ));
    assert!(started.elapsed() < Duration::from_secs(5), "有界等待");
    assert_eq!(live.view(), SlotView::Live(HandleKind::Publisher));
    assert!(!release.load(Ordering::Acquire));
    release.store(true, Ordering::Release);
    assert!(matches!(
        join_with_deadline(
            &mut live,
            clock.as_ref(),
            deadline_after(&clock, 5_000),
            POLL
        ),
        SlotJoin::Joined(JoinedOutcome::Worker(Ok(())))
    ));
    assert_eq!(live.view(), SlotView::Joined(HandleKind::Publisher));
    case.complete();
}

/// health 变体：运行中 `join(self)` 以 `Err(slot)` 原样退回句柄（不阻塞、不伪造 Ok）；
/// `join_with_deadline` 遇 Pending 放回 `Live`；请求停止后真实 join。
#[test]
fn health_pending_join_returns_the_handle_and_slot_stays_live() {
    let Some(case) = TestCase::enter("health_pending_join_returns_the_handle_and_slot_stays_live")
    else {
        return;
    };
    let monitor = spawn_monitor(MonitorConfig {
        interval: Duration::from_millis(20),
        interested_processes: Vec::new(),
        top_n: 0,
    });
    let slot = WorkerSlot::HealthMonitor(monitor);
    let slot = match slot.join() {
        Ok(outcome) => panic!("运行中的 monitor 不得报告已 join: {outcome:?}"),
        Err(slot) => slot,
    };
    assert_eq!(slot.kind(), HandleKind::HealthMonitor);
    slot.request_stop();
    wait_until(|| slot.is_finished(), "monitor 应在停止后结束");
    assert_worker_joined(slot);

    let clock = Arc::new(StrictClock::new());
    let watchdog =
        spawn_watchdog_with_usec(vec![], WatchdogConfig::new("w2b_wd_pending"), 20_000_000)
            .expect("usec > 0");
    let mut live = Slot::live(WorkerSlot::SystemdWatchdog(watchdog));
    assert!(matches!(
        join_with_deadline(&mut live, clock.as_ref(), deadline_after(&clock, 30), POLL),
        SlotJoin::Pending
    ));
    assert_eq!(live.view(), SlotView::Live(HandleKind::SystemdWatchdog));
    // 真实 health Drop 停止并 join；若阻塞，由父进程截止负责终止并回收，非事后耗时断言。
    drop(live);
    case.complete();
}

/// root-2：同 kind 二次插入 → `Err((AlreadyPresent, 原句柄))`；返还的 `Raw` 与 health 句柄
/// 都仍可显式停止并真实 join（health 不触发阻塞 Drop）。
#[test]
fn duplicate_spawn_returns_original_handle_which_can_still_stop_and_join() {
    let Some(case) =
        TestCase::enter("duplicate_spawn_returns_original_handle_which_can_still_stop_and_join")
    else {
        return;
    };
    let clock = Arc::new(StrictClock::new());
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut slots = HandleSlots::new(HandleKindSet::EMPTY);
    let (first, first_raw) = spawn_raw(HandleKind::Publisher, &clock, &log);
    let (second, second_raw) = spawn_raw(HandleKind::Publisher, &clock, &log);
    slots.spawned(first).unwrap();
    let Err((error, returned)) = slots.spawned(second) else {
        panic!("同 kind 二次插入必须被拒绝");
    };
    assert_eq!(error, InsertError::AlreadyPresent);
    returned.request_stop();
    wait_until(|| returned.is_finished(), "返还的 Raw 句柄应结束");
    assert_worker_joined(returned);
    assert!(second_raw.exited.load(Ordering::Acquire));
    assert!(
        !first_raw.stop.load(Ordering::Acquire),
        "集合内原句柄未被触碰"
    );

    let config = MonitorConfig {
        interval: Duration::from_millis(20),
        interested_processes: Vec::new(),
        top_n: 0,
    };
    slots
        .spawned(WorkerSlot::HealthMonitor(spawn_monitor(config.clone())))
        .unwrap();
    let Err((error, returned)) = slots.spawned(WorkerSlot::HealthMonitor(spawn_monitor(config)))
    else {
        panic!("同 kind 二次插入必须被拒绝");
    };
    assert_eq!(error, InsertError::AlreadyPresent);
    assert_eq!(returned.kind(), HandleKind::HealthMonitor);
    returned.request_stop();
    wait_until(|| returned.is_finished(), "返还的 monitor 应结束");
    assert_worker_joined(returned);

    let mut gates = ScriptedGates::fixed(true, true);
    let report = teardown(
        slots,
        &mut gates,
        clock.as_ref(),
        deadline_after(&clock, 5_000),
        POLL,
    );
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(
        kinds_of(&report.stop_trace),
        vec![HandleKind::Publisher, HandleKind::HealthMonitor]
    );
    assert!(first_raw.exited.load(Ordering::Acquire));
    case.complete();
}

// ============================================================================
// teardown 顺序 / 责任 / 报告
// ============================================================================

struct Rig {
    clock: Arc<StrictClock>,
    log: Arc<Mutex<Vec<(HandleKind, u64)>>>,
    slots: HandleSlots,
    threads: Vec<(HandleKind, RawThread)>,
}

impl Rig {
    fn new(required: HandleKindSet, kinds: &[HandleKind]) -> Self {
        let clock = Arc::new(StrictClock::new());
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut slots = HandleSlots::new(required);
        let mut threads = Vec::new();
        for kind in kinds {
            let (slot, raw) = spawn_raw(*kind, &clock, &log);
            slots.spawned(slot).unwrap();
            threads.push((*kind, raw));
        }
        Self {
            clock,
            log,
            slots,
            threads,
        }
    }

    /// 按值取走集合（`Rig` 其余部分仍可借用以核线程状态）。
    fn take_slots(&mut self) -> HandleSlots {
        std::mem::replace(&mut self.slots, HandleSlots::new(HandleKindSet::EMPTY))
    }

    fn thread(&self, kind: HandleKind) -> &RawThread {
        &self.threads.iter().find(|(k, _)| *k == kind).unwrap().1
    }

    fn observed(&self, kind: HandleKind) -> u64 {
        self.log
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| *k == kind)
            .map_or_else(|| panic!("{kind:?} 未观察到停止"), |(_, at)| *at)
    }
}

const SEVEN: [HandleKind; 7] = [
    HandleKind::ClearRr,
    HandleKind::SetRr,
    HandleKind::Publisher,
    HandleKind::Io,
    HandleKind::Executor,
    HandleKind::Ipc,
    HandleKind::Watchdog,
];

const PROTECTED: [HandleKind; 3] = [HandleKind::Executor, HandleKind::Ipc, HandleKind::Watchdog];

fn required_core() -> HandleKindSet {
    HandleKindSet::EMPTY
        .with(HandleKind::Executor)
        .with(HandleKind::Ipc)
        .with(HandleKind::Watchdog)
}

/// standalone（无 systemd / freeze / hook / writer：可选 NotSpawned）：无 JoinTimeout、无
/// failure、全部 Joined、residual 空、`executor_joined && ipc_joined`，退出码 0。
#[test]
fn standalone_without_optional_helpers_has_no_failures() {
    let Some(case) = TestCase::enter("standalone_without_optional_helpers_has_no_failures") else {
        return;
    };
    let mut rig = Rig::new(required_core(), &SEVEN);
    let mut gates = ScriptedGates::fixed(true, true);
    let report = teardown(
        rig.take_slots(),
        &mut gates,
        rig.clock.as_ref(),
        deadline_after(&rig.clock, 5_000),
        POLL,
    );
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(report.residual.is_empty());
    assert_eq!(report.joined.len(), 7);
    assert!(report.joined.iter().all(|(_, result)| result.is_ok()));
    assert!(report.executor_joined && report.ipc_joined);
    assert_eq!(kinds_of(&report.stop_trace), SEVEN.to_vec());
    for (_, raw) in &rig.threads {
        assert!(raw.exited.load(Ordering::Acquire));
    }
    let clean = CleanInputs {
        runtime_ok: true,
        ack_before_executor_stop: true,
        clear_retired: true,
        flushed: 1,
        failures: &report.failures,
        residual_empty: report.residual.is_empty(),
    };
    assert_eq!(exit_code(&clean), 0);
    case.complete();
}

/// `required` 含 Executor 而未创建 → `RequiredMissing(Executor)`、`executor_joined=false`，
/// 退出码 1；其它活槽照常收尾。
#[test]
fn required_executor_missing_is_reported_and_exit_code_is_one() {
    let Some(case) = TestCase::enter("required_executor_missing_is_reported_and_exit_code_is_one")
    else {
        return;
    };
    let mut rig = Rig::new(
        HandleKindSet::EMPTY.with(HandleKind::Executor),
        &[HandleKind::Ipc, HandleKind::Watchdog],
    );
    let mut gates = ScriptedGates::fixed(true, true);
    let report = teardown(
        rig.take_slots(),
        &mut gates,
        rig.clock.as_ref(),
        deadline_after(&rig.clock, 5_000),
        POLL,
    );
    assert_eq!(
        report.failures,
        vec![ShutdownFailure::RequiredMissing(HandleKind::Executor)]
    );
    assert!(!report.executor_joined);
    assert!(report.ipc_joined);
    assert!(report.residual.is_empty());
    assert_eq!(
        kinds_of(&report.stop_trace),
        vec![HandleKind::Ipc, HandleKind::Watchdog]
    );
    let inputs = CleanInputs {
        runtime_ok: true,
        ack_before_executor_stop: true,
        clear_retired: true,
        flushed: 1,
        failures: &report.failures,
        residual_empty: true,
    };
    assert_eq!(exit_code(&inputs), 1);
    case.complete();
}

/// 顺序：① control → ② 辅助（含真实 health monitor / systemd watchdog）→ ③ ACK 真才停
/// Executor → ④ Clear/flush 真才停 Ipc → ⑤ Watchdog；门读取辅助 / 执行器的退出路径观测，
/// 报告还须证明全部句柄已真实 join、residual 为空。
/// 只比较这些门建立的因果关系，不要求请求后的 trace 时间早于并发 worker 的观察时间。
#[test]
fn teardown_orders_helpers_executor_ipc_watchdog_with_real_health_handles() {
    let Some(case) =
        TestCase::enter("teardown_orders_helpers_executor_ipc_watchdog_with_real_health_handles")
    else {
        return;
    };
    let mut rig = Rig::new(required_core(), &SEVEN);
    rig.slots
        .spawned(WorkerSlot::HealthMonitor(spawn_monitor(MonitorConfig {
            interval: Duration::from_millis(20),
            interested_processes: Vec::new(),
            top_n: 0,
        })))
        .unwrap();
    rig.slots
        .spawned(WorkerSlot::SystemdWatchdog(
            spawn_watchdog_with_usec(vec![], WatchdogConfig::new("w2b_wd_order"), 20_000_000)
                .expect("usec > 0"),
        ))
        .unwrap();
    let helpers_exited = all_exited(&[
        rig.thread(HandleKind::ClearRr),
        rig.thread(HandleKind::SetRr),
        rig.thread(HandleKind::Publisher),
        rig.thread(HandleKind::Io),
    ]);
    let executor_exited = all_exited(&[rig.thread(HandleKind::Executor)]);
    let mut gates = ScriptedGates {
        ack: Box::new(helpers_exited),
        clear: Box::new(executor_exited),
        calls: Vec::new(),
    };
    let report = teardown(
        rig.take_slots(),
        &mut gates,
        rig.clock.as_ref(),
        deadline_after(&rig.clock, 5_000),
        POLL,
    );
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(report.residual.is_empty());
    assert_eq!(report.joined.len(), 9);
    assert_eq!(
        kinds_of(&report.stop_trace),
        vec![
            HandleKind::ClearRr,
            HandleKind::SetRr,
            HandleKind::Publisher,
            HandleKind::Io,
            HandleKind::HealthMonitor,
            HandleKind::SystemdWatchdog,
            HandleKind::Executor,
            HandleKind::Ipc,
            HandleKind::Watchdog,
        ]
    );
    assert!(strictly_increasing(&report.stop_trace));
    assert_eq!(gates.calls.first(), Some(&("control", 0)));
    let first_ack = gates.first_call("ack").unwrap();
    let first_clear = gates.first_call("clear").unwrap();
    let requested = |kind: HandleKind| {
        report
            .stop_trace
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, at)| *at)
            .unwrap()
    };
    // ACK/Clear 可以先轮询到 false；成功必须晚于依赖线程的退出观测。
    // 请求后 trace 可能晚于 worker 观察，下面只比较跨门因果及主线程请求顺序。
    let helpers_observed = [
        HandleKind::ClearRr,
        HandleKind::SetRr,
        HandleKind::Publisher,
        HandleKind::Io,
    ]
    .iter()
    .map(|kind| rig.observed(*kind))
    .max()
    .unwrap();
    assert!(helpers_observed < requested(HandleKind::Executor));
    assert!(first_ack < requested(HandleKind::Executor));
    assert!(first_ack < rig.observed(HandleKind::Executor));
    assert!(rig.observed(HandleKind::Executor) < requested(HandleKind::Ipc));
    assert!(first_clear < requested(HandleKind::Ipc));
    assert!(first_clear < rig.observed(HandleKind::Ipc));
    assert!(requested(HandleKind::Ipc) < requested(HandleKind::Watchdog));
    assert!(first_clear < rig.observed(HandleKind::Watchdog));
    case.complete();
}

/// ACK 假：Executor / Ipc / Watchdog 未收 stop、`LoweringUnconfirmed`、三者进 residual
/// （JoinTimeout），辅助照常 Joined；`RunExit::into_parts` 保持 residual，退出码 1。
#[test]
fn ack_refused_protects_executor_ipc_and_watchdog_and_keeps_residual() {
    let Some(case) =
        TestCase::enter("ack_refused_protects_executor_ipc_and_watchdog_and_keeps_residual")
    else {
        return;
    };
    let mut rig = Rig::new(required_core(), &SEVEN);
    let mut gates = ScriptedGates::fixed(false, true);
    let started = Instant::now();
    let report = teardown(
        rig.take_slots(),
        &mut gates,
        rig.clock.as_ref(),
        deadline_after(&rig.clock, 100),
        POLL,
    );
    assert!(started.elapsed() < Duration::from_secs(5), "有界收尾");
    assert_eq!(
        report.failures,
        vec![
            ShutdownFailure::LoweringUnconfirmed,
            ShutdownFailure::JoinTimeout(HandleKind::Executor),
            ShutdownFailure::JoinTimeout(HandleKind::Ipc),
            ShutdownFailure::JoinTimeout(HandleKind::Watchdog),
        ]
    );
    for kind in PROTECTED {
        assert!(
            !rig.thread(kind).stop.load(Ordering::Acquire),
            "{kind:?} 不得收到停止"
        );
    }
    assert_eq!(
        kinds_of(&report.stop_trace),
        vec![
            HandleKind::ClearRr,
            HandleKind::SetRr,
            HandleKind::Publisher,
            HandleKind::Io
        ]
    );
    assert!(
        gates.first_call("clear").is_none(),
        "ACK 失败不评估 Clear 门"
    );
    assert_eq!(report.joined.len(), 4);
    assert!(!report.executor_joined && !report.ipc_joined);
    assert_eq!(
        report.residual.bitmap(),
        HandleKind::Executor.bit() | HandleKind::Ipc.bit() | HandleKind::Watchdog.bit()
    );
    assert_eq!(
        report.residual.kinds().collect::<Vec<_>>(),
        PROTECTED.to_vec()
    );
    let inputs = CleanInputs {
        runtime_ok: true,
        ack_before_executor_stop: false,
        clear_retired: false,
        flushed: 0,
        failures: &report.failures,
        residual_empty: report.residual.is_empty(),
    };
    let run = RunExit::new(exit_code(&inputs), report.residual);
    assert_eq!(run.code(), 1);
    let (code, residual) = run.into_parts();
    assert_eq!(code, 1);
    assert_eq!(residual.len(), 3);
    assert_eq!(residual.kinds().collect::<Vec<_>>(), PROTECTED.to_vec());
    // residual 保留所有权到子进程终止，不能先 drop 活句柄再把退出观测当成 join。
    case.complete();
}

/// ACK 真 + Clear/flush 假：Executor 停并 Joined；Ipc / Watchdog 未收 stop、
/// `ClearOrFlushIncomplete`、二者进 residual。
#[test]
fn clear_or_flush_incomplete_stops_executor_but_protects_ipc_and_watchdog() {
    let Some(case) =
        TestCase::enter("clear_or_flush_incomplete_stops_executor_but_protects_ipc_and_watchdog")
    else {
        return;
    };
    let mut rig = Rig::new(required_core(), &SEVEN);
    let mut gates = ScriptedGates::fixed(true, false);
    let report = teardown(
        rig.take_slots(),
        &mut gates,
        rig.clock.as_ref(),
        deadline_after(&rig.clock, 100),
        POLL,
    );
    assert_eq!(
        report.failures,
        vec![
            ShutdownFailure::ClearOrFlushIncomplete,
            ShutdownFailure::JoinTimeout(HandleKind::Ipc),
            ShutdownFailure::JoinTimeout(HandleKind::Watchdog),
        ]
    );
    assert!(report.executor_joined && !report.ipc_joined);
    assert!(
        rig.thread(HandleKind::Executor)
            .exited
            .load(Ordering::Acquire)
    );
    assert!(!rig.thread(HandleKind::Ipc).stop.load(Ordering::Acquire));
    assert!(
        !rig.thread(HandleKind::Watchdog)
            .stop
            .load(Ordering::Acquire)
    );
    assert_eq!(
        report.stop_trace.last().map(|(kind, _)| *kind),
        Some(HandleKind::Executor)
    );
    assert_eq!(
        report.residual.bitmap(),
        HandleKind::Ipc.bit() | HandleKind::Watchdog.bit()
    );
    case.complete();
}

/// 辅助线程 panic → 真实 join 得 `Err`、记 `Panicked(kind)`，不进 residual、无 `JoinTimeout`。
#[test]
fn helper_panic_is_reported_as_panicked_and_not_residual() {
    let Some(case) = TestCase::enter("helper_panic_is_reported_as_panicked_and_not_residual")
    else {
        return;
    };
    let mut rig = Rig::new(
        required_core(),
        &[HandleKind::Executor, HandleKind::Ipc, HandleKind::Watchdog],
    );
    let mut slots = rig.take_slots();
    slots
        .spawned(WorkerSlot::Raw {
            kind: HandleKind::Publisher,
            join: thread::spawn(|| panic!("测试注入：publisher panic")),
            stop: Arc::new(AtomicBool::new(false)),
        })
        .unwrap();
    let mut gates = ScriptedGates::fixed(true, true);
    let report = teardown(
        slots,
        &mut gates,
        rig.clock.as_ref(),
        deadline_after(&rig.clock, 5_000),
        POLL,
    );
    assert_eq!(
        report.failures,
        vec![ShutdownFailure::Panicked(HandleKind::Publisher)]
    );
    assert!(report.residual.is_empty());
    let (_, publisher) = report
        .joined
        .iter()
        .find(|(kind, _)| *kind == HandleKind::Publisher)
        .unwrap();
    assert!(publisher.is_err(), "panic 以真实 join 的 Err 表达");
    assert_eq!(report.joined.len(), 4);
    assert!(report.executor_joined && report.ipc_joined);
    case.complete();
}

// ============================================================================
// 真实 latch writer 槽（root 增补合同：唯一 spawn 绑定同一 Arc<C>）
// ============================================================================

/// 增补 §3.1：`request_stop` 作用于 spawn 时绑定的同一控制对象 → `is_finished` 有界为真 →
/// `join` = `(Ok(Stopped), joined)` 且凭证与句柄 writer 一致；配对 standby `after_join` Ok。
#[test]
fn managed_writer_stop_request_reaches_bound_thread_and_join_yields_bound_credential() {
    let Some(case) = TestCase::enter(
        "managed_writer_stop_request_reaches_bound_thread_and_join_yields_bound_credential",
    ) else {
        return;
    };
    let mut rig = WriterRig::spawn(Box::new(NoHook));
    let handle = rig.take_handle();
    assert!(!handle.is_finished());
    assert!(rig.control.running());
    handle.request_stop();
    assert!(!rig.control.running(), "请求作用于同一控制对象");
    wait_until(|| handle.is_finished(), "writer 应在停止请求后结束");
    let (result, joined) = handle.join();
    assert_eq!(result.unwrap(), WriterExit::Stopped);
    assert_eq!(joined.writer(), rig.writer_id);
    let mut standby = rig.take_standby();
    assert!(standby.after_join(joined).is_ok(), "实例绑定保持");
    case.complete();
}

/// 文件步骤 panic 由 teardown 真正 join，报告保存 Err 与唯一凭证；既不造超时也不留 residual。
fn panic_writer_report(writer: &mut WriterRig) -> TeardownReport {
    assert_eq!(
        writer.shared.obligation.request(MonoRawNs(BASE.0 + 3)),
        RequestOutcome::Registered
    );
    wait_until(
        || writer.handle.as_ref().unwrap().is_finished(),
        "文件步骤 panic 的 writer 应结束",
    );
    let mut rig = Rig::new(HandleKindSet::EMPTY, &[]);
    let mut gates = ScriptedGates::fixed(true, true);
    let report = writer_teardown(&mut rig, writer, &mut gates, 5_000);
    assert_eq!(
        report.failures,
        vec![ShutdownFailure::Panicked(HandleKind::LatchWriter)]
    );
    assert!(report.residual.is_empty(), "panic 后真实 join，不进入残余");
    assert!(report.joined.is_empty(), "writer 独立进入唯一凭证槽");
    report
}

/// 增补 §3.2 + r3 B1：两次真实文件步骤 panic 都经 teardown → `take_writer`；
/// 独立凭证分别由配对 standby 接受、错配 standby 拒绝，不复用已消费的凭证。
#[test]
fn managed_writer_panic_in_file_step_still_yields_credential_and_foreign_standby_refuses() {
    let Some(case) = TestCase::enter(
        "managed_writer_panic_in_file_step_still_yields_credential_and_foreign_standby_refuses",
    ) else {
        return;
    };
    let mut matching = WriterRig::spawn(Box::new(PanicAt(ClearTxnStage::CreatePrimary)));
    let mut matching_report = panic_writer_report(&mut matching);
    let (result, joined) = matching_report
        .take_writer()
        .expect("panic 仍有唯一 writer 凭证");
    assert!(result.is_err(), "panic 以真实 join 的 Err 表达");
    assert_eq!(joined.writer(), matching.writer_id);
    assert!(
        matching_report.take_writer().is_none(),
        "第二次不可取出凭证"
    );
    let mut matching_standby = matching.take_standby();
    assert!(
        matching_standby.after_join(joined).is_ok(),
        "panic 凭证仍被正确实例接受"
    );

    let mut mismatching = WriterRig::spawn(Box::new(PanicAt(ClearTxnStage::CreatePrimary)));
    let mut mismatching_report = panic_writer_report(&mut mismatching);
    let (result, joined) = mismatching_report
        .take_writer()
        .expect("第二次真实 panic 的独立凭证");
    assert!(result.is_err());
    assert_eq!(joined.writer(), mismatching.writer_id);
    assert_ne!(matching.writer_id, mismatching.writer_id);
    assert!(mismatching_report.take_writer().is_none());
    assert!(matches!(
        matching_standby.after_join(joined),
        Err(TokenRefused::WriterMismatch)
    ));
    case.complete();
}

/// 增补 §3.3：未请求停止时 `join_with_deadline` 为 `Pending` 且 `Live(LatchWriter)` 保留；
/// 经同一控制对象请求后 → `Joined(Writer{Ok(Stopped)})`，槽转 `Joined(LatchWriter)`。
#[test]
fn join_with_deadline_on_live_managed_writer_is_pending_until_requested() {
    let Some(case) =
        TestCase::enter("join_with_deadline_on_live_managed_writer_is_pending_until_requested")
    else {
        return;
    };
    let clock = Arc::new(StrictClock::new());
    let mut rig = WriterRig::spawn(Box::new(NoHook));
    let mut live = Slot::live(WorkerSlot::LatchWriter(rig.take_handle()));
    assert!(matches!(
        join_with_deadline(&mut live, clock.as_ref(), deadline_after(&clock, 50), POLL),
        SlotJoin::Pending
    ));
    assert_eq!(live.view(), SlotView::Live(HandleKind::LatchWriter));
    assert!(rig.control.running());
    rig.control.request_stop();
    let SlotJoin::Joined(JoinedOutcome::Writer { result, joined }) = join_with_deadline(
        &mut live,
        clock.as_ref(),
        deadline_after(&clock, 5_000),
        POLL,
    ) else {
        panic!("请求停止后应真实 join");
    };
    assert_eq!(result.unwrap(), WriterExit::Stopped);
    assert_eq!(joined.writer(), rig.writer_id);
    assert_eq!(live.view(), SlotView::Joined(HandleKind::LatchWriter));
    case.complete();
}

fn writer_teardown(
    rig: &mut Rig,
    writer: &mut WriterRig,
    gates: &mut ScriptedGates,
    ms: u64,
) -> TeardownReport {
    let mut slots = rig.take_slots();
    slots
        .spawned(WorkerSlot::LatchWriter(writer.take_handle()))
        .unwrap();
    teardown(
        slots,
        gates,
        rig.clock.as_ref(),
        deadline_after(&rig.clock, ms),
        POLL,
    )
}

/// 增补 §3.4 + r3 表②③④⑤：ACK 假 → LatchWriter（与 Executor / Ipc / Watchdog）未收
/// 停止且进 residual；ACK 真 + Clear/flush 假 → 同样未收；两种情况 `take_writer` 为 `None`。
#[test]
fn writer_slot_is_protected_by_ack_and_clear_gates() {
    let Some(case) = TestCase::enter("writer_slot_is_protected_by_ack_and_clear_gates") else {
        return;
    };
    // 两种门失败的报告、锁和共享状态均保持所有权到整个子进程结束。
    let mut retained = Vec::new();
    for (ack, clear, expected_first) in [
        (false, true, ShutdownFailure::LoweringUnconfirmed),
        (true, false, ShutdownFailure::ClearOrFlushIncomplete),
    ] {
        let mut rig = Rig::new(required_core(), &SEVEN);
        let mut writer = WriterRig::spawn(Box::new(NoHook));
        let mut gates = ScriptedGates::fixed(ack, clear);
        let mut report = writer_teardown(&mut rig, &mut writer, &mut gates, 100);
        assert_eq!(
            report.failures.first(),
            Some(&expected_first),
            "{ack}/{clear}"
        );
        assert!(
            report
                .failures
                .contains(&ShutdownFailure::JoinTimeout(HandleKind::LatchWriter))
        );
        assert!(
            writer.control.running(),
            "门失败时 writer 不得收到停止请求（{ack}/{clear}）"
        );
        assert!(!writer.exited());
        assert!(
            report
                .residual
                .kinds()
                .any(|kind| kind == HandleKind::LatchWriter)
        );
        assert!(
            report.take_writer().is_none(),
            "未 join 的 writer 无凭证可发"
        );
        assert!(
            !report
                .stop_trace
                .iter()
                .any(|(kind, _)| *kind == HandleKind::LatchWriter)
        );
        retained.push((report, writer, rig));
    }
    assert_eq!(retained.len(), 2);
    case.complete();
}

/// 增补 §3.4 全真：LatchWriter 在 Ipc 之后、Watchdog 之前收到停止并真实 join；
/// `take_writer` 一次得 `(Ok(Stopped), joined)` 且配对 standby 接受，第二次为 `None`。
#[test]
fn writer_slot_stops_after_ipc_and_credential_is_taken_exactly_once() {
    let Some(case) =
        TestCase::enter("writer_slot_stops_after_ipc_and_credential_is_taken_exactly_once")
    else {
        return;
    };
    let mut rig = Rig::new(required_core(), &SEVEN);
    let mut writer = WriterRig::spawn(Box::new(NoHook));
    let mut gates = ScriptedGates::fixed(true, true);
    let mut report = writer_teardown(&mut rig, &mut writer, &mut gates, 5_000);
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(report.residual.is_empty());
    assert_eq!(
        kinds_of(&report.stop_trace),
        vec![
            HandleKind::ClearRr,
            HandleKind::SetRr,
            HandleKind::Publisher,
            HandleKind::Io,
            HandleKind::Executor,
            HandleKind::Ipc,
            HandleKind::LatchWriter,
            HandleKind::Watchdog,
        ]
    );
    assert!(strictly_increasing(&report.stop_trace));
    assert!(writer.exited());
    let (result, joined) = report.take_writer().expect("唯一 writer 凭证");
    assert_eq!(result.unwrap(), WriterExit::Stopped);
    assert_eq!(joined.writer(), writer.writer_id);
    assert!(report.take_writer().is_none(), "凭证按值只取一次");
    let mut standby = writer.take_standby();
    assert!(standby.after_join(joined).is_ok());
    case.complete();
}
