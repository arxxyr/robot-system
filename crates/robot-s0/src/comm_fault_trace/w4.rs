//! W4a 固定事件编解码；复用原环字段，完整保留事务身份，拒绝未知标志位。

use super::{TraceContext, TraceEvent, TraceKind};
use crate::clear_estop_transaction::{ClearTxnStage, StepPhase};
use crate::latch_writer::ClearTxnIdentity;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    Prepare,
    RepairLatched,
    RepairOriginal,
    RemoveCommitted,
    RemoveRepaired,
    Resync,
    RemoveRepairedOriginal,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    None,
    NoMarker,
    Prepared,
    Created,
    Durable,
    Repaired,
    Unsynced,
    UnsyncedBeforeRepairLatched,
    UnsyncedBeforeRepairOriginal,
    Lost,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Prepared,
    PrepareNotOwned,
    PrepareCreated,
    PrepareDurable,
    RepairedLatched,
    RepairedOriginal,
    RepairFailedKept,
    RepairFailedLost,
    RemovedCommitted,
    RemovedRepaired,
    RemoveFailedRetained,
    RemoveFailedUnsynced,
    RemoveFailedLost,
    Resynced,
    RefusedIdentity,
    RefusedObligation,
    RefusedStopping,
    RefusedNoTransaction,
    RefusedPair,
    RefusedToken,
    RemovedRepairedOriginal,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureFailure {
    Protocol,
    Open,
    Metadata,
    Identity,
    UnexpectedResult,
    CounterOverflow,
}

/// 仅事件负载；真实 sample/source/时间戳始终由原 `TraceEvent` 携带。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum W4Payload {
    CommandBegin {
        txn: ClearTxnIdentity,
        command: CommandKind,
    },
    Token {
        after: bool,
        token: TokenKind,
    },
    Completion {
        kind: CompletionKind,
        failure: Option<(ClearTxnStage, StepPhase)>,
    },
    CommandEnd {
        completed: bool,
    },
    Stage {
        succeeded: bool,
        step_seq: u64,
        stage: ClearTxnStage,
        occurrence: u64,
    },
    FaultSelected {
        slot: u8,
        step_seq: u64,
        stage: ClearTxnStage,
        occurrence: u64,
    },
    Alias {
        verified: bool,
        slot: u8,
        step_seq: u64,
        dev: u64,
        ino: u64,
    },
    DirSyncCall {
        slot: u8,
        step_seq: u64,
    },
    DirSyncReturned {
        slot: u8,
        step_seq: u64,
        success: bool,
        errno: Option<i32>,
    },
    Failure {
        kind: FixtureFailure,
        slot: Option<u8>,
        step_seq: u64,
        errno: Option<i32>,
    },
    KillReached {
        step_seq: u64,
        stage: ClearTxnStage,
        entered_at: u64,
        pause_until: u64,
    },
}

fn errno_word(errno: Option<i32>) -> u64 {
    errno.map_or(0, |value| i64::from(value).cast_unsigned())
}
fn decode_errno(present: bool, word: u64) -> Result<Option<i32>, &'static str> {
    match (present, word) {
        (false, 0) => Ok(None),
        (false, _) => Err("W4 无 errno 标志却携带非零值"),
        (true, value) => i32::try_from(value.cast_signed())
            .map(Some)
            .map_err(|_| "W4 errno 越界"),
    }
}
fn slot_code(slot: u8) -> Result<u32, &'static str> {
    match slot {
        0 => Ok(1),
        1 => Ok(2),
        _ => Err("W4 编码 slot 越界"),
    }
}
fn decode_slot(code: u32) -> Result<u8, &'static str> {
    match code {
        1 => Ok(0),
        2 => Ok(1),
        _ => Err("W4 未知 slot"),
    }
}

/// 从原 writer 调用点编码，不缓存、不替代真实完成。
pub(crate) fn emit(source: u64, payload: W4Payload) {
    let (kind, normal_seq, generation, a, b) = encode(payload).unwrap_or((
        TraceKind::W4FixtureFailure,
        0,
        FixtureFailure::Protocol.code(),
        0,
        0,
    ));
    super::emit(
        kind,
        super::now(),
        TraceContext {
            source,
            normal_seq,
            generation,
        },
        a,
        b,
    );
}

