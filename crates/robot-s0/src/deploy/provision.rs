//! **latch provision**（v0.5 候选 E 段(9)，Spec D2 第 1 条 r5-r9 + AC-E21；
//! S48 A3d / W1 接入目标稳定锁与绑定目录 fd）。
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
//! **所有分支收敛后必经**绑定目录 fd 的 fsync（严格：失败 → provision 返回错误、
//! 禁止 exec）。闭掉这条绕过序列：*file fsync + rename 成功 → dir-fsync 失败 →
//! 重试命中"双份存在"→ 上一次未确认的目录持久化被跳过*。目录 fsync 幂等，
//! primary/backup 同 parent 一次即可。
//!
//! ## W1：稳定锁覆盖整个过程，检查与写入相对绑定目录 fd
//!
//! 入口先 [`crate::latch_lock::acquire_latch_lock`]（名称校验 → canonical 父目录
//! → `<name>.lock`），锁在函数返回时随 RAII 释放，guard 因而在 `exec s0_daemon`
//! 之前已不持有它，daemon 再自行获取。持锁后通过
//! [`LatchTargetLock::check_data_objects`] 判定"缺失"：不跟随符号链接、只接受
//! `nlink == 1` 的普通文件，仅真实 `NotFound` 进入三分支，symlink / hardlink /
//! 特殊文件一律拒绝且不自愈。读取与写入（临时文件 → fsync → `renameat` → 目录
//! fsync）全部相对同一目录 fd，持锁后父目录被替换也不会写进新目录。
//!
//! provision **不消费**有效的 Clear pending 标记（daemon 启动恢复才严格处理）。
//!
//! ## 退出码分流（W1）
//!
//! [`ProvisionError::exit_code`]：锁被占用 → 89；名字 / 锁 IO / 对象身份错误 → 1；
//! 原三分支业务失败 → 71（guard 再按连败预算映射 86）。

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use super::guard::PROVISION_FAIL_EXIT_CODE;
use crate::latch_lock::{
    LATCH_LOCK_FAILURE_EXIT_CODE, LATCH_LOCK_HELD_EXIT_CODE, LatchLockError, LatchNameError,
    LatchObject, LatchTargetLock, acquire_latch_lock,
};

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
    /// 统一最终步骤：绑定目录 fsync 失败。
    #[error("fsync parent dir of {path}: {source}")]
    DirFsync {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// W1：latch 名字不合法（配置错误，退 1）。
    #[error("latch name rejected: {0}")]
    InvalidLatchName(#[source] LatchNameError),
    /// W1：目标稳定锁被另一进程持有（退 89）。
    #[error("latch lock held by pid {pid:?}")]
    LatchLockHeld { pid: Option<u32> },
    /// W1：锁 IO / 目录身份 / 现存对象类型或链接数错误（退 1，不自愈）。
    #[error("latch lock or data object check failed: {0}")]
    LatchLock(#[source] LatchLockError),
}

impl ProvisionError {
    /// 进程退出码：锁被占用 89；名字 / 锁 IO / 对象身份 1；三分支业务失败 71。
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::LatchLockHeld { .. } => LATCH_LOCK_HELD_EXIT_CODE,
            Self::InvalidLatchName(_) | Self::LatchLock(_) => LATCH_LOCK_FAILURE_EXIT_CODE,
            Self::UnusableExisting { .. } | Self::Write { .. } | Self::DirFsync { .. } => {
                PROVISION_FAIL_EXIT_CODE
            }
        }
    }
}

impl From<LatchLockError> for ProvisionError {
    fn from(error: LatchLockError) -> Self {
        match error {
            LatchLockError::InvalidName(name) => Self::InvalidLatchName(name),
            LatchLockError::Held { pid } => Self::LatchLockHeld { pid },
            other @ (LatchLockError::Io { .. } | LatchLockError::Identity { .. }) => {
                Self::LatchLock(other)
            }
        }
    }
}

