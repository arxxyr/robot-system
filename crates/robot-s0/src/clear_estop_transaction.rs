//! `ClearEStop` 的纯文件事务：先建立持久标记，再准备双份未锁存值；失败按
//! 真实已完成的文件步骤返还所有权凭证。
//!
//! 本模块只供普通磁盘 writer 使用，尚未接入生产启动或控制链。它不判断
//! 当前安全代次，也不授予清锁、使能或回复成功的权限。删除标记之前，上层
//! 必须已经持有当前事务的 Finalize 许可；磁盘操作与内存提交并非原子操作。
//!
//! ## 目录 fd 与身份
//!
//! [`ClearTxnStore::open`] 打开 canonical 父目录并保留其活 fd 与 `(dev, ino)`；
//! 之后所有读写、创建、rename、unlink、恢复、自愈、临时文件清理都相对这个
//! fd 进行，目录同步只 `fsync` 这个 fd，不按路径重开父目录。父目录被改名或
//! 原路径被替换后，操作对象仍是打开时的那个目录。单 writer、目标锁与目录内
//! 无并发改名者仍是接线前提；本模块不安装目标锁，也不宣称抗敌对目录竞态。
//!
//! ## 三相与凭证
//!
//! 每个文件步骤按 `Before`（[`StepGuard`]）→ `Operation`（真实系统调用）→
//! `After`（仅测试的后置检查点）执行，失败以 [`StageError`] 记录阶段与相位。
//! 凭证只随真实 `Operation` 成功推进：`create_new` 得到文件即
//! [`CreatedMarker`]，标记目录同步成功才 [`DurableMarker`]，unlink 成功即
//! [`UnsyncedRemoval`]；`Before` 相中断不改资源、返还原凭证，`After` 相失败
//! 返还已经推进的凭证。只有真实 `NotFound` 或确认记录 / 身份 / 内容不符才失权。
//!
//! ## 凭证身份的适用前提与识别局限
//!
//! [`CreatedMarker`] 持有活 fd，其 inode 在凭证存续期间不会被释放复用。
//! [`PreparedClear`] / [`DurableMarker`] / [`RepairedClear`] 不持 fd，只携带完整
//! 记录（目标摘要、唯一 txn、epoch、原锁存值与校验）和创建时的 `(dev, ino)`；它们
//! 在"完整记录 + 上层保证 txn 唯一 + 单 writer + 目录内无并发改名者"的前提下
//! 使用，`FileIdentity` 只是附加核对。本模块**不宣称**抵抗 inode ABA：文件系统
//! 在 unlink 后可能立即复用 inode 号，若同一唯一事务的原记录被外部逐字节复刻
//! 且 inode 号复用，文件层无法识别；相同可读字节既不证明新文件已 durable，也
//! 不证明原物理对象仍在。该交错不属于当前受支持的生产前提；不同 txn / epoch /
//! 原锁存值的旧凭证仍由完整记录比较拒绝。
//!
//! ## 只读双份核验（W0.1）
//!
//! [`ClearTxnStore::inspect_pair`] 在一个 [`ClearTxnStage::InspectPair`] 步骤内相对
//! 绑定目录 fd 依次读取 primary、backup，返回 [`PairView`]：值 / 真实缺失 / 坏值，
//! 附打开时采样的目录身份。它不写、不自愈、不同步、不碰标记、不产生或消费凭证，
//! 供上层在不借用会自愈或恢复标记的 `recover_and_load` 的情况下核验真实双份。
//!
//! 调用方必须保证目标只有一个 writer，并持有部署层的排他锁。非 `Clone`
//! 的 store 与按值消费的凭证只约束本实例，不能代替跨实例或跨进程互斥。

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

mod credential;
mod dirfd;
mod guard;

#[cfg(test)]
mod tests;
#[cfg(all(test, feature = "test-comm-fault-hook"))]
mod w4_tests;

pub use credential::{
    ClearTxnRecord, CreatedMarker, DurableMarker, FileIdentity, MarkerCredential, PendingClear,
    PrepareError, PrepareFailureToken, PreparedClear, RemoveError, RemoveFailureToken, RepairError,
    RepairedClear, UnsyncedRemoval,
};
#[cfg(feature = "test-comm-fault-hook")]
pub use guard::{AliasPreparation, DirectorySyncEvent, DirectorySyncInput};
pub use guard::{GuardInterrupt, InterruptReason, NoGuard, StageError, StepGuard, StepPhase};

use credential::{Binding, RECORD_BYTES};
use dirfd::{DirHandle, TemporaryLatch};

#[cfg(test)]
use credential::CHECKSUM_OFFSET;

const IDENTITY_DOMAIN: &str = "robot-system clear-estop target identity v1";

/// 仅从本次真实 Pending 借读生成；这些字节不携带修复或删除权限。
#[cfg(feature = "test-comm-fault-hook")]
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecoveryObservation {
    pub(crate) directory: FileIdentity,
    pub(crate) marker: FileIdentity,
    pub(crate) txn_id: u128,
    pub(crate) expected_epoch: u64,
    pub(crate) original_latched: bool,
    pub(crate) record: [u8; RECORD_BYTES],
}

