//! 模块内测试：可直接读写私有权限字并注入 CAS 前交错，证明真实错误与交错。

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Barrier};

use OutputSafetyState as S;

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

fn init(latch: bool, safety: S) -> AuthorityInit {
    AuthorityInit::new(latch, safety, InhibitSet::EMPTY)
}

fn fresh(latch: bool, safety: S) -> ControlAuthority {
    ControlAuthority::new(init(latch, safety)).unwrap()
}

fn at_epoch(latch: bool, safety: S, epoch: u64) -> ControlAuthority {
    ControlAuthority::new_with_permission_epoch(
        init(latch, safety),
        PermissionEpoch::new(epoch).unwrap(),
    )
    .unwrap()
}

fn raw(authority: &ControlAuthority) -> u64 {
    authority.word.load(Ordering::Acquire)
}

fn rewrite(authority: &ControlAuthority, change: impl FnOnce(Word) -> Word) {
    let word = change(authority.load());
    authority.word.store(word.raw(), Ordering::Release);
}

fn epoch(authority: &ControlAuthority) -> u64 {
    authority.snapshot().epoch.get()
}

fn phase(authority: &ControlAuthority) -> Phase {
    authority.snapshot().phase.unwrap()
}

fn install_hook(
    authority: &ControlAuthority,
    hook: impl FnMut(&ControlAuthority) + Send + 'static,
) {
    *authority.before_cas.lock().unwrap() = Some(Box::new(hook));
}

fn once(
    mut hook: impl FnMut(&ControlAuthority) + Send + 'static,
) -> impl FnMut(&ControlAuthority) + Send + 'static {
    let mut fired = false;
    move |authority| {
        if !fired {
            fired = true;
            hook(authority);
        }
    }
}

/// 走到 Succeeded + TorqueOn 已发布的 Enable 事务。
fn succeeded_enable(authority: &ControlAuthority) -> EnableOwner {
    let (owner, view) = authority.reserve_enable().unwrap();
    let execution = authority.start_enable(view).unwrap();
    let finish = authority
        .finish_enable(execution, EnableResult::Succeeded)
        .unwrap();
    assert!(finish.torque_on_published);
    assert_eq!(authority.snapshot().safety, S::TorqueOn);
    owner
}

// ---------------------------------------------------------------------------
// 类型合同与位布局
// ---------------------------------------------------------------------------

#[test]
fn authority_and_tokens_are_send_sync_and_snapshot_is_copy() {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_copy<T: Copy>() {}
    assert_send_sync::<ControlAuthority>();
    assert_send_sync::<EnableOwner>();
    assert_send_sync::<EnableExecution>();
    assert_send_sync::<ClearPendingOwner>();
    assert_send_sync::<ClearFinalizingOwner>();
    assert_send_sync::<RepairPermit>();
    assert_send_sync::<CleanupPermit>();
    assert_send_sync::<InhibitReleasePermit>();
    assert_send_sync::<SafetyObservation>();
    assert_send_sync::<NormalIoLease>();
    assert_copy::<AuthoritySnapshot>();
    assert_copy::<EnableStartView>();
    assert_copy::<EnableExecutionView>();
    assert_copy::<Refusal>();
}

#[test]
fn word_layout_fields_are_disjoint_and_round_trip() {
    let fields = [
        word::SAFETY_MASK,
        SERIAL_ESTOP_PENDING,
        IPC_ESTOP_NOTICE,
        NORMAL_IO_ACTIVE,
        LATCH,
        word::PHASE_MASK,
        DISABLED,
        SHUTDOWN,
        EXHAUSTED,
        word::INHIBIT_MASK,
        word::EPOCH_MAX << word::EPOCH_SHIFT,
    ];
    let mut union = 0;
    for (index, field) in fields.iter().enumerate() {
        assert_eq!(union & field, 0, "字段 {index} 与前面的字段重叠");
        union |= field;
    }
    assert_eq!(union, u64::MAX, "64 位必须全部被分配");
    assert_eq!(word::EPOCH_MAX, (1 << 40) - 1);
    assert_eq!(PermissionEpoch::MAX.get(), word::EPOCH_MAX);

    let mut word = Word::new(0);
    for state in [
        S::TorqueUnknown,
        S::TorqueOn,
        S::StopCommanded,
        S::TorqueOffConfirmed,
        S::StopFailed,
        S::AdapterUncertain,
    ] {
        word = word.with_safety(state);
        assert_eq!(word.safety(), state);
    }
    for bits in 0..7 {
        let phase = Phase::from_bits(bits).unwrap();
        assert_eq!(phase.bits(), bits);
        word = word.with_phase(phase);
        assert_eq!(word.phase(), Some(phase));
        assert_eq!(word.phase_bits(), bits);
    }
    assert_eq!(Phase::from_bits(7), None);
    assert_eq!(Word::new(7 << word::PHASE_SHIFT).phase(), None);
    for reason in InhibitReason::ALL {
        let set = InhibitSet::EMPTY.with(reason);
        assert_eq!(word.with_inhibits(set).inhibits(), set);
        assert_eq!(
            word.with_inhibits(set)
                .with_inhibits(InhibitSet::EMPTY)
                .inhibits(),
            InhibitSet::EMPTY
        );
    }
    assert_eq!(word.with_inhibits(InhibitSet::ALL).inhibits().bits(), 0x3f);
    assert_eq!(InhibitSet::from_bits(0x40), None);
    assert_eq!(InhibitSet::from_bits(0x3f), Some(InhibitSet::ALL));
    for epoch in [1, 2, 0xdead_beef, word::EPOCH_MAX] {
        assert_eq!(word.with_epoch(epoch).epoch(), epoch);
        assert_eq!(
            word.with_epoch(epoch).raw() & 0x00ff_ffff,
            word.raw() & 0x00ff_ffff
        );
    }
    assert_eq!(Word::new(0).with_epoch(word::EPOCH_MAX).next_epoch(), None);
    assert_eq!(
        Word::new(0).with_epoch(word::EPOCH_MAX - 1).next_epoch(),
        Some(word::EPOCH_MAX)
    );
    let (saturated, advance) = Word::new(0)
        .with_epoch(word::EPOCH_MAX)
        .advance_or_exhaust();
    assert_eq!(saturated.epoch(), word::EPOCH_MAX);
    assert!(saturated.has(EXHAUSTED) && saturated.has(DISABLED));
    assert_eq!(
        advance,
        Advance {
            advanced: false,
            exhausted: true
        }
    );
}

// ---------------------------------------------------------------------------
// 构造与 domain
// ---------------------------------------------------------------------------

#[test]
fn construction_uses_fixed_initial_state_and_given_parameters() {
    let inhibits = InhibitSet::EMPTY.with(InhibitReason::Boot);
    let authority =
        ControlAuthority::new(AuthorityInit::new(true, S::StopCommanded, inhibits)).unwrap();
    let snapshot = authority.snapshot();
    assert_eq!(snapshot.epoch, PermissionEpoch::INITIAL);
    assert_eq!(snapshot.phase, Some(Phase::Idle));
    assert!(snapshot.disabled && snapshot.latch);
    assert!(!snapshot.shutdown && !snapshot.exhausted);
    assert!(!snapshot.serial_estop_pending && !snapshot.ipc_estop_notice);
    assert!(!snapshot.normal_io_active);
    assert_eq!(snapshot.safety, S::StopCommanded);
    assert_eq!(snapshot.inhibits, inhibits);
    assert_eq!(snapshot.domain, authority.domain());

    let later = fresh(false, S::TorqueUnknown);
    assert!(later.domain() > authority.domain());
    assert!(!later.snapshot().latch);
    assert_eq!(later.snapshot().inhibits, InhibitSet::EMPTY);
}

#[test]
fn domain_allocation_never_wraps_and_fails_when_exhausted() {
    let counter = AtomicU64::new(u64::MAX - 1);
    let first = allocate_domain(&counter).unwrap();
    assert_eq!(first.get(), u64::MAX - 1);
    assert_eq!(counter.load(Ordering::Acquire), u64::MAX);
    for _ in 0..3 {
        assert_eq!(allocate_domain(&counter), None);
        assert_eq!(
            counter.load(Ordering::Acquire),
            u64::MAX,
            "耗尽后不得回绕或变化"
        );
    }
    let zero = AtomicU64::new(0);
    assert_eq!(allocate_domain(&zero), None, "0 永不作为 domain 分配");
    assert_eq!(zero.load(Ordering::Acquire), 0);

    let exhausted = AtomicU64::new(u64::MAX);
    assert_eq!(
        ControlAuthority::with_counter(
            &exhausted,
            init(false, S::TorqueUnknown),
            PermissionEpoch::INITIAL
        )
        .err(),
        Some(AuthorityInitError::DomainExhausted)
    );
    let live = AtomicU64::new(7);
    let authority = ControlAuthority::with_counter(
        &live,
        init(false, S::TorqueUnknown),
        PermissionEpoch::INITIAL,
    )
    .unwrap();
    assert_eq!(authority.domain().get(), 7);
    let _ = authority.estop();
    assert!(
        authority.snapshot().latch,
        "已存在权威不受后续 domain 耗尽影响"
    );
}

#[test]
fn initial_epoch_must_be_within_range() {
    assert_eq!(PermissionEpoch::new(0), None);
    assert_eq!(PermissionEpoch::new(word::EPOCH_MAX + 1), None);
    assert_eq!(
        PermissionEpoch::new(word::EPOCH_MAX),
        Some(PermissionEpoch::MAX)
    );
    assert_eq!(
        ControlAuthority::with_counter(
            &AtomicU64::new(1),
            init(false, S::TorqueUnknown),
            PermissionEpoch(0)
        )
        .err(),
        Some(AuthorityInitError::InvalidPermissionEpoch(0))
    );
    assert_eq!(
        epoch(&at_epoch(false, S::TorqueUnknown, word::EPOCH_MAX)),
        word::EPOCH_MAX
    );
}

#[test]
fn corrupted_phase_bits_fail_closed_but_tightening_still_works() {
    let authority = fresh(false, S::TorqueOffConfirmed);
    rewrite(&authority, |word| Word::new(word.raw() | word::PHASE_MASK));
    assert_eq!(authority.snapshot().phase, None);
    assert_eq!(
        authority.reserve_enable().err(),
        Some(Refusal::CorruptedPhase)
    );
    assert_eq!(
        authority.reserve_clear().err(),
        Some(Refusal::CorruptedPhase)
    );
    assert_eq!(
        authority
            .acquire_normal_io(NormalIoIntent::PeriodicRead)
            .err(),
        Some(Refusal::CorruptedPhase)
    );
    let observation = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
    assert!(matches!(
        authority.complete_safety_observation(observation),
        Err(SafetyObservationError::Rejected(Refusal::CorruptedPhase))
    ));
    let report = authority.estop();
    assert_eq!(report.outcome, EStopOutcome::StopIssued);
    assert_eq!(epoch(&authority), 2);
    assert_eq!(
        authority.snapshot().phase,
        None,
        "收紧不改 Phase，即便它已损坏"
    );
}

// ---------------------------------------------------------------------------
// 安全撤销
// ---------------------------------------------------------------------------

