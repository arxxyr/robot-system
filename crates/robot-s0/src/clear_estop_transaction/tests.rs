use super::*;
use std::ffi::CString;
use std::fs::OpenOptions;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

use crate::estop_latch::backup_path;

const ROBOT_ID: &str = "测试机器人";
const HARDWARE_ID: &str = "左臂";

/// 在指定阶段之前中断的守门；其余阶段放行。
struct FailBefore(ClearTxnStage);

impl StepGuard for FailBefore {
    fn before(&mut self, stage: ClearTxnStage) -> Result<(), GuardInterrupt> {
        if stage == self.0 {
            Err(GuardInterrupt::new(InterruptReason::TestBarrier))
        } else {
            Ok(())
        }
    }
}

#[test]
fn store_and_owned_credentials_can_move_to_the_single_writer_thread() {
    fn assert_send<T: Send>() {}
    assert_send::<ClearTxnStore>();
    assert_send::<PendingClear>();
    assert_send::<PreparedClear>();
    assert_send::<DurableMarker>();
    assert_send::<RepairedClear>();
    assert_send::<CreatedMarker>();
    assert_send::<UnsyncedRemoval>();
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

/// 记录每个已完成步骤，并在第 `fail_times` 次命中 `failure` 之前（含）的后置检查点失败。
fn record_and_fail_after(
    store: &mut ClearTxnStore,
    failure: ClearTxnStage,
    fail_times: usize,
) -> Arc<Mutex<Vec<ClearTxnStage>>> {
    let steps = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&steps);
    let mut remaining = fail_times;
    store.after_step = Some(Box::new(move |stage| {
        observed.lock().unwrap().push(stage);
        if stage == failure && remaining > 0 {
            remaining -= 1;
            return Err(io::Error::other("测试在已完成文件步骤后中断"));
        }
        Ok(())
    }));
    steps
}

fn refresh_checksum(bytes: &mut [u8]) {
    let checksum = blake3::hash(&bytes[..CHECKSUM_OFFSET]);
    bytes[CHECKSUM_OFFSET..RECORD_BYTES].copy_from_slice(checksum.as_bytes());
}

fn prepare(store: &mut ClearTxnStore) -> PreparedClear {
    store.prepare_clear(1, 2, true, &mut NoGuard).unwrap()
}

