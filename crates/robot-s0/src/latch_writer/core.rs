//! writer 本体：步骤守门、令牌状态机、命令执行、义务服务、主循环与测试线程。
//!
//! - [`WriterGuard`]：`before(stage)` 第一条语句发布 `FileStep` 戳 → 测试屏障 →
//!   stop → `ShuttingDown` → 清零命令的 latch 写步骤遇 Pending 义务 → `EStopObligationPending`。
//! - [`WriterToken`] × [`PersistCommand`] 矩阵按已批准合同执行；任何 `Refused` 不退休
//!   令牌，`current` 只在 `MarkerRemoved` / 最终 `Resynced` 清。
//! - `Created` 删除后目录同步失败保留 [`WriterToken::UnsyncedBeforeRepair`] 与原目标；
//!   重投 `RepairClear` 先 `resync_removal` 再继续**原目标**的真实修复，中间的重同步
//!   成功不充作 `Repaired`。
//! - `OriginalUnlatched` 在 `NoMarker` / `Created` 路径只经 W0.1 `inspect_pair` 核真实
//!   `0/0`；其它值 / 缺失 / 坏值 → `Refused(PairMismatch)`，真实 IO → `RepairFailed`
//!   保留令牌与 stage / phase，不借会自愈的旧 loader。
//! - writer 永不修改 state / guards / executor / authority，不读 epoch 推断目标，不在
//!   命令外删除或重建标记。[`spawn_writer`] 只是测试线程，不是生产第八 worker。

use std::io;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{RecvTimeoutError, SendTimeoutError};
use robot_core::control_authority::RepairTarget;
use robot_core::time::{DurationNs, RobotClock};

use super::obligation::{ClaimRefused, EStopObligation, Settle};
use super::pairing::{TargetKey, WriterId, WriterJoined, WriterSlot};
use super::progress::{ExitStamp, Phase, WriterProgress};
use super::protocol::{
    ClearTxnIdentity, PersistCommand, PersistCompletion, PersistEndpoint, PersistShared,
    PrepareTokenKind, RemoveTokenKind, WriterConfig, WriterExit, WriterHook, WriterRefusal,
    WriterStop,
};
use crate::clear_estop_transaction::{
    ClearTxnStage, ClearTxnStore, CreatedMarker, DurableMarker, GuardInterrupt, InterruptReason,
    LatchCopyState, PrepareError, PrepareFailureToken, PreparedClear, RemoveError,
    RemoveFailureToken, RepairError, RepairTarget as FileTarget, RepairedClear, StageError,
    StepGuard, UnsyncedRemoval,
};

/// 该文件步骤是否改写 latch 值（清零命令在这些步骤前核 `EStop` 义务）。显式 `match`，
/// 不改 W0。
#[must_use]
pub const fn writes_latch_value(stage: ClearTxnStage) -> bool {
    match stage {
        ClearTxnStage::CreateBackup
        | ClearTxnStage::WriteBackup
        | ClearTxnStage::SyncBackupFile
        | ClearTxnStage::RenameBackup
        | ClearTxnStage::SyncBackupDirectory
        | ClearTxnStage::CreatePrimary
        | ClearTxnStage::WritePrimary
        | ClearTxnStage::SyncPrimaryFile
        | ClearTxnStage::RenamePrimary
        | ClearTxnStage::SyncPrimaryDirectory => true,
        ClearTxnStage::OpenStore
        | ClearTxnStage::ValidateCredential
        | ClearTxnStage::ReadMarker
        | ClearTxnStage::ValidateMarker
        | ClearTxnStage::CreateMarker
        | ClearTxnStage::SampleMarkerIdentity
        | ClearTxnStage::WriteMarker
        | ClearTxnStage::SyncMarkerFile
        | ClearTxnStage::SyncMarkerDirectory
        | ClearTxnStage::RemoveMarker
        | ClearTxnStage::SyncRemovalDirectory
        | ClearTxnStage::LoadLatch
        | ClearTxnStage::InspectPair => false,
    }
}

/// 一次文件操作的环境：时钟、停止信号与测试屏障。
pub struct StepEnv<'a> {
    pub clock: &'a dyn RobotClock,
    pub stop: &'a dyn WriterStop,
    pub hook: &'a mut dyn WriterHook,
}

/// writer 的步骤守门。
pub struct WriterGuard<'a> {
    obligation: &'a EStopObligation,
    progress: &'a WriterProgress,
    env: &'a mut StepEnv<'a>,
    zeroing: bool,
}

impl<'a> WriterGuard<'a> {
    pub fn new(shared: &'a PersistShared, env: &'a mut StepEnv<'a>, zeroing: bool) -> Self {
        Self {
            obligation: &shared.obligation,
            progress: &shared.progress,
            env,
            zeroing,
        }
    }
}

impl StepGuard for WriterGuard<'_> {
    #[cfg(feature = "test-comm-fault-hook")]
    fn after_success(&mut self, stage: ClearTxnStage) {
        self.env.hook.after_step_success(stage);
    }
    #[cfg(feature = "test-comm-fault-hook")]
    fn directory_sync_input(
        &mut self,
        stage: ClearTxnStage,
    ) -> crate::clear_estop_transaction::DirectorySyncInput {
        self.env.hook.directory_sync_input(stage)
    }
    #[cfg(feature = "test-comm-fault-hook")]
    fn directory_sync_event(
        &mut self,
        stage: ClearTxnStage,
        event: crate::clear_estop_transaction::DirectorySyncEvent,
    ) {
        self.env.hook.directory_sync_event(stage, event);
    }
    fn before(&mut self, stage: ClearTxnStage) -> Result<(), GuardInterrupt> {
        // 第一条实际语句：真实进入文件步骤的时刻；编码失败仍发布阶段（时间位 0）。
        let now = self.env.clock.now();
        if self.progress.store(Phase::FileStep, now).is_err() {
            self.progress.store_unstamped(Phase::FileStep);
        }
        self.env.hook.before_step(stage);
        if !self.env.stop.running() {
            return Err(GuardInterrupt::new(InterruptReason::ShuttingDown));
        }
        if self.zeroing && writes_latch_value(stage) && self.obligation.has_pending() {
            return Err(GuardInterrupt::new(InterruptReason::EStopObligationPending));
        }
        Ok(())
    }
}

/// 仅借用观察钩子；正常返回与 unwind 都清理诊断 scope，不接管令牌。
#[cfg(feature = "test-comm-fault-hook")]
struct CommandHookScope<'a> {
    hook: &'a mut dyn WriterHook,
    completed: bool,
}
#[cfg(feature = "test-comm-fault-hook")]
impl Drop for CommandHookScope<'_> {
    fn drop(&mut self) {
        self.hook.command_scope_exit(self.completed);
    }
}

/// writer 持有的所有权事实。
#[derive(Debug)]
pub enum WriterToken {
    /// 没有本 store 拥有的标记（`NotOwned` / 已删除）。
    NoMarker,
    Prepared(PreparedClear),
    Created(CreatedMarker),
    Durable(DurableMarker),
    Repaired(RepairedClear),
    Unsynced(UnsyncedRemoval),
    /// `Created` 已 unlink 但目录同步失败，且原修复目标尚未完成。
    UnsyncedBeforeRepair {
        unsynced: UnsyncedRemoval,
        target: RepairTarget,
    },
    /// 记录 / 身份不符或标记真实缺失，凭证失效。
    Lost,
}

/// 令牌种类（`Copy`，供观察与测试）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterTokenKind {
    NoMarker,
    Prepared,
    Created,
    Durable,
    Repaired,
    Unsynced,
    UnsyncedBeforeRepair,
    Lost,
}

impl WriterToken {
    #[must_use]
    pub const fn kind(&self) -> WriterTokenKind {
        match self {
            Self::NoMarker => WriterTokenKind::NoMarker,
            Self::Prepared(_) => WriterTokenKind::Prepared,
            Self::Created(_) => WriterTokenKind::Created,
            Self::Durable(_) => WriterTokenKind::Durable,
            Self::Repaired(_) => WriterTokenKind::Repaired,
            Self::Unsynced(_) => WriterTokenKind::Unsynced,
            Self::UnsyncedBeforeRepair { .. } => WriterTokenKind::UnsyncedBeforeRepair,
            Self::Lost => WriterTokenKind::Lost,
        }
    }
}

/// 当前事务：身份 + 所有权令牌。
#[derive(Debug)]
pub struct WriterTxn {
    txn: ClearTxnIdentity,
    token: WriterToken,
}

impl WriterTxn {
    #[must_use]
    pub const fn txn(&self) -> ClearTxnIdentity {
        self.txn
    }

    #[must_use]
    pub const fn token(&self) -> &WriterToken {
        &self.token
    }
}

/// 义务服务结果。
#[derive(Debug)]
pub enum ObligationServiced {
    /// 写入成功并按 CAS 协议完成。
    Written(Settle),
    /// 写入失败：责任已恢复 Pending，起点不变。
    Failed(StageError),
}