#[test]
fn estop_follows_the_transition_table_and_preserves_phase_and_io() {
    let cases = [
        (S::TorqueUnknown, S::StopCommanded, EStopOutcome::StopIssued),
        (S::TorqueOn, S::StopCommanded, EStopOutcome::StopIssued),
        (
            S::TorqueOffConfirmed,
            S::StopCommanded,
            EStopOutcome::StopIssued,
        ),
        (S::StopCommanded, S::StopCommanded, EStopOutcome::Merged),
        (S::StopFailed, S::StopFailed, EStopOutcome::LatchOnly),
        (
            S::AdapterUncertain,
            S::AdapterUncertain,
            EStopOutcome::LatchOnly,
        ),
    ];
    for (previous, expected, outcome) in cases {
        let authority = fresh(false, previous);
        let (owner, _view) = match previous {
            S::TorqueUnknown | S::TorqueOn | S::TorqueOffConfirmed | S::StopCommanded => {
                let reserved = authority.reserve_enable().unwrap();
                (Some(reserved.0), Some(reserved.1))
            }
            S::StopFailed | S::AdapterUncertain => (None, None),
        };
        let lease = authority
            .acquire_normal_io(NormalIoIntent::PeriodicRead)
            .unwrap();
        let before = raw(&authority);
        let report = authority.estop();
        assert_eq!(report.outcome, outcome, "previous={previous:?}");
        assert!(report.normal_io_active);
        assert!(report.tightened.advanced && !report.tightened.exhausted);
        let snapshot = authority.snapshot();
        assert_eq!(snapshot.safety, expected);
        assert!(snapshot.latch && snapshot.disabled);
        assert!(snapshot.serial_estop_pending && snapshot.ipc_estop_notice);
        assert!(snapshot.normal_io_active, "EStop 不清在途 IO 位");
        assert_eq!(snapshot.epoch.get(), (before >> word::EPOCH_SHIFT) + 1);
        assert_eq!(
            snapshot.phase,
            Some(if owner.is_some() {
                Phase::EnableQueued
            } else {
                Phase::Idle
            })
        );
        // 第二次 EStop：pending 仍在 → Merged，且再推进 epoch。
        let merged = authority.estop();
        assert_eq!(merged.outcome, EStopOutcome::Merged);
        assert_eq!(epoch(&authority), (before >> word::EPOCH_SHIFT) + 2);
        let released = authority.release_normal_io(lease).unwrap();
        assert!(released.superseded);
        if let Some(owner) = owner {
            let retired = authority.retire_enable(owner).unwrap();
            assert_eq!(retired.retired_from, Phase::EnableQueued);
            assert!(retired.superseded);
        }
    }
}

#[test]
fn disable_only_disables_and_shutdown_is_irreversible() {
    let authority = fresh(false, S::TorqueOffConfirmed);
    let (owner, _view) = authority.reserve_enable().unwrap();
    let before = authority.snapshot();
    let tightened = authority.disable();
    assert_eq!(tightened.epoch.get(), before.epoch.get() + 1);
    let after = authority.snapshot();
    assert!(after.disabled && !after.latch);
    assert!(
        !after.serial_estop_pending && !after.ipc_estop_notice,
        "Disable 不创造 EStop"
    );
    assert_eq!(after.safety, before.safety);
    assert_eq!(after.phase, Some(Phase::EnableQueued));

    let tightened = authority.shutdown();
    assert_eq!(tightened.epoch.get(), before.epoch.get() + 2);
    assert!(authority.snapshot().shutdown);
    let retired = authority.retire_enable(owner).unwrap();
    assert!(retired.superseded);
    assert!(authority.snapshot().shutdown, "退休不解除 Shutdown");
    assert_eq!(authority.reserve_enable().err(), Some(Refusal::Shutdown));
    assert_eq!(authority.reserve_clear().err(), Some(Refusal::Shutdown));
    assert_eq!(
        authority.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::Shutdown)
    );
    assert!(
        authority
            .acquire_normal_io(NormalIoIntent::NonEnergizingControl(
                NonEnergizingControl::Disable
            ))
            .is_ok(),
        "非上扭控制只保留基础门"
    );
}

#[test]
fn typed_safety_tightening_is_ordered_and_always_disables() {
    let all = [
        S::TorqueUnknown,
        S::TorqueOn,
        S::StopCommanded,
        S::TorqueOffConfirmed,
        S::StopFailed,
        S::AdapterUncertain,
    ];
    for previous in all {
        for tightening in [
            SafetyTightening::StopCommanded,
            SafetyTightening::AdapterUncertain,
            SafetyTightening::StopFailed,
        ] {
            let authority = fresh(false, previous);
            let (owner, _view) = authority.reserve_enable_or_none();
            rewrite(&authority, |word| word.clear(DISABLED));
            let report = authority.tighten_safety(tightening);
            let expected = match (tightening, previous) {
                (SafetyTightening::StopFailed, _)
                | (SafetyTightening::AdapterUncertain, S::StopFailed) => S::StopFailed,
                (SafetyTightening::AdapterUncertain, _) => S::AdapterUncertain,
                (SafetyTightening::StopCommanded, S::StopFailed | S::AdapterUncertain) => previous,
                (SafetyTightening::StopCommanded, _) => S::StopCommanded,
            };
            assert_eq!(report.previous, previous);
            assert_eq!(
                report.safety, expected,
                "{tightening:?} 作用于 {previous:?}"
            );
            assert_eq!(report.tightened.epoch.get(), 2 + u64::from(owner.is_some()));
            let snapshot = authority.snapshot();
            assert_eq!(snapshot.safety, expected);
            assert!(snapshot.disabled);
            assert!(!snapshot.latch && !snapshot.serial_estop_pending);
            assert_eq!(
                snapshot.phase,
                Some(if owner.is_some() {
                    Phase::EnableQueued
                } else {
                    Phase::Idle
                })
            );
            if let Some(owner) = owner {
                assert!(authority.retire_enable(owner).unwrap().superseded);
            }
        }
    }
}

impl ControlAuthority {
    /// 测试便利：恢复门允许时预约 Enable，否则返回 None（不改位）。
    fn reserve_enable_or_none(&self) -> (Option<EnableOwner>, Option<EnableStartView>) {
        match self.reserve_enable() {
            Ok((owner, view)) => (Some(owner), Some(view)),
            Err(_) => (None, None),
        }
    }
}

#[test]
fn inhibit_tightening_advances_epoch_and_claims_do_not() {
    let authority = fresh(false, S::TorqueOffConfirmed);
    let mut expected = InhibitSet::EMPTY;
    for (index, reason) in InhibitReason::ALL.into_iter().enumerate() {
        let report = authority.tighten_inhibit(reason);
        expected = expected.with(reason);
        assert_eq!(report.epoch.get(), 2 + index as u64);
        assert_eq!(authority.snapshot().inhibits, expected);
    }
    let _ = authority.tighten_inhibit(InhibitReason::Boot);
    assert_eq!(epoch(&authority), 8, "重复收紧同一原因仍是撤销事件");
    assert_eq!(authority.snapshot().inhibits, InhibitSet::ALL);

    let _ = authority.estop();
    let before = raw(&authority);
    assert!(authority.claim_serial_estop());
    assert_eq!(raw(&authority), before & !SERIAL_ESTOP_PENDING);
    assert!(!authority.claim_serial_estop());
    assert!(authority.claim_ipc_estop());
    assert_eq!(
        raw(&authority),
        before & !(SERIAL_ESTOP_PENDING | IPC_ESTOP_NOTICE)
    );
    assert!(!authority.claim_ipc_estop());
    assert_eq!(epoch(&authority), 9, "notice 消费不推进 epoch");
}

// ---------------------------------------------------------------------------
// Enable 生命周期
// ---------------------------------------------------------------------------

#[test]
fn enable_transaction_starts_once_and_commits_with_current_torque_on() {
    let authority = fresh(false, S::TorqueOffConfirmed);
    let (owner, view) = authority.reserve_enable().unwrap();
    assert_eq!(view.epoch().get(), 2);
    assert_eq!(owner.epoch(), view.epoch());
    assert_eq!(phase(&authority), Phase::EnableQueued);
    assert_eq!(
        authority.reserve_enable().err(),
        Some(Refusal::Busy(Phase::EnableQueued))
    );
    assert_eq!(
        authority.reserve_clear().err(),
        Some(Refusal::Busy(Phase::EnableQueued))
    );

    let copied = view;
    let execution = authority.start_enable(view).unwrap();
    assert_eq!(phase(&authority), Phase::EnableExecuting);
    assert_eq!(epoch(&authority), 2, "Queued → Executing 不推进 epoch");
    assert_eq!(
        authority.start_enable(copied).err(),
        Some(Refusal::WrongPhase(Phase::EnableExecuting)),
        "复制的 StartView 不能重启"
    );

    // 动作包含多个 wire 步骤：每步经同域 EnableStep 核验，release 一步不等于完成。
    let step_view = execution.view();
    for _ in 0..3 {
        let lease = authority
            .acquire_normal_io(NormalIoIntent::EnableStep(step_view))
            .unwrap();
        assert_eq!(
            authority
                .acquire_normal_io(NormalIoIntent::PeriodicRead)
                .err(),
            Some(Refusal::NormalIoActive)
        );
        assert!(!authority.release_normal_io(lease).unwrap().superseded);
        assert_eq!(phase(&authority), Phase::EnableExecuting);
    }
    assert_eq!(
        authority.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::Disabled),
        "Enable 完成前 Disabled 仍挡 Motion"
    );

    let finish = authority
        .finish_enable(execution, EnableResult::Succeeded)
        .unwrap();
    assert_eq!(
        finish,
        EnableFinish {
            phase: Phase::EnableSucceeded,
            torque_on_published: true,
            superseded: false
        }
    );
    assert_eq!(authority.snapshot().safety, S::TorqueOn);
    assert_eq!(
        authority
            .acquire_normal_io(NormalIoIntent::EnableStep(step_view))
            .err(),
        Some(Refusal::WrongPhase(Phase::EnableSucceeded)),
        "Succeeded 后执行视图不再授权 wire 步骤"
    );

    let committed = authority.commit_enable(owner).unwrap();
    assert_eq!(committed.epoch.get(), 3);
    let snapshot = authority.snapshot();
    assert!(!snapshot.disabled);
    assert_eq!(snapshot.phase, Some(Phase::Idle));
    assert_eq!(snapshot.safety, S::TorqueOn);
    let lease = authority.acquire_normal_io(NormalIoIntent::Motion).unwrap();
    assert_eq!(lease.epoch().get(), 3);
    let _ = authority.release_normal_io(lease).unwrap();
    assert_eq!(
        authority.start_enable(copied).err(),
        Some(Refusal::WrongPhase(Phase::Idle)),
        "旧视图在事务结束后永不重新有效"
    );
    assert_eq!(
        authority.reserve_enable().err(),
        Some(Refusal::NotDisabled),
        "已提交 Enable 之后不再占用底层使能事务"
    );
    let _ = authority.disable();
    let (next_owner, _next_view) = authority.reserve_enable().unwrap();
    assert_eq!(next_owner.epoch().get(), 5);
    assert_eq!(
        authority.start_enable(copied).err(),
        Some(Refusal::StaleEpoch {
            held: PermissionEpoch(2),
            current: PermissionEpoch(5)
        }),
        "旧视图不能启动后继事务"
    );
    let _ = authority.retire_enable(next_owner).unwrap();
}

#[test]
fn reserve_enable_requires_disabled_until_a_new_disable_arrives() {
    let authority = fresh(false, S::TorqueOffConfirmed);
    let owner = succeeded_enable(&authority);
    let _ = authority.commit_enable(owner).unwrap();
    assert!(!authority.snapshot().disabled);
    for _ in 0..2 {
        assert_eq!(authority.reserve_enable().err(), Some(Refusal::NotDisabled));
        assert_eq!(phase(&authority), Phase::Idle);
        assert_eq!(epoch(&authority), 3, "拒绝不推进 epoch");
    }
    let tightened = authority.disable();
    assert_eq!(tightened.epoch.get(), 4);
    let (owner, view) = authority.reserve_enable().unwrap();
    assert_eq!(owner.epoch().get(), 5);
    rewrite(&authority, |word| word.clear(DISABLED));
    assert_eq!(
        authority.start_enable(view).err(),
        Some(Refusal::NotDisabled)
    );
    rewrite(&authority, |word| word.set(DISABLED));
    let execution = authority.start_enable(view).unwrap();
    let _ = authority
        .finish_enable(execution, EnableResult::Succeeded)
        .unwrap();
    rewrite(&authority, |word| word.clear(DISABLED));
    let Err(Refused {
        token: owner,
        reason,
    }) = authority.commit_enable(owner)
    else {
        panic!("Disabled 已不成立时不得提交");
    };
    assert_eq!(reason, Refusal::NotDisabled);
    rewrite(&authority, |word| word.set(DISABLED));
    let _ = authority.commit_enable(owner).unwrap();
}

