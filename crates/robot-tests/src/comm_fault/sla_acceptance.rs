//! AC-12 解析器的独立 oracle：合成事件仅用于拒绝错误证据，绝不作为实测 SLA 数据。
//!
//! 时间关系来自 Spec 的 t0 / t1 / normal 事务执行权与物理全关语义。
//! 采集测试仅替换一次输入批次；不重建执行器状态机，不把合成证据当实测数据。

use robot_s0::comm_fault_trace::{TraceEvent, TraceKind};

use super::sla::{Sample, analyze};

const EPOCH: u64 = 73;
const BASE_NS: u64 = 1_000_000_000;
const NORMAL_SEQ: u64 = 37;
const T0: u64 = BASE_NS + 2_000_000;
const T1: u64 = BASE_NS + 2_200_000;
const ALL_OFF: u64 = BASE_NS + 12_000_000;

fn event(kind: TraceKind, stamp_ns: u64) -> TraceEvent {
    TraceEvent {
        sample_seq: EPOCH,
        stamp_ns,
        kind,
        source: 0,
        normal_seq: 0,
        generation: 0,
        a: 0,
        b: 0,
    }
}

fn successful(kind: TraceKind, stamp_ns: u64, a: u64) -> TraceEvent {
    TraceEvent {
        a,
        b: 1,
        ..event(kind, stamp_ns)
    }
}

fn interval(kind: TraceKind, stamp_ns: u64, normal_seq: u64) -> TraceEvent {
    TraceEvent {
        normal_seq,
        ..successful(kind, stamp_ns, 0)
    }
}

fn valid_events(external: bool, in_flight: bool) -> Vec<TraceEvent> {
    let mut events = vec![
        event(
            if external {
                TraceKind::ExternalEnter
            } else {
                TraceKind::TopicReceived
            },
            T0,
        ),
        TraceEvent {
            a: 2,
            b: u64::from(in_flight),
            normal_seq: if in_flight { NORMAL_SEQ } else { 0 },
            ..event(TraceKind::PrimitiveCommitted, T1)
        },
        successful(TraceKind::SerialClaim, BASE_NS + 4_000_000, 3),
        successful(TraceKind::IpcClaim, BASE_NS + 4_500_000, 5),
        TraceEvent {
            a: 1,
            ..event(TraceKind::SafetyStart, BASE_NS + 5_000_000)
        },
        successful(TraceKind::SafetyReturn, BASE_NS + 6_000_000, 1),
        event(TraceKind::IpcEStopCommit, BASE_NS + 7_000_000),
        successful(TraceKind::ConfirmReturn, ALL_OFF, 0),
        event(TraceKind::AllOffTrusted, ALL_OFF),
    ];
    if in_flight {
        events.push(interval(
            TraceKind::NormalAcquire,
            BASE_NS + 1_000_000,
            NORMAL_SEQ,
        ));
        events.push(interval(
            TraceKind::NormalRelease,
            BASE_NS + 3_500_000,
            NORMAL_SEQ,
        ));
    }
    events.sort_by_key(|event| event.stamp_ns);
    events
}

fn one(events: &mut [TraceEvent], kind: TraceKind) -> &mut TraceEvent {
    events
        .iter_mut()
        .find(|event| event.kind == kind)
        .expect("独立基准表缺少预期事件")
}

fn valid_sample(events: &[TraceEvent], external: bool, in_flight: bool) -> Sample {
    let sample =
        analyze(events, external).unwrap_or_else(|error| panic!("完整独立基准表被拒绝：{error}"));
    assert_eq!(sample.in_flight, in_flight);
    sample
}

fn rejected(mut events: Vec<TraceEvent>, external: bool, reason: &str) {
    events.sort_by_key(|event| event.stamp_ns);
    let result = analyze(&events, external);
    assert!(
        result.is_err(),
        "错误证据必须拒绝：{reason}；实际结果={result:?}；events={events:?}"
    );
}