/// writer 核心：worker store + 共享字 + 当前事务。
pub struct WriterCore {
    store: ClearTxnStore,
    shared: Arc<PersistShared>,
    writer: WriterId,
    target: TargetKey,
    current: Option<WriterTxn>,
}

fn a2_to_file_target(target: RepairTarget) -> FileTarget {
    match target {
        RepairTarget::Latched => FileTarget::Latched,
        RepairTarget::OriginalUnlatched => FileTarget::Original,
    }
}

impl WriterCore {
    #[must_use]
    pub fn from_slot(slot: WriterSlot) -> Self {
        Self {
            store: slot.store,
            shared: slot.shared,
            writer: slot.writer,
            target: slot.target,
            current: None,
        }
    }

    #[must_use]
    pub const fn writer(&self) -> WriterId {
        self.writer
    }

    #[must_use]
    pub const fn target(&self) -> &TargetKey {
        &self.target
    }

    #[must_use]
    pub const fn shared(&self) -> &Arc<PersistShared> {
        &self.shared
    }

    #[must_use]
    pub const fn current(&self) -> Option<&WriterTxn> {
        self.current.as_ref()
    }

    #[must_use]
    pub const fn store(&self) -> &ClearTxnStore {
        &self.store
    }

    /// 服务 `EStop` 义务：有 Pending 才领取；`persist_latched(guard{zeroing:false})`
    /// 成功 `commit`、失败 `fail`（恢复 Pending、起点不变）。无 Pending / 已 Active → `None`。
    pub fn service_obligation(&mut self, env: &mut StepEnv<'_>) -> Option<ObligationServiced> {
        #[cfg(feature = "test-comm-fault-hook")]
        env.hook.outside_command();
        let obligation = &self.shared.obligation;
        if !obligation.has_pending() {
            return None;
        }
        let now = env.clock.now();
        if self.shared.progress.store(Phase::Claiming, now).is_err() {
            self.shared.progress.store_unstamped(Phase::Claiming);
        }
        let guard = match obligation.claim_guard() {
            Ok(guard) => guard,
            Err(ClaimRefused::NoPending | ClaimRefused::AlreadyActive) => return None,
        };
        let result = with_guard(&self.shared, env, false, |step_guard| {
            self.store.persist_latched(step_guard)
        });
        Some(match result {
            Ok(()) => ObligationServiced::Written(guard.commit()),
            Err(error) => {
                guard.fail();
                ObligationServiced::Failed(error)
            }
        })
    }

    /// 执行一条命令；永不 panic 地返回完成，令牌按真实结果推进或保留。
    pub fn execute(&mut self, command: PersistCommand, env: &mut StepEnv<'_>) -> PersistCompletion {
        #[cfg(feature = "test-comm-fault-hook")]
        {
            let mut scope = CommandHookScope {
                hook: env.hook,
                completed: false,
            };
            scope
                .hook
                .command_begin(command, self.current.as_ref().map(WriterTxn::token));
            let completion = self.execute_inner(
                command,
                &mut StepEnv {
                    clock: env.clock,
                    stop: env.stop,
                    hook: scope.hook,
                },
            );
            scope
                .hook
                .command_completed(&completion, self.current.as_ref().map(WriterTxn::token));
            scope.completed = true;
            completion
        }
        #[cfg(not(feature = "test-comm-fault-hook"))]
        self.execute_inner(command, env)
    }

    fn execute_inner(
        &mut self,
        command: PersistCommand,
        env: &mut StepEnv<'_>,
    ) -> PersistCompletion {
        match command {
            PersistCommand::PrepareClear { txn } => self.prepare(txn, env),
            PersistCommand::RepairClear { txn, target } => self.repair(txn, target, env),
            PersistCommand::RemoveMarker { txn, kind } => self.remove(txn, kind, env),
            PersistCommand::ResyncRemoval { txn } => self.resync(txn, env),
        }
    }

    fn refuse(txn: ClearTxnIdentity, reason: WriterRefusal) -> PersistCompletion {
        PersistCompletion::Refused {
            txn: Some(txn),
            reason,
        }
    }

    /// 身份 / 停止前置：返回当前事务的可变引用或拒绝。
    fn check_current(
        &mut self,
        txn: ClearTxnIdentity,
        env: &StepEnv<'_>,
    ) -> Result<&mut WriterTxn, PersistCompletion> {
        let Some(current) = self.current.as_mut() else {
            return Err(Self::refuse(txn, WriterRefusal::NoTransaction));
        };
        if current.txn != txn {
            return Err(Self::refuse(txn, WriterRefusal::IdentityMismatch));
        }
        if !env.stop.running() {
            return Err(Self::refuse(txn, WriterRefusal::ShuttingDown));
        }
        Ok(current)
    }

