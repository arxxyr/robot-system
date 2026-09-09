//! S48 A3d / W1：latch 目标的稳定锁、共享名称校验与绑定目录 fd 的数据对象检查。
//!
//! 一个 latch 目标由 canonical 父目录加 primary 文件名构成；派生文件 `<name>.bak`、
//! `<name>.clear_pending`、`<name>.lock` 与两种临时家族 `<name>.tmp.<pid>.<seq>`、
//! `<name>.clear_tmp.<pid>.<seq>` 都在同一父目录。[`validate_latch_name`] 只按
//! `OsStr` 原始字节拒绝会与这些派生名重合的 primary 名字，使得同目录内合法
//! primary 与全部派生名互不相交：任一派生名相等只能来自同一 primary。
//!
//! 稳定锁 `<name>.lock` 永不 rename / unlink，按绑定的 canonical 目录 fd 相对打开，
//! 先核普通文件且 `nlink == 1`，再非阻塞 `flock`；只有持锁者才改写诊断 PID，失败
//! 竞争者不改变任何字节。锁的活 `File` 是唯一所有者（RAII，不可 `Clone`），
//! `drop` 即释放；fd 带 `O_CLOEXEC`，不跨 `exec` 继承。
//!
//! 持锁后对现存 primary / backup 的检查、读取与 provision 写入全部相对同一目录 fd
//! 进行：不跟随符号链接、只接受 `nlink == 1` 的普通文件，仅真实 `NotFound` 进入
//! 各入口原有的缺失规则。父目录在持锁后被替换时，写入仍落在打开时的那个目录，
//! 新目录不会收到任何字节；daemon 侧再以 W0 store 的目录身份与锁比对拒绝启动。
//!
//! 本模块不复制 W0 Clear 事务算法，也不承担监督、writer 或权限接线。

use std::ffi::{CString, OsStr, OsString};
use std::fmt;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

use crate::clear_estop_transaction::FileIdentity;
use crate::rx_trust::PRIMARY_NAME as RX_TRUST_PRIMARY_NAME;

/// latch 稳定锁被另一进程持有时的退出码；systemd unit 禁止按此码重启。
pub const LATCH_LOCK_HELD_EXIT_CODE: u8 = 89;
/// latch 名字、锁 IO 或对象身份错误的退出码（配置 / 系统错误，不伪称被占用）。
pub const LATCH_LOCK_FAILURE_EXIT_CODE: u8 = 1;

const BACKUP_SUFFIX: &str = ".bak";
const MARKER_SUFFIX: &str = ".clear_pending";
const LOCK_SUFFIX: &str = ".lock";
/// 旧 `estop_latch` 写路径的临时家族标签（含首尾分隔符）。
const LEGACY_TEMP_TAG: &[u8] = b".tmp.";
/// W0 Clear 事务的临时家族标签（含首尾分隔符）。
const CLEAR_TEMP_TAG: &[u8] = b".clear_tmp.";
/// RX 信任稳定锁 `rx_trust.lock` 的主干名；它本身不可作为 latch primary。
const RX_TRUST_LOCK_STEM: &[u8] = b"rx_trust";
/// 锁文件权限；只有持锁者写 PID。
const LOCK_MODE: libc::c_uint = 0o600;
/// provision 写入的 latch 文件权限，与 `estop_latch` 写路径一致。
const LATCH_MODE: libc::c_uint = 0o640;
/// 临时文件连续候选名上限。
const TEMP_ATTEMPTS: u32 = 64;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// primary 名字被拒绝的原因；调用方应视为配置错误（退出码 1），不是被占用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LatchNameError {
    /// 空名、`.` 或 `..`。
    #[error("latch 名字为空或为 `.`/`..`")]
    EmptyOrDot,
    /// 以派生文件保留尾缀结尾。
    #[error("latch 名字以保留尾缀 {0} 结尾")]
    ReservedSuffix(&'static str),
    /// 匹配完整临时文件家族（两个数字段非空、只含 ASCII 数字，不按可否解析判定）。
    #[error("latch 名字匹配临时文件家族 {0}")]
    TemporaryFamily(&'static str),
    /// 与固定 RX 信任资源重合。
    #[error("latch 名字与 RX 信任资源 {0} 重合")]
    RxTrustResource(&'static str),
}

/// 按 `OsStr` 原始字节校验 primary 名字；不依赖 UTF-8，不做任何文件操作。
///
/// # Errors
///
/// 见 [`LatchNameError`]。
pub fn validate_latch_name(name: &OsStr) -> Result<(), LatchNameError> {
    let bytes = name.as_encoded_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." {
        return Err(LatchNameError::EmptyOrDot);
    }
    for suffix in [BACKUP_SUFFIX, MARKER_SUFFIX, LOCK_SUFFIX] {
        if bytes.ends_with(suffix.as_bytes()) {
            return Err(LatchNameError::ReservedSuffix(suffix));
        }
    }
    for (tag, label) in [
        (CLEAR_TEMP_TAG, ".clear_tmp.<pid>.<seq>"),
        (LEGACY_TEMP_TAG, ".tmp.<pid>.<seq>"),
    ] {
        if is_temporary_family(bytes, tag) {
            return Err(LatchNameError::TemporaryFamily(label));
        }
    }
    if bytes == RX_TRUST_PRIMARY_NAME.as_bytes() {
        return Err(LatchNameError::RxTrustResource(RX_TRUST_PRIMARY_NAME));
    }
    if bytes == RX_TRUST_LOCK_STEM {
        return Err(LatchNameError::RxTrustResource("rx_trust"));
    }
    Ok(())
}

/// 只校验路径末段的 primary 名字（不 canonicalize、不触碰文件系统）。
///
/// # Errors
///
/// 路径没有文件名（如 `/`、`..` 结尾）按 [`LatchNameError::EmptyOrDot`] 拒绝。
pub fn validate_latch_path(latch_path: &Path) -> Result<(), LatchNameError> {
    let name = latch_path.file_name().ok_or(LatchNameError::EmptyOrDot)?;
    validate_latch_name(name)
}

/// 名字是否以 `<tag><digits>.<digits>` 结尾：两个数字段非空且只含 ASCII 数字，
/// 匹配从 `tag` 起到 basename 结尾；过长数字同样匹配，不尝试解析。
fn is_temporary_family(bytes: &[u8], tag: &[u8]) -> bool {
    let Some(seq_dot) = bytes.iter().rposition(|&byte| byte == b'.') else {
        return false;
    };
    if !is_digits(&bytes[seq_dot + 1..]) {
        return false;
    }
    let head = &bytes[..seq_dot];
    let Some(pid_dot) = head.iter().rposition(|&byte| byte == b'.') else {
        return false;
    };
    if !is_digits(&head[pid_dot + 1..]) {
        return false;
    }
    head[..=pid_dot].ends_with(tag)
}

fn is_digits(field: &[u8]) -> bool {
    !field.is_empty() && field.iter().all(u8::is_ascii_digit)
}

/// latch 的两个数据对象。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatchObject {
    /// `<name>`。
    Primary,
    /// `<name>.bak`。
    Backup,
}

