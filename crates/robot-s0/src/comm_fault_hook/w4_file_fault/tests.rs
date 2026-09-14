use super::*;
use crate::clear_estop_transaction::{ClearTxnStore, NoGuard};
use crate::comm_fault_trace::{TraceEvent, capture_emitted};
use crate::latch_lock::{LatchTargetLock, acquire_latch_lock};
use crate::latch_writer::{StepEnv, WriterCore, WriterPairing, WriterStop};
use robot_core::time::{MockClock, MonoRawNs};

struct Stop(bool);
impl WriterStop for Stop {
    fn running(&self) -> bool {
        self.0
    }
}
struct Rig {
    core: WriterCore,
    hook: FileFaultHook,
    clock: MockClock,
    stop: Stop,
    latch: std::path::PathBuf,
    _lock: LatchTargetLock,
    directory: tempfile::TempDir,
}
const TXN: ClearTxnIdentity = ClearTxnIdentity {
    txn_id: (1_u128 << 100) | 0x13,
    expected_epoch: u64::MAX - 1,
    original_latched: true,
};
impl Rig {
    fn new(mode: FileFaultMode) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let latch = dir.path().join("estop_latch");
        std::fs::write(&latch, b"1\n").unwrap();
        std::fs::write(crate::estop_latch::backup_path(&latch), b"1\n").unwrap();
        let lock = acquire_latch_lock(&latch).unwrap();
        let open = || ClearTxnStore::open(&latch, "w4", "mock").unwrap();
        let (slot, _standby) = WriterPairing::new(open(), open(), &lock, MonoRawNs(1))
            .unwrap()
            .into_parts();
        Self {
            core: WriterCore::from_slot(slot),
            hook: FileFaultHook::new(mode),
            clock: MockClock::new(MonoRawNs(10)),
            stop: Stop(true),
            latch,
            _lock: lock,
            directory: dir,
        }
    }
    fn command(&mut self, command: PersistCommand) -> PersistCompletion {
        let result = self.core.execute(
            command,
            &mut StepEnv {
                clock: &self.clock,
                stop: &self.stop,
                hook: &mut self.hook,
            },
        );
        assert!(
            self.hook.scope.is_none(),
            "真实 execute 返回后 scope 必须清除"
        );
        result
    }
}
fn payloads(events: &[TraceEvent]) -> Vec<P> {
    events
        .iter()
        .filter_map(|event| w4::decode(event).unwrap())
        .collect()
}