    #[allow(
        clippy::result_large_err,
        reason = "W0 失败变体按设计携带被返还的凭证本体；非实时路径"
    )]
    fn prepare(&mut self, txn: ClearTxnIdentity, env: &mut StepEnv<'_>) -> PersistCompletion {
        match &self.current {
            Some(current) if current.txn != txn => {
                return Self::refuse(txn, WriterRefusal::IdentityMismatch);
            }
            Some(_) => return Self::refuse(txn, WriterRefusal::TokenStateMismatch),
            None => {}
        }
        if !self.shared.obligation.is_settled() {
            return Self::refuse(txn, WriterRefusal::EStopPersistOutstanding);
        }
        if !env.stop.running() {
            return Self::refuse(txn, WriterRefusal::ShuttingDown);
        }
        let result = with_guard(&self.shared, env, true, |guard| {
            self.store
                .prepare_clear(txn.txn_id, txn.expected_epoch, txn.original_latched, guard)
        });
        match result {
            Ok(prepared) => {
                self.current = Some(WriterTxn {
                    txn,
                    token: WriterToken::Prepared(prepared),
                });
                PersistCompletion::Prepared {
                    txn,
                    at: env.clock.now(),
                }
            }
            Err(PrepareError { error, token }) => {
                let (token, kind) = match token {
                    PrepareFailureToken::NotOwned => {
                        (WriterToken::NoMarker, PrepareTokenKind::NotOwned)
                    }
                    PrepareFailureToken::Created(created) => {
                        (WriterToken::Created(created), PrepareTokenKind::Created)
                    }
                    PrepareFailureToken::Durable(durable) => {
                        (WriterToken::Durable(durable), PrepareTokenKind::Durable)
                    }
                };
                self.current = Some(WriterTxn { txn, token });
                PersistCompletion::PrepareFailed {
                    txn,
                    stage: error.stage,
                    phase: error.phase,
                    token: kind,
                }
            }
        }
    }

    fn repair(
        &mut self,
        txn: ClearTxnIdentity,
        target: RepairTarget,
        env: &mut StepEnv<'_>,
    ) -> PersistCompletion {
        if let Err(refused) = self.check_current(txn, env) {
            return refused;
        }
        if target == RepairTarget::OriginalUnlatched && txn.original_latched {
            return Self::refuse(txn, WriterRefusal::IdentityMismatch);
        }
        let token = std::mem::replace(
            &mut self.current.as_mut().expect("check_current 已核").token,
            WriterToken::Lost,
        );
        let (token, completion) = match token {
            WriterToken::NoMarker => self.repair_without_marker(txn, target, env),
            WriterToken::Created(created) => self.repair_after_created(txn, created, target, env),
            WriterToken::UnsyncedBeforeRepair {
                unsynced,
                target: original,
            } => self.repair_after_unsynced(txn, unsynced, original, target, env),
            WriterToken::Prepared(credential) => {
                self.repair_with_credential(txn, credential, target, env, WriterToken::Prepared)
            }
            WriterToken::Durable(credential) => {
                self.repair_with_credential(txn, credential, target, env, WriterToken::Durable)
            }
            WriterToken::Repaired(credential) => {
                self.repair_with_credential(txn, credential, target, env, WriterToken::Repaired)
            }
            token @ (WriterToken::Unsynced(_) | WriterToken::Lost) => {
                (token, Self::refuse(txn, WriterRefusal::TokenStateMismatch))
            }
        };
        self.current.as_mut().expect("check_current 已核").token = token;
        completion
    }

    /// `NoMarker`：`Latched` → `persist_latched`；`OriginalUnlatched` → W0.1 `inspect_pair`
    /// 须 `Value(false)/Value(false)`，否则 `Refused(PairMismatch)`；真实 IO 保留令牌。
    fn repair_without_marker(
        &mut self,
        txn: ClearTxnIdentity,
        target: RepairTarget,
        env: &mut StepEnv<'_>,
    ) -> (WriterToken, PersistCompletion) {
        let completion = match target {
            RepairTarget::Latched => {
                match with_guard(&self.shared, env, false, |guard| {
                    self.store.persist_latched(guard)
                }) {
                    Ok(()) => PersistCompletion::Repaired {
                        txn,
                        target,
                        at: env.clock.now(),
                    },
                    Err(error) => repair_failed(txn, &error, true),
                }
            }
            RepairTarget::OriginalUnlatched => {
                match with_guard(&self.shared, env, true, |guard| {
                    self.store.inspect_pair(guard)
                }) {
                    Ok(view)
                        if view.primary == LatchCopyState::Value(false)
                            && view.backup == LatchCopyState::Value(false) =>
                    {
                        PersistCompletion::Repaired {
                            txn,
                            target,
                            at: env.clock.now(),
                        }
                    }
                    Ok(_) => Self::refuse(txn, WriterRefusal::PairMismatch),
                    Err(error) => repair_failed(txn, &error, true),
                }
            }
        };
        (WriterToken::NoMarker, completion)
    }

    /// `Created`：先 `discard_created`，按真实结果推进令牌；成功后同 `NoMarker` 路径。
    #[allow(
        clippy::result_large_err,
        reason = "W0 失败变体按设计携带被返还的凭证本体；非实时路径"
    )]
    fn repair_after_created(
        &mut self,
        txn: ClearTxnIdentity,
        created: CreatedMarker,
        target: RepairTarget,
        env: &mut StepEnv<'_>,
    ) -> (WriterToken, PersistCompletion) {
        let discarded = with_guard(&self.shared, env, false, |guard| {
            self.store.discard_created(created, guard)
        });
        match discarded {
            Ok(()) => self.repair_without_marker(txn, target, env),
            Err(RemoveError { error, token }) => match token {
                RemoveFailureToken::Retained(created) => (
                    WriterToken::Created(created),
                    repair_failed(txn, &error, true),
                ),
                RemoveFailureToken::Unsynced(unsynced) => (
                    WriterToken::UnsyncedBeforeRepair { unsynced, target },
                    repair_failed(txn, &error, true),
                ),
                RemoveFailureToken::Lost => (WriterToken::Lost, repair_failed(txn, &error, false)),
            },
        }
    }

    /// `UnsyncedBeforeRepair`：先 `resync_removal`，成功后继续**原目标**的真实修复；
    /// 中间的重同步成功不充作 `Repaired`。命令目标与原目标不同 → `IdentityMismatch`。
    fn repair_after_unsynced(
        &mut self,
        txn: ClearTxnIdentity,
        unsynced: UnsyncedRemoval,
        original: RepairTarget,
        requested: RepairTarget,
        env: &mut StepEnv<'_>,
    ) -> (WriterToken, PersistCompletion) {
        if requested != original {
            return (
                WriterToken::UnsyncedBeforeRepair {
                    unsynced,
                    target: original,
                },
                Self::refuse(txn, WriterRefusal::IdentityMismatch),
            );
        }
        let resynced = with_guard(&self.shared, env, false, |guard| {
            self.store.resync_removal(unsynced, guard)
        });
        match resynced {
            Ok(()) => self.repair_without_marker(txn, original, env),
            Err(RemoveError { error, token }) => match token {
                RemoveFailureToken::Retained(unsynced) | RemoveFailureToken::Unsynced(unsynced) => {
                    (
                        WriterToken::UnsyncedBeforeRepair {
                            unsynced,
                            target: original,
                        },
                        repair_failed(txn, &error, true),
                    )
                }
                RemoveFailureToken::Lost => (WriterToken::Lost, repair_failed(txn, &error, false)),
            },
        }
    }

    /// `Prepared` / `Durable` / `Repaired`：`repair<C>`，失败按 `Some(C)` / `None` 保留或失权。
    fn repair_with_credential<C: crate::clear_estop_transaction::MarkerCredential>(
        &mut self,
        txn: ClearTxnIdentity,
        credential: C,
        target: RepairTarget,
        env: &mut StepEnv<'_>,
        keep: fn(C) -> WriterToken,
    ) -> (WriterToken, PersistCompletion) {
        let zeroing = target == RepairTarget::OriginalUnlatched;
        let repaired = with_guard(&self.shared, env, zeroing, |guard| {
            self.store
                .repair(credential, a2_to_file_target(target), guard)
        });
        match repaired {
            Ok(repaired) => (
                WriterToken::Repaired(repaired),
                PersistCompletion::Repaired {
                    txn,
                    target,
                    at: env.clock.now(),
                },
            ),
            Err(RepairError { error, token }) => match token {
                Some(credential) => (keep(credential), repair_failed(txn, &error, true)),
                None => (WriterToken::Lost, repair_failed(txn, &error, false)),
            },
        }
    }

    fn remove(
        &mut self,
        txn: ClearTxnIdentity,
        kind: robot_core::control_authority::CleanupKind,
        env: &mut StepEnv<'_>,
    ) -> PersistCompletion {
        if let Err(refused) = self.check_current(txn, env) {
            return refused;
        }
        let token = std::mem::replace(
            &mut self.current.as_mut().expect("check_current 已核").token,
            WriterToken::Lost,
        );
        let removed = |at| PersistCompletion::MarkerRemoved { txn, kind, at };
        let (token, completion, finished) = match token {
            WriterToken::NoMarker => (WriterToken::NoMarker, removed(env.clock.now()), true),
            WriterToken::Prepared(credential) => {
                self.remove_with_credential(txn, credential, env, WriterToken::Prepared, removed)
            }
            WriterToken::Durable(credential) => {
                self.remove_with_credential(txn, credential, env, WriterToken::Durable, removed)
            }
            WriterToken::Repaired(credential) => {
                self.remove_with_credential(txn, credential, env, WriterToken::Repaired, removed)
            }
            WriterToken::Unsynced(unsynced) => self.resync_unsynced(txn, unsynced, env),
            token @ (WriterToken::Created(_)
            | WriterToken::UnsyncedBeforeRepair { .. }
            | WriterToken::Lost) => (
                token,
                Self::refuse(txn, WriterRefusal::TokenStateMismatch),
                false,
            ),
        };
        if finished {
            self.current = None;
        } else {
            self.current.as_mut().expect("check_current 已核").token = token;
        }
        completion
    }

    fn remove_with_credential<C: crate::clear_estop_transaction::MarkerCredential>(
        &mut self,
        txn: ClearTxnIdentity,
        credential: C,
        env: &mut StepEnv<'_>,
        keep: fn(C) -> WriterToken,
        removed: impl FnOnce(robot_core::time::MonoRawNs) -> PersistCompletion,
    ) -> (WriterToken, PersistCompletion, bool) {
        let result = with_guard(&self.shared, env, false, |guard| {
            self.store.remove(credential, guard)
        });
        match result {
            Ok(()) => (WriterToken::NoMarker, removed(env.clock.now()), true),
            Err(RemoveError { error, token }) => match token {
                RemoveFailureToken::Retained(credential) => (
                    keep(credential),
                    remove_failed(txn, &error, RemoveTokenKind::Retained),
                    false,
                ),
                RemoveFailureToken::Unsynced(unsynced) => (
                    WriterToken::Unsynced(unsynced),
                    remove_failed(txn, &error, RemoveTokenKind::Unsynced),
                    false,
                ),
                RemoveFailureToken::Lost => (
                    WriterToken::Lost,
                    remove_failed(txn, &error, RemoveTokenKind::Lost),
                    false,
                ),
            },
        }
    }

    /// `Unsynced` 只允许重同步：成功 → `Resynced` 并清当前事务；失败保留令牌。
    fn resync_unsynced(
        &mut self,
        txn: ClearTxnIdentity,
        unsynced: UnsyncedRemoval,
        env: &mut StepEnv<'_>,
    ) -> (WriterToken, PersistCompletion, bool) {
        let result = with_guard(&self.shared, env, false, |guard| {
            self.store.resync_removal(unsynced, guard)
        });
        match result {
            Ok(()) => (
                WriterToken::NoMarker,
                PersistCompletion::Resynced { txn },
                true,
            ),
            Err(RemoveError { error, token }) => match token {
                RemoveFailureToken::Retained(unsynced) | RemoveFailureToken::Unsynced(unsynced) => {
                    (
                        WriterToken::Unsynced(unsynced),
                        remove_failed(txn, &error, RemoveTokenKind::Unsynced),
                        false,
                    )
                }
                RemoveFailureToken::Lost => (
                    WriterToken::Lost,
                    remove_failed(txn, &error, RemoveTokenKind::Lost),
                    false,
                ),
            },
        }
    }

    fn resync(&mut self, txn: ClearTxnIdentity, env: &mut StepEnv<'_>) -> PersistCompletion {
        if let Err(refused) = self.check_current(txn, env) {
            return refused;
        }
        let token = std::mem::replace(
            &mut self.current.as_mut().expect("check_current 已核").token,
            WriterToken::Lost,
        );
        let (token, completion, finished) = match token {
            WriterToken::Unsynced(unsynced) => self.resync_unsynced(txn, unsynced, env),
            other => (
                other,
                Self::refuse(txn, WriterRefusal::TokenStateMismatch),
                false,
            ),
        };
        if finished {
            self.current = None;
        } else {
            self.current.as_mut().expect("check_current 已核").token = token;
        }
        completion
    }
}

