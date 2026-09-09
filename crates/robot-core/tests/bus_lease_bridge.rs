//! 只经公开 API 验证总线 wrapper 与真实权限域的唯一租约责任。

use robot_core::control_authority::{
    AuthorityInit, ControlAuthority, EnableResult, InhibitSet, NonEnergizingControl,
    NormalIoIntent, Refusal,
};
use robot_core::executor_ctrl::OutputSafetyState;
use robot_core::serial_services::{BusLease, BusOpClass};

fn authority() -> ControlAuthority {
    ControlAuthority::new(AuthorityInit::new(
        false,
        OutputSafetyState::TorqueOffConfirmed,
        InhibitSet::EMPTY,
    ))
    .unwrap()
}

#[test]
fn wrong_domain_returns_original_bus_lease_until_real_release() {
    let a = authority();
    let b = authority();
    let lease = a.acquire_normal_io(NormalIoIntent::PeriodicRead).unwrap();
    let bus = BusLease::with_normal(BusOpClass::PeriodicRead, lease).unwrap();
    let before_a = a.snapshot();
    let before_b = b.snapshot();
    let rejected = bus.release(&b).unwrap_err();
    assert!(matches!(rejected.reason, Refusal::CrossDomain { .. }));
    assert_eq!(a.snapshot(), before_a);
    assert_eq!(b.snapshot(), before_b);
    assert_eq!(rejected.token.class(), BusOpClass::PeriodicRead);
    assert!(rejected.token.holds_normal_lease());
    let released = rejected.token.release(&a).unwrap().unwrap();
    assert_eq!(released.intent, NormalIoIntent::PeriodicRead);
    assert!(!released.superseded);
    assert!(!a.snapshot().normal_io_active);
    let again = a.acquire_normal_io(NormalIoIntent::PeriodicRead).unwrap();
    a.release_normal_io(again).unwrap();
}

#[test]
fn complete_class_intent_matrix_preserves_rejected_leases() {
    let classes = [
        BusOpClass::PeriodicRead,
        BusOpClass::PeriodicWrite,
        BusOpClass::NormalEffectful,
        BusOpClass::Safety,
        BusOpClass::Confirm,
        BusOpClass::Connect,
    ];
    for class in classes {
        let authority = authority();
        let (owner, view) = authority.reserve_enable().unwrap();
        let execution = authority.start_enable(view).unwrap();
        let intent = NormalIoIntent::EnableStep(execution.view());
        check_pair(
            &authority,
            class,
            intent,
            class == BusOpClass::NormalEffectful,
        );
        authority
            .finish_enable(execution, EnableResult::Succeeded)
            .unwrap();
        authority.commit_enable(owner).unwrap();
        for (intent, allowed) in [
            (
                NormalIoIntent::Motion,
                matches!(
                    class,
                    BusOpClass::PeriodicWrite | BusOpClass::NormalEffectful
                ),
            ),
            (
                NormalIoIntent::PeriodicRead,
                class == BusOpClass::PeriodicRead,
            ),
            (
                NormalIoIntent::NonEnergizingControl(NonEnergizingControl::Connect),
                class == BusOpClass::NormalEffectful,
            ),
            (
                NormalIoIntent::NonEnergizingControl(NonEnergizingControl::Disconnect),
                class == BusOpClass::NormalEffectful,
            ),
            (
                NormalIoIntent::NonEnergizingControl(NonEnergizingControl::Disable),
                class == BusOpClass::NormalEffectful,
            ),
        ] {
            check_pair(&authority, class, intent, allowed);
        }
    }
}

fn check_pair(
    authority: &ControlAuthority,
    class: BusOpClass,
    intent: NormalIoIntent,
    allowed: bool,
) {
    let lease = authority.acquire_normal_io(intent).unwrap();
    let before = authority.snapshot();
    match BusLease::with_normal(class, lease) {
        Ok(bus) => {
            assert!(allowed);
            assert_eq!(bus.class(), class);
            assert!(bus.holds_normal_lease());
            assert_eq!(authority.snapshot(), before);
            assert_eq!(bus.release(authority).unwrap().unwrap().intent, intent);
        }
        Err(lease) => {
            assert!(!allowed);
            assert_eq!(lease.intent(), intent);
            assert_eq!(authority.snapshot(), before);
            authority.release_normal_io(lease).unwrap();
        }
    }
    assert!(!authority.snapshot().normal_io_active);
}

#[test]
fn special_classes_never_touch_authority_even_with_an_active_normal_lease() {
    let authority = authority();
    let normal = authority
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    let before = authority.snapshot();
    for class in [BusOpClass::Safety, BusOpClass::Confirm, BusOpClass::Connect] {
        let lease = BusLease::special(class).unwrap();
        assert_eq!(lease.class(), class);
        assert!(!lease.holds_normal_lease());
        assert_eq!(lease.release(&authority).unwrap(), None);
        assert_eq!(authority.snapshot(), before);
    }
    for class in [
        BusOpClass::PeriodicRead,
        BusOpClass::PeriodicWrite,
        BusOpClass::NormalEffectful,
    ] {
        assert!(BusLease::special(class).is_none());
    }
    authority.release_normal_io(normal).unwrap();
}

#[test]
fn dropping_wrapper_does_not_release_real_normal_io() {
    let authority = authority();
    let lease = authority
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    {
        let _bus = BusLease::with_normal(BusOpClass::PeriodicRead, lease).unwrap();
    }
    assert!(authority.snapshot().normal_io_active);
    assert!(matches!(
        authority.acquire_normal_io(NormalIoIntent::PeriodicRead),
        Err(Refusal::NormalIoActive)
    ));
}

#[test]
fn tightening_does_not_prevent_consuming_release() {
    let authority = authority();
    let lease = authority
        .acquire_normal_io(NormalIoIntent::PeriodicRead)
        .unwrap();
    let bus = BusLease::with_normal(BusOpClass::PeriodicRead, lease).unwrap();
    let _ = authority.disable();
    let released = bus.release(&authority).unwrap().unwrap();
    assert!(released.superseded);
    assert!(!authority.snapshot().normal_io_active);
    assert!(authority.snapshot().disabled);
}