/// 失败所在文件步骤；目录同步失败不意味着此前的 rename 或 unlink 未发生。
/// 声明顺序即执行顺序，可按序比较。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ClearTxnStage {
    OpenStore,
    /// 凭证与本 store 的目标 / 目录绑定核对；不做任何文件操作。
    ValidateCredential,
    ReadMarker,
    ValidateMarker,
    CreateMarker,
    /// 对刚创建的 marker 活 fd 采样 `(dev, ino)`；失败仍返还持活 fd 的 Created。
    SampleMarkerIdentity,
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
    /// W0.1 只读双份核验：一个步骤内顺序读取 primary、backup，不写不自愈。
    InspectPair,
}

/// W0.1：单份 latch 文件的只读核验结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatchCopyState {
    /// `nlink == 1` 的普通文件，内容为合法 `0` / `1`（可带一个换行）。
    Value(bool),
    /// 打开时真实 `ENOENT`。
    Missing,
    /// 普通文件但内容不是合法值（含空文件）。
    Corrupt,
}

/// W0.1：一次 [`ClearTxnStage::InspectPair`] 步骤读取到的双份状态与绑定目录身份。
///
/// 它不是 durability 证明、不是清锁许可，也不承诺外部并发改名下的原子快照；
/// 两份按 primary → backup 顺序各读一次。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairView {
    pub primary: LatchCopyState,
    pub backup: LatchCopyState,
    /// store 打开时采样的目录身份（读取相对该 fd 进行，父目录被替换也不变）。
    pub directory_identity: FileIdentity,
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

/// 相对目录 fd 的三个文件名。
#[derive(Debug)]
struct Layout {
    latch: OsString,
    backup: OsString,
    marker: OsString,
}

#[cfg(test)]
type AfterStep = Option<Box<dyn FnMut(ClearTxnStage) -> io::Result<()> + Send>>;

/// 单 writer 的纯文件事务存储。构造只打开目录，不读取或修复 latch。
pub struct ClearTxnStore {
    dir: DirHandle,
    layout: Layout,
    target_identity: [u8; 32],
    /// 仅供诊断与测试；文件操作一律相对目录 fd。
    latch_path: PathBuf,
    marker_path: PathBuf,
    #[cfg(test)]
    after_step: AfterStep,
    /// 仅测试：让指定步骤的 `Operation` 相在不执行系统调用的情况下返回 `EIO`，
    /// 用于确定性注入"前一步骤已真实成功、本步骤系统调用失败"。
    #[cfg(test)]
    fail_operation: Option<(ClearTxnStage, usize)>,
}

impl std::fmt::Debug for ClearTxnStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClearTxnStore")
            .field("dir", &self.dir)
            .field("layout", &self.layout)
            .field("latch_path", &self.latch_path)
            .finish_non_exhaustive()
    }
}

impl ClearTxnStore {
    /// 身份绑定已存在父目录的规范路径与原文件名，不依赖 latch 的存在或 inode；
    /// 同时打开该目录并保留活 fd。
    pub fn open(latch_path: &Path, robot_id: &str, hardware_id: &str) -> Result<Self, StageError> {
        let open_error = |source| StageError::operation(ClearTxnStage::OpenStore, source);
        let name = latch_path.file_name().ok_or_else(|| {
            open_error(io::Error::new(
                io::ErrorKind::InvalidInput,
                "latch 路径必须包含文件名",
            ))
        })?;
        let parent = latch_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let canonical_parent = std::fs::canonicalize(parent).map_err(open_error)?;
        let dir = DirHandle::open(&canonical_parent).map_err(open_error)?;
        let canonical = canonical_parent.join(name);
        let mut hasher = blake3::Hasher::new_derive_key(IDENTITY_DOMAIN);
        for bytes in [
            robot_id.as_bytes(),
            hardware_id.as_bytes(),
            canonical.as_os_str().as_encoded_bytes(),
        ] {
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        }
        let mut backup = name.to_os_string();
        backup.push(".bak");
        let mut marker = name.to_os_string();
        marker.push(".clear_pending");
        let marker_path = canonical.with_file_name(&marker);
        Ok(Self {
            dir,
            layout: Layout {
                latch: name.to_os_string(),
                backup,
                marker,
            },
            target_identity: *hasher.finalize().as_bytes(),
            latch_path: canonical,
            marker_path,
            #[cfg(test)]
            after_step: None,
            #[cfg(test)]
            fail_operation: None,
        })
    }

    /// 打开时采样的目录身份；凭证跨 store 使用时据此拒绝。
    #[must_use]
    pub const fn directory_identity(&self) -> FileIdentity {
        self.dir.identity()
    }

    /// 规范 latch 路径，仅供诊断；不用于任何文件操作。
    #[must_use]
    pub fn latch_path(&self) -> &Path {
        &self.latch_path
    }

    /// 规范标记路径，仅供诊断；不用于任何文件操作。
    #[must_use]
    pub fn marker_path(&self) -> &Path {
        &self.marker_path
    }

    /// 只读校验，合法标记可能尚未经过任何同步；错误时不触发 latch 自愈。
    pub fn inspect_pending(&mut self) -> Result<Option<PendingClear>, StageError> {
        let mut guard = NoGuard;
        let directory = self.dir.identity();
        let mut engine = self.engine(&mut guard);
        let file = match engine.open_marker() {
            Ok(file) => file,
            Err(failure) => {
                let error = failure.into_error();
                if error.phase == StepPhase::Operation
                    && error.source.kind() == io::ErrorKind::NotFound
                {
                    return Ok(None);
                }
                return Err(error);
            }
        };
        let (record, marker) = engine.read_record(&file).map_err(StepFailure::into_error)?;
        Ok(Some(PendingClear {
            binding: Binding {
                record,
                marker,
                directory,
            },
        }))
    }