/// provision 的两个可注入 I/O 点（round 1 P1-5；W1 改为相对持有的锁执行）。
///
/// 抽出来只为让 **AC-E21 的"dir-fsync 失败 → 重试命中双份存在仍必须再
/// fsync"序列**和 **backup 写失败 → 重试补齐**可被确定性覆盖：真实现完全
/// 委托 [`LatchTargetLock`] 的绑定 fd 写入，测试实现在真文件系统上按次注入失败，
/// 故重试看到的是**真实盘上状态**而非 mock 编造的状态。
pub trait ProvisionIo {
    /// 严格原子写（tmp → file fsync → `renameat` → 目录 fsync），相对锁绑定的目录。
    ///
    /// # Errors
    ///
    /// 透传底层 I/O 错误。
    fn write_latch(
        &mut self,
        lock: &LatchTargetLock,
        object: LatchObject,
        latched: bool,
    ) -> io::Result<()>;

    /// 统一最终步骤：锁绑定目录的 fsync（严格）。
    ///
    /// # Errors
    ///
    /// 透传底层 I/O 错误。
    fn fsync_directory(&mut self, lock: &LatchTargetLock) -> io::Result<()>;
}

/// 生产实现——运行期 latch 写路径 0 改（严格语义只住 provision 入口）。
#[derive(Debug, Default, Clone, Copy)]
pub struct RealProvisionIo;

impl ProvisionIo for RealProvisionIo {
    fn write_latch(
        &mut self,
        lock: &LatchTargetLock,
        object: LatchObject,
        latched: bool,
    ) -> io::Result<()> {
        lock.write_latch_strict(object, latched)
    }

    fn fsync_directory(&mut self, lock: &LatchTargetLock) -> io::Result<()> {
        lock.sync_directory()
    }
}

/// 持目标稳定锁按三分支 provision latch，并执行统一最终步骤（dir fsync）。
///
/// # Errors
///
/// 见 [`ProvisionError`]——任一种都必须 fail-closed（guard 不得 exec
/// `s0_daemon`）。
pub fn provision_latch(latch_path: &Path) -> Result<ProvisionOutcome, ProvisionError> {
    provision_latch_with(latch_path, &mut RealProvisionIo)
}

/// [`provision_latch`] 的可注入 I/O 版本。锁在本函数内获取、返回时释放，
/// 覆盖整个 read / repair / fsync 过程；不在外层二次取锁。
///
/// # Errors
///
/// 见 [`ProvisionError`]。
pub fn provision_latch_with<I: ProvisionIo + ?Sized>(
    latch_path: &Path,
    io: &mut I,
) -> Result<ProvisionOutcome, ProvisionError> {
    let lock = acquire_latch_lock(latch_path)?;
    let objects = lock.check_data_objects()?;

    let outcome = match (objects.primary.is_some(), objects.backup.is_some()) {
        // ① 双缺失 → 写 `1\n`（latched）双份。
        (false, false) => {
            write_strict(io, &lock, LatchObject::Primary, true)?;
            write_strict(io, &lock, LatchObject::Backup, true)?;
            ProvisionOutcome::WroteBoth
        }
        // ② 单份缺失 → 现存副本必须**严格合法**，只补写缺失那份。
        (true, false) => {
            let value = read_existing(&lock, LatchObject::Primary)?;
            write_strict(io, &lock, LatchObject::Backup, value)?;
            ProvisionOutcome::RepairedMissing
        }
        (false, true) => {
            let value = read_existing(&lock, LatchObject::Backup)?;
            write_strict(io, &lock, LatchObject::Primary, value)?;
            ProvisionOutcome::RepairedMissing
        }
        // ③ 双份都在 → 内容不动。
        (true, true) => ProvisionOutcome::Untouched,
    };

    // 统一最终步骤（r9 Z2）：所有分支收敛后必经，含"双份均存在"路径。
    io.fsync_directory(&lock)
        .map_err(|source| ProvisionError::DirFsync {
            path: lock.target().path_of(LatchObject::Primary),
            source,
        })?;
    Ok(outcome)
}

/// 读现存副本；**损坏 / 不可读 / 值非法一律失败**（不得据此创建另一份）。
fn read_existing(lock: &LatchTargetLock, object: LatchObject) -> Result<bool, ProvisionError> {
    lock.read_latch_value(object)
        .map_err(|source| ProvisionError::UnusableExisting {
            path: lock.target().path_of(object),
            source,
        })
}

