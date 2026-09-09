//! `ClearEStop` 的纯文件事务：先建立持久标记，再准备双份未锁存值。
//!
//! 本模块只供普通磁盘 writer 使用，尚未接入生产启动或控制链。它不判断
//! 当前安全代次，也不授予清锁、使能或回复成功的权限。删除标记之前，上层
//! 必须已经持有当前事务的 Finalize 许可；磁盘操作与内存提交并非原子操作。
//!
//! 调用方必须保证目标只有一个 writer，并持有部署层的排他锁。非 `Clone`
//! 的 store 与按值消费的凭证只约束本实例，不能代替跨实例或跨进程互斥。

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::estop_latch::{backup_path, fsync_parent_dir_strict, load_estop_latch};

const MAGIC: &[u8; 8] = b"RBTCLRTX";
const VERSION: u16 = 1;
const RECORD_BYTES: usize = 112;
const CHECKSUM_OFFSET: usize = 80;
const IDENTITY_DOMAIN: &str = "robot-system clear-estop target identity v1";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 失败所在文件步骤；目录同步失败不意味着此前的 rename 或 unlink 未发生。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearTxnStage {
    OpenStore,
    ReadMarker,
    ValidateMarker,
    CreateMarker,
    WriteMarker,
    SyncMarkerFile,
    SyncMarkerDirectory,
    CreateBackup,
    WriteBackup,
    SyncBackupFile,
    RenameBackup,
    SyncBackupDirectory,
    CreatePrimary,
    WritePrimary,
    SyncPrimaryFile,
    RenamePrimary,
    SyncPrimaryDirectory,
    RemoveMarker,
    SyncRemovalDirectory,
    LoadLatch,
}

/// 保留明确文件阶段与原始 IO 错误，不把部分成功改写成“没有副作用”。
#[derive(Debug, thiserror::Error)]
#[error("Clear 事务文件步骤 {stage:?} 失败: {source}")]
pub struct ClearTxnError {
    pub stage: ClearTxnStage,
    #[source]
    pub source: io::Error,
}

impl ClearTxnError {
    fn new(stage: ClearTxnStage, source: io::Error) -> Self {
        Self { stage, source }
    }
}

/// 完整事务身份。字段只读；它既不是 durability 证明，也不是清锁许可。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClearTxnRecord {
    target_identity: [u8; 32],
    txn_id: u128,
    expected_epoch: u64,
    original_latched: bool,
}

impl ClearTxnRecord {
    #[must_use]
    pub const fn txn_id(&self) -> u128 {
        self.txn_id
    }

    #[must_use]
    pub const fn expected_epoch(&self) -> u64 {
        self.expected_epoch
    }

    #[must_use]
    pub const fn original_latched(&self) -> bool {
        self.original_latched
    }

    fn encode(self) -> [u8; RECORD_BYTES] {
        let mut bytes = [0; RECORD_BYTES];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..10].copy_from_slice(&VERSION.to_le_bytes());
        bytes[16..48].copy_from_slice(&self.target_identity);
        bytes[48..64].copy_from_slice(&self.txn_id.to_le_bytes());
        bytes[64..72].copy_from_slice(&self.expected_epoch.to_le_bytes());
        bytes[72] = u8::from(self.original_latched);
        let checksum = blake3::hash(&bytes[..CHECKSUM_OFFSET]);
        bytes[CHECKSUM_OFFSET..].copy_from_slice(checksum.as_bytes());
        bytes
    }

    fn decode(bytes: &[u8], target_identity: [u8; 32]) -> io::Result<Self> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "Clear 事务标记损坏或身份不符");
        if bytes.len() != RECORD_BYTES
            || &bytes[..8] != MAGIC
            || bytes[8..10] != VERSION.to_le_bytes()
            || bytes[10..16] != [0; 6]
            || bytes[73..80] != [0; 7]
            || bytes[16..48] != target_identity
            || bytes[CHECKSUM_OFFSET..] != *blake3::hash(&bytes[..CHECKSUM_OFFSET]).as_bytes()
        {
            return Err(invalid());
        }
        let original_latched = match bytes[72] {
            0 => false,
            1 => true,
            _ => return Err(invalid()),
        };
        let txn_id = u128::from_le_bytes(bytes[48..64].try_into().map_err(|_| invalid())?);
        if txn_id == 0 {
            return Err(invalid());
        }
        let expected_epoch = u64::from_le_bytes(bytes[64..72].try_into().map_err(|_| invalid())?);
        Ok(Self {
            target_identity,
            txn_id,
            expected_epoch,
            original_latched,
        })
    }
}

