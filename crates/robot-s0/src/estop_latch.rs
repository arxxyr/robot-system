//! `EStop` latch 持久化：S0 启动期读，触发 `EStop` 后写，`ClearEStop`
//! control action 清（RPC / Web / 本地维护面板都是同一 action 的 transport
//! 入口；详 `docs/CHANGELOG.md` 2026-04-26 "v0.3 entry：`ClearEStop RPC`
//! 文案的控制动作解释"条目）。
//!
//! 对应 `docs/00_design.md` §6.6 "`EStop` 不可被软件清除为失忆态"（即：S0 重启不
//! 失忆）+ §6.7 + `docs/00_design.md:1394` 风险表 "`EStop` latch 文件损坏 →
//! `fsync` **双写 + 备份文件**"。Step 8 落地（commit 待补）。
//!
//! ### 文件格式
//!
//! 单行文本：`"1\n"` = latched，`"0\n"` = not latched。其他内容（长度 > 2 /
//! 首字符不是 `0` 或 `1`）= `InvalidData` 错误。
//!
//! **缺失语义 = strict fail-closed**（reviewer round 4 阻断 2 实施
//! `docs/00_design.md` §6.7 #3，A1-5 撤掉 `--allow-empty-latch` opt-in 后
//! 锁死为唯一行为）：primary + backup 都不存在时 [`load_estop_latch`] 返
//! `io::ErrorKind::NotFound`，调用方拒启动 —— "rm latch && restart" **不**
//! 等价于"清空 EStop"，要么 (a) 首次部署期间运维显式 provision `0\n` 到
//! 两份 latch 文件后再起 daemon（详 `scripts/systemd/README.md`），要么
//! (b) daemon 运行期通过 v0.3 A1 `ClearEStop` control action（gRPC /
//! Web / 本地维护面板都是同一 action 的 transport）真清。
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
//! - **`NotFound`**：尝试读 backup；backup 合法 → 用之 + 把值**回写 primary**
//!   （自愈）；backup 也 `NotFound` → 返 `NotFound` err（fail-closed，
//!   调用方拒启动；A1-5 之前曾走 `MissingLatchPolicy::AllowEmpty` opt-in
//!   返 `Ok(false)`，已撤）
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
//! `robot-s0 --latch-path <abs_path>` 可直接覆盖完整路径，方便测试（给 tmpdir 下的 path）。
//! backup 路径由 [`backup_path`] 派生（追加 `.bak` 后缀），调用方一般无需感知。

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_SUFFIX_SEQ: AtomicU64 = AtomicU64::new(0);

/// 从 primary `path` 读 `EStop` latch 状态；primary 缺失 / 损坏时 fallback
/// 到 `<path>.bak` 并自愈回写 primary。
///
/// **strict fail-closed**（A1-5 后唯一行为）：两份文件都不存在 → 返
/// `io::ErrorKind::NotFound`，调用方应拒启动；运维侧补救路径：
///
/// - **首次部署**：手动 provision `0\n` 到 primary + backup 两个文件后再
///   起 daemon（详 `scripts/systemd/README.md` "首次部署 — latch
///   provision **必读**" 段）。
/// - **运行期**：v0.3 A1 `ClearEStop` control action（gRPC / Web / 本地
///   维护面板都是同一 action 的 transport 入口）真清。
///
/// A1-5 之前曾有 `--allow-empty-latch` opt-in / `MissingLatchPolicy::AllowEmpty`
/// 模式让两份缺失返 `Ok(false)`（首次部署 / bring-up 实验语义），已撤。
///
/// # Errors
///
/// - `io::ErrorKind::NotFound`：primary + backup 都不存在（fail-closed，
///   §6.7 #3 守门）。
/// - `io::ErrorKind::InvalidData`：primary **和** backup 都损坏（系统级
///   故障，调用方应进 Fault 状态）。
/// - 底层 IO 错误（权限 / 磁盘满等）：透传。
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
                        // 两份都不存在 — strict fail-closed（reviewer round 4
                        // 阻断 2 实施 §6.7 #3；A1-5 撤掉 `--allow-empty-latch`
                        // opt-in 后锁死为唯一行为）：返 NotFound err，daemon
                        // 拒启动。"rm latch && restart" 不等价于 "ClearEStop"；
                        // 真清 latch 路径 = (a) 首次部署运维显式 provision
                        // `0\n` 到两份文件后再起 daemon，或 (b) 运行期通过
                        // v0.3 A1 `ClearEStop` control action（gRPC / Web /
                        // 本地维护面板都是同一 action 的 transport）真清。
                        // control action 解释见 docs/CHANGELOG.md 2026-04-26
                        // "v0.3 entry：ClearEStop RPC 文案的控制动作解释" 条目。
                        tracing::error!(
                            primary = %path.display(),
                            backup = %bak.display(),
                            "estop_latch: primary AND backup both missing; \
                             fail-closed (provision `0\\n` to both files for first \
                             deploy, or use ClearEStop control action at runtime)"
                        );
                        Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            "estop_latch: primary AND backup both missing \
                             (fail-closed; first-deploy: provision \"0\\n\" to both \
                             primary + backup before start; runtime clear: ClearEStop \
                             control action)",
                        ))
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
/// **适用场景**：`EStop` 触发（`latched=true` 单调写入）+ S0 启动期 baseline
/// 恢复 + 任何"latched=true 是更保守的状态"的写入。**不**适用于 `ClearEStop`
/// 路径——partial failure 时 primary 已写新值，破坏 §6.7 #4 "S0 重启不失忆"
/// 在 `latched=false` 方向的语义。详 [`save_estop_latch_clear`]。
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

