//! 仅构造诊断输入检验 oracle；不生成 daemon、权限、凭证或真实 SIGKILL 证据。

use super::super::{Point, validate_prefix, verify_temporary_files, verify_untouched_temporaries};
use super::*;
use robot_s0::comm_fault_trace::{TraceEvent, TraceKind};
use std::collections::BTreeSet;

const TXN: ClearTxnIdentity = ClearTxnIdentity {
    txn_id: (11_u128 << 112) | 7,
    expected_epoch: 29,
    original_latched: true,
};
type Events = Vec<(TraceEvent, P)>;

fn diagnostic(source: u64, stamp_ns: u64, payload: P) -> (TraceEvent, P) {
    // 与父旧 oracle 夹具一致：Prefix 仅读元数据和已解码载荷，不把零字段冒作 wire。
    let event = TraceEvent {
        sample_seq: 7,
        stamp_ns,
        kind: TraceKind::W4CommandBegin,
        source,
        normal_seq: 0,
        generation: 0,
        a: 0,
        b: 0,
    };
    (event, payload)
}

fn push(events: &mut Events, source: u64, payload: P) {
    let stamp = 1_000 + u64::try_from(events.len()).unwrap();
    events.push(diagnostic(source, stamp, payload));
}

fn begin(events: &mut Events, source: u64, command: C, token: T) {
    push(events, source, P::CommandBegin { txn: TXN, command });
    push(
        events,
        source,
        P::Token {
            after: false,
            token,
        },
    );
}

fn steps(events: &mut Events, source: u64, stages: &[S], target: S) {
    for (index, &stage) in stages.iter().enumerate() {
        for succeeded in [false, true] {
            push(
                events,
                source,
                P::Stage {
                    succeeded,
                    step_seq: u64::try_from(index + 1).unwrap(),
                    stage,
                    occurrence: u64::from(stage == target),
                },
            );
        }
    }
}

fn completion(events: &mut Events, kind: K, token: T, failure: Option<(S, StepPhase)>) {
    push(events, 1, P::Completion { kind, failure });
    push(events, 1, P::Token { after: true, token });
    push(events, 1, P::CommandEnd { completed: true });
}

fn precursor(events: &mut Events) {
    push(
        events,
        1,
        P::Stage {
            succeeded: false,
            step_seq: 15,
            stage: S::SyncPrimaryDirectory,
            occurrence: 1,
        },
    );
    push(
        events,
        1,
        P::FaultSelected {
            slot: 0,
            step_seq: 15,
            stage: S::SyncPrimaryDirectory,
            occurrence: 1,
        },
    );
    for verified in [false, true] {
        push(
            events,
            1,
            P::Alias {
                verified,
                slot: 0,
                step_seq: 15,
                dev: 41,
                ino: 73,
            },
        );
    }
    push(
        events,
        1,
        P::DirSyncCall {
            slot: 0,
            step_seq: 15,
        },
    );
    push(
        events,
        1,
        P::DirSyncReturned {
            slot: 0,
            step_seq: 15,
            success: false,
            errno: Some(libc::EBADF),
        },
    );
    completion(
        events,
        K::PrepareDurable,
        T::Durable,
        Some((S::SyncPrimaryDirectory, StepPhase::Operation)),
    );
}

fn fixture(spec: Spec) -> Events {
    let mut events = Vec::new();
    begin(&mut events, 1, C::Prepare, T::None);
    let (source, count) = match spec.family {
        Family::Prepare => {
            let count = PREPARE
                .iter()
                .position(|&stage| stage == spec.stage)
                .unwrap()
                + 1;
            steps(&mut events, 1, &PREPARE[..count], spec.stage);
            (1, count)
        }
        Family::FinalizeBefore => {
            steps(&mut events, 1, &PREPARE, S::RemoveMarker);
            completion(&mut events, K::Prepared, T::Prepared, None);
            begin(&mut events, 2, C::RemoveCommitted, T::Prepared);
            steps(&mut events, 2, &FINALIZE[..2], S::RemoveMarker);
            push(
                &mut events,
                2,
                P::Stage {
                    succeeded: false,
                    step_seq: 3,
                    stage: S::RemoveMarker,
                    occurrence: 1,
                },
            );
            (2, 3)
        }
        Family::Repair => {
            steps(&mut events, 1, &PREPARE[..14], S::SyncPrimaryDirectory);
            precursor(&mut events);
            begin(&mut events, 2, C::RepairLatched, T::Durable);
            let count = REPAIR
                .iter()
                .position(|&stage| stage == spec.stage)
                .unwrap()
                + 1;
            steps(&mut events, 2, &REPAIR[..count], spec.stage);
            (2, count)
        }
    };
    push(
        &mut events,
        source,
        P::KillReached {
            step_seq: u64::try_from(count).unwrap(),
            stage: spec.stage,
            entered_at: 1_000,
            pause_until: 1_000_000,
        },
    );
    events
}