#[allow(
    clippy::too_many_lines,
    reason = "固定事件按单一穷举表编码，字段与解码表可逐项对照"
)]
fn encode(payload: W4Payload) -> Result<(TraceKind, u64, u32, u64, u64), &'static str> {
    use TraceKind as T;
    Ok(match payload {
        W4Payload::CommandBegin { txn, command } => {
            let bytes = txn.txn_id.to_le_bytes();
            let low = u64::from_le_bytes(bytes[..8].try_into().expect("固定低八字节"));
            let high = u64::from_le_bytes(bytes[8..].try_into().expect("固定高八字节"));
            (
                T::W4CommandBegin,
                txn.expected_epoch,
                command.code() | (u32::from(txn.original_latched) << 8),
                low,
                high,
            )
        }
        W4Payload::Token { after, token } => (
            if after {
                T::W4TokenAfter
            } else {
                T::W4TokenBefore
            },
            0,
            token.code(),
            0,
            0,
        ),
        W4Payload::Completion { kind, failure } => {
            let flags = match failure {
                None => 0,
                Some((stage, phase)) => stage_code(stage) << 8 | phase_code(phase) << 16,
            };
            (T::W4Completion, 0, kind.code() | flags, 0, 0)
        }
        W4Payload::CommandEnd { completed } => (T::W4CommandEnd, 0, u32::from(completed), 0, 0),
        W4Payload::Stage {
            succeeded,
            step_seq,
            stage,
            occurrence,
        } => (
            if succeeded {
                T::W4OperationSucceeded
            } else {
                T::W4BeforeEntered
            },
            step_seq,
            stage_code(stage),
            occurrence,
            0,
        ),
        W4Payload::FaultSelected {
            slot,
            step_seq,
            stage,
            occurrence,
        } => (
            T::W4FaultSelected,
            step_seq,
            slot_code(slot)? | stage_code(stage) << 8,
            occurrence,
            0,
        ),
        W4Payload::Alias {
            verified,
            slot,
            step_seq,
            dev,
            ino,
        } => (
            if verified {
                T::W4AliasVerified
            } else {
                T::W4AliasExpected
            },
            step_seq,
            slot_code(slot)?,
            dev,
            ino,
        ),
        W4Payload::DirSyncCall { slot, step_seq } => {
            (T::W4DirSyncCall, step_seq, slot_code(slot)?, 0, 0)
        }
        W4Payload::DirSyncReturned {
            slot,
            step_seq,
            success,
            errno,
        } => (
            T::W4DirSyncReturned,
            step_seq,
            slot_code(slot)? | u32::from(errno.is_some()) << 8 | u32::from(success) << 9,
            errno_word(errno),
            0,
        ),
        W4Payload::Failure {
            kind,
            slot,
            step_seq,
            errno,
        } => (
            T::W4FixtureFailure,
            step_seq,
            kind.code()
                | slot.map(slot_code).transpose()?.unwrap_or(0) << 8
                | u32::from(errno.is_some()) << 16,
            errno_word(errno),
            0,
        ),
        W4Payload::KillReached {
            step_seq,
            stage,
            entered_at,
            pause_until,
        } => {
            check_kill_fields(step_seq, stage, entered_at, pause_until)?;
            (
                T::W4KillReached,
                step_seq,
                stage_code(stage),
                entered_at,
                pause_until,
            )
        }
    })
}

