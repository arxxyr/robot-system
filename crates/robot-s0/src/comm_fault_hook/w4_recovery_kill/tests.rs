use super::*;
use crate::clear_estop_transaction::{ClearTxnStore, NoGuard, StepGuard, StepPhase};
use crate::estop_latch::backup_path;
use crate::latch_lock::acquire_latch_lock;
use robot_core::time::MockClock;
use std::cell::Cell;
use std::os::unix::fs::MetadataExt;

pub(super) type Output = Box<dyn FnOnce(Publication) -> io::Result<()>>;
thread_local! {
    static OUTPUT: RefCell<Option<Output>> = const { RefCell::new(None) };
}

pub(super) fn take_output() -> Option<Output> {
    OUTPUT.with(|slot| slot.borrow_mut().take())
}

struct OutputGuard;
impl Drop for OutputGuard {
    fn drop(&mut self) {
        OUTPUT.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

fn output(callback: impl FnOnce(Publication) -> io::Result<()> + 'static) -> OutputGuard {
    OUTPUT.with(|slot| {
        assert!(slot.borrow_mut().replace(Box::new(callback)).is_none());
    });
    OutputGuard
}

fn with_prepared(run: impl FnOnce(&mut ClearTxnStore, &[u8])) {
    let directory = tempfile::tempdir().unwrap();
    let latch = directory.path().join("estop_latch");
    std::fs::write(&latch, b"1\n").unwrap();
    std::fs::write(backup_path(&latch), b"1\n").unwrap();
    let lock = acquire_latch_lock(&latch).unwrap();
    let mut store = ClearTxnStore::open(&latch, "恢复观察测试", "mock").unwrap();
    assert!(
        lock.directory_identity()
            .matches(store.directory_identity())
    );
    let _ = store
        .prepare_clear((3_u128 << 112) | 7, 29, true, &mut NoGuard)
        .unwrap();
    // 凭证来自真实 Prepare；恢复入口稍后自行重新 inspect，测试不构造 Pending。
    let record = std::fs::read(store.marker_path()).unwrap();
    assert_eq!(record.len(), 112);
    run(&mut store, &record);
}

fn assert_scope_cleared() {
    assert!(RECOVERY.with(|slot| slot.borrow().is_none()));
}

#[test]
fn fixed_configuration_preserves_absence_and_rejects_invalid_conflicts_and_platform() {
    assert!(!parse_configuration(None, true, false).unwrap());
    assert!(parse_configuration(Some(OsStr::new(CONFIGURATION)), false, true).unwrap());
    for name in [
        "",
        "repair-rename-primary ",
        "repair-rename-primary:2",
        "repair-before-rename-primary",
        "REPAIR-RENAME-PRIMARY",
    ] {
        assert_eq!(
            parse_configuration(Some(OsStr::new(name)), false, true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
    assert_eq!(
        parse_configuration(Some(OsStr::new(CONFIGURATION)), true, true)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        parse_configuration(Some(OsStr::new(CONFIGURATION)), false, false)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
}

#[test]
fn real_recovery_records_eight_successes_same_pending_and_original_bytes() {
    with_prepared(|store, record| {
        let directory = std::fs::metadata(store.latch_path().parent().unwrap()).unwrap();
        let marker = std::fs::metadata(store.marker_path()).unwrap();
        let expected_record = record.to_vec();
        let called = Rc::new(Cell::new(0));
        let seen = called.clone();
        let latch = store.latch_path().to_owned();
        let marker_path = store.marker_path().to_owned();
        let base = SystemClock.now();
        let _output = output(move |reached| {
            assert!(RECOVERY.with(|slot| slot.try_borrow_mut().is_ok()));
            assert_eq!(reached.base, base.0);
            assert_eq!(reached.observation.record.as_slice(), expected_record);
            assert_eq!(reached.observation.directory.dev(), directory.dev());
            assert_eq!(reached.observation.directory.ino(), directory.ino());
            assert_eq!(reached.observation.marker.dev(), marker.dev());
            assert_eq!(reached.observation.marker.ino(), marker.ino());
            assert_eq!(std::fs::read(&latch).unwrap(), b"1\n");
            assert_eq!(std::fs::read(backup_path(&latch)).unwrap(), b"0\n");
            assert_eq!(std::fs::read(&marker_path).unwrap(), expected_record);
            assert_eq!(std::fs::metadata(&marker_path).unwrap().ino(), marker.ino());
            let line = record_line(reached)?;
            assert!(line.len() <= MAX_RECORD_BYTES);
            assert_eq!(line.lines().count(), 1);
            assert!(line.ends_with(&format!(" prefix={PREFIX}\n")));
            assert!(line.contains(&format!(
                " record={} checksum={}",
                hex(&expected_record),
                hex(&expected_record[80..112])
            )));
            let mut bytes = Vec::new();
            write_record(
                &mut bytes,
                &line,
                reached,
                &MockClock::new(MonoRawNs(reached.reached)),
            )?;
            assert_eq!(bytes, line.as_bytes());
            seen.set(seen.get() + 1);
            NoGuard.after_success(S::RenamePrimary);
            Ok(())
        });
        let scope = install(base, store.directory_identity()).unwrap();
        assert!(store.recover_and_load().unwrap());
        assert_eq!(called.get(), 1);
        assert!(!store.marker_path().exists());
        assert_eq!(std::fs::read(store.latch_path()).unwrap(), b"1\n");
        assert_eq!(
            std::fs::read(backup_path(store.latch_path())).unwrap(),
            b"1\n"
        );
        drop(scope);
        assert_scope_cleared();
    });
}

#[derive(Clone, Copy)]
enum OutputFailure {
    Write,
    PartialWrite,
    Flush,
}

struct FailingOutput {
    phase: OutputFailure,
    bytes: Vec<u8>,
    flushed: bool,
}

impl Write for FailingOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self.phase {
            OutputFailure::Write => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "测试记录写出失败",
                ));
            }
            OutputFailure::PartialWrite if self.bytes.is_empty() => {
                let count = bytes.len().min(8);
                self.bytes.extend_from_slice(&bytes[..count]);
                return Ok(count);
            }
            OutputFailure::PartialWrite => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "测试记录部分写出后失败",
                ));
            }
            OutputFailure::Flush => {}
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.flushed = true;
        Err(io::Error::other("测试记录 flush 失败"))
    }
}