fn validate(events: &[(TraceEvent, P)], spec: Spec) -> Result<Value, String> {
    validate_prefix(events, Point::Batch(spec), 7, 500, 10_000, 10_001)
}

fn replace(events: &mut [(TraceEvent, P)], index: usize, payload: P) {
    let old = events[index].0;
    events[index] = diagnostic(old.source, old.stamp_ns, payload);
}

fn index_of(events: &[(TraceEvent, P)], predicate: impl Fn(&(TraceEvent, P)) -> bool) -> usize {
    events.iter().position(predicate).unwrap()
}

#[test]
fn fixed_table_has_twenty_five_unique_labels_and_twelve_one_twelve_families() {
    assert_eq!(POINTS.len(), 25);
    let labels: BTreeSet<_> = POINTS.iter().map(|spec| spec.label).collect();
    assert_eq!(labels.len(), 25);
    for (family, count) in [
        (Family::Prepare, 12),
        (Family::FinalizeBefore, 1),
        (Family::Repair, 12),
    ] {
        assert_eq!(
            POINTS.iter().filter(|spec| spec.family == family).count(),
            count
        );
    }
    for stage in REPAIR {
        assert_eq!(
            POINTS
                .iter()
                .filter(|spec| spec.family == Family::Repair && spec.stage == stage)
                .count(),
            1
        );
    }
    for stage in PREPARE.into_iter().filter(|stage| {
        !matches!(
            stage,
            S::CreateMarker | S::RenameBackup | S::SyncPrimaryDirectory
        )
    }) {
        assert_eq!(
            POINTS
                .iter()
                .filter(|spec| spec.family == Family::Prepare && spec.stage == stage)
                .count(),
            1
        );
    }
    for spec in POINTS {
        assert_eq!(Point::Batch(spec).configuration(), spec.label);
        assert_eq!(Point::Batch(spec).stage(), spec.stage);
    }
    assert!(labels.contains("prepare-sample-marker-identity"));
    assert!(labels.contains("finalize-before-unlink"));
    assert!(labels.contains("repair-after-read_marker"));
    assert!(labels.contains("repair-after-sync_backup_directory"));
}

#[test]
fn all_twenty_five_complete_diagnostic_prefixes_parse() {
    for spec in POINTS {
        let events = fixture(spec);
        let report =
            validate(&events, spec).unwrap_or_else(|error| panic!("{}: {error}", spec.label));
        assert_eq!(report["txn_id"], TXN.txn_id.to_string());
        assert_eq!(report["expected_epoch"], TXN.expected_epoch);
        assert_eq!(report["first_rr_retired"], false);
        assert_eq!(
            report["target_phase"],
            if spec.family == Family::FinalizeBefore {
                "Before"
            } else {
                "AfterSuccess"
            }
        );
        if spec.family == Family::Repair {
            assert_eq!(report["predecessor"]["actual_errno"], libc::EBADF);
            assert_eq!(report["source"], 2);
        } else {
            assert!(report["predecessor"].is_null());
        }
    }
}

#[test]
fn every_missing_step_or_step_phase_is_rejected_in_each_fixed_prefix() {
    for spec in POINTS {
        let valid = fixture(spec);
        for (index, &(event, payload)) in valid.iter().enumerate() {
            let P::Stage { step_seq, .. } = payload else {
                continue;
            };
            let mut missing_phase = valid.clone();
            missing_phase.remove(index);
            assert!(
                validate(&missing_phase, spec).is_err(),
                "{} 第 {index} 相缺失",
                spec.label
            );
            let mut missing_step = valid.clone();
            missing_step.retain(|(candidate, payload)| {
                !(candidate.source == event.source
                    && matches!(payload, P::Stage { step_seq: actual, .. } if *actual == step_seq))
            });
            assert!(
                validate(&missing_step, spec).is_err(),
                "{} 第 {step_seq} 步缺失",
                spec.label
            );
        }
    }
}

