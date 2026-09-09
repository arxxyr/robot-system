//! 绑定目录 fd 的文件操作：所有读写、创建、rename、unlink 与目录同步都相对
//! 同一个已打开的目录进行，不按路径重开父目录。父目录被改名或原路径被
//! 替换后，操作对象仍是打开时的那个目录。

use std::ffi::{CString, OsStr, OsString};
use std::fs::{File, FileType, Metadata, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use super::credential::FileIdentity;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 已打开的目标目录及其身份。
#[derive(Debug)]
pub(super) struct DirHandle {
    file: File,
    identity: FileIdentity,
}

impl DirHandle {
    /// 以 `O_DIRECTORY | O_CLOEXEC` 打开规范父目录并采样其 `(dev, ino)`。
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "latch 父目录必须是目录",
            ));
        }
        Ok(Self {
            identity: FileIdentity::of(&metadata),
            file,
        })
    }

    pub(super) const fn identity(&self) -> FileIdentity {
        self.identity
    }

    /// 同步目录项；这是唯一的目录同步入口。
    pub(super) fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    /// 相对本目录打开已存在的条目；`flags` 不含 `O_CREAT`。
    pub(super) fn open_at(&self, name: &OsStr, flags: libc::c_int) -> io::Result<File> {
        let name = c_name(name)?;
        let fd = retry_eintr(|| {
            // SAFETY：目录 fd 在 self 存活期间有效；name 是有效的 NUL 结尾字符串；
            // 不传 O_CREAT，因此不需要 mode 参数。
            unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    flags | libc::O_CLOEXEC,
                )
            }
        })?;
        // SAFETY：fd 由 openat 刚返回且只在此处转移所有权。
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// 相对本目录排他创建普通文件（`O_CREAT | O_EXCL | O_CLOEXEC`）。
    pub(super) fn create_new_at(
        &self,
        name: &OsStr,
        flags: libc::c_int,
        mode: libc::c_uint,
    ) -> io::Result<File> {
        let name = c_name(name)?;
        let fd = retry_eintr(|| {
            // SAFETY：目录 fd 有效；name 以 NUL 结尾；O_CREAT 时 openat 读取一个
            // mode_t 可变参数，这里显式传入 c_uint。
            unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    flags | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
                    mode,
                )
            }
        })?;
        // SAFETY：fd 由 openat 刚返回且只在此处转移所有权。
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// 相对本目录删除条目；不删除目录。
    pub(super) fn unlink_at(&self, name: &OsStr) -> io::Result<()> {
        let name = c_name(name)?;
        retry_eintr(|| {
            // SAFETY：目录 fd 有效；name 以 NUL 结尾；flags 为 0。
            unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) }
        })
        .map(|_| ())
    }

    /// 同一目录内原子改名。
    pub(super) fn rename_at(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        let from = c_name(from)?;
        let to = c_name(to)?;
        retry_eintr(|| {
            // SAFETY：两个目录 fd 参数都是同一个有效 fd；路径均以 NUL 结尾。
            unsafe {
                libc::renameat(
                    self.file.as_raw_fd(),
                    from.as_ptr(),
                    self.file.as_raw_fd(),
                    to.as_ptr(),
                )
            }
        })
        .map(|_| ())
    }

    /// 不跟随符号链接地采样条目的身份与类型（`O_PATH | O_NOFOLLOW`）。
    pub(super) fn identity_at_nofollow(
        &self,
        name: &OsStr,
    ) -> io::Result<(FileIdentity, FileType)> {
        let file = self.open_at(name, libc::O_PATH | libc::O_NOFOLLOW)?;
        let metadata: Metadata = file.metadata()?;
        Ok((FileIdentity::of(&metadata), metadata.file_type()))
    }
}

fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "文件名含 NUL"))
}

/// 只对系统调用的 `EINTR` 重试；守门中断不经过这里。
fn retry_eintr(mut call: impl FnMut() -> libc::c_int) -> io::Result<libc::c_int> {
    loop {
        let result = call();
        if result >= 0 {
            return Ok(result);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

/// 临时 latch 文件：常规错误时经同一目录 fd 清理；进程中断留下的临时文件不
/// 参与 latch 恢复。
pub(super) struct TemporaryLatch<'a> {
    dir: &'a DirHandle,
    pub(super) name: OsString,
}

impl<'a> TemporaryLatch<'a> {
    pub(super) fn create(dir: &'a DirHandle, target: &OsStr) -> io::Result<(Self, File)> {
        Self::create_with_sequence(dir, target, &TEMP_SEQUENCE)
    }

    /// 撞上残留文件只更换序号，最多 64 次；耗尽或其他错误直接返回。
    pub(super) fn create_with_sequence(
        dir: &'a DirHandle,
        target: &OsStr,
        sequence: &AtomicU64,
    ) -> io::Result<(Self, File)> {
        for _ in 0..64 {
            let next = sequence
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    value.checked_add(1)
                })
                .map_err(|_| io::Error::other("Clear 临时文件序号耗尽"))?;
            let mut name = target.to_os_string();
            name.push(format!(".clear_tmp.{}.{next}", std::process::id()));
            match dir.create_new_at(&name, libc::O_WRONLY, 0o640) {
                Ok(file) => return Ok((Self { dir, name }, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Clear 临时文件连续 64 个候选名已存在",
        ))
    }
}

impl Drop for TemporaryLatch<'_> {
    fn drop(&mut self) {
        let _ = self.dir.unlink_at(&self.name);
    }
}