/// 非 W4 事件返回 None；W4 保留位、枚举值或字段组合未知时明确拒绝。
///
/// # Errors
/// W4 事件含未知编码、非零保留字段或越界 errno。
#[allow(
    clippy::too_many_lines,
    reason = "同一固定协议逐事件校验全部保留位与字段，避免分散校验遗漏"
)]
pub fn decode(event: &TraceEvent) -> Result<Option<W4Payload>, &'static str> {
    use TraceKind as T;
    let g = event.generation;
    let step = event.normal_seq;
    let (a, b) = (event.a, event.b);
    let payload = match event.kind {
        T::W4CommandBegin => {
            if g & !0x1ff != 0 {
                return Err("W4 command 保留位非零");
            }
            W4Payload::CommandBegin {
                txn: ClearTxnIdentity {
                    txn_id: u128::from(a) | u128::from(b) << 64,
                    expected_epoch: step,
                    original_latched: g & 0x100 != 0,
                },
                command: CommandKind::decode(g & 0xff)?,
            }
        }
        T::W4TokenBefore | T::W4TokenAfter => {
            zero([step, a, b])?;
            W4Payload::Token {
                after: event.kind == T::W4TokenAfter,
                token: TokenKind::decode(g)?,
            }
        }
        T::W4Completion => {
            zero([step, a, b])?;
            if g & 0xff00_0000 != 0 {
                return Err("W4 completion 保留位非零");
            }
            let failure = match ((g >> 8) & 0xff, (g >> 16) & 0xff) {
                (0, 0) => None,
                (stage, phase) => Some((decode_stage(stage)?, decode_phase(phase)?)),
            };
            let kind = CompletionKind::decode(g & 0xff)?;
            if kind.failed() != failure.is_some() {
                return Err("W4 completion 与失败步骤不符");
            }
            W4Payload::Completion { kind, failure }
        }
        T::W4CommandEnd => {
            zero([step, a, b])?;
            W4Payload::CommandEnd {
                completed: match g {
                    0 => false,
                    1 => true,
                    _ => return Err("W4 end 未知值"),
                },
            }
        }
        T::W4BeforeEntered | T::W4OperationSucceeded => {
            zero([b])?;
            W4Payload::Stage {
                succeeded: event.kind == T::W4OperationSucceeded,
                step_seq: step,
                stage: decode_stage(g)?,
                occurrence: a,
            }
        }
        T::W4FaultSelected => {
            zero([b])?;
            if g & !0xffff != 0 {
                return Err("W4 selected 保留位非零");
            }
            W4Payload::FaultSelected {
                slot: decode_slot(g & 0xff)?,
                step_seq: step,
                stage: decode_stage(g >> 8)?,
                occurrence: a,
            }
        }
        T::W4AliasExpected | T::W4AliasVerified => W4Payload::Alias {
            verified: event.kind == T::W4AliasVerified,
            slot: decode_slot(g)?,
            step_seq: step,
            dev: a,
            ino: b,
        },
        T::W4DirSyncCall => {
            zero([a, b])?;
            W4Payload::DirSyncCall {
                slot: decode_slot(g)?,
                step_seq: step,
            }
        }
        T::W4DirSyncReturned => {
            zero([b])?;
            if g & !0x3ff != 0 {
                return Err("W4 returned 保留位非零");
            }
            let errno = decode_errno(g & 0x100 != 0, a)?;
            let success = g & 0x200 != 0;
            if success && errno.is_some() {
                return Err("W4 成功结果携带错误码");
            }
            W4Payload::DirSyncReturned {
                slot: decode_slot(g & 0xff)?,
                step_seq: step,
                success,
                errno,
            }
        }
        T::W4FixtureFailure => {
            zero([b])?;
            if g & !0x1ffff != 0 {
                return Err("W4 fixture 保留位非零");
            }
            W4Payload::Failure {
                kind: FixtureFailure::decode(g & 0xff)?,
                slot: match (g >> 8) & 0xff {
                    0 => None,
                    code => Some(decode_slot(code)?),
                },
                step_seq: step,
                errno: decode_errno(g & 0x10000 != 0, a)?,
            }
        }
        T::W4KillReached => {
            let stage = decode_stage(g)?;
            check_kill_fields(step, stage, a, b)?;
            if event.sample_seq == 0
                || event.source == 0
                || event.stamp_ns < a
                || event.stamp_ns >= b
                || b - event.stamp_ns > 250_000_000
            {
                return Err("W4 SIGKILL 命中来源或真实发布时刻无效");
            }
            W4Payload::KillReached {
                step_seq: step,
                stage,
                entered_at: a,
                pause_until: b,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(payload))
}

fn check_kill_fields(
    step: u64,
    stage: ClearTxnStage,
    entered_at: u64,
    pause_until: u64,
) -> Result<(), &'static str> {
    match stage {
        ClearTxnStage::ReadMarker
        | ClearTxnStage::ValidateMarker
        | ClearTxnStage::CreateMarker
        | ClearTxnStage::SampleMarkerIdentity
        | ClearTxnStage::WriteMarker
        | ClearTxnStage::SyncMarkerFile
        | ClearTxnStage::SyncMarkerDirectory
        | ClearTxnStage::CreateBackup
        | ClearTxnStage::WriteBackup
        | ClearTxnStage::SyncBackupFile
        | ClearTxnStage::RenameBackup
        | ClearTxnStage::SyncBackupDirectory
        | ClearTxnStage::CreatePrimary
        | ClearTxnStage::WritePrimary
        | ClearTxnStage::SyncPrimaryFile
        | ClearTxnStage::RenamePrimary
        | ClearTxnStage::SyncPrimaryDirectory
        | ClearTxnStage::RemoveMarker
        | ClearTxnStage::SyncRemovalDirectory => {}
        _ => return Err("W4 SIGKILL 仅接受固定 Prepare、Repair 与 Finalize 文件阶段"),
    }
    let deadline = entered_at
        .checked_add(2_000_000_000)
        .ok_or("W4 SIGKILL 原步骤截止溢出")?;
    if step == 0 || entered_at == 0 || pause_until <= entered_at || pause_until >= deadline {
        return Err("W4 SIGKILL 步骤或固定窗口无效");
    }
    Ok(())
}

