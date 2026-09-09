use super::*;
use std::ffi::CString;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::Command;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const ROBOT_ID: &str = "测试机器人";
const HARDWARE_ID: &str = "左臂";

#[test]
fn store_and_owned_credentials_can_move_to_the_single_writer_thread() {
    fn assert_send<T: Send>() {}
    assert_send::<ClearTxnStore>();
    assert_send::<PendingClear>();
    assert_send::<PreparedClear>();
    assert_send::<RepairedClear>();
}

fn open_store(path: &Path) -> ClearTxnStore {
    ClearTxnStore::open(path, ROBOT_ID, HARDWARE_ID).unwrap()
}

fn fixture(latched: bool) -> (TempDir, ClearTxnStore) {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir.path().join("estop_latch"));
    write_pair(&store, latched);
    (dir, store)
}

fn write_pair(store: &ClearTxnStore, latched: bool) {
    let value = if latched { b"1\n" } else { b"0\n" };
    std::fs::write(&store.latch_path, value).unwrap();
    std::fs::write(backup_path(&store.latch_path), value).unwrap();
}

fn assert_pair(store: &ClearTxnStore, latched: bool) {
    let expected = if latched { b"1\n" } else { b"0\n" };
    assert_eq!(std::fs::read(&store.latch_path).unwrap(), expected);
    assert_eq!(
        std::fs::read(backup_path(&store.latch_path)).unwrap(),
        expected
    );
}

fn pending_record(store: &ClearTxnStore, original_latched: bool) -> ClearTxnRecord {
    ClearTxnRecord {
        target_identity: store.target_identity,
        txn_id: 123,
        expected_epoch: 456,
        original_latched,
    }
}

fn write_readable_marker(store: &ClearTxnStore, record: ClearTxnRecord) {
    // 此夹具刻意不做 fsync：可读记录不得被当成已持久化凭证。
    std::fs::write(&store.marker_path, record.encode()).unwrap();
}

fn record_steps(store: &mut ClearTxnStore) -> Arc<Mutex<Vec<ClearTxnStage>>> {
    let steps = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&steps);
    store.after_step = Some(Box::new(move |stage| {
        observed.lock().unwrap().push(stage);
        Ok(())
    }));
    steps
}

fn fail_after(store: &mut ClearTxnStore, failure: ClearTxnStage) {
    store.after_step = Some(Box::new(move |stage| {
        if stage == failure {
            Err(io::Error::other("测试在已完成文件步骤后中断"))
        } else {
            Ok(())
        }
    }));
}

fn refresh_checksum(bytes: &mut [u8]) {
    let checksum = blake3::hash(&bytes[..CHECKSUM_OFFSET]);
    bytes[CHECKSUM_OFFSET..RECORD_BYTES].copy_from_slice(checksum.as_bytes());
}

