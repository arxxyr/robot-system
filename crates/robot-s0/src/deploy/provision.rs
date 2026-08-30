//! **latch provision**（v0.5 候选 E 段(9)，Spec D2 第 1 条 r5-r9 + AC-E21）。
//!
//! 两个新 arm 身份（`xlerobot_left` / `xlerobot_right`）的 latch 路径是**全新
//! 的**，而 `s0_daemon` 走 strict fail-closed（primary + backup 双缺失即拒
//! 启动）。首次部署必须 provision，但**不能**借换 `robot_id` 丢掉 `EStop` 记忆
//! ——故双缺失时写入 **`1\n`（latched）**，由人显式走 `ClearEStop` control
//! action 才解除。
//!
//! ## 三分支（r6 W2 以**合法性**为前提，非仅存在性）
//!
//! | primary | backup | 动作 |
//! |---|---|---|
//! | 缺失 | 缺失 | 写入 **`1\n` 双份**（latched 首启） |
//! | 单份缺失，现存副本**严格解析为 0/1** | | **只写缺失的那份**（值 = 现存副本值），**不重写**现存副本 |
//! | 单份缺失，现存副本**损坏 / 不可读 / 值非法** | | **provision 失败，且不得创建另一份**（交给限流 → failed，人工介入） |
//! | 存在 | 存在 | **内容不动**（交给 loader 既有语义） |
//!
//! ## 统一最终步骤（r9 Z2）
//!
//! **所有分支收敛后必经** parent-dir open + fsync（严格：任一步失败 →
//! provision 返回错误、禁止 exec）。闭掉这条绕过序列：*file fsync + rename
//! 成功 → dir-fsync 失败 → 重试命中"双份存在"→ 上一次未确认的目录持久化被
//! 跳过*。目录 fsync 幂等，primary/backup 同 parent 一次即可。
//!
//! 写入本身走 [`crate::estop_latch::save_single_strict`]（tmp → file fsync →
//! rename → parent-dir open+fsync，**严格**）——运行期 latch 写路径 0 改。

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::estop_latch::{backup_path, fsync_parent_dir_strict, read_single, save_single_strict};

/// provision 的三种成功形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionOutcome {
    /// 双份都缺失 → 写入 `1\n`（latched）双份。
    WroteBoth,
    /// 单份缺失 → 按现存合法副本的值补写缺失的那份。
    RepairedMissing,
    /// 双份都在 → 内容不动（仍执行统一最终步骤 dir fsync）。
    Untouched,
}