fn zero<const N: usize>(values: [u64; N]) -> Result<(), &'static str> {
    if values == [0; N] {
        Ok(())
    } else {
        Err("W4 保留字段非零")
    }
}

fn phase_code(value: StepPhase) -> u32 {
    match value {
        StepPhase::Before => 1,
        StepPhase::Operation => 2,
        StepPhase::After => 3,
    }
}
fn decode_phase(value: u32) -> Result<StepPhase, &'static str> {
    match value {
        1 => Ok(StepPhase::Before),
        2 => Ok(StepPhase::Operation),
        3 => Ok(StepPhase::After),
        _ => Err("W4 未知步骤相位"),
    }
}

impl CompletionKind {
    const fn failed(self) -> bool {
        matches!(
            self,
            Self::PrepareNotOwned
                | Self::PrepareCreated
                | Self::PrepareDurable
                | Self::RepairFailedKept
                | Self::RepairFailedLost
                | Self::RemoveFailedRetained
                | Self::RemoveFailedUnsynced
                | Self::RemoveFailedLost
        )
    }
}

impl CommandKind {
    const fn code(self) -> u32 {
        match self {
            Self::Prepare => 1,
            Self::RepairLatched => 2,
            Self::RepairOriginal => 3,
            Self::RemoveCommitted => 4,
            Self::RemoveRepaired => 5,
            Self::Resync => 6,
            Self::RemoveRepairedOriginal => 7,
        }
    }
    fn decode(value: u32) -> Result<Self, &'static str> {
        match value {
            1 => Ok(Self::Prepare),
            2 => Ok(Self::RepairLatched),
            3 => Ok(Self::RepairOriginal),
            4 => Ok(Self::RemoveCommitted),
            5 => Ok(Self::RemoveRepaired),
            6 => Ok(Self::Resync),
            7 => Ok(Self::RemoveRepairedOriginal),
            _ => Err("W4 未知 CommandKind"),
        }
    }
}

impl TokenKind {
    const fn code(self) -> u32 {
        match self {
            Self::None => 0,
            Self::NoMarker => 1,
            Self::Prepared => 2,
            Self::Created => 3,
            Self::Durable => 4,
            Self::Repaired => 5,
            Self::Unsynced => 6,
            Self::UnsyncedBeforeRepairLatched => 7,
            Self::UnsyncedBeforeRepairOriginal => 8,
            Self::Lost => 9,
        }
    }
    fn decode(value: u32) -> Result<Self, &'static str> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::NoMarker),
            2 => Ok(Self::Prepared),
            3 => Ok(Self::Created),
            4 => Ok(Self::Durable),
            5 => Ok(Self::Repaired),
            6 => Ok(Self::Unsynced),
            7 => Ok(Self::UnsyncedBeforeRepairLatched),
            8 => Ok(Self::UnsyncedBeforeRepairOriginal),
            9 => Ok(Self::Lost),
            _ => Err("W4 未知 TokenKind"),
        }
    }
}

impl CompletionKind {
    const fn code(self) -> u32 {
        match self {
            Self::Prepared => 1,
            Self::PrepareNotOwned => 2,
            Self::PrepareCreated => 3,
            Self::PrepareDurable => 4,
            Self::RepairedLatched => 5,
            Self::RepairedOriginal => 6,
            Self::RepairFailedKept => 7,
            Self::RepairFailedLost => 8,
            Self::RemovedCommitted => 9,
            Self::RemovedRepaired => 10,
            Self::RemoveFailedRetained => 11,
            Self::RemoveFailedUnsynced => 12,
            Self::RemoveFailedLost => 13,
            Self::Resynced => 14,
            Self::RefusedIdentity => 15,
            Self::RefusedObligation => 16,
            Self::RefusedStopping => 17,
            Self::RefusedNoTransaction => 18,
            Self::RefusedPair => 19,
            Self::RefusedToken => 20,
            Self::RemovedRepairedOriginal => 21,
        }
    }
    fn decode(value: u32) -> Result<Self, &'static str> {
        match value {
            1 => Ok(Self::Prepared),
            2 => Ok(Self::PrepareNotOwned),
            3 => Ok(Self::PrepareCreated),
            4 => Ok(Self::PrepareDurable),
            5 => Ok(Self::RepairedLatched),
            6 => Ok(Self::RepairedOriginal),
            7 => Ok(Self::RepairFailedKept),
            8 => Ok(Self::RepairFailedLost),
            9 => Ok(Self::RemovedCommitted),
            10 => Ok(Self::RemovedRepaired),
            11 => Ok(Self::RemoveFailedRetained),
            12 => Ok(Self::RemoveFailedUnsynced),
            13 => Ok(Self::RemoveFailedLost),
            14 => Ok(Self::Resynced),
            15 => Ok(Self::RefusedIdentity),
            16 => Ok(Self::RefusedObligation),
            17 => Ok(Self::RefusedStopping),
            18 => Ok(Self::RefusedNoTransaction),
            19 => Ok(Self::RefusedPair),
            20 => Ok(Self::RefusedToken),
            21 => Ok(Self::RemovedRepairedOriginal),
            _ => Err("W4 未知 CompletionKind"),
        }
    }
}