#[test]
fn configuration_keeps_absence_and_old_pause_and_rejects_invalid_or_non_linux() {
    for linux in [true, false] {
        assert_eq!(parse_file_fault(None, true, linux).unwrap(), None);
    }
    assert_eq!(
        parse_file_fault(Some(OsStr::new("created")), false, true).unwrap(),
        Some(FileFaultMode::Created)
    );
    assert_eq!(
        parse_file_fault(Some(OsStr::new("created-unsynced")), false, true).unwrap(),
        Some(FileFaultMode::CreatedUnsynced)
    );
    assert_eq!(
        parse_file_fault(Some(OsStr::new("ordinary-resync")), false, true).unwrap(),
        Some(FileFaultMode::OrdinaryResync)
    );
    for raw in [
        "",
        "Created",
        "created ",
        "created-unsynced:2",
        "ordinary-resync:2",
        "ordinary_resync",
    ] {
        assert_eq!(
            parse_file_fault(Some(OsStr::new(raw)), false, true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
    assert_eq!(
        parse_file_fault(Some(OsStr::new("created")), true, true)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        parse_file_fault(Some(OsStr::new("created")), false, false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
}

#[cfg(target_os = "linux")]
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "同一原writer凭证贯穿两故障分支及唯一退休，保留顺序断言便于核责任链"
)]
fn real_writer_created_and_unsynced_scopes_keep_original_target_and_single_syscalls() {
    for mode in [FileFaultMode::Created, FileFaultMode::CreatedUnsynced] {
        let mut rig = Rig::new(mode);
        let events = capture_emitted(|| {
            assert!(matches!(
                rig.command(PersistCommand::PrepareClear { txn: TXN }),
                PersistCompletion::PrepareFailed {
                    stage: S::SyncMarkerDirectory,
                    phase: StepPhase::Operation,
                    token: PrepareTokenKind::Created,
                    ..
                }
            ));
            assert!(matches!(
                rig.core.current().unwrap().token(),
                WriterToken::Created(_)
            ));
            let repair = PersistCommand::RepairClear {
                txn: TXN,
                target: RepairTarget::Latched,
            };
            match mode {
                FileFaultMode::Created => assert!(matches!(
                    rig.command(repair),
                    PersistCompletion::Repaired {
                        target: RepairTarget::Latched,
                        ..
                    }
                )),
                FileFaultMode::CreatedUnsynced => {
                    assert!(matches!(
                        rig.command(repair),
                        PersistCompletion::RepairFailed {
                            stage: S::SyncRemovalDirectory,
                            phase: StepPhase::Operation,
                            credential_kept: true,
                            ..
                        }
                    ));
                    assert!(matches!(
                        rig.core.current().unwrap().token(),
                        WriterToken::UnsyncedBeforeRepair {
                            target: RepairTarget::Latched,
                            ..
                        }
                    ));
                    assert!(matches!(
                        rig.command(repair),
                        PersistCompletion::Repaired {
                            target: RepairTarget::Latched,
                            ..
                        }
                    ));
                }
                FileFaultMode::OrdinaryResync => unreachable!("旧 Created 用例不包含普通 Resync"),
            }
            assert!(matches!(
                rig.command(PersistCommand::RemoveMarker {
                    txn: TXN,
                    kind: CleanupKind::Repaired(RepairTarget::Latched)
                }),
                PersistCompletion::MarkerRemoved {
                    kind: CleanupKind::Repaired(RepairTarget::Latched),
                    ..
                }
            ));
            assert!(rig.core.current().is_none());
            assert!(!rig.core.store().marker_path().exists());
            assert_eq!(std::fs::read(&rig.latch).unwrap(), b"1\n");
            assert_eq!(
                std::fs::read(crate::estop_latch::backup_path(&rig.latch)).unwrap(),
                b"1\n"
            );
            let before = rig.hook.execute_sequence;
            rig.core.shared().obligation.request(MonoRawNs(20));
            assert!(
                rig.core
                    .service_obligation(&mut StepEnv {
                        clock: &rig.clock,
                        stop: &rig.stop,
                        hook: &mut rig.hook
                    })
                    .is_some()
            );
            assert!(rig.hook.scope.is_none());
            assert_eq!(rig.hook.execute_sequence, before, "义务不借命令 scope");
        });
        let payloads = payloads(&events);
        let expected = match mode {
            FileFaultMode::Created => 1,
            FileFaultMode::CreatedUnsynced => 2,
            FileFaultMode::OrdinaryResync => unreachable!("旧 Created 用例不包含普通 Resync"),
        };
        assert!(!rig.hook.failed);
        assert_eq!(
            payloads
                .iter()
                .filter(|p| matches!(p, P::DirSyncCall { .. }))
                .count(),
            expected
        );
        assert_eq!(
            payloads
                .iter()
                .filter(|p| matches!(
                    p,
                    P::DirSyncReturned {
                        success: false,
                        errno: Some(libc::EBADF),
                        ..
                    }
                ))
                .count(),
            expected
        );
        assert_eq!(
            payloads
                .iter()
                .filter(|p| matches!(
                    p,
                    P::Stage {
                        succeeded: true,
                        stage: S::RemoveMarker,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(!payloads.iter().any(|p| matches!(p, P::Failure { .. })));
        for slot in 0..expected {
            assert_eq!(payloads.iter().filter(|p| matches!(p, P::FaultSelected { slot: actual, .. } if usize::from(*actual) == slot)).count(), 1);
        }
    }
}

#[test]
fn preparation_failure_never_counts_as_a_sync_call_and_disables_other_slots() {
    for phase in [
        AliasPreparation::Open,
        AliasPreparation::Metadata,
        AliasPreparation::Identity,
    ] {
        let mut rig = Rig::new(FileFaultMode::CreatedUnsynced);
        let identity = ClearTxnStore::open(&rig.latch, "w4", "mock")
            .unwrap()
            .inspect_pair(&mut NoGuard)
            .unwrap()
            .directory_identity;
        let events = capture_emitted(|| {
            rig.hook
                .command_begin(PersistCommand::PrepareClear { txn: TXN }, None);
            rig.hook.before_step(S::SyncMarkerDirectory);
            assert_eq!(
                rig.hook.directory_sync_input(S::SyncMarkerDirectory),
                DirectorySyncInput::OPathAlias
            );
            rig.hook
                .directory_sync_event(S::SyncMarkerDirectory, E::Expected(identity));
            // 本例仅核 callback 分类；不把手送准备错误当真实系统调用证据。
            rig.hook.directory_sync_event(
                S::SyncMarkerDirectory,
                E::PreparationFailed { phase, errno: None },
            );
            assert!(rig.hook.failed);
            assert_eq!(rig.hook.slots[0], Some(FaultSlot::PreparationFailed));
            assert_eq!(
                rig.hook.directory_sync_input(S::SyncMarkerDirectory),
                DirectorySyncInput::Original
            );
            rig.hook.command_scope_exit(false);
        });
        assert!(
            !payloads(&events)
                .iter()
                .any(|p| matches!(p, P::DirSyncCall { .. } | P::DirSyncReturned { .. }))
        );
        assert!(rig.hook.scope.is_none());
    }
}

#[test]
fn unexpected_sync_callback_result_preserves_facts_and_disables_second_slot() {
    for (success, errno) in [(true, None), (false, Some(libc::EIO))] {
        let mut rig = Rig::new(FileFaultMode::CreatedUnsynced);
        let identity = ClearTxnStore::open(&rig.latch, "w4", "mock")
            .unwrap()
            .inspect_pair(&mut NoGuard)
            .unwrap()
            .directory_identity;
        let events = capture_emitted(|| {
            rig.hook
                .command_begin(PersistCommand::PrepareClear { txn: TXN }, None);
            rig.hook.before_step(S::SyncMarkerDirectory);
            assert_eq!(
                rig.hook.directory_sync_input(S::SyncMarkerDirectory),
                DirectorySyncInput::OPathAlias
            );
            // 本例只核回调分类；不声称 O_PATH 实际返回成功或 EIO。
            for event in [
                E::Expected(identity),
                E::Verified(identity),
                E::Call,
                E::Returned { success, errno },
            ] {
                rig.hook.directory_sync_event(S::SyncMarkerDirectory, event);
            }
            assert!(rig.hook.failed);
            assert_eq!(
                rig.hook.slots,
                [
                    Some(FaultSlot::Executed { bad_fd: false }),
                    Some(FaultSlot::Armed)
                ]
            );
            rig.hook.command_scope_exit(true);
            assert!(rig.hook.scope.is_none());
            rig.hook.command_begin(
                PersistCommand::RepairClear {
                    txn: TXN,
                    target: RepairTarget::Latched,
                },
                None,
            );
            rig.hook.before_step(S::SyncRemovalDirectory);
            assert_eq!(
                rig.hook.directory_sync_input(S::SyncRemovalDirectory),
                DirectorySyncInput::Original
            );
            assert!(rig.hook.failed, "新 scope 不能清除首个夹具失败");
            assert_eq!(rig.hook.slots[1], Some(FaultSlot::Armed));
            rig.hook.command_scope_exit(true);
            assert!(rig.hook.scope.is_none());
        });
        let facts = payloads(&events);
        assert!(facts.contains(&P::DirSyncReturned {
            slot: 0,
            step_seq: 1,
            success,
            errno,
        }));
        assert!(facts.contains(&P::Failure {
            kind: F::UnexpectedResult,
            slot: Some(0),
            step_seq: 1,
            errno,
        }));
        assert_eq!(
            facts
                .iter()
                .filter(|p| matches!(p, P::FaultSelected { .. }))
                .count(),
            1
        );
        assert!(
            !facts
                .iter()
                .any(|p| matches!(p, P::FaultSelected { slot: 1, .. }))
        );
    }
}

#[test]
fn refused_original_command_clears_scope_and_cannot_rearm_first_fault() {
    let mut rig = Rig::new(FileFaultMode::CreatedUnsynced);
    rig.stop.0 = false;
    assert!(matches!(
        rig.command(PersistCommand::PrepareClear { txn: TXN }),
        PersistCompletion::Refused {
            reason: WriterRefusal::ShuttingDown,
            ..
        }
    ));
    assert!(rig.hook.failed);
    assert_eq!(rig.hook.slots[0], Some(FaultSlot::Armed));
    rig.hook.command_begin(
        PersistCommand::RepairClear {
            txn: TXN,
            target: RepairTarget::Latched,
        },
        None,
    );
    rig.hook.before_step(S::SyncRemovalDirectory);
    assert_eq!(
        rig.hook.directory_sync_input(S::SyncRemovalDirectory),
        DirectorySyncInput::Original
    );
    rig.hook.outside_command();
    assert!(rig.hook.scope.is_none());
}

#[test]
fn counters_and_nested_or_wrong_transaction_scopes_fail_without_wrap() {
    let mut hook = FileFaultHook::new(FileFaultMode::Created);
    hook.execute_sequence = u64::MAX;
    hook.command_begin(PersistCommand::PrepareClear { txn: TXN }, None);
    assert!(hook.failed);
    assert_eq!(hook.execute_sequence, u64::MAX);
    assert!(hook.scope.is_none());
    for bad in [
        PersistCommand::PrepareClear { txn: TXN },
        PersistCommand::RepairClear {
            txn: ClearTxnIdentity {
                txn_id: TXN.txn_id + 1,
                ..TXN
            },
            target: RepairTarget::Latched,
        },
    ] {
        let mut hook = FileFaultHook::new(FileFaultMode::Created);
        hook.command_begin(PersistCommand::PrepareClear { txn: TXN }, None);
        hook.command_begin(bad, None);
        assert!(hook.failed);
        hook.outside_command();
        assert!(hook.scope.is_none());
    }
}

#[test]
fn real_execute_unwind_clears_scope_without_fabricating_completion() {
    struct PanicHook(FileFaultHook);
    impl WriterHook for PanicHook {
        fn before_step(&mut self, stage: S) {
            self.0.before_step(stage);
            panic!("测试真实步骤前 unwind");
        }
        fn command_begin(&mut self, command: PersistCommand, token: Option<&WriterToken>) {
            self.0.command_begin(command, token);
        }
        fn command_scope_exit(&mut self, completed: bool) {
            self.0.command_scope_exit(completed);
        }
    }
    let mut rig = Rig::new(FileFaultMode::Created);
    let mut hook = PanicHook(FileFaultHook::new(FileFaultMode::Created));
    let events = capture_emitted(|| {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rig.core.execute(
                PersistCommand::PrepareClear { txn: TXN },
                &mut StepEnv {
                    clock: &rig.clock,
                    stop: &rig.stop,
                    hook: &mut hook,
                },
            )
        }));
        assert!(result.is_err());
    });
    assert!(hook.0.scope.is_none());
    let p = payloads(&events);
    assert!(p.contains(&P::CommandEnd { completed: false }));
    assert!(!p.iter().any(|p| matches!(p, P::Completion { .. })));
}

fn ordinary_until_retry(rig: &mut Rig) {
    assert!(matches!(
        rig.command(PersistCommand::PrepareClear { txn: TXN }),
        PersistCompletion::Prepared { txn: TXN, .. }
    ));
    assert!(matches!(
        rig.core.current().unwrap().token(),
        WriterToken::Prepared(_)
    ));
    for command in [
        PersistCommand::RemoveMarker {
            txn: TXN,
            kind: CleanupKind::Finalize,
        },
        PersistCommand::ResyncRemoval { txn: TXN },
    ] {
        assert!(matches!(
            rig.command(command),
            PersistCompletion::MarkerRemoveFailed {
                txn: TXN,
                stage: S::SyncRemovalDirectory,
                phase: StepPhase::Operation,
                token: RemoveTokenKind::Unsynced,
            }
        ));
        assert!(matches!(
            rig.core.current().unwrap().token(),
            WriterToken::Unsynced(_)
        ));
        assert_eq!(rig.core.current().unwrap().txn(), TXN);
        assert!(!rig.core.store().marker_path().exists());
    }
    assert_eq!(rig.hook.ordinary, Some(OrdinaryStep::Retry));
    assert_eq!(
        rig.hook.slots,
        [Some(FaultSlot::Executed { bad_fd: true }); 2]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn real_ordinary_resync_keeps_complete_identity_two_alias_errors_and_one_original_sync() {
    use std::os::unix::fs::MetadataExt;

    let mut rig = Rig::new(FileFaultMode::OrdinaryResync);
    let metadata = std::fs::metadata(rig.directory.path()).unwrap();
    let events = capture_emitted(|| {
        ordinary_until_retry(&mut rig);
        assert_eq!(
            rig.command(PersistCommand::ResyncRemoval { txn: TXN }),
            PersistCompletion::Resynced { txn: TXN }
        );
    });
    assert!(!rig.hook.failed);
    assert_eq!(rig.hook.ordinary, Some(OrdinaryStep::Complete));
    assert_eq!(rig.hook.execute_sequence, 4);
    assert!(rig.core.current().is_none());
    assert_eq!(std::fs::read(&rig.latch).unwrap(), b"0\n");
    assert_eq!(
        std::fs::read(crate::estop_latch::backup_path(&rig.latch)).unwrap(),
        b"0\n"
    );
    let facts = payloads(&events);
    assert_ordinary_trace(&facts);
    for slot in 0..2 {
        for verified in [false, true] {
            assert_eq!(
                facts
                    .iter()
                    .filter(|fact| matches!(fact,
                P::Alias { slot: actual, verified: actual_verified, dev, ino, .. }
                if *actual == slot && *actual_verified == verified
                    && *dev == metadata.dev() && *ino == metadata.ino()))
                    .count(),
                1
            );
        }
        assert_eq!(
            facts
                .iter()
                .filter(|fact| matches!(fact,
            P::DirSyncReturned { slot: actual, success: false, errno: Some(libc::EBADF), .. }
            if *actual == slot))
                .count(),
            1
        );
        assert_eq!(
            facts
                .iter()
                .filter(|fact| matches!(fact,
            P::DirSyncCall { slot: actual, .. } if *actual == slot))
                .count(),
            1
        );
    }
}

fn assert_ordinary_trace(facts: &[P]) {
    let commands: Vec<_> = facts
        .iter()
        .filter_map(|fact| match fact {
            P::CommandBegin { txn, command } => {
                assert_eq!(*txn, TXN);
                Some(*command)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        commands,
        [C::Prepare, C::RemoveCommitted, C::Resync, C::Resync]
    );
    let completions: Vec<_> = facts
        .iter()
        .filter_map(|fact| match fact {
            P::Completion { kind, failure } => Some((*kind, *failure)),
            _ => None,
        })
        .collect();
    let failed = Some((S::SyncRemovalDirectory, StepPhase::Operation));
    assert_eq!(
        completions,
        [
            (K::Prepared, None),
            (K::RemoveFailedUnsynced, failed),
            (K::RemoveFailedUnsynced, failed),
            (K::Resynced, None)
        ]
    );
    let tokens: Vec<_> = facts
        .iter()
        .filter_map(|fact| match fact {
            P::Token { after, token } => Some((*after, *token)),
            _ => None,
        })
        .collect();
    assert_eq!(
        tokens,
        [
            (false, T::None),
            (true, T::Prepared),
            (false, T::Prepared),
            (true, T::Unsynced),
            (false, T::Unsynced),
            (true, T::Unsynced),
            (false, T::Unsynced),
            (true, T::None)
        ]
    );
    assert_eq!(
        facts
            .iter()
            .filter(|fact| matches!(
                fact,
                P::Stage {
                    stage: S::RemoveMarker,
                    succeeded: true,
                    ..
                }
            ))
            .count(),
        1
    );
    let mut command = C::Prepare;
    let mut resync_before = 0;
    let mut resync_after = 0;
    for fact in facts {
        match fact {
            P::CommandBegin {
                command: actual, ..
            } => command = *actual,
            P::Stage {
                stage, succeeded, ..
            } if command == C::Resync => {
                assert_eq!(*stage, S::SyncRemovalDirectory);
                match succeeded {
                    true => resync_after += 1,
                    false => resync_before += 1,
                }
            }
            P::Failure { .. } => panic!("真实四命令不得出现夹具失败：{fact:?}"),
            _ => {}
        }
    }
    assert_eq!((resync_before, resync_after), (2, 1));
}

#[cfg(target_os = "linux")]
#[test]
fn ordinary_retry_uses_bound_directory_after_original_path_is_replaced() {
    struct RestoreDirectory {
        original: std::path::PathBuf,
        moved: std::path::PathBuf,
    }
    impl Drop for RestoreDirectory {
        fn drop(&mut self) {
            if self.original.is_dir() {
                std::fs::remove_dir_all(&self.original).unwrap();
            } else if self.original.exists() {
                std::fs::remove_file(&self.original).unwrap();
            }
            std::fs::rename(&self.moved, &self.original).unwrap();
        }
    }
    let mut rig = Rig::new(FileFaultMode::OrdinaryResync);
    ordinary_until_retry(&mut rig);
    let original = rig.directory.path().to_path_buf();
    let moved = original.with_extension("bound-directory");
    std::fs::rename(&original, &moved).unwrap();
    std::fs::create_dir(&original).unwrap();
    let restore = RestoreDirectory { original, moved };
    // 原路径现为普通文件，若第三次按路径重开父目录，其 fsync 路径不可能成功。
    std::fs::remove_dir(&restore.original).unwrap();
    std::fs::write(&restore.original, "替换路径，不是原目录".as_bytes()).unwrap();
    assert_eq!(
        rig.command(PersistCommand::ResyncRemoval { txn: TXN }),
        PersistCompletion::Resynced { txn: TXN }
    );
    assert!(!rig.hook.failed);
    assert!(rig.core.current().is_none());
    std::fs::remove_file(&restore.original).unwrap();
    std::fs::create_dir(&restore.original).unwrap();
    drop(restore);
}

#[test]
fn ordinary_rejects_wrong_order_and_prepare_without_real_success() {
    for command in [
        PersistCommand::RemoveMarker {
            txn: TXN,
            kind: CleanupKind::Finalize,
        },
        PersistCommand::ResyncRemoval { txn: TXN },
        PersistCommand::RepairClear {
            txn: TXN,
            target: RepairTarget::Latched,
        },
    ] {
        let mut rig = Rig::new(FileFaultMode::OrdinaryResync);
        assert!(matches!(
            rig.command(command),
            PersistCompletion::Refused { .. }
        ));
        assert!(rig.hook.failed);
        assert_eq!(rig.hook.slots, [Some(FaultSlot::Armed); 2]);
    }
    let mut rig = Rig::new(FileFaultMode::OrdinaryResync);
    rig.stop.0 = false;
    assert!(matches!(
        rig.command(PersistCommand::PrepareClear { txn: TXN }),
        PersistCompletion::Refused { .. }
    ));
    assert!(rig.hook.failed);
    assert_eq!(rig.hook.ordinary, Some(OrdinaryStep::Prepare));
    assert_eq!(rig.hook.slots, [Some(FaultSlot::Armed); 2]);
}

#[cfg(target_os = "linux")]
#[test]
fn ordinary_rejects_wrong_complete_transaction_and_repeated_commands() {
    for txn in [
        ClearTxnIdentity {
            txn_id: TXN.txn_id + (1_u128 << 80),
            ..TXN
        },
        ClearTxnIdentity {
            expected_epoch: TXN.expected_epoch - 1,
            ..TXN
        },
        ClearTxnIdentity {
            original_latched: false,
            ..TXN
        },
    ] {
        let mut rig = Rig::new(FileFaultMode::OrdinaryResync);
        assert!(matches!(
            rig.command(PersistCommand::PrepareClear { txn: TXN }),
            PersistCompletion::Prepared { .. }
        ));
        assert!(matches!(
            rig.command(PersistCommand::RemoveMarker {
                txn,
                kind: CleanupKind::Finalize
            }),
            PersistCompletion::Refused {
                reason: WriterRefusal::IdentityMismatch,
                ..
            }
        ));
        assert!(rig.hook.failed);
        assert_eq!(rig.hook.slots, [Some(FaultSlot::Armed); 2]);
    }
    let mut rig = Rig::new(FileFaultMode::OrdinaryResync);
    ordinary_until_retry(&mut rig);
    assert_eq!(
        rig.command(PersistCommand::ResyncRemoval { txn: TXN }),
        PersistCompletion::Resynced { txn: TXN }
    );
    assert!(matches!(
        rig.command(PersistCommand::ResyncRemoval { txn: TXN }),
        PersistCompletion::Refused { .. }
    ));
    assert!(rig.hook.failed);
    assert_eq!(
        rig.hook.slots,
        [Some(FaultSlot::Executed { bad_fd: true }); 2]
    );
}

#[test]
fn ordinary_missing_or_duplicate_slot_input_is_fixture_failure() {
    for duplicate in [false, true] {
        let mut rig = Rig::new(FileFaultMode::OrdinaryResync);
        assert!(matches!(
            rig.command(PersistCommand::PrepareClear { txn: TXN }),
            PersistCompletion::Prepared { .. }
        ));
        rig.hook.command_begin(
            PersistCommand::RemoveMarker {
                txn: TXN,
                kind: CleanupKind::Finalize,
            },
            Some(rig.core.current().unwrap().token()),
        );
        rig.hook.before_step(S::SyncRemovalDirectory);
        if duplicate {
            // 只验证选择回调的拒绝边界，不把手送回调当作真实 fsync 证明。
            assert_eq!(
                rig.hook.directory_sync_input(S::SyncRemovalDirectory),
                DirectorySyncInput::OPathAlias
            );
            assert_eq!(
                rig.hook.directory_sync_input(S::SyncRemovalDirectory),
                DirectorySyncInput::Original
            );
        }
        rig.hook.command_scope_exit(true);
        assert!(rig.hook.failed);
        assert_eq!(rig.hook.slots[1], Some(FaultSlot::Armed));
        assert!(rig.hook.scope.is_none());
    }
}
