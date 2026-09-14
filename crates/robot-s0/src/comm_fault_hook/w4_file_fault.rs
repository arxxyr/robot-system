//! W4 固定文件故障输入；只观察原命令、凭证和 syscall，不参与许可或退休。

#[cfg(test)]
mod tests;

use crate::clear_estop_transaction::{
    AliasPreparation, ClearTxnStage as S, DirectorySyncEvent as E, DirectorySyncInput, StepPhase,
};
use crate::comm_fault_trace::w4::{
    self, CommandKind as C, CompletionKind as K, FixtureFailure as F, TokenKind as T,
    W4Payload as P,
};
use crate::latch_writer::{
    ClearTxnIdentity, PersistCommand, PersistCompletion, PrepareTokenKind, RemoveTokenKind,
    WriterHook, WriterRefusal, WriterToken,
};
use robot_core::control_authority::{CleanupKind, RepairTarget};
use robot_core::time::RobotClock;
use std::ffi::OsStr;
use std::io;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFaultMode {
    Created,
    CreatedUnsynced,
    OrdinaryResync,
    OrdinaryResyncExhausted,
}

/// 新配置只在 daemon 的资源准备之前读一次；没有配置时不触碰旧 pause 解析。
///
/// # Errors
/// 非法值、与旧 pause 并存，或非 Linux 平台配置了该故障计划。
pub fn file_fault_from_env() -> io::Result<Option<FileFaultMode>> {
    parse_file_fault(
        std::env::var_os("ROBOT_TEST_W4_FILE_FAULT").as_deref(),
        std::env::var_os("ROBOT_TEST_WRITER_PAUSE").is_some(),
        cfg!(target_os = "linux"),
    )
}

fn parse_file_fault(
    raw: Option<&OsStr>,
    old_pause_present: bool,
    linux: bool,
) -> io::Result<Option<FileFaultMode>> {
    let Some(raw) = raw else { return Ok(None) };
    if old_pause_present {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "W4 文件故障与旧 writer pause 不可同时配置",
        ));
    }
    let mode = match raw.to_str() {
        Some("created") => FileFaultMode::Created,
        Some("created-unsynced") => FileFaultMode::CreatedUnsynced,
        Some("ordinary-resync") => FileFaultMode::OrdinaryResync,
        Some("ordinary-resync-exhausted") => FileFaultMode::OrdinaryResyncExhausted,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ROBOT_TEST_W4_FILE_FAULT 仅接受 created / created-unsynced / ordinary-resync / ordinary-resync-exhausted",
            ));
        }
    };
    if !linux {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "W4 O_PATH 故障输入仅支持 Linux",
        ));
    }
    Ok(Some(mode))
}

