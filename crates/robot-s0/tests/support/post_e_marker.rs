//! 独立文件系统模型：当前命名空间与持久命名空间分开，目录同步提交整个目录。

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use robot_s0::rx_trust::{
    MarkerCommand, MarkerCommandOutcome, MarkerFs, MarkerValue, MarkerWriteError,
    consume_authorization, run_marker_command, write_marker_two_phase,
};

const DIR: &str = "/独立验收/marker";

#[derive(Default)]
struct Disk {
    visible: BTreeMap<PathBuf, Vec<u8>>,
    durable: BTreeMap<PathBuf, Vec<u8>>,
    step: usize,
    fail_at: Option<usize>,
    temp_seq: usize,
    trace: Vec<String>,
    denied: Option<PathBuf>,
}

struct Fs {
    disk: Mutex<Disk>,
}

impl Fs {
    fn assert_pair(&self, primary: &[u8], backup: &[u8]) {
        assert_eq!(self.pair(), (Some(primary.to_vec()), Some(backup.to_vec())));
    }

    fn assert_backup_first(&self, attempted: usize) {
        let expected: Vec<_> = ["rx_trust_marker.bak", "rx_trust_marker"]
            .into_iter()
            .flat_map(|name| {
                ["temp写入并同步", "rename", "目录同步"]
                    .into_iter()
                    .map(move |operation| format!("{operation}:{DIR}/{name}"))
            })
            .take(attempted)
            .collect();
        assert_eq!(
            self.disk.lock().unwrap().trace,
            expected,
            "必须先完整处理备份，再接主文件"
        );
    }
    fn new(primary: Option<&[u8]>, backup: Option<&[u8]>, fail_at: Option<usize>) -> Self {
        let mut disk = Disk {
            fail_at,
            ..Disk::default()
        };
        for (name, value) in [
            ("rx_trust_marker", primary),
            ("rx_trust_marker.bak", backup),
        ] {
            if let Some(value) = value {
                disk.visible
                    .insert(Path::new(DIR).join(name), value.to_vec());
            }
        }
        disk.visible
            .insert(Path::new(DIR).join("estop_latch"), b"1\n".to_vec());
        disk.durable = disk.visible.clone();
        Self {
            disk: Mutex::new(disk),
        }
    }

    fn fail(disk: &mut Disk, operation: &str, path: &Path) -> io::Result<()> {
        disk.step += 1;
        disk.trace.push(format!("{operation}:{}", path.display()));
        if disk.fail_at == Some(disk.step) {
            return Err(io::Error::other("独立注入存储失败"));
        }
        Ok(())
    }

    fn bytes(&self, name: &str) -> Option<Vec<u8>> {
        self.disk
            .lock()
            .unwrap()
            .visible
            .get(&Path::new(DIR).join(name))
            .cloned()
    }
    fn pair(&self) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        (
            self.bytes("rx_trust_marker"),
            self.bytes("rx_trust_marker.bak"),
        )
    }
    fn crash(&self) {
        let mut disk = self.disk.lock().unwrap();
        disk.visible = disk.durable.clone();
        disk.fail_at = None;
    }
    fn clear_failure(&self) {
        self.disk.lock().unwrap().fail_at = None;
    }
}

