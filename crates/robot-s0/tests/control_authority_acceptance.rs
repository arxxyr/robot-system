//! `control_authority` 公开 API 验收与真实 allocation guard。
//!
//! 只使用 `robot_s0::control_authority` 的公开接口，不依赖模块内 CAS hook。
//! S48 A3a 起本体位于 `robot_core::control_authority`，`robot_s0::control_authority`
//! 是同一模块的重导出；本文件另验证两条路径是同一类型、令牌可交叉传递、
//! 交错构造的 domain 来自唯一计数器。
//!
//! - 默认 features：公开 API 反例（Enable / Clear / IO / inhibit / 观测 / 跨 domain）。
//! - `test-comm-fault-hook`：epoch MAX 用例（需要 `new_with_permission_epoch` 验收构造）。
//! - `rt_guard`：本二进制安装真实 `assert_no_alloc::AllocDisabler` 作 global
//!   allocator；三个独立子进程分别证明 alloc / realloc / dealloc 在 scope 内实际
//!   abort，再让全部热路径原语在同一 scope 内跑通（含 abandon_cleanup 往返与已
//!   提交 Enable 的 Motion 租约）。`rt_guard` + `test-comm-fault-hook` 同时开启时，
//!   epoch MAX 的耗尽拒绝、旧 false 修复被新 EStop 否决、收紧 / lease 归还 / owner
//!   退休也在真实 guard scope 内执行。debug / release 均有效
//!   （workspace 对 `assert_no_alloc` 关闭 `disable_release`）。
//!
//! ```bash
//! cargo +nightly test --locked -p robot-s0 --features "rt_guard test-comm-fault-hook" \
//!     --test control_authority_acceptance -- --test-threads=1
//! cargo +nightly test --locked --release -p robot-s0 --features "rt_guard test-comm-fault-hook" \
//!     --test control_authority_acceptance -- --test-threads=1
//! ```

#![allow(clippy::doc_markdown)]

use robot_core::executor_ctrl::{EStopOutcome, OutputSafetyState as S};
use robot_s0::control_authority::{
    AuthorityInit, CleanupKind, ClearOwner, ClearRetirement, ControlAuthority, EnableFinish,
    EnableResult, EnableRetired, InhibitReason, InhibitReleaseError, InhibitSet,
    NonEnergizingControl, NormalIoIntent, ObservedField, PermissionEpoch, Phase, Refusal, Refused,
    RepairTarget, SafetyObservationError, SafetyOperation, SafetyTightening,
};

#[cfg(feature = "rt_guard")]
#[global_allocator]
static ALLOC_GUARD: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

fn authority(latch: bool, safety: S) -> ControlAuthority {
    ControlAuthority::new(AuthorityInit::new(latch, safety, InhibitSet::EMPTY)).unwrap()
}

fn epoch(authority: &ControlAuthority) -> u64 {
    authority.snapshot().epoch.get()
}

fn phase(authority: &ControlAuthority) -> Phase {
    authority
        .snapshot()
        .phase
        .expect("本模块从不产生非法 Phase")
}

fn epoch_at(raw: u64) -> PermissionEpoch {
    PermissionEpoch::new(raw).unwrap()
}

/// 走到 Succeeded + TorqueOn 已发布。
fn succeeded_enable(authority: &ControlAuthority) -> robot_s0::control_authority::EnableOwner {
    let (owner, view) = authority.reserve_enable().unwrap();
    let execution = authority.start_enable(view).unwrap();
    let finish = authority
        .finish_enable(execution, EnableResult::Succeeded)
        .unwrap();
    assert!(finish.torque_on_published && !finish.superseded);
    owner
}

// ---------------------------------------------------------------------------
// Enable
// ---------------------------------------------------------------------------

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "一次完整走通 Enable 生命周期的确定性反例，拆分会丢失顺序语义"
)]
fn enable_starts_once_finishes_after_io_release_and_needs_disabled_again() {
    let authority = authority(false, S::TorqueOffConfirmed);
    let (owner, view) = authority.reserve_enable().unwrap();
    assert_eq!(owner.epoch(), view.epoch());
    assert_eq!(view.epoch(), epoch_at(2));
    assert_eq!(
        authority.reserve_enable().err(),
        Some(Refusal::Busy(Phase::EnableQueued))
    );
    let copied = view;
    let mut execution = authority.start_enable(view).unwrap();
    assert_eq!(
        authority.start_enable(copied).err(),
        Some(Refusal::WrongPhase(Phase::EnableExecuting)),
        "复制的 StartView 不能重启"
    );
    let step = execution.view();
    for _ in 0..3 {
        let lease = authority
            .acquire_normal_io(NormalIoIntent::EnableStep(step))
            .unwrap();
        assert_eq!(
            authority
                .acquire_normal_io(NormalIoIntent::PeriodicRead)
                .err(),
            Some(Refusal::NormalIoActive)
        );
        let Err(Refused { token, reason }) =
            authority.finish_enable(execution, EnableResult::Succeeded)
        else {
            panic!("持有 wire 租约时不得进入终态");
        };
        assert_eq!(reason, Refusal::NormalIoActive);
        execution = token;
        assert!(!authority.release_normal_io(lease).unwrap().superseded);
        assert_eq!(phase(&authority), Phase::EnableExecuting);
    }
    assert_eq!(
        authority.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::Disabled)
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
            .acquire_normal_io(NormalIoIntent::EnableStep(step))
            .err(),
        Some(Refusal::WrongPhase(Phase::EnableSucceeded))
    );
    let committed = authority.commit_enable(owner).unwrap();
    assert_eq!(committed.epoch, epoch_at(3));
    let snapshot = authority.snapshot();
    assert!(!snapshot.disabled);
    assert_eq!(snapshot.phase, Some(Phase::Idle));
    let lease = authority.acquire_normal_io(NormalIoIntent::Motion).unwrap();
    let _ = authority.release_normal_io(lease).unwrap();
    assert_eq!(
        authority.start_enable(copied).err(),
        Some(Refusal::WrongPhase(Phase::Idle))
    );
    assert_eq!(
        authority.reserve_enable().err(),
        Some(Refusal::NotDisabled),
        "已提交 Enable 之后不再占用底层使能事务"
    );
    assert_eq!(authority.disable().epoch, epoch_at(4));
    let (owner, _view) = authority.reserve_enable().unwrap();
    assert_eq!(owner.epoch(), epoch_at(5));
    assert_eq!(
        authority.start_enable(copied).err(),
        Some(Refusal::StaleEpoch {
            held: epoch_at(2),
            current: epoch_at(5)
        })
    );
    assert_eq!(
        authority.retire_enable(owner).unwrap(),
        EnableRetired {
            retired_from: Phase::EnableQueued,
            superseded: false
        }
    );
}

#[test]
fn executing_cannot_retire_early_and_failure_never_publishes_torque_on() {
    let authority = authority(false, S::TorqueOffConfirmed);
    let (owner, view) = authority.reserve_enable().unwrap();
    let execution = authority.start_enable(view).unwrap();
    let Err(Refused {
        token: owner,
        reason,
    }) = authority.retire_enable(owner)
    else {
        panic!("Executing 不得提前退休");
    };
    assert_eq!(reason, Refusal::WrongPhase(Phase::EnableExecuting));
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
    assert_eq!(authority.snapshot().safety, S::TorqueOffConfirmed);
    let Err(Refused {
        token: owner,
        reason,
    }) = authority.commit_enable(owner)
    else {
        panic!("失败终态不能提交");
    };
    assert_eq!(reason, Refusal::WrongPhase(Phase::EnableFailed));
    assert_eq!(
        authority.retire_enable(owner).unwrap(),
        EnableRetired {
            retired_from: Phase::EnableFailed,
            superseded: false
        }
    );
    assert!(authority.snapshot().disabled);
}

