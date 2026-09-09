//! 事务记录、文件身份与按值消费的凭证族。
//!
//! 凭证全部非 `Clone`、字段私有；每个凭证绑定目标身份摘要、marker 的
//! `(dev, ino)` 与所属目录的 `(dev, ino)`。凭证由真实系统调用成功形成；
//! 失败入口按实际已完成的资源步骤返还凭证或明确失权，调用方不得凭
//! 路径或可读记录重建它们。

use std::fmt;
use std::fs::{File, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;

use super::guard::StageError;

pub(super) const MAGIC: &[u8; 8] = b"RBTCLRTX";
pub(super) const VERSION: u16 = 1;
pub(super) const RECORD_BYTES: usize = 112;
pub(super) const CHECKSUM_OFFSET: usize = 80;

/// 文件系统对象身份：`st_dev` 与 `st_ino`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    dev: u64,
    ino: u64,
}

impl FileIdentity {
    pub(super) fn of(metadata: &Metadata) -> Self {
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
}

/// 完整事务身份。字段只读；它既不是 durability 证明，也不是清锁许可。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClearTxnRecord {
    pub(super) target_identity: [u8; 32],
    pub(super) txn_id: u128,
    pub(super) expected_epoch: u64,
    pub(super) original_latched: bool,
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

