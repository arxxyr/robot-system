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

const EXPANDED_MODES: [(FileKillMode, &str); 25] = [
    (
        FileKillMode::PrepareAfter(PrepareAfterPoint::SampleMarkerIdentity),
        "prepare-sample-marker-identity",
    ),
    (
        FileKillMode::PrepareAfter(PrepareAfterPoint::WriteMarker),
        "prepare-write-marker",
    ),
    (
        FileKillMode::PrepareAfter(PrepareAfterPoint::SyncMarkerFile),
        "prepare-sync-marker-file",
    ),
    (
        FileKillMode::PrepareAfter(PrepareAfterPoint::SyncMarkerDirectory),
        "prepare-sync-marker-directory",
    ),
    (
        FileKillMode::PrepareAfter(PrepareAfterPoint::CreateBackup),
        "prepare-create-backup",
    ),
    (
        FileKillMode::PrepareAfter(PrepareAfterPoint::WriteBackup),
        "prepare-write-backup",
    ),
    (
        FileKillMode::PrepareAfter(PrepareAfterPoint::SyncBackupFile),
        "prepare-sync-backup-file",
    ),
    (
        FileKillMode::PrepareAfter(PrepareAfterPoint::SyncBackupDirectory),
        "prepare-sync-backup-directory",
    ),
    (
        FileKillMode::PrepareAfter(PrepareAfterPoint::CreatePrimary),
        "prepare-create-primary",
    ),
    (
        FileKillMode::PrepareAfter(PrepareAfterPoint::WritePrimary),
        "prepare-write-primary",
    ),
    (
        FileKillMode::PrepareAfter(PrepareAfterPoint::SyncPrimaryFile),
        "prepare-sync-primary-file",
    ),
    (
        FileKillMode::PrepareAfter(PrepareAfterPoint::RenamePrimary),
        "prepare-rename-primary",
    ),
    (FileKillMode::FinalizeBeforeUnlink, "finalize-before-unlink"),
    (
        FileKillMode::RepairAfter(RepairAfterPoint::ReadMarker),
        "repair-after-read_marker",
    ),
    (
        FileKillMode::RepairAfter(RepairAfterPoint::ValidateMarker),
        "repair-after-validate_marker",
    ),
    (
        FileKillMode::RepairAfter(RepairAfterPoint::CreatePrimary),
        "repair-after-create_primary",
    ),
    (
        FileKillMode::RepairAfter(RepairAfterPoint::WritePrimary),
        "repair-after-write_primary",
    ),
    (
        FileKillMode::RepairAfter(RepairAfterPoint::SyncPrimaryFile),
        "repair-after-sync_primary_file",
    ),
    (
        FileKillMode::RepairAfter(RepairAfterPoint::RenamePrimary),
        "repair-after-rename_primary",
    ),
    (
        FileKillMode::RepairAfter(RepairAfterPoint::SyncPrimaryDirectory),
        "repair-after-sync_primary_directory",
    ),
    (
        FileKillMode::RepairAfter(RepairAfterPoint::CreateBackup),
        "repair-after-create_backup",
    ),
    (
        FileKillMode::RepairAfter(RepairAfterPoint::WriteBackup),
        "repair-after-write_backup",
    ),
    (
        FileKillMode::RepairAfter(RepairAfterPoint::SyncBackupFile),
        "repair-after-sync_backup_file",
    ),
    (
        FileKillMode::RepairAfter(RepairAfterPoint::RenameBackup),
        "repair-after-rename_backup",
    ),
    (
        FileKillMode::RepairAfter(RepairAfterPoint::SyncBackupDirectory),
        "repair-after-sync_backup_directory",
    ),
];

fn with_real_core(run: impl FnOnce(&mut WriterCore, &std::path::Path)) {
    let dir = tempfile::tempdir().unwrap();
    let latch = dir.path().join("latch");
    std::fs::write(&latch, b"1\n").unwrap();
    std::fs::write(crate::estop_latch::backup_path(&latch), b"1\n").unwrap();
    let lock = acquire_latch_lock(&latch).unwrap();
    let open = || ClearTxnStore::open(&latch, "w4b3", "mock").unwrap();
    let (slot, _standby) = WriterPairing::new(open(), open(), &lock, MonoRawNs(1))
        .unwrap()
        .into_parts();
    let mut core = WriterCore::from_slot(slot);
    run(&mut core, &latch);
}

