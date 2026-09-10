//! W4a 两项固定故障输入；只观察原命令、凭证和 syscall，不参与许可或退休。

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
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ROBOT_TEST_W4_FILE_FAULT 仅接受 created / created-unsynced",
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
}
struct FileFaultHook {
    slots: [Option<FaultSlot>; 2],
    binding: Option<ClearTxnIdentity>,
    execute_sequence: u64,
    prepare_sequence: u64,
    repair_sequence: u64,
    scope: Option<Scope>,
    created_proven: bool,
    failed: bool,
}
impl FileFaultHook {
    const fn new(mode: FileFaultMode) -> Self {
        Self {
            slots: [
                Some(FaultSlot::Armed),
                match mode {
                    FileFaultMode::Created => None,
                    FileFaultMode::CreatedUnsynced => Some(FaultSlot::Armed),
                },
            ],
            binding: None,
            execute_sequence: 0,
            prepare_sequence: 0,
            repair_sequence: 0,
            scope: None,
            created_proven: false,
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
}

fn target_index(stage: S) -> Option<usize> {
    match stage {
        S::SyncMarkerDirectory => Some(0),
        S::SyncRemovalDirectory => Some(1),
        _ => None,
    }
}
fn command_kind(command: PersistCommand) -> C {
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
fn token_kind(token: Option<&WriterToken>) -> T {
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
fn completion_payload(completion: &PersistCompletion) -> P {
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
        if matches!(kind, C::RepairLatched | C::RepairOriginal)
            && kind_sequence == 1
            && kind != C::RepairLatched
        {
            self.fail(F::Protocol, None, None);
        }
    }

    fn command_completed(&mut self, completion: &PersistCompletion, token: Option<&WriterToken>) {
        let Some(scope) = self.scope else {
            self.fail(F::Protocol, None, None);
            return;
        };
        let payload = completion_payload(completion);
        self.emit(payload);
        self.emit(P::Token {
            after: true,
            token: token_kind(token),
        });
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
        if !completed {
            self.fail(F::Protocol, None, None);
        }
        let expected = match (scope.command, scope.kind_sequence) {
            (C::Prepare, 1) => Some(0),
            (C::RepairLatched, 1) if self.slots[1].is_some() => Some(1),
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
        scope.step_seq = next;
        scope.stage = Some(stage);
        scope.selected = None;
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
        let Some(scope) = self.scope else { return };
        if scope.stage != Some(stage) {
            self.fail(F::Protocol, None, None);
            return;
        }
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
        let slot = match (
            scope.command,
            scope.kind_sequence,
            stage,
            Self::occurrence(scope, stage),
        ) {
            (C::Prepare, 1, S::SyncMarkerDirectory, 1) => 0,
            (C::RepairLatched, 1, S::SyncRemovalDirectory, 1) if self.created_proven => 1,
            _ => return DirectorySyncInput::Original,
        };
        match self.slots[usize::from(slot)] {
            None => return DirectorySyncInput::Original,
            Some(FaultSlot::Armed) => {}
            Some(_) => {
                self.fail(F::Protocol, Some(slot), None);
                return DirectorySyncInput::Original;
            }
        }
        self.slots[usize::from(slot)] = Some(FaultSlot::Selected);
        scope.selected = Some(Selected {
            slot,
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
                self.slots[usize::from(slot)] = Some(FaultSlot::PreparationFailed);
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
                self.slots[usize::from(slot)] = Some(FaultSlot::Executed { bad_fd });
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
