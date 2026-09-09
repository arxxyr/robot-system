//! 从公开 API 验证事务恢复、凭证返还与旧 loader 的边界；不依赖模块内的故障注入实现。

use std::fs;
use std::path::{Path, PathBuf};

use robot_s0::clear_estop_transaction::{
    ClearTxnStage, ClearTxnStore, GuardInterrupt, InterruptReason, LatchCopyState, NoGuard,
    PairView, PrepareFailureToken, RemoveFailureToken, RepairTarget, StepGuard, StepPhase,
};
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

/// 公开守门实现：在指定步骤前以急停义务为由让路。
struct YieldBefore(ClearTxnStage);

impl StepGuard for YieldBefore {
    fn before(&mut self, stage: ClearTxnStage) -> Result<(), GuardInterrupt> {
        if stage == self.0 {
            Err(GuardInterrupt::new(InterruptReason::EStopObligationPending))
        } else {
            Ok(())
        }
    }
}

#[test]
fn pending_transaction_recovers_before_backup_can_self_heal_primary() {
    let (_dir, latch, marker) = fixture();
    let prepared = open(&latch)
        .prepare_clear(11, 4, true, &mut NoGuard)
        .unwrap();
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
        let _prepared = open(&latch)
            .prepare_clear(12, 5, true, &mut NoGuard)
            .unwrap();
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
        let stale = store.prepare_clear(21, 8, true, &mut NoGuard).unwrap();
        // 夹具模拟外部生命周期已替换事务；旧凭证必须失效。
        fs::remove_file(&marker).unwrap();
        let _current = store.prepare_clear(22, 9, false, &mut NoGuard).unwrap();
        let current_marker = fs::read(&marker).unwrap();

        let error = if repair {
            let error = store
                .repair(stale, RepairTarget::Latched, &mut NoGuard)
                .unwrap_err();
            assert!(error.token.is_none(), "旧凭证必须失效");
            error.error
        } else {
            let error = store.remove(stale, &mut NoGuard).unwrap_err();
            assert!(matches!(error.token, RemoveFailureToken::Lost));
            error.error
        };
        assert_eq!(error.stage, ClearTxnStage::ValidateMarker);
        assert_eq!(error.phase, StepPhase::Operation);
        assert_pair(&latch, b"0\n");
        assert_eq!(fs::read(&marker).unwrap(), current_marker);
    }
}