impl FixtureFailure {
    const fn code(self) -> u32 {
        match self {
            Self::Protocol => 1,
            Self::Open => 2,
            Self::Metadata => 3,
            Self::Identity => 4,
            Self::UnexpectedResult => 5,
            Self::CounterOverflow => 6,
        }
    }
    fn decode(value: u32) -> Result<Self, &'static str> {
        match value {
            1 => Ok(Self::Protocol),
            2 => Ok(Self::Open),
            3 => Ok(Self::Metadata),
            4 => Ok(Self::Identity),
            5 => Ok(Self::UnexpectedResult),
            6 => Ok(Self::CounterOverflow),
            _ => Err("W4 未知 FixtureFailure"),
        }
    }
}

fn stage_code(stage: ClearTxnStage) -> u32 {
    match stage {
        ClearTxnStage::OpenStore => 1,
        ClearTxnStage::ValidateCredential => 2,
        ClearTxnStage::ReadMarker => 3,
        ClearTxnStage::ValidateMarker => 4,
        ClearTxnStage::CreateMarker => 5,
        ClearTxnStage::SampleMarkerIdentity => 6,
        ClearTxnStage::WriteMarker => 7,
        ClearTxnStage::SyncMarkerFile => 8,
        ClearTxnStage::SyncMarkerDirectory => 9,
        ClearTxnStage::CreateBackup => 10,
        ClearTxnStage::WriteBackup => 11,
        ClearTxnStage::SyncBackupFile => 12,
        ClearTxnStage::RenameBackup => 13,
        ClearTxnStage::SyncBackupDirectory => 14,
        ClearTxnStage::CreatePrimary => 15,
        ClearTxnStage::WritePrimary => 16,
        ClearTxnStage::SyncPrimaryFile => 17,
        ClearTxnStage::RenamePrimary => 18,
        ClearTxnStage::SyncPrimaryDirectory => 19,
        ClearTxnStage::RemoveMarker => 20,
        ClearTxnStage::SyncRemovalDirectory => 21,
        ClearTxnStage::LoadLatch => 22,
        ClearTxnStage::InspectPair => 23,
    }
}
fn decode_stage(value: u32) -> Result<ClearTxnStage, &'static str> {
    match value {
        1 => Ok(ClearTxnStage::OpenStore),
        2 => Ok(ClearTxnStage::ValidateCredential),
        3 => Ok(ClearTxnStage::ReadMarker),
        4 => Ok(ClearTxnStage::ValidateMarker),
        5 => Ok(ClearTxnStage::CreateMarker),
        6 => Ok(ClearTxnStage::SampleMarkerIdentity),
        7 => Ok(ClearTxnStage::WriteMarker),
        8 => Ok(ClearTxnStage::SyncMarkerFile),
        9 => Ok(ClearTxnStage::SyncMarkerDirectory),
        10 => Ok(ClearTxnStage::CreateBackup),
        11 => Ok(ClearTxnStage::WriteBackup),
        12 => Ok(ClearTxnStage::SyncBackupFile),
        13 => Ok(ClearTxnStage::RenameBackup),
        14 => Ok(ClearTxnStage::SyncBackupDirectory),
        15 => Ok(ClearTxnStage::CreatePrimary),
        16 => Ok(ClearTxnStage::WritePrimary),
        17 => Ok(ClearTxnStage::SyncPrimaryFile),
        18 => Ok(ClearTxnStage::RenamePrimary),
        19 => Ok(ClearTxnStage::SyncPrimaryDirectory),
        20 => Ok(ClearTxnStage::RemoveMarker),
        21 => Ok(ClearTxnStage::SyncRemovalDirectory),
        22 => Ok(ClearTxnStage::LoadLatch),
        23 => Ok(ClearTxnStage::InspectPair),
        _ => Err("W4 未知文件阶段"),
    }
}

#[cfg(test)]
mod tests;
