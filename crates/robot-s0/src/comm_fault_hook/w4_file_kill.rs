//! 封闭的文件 SIGKILL 观察点；不写权限、原职责或生产期限。

#[cfg(test)]
mod tests;

use std::ffi::OsStr;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use robot_core::time::{MonoRawNs, RobotClock};

use super::w4_file_fault::{command_kind, completion_payload, token_kind};
use crate::boot_supervisor::BootShared;
use crate::clear_estop_transaction::{
    AliasPreparation, ClearTxnStage as S, DirectorySyncEvent as E, DirectorySyncInput,
    FileIdentity, StepPhase,
};
use crate::comm_fault_trace::w4::{
    self, CommandKind as C, FixtureFailure as F, TokenKind as T, W4Payload as P,
};
use crate::latch_writer::{
    Budgets, ClearTxnIdentity, PersistCommand, PersistCompletion, PersistShared, Phase,
    PrepareTokenKind, ProgressView, WriterHook, WriterToken,
};

const PAUSE_NS: u64 = 250_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareAfterPoint {
    SampleMarkerIdentity,
    WriteMarker,
    SyncMarkerFile,
    SyncMarkerDirectory,
    CreateBackup,
    WriteBackup,
    SyncBackupFile,
    SyncBackupDirectory,
    CreatePrimary,
    WritePrimary,
    SyncPrimaryFile,
    RenamePrimary,
}