#[test]
fn sla_parser_accepts_all_four_complete_source_and_load_combinations() {
    for external in [false, true] {
        for in_flight in [false, true] {
            let sample = valid_sample(&valid_events(external, in_flight), external, in_flight);
            assert_eq!(sample.t0_ns, T0);
            assert_eq!(sample.t1_ns, T1);
            assert_eq!(sample.all_off_ns, ALL_OFF);
            assert_eq!(sample.primitive_retries, 2);
        }
    }
}

#[test]
fn sla_parser_accepts_periodic_sequence_zero_and_non_authoritative_primitive_sequence() {
    for external in [false, true] {
        let mut events = valid_events(external, true);
        one(&mut events, TraceKind::PrimitiveCommitted).normal_seq = 0;
        valid_sample(&events, external, true);
        // primitive 的旁观 seq 在 CAS 后读取；不能当作线性化瞬间的动作身份。
        one(&mut events, TraceKind::PrimitiveCommitted).normal_seq = NORMAL_SEQ + 99;
        valid_sample(&events, external, true);
        for event in &mut events {
            if matches!(
                event.kind,
                TraceKind::NormalAcquire | TraceKind::NormalRelease
            ) {
                event.normal_seq = 0;
            }
        }
        valid_sample(&events, external, true);
    }
}

#[test]
fn sla_parser_rejects_empty_epoch_zero_and_mixed_epochs() {
    assert!(analyze(&[], false).is_err());
    for external in [false, true] {
        let mut events = valid_events(external, true);
        for event in &mut events {
            event.sample_seq = 0;
        }
        rejected(events, external, "epoch 零不能作为一次正式采样");
        for kind in [
            TraceKind::NormalRelease,
            TraceKind::SafetyReturn,
            TraceKind::AllOffTrusted,
        ] {
            let mut events = valid_events(external, true);
            one(&mut events, kind).sample_seq += 1;
            rejected(events, external, "混入其它 epoch 的完成不能补齐当前样本");
        }
    }
}

#[test]
fn sla_parser_rejects_missing_primitive_claims_safety_and_physical_confirmation() {
    for external in [false, true] {
        for in_flight in [false, true] {
            for missing in [
                TraceKind::PrimitiveCommitted,
                TraceKind::SerialClaim,
                TraceKind::IpcClaim,
                TraceKind::IpcEStopCommit,
                TraceKind::SafetyStart,
                TraceKind::SafetyReturn,
                TraceKind::ConfirmReturn,
                TraceKind::AllOffTrusted,
            ] {
                let mut events = valid_events(external, in_flight);
                events.retain(|event| event.kind != missing);
                rejected(events, external, &format!("缺少 {missing:?}"));
            }
        }
    }
}

#[test]
fn sla_parser_rejects_second_primitive_or_second_successful_claim() {
    for external in [false, true] {
        for kind in [
            TraceKind::PrimitiveCommitted,
            TraceKind::SerialClaim,
            TraceKind::IpcClaim,
            TraceKind::IpcEStopCommit,
            TraceKind::AllOffTrusted,
        ] {
            let mut events = valid_events(external, true);
            let mut duplicate = *one(&mut events, kind);
            duplicate.stamp_ns += 1;
            events.push(duplicate);
            rejected(
                events,
                external,
                "一次逻辑 EStop 不得重复原语、成功 claim 或提交",
            );
        }
    }
}

#[test]
fn sla_parser_allows_failed_claim_attempts_without_counting_them_as_successful_claims() {
    for external in [false, true] {
        let mut events = valid_events(external, false);
        events.push(event(TraceKind::SerialClaim, BASE_NS + 500_000));
        events.push(event(TraceKind::IpcClaim, BASE_NS + 1_000_000));
        events.push(event(TraceKind::SerialClaim, BASE_NS + 8_000_000));
        events.sort_by_key(|event| event.stamp_ns);
        valid_sample(&events, external, false);
    }
}

