//! post-E 独立验收：由整合 / QA 维护，直接调用生产契约，避免复制实现充当被测对象。
//!
//! 本模块只覆盖已落地工作包；完整候选仍以 Spec 的全部 AC 和最终执行记录为准。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use robot_core::executor_ctrl::{
    AdapterErr, AdapterNormalReq, AdapterResp, EStopOutcome, ExecutorCtrlClient, NormalReqStatus,
    OutputSafetyState, SerialEmergencyCtrl,
};
use robot_core::serial_services::{CommErrorKind, NormalAbortTarget};

use crate::hard_rt_ctrl::{HardRtCtrlClient, HardRtCtrlInner};

/// 固定 wire 字节是旧客户端兼容性证据，不能只测编解码互逆。
#[test]
fn adapter_error_all_bytes_preserve_wire_contract() {
    let legacy = [
        AdapterErr::Other,
        AdapterErr::NotConnected,
        AdapterErr::Unsupported,
        AdapterErr::HardwareFault,
        AdapterErr::PreemptedByEmergency,
        AdapterErr::SafetyLatched,
        AdapterErr::AbortedByTimeout,
        AdapterErr::AuthorityRevoked,
    ];
    for (raw, expected) in (0_u8..=7).zip(legacy) {
        assert_eq!(AdapterErr::from_u8(raw), expected, "旧协议字节 {raw}");
        assert_eq!(expected.to_u8(), raw, "旧协议编码 {raw}");
    }

    let comm = [
        CommErrorKind::Timeout,
        CommErrorKind::Disconnected,
        CommErrorKind::IoRead,
        CommErrorKind::IoWrite,
        CommErrorKind::OpenFailed,
        CommErrorKind::FrameCorrupt,
        CommErrorKind::ResponseIdMismatch,
        CommErrorKind::PeerNoResponse,
        CommErrorKind::DiscardFailed,
        CommErrorKind::PendingDeadline,
    ];
    for (raw, kind) in (0x40_u8..=0x49).zip(comm) {
        assert_eq!(AdapterErr::from_u8(raw), AdapterErr::Comm(kind));
        assert_eq!(AdapterErr::Comm(kind).to_u8(), raw);
    }

    for raw in 0_u8..=u8::MAX {
        let expected = match raw {
            0..=7 | 0x40..=0x49 => raw,
            0x4a..=0x7f => 0x7f,
            _ => 0,
        };
        assert_eq!(AdapterErr::from_u8(raw).to_u8(), expected, "raw={raw}");
    }
}

/// 本地 deadline 不代表 RX 失步；所有未知编码都按失步处理。
#[test]
fn trust_clearing_kinds_match_the_frozen_protocol() {
    for raw in 0_u8..=u8::MAX {
        let kind = CommErrorKind::from_u8(raw);
        assert_eq!(kind.clears_trust(), !matches!(raw, 4 | 9), "raw={raw}");
    }
    assert!(!CommErrorKind::PendingDeadline.clears_trust());
    assert!(CommErrorKind::Unknown.clears_trust());
}

/// 空槽、未提交标签和合法 seq 必须是三个不相交的值域。
#[test]
fn abort_tags_reject_mixed_bits_instead_of_masking_to_a_sequence() {
    let largest_seq = (1_u64 << 40) - 1;
    assert_eq!(NormalAbortTarget::decode(0), Ok(None));
    assert_eq!(
        NormalAbortTarget::decode(1_u64 << 63),
        Ok(Some(NormalAbortTarget::Unsubmitted))
    );
    assert_eq!(NormalAbortTarget::Unsubmitted.encode(), Some(1_u64 << 63));
    for seq in [1, 2, largest_seq - 1, largest_seq] {
        assert_eq!(
            NormalAbortTarget::decode(seq),
            Ok(Some(NormalAbortTarget::Submitted { seq }))
        );
        assert_eq!(NormalAbortTarget::Submitted { seq }.encode(), Some(seq));
    }
    for seq in [0, 1_u64 << 40, 1_u64 << 63, u64::MAX] {
        assert_eq!(NormalAbortTarget::Submitted { seq }.encode(), None);
    }
    for bit in 40..64 {
        let dirty = (1_u64 << bit) | 1;
        assert!(NormalAbortTarget::decode(dirty).is_err(), "raw={dirty:#x}");
    }
    assert!(NormalAbortTarget::decode(u64::MAX).is_err());
}