#[test]
fn disable_after_hardware_success_only_allows_retirement() {
    let authority = authority(false, S::TorqueOffConfirmed);
    let owner = succeeded_enable(&authority);
    let _ = authority.disable();
    let Err(Refused {
        token: owner,
        reason,
    }) = authority.commit_enable(owner)
    else {
        panic!("Disable 到达后旧成功不能提交 ActiveForward");
    };
    assert_eq!(
        reason,
        Refusal::StaleEpoch {
            held: epoch_at(2),
            current: epoch_at(3)
        }
    );
    assert_eq!(
        authority.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::Disabled)
    );
    assert_eq!(
        authority.snapshot().safety,
        S::TorqueOn,
        "真实硬件结果如实记录"
    );
    assert_eq!(
        authority.retire_enable(owner).unwrap(),
        EnableRetired {
            retired_from: Phase::EnableSucceeded,
            superseded: true
        }
    );
    assert!(authority.snapshot().disabled);
}

#[test]
fn estop_during_queued_keeps_the_placeholder_until_the_owner_retires() {
    let authority = authority(false, S::TorqueOffConfirmed);
    let (owner, view) = authority.reserve_enable().unwrap();
    let report = authority.estop();
    assert_eq!(report.outcome, EStopOutcome::StopIssued);
    assert!(!report.normal_io_active);
    assert_eq!(phase(&authority), Phase::EnableQueued);
    assert_eq!(
        authority.start_enable(view).err(),
        Some(Refusal::StaleEpoch {
            held: epoch_at(2),
            current: epoch_at(3)
        })
    );
    assert_eq!(
        authority.reserve_enable().err(),
        Some(Refusal::Busy(Phase::EnableQueued))
    );
    assert_eq!(
        authority.reserve_clear().err(),
        Some(Refusal::Busy(Phase::EnableQueued))
    );
    assert_eq!(
        authority.retire_enable(owner).unwrap(),
        EnableRetired {
            retired_from: Phase::EnableQueued,
            superseded: true
        }
    );
    assert_eq!(phase(&authority), Phase::Idle);
    assert!(authority.snapshot().latch);
    assert_eq!(authority.reserve_enable().err(), Some(Refusal::Latched));
}

#[test]
fn both_same_epoch_confirm_interleavings_are_refused_deterministically() {
    // 交错一：Executing 期间的旧 Confirm 不能在 finish 发布 On 之后写回 Off。
    let authority = authority(false, S::StopCommanded);
    let (owner, view) = authority.reserve_enable().unwrap();
    let execution = authority.start_enable(view).unwrap();
    let stale = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
    let finish = authority
        .finish_enable(execution, EnableResult::Succeeded)
        .unwrap();
    assert!(finish.torque_on_published);
    assert_eq!(epoch(&authority), 2);
    let Err(SafetyObservationError::Rejected(reason)) =
        authority.complete_safety_observation(stale)
    else {
        panic!("旧 Confirm 不得覆盖 Succeeded / On");
    };
    assert_eq!(reason, Refusal::ObservationStale(ObservedField::Phase));
    assert_eq!(authority.snapshot().safety, S::TorqueOn);
    let _ = authority.commit_enable(owner).unwrap();

    // 交错二：Succeeded 之后新的真实 Confirm 已见 Off，旧 Enable 不得提交。
    let authority = self::authority(false, S::TorqueOffConfirmed);
    let owner = succeeded_enable(&authority);
    let observation = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
    let committed = authority.complete_safety_observation(observation).unwrap();
    assert_eq!(committed.previous, S::TorqueOn);
    assert_eq!(committed.safety, S::TorqueOffConfirmed);
    let Err(Refused {
        token: owner,
        reason,
    }) = authority.commit_enable(owner)
    else {
        panic!("当前 safety 不是 TorqueOn 时不得提交旧使能");
    };
    assert_eq!(reason, Refusal::SafetyNotTorqueOn(S::TorqueOffConfirmed));
    assert!(authority.snapshot().disabled);
    let _ = authority.retire_enable(owner).unwrap();
}

#[test]
fn dropped_owner_keeps_the_authority_stuck_but_tightenable() {
    let authority = authority(false, S::TorqueOffConfirmed);
    {
        // owner 与 view 在本作用域结束时被丢弃；Drop 不解除任何位。
        let (_owner, _view) = authority.reserve_enable().unwrap();
    }
    assert_eq!(phase(&authority), Phase::EnableQueued, "Drop 不解除任何位");
    assert_eq!(
        authority.reserve_enable().err(),
        Some(Refusal::Busy(Phase::EnableQueued))
    );
    let _ = authority.estop();
    assert!(authority.snapshot().latch && authority.snapshot().disabled);
    let _ = authority.tighten_safety(SafetyTightening::StopFailed);
    assert_eq!(authority.snapshot().safety, S::StopFailed);
}

// ---------------------------------------------------------------------------
// Clear
// ---------------------------------------------------------------------------

#[test]
fn clear_commits_then_finalizes_and_a_late_disable_does_not_roll_it_back() {
    let authority = authority(true, S::TorqueOffConfirmed);
    let pending = authority.reserve_clear().unwrap();
    assert!(pending.original_latched());
    assert_eq!(pending.epoch(), epoch_at(2));
    let finalizing = authority.commit_clear(pending).unwrap();
    assert_eq!(finalizing.epoch(), epoch_at(3));
    let snapshot = authority.snapshot();
    assert!(!snapshot.latch && snapshot.disabled);
    assert_eq!(snapshot.phase, Some(Phase::ClearFinalizing));
    let _ = authority.disable();
    let Err(Refused { token, reason }) = authority.begin_repair(ClearOwner::Finalizing(finalizing))
    else {
        panic!("Latch=false 的 Finalizing 不需要修复");
    };
    assert_eq!(reason, Refusal::RepairNotNeeded);
    let ClearOwner::Finalizing(finalizing) = token else {
        panic!("归还同一 owner");
    };
    let permit = authority.authorize_finalize(finalizing).unwrap();
    assert_eq!(permit.kind(), CleanupKind::Finalize);
    assert_eq!(
        authority.retire_clear_cleanup(permit).unwrap(),
        ClearRetirement {
            kind: CleanupKind::Finalize,
            logical_committed: true,
            superseded: true,
            latch: false
        }
    );
    assert_eq!(phase(&authority), Phase::Idle);
    assert_eq!(
        authority.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::Disabled),
        "后到 Disable 使运动仍被禁止"
    );
    let (owner, _view) = authority.reserve_enable().unwrap();
    assert_eq!(owner.epoch(), epoch_at(5));
}