    /// 建立排他事务标记并严格准备双份 `0`。事务 ID 的唯一性由上层保证。
    ///
    /// 失败时按已完成的真实步骤返还所有权：`NotOwned` / [`CreatedMarker`] /
    /// [`DurableMarker`]。
    #[allow(
        clippy::result_large_err,
        reason = "错误变体按设计携带被返还的凭证本体；非实时路径，装箱只会掩盖所有权"
    )]
    pub fn prepare_clear(
        &mut self,
        txn_id: u128,
        expected_epoch: u64,
        original_latched: bool,
        guard: &mut dyn StepGuard,
    ) -> Result<PreparedClear, PrepareError> {
        if txn_id == 0 {
            return Err(PrepareError {
                error: StageError::operation(
                    ClearTxnStage::ValidateMarker,
                    io::Error::new(io::ErrorKind::InvalidInput, "Clear 事务 ID 不得为零"),
                ),
                token: PrepareFailureToken::NotOwned,
            });
        }
        let record = ClearTxnRecord {
            target_identity: self.target_identity,
            txn_id,
            expected_epoch,
            original_latched,
        };
        self.engine(guard).prepare(record)
    }

    /// 消费"标记完整且 durable"的凭证，完整核对当前标记后按目标修复；标记保留。
    ///
    /// `Before` 中断、临时 IO 错误或 latch 部分写入后凭证原样返还；只有真实
    /// `NotFound` 或确认记录 / 身份不符才失权。
    pub fn repair<C: MarkerCredential>(
        &mut self,
        credential: C,
        target: RepairTarget,
        guard: &mut dyn StepGuard,
    ) -> Result<RepairedClear, RepairError<C>> {
        if let Err(error) = self.check_binding(credential.record(), credential.directory_identity())
        {
            return Err(RepairError {
                error,
                token: Some(credential),
            });
        }
        let record = *credential.record();
        let marker = credential.marker_identity();
        let directory = self.dir.identity();
        let mut engine = self.engine(guard);
        if let Err(failure) = engine.matching_marker(&record, marker) {
            let lost = failure.confirms_loss();
            return Err(RepairError {
                error: failure.into_error(),
                token: (!lost).then_some(credential),
            });
        }
        if let Err(error) = engine.write_pair(target.latched(record)) {
            return Err(RepairError {
                error,
                token: Some(credential),
            });
        }
        Ok(RepairedClear {
            binding: Binding {
                record,
                marker,
                directory,
            },
        })
    }

    /// 消费凭证后清理标记。调用方必须已持有匹配的 Finalize 许可。
    ///
    /// unlink 前失败返还凭证；unlink 已发生但目录同步失败返还
    /// [`UnsyncedRemoval`]，此后只能重同步，绝不再次 unlink。
    pub fn remove<C: MarkerCredential>(
        &mut self,
        credential: C,
        guard: &mut dyn StepGuard,
    ) -> Result<(), RemoveError<C>> {
        if let Err(error) = self.check_binding(credential.record(), credential.directory_identity())
        {
            return Err(RemoveError {
                error,
                token: RemoveFailureToken::Retained(credential),
            });
        }
        let record = *credential.record();
        let marker = credential.marker_identity();
        let mut engine = self.engine(guard);
        if let Err(failure) = engine.matching_marker(&record, marker) {
            let lost = failure.confirms_loss();
            return Err(RemoveError {
                error: failure.into_error(),
                token: if lost {
                    RemoveFailureToken::Lost
                } else {
                    RemoveFailureToken::Retained(credential)
                },
            });
        }
        engine.unlink_and_sync(record, credential)
    }

    /// 收敛本 store 以 `create_new` 建立、之后写入或同步失败的标记。
    ///
    /// 先核目标与目录，再核活 fd 为普通文件、`nlink == 1`、长度 `<= 112`，
    /// 从偏移 0 读取完整内容并确认 EOF，内容必须是本次记录编码的前缀；最后核
    /// 相对路径仍指向同一 inode。任何确认不符都拒绝且不 unlink；临时 IO 错误
    /// 返还原凭证。
    #[allow(
        clippy::result_large_err,
        reason = "错误变体按设计携带被返还的凭证本体（含活 fd）；非实时路径"
    )]
    pub fn discard_created(
        &mut self,
        credential: CreatedMarker,
        guard: &mut dyn StepGuard,
    ) -> Result<(), RemoveError<CreatedMarker>> {
        if let Err(error) = self.check_binding(credential.record(), credential.directory_identity())
        {
            return Err(RemoveError {
                error,
                token: RemoveFailureToken::Retained(credential),
            });
        }
        let record = *credential.record();
        let mut engine = self.engine(guard);
        if let Err(failure) = engine.verify_created(&credential.file, &record) {
            let lost = failure.confirms_loss();
            return Err(RemoveError {
                error: failure.into_error(),
                token: if lost {
                    RemoveFailureToken::Lost
                } else {
                    RemoveFailureToken::Retained(credential)
                },
            });
        }
        engine.unlink_and_sync(record, credential)
    }

    /// 只重试发生 unlink 的那个目录的同步；失败保留凭证，绝不再次 unlink，
    /// 也不凭当前路径不存在宣布 strict 完成。
    pub fn resync_removal(
        &mut self,
        credential: UnsyncedRemoval,
        guard: &mut dyn StepGuard,
    ) -> Result<(), RemoveError<UnsyncedRemoval>> {
        if let Err(error) = self.check_binding(credential.record(), credential.directory_identity())
        {
            return Err(RemoveError {
                error,
                token: RemoveFailureToken::Retained(credential),
            });
        }
        let mut engine = self.engine(guard);
        match engine.sync_directory(ClearTxnStage::SyncRemovalDirectory) {
            Ok(()) => Ok(()),
            Err(error) => Err(RemoveError {
                error,
                token: RemoveFailureToken::Retained(credential),
            }),
        }
    }

    /// W0.1 只读双份核验：一个 [`ClearTxnStage::InspectPair`] 步骤内先守门恰一次，
    /// 再在 `Operation` 相相对绑定目录 fd 依次读取 primary、backup（每份
    /// `O_RDONLY | O_NOFOLLOW | O_NONBLOCK` 打开，同一 fd 先核普通文件且
    /// `nlink == 1` 再读前缀解析），两份都成功后才执行测试 `After` 检查点。
    ///
    /// 仅 open 的真实 `ENOENT` 记为 [`LatchCopyState::Missing`]；普通文件的坏字节
    /// 记为 [`LatchCopyState::Corrupt`]；权限、`ELOOP`、非普通文件 / `nlink != 1`
    /// （`InvalidInput`）、fstat / read 等真实错误直接以 [`StageError`] 返回，不返回
    /// 部分结果。绝不写入、创建、删除、同步、自愈，不读取或改变标记，不产生或
    /// 消费任何凭证；不承诺外部并发改名下的原子快照。
    ///
    /// # Errors
    ///
    /// 见 [`StageError`]：守门拒绝为 `Before`，真实 IO / 对象错误为 `Operation`，
    /// 测试后置检查点失败为 `After`。
    pub fn inspect_pair(&mut self, guard: &mut dyn StepGuard) -> Result<PairView, StageError> {
        let directory_identity = self.dir.identity();
        let (primary, backup) = self.engine(guard).inspect_pair()?;
        Ok(PairView {
            primary,
            backup,
            directory_identity,
        })
    }

    /// 写 1：primary `1` → backup `1`，全部严格同步；供急停持久化义务与无
    /// 标记事务的 `Latched` 修复使用。
    pub fn persist_latched(&mut self, guard: &mut dyn StepGuard) -> Result<(), StageError> {
        self.engine(guard).write_pair(true)
    }

    /// 先检查事务标记，再决定是否允许沿用旧 loader 的读取与自愈逻辑。
    ///
    /// 有效 pending 标记 → strict 双 `1` → 清理标记 → `true`；坏 / 身份不符 /
    /// 读错直接拒绝且不自愈。无标记时保持原 primary / backup / 缺失语义，
    /// 自愈回写同样相对目录 fd 完成。
    #[allow(
        clippy::single_match_else,
        reason = "按项目约定显式列出 Option 两类恢复路径"
    )]
    pub fn recover_and_load(&mut self) -> Result<bool, StageError> {
        match self.inspect_pending()? {
            Some(pending) => {
                #[cfg(feature = "test-comm-fault-hook")]
                crate::comm_fault_hook::w4_recovery_kill::bind_pending(RecoveryObservation {
                    directory: pending.binding.directory,
                    marker: pending.identity(),
                    txn_id: pending.record().txn_id(),
                    expected_epoch: pending.record().expected_epoch(),
                    original_latched: pending.record().original_latched(),
                    record: pending.record().encode(),
                });
                let repaired = self.repair_pending(pending, RepairTarget::Latched)?;
                let mut guard = NoGuard;
                self.remove(repaired, &mut guard)
                    .map_err(|failure| failure.error)?;
                Ok(true)
            }
            None => {
                let mut guard = NoGuard;
                self.engine(&mut guard).load_latch()
            }
        }
    }

    /// 从可读记录恢复修复流程；先重核并同步标记，不能凭可读状态直接写 `0`。
    /// 仅启动恢复使用；运行期修复必须持有 [`MarkerCredential`]。
    #[allow(
        clippy::needless_pass_by_value,
        reason = "修复必须消费本次检查结果，禁止重复使用旧检查"
    )]
    pub(crate) fn repair_pending(
        &mut self,
        pending: PendingClear,
        target: RepairTarget,
    ) -> Result<RepairedClear, StageError> {
        let record = *pending.record();
        let marker = pending.identity();
        let directory = self.dir.identity();
        self.check_binding(&record, pending.binding.directory)?;
        let mut guard = NoGuard;
        let mut engine = self.engine(&mut guard);
        let file = engine
            .matching_marker(&record, marker)
            .map_err(StepFailure::into_error)?;
        engine.run_flat(ClearTxnStage::SyncMarkerFile, || file.sync_all())?;
        engine.sync_directory(ClearTxnStage::SyncMarkerDirectory)?;
        engine.write_pair(target.latched(record))?;
        Ok(RepairedClear {
            binding: Binding {
                record,
                marker,
                directory,
            },
        })
    }

    fn check_binding(
        &self,
        record: &ClearTxnRecord,
        directory: FileIdentity,
    ) -> Result<(), StageError> {
        if record.target_identity != self.target_identity || directory != self.dir.identity() {
            return Err(StageError::operation(
                ClearTxnStage::ValidateCredential,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "凭证不属于本 store 的目标或目录",
                ),
            ));
        }
        Ok(())
    }

    fn engine<'a>(&'a mut self, guard: &'a mut dyn StepGuard) -> Engine<'a> {
        Engine {
            dir: &self.dir,
            layout: &self.layout,
            target_identity: self.target_identity,
            ctx: StepContext {
                guard,
                #[cfg(test)]
                after_step: &mut self.after_step,
                #[cfg(test)]
                fail_operation: &mut self.fail_operation,
            },
        }
    }
}

