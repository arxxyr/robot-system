//! 从公开 API 验证事务恢复与旧 loader 的边界；不依赖模块内的故障注入实现。

use std::fs;
use std::path::{Path, PathBuf};

use robot_s0::clear_estop_transaction::{ClearTxnStage, ClearTxnStore, RepairTarget};
use robot_s0::estop_latch::backup_path;

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let latch = dir.path().join("estop_latch");
    let marker = dir.path().join("estop_latch.clear_pending");
    fs::write(&latch, b"1\n").unwrap();
    fs::write(backup_path(&latch), b"1\n").unwrap();
    (dir, latch, marker)
}

fn open(latch: &Path) -> ClearTxnStore {
    ClearTxnStore::open(latch, "xlerobot_left", "left_arm").unwrap()
}

fn assert_pair(latch: &Path, value: &[u8]) {
    assert_eq!(fs::read(latch).unwrap(), value);
    assert_eq!(fs::read(backup_path(latch)).unwrap(), value);
}

#[test]
fn pending_transaction_recovers_before_backup_can_self_heal_primary() {
    let (_dir, latch, marker) = fixture();
    let prepared = open(&latch).prepare_clear(11, 4, true).unwrap();
    assert_eq!(prepared.record().txn_id(), 11);
    assert_pair(&latch, b"0\n");
    fs::remove_file(&latch).unwrap();

    assert!(open(&latch).recover_and_load().unwrap());
    assert_pair(&latch, b"1\n");
    assert!(!marker.exists());
}

#[test]
fn invalid_marker_never_enters_legacy_self_heal() {
    for corruption in 0..5 {
        let (_dir, latch, marker) = fixture();
        let _prepared = open(&latch).prepare_clear(12, 5, true).unwrap();
        let mut bytes = fs::read(&marker).unwrap();
        match corruption {
            0 => bytes.truncate(31),
            1 => bytes.push(0),
            2 => bytes[8..10].copy_from_slice(&2_u16.to_le_bytes()),
            3 => bytes[10] = 1,
            4 => bytes[72] = 2,
            _ => unreachable!(),
        }
        if matches!(corruption, 2..=4) {
            let checksum = blake3::hash(&bytes[..80]);
            bytes[80..].copy_from_slice(checksum.as_bytes());
        }
        fs::write(&marker, &bytes).unwrap();
        fs::remove_file(&latch).unwrap();

        assert!(open(&latch).recover_and_load().is_err());
        assert!(!latch.exists(), "坏 marker 不得触发 primary 自愈");
        assert_eq!(fs::read(backup_path(&latch)).unwrap(), b"0\n");
        assert_eq!(fs::read(&marker).unwrap(), bytes);
    }
}

#[test]
fn stale_prepared_proof_cannot_repair_or_remove_a_new_transaction() {
    for repair in [false, true] {
        let (_dir, latch, marker) = fixture();
        let mut store = open(&latch);
        let stale = store.prepare_clear(21, 8, true).unwrap();
        // 夹具模拟外部生命周期已替换事务；旧凭证必须失效。
        fs::remove_file(&marker).unwrap();
        let _current = store.prepare_clear(22, 9, false).unwrap();
        let current_marker = fs::read(&marker).unwrap();

        let error = if repair {
            store
                .repair_prepared(stale, RepairTarget::Latched)
                .unwrap_err()
        } else {
            store.remove_prepared_marker(stale).unwrap_err()
        };
        assert_eq!(error.stage, ClearTxnStage::ValidateMarker);
        assert_pair(&latch, b"0\n");
        assert_eq!(fs::read(&marker).unwrap(), current_marker);
    }
}

#[test]
fn identity_components_cannot_be_resegmented_to_authorize_recovery() {
    let (_dir, latch, marker) = fixture();
    let _prepared = ClearTxnStore::open(&latch, "ab", "c")
        .unwrap()
        .prepare_clear(31, 10, true)
        .unwrap();
    let bytes = fs::read(&marker).unwrap();
    let mut wrong = ClearTxnStore::open(&latch, "a", "bc").unwrap();
    assert!(wrong.recover_and_load().is_err());
    assert_pair(&latch, b"0\n");
    assert_eq!(fs::read(marker).unwrap(), bytes);
}

#[test]
fn absent_marker_preserves_legacy_false_fallback_and_missing_errors() {
    let (_dir, latch, _marker) = fixture();
    fs::write(backup_path(&latch), b"0\n").unwrap();
    fs::remove_file(&latch).unwrap();
    assert!(!open(&latch).recover_and_load().unwrap());
    assert_pair(&latch, b"0\n");
    fs::remove_file(&latch).unwrap();
    fs::remove_file(backup_path(&latch)).unwrap();
    let error = open(&latch).recover_and_load().unwrap_err();
    assert_eq!(error.stage, ClearTxnStage::LoadLatch);
    assert_eq!(error.source.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn disable_repair_does_not_create_an_estop_when_original_was_false() {
    let (_dir, latch, marker) = fixture();
    let mut store = open(&latch);
    let prepared = store.prepare_clear(41, 20, false).unwrap();
    let repaired = store
        .repair_prepared(prepared, RepairTarget::Original)
        .unwrap();
    assert!(marker.exists());
    store.remove_repaired_marker(repaired).unwrap();
    assert!(!open(&latch).recover_and_load().unwrap());
    assert_pair(&latch, b"0\n");
}

#[test]
fn special_marker_files_cannot_bypass_recovery_or_block_on_fifo() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;

    for special in 0..4 {
        let (dir, latch, marker) = fixture();
        match special {
            0 => symlink(dir.path().join("missing"), &marker).unwrap(),
            1 => {
                let _prepared = open(&latch).prepare_clear(51, 30, true).unwrap();
                let other = dir.path().join("other_marker");
                fs::rename(&marker, &other).unwrap();
                symlink(other, &marker).unwrap();
            }
            2 => fs::create_dir(&marker).unwrap(),
            3 => {
                let path = CString::new(marker.as_os_str().as_bytes()).unwrap();
                // SAFETY：路径以 NUL 结尾且在调用期间有效；只创建隔离临时目录内的 FIFO。
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            }
            _ => unreachable!(),
        }
        fs::remove_file(&latch).unwrap();
        fs::write(backup_path(&latch), b"0\n").unwrap();
        assert!(open(&latch).recover_and_load().is_err());
        assert!(!latch.exists());
        assert_eq!(fs::read(backup_path(&latch)).unwrap(), b"0\n");
        assert!(fs::symlink_metadata(&marker).is_ok());
    }
}
