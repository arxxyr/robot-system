//! R4b3 固定25点表与真实前缀判据；表只给验收标准，不生成运行事件。

#[cfg(test)]
mod tests;

use robot_s0::clear_estop_transaction::{ClearTxnStage as S, StepPhase};
use robot_s0::comm_fault_trace::w4::{
    CommandKind as C, CompletionKind as K, TokenKind as T, W4Payload as P,
};
use robot_s0::latch_writer::ClearTxnIdentity;
use serde_json::{Value, json};

use super::{FINALIZE, PREPARE, Prefix};

pub(super) const FILTER: &str = "R4-T8-WRITER-KILL-25";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Family {
    Prepare,
    FinalizeBefore,
    Repair,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Temporary {
    PrimaryEmpty,
    PrimaryZero,
    PrimaryOne,
    BackupEmpty,
    BackupZero,
    BackupOne,
}

impl Temporary {
    pub(super) const fn target(self) -> &'static str {
        match self {
            Self::PrimaryEmpty | Self::PrimaryZero | Self::PrimaryOne => "estop_latch",
            Self::BackupEmpty | Self::BackupZero | Self::BackupOne => "estop_latch.bak",
        }
    }
    pub(super) const fn bytes(self) -> &'static [u8] {
        match self {
            Self::PrimaryEmpty | Self::BackupEmpty => b"",
            Self::PrimaryZero | Self::BackupZero => b"0\n",
            Self::PrimaryOne | Self::BackupOne => b"1\n",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Spec {
    pub(super) label: &'static str,
    pub(super) family: Family,
    pub(super) stage: S,
    pub(super) pair: (u8, u8),
    pub(super) temporary: Option<Temporary>,
}

const fn prepare(
    label: &'static str,
    stage: S,
    pair: (u8, u8),
    temporary: Option<Temporary>,
) -> Spec {
    Spec {
        label,
        family: Family::Prepare,
        stage,
        pair,
        temporary,
    }
}
const fn repair(
    label: &'static str,
    stage: S,
    pair: (u8, u8),
    temporary: Option<Temporary>,
) -> Spec {
    Spec {
        label,
        family: Family::Repair,
        stage,
        pair,
        temporary,
    }
}

pub(super) const POINTS: [Spec; 25] = [
    prepare(
        "prepare-sample-marker-identity",
        S::SampleMarkerIdentity,
        (1, 1),
        None,
    ),
    prepare("prepare-write-marker", S::WriteMarker, (1, 1), None),
    prepare("prepare-sync-marker-file", S::SyncMarkerFile, (1, 1), None),
    prepare(
        "prepare-sync-marker-directory",
        S::SyncMarkerDirectory,
        (1, 1),
        None,
    ),
    prepare(
        "prepare-create-backup",
        S::CreateBackup,
        (1, 1),
        Some(Temporary::BackupEmpty),
    ),
    prepare(
        "prepare-write-backup",
        S::WriteBackup,
        (1, 1),
        Some(Temporary::BackupZero),
    ),
    prepare(
        "prepare-sync-backup-file",
        S::SyncBackupFile,
        (1, 1),
        Some(Temporary::BackupZero),
    ),
    prepare(
        "prepare-sync-backup-directory",
        S::SyncBackupDirectory,
        (1, 0),
        None,
    ),
    prepare(
        "prepare-create-primary",
        S::CreatePrimary,
        (1, 0),
        Some(Temporary::PrimaryEmpty),
    ),
    prepare(
        "prepare-write-primary",
        S::WritePrimary,
        (1, 0),
        Some(Temporary::PrimaryZero),
    ),
    prepare(
        "prepare-sync-primary-file",
        S::SyncPrimaryFile,
        (1, 0),
        Some(Temporary::PrimaryZero),
    ),
    prepare("prepare-rename-primary", S::RenamePrimary, (0, 0), None),
    Spec {
        label: "finalize-before-unlink",
        family: Family::FinalizeBefore,
        stage: S::RemoveMarker,
        pair: (0, 0),
        temporary: None,
    },
    repair("repair-after-read_marker", S::ReadMarker, (0, 0), None),
    repair(
        "repair-after-validate_marker",
        S::ValidateMarker,
        (0, 0),
        None,
    ),
    repair(
        "repair-after-create_primary",
        S::CreatePrimary,
        (0, 0),
        Some(Temporary::PrimaryEmpty),
    ),
    repair(
        "repair-after-write_primary",
        S::WritePrimary,
        (0, 0),
        Some(Temporary::PrimaryOne),
    ),
    repair(
        "repair-after-sync_primary_file",
        S::SyncPrimaryFile,
        (0, 0),
        Some(Temporary::PrimaryOne),
    ),
    repair(
        "repair-after-rename_primary",
        S::RenamePrimary,
        (1, 0),
        None,
    ),
    repair(
        "repair-after-sync_primary_directory",
        S::SyncPrimaryDirectory,
        (1, 0),
        None,
    ),
    repair(
        "repair-after-create_backup",
        S::CreateBackup,
        (1, 0),
        Some(Temporary::BackupEmpty),
    ),
    repair(
        "repair-after-write_backup",
        S::WriteBackup,
        (1, 0),
        Some(Temporary::BackupOne),
    ),
    repair(
        "repair-after-sync_backup_file",
        S::SyncBackupFile,
        (1, 0),
        Some(Temporary::BackupOne),
    ),
    repair("repair-after-rename_backup", S::RenameBackup, (1, 1), None),
    repair(
        "repair-after-sync_backup_directory",
        S::SyncBackupDirectory,
        (1, 1),
        None,
    ),
];

pub(super) const REPAIR: [S; 12] = [
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

pub(super) struct PrefixEnd {
    pub(super) source: u64,
    pub(super) count: usize,
    pub(super) predecessor: Option<Value>,
}

pub(super) fn parse_prefix(
    cursor: &mut Prefix<'_>,
    spec: Spec,
    txn: ClearTxnIdentity,
) -> Result<PrefixEnd, String> {
    match spec.family {
        Family::Prepare => {
            let count = PREPARE
                .iter()
                .position(|&stage| stage == spec.stage)
                .ok_or("未知Prepare固定点")?
                + 1;
            cursor.steps(1, &PREPARE[..count], spec.stage)?;
            Ok(PrefixEnd {
                source: 1,
                count,
                predecessor: None,
            })
        }
        Family::FinalizeBefore => {
            cursor.steps(1, &PREPARE, S::RemoveMarker)?;
            complete(cursor, K::Prepared, T::Prepared, None)?;
            let _ = cursor.begin(2, C::RemoveCommitted, T::Prepared, Some(txn))?;
            cursor.steps(2, &FINALIZE[..2], S::RemoveMarker)?;
            cursor.expect(
                2,
                P::Stage {
                    succeeded: false,
                    step_seq: 3,
                    stage: S::RemoveMarker,
                    occurrence: 1,
                },
            )?;
            Ok(PrefixEnd {
                source: 2,
                count: 3,
                predecessor: None,
            })
        }
        Family::Repair => {
            cursor.steps(1, &PREPARE[..14], S::SyncPrimaryDirectory)?;
            let predecessor = failing_prepare(cursor)?;
            complete(
                cursor,
                K::PrepareDurable,
                T::Durable,
                Some((S::SyncPrimaryDirectory, StepPhase::Operation)),
            )?;
            let _ = cursor.begin(2, C::RepairLatched, T::Durable, Some(txn))?;
            let count = REPAIR
                .iter()
                .position(|&stage| stage == spec.stage)
                .ok_or("未知Repair固定点")?
                + 1;
            cursor.steps(2, &REPAIR[..count], spec.stage)?;
            Ok(PrefixEnd {
                source: 2,
                count,
                predecessor: Some(predecessor),
            })
        }
    }
}

fn complete(
    cursor: &mut Prefix<'_>,
    kind: K,
    token: T,
    failure: Option<(S, StepPhase)>,
) -> Result<(), String> {
    cursor.expect(1, P::Completion { kind, failure })?;
    cursor.expect(1, P::Token { after: true, token })?;
    cursor.expect(1, P::CommandEnd { completed: true })
}

fn failing_prepare(cursor: &mut Prefix<'_>) -> Result<Value, String> {
    cursor.expect(
        1,
        P::Stage {
            succeeded: false,
            step_seq: 15,
            stage: S::SyncPrimaryDirectory,
            occurrence: 1,
        },
    )?;
    cursor.expect(
        1,
        P::FaultSelected {
            slot: 0,
            step_seq: 15,
            stage: S::SyncPrimaryDirectory,
            occurrence: 1,
        },
    )?;
    let (_, payload) = cursor.next(1)?;
    let P::Alias {
        verified: false,
        slot: 0,
        step_seq: 15,
        dev,
        ino,
    } = payload
    else {
        return Err("原末同步缺Expected目录身份".into());
    };
    if ino == 0 {
        return Err("原目录inode为0".into());
    }
    cursor.expect(
        1,
        P::Alias {
            verified: true,
            slot: 0,
            step_seq: 15,
            dev,
            ino,
        },
    )?;
    cursor.expect(
        1,
        P::DirSyncCall {
            slot: 0,
            step_seq: 15,
        },
    )?;
    cursor.expect(
        1,
        P::DirSyncReturned {
            slot: 0,
            step_seq: 15,
            success: false,
            errno: Some(libc::EBADF),
        },
    )?;
    Ok(
        json!({"source":1,"step_seq":15,"slot":0,"directory_dev":dev,"directory_ino":ino,
        "actual_errno":libc::EBADF,"completion":"PrepareDurable","phase":"Operation"}),
    )
}