impl MarkerFs for Fs {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let disk = self.disk.lock().unwrap();
        if disk.denied.as_deref() == Some(path) {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        disk.visible
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
    fn write_sync_temp(&self, path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        let mut disk = self.disk.lock().unwrap();
        Self::fail(&mut disk, "temp写入并同步", path)?;
        disk.temp_seq += 1;
        let temp = path.with_file_name(format!(".临时_{}", disk.temp_seq));
        disk.visible.insert(temp.clone(), bytes.to_vec());
        Ok(temp)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut disk = self.disk.lock().unwrap();
        Self::fail(&mut disk, "rename", to)?;
        let bytes = disk
            .visible
            .remove(from)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        disk.visible.insert(to.to_owned(), bytes);
        Ok(())
    }
    fn sync_parent(&self, path: &Path) -> io::Result<()> {
        let mut disk = self.disk.lock().unwrap();
        Self::fail(&mut disk, "目录同步", path)?;
        let parent = path.parent();
        // 同目录所有此前rename一起持久化；不能只提交传入path。
        disk.durable.retain(|key, _| key.parent() != parent);
        let snapshot: Vec<_> = disk
            .visible
            .iter()
            .filter(|(key, _)| key.parent() == parent)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        disk.durable.extend(snapshot);
        Ok(())
    }
    fn remove_temp(&self, path: &Path) -> io::Result<()> {
        self.disk.lock().unwrap().visible.remove(path);
        Ok(())
    }
}

#[test]
fn marker_truth_table_never_treats_missing_or_noncanonical_bytes_as_authorized() {
    let values: [Option<&[u8]>; 8] = [
        Some(b"0\n"),
        Some(b"1\n"),
        None,
        Some(b"0"),
        Some(b"0\n\n"),
        Some(b""),
        Some(b"garbage"),
        Some(&[255]),
    ];
    for primary in values {
        for backup in values {
            let fs = Fs::new(primary, backup, None);
            let outcome = consume_authorization(&fs, Path::new(DIR)).expect("纯模型无存储错误");
            assert_eq!(
                outcome.authorized,
                primary == Some(b"0\n".as_slice()) && backup == Some(b"0\n".as_slice())
            );
            assert_eq!(fs.pair(), (Some(b"1\n".to_vec()), Some(b"1\n".to_vec())));
            assert_eq!(fs.bytes("estop_latch"), Some(b"1\n".to_vec()));
            assert!(
                !consume_authorization(&fs, Path::new(DIR))
                    .unwrap()
                    .authorized
            );
        }
    }
}

#[test]
fn two_phase_write_reports_visibility_and_durability_separately() {
    for fail_at in 1..=3 {
        let fs = Fs::new(Some(b"1\n"), Some(b"1\n"), Some(fail_at));
        let result = write_marker_two_phase(
            &fs,
            &Path::new(DIR).join("rx_trust_marker"),
            MarkerValue::AuthorizedOnce,
        );
        match fail_at {
            1 | 2 => {
                assert!(matches!(result, Err(MarkerWriteError::NotCommitted(_))));
                assert_eq!(fs.bytes("rx_trust_marker"), Some(b"1\n".to_vec()));
            }
            _ => {
                assert!(matches!(
                    result,
                    Err(MarkerWriteError::CommittedNotDurable(_))
                ));
                assert_eq!(fs.bytes("rx_trust_marker"), Some(b"0\n".to_vec()));
            }
        }
        assert!(fs.disk.lock().unwrap().visible.keys().all(|path| {
            !path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".临时")
        }));
        fs.crash();
        assert_eq!(fs.bytes("rx_trust_marker"), Some(b"1\n".to_vec()));
    }
}

#[test]
fn permission_denied_is_an_error_and_never_missing_authorization() {
    for name in ["rx_trust_marker", "rx_trust_marker.bak"] {
        let fs = Fs::new(Some(b"0\n"), Some(b"0\n"), None);
        fs.disk.lock().unwrap().denied = Some(Path::new(DIR).join(name));
        assert!(consume_authorization(&fs, Path::new(DIR)).is_err());
        assert_eq!(fs.disk.lock().unwrap().step, 0);
        assert_eq!(fs.pair(), (Some(b"0\n".to_vec()), Some(b"0\n".to_vec())));
    }
}

