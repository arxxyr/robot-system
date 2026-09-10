//! AC-12 解析器的独立 oracle：合成事件仅用于拒绝错误证据，绝不作为实测 SLA 数据。
//!
//! 时间关系来自 Spec 的 t0 / t1 / normal 事务执行权与物理全关语义。
//! 采集测试仅替换一次输入批次；不重建执行器状态机，不把合成证据当实测数据。

use robot_s0::comm_fault_trace::{TraceEvent, TraceKind};

use super::sla::{
    H2RtProfile, ProcessRtSnapshot, Sample, ThreadRtSnapshot, analyze, read_s0_threads, s0_command,
    validate_ordinary_rt_snapshot, validate_s0_rt_snapshot,
};

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
        // 真机出现过 t0 在途、t1 已 release；t1 旧 ACTIVE=0 不能将其误判为 idle。
        let mut ends_before_primitive = valid_events(external, true);
        one(&mut ends_before_primitive, TraceKind::NormalRelease).stamp_ns = T0 + 1;
        one(&mut ends_before_primitive, TraceKind::PrimitiveCommitted).b = 0;
        ends_before_primitive.sort_by_key(|event| event.stamp_ns);
        valid_sample(&ends_before_primitive, external, true);
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

// PX-56 的独立环境反例；这些合成快照不能用于宣称真实 Orin 测量通过。
struct RtDirectory(std::path::PathBuf);

impl RtDirectory {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "robot-rt-preflight-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for RtDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ordinary_thread(tid: u32, comm: &str) -> ThreadRtSnapshot {
    ThreadRtSnapshot {
        tid,
        comm: Some(comm.to_owned()),
        policy: Some(libc::SCHED_OTHER),
        priority: Some(0),
        cpus: Some(vec![0, 1, 2, 3]),
    }
}

fn px56_snapshot(serial_cpu: usize) -> ProcessRtSnapshot {
    ProcessRtSnapshot {
        pid: 700,
        tids_before: vec![700, 701, 702],
        tids_after: vec![700, 701, 702],
        threads: vec![
            ordinary_thread(700, "s0_daemon"),
            ordinary_thread(701, "s0_ipc_thread"),
            ThreadRtSnapshot {
                policy: Some(libc::SCHED_FIFO),
                priority: Some(70),
                cpus: Some(vec![serial_cpu]),
                ..ordinary_thread(702, "s0_serial_io_th")
            },
        ],
        errors: Vec::new(),
    }
}

#[test]
fn rt_preflight_accepts_left_and_right_px56_but_not_each_others_cpu() {
    for (identity, serial_cpu) in [("left_arm", 4), ("right_arm", 5)] {
        let profile = H2RtProfile::for_hardware_id(identity).unwrap();
        assert_eq!(profile.serial_cpu, serial_cpu);
        validate_s0_rt_snapshot(profile, &px56_snapshot(serial_cpu)).unwrap();
        assert!(validate_s0_rt_snapshot(profile, &px56_snapshot(9 - serial_cpu)).is_err());
    }
    for unknown in ["arm", "left", "right", "", "LEFT_ARM"] {
        assert!(H2RtProfile::for_hardware_id(unknown).is_err());
    }
}

#[test]
fn rt_preflight_rejects_serial_policy_priority_affinity_and_identity_errors() {
    let profile = H2RtProfile::for_hardware_id("left_arm").unwrap();
    let baseline = px56_snapshot(4);
    let serial = baseline.threads[2].clone();
    let mut variants = Vec::new();
    for policy in [libc::SCHED_OTHER, libc::SCHED_RR, libc::SCHED_BATCH] {
        variants.push(ThreadRtSnapshot {
            policy: Some(policy),
            ..serial.clone()
        });
    }
    for priority in [0, 69, 71, 99] {
        variants.push(ThreadRtSnapshot {
            priority: Some(priority),
            ..serial.clone()
        });
    }
    for cpus in [vec![], vec![0], vec![5], vec![4, 5], vec![4, 4]] {
        variants.push(ThreadRtSnapshot {
            cpus: Some(cpus),
            ..serial.clone()
        });
    }
    variants.push(ThreadRtSnapshot {
        comm: Some("s0_serial_io_thread".to_owned()),
        ..serial
    });
    for invalid in variants {
        let mut snapshot = baseline.clone();
        snapshot.threads[2] = invalid;
        assert!(
            validate_s0_rt_snapshot(profile, &snapshot).is_err(),
            "{snapshot:?}"
        );
    }
    let mut duplicate = baseline.clone();
    duplicate.threads[1] = ThreadRtSnapshot {
        tid: 701,
        ..baseline.threads[2].clone()
    };
    assert!(validate_s0_rt_snapshot(profile, &duplicate).is_err());
    let mut missing = baseline;
    missing.tids_before.pop();
    missing.tids_after.pop();
    missing.threads.pop();
    validate_ordinary_rt_snapshot(&missing).unwrap();
    assert!(validate_s0_rt_snapshot(profile, &missing).is_err());
}