/// 锁与检查的失败阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStage {
    /// `canonicalize(parent)`。
    CanonicalizeParent,
    /// 打开并核对 canonical 父目录。
    OpenDirectory,
    /// 相对目录 fd 打开 / 创建锁文件。
    OpenLock,
    /// 锁文件必须是 `nlink == 1` 的普通文件。
    InspectLock,
    /// 非阻塞 `flock`。
    Flock,
    /// 持锁后写诊断 PID。
    WritePid,
    /// 相对目录 fd 检查现存数据对象。
    InspectObject(LatchObject),
}

/// 稳定锁获取或对象检查失败。
#[derive(Debug, Error)]
pub enum LatchLockError {
    /// primary 名字不合法（配置错误）。
    #[error("latch 名字不合法: {0}")]
    InvalidName(#[source] LatchNameError),
    /// 另一进程持有同一目标的锁；`pid` 为其诊断记录（可能缺失）。
    #[error("latch 锁被占用（持有者 pid {pid:?}）")]
    Held { pid: Option<u32> },
    /// 系统调用失败。
    #[error("latch 锁 {stage:?} 阶段系统错误: {source}")]
    Io {
        stage: LockStage,
        #[source]
        source: io::Error,
    },
    /// 对象存在但类型或链接数不合法（符号链接、硬链接、特殊文件、目录）。
    #[error("latch {stage:?} 对象身份不合法: {detail}")]
    Identity {
        stage: LockStage,
        detail: &'static str,
    },
}

impl LatchLockError {
    /// 进程退出码：只有 [`LatchLockError::Held`] 是 89，其余都是 1。
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Held { .. } => LATCH_LOCK_HELD_EXIT_CODE,
            Self::InvalidName(_) | Self::Io { .. } | Self::Identity { .. } => {
                LATCH_LOCK_FAILURE_EXIT_CODE
            }
        }
    }
}

/// 文件系统对象身份 `(st_dev, st_ino)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectIdentity {
    dev: u64,
    ino: u64,
}

impl ObjectIdentity {
    fn of(metadata: &Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }

    #[must_use]
    pub const fn dev(self) -> u64 {
        self.dev
    }

    #[must_use]
    pub const fn ino(self) -> u64 {
        self.ino
    }

    /// 与 W0 store 打开时采样的目录身份比较。
    #[must_use]
    pub fn matches(self, identity: FileIdentity) -> bool {
        self.dev == identity.dev() && self.ino == identity.ino()
    }
}

impl fmt::Display for ObjectIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dev={} ino={}", self.dev, self.ino)
    }
}

/// 持锁后检查到的现存数据对象；`None` 表示真实 `NotFound`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataObjects {
    pub primary: Option<ObjectIdentity>,
    pub backup: Option<ObjectIdentity>,
}