/// 同一底层控制类型必须只在 `SerialRT` 收紧到 40 位。
#[test]
fn sequence_exhaustion_is_serial_only_and_preserves_hard_rt_responses() {
    let serial = Arc::new(HardRtCtrlInner::new_serial(
        crate::hard_rt_ctrl::test_authority(true),
    ));
    let serial_client = HardRtCtrlClient::new(Arc::clone(&serial));
    serial
        .seq_counter
        .store((1_u64 << 40) - 2, Ordering::Release);
    for expected in [(1_u64 << 40) - 2, (1_u64 << 40) - 1] {
        assert!(matches!(
            serial_client.request_normal(AdapterNormalReq::Enable(false)),
            NormalReqStatus::Submitted { seq } if seq == expected
        ));
        assert_eq!(
            serial.pending_normal.swap(0, Ordering::AcqRel) >> 16,
            expected
        );
    }
    assert!(matches!(
        serial_client.request_normal(AdapterNormalReq::Enable(false)),
        NormalReqStatus::Unsupported(_)
    ));
    assert_eq!(serial.pending_normal.load(Ordering::Acquire), 0);
    serial.seq_counter.store(0, Ordering::Release);
    assert!(matches!(
        serial_client.request_normal(AdapterNormalReq::Enable(false)),
        NormalReqStatus::Unsupported(_)
    ));

    let hard = Arc::new(HardRtCtrlInner::new(crate::hard_rt_ctrl::test_authority(
        false,
    )));
    let hard_client = HardRtCtrlClient::new(Arc::clone(&hard));
    hard.seq_counter.store((1_u64 << 40) - 1, Ordering::Release);
    for expected in [(1_u64 << 40) - 1, 1_u64 << 40, (1_u64 << 40) + 1] {
        assert!(matches!(
            hard_client.request_normal(AdapterNormalReq::Enable(false)),
            NormalReqStatus::Submitted { seq } if seq == expected
        ));
        assert_eq!(
            hard.pending_normal.swap(0, Ordering::AcqRel) >> 16,
            expected
        );
        // 固定协议布局由测试编码，避免生产 pack/unpack 同错仍互逆。
        hard.last_completed.store(expected << 16, Ordering::Release);
        assert_eq!(
            hard_client.poll_last_normal_resp(expected),
            Some(AdapterResp::Ok)
        );
        assert_eq!(hard_client.poll_last_normal_resp(expected + 1), None);
    }
    hard_client.abort_pending_normal(NormalAbortTarget::Unsubmitted);
    assert_eq!(hard.abort_request.load(Ordering::Acquire), 0);
    for expected in [(1_u64 << 47) - 1, 1_u64 << 47, (1_u64 << 48) - 1] {
        hard.seq_counter.store(expected, Ordering::Release);
        assert!(
            matches!(hard_client.request_normal(AdapterNormalReq::Enable(false)), NormalReqStatus::Submitted { seq } if seq == expected)
        );
        let raw = hard.pending_normal.load(Ordering::Acquire);
        assert_eq!(raw >> 16, expected, "HardRT 的 bit63 属于完整 48 位序号");
        hard_client.safety_stop();
        assert_eq!(
            hard_client.poll_last_normal_resp(expected),
            None,
            "停止请求不伪造执行退休"
        );
        let raw = hard.pending_normal.swap(0, Ordering::AcqRel);
        let original = hard.decode_pending(raw).unwrap().unwrap().action;
        assert_eq!(original.seq, expected);
        crate::serial_convergence::publish_completion(
            &hard,
            original,
            AdapterResp::Err(AdapterErr::PreemptedByEmergency),
        );
        assert_eq!(
            hard_client.poll_last_normal_resp(expected),
            Some(AdapterResp::Err(AdapterErr::PreemptedByEmergency))
        );
        assert_eq!(hard.pending_normal.load(Ordering::Acquire), 0);
    }
}