fn execute(
    core: &mut WriterCore,
    hook: &mut dyn WriterHook,
    command: PersistCommand,
) -> PersistCompletion {
    core.execute(
        command,
        &mut StepEnv {
            clock: &MockClock::new(MonoRawNs(10)),
            stop: &Stop(true),
            hook,
        },
    )
}

#[test]
fn expanded_configuration_keeps_closed_table_and_rejects_invalid_reached_stages() {
    for (mode, name) in MODES.into_iter().chain(EXPANDED_MODES) {
        assert_eq!(
            parse_file_kill(Some(OsStr::new(name)), false, false, true).unwrap(),
            Some(mode)
        );
        for (pause, fault, linux) in [
            (true, false, true),
            (false, true, true),
            (false, false, false),
        ] {
            assert!(parse_file_kill(Some(OsStr::new(name)), pause, fault, linux).is_err());
        }
        for invalid in [format!("{name}:2"), format!("{name} "), name.to_uppercase()] {
            assert!(parse_file_kill(Some(OsStr::new(&invalid)), false, false, true).is_err());
        }
    }
    for stage in [
        S::OpenStore,
        S::ValidateCredential,
        S::LoadLatch,
        S::InspectPair,
    ] {
        let events = capture_emitted(|| {
            w4::emit(
                1,
                P::KillReached {
                    step_seq: 1,
                    stage,
                    entered_at: 1,
                    pause_until: 250_000_002,
                },
            );
        });
        assert_eq!(events.len(), 1);
        assert_eq!(
            w4::decode(&events[0]).unwrap(),
            Some(P::Failure {
                kind: F::Protocol,
                slot: None,
                step_seq: 0,
                errno: None
            })
        );
    }
    for name in [
        "repair-after-write-primary",
        "repair-after-create_marker",
        "prepare-read-marker",
        "finalize-before-unlink:before",
        "repair-after-open_store",
    ] {
        assert!(parse_file_kill(Some(OsStr::new(name)), false, false, true).is_err());
    }
}

#[test]
fn twelve_prepare_points_observe_real_success_before_attempting_pause() {
    for (mode, _) in EXPANDED_MODES {
        if !matches!(mode, FileKillMode::PrepareAfter(_)) {
            continue;
        }
        with_real_core(|core, latch| {
            let mut hook = new_hook(mode);
            // 缺页使暂停尝试立即报错；这里只证明真实文件步骤与 hook 顺序。
            let events = capture_emitted(|| {
                assert!(matches!(
                    execute(core, &mut hook, PersistCommand::PrepareClear { txn: TXN }),
                    PersistCompletion::Prepared { txn: TXN, .. }
                ));
            });
            assert!(hook.consumed && hook.failed && hook.scope.is_none());
            let payloads: Vec<_> = events
                .iter()
                .map(|event| w4::decode(event).unwrap().unwrap())
                .collect();
            let failure = payloads
                .iter()
                .position(|payload| matches!(payload, P::Failure { .. }))
                .unwrap();
            assert!(
                matches!(payloads[failure - 1], P::Stage { succeeded: true, stage, occurrence: 1, .. }
                if stage == mode.point().1)
            );
            assert!(
                !payloads
                    .iter()
                    .any(|payload| matches!(payload, P::KillReached { .. }))
            );
            assert_eq!(std::fs::read(latch).unwrap(), b"0\n");
            assert_eq!(core.current().unwrap().txn(), TXN);
        });
    }
}

fn assert_real_precursor(events: &[crate::comm_fault_trace::TraceEvent], latch: &std::path::Path) {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(latch.parent().unwrap()).unwrap();
    let payloads: Vec<_> = events
        .iter()
        .filter(|event| event.source == 1)
        .map(|event| w4::decode(event).unwrap().unwrap())
        .collect();
    assert_eq!(
        payloads
            .iter()
            .filter(|payload| matches!(
                payload,
                P::Stage {
                    succeeded: true,
                    ..
                }
            ))
            .count(),
        14
    );
    assert_eq!(
        payloads
            .iter()
            .filter(|payload| matches!(
                payload,
                P::FaultSelected {
                    slot: 0,
                    step_seq: 15,
                    ..
                }
            ))
            .count(),
        1
    );
    for verified in [false, true] {
        assert!(payloads.contains(&P::Alias {
            verified,
            slot: 0,
            step_seq: 15,
            dev: metadata.dev(),
            ino: metadata.ino()
        }));
    }
    assert!(payloads.contains(&P::DirSyncReturned {
        slot: 0,
        step_seq: 15,
        success: false,
        errno: Some(libc::EBADF)
    }));
    assert_eq!(
        &payloads[payloads.len() - 3..],
        &[
            P::Completion {
                kind: w4::CompletionKind::PrepareDurable,
                failure: Some((S::SyncPrimaryDirectory, StepPhase::Operation))
            },
            P::Token {
                after: true,
                token: T::Durable
            },
            P::CommandEnd { completed: true },
        ]
    );
    assert!(
        !payloads
            .iter()
            .any(|payload| matches!(payload, P::Failure { .. } | P::KillReached { .. }))
    );
}

