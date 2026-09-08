//! RX 信任的一次性授权、两阶段持久化与稳定文件独占锁。
//!
//! 授权只来自两份严格的 `0\n`；消费先持久收紧备份屏障，再收紧主文件。
//! 本模块不读取、清除或恢复独立的 `EStop` latch。

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const PRIMARY_NAME: &str = "rx_trust_marker";
pub const BACKUP_NAME: &str = "rx_trust_marker.bak";
pub const LOCK_NAME: &str = "rx_trust.lock";

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// rename 之前失败与 rename 已生效但目录尚未确认持久是不同的业务结果。
#[derive(Debug)]
pub enum MarkerWriteError {
    NotCommitted(io::Error),
    CommittedNotDurable(io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerValue {
    Dirty,
    AuthorizedOnce,
    Missing,
    Corrupt,
}

impl MarkerValue {
    const fn bytes(self) -> Option<&'static [u8]> {
        match self {
            Self::Dirty => Some(b"1\n"),
            Self::AuthorizedOnce => Some(b"0\n"),
            Self::Missing | Self::Corrupt => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkerStatus {
    pub primary: MarkerValue,
    pub backup: MarkerValue,
}

impl MarkerStatus {
    #[must_use]
    pub const fn canonical_dirty(self) -> bool {
        matches!(self.primary, MarkerValue::Dirty) && matches!(self.backup, MarkerValue::Dirty)
    }

    #[must_use]
    pub const fn authorized_candidate(self) -> bool {
        matches!(self.primary, MarkerValue::AuthorizedOnce)
            && matches!(self.backup, MarkerValue::AuthorizedOnce)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsumeOutcome {
    pub authorized: bool,
    /// 消费之前读取的状态，用于准确审计授权来源。
    pub status: MarkerStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerCommand {
    Authorize,
    Revoke,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerCommandOutcome {
    Complete,
    Rejected,
    AuthorizedNotDurable,
    RevokedNotCanonical,
}

impl MarkerCommandOutcome {
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Complete => 0,
            Self::Rejected => 2,
            Self::AuthorizedNotDurable => 3,
            Self::RevokedNotCanonical => 4,
        }
    }
}

/// 真实文件系统与确定性掉电测试共用同一写入算法。
pub trait MarkerFs {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn write_sync_temp(&self, path: &Path, bytes: &[u8]) -> io::Result<PathBuf>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn sync_parent(&self, path: &Path) -> io::Result<()>;
    fn remove_temp(&self, path: &Path) -> io::Result<()>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RealMarkerFs;

impl MarkerFs for RealMarkerFs {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        // 三字节已足以区分全部合法内容与损坏，避免误配置的大文件无界分配。
        let mut bytes = Vec::with_capacity(3);
        File::open(path)?.take(3).read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn write_sync_temp(&self, path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        let file_name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "marker 路径必须包含文件名")
        })?;
        let (temporary, mut file) = loop {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let mut name = file_name.to_os_string();
            name.push(format!(".tmp.{}.{sequence}", std::process::id()));
            let temporary = path.with_file_name(name);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
            {
                Ok(file) => break (temporary, file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        };
        let result = file.write_all(bytes).and_then(|()| {
            #[cfg(feature = "test-comm-fault-hook")]
            crate::comm_fault_hook::boot_pause("marker_write");
            file.sync_all()
        });
        match result {
            Ok(()) => Ok(temporary),
            Err(error) => {
                let _ = std::fs::remove_file(&temporary);
                Err(error)
            }
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn sync_parent(&self, path: &Path) -> io::Result<()> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        File::open(parent.unwrap_or_else(|| Path::new(".")))?.sync_all()
    }

    fn remove_temp(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
}

/// 写入的线性化点是 rename；只有后续父目录同步成功才宣称持久。
///
/// # Errors
///
/// 失败阶段由返回枚举区分；`Missing` / `Corrupt` 不是可写入的状态。
pub fn write_marker_two_phase(
    fs: &dyn MarkerFs,
    path: &Path,
    value: MarkerValue,
) -> Result<(), MarkerWriteError> {
    let bytes = value.bytes().ok_or_else(|| {
        MarkerWriteError::NotCommitted(io::Error::new(
            io::ErrorKind::InvalidInput,
            "marker 只允许写入 Dirty 或 AuthorizedOnce",
        ))
    })?;
    let temporary = fs
        .write_sync_temp(path, bytes)
        .map_err(MarkerWriteError::NotCommitted)?;
    if let Err(error) = fs.rename(&temporary, path) {
        let _ = fs.remove_temp(&temporary);
        return Err(MarkerWriteError::NotCommitted(error));
    }
    fs.sync_parent(path)
        .map_err(MarkerWriteError::CommittedNotDurable)
}

fn read_value(fs: &dyn MarkerFs, path: &Path) -> io::Result<MarkerValue> {
    match fs.read(path) {
        Ok(bytes) => Ok(match bytes.as_slice() {
            b"1\n" => MarkerValue::Dirty,
            b"0\n" => MarkerValue::AuthorizedOnce,
            _ => MarkerValue::Corrupt,
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(MarkerValue::Missing),
        Err(error) => Err(error),
    }
}

/// 只分类内容，缺失与损坏不等于授权。
///
/// # Errors
///
/// 权限等读取错误透传，不降格为缺失或损坏。
pub fn marker_status(fs: &dyn MarkerFs, dir: &Path) -> io::Result<MarkerStatus> {
    Ok(MarkerStatus {
        primary: read_value(fs, &dir.join(PRIMARY_NAME))?,
        backup: read_value(fs, &dir.join(BACKUP_NAME))?,
    })
}

fn log_write_failure(path: &Path, error: &MarkerWriteError) {
    tracing::warn!(path = %path.display(), ?error, "RX marker 写入未完整持久");
}

fn normalize_dirty(fs: &dyn MarkerFs, path: &Path) -> bool {
    match write_marker_two_phase(fs, path, MarkerValue::Dirty) {
        Ok(()) => true,
        Err(error) => {
            log_write_failure(path, &error);
            false
        }
    }
}

/// 只有两份授权都成功持久收紧时，当前进程才取得一次性信任。
///
/// # Errors
///
/// 读取失败，或授权候选的备份在 rename 前失败，必须拒绝启动。
pub fn consume_authorization(fs: &dyn MarkerFs, dir: &Path) -> io::Result<ConsumeOutcome> {
    let status = marker_status(fs, dir)?;
    let primary = dir.join(PRIMARY_NAME);
    let backup = dir.join(BACKUP_NAME);
    let authorized = if status.authorized_candidate() {
        let backup_durable = match write_marker_two_phase(fs, &backup, MarkerValue::Dirty) {
            Ok(()) => true,
            Err(MarkerWriteError::NotCommitted(error)) => return Err(error),
            Err(error @ MarkerWriteError::CommittedNotDurable(_)) => {
                log_write_failure(&backup, &error);
                false
            }
        };
        // 即便备份目录同步失败，也继续主文件规范化，但本进程不再使用授权。
        let primary_durable = normalize_dirty(fs, &primary);
        backup_durable && primary_durable
    } else {
        if status.backup != MarkerValue::Dirty {
            let _ = normalize_dirty(fs, &backup);
        }
        if status.primary != MarkerValue::Dirty {
            let _ = normalize_dirty(fs, &primary);
        }
        false
    };
    Ok(ConsumeOutcome { authorized, status })
}

/// 调用者必须持有独占锁；只有规范的 Dirty 双文件允许放宽。
///
/// # Errors
///
/// 状态读取的系统错误透传；写入失败按命令的线性化与屏障语义返回业务结果。
pub fn run_marker_command(
    fs: &dyn MarkerFs,
    dir: &Path,
    command: MarkerCommand,
) -> io::Result<MarkerCommandOutcome> {
    let status = marker_status(fs, dir)?;
    let backup = dir.join(BACKUP_NAME);
    let primary = dir.join(PRIMARY_NAME);
    match command {
        MarkerCommand::Status => Ok(MarkerCommandOutcome::Complete),
        MarkerCommand::Authorize => {
            if !status.canonical_dirty() {
                return Ok(MarkerCommandOutcome::Rejected);
            }
            if let Err(error) = write_marker_two_phase(fs, &backup, MarkerValue::AuthorizedOnce) {
                log_write_failure(&backup, &error);
                return Ok(MarkerCommandOutcome::Rejected);
            }
            Ok(
                match write_marker_two_phase(fs, &primary, MarkerValue::AuthorizedOnce) {
                    Ok(()) => MarkerCommandOutcome::Complete,
                    Err(error @ MarkerWriteError::NotCommitted(_)) => {
                        log_write_failure(&primary, &error);
                        MarkerCommandOutcome::Rejected
                    }
                    Err(error @ MarkerWriteError::CommittedNotDurable(_)) => {
                        log_write_failure(&primary, &error);
                        MarkerCommandOutcome::AuthorizedNotDurable
                    }
                },
            )
        }
        MarkerCommand::Revoke => {
            if !normalize_dirty(fs, &backup) {
                return Ok(MarkerCommandOutcome::Rejected);
            }
            Ok(if normalize_dirty(fs, &primary) {
                MarkerCommandOutcome::Complete
            } else {
                MarkerCommandOutcome::RevokedNotCanonical
            })
        }
    }
}

#[derive(Debug)]
pub enum TrustLockError {
    Held { pid: Option<u32> },
    Io(io::Error),
}

/// 稳定 fd 的生命周期就是独占锁的生命周期，不重命名、不删除锁文件。
#[derive(Debug)]
pub struct RxTrustLock {
    _file: File,
}

trait LockIo {
    fn set_cloexec(&self, file: &File) -> io::Result<()>;
    fn try_lock(&self, file: &File) -> io::Result<()>;
}

struct RealLockIo;

impl LockIo for RealLockIo {
    fn set_cloexec(&self, file: &File) -> io::Result<()> {
        // SAFETY: file 保持存活，fd 有效；此 fcntl 命令只接整数标志。
        let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
        syscall_result(result)
    }

    fn try_lock(&self, file: &File) -> io::Result<()> {
        // SAFETY: file 保持存活，fd 有效；只尝试非阻塞排他锁。
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        syscall_result(result)
    }
}

fn syscall_result(result: libc::c_int) -> io::Result<()> {
    match result {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

fn retry_interrupted(mut operation: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    loop {
        match operation() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

fn acquire_with_io(dir: &Path, lock_io: &dyn LockIo) -> Result<RxTrustLock, TrustLockError> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join(LOCK_NAME))
        .map_err(TrustLockError::Io)?;
    retry_interrupted(|| lock_io.set_cloexec(&file)).map_err(TrustLockError::Io)?;
    match retry_interrupted(|| lock_io.try_lock(&file)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            let mut text = String::new();
            let _ = (&mut file).take(32).read_to_string(&mut text);
            return Err(TrustLockError::Held {
                pid: text.trim().parse().ok(),
            });
        }
        Err(error) => return Err(TrustLockError::Io(error)),
    }
    // 取锁后才覆盖诊断 PID；失败者绝不能破坏当前持有者的记录。
    file.set_len(0).map_err(TrustLockError::Io)?;
    file.seek(SeekFrom::Start(0)).map_err(TrustLockError::Io)?;
    writeln!(file, "{}", std::process::id()).map_err(TrustLockError::Io)?;
    Ok(RxTrustLock { _file: file })
}

/// daemon 与三个维护子命令共用同一个非阻塞文件锁。
///
/// # Errors
///
/// `Held` 仅表示 EAGAIN/EWOULDBLOCK，调用者退 87；其余系统错误退 1。
pub fn acquire_trust_lock(dir: &Path) -> Result<RxTrustLock, TrustLockError> {
    acquire_with_io(dir, &RealLockIo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    struct ScriptedLockIo {
        cloexec_errno: Cell<Option<i32>>,
        lock_errnos: RefCell<VecDeque<Option<i32>>>,
        cloexec_calls: Cell<usize>,
    }

    impl LockIo for ScriptedLockIo {
        fn set_cloexec(&self, _file: &File) -> io::Result<()> {
            self.cloexec_calls.set(self.cloexec_calls.get() + 1);
            self.cloexec_errno
                .take()
                .map_or(Ok(()), |errno| Err(io::Error::from_raw_os_error(errno)))
        }

        fn try_lock(&self, _file: &File) -> io::Result<()> {
            self.lock_errnos
                .borrow_mut()
                .pop_front()
                .expect("未预期的额外 flock 调用")
                .map_or(Ok(()), |errno| Err(io::Error::from_raw_os_error(errno)))
        }
    }

    fn scripted(errnos: impl IntoIterator<Item = Option<i32>>) -> ScriptedLockIo {
        ScriptedLockIo {
            cloexec_errno: Cell::new(None),
            lock_errnos: RefCell::new(errnos.into_iter().collect()),
            cloexec_calls: Cell::new(0),
        }
    }

    #[test]
    fn lock_retries_eintr_and_preserves_exact_non_contention_errno() {
        for errno in [libc::EIO, libc::EPERM, libc::ENOLCK, libc::ENOSPC] {
            let directory = tempfile::tempdir().unwrap();
            let script = scripted([Some(libc::EINTR), Some(errno)]);
            script.cloexec_errno.set(Some(libc::EINTR));
            let result = acquire_with_io(directory.path(), &script);
            assert!(
                matches!(result, Err(TrustLockError::Io(ref error)) if error.raw_os_error() == Some(errno))
            );
            assert_eq!(script.cloexec_calls.get(), 2);
            assert!(script.lock_errnos.borrow().is_empty());
        }
    }

    #[test]
    fn only_would_block_is_lock_held_and_loser_does_not_overwrite_pid() {
        for errno in [libc::EAGAIN, libc::EWOULDBLOCK] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join(LOCK_NAME);
            std::fs::write(&path, b"12345\n").unwrap();
            let script = scripted([Some(errno)]);
            let result = acquire_with_io(directory.path(), &script);
            assert!(matches!(
                result,
                Err(TrustLockError::Held { pid: Some(12345) })
            ));
            assert_eq!(std::fs::read(path).unwrap(), b"12345\n");
        }
    }

    #[test]
    fn cloexec_failure_is_system_error_without_attempting_flock() {
        let directory = tempfile::tempdir().unwrap();
        let script = scripted([]);
        script.cloexec_errno.set(Some(libc::EPERM));
        let result = acquire_with_io(directory.path(), &script);
        assert!(
            matches!(result, Err(TrustLockError::Io(ref error)) if error.raw_os_error() == Some(libc::EPERM))
        );
        assert_eq!(script.cloexec_calls.get(), 1);
    }

    #[test]
    #[expect(
        clippy::used_underscore_binding,
        reason = "锁文件平时只持有以维持 flock；此测试检查其真实 FD 标志"
    )]
    fn real_lock_is_cloexec_and_releases_without_deleting_stable_inode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let lock = acquire_trust_lock(directory.path()).unwrap();
        let path = directory.path().join(LOCK_NAME);
        let inode = std::fs::metadata(&path).unwrap().ino();
        // SAFETY: lock 持有的有效 fd 在调用全程保持存活。
        let flags = unsafe { libc::fcntl(lock._file.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(matches!(
            acquire_trust_lock(directory.path()),
            Err(TrustLockError::Held { .. })
        ));
        drop(lock);
        let _next = acquire_trust_lock(directory.path()).unwrap();
        assert_eq!(std::fs::metadata(path).unwrap().ino(), inode);
    }
}