#[test]
fn prepare_has_fixed_record_and_strict_backup_first_order() {
    use ClearTxnStage as Stage;
    let (_dir, mut store) = fixture(true);
    let steps = record_steps(&mut store);
    let prepared = store.prepare_clear(u128::MAX, u64::MAX, true).unwrap();
    assert_eq!(prepared.record().txn_id(), u128::MAX);
    assert_eq!(prepared.record().expected_epoch(), u64::MAX);
    assert!(prepared.record().original_latched());
    assert_pair(&store, false);
    let bytes = std::fs::read(&store.marker_path).unwrap();
    assert_eq!(bytes.len(), 112);
    assert_eq!(&bytes[..8], b"RBTCLRTX");
    assert_eq!(&bytes[8..10], &[1, 0]);
    assert_eq!(&bytes[48..64], &u128::MAX.to_le_bytes());
    assert_eq!(&bytes[64..72], &u64::MAX.to_le_bytes());
    assert_eq!(bytes[72], 1);
    assert_eq!(
        std::fs::metadata(&store.marker_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        &*steps.lock().unwrap(),
        &[
            Stage::CreateMarker,
            Stage::WriteMarker,
            Stage::SyncMarkerFile,
            Stage::SyncMarkerDirectory,
            Stage::CreateBackup,
            Stage::WriteBackup,
            Stage::SyncBackupFile,
            Stage::RenameBackup,
            Stage::SyncBackupDirectory,
            Stage::CreatePrimary,
            Stage::WritePrimary,
            Stage::SyncPrimaryFile,
            Stage::RenamePrimary,
            Stage::SyncPrimaryDirectory,
        ]
    );
    store.after_step = None;
    assert_eq!(
        store.inspect_pending().unwrap().unwrap().record(),
        prepared.record()
    );
    store.remove_prepared_marker(prepared).unwrap();
    assert!(!store.marker_path.exists());
    assert!(!open_store(&store.latch_path).recover_and_load().unwrap());
}

#[test]
fn zero_transaction_id_and_existing_marker_are_rejected_without_latch_writes() {
    let (_dir, mut store) = fixture(true);
    let error = store.prepare_clear(0, 1, true).unwrap_err();
    assert_eq!(error.stage, ClearTxnStage::ValidateMarker);
    assert_eq!(error.source.kind(), io::ErrorKind::InvalidInput);
    assert!(!store.marker_path.exists());
    assert_pair(&store, true);

    for contents in [
        Vec::new(),
        "已有的损坏标记".as_bytes().to_vec(),
        pending_record(&store, true).encode().to_vec(),
    ] {
        std::fs::write(&store.marker_path, &contents).unwrap();
        let error = store.prepare_clear(2, 3, true).unwrap_err();
        assert_eq!(error.stage, ClearTxnStage::CreateMarker);
        assert_eq!(error.source.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&store.marker_path).unwrap(), contents);
        assert_pair(&store, true);
    }
}

#[test]
fn record_rejects_every_structural_corruption_before_old_loader_can_self_heal() {
    let (_dir, mut store) = fixture(false);
    let valid = pending_record(&store, true).encode();
    let mut malformed = vec![Vec::new(), valid[..111].to_vec()];
    let mut trailing = valid.to_vec();
    trailing.push(0);
    malformed.push(trailing);
    let mut very_long = valid.to_vec();
    very_long.resize(4096, 0);
    malformed.push(very_long);
    // 重算校验，确保逐项验证格式本身，而不只命中校验和错误。
    for (offset, replacement) in [
        (0, b'X'),
        (8, 2),
        (10, 1),
        (15, 1),
        (72, 2),
        (73, 1),
        (79, 1),
    ] {
        let mut bytes = valid;
        bytes[offset] = replacement;
        refresh_checksum(&mut bytes);
        malformed.push(bytes.to_vec());
    }
    let mut zero_txn = valid;
    zero_txn[48..64].fill(0);
    refresh_checksum(&mut zero_txn);
    malformed.push(zero_txn.to_vec());
    let mut bad_checksum = valid;
    bad_checksum[111] ^= 1;
    malformed.push(bad_checksum.to_vec());
    let mut bad_identity = valid;
    bad_identity[16] ^= 1;
    refresh_checksum(&mut bad_identity);
    malformed.push(bad_identity.to_vec());

    std::fs::remove_file(&store.latch_path).unwrap();
    for bytes in malformed {
        std::fs::write(&store.marker_path, &bytes).unwrap();
        let error = store.recover_and_load().unwrap_err();
        assert_eq!(error.stage, ClearTxnStage::ValidateMarker);
        assert_eq!(error.source.kind(), io::ErrorKind::InvalidData);
        assert!(
            !store.latch_path.exists(),
            "坏标记不允许旧 loader 回写 primary"
        );
        assert_eq!(
            std::fs::read(backup_path(&store.latch_path)).unwrap(),
            b"0\n"
        );
        assert_eq!(std::fs::read(&store.marker_path).unwrap(), bytes);
    }
}