/// 专为 `ClearEStop` 设计的 latch 写入：保证 partial failure 时
/// `docs/00_design.md` §6.7 #4 "S0 重启不失忆" 不被破坏。
///
/// **写入顺序：backup 先写完才写 primary**——这是 [`save_estop_latch`]
/// 的反向。
///
/// ### 为什么 `ClearEStop` 不复用 `save_estop_latch(_, false)`
///
/// [`save_estop_latch`] 是 primary-first 设计。partial failure（primary
/// 写成功 + backup 写失败）后磁盘状态：primary `= "0\n"`，backup `= "1\n"`，
/// 函数返 `Err`。`ClearEStop` 调用方虽然能感知 `Err` 不翻 in-memory guard /
/// 不调 `executor_ctrl.clear_safety_latch()`，但 daemon crash/restart 后
/// [`load_estop_latch`] 默认走 primary，读 `"0\n"` → `latched=false` →
/// **软件 latch 被错误清掉**，违反 A1-2 失败不变量"磁盘 latch 不动"。
///
/// `save_estop_latch_clear` 改成 backup-first 后，所有 partial failure
/// 都安全：
///
/// - **backup 写失败** → primary 没动（仍 `"1\n"`）→ 重启 load primary
///   `latched=true`（安全）。
/// - **backup 成功 + primary 写失败** → primary 没动（rename 原子；仍
///   `"1\n"`）→ 重启 load primary `latched=true`。backup 的 `"0\n"` 不会
///   被 load 读到，因为 [`load_estop_latch`] 默认走 primary，只在 primary
///   `NotFound` / `InvalidData` 时才 fallback backup。
/// - **两步全过** → primary + backup 都 `"0\n"` → 重启 load `latched=false`
///   （正常 `ClearEStop` 成功路径）。
///
/// ### 调用方契约
///
/// 返回 `Err(_)` 后调用方**不得**翻 in-memory `estop_latched` guard、
/// **不得**调用 `executor_ctrl.clear_safety_latch()`，让上层重发 `ClearEStop`
/// 重试。
///
/// # Errors
///
/// backup 或 primary 任一 `save_single_atomic` 失败时透传 `io::Error`。
pub fn save_estop_latch_clear(path: &Path) -> io::Result<()> {
    let bak = backup_path(path);
    save_single_atomic(&bak, false)?;
    save_single_atomic(path, false)?;
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
pub(crate) fn read_single(path: &Path) -> io::Result<bool> {
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

/// 单文件原子写：tmp file + fsync + rename + parent dir fsync
/// （**best-effort** dir sync——运行期路径语义 0 改）。
fn save_single_atomic(path: &Path, latched: bool) -> io::Result<()> {
    save_single_inner(path, latched, false)
}

fn save_single_inner(path: &Path, latched: bool, strict_dir_sync: bool) -> io::Result<()> {
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

    if strict_dir_sync {
        // provision 入口：目录持久化失败必须拒启动（掉电保证仅对 provision
        // 宣称）。
        let dir = File::open(parent)?;
        dir.sync_all()?;
    } else if let Ok(dir) = File::open(parent) {
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

    /// **reviewer round 4 阻断 2 实施**（A1-5 锁死为唯一行为）：latch 缺失
    /// 必须 fail-closed（NotFound err），daemon 拒启动。err 文案必须给运
    /// 维明确补救路径（首次部署手动 provision 或运行期 `ClearEStop`
    /// control action），跟 `scripts/systemd/README.md` 的 "首次部署 — latch
    /// provision **必读**" 段语义对齐。
    #[test]
    fn load_nonexistent_returns_not_found_err() {
        let dir = TempDir::new().unwrap();
        let err = load_estop_latch(&latch_path(&dir)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(
            err.to_string().contains("ClearEStop control action"),
            "err message must hint at runtime ClearEStop control action: {err}"
        );
        assert!(
            err.to_string().contains("first-deploy"),
            "err message must hint at first-deploy provision path: {err}"
        );
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
    fn load_returns_not_found_when_both_missing() {
        // **reviewer round 4 阻断 2 实施**（A1-5 锁死为唯一行为）：primary
        // + backup 都不存在 → fail-closed NotFound err，不再返 Ok(false)。
        // 旧 `MissingLatchPolicy::AllowEmpty` opt-in 路径 A1-5 撤掉。
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        let err = load_estop_latch(&p).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
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

    // ============================================================================
    // save_estop_latch_clear（A1-2 reviewer round 2 — backup-first 失败原子性）
    // ============================================================================

    #[test]
    fn save_estop_latch_clear_writes_both_files() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        let bak = backup_path(&p);
        // 预置 latched=true（模拟 EStop 已落 latch）
        save_estop_latch(&p, true).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"1\n");
        assert_eq!(std::fs::read(&bak).unwrap(), b"1\n");
        // ClearEStop 路径：backup-first 写入 latch=false
        save_estop_latch_clear(&p).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"0\n");
        assert_eq!(std::fs::read(&bak).unwrap(), b"0\n");
        assert!(!load_estop_latch(&p).unwrap());
    }

    /// **A1-2 reviewer round 2 partial failure 锁测**：backup-first 写入
    /// 顺序保证 backup 写失败时 primary 不动；重启 [`load_estop_latch`]
    /// 仍读到 `latched=true`，不破坏 `docs/00_design.md` §6.7 #4 "S0 重启
    /// 不失忆" + A1-2 失败不变量"磁盘 latch 不动"。
    ///
    /// 制造 backup 写失败的方法：把 `<latch>.bak` 替换成非空目录，
    /// `save_single_atomic` 内部 `rename(tmp, bak_path)` 在 Linux 会因
    /// `EISDIR` / `ENOTEMPTY` 失败。
    ///
    /// 旧实现 `save_estop_latch(_, false)`（primary-first）在这条路径会
    /// 暴露安全 bug：primary 已写 `"0\n"`，bak 失败返 Err，但 daemon
    /// crash/restart 后 [`load_estop_latch`] 走 primary 读 `"0\n"` →
    /// `latched=false` → 软件 latch 被错误清掉。本测试钉死该 bug 不再
    /// 出现。
    #[test]
    fn save_estop_latch_clear_backup_failure_preserves_primary_latched() {
        let dir = TempDir::new().unwrap();
        let p = latch_path(&dir);
        let bak = backup_path(&p);

        // 1) 预置 primary + bak 都 latched=true
        save_estop_latch(&p, true).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"1\n");
        assert_eq!(std::fs::read(&bak).unwrap(), b"1\n");

        // 2) 把 bak 替换成非空目录（rename target 是非空目录 → 失败）
        std::fs::remove_file(&bak).unwrap();
        std::fs::create_dir(&bak).unwrap();
        std::fs::write(bak.join("nonempty"), b"x").unwrap();

        // 3) save_estop_latch_clear 必须返 Err（backup 写失败）
        let res = save_estop_latch_clear(&p);
        assert!(
            res.is_err(),
            "save_estop_latch_clear 应返 Err（backup rename target is non-empty dir）"
        );

        // 4) primary 必须保持 "1\n"（backup-first 顺序保证 partial failure 不动 primary）
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"1\n",
            "primary 必须保持 latched=true；backup-first 顺序：backup 失败前 primary 没动"
        );

        // 5) **核心断言**：重启 load 仍读到 latched=true（§6.7 #4 不失忆）
        assert!(
            load_estop_latch(&p).unwrap(),
            "daemon crash/restart 后 load_estop_latch 必须仍返 latched=true"
        );
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