#[test]
fn finish_enable_requires_every_io_lease_to_be_released_first() {
    for result in [EnableResult::Succeeded, EnableResult::Failed] {
        for revoke in [false, true] {
            let authority = fresh(false, S::TorqueOffConfirmed);
            let (owner, view) = authority.reserve_enable().unwrap();
            let execution = authority.start_enable(view).unwrap();
            let lease = authority
                .acquire_normal_io(NormalIoIntent::EnableStep(execution.view()))
                .unwrap();
            if revoke {
                let _ = authority.estop();
            }
            let Err(Refused {
                token: execution,
                reason,
            }) = authority.finish_enable(execution, result)
            else {
                panic!("仍持有 wire 租约时不得进入终态 ({result:?}, revoke={revoke})");
            };
            assert_eq!(reason, Refusal::NormalIoActive);
            assert_eq!(phase(&authority), Phase::EnableExecuting);
            assert_eq!(
                authority.snapshot().safety,
                if revoke {
                    S::StopCommanded
                } else {
                    S::TorqueOffConfirmed
                }
            );
            let Err(Refused {
                token: owner,
                reason,
            }) = authority.retire_enable(owner)
            else {
                panic!("Executing 仍不可提前退休");
            };
            assert_eq!(reason, Refusal::WrongPhase(Phase::EnableExecuting));
            let released = authority.release_normal_io(lease).unwrap();
            assert_eq!(released.superseded, revoke);
            let finish = authority.finish_enable(execution, result).unwrap();
            let expected_phase = match result {
                EnableResult::Succeeded => Phase::EnableSucceeded,
                EnableResult::Failed => Phase::EnableFailed,
            };
            assert_eq!(finish.phase, expected_phase);
            assert_eq!(finish.superseded, revoke);
            assert_eq!(
                finish.torque_on_published,
                result == EnableResult::Succeeded && !revoke
            );
            let retired = authority.retire_enable(owner).unwrap();
            assert_eq!(retired.retired_from, expected_phase);
            assert_eq!(retired.superseded, revoke);
        }
    }
}

#[test]
fn finalize_authorization_reevaluates_when_estop_lands_before_its_cas() {
    let authority = fresh(true, S::TorqueOffConfirmed);
    let finalizing = authority
        .commit_clear(authority.reserve_clear().unwrap())
        .unwrap();
    let fired = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&fired);
    install_hook(
        &authority,
        once(move |authority| {
            observed.store(true, Ordering::Release);
            let _ = authority.estop();
        }),
    );
    let Err(Refused {
        token: finalizing,
        reason,
    }) = authority.authorize_finalize(finalizing)
    else {
        panic!("捕获旧 word 之后、授权 CAS 之前的 EStop 必须重判并拒旧 Finalize");
    };
    assert!(fired.load(Ordering::Acquire));
    assert_eq!(reason, Refusal::Latched);
    assert_eq!(phase(&authority), Phase::ClearFinalizing);
    let permit = authority
        .begin_repair(ClearOwner::Finalizing(finalizing))
        .unwrap();
    assert_eq!(permit.target(), RepairTarget::Latched);
    let cleanup = authority.authorize_repair_cleanup(permit).unwrap();
    let retirement = authority.retire_clear_cleanup(cleanup).unwrap();
    assert!(retirement.logical_committed && retirement.superseded && retirement.latch);
}

#[test]
fn repair_cleanup_authorization_reevaluates_when_a_revocation_lands_before_its_cas() {
    // 原 false 修复：授权 CAS 之前的 EStop 改变所需目标。
    let authority = fresh(false, S::TorqueOffConfirmed);
    let pending = authority.reserve_clear().unwrap();
    let permit = authority
        .begin_repair(ClearOwner::Pending(pending))
        .unwrap();
    assert_eq!(permit.target(), RepairTarget::OriginalUnlatched);
    install_hook(
        &authority,
        once(|authority| {
            let _ = authority.estop();
        }),
    );
    let Err(Refused {
        token: owner,
        reason,
    }) = authority.authorize_repair_cleanup(permit)
    else {
        panic!("授权 CAS 之前的 EStop 必须拒绝旧 false 修复");
    };
    assert_eq!(
        reason,
        Refusal::StaleEpoch {
            held: PermissionEpoch(2),
            current: PermissionEpoch(3)
        }
    );
    assert_eq!(phase(&authority), Phase::ClearPreparing);
    let permit = authority.begin_repair(owner).unwrap();
    assert_eq!(permit.target(), RepairTarget::Latched);
    let cleanup = authority.authorize_repair_cleanup(permit).unwrap();
    assert!(authority.retire_clear_cleanup(cleanup).unwrap().latch);

    // Latched 修复：授权 CAS 之前的 Disable 使采样 epoch 过期，同样重判。
    let authority = fresh(true, S::TorqueOffConfirmed);
    let pending = authority.reserve_clear().unwrap();
    let _ = authority.estop();
    let permit = authority
        .begin_repair(ClearOwner::Pending(pending))
        .unwrap();
    assert_eq!(permit.sampled_epoch().get(), 3);
    install_hook(
        &authority,
        once(|authority| {
            let _ = authority.disable();
        }),
    );
    let Err(Refused {
        token: owner,
        reason,
    }) = authority.authorize_repair_cleanup(permit)
    else {
        panic!("授权 CAS 之前的 Disable 必须重判");
    };
    assert_eq!(
        reason,
        Refusal::StaleEpoch {
            held: PermissionEpoch(3),
            current: PermissionEpoch(4)
        }
    );
    let cleanup = authority
        .authorize_repair_cleanup(authority.begin_repair(owner).unwrap())
        .unwrap();
    assert_eq!(cleanup.sampled_epoch().get(), 4);
    let _ = authority.retire_clear_cleanup(cleanup).unwrap();
}

#[test]
fn retire_refuses_executing_and_only_releases_queued_or_terminal_phases() {
    let authority = fresh(false, S::TorqueOffConfirmed);
    let (owner, view) = authority.reserve_enable().unwrap();
    let execution = authority.start_enable(view).unwrap();
    let Err(Refused {
        token: owner,
        reason,
    }) = authority.retire_enable(owner)
    else {
        panic!("Executing 必须拒绝退休");
    };
    assert_eq!(reason, Refusal::WrongPhase(Phase::EnableExecuting));
    assert_eq!(phase(&authority), Phase::EnableExecuting);
    let finish = authority
        .finish_enable(execution, EnableResult::Failed)
        .unwrap();
    assert_eq!(
        finish,
        EnableFinish {
            phase: Phase::EnableFailed,
            torque_on_published: false,
            superseded: false
        }
    );
    assert_eq!(
        authority.snapshot().safety,
        S::TorqueOffConfirmed,
        "失败不改安全态"
    );
    let Err(Refused {
        token: owner,
        reason,
    }) = authority.commit_enable(owner)
    else {
        panic!("Failed 不能提交");
    };
    assert_eq!(reason, Refusal::WrongPhase(Phase::EnableFailed));
    let retired = authority.retire_enable(owner).unwrap();
    assert_eq!(
        retired,
        EnableRetired {
            retired_from: Phase::EnableFailed,
            superseded: false
        }
    );
    assert_eq!(phase(&authority), Phase::Idle);
    assert_eq!(epoch(&authority), 2, "退休不推进 epoch");
    assert!(authority.snapshot().disabled);
}

#[test]
fn estop_during_queued_keeps_the_placeholder_until_the_owner_retires() {
    let authority = fresh(false, S::TorqueOffConfirmed);
    let (owner, view) = authority.reserve_enable().unwrap();
    let _ = authority.estop();
    assert_eq!(phase(&authority), Phase::EnableQueued, "撤销保留占位");
    assert_eq!(
        authority.start_enable(view).err(),
        Some(Refusal::StaleEpoch {
            held: PermissionEpoch(2),
            current: PermissionEpoch(3)
        })
    );
    assert_eq!(
        authority.reserve_enable().err(),
        Some(Refusal::Busy(Phase::EnableQueued))
    );
    let retired = authority.retire_enable(owner).unwrap();
    assert_eq!(
        retired,
        EnableRetired {
            retired_from: Phase::EnableQueued,
            superseded: true
        }
    );
    assert_eq!(phase(&authority), Phase::Idle);
    assert!(authority.snapshot().latch, "退休保留最新保护");
    assert_eq!(authority.reserve_enable().err(), Some(Refusal::Latched));
}

#[test]
fn disable_between_hardware_success_and_commit_only_allows_retirement() {
    let authority = fresh(false, S::TorqueOffConfirmed);
    let owner = succeeded_enable(&authority);
    let _ = authority.disable();
    let Err(Refused {
        token: owner,
        reason,
    }) = authority.commit_enable(owner)
    else {
        panic!("Disable 后旧成功不能提交");
    };
    assert_eq!(
        reason,
        Refusal::StaleEpoch {
            held: PermissionEpoch(2),
            current: PermissionEpoch(3)
        }
    );
    let snapshot = authority.snapshot();
    assert!(snapshot.disabled);
    assert_eq!(snapshot.phase, Some(Phase::EnableSucceeded));
    assert_eq!(snapshot.safety, S::TorqueOn, "真实硬件结果仍如实记录");
    assert_eq!(
        authority.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::Disabled),
        "不能恢复运动"
    );
    let retired = authority.retire_enable(owner).unwrap();
    assert_eq!(
        retired,
        EnableRetired {
            retired_from: Phase::EnableSucceeded,
            superseded: true
        }
    );
    assert!(authority.snapshot().disabled);
}

#[test]
fn finish_after_revocation_records_the_real_result_without_publishing_on() {
    for revoke in 0..3 {
        let authority = fresh(false, S::TorqueOffConfirmed);
        let (owner, view) = authority.reserve_enable().unwrap();
        let execution = authority.start_enable(view).unwrap();
        let step = execution.view();
        match revoke {
            0 => {
                let _ = authority.estop();
            }
            1 => {
                let _ = authority.tighten_inhibit(InhibitReason::CommFault);
            }
            _ => {
                let _ = authority.tighten_safety(SafetyTightening::AdapterUncertain);
            }
        }
        let expected_safety = authority.snapshot().safety;
        assert!(
            authority
                .acquire_normal_io(NormalIoIntent::EnableStep(step))
                .is_err(),
            "撤销后 wire 步骤不再获准"
        );
        let finish = authority
            .finish_enable(execution, EnableResult::Succeeded)
            .unwrap();
        assert_eq!(
            finish,
            EnableFinish {
                phase: Phase::EnableSucceeded,
                torque_on_published: false,
                superseded: true
            }
        );
        assert_eq!(
            authority.snapshot().safety,
            expected_safety,
            "旧成功不能把新保护写成 TorqueOn"
        );
        assert!(authority.commit_enable(owner).is_err());
    }
}

#[test]
fn old_confirm_during_executing_cannot_write_off_after_finish_publishes_on() {
    let authority = fresh(false, S::StopCommanded);
    let (owner, view) = authority.reserve_enable().unwrap();
    let execution = authority.start_enable(view).unwrap();
    let stale = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
    assert_eq!(stale.safety(), S::StopCommanded);
    let finish = authority
        .finish_enable(execution, EnableResult::Succeeded)
        .unwrap();
    assert!(finish.torque_on_published);
    assert_eq!(epoch(&authority), 2, "同代次：finish 不推进 epoch");
    let Err(SafetyObservationError::Rejected(reason)) =
        authority.complete_safety_observation(stale)
    else {
        panic!("Executing 期间的旧 Confirm 不得覆盖 On");
    };
    assert_eq!(reason, Refusal::ObservationStale(ObservedField::Phase));
    assert_eq!(authority.snapshot().safety, S::TorqueOn);
    let committed = authority.commit_enable(owner).unwrap();
    assert_eq!(committed.epoch.get(), 3);
}