#[test]
fn identity_binds_robot_hardware_and_canonical_target_without_concatenation_ambiguity() {
    let (dir, store) = fixture(true);
    let record = pending_record(&store, true);
    write_readable_marker(&store, record);
    for (robot_id, hardware_id) in [("另一机器人", HARDWARE_ID), (ROBOT_ID, "右臂")] {
        let mut other = ClearTxnStore::open(&store.latch_path, robot_id, hardware_id).unwrap();
        assert_eq!(
            other.recover_and_load().unwrap_err().stage,
            ClearTxnStage::ValidateMarker
        );
        assert_pair(&store, true);
    }
    let mut other_target = open_store(&dir.path().join("other_latch"));
    write_pair(&other_target, false);
    write_readable_marker(&other_target, record);
    assert_eq!(
        other_target.recover_and_load().unwrap_err().stage,
        ClearTxnStage::ValidateMarker
    );
    assert_pair(&other_target, false);
    let first = ClearTxnStore::open(&store.latch_path, "ab", "c").unwrap();
    let second = ClearTxnStore::open(&store.latch_path, "a", "bc").unwrap();
    assert_ne!(first.target_identity, second.target_identity);
}

#[test]
fn canonical_parent_alias_and_atomic_latch_replacement_preserve_identity() {
    let dir = TempDir::new().unwrap();
    let actual = dir.path().join("actual");
    std::fs::create_dir(&actual).unwrap();
    let alias = dir.path().join("alias");
    symlink(&actual, &alias).unwrap();
    let mut original = open_store(&actual.join("estop_latch"));
    let missing_identity = original.target_identity;
    write_pair(&original, true);
    let prepared = original.prepare_clear(1, 2, true).unwrap();
    let mut through_alias = open_store(&alias.join("estop_latch"));
    assert_eq!(through_alias.target_identity, missing_identity);
    assert_eq!(
        through_alias.inspect_pending().unwrap().unwrap().record(),
        prepared.record()
    );
    assert!(through_alias.recover_and_load().unwrap());
    assert_pair(&original, true);
    assert_eq!(
        open_store(&original.latch_path).target_identity,
        missing_identity
    );
}

#[test]
fn no_marker_preserves_original_loader_and_missing_policy() {
    for value in [false, true] {
        let (_dir, mut store) = fixture(value);
        assert_eq!(store.recover_and_load().unwrap(), value);
        for missing in [true, false] {
            if missing {
                std::fs::remove_file(&store.latch_path).unwrap();
            } else {
                std::fs::write(&store.latch_path, "损坏").unwrap();
            }
            assert_eq!(store.recover_and_load().unwrap(), value);
            assert_pair(&store, value);
        }
        std::fs::remove_file(&store.latch_path).unwrap();
        std::fs::remove_file(backup_path(&store.latch_path)).unwrap();
        let error = store.recover_and_load().unwrap_err();
        assert_eq!(error.stage, ClearTxnStage::LoadLatch);
        assert_eq!(error.source.kind(), io::ErrorKind::NotFound);
        assert!(!store.marker_path.exists());
    }
}

#[test]
fn valid_pending_recovers_conservatively_regardless_of_latch_contents() {
    for (primary, backup) in [
        (Some(b"0\n".as_slice()), Some(b"0\n".as_slice())),
        (Some(b"0\n".as_slice()), Some(b"1\n".as_slice())),
        (Some(b"1\n".as_slice()), Some(b"0\n".as_slice())),
        (Some(b"1\n".as_slice()), Some(b"1\n".as_slice())),
        (None, None),
        (Some("损坏".as_bytes()), Some(b"0\n".as_slice())),
    ] {
        let (_dir, mut store) = fixture(false);
        for (path, contents) in [
            (&store.latch_path, primary),
            (&backup_path(&store.latch_path), backup),
        ] {
            match contents {
                Some(bytes) => std::fs::write(path, bytes).unwrap(),
                None => std::fs::remove_file(path).unwrap(),
            }
        }
        write_readable_marker(&store, pending_record(&store, false));
        assert!(store.recover_and_load().unwrap());
        assert_pair(&store, true);
        assert!(!store.marker_path.exists());
        assert!(open_store(&store.latch_path).recover_and_load().unwrap());
    }
}

