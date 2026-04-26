//! `EStop` latch 持久化：S0 启动期读，触发 `EStop` 后写，`ClearEStop` RPC 清。
//!
//! 对应 `docs/00_design.md` §6.6 "`EStop` 不可被软件清除为失忆态"（即：S0 重启不
//! 失忆）+ §6.7 + `docs/00_design.md:1394` 风险表 "`EStop` latch 文件损坏 →
//! `fsync` **双写 + 备份文件**"。Step 8 落地（commit 待补）。
//!
//! ### 文件格式
//!
//! 单行文本：`"1\n"` = latched，`"0\n"` = not latched。文件不存在 = not latched。
//! 其他内容（长度 > 2 / 首字符不是 `0` 或 `1`）= `InvalidData` 错误。
//!
//! ### 原子写（**双文件**）
//!
//! 每次 [`save_estop_latch`] 写两份独立文件：
//!
//! - **primary**：调用方传入 `<latch_path>`（如 `/var/lib/robot/<id>/estop_latch`）
//! - **backup**：派生 `<latch_path>.bak`（如 `/var/lib/robot/<id>/estop_latch.bak`）
//!
//! 每份独立走 tmp file + `fsync` + `rename` + parent dir `fsync` 的 POSIX 套路：
//!
//! 1. 在 parent dir 建 `<name>.tmp.<pid>.<seq>`
//! 2. 写内容 + `file.sync_all()`（fsync）
//! 3. `rename` 到目标路径（atomic）
//! 4. `open(parent) + sync_all()` 让目录项落盘
//!
//! **写入顺序：primary 先写完才写 backup**。crash 在两步之间时 primary 必然
//! 是新值（rename 原子）—— 重启 [`load_estop_latch`] 读 primary 拿到新值，不
//! 丢更新。
//!
//! ### 读 fallback 自愈
//!
//! [`load_estop_latch`] 读 primary：
//!
//! - 合法 `0\n` / `1\n` → 直接用
//! - **`NotFound`**：尝试读 backup；backup 也 `NotFound` → 返回 `false`；
//!   backup 合法 → 用之 + 把值**回写 primary**（自愈）
//! - **`InvalidData`**（文件损坏）：尝试读 backup；backup 合法 → 用之 + 回写
//!   primary（自愈）；backup 也损坏 → 返回 `InvalidData`（系统级故障，调用方
//!   应进 Fault 而非默认 not-latched）
//! - 其他 IO 错误（权限 / 磁盘满）：透传
//!
//! 自愈写回**只在内容确实可解析时执行**，避免把 backup 的损坏复制给 primary。
//!
//! ### 与 CLI / env 的粘接
//!
//! 默认路径：`/var/lib/robot/<robot_id>/estop_latch`。
//! `s0_daemon --latch-path <abs_path>` 可直接覆盖完整路径，方便测试（给 tmpdir 下的 path）。
//! backup 路径由 [`backup_path`] 派生（追加 `.bak` 后缀），调用方一般无需感知。

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_SUFFIX_SEQ: AtomicU64 = AtomicU64::new(0);

/// 从 primary `path` 读 `EStop` latch 状态；primary 缺失 / 损坏时 fallback 到
/// `<path>.bak` 并自愈回写 primary。
///
/// # Errors
///
/// - `io::ErrorKind::InvalidData`：primary **和** backup 都损坏（系统级故障，
///   调用方应进 Fault 状态）
/// - 底层 IO 错误（权限 / 磁盘满等）：透传
///
/// 两份文件都不存在：返回 `Ok(false)`（S0 首次启动常态）。
pub fn load_estop_latch(path: &Path) -> io::Result<bool> {
    let bak = backup_path(path);
    match read_single(path) {
        Ok(v) => Ok(v),
        Err(e_primary) => match e_primary.kind() {
            io::ErrorKind::NotFound => {
                // primary 不存在：试 backup
                match read_single(&bak) {
                    Ok(v) => {
                        tracing::warn!(
                            primary = %path.display(),
                            backup = %bak.display(),
                            latched = v,
                            "estop_latch: primary missing, restored from backup"
                        );
                        // 自愈：把 backup 的合法值回写 primary（best-effort，不报错）
                        let _ = save_single_atomic(path, v);
                        Ok(v)
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        // 两份都不存在 = S0 第一次启动 / 全新部署
                        Ok(false)
                    }
                    Err(e) => Err(e),
                }
            }
            io::ErrorKind::InvalidData => {
                // primary 文件损坏：试 backup
                match read_single(&bak) {
                    Ok(v) => {
                        tracing::warn!(
                            primary = %path.display(),
                            backup = %bak.display(),
                            latched = v,
                            "estop_latch: primary corrupted, restored from backup"
                        );
                        let _ = save_single_atomic(path, v);
                        Ok(v)
                    }
                    Err(e_bak) => {
                        tracing::error!(
                            err_primary = %e_primary,
                            err_backup = %e_bak,
                            "estop_latch: primary corrupted, backup also bad; both unsalvageable"
                        );
                        // 同时坏 → 返回 InvalidData，调用方按 Fault 处理（**不**默认 false）
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "estop_latch: primary AND backup both corrupted/unreadable",
                        ))
                    }
                }
            }
            _ => Err(e_primary),
        },
    }
}