/// 同一真实 A2 word 的六安全初态、在途租约及已发布请求组合。
#[test]
fn estop_preserves_severity_and_real_inflight_lease_while_requests_merge() {
    use robot_core::control_authority::{
        AuthorityInit, ControlAuthority, InhibitSet, NormalIoIntent,
    };
    for state in 0_u8..=5 {
        for active in [false, true] {
            for already_pending in [false, true] {
                let authority = Arc::new(
                    ControlAuthority::new(AuthorityInit::new(
                        false,
                        OutputSafetyState::from_u8(state),
                        InhibitSet::EMPTY,
                    ))
                    .unwrap(),
                );
                let lease = active.then(|| {
                    authority
                        .acquire_normal_io(NormalIoIntent::PeriodicRead)
                        .unwrap()
                });
                let inner = Arc::new(HardRtCtrlInner::new_serial(authority.clone()));
                let client = HardRtCtrlClient::new(inner.clone());
                if already_pending {
                    let _ = client.estop_monotonic();
                }
                let before = authority.snapshot();
                let expected = if already_pending {
                    EStopOutcome::Merged
                } else {
                    match state {
                        0 | 1 | 3 => EStopOutcome::StopIssued,
                        2 => EStopOutcome::Merged,
                        4 | 5 => EStopOutcome::LatchOnly,
                        _ => unreachable!(),
                    }
                };
                assert_eq!(client.estop_monotonic(), expected);
                let after = authority.snapshot();
                assert!(
                    after.latch
                        && after.disabled
                        && after.serial_estop_pending
                        && after.ipc_estop_notice
                );
                assert_eq!(after.normal_io_active, active);
                assert_eq!(after.inhibits, before.inhibits);
                assert_eq!(
                    after.safety,
                    match state {
                        4 | 5 => OutputSafetyState::from_u8(state),
                        _ => OutputSafetyState::StopCommanded,
                    }
                );
                if let Some(lease) = lease {
                    assert!(authority.release_normal_io(lease).unwrap().superseded);
                }
            }
        }
    }
}

#[test]
fn normal_acquire_and_estop_have_two_distinct_valid_orders() {
    use robot_core::control_authority::NormalIoIntent;
    let before = Arc::new(HardRtCtrlInner::new_serial(
        crate::hard_rt_ctrl::test_authority(false),
    ));
    let client = HardRtCtrlClient::new(before.clone());
    assert_eq!(client.estop_monotonic(), EStopOutcome::StopIssued);
    assert!(
        before
            .authority()
            .acquire_normal_io(NormalIoIntent::PeriodicRead)
            .is_err()
    );
    assert!(!before.authority().snapshot().normal_io_active);
    let during = Arc::new(HardRtCtrlInner::new_serial(
        crate::hard_rt_ctrl::test_authority(false),
    ));
    let client = HardRtCtrlClient::new(during.clone());
    let lease = during
        .authority()
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    assert!(
        during
            .authority()
            .acquire_normal_io(NormalIoIntent::PeriodicRead)
            .is_err()
    );
    assert_eq!(client.estop_monotonic(), EStopOutcome::StopIssued);
    assert!(during.authority().snapshot().normal_io_active);
    assert!(
        during
            .authority()
            .release_normal_io(lease)
            .unwrap()
            .superseded
    );
    assert!(!during.authority().snapshot().normal_io_active);
    assert!(during.authority().snapshot().serial_estop_pending);
    assert!(during.authority().snapshot().ipc_estop_notice);
}

#[test]
fn safety_observers_borrow_only_the_injected_authority_for_both_ctrl_kinds() {
    let authority = crate::hard_rt_ctrl::test_authority(false);
    let unrelated = crate::hard_rt_ctrl::test_authority(false);
    let hard = Arc::new(HardRtCtrlInner::new(authority.clone()));
    let serial = Arc::new(HardRtCtrlInner::new_serial(authority.clone()));
    let _ = authority.tighten_safety(robot_core::control_authority::SafetyTightening::StopFailed);
    for inner in [hard, serial] {
        assert!(Arc::ptr_eq(inner.authority(), &authority));
        assert_eq!(inner.safety_state(), OutputSafetyState::StopFailed);
        assert_eq!(
            HardRtCtrlClient::new(inner).safety_state(),
            OutputSafetyState::StopFailed
        );
    }
    assert_eq!(
        unrelated.snapshot().safety,
        OutputSafetyState::TorqueOffConfirmed
    );
}

#[test]
fn diag_capacity_is_256_and_drops_newest_across_wraparound() {
    use robot_core::diag::{DiagEvent, DiagSink};

    assert_eq!(std::mem::size_of::<DiagEvent>(), 16);
    let ring = crate::diag::SerialDiagRing::new();
    for cycle in 0_u32..4 {
        for index in 0_u32..259 {
            let seq = cycle * 1000 + index;
            ring.record(DiagEvent {
                stamp_ns: u64::from(seq),
                site: 19,
                kind: 7,
                a: 31,
                b: 11,
                seq_lo: seq,
            });
        }
        assert_eq!(ring.dropped(), u64::from((cycle + 1) * 3));
        for index in 0_u32..256 {
            let event = ring.pop().expect("已接受的早期事件必须保留");
            let expected = cycle * 1000 + index;
            assert_eq!(event.stamp_ns, u64::from(expected));
            assert_eq!(event.seq_lo, expected);
            assert_eq!((event.site, event.kind, event.a, event.b), (19, 7, 31, 11));
        }
        assert!(ring.pop().is_none());
    }
}