/// 唯一创建点；None 延用原解析时机和旧单次屏障。
///
/// # Errors
/// 旧 pause 配置格式无效。
pub fn writer_hook(
    mode: Option<FileFaultMode>,
    clock: Arc<dyn RobotClock>,
) -> io::Result<Box<dyn WriterHook>> {
    match mode {
        Some(mode) => Ok(Box::new(FileFaultHook::new(mode))),
        None => Ok(Box::new(super::WriterStepPause::from_env(clock)?)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FaultSlot {
    Armed,
    Selected,
    Executed { bad_fd: bool },
    PreparationFailed,
}
#[derive(Debug, Clone, Copy)]
struct Selected {
    slot: u8,
    fault_index: Option<usize>,
    stage: S,
    step_seq: u64,
    phase: AliasPhase,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AliasPhase {
    Selected,
    Expected,
    Verified,
    Called,
    Returned,
}
#[derive(Debug, Clone, Copy)]
struct Scope {
    source: u64,
    command: C,
    kind_sequence: u64,
    step_seq: u64,
    stage: Option<S>,
    occurrences: [u64; 2],
    selected: Option<Selected>,
    ordinary: Option<OrdinaryStep>,
    completion_seen: bool,
    sync_input_seen: bool,
    stage_succeeded: bool,
}

/// 仅约束固定夹具的真实观察顺序，不持有或构造 writer 令牌。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrdinaryStep {
    Prepare,
    Remove,
    Resync,
    Retry,
    Complete,
}

/// 十项各自保存原故障状态；wire slot 仍只有 Remove/Resync 两种角色。
struct ExhaustedPlan {
    faults: [FaultSlot; 10],
    next_source: u64,
}

const EXHAUSTED_PREPARE_STAGES: [S; 15] = [
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

struct FileFaultHook {
    slots: [Option<FaultSlot>; 2],
    binding: Option<ClearTxnIdentity>,
    execute_sequence: u64,
    prepare_sequence: u64,
    repair_sequence: u64,
    scope: Option<Scope>,
    created_proven: bool,
    ordinary: Option<OrdinaryStep>,
    exhausted: Option<ExhaustedPlan>,
    failed: bool,
}
impl FileFaultHook {
    const fn new(mode: FileFaultMode) -> Self {
        Self {
            slots: [
                Some(FaultSlot::Armed),
                match mode {
                    FileFaultMode::Created => None,
                    FileFaultMode::CreatedUnsynced
                    | FileFaultMode::OrdinaryResync
                    | FileFaultMode::OrdinaryResyncExhausted => Some(FaultSlot::Armed),
                },
            ],
            binding: None,
            execute_sequence: 0,
            prepare_sequence: 0,
            repair_sequence: 0,
            scope: None,
            created_proven: false,
            ordinary: match mode {
                FileFaultMode::OrdinaryResync => Some(OrdinaryStep::Prepare),
                FileFaultMode::Created
                | FileFaultMode::CreatedUnsynced
                | FileFaultMode::OrdinaryResyncExhausted => None,
            },
            exhausted: match mode {
                FileFaultMode::OrdinaryResyncExhausted => Some(ExhaustedPlan {
                    faults: [FaultSlot::Armed; 10],
                    next_source: 1,
                }),
                _ => None,
            },
            failed: false,
        }
    }
    fn emit(&self, payload: P) {
        w4::emit(
            self.scope
                .map_or(self.execute_sequence, |scope| scope.source),
            payload,
        );
    }
    fn fail(&mut self, kind: F, slot: Option<u8>, errno: Option<i32>) {
        self.failed = true;
        self.emit(P::Failure {
            kind,
            slot,
            step_seq: self.scope.map_or(0, |scope| scope.step_seq),
            errno,
        });
    }
    fn count(&mut self, value: u64) -> Option<u64> {
        let next = value.checked_add(1);
        if next.is_none() {
            self.fail(F::CounterOverflow, None, None);
        }
        next
    }
    fn occurrence(scope: Scope, stage: S) -> u64 {
        target_index(stage).map_or(0, |index| scope.occurrences[index])
    }

    fn fault_state(&self, slot: u8, index: Option<usize>) -> Option<FaultSlot> {
        match index {
            Some(index) => self.exhausted.as_ref()?.faults.get(index).copied(),
            None => self.slots[usize::from(slot)],
        }
    }

    fn set_fault_state(&mut self, slot: u8, index: Option<usize>, state: FaultSlot) {
        match index {
            Some(index) => match self
                .exhausted
                .as_mut()
                .and_then(|plan| plan.faults.get_mut(index))
            {
                Some(original) => *original = state,
                None => self.fail(F::Protocol, Some(slot), None),
            },
            None => self.slots[usize::from(slot)] = Some(state),
        }
    }

    fn begin_exhausted(&mut self, scope: Scope, token: T) {
        let expected = match scope.source {
            1 => (C::Prepare, T::None),
            2 => (C::RemoveCommitted, T::Prepared),
            3..=11 => (C::Resync, T::Unsynced),
            _ => {
                self.fail(F::Protocol, None, None);
                return;
            }
        };
        if (scope.command, token) != expected
            || self
                .exhausted
                .as_ref()
                .is_none_or(|plan| plan.next_source != scope.source)
        {
            self.fail(F::Protocol, None, None);
        }
    }

    fn before_exhausted_step(&mut self, scope: Scope, stage: S, next: u64) {
        let expected = match (scope.source, next) {
            (1, 1..=15) => EXHAUSTED_PREPARE_STAGES
                .get(usize::try_from(next - 1).expect("最多15步"))
                .copied(),
            (2, 1) => Some(S::ReadMarker),
            (2, 2) => Some(S::ValidateMarker),
            (2, 3) => Some(S::RemoveMarker),
            (2, 4) | (3..=11, 1) => Some(S::SyncRemovalDirectory),
            _ => None,
        };
        if expected != Some(stage) || (scope.step_seq != 0 && !scope.stage_succeeded) {
            self.fail(F::Protocol, None, None);
        }
    }

    fn complete_exhausted(&mut self, scope: Scope, completion: &PersistCompletion, token: T) {
        let valid = match (scope.source, *completion, token) {
            (1, PersistCompletion::Prepared { txn, .. }, T::Prepared) => {
                self.binding == Some(txn) && scope.step_seq == 15 && scope.stage_succeeded
            }
            (
                source @ 2..=11,
                PersistCompletion::MarkerRemoveFailed {
                    txn,
                    stage: S::SyncRemovalDirectory,
                    phase: StepPhase::Operation,
                    token: RemoveTokenKind::Unsynced,
                },
                T::Unsynced,
            ) => {
                let index = usize::try_from(source - 2).expect("最多十项故障");
                self.binding == Some(txn)
                    && scope.step_seq == if source == 2 { 4 } else { 1 }
                    && scope.sync_input_seen
                    && !scope.stage_succeeded
                    && self.fault_state(u8::from(source != 2), Some(index))
                        == Some(FaultSlot::Executed { bad_fd: true })
            }
            _ => false,
        };
        if !valid {
            self.fail(F::Protocol, None, None);
        }
    }

    fn finish_exhausted_scope(&mut self, scope: Scope, completed: bool) {
        if !completed || !scope.completion_seen || self.failed {
            self.fail(F::Protocol, None, None);
        } else if let Some(plan) = self.exhausted.as_mut() {
            plan.next_source = scope.source + 1;
        }
        self.emit(P::CommandEnd { completed });
        self.scope = None;
    }

    fn fault_for_scope(&mut self, scope: Scope, stage: S) -> Option<(u8, Option<usize>)> {
        if self.exhausted.is_some() {
            return match (scope.source, stage, Self::occurrence(scope, stage)) {
                (1, _, _) => None,
                (source @ 2..=11, S::SyncRemovalDirectory, 1) => Some((
                    u8::from(source != 2),
                    Some(usize::try_from(source - 2).expect("最多十项故障")),
                )),
                _ => {
                    self.fail(F::Protocol, None, None);
                    None
                }
            };
        }
        let slot = match (
            scope.ordinary,
            scope.command,
            scope.kind_sequence,
            stage,
            Self::occurrence(scope, stage),
        ) {
            (Some(OrdinaryStep::Remove), C::RemoveCommitted, _, S::SyncRemovalDirectory, 1)
            | (None, C::Prepare, 1, S::SyncMarkerDirectory, 1) => 0,
            (Some(OrdinaryStep::Resync), C::Resync, _, S::SyncRemovalDirectory, 1) => 1,
            (None, C::RepairLatched, 1, S::SyncRemovalDirectory, 1) if self.created_proven => 1,
            _ => return None,
        };
        Some((slot, None))
    }

    fn begin_ordinary(&mut self, step: OrdinaryStep, command: C, token: T) {
        let expected = match step {
            OrdinaryStep::Prepare => (C::Prepare, T::None),
            OrdinaryStep::Remove => (C::RemoveCommitted, T::Prepared),
            OrdinaryStep::Resync | OrdinaryStep::Retry => (C::Resync, T::Unsynced),
            OrdinaryStep::Complete => {
                self.fail(F::Protocol, None, None);
                return;
            }
        };
        if (command, token) != expected {
            self.fail(F::Protocol, None, None);
        }
    }

    fn complete_ordinary(&mut self, step: OrdinaryStep, completion: &PersistCompletion, token: T) {
        let next = match (step, *completion, token) {
            (OrdinaryStep::Prepare, PersistCompletion::Prepared { txn, .. }, T::Prepared)
                if self.binding == Some(txn)
                    && self
                        .scope
                        .is_some_and(|scope| scope.step_seq > 0 && scope.stage_succeeded) =>
            {
                Some(OrdinaryStep::Remove)
            }
            (
                OrdinaryStep::Remove | OrdinaryStep::Resync,
                PersistCompletion::MarkerRemoveFailed {
                    txn,
                    stage: S::SyncRemovalDirectory,
                    phase: StepPhase::Operation,
                    token: RemoveTokenKind::Unsynced,
                },
                T::Unsynced,
            ) if self.binding == Some(txn) => {
                let (slot, next) = match step {
                    OrdinaryStep::Remove => (0, OrdinaryStep::Resync),
                    _ => (1, OrdinaryStep::Retry),
                };
                (self.slots[slot] == Some(FaultSlot::Executed { bad_fd: true })).then_some(next)
            }
            (OrdinaryStep::Retry, PersistCompletion::Resynced { txn }, T::None)
                if self.binding == Some(txn)
                    && self.scope.is_some_and(|scope| {
                        scope.step_seq == 1
                            && scope.stage == Some(S::SyncRemovalDirectory)
                            && scope.sync_input_seen
                            && scope.stage_succeeded
                            && scope.selected.is_none()
                    }) =>
            {
                Some(OrdinaryStep::Complete)
            }
            _ => None,
        };
        match next {
            Some(next) if !self.failed => self.ordinary = Some(next),
            _ => self.fail(F::Protocol, None, None),
        }
    }
}

fn target_index(stage: S) -> Option<usize> {
    match stage {
        S::SyncMarkerDirectory => Some(0),
        S::SyncRemovalDirectory => Some(1),
        _ => None,
    }
}
pub(super) fn command_kind(command: PersistCommand) -> C {
    match command {
        PersistCommand::PrepareClear { .. } => C::Prepare,
        PersistCommand::RepairClear {
            target: RepairTarget::Latched,
            ..
        } => C::RepairLatched,
        PersistCommand::RepairClear {
            target: RepairTarget::OriginalUnlatched,
            ..
        } => C::RepairOriginal,
        PersistCommand::RemoveMarker {
            kind: CleanupKind::Finalize,
            ..
        } => C::RemoveCommitted,
        PersistCommand::RemoveMarker {
            kind: CleanupKind::Repaired(RepairTarget::Latched),
            ..
        } => C::RemoveRepaired,
        PersistCommand::RemoveMarker {
            kind: CleanupKind::Repaired(RepairTarget::OriginalUnlatched),
            ..
        } => C::RemoveRepairedOriginal,
        PersistCommand::ResyncRemoval { .. } => C::Resync,
    }
}
pub(super) fn token_kind(token: Option<&WriterToken>) -> T {
    match token {
        None => T::None,
        Some(WriterToken::NoMarker) => T::NoMarker,
        Some(WriterToken::Prepared(_)) => T::Prepared,
        Some(WriterToken::Created(_)) => T::Created,
        Some(WriterToken::Durable(_)) => T::Durable,
        Some(WriterToken::Repaired(_)) => T::Repaired,
        Some(WriterToken::Unsynced(_)) => T::Unsynced,
        Some(WriterToken::UnsyncedBeforeRepair {
            target: RepairTarget::Latched,
            ..
        }) => T::UnsyncedBeforeRepairLatched,
        Some(WriterToken::UnsyncedBeforeRepair {
            target: RepairTarget::OriginalUnlatched,
            ..
        }) => T::UnsyncedBeforeRepairOriginal,
        Some(WriterToken::Lost) => T::Lost,
    }
}
pub(super) fn completion_payload(completion: &PersistCompletion) -> P {
    let (kind, failure) = match *completion {
        PersistCompletion::Prepared { .. } => (K::Prepared, None),
        PersistCompletion::PrepareFailed {
            stage,
            phase,
            token,
            ..
        } => (
            match token {
                PrepareTokenKind::NotOwned => K::PrepareNotOwned,
                PrepareTokenKind::Created => K::PrepareCreated,
                PrepareTokenKind::Durable => K::PrepareDurable,
            },
            Some((stage, phase)),
        ),
        PersistCompletion::Repaired { target, .. } => (
            match target {
                RepairTarget::Latched => K::RepairedLatched,
                RepairTarget::OriginalUnlatched => K::RepairedOriginal,
            },
            None,
        ),
        PersistCompletion::RepairFailed {
            stage,
            phase,
            credential_kept,
            ..
        } => (
            if credential_kept {
                K::RepairFailedKept
            } else {
                K::RepairFailedLost
            },
            Some((stage, phase)),
        ),
        PersistCompletion::MarkerRemoved { kind, .. } => (
            match kind {
                CleanupKind::Finalize => K::RemovedCommitted,
                CleanupKind::Repaired(RepairTarget::Latched) => K::RemovedRepaired,
                CleanupKind::Repaired(RepairTarget::OriginalUnlatched) => {
                    K::RemovedRepairedOriginal
                }
            },
            None,
        ),
        PersistCompletion::MarkerRemoveFailed {
            stage,
            phase,
            token,
            ..
        } => (
            match token {
                RemoveTokenKind::Retained => K::RemoveFailedRetained,
                RemoveTokenKind::Unsynced => K::RemoveFailedUnsynced,
                RemoveTokenKind::Lost => K::RemoveFailedLost,
            },
            Some((stage, phase)),
        ),
        PersistCompletion::Resynced { .. } => (K::Resynced, None),
        PersistCompletion::Refused { reason, .. } => (
            match reason {
                WriterRefusal::IdentityMismatch => K::RefusedIdentity,
                WriterRefusal::EStopPersistOutstanding => K::RefusedObligation,
                WriterRefusal::ShuttingDown => K::RefusedStopping,
                WriterRefusal::NoTransaction => K::RefusedNoTransaction,
                WriterRefusal::PairMismatch => K::RefusedPair,
                WriterRefusal::TokenStateMismatch => K::RefusedToken,
            },
            None,
        ),
    };
    P::Completion { kind, failure }
}

impl WriterHook for FileFaultHook {
    fn command_begin(&mut self, command: PersistCommand, token: Option<&WriterToken>) {
        if self.scope.is_some() {
            self.fail(F::Protocol, None, None);
            self.scope = None;
        }
        let Some(sequence) = self.count(self.execute_sequence) else {
            return;
        };
        self.execute_sequence = sequence;
        let kind = command_kind(command);
        let kind_sequence = match kind {
            C::Prepare => {
                let Some(next) = self.count(self.prepare_sequence) else {
                    return;
                };
                self.prepare_sequence = next;
                next
            }
            C::RepairLatched | C::RepairOriginal => {
                let Some(next) = self.count(self.repair_sequence) else {
                    return;
                };
                self.repair_sequence = next;
                next
            }
            C::RemoveCommitted | C::RemoveRepaired | C::RemoveRepairedOriginal | C::Resync => {
                sequence
            }
        };
        self.scope = Some(Scope {
            source: sequence,
            command: kind,
            kind_sequence,
            step_seq: 0,
            stage: None,
            occurrences: [0; 2],
            selected: None,
            ordinary: self.ordinary,
            completion_seen: false,
            sync_input_seen: false,
            stage_succeeded: false,
        });
        let txn = command.txn();
        self.emit(P::CommandBegin { txn, command: kind });
        self.emit(P::Token {
            after: false,
            token: token_kind(token),
        });
        match self.binding {
            None if sequence == 1 && kind == C::Prepare && txn.original_latched => {
                self.binding = Some(txn);
            }
            Some(bound) if bound == txn && kind != C::Prepare => {}
            _ => self.fail(F::Protocol, None, None),
        }
        if let Some(step) = self.ordinary {
            self.begin_ordinary(step, kind, token_kind(token));
        }
        if self.exhausted.is_some()
            && let Some(scope) = self.scope
        {
            self.begin_exhausted(scope, token_kind(token));
        }
        if matches!(kind, C::RepairLatched | C::RepairOriginal)
            && kind_sequence == 1
            && kind != C::RepairLatched
        {
            self.fail(F::Protocol, None, None);
        }
    }

    fn command_completed(&mut self, completion: &PersistCompletion, token: Option<&WriterToken>) {
        let Some(mut scope) = self.scope else {
            self.fail(F::Protocol, None, None);
            return;
        };
        let payload = completion_payload(completion);
        self.emit(payload);
        self.emit(P::Token {
            after: true,
            token: token_kind(token),
        });
        if self.exhausted.is_some() {
            if scope.completion_seen {
                self.fail(F::Protocol, None, None);
                return;
            }
            scope.completion_seen = true;
            self.scope = Some(scope);
            self.complete_exhausted(scope, completion, token_kind(token));
            return;
        }
        if let Some(step) = scope.ordinary {
            if scope.completion_seen {
                self.fail(F::Protocol, None, None);
                return;
            }
            scope.completion_seen = true;
            self.scope = Some(scope);
            self.complete_ordinary(step, completion, token_kind(token));
            return;
        }
        if scope.command == C::Prepare {
            self.created_proven =
                matches!(self.slots[0], Some(FaultSlot::Executed { bad_fd: true }))
                    && token_kind(token) == T::Created
                    && matches!(
                        payload,
                        P::Completion {
                            kind: K::PrepareCreated,
                            failure: Some((S::SyncMarkerDirectory, StepPhase::Operation))
                        }
                    );
            if !self.created_proven {
                self.fail(F::Protocol, Some(0), None);
            }
        }
    }

    fn command_scope_exit(&mut self, completed: bool) {
        let Some(scope) = self.scope else { return };
        if self.exhausted.is_some() {
            self.finish_exhausted_scope(scope, completed);
            return;
        }
        if !completed {
            self.fail(F::Protocol, None, None);
        }
        if scope.ordinary.is_some() && !scope.completion_seen {
            self.fail(F::Protocol, None, None);
        }
        let expected = match (scope.ordinary, scope.command, scope.kind_sequence) {
            (Some(OrdinaryStep::Remove), _, _) | (None, C::Prepare, 1) => Some(0),
            (Some(OrdinaryStep::Resync), _, _) => Some(1),
            (None, C::RepairLatched, 1) if self.slots[1].is_some() => Some(1),
            _ => None,
        };
        if let Some(slot) = expected
            && self.slots[usize::from(slot)] == Some(FaultSlot::Armed)
        {
            self.fail(F::Protocol, Some(slot), None);
        }
        self.emit(P::CommandEnd { completed });
        self.scope = None;
    }

    fn outside_command(&mut self) {
        if self.scope.is_some() {
            self.fail(F::Protocol, None, None);
            self.scope = None;
        }
    }

    fn before_step(&mut self, stage: S) {
        let Some(mut scope) = self.scope else { return };
        let Some(next) = self.count(scope.step_seq) else {
            return;
        };
        if self.exhausted.is_some() {
            self.before_exhausted_step(scope, stage, next);
        }
        scope.step_seq = next;
        scope.stage = Some(stage);
        scope.selected = None;
        scope.sync_input_seen = false;
        scope.stage_succeeded = false;
        if matches!(
            scope.ordinary,
            Some(OrdinaryStep::Resync | OrdinaryStep::Retry)
        ) && (stage != S::SyncRemovalDirectory || next != 1)
        {
            self.fail(F::Protocol, None, None);
        }
        if let Some(index) = target_index(stage) {
            let Some(next) = self.count(scope.occurrences[index]) else {
                return;
            };
            scope.occurrences[index] = next;
        }
        self.scope = Some(scope);
        self.emit(P::Stage {
            succeeded: false,
            step_seq: scope.step_seq,
            stage,
            occurrence: Self::occurrence(scope, stage),
        });
    }

    fn after_step_success(&mut self, stage: S) {
        let Some(mut scope) = self.scope else { return };
        if scope.stage != Some(stage) || (self.exhausted.is_some() && scope.stage_succeeded) {
            self.fail(F::Protocol, None, None);
            return;
        }
        scope.stage_succeeded = true;
        self.scope = Some(scope);
        self.emit(P::Stage {
            succeeded: true,
            step_seq: scope.step_seq,
            stage,
            occurrence: Self::occurrence(scope, stage),
        });
    }

    fn directory_sync_input(&mut self, stage: S) -> DirectorySyncInput {
        let Some(mut scope) = self.scope else {
            return DirectorySyncInput::Original;
        };
        if self.failed {
            return DirectorySyncInput::Original;
        }
        if scope.stage != Some(stage) {
            self.fail(F::Protocol, None, None);
            return DirectorySyncInput::Original;
        }
        if scope.ordinary.is_some() || self.exhausted.is_some() {
            if scope.sync_input_seen {
                self.fail(F::Protocol, None, None);
                return DirectorySyncInput::Original;
            }
            scope.sync_input_seen = true;
            self.scope = Some(scope);
        }
        let Some((slot, fault_index)) = self.fault_for_scope(scope, stage) else {
            return DirectorySyncInput::Original;
        };
        match self.fault_state(slot, fault_index) {
            None => return DirectorySyncInput::Original,
            Some(FaultSlot::Armed) => {}
            Some(_) => {
                self.fail(F::Protocol, Some(slot), None);
                return DirectorySyncInput::Original;
            }
        }
        self.set_fault_state(slot, fault_index, FaultSlot::Selected);
        scope.selected = Some(Selected {
            slot,
            fault_index,
            stage,
            step_seq: scope.step_seq,
            phase: AliasPhase::Selected,
        });
        self.scope = Some(scope);
        self.emit(P::FaultSelected {
            slot,
            step_seq: scope.step_seq,
            stage,
            occurrence: Self::occurrence(scope, stage),
        });
        DirectorySyncInput::OPathAlias
    }

    fn directory_sync_event(&mut self, stage: S, event: E) {
        let Some(mut scope) = self.scope else {
            self.fail(F::Protocol, None, None);
            return;
        };
        let Some(mut selected) = scope.selected else {
            self.fail(F::Protocol, None, None);
            return;
        };
        let slot = selected.slot;
        if selected.stage != stage || selected.step_seq != scope.step_seq {
            self.fail(F::Protocol, Some(slot), None);
            return;
        }
        match event {
            E::Expected(identity) if selected.phase == AliasPhase::Selected => {
                selected.phase = AliasPhase::Expected;
                self.emit(P::Alias {
                    verified: false,
                    slot,
                    step_seq: selected.step_seq,
                    dev: identity.dev(),
                    ino: identity.ino(),
                });
            }
            E::Verified(identity) if selected.phase == AliasPhase::Expected => {
                selected.phase = AliasPhase::Verified;
                self.emit(P::Alias {
                    verified: true,
                    slot,
                    step_seq: selected.step_seq,
                    dev: identity.dev(),
                    ino: identity.ino(),
                });
            }
            E::PreparationFailed { phase, errno } if selected.phase == AliasPhase::Expected => {
                self.set_fault_state(slot, selected.fault_index, FaultSlot::PreparationFailed);
                selected.phase = AliasPhase::Returned;
                self.fail(
                    match phase {
                        AliasPreparation::Open => F::Open,
                        AliasPreparation::Metadata => F::Metadata,
                        AliasPreparation::Identity => F::Identity,
                    },
                    Some(slot),
                    errno,
                );
            }
            E::Call if selected.phase == AliasPhase::Verified => {
                selected.phase = AliasPhase::Called;
                self.emit(P::DirSyncCall {
                    slot,
                    step_seq: selected.step_seq,
                });
            }
            E::Returned { success, errno } if selected.phase == AliasPhase::Called => {
                let bad_fd = !success && errno == Some(libc::EBADF);
                self.set_fault_state(slot, selected.fault_index, FaultSlot::Executed { bad_fd });
                selected.phase = AliasPhase::Returned;
                self.emit(P::DirSyncReturned {
                    slot,
                    step_seq: selected.step_seq,
                    success,
                    errno,
                });
                if !bad_fd {
                    self.fail(F::UnexpectedResult, Some(slot), errno);
                }
            }
            _ => self.fail(F::Protocol, Some(slot), None),
        }
        scope.selected = Some(selected);
        self.scope = Some(scope);
    }
}