/// 已解析的 latch 目标：canonical 父目录与派生文件名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatchTarget {
    canonical_parent: PathBuf,
    primary: OsString,
    backup: OsString,
    lock: OsString,
}

impl LatchTarget {
    /// 校验末段名字并 canonicalize 父目录；末段本身不解析符号链接（与 W0 一致）。
    ///
    /// # Errors
    ///
    /// 名字不合法返回 [`LatchLockError::InvalidName`]；父目录不可 canonicalize
    /// 返回 [`LatchLockError::Io`]。
    pub fn resolve(latch_path: &Path) -> Result<Self, LatchLockError> {
        let name = latch_path
            .file_name()
            .ok_or(LatchLockError::InvalidName(LatchNameError::EmptyOrDot))?;
        validate_latch_name(name).map_err(LatchLockError::InvalidName)?;
        let parent = latch_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let canonical_parent =
            std::fs::canonicalize(parent).map_err(|source| LatchLockError::Io {
                stage: LockStage::CanonicalizeParent,
                source,
            })?;
        Ok(Self {
            canonical_parent,
            primary: name.to_os_string(),
            backup: derived_name(name, BACKUP_SUFFIX),
            lock: derived_name(name, LOCK_SUFFIX),
        })
    }

    #[must_use]
    pub fn canonical_parent(&self) -> &Path {
        &self.canonical_parent
    }

    #[must_use]
    pub fn name_of(&self, object: LatchObject) -> &OsStr {
        match object {
            LatchObject::Primary => &self.primary,
            LatchObject::Backup => &self.backup,
        }
    }

    #[must_use]
    pub fn lock_name(&self) -> &OsStr {
        &self.lock
    }

    /// 仅供诊断的 canonical 路径；不用于任何文件操作。
    #[must_use]
    pub fn path_of(&self, object: LatchObject) -> PathBuf {
        self.canonical_parent.join(self.name_of(object))
    }

    /// 仅供诊断的锁路径；锁本身总是相对目录 fd 打开。
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.canonical_parent.join(&self.lock)
    }
}

fn derived_name(name: &OsStr, suffix: &str) -> OsString {
    let mut derived = name.to_os_string();
    derived.push(suffix);
    derived
}

/// 一个 latch 目标的独占稳定锁；活 `File` 即锁，`drop` 释放，永不 rename / unlink。
#[derive(Debug)]
pub struct LatchTargetLock {
    target: LatchTarget,
    directory: File,
    directory_identity: ObjectIdentity,
    lock: File,
}

/// 非阻塞 `flock` 的系统调用入口；测试注入只替换这一层。
trait LockSyscalls {
    fn flock_exclusive_nonblocking(&self, file: &File) -> io::Result<()>;
}

struct RealLockSyscalls;

impl LockSyscalls for RealLockSyscalls {
    fn flock_exclusive_nonblocking(&self, file: &File) -> io::Result<()> {
        // SAFETY：file 在调用期间存活，fd 有效；只请求非阻塞排他锁。
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// 获取 `latch_path` 目标的稳定锁。
///
/// 顺序：校验名字 → canonicalize 父目录 → 以 `O_DIRECTORY | O_CLOEXEC` 打开并采样
/// 目录身份 → 相对目录 fd 以 `O_RDWR | O_CREAT | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK`
/// mode 0600 打开 `<name>.lock`（不带 `O_TRUNC`）→ fstat 必须是 `nlink == 1` 的普通
/// 文件 → `flock(LOCK_EX | LOCK_NB)` 仅对 `EINTR` 重试 → 只有成功持锁后才
/// `set_len / seek / write` 本进程 PID。
///
/// # Errors
///
/// `EAGAIN` / `EWOULDBLOCK` 返回 [`LatchLockError::Held`]（退出码 89）；其余名字、
/// IO 与身份错误返回对应变体（退出码 1）。失败路径不改变任何 latch 或锁字节。
pub fn acquire_latch_lock(latch_path: &Path) -> Result<LatchTargetLock, LatchLockError> {
    let target = LatchTarget::resolve(latch_path)?;
    acquire_with(target, &RealLockSyscalls)
}

fn acquire_with(
    target: LatchTarget,
    syscalls: &dyn LockSyscalls,
) -> Result<LatchTargetLock, LatchLockError> {
    let io_at = |stage: LockStage| move |source: io::Error| LatchLockError::Io { stage, source };
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(target.canonical_parent())
        .map_err(io_at(LockStage::OpenDirectory))?;
    let directory_metadata = directory
        .metadata()
        .map_err(io_at(LockStage::OpenDirectory))?;
    if !directory_metadata.file_type().is_dir() {
        return Err(LatchLockError::Identity {
            stage: LockStage::OpenDirectory,
            detail: "latch 父目录不是目录",
        });
    }
    let directory_identity = ObjectIdentity::of(&directory_metadata);

    let lock = open_at(
        &directory,
        target.lock_name(),
        libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        LOCK_MODE,
    )
    .map_err(io_at(LockStage::OpenLock))?;
    let lock_metadata = lock.metadata().map_err(io_at(LockStage::InspectLock))?;
    check_regular_single_link(&lock_metadata, LockStage::InspectLock)?;

    match retry_interrupted(|| syscalls.flock_exclusive_nonblocking(&lock)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            return Err(LatchLockError::Held {
                pid: read_holder_pid(&lock),
            });
        }
        Err(source) => {
            return Err(LatchLockError::Io {
                stage: LockStage::Flock,
                source,
            });
        }
    }
    // 只有持锁者才改写诊断 PID；失败竞争者在上面已经返回。
    write_holder_pid(&lock).map_err(io_at(LockStage::WritePid))?;
    Ok(LatchTargetLock {
        target,
        directory,
        directory_identity,
        lock,
    })
}

fn read_holder_pid(lock: &File) -> Option<u32> {
    let mut text = String::new();
    let _ = lock.take(32).read_to_string(&mut text);
    text.trim().parse().ok()
}

fn write_holder_pid(mut lock: &File) -> io::Result<()> {
    lock.set_len(0)?;
    lock.seek(SeekFrom::Start(0))?;
    writeln!(lock, "{}", std::process::id())
}

fn check_regular_single_link(metadata: &Metadata, stage: LockStage) -> Result<(), LatchLockError> {
    if !metadata.file_type().is_file() {
        return Err(LatchLockError::Identity {
            stage,
            detail: "不是普通文件（符号链接 / 目录 / 特殊文件）",
        });
    }
    if metadata.nlink() != 1 {
        return Err(LatchLockError::Identity {
            stage,
            detail: "硬链接数不为 1",
        });
    }
    Ok(())
}

impl LatchTargetLock {
    /// 已解析的目标（canonical 父目录与派生名）。
    #[must_use]
    pub const fn target(&self) -> &LatchTarget {
        &self.target
    }