#[test]
fn marker_read_failure_prevents_latch_load_or_self_heal() {
    let (_dir, mut store) = fixture(false);
    std::fs::remove_file(&store.latch_path).unwrap();
    std::fs::create_dir(&store.marker_path).unwrap();
    let error = store.recover_and_load().unwrap_err();
    assert_eq!(error.stage, ClearTxnStage::ReadMarker);
    assert!(!store.latch_path.exists());
    assert_eq!(
        std::fs::read(backup_path(&store.latch_path)).unwrap(),
        b"0\n"
    );
}

#[derive(Clone, Copy, Debug)]
enum SpecialMarker {
    Symlink,
    DanglingSymlink,
    Fifo,
    Directory,
}

const SPECIAL_MARKERS: [SpecialMarker; 4] = [
    SpecialMarker::Symlink,
    SpecialMarker::DanglingSymlink,
    SpecialMarker::Fifo,
    SpecialMarker::Directory,
];

fn install_special_marker(store: &ClearTxnStore, kind: SpecialMarker) {
    let target = store.marker_path.with_extension("target");
    match kind {
        SpecialMarker::Symlink => {
            std::fs::write(&target, pending_record(store, true).encode()).unwrap();
            symlink(&target, &store.marker_path).unwrap();
        }
        SpecialMarker::DanglingSymlink => symlink(&target, &store.marker_path).unwrap(),
        SpecialMarker::Fifo => {
            let path = CString::new(store.marker_path.as_os_str().as_encoded_bytes()).unwrap();
            // 安全：CString 提供有效且以零结尾的路径；mkfifo 不保存该指针。
            let result = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
            assert_eq!(
                result,
                0,
                "创建测试 FIFO 失败：{}",
                io::Error::last_os_error()
            );
        }
        SpecialMarker::Directory => std::fs::create_dir(&store.marker_path).unwrap(),
    }
}

#[test]
fn special_marker_types_are_rejected_before_fallback_and_never_treated_as_missing() {
    for kind in SPECIAL_MARKERS {
        let (_dir, mut store) = fixture(false);
        std::fs::remove_file(&store.latch_path).unwrap();
        install_special_marker(&store, kind);
        let error = store.recover_and_load().unwrap_err();
        assert_eq!(error.stage, ClearTxnStage::ReadMarker, "{kind:?}");
        assert_ne!(error.source.kind(), io::ErrorKind::NotFound, "{kind:?}");
        assert!(std::fs::symlink_metadata(&store.marker_path).is_ok());
        assert!(!store.latch_path.exists(), "{kind:?} 不得触发 primary 自愈");
        assert_eq!(
            std::fs::read(backup_path(&store.latch_path)).unwrap(),
            b"0\n"
        );
    }
}

#[test]
fn all_credential_matching_paths_reject_special_marker_replacements() {
    for kind in SPECIAL_MARKERS {
        for operation in 0..4 {
            let (_dir, mut store) = fixture(true);
            let mut prepared = Some(store.prepare_clear(123, 456, true).unwrap());
            let pending = store.inspect_pending().unwrap().unwrap();
            let repaired = match operation {
                3 => Some(
                    store
                        .repair_prepared(prepared.take().unwrap(), RepairTarget::Latched)
                        .unwrap(),
                ),
                _ => None,
            };
            std::fs::remove_file(&store.marker_path).unwrap();
            install_special_marker(&store, kind);
            write_pair(&store, true);
            let error = match operation {
                0 => store
                    .repair_prepared(prepared.unwrap(), RepairTarget::Original)
                    .unwrap_err(),
                1 => store.remove_prepared_marker(prepared.unwrap()).unwrap_err(),
                2 => store
                    .repair_pending(pending, RepairTarget::Original)
                    .unwrap_err(),
                3 => store.remove_repaired_marker(repaired.unwrap()).unwrap_err(),
                _ => unreachable!(),
            };
            assert_eq!(error.stage, ClearTxnStage::ReadMarker, "{kind:?}");
            assert_ne!(error.source.kind(), io::ErrorKind::NotFound, "{kind:?}");
            assert!(std::fs::symlink_metadata(&store.marker_path).is_ok());
            assert_pair(&store, true);
        }
    }
}