/// `inspect_pending` 的只读结果；不能用它直接构造已准备或已修复凭证。
#[derive(Debug)]
pub struct PendingClear {
    record: ClearTxnRecord,
}

impl PendingClear {
    #[must_use]
    pub const fn record(&self) -> &ClearTxnRecord {
        &self.record
    }
}

/// 标记与双份 `0` 均完成严格同步；仍需要上层执行权限仲裁。
///
/// 准备凭证不能复制，也不能在被消费后再次使用：
///
/// ```compile_fail
/// use robot_s0::clear_estop_transaction::PreparedClear;
/// fn duplicate(prepared: PreparedClear) { let _ = prepared.clone(); }
/// ```
///
/// ```compile_fail
/// use robot_s0::clear_estop_transaction::{ClearTxnStore, PreparedClear};
/// fn remove_twice(store: &mut ClearTxnStore, prepared: PreparedClear) {
///     store.remove_prepared_marker(prepared).unwrap();
///     store.remove_prepared_marker(prepared).unwrap();
/// }
/// ```
#[derive(Debug)]
pub struct PreparedClear {
    record: ClearTxnRecord,
}

impl PreparedClear {
    #[must_use]
    pub const fn record(&self) -> &ClearTxnRecord {
        &self.record
    }
}

/// 指定修复目标已严格落盘，标记仍存在；删除时继续核完整事务身份。
///
/// ```compile_fail
/// use robot_s0::clear_estop_transaction::RepairedClear;
/// fn duplicate(repaired: RepairedClear) { let _ = repaired.clone(); }
/// ```
#[derive(Debug)]
pub struct RepairedClear {
    record: ClearTxnRecord,
}

/// 由唯一 owner 根据当前权限决定；文件层不自行猜测新事件或代次。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairTarget {
    /// 还原该事务开始前的 latch，适用于只被 Disable 否决的恢复。
    Original,
    /// 新 `EStop` 或未完成事务的启动恢复必须保守锁存。
    Latched,
}

impl RepairTarget {
    const fn latched(self, record: ClearTxnRecord) -> bool {
        match self {
            Self::Original => record.original_latched,
            Self::Latched => true,
        }
    }
}

/// 单 writer 的纯文件事务存储。构造不读取或修复 latch。
pub struct ClearTxnStore {
    latch_path: PathBuf,
    marker_path: PathBuf,
    target_identity: [u8; 32],
    #[cfg(test)]
    after_step: Option<Box<dyn FnMut(ClearTxnStage) -> io::Result<()> + Send>>,
}

impl ClearTxnStore {
    /// 身份绑定已存在父目录的规范路径与原文件名，不依赖 latch 的存在或 inode。
    pub fn open(
        latch_path: &Path,
        robot_id: &str,
        hardware_id: &str,
    ) -> Result<Self, ClearTxnError> {
        let invalid_path = || {
            ClearTxnError::new(
                ClearTxnStage::OpenStore,
                io::Error::new(io::ErrorKind::InvalidInput, "latch 路径必须包含文件名"),
            )
        };
        let name = latch_path.file_name().ok_or_else(invalid_path)?;
        let parent = latch_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let canonical = std::fs::canonicalize(parent)
            .map_err(|error| ClearTxnError::new(ClearTxnStage::OpenStore, error))?
            .join(name);
        let mut hasher = blake3::Hasher::new_derive_key(IDENTITY_DOMAIN);
        for bytes in [
            robot_id.as_bytes(),
            hardware_id.as_bytes(),
            canonical.as_os_str().as_encoded_bytes(),
        ] {
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        }
        let mut marker_name = OsString::from(name);
        marker_name.push(".clear_pending");
        Ok(Self {
            marker_path: canonical.with_file_name(marker_name),
            latch_path: canonical,
            target_identity: *hasher.finalize().as_bytes(),
            #[cfg(test)]
            after_step: None,
        })
    }