impl PrepareAfterPoint {
    const fn stage(self) -> S {
        match self {
            Self::SampleMarkerIdentity => S::SampleMarkerIdentity,
            Self::WriteMarker => S::WriteMarker,
            Self::SyncMarkerFile => S::SyncMarkerFile,
            Self::SyncMarkerDirectory => S::SyncMarkerDirectory,
            Self::CreateBackup => S::CreateBackup,
            Self::WriteBackup => S::WriteBackup,
            Self::SyncBackupFile => S::SyncBackupFile,
            Self::SyncBackupDirectory => S::SyncBackupDirectory,
            Self::CreatePrimary => S::CreatePrimary,
            Self::WritePrimary => S::WritePrimary,
            Self::SyncPrimaryFile => S::SyncPrimaryFile,
            Self::RenamePrimary => S::RenamePrimary,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairAfterPoint {
    ReadMarker,
    ValidateMarker,
    CreatePrimary,
    WritePrimary,
    SyncPrimaryFile,
    RenamePrimary,
    SyncPrimaryDirectory,
    CreateBackup,
    WriteBackup,
    SyncBackupFile,
    RenameBackup,
    SyncBackupDirectory,
}

impl RepairAfterPoint {
    const fn stage(self) -> S {
        match self {
            Self::ReadMarker => S::ReadMarker,
            Self::ValidateMarker => S::ValidateMarker,
            Self::CreatePrimary => S::CreatePrimary,
            Self::WritePrimary => S::WritePrimary,
            Self::SyncPrimaryFile => S::SyncPrimaryFile,
            Self::RenamePrimary => S::RenamePrimary,
            Self::SyncPrimaryDirectory => S::SyncPrimaryDirectory,
            Self::CreateBackup => S::CreateBackup,
            Self::WriteBackup => S::WriteBackup,
            Self::SyncBackupFile => S::SyncBackupFile,
            Self::RenameBackup => S::RenameBackup,
            Self::SyncBackupDirectory => S::SyncBackupDirectory,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKillMode {
    PrepareCreateMarker,
    PrepareRenameBackup,
    PrepareSyncPrimaryDirectory,
    FinalizeRemoveMarker,
    FinalizeSyncRemovalDirectory,
    PrepareAfter(PrepareAfterPoint),
    FinalizeBeforeUnlink,
    RepairAfter(RepairAfterPoint),
}

impl FileKillMode {
    const fn point(self) -> (C, S) {
        match self {
            Self::PrepareCreateMarker => (C::Prepare, S::CreateMarker),
            Self::PrepareRenameBackup => (C::Prepare, S::RenameBackup),
            Self::PrepareSyncPrimaryDirectory => (C::Prepare, S::SyncPrimaryDirectory),
            Self::FinalizeRemoveMarker | Self::FinalizeBeforeUnlink => {
                (C::RemoveCommitted, S::RemoveMarker)
            }
            Self::FinalizeSyncRemovalDirectory => (C::RemoveCommitted, S::SyncRemovalDirectory),
            Self::PrepareAfter(point) => (C::Prepare, point.stage()),
            Self::RepairAfter(point) => (C::RepairLatched, point.stage()),
        }
    }

    const fn expanded(self) -> bool {
        matches!(
            self,
            Self::PrepareAfter(_) | Self::FinalizeBeforeUnlink | Self::RepairAfter(_)
        )
    }

    const fn repair(self) -> bool {
        matches!(self, Self::RepairAfter(_))
    }
}

/// 资源准备前只读取一次；无配置不接管原 W4a / pause 路径。
///
/// # Errors
/// 值非法、与其它 writer 故障并存，或配置了非 Linux 场景。
pub fn file_kill_from_env() -> io::Result<Option<FileKillMode>> {
    parse_file_kill(
        std::env::var_os("ROBOT_TEST_W4_FILE_KILL").as_deref(),
        std::env::var_os("ROBOT_TEST_WRITER_PAUSE").is_some(),
        std::env::var_os("ROBOT_TEST_W4_FILE_FAULT").is_some(),
        cfg!(target_os = "linux"),
    )
}

fn parse_file_kill(
    raw: Option<&OsStr>,
    old_pause_present: bool,
    file_fault_present: bool,
    linux: bool,
) -> io::Result<Option<FileKillMode>> {
    let Some(raw) = raw else { return Ok(None) };
    if old_pause_present || file_fault_present {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "W4 SIGKILL 不可与其它 writer 故障并存",
        ));
    }
    let mode = fixed_mode(raw)?;
    if !linux {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "W4 SIGKILL 场景仅支持 Linux",
        ));
    }
    Ok(Some(mode))
}

fn fixed_mode(raw: &OsStr) -> io::Result<FileKillMode> {
    Ok(match raw.to_str() {
        Some("prepare-create-marker") => FileKillMode::PrepareCreateMarker,
        Some("prepare-rename-backup") => FileKillMode::PrepareRenameBackup,
        Some("prepare-sync-primary-directory") => FileKillMode::PrepareSyncPrimaryDirectory,
        Some("finalize-remove-marker") => FileKillMode::FinalizeRemoveMarker,
        Some("finalize-sync-removal-directory") => FileKillMode::FinalizeSyncRemovalDirectory,
        Some("prepare-sample-marker-identity") => {
            FileKillMode::PrepareAfter(PrepareAfterPoint::SampleMarkerIdentity)
        }
        Some("prepare-write-marker") => FileKillMode::PrepareAfter(PrepareAfterPoint::WriteMarker),
        Some("prepare-sync-marker-file") => {
            FileKillMode::PrepareAfter(PrepareAfterPoint::SyncMarkerFile)
        }
        Some("prepare-sync-marker-directory") => {
            FileKillMode::PrepareAfter(PrepareAfterPoint::SyncMarkerDirectory)
        }
        Some("prepare-create-backup") => {
            FileKillMode::PrepareAfter(PrepareAfterPoint::CreateBackup)
        }
        Some("prepare-write-backup") => FileKillMode::PrepareAfter(PrepareAfterPoint::WriteBackup),
        Some("prepare-sync-backup-file") => {
            FileKillMode::PrepareAfter(PrepareAfterPoint::SyncBackupFile)
        }
        Some("prepare-sync-backup-directory") => {
            FileKillMode::PrepareAfter(PrepareAfterPoint::SyncBackupDirectory)
        }
        Some("prepare-create-primary") => {
            FileKillMode::PrepareAfter(PrepareAfterPoint::CreatePrimary)
        }
        Some("prepare-write-primary") => {
            FileKillMode::PrepareAfter(PrepareAfterPoint::WritePrimary)
        }
        Some("prepare-sync-primary-file") => {
            FileKillMode::PrepareAfter(PrepareAfterPoint::SyncPrimaryFile)
        }
        Some("prepare-rename-primary") => {
            FileKillMode::PrepareAfter(PrepareAfterPoint::RenamePrimary)
        }
        Some("finalize-before-unlink") => FileKillMode::FinalizeBeforeUnlink,
        Some("repair-after-read_marker") => FileKillMode::RepairAfter(RepairAfterPoint::ReadMarker),
        Some("repair-after-validate_marker") => {
            FileKillMode::RepairAfter(RepairAfterPoint::ValidateMarker)
        }
        Some("repair-after-create_primary") => {
            FileKillMode::RepairAfter(RepairAfterPoint::CreatePrimary)
        }
        Some("repair-after-write_primary") => {
            FileKillMode::RepairAfter(RepairAfterPoint::WritePrimary)
        }
        Some("repair-after-sync_primary_file") => {
            FileKillMode::RepairAfter(RepairAfterPoint::SyncPrimaryFile)
        }
        Some("repair-after-rename_primary") => {
            FileKillMode::RepairAfter(RepairAfterPoint::RenamePrimary)
        }
        Some("repair-after-sync_primary_directory") => {
            FileKillMode::RepairAfter(RepairAfterPoint::SyncPrimaryDirectory)
        }
        Some("repair-after-create_backup") => {
            FileKillMode::RepairAfter(RepairAfterPoint::CreateBackup)
        }
        Some("repair-after-write_backup") => {
            FileKillMode::RepairAfter(RepairAfterPoint::WriteBackup)
        }
        Some("repair-after-sync_backup_file") => {
            FileKillMode::RepairAfter(RepairAfterPoint::SyncBackupFile)
        }
        Some("repair-after-rename_backup") => {
            FileKillMode::RepairAfter(RepairAfterPoint::RenameBackup)
        }
        Some("repair-after-sync_backup_directory") => {
            FileKillMode::RepairAfter(RepairAfterPoint::SyncBackupDirectory)
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "W4 SIGKILL 只接受三十个固定配置值",
            ));
        }
    })
}