    pub(super) fn encode(self) -> [u8; RECORD_BYTES] {
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

    pub(super) fn decode(bytes: &[u8], target_identity: [u8; 32]) -> io::Result<Self> {
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

/// 凭证共有的绑定信息：记录、marker 身份与目录身份。
#[derive(Debug, Clone, Copy)]
pub(super) struct Binding {
    pub(super) record: ClearTxnRecord,
    pub(super) marker: FileIdentity,
    pub(super) directory: FileIdentity,
}

/// `inspect_pending` 的只读结果；不能用它直接构造已准备或已修复凭证，
/// 也不实现删除凭证。
#[derive(Debug)]
pub struct PendingClear {
    pub(super) binding: Binding,
}

impl PendingClear {
    #[must_use]
    pub const fn record(&self) -> &ClearTxnRecord {
        &self.binding.record
    }

    /// 检查时 marker 的文件身份；合法可读不等于 marker 已 durable。
    #[must_use]
    pub const fn identity(&self) -> FileIdentity {
        self.binding.marker
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
/// use robot_s0::clear_estop_transaction::{ClearTxnStore, NoGuard, PreparedClear};
/// fn remove_twice(store: &mut ClearTxnStore, prepared: PreparedClear) {
///     store.remove(prepared, &mut NoGuard).unwrap();
///     store.remove(prepared, &mut NoGuard).unwrap();
/// }
/// ```
#[derive(Debug)]
pub struct PreparedClear {
    pub(super) binding: Binding,
}

/// 标记完整且已严格同步，但双份 `0` 尚未全部落盘：pair 可能部分为 `0`。
#[derive(Debug)]
pub struct DurableMarker {
    pub(super) binding: Binding,
}

/// 指定修复目标已严格落盘，标记仍存在；删除时继续核完整事务身份。
///
/// ```compile_fail
/// use robot_s0::clear_estop_transaction::RepairedClear;
/// fn duplicate(repaired: RepairedClear) { let _ = repaired.clone(); }
/// ```
#[derive(Debug)]
pub struct RepairedClear {
    pub(super) binding: Binding,
}

/// 本 store 以 `create_new` 建立的 marker，其后的身份采样、写入或同步失败：
/// pair 未动，文件内容可能为空、半写或完整但未 durable。`open` 一成功即形成
/// 本凭证；持有创建时得到的可读写活 fd，阻止 inode 被释放复用。marker 的
/// `(dev, ino)` 由独立的 `SampleMarkerIdentity` 步骤采样，采样失败时为 `None`，
/// 由活 fd 本身承担身份，不填伪造值。只能经
/// [`super::ClearTxnStore::discard_created`] 收敛。
///
/// ```compile_fail
/// use robot_s0::clear_estop_transaction::CreatedMarker;
/// fn duplicate(created: CreatedMarker) { let _ = created.clone(); }
/// ```
#[derive(Debug)]
pub struct CreatedMarker {
    pub(super) record: ClearTxnRecord,
    pub(super) directory: FileIdentity,
    pub(super) marker: Option<FileIdentity>,
    pub(super) file: File,
}

/// unlink 已发生、目录同步失败：marker 可见性未知，只欠对原目录的同步。
#[derive(Debug)]
pub struct UnsyncedRemoval {
    pub(super) record: ClearTxnRecord,
    pub(super) directory: FileIdentity,
}

impl UnsyncedRemoval {
    #[must_use]
    pub const fn record(&self) -> &ClearTxnRecord {
        &self.record
    }

    /// 发生 unlink 的目录身份；重同步只能针对该目录。
    #[must_use]
    pub const fn directory_identity(&self) -> FileIdentity {
        self.directory
    }
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::PreparedClear {}
    impl Sealed for super::DurableMarker {}
    impl Sealed for super::RepairedClear {}
}

/// 表示"marker 完整、已 durable、记录与身份已知"的凭证；密封，只有
/// [`PreparedClear`]、[`DurableMarker`]、[`RepairedClear`] 实现。
pub trait MarkerCredential: sealed::Sealed + fmt::Debug + Send {
    fn record(&self) -> &ClearTxnRecord;
    fn marker_identity(&self) -> FileIdentity;
    fn directory_identity(&self) -> FileIdentity;
}

macro_rules! impl_marker_credential {
    ($($name:ident),*) => {$(
        impl $name {
            #[must_use]
            pub const fn record(&self) -> &ClearTxnRecord {
                &self.binding.record
            }

            /// marker 文件身份；删除与修复前都会重新核对。
            #[must_use]
            pub const fn identity(&self) -> FileIdentity {
                self.binding.marker
            }
        }

        impl MarkerCredential for $name {
            fn record(&self) -> &ClearTxnRecord {
                &self.binding.record
            }

            fn marker_identity(&self) -> FileIdentity {
                self.binding.marker
            }

            fn directory_identity(&self) -> FileIdentity {
                self.binding.directory
            }
        }
    )*};
}

impl_marker_credential!(PreparedClear, DurableMarker, RepairedClear);

impl CreatedMarker {
    #[must_use]
    pub const fn record(&self) -> &ClearTxnRecord {
        &self.record
    }

    /// 已采样的 marker 身份；`None` 表示采样步骤失败前就已中断，身份仍由活 fd 承担。
    #[must_use]
    pub const fn identity(&self) -> Option<FileIdentity> {
        self.marker
    }

    #[must_use]
    pub const fn directory_identity(&self) -> FileIdentity {
        self.directory
    }
}

/// `prepare_clear` 失败时返还的所有权。
#[derive(Debug)]
pub enum PrepareFailureToken {
    /// 本次未取得 marker 所有权；不证明磁盘上没有 marker。
    NotOwned,
    /// marker 已由本 store 创建，pair 未动。
    Created(CreatedMarker),
    /// marker 已 durable，pair 可能部分为 `0`。
    Durable(DurableMarker),
}

/// `prepare_clear` 的失败：真实阶段 + 已取得的所有权。
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct PrepareError {
    #[source]
    pub error: StageError,
    pub token: PrepareFailureToken,
}

/// 修复失败：`token` 为 `Some` 时凭证仍有效（marker 未动或 pair 部分写入），
/// `None` 表示已确认记录/身份不符或 marker 真实缺失。
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct RepairError<C: fmt::Debug> {
    #[source]
    pub error: StageError,
    pub token: Option<C>,
}

/// 删除失败时的所有权事实。
#[derive(Debug)]
pub enum RemoveFailureToken<C> {
    /// unlink 尚未发生，凭证原样返还。
    Retained(C),
    /// unlink 已发生，仍欠目录同步。
    Unsynced(UnsyncedRemoval),
    /// 已确认记录/身份不符或 marker 真实缺失，凭证失效。
    Lost,
}

/// 删除或重同步失败：真实阶段 + 所有权事实。
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct RemoveError<C: fmt::Debug> {
    #[source]
    pub error: StageError,
    pub token: RemoveFailureToken<C>,
}