/// 一个文件步骤的失败；`After` 携带已经成功的操作结果。
enum StepFailure<T> {
    Before(StageError),
    Operation(StageError),
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "操作成功后注入失败仅用于测试，生产构建共用相同的恢复分支"
        )
    )]
    After(StageError, T),
}

impl<T> StepFailure<T> {
    fn into_error(self) -> StageError {
        match self {
            Self::Before(error) | Self::Operation(error) | Self::After(error, _) => error,
        }
    }

    /// 丢弃 `After` 相携带的操作结果，只保留失败事实。
    fn discard_value(self) -> StepFailure<()> {
        match self {
            Self::Before(error) => StepFailure::Before(error),
            Self::Operation(error) => StepFailure::Operation(error),
            Self::After(error, _) => StepFailure::After(error, ()),
        }
    }

    /// 仅真实 `Operation` 失败中的 `NotFound`、符号链接、类型 / 记录 / 身份
    /// 不符才证明凭证已失效。
    fn confirms_loss(&self) -> bool {
        let Self::Operation(error) = self else {
            return false;
        };
        let source = &error.source;
        let is_loop = source.raw_os_error() == Some(libc::ELOOP);
        match error.stage {
            ClearTxnStage::ReadMarker | ClearTxnStage::ValidateMarker => {
                is_loop
                    || matches!(
                        source.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::InvalidData
                    )
            }
            ClearTxnStage::RemoveMarker => source.kind() == io::ErrorKind::NotFound,
            _ => false,
        }
    }
}