/// 在原 runtime 安装后绑定同一个 writer 与原观测页；只持非 RT Arc。
///
/// # Errors
/// 缺失原安装、writer 不同实例或未配置原观测页。
pub fn make_file_kill_hook(
    mode: FileKillMode,
    clock: Arc<dyn RobotClock>,
    boot: Arc<BootShared>,
    expected_writer: &Arc<PersistShared>,
) -> io::Result<Box<dyn WriterHook>> {
    check_binding(&boot, expected_writer)?;
    Ok(Box::new(FileKillHook::new(mode, clock, boot)))
}

fn check_binding(boot: &BootShared, expected_writer: &Arc<PersistShared>) -> io::Result<()> {
    let binding = boot
        .binding()
        .ok_or_else(|| io::Error::other("W4 SIGKILL 缺少原 runtime 安装"))?;
    if !Arc::ptr_eq(binding.writer(), expected_writer) {
        return Err(io::Error::other("W4 SIGKILL writer 与原 runtime 实例不同"));
    }
    if boot.observation().is_none() {
        return Err(io::Error::other("W4 SIGKILL 缺少原 observation"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct KillScope {
    source: u64,
    command: C,
    kind_sequence: u64,
    step_seq: u64,
    stage: Option<S>,
    succeeded: bool,
    target_occurrence: u64,
    completion_seen: bool,
    sync_input_seen: bool,
}

const PREPARE_STAGES: [S; 15] = [
    S::CreateMarker,
    S::SampleMarkerIdentity,
    S::WriteMarker,
    S::SyncMarkerFile,
    S::SyncMarkerDirectory,
    S::CreateBackup,
    S::WriteBackup,
    S::SyncBackupFile,
    S::RenameBackup,
    S::SyncBackupDirectory,
    S::CreatePrimary,
    S::WritePrimary,
    S::SyncPrimaryFile,
    S::RenamePrimary,
    S::SyncPrimaryDirectory,
];
const REPAIR_STAGES: [S; 12] = [
    S::ReadMarker,
    S::ValidateMarker,
    S::CreatePrimary,
    S::WritePrimary,
    S::SyncPrimaryFile,
    S::RenamePrimary,
    S::SyncPrimaryDirectory,
    S::CreateBackup,
    S::WriteBackup,
    S::SyncBackupFile,
    S::RenameBackup,
    S::SyncBackupDirectory,
];
const REMOVE_STAGES: [S; 4] = [
    S::ReadMarker,
    S::ValidateMarker,
    S::RemoveMarker,
    S::SyncRemovalDirectory,
];

/// 单次末同步故障只保存观察状态，不持有或构造任何事务凭证。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepairPrecursor {
    Armed,
    Selected,
    Expected(FileIdentity),
    Verified,
    Called,
    Returned,
}

fn scope_stages(command: C) -> &'static [S] {
    match command {
        C::Prepare => &PREPARE_STAGES,
        C::RemoveCommitted => &REMOVE_STAGES,
        C::RepairLatched => &REPAIR_STAGES,
        _ => &[],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KillWindow {
    entered_at: u64,
    pause_until: u64,
}

fn checked_window(
    progress: ProgressView,
    base: MonoRawNs,
    armed: u64,
    now: MonoRawNs,
) -> Result<KillWindow, F> {
    let entered_at = progress.entered_at.0;
    if progress.phase != Phase::FileStep
        || entered_at <= base.0
        || armed == 0
        || entered_at < armed
        || now.0 < entered_at
    {
        return Err(F::Protocol);
    }
    let deadline = entered_at
        .checked_add(Budgets::default().step.0)
        .ok_or(F::CounterOverflow)?;
    let pause_until = now.0.checked_add(PAUSE_NS).ok_or(F::CounterOverflow)?;
    if pause_until >= deadline {
        return Err(F::Protocol);
    }
    Ok(KillWindow {
        entered_at,
        pause_until,
    })
}

fn shutdown_is_unarmed(boot: &BootShared) -> bool {
    boot.runtime_words().shutdown_deadline().is_none()
}

struct FileKillHook {
    mode: FileKillMode,
    clock: Arc<dyn RobotClock>,
    boot: Arc<BootShared>,
    binding: Option<ClearTxnIdentity>,
    scope: Option<KillScope>,
    execute_sequence: u64,
    prepare_sequence: u64,
    finalize_sequence: u64,
    consumed: bool,
    failed: bool,
    precursor: RepairPrecursor,
    prepare_ended: bool,
}

impl FileKillHook {
    fn new(mode: FileKillMode, clock: Arc<dyn RobotClock>, boot: Arc<BootShared>) -> Self {
        Self {
            mode,
            clock,
            boot,
            binding: None,
            scope: None,
            execute_sequence: 0,
            prepare_sequence: 0,
            finalize_sequence: 0,
            consumed: false,
            failed: false,
            precursor: RepairPrecursor::Armed,
            prepare_ended: false,
        }
    }

    fn emit(&self, payload: P) {
        w4::emit(
            self.scope
                .map_or(self.execute_sequence, |scope| scope.source),
            payload,
        );
    }

    fn fail(&mut self, kind: F) {
        self.failed = true;
        self.emit(P::Failure {
            kind,
            slot: None,
            step_seq: self.scope.map_or(0, |scope| scope.step_seq),
            errno: None,
        });
    }

    #[allow(
        clippy::single_match_else,
        reason = "按项目枚举分派约定穷举递增成功与溢出，保持原单次受检操作"
    )]
    fn next(&mut self, value: u64) -> Option<u64> {
        match value.checked_add(1) {
            Some(next) => Some(next),
            None => {
                self.fail(F::CounterOverflow);
                None
            }
        }
    }

    fn read_window(&self) -> Result<KillWindow, F> {
        let writer = self.boot.binding().ok_or(F::Protocol)?.writer();
        let progress = writer.progress.load().map_err(|_| F::Protocol)?;
        let armed = self
            .boot
            .observation()
            .ok_or(F::Protocol)?
            .snapshot()
            .runtime_armed_ns;
        checked_window(progress, writer.progress.base(), armed, self.clock.now())
    }

    fn pause_at_point(&mut self, scope: KillScope) {
        self.consumed = true;
        let window = match self.read_window() {
            Ok(window) => window,
            Err(reason) => {
                self.fail(reason);
                return;
            }
        };
        // 只证紧邻 Reached 的本次 Acquire 读为 None，不声称未来不能 arm。
        if !shutdown_is_unarmed(&self.boot) || self.clock.now().0 >= window.pause_until {
            self.fail(F::Protocol);
            return;
        }
        self.emit(P::KillReached {
            step_seq: scope.step_seq,
            stage: scope.stage.expect("真实步骤回调已核 stage"),
            entered_at: window.entered_at,
            pause_until: window.pause_until,
        });
        loop {
            let now = self.clock.now().0;
            if now >= window.pause_until {
                break;
            }
            std::thread::sleep(Duration::from_nanos(
                (window.pause_until - now).min(1_000_000),
            ));
        }
        // 未被父进程及时杀死是夹具失败；不把成功操作改写为合成 IO 失败。
        self.fail(F::Protocol);
    }

    fn is_target(&self, scope: KillScope, stage: S) -> bool {
        let source = if self.mode.point().0 == C::Prepare {
            1
        } else {
            2
        };
        scope.source == source
            && (scope.command, stage) == self.mode.point()
            && scope.kind_sequence == if self.mode.repair() { 2 } else { 1 }
    }

    fn is_precursor(&self, scope: KillScope, stage: S) -> bool {
        self.mode.repair()
            && scope.source == 1
            && scope.command == C::Prepare
            && scope.step_seq == 15
            && stage == S::SyncPrimaryDirectory
    }

    fn occurrence(&self, scope: KillScope, stage: S) -> u64 {
        if self.is_target(scope, stage) || self.is_precursor(scope, stage) {
            scope.target_occurrence
        } else {
            0
        }
    }

    fn check_expanded_begin(&mut self, scope: KillScope, token: T) {
        let valid = match (scope.source, scope.command, token) {
            (1, C::Prepare, T::None) => true,
            (2, C::RemoveCommitted, T::Prepared) => {
                self.mode == FileKillMode::FinalizeBeforeUnlink && self.prepare_ended
            }
            (2, C::RepairLatched, T::Durable) => self.mode.repair() && self.prepare_ended,
            _ => false,
        };
        if !valid {
            self.fail(F::Protocol);
        }
    }

    fn check_expanded_completion(&mut self, completion: &PersistCompletion, token: T) {
        let Some(mut scope) = self.scope else { return };
        let last_step = u64::try_from(scope_stages(scope.command).len()).unwrap_or(u64::MAX);
        let complete_steps = scope.step_seq == last_step;
        let valid = match *completion {
            PersistCompletion::PrepareFailed {
                txn,
                stage: S::SyncPrimaryDirectory,
                phase: StepPhase::Operation,
                token: PrepareTokenKind::Durable,
            } => {
                self.mode.repair()
                    && scope.source == 1
                    && scope.command == C::Prepare
                    && self.binding == Some(txn)
                    && token == T::Durable
                    && complete_steps
                    && !scope.succeeded
                    && self.precursor == RepairPrecursor::Returned
            }
            PersistCompletion::Prepared { txn, .. } => {
                !self.mode.repair()
                    && scope.source == 1
                    && scope.command == C::Prepare
                    && self.binding == Some(txn)
                    && token == T::Prepared
                    && complete_steps
                    && scope.succeeded
            }
            PersistCompletion::Repaired {
                txn,
                target: robot_core::control_authority::RepairTarget::Latched,
                ..
            } => {
                self.mode.repair()
                    && scope.source == 2
                    && scope.command == C::RepairLatched
                    && self.binding == Some(txn)
                    && token == T::Repaired
                    && complete_steps
                    && scope.succeeded
            }
            PersistCompletion::MarkerRemoved {
                txn,
                kind: robot_core::control_authority::CleanupKind::Finalize,
                ..
            } => {
                self.mode == FileKillMode::FinalizeBeforeUnlink
                    && scope.source == 2
                    && scope.command == C::RemoveCommitted
                    && self.binding == Some(txn)
                    && token == T::None
                    && complete_steps
                    && scope.succeeded
            }
            _ => false,
        };
        if scope.completion_seen || !valid {
            self.fail(F::Protocol);
        }
        scope.completion_seen = true;
        self.scope = Some(scope);
    }

    fn fail_precursor(&mut self, kind: F, errno: Option<i32>) {
        self.failed = true;
        self.emit(P::Failure {
            kind,
            slot: Some(0),
            step_seq: self.scope.map_or(0, |scope| scope.step_seq),
            errno,
        });
    }

    fn observe_precursor(&mut self, event: E) {
        match (self.precursor, event) {
            (RepairPrecursor::Selected, E::Expected(identity)) => {
                self.precursor = RepairPrecursor::Expected(identity);
                self.emit(P::Alias {
                    verified: false,
                    slot: 0,
                    step_seq: 15,
                    dev: identity.dev(),
                    ino: identity.ino(),
                });
            }
            (RepairPrecursor::Expected(expected), E::Verified(identity)) => {
                self.emit(P::Alias {
                    verified: true,
                    slot: 0,
                    step_seq: 15,
                    dev: identity.dev(),
                    ino: identity.ino(),
                });
                if expected == identity {
                    self.precursor = RepairPrecursor::Verified;
                } else {
                    self.fail_precursor(F::Identity, None);
                }
            }
            (RepairPrecursor::Expected(_), E::PreparationFailed { phase, errno }) => {
                let kind = match phase {
                    AliasPreparation::Open => F::Open,
                    AliasPreparation::Metadata => F::Metadata,
                    AliasPreparation::Identity => F::Identity,
                };
                self.fail_precursor(kind, errno);
            }
            (RepairPrecursor::Verified, E::Call) => {
                self.precursor = RepairPrecursor::Called;
                self.emit(P::DirSyncCall {
                    slot: 0,
                    step_seq: 15,
                });
            }
            (RepairPrecursor::Called, E::Returned { success, errno }) => {
                self.emit(P::DirSyncReturned {
                    slot: 0,
                    step_seq: 15,
                    success,
                    errno,
                });
                if !success && errno == Some(libc::EBADF) {
                    self.precursor = RepairPrecursor::Returned;
                } else {
                    self.fail_precursor(F::UnexpectedResult, errno);
                }
            }
            _ => self.fail_precursor(F::Protocol, None),
        }
    }
}

impl WriterHook for FileKillHook {
    fn command_begin(&mut self, command: PersistCommand, token: Option<&WriterToken>) {
        if self.scope.is_some() {
            self.fail(F::Protocol);
            self.scope = None;
        }
        let Some(source) = self.next(self.execute_sequence) else {
            return;
        };
        self.execute_sequence = source;
        let kind = command_kind(command);
        let kind_sequence = match kind {
            C::Prepare => {
                let Some(next) = self.next(self.prepare_sequence) else {
                    return;
                };
                self.prepare_sequence = next;
                next
            }
            C::RemoveCommitted => {
                let Some(next) = self.next(self.finalize_sequence) else {
                    return;
                };
                self.finalize_sequence = next;
                next
            }
            _ => source,
        };
        self.scope = Some(KillScope {
            source,
            command: kind,
            kind_sequence,
            step_seq: 0,
            stage: None,
            succeeded: false,
            target_occurrence: 0,
            completion_seen: false,
            sync_input_seen: false,
        });
        let txn = command.txn();
        self.emit(P::CommandBegin { txn, command: kind });
        self.emit(P::Token {
            after: false,
            token: token_kind(token),
        });
        match self.binding {
            None if source == 1 && kind == C::Prepare && txn.original_latched => {
                self.binding = Some(txn);
            }
            Some(bound)
                if bound == txn
                    && (self.mode.expanded()
                        || (kind == C::RemoveCommitted && kind_sequence == 1)) => {}
            _ => self.fail(F::Protocol),
        }
        if self.mode.expanded() {
            self.check_expanded_begin(
                self.scope.expect("刚安装的真实命令 scope"),
                token_kind(token),
            );
        }
    }

    fn command_completed(&mut self, completion: &PersistCompletion, token: Option<&WriterToken>) {
        if self.scope.is_none() {
            self.fail(F::Protocol);
            return;
        }
        self.emit(completion_payload(completion));
        self.emit(P::Token {
            after: true,
            token: token_kind(token),
        });
        if self.mode.expanded() {
            self.check_expanded_completion(completion, token_kind(token));
        }
    }

    fn command_scope_exit(&mut self, completed: bool) {
        let Some(scope) = self.scope else { return };
        if !completed || (scope.command == self.mode.point().0 && !self.consumed) {
            self.fail(F::Protocol);
        }
        if self.mode.expanded() {
            if !scope.completion_seen {
                self.fail(F::Protocol);
            }
            if completed && scope.source == 1 && scope.completion_seen && !self.failed {
                self.prepare_ended = true;
            }
        }
        self.emit(P::CommandEnd { completed });
        self.scope = None;
    }

    fn outside_command(&mut self) {
        if self.scope.is_some() {
            self.fail(F::Protocol);
            self.scope = None;
        }
    }

    fn before_step(&mut self, stage: S) {
        let Some(mut scope) = self.scope else { return };
        if self.mode.expanded() {
            let expected = usize::try_from(scope.step_seq)
                .ok()
                .and_then(|index| scope_stages(scope.command).get(index))
                .copied();
            if expected != Some(stage)
                || scope.completion_seen
                || (scope.step_seq != 0 && !scope.succeeded)
            {
                self.fail(F::Protocol);
            }
        }
        let Some(next) = self.next(scope.step_seq) else {
            return;
        };
        scope.step_seq = next;
        scope.stage = Some(stage);
        scope.succeeded = false;
        scope.sync_input_seen = false;
        if self.is_target(scope, stage) || self.is_precursor(scope, stage) {
            let Some(occurrence) = self.next(scope.target_occurrence) else {
                return;
            };
            scope.target_occurrence = occurrence;
        }
        self.scope = Some(scope);
        self.emit(P::Stage {
            succeeded: false,
            step_seq: next,
            stage,
            occurrence: self.occurrence(scope, stage),
        });
        if !self.failed
            && !self.consumed
            && self.mode == FileKillMode::FinalizeBeforeUnlink
            && self.is_target(scope, stage)
        {
            // WriterGuard 已发布原 FileStep，且本函数刚发出真实 Stage-before。
            self.pause_at_point(scope);
        }
    }

    fn after_step_success(&mut self, stage: S) {
        let Some(mut scope) = self.scope else { return };
        if scope.stage != Some(stage) || scope.succeeded || scope.completion_seen {
            self.fail(F::Protocol);
            return;
        }
        scope.succeeded = true;
        self.scope = Some(scope);
        self.emit(P::Stage {
            succeeded: true,
            step_seq: scope.step_seq,
            stage,
            occurrence: self.occurrence(scope, stage),
        });
        if self.is_precursor(scope, stage) {
            self.fail_precursor(F::UnexpectedResult, None);
        }
        if !self.failed
            && !self.consumed
            && self.mode != FileKillMode::FinalizeBeforeUnlink
            && self.is_target(scope, stage)
        {
            self.pause_at_point(scope);
        }
    }

    fn directory_sync_input(&mut self, stage: S) -> DirectorySyncInput {
        if !self.mode.repair() || self.failed {
            return DirectorySyncInput::Original;
        }
        let Some(mut scope) = self.scope else {
            self.fail_precursor(F::Protocol, None);
            return DirectorySyncInput::Original;
        };
        if scope.stage != Some(stage)
            || scope.sync_input_seen
            || scope.succeeded
            || !matches!(
                stage,
                S::SyncMarkerDirectory | S::SyncBackupDirectory | S::SyncPrimaryDirectory
            )
        {
            self.fail_precursor(F::Protocol, None);
            return DirectorySyncInput::Original;
        }
        scope.sync_input_seen = true;
        self.scope = Some(scope);
        if !self.is_precursor(scope, stage) {
            return DirectorySyncInput::Original;
        }
        if self.precursor != RepairPrecursor::Armed {
            self.fail_precursor(F::Protocol, None);
            return DirectorySyncInput::Original;
        }
        self.precursor = RepairPrecursor::Selected;
        self.emit(P::FaultSelected {
            slot: 0,
            step_seq: 15,
            stage,
            occurrence: 1,
        });
        DirectorySyncInput::OPathAlias
    }

    fn directory_sync_event(&mut self, stage: S, event: E) {
        let Some(scope) = self.scope else {
            self.fail_precursor(F::Protocol, None);
            return;
        };
        if !self.is_precursor(scope, stage)
            || scope.stage != Some(stage)
            || scope.succeeded
            || !scope.sync_input_seen
            || self.failed
        {
            self.fail_precursor(F::Protocol, None);
            return;
        }
        self.observe_precursor(event);
    }
}