#[test]
fn output_write_and_flush_errors_preserve_original_recovery_success_or_file_failure() {
    for phase in [
        OutputFailure::Write,
        OutputFailure::PartialWrite,
        OutputFailure::Flush,
    ] {
        for fail_backup in [false, true] {
            with_prepared(|store, _| {
                if fail_backup {
                    let backup = backup_path(store.latch_path());
                    std::fs::remove_file(&backup).unwrap();
                    std::fs::create_dir(&backup).unwrap();
                }
                let called = Rc::new(Cell::new(false));
                let seen = called.clone();
                let _output = output(move |reached| {
                    seen.set(true);
                    let line = record_line(reached)?;
                    let mut writer = FailingOutput {
                        phase,
                        bytes: Vec::new(),
                        flushed: false,
                    };
                    let result = write_record(
                        &mut writer,
                        &line,
                        reached,
                        &MockClock::new(MonoRawNs(reached.reached)),
                    );
                    match phase {
                        OutputFailure::Write => assert!(!writer.flushed && writer.bytes.is_empty()),
                        OutputFailure::PartialWrite => {
                            assert!(!writer.flushed && writer.bytes.len() == 8);
                        }
                        OutputFailure::Flush => {
                            assert!(writer.flushed && writer.bytes == line.as_bytes());
                        }
                    }
                    assert!(result.is_err());
                    result
                });
                let scope = install(SystemClock.now(), store.directory_identity()).unwrap();
                let result = store.recover_and_load();
                assert!(called.get());
                assert!(
                    RECOVERY.with(|slot| matches!(
                        slot.borrow().as_ref().unwrap().phase,
                        Phase::Failed
                    ))
                );
                if fail_backup {
                    let error = result.unwrap_err();
                    assert_eq!(error.stage, S::RenameBackup);
                    assert_eq!(error.phase, StepPhase::Operation);
                    assert_eq!(error.source.raw_os_error(), Some(libc::EISDIR));
                    assert!(store.marker_path().exists());
                } else {
                    assert!(result.unwrap());
                    assert!(!store.marker_path().exists());
                }
                drop(scope);
                assert_scope_cleared();
            });
        }
    }
}

#[test]
fn real_operation_failure_before_target_never_emits_and_preserves_stage_errno() {
    with_prepared(|store, record| {
        std::fs::remove_file(store.latch_path()).unwrap();
        std::fs::create_dir(store.latch_path()).unwrap();
        let _output = output(|_| panic!("RenamePrimary 未成功，不能产出 Publication"));
        let scope = install(SystemClock.now(), store.directory_identity()).unwrap();
        let error = store.recover_and_load().unwrap_err();
        assert_eq!(error.stage, S::RenamePrimary);
        assert_eq!(error.phase, StepPhase::Operation);
        assert_eq!(error.source.raw_os_error(), Some(libc::EISDIR));
        assert_eq!(std::fs::read(store.marker_path()).unwrap(), record);
        drop(scope);
        assert_scope_cleared();
    });
}