fn temporary_candidate(target: &Path, sequence: u64) -> PathBuf {
    target.with_file_name(format!(
        "estop_latch.clear_tmp.{}.{sequence}",
        std::process::id()
    ))
}

#[test]
fn temporary_name_collision_uses_next_sequence_without_overwriting_existing_file() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("estop_latch");
    let existing = temporary_candidate(&target, 0);
    std::fs::write(&existing, "保留已有临时内容").unwrap();
    let sequence = AtomicU64::new(0);
    let (temporary, file) = TemporaryLatch::create_with_sequence(&target, &sequence).unwrap();
    let allocated = temporary.path.clone();
    assert_eq!(allocated, temporary_candidate(&target, 1));
    assert_eq!(sequence.load(Ordering::Relaxed), 2);
    assert_eq!(
        std::fs::read(&existing).unwrap(),
        "保留已有临时内容".as_bytes()
    );
    assert_eq!(file.metadata().unwrap().len(), 0);
    drop(file);
    drop(temporary);
    assert!(!allocated.exists());
    assert!(existing.exists());
}

#[test]
fn temporary_name_collision_budget_stops_at_64_without_trying_the_available_next_name() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("estop_latch");
    for sequence in 0..64 {
        std::fs::write(
            temporary_candidate(&target, sequence),
            sequence.to_le_bytes(),
        )
        .unwrap();
    }
    let sequence = AtomicU64::new(0);
    let error = TemporaryLatch::create_with_sequence(&target, &sequence)
        .err()
        .expect("64 次冲突必须拒绝");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(sequence.load(Ordering::Relaxed), 64);
    assert!(!temporary_candidate(&target, 64).exists());
    for sequence in 0..64 {
        assert_eq!(
            std::fs::read(temporary_candidate(&target, sequence)).unwrap(),
            sequence.to_le_bytes()
        );
    }
}

#[test]
fn temporary_sequence_exhaustion_does_not_wrap_or_allocate() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("estop_latch");
    let sequence = AtomicU64::new(u64::MAX);
    let error = TemporaryLatch::create_with_sequence(&target, &sequence)
        .err()
        .expect("耗尽必须拒绝");
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert_eq!(sequence.load(Ordering::Relaxed), u64::MAX);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn temporary_non_collision_io_error_is_not_retried() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("missing_parent").join("estop_latch");
    let sequence = AtomicU64::new(0);
    let error = TemporaryLatch::create_with_sequence(&target, &sequence)
        .err()
        .expect("不存在的父目录必须拒绝");
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert_eq!(sequence.load(Ordering::Relaxed), 1);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn prepare_interruption_before_marker_durability_never_writes_zero() {
    for stage in [
        ClearTxnStage::CreateMarker,
        ClearTxnStage::WriteMarker,
        ClearTxnStage::SyncMarkerFile,
        ClearTxnStage::SyncMarkerDirectory,
    ] {
        let (_dir, mut store) = fixture(true);
        fail_after(&mut store, stage);
        assert_eq!(store.prepare_clear(1, 2, true).unwrap_err().stage, stage);
        assert_pair(&store, true);
        assert!(store.marker_path.exists());
        let mut reopened = open_store(&store.latch_path);
        match stage {
            ClearTxnStage::CreateMarker => assert_eq!(
                reopened.recover_and_load().unwrap_err().stage,
                ClearTxnStage::ValidateMarker
            ),
            _ => assert!(reopened.recover_and_load().unwrap()),
        }
    }
}