/// 非约定的并发 safe 调用也不能制造重复消费、撕裂记录或未记账丢失。
/// 压力通过不替代 reviewer 对 `UnsafeCell` 准入与内存序的证明。
#[test]
fn diag_safe_api_accounts_for_concurrent_callers_without_torn_records() {
    use std::collections::HashSet;
    use std::sync::atomic::AtomicU32;

    use robot_core::diag::{DiagEvent, DiagSink};

    let ring = crate::diag::SerialDiagRing::new();
    let finished = AtomicU32::new(0);
    let observed = std::thread::scope(|scope| {
        for producer in 0_u32..4 {
            let ring = &ring;
            let finished = &finished;
            scope.spawn(move || {
                for index in 0_u32..1000 {
                    let seq = producer * 1000 + index;
                    ring.record(DiagEvent {
                        stamp_ns: !u64::from(seq),
                        site: 19,
                        kind: 7,
                        a: 31,
                        b: 11,
                        seq_lo: seq,
                    });
                }
                finished.fetch_add(1, Ordering::Release);
            });
        }
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let ring = &ring;
                let finished = &finished;
                scope.spawn(move || {
                    let mut events = Vec::new();
                    loop {
                        // 先观察 producer 完成，再尝试最后一次 pop；避免空读和完成之间的间隙。
                        let all_done = finished.load(Ordering::Acquire) == 4;
                        match ring.pop() {
                            Some(event) => events.push(event),
                            None if all_done => break,
                            None => std::thread::yield_now(),
                        }
                    }
                    events
                })
            })
            .collect();
        readers
            .into_iter()
            .flat_map(|reader| reader.join().expect("诊断消费者不能 panic"))
            .collect::<Vec<_>>()
    });
    assert_eq!(observed.len() as u64 + ring.dropped(), 4000);
    let mut unique = HashSet::new();
    for event in observed {
        assert!(event.seq_lo < 4000);
        assert_eq!(event.stamp_ns, !u64::from(event.seq_lo));
        assert_eq!((event.site, event.kind, event.a, event.b), (19, 7, 31, 11));
        assert!(unique.insert(event.seq_lo), "同一事件不能被消费两次");
    }
    assert!(ring.pop().is_none());
}

#[test]
fn estop_preserves_a_real_stop_failure_with_an_original_lease() {
    use robot_core::control_authority::{NormalIoIntent, SafetyTightening};
    let inner = Arc::new(HardRtCtrlInner::new_serial(
        crate::hard_rt_ctrl::test_authority(false),
    ));
    let client = HardRtCtrlClient::new(inner.clone());
    let lease = inner
        .authority()
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    let _ = inner
        .authority()
        .tighten_safety(SafetyTightening::StopFailed);
    assert_eq!(client.estop_monotonic(), EStopOutcome::LatchOnly);
    assert_eq!(inner.safety_state(), OutputSafetyState::StopFailed);
    assert!(inner.authority().snapshot().normal_io_active);
    assert!(
        inner
            .authority()
            .release_normal_io(lease)
            .unwrap()
            .superseded
    );
    assert_eq!(inner.estop_requests.load(Ordering::Acquire), 1);
}

#[test]
fn policy_cas_never_overwrites_a_concurrent_disabled_commit() {
    use crate::hard_rt_ctrl::{tighten_estop_policy, tighten_estop_policy_before_cas};
    use std::sync::atomic::AtomicU8;

    for active_policy in 1_u8..=4 {
        let policy = AtomicU8::new(active_policy);
        let mut attempts = 0;
        tighten_estop_policy_before_cas(&policy, || {
            attempts += 1;
            policy.store(0, Ordering::Release);
        });
        assert_eq!(attempts, 1);
        assert_eq!(policy.load(Ordering::Acquire), 0);
        policy.store(active_policy, Ordering::Release);
        tighten_estop_policy(&policy);
        assert_eq!(policy.load(Ordering::Acquire), 5);
        policy.store(0, Ordering::Release);
        assert_eq!(policy.load(Ordering::Acquire), 0);
    }
    for inert_policy in [0_u8, 5, 6, u8::MAX] {
        let policy = AtomicU8::new(inert_policy);
        tighten_estop_policy_before_cas(&policy, || panic!("已禁止输出，不应尝试写 CAS"));
        assert_eq!(policy.load(Ordering::Acquire), inert_policy);
    }
}