#[test]
fn new_confirm_after_success_makes_commit_refuse_the_old_enable() {
    let authority = fresh(false, S::TorqueOffConfirmed);
    let owner = succeeded_enable(&authority);
    let observation = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
    assert_eq!(observation.safety(), S::TorqueOn);
    let committed = authority.complete_safety_observation(observation).unwrap();
    assert_eq!(committed.previous, S::TorqueOn);
    assert_eq!(committed.safety, S::TorqueOffConfirmed);
    assert_eq!(committed.tightened, None);
    let Err(Refused {
        token: owner,
        reason,
    }) = authority.commit_enable(owner)
    else {
        panic!("真实 Confirm 已见 Off，旧 Enable 不得提交");
    };
    assert_eq!(reason, Refusal::SafetyNotTorqueOn(S::TorqueOffConfirmed));
    assert!(authority.snapshot().disabled);
    let retired = authority.retire_enable(owner).unwrap();
    assert_eq!(
        retired,
        EnableRetired {
            retired_from: Phase::EnableSucceeded,
            superseded: false
        }
    );
}

#[test]
fn dropped_or_forgotten_owner_keeps_the_authority_conservatively_stuck() {
    let authority = fresh(false, S::TorqueOffConfirmed);
    {
        // owner 与 view 在本作用域结束时被丢弃；Drop 不解除任何位。
        let (_owner, _view) = authority.reserve_enable().unwrap();
    }
    assert_eq!(phase(&authority), Phase::EnableQueued, "Drop 不解除任何位");
    assert_eq!(
        authority.reserve_enable().err(),
        Some(Refusal::Busy(Phase::EnableQueued))
    );
    assert_eq!(
        authority.reserve_clear().err(),
        Some(Refusal::Busy(Phase::EnableQueued))
    );
    assert_eq!(
        authority.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::Disabled)
    );
    let _ = authority.estop();
    assert!(authority.snapshot().latch, "安全收紧不受卡住的占位影响");
    let lease = authority.acquire_normal_io(NormalIoIntent::PeriodicRead);
    assert_eq!(lease.err(), Some(Refusal::SerialEStopPending));
    assert!(authority.claim_serial_estop());
    let lease = authority
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    let _ = authority.release_normal_io(lease).unwrap();
}

// ---------------------------------------------------------------------------
// Clear 生命周期
// ---------------------------------------------------------------------------

#[test]
fn clear_expected_epoch_preserves_the_original_reservation_lifecycle() {
    let authority = fresh(true, S::TorqueOffConfirmed);
    let expected = authority.snapshot().epoch;
    let pending = authority.reserve_clear_if_epoch(expected).unwrap();
    assert_eq!(pending.domain(), authority.domain());
    assert_eq!(pending.epoch().get(), expected.get() + 1);
    assert!(pending.original_latched());
    let before = raw(&authority);
    assert_eq!(
        authority.reserve_clear_if_epoch(pending.epoch()).err(),
        Some(Refusal::Busy(Phase::ClearPreparing))
    );
    assert_eq!(raw(&authority), before);
    let finalizing = authority.commit_clear(pending).unwrap();
    let cleanup = authority.authorize_finalize(finalizing).unwrap();
    let retired = authority.retire_clear_cleanup(cleanup).unwrap();
    assert!(retired.logical_committed && !retired.superseded && !retired.latch);
    assert_eq!(phase(&authority), Phase::Idle);
    assert!(authority.snapshot().disabled);
}

#[test]
fn clear_stale_expected_epoch_is_a_zero_side_effect_refusal() {
    let authority = fresh(true, S::TorqueOffConfirmed);
    let held = authority.snapshot().epoch;
    let _ = authority.disable();
    let current = authority.snapshot().epoch;
    let before = raw(&authority);
    assert_eq!(
        authority.reserve_clear_if_epoch(held).err(),
        Some(Refusal::StaleEpoch { held, current })
    );
    assert_eq!(raw(&authority), before);
    // 新入口不改变旧预约语义：新的调用仍可用同一个原实现取得 owner。
    let pending = authority.reserve_clear().unwrap();
    assert_eq!(pending.epoch().get(), current.get() + 1);
    let repair = authority
        .begin_repair(ClearOwner::Pending(pending))
        .unwrap();
    let cleanup = authority.authorize_repair_cleanup(repair).unwrap();
    assert!(authority.retire_clear_cleanup(cleanup).unwrap().latch);
}

#[test]
fn clear_expected_epoch_rechecks_after_estop_and_confirm_win_its_cas() {
    let authority = fresh(true, S::TorqueOffConfirmed);
    let held = authority.snapshot().epoch;
    let after_interleaving = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let observed = Arc::clone(&after_interleaving);
    install_hook(
        &authority,
        once(move |authority| {
            // 同一个权限字的真实更新击败原预约 CAS；没有改写 raw 夹具。
            assert_eq!(authority.estop().outcome, EStopOutcome::StopIssued);
            assert!(authority.claim_serial_estop());
            assert!(authority.claim_ipc_estop());
            let confirmation = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
            authority.complete_safety_observation(confirmation).unwrap();
            assert_eq!(authority.snapshot().safety, S::TorqueOffConfirmed);
            observed.store(raw(authority), Ordering::Release);
        }),
    );
    assert_eq!(
        authority.reserve_clear_if_epoch(held).err(),
        Some(Refusal::StaleEpoch {
            held,
            current: authority.snapshot().epoch,
        })
    );
    let after = after_interleaving.load(Ordering::Acquire);
    assert_ne!(after, 0, "预约真实到达 CAS 前 hook");
    assert_eq!(
        raw(&authority),
        after,
        "旧预约不能改新 EStop/Confirm 的 word"
    );
    assert_eq!(phase(&authority), Phase::Idle);
    assert!(authority.snapshot().latch);
    // 反例中的普通 Clear 门已经重新满足；必须由原 expected 拒绝旧请求。
    let pending = authority
        .reserve_clear_if_epoch(authority.snapshot().epoch)
        .unwrap();
    let repair = authority
        .begin_repair(ClearOwner::Pending(pending))
        .unwrap();
    let cleanup = authority.authorize_repair_cleanup(repair).unwrap();
    assert!(authority.retire_clear_cleanup(cleanup).unwrap().latch);
}

#[test]
fn clear_expected_epoch_preserves_max_epoch_exhaustion() {
    let authority = at_epoch(true, S::TorqueOffConfirmed, word::EPOCH_MAX);
    let before = raw(&authority);
    assert_eq!(
        authority
            .reserve_clear_if_epoch(PermissionEpoch::INITIAL)
            .err(),
        Some(Refusal::StaleEpoch {
            held: PermissionEpoch::INITIAL,
            current: PermissionEpoch::MAX,
        })
    );
    assert_eq!(raw(&authority), before, "失配不能提前写 Exhausted");
    assert_eq!(
        authority.reserve_clear_if_epoch(PermissionEpoch::MAX).err(),
        Some(Refusal::EpochExhausted)
    );
    assert!(authority.snapshot().exhausted && authority.snapshot().disabled);
    assert_eq!(authority.snapshot().epoch, PermissionEpoch::MAX);
    assert_eq!(
        authority.reserve_clear_if_epoch(PermissionEpoch::MAX).err(),
        Some(Refusal::Exhausted)
    );
    assert_eq!(phase(&authority), Phase::Idle);
}

#[test]
fn clear_transaction_commits_then_finalizes_and_keeps_disabled() {
    let authority = fresh(true, S::TorqueOffConfirmed);
    assert_eq!(authority.reserve_enable().err(), Some(Refusal::Latched));
    let pending = authority.reserve_clear().unwrap();
    assert_eq!(pending.epoch().get(), 2);
    assert!(pending.original_latched());
    assert_eq!(phase(&authority), Phase::ClearPreparing);
    assert_eq!(
        authority.reserve_clear().err(),
        Some(Refusal::Busy(Phase::ClearPreparing))
    );
    let read = authority
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    let _ = authority.release_normal_io(read).unwrap();

    let finalizing = authority.commit_clear(pending).unwrap();
    assert_eq!(finalizing.epoch().get(), 3);
    assert!(finalizing.original_latched());
    let snapshot = authority.snapshot();
    assert!(!snapshot.latch && snapshot.disabled);
    assert_eq!(snapshot.phase, Some(Phase::ClearFinalizing));
    assert_eq!(
        authority.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::Disabled),
        "Clear 不授运动权限"
    );

    let permit = authority.authorize_finalize(finalizing).unwrap();
    assert_eq!(permit.kind(), CleanupKind::Finalize);
    assert_eq!(permit.sampled_epoch().get(), 3);
    let retirement = authority.retire_clear_cleanup(permit).unwrap();
    assert_eq!(
        retirement,
        ClearRetirement {
            kind: CleanupKind::Finalize,
            logical_committed: true,
            superseded: false,
            latch: false
        }
    );
    let snapshot = authority.snapshot();
    assert_eq!(snapshot.phase, Some(Phase::Idle));
    assert!(snapshot.disabled && !snapshot.latch);
    assert_eq!(snapshot.epoch.get(), 3, "退休不推进 epoch");
    let (owner, _view) = authority.reserve_enable().unwrap();
    assert_eq!(owner.epoch().get(), 4);
}

#[test]
fn clear_gates_require_disabled_idle_off_confirmed_and_no_blockers() {
    let authority = fresh(true, S::TorqueOffConfirmed);
    rewrite(&authority, |word| word.clear(DISABLED));
    assert_eq!(authority.reserve_clear().err(), Some(Refusal::NotDisabled));
    rewrite(&authority, |word| word.set(DISABLED));
    for state in [
        S::TorqueUnknown,
        S::TorqueOn,
        S::StopCommanded,
        S::StopFailed,
        S::AdapterUncertain,
    ] {
        rewrite(&authority, |word| word.with_safety(state));
        assert_eq!(
            authority.reserve_clear().err(),
            Some(Refusal::SafetyNotOffConfirmed(state))
        );
    }
    rewrite(&authority, |word| word.with_safety(S::TorqueOffConfirmed));
    let _ = authority.tighten_inhibit(InhibitReason::FaultPending);
    assert_eq!(
        authority.reserve_clear().err(),
        Some(Refusal::Inhibited(
            InhibitSet::EMPTY.with(InhibitReason::FaultPending)
        ))
    );
    let permit = authority
        .capture_inhibit_release(InhibitReason::FaultPending)
        .unwrap();
    let _ = authority.try_release_inhibit(permit).unwrap();
    let (owner, _view) = {
        rewrite(&authority, |word| word.clear(LATCH));
        authority.reserve_enable().unwrap()
    };
    assert_eq!(
        authority.reserve_clear().err(),
        Some(Refusal::Busy(Phase::EnableQueued))
    );
    let _ = authority.retire_enable(owner).unwrap();
    let pending = authority.reserve_clear().unwrap();
    assert!(
        !pending.original_latched(),
        "原 latch 为 false 的 Clear 允许，但记录原值"
    );
    let _ = authority.retire_clear_cleanup(
        authority
            .authorize_repair_cleanup(
                authority
                    .begin_repair(ClearOwner::Pending(pending))
                    .unwrap(),
            )
            .unwrap(),
    );
    let _ = authority.shutdown();
    assert_eq!(authority.reserve_clear().err(), Some(Refusal::Shutdown));
}

#[test]
fn pre_commit_estop_revokes_clear_and_requires_a_latched_repair() {
    let authority = fresh(true, S::TorqueOffConfirmed);
    let pending = authority.reserve_clear().unwrap();
    let _ = authority.estop();
    let Err(Refused {
        token: pending,
        reason,
    }) = authority.commit_clear(pending)
    else {
        panic!("新 EStop 后 Clear 不得提交");
    };
    assert_eq!(
        reason,
        Refusal::StaleEpoch {
            held: PermissionEpoch(2),
            current: PermissionEpoch(3)
        }
    );
    assert_eq!(phase(&authority), Phase::ClearPreparing, "阶段占位保留");
    let permit = authority
        .begin_repair(ClearOwner::Pending(pending))
        .unwrap();
    assert_eq!(permit.target(), RepairTarget::Latched);
    assert_eq!(permit.sampled_epoch().get(), 3);
    let cleanup = authority.authorize_repair_cleanup(permit).unwrap();
    assert_eq!(cleanup.kind(), CleanupKind::Repaired(RepairTarget::Latched));
    let retirement = authority.retire_clear_cleanup(cleanup).unwrap();
    assert_eq!(
        retirement,
        ClearRetirement {
            kind: CleanupKind::Repaired(RepairTarget::Latched),
            logical_committed: false,
            superseded: true,
            latch: true
        }
    );
    assert_eq!(phase(&authority), Phase::Idle);
    assert!(authority.snapshot().latch);
}