#[test]
fn prepared_repair_honors_explicit_target_and_keeps_marker_until_consumed() {
    for (original, target, expected) in [
        (false, RepairTarget::Original, false),
        (true, RepairTarget::Original, true),
        (false, RepairTarget::Latched, true),
        (true, RepairTarget::Latched, true),
    ] {
        let (_dir, mut store) = fixture(original);
        let prepared = store.prepare_clear(11, 12, original).unwrap();
        let repaired = store.repair_prepared(prepared, target).unwrap();
        assert_pair(&store, expected);
        assert!(store.marker_path.exists());
        store.remove_repaired_marker(repaired).unwrap();
        assert!(!store.marker_path.exists());
        assert_eq!(
            open_store(&store.latch_path).recover_and_load().unwrap(),
            expected
        );
    }
}

#[test]
fn readable_pending_false_repair_rechecks_and_resynchronizes_before_latch_writes() {
    use ClearTxnStage as Stage;
    let (_dir, mut store) = fixture(true);
    write_readable_marker(&store, pending_record(&store, false));
    let pending = store.inspect_pending().unwrap().unwrap();
    let steps = record_steps(&mut store);
    let repaired = store
        .repair_pending(pending, RepairTarget::Original)
        .unwrap();
    assert_pair(&store, false);
    assert_eq!(
        &steps.lock().unwrap()[..5],
        &[
            Stage::ReadMarker,
            Stage::ValidateMarker,
            Stage::SyncMarkerFile,
            Stage::SyncMarkerDirectory,
            Stage::CreateBackup
        ]
    );
    assert!(store.marker_path.exists());
    store.remove_repaired_marker(repaired).unwrap();
}

#[test]
fn conservative_recovery_strictly_writes_primary_then_backup_before_marker_cleanup() {
    use ClearTxnStage as Stage;
    let (_dir, mut store) = fixture(false);
    write_readable_marker(&store, pending_record(&store, false));
    let steps = record_steps(&mut store);
    assert!(store.recover_and_load().unwrap());
    assert_pair(&store, true);
    assert_eq!(
        &*steps.lock().unwrap(),
        &[
            Stage::ReadMarker,
            Stage::ValidateMarker,
            Stage::ReadMarker,
            Stage::ValidateMarker,
            Stage::SyncMarkerFile,
            Stage::SyncMarkerDirectory,
            Stage::CreatePrimary,
            Stage::WritePrimary,
            Stage::SyncPrimaryFile,
            Stage::RenamePrimary,
            Stage::SyncPrimaryDirectory,
            Stage::CreateBackup,
            Stage::WriteBackup,
            Stage::SyncBackupFile,
            Stage::RenameBackup,
            Stage::SyncBackupDirectory,
            Stage::ReadMarker,
            Stage::ValidateMarker,
            Stage::RemoveMarker,
            Stage::SyncRemovalDirectory,
        ]
    );
}

#[test]
fn failed_pending_marker_resync_never_writes_false() {
    for stage in [
        ClearTxnStage::SyncMarkerFile,
        ClearTxnStage::SyncMarkerDirectory,
    ] {
        let (_dir, mut store) = fixture(true);
        write_readable_marker(&store, pending_record(&store, false));
        let pending = store.inspect_pending().unwrap().unwrap();
        fail_after(&mut store, stage);
        assert_eq!(
            store
                .repair_pending(pending, RepairTarget::Original)
                .unwrap_err()
                .stage,
            stage
        );
        assert_pair(&store, true);
        assert!(store.marker_path.exists());
    }
}

fn replace_record(store: &ClearTxnStore, record: ClearTxnRecord, field: usize) {
    let mut replacement = record;
    match field {
        0 => replacement.txn_id += 1,
        1 => replacement.expected_epoch += 1,
        2 => replacement.original_latched = !replacement.original_latched,
        _ => unreachable!(),
    }
    write_readable_marker(store, replacement);
}