/// 守门与测试后置检查点；真实操作永远先于 `After` 相的 hook。
struct StepContext<'a> {
    guard: &'a mut dyn StepGuard,
    #[cfg(test)]
    after_step: &'a mut AfterStep,
    #[cfg(test)]
    fail_operation: &'a mut Option<(ClearTxnStage, usize)>,
}

impl StepContext<'_> {
    fn run<T>(
        &mut self,
        stage: ClearTxnStage,
        operation: impl FnOnce() -> io::Result<T>,
    ) -> Result<T, StepFailure<T>> {
        self.run_with_guard(stage, |_| operation())
    }

    fn run_directory_sync(
        &mut self,
        stage: ClearTxnStage,
        dir: &DirHandle,
    ) -> Result<(), StepFailure<()>> {
        self.run_with_guard(stage, |guard| {
            #[cfg(not(feature = "test-comm-fault-hook"))]
            let _ = guard;
            #[cfg(feature = "test-comm-fault-hook")]
            match guard.directory_sync_input(stage) {
                DirectorySyncInput::Original => {}
                DirectorySyncInput::OPathAlias => {
                    #[cfg(target_os = "linux")]
                    return dir.sync_with_alias_input(|event| {
                        guard.directory_sync_event(stage, event);
                    });
                    #[cfg(not(target_os = "linux"))]
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "W4 目录别名故障仅支持 Linux",
                    ));
                }
            }
            dir.sync()
        })
    }

    fn run_with_guard<T>(
        &mut self,
        stage: ClearTxnStage,
        operation: impl FnOnce(&mut dyn StepGuard) -> io::Result<T>,
    ) -> Result<T, StepFailure<T>> {
        if let Err(interrupt) = self.guard.before(stage) {
            return Err(StepFailure::Before(StageError::new(
                stage,
                StepPhase::Before,
                interrupt.into_io_error(),
            )));
        }
        #[cfg(test)]
        if let Some((failing, remaining)) = self.fail_operation.as_mut()
            && *failing == stage
            && *remaining > 0
        {
            *remaining -= 1;
            return Err(StepFailure::Operation(StageError::new(
                stage,
                StepPhase::Operation,
                io::Error::from_raw_os_error(libc::EIO),
            )));
        }
        let value = operation(self.guard).map_err(|error| {
            StepFailure::Operation(StageError::new(stage, StepPhase::Operation, error))
        })?;
        #[cfg(feature = "test-comm-fault-hook")]
        self.guard.after_success(stage);
        #[cfg(test)]
        if let Some(after_step) = self.after_step.as_mut()
            && let Err(error) = after_step(stage)
        {
            return Err(StepFailure::After(
                StageError::new(stage, StepPhase::After, error),
                value,
            ));
        }
        Ok(value)
    }
}

/// 一次公开操作内的借用集合：目录 fd、名字、目标身份与步骤上下文。
struct Engine<'a> {
    dir: &'a DirHandle,
    layout: &'a Layout,
    target_identity: [u8; 32],
    ctx: StepContext<'a>,
}