/// provision 失败——**一律禁止启动 S0**。
#[derive(Debug, Error)]
pub enum ProvisionError {
    /// 现存的唯一副本损坏 / 不可读 / 值非法 → **不得**据此创建另一份。
    #[error(
        "existing latch copy {path} is unusable ({source}); refusing to create the missing one"
    )]
    UnusableExisting {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// 写入失败（tmp / fsync / rename / 目录 fsync 任一步）。
    #[error("write latch {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// 统一最终步骤：parent-dir open + fsync 失败。
    #[error("fsync parent dir of {path}: {source}")]
    DirFsync {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// provision 的两个可注入 I/O 点（round 1 P1-5）。
///
/// 抽出来只为让 **AC-E21 的"dir-fsync 失败 → 重试命中双份存在仍必须再
/// fsync"序列**和 **backup 写失败 → 重试补齐**可被确定性覆盖：真实现完全
/// 委托 [`crate::estop_latch`]，测试实现在真文件系统上按次注入失败，故
/// 重试看到的是**真实盘上状态**而非 mock 编造的状态。
pub trait ProvisionIo {
    /// 严格原子写（tmp → file fsync → rename → parent-dir open+fsync）。
    ///
    /// # Errors
    ///
    /// 透传底层 I/O 错误。
    fn write_latch(&mut self, path: &Path, latched: bool) -> io::Result<()>;

    /// 统一最终步骤：parent-dir open + fsync（严格）。
    ///
    /// # Errors
    ///
    /// 透传底层 I/O 错误。
    fn fsync_parent_dir(&mut self, path: &Path) -> io::Result<()>;
}

/// 生产实现——运行期 latch 写路径 0 改（严格语义只住 provision 入口）。
#[derive(Debug, Default, Clone, Copy)]
pub struct RealProvisionIo;

impl ProvisionIo for RealProvisionIo {
    fn write_latch(&mut self, path: &Path, latched: bool) -> io::Result<()> {
        save_single_strict(path, latched)
    }

    fn fsync_parent_dir(&mut self, path: &Path) -> io::Result<()> {
        fsync_parent_dir_strict(path)
    }
}

/// 按三分支 provision latch，并执行统一最终步骤（dir fsync）。
///
/// # Errors
///
/// 见 [`ProvisionError`]——任一种都必须 fail-closed（guard 不得 exec
/// `s0_daemon`）。
pub fn provision_latch(latch_path: &Path) -> Result<ProvisionOutcome, ProvisionError> {
    provision_latch_with(latch_path, &mut RealProvisionIo)
}

/// [`provision_latch`] 的可注入 I/O 版本。
///
/// # Errors
///
/// 见 [`ProvisionError`]。
pub fn provision_latch_with<I: ProvisionIo + ?Sized>(
    latch_path: &Path,
    io: &mut I,
) -> Result<ProvisionOutcome, ProvisionError> {
    let backup = backup_path(latch_path);
    let primary_exists = latch_path.exists();
    let backup_exists = backup.exists();

    let outcome = match (primary_exists, backup_exists) {
        // ① 双缺失 → 写 `1\n`（latched）双份。
        (false, false) => {
            write_strict(io, latch_path, true)?;
            write_strict(io, &backup, true)?;
            ProvisionOutcome::WroteBoth
        }
        // ② 单份缺失 → 现存副本必须**严格合法**，只补写缺失那份。
        (true, false) => {
            let value = read_existing(latch_path)?;
            write_strict(io, &backup, value)?;
            ProvisionOutcome::RepairedMissing
        }
        (false, true) => {
            let value = read_existing(&backup)?;
            write_strict(io, latch_path, value)?;
            ProvisionOutcome::RepairedMissing
        }
        // ③ 双份都在 → 内容不动。
        (true, true) => ProvisionOutcome::Untouched,
    };

    // 统一最终步骤（r9 Z2）：所有分支收敛后必经，含"双份均存在"路径。
    io.fsync_parent_dir(latch_path)
        .map_err(|source| ProvisionError::DirFsync {
            path: latch_path.to_path_buf(),
            source,
        })?;
    Ok(outcome)
}

/// 读现存副本；**损坏 / 不可读 / 值非法一律失败**（不得据此创建另一份）。
fn read_existing(path: &Path) -> Result<bool, ProvisionError> {
    read_single(path).map_err(|source| ProvisionError::UnusableExisting {
        path: path.to_path_buf(),
        source,
    })
}

fn write_strict<I: ProvisionIo + ?Sized>(
    io: &mut I,
    path: &Path,
    latched: bool,
) -> Result<(), ProvisionError> {
    io.write_latch(path, latched)
        .map_err(|source| ProvisionError::Write {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use tempfile::TempDir;

    fn latch_in(dir: &TempDir) -> PathBuf {
        dir.path().join("estop_latch")
    }

    fn read_raw(p: &Path) -> String {
        std::fs::read_to_string(p).expect("read")
    }

    /// 分支 ①：双缺失 → 写入 `1\n` 双份（**latched 首启**，不借换身份丢
    /// `EStop` 记忆）。
    #[test]
    fn both_missing_writes_latched_pair() {
        let d = TempDir::new().unwrap();
        let p = latch_in(&d);
        assert_eq!(
            provision_latch(&p).expect("provision"),
            ProvisionOutcome::WroteBoth
        );
        assert_eq!(read_raw(&p), "1\n");
        assert_eq!(read_raw(&backup_path(&p)), "1\n");
    }

    /// 分支 ②：单份缺失 + 现存合法 → 只补写缺失那份，值 = 现存值，
    /// **不重写**现存副本（inode 不变）。
    #[test]
    fn single_missing_repairs_from_legal_existing() {
        for (value, text) in [(false, "0\n"), (true, "1\n")] {
            // primary 在、backup 缺
            let d = TempDir::new().unwrap();
            let p = latch_in(&d);
            std::fs::write(&p, text).unwrap();
            let ino_before = std::fs::metadata(&p).unwrap().ino();
            assert_eq!(
                provision_latch(&p).expect("provision"),
                ProvisionOutcome::RepairedMissing
            );
            assert_eq!(read_raw(&backup_path(&p)), text, "补写值必须 = 现存值");
            assert_eq!(
                std::fs::metadata(&p).unwrap().ino(),
                ino_before,
                "现存副本不得被重写"
            );

            // backup 在、primary 缺（反方向）
            let d2 = TempDir::new().unwrap();
            let p2 = latch_in(&d2);
            std::fs::write(backup_path(&p2), text).unwrap();
            assert_eq!(
                provision_latch(&p2).expect("provision"),
                ProvisionOutcome::RepairedMissing
            );
            assert_eq!(read_raw(&p2), text);
            let _ = value;
        }
    }

    /// 分支 ②反例（r6 W2）：现存副本**损坏** → provision 失败，且
    /// **不得创建**另一份。两个方向各一。
    #[test]
    fn corrupt_existing_fails_and_creates_nothing() {
        // primary 损坏 + backup 缺失
        let d = TempDir::new().unwrap();
        let p = latch_in(&d);
        std::fs::write(&p, "garbage").unwrap();
        let err = provision_latch(&p).expect_err("损坏副本必须拒绝");
        assert!(
            matches!(err, ProvisionError::UnusableExisting { .. }),
            "{err}"
        );
        assert!(!backup_path(&p).exists(), "不得创建另一份");

        // backup 损坏 + primary 缺失（反方向）
        let d2 = TempDir::new().unwrap();
        let p2 = latch_in(&d2);
        std::fs::write(backup_path(&p2), "2\n").unwrap();
        let err = provision_latch(&p2).expect_err("非法值必须拒绝");
        assert!(
            matches!(err, ProvisionError::UnusableExisting { .. }),
            "{err}"
        );
        assert!(!p2.exists(), "不得创建另一份");
    }

    /// 分支 ③：双份都在 → 内容一字不动（inode 与内容都不变），但**仍执行**
    /// 统一最终步骤。
    #[test]
    fn both_present_is_untouched() {
        let d = TempDir::new().unwrap();
        let p = latch_in(&d);
        std::fs::write(&p, "0\n").unwrap();
        std::fs::write(backup_path(&p), "1\n").unwrap(); // 故意不一致：由 loader 语义消解
        let (i1, i2) = (
            std::fs::metadata(&p).unwrap().ino(),
            std::fs::metadata(backup_path(&p)).unwrap().ino(),
        );
        assert_eq!(
            provision_latch(&p).expect("provision"),
            ProvisionOutcome::Untouched
        );
        assert_eq!(read_raw(&p), "0\n");
        assert_eq!(read_raw(&backup_path(&p)), "1\n");
        assert_eq!(std::fs::metadata(&p).unwrap().ino(), i1);
        assert_eq!(std::fs::metadata(backup_path(&p)).unwrap().ino(), i2);
    }

    /// r9 Z2 统一最终步骤：父目录不可 open（被删）→ **所有分支**都必须失败，
    /// 包括"双份均存在"这条本来不写盘的路径。
    #[test]
    fn dir_fsync_failure_fails_even_on_untouched_branch() {
        let d = TempDir::new().unwrap();
        let sub = d.path().join("nested");
        std::fs::create_dir(&sub).unwrap();
        let p = sub.join("estop_latch");
        std::fs::write(&p, "0\n").unwrap();
        std::fs::write(backup_path(&p), "0\n").unwrap();
        // 删掉父目录内容 + 父目录本身 → open(parent) 必失败。
        std::fs::remove_file(&p).unwrap();
        std::fs::remove_file(backup_path(&p)).unwrap();
        std::fs::remove_dir(&sub).unwrap();
        // 双份此时都不存在 → 走 WroteBoth 分支，但写入就会因父目录缺失失败。
        let err = provision_latch(&p).expect_err("父目录缺失必须失败");
        assert!(
            matches!(
                err,
                ProvisionError::Write { .. } | ProvisionError::DirFsync { .. }
            ),
            "{err}"
        );

        // "双份存在"路径上的 dir fsync：用一个**存在**的父目录验证成功侧，
        // 再删父目录验证失败侧（`Untouched` 分支同样必经最终步骤）。
        let d2 = TempDir::new().unwrap();
        let p2 = latch_in(&d2);
        std::fs::write(&p2, "0\n").unwrap();
        std::fs::write(backup_path(&p2), "0\n").unwrap();
        assert_eq!(
            provision_latch(&p2).expect("父目录在 → 成功"),
            ProvisionOutcome::Untouched
        );
    }

    /// 按次注入失败的 I/O：其余全部委托真实现，故重试看到的是**真实盘上
    /// 状态**（AC-E21 序列测试的前提）。
    struct FlakyIo {
        inner: RealProvisionIo,
        writes: usize,
        fsyncs: usize,
        /// 第 N 次 `write_latch` 失败（1-based；0 = 不注入）。
        fail_write_nth: usize,
        /// 第 N 次 `fsync_parent_dir` 失败（1-based；0 = 不注入）。
        fail_fsync_nth: usize,
    }

    impl FlakyIo {
        fn new() -> Self {
            Self {
                inner: RealProvisionIo,
                writes: 0,
                fsyncs: 0,
                fail_write_nth: 0,
                fail_fsync_nth: 0,
            }
        }
    }

    impl ProvisionIo for FlakyIo {
        fn write_latch(&mut self, path: &Path, latched: bool) -> io::Result<()> {
            self.writes += 1;
            if self.writes == self.fail_write_nth {
                return Err(io::Error::other("injected write failure"));
            }
            self.inner.write_latch(path, latched)
        }

        fn fsync_parent_dir(&mut self, path: &Path) -> io::Result<()> {
            self.fsyncs += 1;
            if self.fsyncs == self.fail_fsync_nth {
                return Err(io::Error::other("injected dir fsync failure"));
            }
            self.inner.fsync_parent_dir(path)
        }
    }

    /// **AC-E21 序列（r9 Z2，round 1 P1-5）**：文件已写完 + rename 成功 →
    /// **最终 dir-fsync 失败** → 重试命中"双份存在"的 `Untouched` 分支 →
    /// **仍必须再执行一次 dir-fsync 并成功**，否则上一次未确认的目录持久化
    /// 被永久跳过。
    #[test]
    fn dir_fsync_failure_then_retry_on_untouched_branch_fsyncs_again() {
        let d = TempDir::new().unwrap();
        let p = latch_in(&d);

        // 第一次：两份都写成，只有最终 dir-fsync 失败。
        let mut io1 = FlakyIo::new();
        io1.fail_fsync_nth = 1;
        let err = provision_latch_with(&p, &mut io1).expect_err("dir-fsync 必须失败");
        assert!(matches!(err, ProvisionError::DirFsync { .. }), "{err}");
        assert_eq!(io1.writes, 2, "两份都应已写盘");
        assert_eq!(io1.fsyncs, 1);
        assert_eq!(read_raw(&p), "1\n");
        assert_eq!(read_raw(&backup_path(&p)), "1\n");

        // 第二次：命中 Untouched，**不写**任何文件，但必须再 fsync 一次。
        let mut io2 = FlakyIo::new();
        assert_eq!(
            provision_latch_with(&p, &mut io2).expect("重试必须成功"),
            ProvisionOutcome::Untouched
        );
        assert_eq!(io2.writes, 0, "Untouched 分支不得写文件");
        assert_eq!(io2.fsyncs, 1, "Untouched 分支仍必须执行统一最终步骤");
    }

    /// **AC-E21 "backup 写失败 → 重试补齐"**：首次 primary 成功、backup 注入
    /// 失败 → provision 失败但 primary 已在盘上；重试走单份缺失分支，按
    /// primary 的值补齐 backup，且**不重写** primary（inode 不变）。
    #[test]
    fn backup_write_failure_is_repaired_on_retry() {
        let d = TempDir::new().unwrap();
        let p = latch_in(&d);

        let mut io1 = FlakyIo::new();
        io1.fail_write_nth = 2; // 第 2 次写 = backup
        let err = provision_latch_with(&p, &mut io1).expect_err("backup 写失败");
        assert!(matches!(err, ProvisionError::Write { .. }), "{err}");
        assert_eq!(read_raw(&p), "1\n", "primary 已落盘");
        assert!(!backup_path(&p).exists(), "backup 未创建");
        assert_eq!(io1.fsyncs, 0, "写失败即返回，不做最终 fsync");
        let ino_before = std::fs::metadata(&p).unwrap().ino();

        let mut io2 = FlakyIo::new();
        assert_eq!(
            provision_latch_with(&p, &mut io2).expect("重试必须补齐"),
            ProvisionOutcome::RepairedMissing
        );
        assert_eq!(read_raw(&backup_path(&p)), "1\n", "补写值 = primary 值");
        assert_eq!(
            std::fs::metadata(&p).unwrap().ino(),
            ino_before,
            "primary 不得被重写"
        );
        assert_eq!(io2.writes, 1, "只补写缺失那份");
        assert_eq!(io2.fsyncs, 1);
    }

    /// 幂等：重复 provision 不改变已就绪的双份。
    #[test]
    fn provision_is_idempotent_after_first_run() {
        let d = TempDir::new().unwrap();
        let p = latch_in(&d);
        assert_eq!(provision_latch(&p).unwrap(), ProvisionOutcome::WroteBoth);
        assert_eq!(provision_latch(&p).unwrap(), ProvisionOutcome::Untouched);
        assert_eq!(provision_latch(&p).unwrap(), ProvisionOutcome::Untouched);
        assert_eq!(read_raw(&p), "1\n");
    }
}