/// 在一个新的 [`WriterGuard`] 下执行一次 store 操作。
fn with_guard<T>(
    shared: &PersistShared,
    env: &mut StepEnv<'_>,
    zeroing: bool,
    operation: impl FnOnce(&mut dyn StepGuard) -> T,
) -> T {
    let mut reborrowed = StepEnv {
        clock: env.clock,
        stop: env.stop,
        hook: &mut *env.hook,
    };
    let mut guard = WriterGuard::new(shared, &mut reborrowed, zeroing);
    operation(&mut guard)
}

fn repair_failed(txn: ClearTxnIdentity, error: &StageError, kept: bool) -> PersistCompletion {
    PersistCompletion::RepairFailed {
        txn,
        stage: error.stage,
        phase: error.phase,
        credential_kept: kept,
    }
}

fn remove_failed(
    txn: ClearTxnIdentity,
    error: &StageError,
    token: RemoveTokenKind,
) -> PersistCompletion {
    PersistCompletion::MarkerRemoveFailed {
        txn,
        stage: error.stage,
        phase: error.phase,
        token,
    }
}

fn to_duration(ns: DurationNs) -> Duration {
    Duration::from_nanos(ns.0)
}

/// 分片睡眠：每片不超过 `slice`，期间核停止信号。
fn sleep_sliced(total: DurationNs, slice: DurationNs, stop: &dyn WriterStop) {
    let mut remaining = total.0;
    while remaining > 0 && stop.running() {
        let chunk = remaining.min(slice.0.max(1));
        thread::sleep(Duration::from_nanos(chunk));
        remaining -= chunk;
    }
}

/// 把完成送回 IPC：`send_timeout` 循环直至成功或停止 / 对端断开。
fn deliver(
    endpoint: &PersistEndpoint,
    mut completion: PersistCompletion,
    config: WriterConfig,
    stop: &dyn WriterStop,
) -> bool {
    loop {
        match endpoint
            .completions
            .send_timeout(completion, to_duration(config.deliver_timeout))
        {
            Ok(()) => return true,
            Err(SendTimeoutError::Timeout(pending)) => {
                completion = pending;
                if !stop.running() {
                    return false;
                }
            }
            Err(SendTimeoutError::Disconnected(_)) => return false,
        }
    }
}