#[test]
fn each_estop_consumer_claims_only_its_own_bit_and_allows_republication() {
    let inner = Arc::new(HardRtCtrlInner::new_serial(
        crate::hard_rt_ctrl::test_authority(true),
    ));
    let client = HardRtCtrlClient::new(Arc::clone(&inner));
    assert_eq!(client.estop_monotonic(), EStopOutcome::StopIssued);
    assert!(inner.claim_serial_estop());
    assert!(!inner.claim_serial_estop());
    assert!(inner.claim_ipc_estop());
    assert!(!inner.claim_ipc_estop());
    assert_eq!(client.estop_monotonic(), EStopOutcome::Merged);
    assert!(inner.claim_ipc_estop());
    assert!(inner.claim_serial_estop());
    assert_eq!(inner.estop_requests.load(Ordering::Acquire), 2);
    assert_eq!(inner.safety_state(), OutputSafetyState::StopCommanded);
}

#[test]
fn retiring_one_abort_request_cannot_erase_a_new_request() {
    for (first, second) in [
        (
            NormalAbortTarget::Submitted { seq: 1 },
            NormalAbortTarget::Submitted { seq: 2 },
        ),
        (
            NormalAbortTarget::Unsubmitted,
            NormalAbortTarget::Submitted { seq: 1 },
        ),
        (
            NormalAbortTarget::Submitted { seq: 1 },
            NormalAbortTarget::Unsubmitted,
        ),
    ] {
        let inner = Arc::new(HardRtCtrlInner::new_serial(
            crate::hard_rt_ctrl::test_authority(true),
        ));
        let client = HardRtCtrlClient::new(Arc::clone(&inner));
        client.abort_pending_normal(first);
        let observed = inner.abort_request.load(Ordering::Acquire);
        client.abort_pending_normal(second);
        assert!(!inner.clear_abort(observed));
        assert_eq!(
            inner.abort_request.load(Ordering::Acquire),
            second.encode().unwrap()
        );
        assert!(inner.clear_abort(second.encode().unwrap()));
        assert_eq!(inner.abort_request.load(Ordering::Acquire), 0);
    }
}

#[test]
fn mailbox_generation_binds_details_and_old_ack_cannot_release_new_fault() {
    use crate::hard_rt_ctrl::FaultOrigin;
    use crate::state_machine::FaultCode;

    let inner = HardRtCtrlInner::new_serial(crate::hard_rt_ctrl::test_authority(true));
    assert!(inner.mailbox_snapshot().is_none());
    let (first_generation, first_epoch) = inner.publish_fault_with_epoch(
        OutputSafetyState::StopCommanded,
        1,
        FaultCode::CommFault,
        9,
        FaultOrigin::PendingTimeout,
    );
    assert_eq!(first_generation, Some(1));
    let first_word = 1_u64 | (7 << 32) | (9 << 40) | (2 << 48);
    assert_eq!(inner.fault_mailbox.load(Ordering::Acquire), first_word);
    let old = inner.mailbox_snapshot().unwrap();
    assert_eq!(
        (old.generation, old.code, old.detail, old.origin),
        (1, FaultCode::CommFault, 9, FaultOrigin::PendingTimeout)
    );

    let (second_generation, second_epoch) = inner.publish_fault_with_epoch(
        OutputSafetyState::StopFailed,
        4,
        FaultCode::SafetyStopFailed,
        0x89,
        FaultOrigin::Confirm,
    );
    assert_eq!(second_generation, Some(2));
    let second_word = 2_u64 | (6 << 32) | (0x89 << 40) | (4 << 48);
    assert_eq!(inner.fault_mailbox.load(Ordering::Acquire), second_word);
    inner.ack_fault(old.generation);
    assert!(inner.clear_acked_fault_pending_at(first_epoch).is_none());
    assert!(inner.clear_acked_fault_pending_at(second_epoch).is_none());
    assert_eq!(inner.inhibit_bits(), (1 << 5) | (1 << 4) | 5);
    inner.ack_fault(2);
    assert!(inner.clear_acked_fault_pending_at(second_epoch).is_some());
    assert_eq!(inner.inhibit_bits(), (1 << 5) | 5);
    assert_eq!(inner.safety_state(), OutputSafetyState::StopFailed);

    inner
        .fault_mailbox
        .store(u64::from(u32::MAX - 1) | (6 << 32), Ordering::Release);
    assert_eq!(
        inner.publish_fault(
            OutputSafetyState::StopFailed,
            4,
            FaultCode::SafetyStopFailed,
            0x89,
            FaultOrigin::Confirm
        ),
        None
    );
    assert_eq!(inner.mailbox_snapshot().unwrap().generation, u32::MAX - 1);
    assert_ne!(inner.inhibit_bits() & 4, 0);
}
