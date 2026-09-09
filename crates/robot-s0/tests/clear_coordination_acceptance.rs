// W3a 公开验收：真实 W0 store、W1 锁、W2a mailbox/writer 与 A2；没有生产调用。

use robot_core::control_authority::{
    AuthorityInit, ControlAuthority, InhibitSet, Phase as AuthorityPhase, RepairTarget,
};
use robot_core::executor_ctrl::OutputSafetyState;
use robot_core::time::{Deadline, DurationNs, MonoRawNs, RobotClock};
use robot_s0::clear_coordination::{
    AdvanceInputs, ClearCoordination, ClearEvent, ClearOutcome, ClearStage, DispatchIntent,
    DispatchState, HeldPermit, RetiredView, RetirementRecord, RetryPolicy, RrIdentity, StartInputs,
    StartOutcome, StuckReason, SupersedeReason,
};
use robot_s0::clear_estop_transaction::{ClearTxnStage, ClearTxnStore};
use robot_s0::latch_lock::{LatchTargetLock, acquire_latch_lock};
use robot_s0::latch_writer::{
    PersistClient, PersistEndpoint, PersistShared, Phase, StandbyStore, WriterConfig, WriterHandle,
    WriterHook, WriterPairing, WriterSlot, WriterStop, mailbox, spawn_writer,
};
use std::collections::VecDeque;
use std::num::NonZeroU128;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BASE: u64 = 1_000_000_000;
const WAIT: Duration = Duration::from_secs(10);