/// 原子写 latch（双文件）。先写 primary 再写 backup —— 中途 crash 时 primary
/// 必然是新值（rename 原子）。
///
/// # Errors
///
/// primary 写入失败：直接返回错误（**不**回退到只写 backup —— 保证调用方知道
/// "至少 primary 一份已落盘"的语义）。backup 写入失败：返回错误，但 primary
/// 已成功（下次 load 仍能正确读到）。
pub fn save_estop_latch(path: &Path, latched: bool) -> io::Result<()> {
    save_single_atomic(path, latched)?;
    let bak = backup_path(path);
    save_single_atomic(&bak, latched)?;
    Ok(())
}

/// 清除 latch（删除 primary **和** backup）。两份都不存在视为成功。
///
/// # Errors
///
/// 任一文件删除失败（NotFound 除外）→ 透传。
pub fn clear_estop_latch(path: &Path) -> io::Result<()> {
    let bak = backup_path(path);
    let r1 = match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    };
    let r2 = match std::fs::remove_file(&bak) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    };
    r1?;
    r2
}

/// 派生 backup 路径：把 primary 文件名追加 `.bak` 后缀，放在 primary 同目录。
///
/// 例：`/var/lib/robot/xlerobot/estop_latch` → `/var/lib/robot/xlerobot/estop_latch.bak`
#[must_use]
pub fn backup_path(primary: &Path) -> PathBuf {
    let parent = primary.parent().unwrap_or_else(|| Path::new("."));
    let mut name = primary
        .file_name()
        .map_or_else(|| OsString::from("estop_latch"), OsString::from);
    name.push(".bak");
    parent.join(name)
}

// ============================================================================
// 内部：单文件读 / 写
// ============================================================================