/// 用后置检查点在 `stage` 之后中断，取得本 store 创建的 `CreatedMarker`。
fn created_marker(store: &mut ClearTxnStore, stage: ClearTxnStage) -> CreatedMarker {
    fail_after(store, stage);
    let error = store.prepare_clear(7, 8, true, &mut NoGuard).unwrap_err();
    store.after_step = None;
    assert_eq!(error.error.stage, stage);
    assert_eq!(error.error.phase, StepPhase::After);
    match error.token {
        PrepareFailureToken::Created(created) => created,
        other => panic!("{stage:?} 之后必须返还 Created，实际 {other:?}"),
    }
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn prepare_has_fixed_record_and_strict_backup_first_order() {
    use ClearTxnStage as Stage;
    let (_dir, mut store) = fixture(true);
    let steps = record_steps(&mut store);
    let prepared = store
        .prepare_clear(u128::MAX, u64::MAX, true, &mut NoGuard)
        .unwrap();
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
            Stage::SampleMarkerIdentity,
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
    let pending = store.inspect_pending().unwrap().unwrap();
    assert_eq!(pending.record(), prepared.record());
    assert_eq!(pending.identity(), prepared.identity());
    store.remove(prepared, &mut NoGuard).unwrap();
    assert!(!store.marker_path.exists());
    assert!(!open_store(&store.latch_path).recover_and_load().unwrap());
}

#[test]
fn zero_transaction_id_and_existing_marker_are_rejected_without_latch_writes() {
    let (_dir, mut store) = fixture(true);
    let error = store.prepare_clear(0, 1, true, &mut NoGuard).unwrap_err();
    assert_eq!(error.error.stage, ClearTxnStage::ValidateMarker);
    assert_eq!(error.error.source.kind(), io::ErrorKind::InvalidInput);
    assert!(matches!(error.token, PrepareFailureToken::NotOwned));
    assert!(!store.marker_path.exists());
    assert_pair(&store, true);

    for contents in [
        Vec::new(),
        "已有的损坏标记".as_bytes().to_vec(),
        pending_record(&store, true).encode().to_vec(),
    ] {
        std::fs::write(&store.marker_path, &contents).unwrap();
        let error = store.prepare_clear(2, 3, true, &mut NoGuard).unwrap_err();
        assert_eq!(error.error.stage, ClearTxnStage::CreateMarker);
        assert_eq!(error.error.phase, StepPhase::Operation);
        assert_eq!(error.error.source.kind(), io::ErrorKind::AlreadyExists);
        // NotOwned 只表示本次未取得所有权：既有标记原样保留。
        assert!(matches!(error.token, PrepareFailureToken::NotOwned));
        assert_eq!(std::fs::read(&store.marker_path).unwrap(), contents);
        assert_pair(&store, true);
    }
}

#[test]
fn open_rejects_missing_file_name_and_non_directory_parent() {
    let dir = TempDir::new().unwrap();
    let error = ClearTxnStore::open(Path::new("/"), ROBOT_ID, HARDWARE_ID)
        .err()
        .map(|error| error.stage);
    assert_eq!(error, Some(ClearTxnStage::OpenStore));
    let file_parent = dir.path().join("not_a_directory");
    std::fs::write(&file_parent, b"x").unwrap();
    let error =
        ClearTxnStore::open(&file_parent.join("estop_latch"), ROBOT_ID, HARDWARE_ID).unwrap_err();
    assert_eq!(error.stage, ClearTxnStage::OpenStore);
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
    let prepared = prepare(&mut original);
    let mut through_alias = open_store(&alias.join("estop_latch"));
    assert_eq!(through_alias.target_identity, missing_identity);
    assert_eq!(
        through_alias.directory_identity(),
        original.directory_identity()
    );
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
        std::fs::write(&store.latch_path, "损坏").unwrap();
        std::fs::write(backup_path(&store.latch_path), "也损坏").unwrap();
        let error = store.recover_and_load().unwrap_err();
        assert_eq!(error.stage, ClearTxnStage::LoadLatch);
        assert_eq!(error.source.kind(), io::ErrorKind::InvalidData);
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
fn all_credential_matching_paths_reject_special_marker_replacements_and_lose_credentials() {
    for kind in SPECIAL_MARKERS {
        for operation in 0..4 {
            let (_dir, mut store) = fixture(true);
            let mut prepared = Some(prepare(&mut store));
            let pending = store.inspect_pending().unwrap().unwrap();
            let repaired = match operation {
                3 => Some(
                    store
                        .repair(
                            prepared.take().unwrap(),
                            RepairTarget::Latched,
                            &mut NoGuard,
                        )
                        .unwrap(),
                ),
                _ => None,
            };
            std::fs::remove_file(&store.marker_path).unwrap();
            install_special_marker(&store, kind);
            write_pair(&store, true);
            let error = match operation {
                0 => {
                    let error = store
                        .repair(prepared.unwrap(), RepairTarget::Original, &mut NoGuard)
                        .unwrap_err();
                    assert!(error.token.is_none(), "{kind:?} 确认不符必须失权");
                    error.error
                }
                1 => {
                    let error = store.remove(prepared.unwrap(), &mut NoGuard).unwrap_err();
                    assert!(matches!(error.token, RemoveFailureToken::Lost), "{kind:?}");
                    error.error
                }
                2 => store
                    .repair_pending(pending, RepairTarget::Original)
                    .unwrap_err(),
                3 => {
                    let error = store.remove(repaired.unwrap(), &mut NoGuard).unwrap_err();
                    assert!(matches!(error.token, RemoveFailureToken::Lost), "{kind:?}");
                    error.error
                }
                _ => unreachable!(),
            };
            assert_eq!(error.stage, ClearTxnStage::ReadMarker, "{kind:?}");
            assert_eq!(error.phase, StepPhase::Operation, "{kind:?}");
            assert_ne!(error.source.kind(), io::ErrorKind::NotFound, "{kind:?}");
            assert!(std::fs::symlink_metadata(&store.marker_path).is_ok());
            assert_pair(&store, true);
        }
    }
}

fn temporary_candidate(dir: &Path, sequence: u64) -> PathBuf {
    dir.join(format!(
        "estop_latch.clear_tmp.{}.{sequence}",
        std::process::id()
    ))
}

#[test]
fn temporary_name_collision_uses_next_sequence_without_overwriting_existing_file() {
    let dir = TempDir::new().unwrap();
    let handle = DirHandle::open(dir.path()).unwrap();
    let existing = temporary_candidate(dir.path(), 0);
    std::fs::write(&existing, "保留已有临时内容").unwrap();
    let sequence = AtomicU64::new(0);
    let (temporary, file) =
        TemporaryLatch::create_with_sequence(&handle, OsStr::new("estop_latch"), &sequence)
            .unwrap();
    let allocated = dir.path().join(&temporary.name);
    assert_eq!(allocated, temporary_candidate(dir.path(), 1));
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
    let handle = DirHandle::open(dir.path()).unwrap();
    for sequence in 0..64 {
        std::fs::write(
            temporary_candidate(dir.path(), sequence),
            sequence.to_le_bytes(),
        )
        .unwrap();
    }
    let sequence = AtomicU64::new(0);
    let error = TemporaryLatch::create_with_sequence(&handle, OsStr::new("estop_latch"), &sequence)
        .err()
        .expect("64 次冲突必须拒绝");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(sequence.load(Ordering::Relaxed), 64);
    assert!(!temporary_candidate(dir.path(), 64).exists());
    for sequence in 0..64 {
        assert_eq!(
            std::fs::read(temporary_candidate(dir.path(), sequence)).unwrap(),
            sequence.to_le_bytes()
        );
    }
}

#[test]
fn temporary_sequence_exhaustion_does_not_wrap_or_allocate() {
    let dir = TempDir::new().unwrap();
    let handle = DirHandle::open(dir.path()).unwrap();
    let sequence = AtomicU64::new(u64::MAX);
    let error = TemporaryLatch::create_with_sequence(&handle, OsStr::new("estop_latch"), &sequence)
        .err()
        .expect("耗尽必须拒绝");
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert_eq!(sequence.load(Ordering::Relaxed), u64::MAX);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn temporary_non_collision_io_error_is_not_retried() {
    let dir = TempDir::new().unwrap();
    let handle = DirHandle::open(dir.path()).unwrap();
    set_mode(dir.path(), 0o500);
    let sequence = AtomicU64::new(0);
    let error = TemporaryLatch::create_with_sequence(&handle, OsStr::new("estop_latch"), &sequence)
        .err()
        .expect("不可写目录必须拒绝");
    set_mode(dir.path(), 0o700);
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(sequence.load(Ordering::Relaxed), 1);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

const PREPARE_STAGES: [ClearTxnStage; 15] = [
    ClearTxnStage::CreateMarker,
    ClearTxnStage::SampleMarkerIdentity,
    ClearTxnStage::WriteMarker,
    ClearTxnStage::SyncMarkerFile,
    ClearTxnStage::SyncMarkerDirectory,
    ClearTxnStage::CreateBackup,
    ClearTxnStage::WriteBackup,
    ClearTxnStage::SyncBackupFile,
    ClearTxnStage::RenameBackup,
    ClearTxnStage::SyncBackupDirectory,
    ClearTxnStage::CreatePrimary,
    ClearTxnStage::WritePrimary,
    ClearTxnStage::SyncPrimaryFile,
    ClearTxnStage::RenamePrimary,
    ClearTxnStage::SyncPrimaryDirectory,
];

#[test]
fn prepare_interruption_before_marker_durability_never_writes_zero() {
    for stage in [
        ClearTxnStage::CreateMarker,
        ClearTxnStage::SampleMarkerIdentity,
        ClearTxnStage::WriteMarker,
        ClearTxnStage::SyncMarkerFile,
        ClearTxnStage::SyncMarkerDirectory,
    ] {
        let (_dir, mut store) = fixture(true);
        fail_after(&mut store, stage);
        let error = store.prepare_clear(1, 2, true, &mut NoGuard).unwrap_err();
        assert_eq!(error.error.stage, stage);
        assert_eq!(error.error.phase, StepPhase::After);
        match (stage, &error.token) {
            (ClearTxnStage::SyncMarkerDirectory, PrepareFailureToken::Durable(_)) => {}
            (ClearTxnStage::CreateMarker, PrepareFailureToken::Created(created)) => {
                assert!(created.identity().is_none(), "采样步骤之前身份未采样");
            }
            (_, PrepareFailureToken::Created(created))
                if stage != ClearTxnStage::SyncMarkerDirectory =>
            {
                assert!(created.identity().is_some(), "{stage:?} 之后身份已采样");
            }
            (_, token) => panic!("{stage:?} 的后置失败返还了错误凭证 {token:?}"),
        }
        assert_pair(&store, true);
        assert!(store.marker_path.exists());
        drop(error);
        let mut reopened = open_store(&store.latch_path);
        match stage {
            // 写入之前中断：marker 为空文件，重启必须 fail-closed 而不是自愈。
            ClearTxnStage::CreateMarker | ClearTxnStage::SampleMarkerIdentity => assert_eq!(
                reopened.recover_and_load().unwrap_err().stage,
                ClearTxnStage::ValidateMarker
            ),
            _ => assert!(reopened.recover_and_load().unwrap()),
        }
    }
}

#[test]
fn before_interrupt_at_every_prepare_stage_returns_ownership_of_completed_operations() {
    for stage in PREPARE_STAGES {
        let (_dir, mut store) = fixture(true);
        let error = store
            .prepare_clear(3, 4, true, &mut FailBefore(stage))
            .unwrap_err();
        assert_eq!(error.error.stage, stage);
        assert_eq!(error.error.phase, StepPhase::Before);
        assert_eq!(
            error.error.guard_interrupt(),
            Some(InterruptReason::TestBarrier)
        );
        assert_ne!(error.error.source.kind(), io::ErrorKind::Interrupted);
        match error.token {
            PrepareFailureToken::NotOwned => {
                assert_eq!(stage, ClearTxnStage::CreateMarker);
                assert!(!store.marker_path.exists());
                assert_pair(&store, true);
            }
            PrepareFailureToken::Created(created) => {
                assert!(matches!(
                    stage,
                    ClearTxnStage::SampleMarkerIdentity
                        | ClearTxnStage::WriteMarker
                        | ClearTxnStage::SyncMarkerFile
                        | ClearTxnStage::SyncMarkerDirectory
                ));
                // 采样步骤之前中断：身份未采样，由活 fd 承担；之后中断：已采样。
                assert_eq!(
                    created.identity().is_some(),
                    stage != ClearTxnStage::SampleMarkerIdentity
                );
                assert_pair(&store, true);
                assert!(store.marker_path.exists());
                store.discard_created(created, &mut NoGuard).unwrap();
                assert!(!store.marker_path.exists());
                assert_pair(&store, true);
            }
            PrepareFailureToken::Durable(durable) => {
                assert!(stage >= ClearTxnStage::CreateBackup, "{stage:?}");
                assert!(store.marker_path.exists());
                let repaired = store
                    .repair(durable, RepairTarget::Latched, &mut NoGuard)
                    .unwrap();
                assert_pair(&store, true);
                store.remove(repaired, &mut NoGuard).unwrap();
                assert!(!store.marker_path.exists());
            }
        }
        assert!(open_store(&store.latch_path).recover_and_load().unwrap());
    }
}

#[test]
fn after_failure_at_every_prepare_stage_returns_advanced_credential() {
    for stage in PREPARE_STAGES {
        let (_dir, mut store) = fixture(true);
        fail_after(&mut store, stage);
        let error = store.prepare_clear(5, 6, true, &mut NoGuard).unwrap_err();
        store.after_step = None;
        assert_eq!(error.error.stage, stage);
        assert_eq!(error.error.phase, StepPhase::After);
        match error.token {
            PrepareFailureToken::NotOwned => panic!("{stage:?} 操作已成功，不得返回 NotOwned"),
            PrepareFailureToken::Created(created) => {
                assert!(stage < ClearTxnStage::SyncMarkerDirectory, "{stage:?}");
                assert_pair(&store, true);
                store.discard_created(created, &mut NoGuard).unwrap();
                assert!(!store.marker_path.exists());
            }
            PrepareFailureToken::Durable(durable) => {
                assert!(stage >= ClearTxnStage::SyncMarkerDirectory, "{stage:?}");
                let repaired = store
                    .repair(durable, RepairTarget::Latched, &mut NoGuard)
                    .unwrap();
                store.remove(repaired, &mut NoGuard).unwrap();
            }
        }
        assert_pair(&store, true);
        assert!(!store.marker_path.exists());
    }
}

#[test]
fn created_marker_discard_accepts_empty_half_written_and_full_contents() {
    for (stage, truncate_to) in [
        (ClearTxnStage::CreateMarker, Some(0)),
        (ClearTxnStage::WriteMarker, Some(50)),
        (ClearTxnStage::WriteMarker, None),
        (ClearTxnStage::SyncMarkerFile, None),
    ] {
        let (_dir, mut store) = fixture(true);
        let created = created_marker(&mut store, stage);
        let encoded = created.record().encode();
        if let Some(len) = truncate_to {
            std::fs::write(&store.marker_path, &encoded[..len]).unwrap();
        }
        let steps = record_steps(&mut store);
        store.discard_created(created, &mut NoGuard).unwrap();
        assert_eq!(
            &*steps.lock().unwrap(),
            &[
                ClearTxnStage::ReadMarker,
                ClearTxnStage::ValidateMarker,
                ClearTxnStage::RemoveMarker,
                ClearTxnStage::SyncRemovalDirectory,
            ]
        );
        assert!(!store.marker_path.exists());
        assert_pair(&store, true);
    }
}

#[test]
fn created_marker_discard_rejects_trailing_nonprefix_inode_swap_hard_link_and_unlinked() {
    for case in 0..5 {
        let (_dir, mut store) = fixture(true);
        let created = created_marker(&mut store, ClearTxnStage::WriteMarker);
        let full = created.record().encode();
        let expected_stage = match case {
            0 => {
                let mut file = OpenOptions::new()
                    .append(true)
                    .open(&store.marker_path)
                    .unwrap();
                file.write_all(&[0]).unwrap();
                ClearTxnStage::ReadMarker
            }
            1 => {
                let mut wrong = full;
                wrong[3] ^= 1;
                std::fs::write(&store.marker_path, &wrong[..40]).unwrap();
                ClearTxnStage::ValidateMarker
            }
            2 => {
                // 本次 inode 仍以另一名字存在（nlink=1），但标记路径已指向别的文件。
                let moved = store.marker_path.with_extension("moved");
                std::fs::rename(&store.marker_path, &moved).unwrap();
                std::fs::write(&store.marker_path, full).unwrap();
                ClearTxnStage::ValidateMarker
            }
            3 => {
                std::fs::hard_link(&store.marker_path, store.marker_path.with_extension("link"))
                    .unwrap();
                ClearTxnStage::ReadMarker
            }
            4 => {
                std::fs::remove_file(&store.marker_path).unwrap();
                ClearTxnStage::ReadMarker
            }
            _ => unreachable!(),
        };
        let before: Vec<_> = std::fs::read_dir(store.latch_path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let error = store.discard_created(created, &mut NoGuard).unwrap_err();
        assert_eq!(error.error.stage, expected_stage, "case {case}");
        assert_eq!(error.error.phase, StepPhase::Operation, "case {case}");
        assert_eq!(error.error.source.kind(), io::ErrorKind::InvalidData);
        assert!(
            matches!(error.token, RemoveFailureToken::Lost),
            "case {case}"
        );
        let after: Vec<_> = std::fs::read_dir(store.latch_path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(before, after, "case {case}: 确认不符时不得删除任何文件");
        assert_pair(&store, true);
    }
}

#[test]
fn created_marker_discard_temporary_errors_and_before_interrupts_keep_ownership() {
    let (dir, mut store) = fixture(true);
    let created = created_marker(&mut store, ClearTxnStage::WriteMarker);
    let error = store
        .discard_created(created, &mut FailBefore(ClearTxnStage::RemoveMarker))
        .unwrap_err();
    assert_eq!(error.error.phase, StepPhase::Before);
    let RemoveFailureToken::Retained(created) = error.token else {
        panic!("unlink 前中断必须返还凭证");
    };
    assert!(store.marker_path.exists());

    set_mode(dir.path(), 0o500);
    let error = store.discard_created(created, &mut NoGuard).unwrap_err();
    set_mode(dir.path(), 0o700);
    assert_eq!(error.error.stage, ClearTxnStage::RemoveMarker);
    assert_eq!(error.error.phase, StepPhase::Operation);
    assert_eq!(error.error.source.kind(), io::ErrorKind::PermissionDenied);
    let RemoveFailureToken::Retained(created) = error.token else {
        panic!("临时 unlink 失败必须返还凭证");
    };
    assert!(store.marker_path.exists());

    let error = store
        .discard_created(
            created,
            &mut FailBefore(ClearTxnStage::SyncRemovalDirectory),
        )
        .unwrap_err();
    assert_eq!(error.error.stage, ClearTxnStage::SyncRemovalDirectory);
    let RemoveFailureToken::Unsynced(unsynced) = error.token else {
        panic!("unlink 之后的中断必须返还 Unsynced");
    };
    assert!(!store.marker_path.exists());
    assert_eq!(unsynced.directory_identity(), store.directory_identity());
    store.resync_removal(unsynced, &mut NoGuard).unwrap();
    assert_pair(&store, true);
}

#[test]
fn remove_before_interrupts_retain_and_post_unlink_failures_return_unsynced_without_re_unlink() {
    let (_dir, mut store) = fixture(true);
    let mut prepared = prepare(&mut store);
    for stage in [
        ClearTxnStage::ReadMarker,
        ClearTxnStage::ValidateMarker,
        ClearTxnStage::RemoveMarker,
    ] {
        let error = store.remove(prepared, &mut FailBefore(stage)).unwrap_err();
        assert_eq!(error.error.stage, stage);
        assert_eq!(error.error.phase, StepPhase::Before);
        let RemoveFailureToken::Retained(returned) = error.token else {
            panic!("{stage:?} 之前中断必须返还凭证");
        };
        prepared = returned;
        assert!(store.marker_path.exists());
    }
    fail_after(&mut store, ClearTxnStage::RemoveMarker);
    let error = store.remove(prepared, &mut NoGuard).unwrap_err();
    assert_eq!(error.error.stage, ClearTxnStage::RemoveMarker);
    assert_eq!(error.error.phase, StepPhase::After);
    let RemoveFailureToken::Unsynced(unsynced) = error.token else {
        panic!("unlink 已发生必须返还 Unsynced");
    };
    assert!(!store.marker_path.exists());

    // 之后所有重同步尝试都在同一记录器下进行：失败两次（After 相、Before 相）后成功，
    // 全程不得再出现 RemoveMarker 步骤。
    let steps = record_and_fail_after(&mut store, ClearTxnStage::SyncRemovalDirectory, 1);
    let error = store.resync_removal(unsynced, &mut NoGuard).unwrap_err();
    assert_eq!(error.error.stage, ClearTxnStage::SyncRemovalDirectory);
    assert_eq!(error.error.phase, StepPhase::After);
    let RemoveFailureToken::Retained(unsynced) = error.token else {
        panic!("重同步失败必须保留 Unsynced");
    };
    let error = store
        .resync_removal(
            unsynced,
            &mut FailBefore(ClearTxnStage::SyncRemovalDirectory),
        )
        .unwrap_err();
    assert_eq!(error.error.phase, StepPhase::Before);
    let RemoveFailureToken::Retained(unsynced) = error.token else {
        panic!("重同步中断必须保留 Unsynced");
    };
    store.resync_removal(unsynced, &mut NoGuard).unwrap();
    assert_eq!(
        &*steps.lock().unwrap(),
        &[
            ClearTxnStage::SyncRemovalDirectory,
            ClearTxnStage::SyncRemovalDirectory,
        ]
    );
    assert_pair(&store, false);
    assert!(!open_store(&store.latch_path).recover_and_load().unwrap());
}

#[test]
fn unlink_failure_retains_credential_and_the_same_credential_retries_successfully() {
    let (dir, mut store) = fixture(true);
    let prepared = prepare(&mut store);
    set_mode(dir.path(), 0o500);
    let error = store.remove(prepared, &mut NoGuard).unwrap_err();
    set_mode(dir.path(), 0o700);
    assert_eq!(error.error.stage, ClearTxnStage::RemoveMarker);
    assert_eq!(error.error.phase, StepPhase::Operation);
    assert_eq!(error.error.source.kind(), io::ErrorKind::PermissionDenied);
    let RemoveFailureToken::Retained(prepared) = error.token else {
        panic!("unlink 失败必须返还凭证");
    };
    assert!(store.marker_path.exists());
    store.remove(prepared, &mut NoGuard).unwrap();
    assert!(!store.marker_path.exists());
}

#[test]
fn repair_before_interrupts_and_temporary_read_errors_keep_the_credential() {
    let (_dir, mut store) = fixture(true);
    let mut prepared = prepare(&mut store);
    for stage in [
        ClearTxnStage::ReadMarker,
        ClearTxnStage::ValidateMarker,
        ClearTxnStage::CreatePrimary,
        ClearTxnStage::RenameBackup,
    ] {
        let error = store
            .repair(prepared, RepairTarget::Latched, &mut FailBefore(stage))
            .unwrap_err();
        assert_eq!(error.error.stage, stage);
        assert_eq!(error.error.phase, StepPhase::Before);
        prepared = error.token.expect("守门中断不得失权");
        assert!(store.marker_path.exists());
    }
    set_mode(&store.marker_path, 0o000);
    let error = store
        .repair(prepared, RepairTarget::Latched, &mut NoGuard)
        .unwrap_err();
    set_mode(&store.marker_path, 0o600);
    assert_eq!(error.error.stage, ClearTxnStage::ReadMarker);
    assert_eq!(error.error.source.kind(), io::ErrorKind::PermissionDenied);
    let prepared = error.token.expect("临时读取错误不得失权");
    let repaired = store
        .repair(prepared, RepairTarget::Latched, &mut NoGuard)
        .unwrap();
    assert_pair(&store, true);
    store.remove(repaired, &mut NoGuard).unwrap();
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
        let prepared = store.prepare_clear(11, 12, original, &mut NoGuard).unwrap();
        let repaired = store.repair(prepared, target, &mut NoGuard).unwrap();
        assert_pair(&store, expected);
        assert!(store.marker_path.exists());
        store.remove(repaired, &mut NoGuard).unwrap();
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
    store.after_step = None;
    store.remove(repaired, &mut NoGuard).unwrap();
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
fn persist_latched_writes_primary_then_backup_strictly() {
    use ClearTxnStage as Stage;
    let (_dir, mut store) = fixture(false);
    let steps = record_steps(&mut store);
    store.persist_latched(&mut NoGuard).unwrap();
    assert_pair(&store, true);
    assert_eq!(
        &*steps.lock().unwrap(),
        &[
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
        ]
    );
    let error = store
        .persist_latched(&mut FailBefore(Stage::CreateBackup))
        .unwrap_err();
    assert_eq!(error.stage, Stage::CreateBackup);
    assert_eq!(error.phase, StepPhase::Before);
    assert_pair(&store, true);
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
fn full_record_mismatch_loses_old_prepared_pending_and_repaired_credentials() {
    for field in 0..3 {
        for operation in 0..4 {
            let (_dir, mut store) = fixture(true);
            let mut prepared = Some(store.prepare_clear(11, 12, true, &mut NoGuard).unwrap());
            let record = *prepared.as_ref().unwrap().record();
            let pending = store.inspect_pending().unwrap().unwrap();
            let repaired = match operation {
                3 => Some(
                    store
                        .repair(
                            prepared.take().unwrap(),
                            RepairTarget::Latched,
                            &mut NoGuard,
                        )
                        .unwrap(),
                ),
                _ => None,
            };
            replace_record(&store, record, field);
            write_pair(&store, true);
            let replacement = std::fs::read(&store.marker_path).unwrap();
            let error = match operation {
                0 => {
                    let error = store
                        .repair(prepared.unwrap(), RepairTarget::Original, &mut NoGuard)
                        .unwrap_err();
                    assert!(error.token.is_none());
                    error.error
                }
                1 => {
                    let error = store.remove(prepared.unwrap(), &mut NoGuard).unwrap_err();
                    assert!(matches!(error.token, RemoveFailureToken::Lost));
                    error.error
                }
                2 => store
                    .repair_pending(pending, RepairTarget::Original)
                    .unwrap_err(),
                3 => {
                    let error = store.remove(repaired.unwrap(), &mut NoGuard).unwrap_err();
                    assert!(matches!(error.token, RemoveFailureToken::Lost));
                    error.error
                }
                _ => unreachable!(),
            };
            assert_eq!(error.stage, ClearTxnStage::ValidateMarker);
            assert_eq!(error.phase, StepPhase::Operation);
            assert_eq!(error.source.kind(), io::ErrorKind::InvalidData);
            assert_pair(&store, true);
            assert_eq!(std::fs::read(&store.marker_path).unwrap(), replacement);
        }
    }
}

#[test]
fn same_record_but_different_inode_loses_credentials() {
    for operation in 0..2 {
        let (_dir, mut store) = fixture(true);
        let prepared = prepare(&mut store);
        let bytes = std::fs::read(&store.marker_path).unwrap();
        // 覆盖范围：只证明 (dev, ino) 数值确实不同时凭证被拒绝。文件系统在 unlink 后
        // 会立即复用 inode 号（round4 真实失败记录），因此把原 inode 改名保活后再在
        // 原路径建立同字节新文件；同字节且 inode 号复用的替换不在本模块识别范围内。
        let kept_alive = store.marker_path.with_extension("kept");
        std::fs::rename(&store.marker_path, &kept_alive).unwrap();
        std::fs::write(&store.marker_path, &bytes).unwrap();
        let error = match operation {
            0 => {
                let error = store
                    .repair(prepared, RepairTarget::Latched, &mut NoGuard)
                    .unwrap_err();
                assert!(error.token.is_none());
                error.error
            }
            1 => {
                let error = store.remove(prepared, &mut NoGuard).unwrap_err();
                assert!(matches!(error.token, RemoveFailureToken::Lost));
                error.error
            }
            _ => unreachable!(),
        };
        assert_eq!(error.stage, ClearTxnStage::ValidateMarker);
        assert_eq!(error.source.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&store.marker_path).unwrap(), bytes);
        assert_eq!(std::fs::read(&kept_alive).unwrap(), bytes);
        assert_pair(&store, false);
    }
}

#[test]
fn corrupted_marker_rejects_prepared_repair_before_any_latch_write() {
    let (_dir, mut store) = fixture(true);
    let prepared = prepare(&mut store);
    let mut bytes = std::fs::read(&store.marker_path).unwrap();
    bytes[100] ^= 1;
    std::fs::write(&store.marker_path, &bytes).unwrap();
    let error = store
        .repair(prepared, RepairTarget::Latched, &mut NoGuard)
        .unwrap_err();
    assert_eq!(error.error.stage, ClearTxnStage::ValidateMarker);
    assert!(error.token.is_none());
    assert_pair(&store, false);
    assert_eq!(std::fs::read(&store.marker_path).unwrap(), bytes);
}

#[test]
fn repair_failure_at_every_latch_step_keeps_marker_and_credential_for_retry() {
    let stages = LatchCopy::Backup
        .stages()
        .into_iter()
        .chain(LatchCopy::Primary.stages());
    for stage in stages {
        for retry_in_process in [false, true] {
            let (_dir, mut store) = fixture(true);
            let prepared = prepare(&mut store);
            fail_after(&mut store, stage);
            let error = store
                .repair(prepared, RepairTarget::Latched, &mut NoGuard)
                .unwrap_err();
            assert_eq!(error.error.stage, stage);
            assert_eq!(error.error.phase, StepPhase::After);
            assert!(store.marker_path.exists());
            store.after_step = None;
            match error.token {
                Some(prepared) if retry_in_process => {
                    let repaired = store
                        .repair(prepared, RepairTarget::Latched, &mut NoGuard)
                        .unwrap();
                    assert_pair(&store, true);
                    store.remove(repaired, &mut NoGuard).unwrap();
                    assert!(!store.marker_path.exists());
                }
                Some(_) => {
                    let mut reopened = open_store(&store.latch_path);
                    assert!(reopened.recover_and_load().unwrap());
                    assert_pair(&reopened, true);
                    assert!(!reopened.marker_path.exists());
                }
                None => panic!("{stage:?}: latch 部分写入不得失权"),
            }
        }
    }
}

#[test]
fn real_backup_rename_failure_preserves_marker_and_primary() {
    let (_dir, mut store) = fixture(true);
    std::fs::remove_file(backup_path(&store.latch_path)).unwrap();
    std::fs::create_dir(backup_path(&store.latch_path)).unwrap();
    let error = store.prepare_clear(1, 2, true, &mut NoGuard).unwrap_err();
    assert_eq!(error.error.stage, ClearTxnStage::RenameBackup);
    assert_eq!(error.error.phase, StepPhase::Operation);
    assert!(matches!(error.token, PrepareFailureToken::Durable(_)));
    assert_eq!(std::fs::read(&store.latch_path).unwrap(), b"1\n");
    assert!(store.marker_path.exists());
    assert!(store.inspect_pending().unwrap().is_some());
}

#[test]
fn parent_rename_and_path_replacement_still_operate_on_the_original_directory() {
    let dir = TempDir::new().unwrap();
    let original = dir.path().join("original");
    let moved = dir.path().join("moved");
    std::fs::create_dir(&original).unwrap();
    let mut store = open_store(&original.join("estop_latch"));
    write_pair(&store, true);
    let prepared = prepare(&mut store);
    std::fs::rename(&original, &moved).unwrap();
    // 原路径被一个内容不同的新目录替换：旧目录 fd 上的操作不得触碰它。
    std::fs::create_dir(&original).unwrap();
    std::fs::write(original.join("estop_latch"), b"1\n").unwrap();
    std::fs::write(original.join("estop_latch.bak"), b"1\n").unwrap();
    std::fs::write(original.join("estop_latch.clear_pending"), b"fake").unwrap();

    let repaired = store
        .repair(prepared, RepairTarget::Latched, &mut NoGuard)
        .unwrap();
    store.remove(repaired, &mut NoGuard).unwrap();
    store.persist_latched(&mut NoGuard).unwrap();
    assert!(!moved.join("estop_latch.clear_pending").exists());
    assert_eq!(std::fs::read(moved.join("estop_latch")).unwrap(), b"1\n");
    assert_eq!(
        std::fs::read(moved.join("estop_latch.bak")).unwrap(),
        b"1\n"
    );
    assert_eq!(
        std::fs::read(original.join("estop_latch.clear_pending")).unwrap(),
        b"fake"
    );
    assert!(std::fs::read_dir(&moved).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("clear_tmp")
    }));
    assert_eq!(std::fs::read_dir(&original).unwrap().count(), 3);
    std::fs::remove_dir_all(&original).unwrap();
    std::fs::rename(&moved, &original).unwrap();
    assert!(
        open_store(&original.join("estop_latch"))
            .recover_and_load()
            .unwrap()
    );
}

#[test]
fn created_then_unsynced_then_resync_then_repair_continues_to_the_target() {
    use ClearTxnStage as Stage;
    for (original, target, expected) in [
        (true, RepairTarget::Latched, true),
        (false, RepairTarget::Latched, true),
        (false, RepairTarget::Original, false),
    ] {
        let (_dir, mut store) = fixture(original);
        fail_after(&mut store, Stage::SyncMarkerFile);
        let error = store
            .prepare_clear(9, 10, original, &mut NoGuard)
            .unwrap_err();
        let PrepareFailureToken::Created(created) = error.token else {
            panic!("marker 未 durable 必须返还 Created");
        };
        fail_after(&mut store, Stage::SyncRemovalDirectory);
        let error = store.discard_created(created, &mut NoGuard).unwrap_err();
        let RemoveFailureToken::Unsynced(unsynced) = error.token else {
            panic!("目录同步失败必须返还 Unsynced");
        };
        assert!(!store.marker_path.exists());
        store.after_step = None;
        store.resync_removal(unsynced, &mut NoGuard).unwrap();
        // 目录同步只结束删除义务：此刻 pair 仍是事务开始前的值，尚未到达目标。
        assert_pair(&store, original);
        let steps = record_steps(&mut store);
        match target {
            RepairTarget::Latched => {
                // 无 marker 事务的 Latched 修复 = 真实写 1/1，严格 primary → backup。
                store.persist_latched(&mut NoGuard).unwrap();
                assert_eq!(
                    &*steps.lock().unwrap(),
                    &[
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
                    ]
                );
            }
            RepairTarget::Original => {
                // OriginalUnlatched：只核对 pair 仍为 0/0，不制造额外锁存，无任何 latch 步骤。
                assert!(!original);
                assert!(steps.lock().unwrap().is_empty());
            }
        }
        store.after_step = None;
        assert_pair(&store, expected);
        assert_eq!(
            open_store(&store.latch_path).recover_and_load().unwrap(),
            expected
        );
    }
}

#[test]
fn created_marker_survives_first_fstat_failure_and_transient_errors_then_discards_strictly() {
    let (_dir, mut store) = fixture(true);
    // openat 已真实成功，紧随其后的身份采样系统调用失败：所有权必须已经建立。
    store.fail_operation = Some((ClearTxnStage::SampleMarkerIdentity, 1));
    let error = store.prepare_clear(13, 14, true, &mut NoGuard).unwrap_err();
    assert_eq!(error.error.stage, ClearTxnStage::SampleMarkerIdentity);
    assert_eq!(error.error.phase, StepPhase::Operation);
    assert_eq!(error.error.source.raw_os_error(), Some(libc::EIO));
    let PrepareFailureToken::Created(created) = error.token else {
        panic!("openat 成功后的采样失败必须返还 Created");
    };
    assert!(created.identity().is_none(), "采样失败不得填伪造身份");
    assert_eq!(created.record().txn_id(), 13);
    // 没有隐式 unlink：空 marker 仍在，pair 未动。
    assert_eq!(std::fs::metadata(&store.marker_path).unwrap().len(), 0);
    assert_pair(&store, true);
    let entries_before = std::fs::read_dir(store.latch_path.parent().unwrap())
        .unwrap()
        .count();

    // 同一凭证再遇 metadata / 读取临时错误：仍返还，不删除、不失权。
    let mut created = created;
    for stage in [ClearTxnStage::ReadMarker, ClearTxnStage::ValidateMarker] {
        store.fail_operation = Some((stage, 1));
        let error = store.discard_created(created, &mut NoGuard).unwrap_err();
        assert_eq!(error.error.stage, stage);
        assert_eq!(error.error.phase, StepPhase::Operation);
        assert_eq!(error.error.source.raw_os_error(), Some(libc::EIO));
        let RemoveFailureToken::Retained(returned) = error.token else {
            panic!("{stage:?} 临时错误必须返还凭证");
        };
        created = returned;
        assert!(store.marker_path.exists());
        assert_eq!(
            std::fs::read_dir(store.latch_path.parent().unwrap())
                .unwrap()
                .count(),
            entries_before
        );
    }
    assert_eq!(
        store.fail_operation,
        Some((ClearTxnStage::ValidateMarker, 0))
    );

    // 最终经 strict 删除成功：真实 fstat + 读取 + unlink + 目录同步。
    let steps = record_steps(&mut store);
    store.discard_created(created, &mut NoGuard).unwrap();
    assert_eq!(
        &*steps.lock().unwrap(),
        &[
            ClearTxnStage::ReadMarker,
            ClearTxnStage::ValidateMarker,
            ClearTxnStage::RemoveMarker,
            ClearTxnStage::SyncRemovalDirectory,
        ]
    );
    assert!(!store.marker_path.exists());
    assert_pair(&store, true);
    assert!(open_store(&store.latch_path).recover_and_load().unwrap());
}

#[test]
fn credentials_are_refused_by_another_store_without_file_changes() {
    let (_dir, mut store) = fixture(true);
    let other_dir = TempDir::new().unwrap();
    let mut other = open_store(&other_dir.path().join("estop_latch"));
    write_pair(&other, false);
    let mut same_dir_other_target = open_store(&store.latch_path.with_file_name("other_latch"));
    write_pair(&same_dir_other_target, false);
    let prepared = prepare(&mut store);
    let marker_bytes = std::fs::read(&store.marker_path).unwrap();

    let error = other
        .repair(prepared, RepairTarget::Latched, &mut NoGuard)
        .unwrap_err();
    assert_eq!(error.error.stage, ClearTxnStage::ValidateCredential);
    assert_eq!(error.error.source.kind(), io::ErrorKind::InvalidInput);
    let prepared = error.token.expect("跨 store 拒绝不得失权");
    let error = same_dir_other_target
        .remove(prepared, &mut NoGuard)
        .unwrap_err();
    assert_eq!(error.error.stage, ClearTxnStage::ValidateCredential);
    let RemoveFailureToken::Retained(prepared) = error.token else {
        panic!("跨目标拒绝必须返还凭证");
    };
    assert_eq!(std::fs::read(&store.marker_path).unwrap(), marker_bytes);
    assert_pair(&other, false);
    assert_pair(&same_dir_other_target, false);

    let created = created_marker(&mut other, ClearTxnStage::WriteMarker);
    let error = store.discard_created(created, &mut NoGuard).unwrap_err();
    assert_eq!(error.error.stage, ClearTxnStage::ValidateCredential);
    let RemoveFailureToken::Retained(created) = error.token else {
        panic!("跨 store 丢弃必须返还凭证");
    };
    assert!(other.marker_path.exists());
    other.discard_created(created, &mut NoGuard).unwrap();

    fail_after(&mut store, ClearTxnStage::SyncRemovalDirectory);
    let error = store.remove(prepared, &mut NoGuard).unwrap_err();
    store.after_step = None;
    let RemoveFailureToken::Unsynced(unsynced) = error.token else {
        panic!("目录同步失败必须返还 Unsynced");
    };
    let error = other.resync_removal(unsynced, &mut NoGuard).unwrap_err();
    assert_eq!(error.error.stage, ClearTxnStage::ValidateCredential);
    let RemoveFailureToken::Retained(unsynced) = error.token else {
        panic!("跨 store 重同步必须返还凭证");
    };
    store.resync_removal(unsynced, &mut NoGuard).unwrap();
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
        Stage::SampleMarkerIdentity,
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
            Stage::CreateMarker | Stage::SampleMarkerIdentity => {
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
    let prepared = store.prepare_clear(1, 2, true, &mut NoGuard).unwrap();
    store.remove(prepared, &mut NoGuard).unwrap();
    panic!("子进程未命中指定文件步骤");
}

// ── W0.1 只读双份核验 ──────────────────────────────────────────────────────

/// 记录每次守门调用的阶段；核 `inspect_pair` 一个步骤内守门恰一次。
struct CountingGuard(Vec<ClearTxnStage>);

impl StepGuard for CountingGuard {
    fn before(&mut self, stage: ClearTxnStage) -> Result<(), GuardInterrupt> {
        self.0.push(stage);
        Ok(())
    }
}

/// 目录条目快照：名字、普通文件内容与是否符号链接。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DirEntrySnapshot {
    name: std::ffi::OsString,
    bytes: Vec<u8>,
    symlink: bool,
}

fn dir_snapshot(dir: &Path) -> Vec<DirEntrySnapshot> {
    let mut rows: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = std::fs::symlink_metadata(entry.path()).unwrap();
            let bytes = if metadata.is_file() {
                std::fs::read(entry.path()).unwrap()
            } else {
                Vec::new()
            };
            DirEntrySnapshot {
                name: entry.file_name(),
                bytes,
                symlink: metadata.file_type().is_symlink(),
            }
        })
        .collect();
    rows.sort();
    rows
}

/// 值 / 缺失 / 坏值矩阵：每种组合返回真实状态，且目录条目与所有现存内容（含标记）
/// 逐字节不变；守门恰一次、仅 `InspectPair` 一个步骤。
#[test]
fn inspect_pair_reports_values_missing_and_corrupt_without_side_effects() {
    use LatchCopyState as S;
    /// (primary 内容, backup 内容, 期望 primary 状态, 期望 backup 状态)；`None` = 文件不存在。
    type Case = (Option<&'static [u8]>, Option<&'static [u8]>, S, S);
    let cases: [Case; 12] = [
        (Some(b"0\n"), Some(b"0\n"), S::Value(false), S::Value(false)),
        (Some(b"1\n"), Some(b"1\n"), S::Value(true), S::Value(true)),
        (Some(b"1\n"), Some(b"0\n"), S::Value(true), S::Value(false)),
        (Some(b"0\n"), Some(b"1\n"), S::Value(false), S::Value(true)),
        (Some(b"0"), Some(b"1"), S::Value(false), S::Value(true)),
        (None, Some(b"1\n"), S::Missing, S::Value(true)),
        (Some(b"0\n"), None, S::Value(false), S::Missing),
        (None, None, S::Missing, S::Missing),
        (Some(b"garbage"), Some(b"0\n"), S::Corrupt, S::Value(false)),
        (Some(b""), Some(b"2\n"), S::Corrupt, S::Corrupt),
        (Some(b"1\n\n"), None, S::Corrupt, S::Missing),
        (Some(b"01"), Some(b"1\r\n"), S::Corrupt, S::Corrupt),
    ];
    for (primary, backup, expect_primary, expect_backup) in cases {
        let dir = TempDir::new().unwrap();
        let latch = dir.path().join("estop_latch");
        if let Some(bytes) = primary {
            std::fs::write(&latch, bytes).unwrap();
        }
        if let Some(bytes) = backup {
            std::fs::write(backup_path(&latch), bytes).unwrap();
        }
        // 标记只允许存在、不得被读改：用坏字节证明它既不触发恢复也不被动。
        std::fs::write(
            dir.path().join("estop_latch.clear_pending"),
            b"stale-marker",
        )
        .unwrap();
        let before = dir_snapshot(dir.path());
        let mut store = open_store(&latch);
        let steps = record_steps(&mut store);
        let mut guard = CountingGuard(Vec::new());
        let view = store.inspect_pair(&mut guard).unwrap();
        assert_eq!(view.primary, expect_primary, "{primary:?}/{backup:?}");
        assert_eq!(view.backup, expect_backup, "{primary:?}/{backup:?}");
        assert_eq!(view.directory_identity, store.directory_identity());
        assert_eq!(guard.0, vec![ClearTxnStage::InspectPair], "守门恰一次");
        assert_eq!(&*steps.lock().unwrap(), &[ClearTxnStage::InspectPair]);
        assert_eq!(dir_snapshot(dir.path()), before, "不得写入 / 自愈 / 改标记");
        store.after_step = None;
    }
}

/// 符号链接 / 硬链接 / 目录：Operation 相真实错误（`ELOOP` / `InvalidInput`），不伪装
/// 成 Corrupt，不返回部分结果，目录不变。
#[test]
fn inspect_pair_rejects_symlink_hardlink_and_directory_objects() {
    // primary 为符号链接（O_NOFOLLOW → ELOOP）。
    let dir = TempDir::new().unwrap();
    let latch = dir.path().join("estop_latch");
    std::fs::write(dir.path().join("real"), b"1\n").unwrap();
    symlink("real", &latch).unwrap();
    std::fs::write(backup_path(&latch), b"1\n").unwrap();
    let before = dir_snapshot(dir.path());
    let mut store = open_store(&latch);
    let error = store.inspect_pair(&mut NoGuard).unwrap_err();
    assert_eq!(error.stage, ClearTxnStage::InspectPair);
    assert_eq!(error.phase, StepPhase::Operation);
    assert_eq!(error.source.raw_os_error(), Some(libc::ELOOP));
    assert_eq!(dir_snapshot(dir.path()), before);

    // backup 与 primary 互为硬链接（nlink == 2）。
    let dir = TempDir::new().unwrap();
    let latch = dir.path().join("estop_latch");
    std::fs::write(&latch, b"0\n").unwrap();
    std::fs::hard_link(&latch, backup_path(&latch)).unwrap();
    let before = dir_snapshot(dir.path());
    let mut store = open_store(&latch);
    let error = store.inspect_pair(&mut NoGuard).unwrap_err();
    assert_eq!(error.stage, ClearTxnStage::InspectPair);
    assert_eq!(error.phase, StepPhase::Operation);
    assert_eq!(error.source.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(dir_snapshot(dir.path()), before);

    // backup 是目录。
    let dir = TempDir::new().unwrap();
    let latch = dir.path().join("estop_latch");
    std::fs::write(&latch, b"0\n").unwrap();
    std::fs::create_dir(backup_path(&latch)).unwrap();
    let mut store = open_store(&latch);
    let error = store.inspect_pair(&mut NoGuard).unwrap_err();
    assert_eq!(error.stage, ClearTxnStage::InspectPair);
    assert_eq!(error.phase, StepPhase::Operation);
    assert_eq!(error.source.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(std::fs::read(&latch).unwrap(), b"0\n");
}

/// 真实系统错误（primary 不可读 `EACCES`）直接传播为 Operation 错误，不吞成 Corrupt，
/// 也不回退 backup；root 下无法构造则跳过。
#[test]
fn inspect_pair_propagates_real_io_errors_instead_of_corrupt() {
    // SAFETY：geteuid 无参数、无副作用、不会失败。
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = TempDir::new().unwrap();
    let latch = dir.path().join("estop_latch");
    std::fs::write(&latch, b"1\n").unwrap();
    std::fs::write(backup_path(&latch), b"1\n").unwrap();
    std::fs::set_permissions(&latch, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut store = open_store(&latch);
    let error = store.inspect_pair(&mut NoGuard).unwrap_err();
    std::fs::set_permissions(&latch, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert_eq!(error.stage, ClearTxnStage::InspectPair);
    assert_eq!(error.phase, StepPhase::Operation);
    assert_eq!(error.source.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(std::fs::read(&latch).unwrap(), b"1\n");
}

/// 父目录被替换后仍读打开时的目录 fd：返回旧目录的值与原身份，不读新目录。
#[test]
fn inspect_pair_reads_the_bound_directory_after_parent_replacement() {
    let dir = TempDir::new().unwrap();
    let parent = dir.path().join("robot");
    std::fs::create_dir(&parent).unwrap();
    let latch = parent.join("estop_latch");
    std::fs::write(&latch, b"0\n").unwrap();
    std::fs::write(backup_path(&latch), b"0\n").unwrap();
    let mut store = open_store(&latch);
    let identity = store.directory_identity();

    std::fs::rename(&parent, dir.path().join("robot.old")).unwrap();
    std::fs::create_dir(&parent).unwrap();
    std::fs::write(&latch, b"1\n").unwrap();
    std::fs::write(backup_path(&latch), b"1\n").unwrap();

    let view = store.inspect_pair(&mut NoGuard).unwrap();
    assert_eq!(view.primary, LatchCopyState::Value(false));
    assert_eq!(view.backup, LatchCopyState::Value(false));
    assert_eq!(view.directory_identity, identity);
    let new_identity = FileIdentity::of(&std::fs::metadata(&parent).unwrap());
    assert_ne!(view.directory_identity, new_identity, "不得读替换目录");
}

/// 三相注入：守门拒绝为 Before 且不进入文件操作；Operation 注入为真实 EIO；After
/// 注入在两份读取成功后才触发。
#[test]
fn inspect_pair_records_real_phase_for_before_operation_and_after() {
    let (dir, mut store) = fixture(false);
    let before = dir_snapshot(dir.path());

    let steps = record_steps(&mut store);
    let error = store
        .inspect_pair(&mut FailBefore(ClearTxnStage::InspectPair))
        .unwrap_err();
    assert_eq!(error.stage, ClearTxnStage::InspectPair);
    assert_eq!(error.phase, StepPhase::Before);
    assert_eq!(error.guard_interrupt(), Some(InterruptReason::TestBarrier));
    assert!(
        steps.lock().unwrap().is_empty(),
        "Before 拒绝不得进入 Operation"
    );

    store.fail_operation = Some((ClearTxnStage::InspectPair, 1));
    let error = store.inspect_pair(&mut NoGuard).unwrap_err();
    assert_eq!(error.stage, ClearTxnStage::InspectPair);
    assert_eq!(error.phase, StepPhase::Operation);
    assert_eq!(error.source.raw_os_error(), Some(libc::EIO));
    assert_eq!(store.fail_operation, Some((ClearTxnStage::InspectPair, 0)));
    assert!(
        steps.lock().unwrap().is_empty(),
        "Operation 失败不得到 After"
    );

    fail_after(&mut store, ClearTxnStage::InspectPair);
    let error = store.inspect_pair(&mut NoGuard).unwrap_err();
    assert_eq!(error.stage, ClearTxnStage::InspectPair);
    assert_eq!(error.phase, StepPhase::After);

    store.after_step = None;
    let view = store.inspect_pair(&mut NoGuard).unwrap();
    assert_eq!(view.primary, LatchCopyState::Value(false));
    assert_eq!(view.backup, LatchCopyState::Value(false));
    assert_eq!(dir_snapshot(dir.path()), before);
}

/// 纯字节解析与旧 loader 接受形态完全一致。
#[test]
fn parse_latch_bytes_matches_legacy_reader() {
    assert_eq!(parse_latch_bytes(b"0"), Some(false));
    assert_eq!(parse_latch_bytes(b"0\n"), Some(false));
    assert_eq!(parse_latch_bytes(b"1"), Some(true));
    assert_eq!(parse_latch_bytes(b"1\n"), Some(true));
    for bad in [&b""[..], b"2", b"01", b"1\n\n", b"1\r\n", b"garb", b" 1"] {
        assert_eq!(parse_latch_bytes(bad), None, "{bad:?}");
    }
}