#[test]
fn rt_preflight_rejects_missing_fields_even_without_reported_reader_error() {
    let profile = H2RtProfile::for_hardware_id("left_arm").unwrap();
    for thread in 0..3 {
        for field in 0..4 {
            let mut snapshot = px56_snapshot(4);
            let row = &mut snapshot.threads[thread];
            match field {
                0 => row.comm = None,
                1 => row.policy = None,
                2 => row.priority = None,
                _ => row.cpus = None,
            }
            assert!(
                validate_s0_rt_snapshot(profile, &snapshot).is_err(),
                "{snapshot:?}"
            );
        }
    }
}

#[test]
fn rt_preflight_rejects_nonserial_rt_threads_and_inexact_ordinary_masks() {
    let profile = H2RtProfile::for_hardware_id("left_arm").unwrap();
    for index in [0, 1] {
        for (policy, priority, cpus) in [
            (libc::SCHED_FIFO, 70, vec![0, 1, 2, 3]),
            (libc::SCHED_RR, 1, vec![0, 1, 2, 3]),
            (libc::SCHED_OTHER, 1, vec![0, 1, 2, 3]),
            (libc::SCHED_OTHER, 0, vec![0, 1, 2, 3, 4]),
            (libc::SCHED_OTHER, 0, vec![0, 1, 2]),
            (libc::SCHED_OTHER, 0, vec![]),
        ] {
            let mut snapshot = px56_snapshot(4);
            snapshot.threads[index].policy = Some(policy);
            snapshot.threads[index].priority = Some(priority);
            snapshot.threads[index].cpus = Some(cpus);
            assert!(
                validate_s0_rt_snapshot(profile, &snapshot).is_err(),
                "{snapshot:?}"
            );
        }
    }
}

#[test]
fn rt_preflight_rejects_incomplete_changed_and_duplicate_thread_census() {
    let profile = H2RtProfile::for_hardware_id("left_arm").unwrap();
    let mut invalid = Vec::new();
    for field in 0..8 {
        let mut snapshot = px56_snapshot(4);
        match field {
            0 => snapshot.tids_after.push(703),
            1 => {
                snapshot.tids_before.pop();
            }
            2 => {
                snapshot.threads.pop();
            }
            3 => snapshot.threads.push(snapshot.threads[0].clone()),
            4 => {
                snapshot.pid = 699;
            }
            5 => {
                snapshot.tids_before.push(700);
                snapshot.tids_after.push(700);
            }
            6 => snapshot.errors.push("读取 TID 时 ESRCH".to_owned()),
            _ => {
                snapshot.tids_before.clear();
                snapshot.tids_after.clear();
                snapshot.threads.clear();
            }
        }
        invalid.push(snapshot);
    }
    for snapshot in invalid {
        assert!(
            validate_s0_rt_snapshot(profile, &snapshot).is_err(),
            "{snapshot:?}"
        );
    }
}

#[test]
fn rt_preflight_applies_ordinary_rules_to_every_tool_and_gateway_thread() {
    let mut ordinary = px56_snapshot(4);
    ordinary.threads[2] = ordinary_thread(702, "tokio-runtime-w");
    validate_ordinary_rt_snapshot(&ordinary).unwrap();
    for index in 0..ordinary.threads.len() {
        let mut invalid = ordinary.clone();
        invalid.threads[index].cpus = Some(vec![0, 1, 2, 3, 4]);
        assert!(validate_ordinary_rt_snapshot(&invalid).is_err());
        invalid = ordinary.clone();
        invalid.threads[index].policy = Some(libc::SCHED_FIFO);
        invalid.threads[index].priority = Some(70);
        assert!(validate_ordinary_rt_snapshot(&invalid).is_err());
    }
    assert!(validate_ordinary_rt_snapshot(&px56_snapshot(4)).is_err());
}