#[test]
fn twelve_repairs_use_real_final_sync_ebadf_and_the_same_durable_transaction() {
    for (mode, _) in EXPANDED_MODES {
        if !mode.repair() {
            continue;
        }
        with_real_core(|core, latch| {
            let mut hook = new_hook(mode);
            let prepare = capture_emitted(|| {
                assert_eq!(
                    execute(core, &mut hook, PersistCommand::PrepareClear { txn: TXN }),
                    PersistCompletion::PrepareFailed {
                        txn: TXN,
                        stage: S::SyncPrimaryDirectory,
                        phase: StepPhase::Operation,
                        token: PrepareTokenKind::Durable
                    }
                );
            });
            assert!(!hook.failed && !hook.consumed && hook.prepare_ended);
            assert_real_precursor(&prepare, latch);
            assert_eq!(core.current().unwrap().txn(), TXN);
            assert_eq!(
                token_kind(Some(core.current().unwrap().token())),
                T::Durable
            );
            let repair = capture_emitted(|| {
                assert!(matches!(
                    execute(
                        core,
                        &mut hook,
                        PersistCommand::RepairClear {
                            txn: TXN,
                            target: robot_core::control_authority::RepairTarget::Latched,
                        }
                    ),
                    PersistCompletion::Repaired {
                        txn: TXN,
                        target: robot_core::control_authority::RepairTarget::Latched,
                        ..
                    }
                ));
            });
            assert!(hook.consumed && hook.failed && hook.scope.is_none());
            assert!(repair.iter().all(|event| event.source == 2));
            let payloads: Vec<_> = repair
                .iter()
                .map(|event| w4::decode(event).unwrap().unwrap())
                .collect();
            assert_eq!(
                payloads[1],
                P::Token {
                    after: false,
                    token: T::Durable
                }
            );
            let failure = payloads
                .iter()
                .position(|payload| matches!(payload, P::Failure { .. }))
                .unwrap();
            assert!(
                matches!(payloads[failure - 1], P::Stage { succeeded: true, stage, occurrence: 1, .. }
                if stage == mode.point().1)
            );
            assert_eq!(
                payloads
                    .iter()
                    .filter(|payload| matches!(
                        payload,
                        P::Stage {
                            succeeded: true,
                            ..
                        }
                    ))
                    .count(),
                12
            );
            assert!(!payloads.iter().any(|payload| matches!(
                payload,
                P::FaultSelected { .. }
                    | P::Alias { .. }
                    | P::DirSyncCall { .. }
                    | P::KillReached { .. }
            )));
            assert_eq!(std::fs::read(latch).unwrap(), b"1\n");
            assert_eq!(
                std::fs::read(crate::estop_latch::backup_path(latch)).unwrap(),
                b"1\n"
            );
            assert_eq!(core.current().unwrap().txn(), TXN);
            assert_eq!(
                token_kind(Some(core.current().unwrap().token())),
                T::Repaired
            );
        });
    }
}

#[derive(Clone, Copy)]
enum ObservationChange {
    StopBefore,
    WrongErrno,
    UnexpectedSuccess,
    PreparationFailure,
    DuplicateCall,
    MissingExpected,
}

struct TestObserver {
    hook: FileKillHook,
    change: ObservationChange,
    running: Arc<std::sync::atomic::AtomicBool>,
    writer: Arc<PersistShared>,
    before_progress: Option<ProgressView>,
}