    /// 锁绑定的目录身份；daemon 用它与 W0 store 的 `directory_identity()` 比对。
    #[must_use]
    pub const fn directory_identity(&self) -> ObjectIdentity {
        self.directory_identity
    }

    /// 相对绑定目录 fd 检查现存 primary 与 backup：不跟随符号链接，必须是
    /// `nlink == 1` 的普通文件；仅真实 `NotFound` 记为缺失，其余 IO 错误 fail-closed。
    ///
    /// # Errors
    ///
    /// 见 [`LatchLockError::Io`] / [`LatchLockError::Identity`]。检查不写任何字节。
    pub fn check_data_objects(&self) -> Result<DataObjects, LatchLockError> {
        let primary = self.inspect_object(LatchObject::Primary)?;
        let backup = self.inspect_object(LatchObject::Backup)?;
        if let (Some(primary), Some(backup)) = (primary, backup)
            && primary == backup
        {
            return Err(LatchLockError::Identity {
                stage: LockStage::InspectObject(LatchObject::Backup),
                detail: "primary 与 backup 是同一个对象",
            });
        }
        Ok(DataObjects { primary, backup })
    }

    fn inspect_object(
        &self,
        object: LatchObject,
    ) -> Result<Option<ObjectIdentity>, LatchLockError> {
        let stage = LockStage::InspectObject(object);
        let file = match open_at(
            &self.directory,
            self.target.name_of(object),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(LatchLockError::Io { stage, source }),
        };
        let metadata = file
            .metadata()
            .map_err(|source| LatchLockError::Io { stage, source })?;
        check_regular_single_link(&metadata, stage)?;
        Ok(Some(ObjectIdentity::of(&metadata)))
    }