impl<'a> Engine<'a> {
    fn run_flat<T>(
        &mut self,
        stage: ClearTxnStage,
        operation: impl FnOnce() -> io::Result<T>,
    ) -> Result<T, StageError> {
        self.ctx
            .run(stage, operation)
            .map_err(StepFailure::into_error)
    }

    fn sync_directory(&mut self, stage: ClearTxnStage) -> Result<(), StageError> {
        self.ctx
            .run_directory_sync(stage, self.dir)
            .map_err(StepFailure::into_error)
    }

    /// 不跟随链接、不在 FIFO 上等待地打开标记；非普通文件视为确认不符。
    fn open_marker(&mut self) -> Result<File, StepFailure<File>> {
        let dir = self.dir;
        let layout = self.layout;
        let name = &layout.marker;
        self.ctx.run(ClearTxnStage::ReadMarker, || {
            let file = dir.open_at(name, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK)?;
            if !file.metadata()?.file_type().is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Clear 事务标记必须是普通文件",
                ));
            }
            Ok(file)
        })
    }

    /// 有界读取并校验记录，同时采样该文件的身份。
    fn read_record(
        &mut self,
        file: &File,
    ) -> Result<(ClearTxnRecord, FileIdentity), StepFailure<(ClearTxnRecord, FileIdentity)>> {
        let target_identity = self.target_identity;
        self.ctx.run(ClearTxnStage::ValidateMarker, || {
            let mut bytes = [0; RECORD_BYTES + 1];
            let len = read_prefix_at(file, &mut bytes)?;
            let record = ClearTxnRecord::decode(&bytes[..len], target_identity)?;
            let identity = FileIdentity::of(&file.metadata()?);
            Ok((record, identity))
        })
    }

    /// 打开并核对当前标记与凭证的记录、身份完全一致。
    fn matching_marker(
        &mut self,
        expected: &ClearTxnRecord,
        marker: FileIdentity,
    ) -> Result<File, StepFailure<File>> {
        let file = self.open_marker()?;
        let target_identity = self.target_identity;
        match self.ctx.run(ClearTxnStage::ValidateMarker, || {
            let mut bytes = [0; RECORD_BYTES + 1];
            let len = read_prefix_at(&file, &mut bytes)?;
            let actual = ClearTxnRecord::decode(&bytes[..len], target_identity)?;
            if actual != *expected || FileIdentity::of(&file.metadata()?) != marker {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Clear 事务标记已不属于当前凭证",
                ));
            }
            Ok(())
        }) {
            Ok(()) => Ok(file),
            Err(StepFailure::Before(error)) => Err(StepFailure::Before(error)),
            Err(StepFailure::Operation(error)) => Err(StepFailure::Operation(error)),
            Err(StepFailure::After(error, ())) => Err(StepFailure::After(error, file)),
        }
    }

    /// `discard_created` 的核对：活 fd 的类型 / `nlink` / 长度、完整内容前缀与
    /// 路径身份。
    fn verify_created(
        &mut self,
        file: &File,
        record: &ClearTxnRecord,
    ) -> Result<(), StepFailure<()>> {
        let metadata = self
            .ctx
            .run(ClearTxnStage::ReadMarker, || {
                let metadata = file.metadata()?;
                if !metadata.file_type().is_file()
                    || metadata.nlink() != 1
                    || metadata.len() > RECORD_BYTES as u64
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "已创建的标记不再是本次建立的普通文件",
                    ));
                }
                Ok(metadata)
            })
            .map_err(StepFailure::discard_value)?;
        let expected = record.encode();
        let dir = self.dir;
        let layout = self.layout;
        let name = &layout.marker;
        self.ctx.run(ClearTxnStage::ValidateMarker, || {
            let mut bytes = [0; RECORD_BYTES + 1];
            let len = read_prefix_at(file, &mut bytes)?;
            if u64::try_from(len)
                .ok()
                .is_none_or(|read| read != metadata.len())
                || len > RECORD_BYTES
                || bytes[..len] != expected[..len]
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "已创建的标记内容不是本次记录编码的前缀",
                ));
            }
            // 身份以活 fd 此刻的 fstat 为准，不依赖创建时是否采样成功。
            let (identity, kind) = dir.identity_at_nofollow(name)?;
            if !kind.is_file() || identity != FileIdentity::of(&metadata) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "标记路径已不再指向本次创建的 inode",
                ));
            }
            Ok(())
        })
    }

    /// 核对通过后的 unlink + 目录同步；三相分别映射为 Retained / Lost / Unsynced。
    fn unlink_and_sync<C>(
        &mut self,
        record: ClearTxnRecord,
        credential: C,
    ) -> Result<(), RemoveError<C>>
    where
        C: std::fmt::Debug,
    {
        let dir = self.dir;
        let layout = self.layout;
        let name = &layout.marker;
        let unsynced = || UnsyncedRemoval {
            record,
            directory: dir.identity(),
        };
        match self
            .ctx
            .run(ClearTxnStage::RemoveMarker, || dir.unlink_at(name))
        {
            Ok(()) => {}
            Err(StepFailure::Before(error)) => {
                return Err(RemoveError {
                    error,
                    token: RemoveFailureToken::Retained(credential),
                });
            }
            Err(StepFailure::Operation(error)) => {
                let token = if error.source.kind() == io::ErrorKind::NotFound {
                    RemoveFailureToken::Lost
                } else {
                    RemoveFailureToken::Retained(credential)
                };
                return Err(RemoveError { error, token });
            }
            Err(StepFailure::After(error, ())) => {
                return Err(RemoveError {
                    error,
                    token: RemoveFailureToken::Unsynced(unsynced()),
                });
            }
        }
        self.sync_directory(ClearTxnStage::SyncRemovalDirectory)
            .map_err(|error| RemoveError {
                error,
                token: RemoveFailureToken::Unsynced(unsynced()),
            })
    }

    /// `CreateMarker` → `SampleMarkerIdentity` → `WriteMarker` → `SyncMarkerFile`：
    /// `openat` 一成功即取得所有权，之后任何失败都返还持活 fd 的 `Created`；
    /// 身份采样是唯一可失败的采样步骤，且在 Durable 提升之前完成。
    #[allow(
        clippy::result_large_err,
        reason = "错误变体按设计携带被返还的凭证本体；非实时路径"
    )]
    fn create_marker(
        &mut self,
        record: ClearTxnRecord,
    ) -> Result<(CreatedMarker, FileIdentity), PrepareError> {
        let dir = self.dir;
        let layout = self.layout;
        let name = &layout.marker;
        let directory = dir.identity();
        let file = match self.ctx.run(ClearTxnStage::CreateMarker, || {
            dir.create_new_at(name, libc::O_RDWR, 0o600)
        }) {
            Ok(file) => file,
            Err(StepFailure::Before(error) | StepFailure::Operation(error)) => {
                return Err(PrepareError {
                    error,
                    token: PrepareFailureToken::NotOwned,
                });
            }
            Err(StepFailure::After(error, file)) => {
                return Err(PrepareError {
                    error,
                    token: PrepareFailureToken::Created(CreatedMarker {
                        record,
                        directory,
                        marker: None,
                        file,
                    }),
                });
            }
        };
        let mut created = CreatedMarker {
            record,
            directory,
            marker: None,
            file,
        };
        let sampled = self.ctx.run(ClearTxnStage::SampleMarkerIdentity, || {
            created
                .file
                .metadata()
                .map(|metadata| FileIdentity::of(&metadata))
        });
        let marker = match sampled {
            Ok(marker) => marker,
            Err(StepFailure::Before(error) | StepFailure::Operation(error)) => {
                return Err(PrepareError {
                    error,
                    token: PrepareFailureToken::Created(created),
                });
            }
            Err(StepFailure::After(error, marker)) => {
                created.marker = Some(marker);
                return Err(PrepareError {
                    error,
                    token: PrepareFailureToken::Created(created),
                });
            }
        };
        created.marker = Some(marker);
        let encoded = record.encode();
        if let Err(failure) = self.ctx.run(ClearTxnStage::WriteMarker, || {
            created.file.write_all(&encoded)
        }) {
            return Err(PrepareError {
                error: failure.into_error(),
                token: PrepareFailureToken::Created(created),
            });
        }
        if let Err(failure) = self
            .ctx
            .run(ClearTxnStage::SyncMarkerFile, || created.file.sync_all())
        {
            return Err(PrepareError {
                error: failure.into_error(),
                token: PrepareFailureToken::Created(created),
            });
        }
        Ok((created, marker))
    }

    /// 排他创建标记并写入、同步文件（[`Self::create_marker`]），再同步目录提升为
    /// Durable，最后严格准备双份 `0`。
    #[allow(
        clippy::result_large_err,
        reason = "错误变体按设计携带被返还的凭证本体；非实时路径"
    )]
    fn prepare(&mut self, record: ClearTxnRecord) -> Result<PreparedClear, PrepareError> {
        let dir = self.dir;
        let (created, marker) = self.create_marker(record)?;
        // 身份已在 Created 阶段采样完成；目录同步成功后的 Durable 提升不再有可失败步骤。
        let binding = Binding {
            record,
            marker,
            directory: created.directory,
        };
        match self
            .ctx
            .run_directory_sync(ClearTxnStage::SyncMarkerDirectory, dir)
        {
            Ok(()) => {}
            Err(StepFailure::Before(error) | StepFailure::Operation(error)) => {
                return Err(PrepareError {
                    error,
                    token: PrepareFailureToken::Created(created),
                });
            }
            Err(StepFailure::After(error, ())) => {
                return Err(PrepareError {
                    error,
                    token: PrepareFailureToken::Durable(DurableMarker { binding }),
                });
            }
        }
        // 标记已 durable：活 fd 不再需要，凭证升级为 Durable。
        drop(created);
        let durable = DurableMarker { binding };
        if let Err(error) = self.write_pair(false) {
            return Err(PrepareError {
                error,
                token: PrepareFailureToken::Durable(durable),
            });
        }
        Ok(PreparedClear {
            binding: durable.binding,
        })
    }

    /// `false` 按 backup → primary，`true` 按 primary → backup，全部严格同步。
    fn write_pair(&mut self, latched: bool) -> Result<(), StageError> {
        let layout = self.layout;
        if latched {
            self.write_latch(&layout.latch, true, LatchCopy::Primary)?;
            self.write_latch(&layout.backup, true, LatchCopy::Backup)
        } else {
            self.write_latch(&layout.backup, false, LatchCopy::Backup)?;
            self.write_latch(&layout.latch, false, LatchCopy::Primary)
        }
    }

    fn write_latch(
        &mut self,
        name: &OsStr,
        latched: bool,
        copy: LatchCopy,
    ) -> Result<(), StageError> {
        let dir: &'a DirHandle = self.dir;
        let stages = copy.stages();
        let (temporary, mut file) =
            self.run_flat(stages[0], || TemporaryLatch::create(dir, name))?;
        self.run_flat(stages[1], || {
            file.write_all(if latched { b"1\n" } else { b"0\n" })
        })?;
        self.run_flat(stages[2], || file.sync_all())?;
        self.run_flat(stages[3], || dir.rename_at(&temporary.name, name))?;
        self.sync_directory(stages[4])
    }

    /// 无标记时的旧 loader 语义：primary 优先；缺失 / 损坏回退 backup 并
    /// best-effort 自愈回写 primary；双缺失返回 `NotFound`，双损坏返回
    /// `InvalidData`。
    fn load_latch(&mut self) -> Result<bool, StageError> {
        let load_error = |source| StageError::operation(ClearTxnStage::LoadLatch, source);
        let layout = self.layout;
        let primary = self.read_latch_value(&layout.latch);
        let value = match primary {
            Ok(value) => return Ok(value),
            Err(error) => match error.kind() {
                io::ErrorKind::NotFound => match self.read_latch_value(&layout.backup) {
                    Ok(value) => value,
                    Err(backup) if backup.kind() == io::ErrorKind::NotFound => {
                        return Err(load_error(io::Error::new(
                            io::ErrorKind::NotFound,
                            "estop_latch: primary AND backup both missing \
                             (fail-closed; first-deploy: provision \"0\\n\" to both \
                             primary + backup before start; runtime clear: ClearEStop \
                             control action)",
                        )));
                    }
                    Err(backup) => return Err(load_error(backup)),
                },
                io::ErrorKind::InvalidData => match self.read_latch_value(&layout.backup) {
                    Ok(value) => value,
                    Err(_) => {
                        return Err(load_error(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "estop_latch: primary AND backup both corrupted/unreadable",
                        )));
                    }
                },
                _ => return Err(load_error(error)),
            },
        };
        tracing::warn!(
            latched = value,
            "estop_latch: primary missing or corrupted, restored from backup"
        );
        // 自愈只在 backup 确实可解析时执行；沿旧 loader 的 best-effort 语义忽略失败。
        let _ = self.write_latch(&layout.latch, value, LatchCopy::Primary);
        Ok(value)
    }

    /// 读单个 latch 文件（不做 backup fallback），语义与旧 loader 相同。
    fn read_latch_value(&self, name: &OsStr) -> io::Result<bool> {
        let file = self.dir.open_at(name, libc::O_RDONLY)?;
        let mut buf = [0u8; 4];
        let n = read_prefix_at(&file, &mut buf)?;
        parse_latch_bytes(&buf[..n]).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "estop_latch file: expected '0\\n' or '1\\n'",
            )
        })
    }

    /// W0.1：一个 `InspectPair` 步骤内顺序核验 primary、backup；任一真实错误立即
    /// 返回，不产生部分结果。
    fn inspect_pair(&mut self) -> Result<(LatchCopyState, LatchCopyState), StageError> {
        let dir = self.dir;
        let layout = self.layout;
        self.run_flat(ClearTxnStage::InspectPair, || {
            let primary = inspect_copy(dir, &layout.latch)?;
            let backup = inspect_copy(dir, &layout.backup)?;
            Ok((primary, backup))
        })
    }
}