#[test]
fn identity_components_cannot_be_resegmented_to_authorize_recovery() {
    let (_dir, latch, marker) = fixture();
    let _prepared = ClearTxnStore::open(&latch, "ab", "c")
        .unwrap()
        .prepare_clear(31, 10, true, &mut NoGuard)
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
    let prepared = store.prepare_clear(41, 20, false, &mut NoGuard).unwrap();
    let repaired = store
        .repair(prepared, RepairTarget::Original, &mut NoGuard)
        .unwrap();
    assert!(marker.exists());
    store.remove(repaired, &mut NoGuard).unwrap();
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
                let _prepared = open(&latch)
                    .prepare_clear(51, 30, true, &mut NoGuard)
                    .unwrap();
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

#[test]
fn guard_interrupt_before_marker_write_returns_created_token_that_discards_cleanly() {
    let (_dir, latch, marker) = fixture();
    let mut store = open(&latch);
    let error = store
        .prepare_clear(61, 40, true, &mut YieldBefore(ClearTxnStage::WriteMarker))
        .unwrap_err();
    assert_eq!(error.error.stage, ClearTxnStage::WriteMarker);
    assert_eq!(error.error.phase, StepPhase::Before);
    assert_eq!(
        error.error.guard_interrupt(),
        Some(InterruptReason::EStopObligationPending)
    );
    assert_ne!(error.error.source.kind(), std::io::ErrorKind::Interrupted);
    let PrepareFailureToken::Created(created) = error.token else {
        panic!("marker 已创建必须返还 Created");
    };
    assert_eq!(created.record().txn_id(), 61);
    assert_eq!(fs::metadata(&marker).unwrap().len(), 0);
    assert_pair(&latch, b"1\n");
    store.discard_created(created, &mut NoGuard).unwrap();
    assert!(!marker.exists());
    assert_pair(&latch, b"1\n");
    assert!(open(&latch).recover_and_load().unwrap());
}

#[test]
fn guard_interrupt_before_zeroing_returns_durable_token_that_repairs_to_latched() {
    let (_dir, latch, marker) = fixture();
    let mut store = open(&latch);
    let error = store
        .prepare_clear(62, 41, true, &mut YieldBefore(ClearTxnStage::CreateBackup))
        .unwrap_err();
    assert_eq!(error.error.stage, ClearTxnStage::CreateBackup);
    let PrepareFailureToken::Durable(durable) = error.token else {
        panic!("marker 已 durable 必须返还 Durable");
    };
    assert!(marker.exists());
    assert_pair(&latch, b"1\n");
    let repaired = store
        .repair(durable, RepairTarget::Latched, &mut NoGuard)
        .unwrap();
    store.remove(repaired, &mut NoGuard).unwrap();
    assert!(!marker.exists());
    assert!(open(&latch).recover_and_load().unwrap());
}

#[test]
fn persist_latched_and_cross_store_refusal_are_public_contracts() {
    let (_dir, latch, marker) = fixture();
    fs::write(&latch, b"0\n").unwrap();
    fs::write(backup_path(&latch), b"0\n").unwrap();
    let mut store = open(&latch);
    store.persist_latched(&mut NoGuard).unwrap();
    assert_pair(&latch, b"1\n");

    let other_dir = tempfile::tempdir().unwrap();
    let other_latch = other_dir.path().join("estop_latch");
    fs::write(&other_latch, b"1\n").unwrap();
    fs::write(backup_path(&other_latch), b"1\n").unwrap();
    let mut other = open(&other_latch);
    assert_ne!(other.directory_identity(), store.directory_identity());
    let prepared = store.prepare_clear(71, 50, true, &mut NoGuard).unwrap();
    let error = other
        .repair(prepared, RepairTarget::Latched, &mut NoGuard)
        .unwrap_err();
    assert_eq!(error.error.stage, ClearTxnStage::ValidateCredential);
    let prepared = error.token.expect("跨 store 拒绝不得失权");
    assert!(marker.exists());
    assert_pair(&other_latch, b"1\n");
    store.remove(prepared, &mut NoGuard).unwrap();
    assert!(!marker.exists());
    assert_pair(&latch, b"0\n");
}

// ── W0.1 只读双份核验（公开 API）───────────────────────────────────────────

/// FIFO 子进程模式：环境变量给出 latch 路径，子进程只调用 `inspect_pair` 并以退出码
/// 报告（0 = 得到 `InspectPair`/Operation/`InvalidInput`，2 = 其它结果）。
const INSPECT_FIFO_CHILD_ENV: &str = "ROBOT_S0_INSPECT_PAIR_FIFO_CHILD";

/// 目录条目快照：名字、普通文件内容与是否符号链接。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DirEntrySnapshot {
    name: std::ffi::OsString,
    bytes: Vec<u8>,
    symlink: bool,
}

