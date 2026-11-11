//! guard **连败预算**的计数与单实例锁（v0.5 候选 E 段(9)，Spec D13 r9 Z1 +
//! r10 AA2/AA3 + r11 AB1）。
//!
//! ## 为什么不用 unit 级 `StartLimit*` 兜住
//!
//! 单次 pre 尝试最坏 ~12 s（Feetech 单包 1 s 超时 × ~12 次串口操作），会滚出
//! unit 的 `StartLimitIntervalSec=60s` 窗口 → 慢失败循环抓不住。故 guard 自管
//! **与墙钟无关**的连续失败计数（unit 级 60s/5 仍保留，管快速崩溃风暴）。
//!
//! ## 硬契约（六条）
//!
//! 1. **路径按实例隔离**：`<runtime_dir>/<robot_id>.count` +
//!    **同目录稳定锁文件** `<robot_id>.lock`（双臂各自预算，互不消耗）；
//! 2. **目录归属**：由 systemd `RuntimeDirectory=robot-system/preflight` +
//!    `RuntimeDirectoryMode=0750` + `RuntimeDirectoryPreserve=yes` 提供
//!    （普通 `User` 无权在 root:root 0755 的 `/run` 自建一级目录）；
//!    count / lock 文件 `0640`，owner = unit `User`（本模块创建时置）；
//! 3. **初态与 canonical 范围**：count 文件 **`NotFound` = 0**（/run 每次开机
//!    必然无文件，属正常初态）；**canonical 内容 = `write_count_atomic` 的
//!    唯一输出形态 `N\n`，N ∈ 0..=5**（有界读 ≤ 3 字节；空白 / 多余换行 /
//!    前导零 / 无换行裸 `0` 一律 malformed，`6\n`..`9\n` 越界）；其他读取
//!    错误 / 非法格式 / 越界 → [`CountError`]（调用方退 86）；
//! 4. **I/O 异常全部 fail-closed**：更新失败 / 清零失败一律退 86，
//!    **禁止默认归零、禁止继续 exec**；
//! 5. **原子更新 + 单实例锁**：`flock(<robot_id>.lock)`——锁文件**永不
//!    rename / 删除**（flock 绑定 inode，若锁在 count 上，rename 换 inode 后
//!    竞争进程可同时拿锁）；持锁期间读 count → 跑 preflight/provision →
//!    tmp+rename **仅**更新 count；**exec 前显式关闭锁 fd**；
//! 6. **计数语义 = 未成功 guard 尝试数**（pre-increment）：递增在任何
//!    preflight / provision 之前，故 guard 在慢 preflight 中 panic / SIGKILL
//!    也保留已递增值。

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

/// 连败预算上限：达到即进入停止态（退 86，需人工恢复）。
pub const MAX_CONSECUTIVE_FAILURES: u8 = 5;
/// count / lock 文件权限（owner = unit `User`）。
const FILE_MODE: u32 = 0o640;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 计数读写错误——**任何一种都必须 fail-closed 退 86**。
#[derive(Debug, Error)]
pub enum CountError {
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// 非法格式（非纯十进制、含多余内容等）。
    #[error("malformed count file {path}: {raw:?}")]
    Malformed { path: PathBuf, raw: String },
    /// 越界（> [`MAX_CONSECUTIVE_FAILURES`]）。
    #[error("count out of range in {path}: {value} > {MAX_CONSECUTIVE_FAILURES}")]
    OutOfRange { path: PathBuf, value: u32 },
}

/// 单实例锁错误。
#[derive(Debug, Error)]
pub enum LockError {
    #[error("open lock file {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// `flock` 非阻塞获取失败 = 同实例并发（另一个 guard 在跑）。
    #[error("lock conflict on {path}: another guard instance holds it")]
    Conflict { path: PathBuf },
}

/// count 文件路径（`<runtime_dir>/<robot_id>.count`）。
#[must_use]
pub fn count_path(runtime_dir: &Path, robot_id: &str) -> PathBuf {
    runtime_dir.join(format!("{robot_id}.count"))
}

