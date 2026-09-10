use super::*;

fn event(payload: W4Payload) -> TraceEvent {
    let (kind, normal_seq, generation, a, b) = encode(payload).unwrap();
    TraceEvent {
        sample_seq: 2,
        stamp_ns: 3,
        kind,
        source: 4,
        normal_seq,
        generation,
        a,
        b,
    }
}

#[test]
fn full_transaction_and_all_fixed_payload_families_round_trip_without_truncation() {
    let txn = ClearTxnIdentity {
        txn_id: u128::MAX - 7,
        expected_epoch: u64::MAX - 3,
        original_latched: true,
    };
    for payload in [
        W4Payload::CommandBegin {
            txn,
            command: CommandKind::Prepare,
        },
        W4Payload::CommandBegin {
            txn,
            command: CommandKind::RemoveRepairedOriginal,
        },
        W4Payload::Token {
            after: false,
            token: TokenKind::UnsyncedBeforeRepairLatched,
        },
        W4Payload::Token {
            after: true,
            token: TokenKind::None,
        },
        W4Payload::Completion {
            kind: CompletionKind::PrepareCreated,
            failure: Some((ClearTxnStage::SyncMarkerDirectory, StepPhase::Operation)),
        },
        W4Payload::Completion {
            kind: CompletionKind::RemovedRepairedOriginal,
            failure: None,
        },
        W4Payload::CommandEnd { completed: true },
        W4Payload::Stage {
            succeeded: false,
            step_seq: u64::MAX,
            stage: ClearTxnStage::RemoveMarker,
            occurrence: 0,
        },
        W4Payload::Stage {
            succeeded: true,
            step_seq: 5,
            stage: ClearTxnStage::SyncRemovalDirectory,
            occurrence: 1,
        },
        W4Payload::FaultSelected {
            slot: 1,
            step_seq: 5,
            stage: ClearTxnStage::SyncRemovalDirectory,
            occurrence: 1,
        },
        W4Payload::Alias {
            verified: false,
            slot: 1,
            step_seq: 5,
            dev: u64::MAX,
            ino: u64::MAX - 1,
        },
        W4Payload::Alias {
            verified: true,
            slot: 1,
            step_seq: 5,
            dev: u64::MAX,
            ino: u64::MAX - 1,
        },
        W4Payload::DirSyncCall {
            slot: 0,
            step_seq: 5,
        },
        W4Payload::DirSyncReturned {
            slot: 0,
            step_seq: 5,
            success: false,
            errno: Some(libc::EBADF),
        },
        W4Payload::DirSyncReturned {
            slot: 0,
            step_seq: 5,
            success: true,
            errno: None,
        },
        W4Payload::Failure {
            kind: FixtureFailure::Open,
            slot: Some(0),
            step_seq: 5,
            errno: Some(i32::MIN),
        },
    ] {
        assert_eq!(decode(&event(payload)).unwrap(), Some(payload));
    }
    for value in 1..=18 {
        assert_eq!(TraceKind::decode(value).unwrap() as u64, value);
    }
}

#[test]
fn invalid_slot_255_cannot_carry_into_errno_flag() {
    let payload = W4Payload::Failure {
        kind: FixtureFailure::Protocol,
        slot: Some(255),
        step_seq: 9,
        errno: None,
    };
    assert_eq!(encode(payload), Err("W4 编码 slot 越界"));
    let captured = super::super::capture_emitted(|| emit(7, payload));
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].source, 7);
    assert_eq!(
        decode(&captured[0]).unwrap(),
        Some(W4Payload::Failure {
            kind: FixtureFailure::Protocol,
            slot: None,
            step_seq: 0,
            errno: None
        })
    );
    assert_eq!(captured[0].generation, 1, "不得向 errno 标志进位");
}

#[test]
fn reserved_fields_unknown_values_and_overflow_are_rejected() {
    let valid = event(W4Payload::DirSyncReturned {
        slot: 0,
        step_seq: 3,
        success: false,
        errno: Some(libc::EBADF),
    });
    for corrupt in [
        TraceEvent {
            generation: valid.generation | (1 << 31),
            ..valid
        },
        TraceEvent {
            generation: 3,
            ..valid
        },
        TraceEvent { b: 1, ..valid },
        TraceEvent {
            a: 1_u64 << 40,
            ..valid
        },
        TraceEvent {
            generation: 0x301,
            ..valid
        },
    ] {
        assert!(decode(&corrupt).is_err());
    }
    assert_eq!(TraceKind::decode(32), None);
    assert_eq!(
        decode(&TraceEvent {
            kind: TraceKind::NormalAcquire,
            ..valid
        })
        .unwrap(),
        None
    );
}