fn snapshot(dir: &Path) -> Vec<DirEntrySnapshot> {
    let mut rows: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = fs::symlink_metadata(entry.path()).unwrap();
            let bytes = if metadata.is_file() {
                fs::read(entry.path()).unwrap()
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

/// 只在指定阶段拒绝一次的公开守门。
struct RefuseOnce(Option<ClearTxnStage>);

impl StepGuard for RefuseOnce {
    fn before(&mut self, stage: ClearTxnStage) -> Result<(), GuardInterrupt> {
        if self.0 == Some(stage) {
            self.0 = None;
            return Err(GuardInterrupt::new(InterruptReason::EStopObligationPending));
        }
        Ok(())
    }
}

/// 公开矩阵：0/0、1/1、两种不一致、单 / 双缺失、坏值、合法无换行；标记与全部条目不变。
#[test]
fn inspect_pair_public_matrix_is_read_only() {
    use LatchCopyState as S;
    /// (primary 内容, backup 内容, 期望 primary 状态, 期望 backup 状态)；`None` = 文件不存在。
    type Case = (Option<&'static [u8]>, Option<&'static [u8]>, S, S);
    let cases: [Case; 9] = [
        (Some(b"0\n"), Some(b"0\n"), S::Value(false), S::Value(false)),
        (Some(b"1\n"), Some(b"1\n"), S::Value(true), S::Value(true)),
        (Some(b"1\n"), Some(b"0\n"), S::Value(true), S::Value(false)),
        (Some(b"0\n"), Some(b"1\n"), S::Value(false), S::Value(true)),
        (Some(b"1"), Some(b"0"), S::Value(true), S::Value(false)),
        (None, Some(b"0\n"), S::Missing, S::Value(false)),
        (Some(b"1\n"), None, S::Value(true), S::Missing),
        (None, None, S::Missing, S::Missing),
        (Some(b"xx"), Some(b""), S::Corrupt, S::Corrupt),
    ];
    for (primary, backup, expect_primary, expect_backup) in cases {
        let dir = tempfile::tempdir().unwrap();
        let latch = dir.path().join("estop_latch");
        if let Some(bytes) = primary {
            fs::write(&latch, bytes).unwrap();
        }
        if let Some(bytes) = backup {
            fs::write(backup_path(&latch), bytes).unwrap();
        }
        fs::write(dir.path().join("estop_latch.clear_pending"), b"opaque").unwrap();
        let before = snapshot(dir.path());
        let mut store = open(&latch);
        let view = store.inspect_pair(&mut NoGuard).unwrap();
        assert_eq!(
            view,
            PairView {
                primary: expect_primary,
                backup: expect_backup,
                directory_identity: store.directory_identity(),
            },
            "{primary:?}/{backup:?}"
        );
        assert_eq!(snapshot(dir.path()), before, "只读：不得写 / 自愈 / 改标记");
        // 再核一次也不变：核验不产生凭证或状态。
        assert_eq!(store.inspect_pair(&mut NoGuard).unwrap(), view);
    }
}

/// 公开对象拒绝：symlink / hardlink / 目录 → `InspectPair` Operation 真实错误，
/// 不是 Corrupt，目录不变。
#[test]
fn inspect_pair_public_rejects_links_and_directories() {
    let dir = tempfile::tempdir().unwrap();
    let latch = dir.path().join("estop_latch");
    fs::write(dir.path().join("real"), b"0\n").unwrap();
    std::os::unix::fs::symlink("real", &latch).unwrap();
    fs::write(backup_path(&latch), b"0\n").unwrap();
    let before = snapshot(dir.path());
    let error = open(&latch).inspect_pair(&mut NoGuard).unwrap_err();
    assert_eq!(error.stage, ClearTxnStage::InspectPair);
    assert_eq!(error.phase, StepPhase::Operation);
    assert_eq!(error.source.raw_os_error(), Some(libc::ELOOP));
    assert_eq!(snapshot(dir.path()), before);

    let dir = tempfile::tempdir().unwrap();
    let latch = dir.path().join("estop_latch");
    fs::write(&latch, b"1\n").unwrap();
    fs::hard_link(&latch, dir.path().join("other")).unwrap();
    fs::write(backup_path(&latch), b"1\n").unwrap();
    let error = open(&latch).inspect_pair(&mut NoGuard).unwrap_err();
    assert_eq!(error.stage, ClearTxnStage::InspectPair);
    assert_eq!(error.phase, StepPhase::Operation);
    assert_eq!(error.source.kind(), std::io::ErrorKind::InvalidInput);

    let dir = tempfile::tempdir().unwrap();
    let latch = dir.path().join("estop_latch");
    fs::create_dir(&latch).unwrap();
    let error = open(&latch).inspect_pair(&mut NoGuard).unwrap_err();
    assert_eq!(error.stage, ClearTxnStage::InspectPair);
    assert_eq!(error.phase, StepPhase::Operation);
    assert_eq!(error.source.kind(), std::io::ErrorKind::InvalidInput);
}

/// 子进程入口：父进程模式（无环境变量）直接返回。
#[test]
fn inspect_pair_fifo_child() {
    let Some(latch) = std::env::var_os(INSPECT_FIFO_CHILD_ENV) else {
        return;
    };
    let code = match open(Path::new(&latch)).inspect_pair(&mut NoGuard) {
        Err(error)
            if error.stage == ClearTxnStage::InspectPair
                && error.phase == StepPhase::Operation
                && error.source.kind() == std::io::ErrorKind::InvalidInput =>
        {
            0
        }
        _ => 2,
    };
    std::process::exit(code);
}

/// FIFO：在有界超时的子进程里核验（错误实现若在 open/read 上阻塞，父进程会 kill 并
/// 判失败而不是挂住测试）；正确实现以 `O_NONBLOCK` 取得类型后拒绝为 `InvalidInput`。
#[test]
fn inspect_pair_public_rejects_fifo_without_blocking() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileTypeExt;
    use std::time::{Duration, Instant};

    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let latch = dir.path().join("estop_latch");
    fs::write(&latch, b"0\n").unwrap();
    let fifo = backup_path(&latch);
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY：路径以 NUL 结尾；mkfifo 只创建命名管道，不打开它。
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let before = snapshot(dir.path());

    let mut child = ChildGuard(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "inspect_pair_fifo_child",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env(INSPECT_FIFO_CHILD_ENV, &latch)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let code = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status.code();
        }
        assert!(
            Instant::now() < deadline,
            "inspect_pair 在 FIFO 上阻塞：子进程未在期限内退出"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(code, Some(0), "FIFO 必须以 InvalidInput 拒绝且不阻塞");
    assert_eq!(snapshot(dir.path()), before);
    assert!(
        fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo(),
        "FIFO 不得被替换或删除"
    );
}

/// 父目录被替换后仍读打开时的目录 fd：返回旧值与原身份。
#[test]
fn inspect_pair_public_reads_bound_directory_after_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("robot");
    fs::create_dir(&parent).unwrap();
    let latch = parent.join("estop_latch");
    fs::write(&latch, b"1\n").unwrap();
    fs::write(backup_path(&latch), b"0\n").unwrap();
    let mut store = open(&latch);
    let identity = store.directory_identity();
    fs::rename(&parent, dir.path().join("robot.old")).unwrap();
    fs::create_dir(&parent).unwrap();
    fs::write(&latch, b"0\n").unwrap();
    fs::write(backup_path(&latch), b"1\n").unwrap();
    let view = store.inspect_pair(&mut NoGuard).unwrap();
    assert_eq!(view.primary, LatchCopyState::Value(true));
    assert_eq!(view.backup, LatchCopyState::Value(false));
    assert_eq!(view.directory_identity, identity);
    let replaced = open(&latch);
    assert_ne!(replaced.directory_identity(), identity, "新目录身份不同");
}

/// 公开守门在 `InspectPair` 拒绝 → Before 相、真实中断原因，不读文件；放行后正常返回。
#[test]
fn inspect_pair_public_guard_refusal_is_before_phase() {
    let (dir, latch, marker) = fixture();
    fs::write(&marker, b"opaque").unwrap();
    let before = snapshot(dir.path());
    let mut store = open(&latch);
    let mut guard = RefuseOnce(Some(ClearTxnStage::InspectPair));
    let error = store.inspect_pair(&mut guard).unwrap_err();
    assert_eq!(error.stage, ClearTxnStage::InspectPair);
    assert_eq!(error.phase, StepPhase::Before);
    assert_eq!(
        error.guard_interrupt(),
        Some(InterruptReason::EStopObligationPending)
    );
    let view = store.inspect_pair(&mut guard).unwrap();
    assert_eq!(view.primary, LatchCopyState::Value(true));
    assert_eq!(view.backup, LatchCopyState::Value(true));
    assert_eq!(snapshot(dir.path()), before);
}