#[test]
fn rt_preflight_reads_missing_process_as_failure_without_fabricating_defaults() {
    let snapshot = read_s0_threads(u32::MAX);
    assert_eq!(snapshot.pid, u32::MAX);
    assert_ne!(snapshot.errors, Vec::<String>::new());
    assert!(snapshot.threads.is_empty());
    assert!(validate_ordinary_rt_snapshot(&snapshot).is_err());
}

#[test]
fn rt_preflight_assembles_real_s0_commands_and_preserves_software_probe() {
    let temp = RtDirectory::new();
    let case = super::harness::Case::new(temp.path(), temp.path(), "rt_args", 6).unwrap();
    for (hardware, physical, extra) in [
        (
            "left_arm",
            true,
            vec!["--serial-fifo", "70", "--cpu", "4", "--strict-rt"],
        ),
        (
            "right_arm",
            true,
            vec!["--serial-fifo", "70", "--cpu", "5", "--strict-rt"],
        ),
        ("arm", false, vec![]),
    ] {
        let command = s0_command(&case, hardware, physical).unwrap();
        let actual: Vec<String> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let mut expected = vec![
            "--config".to_owned(),
            case.config.display().to_string(),
            "--hardware-id".to_owned(),
            hardware.to_owned(),
            "--latch-path".to_owned(),
            case.latch.display().to_string(),
            "--rx-trust-dir".to_owned(),
            case.trust.display().to_string(),
            "--notify-mode".to_owned(),
            "standalone".to_owned(),
        ];
        expected.extend(extra.into_iter().map(str::to_owned));
        assert_eq!(actual, expected);
    }
    assert!(s0_command(&case, "arm", true).is_err());
}

#[test]
fn rt_preflight_reads_real_child_affinity_without_changing_parent() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn allowed_cpus() -> Vec<usize> {
        // SAFETY：完整初始化的 cpu_set_t，pid=0 只读取当前测试线程。
        let mut mask: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&mask), &raw mut mask) },
            0
        );
        (0..libc::CPU_SETSIZE as usize)
            .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &mask) })
            .collect()
    }
    let parent_before = allowed_cpus();
    let selected = *parent_before.first().expect("测试线程必须至少允许一个 CPU");
    // SAFETY：掩码在父进程准备，只选择其实际允许的 CPU。
    let mut mask: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe { libc::CPU_SET(selected, &mut mask) };
    let mut command = Command::new("/bin/cat");
    command.stdin(Stdio::piped()).stdout(Stdio::piped());
    // SAFETY：子端只做无分配的调度系统调用；不接触测试父线程或机器人进程。
    unsafe {
        command.pre_exec(move || {
            let param = libc::sched_param { sched_priority: 0 };
            if libc::sched_setscheduler(0, libc::SCHED_OTHER, &raw const param) != 0
                || libc::sched_setaffinity(0, std::mem::size_of_val(&mask), &raw const mask) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = ChildGuard(command.spawn().unwrap());
    child
        .0
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"ready\n")
        .unwrap();
    let mut ready = String::new();
    BufReader::new(child.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready, "ready\n");
    let actual = read_s0_threads(child.0.id());
    assert!(actual.errors.is_empty(), "{actual:?}");
    assert_eq!(actual.tids_before, vec![child.0.id()]);
    assert_eq!(actual.tids_after, actual.tids_before);
    assert_eq!(actual.threads.len(), 1);
    let thread = &actual.threads[0];
    assert_eq!(thread.comm.as_deref(), Some("cat"));
    assert_eq!(thread.policy, Some(libc::SCHED_OTHER));
    assert_eq!(thread.priority, Some(0));
    assert_eq!(thread.cpus.as_deref(), Some([selected].as_slice()));
    assert_eq!(allowed_cpus(), parent_before);
}

#[test]
fn rt_preflight_persists_all_three_process_failures_before_rejecting_run() {
    let temp = RtDirectory::new();
    let profile = H2RtProfile::for_hardware_id("left_arm").unwrap();
    let result = super::sla::rt_preflight(temp.path(), profile, u32::MAX, Some(u32::MAX - 1), None);
    assert!(result.is_err());
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("rt_preflight.json")).unwrap())
            .unwrap();
    assert_eq!(report["passed"], false);
    assert_eq!(report["profile"]["serial_cpu"], 4);
    assert!(report["finished_ns"].as_u64().unwrap() >= report["started_ns"].as_u64().unwrap());
    let processes = report["processes"].as_array().unwrap();
    assert_eq!(processes.len(), 3);
    for role in ["tool", "s0", "gateway"] {
        let process = processes
            .iter()
            .find(|process| process["role"] == role)
            .unwrap();
        assert_ne!(process["validation_error"].as_str().unwrap(), "");
        assert_ne!(
            process["snapshot"]["errors"].as_array().unwrap().as_slice(),
            [] as [serde_json::Value; 0]
        );
    }
}