    /// 只读校验，合法标记可能尚未经过任何同步；错误时不触发 latch 自愈。
    pub fn inspect_pending(&mut self) -> Result<Option<PendingClear>, ClearTxnError> {
        let marker_path = &self.marker_path;
        let mut file = match open_marker_file(marker_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ClearTxnError::new(ClearTxnStage::ReadMarker, error)),
        };
        self.read_record(&mut file)
            .map(|record| Some(PendingClear { record }))
    }

    /// 建立排他事务标记并严格准备双份 `0`。事务 ID 的唯一性由上层保证。
    pub fn prepare_clear(
        &mut self,
        txn_id: u128,
        expected_epoch: u64,
        original_latched: bool,
    ) -> Result<PreparedClear, ClearTxnError> {
        if txn_id == 0 {
            return Err(ClearTxnError::new(
                ClearTxnStage::ValidateMarker,
                io::Error::new(io::ErrorKind::InvalidInput, "Clear 事务 ID 不得为零"),
            ));
        }
        let record = ClearTxnRecord {
            target_identity: self.target_identity,
            txn_id,
            expected_epoch,
            original_latched,
        };
        let marker_path = self.marker_path.clone();
        let mut marker = self.step(ClearTxnStage::CreateMarker, || {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&marker_path)
        })?;
        self.step(ClearTxnStage::WriteMarker, || {
            marker.write_all(&record.encode())
        })?;
        self.sync_marker(&marker)?;
        self.write_pair(false)?;
        Ok(PreparedClear { record })
    }

    /// 消费准备凭证，完整核对当前标记后修复。失败时不删除标记。
    #[allow(
        clippy::needless_pass_by_value,
        reason = "必须消费一次性凭证，不能改为借用或 Copy"
    )]
    pub fn repair_prepared(
        &mut self,
        prepared: PreparedClear,
        target: RepairTarget,
    ) -> Result<RepairedClear, ClearTxnError> {
        self.matching_marker(prepared.record)?;
        self.write_pair(target.latched(prepared.record))?;
        Ok(RepairedClear {
            record: prepared.record,
        })
    }

    /// 从可读记录恢复修复流程；先重核并同步标记，不能凭可读状态直接写 `0`。
    #[allow(
        clippy::needless_pass_by_value,
        reason = "修复必须消费本次检查结果，禁止重复使用旧凭证"
    )]
    pub fn repair_pending(
        &mut self,
        pending: PendingClear,
        target: RepairTarget,
    ) -> Result<RepairedClear, ClearTxnError> {
        let marker = self.matching_marker(pending.record)?;
        self.sync_marker(&marker)?;
        self.write_pair(target.latched(pending.record))?;
        Ok(RepairedClear {
            record: pending.record,
        })
    }

    /// 消费准备凭证后清理标记。调用方必须已持有匹配的 Finalize 许可。
    ///
    /// 目录同步失败时 unlink 可能已经生效；不重建标记，也不自动重试。
    #[allow(clippy::needless_pass_by_value, reason = "删除必须消费一次性准备凭证")]
    pub fn remove_prepared_marker(&mut self, prepared: PreparedClear) -> Result<(), ClearTxnError> {
        self.remove_marker(prepared.record)
    }

    /// 修复目标已严格落盘后消费修复凭证，清理本事务标记。
    #[allow(clippy::needless_pass_by_value, reason = "删除必须消费一次性修复凭证")]
    pub fn remove_repaired_marker(&mut self, repaired: RepairedClear) -> Result<(), ClearTxnError> {
        self.remove_marker(repaired.record)
    }

    /// 先检查事务标记，再决定是否允许沿用旧 loader 的读取与自愈逻辑。
    #[allow(
        clippy::single_match_else,
        reason = "按项目约定显式列出 Option 两类恢复路径"
    )]
    pub fn recover_and_load(&mut self) -> Result<bool, ClearTxnError> {
        match self.inspect_pending()? {
            Some(pending) => {
                let repaired = self.repair_pending(pending, RepairTarget::Latched)?;
                self.remove_repaired_marker(repaired)?;
                Ok(true)
            }
            None => {
                let latch_path = self.latch_path.clone();
                self.step(ClearTxnStage::LoadLatch, || load_estop_latch(&latch_path))
            }
        }
    }

    fn read_record(&mut self, file: &mut File) -> Result<ClearTxnRecord, ClearTxnError> {
        let mut bytes = [0; RECORD_BYTES + 1];
        let len = self.step(ClearTxnStage::ReadMarker, || {
            let mut reader = file.take(bytes.len() as u64);
            let mut len = 0;
            loop {
                match reader.read(&mut bytes[len..]) {
                    Ok(0) => return Ok(len),
                    Ok(count) => len += count,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
        })?;
        let target_identity = self.target_identity;
        self.step(ClearTxnStage::ValidateMarker, || {
            ClearTxnRecord::decode(&bytes[..len], target_identity)
        })
    }

    fn matching_marker(&mut self, expected: ClearTxnRecord) -> Result<File, ClearTxnError> {
        let mut file = open_marker_file(&self.marker_path)
            .map_err(|error| ClearTxnError::new(ClearTxnStage::ReadMarker, error))?;
        let actual = self.read_record(&mut file)?;
        if actual != expected {
            return Err(ClearTxnError::new(
                ClearTxnStage::ValidateMarker,
                io::Error::new(io::ErrorKind::InvalidData, "Clear 事务标记已不属于当前凭证"),
            ));
        }
        Ok(file)
    }

    fn sync_marker(&mut self, marker: &File) -> Result<(), ClearTxnError> {
        self.step(ClearTxnStage::SyncMarkerFile, || marker.sync_all())?;
        let marker_path = self.marker_path.clone();
        self.step(ClearTxnStage::SyncMarkerDirectory, || {
            fsync_parent_dir_strict(&marker_path)
        })
    }

    fn write_pair(&mut self, latched: bool) -> Result<(), ClearTxnError> {
        let primary = self.latch_path.clone();
        let backup = backup_path(&primary);
        if latched {
            self.write_latch(&primary, true, LatchCopy::Primary)?;
            self.write_latch(&backup, true, LatchCopy::Backup)
        } else {
            self.write_latch(&backup, false, LatchCopy::Backup)?;
            self.write_latch(&primary, false, LatchCopy::Primary)
        }
    }

    fn write_latch(
        &mut self,
        path: &Path,
        latched: bool,
        copy: LatchCopy,
    ) -> Result<(), ClearTxnError> {
        let stages = copy.stages();
        let (temporary, mut file) = self.step(stages[0], || TemporaryLatch::create(path))?;
        self.step(stages[1], || {
            file.write_all(if latched { b"1\n" } else { b"0\n" })
        })?;
        self.step(stages[2], || file.sync_all())?;
        self.step(stages[3], || std::fs::rename(&temporary.path, path))?;
        self.step(stages[4], || fsync_parent_dir_strict(path))
    }

    fn remove_marker(&mut self, record: ClearTxnRecord) -> Result<(), ClearTxnError> {
        self.matching_marker(record)?;
        let marker_path = self.marker_path.clone();
        self.step(ClearTxnStage::RemoveMarker, || {
            std::fs::remove_file(&marker_path)
        })?;
        self.step(ClearTxnStage::SyncRemovalDirectory, || {
            fsync_parent_dir_strict(&marker_path)
        })
    }

    #[cfg_attr(
        not(test),
        allow(
            clippy::unused_self,
            reason = "测试注入属于 store 实例，生产与测试共用相同文件算法"
        )
    )]
    fn step<T>(
        &mut self,
        stage: ClearTxnStage,
        operation: impl FnOnce() -> io::Result<T>,
    ) -> Result<T, ClearTxnError> {
        let result = operation().map_err(|error| ClearTxnError::new(stage, error))?;
        #[cfg(test)]
        if let Some(after_step) = &mut self.after_step {
            after_step(stage).map_err(|error| ClearTxnError::new(stage, error))?;
        }
        Ok(result)
    }
}