#[test]
fn pre_commit_disable_repairs_to_the_original_latch_value() {
    for original in [false, true] {
        let authority = fresh(original, S::TorqueOffConfirmed);
        let pending = authority.reserve_clear().unwrap();
        assert_eq!(pending.original_latched(), original);
        let _ = authority.disable();
        let Err(Refused { token: pending, .. }) = authority.commit_clear(pending) else {
            panic!("Disable 撤销恢复权限");
        };
        let permit = authority
            .begin_repair(ClearOwner::Pending(pending))
            .unwrap();
        let expected = if original {
            RepairTarget::Latched
        } else {
            RepairTarget::OriginalUnlatched
        };
        assert_eq!(
            permit.target(),
            expected,
            "只有原 false 才能用 OriginalUnlatched"
        );
        let cleanup = authority.authorize_repair_cleanup(permit).unwrap();
        let retirement = authority.retire_clear_cleanup(cleanup).unwrap();
        assert_eq!(retirement.kind, CleanupKind::Repaired(expected));
        assert!(!retirement.logical_committed && retirement.superseded);
        assert_eq!(retirement.latch, original, "Disable 不创造 EStop");
        assert_eq!(authority.snapshot().latch, original);
    }
}

#[test]
fn aborted_clear_without_revocation_repairs_and_is_not_superseded() {
    // A1 磁盘失败中止事务：权限未被撤销，修复目标仍保原 latch。
    let authority = fresh(true, S::TorqueOffConfirmed);
    let pending = authority.reserve_clear().unwrap();
    let permit = authority
        .begin_repair(ClearOwner::Pending(pending))
        .unwrap();
    assert_eq!(permit.target(), RepairTarget::Latched);
    let cleanup = authority.authorize_repair_cleanup(permit).unwrap();
    let retirement = authority.retire_clear_cleanup(cleanup).unwrap();
    assert!(!retirement.logical_committed && !retirement.superseded);
    assert_eq!(epoch(&authority), 2);
}

#[test]
fn post_commit_disable_finalizes_without_rolling_back_the_clear() {
    let authority = fresh(true, S::TorqueOffConfirmed);
    let finalizing = authority
        .commit_clear(authority.reserve_clear().unwrap())
        .unwrap();
    let _ = authority.disable();
    let Err(Refused {
        token: finalizing,
        reason,
    }) = authority.begin_repair(ClearOwner::Finalizing(finalizing))
    else {
        panic!("Latch=false 的 Finalizing 不需要修复");
    };
    assert_eq!(reason, Refusal::RepairNotNeeded);
    let ClearOwner::Finalizing(finalizing) = finalizing else {
        panic!("归还的必须是同一 owner 种类");
    };
    let permit = authority.authorize_finalize(finalizing).unwrap();
    assert_eq!(permit.kind(), CleanupKind::Finalize);
    let retirement = authority.retire_clear_cleanup(permit).unwrap();
    assert_eq!(
        retirement,
        ClearRetirement {
            kind: CleanupKind::Finalize,
            logical_committed: true,
            superseded: true,
            latch: false
        }
    );
    let snapshot = authority.snapshot();
    assert!(!snapshot.latch && snapshot.disabled);
    assert_eq!(
        authority.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::Disabled),
        "后到 Disable 使运动仍被禁止"
    );
}

#[test]
fn post_commit_estop_blocks_finalize_and_only_allows_a_latched_repair() {
    let authority = fresh(true, S::TorqueOffConfirmed);
    let finalizing = authority
        .commit_clear(authority.reserve_clear().unwrap())
        .unwrap();
    let _ = authority.estop();
    assert!(authority.snapshot().latch);
    let Err(Refused {
        token: finalizing,
        reason,
    }) = authority.authorize_finalize(finalizing)
    else {
        panic!("Latch=true 不得 Finalize");
    };
    assert_eq!(reason, Refusal::Latched);
    let permit = authority
        .begin_repair(ClearOwner::Finalizing(finalizing))
        .unwrap();
    assert_eq!(permit.target(), RepairTarget::Latched);
    let cleanup = authority.authorize_repair_cleanup(permit).unwrap();
    let retirement = authority.retire_clear_cleanup(cleanup).unwrap();
    assert_eq!(
        retirement,
        ClearRetirement {
            kind: CleanupKind::Repaired(RepairTarget::Latched),
            logical_committed: true,
            superseded: true,
            latch: true
        }
    );
    assert_eq!(phase(&authority), Phase::Idle);
}

#[test]
fn repair_permit_expires_when_the_epoch_moves_and_returns_the_same_owner() {
    let authority = fresh(true, S::TorqueOffConfirmed);
    let pending = authority.reserve_clear().unwrap();
    let permit = authority
        .begin_repair(ClearOwner::Pending(pending))
        .unwrap();
    let _ = authority.disable();
    let Err(Refused {
        token: owner,
        reason,
    }) = authority.authorize_repair_cleanup(permit)
    else {
        panic!("采样 epoch 已变");
    };
    assert_eq!(
        reason,
        Refusal::StaleEpoch {
            held: PermissionEpoch(2),
            current: PermissionEpoch(3)
        }
    );
    assert_eq!(
        phase(&authority),
        Phase::ClearPreparing,
        "占位与 marker 责任保留"
    );
    let permit = authority.begin_repair(owner).unwrap();
    assert_eq!(permit.sampled_epoch().get(), 3);
    let cleanup = authority.authorize_repair_cleanup(permit).unwrap();
    let _ = authority.retire_clear_cleanup(cleanup).unwrap();
}

#[test]
fn abandoned_cleanup_returns_the_owner_for_a_new_round() {
    let authority = fresh(true, S::TorqueOffConfirmed);
    let finalizing = authority
        .commit_clear(authority.reserve_clear().unwrap())
        .unwrap();
    let permit = authority.authorize_finalize(finalizing).unwrap();
    // 许可后、unlink 前到达新 EStop：原语先行、持久化随后。
    let _ = authority.estop();
    let owner = authority.abandon_cleanup(permit).unwrap();
    assert_eq!(phase(&authority), Phase::ClearFinalizing);
    let repair = authority.begin_repair(owner).unwrap();
    assert_eq!(repair.target(), RepairTarget::Latched);
    let cleanup = authority.authorize_repair_cleanup(repair).unwrap();
    let retirement = authority.retire_clear_cleanup(cleanup).unwrap();
    assert!(retirement.logical_committed && retirement.superseded && retirement.latch);
}

#[test]
fn clear_phases_never_release_inhibits_but_keep_health_reads() {
    let authority = fresh(true, S::TorqueOffConfirmed);
    let pending = authority.reserve_clear().unwrap();
    let _ = authority.tighten_inhibit(InhibitReason::Converging);
    let Err(Refused { token: pending, .. }) = authority.commit_clear(pending) else {
        panic!("收敛抑制撤销 Clear 权限");
    };
    assert_eq!(
        authority
            .acquire_normal_io(NormalIoIntent::PeriodicRead)
            .err(),
        Some(Refusal::Inhibited(
            InhibitSet::EMPTY.with(InhibitReason::Converging)
        )),
        "健康读取不得绕过收敛门"
    );
    let permit = authority
        .capture_inhibit_release(InhibitReason::Converging)
        .unwrap();
    let _ = authority.try_release_inhibit(permit).unwrap();
    let read = authority
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    let _ = authority.release_normal_io(read).unwrap();
    let cleanup = authority
        .authorize_repair_cleanup(
            authority
                .begin_repair(ClearOwner::Pending(pending))
                .unwrap(),
        )
        .unwrap();
    let _ = authority.retire_clear_cleanup(cleanup).unwrap();
}

// ---------------------------------------------------------------------------
// epoch MAX / Exhausted
// ---------------------------------------------------------------------------

#[test]
fn estop_at_max_epoch_exhausts_and_vetoes_the_old_unlatched_repair() {
    let authority = at_epoch(false, S::TorqueOffConfirmed, word::EPOCH_MAX - 1);
    let pending = authority.reserve_clear().unwrap();
    assert_eq!(epoch(&authority), word::EPOCH_MAX);
    assert!(!authority.snapshot().exhausted, "到达 MAX 本身仍有效");
    let permit = authority
        .begin_repair(ClearOwner::Pending(pending))
        .unwrap();
    assert_eq!(permit.target(), RepairTarget::OriginalUnlatched);
    let stale_confirm = authority.begin_safety_observation(SafetyOperation::ConfirmOff);

    let report = authority.estop();
    assert_eq!(report.outcome, EStopOutcome::StopIssued);
    assert_eq!(
        report.tightened,
        Tightened {
            epoch: PermissionEpoch::MAX,
            advanced: false,
            exhausted: true
        }
    );
    let snapshot = authority.snapshot();
    assert!(snapshot.exhausted && snapshot.latch && snapshot.disabled);
    assert_eq!(snapshot.epoch, PermissionEpoch::MAX, "不回绕");
    assert_eq!(snapshot.phase, Some(Phase::ClearPreparing));

    let Err(Refused {
        token: owner,
        reason,
    }) = authority.authorize_repair_cleanup(permit)
    else {
        panic!("false 修复后的新 EStop 即使仍为 MAX 也不能授权删除 marker");
    };
    assert_eq!(
        reason,
        Refusal::RepairTargetChanged {
            required: RepairTarget::Latched
        }
    );
    assert!(matches!(
        authority.complete_safety_observation(stale_confirm),
        Err(SafetyObservationError::Rejected(Refusal::Exhausted))
    ));
    assert_eq!(authority.snapshot().safety, S::StopCommanded);
    assert_eq!(
        authority.reserve_enable().err(),
        Some(Refusal::Busy(Phase::ClearPreparing))
    );
    assert_eq!(
        authority.capture_inhibit_release(InhibitReason::Boot).err(),
        Some(Refusal::Exhausted)
    );
    let tightened = authority.tighten_safety(SafetyTightening::StopFailed);
    assert_eq!(tightened.safety, S::StopFailed);
    assert_eq!(tightened.tightened.epoch, PermissionEpoch::MAX);
    assert!(authority.claim_serial_estop());

    let repair = authority.begin_repair(owner).unwrap();
    assert_eq!(repair.target(), RepairTarget::Latched);
    let cleanup = authority.authorize_repair_cleanup(repair).unwrap();
    let retirement = authority.retire_clear_cleanup(cleanup).unwrap();
    assert_eq!(
        retirement,
        ClearRetirement {
            kind: CleanupKind::Repaired(RepairTarget::Latched),
            logical_committed: false,
            superseded: true,
            latch: true
        }
    );
    assert_eq!(phase(&authority), Phase::Idle);
    assert_eq!(authority.reserve_enable().err(), Some(Refusal::Exhausted));
    assert_eq!(authority.reserve_clear().err(), Some(Refusal::Exhausted));
}