#[test]
fn tls_scope_rejects_nesting_cleans_unwind_and_is_independent_between_threads() {
    with_prepared(|store, _| {
        let directory = store.directory_identity();
        let base = SystemClock.now();
        assert!(install(MonoRawNs(0), directory).is_err());
        assert!(install(MonoRawNs(u64::MAX), directory).is_err());
        let scope = install(base, directory).unwrap();
        assert!(install(base, directory).is_err());
        assert!(
            RECOVERY.with(|slot| matches!(slot.borrow().as_ref().unwrap().phase, Phase::Failed))
        );
        std::thread::spawn(move || {
            assert_scope_cleared();
            let own = install(SystemClock.now(), directory).unwrap();
            drop(own);
            assert_scope_cleared();
        })
        .join()
        .unwrap();
        assert!(RECOVERY.with(|slot| slot.borrow().is_some()));
        drop(scope);
        assert_scope_cleared();
        let unwind = std::panic::catch_unwind(|| {
            let _scope = install(base, directory).unwrap();
            panic!("测试作用域 unwind");
        });
        assert!(unwind.is_err());
        assert_scope_cleared();
    });
}

#[test]
fn no_pending_or_wrong_directory_never_selects_and_noguard_keeps_original_inputs() {
    let mut guard = NoGuard;
    assert!(guard.before(S::RenamePrimary).is_ok());
    assert_eq!(
        guard.directory_sync_input(S::SyncPrimaryDirectory),
        crate::clear_estop_transaction::DirectorySyncInput::Original
    );
    guard.after_success(S::RenamePrimary);
    assert_scope_cleared();
    with_prepared(|store, _| {
        let other = tempfile::tempdir().unwrap();
        let unrelated = ClearTxnStore::open(&other.path().join("latch"), "其他", "mock").unwrap();
        let _output = output(|_| panic!("错误目录不得选点"));
        let scope = install(SystemClock.now(), unrelated.directory_identity()).unwrap();
        assert!(store.recover_and_load().unwrap());
        assert!(
            RECOVERY.with(|slot| matches!(slot.borrow().as_ref().unwrap().phase, Phase::Failed))
        );
        drop(scope);
        let scope = install(SystemClock.now(), store.directory_identity()).unwrap();
        assert!(store.recover_and_load().unwrap());
        assert!(
            RECOVERY.with(|slot| matches!(slot.borrow().as_ref().unwrap().phase, Phase::Waiting))
        );
        drop(scope);
        assert_scope_cleared();
    });
}

#[test]
fn fixed_cp1_and_output_window_reject_overflow_late_write_and_clock_regression() {
    with_prepared(|store, _| {
        let _output = output(|reached| {
            let observation = reached.observation;
            let base = MonoRawNs(100);
            let valid = checked_reached(base, MonoRawNs(200), observation).unwrap();
            assert_eq!(valid.pause_until, 200 + PAUSE_NS);
            for (base, now) in [
                (MonoRawNs(0), MonoRawNs(200)),
                (base, MonoRawNs(99)),
                (base, MonoRawNs(base.0 + PRE_BOOT_BUDGET.0 - PAUSE_NS)),
                (MonoRawNs(u64::MAX), MonoRawNs(u64::MAX)),
            ] {
                assert!(checked_reached(base, now, observation).is_err());
            }
            let line = record_line(valid)?;
            for now in [valid.reached - 1, valid.pause_until, valid.pause_until + 1] {
                assert!(
                    write_record(
                        &mut Vec::new(),
                        &line,
                        valid,
                        &MockClock::new(MonoRawNs(now))
                    )
                    .is_err()
                );
            }
            Ok(())
        });
        let scope = install(SystemClock.now(), store.directory_identity()).unwrap();
        assert!(store.recover_and_load().unwrap());
        drop(scope);
        assert_scope_cleared();
    });
}

#[test]
fn diagnostic_counterexamples_reject_missing_repeated_or_reordered_successes() {
    with_prepared(|store, _| {
        let observed = Rc::new(Cell::new(None));
        let destination = observed.clone();
        let _output = output(move |reached| {
            destination.set(Some(reached.observation));
            Ok(())
        });
        let scope = install(SystemClock.now(), store.directory_identity()).unwrap();
        assert!(store.recover_and_load().unwrap());
        drop(scope);
        let observation = observed.get().unwrap();
        // 只复用真实读取所得 POD 检验非法回调；不构造 Pending 或声称再次恢复。
        for stages in [
            &[S::ValidateMarker][..],
            &[S::ReadMarker, S::ReadMarker][..],
            &[S::ReadMarker, S::SyncMarkerFile][..],
            &[S::ReadMarker, S::WriteBackup][..],
        ] {
            let _output = output(|_| panic!("非法成功回调序列不得输出"));
            let scope = install(SystemClock.now(), observation.directory).unwrap();
            bind_pending(observation);
            for &stage in stages {
                NoGuard.after_success(stage);
            }
            assert!(
                RECOVERY
                    .with(|slot| matches!(slot.borrow().as_ref().unwrap().phase, Phase::Failed))
            );
            drop(scope);
            assert_scope_cleared();
        }
        let scope = install(SystemClock.now(), observation.directory).unwrap();
        bind_pending(observation);
        bind_pending(observation);
        assert!(
            RECOVERY.with(|slot| matches!(slot.borrow().as_ref().unwrap().phase, Phase::Failed))
        );
        drop(scope);
        assert_scope_cleared();
    });
}