#[test]
fn finalize_before_rejects_fabricated_success_and_every_prefix_rejects_extra_tail() {
    let spec = POINTS
        .into_iter()
        .find(|spec| spec.family == Family::FinalizeBefore)
        .unwrap();
    let valid = fixture(spec);
    let before = index_of(&valid, |(_, payload)| {
        matches!(
            payload,
            P::Stage {
                succeeded: false,
                stage: S::RemoveMarker,
                ..
            }
        )
    });
    let success = P::Stage {
        succeeded: true,
        step_seq: 3,
        stage: S::RemoveMarker,
        occurrence: 1,
    };
    let mut replaced = valid.clone();
    replace(&mut replaced, before, success);
    assert!(validate(&replaced, spec).is_err());
    let mut inserted = valid;
    inserted.insert(
        before + 1,
        diagnostic(2, inserted[before].0.stamp_ns, success),
    );
    assert!(validate(&inserted, spec).is_err());
    for spec in POINTS {
        let valid = fixture(spec);
        for extra in [
            P::Completion {
                kind: K::RepairedLatched,
                failure: None,
            },
            P::Token {
                after: true,
                token: T::Repaired,
            },
            P::CommandEnd { completed: true },
            valid.last().unwrap().1,
        ] {
            let mut events = valid.clone();
            let last = events.last().unwrap().0;
            events.push(diagnostic(last.source, last.stamp_ns, extra));
            assert!(
                validate(&events, spec).is_err(),
                "{} 额外事件 {extra:?}",
                spec.label
            );
        }
    }
}

#[test]
fn repair_requires_every_alias_fact_and_the_exact_original_syscall_result() {
    for spec in POINTS
        .into_iter()
        .filter(|spec| spec.family == Family::Repair)
    {
        let valid = fixture(spec);
        for (index, (_, payload)) in valid.iter().enumerate() {
            if matches!(
                payload,
                P::FaultSelected { .. }
                    | P::Alias { .. }
                    | P::DirSyncCall { .. }
                    | P::DirSyncReturned { .. }
            ) {
                let mut missing = valid.clone();
                missing.remove(index);
                assert!(validate(&missing, spec).is_err());
            }
        }
        let returned = index_of(&valid, |(_, payload)| {
            matches!(payload, P::DirSyncReturned { .. })
        });
        for (success, errno) in [(false, Some(libc::EIO)), (false, None), (true, None)] {
            let mut wrong = valid.clone();
            replace(
                &mut wrong,
                returned,
                P::DirSyncReturned {
                    slot: 0,
                    step_seq: 15,
                    success,
                    errno,
                },
            );
            assert!(validate(&wrong, spec).is_err());
        }
        let verified = index_of(&valid, |(_, payload)| {
            matches!(payload, P::Alias { verified: true, .. })
        });
        for (dev, ino) in [(42, 73), (41, 74), (41, 0)] {
            let mut wrong = valid.clone();
            replace(
                &mut wrong,
                verified,
                P::Alias {
                    verified: true,
                    slot: 0,
                    step_seq: 15,
                    dev,
                    ino,
                },
            );
            assert!(validate(&wrong, spec).is_err());
        }
    }
}

#[test]
fn repair_rejects_wrong_transaction_source_occurrence_and_early_completion() {
    for spec in POINTS
        .into_iter()
        .filter(|spec| spec.family == Family::Repair)
    {
        let valid = fixture(spec);
        let begin = index_of(&valid, |(event, payload)| {
            event.source == 2 && matches!(payload, P::CommandBegin { .. })
        });
        for txn in [
            ClearTxnIdentity {
                txn_id: TXN.txn_id ^ (1_u128 << 111),
                ..TXN
            },
            ClearTxnIdentity {
                expected_epoch: TXN.expected_epoch + 1,
                ..TXN
            },
            ClearTxnIdentity {
                original_latched: false,
                ..TXN
            },
        ] {
            let mut wrong = valid.clone();
            replace(
                &mut wrong,
                begin,
                P::CommandBegin {
                    txn,
                    command: C::RepairLatched,
                },
            );
            assert!(validate(&wrong, spec).is_err());
        }
        for index in 0..valid.len() {
            let mut wrong = valid.clone();
            wrong[index].0.source += 1;
            assert!(validate(&wrong, spec).is_err());
        }
        for (index, &(_, payload)) in valid.iter().enumerate() {
            if let P::Stage {
                succeeded,
                step_seq,
                stage,
                occurrence,
            } = payload
            {
                let mut wrong = valid.clone();
                replace(
                    &mut wrong,
                    index,
                    P::Stage {
                        succeeded,
                        step_seq,
                        stage,
                        occurrence: occurrence + 1,
                    },
                );
                assert!(validate(&wrong, spec).is_err());
            }
        }
        let mut early = valid.clone();
        early.insert(
            begin,
            diagnostic(
                1,
                valid[begin].0.stamp_ns,
                P::Completion {
                    kind: K::PrepareDurable,
                    failure: Some((S::SyncPrimaryDirectory, StepPhase::Operation)),
                },
            ),
        );
        assert!(validate(&early, spec).is_err());
    }
}