#[test]
fn pre_commit_revocations_repair_to_the_required_target() {
    // 新 EStop：目标 Latched。
    let authority = authority(true, S::TorqueOffConfirmed);
    let pending = authority.reserve_clear().unwrap();
    let _ = authority.estop();
    let Err(Refused {
        token: pending,
        reason,
    }) = authority.commit_clear(pending)
    else {
        panic!("提交前的新 EStop 撤销恢复权限");
    };
    assert_eq!(
        reason,
        Refusal::StaleEpoch {
            held: epoch_at(2),
            current: epoch_at(3)
        }
    );
    assert_eq!(phase(&authority), Phase::ClearPreparing, "阶段占位保留");
    let permit = authority
        .begin_repair(ClearOwner::Pending(pending))
        .unwrap();
    assert_eq!(permit.target(), RepairTarget::Latched);
    let cleanup = authority.authorize_repair_cleanup(permit).unwrap();
    assert_eq!(
        authority.retire_clear_cleanup(cleanup).unwrap(),
        ClearRetirement {
            kind: CleanupKind::Repaired(RepairTarget::Latched),
            logical_committed: false,
            superseded: true,
            latch: true
        }
    );

    // Disable：只有原 false 才能恢复 Original(false)。
    for original in [false, true] {
        let authority = self::authority(original, S::TorqueOffConfirmed);
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
        assert_eq!(permit.target(), expected);
        let cleanup = authority.authorize_repair_cleanup(permit).unwrap();
        let retirement = authority.retire_clear_cleanup(cleanup).unwrap();
        assert_eq!(retirement.kind, CleanupKind::Repaired(expected));
        assert!(!retirement.logical_committed && retirement.superseded);
        assert_eq!(retirement.latch, original, "Disable 不创造 EStop");
    }
}

#[test]
fn post_commit_estop_blocks_finalize_and_a_granted_permit_is_abandoned_for_repair() {
    let authority = authority(true, S::TorqueOffConfirmed);
    let finalizing = authority
        .commit_clear(authority.reserve_clear().unwrap())
        .unwrap();
    let _ = authority.estop();
    let Err(Refused {
        token: finalizing,
        reason,
    }) = authority.authorize_finalize(finalizing)
    else {
        panic!("Latch=true 时不得 Finalize");
    };
    assert_eq!(reason, Refusal::Latched);
    let permit = authority
        .begin_repair(ClearOwner::Finalizing(finalizing))
        .unwrap();
    assert_eq!(permit.target(), RepairTarget::Latched);
    let cleanup = authority.authorize_repair_cleanup(permit).unwrap();
    assert_eq!(
        authority.retire_clear_cleanup(cleanup).unwrap(),
        ClearRetirement {
            kind: CleanupKind::Repaired(RepairTarget::Latched),
            logical_committed: true,
            superseded: true,
            latch: true
        }
    );

    // 许可已授、unlink 之前到达 EStop：原语先行、持久化随后，许可放弃后重新修复。
    let authority = self::authority(true, S::TorqueOffConfirmed);
    let finalizing = authority
        .commit_clear(authority.reserve_clear().unwrap())
        .unwrap();
    let permit = authority.authorize_finalize(finalizing).unwrap();
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
fn repair_permit_expiry_returns_the_same_owner_and_keeps_the_placeholder() {
    let authority = authority(true, S::TorqueOffConfirmed);
    let pending = authority.reserve_clear().unwrap();
    let permit = authority
        .begin_repair(ClearOwner::Pending(pending))
        .unwrap();
    assert_eq!(permit.sampled_epoch(), epoch_at(2));
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
            held: epoch_at(2),
            current: epoch_at(3)
        }
    );
    assert_eq!(phase(&authority), Phase::ClearPreparing);
    let permit = authority.begin_repair(owner).unwrap();
    assert_eq!(permit.sampled_epoch(), epoch_at(3));
    let cleanup = authority.authorize_repair_cleanup(permit).unwrap();
    let _ = authority.retire_clear_cleanup(cleanup).unwrap();
    assert_eq!(phase(&authority), Phase::Idle);
}

#[test]
fn clear_gates_hold_and_health_reads_survive_every_clear_phase() {
    let authority = authority(true, S::StopCommanded);
    assert_eq!(
        authority.reserve_clear().err(),
        Some(Refusal::SafetyNotOffConfirmed(S::StopCommanded))
    );
    let observation = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
    let _ = authority.complete_safety_observation(observation).unwrap();
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
    let _ = authority.shutdown();
    assert_eq!(authority.reserve_clear().err(), Some(Refusal::Shutdown));
    assert!(
        authority
            .acquire_normal_io(NormalIoIntent::NonEnergizingControl(
                NonEnergizingControl::Disable
            ))
            .is_ok(),
        "Shutdown 后非上扭控制仍只受基础门"
    );
}

// ---------------------------------------------------------------------------
// 跨 domain
// ---------------------------------------------------------------------------

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "逐个令牌类型验证跨 domain 拒绝且不改位，顺序依赖同一对权威"
)]
fn cross_domain_tokens_are_refused_returned_and_change_nothing() {
    let source = authority(true, S::TorqueOffConfirmed);
    let target = authority(true, S::TorqueOffConfirmed);
    let cross = Refusal::CrossDomain {
        token: source.domain(),
        authority: target.domain(),
    };
    let before = target.snapshot();

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

    assert_eq!(target.snapshot(), before, "跨 domain 拒绝不改任何位");
}

// ---------------------------------------------------------------------------
// A3a：core / s0 两条公开路径是同一模块
// ---------------------------------------------------------------------------

#[test]
fn core_and_s0_paths_name_the_same_types_and_tokens_cross_freely() {
    fn same_type<T>(_: &T, _: &T) {}
    fn takes_core_snapshot(
        snapshot: robot_core::control_authority::AuthoritySnapshot,
    ) -> robot_core::control_authority::DomainId {
        snapshot.domain
    }
    let core_side = robot_core::control_authority::ControlAuthority::new(
        robot_core::control_authority::AuthorityInit::new(
            false,
            S::TorqueOffConfirmed,
            robot_core::control_authority::InhibitSet::EMPTY,
        ),
    )
    .unwrap();
    let s0_side: robot_s0::control_authority::ControlAuthority =
        authority(false, S::TorqueOffConfirmed);
    same_type(&core_side, &s0_side);

    // core 路径产生的 owner / view / lease 用 s0 路径的类型名接收并消费。
    let (owner, view): (
        robot_s0::control_authority::EnableOwner,
        robot_s0::control_authority::EnableStartView,
    ) = core_side.reserve_enable().unwrap();
    let execution: robot_core::control_authority::EnableExecution =
        core_side.start_enable(view).unwrap();
    let lease: robot_s0::control_authority::NormalIoLease = core_side
        .acquire_normal_io(robot_core::control_authority::NormalIoIntent::EnableStep(
            execution.view(),
        ))
        .unwrap();
    let released: robot_core::control_authority::NormalIoReleased =
        core_side.release_normal_io(lease).unwrap();
    assert!(!released.superseded);
    let finish = core_side
        .finish_enable(
            execution,
            robot_s0::control_authority::EnableResult::Succeeded,
        )
        .unwrap();
    assert!(finish.torque_on_published);
    let committed: robot_s0::control_authority::EnableCommitted =
        core_side.commit_enable(owner).unwrap();
    assert_eq!(committed.epoch, epoch_at(3));

    // 同一类型、不同 domain：s0 路径权威的视图在 core 路径权威上按跨 domain 拒绝。
    let (owner, view) = s0_side.reserve_enable().unwrap();
    assert_eq!(
        core_side.start_enable(view).err(),
        Some(robot_core::control_authority::Refusal::CrossDomain {
            token: s0_side.domain(),
            authority: core_side.domain(),
        })
    );
    let _ = s0_side.retire_enable(owner).unwrap();
    let snapshot: robot_s0::control_authority::AuthoritySnapshot = s0_side.snapshot();
    assert_eq!(takes_core_snapshot(snapshot), s0_side.domain());
}