#[test]
fn grant_paths_at_max_epoch_fail_closed_into_exhausted() {
    // reserve_enable
    let authority = at_epoch(false, S::TorqueOffConfirmed, word::EPOCH_MAX);
    assert_eq!(
        authority.reserve_enable().err(),
        Some(Refusal::EpochExhausted)
    );
    let snapshot = authority.snapshot();
    assert!(snapshot.exhausted && snapshot.disabled);
    assert_eq!(snapshot.phase, Some(Phase::Idle));
    assert_eq!(authority.reserve_enable().err(), Some(Refusal::Exhausted));

    // reserve_clear
    let authority = at_epoch(true, S::TorqueOffConfirmed, word::EPOCH_MAX);
    assert_eq!(
        authority.reserve_clear().err(),
        Some(Refusal::EpochExhausted)
    );
    assert!(authority.snapshot().exhausted);

    // commit_enable：到达 MAX 的事务可以执行并记录成功，但不能提交。
    let authority = at_epoch(false, S::TorqueOffConfirmed, word::EPOCH_MAX - 1);
    let owner = succeeded_enable(&authority);
    let Err(Refused {
        token: owner,
        reason,
    }) = authority.commit_enable(owner)
    else {
        panic!("MAX 后不得授予恢复权限");
    };
    assert_eq!(reason, Refusal::EpochExhausted);
    let snapshot = authority.snapshot();
    assert!(snapshot.exhausted && snapshot.disabled);
    assert_eq!(snapshot.phase, Some(Phase::EnableSucceeded));
    let retired = authority.retire_enable(owner).unwrap();
    assert_eq!(
        retired,
        EnableRetired {
            retired_from: Phase::EnableSucceeded,
            superseded: true
        }
    );

    // commit_clear
    let authority = at_epoch(true, S::TorqueOffConfirmed, word::EPOCH_MAX - 1);
    let pending = authority.reserve_clear().unwrap();
    let Err(Refused {
        token: pending,
        reason,
    }) = authority.commit_clear(pending)
    else {
        panic!("MAX 后不得逻辑提交 Clear");
    };
    assert_eq!(reason, Refusal::EpochExhausted);
    assert!(authority.snapshot().latch);
    let cleanup = authority
        .authorize_repair_cleanup(
            authority
                .begin_repair(ClearOwner::Pending(pending))
                .unwrap(),
        )
        .unwrap();
    assert_eq!(cleanup.kind(), CleanupKind::Repaired(RepairTarget::Latched));
    assert!(authority.retire_clear_cleanup(cleanup).unwrap().superseded);

    // try_release_inhibit
    let authority = ControlAuthority::new_with_permission_epoch(
        AuthorityInit::new(
            false,
            S::TorqueOffConfirmed,
            InhibitSet::EMPTY.with(InhibitReason::Boot),
        ),
        PermissionEpoch::MAX,
    )
    .unwrap();
    let permit = authority
        .capture_inhibit_release(InhibitReason::Boot)
        .unwrap();
    assert!(matches!(
        authority.try_release_inhibit(permit),
        Err(InhibitReleaseError::Expired(Refusal::EpochExhausted))
    ));
    assert!(authority.snapshot().exhausted);
    assert!(authority.snapshot().inhibits.contains(InhibitReason::Boot));

    // Disconnect 观测是收紧：MAX 时仍记录，不推进 epoch。
    let authority = at_epoch(false, S::TorqueOn, word::EPOCH_MAX);
    let observation = authority.begin_safety_observation(SafetyOperation::Disconnect);
    let committed = authority.complete_safety_observation(observation).unwrap();
    assert_eq!(committed.safety, S::TorqueUnknown);
    assert_eq!(
        committed.tightened,
        Some(Tightened {
            epoch: PermissionEpoch::MAX,
            advanced: false,
            exhausted: true
        })
    );
    assert!(authority.snapshot().exhausted && authority.snapshot().disabled);
}

#[test]
fn exhausted_authority_still_tightens_releases_io_and_retires() {
    let authority = at_epoch(false, S::TorqueOffConfirmed, word::EPOCH_MAX - 1);
    let (owner, view) = authority.reserve_enable().unwrap();
    let execution = authority.start_enable(view).unwrap();
    let lease = authority
        .acquire_normal_io(NormalIoIntent::EnableStep(execution.view()))
        .unwrap();
    let _ = authority.disable();
    assert!(authority.snapshot().exhausted);
    let released = authority.release_normal_io(lease).unwrap();
    assert!(released.superseded);
    assert_eq!(
        authority
            .acquire_normal_io(NormalIoIntent::EnableStep(execution.view()))
            .err(),
        Some(Refusal::Exhausted)
    );
    let finish = authority
        .finish_enable(execution, EnableResult::Succeeded)
        .unwrap();
    assert!(finish.superseded && !finish.torque_on_published);
    let _ = authority.tighten_inhibit(InhibitReason::AdapterFault);
    let _ = authority.shutdown();
    let retired = authority.retire_enable(owner).unwrap();
    assert!(retired.superseded);
    assert_eq!(phase(&authority), Phase::Idle);
    let read = authority.acquire_normal_io(NormalIoIntent::PeriodicRead);
    assert_eq!(
        read.err(),
        Some(Refusal::Inhibited(
            InhibitSet::EMPTY.with(InhibitReason::AdapterFault)
        ))
    );
}

// ---------------------------------------------------------------------------
// 普通 IO 与 EStop 两种顺序
// ---------------------------------------------------------------------------

#[test]
fn estop_arriving_between_load_and_cas_makes_the_acquire_fail() {
    let authority = fresh(false, S::TorqueOffConfirmed);
    let owner = succeeded_enable(&authority);
    let _ = authority.commit_enable(owner).unwrap();
    let fired = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&fired);
    install_hook(
        &authority,
        once(move |authority| {
            observed.store(true, Ordering::Release);
            assert_eq!(authority.estop().outcome, EStopOutcome::StopIssued);
        }),
    );
    assert_eq!(
        authority.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::SerialEStopPending),
        "同域 CAS 确定先后：后来的运动获取失败"
    );
    assert!(fired.load(Ordering::Acquire));
    assert!(!authority.snapshot().normal_io_active);
}

#[test]
fn acquire_arriving_between_estop_load_and_cas_is_recorded_and_retires_normally() {
    let authority = fresh(false, S::TorqueOffConfirmed);
    let owner = succeeded_enable(&authority);
    let _ = authority.commit_enable(owner).unwrap();
    let slot: Arc<std::sync::Mutex<Option<NormalIoLease>>> = Arc::new(std::sync::Mutex::new(None));
    let sink = Arc::clone(&slot);
    install_hook(
        &authority,
        once(move |authority| {
            let lease = authority.acquire_normal_io(NormalIoIntent::Motion).unwrap();
            *sink.lock().unwrap() = Some(lease);
        }),
    );
    let report = authority.estop();
    assert_eq!(report.outcome, EStopOutcome::StopIssued);
    assert!(report.normal_io_active, "已开始的事务沿既有安全责任退场");
    let lease = slot.lock().unwrap().take().unwrap();
    assert_eq!(lease.epoch().get(), 3);
    assert_eq!(
        authority
            .acquire_normal_io(NormalIoIntent::PeriodicRead)
            .err(),
        Some(Refusal::SerialEStopPending)
    );
    let released = authority.release_normal_io(lease).unwrap();
    assert!(released.superseded, "lease 跨 epoch 释放");
    assert!(!authority.snapshot().normal_io_active);
    assert!(authority.snapshot().latch, "释放保留新保护");
    assert!(authority.claim_serial_estop());
    let read = authority
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    assert_eq!(
        authority.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::NormalIoActive)
    );
    let _ = authority.release_normal_io(read).unwrap();
    assert_eq!(
        authority.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::Latched)
    );
}

#[test]
fn normal_io_intents_apply_their_documented_gates() {
    let authority = fresh(true, S::TorqueOffConfirmed);
    // Latch + Disabled：读与非上扭控制允许，Motion 拒绝。
    let read = authority
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    let _ = authority.release_normal_io(read).unwrap();
    for control in [
        NonEnergizingControl::Connect,
        NonEnergizingControl::Disconnect,
        NonEnergizingControl::Disable,
    ] {
        let lease = authority
            .acquire_normal_io(NormalIoIntent::NonEnergizingControl(control))
            .unwrap();
        assert_eq!(
            lease.intent(),
            NormalIoIntent::NonEnergizingControl(control)
        );
        let _ = authority.release_normal_io(lease).unwrap();
    }
    assert_eq!(
        authority.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::Latched)
    );

    // Clear 各阶段仍保留健康读取。
    let pending = authority.reserve_clear().unwrap();
    let read = authority
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    let _ = authority.release_normal_io(read).unwrap();
    let finalizing = authority.commit_clear(pending).unwrap();
    let read = authority
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    let _ = authority.release_normal_io(read).unwrap();
    assert_eq!(
        authority.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::Disabled)
    );
    let permit = authority.authorize_finalize(finalizing).unwrap();
    let _ = authority.retire_clear_cleanup(permit).unwrap();

    // Motion 需要 Idle 与 !Disabled；提交后的 Enable 才放行。
    let (owner, view) = authority.reserve_enable().unwrap();
    rewrite(&authority, |word| word.clear(DISABLED));
    assert_eq!(
        authority.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::Busy(Phase::EnableQueued))
    );
    rewrite(&authority, |word| word.set(DISABLED));
    let execution = authority.start_enable(view).unwrap();
    let other = fresh(false, S::TorqueOffConfirmed);
    let (other_owner, other_view) = other.reserve_enable().unwrap();
    let other_execution = other.start_enable(other_view).unwrap();
    assert_eq!(
        authority
            .acquire_normal_io(NormalIoIntent::EnableStep(other_execution.view()))
            .err(),
        Some(Refusal::CrossDomain {
            token: other.domain(),
            authority: authority.domain()
        })
    );
    let _ = other
        .finish_enable(other_execution, EnableResult::Failed)
        .unwrap();
    let _ = other.retire_enable(other_owner).unwrap();
    let _ = authority
        .finish_enable(execution, EnableResult::Succeeded)
        .unwrap();
    let _ = authority.commit_enable(owner).unwrap();
    let lease = authority.acquire_normal_io(NormalIoIntent::Motion).unwrap();
    let _ = authority.release_normal_io(lease).unwrap();

    // 基础门对所有意图生效。
    let _ = authority.tighten_inhibit(InhibitReason::Boot);
    for intent in [
        NormalIoIntent::PeriodicRead,
        NormalIoIntent::Motion,
        NormalIoIntent::NonEnergizingControl(NonEnergizingControl::Disable),
    ] {
        assert_eq!(
            authority.acquire_normal_io(intent).err(),
            Some(Refusal::Inhibited(
                InhibitSet::EMPTY.with(InhibitReason::Boot)
            ))
        );
    }
}

#[test]
fn release_requires_an_active_bit_and_returns_the_lease_on_refusal() {
    let authority = fresh(false, S::TorqueOffConfirmed);
    let lease = authority
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    rewrite(&authority, |word| word.clear(NORMAL_IO_ACTIVE));
    let Err(Refused {
        token: lease,
        reason,
    }) = authority.release_normal_io(lease)
    else {
        panic!("位未置起时不得假装释放");
    };
    assert_eq!(reason, Refusal::LeaseNotActive);
    rewrite(&authority, |word| word.set(NORMAL_IO_ACTIVE));
    let released = authority.release_normal_io(lease).unwrap();
    assert_eq!(released.intent, NormalIoIntent::PeriodicRead);
    assert!(!released.superseded);
}