#[test]
fn repair_rejects_unclosed_or_changed_predecessor_completion_and_tokens() {
    for spec in POINTS
        .into_iter()
        .filter(|spec| spec.family == Family::Repair)
    {
        let valid = fixture(spec);
        let completed = index_of(&valid, |(_, payload)| {
            matches!(payload, P::Completion { .. })
        });
        let after = index_of(&valid, |(_, payload)| {
            matches!(payload, P::Token { after: true, .. })
        });
        let end = index_of(&valid, |(_, payload)| {
            matches!(payload, P::CommandEnd { .. })
        });
        let before = index_of(&valid, |(event, payload)| {
            event.source == 2 && matches!(payload, P::Token { after: false, .. })
        });
        for (index, payload) in [
            (
                completed,
                P::Completion {
                    kind: K::Prepared,
                    failure: None,
                },
            ),
            (
                completed,
                P::Completion {
                    kind: K::PrepareDurable,
                    failure: Some((S::SyncPrimaryDirectory, StepPhase::After)),
                },
            ),
            (
                completed,
                P::Completion {
                    kind: K::PrepareDurable,
                    failure: Some((S::SyncBackupDirectory, StepPhase::Operation)),
                },
            ),
            (
                after,
                P::Token {
                    after: true,
                    token: T::Prepared,
                },
            ),
            (end, P::CommandEnd { completed: false }),
            (
                before,
                P::Token {
                    after: false,
                    token: T::Created,
                },
            ),
        ] {
            let mut wrong = valid.clone();
            replace(&mut wrong, index, payload);
            assert!(validate(&wrong, spec).is_err());
        }
        for index in [completed, after, end, before] {
            let mut missing = valid.clone();
            missing.remove(index);
            assert!(validate(&missing, spec).is_err());
        }
        let mut zero_identity = valid.clone();
        for (index, &(_, payload)) in valid.iter().enumerate() {
            if let P::Alias { verified, .. } = payload {
                replace(
                    &mut zero_identity,
                    index,
                    P::Alias {
                        verified,
                        slot: 0,
                        step_seq: 15,
                        dev: 41,
                        ino: 0,
                    },
                );
            }
        }
        assert!(validate(&zero_identity, spec).is_err());
    }
}

fn file_snapshot(spec: Spec, pid: u32) -> Value {
    let mut files = serde_json::Map::new();
    for name in [
        "estop_latch",
        "estop_latch.bak",
        "estop_latch.clear_pending",
    ] {
        files.insert(name.to_owned(), json!({"present":true}));
    }
    if let Some(temporary) = spec.temporary {
        files.insert(
            format!("{}.clear_tmp.{pid}.1", temporary.target()),
            json!({
                "present":true, "directory":false, "nlink":1, "dev":41, "ino":73,
                "bytes":temporary.bytes(), "len":temporary.bytes().len(),
            }),
        );
    }
    json!({"files":files})
}

#[test]
fn temporary_file_oracle_rejects_missing_extra_wrong_pid_bytes_and_recovery_changes() {
    for spec in POINTS {
        let valid = file_snapshot(spec, 31);
        assert!(verify_temporary_files(Point::Batch(spec), &valid, 31).is_ok());
        assert!(verify_untouched_temporaries(&valid, &valid).is_ok());
        let mut extra = valid.clone();
        extra["files"]["unknown"] = json!({"present":true});
        assert!(verify_temporary_files(Point::Batch(spec), &extra, 31).is_err());
        assert!(verify_untouched_temporaries(&valid, &extra).is_err());
        let Some(temporary) = spec.temporary else {
            continue;
        };
        let name = format!("{}.clear_tmp.31.1", temporary.target());
        let mut missing = valid.clone();
        missing["files"].as_object_mut().unwrap().remove(&name);
        assert!(verify_temporary_files(Point::Batch(spec), &missing, 31).is_err());
        assert!(verify_untouched_temporaries(&valid, &missing).is_err());
        assert!(verify_temporary_files(Point::Batch(spec), &valid, 32).is_err());
        for (field, changed) in [
            ("ino", json!(0)),
            ("bytes", json!([9])),
            ("len", json!(99)),
            ("nlink", json!(2)),
            ("directory", json!(true)),
            ("present", json!(false)),
        ] {
            let mut wrong = valid.clone();
            wrong["files"][&name][field] = changed;
            assert!(verify_temporary_files(Point::Batch(spec), &wrong, 31).is_err());
            assert!(verify_untouched_temporaries(&valid, &wrong).is_err());
        }
        let mut replaced = valid.clone();
        replaced["files"][&name]["dev"] = json!(42);
        assert!(verify_untouched_temporaries(&valid, &replaced).is_err());
    }
}
