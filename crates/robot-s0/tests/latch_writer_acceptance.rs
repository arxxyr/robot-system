//! S48 A3e / W2a：latch writer 纯模块的公开 API 验收——真实线程、真实 W0 store、
//! 真实 W1 锁与 W0.1 只读核验；不 spawn 生产第八 worker，不接 daemon。
//!
//! 每个用例独立 tempdir；线程由 RAII 守卫收尾，等待均有界（`--test-threads=1`）。

#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use robot_core::control_authority::{CleanupKind, RepairTarget};
use robot_core::time::{DurationNs, MonoRawNs, RobotClock};
use robot_s0::clear_estop_transaction::{ClearTxnStage, ClearTxnStore, NoGuard, StepPhase};
use robot_s0::latch_lock::{LatchTargetLock, acquire_latch_lock};
use robot_s0::latch_writer::{
    ClearTxnIdentity, DispatchRefused, LastResortError, ObligationView, PairingRefused,
    PersistClient, PersistCommand, PersistCompletion, Phase, RequestOutcome, Settle, TokenRefused,
    WriterConfig, WriterExit, WriterHandle, WriterHook, WriterPairing, WriterRefusal, WriterStop,
    mailbox, spawn_writer,
};

const BASE: MonoRawNs = MonoRawNs(1_000_000_000);
const WAIT: Duration = Duration::from_secs(10);

/// 每次时钟读数时的共享字快照：义务视图、公告接纳时刻、进度阶段（读数发生在写入之前）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClockObservation {
    obligation: ObligationView,
    announce: Option<MonoRawNs>,
    phase: Phase,
    entered_at: MonoRawNs,
}

/// 单调递增的测试时钟；每次读数记录当时的共享字快照（核 Exited / Claiming 发布顺序）。
struct ObservingClock {
    tick: AtomicU64,
    observed: Mutex<Vec<ClockObservation>>,
    obligation: Mutex<Option<Arc<robot_s0::latch_writer::PersistShared>>>,
}

impl ObservingClock {
    fn new() -> Self {
        Self {
            tick: AtomicU64::new(BASE.0 + 1),
            observed: Mutex::new(Vec::new()),
            obligation: Mutex::new(None),
        }
    }
}

impl RobotClock for ObservingClock {
    fn now(&self) -> MonoRawNs {
        if let Some(shared) = self.obligation.lock().unwrap().as_ref() {
            let view = shared.progress.load().unwrap();
            self.observed.lock().unwrap().push(ClockObservation {
                obligation: shared.obligation.view(),
                announce: shared.announce.offered_at(),
                phase: view.phase,
                entered_at: view.entered_at,
            });
        }
        MonoRawNs(self.tick.fetch_add(1, Ordering::Relaxed))
    }

    fn sleep_until(&self, _deadline: robot_core::time::Deadline) {}

    fn sleep_for(&self, _d: DurationNs) {}
}

/// 停止信号；`running()` 在 writer 每个循环顶端被调用，`polls` 计数增长即证明空轮真实发生。
struct Stop {
    running: AtomicBool,
    polls: AtomicU64,
}

impl Stop {
    fn new() -> Self {
        Self {
            running: AtomicBool::new(true),
            polls: AtomicU64::new(0),
        }
    }
}

impl WriterStop for Stop {
    fn running(&self) -> bool {
        self.polls.fetch_add(1, Ordering::AcqRel);
        self.running.load(Ordering::Acquire)
    }
}

/// 有界屏障：首次到达 `at` 时置 `reached`，然后等待 `release`（期限内未放行则 panic，
/// writer 以 join Err 结束，不会无限阻塞）。
struct ParkOnceAt {
    at: ClearTxnStage,
    reached: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    fired: bool,
}