#[test]
fn rt_preflight_cannot_accept_when_evidence_cannot_be_written() {
    let temp = RtDirectory::new();
    let blocked = temp.path().join("ordinary_file");
    std::fs::write(&blocked, b"file, not directory").unwrap();
    let profile = H2RtProfile::for_hardware_id("right_arm").unwrap();
    assert!(super::sla::rt_preflight(&blocked, profile, u32::MAX, None, None).is_err());
    assert_eq!(std::fs::read(blocked).unwrap(), b"file, not directory");
}

#[test]
fn sample_boundary_reader_rejects_stale_late_expired_and_released_idle_receipts() {
    use super::sla::sample_boundary_ready;
    use robot_s0::comm_fault_trace::{BoundarySnapshot, SampleFailure, SampleMode, SampleStatus};
    let ready = BoundarySnapshot {
        request_seq: 81,
        request_mode: Some(SampleMode::ArmIdle),
        request_deadline_ns: 1000,
        receipt_seq: 81,
        mode: Some(SampleMode::ArmIdle),
        status: SampleStatus::Ready,
        ready_ns: 100,
        idle_deadline_ns: 1100,
        armed_seq: 81,
        gate_active: true,
        gate_release: None,
        failure_seq: 0,
        failure: None,
    };
    assert_eq!(
        sample_boundary_ready(81, SampleMode::ArmIdle, &ready, 1099),
        Ok(true)
    );
    assert!(sample_boundary_ready(81, SampleMode::ArmIdle, &ready, 1100).is_err());
    for broken in [
        BoundarySnapshot {
            request_seq: 82,
            ..ready
        },
        BoundarySnapshot {
            receipt_seq: 80,
            ..ready
        },
        BoundarySnapshot {
            request_mode: Some(SampleMode::ArmInflight),
            ..ready
        },
        BoundarySnapshot {
            mode: Some(SampleMode::ArmInflight),
            ..ready
        },
        BoundarySnapshot {
            ready_ns: 1000,
            ..ready
        },
        BoundarySnapshot {
            gate_active: false,
            ..ready
        },
        BoundarySnapshot {
            armed_seq: 80,
            ..ready
        },
        BoundarySnapshot {
            failure_seq: 80,
            failure: Some(SampleFailure::IdleExpired),
            ..ready
        },
    ] {
        assert_ne!(
            sample_boundary_ready(81, SampleMode::ArmIdle, &broken, 500),
            Ok(true),
            "错误回执不能被接纳：{broken:?}"
        );
    }
    let pending = BoundarySnapshot {
        receipt_seq: 80,
        status: SampleStatus::Pending,
        armed_seq: 0,
        gate_active: false,
        ..ready
    };
    assert_eq!(
        sample_boundary_ready(81, SampleMode::ArmIdle, &pending, 999),
        Ok(false)
    );
    assert!(sample_boundary_ready(81, SampleMode::ArmIdle, &pending, 1000).is_err());
    let disarmed = BoundarySnapshot {
        request_mode: Some(SampleMode::Disarm),
        mode: Some(SampleMode::Disarm),
        status: SampleStatus::Disarmed,
        armed_seq: 0,
        gate_active: false,
        failure_seq: 80,
        failure: Some(SampleFailure::IdleExpired),
        ..ready
    };
    assert_eq!(
        sample_boundary_ready(81, SampleMode::Disarm, &disarmed, 5000),
        Ok(true),
        "及时完成的 disarm 可以晚读，首错留给样本拒绝，不阻独立收尾"
    );
    let late = BoundarySnapshot {
        ready_ns: 1000,
        ..disarmed
    };
    assert!(sample_boundary_ready(81, SampleMode::Disarm, &late, 5000).is_err());
}

