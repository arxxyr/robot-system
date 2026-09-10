//! 新同步接缝与原三相语义的真实文件回归。
use super::*;

#[derive(Default)]
struct Probe {
    selected: Vec<ClearTxnStage>,
    succeeded: Vec<ClearTxnStage>,
    refuse: bool,
}
impl StepGuard for Probe {
    fn before(&mut self, stage: ClearTxnStage) -> Result<(), GuardInterrupt> {
        if self.refuse && stage == ClearTxnStage::SyncMarkerDirectory {
            return Err(GuardInterrupt::new(InterruptReason::TestBarrier));
        }
        Ok(())
    }
    fn directory_sync_input(&mut self, stage: ClearTxnStage) -> DirectorySyncInput {
        self.selected.push(stage);
        DirectorySyncInput::Original
    }
    fn after_success(&mut self, stage: ClearTxnStage) {
        self.succeeded.push(stage);
    }
}
fn store() -> (tempfile::TempDir, ClearTxnStore) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("latch");
    std::fs::write(&path, b"1\n").unwrap();
    std::fs::write(crate::estop_latch::backup_path(&path), b"1\n").unwrap();
    let store = ClearTxnStore::open(&path, "w4", "mock").unwrap();
    (dir, store)
}

#[test]
fn prepare_and_both_latch_directories_use_the_same_sync_kernel() {
    let (_dir, mut store) = store();
    let mut probe = Probe::default();
    let prepared = store.prepare_clear(1, 2, true, &mut probe).unwrap();
    assert_eq!(
        probe.selected,
        [
            ClearTxnStage::SyncMarkerDirectory,
            ClearTxnStage::SyncBackupDirectory,
            ClearTxnStage::SyncPrimaryDirectory
        ]
    );
    for stage in &probe.selected {
        assert_eq!(
            probe
                .succeeded
                .iter()
                .filter(|value| *value == stage)
                .count(),
            1
        );
    }
    assert_eq!(prepared.record().txn_id(), 1);
}

#[test]
fn before_and_old_operation_failure_do_not_select_alias_or_report_success() {
    for before in [true, false] {
        let (_dir, mut store) = store();
        if !before {
            store.fail_operation = Some((ClearTxnStage::SyncMarkerDirectory, 1));
        }
        let mut probe = Probe {
            refuse: before,
            ..Probe::default()
        };
        let error = store.prepare_clear(1, 2, true, &mut probe).unwrap_err();
        assert_eq!(
            error.error.phase,
            if before {
                StepPhase::Before
            } else {
                StepPhase::Operation
            }
        );
        assert!(matches!(error.token, PrepareFailureToken::Created(_)));
        assert_eq!(probe.selected, [] as [ClearTxnStage; 0]);
        assert!(
            !probe
                .succeeded
                .contains(&ClearTxnStage::SyncMarkerDirectory)
        );
    }
}

#[test]
fn real_success_before_old_after_failure_still_returns_durable() {
    let (_dir, mut store) = store();
    store.after_step = Some(Box::new(|stage| {
        if stage == ClearTxnStage::SyncMarkerDirectory {
            Err(io::Error::from_raw_os_error(libc::EIO))
        } else {
            Ok(())
        }
    }));
    let mut probe = Probe::default();
    let error = store.prepare_clear(1, 2, true, &mut probe).unwrap_err();
    assert_eq!(error.error.phase, StepPhase::After);
    assert!(matches!(error.token, PrepareFailureToken::Durable(_)));
    assert_eq!(probe.selected, [ClearTxnStage::SyncMarkerDirectory]);
    assert_eq!(
        probe.succeeded.last(),
        Some(&ClearTxnStage::SyncMarkerDirectory)
    );
}