#[test]
fn full_record_mismatch_rejects_old_prepared_pending_and_repaired_credentials() {
    for field in 0..3 {
        for operation in 0..4 {
            let (_dir, mut store) = fixture(true);
            let mut prepared = Some(store.prepare_clear(11, 12, true).unwrap());
            let record = *prepared.as_ref().unwrap().record();
            let pending = store.inspect_pending().unwrap().unwrap();
            let repaired = match operation {
                3 => Some(
                    store
                        .repair_prepared(prepared.take().unwrap(), RepairTarget::Latched)
                        .unwrap(),
                ),
                _ => None,
            };
            replace_record(&store, record, field);
            write_pair(&store, true);
            let replacement = std::fs::read(&store.marker_path).unwrap();
            let error = match operation {
                0 => store
                    .repair_prepared(prepared.unwrap(), RepairTarget::Original)
                    .unwrap_err(),
                1 => store.remove_prepared_marker(prepared.unwrap()).unwrap_err(),
                2 => store
                    .repair_pending(pending, RepairTarget::Original)
                    .unwrap_err(),
                3 => store.remove_repaired_marker(repaired.unwrap()).unwrap_err(),
                _ => unreachable!(),
            };
            assert_eq!(error.stage, ClearTxnStage::ValidateMarker);
            assert_eq!(error.source.kind(), io::ErrorKind::InvalidData);
            assert_pair(&store, true);
            assert_eq!(std::fs::read(&store.marker_path).unwrap(), replacement);
        }
    }
}

#[test]
fn corrupted_marker_rejects_prepared_repair_before_any_latch_write() {
    let (_dir, mut store) = fixture(true);
    let prepared = store.prepare_clear(1, 2, true).unwrap();
    let mut bytes = std::fs::read(&store.marker_path).unwrap();
    bytes[100] ^= 1;
    std::fs::write(&store.marker_path, &bytes).unwrap();
    assert_eq!(
        store
            .repair_prepared(prepared, RepairTarget::Latched)
            .unwrap_err()
            .stage,
        ClearTxnStage::ValidateMarker
    );
    assert_pair(&store, false);
    assert_eq!(std::fs::read(&store.marker_path).unwrap(), bytes);
}

#[test]
fn repair_failure_at_every_latch_step_keeps_marker_for_conservative_recovery() {
    let stages = LatchCopy::Backup
        .stages()
        .into_iter()
        .chain(LatchCopy::Primary.stages());
    for stage in stages {
        let (_dir, mut store) = fixture(true);
        let prepared = store.prepare_clear(1, 2, true).unwrap();
        fail_after(&mut store, stage);
        assert_eq!(
            store
                .repair_prepared(prepared, RepairTarget::Latched)
                .unwrap_err()
                .stage,
            stage
        );
        assert!(store.marker_path.exists());
        let mut reopened = open_store(&store.latch_path);
        assert!(reopened.recover_and_load().unwrap());
        assert_pair(&reopened, true);
        assert!(!reopened.marker_path.exists());
    }
}

#[test]
fn real_backup_rename_failure_preserves_marker_and_primary() {
    let (_dir, mut store) = fixture(true);
    std::fs::remove_file(backup_path(&store.latch_path)).unwrap();
    std::fs::create_dir(backup_path(&store.latch_path)).unwrap();
    let error = store.prepare_clear(1, 2, true).unwrap_err();
    assert_eq!(error.stage, ClearTxnStage::RenameBackup);
    assert_eq!(std::fs::read(&store.latch_path).unwrap(), b"1\n");
    assert!(store.marker_path.exists());
    assert!(store.inspect_pending().unwrap().is_some());
}

#[test]
fn unlink_success_then_real_directory_sync_failure_leaves_marker_absent() {
    let dir = TempDir::new().unwrap();
    let parent = dir.path().join("original");
    let moved = dir.path().join("moved");
    std::fs::create_dir(&parent).unwrap();
    let mut store = open_store(&parent.join("estop_latch"));
    write_pair(&store, true);
    let prepared = store.prepare_clear(1, 2, true).unwrap();
    let rename_from = parent.clone();
    let rename_to = moved.clone();
    store.after_step = Some(Box::new(move |stage| {
        if stage == ClearTxnStage::RemoveMarker {
            std::fs::rename(&rename_from, &rename_to)?;
        }
        Ok(())
    }));
    let error = store.remove_prepared_marker(prepared).unwrap_err();
    assert_eq!(error.stage, ClearTxnStage::SyncRemovalDirectory);
    assert_eq!(error.source.kind(), io::ErrorKind::NotFound);
    assert!(!moved.join("estop_latch.clear_pending").exists());
    std::fs::rename(&moved, &parent).unwrap();
    assert!(!store.marker_path.exists());
    assert_pair(&store, false);
    assert!(!open_store(&store.latch_path).recover_and_load().unwrap());
}