fn write_strict<I: ProvisionIo + ?Sized>(
    io: &mut I,
    lock: &LatchTargetLock,
    object: LatchObject,
    latched: bool,
) -> Result<(), ProvisionError> {
    io.write_latch(lock, object, latched)
        .map_err(|source| ProvisionError::Write {
            path: lock.target().path_of(object),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estop_latch::backup_path;
    use std::os::unix::fs::MetadataExt;
    use tempfile::TempDir;

    fn latch_in(dir: &TempDir) -> PathBuf {
        dir.path().join("estop_latch")
    }

    fn read_raw(p: &Path) -> String {
        std::fs::read_to_string(p).expect("read")
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
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
        assert_eq!(
            entries(d.path()),
            ["estop_latch", "estop_latch.bak", "estop_latch.lock"],
            "只应有双份与永不删除的锁文件"
        );
    }

    /// 分支 ②：单份缺失 + 现存合法 → 只补写缺失那份，值 = 现存值，
    /// **不重写**现存副本（inode 不变）。
    #[test]
    fn single_missing_repairs_from_legal_existing() {
        for text in ["0\n", "1\n"] {
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
        }
    }

    /// 分支 ②反例（r6 W2）：现存副本**损坏** → provision 失败（71），且
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
        assert_eq!(err.exit_code(), PROVISION_FAIL_EXIT_CODE);
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

    /// 有效 Clear pending 标记存在时 provision 不消费它：双份存在 → Untouched，
    /// 标记原样保留给 daemon 启动恢复。
    #[test]
    fn pending_marker_is_left_for_daemon_recovery() {
        let d = TempDir::new().unwrap();
        let p = latch_in(&d);
        std::fs::write(&p, "0\n").unwrap();
        std::fs::write(backup_path(&p), "0\n").unwrap();
        let marker = d.path().join("estop_latch.clear_pending");
        std::fs::write(&marker, b"pending-bytes").unwrap();
        assert_eq!(provision_latch(&p).unwrap(), ProvisionOutcome::Untouched);
        assert_eq!(std::fs::read(&marker).unwrap(), b"pending-bytes");
        assert_eq!(read_raw(&p), "0\n");
    }

    /// r9 Z2 统一最终步骤：父目录不存在 → 在任何写入前以锁 IO 错误失败（W1：
    /// 父目录不可 canonicalize 属配置 / 系统错误，退 1）；"双份存在"路径上的
    /// dir fsync 成功侧照常验证。
    #[test]
    fn missing_parent_fails_before_any_write() {
        let d = TempDir::new().unwrap();
        let sub = d.path().join("nested");
        let p = sub.join("estop_latch");
        let err = provision_latch(&p).expect_err("父目录缺失必须失败");
        assert!(
            matches!(
                err,
                ProvisionError::LatchLock(LatchLockError::Io {
                    stage: crate::latch_lock::LockStage::CanonicalizeParent,
                    ..
                })
            ),
            "{err}"
        );
        assert_eq!(err.exit_code(), LATCH_LOCK_FAILURE_EXIT_CODE);
        assert!(!sub.exists(), "不得创建父目录或任何文件");

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
        /// 第 N 次 `fsync_directory` 失败（1-based；0 = 不注入）。
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
        fn write_latch(
            &mut self,
            lock: &LatchTargetLock,
            object: LatchObject,
            latched: bool,
        ) -> io::Result<()> {
            self.writes += 1;
            if self.writes == self.fail_write_nth {
                return Err(io::Error::other("injected write failure"));
            }
            self.inner.write_latch(lock, object, latched)
        }

        fn fsync_directory(&mut self, lock: &LatchTargetLock) -> io::Result<()> {
            self.fsyncs += 1;
            if self.fsyncs == self.fail_fsync_nth {
                return Err(io::Error::other("injected dir fsync failure"));
            }
            self.inner.fsync_directory(lock)
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
        assert_eq!(err.exit_code(), PROVISION_FAIL_EXIT_CODE);
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

    // ── W1：稳定锁、名称与对象身份 ──────────────────────────────────────

    /// 锁被另一持有者占用 → 类型化 `LatchLockHeld`（89），不写任何文件、不改 PID。
    #[test]
    fn held_lock_is_typed_89_and_writes_nothing() {
        let d = TempDir::new().unwrap();
        let p = latch_in(&d);
        let holder = acquire_latch_lock(&p).unwrap();
        let err = provision_latch(&p).expect_err("必须被占用");
        assert!(
            matches!(err, ProvisionError::LatchLockHeld { pid: Some(pid) } if pid == std::process::id()),
            "{err:?}"
        );
        assert_eq!(err.exit_code(), LATCH_LOCK_HELD_EXIT_CODE);
        assert_eq!(entries(d.path()), ["estop_latch.lock"], "不得写双份");
        assert_eq!(
            read_raw(&d.path().join("estop_latch.lock")),
            format!("{}\n", std::process::id())
        );
        drop(holder);
        assert_eq!(provision_latch(&p).unwrap(), ProvisionOutcome::WroteBoth);
    }

    /// 派生名 / RX 固定名 → 类型化 `InvalidLatchName`（1），副作用前拒绝。
    #[test]
    fn invalid_names_are_typed_config_errors_without_side_effects() {
        let d = TempDir::new().unwrap();
        for bad in [
            "estop_latch.bak",
            "estop_latch.lock",
            "estop_latch.clear_pending",
            "estop_latch.tmp.1.2",
            "estop_latch.clear_tmp.1.2",
            "rx_trust_marker",
            "rx_trust",
        ] {
            let err = provision_latch(&d.path().join(bad)).expect_err(bad);
            assert!(
                matches!(err, ProvisionError::InvalidLatchName(_)),
                "{bad}: {err:?}"
            );
            assert_eq!(err.exit_code(), LATCH_LOCK_FAILURE_EXIT_CODE, "{bad}");
        }
        assert!(entries(d.path()).is_empty(), "不得创建任何文件");
    }

    /// 现存 primary 为符号链接、backup 缺失 → 拒绝（1）且不自愈：不创建 backup、
    /// 链接目标字节不变。
    #[test]
    fn symlink_primary_is_rejected_without_self_heal() {
        let d = TempDir::new().unwrap();
        let p = latch_in(&d);
        let real = d.path().join("real_latch");
        std::fs::write(&real, "1\n").unwrap();
        std::os::unix::fs::symlink("real_latch", &p).unwrap();
        let err = provision_latch(&p).expect_err("符号链接必须拒绝");
        assert!(
            matches!(
                err,
                ProvisionError::LatchLock(LatchLockError::Identity { .. })
            ),
            "{err:?}"
        );
        assert_eq!(err.exit_code(), LATCH_LOCK_FAILURE_EXIT_CODE);
        assert!(!backup_path(&p).exists(), "不得补写 backup");
        assert_eq!(read_raw(&real), "1\n");
    }

    /// primary 与 backup 互为硬链接（同一对象）→ 拒绝且不改字节。
    #[test]
    fn hardlinked_pair_is_rejected() {
        let d = TempDir::new().unwrap();
        let p = latch_in(&d);
        std::fs::write(&p, "0\n").unwrap();
        std::fs::hard_link(&p, backup_path(&p)).unwrap();
        let err = provision_latch(&p).expect_err("硬链接必须拒绝");
        assert!(
            matches!(
                err,
                ProvisionError::LatchLock(LatchLockError::Identity { .. })
            ),
            "{err:?}"
        );
        assert_eq!(read_raw(&p), "0\n");
    }

    /// backup 为 FIFO 特殊文件 → 拒绝，primary 不动。
    #[test]
    fn special_file_backup_is_rejected() {
        use std::os::unix::ffi::OsStrExt;
        let d = TempDir::new().unwrap();
        let p = latch_in(&d);
        std::fs::write(&p, "1\n").unwrap();
        let fifo = std::ffi::CString::new(backup_path(&p).as_os_str().as_bytes()).unwrap();
        // SAFETY：路径以 NUL 结尾；mkfifo 只创建命名管道。
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let err = provision_latch(&p).expect_err("特殊文件必须拒绝");
        assert!(
            matches!(
                err,
                ProvisionError::LatchLock(LatchLockError::Identity { .. })
            ),
            "{err:?}"
        );
        assert_eq!(read_raw(&p), "1\n");
    }

    /// 锁覆盖整个 read / repair / fsync：注入 I/O 在每个写入点与最终 fsync 点都
    /// 再次尝试取锁，必须得到 `Held`；provision 返回后锁立即可取。
    struct LockProbeIo {
        inner: RealProvisionIo,
        probes: usize,
    }

    impl ProvisionIo for LockProbeIo {
        fn write_latch(
            &mut self,
            lock: &LatchTargetLock,
            object: LatchObject,
            latched: bool,
        ) -> io::Result<()> {
            self.probe(lock);
            self.inner.write_latch(lock, object, latched)
        }

        fn fsync_directory(&mut self, lock: &LatchTargetLock) -> io::Result<()> {
            self.probe(lock);
            self.inner.fsync_directory(lock)
        }
    }

    impl LockProbeIo {
        fn probe(&mut self, lock: &LatchTargetLock) {
            self.probes += 1;
            let path = lock.target().path_of(LatchObject::Primary);
            let err = acquire_latch_lock(&path).expect_err("过程中锁必须被本次 provision 持有");
            assert!(
                matches!(err, LatchLockError::Held { pid: Some(pid) } if pid == std::process::id()),
                "{err:?}"
            );
        }
    }

    #[test]
    fn lock_covers_whole_process_and_is_released_on_return() {
        let d = TempDir::new().unwrap();
        let p = latch_in(&d);
        let mut io = LockProbeIo {
            inner: RealProvisionIo,
            probes: 0,
        };
        assert_eq!(
            provision_latch_with(&p, &mut io).unwrap(),
            ProvisionOutcome::WroteBoth
        );
        assert_eq!(io.probes, 3, "两次写入 + 一次最终 fsync 都在持锁中");
        // 返回即释放（RAII），guard 因而在 exec 前不再持有它。
        drop(acquire_latch_lock(&p).expect("provision 返回后锁必须已释放"));

        // 失败路径同样释放。
        std::fs::remove_file(backup_path(&p)).unwrap();
        std::fs::write(&p, "garbage").unwrap();
        provision_latch(&p).expect_err("损坏副本");
        drop(acquire_latch_lock(&p).expect("失败返回后锁必须已释放"));
    }

    /// 持锁后父目录被替换：写入只落在打开时的旧目录，新目录不得收到任何字节。
    struct ParentSwapIo {
        inner: RealProvisionIo,
        parent: PathBuf,
        moved_to: PathBuf,
        swapped: bool,
    }

    impl ProvisionIo for ParentSwapIo {
        fn write_latch(
            &mut self,
            lock: &LatchTargetLock,
            object: LatchObject,
            latched: bool,
        ) -> io::Result<()> {
            if !self.swapped {
                std::fs::rename(&self.parent, &self.moved_to).unwrap();
                std::fs::create_dir(&self.parent).unwrap();
                self.swapped = true;
            }
            self.inner.write_latch(lock, object, latched)
        }

        fn fsync_directory(&mut self, lock: &LatchTargetLock) -> io::Result<()> {
            self.inner.fsync_directory(lock)
        }
    }

    #[test]
    fn parent_replacement_after_lock_never_writes_the_new_directory() {
        let d = TempDir::new().unwrap();
        let parent = d.path().join("robot");
        std::fs::create_dir(&parent).unwrap();
        let p = parent.join("estop_latch");
        let mut io = ParentSwapIo {
            inner: RealProvisionIo,
            parent: parent.clone(),
            moved_to: d.path().join("robot.old"),
            swapped: false,
        };
        assert_eq!(
            provision_latch_with(&p, &mut io).unwrap(),
            ProvisionOutcome::WroteBoth
        );
        assert!(io.swapped);
        assert!(entries(&parent).is_empty(), "新目录不得收到任何写入");
        assert_eq!(
            entries(&d.path().join("robot.old")),
            ["estop_latch", "estop_latch.bak", "estop_latch.lock"]
        );
        assert_eq!(read_raw(&d.path().join("robot.old/estop_latch")), "1\n");
    }

    /// 退出码映射：89 / 1 / 71 三类互不混淆。
    #[test]
    fn exit_codes_are_partitioned() {
        let held = ProvisionError::LatchLockHeld { pid: None };
        let name = ProvisionError::InvalidLatchName(LatchNameError::EmptyOrDot);
        let lock = ProvisionError::LatchLock(LatchLockError::Identity {
            stage: crate::latch_lock::LockStage::InspectLock,
            detail: "x",
        });
        let business = [
            ProvisionError::UnusableExisting {
                path: PathBuf::from("p"),
                source: io::Error::other("x"),
            },
            ProvisionError::Write {
                path: PathBuf::from("p"),
                source: io::Error::other("x"),
            },
            ProvisionError::DirFsync {
                path: PathBuf::from("p"),
                source: io::Error::other("x"),
            },
        ];
        assert_eq!(held.exit_code(), 89);
        assert_eq!(name.exit_code(), 1);
        assert_eq!(lock.exit_code(), 1);
        for err in business {
            assert_eq!(err.exit_code(), 71, "{err}");
        }
    }
}