#[test]
fn sla_parser_rejects_both_estop_entry_paths_in_one_epoch() {
    for external in [false, true] {
        let mut events = valid_events(external, false);
        events.push(event(
            if external {
                TraceKind::TopicReceived
            } else {
                TraceKind::ExternalEnter
            },
            T0 + 1,
        ));
        rejected(
            events,
            external,
            "两个逻辑入口不能共用一个成功原语和一轮全关",
        );
    }
}

#[test]
fn sla_parser_rejects_merged_latch_only_and_undefined_primitive_outcomes() {
    for external in [false, true] {
        for outcome in [2, 3, 4, 5, 8, 16, u64::MAX] {
            let mut events = valid_events(external, outcome & 1 != 0);
            one(&mut events, TraceKind::PrimitiveCommitted).b = outcome;
            rejected(
                events,
                external,
                "Merged/LatchOnly 或未定义标志不能冒充 StopIssued",
            );
        }
        for outcome in [2, 4] {
            let mut events = valid_events(external, false);
            one(&mut events, TraceKind::PrimitiveCommitted).b = outcome;
            events.retain(|event| {
                !matches!(
                    event.kind,
                    TraceKind::SafetyStart
                        | TraceKind::SafetyReturn
                        | TraceKind::ConfirmReturn
                        | TraceKind::AllOffTrusted
                )
            });
            rejected(
                events,
                external,
                "零 IO 的合并/仅置锁事件不构成物理 SLA 样本",
            );
        }
    }
}

#[test]
fn sla_parser_classifies_t0_from_the_complete_lease_interval_not_presence_of_io() {
    for external in [false, true] {
        // t0 在 acquire 之前，至 t1 时事务已在途；SLA 分组仍按 t0 定义判 idle。
        let mut after_t0 = valid_events(external, true);
        one(&mut after_t0, TraceKind::NormalAcquire).stamp_ns = T0 + 1;
        after_t0.sort_by_key(|event| event.stamp_ns);
        valid_sample(&after_t0, external, false);
        // release 与 t0 同时，采用 acquire <= t0 < release，不能再归为在途。
        let mut ended = valid_events(external, true);
        one(&mut ended, TraceKind::NormalRelease).stamp_ns = T0;
        one(&mut ended, TraceKind::PrimitiveCommitted).b = 0;
        ended.sort_by_key(|event| event.stamp_ns);
        valid_sample(&ended, external, false);
        let mut at_acquire = valid_events(external, true);
        one(&mut at_acquire, TraceKind::NormalAcquire).stamp_ns = T0;
        at_acquire.sort_by_key(|event| event.stamp_ns);
        valid_sample(&at_acquire, external, true);
    }
}

#[test]
fn sla_parser_rejects_active_bit_contradictions_and_unpaired_leases() {
    for external in [false, true] {
        for in_flight in [false, true] {
            let mut events = valid_events(external, in_flight);
            one(&mut events, TraceKind::PrimitiveCommitted).b ^= 1;
            rejected(
                events,
                external,
                "真实成功 CAS 的旧 ACTIVE 必须与完整执行权区间一致",
            );
        }
        for missing in [TraceKind::NormalAcquire, TraceKind::NormalRelease] {
            let mut events = valid_events(external, true);
            events.retain(|event| event.kind != missing);
            rejected(events, external, "在途事务缺少配对 acquire/release");
        }
        let mut events = valid_events(external, true);
        let duplicate = *one(&mut events, TraceKind::NormalAcquire);
        events.push(duplicate);
        rejected(events, external, "同一 bus 不得重复持有执行权");
    }
}

#[test]
fn sla_parser_rejects_normal_sequence_or_generation_mismatch() {
    for external in [false, true] {
        let mut events = valid_events(external, true);
        one(&mut events, TraceKind::NormalRelease).normal_seq += 1;
        rejected(
            events,
            external,
            "一个 normal 动作不能释放另一个动作的执行权",
        );
        let mut events = valid_events(external, true);
        one(&mut events, TraceKind::NormalRelease).generation = 1;
        rejected(
            events,
            external,
            "normal acquire/release 不能跨故障代次拼接",
        );
    }
}