#[test]
fn concurrent_acquires_never_overlap_and_estop_blocks_later_acquires() {
    fn wave(
        authority: &Arc<ControlAuthority>,
        active: &Arc<AtomicUsize>,
        overlaps: &Arc<AtomicUsize>,
        successes: &Arc<AtomicUsize>,
        refusals: &Arc<AtomicUsize>,
    ) {
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let authority = Arc::clone(authority);
                let active = Arc::clone(active);
                let overlaps = Arc::clone(overlaps);
                let successes = Arc::clone(successes);
                let refusals = Arc::clone(refusals);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..2_000 {
                        match authority.acquire_normal_io(NormalIoIntent::PeriodicRead) {
                            Ok(lease) => {
                                if active.fetch_add(1, Ordering::AcqRel) != 0 {
                                    overlaps.fetch_add(1, Ordering::AcqRel);
                                }
                                successes.fetch_add(1, Ordering::AcqRel);
                                active.fetch_sub(1, Ordering::AcqRel);
                                authority.release_normal_io(lease).unwrap();
                            }
                            Err(Refusal::SerialEStopPending) => {
                                refusals.fetch_add(1, Ordering::AcqRel);
                            }
                            Err(Refusal::NormalIoActive) => {}
                            Err(other) => panic!("意外拒绝原因 {other:?}"),
                        }
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    }

    let authority = Arc::new(fresh(false, S::TorqueOffConfirmed));
    let active = Arc::new(AtomicUsize::new(0));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let successes = Arc::new(AtomicUsize::new(0));
    let refusals = Arc::new(AtomicUsize::new(0));

    // 第一波：真实竞争下租约互斥；CAS 失败者必有赢家，因此成功数确定性大于零。
    wave(&authority, &active, &overlaps, &successes, &refusals);
    assert_eq!(overlaps.load(Ordering::Acquire), 0, "租约互斥");
    assert!(successes.load(Ordering::Acquire) > 0);
    assert_eq!(refusals.load(Ordering::Acquire), 0);
    assert!(!authority.snapshot().normal_io_active);

    // EStop 之后的第二波：每次获取都因 serial pending 被拒，没有任何新租约。
    let before = successes.load(Ordering::Acquire);
    let report = authority.estop();
    assert!(report.tightened.advanced && !report.normal_io_active);
    wave(&authority, &active, &overlaps, &successes, &refusals);
    assert_eq!(successes.load(Ordering::Acquire), before);
    assert_eq!(refusals.load(Ordering::Acquire), 8 * 2_000);
    assert_eq!(overlaps.load(Ordering::Acquire), 0);
    assert!(!authority.snapshot().normal_io_active);
    assert_eq!(
        authority
            .acquire_normal_io(NormalIoIntent::PeriodicRead)
            .err(),
        Some(Refusal::SerialEStopPending)
    );
}

// ---------------------------------------------------------------------------
// inhibit 解除
// ---------------------------------------------------------------------------

#[test]
fn sticky_inhibits_cannot_be_captured() {
    let authority = ControlAuthority::new(AuthorityInit::new(
        false,
        S::TorqueOffConfirmed,
        InhibitSet::ALL,
    ))
    .unwrap();
    for reason in [
        InhibitReason::CommFault,
        InhibitReason::AdapterFault,
        InhibitReason::StopFailed,
    ] {
        assert_eq!(
            authority.capture_inhibit_release(reason).err(),
            Some(Refusal::InhibitSticky(reason))
        );
    }
    assert_eq!(authority.snapshot().inhibits, InhibitSet::ALL);
}

#[test]
fn releasable_inhibits_need_a_fresh_permit_after_any_revocation() {
    let authority = ControlAuthority::new(AuthorityInit::new(
        false,
        S::TorqueOffConfirmed,
        InhibitSet::EMPTY
            .with(InhibitReason::Boot)
            .with(InhibitReason::FaultPending),
    ))
    .unwrap();
    assert_eq!(
        authority
            .capture_inhibit_release(InhibitReason::Converging)
            .err(),
        Some(Refusal::InhibitNotSet(InhibitReason::Converging))
    );
    let stale = authority
        .capture_inhibit_release(InhibitReason::Boot)
        .unwrap();
    assert_eq!(stale.epoch().get(), 1);
    let _ = authority.disable();
    assert!(matches!(
        authority.try_release_inhibit(stale),
        Err(InhibitReleaseError::Expired(Refusal::StaleEpoch {
            held: PermissionEpoch(1),
            current: PermissionEpoch(2)
        }))
    ));
    assert!(
        authority.snapshot().inhibits.contains(InhibitReason::Boot),
        "过期许可不改位"
    );

    let first = authority
        .capture_inhibit_release(InhibitReason::Boot)
        .unwrap();
    let second = authority
        .capture_inhibit_release(InhibitReason::Boot)
        .unwrap();
    let released = authority.try_release_inhibit(first).unwrap();
    assert_eq!(released.reason, InhibitReason::Boot);
    assert_eq!(released.epoch.get(), 3);
    assert_eq!(
        released.remaining,
        InhibitSet::EMPTY.with(InhibitReason::FaultPending)
    );
    assert!(matches!(
        authority.try_release_inhibit(second),
        Err(InhibitReleaseError::Expired(Refusal::StaleEpoch { .. }))
    ));
    assert_eq!(
        authority.snapshot().inhibits,
        InhibitSet::EMPTY.with(InhibitReason::FaultPending)
    );
    assert_eq!(
        authority.capture_inhibit_release(InhibitReason::Boot).err(),
        Some(Refusal::InhibitNotSet(InhibitReason::Boot))
    );

    let permit = authority
        .capture_inhibit_release(InhibitReason::FaultPending)
        .unwrap();
    let _ = authority.shutdown();
    assert!(matches!(
        authority.try_release_inhibit(permit),
        Err(InhibitReleaseError::Expired(Refusal::Shutdown))
    ));
    assert_eq!(
        authority
            .capture_inhibit_release(InhibitReason::FaultPending)
            .err(),
        Some(Refusal::Shutdown)
    );
}

// ---------------------------------------------------------------------------
// 安全观测
// ---------------------------------------------------------------------------

#[test]
fn observations_require_the_four_sampled_fields_but_ignore_notices_and_io() {
    let authority = fresh(true, S::StopCommanded);
    let _ = authority.estop();
    let observation = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
    assert_eq!(observation.epoch().get(), 2);
    assert!(authority.claim_serial_estop());
    assert!(authority.claim_ipc_estop());
    let read = authority
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    let _ = authority.release_normal_io(read).unwrap();
    let committed = authority.complete_safety_observation(observation).unwrap();
    assert_eq!(committed.previous, S::StopCommanded);
    assert_eq!(committed.safety, S::TorqueOffConfirmed);
    assert_eq!(epoch(&authority), 2, "ConfirmOff 成功不推进 epoch");

    let by_epoch = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
    let _ = authority.disable();
    assert!(matches!(
        authority.complete_safety_observation(by_epoch),
        Err(SafetyObservationError::Rejected(Refusal::ObservationStale(
            ObservedField::Epoch
        )))
    ));

    let by_phase = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
    let pending = authority.reserve_clear().unwrap();
    let _ = authority.retire_clear_cleanup(
        authority
            .authorize_repair_cleanup(
                authority
                    .begin_repair(ClearOwner::Pending(pending))
                    .unwrap(),
            )
            .unwrap(),
    );
    assert!(matches!(
        authority.complete_safety_observation(by_phase),
        Err(SafetyObservationError::Rejected(Refusal::ObservationStale(
            ObservedField::Epoch
        )))
    ));

    rewrite(&authority, |word| word.with_safety(S::StopCommanded));
    let first = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
    let second = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
    let _ = authority.complete_safety_observation(first).unwrap();
    assert!(matches!(
        authority.complete_safety_observation(second),
        Err(SafetyObservationError::Rejected(Refusal::ObservationStale(
            ObservedField::Safety
        )))
    ));
}

#[test]
fn observation_phase_check_is_independent_of_the_epoch() {
    let authority = fresh(false, S::StopCommanded);
    let (owner, view) = authority.reserve_enable().unwrap();
    let before_start = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
    let execution = authority.start_enable(view).unwrap();
    assert!(matches!(
        authority.complete_safety_observation(before_start),
        Err(SafetyObservationError::Rejected(Refusal::ObservationStale(
            ObservedField::Phase
        )))
    ));
    let _ = authority
        .finish_enable(execution, EnableResult::Failed)
        .unwrap();
    let _ = authority.retire_enable(owner).unwrap();
}

#[test]
fn disable_observation_requires_disabled_and_faults_cannot_be_relaxed() {
    let authority = fresh(false, S::TorqueOn);
    rewrite(&authority, |word| word.clear(DISABLED));
    let observation = authority.begin_safety_observation(SafetyOperation::Disable);
    assert!(matches!(
        authority.complete_safety_observation(observation),
        Err(SafetyObservationError::Rejected(Refusal::NotDisabled))
    ));
    assert_eq!(authority.snapshot().safety, S::TorqueOn);
    let _ = authority.disable();
    let observation = authority.begin_safety_observation(SafetyOperation::Disable);
    let committed = authority.complete_safety_observation(observation).unwrap();
    assert_eq!(committed.safety, S::TorqueOffConfirmed);

    for fault in [S::StopFailed, S::AdapterUncertain] {
        let authority = fresh(false, fault);
        for operation in [SafetyOperation::ConfirmOff, SafetyOperation::Disable] {
            let observation = authority.begin_safety_observation(operation);
            assert!(matches!(
                authority.complete_safety_observation(observation),
                Err(SafetyObservationError::Rejected(Refusal::SafetyNotRelaxable(state))) if state == fault
            ));
        }
        let observation = authority.begin_safety_observation(SafetyOperation::Disconnect);
        let committed = authority.complete_safety_observation(observation).unwrap();
        assert_eq!(committed.safety, fault, "断连保持故障态");
        assert_eq!(
            authority.reserve_enable().err(),
            Some(Refusal::SafetyNotRelaxable(fault))
        );
    }
}

#[test]
fn disconnect_revokes_earlier_observations_and_never_claims_torque_off() {
    for previous in [
        S::TorqueUnknown,
        S::TorqueOn,
        S::StopCommanded,
        S::TorqueOffConfirmed,
    ] {
        let authority = fresh(false, previous);
        rewrite(&authority, |word| word.clear(DISABLED));
        let earlier = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
        let disconnect = authority.begin_safety_observation(SafetyOperation::Disconnect);
        let committed = authority.complete_safety_observation(disconnect).unwrap();
        assert_eq!(committed.previous, previous);
        assert_eq!(committed.safety, S::TorqueUnknown);
        assert_eq!(
            committed.tightened,
            Some(Tightened {
                epoch: PermissionEpoch(2),
                advanced: true,
                exhausted: false
            })
        );
        assert!(authority.snapshot().disabled);
        assert!(matches!(
            authority.complete_safety_observation(earlier),
            Err(SafetyObservationError::Rejected(Refusal::ObservationStale(
                ObservedField::Epoch
            )))
        ));
    }
}

// ---------------------------------------------------------------------------
// 跨 domain
// ---------------------------------------------------------------------------

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "逐个令牌类型验证跨 domain 拒绝且不改位，顺序依赖同一对权威"
)]
fn cross_domain_tokens_are_refused_without_changing_any_bit() {
    let source = fresh(true, S::TorqueOffConfirmed);
    let target = fresh(true, S::TorqueOffConfirmed);
    let cross = Refusal::CrossDomain {
        token: source.domain(),
        authority: target.domain(),
    };
    let before = raw(&target);

    let pending = source.reserve_clear().unwrap();
    let Err(Refused {
        token: pending,
        reason,
    }) = target.commit_clear(pending)
    else {
        panic!()
    };
    assert_eq!(reason, cross);
    let Err(Refused {
        token: owner,
        reason,
    }) = target.begin_repair(ClearOwner::Pending(pending))
    else {
        panic!()
    };
    assert_eq!(reason, cross);
    let permit = source.begin_repair(owner).unwrap();
    let Err(Refused {
        token: owner,
        reason,
    }) = target.authorize_repair_cleanup(permit)
    else {
        panic!()
    };
    assert_eq!(reason, cross);
    let cleanup = source
        .authorize_repair_cleanup(source.begin_repair(owner).unwrap())
        .unwrap();
    let Err(Refused {
        token: cleanup,
        reason,
    }) = target.abandon_cleanup(cleanup)
    else {
        panic!()
    };
    assert_eq!(reason, cross);
    let Err(Refused {
        token: cleanup,
        reason,
    }) = target.retire_clear_cleanup(cleanup)
    else {
        panic!()
    };
    assert_eq!(reason, cross);
    let _ = source.retire_clear_cleanup(cleanup).unwrap();
    let finalizing = source
        .commit_clear(source.reserve_clear().unwrap())
        .unwrap();
    let Err(Refused {
        token: finalizing,
        reason,
    }) = target.authorize_finalize(finalizing)
    else {
        panic!()
    };
    assert_eq!(reason, cross);
    let _ = source
        .retire_clear_cleanup(source.authorize_finalize(finalizing).unwrap())
        .unwrap();

    let (owner, view) = source.reserve_enable().unwrap();
    assert_eq!(target.start_enable(view).err(), Some(cross));
    let execution = source.start_enable(view).unwrap();
    assert_eq!(
        target
            .acquire_normal_io(NormalIoIntent::EnableStep(execution.view()))
            .err(),
        Some(cross)
    );
    let Err(Refused {
        token: execution,
        reason,
    }) = target.finish_enable(execution, EnableResult::Succeeded)
    else {
        panic!()
    };
    assert_eq!(reason, cross);
    let _ = source
        .finish_enable(execution, EnableResult::Succeeded)
        .unwrap();
    let Err(Refused {
        token: owner,
        reason,
    }) = target.commit_enable(owner)
    else {
        panic!()
    };
    assert_eq!(reason, cross);
    let Err(Refused {
        token: owner,
        reason,
    }) = target.retire_enable(owner)
    else {
        panic!()
    };
    assert_eq!(reason, cross);
    let _ = source.retire_enable(owner).unwrap();

    let lease = source
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    let Err(Refused {
        token: lease,
        reason,
    }) = target.release_normal_io(lease)
    else {
        panic!()
    };
    assert_eq!(reason, cross);
    let _ = source.release_normal_io(lease).unwrap();

    let _ = source.tighten_inhibit(InhibitReason::Boot);
    let permit = source.capture_inhibit_release(InhibitReason::Boot).unwrap();
    let Err(InhibitReleaseError::CrossDomain(permit)) = target.try_release_inhibit(permit) else {
        panic!()
    };
    let _ = source.try_release_inhibit(permit).unwrap();

    let observation = source.begin_safety_observation(SafetyOperation::ConfirmOff);
    let Err(SafetyObservationError::CrossDomain(observation)) =
        target.complete_safety_observation(observation)
    else {
        panic!()
    };
    let _ = source.complete_safety_observation(observation).unwrap();

    assert_eq!(raw(&target), before, "跨 domain 拒绝不改任何位");
}