#[test]
fn interleaved_construction_through_both_paths_yields_unique_domains_from_one_counter() {
    fn build(via_core: bool) -> robot_core::control_authority::DomainId {
        let init = robot_core::control_authority::AuthorityInit::new(
            false,
            S::TorqueUnknown,
            robot_core::control_authority::InhibitSet::EMPTY,
        );
        if via_core {
            robot_core::control_authority::ControlAuthority::new(init)
                .unwrap()
                .domain()
        } else {
            robot_s0::control_authority::ControlAuthority::new(init)
                .unwrap()
                .domain()
        }
    }
    // 顺序交错：严格递增即唯一且不回绕。
    let sequential: Vec<_> = (0..64).map(|index| build(index % 2 == 0)).collect();
    assert!(sequential.windows(2).all(|pair| pair[0] < pair[1]));
    // 多线程交错：全部唯一，且都晚于顺序构造的最后一个。
    let handles: Vec<_> = (0..8_u32)
        .map(|thread| {
            std::thread::spawn(move || {
                (0..64_u32)
                    .map(|index| build((thread + index) % 2 == 0))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut concurrent: Vec<_> = handles
        .into_iter()
        .flat_map(|handle| handle.join().unwrap())
        .collect();
    let total = concurrent.len();
    concurrent.sort_unstable();
    concurrent.dedup();
    assert_eq!(concurrent.len(), total, "domain 必须唯一");
    let last_sequential = *sequential.last().unwrap();
    assert!(concurrent.iter().all(|domain| *domain > last_sequential));
}

// ---------------------------------------------------------------------------
// 普通 IO / inhibit
// ---------------------------------------------------------------------------

#[test]
fn io_acquire_and_estop_are_ordered_by_the_same_word_in_both_directions() {
    // EStop → acquire：后来的运动获取失败。
    let authority = authority(false, S::TorqueOffConfirmed);
    let owner = succeeded_enable(&authority);
    let _ = authority.commit_enable(owner).unwrap();
    let lease = authority.acquire_normal_io(NormalIoIntent::Motion).unwrap();
    let _ = authority.release_normal_io(lease).unwrap();
    let report = authority.estop();
    assert_eq!(report.outcome, EStopOutcome::StopIssued);
    assert!(!report.normal_io_active);
    assert_eq!(
        authority.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::SerialEStopPending)
    );

    // acquire → EStop：已开始的事务沿既有安全责任退场，lease 跨 epoch 释放。
    let authority = self::authority(false, S::TorqueOffConfirmed);
    let owner = succeeded_enable(&authority);
    let _ = authority.commit_enable(owner).unwrap();
    let lease = authority.acquire_normal_io(NormalIoIntent::Motion).unwrap();
    assert_eq!(lease.epoch(), epoch_at(3));
    let report = authority.estop();
    assert!(report.normal_io_active);
    assert_eq!(report.tightened.epoch, epoch_at(4));
    assert_eq!(
        authority
            .acquire_normal_io(NormalIoIntent::PeriodicRead)
            .err(),
        Some(Refusal::SerialEStopPending)
    );
    let released = authority.release_normal_io(lease).unwrap();
    assert!(released.superseded);
    let snapshot = authority.snapshot();
    assert!(!snapshot.normal_io_active && snapshot.latch && snapshot.disabled);
    assert!(authority.claim_serial_estop());
    assert!(!authority.claim_serial_estop());
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
    assert!(authority.claim_ipc_estop());
    assert_eq!(
        epoch(&authority),
        4,
        "notice 消费与 IO 获取 / 释放不推进 epoch"
    );
}

#[test]
fn health_reads_are_separate_from_motion_permission_and_inhibits_stay_sticky() {
    let authority = ControlAuthority::new(AuthorityInit::new(
        true,
        S::TorqueOffConfirmed,
        InhibitSet::EMPTY
            .with(InhibitReason::Boot)
            .with(InhibitReason::CommFault),
    ))
    .unwrap();
    let blocked = Some(Refusal::Inhibited(
        InhibitSet::EMPTY
            .with(InhibitReason::Boot)
            .with(InhibitReason::CommFault),
    ));
    assert_eq!(
        authority
            .acquire_normal_io(NormalIoIntent::PeriodicRead)
            .err(),
        blocked,
        "健康读取不得绕过基础通信 / BOOT 门"
    );
    assert_eq!(
        authority
            .acquire_normal_io(NormalIoIntent::NonEnergizingControl(
                NonEnergizingControl::Disconnect
            ))
            .err(),
        blocked
    );
    assert_eq!(
        authority
            .capture_inhibit_release(InhibitReason::CommFault)
            .err(),
        Some(Refusal::InhibitSticky(InhibitReason::CommFault))
    );
    let stale = authority
        .capture_inhibit_release(InhibitReason::Boot)
        .unwrap();
    let _ = authority.disable();
    assert!(matches!(
        authority.try_release_inhibit(stale),
        Err(InhibitReleaseError::Expired(Refusal::StaleEpoch { .. }))
    ));
    let permit = authority
        .capture_inhibit_release(InhibitReason::Boot)
        .unwrap();
    let released = authority.try_release_inhibit(permit).unwrap();
    assert_eq!(
        released.remaining,
        InhibitSet::EMPTY.with(InhibitReason::CommFault)
    );
    assert_eq!(released.epoch, epoch_at(3));
    assert_eq!(
        authority
            .acquire_normal_io(NormalIoIntent::PeriodicRead)
            .err(),
        Some(Refusal::Inhibited(
            InhibitSet::EMPTY.with(InhibitReason::CommFault)
        )),
        "sticky 抑制保持"
    );
    let read_only = self::authority(true, S::TorqueOffConfirmed);
    let read = read_only
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    let _ = read_only.release_normal_io(read).unwrap();
    assert_eq!(
        read_only.acquire_normal_io(NormalIoIntent::Motion).err(),
        Some(Refusal::Latched),
        "Latch 下保留健康读取但拒绝运动"
    );
}

// ---------------------------------------------------------------------------
// typed safety / 观测
// ---------------------------------------------------------------------------

#[test]
fn typed_safety_transitions_are_finite_and_faults_cannot_be_relaxed_by_observation() {
    let authority = authority(false, S::TorqueOn);
    let report = authority.tighten_safety(SafetyTightening::StopCommanded);
    assert_eq!(
        (report.previous, report.safety),
        (S::TorqueOn, S::StopCommanded)
    );
    assert_eq!(report.tightened.epoch, epoch_at(2));
    let report = authority.tighten_safety(SafetyTightening::AdapterUncertain);
    assert_eq!(report.safety, S::AdapterUncertain);
    let report = authority.tighten_safety(SafetyTightening::StopCommanded);
    assert_eq!(
        report.safety,
        S::AdapterUncertain,
        "StopCommanded 不覆盖 Uncertain"
    );
    let report = authority.tighten_safety(SafetyTightening::StopFailed);
    assert_eq!(report.safety, S::StopFailed);
    let report = authority.tighten_safety(SafetyTightening::AdapterUncertain);
    assert_eq!(report.safety, S::StopFailed, "Uncertain 不覆盖 Failed");
    assert_eq!(report.tightened.epoch, epoch_at(6));
    for operation in [SafetyOperation::ConfirmOff, SafetyOperation::Disable] {
        let observation = authority.begin_safety_observation(operation);
        assert!(matches!(
            authority.complete_safety_observation(observation),
            Err(SafetyObservationError::Rejected(
                Refusal::SafetyNotRelaxable(S::StopFailed)
            ))
        ));
    }
    let observation = authority.begin_safety_observation(SafetyOperation::Disconnect);
    let committed = authority.complete_safety_observation(observation).unwrap();
    assert_eq!(
        committed.safety,
        S::StopFailed,
        "断连不冒充断扭，也不放宽故障"
    );
    assert_eq!(committed.tightened.map(|t| t.epoch), Some(epoch_at(7)));
    assert_eq!(
        authority.reserve_enable().err(),
        Some(Refusal::SafetyNotRelaxable(S::StopFailed))
    );
}

#[test]
fn stale_observations_are_consumed_and_disable_observation_needs_disabled() {
    let authority = authority(false, S::TorqueOffConfirmed);
    let owner = succeeded_enable(&authority);
    let _ = authority.commit_enable(owner).unwrap();
    assert!(!authority.snapshot().disabled);
    let observation = authority.begin_safety_observation(SafetyOperation::Disable);
    assert!(matches!(
        authority.complete_safety_observation(observation),
        Err(SafetyObservationError::Rejected(Refusal::NotDisabled))
    ));
    assert_eq!(authority.snapshot().safety, S::TorqueOn);
    let stale = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
    let _ = authority.disable();
    assert!(matches!(
        authority.complete_safety_observation(stale),
        Err(SafetyObservationError::Rejected(Refusal::ObservationStale(
            ObservedField::Epoch
        )))
    ));
    assert_eq!(authority.snapshot().safety, S::TorqueOn, "过期结果不能重报");
    let observation = authority.begin_safety_observation(SafetyOperation::Disable);
    let committed = authority.complete_safety_observation(observation).unwrap();
    assert_eq!(committed.safety, S::TorqueOffConfirmed);
    assert_eq!(committed.tightened, None);
    assert_eq!(epoch(&authority), 4);

    let disconnect = authority.begin_safety_observation(SafetyOperation::Disconnect);
    let committed = authority.complete_safety_observation(disconnect).unwrap();
    assert_eq!(committed.safety, S::TorqueUnknown);
    assert_eq!(committed.tightened.map(|t| t.epoch), Some(epoch_at(5)));
    assert!(authority.snapshot().disabled);
}

// ---------------------------------------------------------------------------
// epoch MAX（需要验收构造）
// ---------------------------------------------------------------------------

/// 验收构造：距 `PermissionEpoch::MAX` 还差 `below` 代次的权威。
#[cfg(feature = "test-comm-fault-hook")]
fn near_max(latch: bool, safety: S, below: u64) -> ControlAuthority {
    ControlAuthority::new_with_permission_epoch(
        AuthorityInit::new(latch, safety, InhibitSet::EMPTY),
        PermissionEpoch::new(PermissionEpoch::MAX.get() - below).unwrap(),
    )
    .unwrap()
}

#[cfg(feature = "test-comm-fault-hook")]
mod max_epoch {
    use super::*;
    use robot_s0::control_authority::Tightened;

    #[test]
    fn estop_at_max_vetoes_the_old_unlatched_repair_and_old_confirm_but_retires() {
        let authority = near_max(false, S::TorqueOffConfirmed, 1);
        let pending = authority.reserve_clear().unwrap();
        assert_eq!(pending.epoch(), PermissionEpoch::MAX);
        assert!(!authority.snapshot().exhausted);
        let permit = authority
            .begin_repair(ClearOwner::Pending(pending))
            .unwrap();
        assert_eq!(permit.target(), RepairTarget::OriginalUnlatched);
        let stale_confirm = authority.begin_safety_observation(SafetyOperation::ConfirmOff);

        let report = authority.estop();
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
        assert_eq!(
            authority.capture_inhibit_release(InhibitReason::Boot).err(),
            Some(Refusal::Exhausted)
        );
        let tightened = authority.tighten_safety(SafetyTightening::StopFailed);
        assert_eq!(tightened.safety, S::StopFailed);
        assert!(!tightened.tightened.advanced);
        assert!(authority.claim_serial_estop());

        let repair = authority.begin_repair(owner).unwrap();
        assert_eq!(repair.target(), RepairTarget::Latched);
        let cleanup = authority.authorize_repair_cleanup(repair).unwrap();
        assert_eq!(
            authority.retire_clear_cleanup(cleanup).unwrap(),
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
    fn grant_paths_at_max_fail_closed_and_existing_owners_still_retire() {
        let authority = near_max(false, S::TorqueOffConfirmed, 0);
        assert_eq!(
            authority.reserve_enable().err(),
            Some(Refusal::EpochExhausted)
        );
        assert!(authority.snapshot().exhausted && authority.snapshot().disabled);
        assert_eq!(authority.reserve_enable().err(), Some(Refusal::Exhausted));

        let authority = near_max(false, S::TorqueOffConfirmed, 1);
        let owner = succeeded_enable(&authority);
        let Err(Refused {
            token: owner,
            reason,
        }) = authority.commit_enable(owner)
        else {
            panic!("MAX 后永不再授予恢复权限");
        };
        assert_eq!(reason, Refusal::EpochExhausted);
        let lease = authority
            .acquire_normal_io(NormalIoIntent::PeriodicRead)
            .unwrap();
        let released = authority.release_normal_io(lease).unwrap();
        assert!(released.superseded);
        assert_eq!(
            authority.retire_enable(owner).unwrap(),
            EnableRetired {
                retired_from: Phase::EnableSucceeded,
                superseded: true
            }
        );

        let authority = near_max(true, S::TorqueOffConfirmed, 1);
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
        assert!(authority.retire_clear_cleanup(cleanup).unwrap().superseded);
        let _ = authority.shutdown();
        assert!(authority.snapshot().shutdown && authority.snapshot().exhausted);
    }
}

// ---------------------------------------------------------------------------
// 真实 allocation guard（需要本二进制安装 AllocDisabler）
// ---------------------------------------------------------------------------

#[cfg(all(feature = "rt_guard", unix))]
mod alloc_guard {
    #[cfg(feature = "test-comm-fault-hook")]
    #[test]
    fn observed_cas_and_bus_lease_run_inside_the_real_allocator_guard() {
        use robot_core::serial_services::{BusLease, BusOpClass};
        let a = super::authority(false, super::S::TorqueOffConfirmed);
        let b = super::authority(false, super::S::TorqueOffConfirmed);
        let facts = assert_no_alloc::assert_no_alloc(|| {
            let acquired = a.acquire_normal_io_observed(super::NormalIoIntent::PeriodicRead);
            let acquisition_count = acquired.failed_cas;
            let lease = acquired.outcome.unwrap();
            let bus = BusLease::with_normal(BusOpClass::PeriodicRead, lease).unwrap();
            let foreign = bus.release_observed(&b);
            let foreign_count = foreign.failed_cas;
            let original = foreign.outcome.unwrap_err().token;
            let stopped = a.estop_observed();
            let serial = a.claim_serial_estop_observed();
            let ipc = a.claim_ipc_estop_observed();
            let release = original.release_observed(&a);
            let release_count = release.failed_cas;
            let superseded = release.outcome.unwrap().unwrap().superseded;
            let special = BusLease::special(BusOpClass::Safety)
                .unwrap()
                .release_observed(&a);
            (
                acquisition_count,
                foreign_count,
                stopped.failed_cas,
                serial.failed_cas,
                ipc.failed_cas,
                release_count,
                stopped.outcome.normal_io_active,
                serial.outcome && ipc.outcome,
                superseded,
                special.failed_cas,
                special.outcome.unwrap().is_none(),
            )
        });
        assert_eq!(
            facts,
            (
                Some(0),
                Some(0),
                Some(0),
                Some(0),
                Some(0),
                Some(0),
                true,
                true,
                true,
                Some(0),
                true
            )
        );
        assert!(!a.snapshot().normal_io_active);
    }

    use super::*;
    use robot_core::time::{RobotClock, SystemClock};
    use std::alloc::Layout;
    use std::hint::black_box;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};

    const CHILD_ENV: &str = "ROBOT_TEST_CONTROL_AUTHORITY_ALLOC_CHILD";
    const SCOPE_MARKER: &[u8] = b"control_authority_alloc_guard entered_scope\n";
    const CHILD_OPS: [&str; 3] = ["alloc", "realloc", "dealloc"];

    fn write_marker() {
        // SAFETY: 静态字节切片有效；直接写 stderr 避免标记本身触发堆分配。
        let written = unsafe {
            libc::write(
                libc::STDERR_FILENO,
                SCOPE_MARKER.as_ptr().cast(),
                SCOPE_MARKER.len(),
            )
        };
        assert_eq!(usize::try_from(written).ok(), Some(SCOPE_MARKER.len()));
    }

    /// 在真实 `assert_no_alloc` scope 内执行一次 alloc / realloc / dealloc；
    /// 守卫有效时进程以 SIGABRT 终止，走到函数末尾表示守卫失效。
    fn violate_inside_real_scope(op: &str) -> ! {
        let core_limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: 指向当前栈上有效 rlimit；只限制本测试子进程，避免预期 abort 留下 core。
        assert_eq!(
            unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const core_limit) },
            0
        );
        let layout = Layout::from_size_align(64, 8).unwrap();
        // SAFETY: layout 非零且合法；在 scope 之外分配，供 realloc / dealloc 反例使用。
        let outside = unsafe { std::alloc::alloc(layout) };
        assert!(!outside.is_null());
        let survived = assert_no_alloc::assert_no_alloc(|| {
            write_marker();
            match op {
                // SAFETY: layout 合法；若守卫失效，返回指针在 scope 外按同一 layout 释放。
                "alloc" => unsafe { std::alloc::alloc(black_box(layout)) },
                // SAFETY: `outside` 来自上面同一 layout 的有效分配且尚未释放。
                "realloc" => unsafe {
                    std::alloc::realloc(black_box(outside), black_box(layout), black_box(128))
                },
                "dealloc" => {
                    // SAFETY: `outside` 来自上面同一 layout 的有效分配且尚未释放。
                    unsafe { std::alloc::dealloc(black_box(outside), black_box(layout)) };
                    std::ptr::null_mut()
                }
                _ => std::ptr::null_mut(),
            }
        });
        black_box(survived);
        // 能运行至此表示 (de)allocation 未被拒绝；父进程只接受 SIGABRT。
        std::process::exit(73);
    }

    #[test]
    fn installed_alloc_disabler_aborts_alloc_realloc_and_dealloc_inside_the_scope() {
        if let Ok(op) = std::env::var(CHILD_ENV) {
            violate_inside_real_scope(&op);
        }
        for op in CHILD_OPS {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "alloc_guard::installed_alloc_disabler_aborts_alloc_realloc_and_dealloc_inside_the_scope",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD_ENV, op)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("启动分配违规子进程");
            let deadline = SystemClock.now().0 + 20_000_000_000;
            while child.try_wait().expect("读取子进程状态").is_none() {
                if SystemClock.now().0 >= deadline {
                    child.kill().expect("终止未退场的子进程");
                    let output = child.wait_with_output().unwrap();
                    panic!("{op} 反例超时：{}", String::from_utf8_lossy(&output.stderr));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let output = child.wait_with_output().unwrap();
            assert_eq!(
                output.status.signal(),
                Some(libc::SIGABRT),
                "{op} 必须被已安装的 AllocDisabler 实际 abort：status={} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                output
                    .stderr
                    .windows(SCOPE_MARKER.len())
                    .filter(|window| *window == SCOPE_MARKER)
                    .count(),
                1,
                "{op} 反例必须先实际进入 scope，不能把其它 abort 充当守卫生效"
            );
        }
    }

    /// scope 内只记录 Copy 结果，断言放在 scope 外：断言失败的格式化不得混入分配统计。
    #[derive(Clone, Copy, Debug, Default)]
    #[allow(
        clippy::struct_excessive_bools,
        reason = "每个字段是一条独立原语在无分配 scope 内的判定"
    )]
    struct HotPathReport {
        idle_disabled: bool,
        finish_refused_while_lease_active: bool,
        torque_on_published: bool,
        committed: bool,
        motion_lease_after_commit: bool,
        reserve_refused_not_disabled: bool,
        motion_refused_after_disable: bool,
        cross_domain_start_refused: bool,
        estop_saw_io_active: bool,
        stale_commit_refused: bool,
        retired_superseded: bool,
        claims: bool,
        confirm_off_committed: bool,
        abandon_kept_placeholder: bool,
        finalize_retirement: bool,
        latched_repair_retirement: bool,
        boot_released: bool,
        sticky_refused: bool,
        fault_not_relaxed: bool,
        disconnect_kept_fault: bool,
        read_blocked_by_inhibit: bool,
        cross_domain_release_refused: bool,
        shutdown_set: bool,
        epoch: u64,
    }

    #[allow(
        clippy::too_many_lines,
        reason = "全部热路径原语必须在同一个无分配 scope 内连续执行"
    )]
    fn run_hot_paths(authority: &ControlAuthority, other: &ControlAuthority) -> HotPathReport {
        let mut report = HotPathReport::default();
        let snapshot = authority.snapshot();
        report.idle_disabled = snapshot.phase == Some(Phase::Idle) && snapshot.disabled;

        // Enable：预约 → 启动 → wire 步骤 → 持租约时 finish 拒绝 → 释放 → 终态 → 提交。
        if let Ok((owner, view)) = authority.reserve_enable()
            && let Ok(execution) = authority.start_enable(view)
            && let Ok(lease) =
                authority.acquire_normal_io(NormalIoIntent::EnableStep(execution.view()))
        {
            let execution = match authority.finish_enable(execution, EnableResult::Succeeded) {
                Err(Refused { token, reason }) => {
                    report.finish_refused_while_lease_active = reason == Refusal::NormalIoActive;
                    Some(token)
                }
                Ok(_) => None,
            };
            let _ = authority.release_normal_io(lease);
            if let Some(execution) = execution
                && let Ok(finish) = authority.finish_enable(execution, EnableResult::Succeeded)
            {
                report.torque_on_published = finish.torque_on_published;
            }
            if let Ok(committed) = authority.commit_enable(owner) {
                report.committed = committed.epoch.get() == 3;
            }
        }
        // 已提交 Enable：实际获取并释放 Motion 租约；Disable 之后 Motion 必须拒绝。
        if let Ok(lease) = authority.acquire_normal_io(NormalIoIntent::Motion)
            && let Ok(released) = authority.release_normal_io(lease)
        {
            report.motion_lease_after_commit = !released.superseded;
        }
        report.reserve_refused_not_disabled =
            authority.reserve_enable().err() == Some(Refusal::NotDisabled);
        let _ = authority.disable();
        report.motion_refused_after_disable =
            authority.acquire_normal_io(NormalIoIntent::Motion).err() == Some(Refusal::Disabled);

        // 跨 domain 视图先拒且不改位。
        if let Ok((other_owner, other_view)) = other.reserve_enable() {
            report.cross_domain_start_refused = matches!(
                authority.start_enable(other_view),
                Err(Refusal::CrossDomain { .. })
            );
            let _ = other.retire_enable(other_owner);
        }

        // 撤销中的 Enable：EStop 线性化时 IO 在途，旧成功不能提交，只能退休。
        if let Ok((owner, view)) = authority.reserve_enable()
            && let Ok(execution) = authority.start_enable(view)
            && let Ok(lease) =
                authority.acquire_normal_io(NormalIoIntent::EnableStep(execution.view()))
        {
            let estop = authority.estop();
            report.estop_saw_io_active =
                estop.normal_io_active && estop.outcome == EStopOutcome::StopIssued;
            let _ = authority.release_normal_io(lease);
            let _ = authority.finish_enable(execution, EnableResult::Succeeded);
            if let Err(Refused {
                token: owner,
                reason,
            }) = authority.commit_enable(owner)
            {
                report.stale_commit_refused = matches!(reason, Refusal::StaleEpoch { .. });
                if let Ok(retired) = authority.retire_enable(owner) {
                    report.retired_superseded = retired.superseded;
                }
            }
        }
        report.claims = authority.claim_serial_estop() && authority.claim_ipc_estop();

        // 观测 + Clear 提交 / Finalize。
        let observation = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
        if let Ok(committed) = authority.complete_safety_observation(observation) {
            report.confirm_off_committed = committed.safety == S::TorqueOffConfirmed;
        }
        // Finalize 许可阶段 abandon 一次：占位与逻辑提交不丢失，归还的 owner 可重新授权并退休。
        if let Ok(pending) = authority.reserve_clear()
            && let Ok(finalizing) = authority.commit_clear(pending)
            && let Ok(permit) = authority.authorize_finalize(finalizing)
            && let Ok(ClearOwner::Finalizing(finalizing)) = authority.abandon_cleanup(permit)
        {
            let snapshot = authority.snapshot();
            report.abandon_kept_placeholder =
                snapshot.phase == Some(Phase::ClearFinalizing) && !snapshot.latch;
            if let Ok(permit) = authority.authorize_finalize(finalizing)
                && let Ok(retirement) = authority.retire_clear_cleanup(permit)
            {
                report.finalize_retirement = retirement.logical_committed && !retirement.superseded;
            }
        }

        // Clear 被新 EStop 否决：Latched 修复 → 清理 → 退休。
        let _ = authority.estop();
        let _ = authority.claim_serial_estop();
        let _ = authority.claim_ipc_estop();
        let observation = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
        let _ = authority.complete_safety_observation(observation);
        if let Ok(pending) = authority.reserve_clear() {
            let _ = authority.estop();
            let _ = authority.claim_serial_estop();
            let _ = authority.claim_ipc_estop();
            if let Err(Refused { token: pending, .. }) = authority.commit_clear(pending)
                && let Ok(permit) = authority.begin_repair(ClearOwner::Pending(pending))
                && let Ok(cleanup) = authority.authorize_repair_cleanup(permit)
                && let Ok(retirement) = authority.retire_clear_cleanup(cleanup)
            {
                report.latched_repair_retirement = retirement.kind
                    == CleanupKind::Repaired(RepairTarget::Latched)
                    && retirement.superseded
                    && !retirement.logical_committed;
            }
        }

        // inhibit：可解除原因经许可解除，sticky 原因拒绝捕获。
        let _ = authority.tighten_inhibit(InhibitReason::Boot);
        if let Ok(permit) = authority.capture_inhibit_release(InhibitReason::Boot)
            && let Ok(released) = authority.try_release_inhibit(permit)
        {
            report.boot_released = released.remaining.is_empty();
        }
        let _ = authority.tighten_inhibit(InhibitReason::CommFault);
        report.sticky_refused = authority
            .capture_inhibit_release(InhibitReason::CommFault)
            .err()
            == Some(Refusal::InhibitSticky(InhibitReason::CommFault));
        report.read_blocked_by_inhibit = matches!(
            authority.acquire_normal_io(NormalIoIntent::PeriodicRead),
            Err(Refusal::Inhibited(_))
        );

        // typed 收紧后普通观测不得放宽；断连保持故障态。
        let _ = authority.tighten_safety(SafetyTightening::AdapterUncertain);
        let observation = authority.begin_safety_observation(SafetyOperation::ConfirmOff);
        report.fault_not_relaxed = matches!(
            authority.complete_safety_observation(observation),
            Err(SafetyObservationError::Rejected(
                Refusal::SafetyNotRelaxable(S::AdapterUncertain)
            ))
        );
        let observation = authority.begin_safety_observation(SafetyOperation::Disconnect);
        if let Ok(committed) = authority.complete_safety_observation(observation) {
            report.disconnect_kept_fault = committed.safety == S::AdapterUncertain;
        }

        // 跨 domain 租约不能在别的权威释放。
        if let Ok(lease) = other.acquire_normal_io(NormalIoIntent::PeriodicRead)
            && let Err(Refused {
                token: lease,
                reason,
            }) = authority.release_normal_io(lease)
        {
            report.cross_domain_release_refused = matches!(reason, Refusal::CrossDomain { .. });
            let _ = other.release_normal_io(lease);
        }

        let _ = authority.shutdown();
        let snapshot = authority.snapshot();
        report.shutdown_set = snapshot.shutdown && snapshot.disabled;
        report.epoch = snapshot.epoch.get();
        report
    }

    #[test]
    fn every_hot_path_primitive_runs_without_alloc_realloc_or_free() {
        // 构造在 scope 之外：domain 分配与测试夹具允许分配。
        let authority = authority(false, S::TorqueOffConfirmed);
        let other = super::authority(false, S::TorqueOffConfirmed);
        let report = assert_no_alloc::assert_no_alloc(|| {
            black_box(run_hot_paths(black_box(&authority), black_box(&other)))
        });
        assert!(report.idle_disabled, "{report:?}");
        assert!(report.finish_refused_while_lease_active, "{report:?}");
        assert!(report.torque_on_published, "{report:?}");
        assert!(report.committed, "{report:?}");
        assert!(report.motion_lease_after_commit, "{report:?}");
        assert!(report.reserve_refused_not_disabled, "{report:?}");
        assert!(report.motion_refused_after_disable, "{report:?}");
        assert!(report.cross_domain_start_refused, "{report:?}");
        assert!(report.estop_saw_io_active, "{report:?}");
        assert!(report.stale_commit_refused, "{report:?}");
        assert!(report.retired_superseded, "{report:?}");
        assert!(report.claims, "{report:?}");
        assert!(report.confirm_off_committed, "{report:?}");
        assert!(report.abandon_kept_placeholder, "{report:?}");
        assert!(report.finalize_retirement, "{report:?}");
        assert!(report.latched_repair_retirement, "{report:?}");
        assert!(report.boot_released, "{report:?}");
        assert!(report.sticky_refused, "{report:?}");
        assert!(report.fault_not_relaxed, "{report:?}");
        assert!(report.disconnect_kept_fault, "{report:?}");
        assert!(report.read_blocked_by_inhibit, "{report:?}");
        assert!(report.cross_domain_release_refused, "{report:?}");
        assert!(report.shutdown_set, "{report:?}");
        assert_eq!(report.epoch, 17, "{report:?}");
    }

    /// epoch MAX 的耗尽路径同样必须在真实 guard scope 内零分配；夹具在 scope 外构造。
    #[cfg(feature = "test-comm-fault-hook")]
    mod max_under_guard {
        use super::*;

        #[derive(Clone, Copy, Debug, Default)]
        #[allow(
            clippy::struct_excessive_bools,
            reason = "每个字段是一条 MAX 路径在无分配 scope 内的判定"
        )]
        struct MaxReport {
            reached_max_without_exhaustion: bool,
            estop_exhausted_without_advance: bool,
            old_unlatched_repair_vetoed: bool,
            stale_confirm_rejected: bool,
            inhibit_capture_refused: bool,
            tightening_still_applies: bool,
            latched_repair_retired: bool,
            new_transactions_refused: bool,
            commit_at_max_fails_closed: bool,
            lease_released_superseded: bool,
            owner_retired: bool,
            idle_reserve_exhausts: bool,
        }

        #[allow(
            clippy::too_many_lines,
            reason = "MAX 耗尽、否决与退休路径必须在同一个无分配 scope 内连续执行"
        )]
        fn run_max_paths(
            repair: &ControlAuthority,
            enable: &ControlAuthority,
            idle: &ControlAuthority,
        ) -> MaxReport {
            let mut report = MaxReport::default();

            // 修复夹具（MAX-1，原 latch=false）：事务到 MAX；新 EStop 耗尽并否决旧 false 修复。
            if let Ok(pending) = repair.reserve_clear() {
                let snapshot = repair.snapshot();
                report.reached_max_without_exhaustion =
                    snapshot.epoch == PermissionEpoch::MAX && !snapshot.exhausted;
                if let Ok(permit) = repair.begin_repair(ClearOwner::Pending(pending)) {
                    let unlatched = permit.target() == RepairTarget::OriginalUnlatched;
                    let stale = repair.begin_safety_observation(SafetyOperation::ConfirmOff);
                    let estop = repair.estop();
                    report.estop_exhausted_without_advance = !estop.tightened.advanced
                        && estop.tightened.exhausted
                        && repair.snapshot().epoch == PermissionEpoch::MAX;
                    if let Err(Refused {
                        token: owner,
                        reason,
                    }) = repair.authorize_repair_cleanup(permit)
                    {
                        report.old_unlatched_repair_vetoed = unlatched
                            && reason
                                == Refusal::RepairTargetChanged {
                                    required: RepairTarget::Latched,
                                };
                        report.stale_confirm_rejected = matches!(
                            repair.complete_safety_observation(stale),
                            Err(SafetyObservationError::Rejected(Refusal::Exhausted))
                        );
                        report.inhibit_capture_refused =
                            repair.capture_inhibit_release(InhibitReason::Boot).err()
                                == Some(Refusal::Exhausted);
                        let tightened = repair.tighten_safety(SafetyTightening::StopFailed);
                        report.tightening_still_applies = tightened.safety == S::StopFailed
                            && !tightened.tightened.advanced
                            && repair.claim_serial_estop();
                        if let Ok(permit) = repair.begin_repair(owner)
                            && permit.target() == RepairTarget::Latched
                            && let Ok(cleanup) = repair.authorize_repair_cleanup(permit)
                            && let Ok(retirement) = repair.retire_clear_cleanup(cleanup)
                        {
                            report.latched_repair_retired = retirement.superseded
                                && !retirement.logical_committed
                                && retirement.latch;
                        }
                    }
                }
            }
            report.new_transactions_refused = repair.reserve_enable().err()
                == Some(Refusal::Exhausted)
                && repair.reserve_clear().err() == Some(Refusal::Exhausted);

            // Enable 夹具（MAX-1）：事务到 MAX，真实成功但提交 fail-closed；lease 归还与退休仍完成。
            if let Ok((owner, view)) = enable.reserve_enable()
                && let Ok(execution) = enable.start_enable(view)
                && let Ok(lease) =
                    enable.acquire_normal_io(NormalIoIntent::EnableStep(execution.view()))
                && let Ok(released) = enable.release_normal_io(lease)
                && !released.superseded
                && let Ok(finish) = enable.finish_enable(execution, EnableResult::Succeeded)
                && finish.torque_on_published
                && let Err(Refused {
                    token: owner,
                    reason,
                }) = enable.commit_enable(owner)
            {
                report.commit_at_max_fails_closed =
                    reason == Refusal::EpochExhausted && enable.snapshot().exhausted;
                if let Ok(lease) = enable.acquire_normal_io(NormalIoIntent::PeriodicRead)
                    && let Ok(released) = enable.release_normal_io(lease)
                {
                    report.lease_released_superseded = released.superseded;
                }
                if let Ok(retired) = enable.retire_enable(owner) {
                    report.owner_retired =
                        retired.superseded && retired.retired_from == Phase::EnableSucceeded;
                }
            }

            // Idle 夹具（已在 MAX）：任何新事务把权威置 Exhausted + Disabled。
            let refused = idle.reserve_enable().err() == Some(Refusal::EpochExhausted);
            let snapshot = idle.snapshot();
            report.idle_reserve_exhausts = refused && snapshot.exhausted && snapshot.disabled;
            report
        }

        #[test]
        fn max_epoch_paths_run_without_allocation_inside_the_real_guard() {
            let repair = near_max(false, S::TorqueOffConfirmed, 1);
            let enable = near_max(false, S::TorqueOffConfirmed, 1);
            let idle = near_max(false, S::TorqueOffConfirmed, 0);
            let report = assert_no_alloc::assert_no_alloc(|| {
                black_box(run_max_paths(
                    black_box(&repair),
                    black_box(&enable),
                    black_box(&idle),
                ))
            });
            assert!(report.reached_max_without_exhaustion, "{report:?}");
            assert!(report.estop_exhausted_without_advance, "{report:?}");
            assert!(report.old_unlatched_repair_vetoed, "{report:?}");
            assert!(report.stale_confirm_rejected, "{report:?}");
            assert!(report.inhibit_capture_refused, "{report:?}");
            assert!(report.tightening_still_applies, "{report:?}");
            assert!(report.latched_repair_retired, "{report:?}");
            assert!(report.new_transactions_refused, "{report:?}");
            assert!(report.commit_at_max_fails_closed, "{report:?}");
            assert!(report.lease_released_superseded, "{report:?}");
            assert!(report.owner_retired, "{report:?}");
            assert!(report.idle_reserve_exhausts, "{report:?}");
        }
    }
}