#[test]
fn sla_parser_rejects_fault_generation_mixing_in_the_physical_chain() {
    for external in [false, true] {
        for kind in [
            TraceKind::PrimitiveCommitted,
            TraceKind::SerialClaim,
            TraceKind::IpcClaim,
            TraceKind::SafetyStart,
            TraceKind::SafetyReturn,
            TraceKind::ConfirmReturn,
            TraceKind::AllOffTrusted,
            TraceKind::IpcEStopCommit,
        ] {
            let mut events = valid_events(external, false);
            one(&mut events, kind).generation = 9;
            rejected(events, external, &format!("{kind:?} 属于其它故障代次"));
        }
    }
}

#[test]
fn sla_parser_rejects_new_normal_acquire_between_t1_and_full_physical_completion() {
    for external in [false, true] {
        for started in [T1, T1 + 1, ALL_OFF - 2] {
            let mut events = valid_events(external, false);
            events.push(interval(TraceKind::NormalAcquire, started, NORMAL_SEQ));
            events.push(interval(TraceKind::NormalRelease, started + 1, NORMAL_SEQ));
            rejected(events, external, "t1 至全关之间不能启动新 normal 事务");
        }
    }
}

#[test]
fn sla_parser_allows_normal_sampling_after_full_physical_completion() {
    for external in [false, true] {
        let mut events = valid_events(external, false);
        events.push(interval(TraceKind::NormalAcquire, ALL_OFF + 1, 0));
        events.push(interval(TraceKind::NormalRelease, ALL_OFF + 2, 0));
        valid_sample(&events, external, false);
    }
}

#[test]
fn sla_parser_rejects_physical_safety_overlapping_a_retained_normal_transaction() {
    for external in [false, true] {
        let mut events = valid_events(external, true);
        one(&mut events, TraceKind::SerialClaim).stamp_ns = T1 + 1;
        one(&mut events, TraceKind::SafetyStart).stamp_ns = T1 + 2;
        one(&mut events, TraceKind::SafetyReturn).stamp_ns = T1 + 3;
        rejected(
            events,
            external,
            "Safety 不能在旧 normal 事务尚未 release 时使用同一总线",
        );
    }
}

#[test]
fn sla_parser_rejects_causal_order_reversals() {
    for external in [false, true] {
        for (kind, stamp_ns) in [
            (TraceKind::PrimitiveCommitted, T0 - 1),
            (TraceKind::SerialClaim, T1 - 1),
            (TraceKind::IpcClaim, T1 - 1),
            (TraceKind::SafetyStart, BASE_NS + 3_900_000),
            (TraceKind::SafetyReturn, BASE_NS + 4_900_000),
            (TraceKind::IpcEStopCommit, BASE_NS + 4_400_000),
            (TraceKind::ConfirmReturn, BASE_NS + 5_900_000),
        ] {
            let mut events = valid_events(external, false);
            one(&mut events, kind).stamp_ns = stamp_ns;
            rejected(events, external, &format!("{kind:?} 早于对应前置"));
        }
    }
}

#[test]
fn sla_parser_rejects_safety_attempt_mismatch_and_unpaired_extra_calls() {
    for external in [false, true] {
        let mut events = valid_events(external, false);
        one(&mut events, TraceKind::SafetyReturn).a = 2;
        rejected(
            events,
            external,
            "Safety 第一次 start 不能由第二次 return 补齐",
        );
        let mut events = valid_events(external, false);
        events.push(TraceEvent {
            a: 2,
            ..event(TraceKind::SafetyStart, BASE_NS + 8_000_000)
        });
        rejected(events, external, "额外 Safety start 没有真实返回");
        let mut events = valid_events(external, false);
        events.push(successful(TraceKind::SafetyReturn, BASE_NS + 8_000_000, 2));
        rejected(events, external, "额外 Safety return 没有对应调用");
    }
}