/// 不跟随链接，不在 FIFO 上等待；只有真正缺失才能进入无标记恢复分支。
fn open_marker_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Clear 事务标记必须是普通文件",
        ));
    }
    Ok(file)
}

#[derive(Clone, Copy)]
enum LatchCopy {
    Backup,
    Primary,
}

impl LatchCopy {
    const fn stages(self) -> [ClearTxnStage; 5] {
        use ClearTxnStage as Stage;
        match self {
            Self::Backup => [
                Stage::CreateBackup,
                Stage::WriteBackup,
                Stage::SyncBackupFile,
                Stage::RenameBackup,
                Stage::SyncBackupDirectory,
            ],
            Self::Primary => [
                Stage::CreatePrimary,
                Stage::WritePrimary,
                Stage::SyncPrimaryFile,
                Stage::RenamePrimary,
                Stage::SyncPrimaryDirectory,
            ],
        }
    }
}

/// 临时文件在常规错误时清理；进程中断留下的临时文件不参与 latch 恢复。
struct TemporaryLatch {
    path: PathBuf,
}

impl TemporaryLatch {
    fn create(target: &Path) -> io::Result<(Self, File)> {
        Self::create_with_sequence(target, &TEMP_SEQUENCE)
    }

    fn create_with_sequence(target: &Path, sequence: &AtomicU64) -> io::Result<(Self, File)> {
        let target_name = target
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "latch 文件名缺失"))?;
        for _ in 0..64 {
            let next = sequence
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    value.checked_add(1)
                })
                .map_err(|_| io::Error::other("Clear 临时文件序号耗尽"))?;
            let mut name = target_name.to_os_string();
            name.push(format!(".clear_tmp.{}.{next}", std::process::id()));
            let path = target.with_file_name(name);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o640)
                .open(&path)
            {
                Ok(file) => return Ok((Self { path }, file)),
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

impl Drop for TemporaryLatch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests;