/// writer 主循环：`ExitStamp` 在入口构造、最后析构（先恢复责任，后宣告退出）。
#[allow(
    clippy::needless_pass_by_value,
    reason = "endpoint 随 writer 生命周期持有：线程返回即丢弃，IPC 侧据此观察到 Disconnected"
)]
pub fn run_writer(
    slot: WriterSlot,
    endpoint: PersistEndpoint,
    config: WriterConfig,
    stop: &dyn WriterStop,
    clock: &dyn RobotClock,
    hook: &mut dyn WriterHook,
) -> WriterExit {
    let shared = Arc::clone(&slot.shared);
    let _exit_stamp = ExitStamp::new(&shared.progress, clock);
    let mut core = WriterCore::from_slot(slot);
    let mut ipc_gone = false;
    loop {
        if !stop.running() {
            return if ipc_gone {
                WriterExit::StoppedAfterIpcDisconnect
            } else {
                WriterExit::Stopped
            };
        }
        let mut env = StepEnv { clock, stop, hook };
        // 先捕获刷新请求，再作本轮唯一义务尝试；失败退避不能饿死关闭结果。
        let flushing = shared.flush_pending();
        let serviced = core.service_obligation(&mut env);
        if flushing {
            use super::protocol::FlushOutcome;
            let outcome = match (shared.obligation.view().is_settled(), core.current()) {
                (false, _) => FlushOutcome::ObligationUnsettled,
                (true, Some(_)) => FlushOutcome::TransactionOpen,
                (true, None) => FlushOutcome::Complete,
            };
            shared.finish_flush(outcome);
        }
        match serviced {
            Some(ObligationServiced::Failed(error)) => {
                tracing::warn!(
                    stage = ?error.stage,
                    phase = ?error.phase,
                    "latch writer: EStop 义务写入失败，退避后重试"
                );
                stamp(&shared.progress, Phase::RetryBackoff, clock);
                sleep_sliced(config.retry_backoff, config.sleep_slice, stop);
                continue;
            }
            // R1：真实义务工作结束、进入等待前发布一次 Idle；只在阶段转换落戳，空转不刷新。
            Some(ObligationServiced::Written(_)) => stamp(&shared.progress, Phase::Idle, clock),
            None => {}
        }
        if ipc_gone {
            sleep_sliced(config.idle_poll, config.sleep_slice, stop);
            continue;
        }
        match endpoint
            .commands
            .recv_timeout(to_duration(config.idle_poll))
        {
            Ok(command) => {
                // R6：先发布 Claiming 再认领公告——任一暂停点至少保留公告或 Claiming 之一，
                // 监督不会同时看到 Idle + 无公告而失去 pickup 来源；真实接受只计一次。
                stamp(&shared.progress, Phase::Claiming, clock);
                let _accepted_at = shared.announce.claim();
                let completion = core.execute(command, &mut env);
                stamp(&shared.progress, Phase::Delivering, clock);
                if !deliver(&endpoint, completion, config, stop) {
                    tracing::warn!("latch writer: 完成未能送回 IPC");
                }
                stamp(&shared.progress, Phase::Idle, clock);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => ipc_gone = true,
        }
    }
}

fn stamp(progress: &WriterProgress, phase: Phase, clock: &dyn RobotClock) {
    if progress.store(phase, clock.now()).is_err() {
        progress.store_unstamped(phase);
    }
}

/// 测试线程句柄：真实 `join`（含 panic `Err`）才产生唯一 [`WriterJoined`]。
#[must_use = "未 join 的 writer 不得兜底写入"]
pub struct WriterHandle {
    join: JoinHandle<WriterExit>,
    writer: WriterId,
    target: TargetKey,
}

impl WriterHandle {
    #[must_use]
    pub const fn writer(&self) -> WriterId {
        self.writer
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }

    /// 真实 join：正常返回与 panic `Err` 两路都产生凭证。
    pub fn join(self) -> (thread::Result<WriterExit>, WriterJoined) {
        let result = self.join.join();
        (
            result,
            WriterJoined {
                writer: self.writer,
                target: self.target,
            },
        )
    }
}

/// 拉起 writer 线程（测试线程，不是生产第八 worker）。
///
/// # Errors
///
/// 线程创建失败。
pub fn spawn_writer(
    slot: WriterSlot,
    endpoint: PersistEndpoint,
    config: WriterConfig,
    stop: Arc<dyn WriterStop>,
    clock: Arc<dyn RobotClock>,
    mut hook: Box<dyn WriterHook>,
) -> io::Result<WriterHandle> {
    let writer = slot.writer;
    let target = slot.target.clone();
    let join = thread::Builder::new()
        .name("latch_writer".into())
        .spawn(move || {
            run_writer(
                slot,
                endpoint,
                config,
                stop.as_ref(),
                clock.as_ref(),
                hook.as_mut(),
            )
        })?;
    Ok(WriterHandle {
        join,
        writer,
        target,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clear_estop_transaction::{ClearTxnStore, StepPhase};
    use crate::latch_lock::acquire_latch_lock;
    use crate::latch_writer::obligation::RequestOutcome;
    use crate::latch_writer::pairing::WriterPairing;
    use crate::latch_writer::progress::ProgressView;
    use robot_core::control_authority::CleanupKind;
    use robot_core::time::MonoRawNs;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use tempfile::TempDir;

    const BASE: MonoRawNs = MonoRawNs(1_000_000);

    struct TickClock(AtomicU64);

    impl RobotClock for TickClock {
        fn now(&self) -> MonoRawNs {
            MonoRawNs(self.0.fetch_add(1, Ordering::Relaxed))
        }

        fn sleep_until(&self, _deadline: robot_core::time::Deadline) {}

        fn sleep_for(&self, _d: DurationNs) {}
    }

    struct Stop(AtomicBool);

    impl WriterStop for Stop {
        fn running(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
    }

    /// 记录每个文件步骤，并可在指定步骤上执行一次动作（制造 Pending / 停止）。
    struct RecordingHook {
        steps: Arc<Mutex<Vec<ClearTxnStage>>>,
        progress_at_hook: Arc<Mutex<Vec<ProgressView>>>,
        shared: Arc<PersistShared>,
        stop: Arc<Stop>,
        request_at: Option<ClearTxnStage>,
        stop_at: Option<ClearTxnStage>,
    }

    impl WriterHook for RecordingHook {
        fn before_step(&mut self, stage: ClearTxnStage) {
            self.steps.lock().unwrap().push(stage);
            self.progress_at_hook
                .lock()
                .unwrap()
                .push(self.shared.progress.load().unwrap());
            if self.request_at.take_if(|at| *at == stage).is_some() {
                self.shared.obligation.request(MonoRawNs(BASE.0 + 500));
            }
            if self.stop_at.take_if(|at| *at == stage).is_some() {
                self.stop.0.store(false, Ordering::Release);
            }
        }
    }

    /// 字段按析构顺序声明：core / store 先结束，锁随后释放（RAII），临时目录最后清理。
    struct Fixture {
        core: WriterCore,
        hook: RecordingHook,
        clock: TickClock,
        stop: Arc<Stop>,
        latch: PathBuf,
        _lock: crate::latch_lock::LatchTargetLock,
        /// 夹具自建的临时目录；目录外置（由测试保留到核验完毕）时为 `None`。
        dir_owner: Option<TempDir>,
    }

    fn txn(original_latched: bool) -> ClearTxnIdentity {
        ClearTxnIdentity {
            txn_id: 42,
            expected_epoch: 9,
            original_latched,
        }
    }

    fn open(latch: &Path) -> ClearTxnStore {
        ClearTxnStore::open(latch, "xlerobot_left", "left_arm").unwrap()
    }

    fn fixture(pair: &[u8]) -> Fixture {
        let dir = TempDir::new().unwrap();
        let mut fixture = fixture_in(dir.path(), pair);
        fixture.dir_owner = Some(dir);
        fixture
    }

    /// 在外部保留的目录内建夹具：目录生命周期由调用方掌握，夹具销毁后仍可核锁 / fd 与再取锁。
    fn fixture_in(dir: &Path, pair: &[u8]) -> Fixture {
        let latch = dir.join("estop_latch");
        std::fs::write(&latch, pair).unwrap();
        std::fs::write(dir.join("estop_latch.bak"), pair).unwrap();
        let lock = acquire_latch_lock(&latch).unwrap();
        let (slot, _standby) = WriterPairing::new(open(&latch), open(&latch), &lock, BASE)
            .unwrap()
            .into_parts();
        // 锁由夹具 RAII 持有贯穿 core 生命周期，销毁时真实释放（R3）。
        let shared = Arc::clone(slot.shared());
        let stop = Arc::new(Stop(AtomicBool::new(true)));
        let hook = RecordingHook {
            steps: Arc::new(Mutex::new(Vec::new())),
            progress_at_hook: Arc::new(Mutex::new(Vec::new())),
            shared,
            stop: Arc::clone(&stop),
            request_at: None,
            stop_at: None,
        };
        Fixture {
            core: WriterCore::from_slot(slot),
            hook,
            clock: TickClock(AtomicU64::new(BASE.0 + 1)),
            stop,
            latch,
            _lock: lock,
            dir_owner: None,
        }
    }

    impl Fixture {
        fn run(&mut self, command: PersistCommand) -> PersistCompletion {
            let mut env = StepEnv {
                clock: &self.clock,
                stop: self.stop.as_ref(),
                hook: &mut self.hook,
            };
            self.core.execute(command, &mut env)
        }

        fn service(&mut self) -> Option<ObligationServiced> {
            let mut env = StepEnv {
                clock: &self.clock,
                stop: self.stop.as_ref(),
                hook: &mut self.hook,
            };
            self.core.service_obligation(&mut env)
        }

        fn token(&self) -> Option<WriterTokenKind> {
            self.core.current().map(|current| current.token().kind())
        }

        fn pair(&self) -> (Vec<u8>, Vec<u8>) {
            (
                std::fs::read(&self.latch).unwrap(),
                std::fs::read(self.latch.with_file_name("estop_latch.bak")).unwrap(),
            )
        }

        fn marker_exists(&self) -> bool {
            self.latch
                .with_file_name("estop_latch.clear_pending")
                .exists()
        }

        fn steps(&self) -> Vec<ClearTxnStage> {
            self.hook.steps.lock().unwrap().clone()
        }

        fn clear_steps(&self) {
            self.hook.steps.lock().unwrap().clear();
        }
    }

    /// 本进程中 readlink 恰等于 `path` 的打开 fd 数（精确目标，不含前缀 / 已删除后缀）。
    fn open_fds_exactly(path: &Path) -> usize {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
            .filter(|target| target == path)
            .count()
    }

    /// R3 回归：测试目录外置保留到核验完毕；夹具销毁后指向锁文件与锁目录的精确 fd 都回到
    /// 基线，并真实重新取得同一目标的锁（不靠进程退出清理）。
    #[test]
    fn fixture_drop_releases_the_real_latch_lock_and_its_fd() {
        let dir = TempDir::new().unwrap();
        let canonical_dir = dir.path().canonicalize().unwrap();
        let latch = dir.path().join("estop_latch");
        let lock_path = canonical_dir.join("estop_latch.lock");
        let dir_fds_before = open_fds_exactly(&canonical_dir);
        let lock_fds_before = open_fds_exactly(&lock_path);

        let f = fixture_in(dir.path(), b"0\n");
        assert_eq!(
            open_fds_exactly(&lock_path),
            lock_fds_before + 1,
            "夹具存活时恰多持一个锁文件 fd"
        );
        // 夹具期间的目录 fd：锁绑定目录 fd + worker store 目录 fd（standby store 在构造夹具时即丢弃）。
        assert_eq!(
            open_fds_exactly(&canonical_dir),
            dir_fds_before + 2,
            "锁目录 fd + worker store 目录 fd"
        );
        assert!(
            matches!(
                acquire_latch_lock(&latch),
                Err(crate::latch_lock::LatchLockError::Held { .. })
            ),
            "夹具存活时同目标锁被持有"
        );

        drop(f);
        assert_eq!(
            open_fds_exactly(&lock_path),
            lock_fds_before,
            "夹具销毁后锁文件 fd 回到基线（RAII 释放，不能 mem::forget）"
        );
        assert_eq!(
            open_fds_exactly(&canonical_dir),
            dir_fds_before,
            "夹具销毁后锁目录 fd 与 store 目录 fd 回到基线"
        );
        let reacquired = acquire_latch_lock(&latch).expect("夹具销毁后同目标可真实重新取锁");
        assert_eq!(
            std::fs::read_to_string(&lock_path).unwrap(),
            format!("{}\n", std::process::id())
        );
        drop(reacquired);
        assert_eq!(open_fds_exactly(&lock_path), lock_fds_before);
    }

    #[test]
    fn writes_latch_value_is_an_explicit_table() {
        for stage in [
            ClearTxnStage::CreateBackup,
            ClearTxnStage::WriteBackup,
            ClearTxnStage::SyncBackupFile,
            ClearTxnStage::RenameBackup,
            ClearTxnStage::SyncBackupDirectory,
            ClearTxnStage::CreatePrimary,
            ClearTxnStage::WritePrimary,
            ClearTxnStage::SyncPrimaryFile,
            ClearTxnStage::RenamePrimary,
            ClearTxnStage::SyncPrimaryDirectory,
        ] {
            assert!(writes_latch_value(stage), "{stage:?}");
        }
        for stage in [
            ClearTxnStage::OpenStore,
            ClearTxnStage::ValidateCredential,
            ClearTxnStage::ReadMarker,
            ClearTxnStage::ValidateMarker,
            ClearTxnStage::CreateMarker,
            ClearTxnStage::SampleMarkerIdentity,
            ClearTxnStage::WriteMarker,
            ClearTxnStage::SyncMarkerFile,
            ClearTxnStage::SyncMarkerDirectory,
            ClearTxnStage::RemoveMarker,
            ClearTxnStage::SyncRemovalDirectory,
            ClearTxnStage::LoadLatch,
            ClearTxnStage::InspectPair,
        ] {
            assert!(!writes_latch_value(stage), "{stage:?}");
        }
    }

    /// 全链：`PrepareClear` → `RepairClear(Latched)` → `RemoveMarker`；`FileStep` 戳先于 hook（hook 观察到的
    /// 进度已是 `FileStep` 且时刻为该步骤时钟读数）。
    #[test]
    fn prepare_repair_remove_full_chain_with_file_step_before_hook() {
        let mut f = fixture(b"1\n");
        let identity = txn(true);
        assert!(matches!(
            f.run(PersistCommand::PrepareClear { txn: identity }),
            PersistCompletion::Prepared { txn, .. } if txn == identity
        ));
        assert_eq!(f.token(), Some(WriterTokenKind::Prepared));
        assert!(f.marker_exists());
        assert_eq!(f.pair(), (b"0\n".to_vec(), b"0\n".to_vec()));
        let steps = f.steps();
        assert_eq!(steps.first(), Some(&ClearTxnStage::CreateMarker));
        assert_eq!(steps.len(), 15, "prepare 为 15 个文件步骤");
        for (index, view) in f.hook.progress_at_hook.lock().unwrap().iter().enumerate() {
            assert_eq!(
                view.phase,
                Phase::FileStep,
                "第 {index} 步 hook 前必须已是 FileStep"
            );
            assert!(view.entered_at.0 > BASE.0, "真实进入时刻");
        }
        f.clear_steps();

        assert!(matches!(
            f.run(PersistCommand::RepairClear {
                txn: identity,
                target: RepairTarget::Latched
            }),
            PersistCompletion::Repaired {
                target: RepairTarget::Latched,
                ..
            }
        ));
        assert_eq!(f.token(), Some(WriterTokenKind::Repaired));
        assert_eq!(f.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
        assert!(f.marker_exists(), "Repaired 仍保留标记，current 不清");

        assert!(matches!(
            f.run(PersistCommand::RemoveMarker {
                txn: identity,
                kind: CleanupKind::Repaired(RepairTarget::Latched)
            }),
            PersistCompletion::MarkerRemoved { .. }
        ));
        assert_eq!(f.token(), None, "MarkerRemoved 才清 current");
        assert!(!f.marker_exists());
    }

    #[test]
    fn refusals_do_not_touch_files_or_tokens() {
        let mut f = fixture(b"1\n");
        let identity = txn(true);
        let other = ClearTxnIdentity {
            txn_id: 7,
            ..identity
        };
        assert!(matches!(
            f.run(PersistCommand::RepairClear {
                txn: identity,
                target: RepairTarget::Latched
            }),
            PersistCompletion::Refused {
                reason: WriterRefusal::NoTransaction,
                ..
            }
        ));
        // 义务未清偿：拒绝开始清零。
        f.core.shared().obligation.request(MonoRawNs(BASE.0 + 1));
        assert!(matches!(
            f.run(PersistCommand::PrepareClear { txn: identity }),
            PersistCompletion::Refused {
                reason: WriterRefusal::EStopPersistOutstanding,
                ..
            }
        ));
        assert!(!f.marker_exists());
        assert!(matches!(
            f.service(),
            Some(ObligationServiced::Written(Settle::Settled))
        ));
        assert!(f.core.shared().obligation.is_settled());
        f.run(PersistCommand::PrepareClear { txn: identity });
        assert_eq!(f.token(), Some(WriterTokenKind::Prepared));
        let before = f.pair();
        assert!(matches!(
            f.run(PersistCommand::RepairClear {
                txn: other,
                target: RepairTarget::Latched
            }),
            PersistCompletion::Refused {
                reason: WriterRefusal::IdentityMismatch,
                ..
            }
        ));
        assert!(matches!(
            f.run(PersistCommand::PrepareClear { txn: identity }),
            PersistCompletion::Refused {
                reason: WriterRefusal::TokenStateMismatch,
                ..
            }
        ));
        assert!(matches!(
            f.run(PersistCommand::ResyncRemoval { txn: identity }),
            PersistCompletion::Refused {
                reason: WriterRefusal::TokenStateMismatch,
                ..
            }
        ));
        // OriginalUnlatched 与 original_latched=true 矛盾。
        assert!(matches!(
            f.run(PersistCommand::RepairClear {
                txn: identity,
                target: RepairTarget::OriginalUnlatched
            }),
            PersistCompletion::Refused {
                reason: WriterRefusal::IdentityMismatch,
                ..
            }
        ));
        f.stop.0.store(false, Ordering::Release);
        assert!(matches!(
            f.run(PersistCommand::RepairClear {
                txn: identity,
                target: RepairTarget::Latched
            }),
            PersistCompletion::Refused {
                reason: WriterRefusal::ShuttingDown,
                ..
            }
        ));
        assert_eq!(f.token(), Some(WriterTokenKind::Prepared), "拒绝不退休令牌");
        assert_eq!(f.pair(), before);
        assert!(f.marker_exists());
    }

    /// 清零命令中途到达 `EStop` 义务：在下一 latch 写步骤前中断（Before 相），令牌保留
    /// Durable；服务义务写 1/1 后 Repair(Latched) 经 repair(Durable) 完成。
    #[test]
    fn zeroing_is_interrupted_by_pending_obligation_and_keeps_durable_token() {
        let mut f = fixture(b"1\n");
        let identity = txn(true);
        f.hook.request_at = Some(ClearTxnStage::CreateBackup);
        let completion = f.run(PersistCommand::PrepareClear { txn: identity });
        assert_eq!(
            completion,
            PersistCompletion::PrepareFailed {
                txn: identity,
                stage: ClearTxnStage::CreateBackup,
                phase: StepPhase::Before,
                token: PrepareTokenKind::Durable,
            }
        );
        assert_eq!(f.token(), Some(WriterTokenKind::Durable));
        assert_eq!(f.pair(), (b"1\n".to_vec(), b"1\n".to_vec()), "latch 未改");
        assert!(f.marker_exists());
        assert!(f.core.shared().obligation.has_pending());
        assert!(matches!(
            f.service(),
            Some(ObligationServiced::Written(Settle::Settled))
        ));
        assert_eq!(f.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
        assert!(matches!(
            f.run(PersistCommand::RepairClear {
                txn: identity,
                target: RepairTarget::Latched
            }),
            PersistCompletion::Repaired { .. }
        ));
        assert_eq!(f.token(), Some(WriterTokenKind::Repaired));
        assert!(matches!(
            f.run(PersistCommand::RemoveMarker {
                txn: identity,
                kind: CleanupKind::Finalize
            }),
            PersistCompletion::MarkerRemoved { .. }
        ));
        assert!(!f.marker_exists());
    }

    /// B1 反例：0/0 → Created（守门在 `SampleMarkerIdentity` 前停止）→ Repair：unlink 成功、
    /// 目录同步在 Before 相被停止 → UnsyncedBeforeRepair{Latched} → 重投 → resync →
    /// primary 五步 / backup 五步写 1 → Repaired；MarkerRemoved 前 current 不清。
    #[test]
    fn created_unlink_then_sync_failure_keeps_target_and_resumes_real_repair() {
        let mut f = fixture(b"0\n");
        let identity = txn(false);
        f.hook.stop_at = Some(ClearTxnStage::SampleMarkerIdentity);
        let completion = f.run(PersistCommand::PrepareClear { txn: identity });
        assert_eq!(
            completion,
            PersistCompletion::PrepareFailed {
                txn: identity,
                stage: ClearTxnStage::SampleMarkerIdentity,
                phase: StepPhase::Before,
                token: PrepareTokenKind::Created,
            }
        );
        assert_eq!(f.token(), Some(WriterTokenKind::Created));
        f.stop.0.store(true, Ordering::Release);
        assert!(f.marker_exists());

        // Created 不能直接删除。
        assert!(matches!(
            f.run(PersistCommand::RemoveMarker {
                txn: identity,
                kind: CleanupKind::Finalize
            }),
            PersistCompletion::Refused {
                reason: WriterRefusal::TokenStateMismatch,
                ..
            }
        ));

        f.clear_steps();
        f.hook.stop_at = Some(ClearTxnStage::SyncRemovalDirectory);
        let completion = f.run(PersistCommand::RepairClear {
            txn: identity,
            target: RepairTarget::Latched,
        });
        assert_eq!(
            completion,
            PersistCompletion::RepairFailed {
                txn: identity,
                stage: ClearTxnStage::SyncRemovalDirectory,
                phase: StepPhase::Before,
                credential_kept: true,
            }
        );
        assert_eq!(f.token(), Some(WriterTokenKind::UnsyncedBeforeRepair));
        assert!(!f.marker_exists(), "unlink 已发生");
        assert_eq!(f.pair(), (b"0\n".to_vec(), b"0\n".to_vec()), "修复尚未开始");
        f.stop.0.store(true, Ordering::Release);

        // 命令目标与原目标不同：拒绝，令牌不变。
        assert!(matches!(
            f.run(PersistCommand::RepairClear {
                txn: identity,
                target: RepairTarget::OriginalUnlatched
            }),
            PersistCompletion::Refused {
                reason: WriterRefusal::IdentityMismatch,
                ..
            }
        ));
        assert_eq!(f.token(), Some(WriterTokenKind::UnsyncedBeforeRepair));
        // ResyncRemoval / RemoveMarker 对 UnsyncedBeforeRepair 也拒绝：不能以中间 Resynced 充作修复。
        assert!(matches!(
            f.run(PersistCommand::ResyncRemoval { txn: identity }),
            PersistCompletion::Refused {
                reason: WriterRefusal::TokenStateMismatch,
                ..
            }
        ));

        f.clear_steps();
        assert_resumed_repair_after_resync(&mut f, identity);
    }

    /// 重投后：`SyncRemovalDirectory` → primary 五步 → backup 五步 → `Repaired`；
    /// `MarkerRemoved` 前 current 不清。
    fn assert_resumed_repair_after_resync(f: &mut Fixture, identity: ClearTxnIdentity) {
        let completion = f.run(PersistCommand::RepairClear {
            txn: identity,
            target: RepairTarget::Latched,
        });
        assert!(matches!(
            completion,
            PersistCompletion::Repaired {
                target: RepairTarget::Latched,
                ..
            }
        ));
        assert_eq!(
            f.steps(),
            vec![
                ClearTxnStage::SyncRemovalDirectory,
                ClearTxnStage::CreatePrimary,
                ClearTxnStage::WritePrimary,
                ClearTxnStage::SyncPrimaryFile,
                ClearTxnStage::RenamePrimary,
                ClearTxnStage::SyncPrimaryDirectory,
                ClearTxnStage::CreateBackup,
                ClearTxnStage::WriteBackup,
                ClearTxnStage::SyncBackupFile,
                ClearTxnStage::RenameBackup,
                ClearTxnStage::SyncBackupDirectory,
            ],
            "重同步后继续原目标的真实修复"
        );
        assert_eq!(f.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
        assert_eq!(
            f.token(),
            Some(WriterTokenKind::NoMarker),
            "MarkerRemoved 前 current 不清"
        );
        assert!(matches!(
            f.run(PersistCommand::RemoveMarker {
                txn: identity,
                kind: CleanupKind::Repaired(RepairTarget::Latched)
            }),
            PersistCompletion::MarkerRemoved { .. }
        ));
        assert_eq!(f.token(), None);
    }

    /// `OriginalUnlatched` 于 `NoMarker`：真实 0/0 才成功；其它值 / 缺失 / 坏值拒绝且不自愈；
    /// 真实 IO 保留令牌与 `InspectPair` 阶段。
    #[test]
    fn original_unlatched_requires_real_pair_verification() {
        let mut f = fixture(b"0\n");
        let identity = txn(false);
        f.hook.stop_at = Some(ClearTxnStage::CreateMarker);
        assert_eq!(
            f.run(PersistCommand::PrepareClear { txn: identity }),
            PersistCompletion::PrepareFailed {
                txn: identity,
                stage: ClearTxnStage::CreateMarker,
                phase: StepPhase::Before,
                token: PrepareTokenKind::NotOwned,
            }
        );
        f.stop.0.store(true, Ordering::Release);
        assert_eq!(f.token(), Some(WriterTokenKind::NoMarker));
        assert!(!f.marker_exists());

        f.clear_steps();
        assert!(matches!(
            f.run(PersistCommand::RepairClear {
                txn: identity,
                target: RepairTarget::OriginalUnlatched
            }),
            PersistCompletion::Repaired {
                target: RepairTarget::OriginalUnlatched,
                ..
            }
        ));
        assert_eq!(f.steps(), vec![ClearTxnStage::InspectPair]);
        assert_eq!(f.pair(), (b"0\n".to_vec(), b"0\n".to_vec()));

        // 不一致 → PairMismatch。
        std::fs::write(&f.latch, b"1\n").unwrap();
        assert!(matches!(
            f.run(PersistCommand::RepairClear {
                txn: identity,
                target: RepairTarget::OriginalUnlatched
            }),
            PersistCompletion::Refused {
                reason: WriterRefusal::PairMismatch,
                ..
            }
        ));
        assert_eq!(f.token(), Some(WriterTokenKind::NoMarker));
        // primary 缺失 + backup 0：Missing → 拒绝，且不借旧 loader 自愈 primary。
        std::fs::remove_file(&f.latch).unwrap();
        assert!(matches!(
            f.run(PersistCommand::RepairClear {
                txn: identity,
                target: RepairTarget::OriginalUnlatched
            }),
            PersistCompletion::Refused {
                reason: WriterRefusal::PairMismatch,
                ..
            }
        ));
        assert!(!f.latch.exists(), "不得自愈");
        // 坏值。
        std::fs::write(&f.latch, b"garbage").unwrap();
        assert!(matches!(
            f.run(PersistCommand::RepairClear {
                txn: identity,
                target: RepairTarget::OriginalUnlatched
            }),
            PersistCompletion::Refused {
                reason: WriterRefusal::PairMismatch,
                ..
            }
        ));
        // 真实 IO：primary 为符号链接 → RepairFailed{InspectPair, Operation, kept}。
        std::fs::remove_file(&f.latch).unwrap();
        std::fs::write(f.latch.with_file_name("real"), b"0\n").unwrap();
        std::os::unix::fs::symlink("real", &f.latch).unwrap();
        assert_eq!(
            f.run(PersistCommand::RepairClear {
                txn: identity,
                target: RepairTarget::OriginalUnlatched
            }),
            PersistCompletion::RepairFailed {
                txn: identity,
                stage: ClearTxnStage::InspectPair,
                phase: StepPhase::Operation,
                credential_kept: true,
            }
        );
        assert_eq!(
            f.token(),
            Some(WriterTokenKind::NoMarker),
            "IO 失败保留令牌"
        );
        // NoMarker 的 RemoveMarker：无文件动作直接完成。
        assert!(matches!(
            f.run(PersistCommand::RemoveMarker {
                txn: identity,
                kind: CleanupKind::Finalize
            }),
            PersistCompletion::MarkerRemoved { .. }
        ));
        assert_eq!(f.token(), None);
    }

    /// 义务服务：写入成功 Settled；写入被停止中断 → 责任恢复 Pending、起点不变；
    /// 服务期间新 request 保留。
    #[test]
    fn service_obligation_commits_or_restores_pending() {
        let mut f = fixture(b"0\n");
        assert!(f.service().is_none());
        assert_eq!(
            f.core.shared().obligation.request(MonoRawNs(BASE.0 + 10)),
            RequestOutcome::Registered
        );
        f.hook.stop_at = Some(ClearTxnStage::CreatePrimary);
        let Some(ObligationServiced::Failed(error)) = f.service() else {
            panic!("停止必须导致失败");
        };
        assert_eq!(error.stage, ClearTxnStage::CreatePrimary);
        assert_eq!(error.phase, StepPhase::Before);
        let view = f.core.shared().obligation.view();
        assert!(view.pending && !view.active);
        assert_eq!(view.unsettled_since, Some(MonoRawNs(BASE.0 + 10)));
        assert_eq!(f.pair(), (b"0\n".to_vec(), b"0\n".to_vec()));
        f.stop.0.store(true, Ordering::Release);
        f.hook.request_at = Some(ClearTxnStage::CreateBackup);
        assert!(matches!(
            f.service(),
            Some(ObligationServiced::Written(Settle::PendingRemains))
        ));
        assert_eq!(f.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
        assert!(
            f.core.shared().obligation.has_pending(),
            "写入期间的新 Pending 保留"
        );
        assert!(matches!(
            f.service(),
            Some(ObligationServiced::Written(Settle::Settled))
        ));
        assert!(f.core.shared().obligation.is_settled());
    }

    /// 在真实第一次失败退避入口记录结果；不调用 `finish_flush` 或改生产共享字。
    struct FlushStop {
        running: AtomicBool,
        shared: Arc<PersistShared>,
        attempts: Arc<AtomicU64>,
        first_backoff: std::sync::OnceLock<(Option<super::super::protocol::FlushOutcome>, u64)>,
    }

    impl WriterStop for FlushStop {
        fn running(&self) -> bool {
            if !self.running.load(Ordering::Acquire) {
                return false;
            }
            if self
                .shared
                .progress
                .load()
                .is_ok_and(|view| view.phase == Phase::RetryBackoff)
            {
                let _ = self.first_backoff.set((
                    self.shared.flush_outcome(),
                    self.attempts.load(Ordering::Acquire),
                ));
            }
            true
        }
    }

    struct FlushHook {
        shared: Arc<PersistShared>,
        attempts: Arc<AtomicU64>,
        steps: Arc<Mutex<Vec<ClearTxnStage>>>,
        interrupt_prepared_at: Option<ClearTxnStage>,
        latch: PathBuf,
    }

    impl WriterHook for FlushHook {
        fn before_step(&mut self, stage: ClearTxnStage) {
            self.steps.lock().unwrap().push(stage);
            if stage == ClearTxnStage::CreatePrimary {
                self.attempts.fetch_add(1, Ordering::AcqRel);
            }
            if self
                .interrupt_prepared_at
                .take_if(|at| *at == stage)
                .is_some()
            {
                // 原双份 0 已写；留下真实 Durable 事务，再让同轮义务写入遭遇真实 rename 错误。
                flush_obstacle(&self.latch);
                self.attempts.store(0, Ordering::Release);
                self.shared.obligation.request(MonoRawNs(BASE.0 + 500));
                self.shared.request_flush();
            }
        }
    }

    fn flush_obstacle(latch: &Path) {
        std::fs::remove_file(latch).unwrap();
        std::fs::create_dir(latch).unwrap();
        std::fs::write(latch.join("obstacle"), b"real rename failure").unwrap();
    }

    /// Drop 也先停止并有界等真结束、真 join，之后才释放原 Store 锁与目录。
    struct FlushFixture {
        handle: Option<WriterHandle>,
        stop: Arc<FlushStop>,
        client: crate::latch_writer::PersistClient,
        shared: Arc<PersistShared>,
        steps: Arc<Mutex<Vec<ClearTxnStage>>>,
        latch: PathBuf,
        _standby: crate::latch_writer::StandbyStore,
        _lock: crate::latch_lock::LatchTargetLock,
        _directory: TempDir,
    }

    impl FlushFixture {
        fn new(interrupt_prepared_at: Option<ClearTxnStage>) -> Self {
            let directory = TempDir::new().unwrap();
            let latch = directory.path().join("estop_latch");
            std::fs::write(&latch, b"0\n").unwrap();
            std::fs::write(directory.path().join("estop_latch.bak"), b"0\n").unwrap();
            let lock = acquire_latch_lock(&latch).unwrap();
            let (slot, standby) = WriterPairing::new(open(&latch), open(&latch), &lock, BASE)
                .unwrap()
                .into_parts();
            let shared = Arc::clone(slot.shared());
            let (client, endpoint) = crate::latch_writer::mailbox(Arc::clone(&shared));
            let attempts = Arc::new(AtomicU64::new(0));
            let steps = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(FlushStop {
                running: AtomicBool::new(true),
                shared: Arc::clone(&shared),
                attempts: Arc::clone(&attempts),
                first_backoff: std::sync::OnceLock::new(),
            });
            let hook = FlushHook {
                shared: Arc::clone(&shared),
                attempts,
                steps: Arc::clone(&steps),
                interrupt_prepared_at,
                latch: latch.clone(),
            };
            let handle = spawn_writer(
                slot,
                endpoint,
                WriterConfig::default(),
                stop.clone(),
                Arc::new(TickClock(AtomicU64::new(BASE.0 + 1))),
                Box::new(hook),
            )
            .unwrap();
            Self {
                handle: Some(handle),
                stop,
                client,
                shared,
                steps,
                latch,
                _standby: standby,
                _lock: lock,
                _directory: directory,
            }
        }

        fn wait_until(&self, condition: impl Fn() -> bool) {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while !condition() {
                assert!(
                    !self.handle.as_ref().unwrap().is_finished(),
                    "原 writer 提前结束"
                );
                assert!(std::time::Instant::now() < deadline, "真实 writer 观测超时");
                thread::sleep(Duration::from_millis(1));
            }
        }

        fn command(&mut self, command: PersistCommand) -> PersistCompletion {
            self.client
                .try_dispatch(command, MonoRawNs(BASE.0 + 1))
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                if let Some(done) = self.client.try_complete() {
                    return done;
                }
                assert!(self.client.alive(), "原 writer mailbox 断开");
                assert!(std::time::Instant::now() < deadline, "真实完成超时");
                thread::sleep(Duration::from_millis(1));
            }
        }

        fn remove_obstacle(&self) {
            std::fs::remove_file(self.latch.join("obstacle")).unwrap();
            std::fs::remove_dir(&self.latch).unwrap();
        }

        fn pair(&self) -> (Vec<u8>, Vec<u8>) {
            (
                std::fs::read(&self.latch).unwrap(),
                std::fs::read(self.latch.with_file_name("estop_latch.bak")).unwrap(),
            )
        }

        fn join(&mut self) -> thread::Result<WriterExit> {
            self.stop.running.store(false, Ordering::Release);
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while !self.handle.as_ref().unwrap().is_finished() {
                if std::time::Instant::now() >= deadline {
                    std::process::abort();
                }
                thread::sleep(Duration::from_millis(1));
            }
            let (result, _original_joined) = self.handle.take().unwrap().join();
            result
        }
    }

    impl Drop for FlushFixture {
        fn drop(&mut self) {
            if self.handle.is_some() {
                let result = self.join();
                assert!(
                    result.is_ok() || std::thread::panicking(),
                    "writer panic 必须显式验收"
                );
            }
        }
    }

    #[test]
    fn flush_unsettled_precedes_open_transaction_and_is_published_before_retry() {
        use super::super::protocol::FlushOutcome;
        let mut f = FlushFixture::new(Some(ClearTxnStage::SyncPrimaryDirectory));
        let identity = txn(false);
        assert!(
            matches!(f.command(PersistCommand::PrepareClear { txn: identity }),
            PersistCompletion::PrepareFailed { txn, stage: ClearTxnStage::SyncPrimaryDirectory,
                phase: StepPhase::Before, token: PrepareTokenKind::Durable } if txn == identity)
        );
        f.wait_until(|| f.stop.first_backoff.get().is_some());
        assert_eq!(
            f.stop.first_backoff.get(),
            Some(&(Some(FlushOutcome::ObligationUnsettled), 1)),
            "首个失败尝试退避前已发布 2，不能等重试或在一轮补第二次写"
        );
        assert!(f.shared.obligation.has_pending());
        assert!(f.latch.with_file_name("estop_latch.clear_pending").exists());
        f.remove_obstacle();
        f.wait_until(|| f.shared.obligation.is_settled());
        assert_eq!(
            f.shared.flush_outcome(),
            Some(FlushOutcome::ObligationUnsettled)
        );
        assert!(
            matches!(f.command(PersistCommand::RepairClear { txn: identity, target: RepairTarget::Latched }),
            PersistCompletion::Repaired { txn, target: RepairTarget::Latched, .. } if txn == identity)
        );
        assert!(
            matches!(f.command(PersistCommand::RemoveMarker { txn: identity, kind: CleanupKind::Repaired(RepairTarget::Latched) }),
            PersistCompletion::MarkerRemoved { txn, .. } if txn == identity)
        );
        f.shared.request_flush();
        assert_eq!(f.join().unwrap(), WriterExit::Stopped);
        assert_eq!(
            f.shared.flush_outcome(),
            Some(FlushOutcome::ObligationUnsettled)
        );
        assert_eq!(f.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
        assert!(!f.latch.with_file_name("estop_latch.clear_pending").exists());
    }

    #[test]
    fn flush_open_transaction_preserves_marker_and_first_result_after_cleanup() {
        use super::super::protocol::FlushOutcome;
        let mut f = FlushFixture::new(None);
        let identity = txn(false);
        assert!(
            matches!(f.command(PersistCommand::PrepareClear { txn: identity }), PersistCompletion::Prepared { txn, .. } if txn == identity)
        );
        f.shared.request_flush();
        f.wait_until(|| f.shared.flush_outcome().is_some());
        assert_eq!(
            f.shared.flush_outcome(),
            Some(FlushOutcome::TransactionOpen)
        );
        assert!(f.shared.obligation.is_settled());
        assert!(f.latch.with_file_name("estop_latch.clear_pending").exists());
        assert_eq!(f.pair(), (b"0\n".to_vec(), b"0\n".to_vec()));
        f.shared.request_flush();
        assert!(
            matches!(f.command(PersistCommand::RemoveMarker { txn: identity, kind: CleanupKind::Finalize }),
            PersistCompletion::MarkerRemoved { txn, .. } if txn == identity)
        );
        assert!(!f.latch.with_file_name("estop_latch.clear_pending").exists());
        // 新命令的实际完成保证已跨过清理后的下一轮；重复请求不能把首个 3 改成 1。
        let next = ClearTxnIdentity {
            txn_id: identity.txn_id + 1,
            ..identity
        };
        assert!(
            matches!(f.command(PersistCommand::PrepareClear { txn: next }), PersistCompletion::Prepared { txn, .. } if txn == next)
        );
        assert_eq!(
            f.shared.flush_outcome(),
            Some(FlushOutcome::TransactionOpen)
        );
        assert!(
            matches!(f.command(PersistCommand::RemoveMarker { txn: next, kind: CleanupKind::Finalize }),
            PersistCompletion::MarkerRemoved { txn, .. } if txn == next)
        );
        assert_eq!(f.join().unwrap(), WriterExit::Stopped);
        assert_eq!(
            f.shared.flush_outcome(),
            Some(FlushOutcome::TransactionOpen)
        );
    }

    #[test]
    fn flush_complete_keeps_zero_pair_and_does_not_hide_late_pending() {
        use super::super::protocol::FlushOutcome;
        let mut f = FlushFixture::new(None);
        f.shared.request_flush();
        f.wait_until(|| f.shared.flush_outcome().is_some());
        assert_eq!(f.shared.flush_outcome(), Some(FlushOutcome::Complete));
        assert!(
            f.steps.lock().unwrap().is_empty(),
            "无义务不能无条件补写 1/1"
        );
        assert_eq!(f.pair(), (b"0\n".to_vec(), b"0\n".to_vec()));
        flush_obstacle(&f.latch);
        assert_eq!(
            f.shared.obligation.request(MonoRawNs(BASE.0 + 500)),
            RequestOutcome::Registered
        );
        f.shared.request_flush();
        f.wait_until(|| f.stop.first_backoff.get().is_some());
        assert_eq!(
            f.stop.first_backoff.get(),
            Some(&(Some(FlushOutcome::Complete), 1))
        );
        assert!(
            !f.shared.obligation.is_settled(),
            "早期 flush=1 不能代替最终义务事实"
        );
        f.remove_obstacle();
        f.wait_until(|| f.shared.obligation.is_settled());
        assert_eq!(f.join().unwrap(), WriterExit::Stopped);
        assert_eq!(f.shared.flush_outcome(), Some(FlushOutcome::Complete));
        assert_eq!(f.pair(), (b"1\n".to_vec(), b"1\n".to_vec()));
    }
}