#[test]
fn sla_parser_rejects_failed_or_torque_on_confirm_as_full_off_evidence() {
    for external in [false, true] {
        for (kind, field_a, field_b) in [
            (TraceKind::SafetyReturn, 1, 0),
            (TraceKind::ConfirmReturn, 0, 0),
            (TraceKind::ConfirmReturn, 1, 1),
        ] {
            let mut events = valid_events(external, false);
            let event = one(&mut events, kind);
            event.a = field_a;
            event.b = field_b;
            rejected(events, external, "失败或仍开扭的返回不能补齐全关证据");
        }
        let mut events = valid_events(external, false);
        one(&mut events, TraceKind::AllOffTrusted).stamp_ns += 1;
        rejected(
            events,
            external,
            "AllOffTrusted 必须对应同一实际 Confirm 返回时刻",
        );
    }
}

#[test]
fn sla_parser_rejects_duplicate_confirmation_at_the_claimed_full_off_boundary() {
    for external in [false, true] {
        let mut events = valid_events(external, false);
        let duplicate = *one(&mut events, TraceKind::ConfirmReturn);
        events.push(duplicate);
        rejected(
            events,
            external,
            "同一全关结果不能对应两个无法区分的物理 Confirm 返回",
        );
    }
}

#[test]
fn sla_parser_rejects_all_fault_observation_kinds_in_a_healthy_sample() {
    for external in [false, true] {
        for kind in [
            TraceKind::CommErrorObserved,
            TraceKind::FirstCommError,
            TraceKind::MailboxPublishing,
            TraceKind::MailboxPublished,
            TraceKind::FaultIpcCommit,
        ] {
            let mut events = valid_events(external, false);
            // 保持其余 generation 不变，单独检验事件种类本身，避免被代次检查代偿。
            events.push(event(kind, BASE_NS + 9_000_000));
            rejected(
                events,
                external,
                "真实故障不能因省略中间通知而变成健康成功样本",
            );
        }
    }
}

#[test]
fn sla_parser_preserves_slow_complete_samples_for_later_percentile_judgment() {
    for external in [false, true] {
        let mut events = valid_events(external, false);
        let late = BASE_NS + 8_000_000_000;
        one(&mut events, TraceKind::ConfirmReturn).stamp_ns = late;
        one(&mut events, TraceKind::AllOffTrusted).stamp_ns = late;
        let sample = valid_sample(&events, external, false);
        assert_eq!(
            sample.all_off_ns, late,
            "解析器不得按时延删掉会使 SLA 判据失败的完整样本"
        );
    }
}

fn physically_loaded_events(external: bool, requested_ns: u64, elapsed_ns: u64) -> Vec<TraceEvent> {
    let mut events = valid_events(external, true);
    one(&mut events, TraceKind::NormalRelease).stamp_ns = BASE_NS + 7_000_000;
    one(&mut events, TraceKind::SerialClaim).stamp_ns = BASE_NS + 8_000_000;
    one(&mut events, TraceKind::SafetyStart).stamp_ns = BASE_NS + 9_000_000;
    one(&mut events, TraceKind::SafetyReturn).stamp_ns = BASE_NS + 10_000_000;
    events.push(TraceEvent {
        a: requested_ns,
        b: elapsed_ns,
        normal_seq: NORMAL_SEQ,
        ..event(TraceKind::InjectedDelay, BASE_NS + 1_250_000)
    });
    events.sort_by_key(|event| event.stamp_ns);
    events
}

#[test]
fn physical_inflight_accepts_actual_delay_within_the_exact_lease_including_bounds() {
    for external in [false, true] {
        for (requested, elapsed) in [
            (1_000_000, 1_000_000),
            (3_000_000, 3_700_000),
            (5_000_000, 5_500_000),
        ] {
            let events = physically_loaded_events(external, requested, elapsed);
            let sample = valid_sample(&events, external, true);
            super::sla::validate_inflight_delay(&events, &sample).unwrap();
            let mut periodic = events.clone();
            for event in &mut periodic {
                if matches!(
                    event.kind,
                    TraceKind::NormalAcquire | TraceKind::NormalRelease | TraceKind::InjectedDelay
                ) {
                    event.normal_seq = 0;
                }
            }
            let sample = valid_sample(&periodic, external, true);
            super::sla::validate_inflight_delay(&periodic, &sample).unwrap();
        }
    }
}