impl WriterHook for TestObserver {
    fn command_begin(&mut self, command: PersistCommand, token: Option<&WriterToken>) {
        self.hook.command_begin(command, token);
    }
    fn command_completed(&mut self, completion: &PersistCompletion, token: Option<&WriterToken>) {
        self.hook.command_completed(completion, token);
    }
    fn command_scope_exit(&mut self, completed: bool) {
        self.hook.command_scope_exit(completed);
    }
    fn before_step(&mut self, stage: S) {
        self.hook.before_step(stage);
        if matches!(self.change, ObservationChange::StopBefore) && self.hook.consumed {
            self.before_progress = Some(self.writer.progress.load().unwrap());
            // 真实守门在 hook 返回后读 stop；不伪造操作成功、权限或完成。
            self.running
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
    fn after_step_success(&mut self, stage: S) {
        self.hook.after_step_success(stage);
    }
    fn directory_sync_input(&mut self, stage: S) -> DirectorySyncInput {
        self.hook.directory_sync_input(stage)
    }
    fn directory_sync_event(&mut self, stage: S, event: E) {
        // 仅篡改 observer 输入作为拒绝反例；真实文件引擎仍返回原 fsync 结果。
        let observed = match (self.change, event) {
            (ObservationChange::MissingExpected, E::Expected(_)) => return,
            (ObservationChange::WrongErrno, E::Returned { .. }) => E::Returned {
                success: false,
                errno: Some(libc::EIO),
            },
            (ObservationChange::UnexpectedSuccess, E::Returned { .. }) => E::Returned {
                success: true,
                errno: None,
            },
            (ObservationChange::PreparationFailure, E::Verified(_)) => E::PreparationFailed {
                phase: AliasPreparation::Open,
                errno: Some(libc::ENOENT),
            },
            (ObservationChange::DuplicateCall, E::Call) => {
                self.hook.directory_sync_event(stage, E::Call);
                E::Call
            }
            _ => event,
        };
        self.hook.directory_sync_event(stage, observed);
    }
}

struct Running(Arc<std::sync::atomic::AtomicBool>);
impl WriterStop for Running {
    fn running(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[test]
fn finalize_before_records_real_file_step_and_never_claims_unlink_success() {
    with_real_core(|core, latch| {
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut observer = TestObserver {
            hook: new_hook(FileKillMode::FinalizeBeforeUnlink),
            change: ObservationChange::StopBefore,
            running: running.clone(),
            writer: core.shared().clone(),
            before_progress: None,
        };
        assert!(matches!(
            execute(
                core,
                &mut observer,
                PersistCommand::PrepareClear { txn: TXN }
            ),
            PersistCompletion::Prepared { .. }
        ));
        assert!(observer.hook.prepare_ended && !observer.hook.consumed && !observer.hook.failed);
        let events = capture_emitted(|| {
            let result = core.execute(
                PersistCommand::RemoveMarker {
                    txn: TXN,
                    kind: robot_core::control_authority::CleanupKind::Finalize,
                },
                &mut StepEnv {
                    clock: &MockClock::new(MonoRawNs(10)),
                    stop: &Running(running),
                    hook: &mut observer,
                },
            );
            assert!(matches!(
                result,
                PersistCompletion::MarkerRemoveFailed {
                    stage: S::RemoveMarker,
                    phase: StepPhase::Before,
                    token: crate::latch_writer::RemoveTokenKind::Retained,
                    ..
                }
            ));
        });
        assert_eq!(observer.before_progress.unwrap().phase, Phase::FileStep);
        assert_eq!(observer.before_progress.unwrap().entered_at, MonoRawNs(10));
        let payloads: Vec<_> = events
            .iter()
            .map(|event| w4::decode(event).unwrap().unwrap())
            .collect();
        let failure = payloads
            .iter()
            .position(|payload| matches!(payload, P::Failure { .. }))
            .unwrap();
        assert_eq!(
            payloads[failure - 1],
            P::Stage {
                succeeded: false,
                step_seq: 3,
                stage: S::RemoveMarker,
                occurrence: 1
            }
        );
        assert!(!payloads.iter().any(|payload| matches!(
            payload,
            P::Stage {
                succeeded: true,
                stage: S::RemoveMarker,
                ..
            } | P::KillReached { .. }
        )));
        assert_eq!(
            token_kind(Some(core.current().unwrap().token())),
            T::Prepared
        );
        assert_eq!(std::fs::read(latch).unwrap(), b"0\n");
        assert!(
            ClearTxnStore::open(latch, "w4b3", "mock")
                .unwrap()
                .marker_path()
                .exists()
        );
    });
}

#[test]
fn corrupted_alias_observations_cannot_unlock_repair_selection() {
    for change in [
        ObservationChange::WrongErrno,
        ObservationChange::UnexpectedSuccess,
        ObservationChange::PreparationFailure,
        ObservationChange::DuplicateCall,
        ObservationChange::MissingExpected,
    ] {
        with_real_core(|core, _| {
            let mut observer = TestObserver {
                hook: new_hook(FileKillMode::RepairAfter(RepairAfterPoint::ReadMarker)),
                change,
                running: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                writer: core.shared().clone(),
                before_progress: None,
            };
            let events = capture_emitted(|| {
                assert!(matches!(
                    execute(
                        core,
                        &mut observer,
                        PersistCommand::PrepareClear { txn: TXN }
                    ),
                    PersistCompletion::PrepareFailed {
                        token: PrepareTokenKind::Durable,
                        ..
                    }
                ));
                assert!(observer.hook.failed && !observer.hook.prepare_ended);
                assert!(matches!(
                    execute(
                        core,
                        &mut observer,
                        PersistCommand::RepairClear {
                            txn: TXN,
                            target: robot_core::control_authority::RepairTarget::Latched,
                        }
                    ),
                    PersistCompletion::Repaired { .. }
                ));
            });
            assert!(!observer.hook.consumed && observer.hook.failed);
            assert!(
                events.iter().all(|event| !matches!(
                    w4::decode(event).unwrap(),
                    Some(P::KillReached { .. })
                ))
            );
        });
    }
}

#[test]
fn repair_rejects_missing_prepare_changed_identity_and_extra_source() {
    use robot_core::control_authority::RepairTarget;
    let mode = FileKillMode::RepairAfter(RepairAfterPoint::ReadMarker);
    with_real_core(|core, _| {
        let mut hook = new_hook(mode);
        let _ = execute(
            core,
            &mut hook,
            PersistCommand::RepairClear {
                txn: TXN,
                target: RepairTarget::Latched,
            },
        );
        assert!(hook.failed && !hook.consumed && !hook.prepare_ended);
    });
    for txn in [
        ClearTxnIdentity {
            txn_id: TXN.txn_id ^ (1_u128 << 119),
            ..TXN
        },
        ClearTxnIdentity {
            expected_epoch: TXN.expected_epoch + 1,
            ..TXN
        },
        ClearTxnIdentity {
            original_latched: false,
            ..TXN
        },
        TXN,
    ] {
        with_real_core(|core, _| {
            let mut hook = new_hook(mode);
            let _ = execute(core, &mut hook, PersistCommand::PrepareClear { txn: TXN });
            assert!(hook.prepare_ended && !hook.failed);
            let command = if txn == TXN {
                PersistCommand::PrepareClear { txn }
            } else {
                PersistCommand::RepairClear {
                    txn,
                    target: RepairTarget::Latched,
                }
            };
            let _ = execute(core, &mut hook, command);
            assert!(hook.failed && !hook.consumed);
            assert_eq!(core.current().unwrap().txn(), TXN);
            assert_eq!(
                token_kind(Some(core.current().unwrap().token())),
                T::Durable
            );
            let _ = execute(
                core,
                &mut hook,
                PersistCommand::RepairClear {
                    txn: TXN,
                    target: RepairTarget::Latched,
                },
            );
            assert_eq!(hook.execute_sequence, 3);
            assert!(!hook.consumed);
        });
    }
}

#[test]
fn missing_duplicate_and_unfinished_steps_never_reach_or_rearm() {
    for stages in [
        vec![S::SampleMarkerIdentity],
        vec![S::CreateMarker, S::CreateMarker],
    ] {
        let mut hook = new_hook(FileKillMode::PrepareAfter(PrepareAfterPoint::WriteMarker));
        let events = capture_emitted(|| {
            hook.command_begin(PersistCommand::PrepareClear { txn: TXN }, None);
            for stage in stages {
                hook.before_step(stage);
                hook.after_step_success(stage);
            }
            hook.command_scope_exit(true);
        });
        assert!(hook.failed && !hook.consumed && !hook.prepare_ended);
        assert!(
            events
                .iter()
                .all(|event| !matches!(w4::decode(event).unwrap(), Some(P::KillReached { .. })))
        );
    }
    let mut hook = new_hook(FileKillMode::PrepareAfter(
        PrepareAfterPoint::SampleMarkerIdentity,
    ));
    hook.command_begin(PersistCommand::PrepareClear { txn: TXN }, None);
    hook.before_step(S::CreateMarker);
    hook.before_step(S::SampleMarkerIdentity);
    hook.after_step_success(S::SampleMarkerIdentity);
    assert!(hook.failed && !hook.consumed);
}

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
