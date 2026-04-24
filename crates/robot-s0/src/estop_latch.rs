//! `EStop` latch 持久化：S0 启动期读，触发 `EStop` 后写，`ClearEStop` RPC 清。
//!
//! 对应 `docs/00_design.md` §6.6 "`EStop` 不可被软件清除为失忆态"（即：S0 重启不
//! 失忆）+ §6.7 "`EStop` / fault code / 最后一次 safe 输出都 fsync 双写"。
//!
//! ### 文件格式
//!
//! 单行文本：`"1\n"` = latched，`"0\n"` = not latched。文件不存在 = not latched。
//! 其他内容（长度 > 2 / 首字符不是 `0` 或 `1`）= `InvalidData` 错误。
//!
//! ### 原子写
//!
//! `save` 走 tmp file + `fsync` + `rename` + parent dir `fsync` 的 POSIX 套路：
//! 1. 在 parent dir 建 `<name>.tmp.<pid>.<nanos>`
//! 2. 写内容 + `file.sync_all()`（fsync）
//! 3. `rename` 到目标路径（atomic）
//! 4. `open(parent) + sync_all()` 让目录项落盘
//!
//! 中途 crash 时要么看见旧内容要么看见新内容，不会看到部分写入。
//!
//! ### 与 CLI / env 的粘接
//!
//! 默认路径：`/var/lib/robot/<robot_id>/estop_latch`。
//! `s0_daemon --latch-path <abs_path>` 可直接覆盖完整路径，方便测试（给 tmpdir 下的 path）。

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_SUFFIX_SEQ: AtomicU64 = AtomicU64::new(0);

/// 从 `path` 读 `EStop` latch 状态。
///
/// # Errors
///
/// - `io::ErrorKind::InvalidData` 当文件存在但内容非法（非 `0\n` / `1\n`）。
/// - 底层 IO 错误（权限 / 磁盘满等）透传。
///
/// 文件不存在不是错误：返回 `Ok(false)`（S0 首次启动常态）。
pub fn load_estop_latch(path: &Path) -> io::Result<bool> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let mut buf = [0u8; 4];
    let n = file.read(&mut buf)?;
    match &buf[..n] {
        b"0" | b"0\n" => Ok(false),
        b"1" | b"1\n" => Ok(true),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "estop_latch file: expected '0\\n' or '1\\n'",
        )),
    }
}

/// 原子写 latch。即使中途 crash 也不会留下部分写入。
///
/// # Errors
///
/// 底层 IO 错误（权限 / 磁盘满 / 目录不存在等）透传。
pub fn save_estop_latch(path: &Path, latched: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "latch path has no parent"))?;
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "latch path has no file name")
    })?;

    // parent dir 必须存在；不存在就报 NotFound 给调用方（S0 启动期应已创建）。
    if !parent.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("parent dir {} does not exist", parent.display()),
        ));
    }

    // 1) 拼 tmp 路径：<name>.tmp.<pid>.<seq>
    let pid = std::process::id();
    let seq = TMP_SUFFIX_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut tmp_name = OsString::from(file_name);
    tmp_name.push(format!(".tmp.{pid}.{seq}"));
    let tmp_path: PathBuf = parent.join(tmp_name);

    // 2) 写 + fsync
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o640)
            .open(&tmp_path)?;
        f.write_all(if latched { b"1\n" } else { b"0\n" })?;
        f.sync_all()?; // fsync 文件
    }

    // 3) rename（原子）
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        // 失败时尝试清理 tmp（best-effort）
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    // 4) 目录 fsync（让 rename 落盘）
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all(); // best-effort；某些 FS 不支持 dir sync
    }

    Ok(())
}

/// 清除 latch（直接删除文件，等价 `save_estop_latch(path, false)` 但留一个
/// "从未 latch 过" 的语义信号）。
///
/// # Errors
///
/// 底层 IO 错误透传（`NotFound` 视为成功）。
pub fn clear_estop_latch(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn latch_path(dir: &TempDir) -> PathBuf {
        dir.path().join("estop_latch")
    }

    #[test]
    fn load_nonexistent_returns_false() {
        let dir = TempDir::new().unwrap();
        assert!(!load_estop_latch(&latch_path(&dir)).unwrap());
    }

    #[test]
    fn save_then_load_true() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        save_estop_latch(&p, true).unwrap();
        assert!(load_estop_latch(&p).unwrap());
    }

    #[test]
    fn save_then_load_false() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        save_estop_latch(&p, false).unwrap();
        assert!(!load_estop_latch(&p).unwrap());
    }

    #[test]
    fn save_overwrites() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        save_estop_latch(&p, true).unwrap();
        save_estop_latch(&p, false).unwrap();
        assert!(!load_estop_latch(&p).unwrap());
        save_estop_latch(&p, true).unwrap();
        assert!(load_estop_latch(&p).unwrap());
    }

    #[test]
    fn clear_removes_file() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        save_estop_latch(&p, true).unwrap();
        assert!(p.exists());
        clear_estop_latch(&p).unwrap();
        assert!(!p.exists());
        // 重复 clear 不报错
        clear_estop_latch(&p).unwrap();
    }

    #[test]
    fn clear_noop_when_missing() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        clear_estop_latch(&p).unwrap(); // OK
    }

    #[test]
    fn load_corrupted_content_errors() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        std::fs::write(&p, "garbage").unwrap();
        let err = load_estop_latch(&p).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn load_tolerates_no_newline() {
        // "1" 不带 \n 也接受
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        std::fs::write(&p, "1").unwrap();
        assert!(load_estop_latch(&p).unwrap());
        std::fs::write(&p, "0").unwrap();
        assert!(!load_estop_latch(&p).unwrap());
    }

    #[test]
    fn save_to_nonexistent_parent_errors() {
        let dir = TempDir::new().unwrap();
        let bad = dir.path().join("nonexistent_subdir").join("estop_latch");
        let err = save_estop_latch(&bad, true).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn no_tmp_file_leftover_after_success() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        save_estop_latch(&p, true).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "tmp files leaked: {leftovers:?}");
    }
}