#[test]
fn sample_poll_never_pairs_old_boundary_with_new_command_confirmation() {
    use super::sla::poll_sample_request;
    use robot_s0::comm_fault_trace::{SampleMode, TracePage};
    use std::sync::atomic::{AtomicU64, Ordering};

    // 在读确认字的接缝发布真实请求：旧顺序先缓存空页，随后会用新确认误判拒绝。
    let directory = RtDirectory::new();
    let path = directory.path().join("trace");
    let writer = TracePage::create(&path).unwrap();
    let reader = TracePage::open(&path).unwrap();
    let applied = AtomicU64::new(0);
    let mut last = None;
    let result = poll_sample_request(
        81,
        SampleMode::ArmInflight,
        &mut last,
        || {
            writer
                .request_boundary(81, SampleMode::ArmInflight, 100)
                .unwrap();
            applied.store(81, Ordering::Release);
            Ok(applied.load(Ordering::Acquire))
        },
        || reader.boundary_snapshot(),
        || 200,
    );
    assert_eq!(result, Ok(false), "已接收而未 Ready 的请求应继续等待");
    let snapshot = last.unwrap();
    assert_eq!(snapshot.request_seq, 81);
    assert_ne!(snapshot.receipt_seq, 81);

    // control 真实拒绝覆盖未完成请求后，确认字虽前进，也不得继续等待或误放行。
    assert!(
        writer
            .request_boundary(82, SampleMode::ArmInflight, 300)
            .is_err()
    );
    applied.store(82, Ordering::Release);
    let result = poll_sample_request(
        82,
        SampleMode::ArmInflight,
        &mut last,
        || Ok(applied.load(Ordering::Acquire)),
        || reader.boundary_snapshot(),
        || 400,
    );
    assert!(result.unwrap_err().contains("拒绝采样请求"));
    let rejected = last.unwrap();
    assert_eq!(rejected.request_seq, 81);
    assert_eq!(rejected.failure_seq, 82);
    assert!(rejected.failure.is_some());
}

#[test]
fn sample_poll_accepts_current_ready_without_requiring_applied_and_waits_on_no_snapshot() {
    use super::sla::poll_sample_request;
    use robot_s0::comm_fault_trace::{BoundarySnapshot, SampleMode, SampleStatus};

    // 回执可以在本轮读取旧确认字之后完成，不能额外要求两者同时已更新。
    let ready = BoundarySnapshot {
        request_seq: 81,
        request_mode: Some(SampleMode::ArmInflight),
        request_deadline_ns: 1000,
        receipt_seq: 81,
        mode: Some(SampleMode::ArmInflight),
        status: SampleStatus::Ready,
        ready_ns: 100,
        idle_deadline_ns: 0,
        armed_seq: 81,
        gate_active: false,
        gate_release: None,
        failure_seq: 0,
        failure: None,
    };
    let mut last = None;
    assert_eq!(
        poll_sample_request(
            81,
            SampleMode::ArmInflight,
            &mut last,
            || Ok(80),
            || Some(ready),
            || 200,
        ),
        Ok(true)
    );
    last = None;
    assert_eq!(
        poll_sample_request(
            81,
            SampleMode::ArmInflight,
            &mut last,
            || Ok(81),
            || None,
            || 200,
        ),
        Ok(false)
    );
    assert!(
        last.is_none(),
        "Writing/Applying 暂无一致快照不能拼造拒绝证据"
    );
}

#[test]
fn sla_parser_rejects_unknown_cas_counts_before_statistics() {
    for external in [false, true] {
        for kind in [
            TraceKind::PrimitiveCommitted,
            TraceKind::NormalAcquire,
            TraceKind::NormalRelease,
            TraceKind::SerialClaim,
            TraceKind::IpcClaim,
        ] {
            let mut events = valid_events(external, true);
            one(&mut events, kind).a = u64::MAX;
            let error = analyze(&events, external).expect_err("未知计数不能补零或进入分布");
            assert!(error.contains("CAS 次数"), "{kind:?}: {error}");
        }
    }
}