#[test]
fn physical_inflight_rejects_missing_zero_out_of_range_and_short_actual_delay() {
    for external in [false, true] {
        let baseline = physically_loaded_events(external, 1_000_000, 1_100_000);
        let sample = valid_sample(&baseline, external, true);
        let mut missing = baseline.clone();
        missing.retain(|event| event.kind != TraceKind::InjectedDelay);
        assert!(super::sla::validate_inflight_delay(&missing, &sample).is_err());
        for (requested, elapsed) in [
            (0, 1_100_000),
            (999_999, 1_100_000),
            (5_000_001, 5_000_001),
            (1_000_000, 999_999),
        ] {
            let mut events = baseline.clone();
            let delay = one(&mut events, TraceKind::InjectedDelay);
            delay.a = requested;
            delay.b = elapsed;
            assert!(
                super::sla::validate_inflight_delay(&events, &sample).is_err(),
                "非法请求/实耗被接纳：{requested}/{elapsed}"
            );
        }
    }
}

#[test]
fn physical_inflight_rejects_delay_identity_mismatch_duplicates_and_outside_lease() {
    let baseline = physically_loaded_events(false, 1_000_000, 1_100_000);
    let sample = valid_sample(&baseline, false, true);
    for changed in 0..3 {
        let mut events = baseline.clone();
        let delay = one(&mut events, TraceKind::InjectedDelay);
        match changed {
            0 => delay.sample_seq += 1,
            1 => delay.generation += 1,
            _ => delay.normal_seq += 1,
        }
        assert!(super::sla::validate_inflight_delay(&events, &sample).is_err());
    }
    for (started, elapsed) in [
        (BASE_NS, 1_100_000),
        (BASE_NS + 6_500_000, 1_100_000),
        (BASE_NS + 8_000_000, 1_100_000),
        (BASE_NS + 1_250_000, u64::MAX),
    ] {
        let mut events = baseline.clone();
        let delay = one(&mut events, TraceKind::InjectedDelay);
        delay.stamp_ns = started;
        delay.b = elapsed;
        assert!(super::sla::validate_inflight_delay(&events, &sample).is_err());
    }
    let mut duplicate = baseline;
    let extra = *one(&mut duplicate, TraceKind::InjectedDelay);
    duplicate.push(extra);
    assert!(super::sla::validate_inflight_delay(&duplicate, &sample).is_err());
}

#[tokio::test]
async fn collect_preserves_foreign_epoch_received_while_waiting_for_completion() {
    let complete = valid_events(false, false);
    let foreign = TraceEvent {
        sample_seq: EPOCH + 1,
        ..event(TraceKind::IpcClaim, T0 + 1)
    };
    let first = complete[..3].to_vec();
    let mut second = complete[3..].to_vec();
    second.insert(0, foreign);
    let mut batches = std::collections::VecDeque::from([first, second]);
    let mut drains = 0;
    let mut captured = Vec::new();
    let result = super::sla::collect_batches(
        EPOCH,
        &mut captured,
        || {
            drains += 1;
            batches.pop_front().expect("应在实际第二批检测混代")
        },
        || 0,
        || Ok(true),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(drains, 2, "必须经过等待中的下一次采集，不能仅调用analyze");
    assert_eq!(
        captured.len(),
        complete.len() + 1,
        "已经drain的外代与本代事件必须全部保留"
    );
    assert_eq!(
        captured
            .iter()
            .filter(|event| event.sample_seq == EPOCH + 1)
            .copied()
            .collect::<Vec<_>>(),
        vec![foreign]
    );
    for event in complete {
        assert!(captured.contains(&event));
    }
}