#[test]
fn recovery_failure_never_reports_success_and_retains_actual_marker_state() {
    for stage in [
        ClearTxnStage::SyncMarkerFile,
        ClearTxnStage::RenameBackup,
        ClearTxnStage::SyncPrimaryDirectory,
        ClearTxnStage::RemoveMarker,
        ClearTxnStage::SyncRemovalDirectory,
    ] {
        let (_dir, mut store) = fixture(false);
        write_readable_marker(&store, pending_record(&store, false));
        fail_after(&mut store, stage);
        assert_eq!(store.recover_and_load().unwrap_err().stage, stage);
        let removed = matches!(
            stage,
            ClearTxnStage::RemoveMarker | ClearTxnStage::SyncRemovalDirectory
        );
        assert_eq!(store.marker_path.exists(), !removed);
        assert!(open_store(&store.latch_path).recover_and_load().unwrap());
        assert_pair(&store, true);
    }
}

#[test]
fn process_interruption_at_real_file_checkpoints_recovers_observed_state() {
    use ClearTxnStage as Stage;
    let stages = [
        Stage::CreateMarker,
        Stage::WriteMarker,
        Stage::SyncMarkerFile,
        Stage::SyncMarkerDirectory,
    ]
    .into_iter()
    .chain(LatchCopy::Backup.stages())
    .chain(LatchCopy::Primary.stages())
    .chain([Stage::RemoveMarker, Stage::SyncRemovalDirectory]);
    for stage in stages {
        let (_dir, store) = fixture(true);
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "clear_estop_transaction::tests::crash_checkpoint_child",
                "--nocapture",
            ])
            .env("ROBOT_CLEAR_TXN_TEST_CHILD_PATH", &store.latch_path)
            .env("ROBOT_CLEAR_TXN_TEST_CHILD_STAGE", format!("{stage:?}"))
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(73),
            "{stage:?}: 子进程未命中真实文件步骤，stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let mut reopened = open_store(&store.latch_path);
        match stage {
            Stage::CreateMarker => {
                assert_eq!(std::fs::metadata(&store.marker_path).unwrap().len(), 0);
                assert_eq!(
                    reopened.recover_and_load().unwrap_err().stage,
                    Stage::ValidateMarker
                );
                assert_pair(&store, true);
            }
            Stage::RemoveMarker | Stage::SyncRemovalDirectory => {
                assert!(!store.marker_path.exists());
                assert!(!reopened.recover_and_load().unwrap());
                assert_pair(&store, false);
            }
            _ => {
                assert!(store.marker_path.exists());
                assert!(reopened.recover_and_load().unwrap());
                assert_pair(&store, true);
                assert!(!store.marker_path.exists());
            }
        }
    }
}

#[test]
fn crash_checkpoint_child() {
    let Some(path) = std::env::var_os("ROBOT_CLEAR_TXN_TEST_CHILD_PATH") else {
        return;
    };
    let checkpoint = std::env::var("ROBOT_CLEAR_TXN_TEST_CHILD_STAGE").unwrap();
    let mut store = open_store(Path::new(&path));
    store.after_step = Some(Box::new(move |stage| {
        if format!("{stage:?}") == checkpoint {
            // 真正终止进程，跳过 Rust 析构；只证明进程中断后的文件状态，不模拟断电。
            std::process::exit(73);
        }
        Ok(())
    }));
    let prepared = store.prepare_clear(1, 2, true).unwrap();
    store.remove_prepared_marker(prepared).unwrap();
    panic!("子进程未命中指定文件步骤");
}