struct Stop(AtomicBool);
impl WriterStop for Stop {
    fn running(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

struct Clock {
    now: AtomicU64,
    shared: Arc<PersistShared>,
    stop: Arc<Stop>,
    deliveries: AtomicU64,
    stop_after: AtomicU64,
    resume: AtomicBool,
    claim_action: AtomicU64,
    coordinator_reads: AtomicU64,
    authority: Arc<ControlAuthority>,
    estop_on_observation: AtomicBool,
    writer_reads: Mutex<Vec<(Phase, u64)>>,
    pause_before_claim: AtomicBool,
    claim_paused: AtomicBool,
    claim_release: AtomicBool,
    pause_after_delivery: AtomicU64,
    delivery_paused: AtomicBool,
    delivery_release: AtomicBool,
}
impl RobotClock for Clock {
    fn now(&self) -> MonoRawNs {
        let now = MonoRawNs(self.now.fetch_add(1, Ordering::AcqRel));
        let writer = std::thread::current().name() == Some("latch_writer");
        if !writer {
            self.coordinator_reads.fetch_add(1, Ordering::AcqRel);
            if self.estop_on_observation.swap(false, Ordering::AcqRel) {
                let _ = self.authority.estop();
            }
            return now;
        }
        let phase = self.shared.progress.load().unwrap().phase;
        self.writer_reads.lock().unwrap().push((phase, now.0));
        if phase == Phase::Idle && self.shared.announce.offered_at().is_some() {
            if self.pause_before_claim.swap(false, Ordering::AcqRel) {
                self.claim_paused.store(true, Ordering::Release);
                wait_until(|| self.claim_release.load(Ordering::Acquire), "领取前放行");
            }
            match self.claim_action.swap(0, Ordering::AcqRel) {
                1 => {
                    let _ = self.shared.obligation.request(now);
                }
                2 => self.stop.0.store(false, Ordering::Release),
                _ => {}
            }
        }
        // writer 在 deliver 成功后、发布 Idle 前读取此时钟，故消息已真实入队。
        if phase == Phase::Delivering {
            if self.resume.swap(false, Ordering::AcqRel) {
                self.stop.0.store(true, Ordering::Release);
            }
            let count = self.deliveries.fetch_add(1, Ordering::AcqRel) + 1;
            if count == self.pause_after_delivery.load(Ordering::Acquire) {
                self.delivery_paused.store(true, Ordering::Release);
                wait_until(
                    || self.delivery_release.load(Ordering::Acquire),
                    "投递后放行",
                );
            }
            if count == self.stop_after.load(Ordering::Acquire) {
                self.stop.0.store(false, Ordering::Release);
            }
        }
        now
    }
    fn sleep_until(&self, _deadline: Deadline) {}
    fn sleep_for(&self, _duration: DurationNs) {}
}

#[derive(Default)]
struct HookState {
    failures: Mutex<VecDeque<ClearTxnStage>>,
    steps: Mutex<Vec<ClearTxnStage>>,
    park: Mutex<Option<ClearTxnStage>>,
    reached: AtomicBool,
    release: AtomicBool,
}
struct Hook {
    state: Arc<HookState>,
    clock: Arc<Clock>,
}
impl WriterHook for Hook {
    fn before_step(&mut self, stage: ClearTxnStage) {
        self.state.steps.lock().unwrap().push(stage);
        let park = {
            let mut slot = self.state.park.lock().unwrap();
            if *slot == Some(stage) {
                slot.take();
                true
            } else {
                false
            }
        };
        if park {
            self.state.reached.store(true, Ordering::Release);
            wait_until(
                || self.state.release.load(Ordering::Acquire),
                "文件屏障放行",
            );
        }
        let fail = {
            let mut failures = self.state.failures.lock().unwrap();
            if failures.front() == Some(&stage) {
                failures.pop_front();
                true
            } else {
                false
            }
        };
        if fail {
            // 使用真实 WriterGuard 中止文件算法，不伪造 completion 或凭证。
            self.clock.resume.store(true, Ordering::Release);
            self.clock.stop.0.store(false, Ordering::Release);
        }
    }
}

fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "等待超时：{what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    latch: PathBuf,
    _lock: LatchTargetLock,
    standby: StandbyStore,
    shared: Arc<PersistShared>,
    client: PersistClient,
    slot: Option<WriterSlot>,
    endpoint: Option<PersistEndpoint>,
    handle: Option<WriterHandle>,
    stop: Arc<Stop>,
    clock: Arc<Clock>,
    hook: Arc<HookState>,
    authority: Arc<ControlAuthority>,
}
impl Rig {
    fn new(latched: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let latch = dir.path().join("estop_latch");
        let contents = if latched { b"1" } else { b"0" };
        std::fs::write(&latch, contents).unwrap();
        std::fs::write(latch.with_file_name("estop_latch.bak"), contents).unwrap();
        let lock = acquire_latch_lock(&latch).unwrap();
        let open = || ClearTxnStore::open(&latch, "w3a", "bus").unwrap();
        let (slot, standby) = WriterPairing::new(open(), open(), &lock, MonoRawNs(BASE))
            .unwrap()
            .into_parts();
        let shared = Arc::clone(slot.shared());
        let (client, endpoint) = mailbox(Arc::clone(&shared));
        let stop = Arc::new(Stop(AtomicBool::new(true)));
        let authority = Arc::new(
            ControlAuthority::new(AuthorityInit::new(
                latched,
                OutputSafetyState::TorqueOffConfirmed,
                InhibitSet::EMPTY,
            ))
            .unwrap(),
        );
        let clock = Arc::new(Clock {
            now: AtomicU64::new(BASE + 1),
            shared: Arc::clone(&shared),
            stop: Arc::clone(&stop),
            deliveries: AtomicU64::new(0),
            stop_after: AtomicU64::new(0),
            resume: AtomicBool::new(false),
            claim_action: AtomicU64::new(0),
            coordinator_reads: AtomicU64::new(0),
            authority: Arc::clone(&authority),
            estop_on_observation: AtomicBool::new(false),
            writer_reads: Mutex::new(Vec::new()),
            pause_before_claim: AtomicBool::new(false),
            claim_paused: AtomicBool::new(false),
            claim_release: AtomicBool::new(false),
            pause_after_delivery: AtomicU64::new(0),
            delivery_paused: AtomicBool::new(false),
            delivery_release: AtomicBool::new(false),
        });
        Self {
            _dir: dir,
            latch,
            _lock: lock,
            standby,
            shared,
            client,
            slot: Some(slot),
            endpoint: Some(endpoint),
            handle: None,
            stop,
            clock,
            hook: Arc::new(HookState::default()),
            authority,
        }
    }
    fn spawn(&mut self) {
        self.handle = Some(
            spawn_writer(
                self.slot.take().unwrap(),
                self.endpoint.take().unwrap(),
                WriterConfig {
                    idle_poll: DurationNs::from_millis(1),
                    ..WriterConfig::default()
                },
                self.stop.clone(),
                self.clock.clone(),
                Box::new(Hook {
                    state: Arc::clone(&self.hook),
                    clock: Arc::clone(&self.clock),
                }),
            )
            .unwrap(),
        );
    }
    fn start(&mut self, coordinator: &mut ClearCoordination) -> StartOutcome {
        coordinator.start(
            RrIdentity {
                request_id: "clear-original".into(),
                deadline_ns: BASE + 1_000_000_000,
            },
            NonZeroU128::new(41).unwrap(),
            &start_inputs(),
            &self.authority,
            &mut self.client,
            BASE + 1,
        )
    }
    fn tick(&mut self, coordinator: &mut ClearCoordination) -> Vec<ClearEvent> {
        self.tick_with(coordinator, false, false)
    }
    fn tick_with(
        &mut self,
        coordinator: &mut ClearCoordination,
        draining: bool,
        estop_seen_since: bool,
    ) -> Vec<ClearEvent> {
        let now_ns = self.clock.now.load(Ordering::Acquire);
        let events = coordinator.advance(
            &self.authority,
            &mut self.client,
            AdvanceInputs {
                now_ns,
                draining,
                estop_seen_since,
            },
            self.clock.as_ref(),
        );
        assert!(events.len() <= 2);
        events
    }
    fn due(&mut self, coordinator: &ClearCoordination) {
        if let ClearStage::AwaitRetry { due_at_ns, .. } = coordinator.view().unwrap().stage() {
            self.clock.now.fetch_max(due_at_ns, Ordering::AcqRel);
        }
    }
    fn delivered(&self, n: u64) {
        wait_until(
            || self.clock.deliveries.load(Ordering::Acquire) >= n,
            "真实完成投递",
        );
    }
    fn join(&mut self) {
        self.hook.release.store(true, Ordering::Release);
        self.clock.claim_release.store(true, Ordering::Release);
        self.clock.delivery_release.store(true, Ordering::Release);
        self.stop.0.store(false, Ordering::Release);
        if let Some(handle) = &self.handle {
            let deadline = Instant::now() + WAIT;
            while !handle.is_finished() {
                if Instant::now() >= deadline {
                    eprintln!("W3a writer 在测试截止前未结束；仍持句柄，终止测试进程");
                    std::process::abort();
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        if let Some(handle) = self.handle.take() {
            let (result, joined) = handle.join();
            assert_eq!(joined.writer(), self.standby.bound_writer());
            assert!(result.is_ok(), "真实 writer 必须正常收尾");
        }
    }
    fn pair(&self) -> (Vec<u8>, Vec<u8>) {
        (
            std::fs::read(&self.latch).unwrap(),
            std::fs::read(self.latch.with_file_name("estop_latch.bak")).unwrap(),
        )
    }
    fn fail(&self, stages: &[ClearTxnStage]) {
        self.hook
            .failures
            .lock()
            .unwrap()
            .extend(stages.iter().copied());
    }
    fn parked(&self, stage: ClearTxnStage) {
        *self.hook.park.lock().unwrap() = Some(stage);
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.join();
    }
}

fn start_inputs() -> StartInputs {
    StartInputs {
        state_safe: true,
        bus_idle_no_lower_obligation: true,
        obligation_settled: true,
    }
}
fn coordinator() -> ClearCoordination {
    ClearCoordination::new(RetryPolicy::default())
}
#[allow(
    clippy::needless_pass_by_value,
    reason = "消费唯一开始结果，测试不得复制请求身份"
)]
fn queued(outcome: StartOutcome) {
    assert!(matches!(
        outcome,
        StartOutcome::Retained {
            dispatch: DispatchState::Queued
        }
    ));
}
fn retirement(events: &[ClearEvent]) -> RetiredView {
    assert_eq!(events.len(), 2);
    match (&events[0], &events[1]) {
        (
            ClearEvent::Retired(view),
            ClearEvent::Reply {
                rr, retired: true, ..
            },
        ) => {
            assert_eq!(rr.request_id, "clear-original");
            *view
        }
        _ => panic!("必须先真实退休再唯一移交原请求"),
    }
}

#[test]
fn real_prepare_commit_cleanup_reply_preserves_epoch_and_marker_timestamp() {
    let mut rig = Rig::new(true);
    let mut coordinator = coordinator();
    let previous = rig.authority.snapshot().epoch.get();
    queued(rig.start(&mut coordinator));
    assert_eq!(
        coordinator.view().unwrap().txn().expected_epoch,
        previous + 1
    );
    assert_eq!(coordinator.view().unwrap().retry_accepted(), 0);
    rig.spawn();
    rig.delivered(1);
    let events = rig.tick(&mut coordinator);
    assert!(matches!(
        events.as_slice(),
        [ClearEvent::CommitLogically { .. }]
    ));
    assert_eq!(
        coordinator.view().unwrap().stage(),
        ClearStage::FinalizeCleanup
    );
    rig.delivered(2);
    let events = rig.tick(&mut coordinator);
    let retired = retirement(&events);
    let reads = rig.clock.writer_reads.lock().unwrap();
    let idle_stamp = reads
        .iter()
        .rposition(|(phase, _)| *phase == Phase::Delivering)
        .unwrap();
    assert_eq!(retired.physical_done_at, Some(reads[idle_stamp - 2].1));
    drop(reads);
    assert!(
        matches!(retired.retirement, RetirementRecord::Cleanup(record) if record.logical_committed && !record.superseded)
    );
    assert!(retired.physical_done_at.is_some());
    assert_eq!(rig.pair(), (b"0\n".to_vec(), b"0\n".to_vec()));
    assert!(
        !rig.latch
            .with_file_name("estop_latch.clear_pending")
            .exists()
    );
    assert!(coordinator.is_retired_or_absent());
    assert_eq!(rig.authority.snapshot().phase, Some(AuthorityPhase::Idle));
    assert!(rig.tick(&mut coordinator).is_empty());
}

#[test]
fn buffered_marker_completion_retires_before_disconnection_health() {
    let mut rig = Rig::new(true);
    let mut coordinator = coordinator();
    queued(rig.start(&mut coordinator));
    rig.clock.stop_after.store(2, Ordering::Release);
    rig.spawn();
    rig.delivered(1);
    let _ = rig.tick(&mut coordinator);
    rig.delivered(2);
    rig.join();
    let events = rig.tick(&mut coordinator);
    let retired = retirement(&events);
    assert!(retired.physical_done_at.is_some());
    assert!(!rig.client.alive());
    assert!(coordinator.is_retired_or_absent());
    assert_eq!(coordinator.stuck(), None);
    assert!(rig.tick(&mut coordinator).is_empty());
}

#[test]
fn buffered_prepared_is_consumed_but_next_command_is_not_sent_after_disconnect() {
    let mut rig = Rig::new(true);
    let mut coordinator = coordinator();
    queued(rig.start(&mut coordinator));
    rig.clock.stop_after.store(1, Ordering::Release);
    rig.spawn();
    rig.delivered(1);
    rig.join();
    let events = rig.tick(&mut coordinator);
    assert!(matches!(
        events.as_slice(),
        [
            ClearEvent::CommitLogically { .. },
            ClearEvent::StuckEntered(StuckReason::WriterGone)
        ]
    ));
    let state = coordinator.view().unwrap();
    assert!(matches!(state.held(), HeldPermit::Cleanup(_)));
    assert!(state.logical_committed_at().is_some());
    assert_eq!(state.physical_done_at(), None);
    assert_eq!(state.in_flight(), None);
    assert_eq!(state.next(), None);
    assert!(rig.tick(&mut coordinator).is_empty());
}

#[test]
fn disconnect_without_completion_keeps_unknown_flight_and_owner() {
    let mut rig = Rig::new(false);
    let mut coordinator = coordinator();
    queued(rig.start(&mut coordinator));
    let flight = coordinator.view().unwrap().in_flight();
    rig.endpoint.take();
    let events = rig.tick(&mut coordinator);
    assert!(matches!(
        events.as_slice(),
        [ClearEvent::StuckEntered(StuckReason::WriterGone)]
    ));
    assert_eq!(coordinator.view().unwrap().in_flight(), flight);
    assert!(matches!(
        coordinator.view().unwrap().held(),
        HeldPermit::Pending(_)
    ));
    assert!(!rig.client.has_credit());
    assert!(!coordinator.is_retired_or_absent());
    assert!(rig.tick(&mut coordinator).is_empty());
}

#[test]
fn real_prepare_preflight_refusals_retire_without_any_file_fact() {
    for (action, expected) in [
        (1, ClearOutcome::PreemptedByEmergency),
        (2, ClearOutcome::Superseded(SupersedeReason::Shutdown)),
    ] {
        let mut rig = Rig::new(true);
        let mut coordinator = coordinator();
        queued(rig.start(&mut coordinator));
        rig.clock.claim_action.store(action, Ordering::Release);
        rig.clock.stop_after.store(1, Ordering::Release);
        rig.spawn();
        rig.delivered(1);
        rig.join();
        let obligation_before = rig.shared.obligation.view();
        let events = rig.tick(&mut coordinator);
        let retired = retirement(&events);
        assert!(matches!(
            retired.retirement,
            RetirementRecord::Unprepared(_)
        ));
        assert_eq!(retired.logical_committed_at, None);
        assert_eq!(retired.physical_done_at, None);
        assert_eq!(rig.shared.obligation.view(), obligation_before);
        assert!(rig.hook.steps.lock().unwrap().is_empty());
        assert_eq!(rig.pair(), (b"1".to_vec(), b"1".to_vec()));
        assert!(matches!(&events[1], ClearEvent::Reply { outcome, .. } if *outcome == expected));
        assert_eq!(rig.authority.snapshot().phase, Some(AuthorityPhase::Idle));
    }
}

#[test]
fn prepare_failed_not_owned_runs_real_repair_instead_of_unprepared_retirement() {
    let mut rig = Rig::new(false);
    let mut coordinator = coordinator();
    rig.fail(&[ClearTxnStage::CreateMarker]);
    queued(rig.start(&mut coordinator));
    rig.spawn();
    rig.delivered(1);
    assert!(rig.tick(&mut coordinator).is_empty());
    assert_eq!(coordinator.view().unwrap().stage(), ClearStage::Repairing);
    assert!(matches!(
        coordinator.view().unwrap().held(),
        HeldPermit::Repair(_)
    ));
    rig.delivered(2);
    assert!(rig.tick(&mut coordinator).is_empty());
    rig.delivered(3);
    let events = rig.tick(&mut coordinator);
    let retired = retirement(&events);
    assert!(
        matches!(retired.retirement, RetirementRecord::Cleanup(record) if !record.logical_committed)
    );
    assert!(matches!(
        events[1],
        ClearEvent::Reply {
            outcome: ClearOutcome::FsyncFailed {
                stage: ClearTxnStage::CreateMarker
            },
            ..
        }
    ));
    assert!(
        rig.hook
            .steps
            .lock()
            .unwrap()
            .contains(&ClearTxnStage::InspectPair)
    );
}

#[test]
fn unsynced_resync_uses_matching_consumption_clock_after_pre_sampled_tick() {
    let mut rig = Rig::new(true);
    let mut coordinator = coordinator();
    rig.fail(&[ClearTxnStage::SyncRemovalDirectory]);
    queued(rig.start(&mut coordinator));
    rig.spawn();
    rig.delivered(1);
    let _ = rig.tick(&mut coordinator);
    rig.delivered(2);
    assert!(rig.tick(&mut coordinator).is_empty());
    assert_eq!(coordinator.view().unwrap().stage(), ClearStage::Resyncing);
    assert_eq!(coordinator.view().unwrap().physical_done_at(), None);
    rig.clock.stop_after.store(3, Ordering::Release);
    rig.delivered(3);
    rig.join();
    let before = rig.clock.now.load(Ordering::Acquire);
    let reads = rig.clock.coordinator_reads.load(Ordering::Acquire);
    let events = coordinator.advance(
        &rig.authority,
        &mut rig.client,
        AdvanceInputs {
            now_ns: BASE + 1,
            draining: false,
            estop_seen_since: false,
        },
        rig.clock.as_ref(),
    );
    let retired = retirement(&events);
    assert_eq!(retired.physical_done_at, Some(before));
    assert_eq!(
        rig.clock.coordinator_reads.load(Ordering::Acquire),
        reads + 1
    );
    assert!(!rig.client.alive());
    assert!(coordinator.is_retired_or_absent());
}

#[test]
fn retire_refusal_preserves_first_physical_fact_and_never_repeats_delete_or_reply() {
    let mut rig = Rig::new(true);
    let mut coordinator = coordinator();
    queued(rig.start(&mut coordinator));
    rig.spawn();
    rig.delivered(1);
    let _ = rig.tick(&mut coordinator);
    rig.delivered(2);
    let other = ControlAuthority::new(AuthorityInit::new(
        false,
        OutputSafetyState::TorqueOffConfirmed,
        InhibitSet::EMPTY,
    ))
    .unwrap();
    let events = coordinator.advance(
        &other,
        &mut rig.client,
        AdvanceInputs {
            now_ns: BASE + 1,
            draining: false,
            estop_seen_since: false,
        },
        rig.clock.as_ref(),
    );
    assert!(matches!(
        events.as_slice(),
        [ClearEvent::StuckEntered(StuckReason::RetireRefused(_))]
    ));
    let fact = coordinator.view().unwrap().physical_done_at();
    assert!(fact.is_some());
    assert!(matches!(
        coordinator.view().unwrap().held(),
        HeldPermit::Cleanup(_)
    ));
    let count = rig.hook.steps.lock().unwrap().len();
    rig.join();
    assert!(rig.tick(&mut coordinator).is_empty());
    assert_eq!(coordinator.view().unwrap().physical_done_at(), fact);
    assert!(matches!(
        coordinator.stuck(),
        Some(StuckReason::RetireRefused(_))
    ));
    assert_eq!(rig.hook.steps.lock().unwrap().len(), count);
}

#[test]
fn in_flight_timeout_new_estop_and_claimed_announcement_do_not_cancel_prepare() {
    let mut rig = Rig::new(false);
    let mut coordinator = coordinator();
    rig.parked(ClearTxnStage::WriteMarker);
    queued(rig.start(&mut coordinator));
    rig.spawn();
    wait_until(
        || rig.hook.reached.load(Ordering::Acquire),
        "真实文件步骤屏障",
    );
    assert_eq!(rig.shared.announce.offered_at(), None);
    let _ = rig.authority.estop();
    rig.clock
        .now
        .fetch_max(BASE + 2_000_000_000, Ordering::AcqRel);
    assert!(rig.tick_with(&mut coordinator, false, true).is_empty());
    assert!(matches!(
        coordinator.view().unwrap().held(),
        HeldPermit::Pending(_)
    ));
    assert_eq!(coordinator.view().unwrap().stage(), ClearStage::Preparing);
    assert!(coordinator.view().unwrap().in_flight().is_some());
    rig.hook.release.store(true, Ordering::Release);
    rig.delivered(1);
    assert!(rig.tick_with(&mut coordinator, false, true).is_empty());
    rig.delivered(2);
    assert!(rig.tick_with(&mut coordinator, false, true).is_empty());
    rig.delivered(3);
    let events = rig.tick_with(&mut coordinator, false, true);
    let retired = retirement(&events);
    assert!(matches!(retired.retirement, RetirementRecord::Cleanup(record) if record.latch));
    assert_eq!(rig.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
}

#[test]
fn real_created_unsynced_continuation_keeps_original_target_until_complete() {
    let mut rig = Rig::new(false);
    let mut coordinator = coordinator();
    rig.fail(&[
        ClearTxnStage::WriteMarker,
        ClearTxnStage::SyncRemovalDirectory,
    ]);
    queued(rig.start(&mut coordinator));
    rig.spawn();
    rig.delivered(1);
    assert!(rig.tick(&mut coordinator).is_empty());
    rig.delivered(2);
    assert!(rig.tick(&mut coordinator).is_empty());
    assert_eq!(
        coordinator.view().unwrap().continuation().unwrap().target,
        RepairTarget::OriginalUnlatched
    );
    let _ = rig.authority.estop();
    rig.due(&coordinator);
    assert!(rig.tick_with(&mut coordinator, false, true).is_empty());
    let flight = coordinator.view().unwrap().in_flight().unwrap();
    assert_eq!(flight.intent, DispatchIntent::Retry);
    assert_eq!(
        flight.expected.target,
        Some(RepairTarget::OriginalUnlatched)
    );
    rig.delivered(3);
    assert!(rig.tick_with(&mut coordinator, false, true).is_empty());
    assert_eq!(coordinator.view().unwrap().retry_accepted(), 1);
    let flight = coordinator.view().unwrap().in_flight().unwrap();
    assert_eq!(flight.intent, DispatchIntent::NextStage);
    assert_eq!(flight.expected.target, Some(RepairTarget::Latched));
    rig.delivered(4);
    assert!(rig.tick_with(&mut coordinator, false, true).is_empty());
    rig.delivered(5);
    let events = rig.tick_with(&mut coordinator, false, true);
    let _ = retirement(&events);
    assert_eq!(rig.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
}

#[test]
fn accepted_retries_share_eight_attempts_and_new_estop_cannot_buy_a_ninth() {
    let mut rig = Rig::new(false);
    let mut coordinator = coordinator();
    rig.fail(&[ClearTxnStage::CreateBackup; 10]);
    queued(rig.start(&mut coordinator));
    rig.spawn();
    rig.delivered(1);
    assert!(rig.tick(&mut coordinator).is_empty());
    rig.delivered(2);
    assert!(rig.tick(&mut coordinator).is_empty());
    for accepted in 1..=8 {
        rig.due(&coordinator);
        assert!(rig.tick(&mut coordinator).is_empty());
        assert_eq!(
            coordinator.view().unwrap().in_flight().unwrap().intent,
            DispatchIntent::Retry
        );
        rig.delivered(accepted + 2);
        assert!(rig.tick(&mut coordinator).is_empty());
        assert_eq!(
            u64::from(coordinator.view().unwrap().retry_accepted()),
            accepted
        );
    }
    let _ = rig.authority.estop();
    rig.due(&coordinator);
    let events = rig.tick_with(&mut coordinator, false, true);
    assert!(matches!(
        events.as_slice(),
        [ClearEvent::StuckEntered(StuckReason::RepairExhausted)]
    ));
    assert_eq!(coordinator.view().unwrap().retry_accepted(), 8);
    assert_eq!(coordinator.view().unwrap().in_flight(), None);
    assert_eq!(rig.clock.deliveries.load(Ordering::Acquire), 10);
    assert!(!coordinator.is_retired_or_absent());
}

#[test]
fn first_resync_after_eight_accepted_retries_is_next_stage_not_ninth_retry() {
    let mut rig = Rig::new(true);
    let mut coordinator = coordinator();
    rig.fail(&[ClearTxnStage::RemoveMarker; 8]);
    rig.fail(&[ClearTxnStage::SyncRemovalDirectory]);
    queued(rig.start(&mut coordinator));
    rig.spawn();
    rig.delivered(1);
    let _ = rig.tick(&mut coordinator);
    rig.delivered(2);
    assert!(rig.tick(&mut coordinator).is_empty());
    for accepted in 1..=8 {
        rig.due(&coordinator);
        assert!(rig.tick(&mut coordinator).is_empty());
        rig.delivered(accepted + 2);
        assert!(rig.tick(&mut coordinator).is_empty());
        assert_eq!(
            u64::from(coordinator.view().unwrap().retry_accepted()),
            accepted
        );
    }
    assert_eq!(coordinator.view().unwrap().stage(), ClearStage::Resyncing);
    assert_eq!(
        coordinator.view().unwrap().in_flight().unwrap().intent,
        DispatchIntent::NextStage
    );
    rig.delivered(11);
    let events = rig.tick(&mut coordinator);
    let _ = retirement(&events);
    assert!(coordinator.is_retired_or_absent());
}

#[test]
fn buffered_repaired_retains_cleanup_but_does_not_dispatch_after_writer_gone() {
    let mut rig = Rig::new(false);
    let mut coordinator = coordinator();
    rig.fail(&[ClearTxnStage::CreateMarker]);
    queued(rig.start(&mut coordinator));
    rig.clock.stop_after.store(2, Ordering::Release);
    rig.spawn();
    rig.delivered(1);
    assert!(rig.tick(&mut coordinator).is_empty());
    rig.delivered(2);
    rig.join();
    let events = rig.tick(&mut coordinator);
    assert!(matches!(
        events.as_slice(),
        [ClearEvent::StuckEntered(StuckReason::WriterGone)]
    ));
    assert!(matches!(
        coordinator.view().unwrap().held(),
        HeldPermit::Cleanup(_)
    ));
    assert_eq!(coordinator.view().unwrap().physical_done_at(), None);
    assert_eq!(coordinator.view().unwrap().next(), None);
    assert_eq!(coordinator.view().unwrap().in_flight(), None);
}

#[test]
fn logical_commit_clock_is_sampled_after_prepared_and_shared_by_all_outcomes() {
    let mut rig = Rig::new(true);
    let mut coordinator = coordinator();
    queued(rig.start(&mut coordinator));
    rig.spawn();
    rig.delivered(1);
    let after_prepared = rig.clock.now.load(Ordering::Acquire);
    let events = coordinator.advance(
        &rig.authority,
        &mut rig.client,
        AdvanceInputs {
            now_ns: BASE + 1,
            draining: false,
            estop_seen_since: false,
        },
        rig.clock.as_ref(),
    );
    let committed_at = match events.as_slice() {
        [ClearEvent::CommitLogically { at }] => *at,
        _ => panic!("真实逻辑提交事件"),
    };
    assert!(committed_at >= after_prepared && committed_at > BASE + 1);
    assert_eq!(
        coordinator.view().unwrap().logical_committed_at(),
        Some(committed_at)
    );
    rig.delivered(2);
    let events = rig.tick(&mut coordinator);
    let retired = retirement(&events);
    assert_eq!(retired.logical_committed_at, Some(committed_at));
    assert!(
        matches!(events[1], ClearEvent::Reply { outcome: ClearOutcome::Cleared { cleared_at_ns, .. }, .. } if cleared_at_ns == committed_at)
    );
}

#[test]
fn four_active_stages_receive_real_shutdown_refusals_and_draining_keeps_retry_intent() {
    for draining in [false, true] {
        for stage in [
            ClearStage::Repairing,
            ClearStage::FinalizeCleanup,
            ClearStage::RepairClean,
            ClearStage::Resyncing,
        ] {
            let mut rig = Rig::new(false);
            let mut coordinator = coordinator();
            match stage {
                ClearStage::Repairing | ClearStage::RepairClean => {
                    rig.fail(&[ClearTxnStage::CreateBackup]);
                }
                ClearStage::Resyncing => rig.fail(&[ClearTxnStage::SyncRemovalDirectory]),
                _ => {}
            }
            queued(rig.start(&mut coordinator));
            rig.spawn();
            rig.delivered(1);
            let prior = match stage {
                ClearStage::RepairClean | ClearStage::Resyncing => {
                    let _ = rig.tick(&mut coordinator);
                    rig.delivered(2);
                    2
                }
                _ => 1,
            };
            rig.clock.claim_action.store(2, Ordering::Release);
            rig.clock.resume.store(true, Ordering::Release);
            let steps = rig.hook.steps.lock().unwrap().len();
            let _ = rig.tick(&mut coordinator);
            let before = coordinator.view().unwrap();
            assert_eq!(before.stage(), stage);
            let command = before.in_flight().unwrap().command;
            let logical = before.logical_committed_at();
            let continuation = before.continuation();
            rig.delivered(prior + 1);
            // check_current 真正拒绝于任何 FileStep 之前；不是 Hook 中止文件操作。
            assert_eq!(rig.hook.steps.lock().unwrap().len(), steps);
            let sampled = rig.clock.now.load(Ordering::Acquire);
            assert!(rig.tick_with(&mut coordinator, draining, false).is_empty());
            let after = coordinator.view().unwrap();
            assert_eq!(after.logical_committed_at(), logical);
            assert_eq!(after.physical_done_at(), None);
            assert_eq!(after.continuation(), continuation);
            assert_eq!(after.retry_accepted(), 0);
            if !draining {
                assert_eq!(
                    after.stage(),
                    ClearStage::AwaitRetry {
                        command,
                        due_at_ns: sampled + 200_000_000,
                    }
                );
                rig.due(&coordinator);
                assert!(rig.tick(&mut coordinator).is_empty());
            }
            assert_eq!(
                coordinator.view().unwrap().in_flight().unwrap().intent,
                DispatchIntent::Retry
            );
            assert_eq!(
                coordinator.view().unwrap().in_flight().unwrap().command,
                command
            );
            rig.delivered(prior + 2);
            let events = rig.tick_with(&mut coordinator, draining, false);
            match stage {
                ClearStage::Repairing => {
                    assert!(events.is_empty());
                    assert_eq!(coordinator.view().unwrap().retry_accepted(), 1);
                    assert_eq!(
                        coordinator.view().unwrap().in_flight().unwrap().intent,
                        DispatchIntent::NextStage
                    );
                    rig.delivered(prior + 3);
                    let _ = retirement(&rig.tick(&mut coordinator));
                }
                _ => {
                    let _ = retirement(&events);
                }
            }
        }
    }
}

#[test]
fn unprepared_cancellation_leaves_obligation_for_same_writer_to_persist_both_ones() {
    let mut rig = Rig::new(false);
    let mut coordinator = coordinator();
    rig.clock.claim_action.store(1, Ordering::Release);
    rig.clock.pause_after_delivery.store(1, Ordering::Release);
    queued(rig.start(&mut coordinator));
    rig.spawn();
    wait_until(
        || rig.clock.delivery_paused.load(Ordering::Acquire),
        "取消前持住同一 writer",
    );
    let obligation = rig.shared.obligation.view();
    assert!(!obligation.is_settled());
    let events = rig.tick(&mut coordinator);
    assert!(matches!(
        retirement(&events).retirement,
        RetirementRecord::Unprepared(_)
    ));
    assert_eq!(rig.shared.obligation.view(), obligation);
    assert_eq!(rig.pair(), (b"0".to_vec(), b"0".to_vec()));
    assert!(rig.hook.steps.lock().unwrap().is_empty());
    rig.clock.delivery_release.store(true, Ordering::Release);
    wait_until(
        || rig.shared.obligation.view().is_settled(),
        "同 writer 真实清偿原义务",
    );
    assert_eq!(rig.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
    assert!(!rig.hook.steps.lock().unwrap().is_empty());
    assert!(rig.tick(&mut coordinator).is_empty());
}

#[test]
fn new_estop_after_real_commit_repairs_original_finalizing_and_preserves_commit_time() {
    let mut rig = Rig::new(false);
    let mut coordinator = coordinator();
    queued(rig.start(&mut coordinator));
    rig.spawn();
    rig.delivered(1);
    rig.clock
        .estop_on_observation
        .store(true, Ordering::Release);
    let events = rig.tick(&mut coordinator);
    let at = match events.as_slice() {
        [ClearEvent::CommitLogically { at }] => *at,
        _ => panic!("真实 commit 后观测触发新 EStop"),
    };
    assert_eq!(coordinator.view().unwrap().stage(), ClearStage::Repairing);
    assert_eq!(
        coordinator
            .view()
            .unwrap()
            .in_flight()
            .unwrap()
            .expected
            .target,
        Some(RepairTarget::Latched)
    );
    assert!(
        matches!(coordinator.view().unwrap().held(), HeldPermit::Repair(permit)
        if matches!(permit.owner(), robot_core::control_authority::ClearOwner::Finalizing(_)))
    );
    rig.delivered(2);
    assert!(rig.tick(&mut coordinator).is_empty());
    rig.delivered(3);
    let events = rig.tick(&mut coordinator);
    let retired = retirement(&events);
    assert_eq!(retired.logical_committed_at, Some(at));
    assert!(
        matches!(events[1], ClearEvent::Reply { outcome: ClearOutcome::Cleared {
        cleared_at_ns, superseded: true }, .. } if cleared_at_ns == at)
    );
    assert_eq!(rig.clock.coordinator_reads.load(Ordering::Acquire), 1);
    assert_eq!(rig.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
}

#[test]
fn completed_original_continuation_pair_mismatch_requires_new_latch_before_retry() {
    for new_latch in [false, true] {
        let mut rig = Rig::new(false);
        let mut coordinator = coordinator();
        rig.fail(&[
            ClearTxnStage::WriteMarker,
            ClearTxnStage::SyncRemovalDirectory,
        ]);
        queued(rig.start(&mut coordinator));
        rig.spawn();
        rig.delivered(1);
        assert!(rig.tick(&mut coordinator).is_empty());
        rig.delivered(2);
        assert!(rig.tick(&mut coordinator).is_empty());
        assert!(coordinator.view().unwrap().continuation().is_some());
        // 真实文件不一致：续段会先严格完成目录同步，再由真实 inspect_pair 拒绝。
        std::fs::write(&rig.latch, b"1").unwrap();
        rig.due(&coordinator);
        assert!(rig.tick(&mut coordinator).is_empty());
        rig.delivered(3);
        assert!(rig.tick(&mut coordinator).is_empty());
        assert_eq!(coordinator.view().unwrap().continuation(), None);
        assert_eq!(coordinator.view().unwrap().retry_accepted(), 1);
        if new_latch {
            let _ = rig.authority.estop();
        }
        rig.due(&coordinator);
        let steps = rig.hook.steps.lock().unwrap().len();
        let events = rig.tick_with(&mut coordinator, false, new_latch);
        if new_latch {
            assert!(events.is_empty());
            let flight = coordinator.view().unwrap().in_flight().unwrap();
            assert_eq!(flight.intent, DispatchIntent::Retry);
            assert_eq!(flight.expected.target, Some(RepairTarget::Latched));
            rig.delivered(4);
            assert!(rig.tick(&mut coordinator).is_empty());
            assert_eq!(coordinator.view().unwrap().retry_accepted(), 2);
            rig.delivered(5);
            let _ = retirement(&rig.tick(&mut coordinator));
            assert_eq!(rig.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
        } else {
            assert!(matches!(
                events.as_slice(),
                [ClearEvent::StuckEntered(StuckReason::PairMismatch)]
            ));
            assert_eq!(rig.hook.steps.lock().unwrap().len(), steps);
            assert_eq!(coordinator.view().unwrap().retry_accepted(), 1);
            assert_eq!(coordinator.view().unwrap().in_flight(), None);
        }
    }
}