/// 稳定锁文件路径（`<runtime_dir>/<robot_id>.lock`；**永不 rename / 删除**）。
#[must_use]
pub fn lock_path(runtime_dir: &Path, robot_id: &str) -> PathBuf {
    runtime_dir.join(format!("{robot_id}.lock"))
}

/// 读上限：canonical 内容恒为 2 字节（`N\n`）；多读 1 字节即可判定"有多余
/// 内容"，同时把损坏 / 恶意大文件挡在有界读之外。
const READ_CAP: usize = 3;

/// 读连败计数。
///
/// **`NotFound` = `Ok(0)`**（/run tmpfs 每次开机的正常初态）。
///
/// **canonical 内容只有 6 种**：`"0\n"` ..= `"5\n"`——即
/// [`write_count_atomic`] 的**唯一**输出形态。空白、多余换行、前导零
/// （`01`）、无换行的裸 `0`、任何前后缀一律 [`CountError::Malformed`]；
/// `6\n`..`9\n` 是**格式合法但越界** → [`CountError::OutOfRange`]。
/// 人工恢复走 `rm` 删文件（`NotFound` = 0），**不是**手改内容，故不放宽。
///
/// # Errors
///
/// 见 [`CountError`]——**全部** fail-closed（调用方退 86）。
pub fn read_count(path: &Path) -> Result<u8, CountError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(source) => {
            return Err(CountError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    // 有界读：最多 READ_CAP 字节，损坏 / 超大文件不进内存。
    let mut raw = Vec::with_capacity(READ_CAP);
    file.take(READ_CAP as u64)
        .read_to_end(&mut raw)
        .map_err(|source| CountError::Read {
            path: path.to_path_buf(),
            source,
        })?;

    let malformed = || CountError::Malformed {
        path: path.to_path_buf(),
        raw: String::from_utf8_lossy(&raw).into_owned(),
    };
    // 恰好 `<digit>\n`：长度、换行位、字符类三项全中才算格式合法。
    let [digit, b'\n'] = raw[..] else {
        return Err(malformed());
    };
    if !digit.is_ascii_digit() {
        return Err(malformed());
    }
    let value = u32::from(digit - b'0');
    if value > u32::from(MAX_CONSECUTIVE_FAILURES) {
        return Err(CountError::OutOfRange {
            path: path.to_path_buf(),
            value,
        });
    }
    // 上界已校验，安全窄化。
    Ok(u8::try_from(value).unwrap_or(MAX_CONSECUTIVE_FAILURES))
}

/// 原子写连败计数（tmp + rename；**只动 `.count`**，绝不碰锁文件）。
///
/// 不做 dir fsync：count 住 `/run` tmpfs，按 `RuntimeDirectory` 契约**不具备
/// 掉电持久性**（r12），fsync 无意义。
///
/// # Errors
///
/// 见 [`CountError::Write`]。
pub fn write_count_atomic(path: &Path, n: u8) -> Result<(), CountError> {
    let wrap = |source: io::Error| CountError::Write {
        path: path.to_path_buf(),
        source,
    };
    let parent = path.parent().ok_or_else(|| {
        wrap(io::Error::new(
            io::ErrorKind::InvalidInput,
            "count path has no parent",
        ))
    })?;
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        "{}.tmp.{}.{seq}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("count"),
        std::process::id()
    ));
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(FILE_MODE)
            .open(&tmp)
            .map_err(wrap)?;
        f.write_all(format!("{n}\n").as_bytes()).map_err(wrap)?;
        f.sync_all().map_err(wrap)?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(wrap(e));
    }
    Ok(())
}

/// 持有中的单实例锁。**exec 前必须显式 [`InstanceLock::close`]**（否则
/// `exec` 后的 `robot-s0` 继承 fd，锁在其整个生命周期内不释放）。
#[derive(Debug)]
pub struct InstanceLock {
    file: Option<File>,
    path: PathBuf,
}