#[test]
fn abandon_repair_returns_exact_owner_and_never_changes_authority_word() {
    for finalizing in [false, true] {
        let authority = fresh(false, S::TorqueOffConfirmed);
        let pending = authority.reserve_clear().unwrap();
        let owner = if finalizing {
            let finalizing = authority.commit_clear(pending).unwrap();
            let _ = authority.estop();
            ClearOwner::Finalizing(finalizing)
        } else {
            ClearOwner::Pending(pending)
        };
        let expected = (
            owner.domain(),
            owner.epoch(),
            owner.original_latched(),
            owner.expected_phase(),
        );
        let repair = authority.begin_repair(owner).unwrap();
        let _ = authority.shutdown();
        let before = raw(&authority);
        let returned = authority.abandon_repair(repair).unwrap();
        assert_eq!(
            (
                returned.domain(),
                returned.epoch(),
                returned.original_latched(),
                returned.expected_phase()
            ),
            expected
        );
        assert_eq!(raw(&authority), before);
        let _ = authority.estop();
        let repair = authority.begin_repair(returned).unwrap();
        assert_eq!(repair.target(), RepairTarget::Latched);
        let cleanup = authority.authorize_repair_cleanup(repair).unwrap();
        authority.retire_clear_cleanup(cleanup).unwrap();
    }
}

#[test]
fn abandon_repair_wrong_domain_returns_unchanged_permit_for_original_authority() {
    let a = fresh(false, S::TorqueOffConfirmed);
    let b = fresh(false, S::TorqueOffConfirmed);
    let owner = a.reserve_clear().unwrap();
    let repair = a.begin_repair(ClearOwner::Pending(owner)).unwrap();
    let expected = (
        repair.owner().epoch(),
        repair.target(),
        repair.sampled_epoch(),
    );
    let before = (raw(&a), raw(&b));
    let refused = b.abandon_repair(repair).unwrap_err();
    assert!(matches!(refused.reason, Refusal::CrossDomain { .. }));
    assert_eq!((raw(&a), raw(&b)), before);
    assert_eq!(
        (
            refused.token.owner().epoch(),
            refused.token.target(),
            refused.token.sampled_epoch()
        ),
        expected
    );
    let returned = a.abandon_repair(refused.token).unwrap();
    let repair = a.begin_repair(returned).unwrap();
    let cleanup = a.authorize_repair_cleanup(repair).unwrap();
    a.retire_clear_cleanup(cleanup).unwrap();
}

#[test]
fn retire_unprepared_clear_changes_only_phase_across_all_tightenings() {
    for tightening in 0..6 {
        let authority = at_epoch(
            false,
            S::TorqueOffConfirmed,
            if tightening == 5 {
                word::EPOCH_MAX - 1
            } else {
                1
            },
        );
        let owner = authority.reserve_clear().unwrap();
        let held_epoch = owner.epoch();
        match tightening {
            0 => {}
            1 | 5 => {
                let _ = authority.disable();
            }
            2 => {
                let _ = authority.estop();
            }
            3 => {
                let _ = authority.shutdown();
            }
            4 => {
                let _ = authority.tighten_inhibit(InhibitReason::ALL[0]);
            }
            _ => unreachable!(),
        }
        let before = authority.load();
        let retired = authority.retire_unprepared_clear(owner).unwrap();
        assert_eq!(raw(&authority), before.with_phase(Phase::Idle).raw());
        assert_eq!(
            retired.superseded,
            before.epoch() != held_epoch.get() || before.has(EXHAUSTED)
        );
        assert_eq!(retired.latch, before.has(LATCH));
    }
}

#[test]
fn retire_unprepared_clear_cas_retry_preserves_new_protection_and_active_io() {
    let authority = fresh(false, S::TorqueOffConfirmed);
    let owner = authority.reserve_clear().unwrap();
    let lease = authority
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    let latest = Arc::new(AtomicU64::new(0));
    let captured = Arc::clone(&latest);
    install_hook(
        &authority,
        once(move |authority| {
            let _ = authority.estop();
            let _ = authority.shutdown();
            let _ = authority.tighten_inhibit(InhibitReason::ALL[0]);
            captured.store(raw(authority), Ordering::Release);
        }),
    );
    let retired = authority.retire_unprepared_clear(owner).unwrap();
    let before = Word::new(latest.load(Ordering::Acquire));
    assert_eq!(raw(&authority), before.with_phase(Phase::Idle).raw());
    assert!(retired.superseded && retired.latch);
    assert!(authority.snapshot().normal_io_active);
    authority.release_normal_io(lease).unwrap();
}

#[test]
fn retire_unprepared_clear_wrong_domain_preserves_both_words_and_owner() {
    let a = fresh(true, S::TorqueOffConfirmed);
    let b = fresh(false, S::TorqueOffConfirmed);
    let owner = a.reserve_clear().unwrap();
    let expected = (owner.domain(), owner.epoch(), owner.original_latched());
    let before = (raw(&a), raw(&b));
    let refused = b.retire_unprepared_clear(owner).unwrap_err();
    assert!(matches!(refused.reason, Refusal::CrossDomain { .. }));
    assert_eq!((raw(&a), raw(&b)), before);
    assert_eq!(
        (
            refused.token.domain(),
            refused.token.epoch(),
            refused.token.original_latched()
        ),
        expected
    );
    assert!(a.retire_unprepared_clear(refused.token).unwrap().latch);
}

#[test]
fn retire_unprepared_clear_wrong_or_corrupted_phase_returns_original_owner() {
    let authority = fresh(false, S::TorqueOffConfirmed);
    let mut owner = authority.reserve_clear().unwrap();
    let expected = owner.epoch();
    for wrong in [Phase::Idle, Phase::ClearFinalizing, Phase::EnableExecuting] {
        rewrite(&authority, |word| word.with_phase(wrong));
        let before = raw(&authority);
        let refused = authority.retire_unprepared_clear(owner).unwrap_err();
        assert_eq!(refused.reason, Refusal::WrongPhase(wrong));
        assert_eq!(raw(&authority), before);
        owner = refused.token;
        assert_eq!(owner.epoch(), expected);
    }
    rewrite(&authority, |word| Word::new(word.raw() | word::PHASE_MASK));
    let before = raw(&authority);
    let refused = authority.retire_unprepared_clear(owner).unwrap_err();
    assert_eq!(refused.reason, Refusal::CorruptedPhase);
    assert_eq!(raw(&authority), before);
    rewrite(&authority, |word| word.with_phase(Phase::ClearPreparing));
    authority.retire_unprepared_clear(refused.token).unwrap();
}

#[test]
fn observed_cas_counts_real_same_authority_interference_and_keep() {
    for failures in [0, 1, 4] {
        let authority = fresh(false, S::TorqueOffConfirmed);
        let mut remaining = failures;
        install_hook(&authority, move |same| {
            if remaining != 0 {
                remaining -= 1;
                let _ = same.disable();
            }
        });
        let result = authority.estop_observed();
        assert_eq!(result.failed_cas, Some(failures));
        assert!(authority.snapshot().latch && authority.snapshot().disabled);
        assert_eq!(epoch(&authority), 2 + failures);
    }
    let authority = fresh(false, S::TorqueOffConfirmed);
    let owner = succeeded_enable(&authority);
    authority.commit_enable(owner).unwrap();
    install_hook(
        &authority,
        once(|same| {
            let _ = same.disable();
        }),
    );
    let observed = authority.acquire_normal_io_observed(NormalIoIntent::Motion);
    assert_eq!(observed.failed_cas, Some(1));
    assert_eq!(observed.outcome.unwrap_err(), Refusal::Disabled);
    assert!(!authority.snapshot().normal_io_active);
}

#[test]
fn observed_five_entries_preserve_real_outcomes_and_cross_domain_lease() {
    let authority = fresh(false, S::TorqueOffConfirmed);
    let acquired = authority.acquire_normal_io_observed(NormalIoIntent::PeriodicRead);
    assert_eq!(acquired.failed_cas, Some(0));
    let lease = acquired.outcome.unwrap();
    let foreign = fresh(false, S::TorqueOffConfirmed);
    let before = (raw(&authority), raw(&foreign));
    let rejected = foreign.release_normal_io_observed(lease);
    assert_eq!(rejected.failed_cas, Some(0));
    assert_eq!((raw(&authority), raw(&foreign)), before);
    let original = rejected.outcome.unwrap_err();
    assert!(matches!(original.reason, Refusal::CrossDomain { .. }));
    let stopped = authority.estop_observed();
    assert_eq!(stopped.failed_cas, Some(0));
    assert!(stopped.outcome.normal_io_active);
    for serial in [true, false] {
        let claim = if serial {
            authority.claim_serial_estop_observed()
        } else {
            authority.claim_ipc_estop_observed()
        };
        assert_eq!(claim.failed_cas, Some(0));
        assert!(claim.outcome);
        let repeated = if serial {
            authority.claim_serial_estop_observed()
        } else {
            authority.claim_ipc_estop_observed()
        };
        assert_eq!(repeated.failed_cas, Some(0));
        assert!(!repeated.outcome);
    }
    let released = authority.release_normal_io_observed(original.token);
    assert_eq!(released.failed_cas, Some(0));
    assert!(released.outcome.unwrap().superseded);
    assert!(!authority.snapshot().normal_io_active);
    assert!(authority.snapshot().latch);

    let plain = fresh(false, S::TorqueOffConfirmed);
    let observed = fresh(false, S::TorqueOffConfirmed);
    let _ = plain.estop();
    let _ = observed.estop_observed().outcome;
    assert_eq!(raw(&plain), raw(&observed));
    assert_eq!(
        plain.claim_serial_estop(),
        observed.claim_serial_estop_observed().outcome
    );
    assert_eq!(
        plain.claim_ipc_estop(),
        observed.claim_ipc_estop_observed().outcome
    );
    let a = plain
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    let b = observed
        .acquire_normal_io_observed(NormalIoIntent::PeriodicRead)
        .outcome
        .unwrap();
    assert_eq!(raw(&plain), raw(&observed));
    let _ = plain.release_normal_io(a).unwrap();
    let _ = observed.release_normal_io_observed(b).outcome.unwrap();
    assert_eq!(raw(&plain), raw(&observed));
}