    /// 相对绑定目录 fd 读取一个数据对象的 latch 值（`0` / `1`，可带一个换行）。
    ///
    /// # Errors
    ///
    /// `NotFound` 原样返回；非普通文件返回 `InvalidData`；内容非法返回 `InvalidData`。
    pub fn read_latch_value(&self, object: LatchObject) -> io::Result<bool> {
        let mut file = open_at(
            &self.directory,
            self.target.name_of(object),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0,
        )?;
        if !file.metadata()?.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "latch 数据对象不是普通文件",
            ));
        }
        let mut buf = [0u8; 4];
        let n = file.read(&mut buf)?;
        parse_latch_value(&buf[..n])
    }

    /// 相对绑定目录 fd 严格写一个数据对象：临时文件 `<name>.tmp.<pid>.<seq>` 写入 →
    /// 文件 fsync → `renameat` → 目录 fsync，任一步失败即返回错误并尽力删除临时文件。
    ///
    /// # Errors
    ///
    /// 底层 IO 错误原样返回。
    pub fn write_latch_strict(&self, object: LatchObject, latched: bool) -> io::Result<()> {
        let target_name = self.target.name_of(object);
        let (temp_name, mut temp) = self.create_temporary(target_name)?;
        let result = temp
            .write_all(if latched { b"1\n" } else { b"0\n" })
            .and_then(|()| temp.sync_all())
            .and_then(|()| rename_at(&self.directory, &temp_name, target_name))
            .and_then(|()| self.directory.sync_all());
        if result.is_err() {
            let _ = unlink_at(&self.directory, &temp_name);
        }
        result
    }

    /// 同步绑定目录本身（provision 统一最终步骤）。
    ///
    /// # Errors
    ///
    /// `fsync` 错误原样返回。
    pub fn sync_directory(&self) -> io::Result<()> {
        self.directory.sync_all()
    }

    fn create_temporary(&self, target_name: &OsStr) -> io::Result<(OsString, File)> {
        for _ in 0..TEMP_ATTEMPTS {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let mut name = target_name.to_os_string();
            name.push(format!(".tmp.{}.{sequence}", std::process::id()));
            match open_at(
                &self.directory,
                &name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                LATCH_MODE,
            ) {
                Ok(file) => return Ok((name, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "latch 临时文件连续候选名均已存在",
        ))
    }

    /// 锁文件 fd 是否带 `FD_CLOEXEC`（供测试与诊断核对）。
    ///
    /// # Errors
    ///
    /// `fcntl` 错误原样返回。
    pub fn lock_is_cloexec(&self) -> io::Result<bool> {
        // SAFETY：fd 在 self 存活期间有效；F_GETFD 不带额外参数。
        let flags = unsafe { libc::fcntl(self.lock.as_raw_fd(), libc::F_GETFD) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(flags & libc::FD_CLOEXEC != 0)
    }
}

/// 与 `estop_latch::read_single` 相同的接受形态：`0`、`1`，各自可带一个换行。
fn parse_latch_value(bytes: &[u8]) -> io::Result<bool> {
    match bytes {
        b"0" | b"0\n" => Ok(false),
        b"1" | b"1\n" => Ok(true),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "estop_latch file: expected '0\\n' or '1\\n'",
        )),
    }
}

fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "文件名含 NUL"))
}

fn open_at(
    directory: &File,
    name: &OsStr,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> io::Result<File> {
    let name = c_name(name)?;
    let fd = retry_eintr_raw(|| {
        // SAFETY：目录 fd 在 directory 存活期间有效；name 以 NUL 结尾；openat 的
        // 可变参数 mode 以 c_uint 传入，不带 O_CREAT 时内核忽略它。
        unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, mode) }
    })?;
    // SAFETY：fd 由 openat 刚返回且只在此处转移所有权。
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn rename_at(directory: &File, from: &OsStr, to: &OsStr) -> io::Result<()> {
    let from = c_name(from)?;
    let to = c_name(to)?;
    retry_eintr_raw(|| {
        // SAFETY：两个目录参数都是同一个有效 fd；两个名字均以 NUL 结尾。
        unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                from.as_ptr(),
                directory.as_raw_fd(),
                to.as_ptr(),
            )
        }
    })
    .map(|_| ())
}

fn unlink_at(directory: &File, name: &OsStr) -> io::Result<()> {
    let name = c_name(name)?;
    retry_eintr_raw(|| {
        // SAFETY：目录 fd 有效；name 以 NUL 结尾；flags 为 0 表示只删文件。
        unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) }
    })
    .map(|_| ())
}