/// 读单个 latch 文件（不做 backup fallback）。
///
/// # Errors
/// - `NotFound`：文件不存在
/// - `InvalidData`：内容非 `0` / `1` / `0\n` / `1\n`
fn read_single(path: &Path) -> io::Result<bool> {
    let mut file = File::open(path)?;
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

/// 单文件原子写：tmp file + fsync + rename + parent dir fsync。
fn save_single_atomic(path: &Path, latched: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "latch path has no parent"))?;
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "latch path has no file name")
    })?;

    if !parent.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("parent dir {} does not exist", parent.display()),
        ));
    }

    let pid = std::process::id();
    let seq = TMP_SUFFIX_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut tmp_name = OsString::from(file_name);
    tmp_name.push(format!(".tmp.{pid}.{seq}"));
    let tmp_path: PathBuf = parent.join(tmp_name);

    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o640)
            .open(&tmp_path)?;
        f.write_all(if latched { b"1\n" } else { b"0\n" })?;
        f.sync_all()?;
    }

    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all(); // best-effort；某些 FS 不支持 dir sync
    }

    Ok(())
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

    // ============================================================================
    // 双文件（Step 8）
    // ============================================================================

    #[test]
    fn backup_path_appends_bak_suffix() {
        assert_eq!(
            backup_path(Path::new("/var/lib/robot/x/estop_latch")),
            PathBuf::from("/var/lib/robot/x/estop_latch.bak")
        );
        // 无父目录的 relative path：parent 是空字符串（不是 None），join 时
        // 当前目录隐式生效；结果仍是相对路径 "estop_latch.bak"。
        assert_eq!(
            backup_path(Path::new("estop_latch")),
            PathBuf::from("estop_latch.bak")
        );
    }

    #[test]
    fn save_writes_both_primary_and_backup() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        save_estop_latch(&p, true).unwrap();
        assert!(p.exists(), "primary missing");
        let bak = backup_path(&p);
        assert!(bak.exists(), "backup missing");
        // 两份内容一致
        assert_eq!(std::fs::read(&p).unwrap(), b"1\n");
        assert_eq!(std::fs::read(&bak).unwrap(), b"1\n");
    }

    #[test]
    fn load_falls_back_to_backup_when_primary_missing() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        save_estop_latch(&p, true).unwrap();
        // 模拟 primary 丢失（只剩 backup）
        std::fs::remove_file(&p).unwrap();
        assert!(!p.exists());
        // load 应该从 backup 拿到 true，并自愈回写 primary
        assert!(load_estop_latch(&p).unwrap());
        assert!(p.exists(), "primary should be self-healed from backup");
        assert_eq!(std::fs::read(&p).unwrap(), b"1\n");
    }

    #[test]
    fn load_falls_back_to_backup_when_primary_corrupted() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        save_estop_latch(&p, true).unwrap();
        // 模拟 primary 损坏
        std::fs::write(&p, b"garbage").unwrap();
        // load 应该从 backup 拿到 true，并自愈回写 primary
        assert!(load_estop_latch(&p).unwrap());
        assert_eq!(std::fs::read(&p).unwrap(), b"1\n");
    }

    #[test]
    fn load_returns_false_when_both_missing() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        // primary + backup 都不存在 = 全新部署，return false
        assert!(!load_estop_latch(&p).unwrap());
    }

    #[test]
    fn load_errors_when_both_corrupted() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        let bak = backup_path(&p);
        std::fs::write(&p, b"garbage1").unwrap();
        std::fs::write(&bak, b"garbage2").unwrap();
        let err = load_estop_latch(&p).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn load_uses_primary_when_backup_corrupted() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        save_estop_latch(&p, true).unwrap();
        // backup 损坏，但 primary 还好 —— 走 primary 路径
        let bak = backup_path(&p);
        std::fs::write(&bak, b"garbage").unwrap();
        assert!(load_estop_latch(&p).unwrap());
    }

    #[test]
    fn clear_removes_both_files() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        save_estop_latch(&p, true).unwrap();
        let bak = backup_path(&p);
        assert!(p.exists() && bak.exists());
        clear_estop_latch(&p).unwrap();
        assert!(!p.exists());
        assert!(!bak.exists());
    }

    #[test]
    fn clear_partial_state_ok() {
        // 只有 primary 存在（backup 已被删）→ clear 仍 OK
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        save_estop_latch(&p, true).unwrap();
        let bak = backup_path(&p);
        std::fs::remove_file(&bak).unwrap();
        clear_estop_latch(&p).unwrap();
        assert!(!p.exists());
    }

    #[test]
    fn save_overwrites_both_files() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        let bak = backup_path(&p);
        save_estop_latch(&p, true).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"1\n");
        assert_eq!(std::fs::read(&bak).unwrap(), b"1\n");
        save_estop_latch(&p, false).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"0\n");
        assert_eq!(std::fs::read(&bak).unwrap(), b"0\n");
    }

    #[test]
    fn load_partial_corruption_self_heals() {
        // 模拟"上次 save 写完 primary 但 backup 写一半 crash"
        // 重启后 load 走 primary（仍合法），不动 backup（broken）
        // 注意：当前实现 load 不主动检查 backup 是否对齐 primary —— 这是
        // 设计选择（primary 是权威），仅在 primary 失败时回 backup
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        save_estop_latch(&p, true).unwrap();
        // backup 损坏
        let bak = backup_path(&p);
        std::fs::write(&bak, b"corrupt").unwrap();
        // load primary 仍正确
        assert!(load_estop_latch(&p).unwrap());
        // backup 没有被自愈（设计选择：primary 权威，不主动修 backup；
        // 下次 save_estop_latch 调用会重写 backup）
        assert_eq!(std::fs::read(&bak).unwrap(), b"corrupt");
    }

    #[test]
    fn restart_persistence_roundtrip() {
        // 模拟 S0 restart：save → 重新 load 得到一样的值
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        save_estop_latch(&p, true).unwrap();
        // 模拟进程重启：再读一次（绕过任何 in-memory 状态）
        assert!(load_estop_latch(&p).unwrap());
        save_estop_latch(&p, false).unwrap();
        assert!(!load_estop_latch(&p).unwrap());
        save_estop_latch(&p, true).unwrap();
        assert!(load_estop_latch(&p).unwrap());
    }
}