#[test]
fn authorize_failure_matrix_preserves_canonical_precondition_and_exit_three_meaning() {
    for fail_at in 1..=6 {
        let fs = Fs::new(Some(b"1\n"), Some(b"1\n"), Some(fail_at));
        let result = run_marker_command(&fs, Path::new(DIR), MarkerCommand::Authorize).unwrap();
        fs.assert_backup_first(fail_at);
        match fail_at {
            1 | 2 => fs.assert_pair(b"1\n", b"1\n"),
            3..=5 => fs.assert_pair(b"1\n", b"0\n"),
            _ => fs.assert_pair(b"0\n", b"0\n"),
        }
        if fail_at == 6 {
            assert!(matches!(result, MarkerCommandOutcome::AuthorizedNotDurable));
            assert_eq!(fs.pair(), (Some(b"0\n".to_vec()), Some(b"0\n".to_vec())));
            fs.clear_failure();
            assert!(
                consume_authorization(&fs, Path::new(DIR))
                    .unwrap()
                    .authorized,
                "退3后的可见授权已经生效"
            );
        } else {
            assert!(matches!(result, MarkerCommandOutcome::Rejected));
            fs.crash();
            assert!(
                !consume_authorization(&fs, Path::new(DIR))
                    .unwrap()
                    .authorized
            );
        }
    }
    let values: [Option<&[u8]>; 8] = [
        Some(b"0\n"),
        Some(b"1\n"),
        None,
        Some(b"0"),
        Some(b"0\n\n"),
        Some(b""),
        Some(b"garbage"),
        Some(&[255]),
    ];
    for primary in values {
        for backup in values {
            if primary == Some(b"1\n".as_slice()) && backup == Some(b"1\n".as_slice()) {
                continue;
            }
            let fs = Fs::new(primary, backup, None);
            let before = fs.pair();
            assert!(matches!(
                run_marker_command(&fs, Path::new(DIR), MarkerCommand::Authorize).unwrap(),
                MarkerCommandOutcome::Rejected
            ));
            assert_eq!(fs.pair(), before);
            assert_eq!(fs.disk.lock().unwrap().step, 0);
        }
    }
}

#[test]
fn revoke_only_claims_a_barrier_after_backup_directory_sync() {
    for fail_at in 1..=6 {
        let fs = Fs::new(Some(b"0\n"), Some(b"0\n"), Some(fail_at));
        let result = run_marker_command(&fs, Path::new(DIR), MarkerCommand::Revoke).unwrap();
        fs.assert_backup_first(fail_at);
        match fail_at {
            1 | 2 => fs.assert_pair(b"0\n", b"0\n"),
            3..=5 => fs.assert_pair(b"0\n", b"1\n"),
            _ => fs.assert_pair(b"1\n", b"1\n"),
        }
        match fail_at {
            1..=3 => assert!(matches!(result, MarkerCommandOutcome::Rejected)),
            _ => assert!(matches!(result, MarkerCommandOutcome::RevokedNotCanonical)),
        }
        fs.crash();
        fs.assert_pair(b"0\n", if fail_at <= 3 { b"0\n" } else { b"1\n" });
        let outcome = consume_authorization(&fs, Path::new(DIR)).unwrap();
        assert_eq!(
            outcome.authorized,
            fail_at <= 3,
            "backup未持久允许掉电恢复；持久后禁止复用"
        );
    }
}

#[test]
fn consume_failure_never_grants_authorization_and_later_directory_sync_commits_prior_renames() {
    for fail_at in 1..=6 {
        let fs = Fs::new(Some(b"0\n"), Some(b"0\n"), Some(fail_at));
        let result = consume_authorization(&fs, Path::new(DIR));
        fs.assert_backup_first(if fail_at == 3 { 6 } else { fail_at });
        match fail_at {
            1 | 2 => fs.assert_pair(b"0\n", b"0\n"),
            3 | 6 => fs.assert_pair(b"1\n", b"1\n"),
            _ => fs.assert_pair(b"0\n", b"1\n"),
        }
        match fail_at {
            1 | 2 => assert!(result.is_err(), "backup phase1失败必须拒启动"),
            _ => assert!(!result.unwrap().authorized),
        }
        fs.crash();
        match fail_at {
            1 | 2 => fs.assert_pair(b"0\n", b"0\n"),
            3 => fs.assert_pair(b"1\n", b"1\n"),
            _ => fs.assert_pair(b"0\n", b"1\n"),
        }
        let next = consume_authorization(&fs, Path::new(DIR)).unwrap();
        // backup目录同步失败后，继续规范化primary的目录同步会把此前backup rename一起持久化。
        assert_eq!(next.authorized, fail_at <= 2);
    }
}

#[test]
fn authorize_power_loss_rolls_back_only_the_uncommitted_directory_changes() {
    for fail_at in 1..=6 {
        let fs = Fs::new(Some(b"1\n"), Some(b"1\n"), Some(fail_at));
        let _ = run_marker_command(&fs, Path::new(DIR), MarkerCommand::Authorize).unwrap();
        fs.assert_backup_first(fail_at);
        fs.crash();
        fs.assert_pair(b"1\n", if fail_at <= 3 { b"1\n" } else { b"0\n" });
        assert!(
            !consume_authorization(&fs, Path::new(DIR))
                .unwrap()
                .authorized
        );
    }
}