/// W0.1：相对绑定目录 fd 只读核验一份。不跟随链接、不在 FIFO 上等待；对同一个
/// 打开的 fd 先核普通文件且 `nlink == 1`，再读前缀解析。仅 open 的真实 `ENOENT`
/// 记为 `Missing`，坏字节记为 `Corrupt`，其余错误原样返回。
fn inspect_copy(dir: &DirHandle, name: &OsStr) -> io::Result<LatchCopyState> {
    let file = match dir.open_at(name, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK) {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
            return Ok(LatchCopyState::Missing);
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "latch 数据对象不是普通文件（符号链接 / 目录 / 特殊文件）",
        ));
    }
    if metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "latch 数据对象硬链接数不为 1",
        ));
    }
    let mut buf = [0u8; 4];
    let n = read_prefix_at(&file, &mut buf)?;
    Ok(parse_latch_bytes(&buf[..n]).map_or(LatchCopyState::Corrupt, LatchCopyState::Value))
}

/// 纯字节解析：`0`、`0\n`、`1`、`1\n` 合法，其它（含空）为 `None`；与旧 loader 相同。
fn parse_latch_bytes(bytes: &[u8]) -> Option<bool> {
    match bytes {
        b"0" | b"0\n" => Some(false),
        b"1" | b"1\n" => Some(true),
        _ => None,
    }
}

/// 从偏移 0 读满缓冲或到 EOF；只对系统调用的 `EINTR` 重试。
fn read_prefix_at(file: &File, buf: &mut [u8]) -> io::Result<usize> {
    let mut len = 0;
    while len < buf.len() {
        match file.read_at(&mut buf[len..], len as u64) {
            Ok(0) => break,
            Ok(count) => len += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(len)
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