impl InstanceLock {
    /// 显式释放（关闭 fd → flock 自动解除）。
    pub fn close(mut self) {
        self.file.take();
    }

    /// 锁文件路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 取单实例锁（`flock(LOCK_EX | LOCK_NB)`）。
///
/// 锁文件**稳定**：只 create/open，**永不 rename / 删除**（r10 AA2——若把锁
/// 打在 count 上，`rename` 换 inode 后两个进程会同时"持锁"）。fd 置
/// `FD_CLOEXEC`，即便调用方忘了 [`InstanceLock::close`]，`exec` 后也不会
/// 把锁带进 `robot-s0`。
///
/// # Errors
///
/// 见 [`LockError`]；`Conflict` = 同实例并发（调用方退 86）。
pub fn acquire_instance_lock(lock_path: &Path) -> Result<InstanceLock, LockError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(FILE_MODE)
        .open(lock_path)
        .map_err(|source| LockError::Open {
            path: lock_path.to_path_buf(),
            source,
        })?;
    let fd = file.as_raw_fd();
    // SAFETY: `fd` 来自上面刚打开且仍存活的 `File`；两个 fcntl/flock 调用都
    // 只作用于该 fd，不转移所有权。
    unsafe {
        // CLOEXEC 兜底：exec 后不把锁带进 robot-s0。
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
        if libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) != 0 {
            return Err(LockError::Conflict {
                path: lock_path.to_path_buf(),
            });
        }
    }
    Ok(InstanceLock {
        file: Some(file),
        path: lock_path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use tempfile::TempDir;

    fn dir() -> TempDir {
        TempDir::new().expect("tempdir")
    }

    #[test]
    fn paths_are_per_instance() {
        let d = dir();
        let a = count_path(d.path(), "xlerobot_left");
        let b = count_path(d.path(), "xlerobot_right");
        assert_ne!(a, b, "双臂预算必须互不消耗");
        assert_eq!(a.file_name().unwrap(), "xlerobot_left.count");
        assert_eq!(
            lock_path(d.path(), "xlerobot_left").file_name().unwrap(),
            "xlerobot_left.lock"
        );
    }

    /// r10 AA3：NotFound 是**正常初态**（/run 每次开机必然无文件）。
    #[test]
    fn missing_count_is_zero_not_error() {
        let d = dir();
        assert_eq!(
            read_count(&count_path(d.path(), "r")).expect("NotFound = 0"),
            0
        );
    }

    #[test]
    fn round_trip_canonical_range() {
        let d = dir();
        let p = count_path(d.path(), "r");
        for n in 0..=MAX_CONSECUTIVE_FAILURES {
            write_count_atomic(&p, n).expect("write");
            assert_eq!(read_count(&p).expect("read"), n);
        }
        // 权限 0640（owner = unit User）。
        let mode = std::fs::metadata(&p).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            FILE_MODE
        );
    }

    /// canonical 只有 `N\n`（writer 的唯一输出形态）——reviewer 段(9)
    /// round 1 同轮修正：`trim().parse()` 会放行空白 / 多余换行 / 前导零 /
    /// 无换行裸值，与"含多余内容即 malformed"的注释自相矛盾。
    #[test]
    fn only_writer_shaped_content_is_canonical() {
        let d = dir();
        let p = count_path(d.path(), "r");
        for bad in [
            "0",     // 无换行裸值
            "0\n\n", // 多余换行
            " 0\n",  // 前导空白
            "0 \n",  // 尾随空白
            "\n0\n", // 前导换行
            "01\n",  // 前导零（两位）
            "0\r\n", // CRLF
            "0\n0",  // 多余内容
        ] {
            std::fs::write(&p, bad).unwrap();
            let err = read_count(&p).expect_err("{bad:?} 必须 malformed");
            assert!(
                matches!(err, CountError::Malformed { .. }),
                "{bad:?} → {err}"
            );
        }
        // 有界读：超大文件不进内存也必须判 malformed。
        std::fs::write(&p, "0\n".repeat(100_000)).unwrap();
        assert!(matches!(
            read_count(&p).expect_err("超长必须 malformed"),
            CountError::Malformed { .. }
        ));
    }

    #[test]
    fn malformed_and_out_of_range_are_errors() {
        let d = dir();
        let p = count_path(d.path(), "r");
        for bad in ["", "abc", "-1", "1.5", "2 3", "0x2"] {
            std::fs::write(&p, bad).unwrap();
            let err = read_count(&p).expect_err("非法格式必须 fail-closed");
            assert!(
                matches!(err, CountError::Malformed { .. }),
                "{bad:?} → {err}"
            );
        }
        // 格式合法但越界：6..=9 全覆盖（单字符 + 换行仍是 canonical 形态）。
        for n in 6..=9u32 {
            std::fs::write(&p, format!("{n}\n")).unwrap();
            let err = read_count(&p).expect_err("越界必须 fail-closed");
            assert!(
                matches!(err, CountError::OutOfRange { value, .. } if value == n),
                "{n} → {err}"
            );
        }
        // 多位数不再是 canonical 形态 → 归 Malformed（不是 OutOfRange）：
        // 收紧后 OutOfRange 只剩"单字符 6..=9"这一档，两类都 fail-closed。
        std::fs::write(&p, "255\n").unwrap();
        assert!(matches!(
            read_count(&p).expect_err("多位数必须 fail-closed"),
            CountError::Malformed { .. }
        ));
    }

    /// 原子更新只动 `.count`，**不碰锁文件**（r10 AA2 的前提）。
    #[test]
    fn write_does_not_touch_lock_file_inode() {
        let d = dir();
        let cp = count_path(d.path(), "r");
        let lp = lock_path(d.path(), "r");
        let lock = acquire_instance_lock(&lp).expect("lock");
        let ino_before = std::fs::metadata(&lp).unwrap().ino();
        for n in 0..=3 {
            write_count_atomic(&cp, n).expect("write");
        }
        let ino_after = std::fs::metadata(&lp).unwrap().ino();
        assert_eq!(
            ino_before, ino_after,
            "锁文件 inode 必须稳定（永不 rename）"
        );
        // count 自身 inode 会变（tmp+rename），这正是锁不能打在它上面的原因。
        lock.close();
    }

    #[test]
    fn second_acquire_conflicts_while_held_and_succeeds_after_close() {
        let d = dir();
        let lp = lock_path(d.path(), "r");
        let first = acquire_instance_lock(&lp).expect("first");
        let err = acquire_instance_lock(&lp).expect_err("并发必须冲突");
        assert!(matches!(err, LockError::Conflict { .. }), "{err}");
        first.close();
        let again = acquire_instance_lock(&lp).expect("释放后可再取");
        assert_eq!(again.path(), lp);
    }

    /// 反向复现：**若把锁打在 count 上**，`rename` 换 inode 后两个进程能同时
    /// 拿锁——这正是 r10 AA2 撤回 `flock(count)` 的原因。本测试锁住"稳定
    /// 锁文件"这一设计前提。
    #[test]
    fn renaming_target_would_break_flock_exclusivity() {
        let d = dir();
        let victim = d.path().join("victim");
        std::fs::write(&victim, "0\n").unwrap();
        let held = acquire_instance_lock(&victim).expect("hold on victim");
        // 模拟 tmp+rename 换 inode。
        let tmp = d.path().join("victim.tmp");
        std::fs::write(&tmp, "1\n").unwrap();
        std::fs::rename(&tmp, &victim).unwrap();
        // 新 inode 上的锁与旧 fd 无关 → 竞争进程能同时拿到。
        let stolen = acquire_instance_lock(&victim)
            .expect("rename 后新 inode 可再次上锁（正是要避免的语义）");
        stolen.close();
        held.close();
    }
}
