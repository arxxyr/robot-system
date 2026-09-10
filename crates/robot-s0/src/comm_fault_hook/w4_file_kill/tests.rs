use super::*;
use crate::clear_estop_transaction::ClearTxnStore;
use crate::comm_fault_trace::capture_emitted;
use crate::latch_lock::acquire_latch_lock;
use crate::latch_writer::{StepEnv, WriterCore, WriterPairing, WriterStop};
use robot_core::control_authority::{AuthorityInit, ControlAuthority, InhibitSet};
use robot_core::executor_ctrl::OutputSafetyState;
use robot_core::time::MockClock;

const MODES: [(FileKillMode, &str); 5] = [
    (FileKillMode::PrepareCreateMarker, "prepare-create-marker"),
    (FileKillMode::PrepareRenameBackup, "prepare-rename-backup"),
    (
        FileKillMode::PrepareSyncPrimaryDirectory,
        "prepare-sync-primary-directory",
    ),
    (FileKillMode::FinalizeRemoveMarker, "finalize-remove-marker"),
    (
        FileKillMode::FinalizeSyncRemovalDirectory,
        "finalize-sync-removal-directory",
    ),
];
const TXN: ClearTxnIdentity = ClearTxnIdentity {
    txn_id: (1_u128 << 120) | 7,
    expected_epoch: 42,
    original_latched: true,
};

fn authority() -> Arc<ControlAuthority> {
    Arc::new(
        ControlAuthority::new(AuthorityInit::new(
            true,
            OutputSafetyState::TorqueOffConfirmed,
            InhibitSet::EMPTY,
        ))
        .unwrap(),
    )
}

fn new_hook(mode: FileKillMode) -> FileKillHook {
    FileKillHook::new(
        mode,
        Arc::new(MockClock::new(MonoRawNs(10))),
        Arc::new(BootShared::new(MonoRawNs(1))),
    )
}