/// 只对返回 `-1 / EINTR` 的原始系统调用重试。
fn retry_eintr_raw(mut call: impl FnMut() -> libc::c_int) -> io::Result<libc::c_int> {
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

/// 只对 `Interrupted` 重试；`WouldBlock` 与其他错误原样返回。
fn retry_interrupted(mut operation: impl FnMut() -> io::Result<()>) -> io::Result<()> {
    loop {
        match operation() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::os::unix::ffi::OsStrExt;

    use tempfile::TempDir;

    use super::*;

    fn name(bytes: &[u8]) -> &OsStr {
        OsStr::from_bytes(bytes)
    }

    /// 合法名字：自定义、含点、非 UTF-8、以 `tmp` 开头但不构成家族的名字。
    #[test]
    fn accepts_custom_and_non_utf8_names() {
        for legal in [
            &b"estop_latch"[..],
            b"left.latch",
            b"latch.bak2",
            b"latch.lockx",
            b"tmp.1.2",
            b"x.tmp.1.",
            b"x.tmp..2",
            b"x.tmp.1a.2",
            b"x.clear_tmp.1",
            b"rx_trust_markerx",
            b"rx_trust2",
            b"\xff\xfe\x80latch",
            b".hidden",
            b"...",
        ] {
            assert_eq!(validate_latch_name(name(legal)), Ok(()), "{legal:?}");
        }
    }

    /// 保留尾缀、完整临时家族（含超长数字）、RX 固定资源与空名全部拒绝。
    #[test]
    fn rejects_reserved_derived_and_rx_names() {
        use LatchNameError as E;
        let cases: [(&[u8], E); 14] = [
            (b"", E::EmptyOrDot),
            (b".", E::EmptyOrDot),
            (b"..", E::EmptyOrDot),
            (b"estop_latch.bak", E::ReservedSuffix(".bak")),
            (b".bak", E::ReservedSuffix(".bak")),
            (
                b"estop_latch.clear_pending",
                E::ReservedSuffix(".clear_pending"),
            ),
            (b"estop_latch.lock", E::ReservedSuffix(".lock")),
            (
                b"estop_latch.tmp.123.4",
                E::TemporaryFamily(".tmp.<pid>.<seq>"),
            ),
            (
                b"estop_latch.tmp.99999999999999999999999999.18446744073709551616",
                E::TemporaryFamily(".tmp.<pid>.<seq>"),
            ),
            (b"\xff.tmp.1.2", E::TemporaryFamily(".tmp.<pid>.<seq>")),
            (
                b"estop_latch.clear_tmp.7.8",
                E::TemporaryFamily(".clear_tmp.<pid>.<seq>"),
            ),
            (
                b".clear_tmp.0.0",
                E::TemporaryFamily(".clear_tmp.<pid>.<seq>"),
            ),
            (b"rx_trust_marker", E::RxTrustResource("rx_trust_marker")),
            (b"rx_trust", E::RxTrustResource("rx_trust")),
        ];
        for (raw, expected) in cases {
            assert_eq!(validate_latch_name(name(raw)), Err(expected), "{raw:?}");
        }
    }

    /// 派生名与合法 primary 互不相交：任何合法名字的派生文件名都不再是合法 primary。
    #[test]
    fn derived_names_are_never_legal_primaries() {
        for primary in [&b"estop_latch"[..], b"a.b", b"\xff"] {
            let target_dir = TempDir::new().unwrap();
            let target = LatchTarget::resolve(&target_dir.path().join(name(primary))).unwrap();
            for derived in [
                target.name_of(LatchObject::Backup).to_os_string(),
                target.lock_name().to_os_string(),
                {
                    let mut marker = target.name_of(LatchObject::Primary).to_os_string();
                    marker.push(".clear_pending");
                    marker
                },
                {
                    let mut temp = target.name_of(LatchObject::Primary).to_os_string();
                    temp.push(".tmp.1.2");
                    temp
                },
                {
                    let mut temp = target.name_of(LatchObject::Primary).to_os_string();
                    temp.push(".clear_tmp.1.2");
                    temp
                },
            ] {
                assert!(validate_latch_name(&derived).is_err(), "{derived:?}");
            }
        }
    }

    #[test]
    fn path_validation_rejects_paths_without_file_name() {
        assert_eq!(
            validate_latch_path(Path::new("/")),
            Err(LatchNameError::EmptyOrDot)
        );
        assert_eq!(
            validate_latch_path(Path::new("/var/lib/robot/..")),
            Err(LatchNameError::EmptyOrDot)
        );
        assert_eq!(
            validate_latch_path(Path::new("relative/estop_latch")),
            Ok(())
        );
    }

    #[test]
    fn latch_value_parsing_matches_legacy_reader() {
        let dir = TempDir::new().unwrap();
        for (raw, expected) in [
            (&b"0"[..], Ok(false)),
            (b"0\n", Ok(false)),
            (b"1", Ok(true)),
            (b"1\n", Ok(true)),
            (b"", Err(io::ErrorKind::InvalidData)),
            (b"2\n", Err(io::ErrorKind::InvalidData)),
            (b"1\n\n", Err(io::ErrorKind::InvalidData)),
            (b"01", Err(io::ErrorKind::InvalidData)),
            (b"garbage", Err(io::ErrorKind::InvalidData)),
        ] {
            let path = dir.path().join("estop_latch");
            std::fs::write(&path, raw).unwrap();
            let legacy = crate::estop_latch::read_single(&path).map_err(|e| e.kind());
            let ours = parse_latch_value(&raw[..raw.len().min(4)]).map_err(|e| e.kind());
            assert_eq!(ours, expected, "{raw:?}");
            assert_eq!(legacy, expected, "legacy {raw:?}");
        }
    }

    #[test]
    fn lock_is_created_with_pid_and_is_exclusive_per_target() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("estop_latch");
        let lock = acquire_latch_lock(&path).expect("首次取锁");
        let lock_path = dir.path().join("estop_latch.lock");
        let metadata = std::fs::metadata(&lock_path).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::read_to_string(&lock_path).unwrap(),
            format!("{}\n", std::process::id())
        );
        assert!(lock.lock_is_cloexec().unwrap(), "锁 fd 必须 CLOEXEC");
        assert_eq!(lock.target().lock_path(), lock_path);
        assert_eq!(
            lock.target().path_of(LatchObject::Backup),
            dir.path().canonicalize().unwrap().join("estop_latch.bak")
        );

        // 同一进程内第二个打开文件描述再取锁也冲突（flock 绑定打开文件描述）。
        let held = acquire_latch_lock(&path).expect_err("必须冲突");
        assert!(
            matches!(held, LatchLockError::Held { pid: Some(pid) } if pid == std::process::id()),
            "{held:?}"
        );
        assert_eq!(held.exit_code(), LATCH_LOCK_HELD_EXIT_CODE);
        // 失败竞争者不改 PID。
        assert_eq!(
            std::fs::read_to_string(&lock_path).unwrap(),
            format!("{}\n", std::process::id())
        );

        // 同目录另一个合法名字可独立持锁。
        let other = acquire_latch_lock(&dir.path().join("other_latch")).expect("不同目标并发持锁");
        drop(other);
        drop(lock);
        acquire_latch_lock(&path).expect("释放后可重取");
        assert!(lock_path.exists(), "锁文件永不删除");
    }

    #[test]
    fn invalid_names_fail_before_any_file_is_created() {
        let dir = TempDir::new().unwrap();
        for bad in [
            "estop_latch.bak",
            "estop_latch.lock",
            "rx_trust_marker",
            "x.tmp.1.2",
        ] {
            let error = acquire_latch_lock(&dir.path().join(bad)).expect_err(bad);
            assert!(
                matches!(error, LatchLockError::InvalidName(_)),
                "{bad}: {error:?}"
            );
            assert_eq!(error.exit_code(), LATCH_LOCK_FAILURE_EXIT_CODE);
        }
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "不得创建任何文件"
        );
    }

    #[test]
    fn missing_parent_is_an_io_error_not_held() {
        let dir = TempDir::new().unwrap();
        let error =
            acquire_latch_lock(&dir.path().join("missing").join("estop_latch")).unwrap_err();
        assert!(
            matches!(
                error,
                LatchLockError::Io {
                    stage: LockStage::CanonicalizeParent,
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(error.exit_code(), 1);
    }

    struct ScriptedFlock {
        inner: RealLockSyscalls,
        errnos: RefCell<Vec<i32>>,
        calls: RefCell<u32>,
    }

    impl LockSyscalls for ScriptedFlock {
        fn flock_exclusive_nonblocking(&self, file: &File) -> io::Result<()> {
            *self.calls.borrow_mut() += 1;
            let mut errnos = self.errnos.borrow_mut();
            if errnos.is_empty() {
                return self.inner.flock_exclusive_nonblocking(file);
            }
            Err(io::Error::from_raw_os_error(errnos.remove(0)))
        }
    }

    /// `EINTR` 重试直到真实成功；`EAGAIN` 判 Held；其他 errno 判 IO，都不写 PID。
    #[test]
    fn flock_retries_only_eintr() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("estop_latch");
        let lock_path = dir.path().join("estop_latch.lock");
        std::fs::write(&lock_path, "4242\n").unwrap();

        let script = ScriptedFlock {
            inner: RealLockSyscalls,
            errnos: RefCell::new(vec![libc::EINTR, libc::EINTR]),
            calls: RefCell::new(0),
        };
        let lock =
            acquire_with(LatchTarget::resolve(&path).unwrap(), &script).expect("EINTR 后成功");
        assert_eq!(*script.calls.borrow(), 3);
        assert_eq!(
            std::fs::read_to_string(&lock_path).unwrap(),
            format!("{}\n", std::process::id())
        );
        drop(lock);
        std::fs::write(&lock_path, "4242\n").unwrap();

        for (errno, expect_held) in [
            (libc::EAGAIN, true),
            (libc::EWOULDBLOCK, true),
            (libc::EIO, false),
        ] {
            let script = ScriptedFlock {
                inner: RealLockSyscalls,
                errnos: RefCell::new(vec![errno]),
                calls: RefCell::new(0),
            };
            let error = acquire_with(LatchTarget::resolve(&path).unwrap(), &script).unwrap_err();
            match error {
                LatchLockError::Held { pid } => {
                    assert!(expect_held, "errno {errno}");
                    assert_eq!(pid, Some(4242));
                }
                LatchLockError::Io { stage, .. } => {
                    assert!(!expect_held, "errno {errno}");
                    assert_eq!(stage, LockStage::Flock);
                }
                other => panic!("errno {errno}: {other:?}"),
            }
            assert_eq!(
                std::fs::read_to_string(&lock_path).unwrap(),
                "4242\n",
                "失败者不改 PID"
            );
        }
    }

    #[test]
    fn lock_object_must_be_regular_single_link() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("estop_latch");
        let lock_path = dir.path().join("estop_latch.lock");

        // 目录。
        std::fs::create_dir(&lock_path).unwrap();
        let error = acquire_latch_lock(&path).unwrap_err();
        assert!(
            matches!(
                error,
                LatchLockError::Io {
                    stage: LockStage::OpenLock,
                    ..
                }
            ),
            "{error:?}"
        );
        std::fs::remove_dir(&lock_path).unwrap();

        // 符号链接（O_NOFOLLOW → ELOOP）。
        std::fs::write(dir.path().join("real"), "1\n").unwrap();
        std::os::unix::fs::symlink("real", &lock_path).unwrap();
        let error = acquire_latch_lock(&path).unwrap_err();
        assert!(
            matches!(
                error,
                LatchLockError::Io {
                    stage: LockStage::OpenLock,
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("real")).unwrap(),
            "1\n"
        );
        std::fs::remove_file(&lock_path).unwrap();

        // 硬链接。
        std::fs::write(&lock_path, "").unwrap();
        std::fs::hard_link(&lock_path, dir.path().join("alias")).unwrap();
        let error = acquire_latch_lock(&path).unwrap_err();
        assert!(
            matches!(
                error,
                LatchLockError::Identity {
                    stage: LockStage::InspectLock,
                    ..
                }
            ),
            "{error:?}"
        );
        std::fs::remove_file(dir.path().join("alias")).unwrap();
        acquire_latch_lock(&path).expect("链接数恢复为 1 后可取锁");
    }

    #[test]
    fn data_object_check_distinguishes_missing_from_illegal() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("estop_latch");
        let lock = acquire_latch_lock(&path).unwrap();
        assert_eq!(
            lock.check_data_objects().unwrap(),
            DataObjects {
                primary: None,
                backup: None
            }
        );

        std::fs::write(&path, "1\n").unwrap();
        let objects = lock.check_data_objects().unwrap();
        assert!(objects.primary.is_some() && objects.backup.is_none());
        assert!(lock.read_latch_value(LatchObject::Primary).unwrap());
        assert_eq!(
            lock.read_latch_value(LatchObject::Backup)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );

        // backup 为符号链接：拒绝，且不动 primary。
        std::os::unix::fs::symlink("estop_latch", dir.path().join("estop_latch.bak")).unwrap();
        let error = lock.check_data_objects().unwrap_err();
        assert!(
            matches!(
                error,
                LatchLockError::Identity {
                    stage: LockStage::InspectObject(LatchObject::Backup),
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(
            lock.read_latch_value(LatchObject::Backup)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ELOOP)
        );
        std::fs::remove_file(dir.path().join("estop_latch.bak")).unwrap();

        // backup 与 primary 互为硬链接：nlink == 2 → 拒绝。
        std::fs::hard_link(&path, dir.path().join("estop_latch.bak")).unwrap();
        let error = lock.check_data_objects().unwrap_err();
        assert!(
            matches!(
                error,
                LatchLockError::Identity {
                    stage: LockStage::InspectObject(LatchObject::Primary),
                    ..
                }
            ),
            "{error:?}"
        );
        std::fs::remove_file(dir.path().join("estop_latch.bak")).unwrap();

        // FIFO 作为 backup：O_PATH 不阻塞，类型拒绝。
        let fifo =
            std::ffi::CString::new(dir.path().join("estop_latch.bak").as_os_str().as_bytes())
                .unwrap();
        // SAFETY：路径以 NUL 结尾；mkfifo 只创建命名管道。
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let error = lock.check_data_objects().unwrap_err();
        assert!(
            matches!(error, LatchLockError::Identity { .. }),
            "{error:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "1\n",
            "不自愈、不改 primary"
        );
    }

    #[test]
    fn strict_write_goes_through_bound_directory_even_after_parent_replacement() {
        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("robot");
        std::fs::create_dir(&parent).unwrap();
        let path = parent.join("estop_latch");
        let lock = acquire_latch_lock(&path).unwrap();

        // 持锁后把父目录整个换掉：旧目录改名、新目录占用原路径。
        let moved = dir.path().join("robot.old");
        std::fs::rename(&parent, &moved).unwrap();
        std::fs::create_dir(&parent).unwrap();

        lock.write_latch_strict(LatchObject::Primary, true).unwrap();
        lock.write_latch_strict(LatchObject::Backup, false).unwrap();
        lock.sync_directory().unwrap();
        assert_eq!(
            std::fs::read_to_string(moved.join("estop_latch")).unwrap(),
            "1\n"
        );
        assert_eq!(
            std::fs::read_to_string(moved.join("estop_latch.bak")).unwrap(),
            "0\n"
        );
        assert_eq!(
            std::fs::read_dir(&parent).unwrap().count(),
            0,
            "新目录不得收到任何写入"
        );
        let temps: Vec<_> = std::fs::read_dir(&moved)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.as_bytes().windows(5).any(|window| window == b".tmp."))
            .collect();
        assert!(temps.is_empty(), "临时文件必须已 rename 掉: {temps:?}");
        assert_eq!(
            std::fs::metadata(moved.join("estop_latch")).unwrap().mode() & 0o777,
            0o640
        );
        assert!(lock.read_latch_value(LatchObject::Primary).unwrap());
        assert!(!lock.read_latch_value(LatchObject::Backup).unwrap());
    }

    #[test]
    fn strict_write_failure_removes_temporary() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("estop_latch");
        let lock = acquire_latch_lock(&path).unwrap();
        // 让目标名成为目录：renameat 文件→目录失败（EISDIR），临时文件必须被清理。
        std::fs::create_dir(&path).unwrap();
        let error = lock
            .write_latch_strict(LatchObject::Primary, true)
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EISDIR), "{error}");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "只剩锁文件与目标目录: {names:?}");
        assert!(
            names
                .iter()
                .all(|n| n == "estop_latch" || n == "estop_latch.lock"),
            "{names:?}"
        );
    }
}