impl WriterHook for ParkOnceAt {
    fn before_step(&mut self, stage: ClearTxnStage) {
        if self.fired || stage != self.at {
            return;
        }
        self.fired = true;
        self.reached.store(true, Ordering::Release);
        let deadline = Instant::now() + WAIT;
        while !self.release.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "屏障未在期限内放行");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// 记录步骤；可在指定步骤 panic（证明真实 join 的 `Err` 路径）。
struct Hook {
    steps: Arc<Mutex<Vec<ClearTxnStage>>>,
    panic_at: Option<ClearTxnStage>,
}

impl WriterHook for Hook {
    fn before_step(&mut self, stage: ClearTxnStage) {
        self.steps.lock().unwrap().push(stage);
        assert!(
            self.panic_at != Some(stage),
            "测试注入：writer 在 {stage:?} 前 panic"
        );
    }
}

/// 在第一步就拒绝的公开守门：核真实文件失败保留 stage / phase。
struct RefuseAll(robot_s0::clear_estop_transaction::InterruptReason);

impl robot_s0::clear_estop_transaction::StepGuard for RefuseAll {
    fn before(
        &mut self,
        _stage: ClearTxnStage,
    ) -> Result<(), robot_s0::clear_estop_transaction::GuardInterrupt> {
        Err(robot_s0::clear_estop_transaction::GuardInterrupt::new(
            self.0,
        ))
    }
}

/// 永不返回的屏障：在指定步骤前写 marker 后一直阻塞（模拟拒停 / 卡死的 writer）。
struct BlockForever {
    at: ClearTxnStage,
    marker: PathBuf,
}

impl WriterHook for BlockForever {
    fn before_step(&mut self, stage: ClearTxnStage) {
        if stage == self.at {
            std::fs::write(&self.marker, b"blocked").unwrap();
            loop {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// R5 子进程模式：在本目录内拉起一个会永久卡在 `CreatePrimary` 的 writer，再经 `Running`
/// 守卫收尾；守卫必须在期限内以明确的进程级失败结束本子进程，绝不无界等待或分离线程。
const R5_CHILD_ENV: &str = "ROBOT_S0_W2A_R5_CHILD_DIR";

fn open(latch: &Path) -> ClearTxnStore {
    ClearTxnStore::open(latch, "xlerobot_left", "left_arm").unwrap()
}

fn txn(original_latched: bool) -> ClearTxnIdentity {
    ClearTxnIdentity {
        txn_id: 99,
        expected_epoch: 4,
        original_latched,
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    latch: PathBuf,
    lock: LatchTargetLock,
}

fn fixture(pair: &[u8]) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let latch = dir.path().join("estop_latch");
    std::fs::write(&latch, pair).unwrap();
    std::fs::write(dir.path().join("estop_latch.bak"), pair).unwrap();
    let lock = acquire_latch_lock(&latch).unwrap();
    Fixture {
        _dir: dir,
        latch,
        lock,
    }
}

impl Fixture {
    fn pair(&self) -> (Vec<u8>, Vec<u8>) {
        (
            std::fs::read(&self.latch).unwrap(),
            std::fs::read(self.latch.with_file_name("estop_latch.bak")).unwrap(),
        )
    }

    fn marker_exists(&self) -> bool {
        self.latch
            .with_file_name("estop_latch.clear_pending")
            .exists()
    }
}

/// 线程守卫（R5）：句柄在确认 `is_finished` 之前始终由守卫持有、不 take；正常与 panic
/// 收尾只对已结束句柄 `join`（有界）；期限内未结束 → 明确的进程级失败（`abort`），
/// 绝不无界等待、不分离活线程当作通过。仅测试策略，不接生产 W2b。
struct Running {
    handle: Option<WriterHandle>,
    stop: Arc<Stop>,
    deadline: Duration,
}

impl Running {
    fn new(handle: WriterHandle, stop: Arc<Stop>) -> Self {
        Self {
            handle: Some(handle),
            stop,
            deadline: WAIT,
        }
    }

    /// 请求停止并有界等待线程结束；期限内未结束 → 进程级失败。返回时句柄已确认结束。
    fn wait_finished(&self, what: &str) {
        self.stop.running.store(false, Ordering::Release);
        let handle = self
            .handle
            .as_ref()
            .expect("句柄在确认结束前始终由守卫持有");
        let deadline = Instant::now() + self.deadline;
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                eprintln!(
                    "latch_writer_acceptance[{what}]: writer 未在期限内退出（{:?}），线程不可强杀，\
                     按测试合同以进程级失败结束，不分离、不无界等待",
                    self.deadline
                );
                std::process::abort();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn join(
        mut self,
    ) -> (
        std::thread::Result<WriterExit>,
        robot_s0::latch_writer::WriterJoined,
    ) {
        self.wait_finished("join");
        let handle = self.handle.take().expect("已确认结束");
        handle.join()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.wait_finished("drop");
            let handle = self.handle.take().expect("已确认结束");
            let (result, _joined) = handle.join();
            // 未被显式 join 的 writer 若 panic，且当前并非正在解栈，则视为测试失败。
            if let Err(payload) = result {
                assert!(
                    std::thread::panicking(),
                    "writer 线程 panic 但用例未显式 join 检查: {payload:?}"
                );
            }
        }
    }
}

fn wait_completion(client: &mut PersistClient) -> PersistCompletion {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(done) = client.try_complete() {
            return done;
        }
        assert!(Instant::now() < deadline, "未在期限内收到完成");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn start(
    f: &Fixture,
    clock: Arc<ObservingClock>,
    hook: impl WriterHook + 'static,
) -> (
    Running,
    PersistClient,
    Arc<robot_s0::latch_writer::PersistShared>,
    robot_s0::latch_writer::StandbyStore,
) {
    let (slot, standby) = WriterPairing::new(open(&f.latch), open(&f.latch), &f.lock, BASE)
        .unwrap()
        .into_parts();
    let shared = Arc::clone(slot.shared());
    *clock.obligation.lock().unwrap() = Some(Arc::clone(&shared));
    let (client, endpoint) = mailbox(Arc::clone(&shared));
    let stop = Arc::new(Stop::new());
    let handle = spawn_writer(
        slot,
        endpoint,
        WriterConfig::default(),
        stop.clone(),
        clock,
        Box::new(hook),
    )
    .unwrap();
    (Running::new(handle, stop), client, shared, standby)
}

/// 真实线程全链：`PrepareClear` → `RepairClear(Latched)` → `RemoveMarker`；单信用；正常 join
/// 产生凭证，standby 经凭证兜底写一次；退出后 `Exited` 已发布。
#[test]
fn writer_thread_runs_clear_chain_and_join_yields_last_resort_token() {
    let f = fixture(b"1\n");
    let clock = Arc::new(ObservingClock::new());
    let steps = Arc::new(Mutex::new(Vec::new()));
    let (running, mut client, shared, mut standby) = start(
        &f,
        clock,
        Hook {
            steps: Arc::clone(&steps),
            panic_at: None,
        },
    );
    let identity = txn(true);
    client
        .try_dispatch(
            PersistCommand::PrepareClear { txn: identity },
            MonoRawNs(BASE.0 + 5),
        )
        .unwrap();
    assert_eq!(
        client
            .try_dispatch(
                PersistCommand::PrepareClear { txn: identity },
                MonoRawNs(BASE.0 + 6)
            )
            .unwrap_err(),
        DispatchRefused::NoCredit,
        "单命令在途"
    );
    assert!(matches!(
        wait_completion(&mut client),
        PersistCompletion::Prepared { txn, .. } if txn == identity
    ));
    assert!(client.has_credit());
    assert_eq!(shared.announce.offered_at(), None, "writer 已认领公告");
    assert!(f.marker_exists());
    assert_eq!(f.pair(), (b"0\n".to_vec(), b"0\n".to_vec()));
    assert_eq!(steps.lock().unwrap().len(), 15);

    client
        .try_dispatch(
            PersistCommand::RepairClear {
                txn: identity,
                target: RepairTarget::Latched,
            },
            MonoRawNs(BASE.0 + 7),
        )
        .unwrap();
    assert!(matches!(
        wait_completion(&mut client),
        PersistCompletion::Repaired {
            target: RepairTarget::Latched,
            ..
        }
    ));
    assert_eq!(f.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
    client
        .try_dispatch(
            PersistCommand::RemoveMarker {
                txn: identity,
                kind: CleanupKind::Repaired(RepairTarget::Latched),
            },
            MonoRawNs(BASE.0 + 8),
        )
        .unwrap();
    assert!(matches!(
        wait_completion(&mut client),
        PersistCompletion::MarkerRemoved { .. }
    ));
    assert!(!f.marker_exists());
    wait_until(
        || shared.progress.load().unwrap().phase == Phase::Idle,
        "投递后回到 Idle",
    );

    let (result, joined) = running.join();
    assert_eq!(result.unwrap(), WriterExit::Stopped);
    assert_eq!(shared.progress.load().unwrap().phase, Phase::Exited);
    assert_eq!(joined.writer(), standby.bound_writer());
    // join 后 standby 兜底：按义务 CAS 协议写一次。
    shared.obligation.request(MonoRawNs(BASE.0 + 900));
    let claim = shared.obligation.begin_write().unwrap();
    standby
        .after_join(joined)
        .unwrap()
        .persist_once(&mut NoGuard)
        .unwrap();
    assert_eq!(shared.obligation.finish_ok(claim).unwrap(), Settle::Settled);
    assert_eq!(f.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
}

/// `EStop` 义务：writer 线程自行服务 Pending（不占命令信用）；清零命令中途到达义务时
/// 在下一 latch 写步骤前中断并保留 Durable 令牌。
#[test]
fn writer_thread_services_obligation_and_interrupts_zeroing() {
    let f = fixture(b"1\n");
    let clock = Arc::new(ObservingClock::new());
    let steps = Arc::new(Mutex::new(Vec::new()));
    let (running, mut client, shared, _standby) = start(
        &f,
        clock,
        Hook {
            steps: Arc::clone(&steps),
            panic_at: None,
        },
    );
    // 义务由线程自行服务。
    std::fs::write(&f.latch, b"0\n").unwrap();
    assert_eq!(
        shared.obligation.request(MonoRawNs(BASE.0 + 3)),
        RequestOutcome::Registered
    );
    wait_until(
        || shared.obligation.is_settled(),
        "义务应被 writer 线程写入并清偿",
    );
    assert_eq!(f.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
    assert!(client.has_credit(), "义务不占命令信用");

    // 清零命令：义务在其执行期间到达 → 中断保留凭证。
    let identity = txn(true);
    let request_at_backup = {
        let shared = Arc::clone(&shared);
        let steps = Arc::clone(&steps);
        std::thread::spawn(move || {
            wait_until(
                || {
                    steps
                        .lock()
                        .unwrap()
                        .contains(&ClearTxnStage::SyncMarkerDirectory)
                },
                "标记应已 durable",
            );
            shared.obligation.request(MonoRawNs(BASE.0 + 50))
        })
    };
    client
        .try_dispatch(
            PersistCommand::PrepareClear { txn: identity },
            MonoRawNs(BASE.0 + 4),
        )
        .unwrap();
    let completion = wait_completion(&mut client);
    let outcome = request_at_backup.join().unwrap();
    assert!(matches!(
        outcome,
        RequestOutcome::Registered | RequestOutcome::Merged
    ));
    assert_zeroing_interrupted_or_completed(completion);
    wait_until(|| shared.obligation.is_settled(), "义务最终被清偿");
    assert_eq!(f.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
    // 义务未清偿时拒绝新的清零；清偿后 Repair(Latched) 继续。
    client
        .try_dispatch(
            PersistCommand::RepairClear {
                txn: identity,
                target: RepairTarget::Latched,
            },
            MonoRawNs(BASE.0 + 60),
        )
        .unwrap();
    assert!(matches!(
        wait_completion(&mut client),
        PersistCompletion::Repaired { .. }
    ));
    client
        .try_dispatch(
            PersistCommand::RemoveMarker {
                txn: identity,
                kind: CleanupKind::Finalize,
            },
            MonoRawNs(BASE.0 + 61),
        )
        .unwrap();
    assert!(matches!(
        wait_completion(&mut client),
        PersistCompletion::MarkerRemoved { .. }
    ));
    let (result, _joined) = running.join();
    assert_eq!(result.unwrap(), WriterExit::Stopped);
}

/// 清零期间到达义务：要么在某个 latch 写步骤前以 Before 相中断并保留 Durable 令牌，要么
/// 义务晚于最后一步到达而清零已完成（随后由线程服务义务）。
fn assert_zeroing_interrupted_or_completed(completion: PersistCompletion) {
    match completion {
        PersistCompletion::PrepareFailed {
            stage,
            phase: StepPhase::Before,
            token,
            ..
        } => {
            assert!(
                robot_s0::latch_writer::writes_latch_value(stage),
                "只在 latch 写步骤前中断: {stage:?}"
            );
            assert_eq!(token, robot_s0::latch_writer::PrepareTokenKind::Durable);
        }
        PersistCompletion::Prepared { .. } => {}
        other => panic!("意外完成 {other:?}"),
    }
}

/// panic 路：writer 在义务写入中 panic → 解栈先恢复 Pending（起点不变）再发布 `Exited`；
/// 真实 join 得 `Err` 仍产生唯一 `WriterJoined`，standby 据此兜底写一次。
#[test]
fn writer_panic_restores_pending_before_exited_and_join_err_still_yields_token() {
    let f = fixture(b"0\n");
    let clock = Arc::new(ObservingClock::new());
    let steps = Arc::new(Mutex::new(Vec::new()));
    let (running, _client, shared, mut standby) = start(
        &f,
        Arc::clone(&clock),
        Hook {
            steps: Arc::clone(&steps),
            panic_at: Some(ClearTxnStage::CreateBackup),
        },
    );
    assert_eq!(
        shared.obligation.request(MonoRawNs(BASE.0 + 3)),
        RequestOutcome::Registered
    );
    wait_until(
        || shared.progress.load().unwrap().phase == Phase::Exited,
        "panic 后必须发布 Exited",
    );
    let view = shared.obligation.view();
    assert!(view.pending && !view.active, "解栈恢复 Pending");
    assert_eq!(
        view.unsettled_since,
        Some(MonoRawNs(BASE.0 + 3)),
        "起点不变"
    );
    // Exited 戳的那次时钟读数发生在责任恢复之后。
    let last = clock.observed.lock().unwrap().last().unwrap().obligation;
    assert!(
        last.pending && !last.active,
        "发布 Exited 时责任已恢复 Pending: {last:?}"
    );
    assert_eq!(
        f.pair(),
        (b"1\n".to_vec(), b"0\n".to_vec()),
        "primary 已写 1，backup 未写"
    );

    let (result, joined) = running.join();
    assert!(result.is_err(), "panic 以 join Err 表达");
    assert_eq!(joined.writer(), standby.bound_writer());
    let claim = shared.obligation.begin_write().unwrap();
    standby
        .after_join(joined)
        .unwrap()
        .persist_once(&mut NoGuard)
        .unwrap();
    assert_eq!(shared.obligation.finish_ok(claim).unwrap(), Settle::Settled);
    assert_eq!(f.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
}

/// 错配：A 的 join 凭证不能授权 B 的 standby；错配后 A 的义务仍未清偿；真实文件失败
/// 保留 stage / phase。
#[test]
fn foreign_join_credential_is_refused_and_obligation_stays_unsettled() {
    let a = fixture(b"1\n");
    let b = fixture(b"1\n");
    let clock = Arc::new(ObservingClock::new());
    let (running_a, _client_a, shared_a, mut standby_a) = start(
        &a,
        Arc::clone(&clock),
        Hook {
            steps: Arc::new(Mutex::new(Vec::new())),
            panic_at: None,
        },
    );
    let (slot_b, mut standby_b) = WriterPairing::new(open(&b.latch), open(&b.latch), &b.lock, BASE)
        .unwrap()
        .into_parts();
    drop(slot_b);
    shared_a.obligation.request(MonoRawNs(BASE.0 + 3));
    wait_until(|| shared_a.obligation.is_settled(), "A 的义务先由线程清偿");
    let (result, joined_a) = running_a.join();
    assert_eq!(result.unwrap(), WriterExit::Stopped);
    // R4：真实 join 之后才登记第二笔义务——此时没有 writer 竞争清偿，前提确定。
    assert_eq!(
        shared_a.obligation.request(MonoRawNs(BASE.0 + 400)),
        RequestOutcome::Registered
    );
    assert!(!shared_a.obligation.is_settled());
    let before_b = b.pair();
    let refused = standby_b.after_join(joined_a).unwrap_err();
    assert_eq!(refused, TokenRefused::WriterMismatch);
    assert_eq!(b.pair(), before_b, "错配不写文件");
    assert!(!shared_a.obligation.is_settled(), "错配不清责任");
    // 另一个 writer 实例（同目标）的凭证同样不属于 standby A：实例绑定，不是目标绑定。
    let (running_a2, _c, _s, _sb) = start(
        &a,
        clock,
        Hook {
            steps: Arc::new(Mutex::new(Vec::new())),
            panic_at: None,
        },
    );
    let (_, joined_a2) = running_a2.join();
    let error = standby_a.after_join(joined_a2).unwrap_err();
    assert_eq!(
        error,
        TokenRefused::WriterMismatch,
        "第二个 writer 的凭证也不属于 standby A"
    );
}

/// 配对：同目录 A 的锁配 B 的 store → `LockDirectoryMismatch`；同目录不同 basename →
/// `LockTargetMismatch` / `StoreTargetMismatch`，各自可独立持锁。
#[test]
fn pairing_refuses_directory_and_target_mismatch() {
    let a = fixture(b"0\n");
    let b = fixture(b"0\n");
    assert_eq!(
        WriterPairing::new(open(&b.latch), open(&b.latch), &a.lock, BASE).unwrap_err(),
        PairingRefused::LockDirectoryMismatch
    );
    let other = a.latch.with_file_name("gripper_latch");
    std::fs::write(&other, b"0\n").unwrap();
    let other_lock = acquire_latch_lock(&other).unwrap();
    assert_eq!(
        WriterPairing::new(open(&a.latch), open(&a.latch), &other_lock, BASE).unwrap_err(),
        PairingRefused::LockTargetMismatch
    );
    assert_eq!(
        WriterPairing::new(open(&a.latch), open(&other), &a.lock, BASE).unwrap_err(),
        PairingRefused::StoreTargetMismatch
    );
    assert!(WriterPairing::new(open(&other), open(&other), &other_lock, BASE).is_ok());
}

/// 真实文件失败经令牌上报为 `Stage(StageError)`，stage / phase 不扁平化。
#[test]
fn last_resort_real_failure_keeps_stage_error() {
    let f = fixture(b"0\n");
    let clock = Arc::new(ObservingClock::new());
    let (running, _client, _shared, mut standby) = start(
        &f,
        clock,
        Hook {
            steps: Arc::new(Mutex::new(Vec::new())),
            panic_at: None,
        },
    );
    let (_, joined) = running.join();
    let error = standby
        .after_join(joined)
        .unwrap()
        .persist_once(&mut RefuseAll(
            robot_s0::clear_estop_transaction::InterruptReason::EStopObligationPending,
        ))
        .unwrap_err();
    let LastResortError::Stage(stage) = error else {
        panic!("真实文件失败必须是 Stage");
    };
    assert_eq!(stage.stage, ClearTxnStage::CreatePrimary);
    assert_eq!(stage.phase, StepPhase::Before);
    assert_eq!(f.pair(), (b"0\n".to_vec(), b"0\n".to_vec()));
}

/// IPC 端断开后 writer 继续服务义务，收到停止后以 `StoppedAfterIpcDisconnect` 退出。
#[test]
fn writer_survives_ipc_disconnect_until_stopped() {
    let f = fixture(b"0\n");
    let clock = Arc::new(ObservingClock::new());
    let (running, client, shared, _standby) = start(
        &f,
        clock,
        Hook {
            steps: Arc::new(Mutex::new(Vec::new())),
            panic_at: None,
        },
    );
    drop(client);
    shared.obligation.request(MonoRawNs(BASE.0 + 3));
    wait_until(|| shared.obligation.is_settled(), "IPC 断开后义务仍被服务");
    assert_eq!(f.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
    let (result, _) = running.join();
    assert_eq!(result.unwrap(), WriterExit::StoppedAfterIpcDisconnect);
}

/// 写前拒绝的公开可见形态：`WriterRefusal` 与 `DispatchRefused` 在真实线程下的往返。
#[test]
fn refusals_round_trip_through_mailbox() {
    let f = fixture(b"1\n");
    let clock = Arc::new(ObservingClock::new());
    let (running, mut client, _shared, _standby) = start(
        &f,
        clock,
        Hook {
            steps: Arc::new(Mutex::new(Vec::new())),
            panic_at: None,
        },
    );
    client
        .try_dispatch(
            PersistCommand::ResyncRemoval { txn: txn(true) },
            MonoRawNs(BASE.0 + 5),
        )
        .unwrap();
    assert_eq!(
        wait_completion(&mut client),
        PersistCompletion::Refused {
            txn: Some(txn(true)),
            reason: WriterRefusal::NoTransaction,
        }
    );
    assert!(client.has_credit(), "Refused 也归还信用");
    let (result, _) = running.join();
    assert_eq!(result.unwrap(), WriterExit::Stopped);
}

/// R1 回归：真实义务成功后 writer 必须回到 `Idle`（IPC 连通与断开两路），空转不刷新
/// 进入时刻；假时钟推进 3 s 后 `evaluate` 不得再有旧 `FileStep` 候选。
#[test]
fn obligation_success_returns_writer_to_idle_without_stale_file_step_candidate() {
    for ipc_connected in [true, false] {
        let f = fixture(b"0\n");
        let clock = Arc::new(ObservingClock::new());
        let (running, client, shared, _standby) = start(
            &f,
            clock,
            Hook {
                steps: Arc::new(Mutex::new(Vec::new())),
                panic_at: None,
            },
        );
        // 连通路保持 client 存活到用例结束；断开路立即丢弃（writer 侧观察到 Disconnected）。
        let _client_keep = if ipc_connected {
            Some(client)
        } else {
            drop(client);
            None
        };
        assert_eq!(
            shared.obligation.request(MonoRawNs(BASE.0 + 3)),
            RequestOutcome::Registered
        );
        wait_until(|| shared.obligation.is_settled(), "义务被线程清偿");
        wait_until(
            || shared.progress.load().unwrap().phase == Phase::Idle,
            &format!("ipc_connected={ipc_connected}: 义务成功后必须回到 Idle"),
        );
        let view = shared.progress.load().unwrap();
        let inputs = robot_s0::latch_writer::Inputs {
            progress: view,
            announce: shared.announce.offered_at(),
            obligation: shared.obligation.view(),
            armed_at: BASE,
            shutdown_deadline: None,
            now: MonoRawNs(view.entered_at.0 + 3_000_000_000),
            budgets: robot_s0::latch_writer::Budgets::default(),
        };
        let decision = robot_s0::latch_writer::evaluate(&inputs);
        assert!(
            decision.candidates[0].is_none(),
            "ipc_connected={ipc_connected}: 不得残留 FileStep 候选 {decision:?}"
        );
        assert_eq!(decision.fire, None, "ipc_connected={ipc_connected}");
        // 空转不刷新：若干个 idle 轮后进入时刻不变（否则 pickup 截止会被洗掉）。
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(
            shared.progress.load().unwrap(),
            view,
            "空转不得重复刷新 Idle"
        );
        let (result, _) = running.join();
        assert!(result.is_ok());
    }
}

/// 目录权限恢复守卫：析构时把目录改回 `0o750`（无论用例成功、断言失败或等待超时）。
struct RestoreDirMode(PathBuf);

impl Drop for RestoreDirMode {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o750));
    }
}

/// 一次性在指定步骤前把 latch 目录改为只读（制造真实 `EACCES` 文件失败），由测试恢复。
struct DenyDirOnceAt {
    at: ClearTxnStage,
    dir: PathBuf,
    fired: bool,
}

impl WriterHook for DenyDirOnceAt {
    fn before_step(&mut self, stage: ClearTxnStage) {
        if !self.fired && stage == self.at {
            self.fired = true;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        }
    }
}

/// 权限恢复 RAII 本身：守卫存活期间目录可被置为只读（0500），析构后恢复 0750，随后临时
/// 目录能正常清理；对 root 同样成立（只核模式位，不依赖 EACCES）。
#[test]
fn restore_dir_mode_restores_permissions_on_drop() {
    let dir = tempfile::TempDir::new().unwrap();
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    {
        let _restore = RestoreDirMode(dir.path().to_path_buf());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        assert_eq!(mode(dir.path()), 0o500);
    }
    assert_eq!(mode(dir.path()), 0o750, "守卫析构后恢复目录权限");
    dir.close().unwrap();
}

/// R1 回归：义务写入失败 → `RetryBackoff`，恢复后成功 → `Idle`；随后登记的新 Pending
/// 由 writer 再次服务后仍回到 `Idle`，且旧 `FileStep` 不残留。
#[test]
fn obligation_failure_then_success_ends_idle() {
    // SAFETY：geteuid 无参数、无副作用、不会失败。
    if unsafe { libc::geteuid() } == 0 {
        // root 不受目录权限约束，构造不出真实 EACCES：直接返回，不做断言。
        return;
    }
    let f = fixture(b"0\n");
    let dir = f.latch.parent().unwrap().to_path_buf();
    // RAII：局部按声明逆序析构——`running`（后声明）先停止并 join writer，随后本守卫恢复
    // 目录权限，最后 `f` 清理临时目录；断言 / 等待失败时同样成立。
    let _restore_mode = RestoreDirMode(dir.clone());
    let clock = Arc::new(ObservingClock::new());
    let (slot, _standby) = WriterPairing::new(open(&f.latch), open(&f.latch), &f.lock, BASE)
        .unwrap()
        .into_parts();
    let shared = Arc::clone(slot.shared());
    *clock.obligation.lock().unwrap() = Some(Arc::clone(&shared));
    let (_client, endpoint) = mailbox(Arc::clone(&shared));
    let stop = Arc::new(Stop::new());
    let hook = DenyDirOnceAt {
        at: ClearTxnStage::CreatePrimary,
        dir: dir.clone(),
        fired: false,
    };
    let handle = spawn_writer(
        slot,
        endpoint,
        WriterConfig::default(),
        stop.clone(),
        clock,
        Box::new(hook),
    )
    .unwrap();
    let running = Running::new(handle, Arc::clone(&stop));
    shared.obligation.request(MonoRawNs(BASE.0 + 3));
    // 第一次服务在 CreatePrimary 的真实 openat 上得到 EACCES：失败 → RetryBackoff，责任恢复 Pending。
    wait_until(
        || shared.progress.load().unwrap().phase == Phase::RetryBackoff,
        "真实文件失败后必须进入 RetryBackoff",
    );
    assert!(shared.obligation.has_pending(), "失败后责任恢复 Pending");
    // 恢复目录权限：writer 退出退避后重试成功并清偿，最终回到 Idle。
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750)).unwrap();
    wait_until(|| shared.obligation.is_settled(), "恢复后义务被清偿");
    wait_until(
        || shared.progress.load().unwrap().phase == Phase::Idle,
        "成功后必须回到 Idle",
    );
    let view = shared.progress.load().unwrap();
    let decision = robot_s0::latch_writer::evaluate(&robot_s0::latch_writer::Inputs {
        progress: view,
        announce: shared.announce.offered_at(),
        obligation: shared.obligation.view(),
        armed_at: BASE,
        shutdown_deadline: None,
        now: MonoRawNs(view.entered_at.0 + 3_000_000_000),
        budgets: robot_s0::latch_writer::Budgets::default(),
    });
    assert_eq!(
        decision.fire, None,
        "成功后不得残留 FileStep / RetryBackoff 触发: {decision:?}"
    );
    assert_eq!(f.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
    let (result, _) = running.join();
    assert_eq!(result.unwrap(), WriterExit::Stopped);
}

/// R4 探针：在请求停止之前登记的第二笔义务会被仍在运行的 writer 清偿——证明旧用例
/// "停止后到达" 的前提不成立（因此错配用例必须在真实 join 之后再登记义务）。
#[test]
fn request_before_stop_can_be_settled_by_running_writer() {
    let f = fixture(b"1\n");
    let clock = Arc::new(ObservingClock::new());
    let (running, _client, shared, _standby) = start(
        &f,
        clock,
        Hook {
            steps: Arc::new(Mutex::new(Vec::new())),
            panic_at: None,
        },
    );
    shared.obligation.request(MonoRawNs(BASE.0 + 3));
    wait_until(|| shared.obligation.is_settled(), "第一笔被清偿");
    shared.obligation.request(MonoRawNs(BASE.0 + 400));
    wait_until(
        || shared.obligation.is_settled(),
        "停止前登记的第二笔同样会被运行中的 writer 清偿",
    );
    let (result, _) = running.join();
    assert_eq!(result.unwrap(), WriterExit::Stopped);
}

/// R5 子进程入口：父进程模式（无环境变量）直接返回。
#[test]
fn running_guard_deadline_child() {
    let Some(dir) = std::env::var_os(R5_CHILD_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let latch = dir.join("estop_latch");
    std::fs::write(&latch, b"0\n").unwrap();
    std::fs::write(dir.join("estop_latch.bak"), b"0\n").unwrap();
    let lock = acquire_latch_lock(&latch).unwrap();
    let (slot, _standby) = WriterPairing::new(open(&latch), open(&latch), &lock, BASE)
        .unwrap()
        .into_parts();
    let shared = Arc::clone(slot.shared());
    let (_client, endpoint) = mailbox(Arc::clone(&shared));
    let stop = Arc::new(Stop::new());
    let handle = spawn_writer(
        slot,
        endpoint,
        WriterConfig::default(),
        stop.clone(),
        Arc::new(ObservingClock::new()),
        Box::new(BlockForever {
            at: ClearTxnStage::CreatePrimary,
            marker: dir.join("blocked"),
        }),
    )
    .unwrap();
    let mut running = Running::new(handle, stop);
    // 子进程用短期限，父进程据此有界判定；策略与 WAIT 相同，只缩短等待。
    running.deadline = Duration::from_secs(2);
    shared.obligation.request(MonoRawNs(BASE.0 + 3));
    wait_until(|| dir.join("blocked").exists(), "writer 应已卡在屏障");
    // 守卫 Drop 收尾：writer 永不退出 → 必须在期限内以进程级失败结束，不得无界等待或分离。
    drop(running);
    std::process::exit(0);
}

/// R5 回归：孤立子进程里的永不退出 writer，经 `Running` 收尾后必须在有限时间内以失败
/// 退出（进程级明确失败）；父进程有界等待并回收子进程，超限即 kill 并判失败。
#[test]
fn running_guard_fails_the_process_within_bound_when_writer_never_stops() {
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "running_guard_deadline_child",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env(R5_CHILD_ENV, dir.path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    wait_until(
        || dir.path().join("blocked").exists(),
        "子进程 writer 应已卡在屏障",
    );
    // 子进程守卫期限 2 s + 回收余量；超过即 kill 并判失败。
    let bound = Duration::from_secs(7);
    let deadline = Instant::now() + bound;
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "Running 守卫没有在 {bound:?} 内以进程级失败结束卡死的 writer（无界等待 / 分离线程）"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stderr = String::new();
    std::io::Read::read_to_string(child.0.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    assert!(
        !status.success(),
        "卡死 writer 的收尾必须是失败，不能假成功: {stderr}"
    );
    assert!(
        stderr.contains("未在期限内退出"),
        "失败原因必须明确指出期限: {stderr}"
    );
    assert!(stderr.contains("[drop]"), "本例走 Drop 收尾路径: {stderr}");
}

/// R6 回归：取到命令后先发布 `Claiming` 再认领公告。命令派发后 writer 的第一次时钟读数
/// 就是 `Claiming` 戳（读数先于写入），此刻公告必须仍在（任一暂停点至少保留公告或
/// Claiming 之一）；随后公告恰被认领一次。
#[test]
fn claiming_is_published_before_announce_is_claimed() {
    let f = fixture(b"1\n");
    let clock = Arc::new(ObservingClock::new());
    let (running, mut client, shared, _standby) = start(
        &f,
        Arc::clone(&clock),
        Hook {
            steps: Arc::new(Mutex::new(Vec::new())),
            panic_at: None,
        },
    );
    let calls_before = clock.observed.lock().unwrap().len();
    let identity = txn(true);
    client
        .try_dispatch(
            PersistCommand::PrepareClear { txn: identity },
            MonoRawNs(BASE.0 + 5),
        )
        .unwrap();
    assert!(matches!(
        wait_completion(&mut client),
        PersistCompletion::Prepared { .. }
    ));
    let observed = clock.observed.lock().unwrap().clone();
    let first = observed
        .get(calls_before)
        .copied()
        .expect("派发后至少一次时钟读数（Claiming 戳）");
    assert_eq!(
        first.phase,
        Phase::Idle,
        "第一次读数发生在 Claiming 写入之前"
    );
    assert_eq!(
        first.announce,
        Some(MonoRawNs(BASE.0 + 5)),
        "Claiming 戳读数时公告必须仍在（先发布 Claiming 再认领公告）: {first:?}"
    );
    assert_eq!(shared.announce.offered_at(), None, "公告随后恰被认领一次");
    let (result, _) = running.join();
    assert_eq!(result.unwrap(), WriterExit::Stopped);
}

/// R7：真实 writer 在命令执行中 panic → 完成永不到达、端点随线程丢弃；client 先 `try_complete`
/// 观察到 `Disconnected` 并锁存 `alive=false`，在飞信用不归还；真实 join 得 `Err`。
#[test]
fn client_latches_dead_after_writer_panics_with_in_flight_command() {
    let f = fixture(b"1\n");
    let clock = Arc::new(ObservingClock::new());
    let (running, mut client, _shared, _standby) = start(
        &f,
        clock,
        Hook {
            steps: Arc::new(Mutex::new(Vec::new())),
            panic_at: Some(ClearTxnStage::SyncMarkerFile),
        },
    );
    client
        .try_dispatch(
            PersistCommand::PrepareClear { txn: txn(true) },
            MonoRawNs(BASE.0 + 5),
        )
        .unwrap();
    assert!(client.alive(), "尚未观察到断开");
    wait_until(
        || {
            client.try_complete();
            !client.alive()
        },
        "writer panic 后 client 必须观察到断开",
    );
    assert!(!client.has_credit(), "在飞信用不归还");
    assert_eq!(
        client
            .try_dispatch(
                PersistCommand::ResyncRemoval { txn: txn(true) },
                MonoRawNs(BASE.0 + 6),
            )
            .unwrap_err(),
        DispatchRefused::NoCredit
    );
    let (result, _) = running.join();
    assert!(result.is_err(), "panic 以 join Err 表达");
}

/// R7：正常退出：writer 线程 join 后端点丢弃，client 轮询观察到断开；未消耗的信用保留。
#[test]
fn client_latches_dead_after_normal_writer_exit() {
    let f = fixture(b"1\n");
    let clock = Arc::new(ObservingClock::new());
    let (running, mut client, _shared, _standby) = start(
        &f,
        clock,
        Hook {
            steps: Arc::new(Mutex::new(Vec::new())),
            panic_at: None,
        },
    );
    let (result, _) = running.join();
    assert_eq!(result.unwrap(), WriterExit::Stopped);
    assert!(client.alive(), "join 本身不改 client：须先轮询");
    assert_eq!(client.try_complete(), None);
    assert!(!client.alive());
    assert!(client.has_credit(), "无在飞命令：信用保留");
}

/// R7：缓冲完成后断开：writer 执行完命令并投递，随后被停止退出；client 先取到完成
/// （信用归还），下一轮才观察到断开。
#[test]
fn client_returns_buffered_completion_before_reporting_disconnect() {
    let f = fixture(b"1\n");
    let clock = Arc::new(ObservingClock::new());
    let (running, mut client, shared, _standby) = start(
        &f,
        clock,
        Hook {
            steps: Arc::new(Mutex::new(Vec::new())),
            panic_at: None,
        },
    );
    let identity = txn(true);
    client
        .try_dispatch(
            PersistCommand::PrepareClear { txn: identity },
            MonoRawNs(BASE.0 + 5),
        )
        .unwrap();
    // 不取完成：等 writer 投递完成并回到 Idle，再停止它。读取顺序（P2 修复）：本线程已发布
    // 公告，先读到公告 None（Acquire）→ writer 的 Claiming 先行写入对随后的进度读取可见，
    // 不可能再读到旧 Idle；再要求完整进度字前后双读一致，只接受"认领后的新 Idle"。
    wait_until(
        || {
            if shared.announce.offered_at().is_some() {
                return false;
            }
            let first = shared.progress.raw();
            let view = shared.progress.load().unwrap();
            view.phase == Phase::Idle && shared.progress.raw() == first
        },
        "writer 应已认领公告、投递完成并回到 Idle",
    );
    let (result, _) = running.join();
    assert_eq!(result.unwrap(), WriterExit::Stopped);
    assert!(client.alive());
    assert!(
        matches!(
            client.try_complete(),
            Some(PersistCompletion::Prepared { .. })
        ),
        "先返回缓冲中的完成"
    );
    assert!(client.has_credit(), "缓冲完成归还信用");
    assert!(client.alive(), "取到完成的这一轮尚未报告断开");
    assert_eq!(client.try_complete(), None);
    assert!(!client.alive(), "随后的空 / 断开轮锁存死亡");
}

/// 假时钟读数序列的判据：每次读数记录写入前的阶段，`Idle` 写入只能从"上一读数阶段 == Idle"
/// 推断。相邻两次读数都为 Idle 即空转刷新；Idle 写入次数 = 读数中 Idle 的个数 +（终态 Idle 的 1）。
fn assert_idle_only_follows_real_work(tail: &[ClockObservation], expected_idle_writes: usize) {
    let washes = tail
        .windows(2)
        .filter(|pair| pair[0].phase == Phase::Idle && pair[1].phase == Phase::Idle)
        .count();
    assert_eq!(washes, 0, "不得出现 Idle→Idle 的空转刷新: {tail:?}");
    let idle_reads = tail.iter().filter(|o| o.phase == Phase::Idle).count();
    assert_eq!(
        idle_reads + 1,
        expected_idle_writes,
        "每段真实工作各发布一次 Idle（含终态）: {tail:?}"
    );
}

/// 取命令的 `Claiming` 读数：此刻进度是义务 A 写完后唯一的 Idle 戳，公告仍是原 `BASE+60`，
/// 后续 Pending 的连续未清偿起点仍是 `BASE+3`（Merged 不换起点，A 的 Idle 转换也不刷新）；
/// 据此推导的 pickup 截止 = `max(min(公告, 起点), Idle 进入时刻, armed) + 500 ms`。
fn assert_pickup_source_preserved_at_command_claiming(tail: &[ClockObservation]) {
    let read = tail
        .iter()
        .find(|o| o.phase == Phase::Idle && o.announce == Some(MonoRawNs(BASE.0 + 60)))
        .unwrap_or_else(|| panic!("取命令的 Claiming 读数应看到 Idle + 原公告: {tail:?}"));
    assert!(
        read.obligation.pending && !read.obligation.active,
        "{read:?}"
    );
    assert_eq!(
        read.obligation.unsettled_since,
        Some(MonoRawNs(BASE.0 + 3)),
        "后续 Pending 的起点不因 A 完成 / Idle 转换刷新"
    );
    let inputs = robot_s0::latch_writer::Inputs {
        progress: robot_s0::latch_writer::ProgressView {
            phase: Phase::Idle,
            entered_at: read.entered_at,
        },
        announce: read.announce,
        obligation: read.obligation,
        armed_at: BASE,
        shutdown_deadline: None,
        now: read.entered_at,
        budgets: robot_s0::latch_writer::Budgets::default(),
    };
    assert_eq!(
        robot_s0::latch_writer::evaluate(&inputs).candidates[1].map(|c| c.deadline),
        Some(MonoRawNs(read.entered_at.0.max(BASE.0 + 3) + 500_000_000)),
        "pickup 截止由原起点与该唯一 Idle 戳推导"
    );
}

/// 稳定的 Idle 戳推导：推进 3 s 不产生触发；若此刻登记公告，pickup 截止 =
/// `max(accepted, idle_entered, armed) + 500 ms`，不会被空转推后。
fn assert_pickup_deadline_derives_from_stable_idle(
    idle_view: robot_s0::latch_writer::ProgressView,
    obligation: ObligationView,
) {
    let base = robot_s0::latch_writer::Inputs {
        progress: idle_view,
        announce: None,
        obligation,
        armed_at: BASE,
        shutdown_deadline: None,
        now: MonoRawNs(idle_view.entered_at.0 + 3_000_000_000),
        budgets: robot_s0::latch_writer::Budgets::default(),
    };
    let decision = robot_s0::latch_writer::evaluate(&base);
    assert_eq!(decision.fire, None, "{decision:?}");
    let with_announce = robot_s0::latch_writer::Inputs {
        announce: Some(MonoRawNs(idle_view.entered_at.0 + 1_000)),
        now: idle_view.entered_at,
        ..base
    };
    assert_eq!(
        robot_s0::latch_writer::evaluate(&with_announce).candidates[1].map(|c| c.deadline),
        Some(MonoRawNs(idle_view.entered_at.0 + 1_000 + 500_000_000)),
        "pickup 截止由稳定的 Idle 戳与公告时刻推导"
    );
}

/// R1 有界同步反例（真实 `run_writer` + 有界屏障 + 假时钟）。可执行同步点与断言：
///
/// 1. 义务 A 停在 `CreateBackup` 屏障（`Active`）；此时登记 B（`Merged`，起点仍 `BASE+3`）
///    并派发 `PrepareClear`（公告 `BASE+60`），随后放行。
/// 2. 按生产循环顺序：A 写完（B 仍 Pending）→ 发布唯一 Idle → 取命令（`Claiming` 读数看到
///    Idle + 原公告 + 起点 `BASE+3`）→ 义务未清偿，完成值只能是
///    `Refused(EStopPersistOutstanding)`（不清掉新义务、不改生产逻辑），信用归还。
/// 3. Delivering → Idle → 服务 B → Idle：三段真实工作 = 三次 Idle 写入，读数序列绝无
///    Idle→Idle 的空转刷新。
/// 4. `running()` 计数再增 ≥5（真实空轮）后进度字纹丝不动；推进 3 s 无触发，pickup 只能由
///    该稳定 Idle 戳推导。
/// 5. 清偿后同一命令真实 `Prepared`（证明先前拒绝仅因未清偿义务），Idle 才因真实工作换戳；
///    有界 join → `Stopped`；义务写入后文件 `1/1`，`Prepared` 后按 W0 事务真实清零 `0/0` 且
///    marker 存在。
#[test]
fn idle_is_stamped_once_per_completed_work_and_never_refreshed_by_idle_loops() {
    let f = fixture(b"0\n");
    let clock = Arc::new(ObservingClock::new());
    let reached = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let (running, mut client, shared, _standby) = start(
        &f,
        Arc::clone(&clock),
        ParkOnceAt {
            at: ClearTxnStage::CreateBackup,
            reached: Arc::clone(&reached),
            release: Arc::clone(&release),
            fired: false,
        },
    );

    // 同步点 1：A 停在屏障；登记 B 与命令。
    assert_eq!(
        shared.obligation.request(MonoRawNs(BASE.0 + 3)),
        RequestOutcome::Registered
    );
    wait_until(
        || reached.load(Ordering::Acquire),
        "writer 应停在 CreateBackup 屏障",
    );
    let parked = shared.obligation.view();
    assert!(parked.active && !parked.pending, "{parked:?}");
    assert_eq!(
        shared.obligation.request(MonoRawNs(BASE.0 + 50)),
        RequestOutcome::Merged
    );
    client
        .try_dispatch(
            PersistCommand::PrepareClear { txn: txn(true) },
            MonoRawNs(BASE.0 + 60),
        )
        .unwrap();
    assert_eq!(shared.announce.offered_at(), Some(MonoRawNs(BASE.0 + 60)));
    let observed_before_release = clock.observed.lock().unwrap().len();
    release.store(true, Ordering::Release);

    // 同步点 2：完成值与可构造前置一致——义务未清偿即拒绝，信用归还。
    assert_eq!(
        wait_completion(&mut client),
        PersistCompletion::Refused {
            txn: Some(txn(true)),
            reason: WriterRefusal::EStopPersistOutstanding,
        }
    );
    assert!(client.has_credit(), "拒绝亦归还信用");

    // 同步点 3：三段真实工作后回到 Idle 且义务清偿；读数序列核每个 Idle 都紧跟真实工作。
    wait_until(
        || {
            shared.announce.offered_at().is_none()
                && shared.obligation.is_settled()
                && shared.progress.load().unwrap().phase == Phase::Idle
        },
        "三段真实工作后回到 Idle",
    );
    assert_eq!(
        f.pair(),
        (b"1\n".to_vec(), b"1\n".to_vec()),
        "A、B 均真实落盘"
    );
    let observed = clock.observed.lock().unwrap().clone();
    let tail = &observed[observed_before_release..];
    assert_idle_only_follows_real_work(tail, 3);
    assert_pickup_source_preserved_at_command_claiming(tail);

    // 同步点 4：空轮不刷新。
    let idle_raw = shared.progress.raw();
    let idle_view = shared.progress.load().unwrap();
    let polls = running.stop.polls.load(Ordering::Acquire);
    wait_until(
        || running.stop.polls.load(Ordering::Acquire) >= polls + 5,
        "writer 应真实空转 ≥5 轮",
    );
    assert_eq!(
        shared.progress.raw(),
        idle_raw,
        "空轮不得刷新 Idle 进入时刻"
    );
    assert_pickup_deadline_derives_from_stable_idle(idle_view, shared.obligation.view());

    // 同步点 5：清偿后同一命令真实完成，Idle 只因真实工作换戳。
    client
        .try_dispatch(
            PersistCommand::PrepareClear { txn: txn(true) },
            MonoRawNs(BASE.0 + 70),
        )
        .unwrap();
    assert!(matches!(
        wait_completion(&mut client),
        PersistCompletion::Prepared { .. }
    ));
    wait_until(
        || shared.progress.load().unwrap().phase == Phase::Idle,
        "命令完成后回到 Idle",
    );
    assert_ne!(shared.progress.raw(), idle_raw, "只有真实工作才换 Idle 戳");
    let (result, _) = running.join();
    assert_eq!(result.unwrap(), WriterExit::Stopped);
    assert!(f.marker_exists(), "Prepared 后 marker 真实存在");
    assert_eq!(
        f.pair(),
        (b"0\n".to_vec(), b"0\n".to_vec()),
        "Prepared 按 W0 事务真实清零"
    );
}