#[test]
fn fixed_configuration_preserves_absence_and_rejects_conflicts_and_non_linux() {
    for linux in [true, false] {
        assert_eq!(parse_file_kill(None, true, true, linux).unwrap(), None);
    }
    for (mode, name) in MODES {
        assert_eq!(
            parse_file_kill(Some(OsStr::new(name)), false, false, true).unwrap(),
            Some(mode)
        );
        for (pause, fault) in [(true, false), (false, true), (true, true)] {
            assert_eq!(
                parse_file_kill(Some(OsStr::new(name)), pause, fault, true)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert_eq!(
            parse_file_kill(Some(OsStr::new(name)), false, false, false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
    }
    for raw in [
        "",
        "prepare-create-marker:2",
        "prepare-create-marker ",
        "PREPARE-CREATE-MARKER",
    ] {
        assert_eq!(
            parse_file_kill(Some(OsStr::new(raw)), false, false, true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

#[test]
fn original_binding_rejects_missing_wrong_writer_or_page_and_only_reads_shutdown() {
    let boot = BootShared::new(MonoRawNs(1));
    let writer = Arc::new(PersistShared::new(MonoRawNs(1)).unwrap());
    assert_eq!(
        check_binding(&boot, &writer).unwrap_err().to_string(),
        "W4 SIGKILL 缺少原 runtime 安装"
    );
    let control = authority();
    let permit = boot
        .install_runtime_for_test(control.clone(), writer.clone())
        .unwrap_or_else(|_| panic!("安装真实原 pair"));
    permit.publish_ready(MonoRawNs(2));
    let another = Arc::new(PersistShared::new(MonoRawNs(1)).unwrap());
    assert_eq!(
        check_binding(&boot, &another).unwrap_err().to_string(),
        "W4 SIGKILL writer 与原 runtime 实例不同"
    );
    assert_eq!(
        check_binding(&boot, &writer).unwrap_err().to_string(),
        "W4 SIGKILL 缺少原 observation"
    );
    assert!(shutdown_is_unarmed(&boot));
    let ticket = boot.arm_runtime_shutdown(MonoRawNs(3)).unwrap();
    let epoch = control.snapshot().epoch;
    let progress = writer.progress.load().unwrap();
    for _ in 0..3 {
        assert!(!shutdown_is_unarmed(&boot));
        assert_eq!(
            boot.runtime_words().shutdown_deadline(),
            Some(ticket.deadline())
        );
        assert_eq!(control.snapshot().epoch, epoch);
        assert_eq!(writer.progress.load().unwrap(), progress);
        assert!(writer.obligation.is_settled());
        assert_eq!(writer.announce.offered_at(), None);
    }
}

#[test]
fn fixed_pause_window_rejects_unarmed_unstamped_late_and_overflow_without_refresh() {
    let base = MonoRawNs(1);
    let writer = PersistShared::new(base).unwrap();
    writer
        .progress
        .store(Phase::FileStep, MonoRawNs(20))
        .unwrap();
    let progress = writer.progress.load().unwrap();
    let window = checked_window(progress, base, 10, MonoRawNs(30)).unwrap();
    assert_eq!(
        window,
        KillWindow {
            entered_at: 20,
            pause_until: 250_000_030
        }
    );
    for (armed, now) in [(0, 30), (21, 30), (10, 19), (10, 1_750_000_020)] {
        assert_eq!(
            checked_window(progress, base, armed, MonoRawNs(now)),
            Err(F::Protocol)
        );
        assert_eq!(writer.progress.load().unwrap(), progress);
    }
    for phase in [
        Phase::Idle,
        Phase::Claiming,
        Phase::Delivering,
        Phase::RetryBackoff,
        Phase::Exited,
    ] {
        assert_eq!(
            checked_window(ProgressView { phase, ..progress }, base, 10, MonoRawNs(30)),
            Err(F::Protocol)
        );
    }
    writer.progress.store_unstamped(Phase::FileStep);
    assert_eq!(
        checked_window(writer.progress.load().unwrap(), base, 1, MonoRawNs(30)),
        Err(F::Protocol)
    );
    assert_eq!(
        checked_window(
            ProgressView {
                phase: Phase::FileStep,
                entered_at: MonoRawNs(u64::MAX)
            },
            base,
            1,
            MonoRawNs(u64::MAX)
        ),
        Err(F::CounterOverflow)
    );
    assert!(writer.obligation.is_settled());
    assert_eq!(writer.announce.offered_at(), None);
}

#[test]
fn scope_keeps_real_transaction_and_never_rearms_after_missing_observation() {
    let mut hook = new_hook(FileKillMode::PrepareCreateMarker);
    let events = capture_emitted(|| {
        hook.command_begin(PersistCommand::PrepareClear { txn: TXN }, None);
        hook.before_step(S::CreateMarker);
        hook.after_step_success(S::CreateMarker);
        assert!(hook.consumed && hook.failed);
        hook.after_step_success(S::CreateMarker);
        hook.outside_command();
        assert!(hook.scope.is_none());
        hook.command_begin(PersistCommand::PrepareClear { txn: TXN }, None);
        hook.before_step(S::CreateMarker);
        hook.after_step_success(S::CreateMarker);
        hook.command_scope_exit(false);
        assert!(hook.scope.is_none());
    });
    assert!(
        events
            .iter()
            .all(|e| !matches!(w4::decode(e).unwrap(), Some(P::KillReached { .. })))
    );
    assert!(events.iter().any(
        |e| matches!(w4::decode(e).unwrap(), Some(P::CommandBegin { txn, .. }) if txn == TXN)
    ));
    let mut overflow = new_hook(FileKillMode::PrepareCreateMarker);
    overflow.execute_sequence = u64::MAX;
    overflow.command_begin(PersistCommand::PrepareClear { txn: TXN }, None);
    assert!(overflow.failed && overflow.scope.is_none());
    assert_eq!(overflow.execute_sequence, u64::MAX);
}

struct Stop(bool);
impl WriterStop for Stop {
    fn running(&self) -> bool {
        self.0
    }
}

#[test]
fn actual_writer_refusal_and_success_keep_original_completion_and_scope() {
    for (mode, _) in MODES {
        let dir = tempfile::tempdir().unwrap();
        let latch = dir.path().join("latch");
        std::fs::write(&latch, b"1\n").unwrap();
        std::fs::write(crate::estop_latch::backup_path(&latch), b"1\n").unwrap();
        let lock = acquire_latch_lock(&latch).unwrap();
        let open = || ClearTxnStore::open(&latch, "w4b", "mock").unwrap();
        let (slot, _standby) = WriterPairing::new(open(), open(), &lock, MonoRawNs(1))
            .unwrap()
            .into_parts();
        let mut core = WriterCore::from_slot(slot);
        let mut observer = new_hook(mode);
        let clock = MockClock::new(MonoRawNs(10));
        // 缺 observation 的私有分类夹具：不冒充工厂已接真实监督或杀进程。
        let events = capture_emitted(|| {
            let result = core.execute(
                PersistCommand::PrepareClear { txn: TXN },
                &mut StepEnv {
                    clock: &clock,
                    stop: &Stop(true),
                    hook: &mut observer,
                },
            );
            assert!(matches!(result, PersistCompletion::Prepared { .. }));
            if mode.point().0 == C::RemoveCommitted {
                let result = core.execute(
                    PersistCommand::RemoveMarker {
                        txn: TXN,
                        kind: robot_core::control_authority::CleanupKind::Finalize,
                    },
                    &mut StepEnv {
                        clock: &clock,
                        stop: &Stop(true),
                        hook: &mut observer,
                    },
                );
                assert!(matches!(result, PersistCompletion::MarkerRemoved { .. }));
            }
            assert!(observer.consumed && observer.failed && observer.scope.is_none());
        });
        assert!(
            events
                .iter()
                .all(|e| !matches!(w4::decode(e).unwrap(), Some(P::KillReached { .. })))
        );
        assert_eq!(std::fs::read(&latch).unwrap(), b"0\n");
        let mut rejected = new_hook(mode);
        let mut other_txn = TXN;
        other_txn.txn_id += 1;
        let _ = core.execute(
            PersistCommand::PrepareClear { txn: other_txn },
            &mut StepEnv {
                clock: &clock,
                stop: &Stop(false),
                hook: &mut rejected,
            },
        );
        assert!(!rejected.consumed);
        assert!(rejected.scope.is_none());
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "三种拒绝共用原令牌与真实完成链，集中核对未重新选择及最后事件顺序"
)]
fn nested_outside_and_wrong_transaction_never_select_again_and_keep_real_tokens() {
    use crate::comm_fault_trace::w4::{CompletionKind, TokenKind};
    use crate::latch_writer::{WriterRefusal, WriterTokenKind};

    #[derive(Clone, Copy)]
    enum Rejection {
        Nested,
        Outside,
        WrongTransaction,
    }

    for rejection in [
        Rejection::Nested,
        Rejection::Outside,
        Rejection::WrongTransaction,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let latch = dir.path().join("latch");
        std::fs::write(&latch, b"1\n").unwrap();
        std::fs::write(crate::estop_latch::backup_path(&latch), b"1\n").unwrap();
        let lock = acquire_latch_lock(&latch).unwrap();
        let open = || ClearTxnStore::open(&latch, "w4b", "mock").unwrap();
        let (slot, _standby) = WriterPairing::new(open(), open(), &lock, MonoRawNs(1))
            .unwrap()
            .into_parts();
        let mut core = WriterCore::from_slot(slot);
        let mut hook = new_hook(FileKillMode::FinalizeRemoveMarker);
        let clock = MockClock::new(MonoRawNs(10));
        let events = capture_emitted(|| {
            let mut env = StepEnv {
                clock: &clock,
                stop: &Stop(true),
                hook: &mut hook,
            };
            assert!(matches!(
                core.execute(PersistCommand::PrepareClear { txn: TXN }, &mut env),
                PersistCompletion::Prepared { .. }
            ));
            let remove = PersistCommand::RemoveMarker {
                txn: TXN,
                kind: robot_core::control_authority::CleanupKind::Finalize,
            };
            // 只构造非法回调次序；文件操作、原 token 和完成仍由同一 WriterCore 执行。
            match rejection {
                Rejection::Nested | Rejection::Outside => {
                    env.hook.command_begin(
                        remove,
                        core.current().map(crate::latch_writer::WriterTxn::token),
                    );
                    if matches!(rejection, Rejection::Outside) {
                        assert!(core.service_obligation(&mut env).is_none());
                    }
                }
                Rejection::WrongTransaction => {
                    let wrong = PersistCommand::RemoveMarker {
                        txn: ClearTxnIdentity {
                            expected_epoch: TXN.expected_epoch + 1,
                            ..TXN
                        },
                        kind: robot_core::control_authority::CleanupKind::Finalize,
                    };
                    assert!(matches!(
                        core.execute(wrong, &mut env),
                        PersistCompletion::Refused {
                            reason: WriterRefusal::IdentityMismatch,
                            ..
                        }
                    ));
                    assert_eq!(core.current().unwrap().txn(), TXN);
                    assert_eq!(
                        core.current().unwrap().token().kind(),
                        WriterTokenKind::Prepared
                    );
                }
            }
            assert!(matches!(
                core.execute(remove, &mut env),
                PersistCompletion::MarkerRemoved { .. }
            ));
            assert!(core.current().is_none());
        });
        assert!(hook.failed && !hook.consumed && hook.scope.is_none());
        assert_eq!(hook.binding, Some(TXN));
        let decoded: Vec<_> = events
            .iter()
            .map(|event| w4::decode(event).unwrap().unwrap())
            .collect();
        assert!(decoded.iter().any(|event| matches!(
            event,
            P::Failure {
                kind: F::Protocol,
                ..
            }
        )));
        assert!(
            !decoded
                .iter()
                .any(|event| matches!(event, P::KillReached { .. }))
        );
        assert!(decoded.contains(&P::Completion {
            kind: CompletionKind::RemovedCommitted,
            failure: None
        }));
        assert_eq!(
            &decoded[decoded.len() - 3..],
            &[
                P::Token {
                    after: true,
                    token: TokenKind::None
                },
                P::Failure {
                    kind: F::Protocol,
                    slot: None,
                    step_seq: 4,
                    errno: None
                },
                P::CommandEnd { completed: true },
            ]
        );
        assert_eq!(std::fs::read(&latch).unwrap(), b"0\n");
        assert_eq!(
            std::fs::read(crate::estop_latch::backup_path(&latch)).unwrap(),
            b"0\n"
        );
    }
}
