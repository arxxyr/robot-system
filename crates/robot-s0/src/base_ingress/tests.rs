use super::*;

const VALUES: [f64; 3] = [1.0, -2.0, 3.0];
const CONNECTION: ConnectionEpoch = ConnectionEpoch::new(7);
const CLOSED: DisabledEvidence = DisabledEvidence {
    disabled: true,
    owner_retired: true,
    torque_off_confirmed: true,
};

fn reducer() -> BaseIngress {
    BaseIngress::new(
        BaseIngressContract::new(
            ConfiguredSourceId::new(0),
            0,
            WheelVelocityProfile::new(0.1, 0.2, 10.0).unwrap(),
            DurationNs::from_nanos(200_000_000),
        )
        .unwrap(),
    )
}

fn origin(node: u128, port: u128) -> PublisherOrigin {
    PublisherOrigin::for_mock(node, port)
}

fn command(node: u128, port: u128, seq: u64, stamp: u64, targets: &[f64]) -> BaseCommandInput<'_> {
    BaseCommandInput {
        origin: Some(origin(node, port)),
        header: ProducerFrame {
            source_id: 0,
            seq,
            stamp: MonoRawNs(stamp),
        },
        layout_id: 0,
        mode: ControlModeRepr::Velocity as u8,
        targets_rad_s: targets,
        deadline: MonoRawNs(stamp + COMMAND_TTL_MAX_NS),
        on_timeout: TimeoutIntent::Zero,
        source_task_id: 0,
    }
}

fn heartbeat(node: u128, port: u128, seq: u64, stamp: u64) -> HeartbeatInput {
    let header = ProducerFrame {
        source_id: 0,
        seq,
        stamp: MonoRawNs(stamp),
    };
    HeartbeatInput {
        origin: Some(origin(node, port)),
        header,
        body: header,
        tick_rate_hz: 50.0,
    }
}

fn pending(value: &mut BaseIngress, stamp: u64) -> RebindTicket {
    let ticket = value
        .begin_explicit_enable(CLOSED, CONNECTION, MonoRawNs(stamp))
        .unwrap();
    assert_eq!(
        value.observe_internal_zero_ack(
            ticket,
            CONNECTION,
            CompletionOutcome::Committed,
            MonoRawNs(stamp + 1)
        ),
        ReducerEffect::Staged
    );
    ticket
}

fn ready_at(stamp: u64) -> (BaseIngress, RebindTicket) {
    let mut value = reducer();
    let ticket = pending(&mut value, stamp);
    assert_eq!(
        value.accept_heartbeat(heartbeat(1, 20, 0, stamp + 2), MonoRawNs(stamp + 2)),
        ReducerEffect::Staged
    );
    assert_eq!(
        value.accept_command(command(1, 10, 0, stamp + 3, &VALUES), MonoRawNs(stamp + 3)),
        ReducerEffect::MayRequestEnable(ticket)
    );
    (value, ticket)
}

fn active() -> (BaseIngress, RebindTicket) {
    let (mut value, ticket) = ready_at(100);
    assert_eq!(
        value.observe_enable_committed(ticket, CompletionOutcome::Committed, MonoRawNs(104)),
        ReducerEffect::EnteredActive
    );
    (value, ticket)
}

fn reject_command(
    value: &mut BaseIngress,
    input: BaseCommandInput<'_>,
    now: u64,
    reason: IngressReject,
) {
    let before = *value;
    assert_eq!(
        value.accept_command(input, MonoRawNs(now)),
        ReducerEffect::Rejected(reason)
    );
    assert_eq!(*value, before);
}

fn reject_heartbeat(
    value: &mut BaseIngress,
    input: HeartbeatInput,
    now: u64,
    reason: IngressReject,
) {
    let before = *value;
    assert_eq!(
        value.accept_heartbeat(input, MonoRawNs(now)),
        ReducerEffect::Rejected(reason)
    );
    assert_eq!(*value, before);
}

#[test]
fn explicit_zero_source_and_full_origin_values_are_preserved() {
    let mut value = reducer();
    let ticket = pending(&mut value, 0);
    let node = (1_u128 << 127) | 3;
    let port = (1_u128 << 126) | 9;
    assert_eq!(
        value.accept_command(command(node, port, 0, 2, &VALUES), MonoRawNs(2)),
        ReducerEffect::Staged
    );
    assert_eq!(
        value.accept_heartbeat(heartbeat(node, 0, 0, 3), MonoRawNs(3)),
        ReducerEffect::MayRequestEnable(ticket)
    );
    let snapshot = value.snapshot();
    assert_eq!(snapshot.command_origin, Some(origin(node, port)));
    assert_eq!(snapshot.heartbeat_origin, Some(origin(node, 0)));
    assert_eq!(snapshot.current_identity.unwrap().node.value(), node);
    assert_eq!(ConfiguredSourceId::new(0).get(), 0);
    assert_eq!(snapshot.accepted_counter.unwrap().get(), 1);
    assert_eq!(
        value.gated_command(MonoRawNs(3)),
        Err(IngressReject::GateClosed)
    );
}

#[test]
fn both_pair_orders_require_actual_enable_committed() {
    for command_first in [true, false] {
        let mut value = reducer();
        let ticket = pending(&mut value, 100);
        let cmd = command(1, 10, 99, 102, &VALUES);
        let hb = heartbeat(1, 20, 7, 102);
        let first = if command_first {
            value.accept_command(cmd, MonoRawNs(102))
        } else {
            value.accept_heartbeat(hb, MonoRawNs(102))
        };
        assert_eq!(first, ReducerEffect::Staged);
        assert_eq!(value.snapshot().accepted_counter, None);
        let second = if command_first {
            value.accept_heartbeat(hb, MonoRawNs(102))
        } else {
            value.accept_command(cmd, MonoRawNs(102))
        };
        assert_eq!(second, ReducerEffect::MayRequestEnable(ticket));
        assert_eq!(
            value.gated_command(MonoRawNs(102)),
            Err(IngressReject::GateClosed)
        );
        assert_eq!(
            value.observe_enable_committed(ticket, CompletionOutcome::Committed, MonoRawNs(103)),
            ReducerEffect::EnteredActive
        );
        assert_eq!(value.gated_command(MonoRawNs(103)).unwrap().seq, 1);
    }
}

#[test]
fn pre_ack_staging_is_discarded_with_partial_identity() {
    let mut value = reducer();
    let ticket = value
        .begin_explicit_enable(CLOSED, CONNECTION, MonoRawNs(100))
        .unwrap();
    assert_eq!(
        value.accept_command(command(1, 10, 500, 101, &VALUES), MonoRawNs(101)),
        ReducerEffect::Staged
    );
    assert_eq!(
        value.accept_heartbeat(heartbeat(1, 20, 600, 102), MonoRawNs(102)),
        ReducerEffect::Staged
    );
    assert_eq!(value.snapshot().accepted_counter, None);
    assert_eq!(value.snapshot().current_identity, None);
    assert_eq!(
        value.observe_internal_zero_ack(
            ticket,
            CONNECTION,
            CompletionOutcome::Committed,
            MonoRawNs(103)
        ),
        ReducerEffect::Staged
    );
    assert_eq!(value.current, None);
    assert_eq!(value.staged_command, None);
    reject_command(
        &mut value,
        command(2, 30, 0, 103, &VALUES),
        104,
        IngressReject::BelowBindingCutoff,
    );
    assert_eq!(
        value.accept_command(command(2, 30, 0, 104, &VALUES), MonoRawNs(104)),
        ReducerEffect::Staged
    );
    assert_eq!(
        value.accept_heartbeat(heartbeat(2, 40, 0, 105), MonoRawNs(105)),
        ReducerEffect::MayRequestEnable(ticket)
    );
}

#[test]
fn duplicate_zero_ack_cannot_move_barrier_or_erase_new_stage() {
    let mut value = reducer();
    let ticket = pending(&mut value, 100);
    assert_eq!(
        value.accept_command(command(1, 10, 0, 102, &VALUES), MonoRawNs(102)),
        ReducerEffect::Staged
    );
    let before = value;
    assert_eq!(
        value.observe_internal_zero_ack(
            ticket,
            CONNECTION,
            CompletionOutcome::Committed,
            MonoRawNs(103)
        ),
        ReducerEffect::Rejected(IngressReject::AckAlreadyObserved)
    );
    assert_eq!(value, before);
    assert_eq!(
        value.accept_heartbeat(heartbeat(1, 20, 0, 103), MonoRawNs(103)),
        ReducerEffect::MayRequestEnable(ticket)
    );
}

#[test]
fn lifecycle_unknown_failed_wrong_ticket_and_connection_are_rejected() {
    let mut value = reducer();
    for evidence in [
        DisabledEvidence {
            disabled: false,
            ..CLOSED
        },
        DisabledEvidence {
            owner_retired: false,
            ..CLOSED
        },
        DisabledEvidence {
            torque_off_confirmed: false,
            ..CLOSED
        },
    ] {
        let before = value;
        assert_eq!(
            value.begin_explicit_enable(evidence, CONNECTION, MonoRawNs(100)),
            Err(IngressReject::DisabledOrRetirementUnconfirmed)
        );
        assert_eq!(value, before);
    }
    let ticket = value
        .begin_explicit_enable(CLOSED, CONNECTION, MonoRawNs(100))
        .unwrap();
    let wrong = RebindTicket(NonZeroU64::new(99).unwrap());
    let before = value;
    for (actual_ticket, connection, outcome, stamp, expected) in [
        (
            wrong,
            CONNECTION,
            CompletionOutcome::Committed,
            101,
            IngressReject::WrongRebindTicket,
        ),
        (
            ticket,
            ConnectionEpoch::new(8),
            CompletionOutcome::Committed,
            101,
            IngressReject::WrongConnectionEpoch,
        ),
        (
            ticket,
            CONNECTION,
            CompletionOutcome::Failed,
            101,
            IngressReject::InternalZeroAckUnconfirmed,
        ),
        (
            ticket,
            CONNECTION,
            CompletionOutcome::Unknown,
            101,
            IngressReject::InternalZeroAckUnconfirmed,
        ),
        (
            ticket,
            CONNECTION,
            CompletionOutcome::Committed,
            99,
            IngressReject::ClockRegression,
        ),
    ] {
        assert_eq!(
            value.observe_internal_zero_ack(actual_ticket, connection, outcome, MonoRawNs(stamp)),
            ReducerEffect::Rejected(expected)
        );
        assert_eq!(value, before);
    }
    let (mut ready, ready_ticket) = ready_at(100);
    let before = ready;
    for (actual_ticket, outcome, error) in [
        (
            wrong,
            CompletionOutcome::Committed,
            IngressReject::WrongRebindTicket,
        ),
        (
            ready_ticket,
            CompletionOutcome::Failed,
            IngressReject::EnableResultUnconfirmed,
        ),
        (
            ready_ticket,
            CompletionOutcome::Unknown,
            IngressReject::EnableResultUnconfirmed,
        ),
    ] {
        assert_eq!(
            ready.observe_enable_committed(actual_ticket, outcome, MonoRawNs(104)),
            ReducerEffect::Rejected(error)
        );
        assert_eq!(ready, before);
    }
}

#[test]
fn unknown_and_wrong_source_do_not_invalidate_identity() {
    let (mut value, _) = active();
    let mut input = command(2, 30, 1, 105, &VALUES);
    input.origin = None;
    reject_command(&mut value, input, 105, IngressReject::UnknownOrigin);
    input.origin = Some(origin(2, 30));
    input.header.source_id = 1;
    reject_command(
        &mut value,
        input,
        105,
        IngressReject::ConfiguredSourceMismatch,
    );
    let mut hb = heartbeat(2, 40, 1, 105);
    hb.origin = None;
    reject_heartbeat(&mut value, hb, 105, IngressReject::UnknownOrigin);
    hb.origin = Some(origin(2, 40));
    hb.header.source_id = 1;
    reject_heartbeat(&mut value, hb, 105, IngressReject::ConfiguredSourceMismatch);
}

#[test]
fn heartbeat_body_and_rate_validation_does_not_refresh_cursor() {
    let (mut value, _) = active();
    for field in 0..3 {
        let mut hb = heartbeat(2, 40, 1, 105);
        match field {
            0 => hb.body.source_id += 1,
            1 => hb.body.seq += 1,
            _ => hb.body.stamp.0 += 1,
        }
        reject_heartbeat(
            &mut value,
            hb,
            105,
            IngressReject::HeartbeatHeaderBodyMismatch,
        );
    }
    for rate in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -1.0] {
        let mut hb = heartbeat(2, 40, 1, 105);
        hb.tick_rate_hz = rate;
        reject_heartbeat(&mut value, hb, 105, IngressReject::InvalidHeartbeatRate);
    }
}

#[test]
fn command_shape_layout_mode_and_timeout_are_strict() {
    let (mut value, _) = active();
    for targets in [
        &[][..],
        &[1.0][..],
        &[1.0, 2.0][..],
        &[1.0; 4][..],
        &[1.0; 33][..],
    ] {
        reject_command(
            &mut value,
            command(2, 30, 1, 105, targets),
            105,
            IngressReject::WrongTargetCount,
        );
    }
    let mut input = command(2, 30, 1, 105, &VALUES);
    input.layout_id = 1;
    reject_command(&mut value, input, 105, IngressReject::InvalidLayout);
    input.layout_id = 0;
    for mode in [0, 1, 3, 4, 255] {
        input.mode = mode;
        reject_command(&mut value, input, 105, IngressReject::InvalidMode);
    }
    input.mode = ControlModeRepr::Velocity as u8;
    for timeout in [
        TimeoutIntent::Hold,
        TimeoutIntent::Stop,
        TimeoutIntent::Unknown(255),
    ] {
        input.on_timeout = timeout;
        reject_command(&mut value, input, 105, IngressReject::UnsupportedTimeout);
    }
}

#[test]
fn target_nan_infinity_and_profile_limit_are_rejected_atomically() {
    let (mut value, _) = active();
    for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for index in 0..3 {
            let mut targets = VALUES;
            targets[index] = invalid;
            reject_command(
                &mut value,
                command(2, 30, 1, 105, &targets),
                105,
                IngressReject::NonFiniteTarget,
            );
        }
    }
    for invalid in [-10.000_001, 10.000_001] {
        reject_command(
            &mut value,
            command(2, 30, 1, 105, &[invalid, 0.0, 0.0]),
            105,
            IngressReject::TargetOutsideProfile,
        );
    }
}

#[test]
fn f32_actual_conversion_cannot_round_above_profile() {
    let limit = 1.0 + f64::from(f32::EPSILON) * 0.75;
    let contract = BaseIngressContract::new(
        ConfiguredSourceId::new(0),
        0,
        WheelVelocityProfile::new(0.1, 0.2, limit).unwrap(),
        DurationNs::from_nanos(200_000_000),
    )
    .unwrap();
    let mut value = BaseIngress::new(contract);
    let _ticket = pending(&mut value, 100);
    reject_command(
        &mut value,
        command(1, 10, 0, 102, &[limit, 0.0, 0.0]),
        102,
        IngressReject::TargetOutsideProfile,
    );
    reject_command(
        &mut value,
        command(1, 10, 0, 102, &[-limit, 0.0, 0.0]),
        102,
        IngressReject::TargetOutsideProfile,
    );
}

#[test]
fn f32_max_and_f64_out_of_range_are_distinguished() {
    let mut value = BaseIngress::new(
        BaseIngressContract::new(
            ConfiguredSourceId::new(0),
            0,
            WheelVelocityProfile::new(0.1, 0.2, f64::MAX).unwrap(),
            DurationNs::from_nanos(200_000_000),
        )
        .unwrap(),
    );
    let ticket = pending(&mut value, 100);
    reject_command(
        &mut value,
        command(1, 10, 0, 102, &[f64::MAX, 0.0, 0.0]),
        102,
        IngressReject::TargetOutsideF32,
    );
    assert_eq!(
        value.accept_command(
            command(1, 10, 0, 102, &[f64::from(f32::MAX), 0.0, 0.0]),
            MonoRawNs(102)
        ),
        ReducerEffect::Staged
    );
    assert_eq!(
        value.accept_heartbeat(heartbeat(1, 20, 0, 103), MonoRawNs(103)),
        ReducerEffect::MayRequestEnable(ticket)
    );
    assert_eq!(
        value.snapshot().targets_rad_s.unwrap()[0].to_bits(),
        f32::MAX.to_bits()
    );
}

#[test]
fn ttl_zero_over_budget_deadline_and_future_stamp_are_rejected() {
    let (mut value, _) = active();
    let mut input = command(1, 10, 1, 105, &VALUES);
    for deadline in [104, 105, 105 + COMMAND_TTL_MAX_NS + 1] {
        input.deadline = MonoRawNs(deadline);
        reject_command(&mut value, input, 105, IngressReject::InvalidCommandTtl);
    }
    input.deadline = MonoRawNs(106);
    reject_command(&mut value, input, 106, IngressReject::ExpiredCommand);
    reject_command(&mut value, input, 104, IngressReject::FutureStamp);
    assert_eq!(
        value.accept_command(input, MonoRawNs(105)),
        ReducerEffect::Accepted
    );
}

#[test]
fn heartbeat_expiry_upper_bound_and_clock_overflow_are_rejected() {
    let (mut value, _) = active();
    reject_heartbeat(
        &mut value,
        heartbeat(2, 40, 1, 105),
        104,
        IngressReject::FutureStamp,
    );
    reject_heartbeat(
        &mut value,
        heartbeat(2, 40, 1, 105),
        105 + 200_000_000,
        IngressReject::ExpiredHeartbeat,
    );
    reject_heartbeat(
        &mut value,
        heartbeat(2, 40, 1, u64::MAX - 1),
        u64::MAX - 1,
        IngressReject::ClockArithmeticOverflow,
    );
    assert_eq!(
        BaseIngressContract::new(
            ConfiguredSourceId::new(0),
            0,
            WheelVelocityProfile::new(0.1, 0.2, 10.0).unwrap(),
            DurationNs::from_nanos(0)
        ),
        Err(IngressReject::InvalidHeartbeatBudget)
    );
}

#[test]
fn producer_seq_is_per_port_and_task_tag_never_becomes_pod_seq() {
    let (mut value, _) = active();
    let mut input = command(1, 10, 1, 105, &VALUES);
    input.source_task_id = u64::MAX;
    assert_eq!(
        value.accept_command(input, MonoRawNs(105)),
        ReducerEffect::Accepted
    );
    assert_eq!(
        value.accept_heartbeat(heartbeat(1, 20, 900, 105), MonoRawNs(105)),
        ReducerEffect::Accepted
    );
    assert_eq!(value.gated_command(MonoRawNs(105)).unwrap().seq, 2);
    assert_eq!(value.snapshot().task_association, Some(u64::MAX));
    input.header.seq = 2;
    input.source_task_id = 0;
    assert_eq!(
        value.accept_command(input, MonoRawNs(105)),
        ReducerEffect::Accepted
    );
    assert_eq!(value.gated_command(MonoRawNs(105)).unwrap().seq, 3);
    assert_eq!(value.snapshot().task_association, Some(0));
}

#[test]
fn duplicate_wrap_and_stamp_regression_preserve_state() {
    let (mut value, _) = active();
    reject_command(
        &mut value,
        command(1, 10, 0, 105, &VALUES),
        105,
        IngressReject::ProducerSeqNotIncreasing,
    );
    reject_command(
        &mut value,
        command(1, 10, 1, 102, &VALUES),
        105,
        IngressReject::ProducerStampRegression,
    );
    assert_eq!(
        value.accept_command(command(1, 10, u64::MAX, 105, &VALUES), MonoRawNs(105)),
        ReducerEffect::Accepted
    );
    for seq in [0, u64::MAX] {
        reject_command(
            &mut value,
            command(1, 10, seq, 106, &VALUES),
            106,
            IngressReject::ProducerSeqNotIncreasing,
        );
    }
    reject_heartbeat(
        &mut value,
        heartbeat(1, 20, 0, 105),
        105,
        IngressReject::ProducerSeqNotIncreasing,
    );
    reject_heartbeat(
        &mut value,
        heartbeat(1, 20, 1, 101),
        105,
        IngressReject::ProducerStampRegression,
    );
}

#[test]
fn pending_staging_keeps_only_highest_valid_seq_without_counter_advance() {
    let mut value = reducer();
    let ticket = pending(&mut value, 100);
    for seq in 0..64 {
        assert_eq!(
            value.accept_command(command(1, 10, seq, 102, &VALUES), MonoRawNs(102)),
            ReducerEffect::Staged
        );
        assert_eq!(value.snapshot().accepted_counter, None);
    }
    reject_command(
        &mut value,
        command(1, 10, 62, 103, &VALUES),
        103,
        IngressReject::ProducerSeqNotIncreasing,
    );
    assert_eq!(
        value.accept_heartbeat(heartbeat(1, 20, 0, 103), MonoRawNs(103)),
        ReducerEffect::MayRequestEnable(ticket)
    );
    assert_eq!(value.current.unwrap().command_cursor.unwrap().seq, 63);
    assert_eq!(value.snapshot().accepted_counter.unwrap().get(), 1);
}

#[test]
fn port_alias_is_rejected_and_full_high_bits_cannot_alias() {
    let mut value = reducer();
    let _ticket = pending(&mut value, 100);
    assert_eq!(
        value.accept_command(command(1, 10, 0, 102, &VALUES), MonoRawNs(102)),
        ReducerEffect::Staged
    );
    reject_heartbeat(
        &mut value,
        heartbeat(1, 10, 1, 103),
        103,
        IngressReject::PortAlias,
    );
    let (mut active_value, _) = active();
    assert_eq!(
        active_value.accept_command(
            command((1_u128 << 127) | 1, 10, 1, 105, &VALUES),
            MonoRawNs(105)
        ),
        ReducerEffect::EnterSafe(IngressReject::IdentityChanged)
    );
    assert_eq!(active_value.snapshot().phase, IngressPhase::RebindRequired);
    let (mut active_value, _) = active();
    assert_eq!(
        active_value.accept_command(
            command(1, (1_u128 << 127) | 0x000a, 1, 105, &VALUES),
            MonoRawNs(105)
        ),
        ReducerEffect::EnterSafe(IngressReject::IdentityChanged)
    );
}

#[test]
fn identity_change_does_not_adopt_new_sample_and_old_a_cannot_resume() {
    let (mut value, _) = active();
    let before = value.snapshot();
    assert_eq!(
        value.accept_heartbeat(heartbeat(2, 40, 1, 105), MonoRawNs(105)),
        ReducerEffect::EnterSafe(IngressReject::IdentityChanged)
    );
    assert_eq!(value.snapshot().accepted_counter, before.accepted_counter);
    assert_eq!(value.snapshot().command_origin, before.command_origin);
    assert_eq!(value.snapshot().retired_identity, before.current_identity);
    assert!(!value.snapshot().enable_committed);
    reject_command(
        &mut value,
        command(1, 10, 1, 106, &VALUES),
        106,
        IngressReject::WrongLifecyclePhase,
    );
    reject_heartbeat(
        &mut value,
        heartbeat(1, 20, 1, 106),
        106,
        IngressReject::WrongLifecyclePhase,
    );
    assert_eq!(
        value.gated_command(MonoRawNs(106)),
        Err(IngressReject::GateClosed)
    );
}

#[test]
fn retired_complete_pair_is_refused_but_one_new_port_can_rebind() {
    let (mut value, _) = active();
    let _ = value.observe_disabled();
    let ticket = pending(&mut value, 200);
    assert_eq!(
        value.accept_command(command(1, 10, 1, 202, &VALUES), MonoRawNs(202)),
        ReducerEffect::Staged
    );
    reject_heartbeat(
        &mut value,
        heartbeat(1, 20, 1, 203),
        203,
        IngressReject::RetiredPair,
    );
    assert_eq!(
        value.accept_heartbeat(heartbeat(1, 30, 0, 203), MonoRawNs(203)),
        ReducerEffect::MayRequestEnable(ticket)
    );
    assert_eq!(value.snapshot().accepted_counter.unwrap().get(), 2);
    assert_eq!(
        value.gated_command(MonoRawNs(203)),
        Err(IngressReject::GateClosed)
    );
    assert_eq!(
        value.observe_enable_committed(ticket, CompletionOutcome::Committed, MonoRawNs(204)),
        ReducerEffect::EnteredActive
    );
}

#[test]
fn retired_floor_survives_ack_and_uses_same_node_same_port_only() {
    let (mut value, _) = active();
    assert_eq!(
        value.accept_command(command(1, 10, 7, 105, &VALUES), MonoRawNs(105)),
        ReducerEffect::Accepted
    );
    assert_eq!(
        value.accept_heartbeat(heartbeat(1, 20, 900, 105), MonoRawNs(105)),
        ReducerEffect::Accepted
    );
    let _ = value.observe_disabled();
    let ticket = pending(&mut value, 200);
    reject_command(
        &mut value,
        command(1, 10, 7, 202, &VALUES),
        202,
        IngressReject::BelowKnownFence,
    );
    assert_eq!(
        value.accept_command(command(1, 10, 8, 202, &VALUES), MonoRawNs(202)),
        ReducerEffect::Staged
    );
    assert_eq!(
        value.accept_heartbeat(heartbeat(1, 30, 0, 203), MonoRawNs(203)),
        ReducerEffect::MayRequestEnable(ticket)
    );
    assert_eq!(value.current.unwrap().command_cursor.unwrap().seq, 8);
    assert_eq!(value.retired.unwrap().heartbeat_cursor.unwrap().seq, 900);
}

#[test]
fn only_new_hb_and_new_command_together_restore_active_from_lost() {
    for heartbeat_first in [true, false] {
        let (mut value, _) = active();
        assert_eq!(
            value.on_control_lost(MonoRawNs(200)),
            ReducerEffect::EnterSafe(IngressReject::ControlLost)
        );
        let fence = value.loss;
        assert_eq!(
            value.on_control_lost(MonoRawNs(201)),
            ReducerEffect::Unchanged
        );
        assert_eq!(value.loss, fence);
        let cmd = command(1, 10, 1, 202, &VALUES);
        let hb = heartbeat(1, 20, 1, 202);
        let first = if heartbeat_first {
            value.accept_heartbeat(hb, MonoRawNs(202))
        } else {
            value.accept_command(cmd, MonoRawNs(202))
        };
        assert_eq!(first, ReducerEffect::Accepted);
        assert_eq!(
            value.gated_command(MonoRawNs(202)),
            Err(IngressReject::GateClosed)
        );
        let second = if heartbeat_first {
            value.accept_command(cmd, MonoRawNs(202))
        } else {
            value.accept_heartbeat(hb, MonoRawNs(202))
        };
        assert_eq!(second, ReducerEffect::Recovered);
        assert_eq!(value.gated_command(MonoRawNs(202)).unwrap().seq, 2);
    }
}

#[test]
fn ready_lost_recovers_only_ready_until_current_ticket_committed() {
    let (mut value, ticket) = ready_at(100);
    assert_eq!(
        value.on_control_lost(MonoRawNs(200)),
        ReducerEffect::EnterSafe(IngressReject::ControlLost)
    );
    assert_eq!(
        value.observe_enable_committed(ticket, CompletionOutcome::Committed, MonoRawNs(201)),
        ReducerEffect::Rejected(IngressReject::WrongLifecyclePhase)
    );
    assert_eq!(
        value.accept_command(command(1, 10, 1, 202, &VALUES), MonoRawNs(202)),
        ReducerEffect::Accepted
    );
    assert_eq!(
        value.accept_heartbeat(heartbeat(1, 20, 1, 203), MonoRawNs(203)),
        ReducerEffect::MayRequestEnable(ticket)
    );
    assert_eq!(value.snapshot().phase, IngressPhase::ReadyToEnable);
    assert!(!value.snapshot().enable_committed);
    assert_eq!(
        value.gated_command(MonoRawNs(203)),
        Err(IngressReject::GateClosed)
    );
    assert_eq!(
        value.observe_enable_committed(ticket, CompletionOutcome::Committed, MonoRawNs(204)),
        ReducerEffect::EnteredActive
    );
}

#[test]
fn expired_recovery_heartbeat_cannot_pair_with_later_command() {
    let (mut value, _) = active();
    value.contract.heartbeat_timeout_ns = 10;
    let _ = value.on_control_lost(MonoRawNs(200));
    assert_eq!(
        value.accept_heartbeat(heartbeat(1, 20, 1, 201), MonoRawNs(201)),
        ReducerEffect::Accepted
    );
    assert_eq!(
        value.accept_command(command(1, 10, 1, 211, &VALUES), MonoRawNs(211)),
        ReducerEffect::Accepted
    );
    assert_eq!(value.snapshot().phase, IngressPhase::Lost);
    assert_eq!(
        value.gated_command(MonoRawNs(211)),
        Err(IngressReject::GateClosed)
    );
    assert_eq!(
        value.accept_heartbeat(heartbeat(1, 20, 2, 212), MonoRawNs(212)),
        ReducerEffect::Recovered
    );
}

#[test]
fn queued_high_seq_at_or_before_loss_cutoff_does_not_refresh() {
    let (mut value, _) = active();
    let _ = value.on_control_lost(MonoRawNs(200));
    for stamp in [199, 200] {
        reject_command(
            &mut value,
            command(1, 10, 500, stamp, &VALUES),
            201,
            IngressReject::BelowRecoveryCutoff,
        );
        reject_heartbeat(
            &mut value,
            heartbeat(1, 20, 600, stamp),
            201,
            IngressReject::BelowRecoveryCutoff,
        );
    }
    let mut expired = command(1, 10, 500, 201, &VALUES);
    expired.deadline = MonoRawNs(202);
    reject_command(&mut value, expired, 202, IngressReject::ExpiredCommand);
    assert_eq!(value.snapshot().loss_counter_floor, Some(1));
}

#[test]
fn current_expiry_requires_two_new_inputs_instead_of_silent_refresh() {
    let (mut value, _) = active();
    let deadline = value.command.unwrap().deadline.0;
    assert_eq!(
        value.gated_command(MonoRawNs(deadline)),
        Err(IngressReject::ExpiredCommand)
    );
    assert_eq!(
        value.accept_command(command(1, 10, 1, deadline, &VALUES), MonoRawNs(deadline)),
        ReducerEffect::EnterSafe(IngressReject::ControlLost)
    );
    assert_eq!(value.snapshot().accepted_counter.unwrap().get(), 1);
    assert_eq!(
        value.accept_command(
            command(1, 10, 1, deadline + 1, &VALUES),
            MonoRawNs(deadline + 1)
        ),
        ReducerEffect::Accepted
    );
    assert_eq!(
        value.accept_heartbeat(heartbeat(1, 20, 1, deadline + 2), MonoRawNs(deadline + 2)),
        ReducerEffect::Recovered
    );
}

#[test]
fn gate_rechecks_expiry_and_clock_regression() {
    let (value, _) = active();
    assert_eq!(
        value.gated_command(MonoRawNs(102)),
        Err(IngressReject::FutureStamp)
    );
    assert_eq!(
        value.gated_command(value.command.unwrap().deadline),
        Err(IngressReject::ExpiredCommand)
    );
    let mut value = value;
    value.contract.heartbeat_timeout_ns = 10;
    assert_eq!(
        value.gated_command(MonoRawNs(112)),
        Err(IngressReject::ExpiredHeartbeat)
    );
    assert_eq!(
        value.on_tick(MonoRawNs(112)),
        ReducerEffect::EnterSafe(IngressReject::ControlLost)
    );
    assert_eq!(
        value.gated_command(MonoRawNs(112)),
        Err(IngressReject::GateClosed)
    );
}

#[test]
fn regressed_lifecycle_clock_cannot_lower_recovery_cutoff() {
    for from_ready in [true, false] {
        let (mut value, ticket) = if from_ready { ready_at(100) } else { active() };
        let previous = value.snapshot();
        let effect = if from_ready {
            value.observe_enable_committed(ticket, CompletionOutcome::Committed, MonoRawNs(102))
        } else {
            value.on_tick(MonoRawNs(102))
        };
        assert_eq!(
            effect,
            ReducerEffect::EnterSafe(IngressReject::ClockRegression)
        );
        assert_eq!(value.snapshot().phase, IngressPhase::RebindRequired);
        assert_eq!(value.snapshot().accepted_counter, previous.accepted_counter);
        assert_eq!(value.snapshot().retired_identity, previous.current_identity);
        assert!(!value.snapshot().enable_committed);
        reject_command(
            &mut value,
            command(1, 10, 1, 103, &VALUES),
            104,
            IngressReject::WrongLifecyclePhase,
        );
        reject_heartbeat(
            &mut value,
            heartbeat(1, 20, 1, 103),
            104,
            IngressReject::WrongLifecyclePhase,
        );
        let before = value;
        assert_eq!(
            value.begin_explicit_enable(CLOSED, CONNECTION, MonoRawNs(102)),
            Err(IngressReject::ClockRegression)
        );
        assert_eq!(value, before);
        assert_eq!(
            value.gated_command(MonoRawNs(104)),
            Err(IngressReject::GateClosed)
        );
    }
}

#[test]
fn explicit_zero_node_and_port_and_heartbeat_wrap_are_validated() {
    let mut value = reducer();
    let ticket = pending(&mut value, 100);
    assert_eq!(
        value.accept_heartbeat(heartbeat(0, 1, u64::MAX, 102), MonoRawNs(102)),
        ReducerEffect::Staged
    );
    assert_eq!(
        value.accept_command(command(0, 0, 0, 103, &VALUES), MonoRawNs(103)),
        ReducerEffect::MayRequestEnable(ticket)
    );
    assert_eq!(value.snapshot().command_origin, Some(origin(0, 0)));
    assert_eq!(
        value.observe_enable_committed(ticket, CompletionOutcome::Committed, MonoRawNs(104)),
        ReducerEffect::EnteredActive
    );
    for seq in [0, u64::MAX] {
        reject_heartbeat(
            &mut value,
            heartbeat(0, 1, seq, 105),
            105,
            IngressReject::ProducerSeqNotIncreasing,
        );
    }
}

#[test]
fn disable_clears_committed_and_old_ticket_cannot_activate_new_binding() {
    let (mut value, old) = active();
    let _ = value.observe_disabled();
    assert!(!value.snapshot().enable_committed);
    assert_eq!(
        value.gated_command(MonoRawNs(104)),
        Err(IngressReject::GateClosed)
    );
    let new = pending(&mut value, 200);
    assert_ne!(old, new);
    assert_eq!(
        value.accept_command(command(2, 30, 0, 202, &VALUES), MonoRawNs(202)),
        ReducerEffect::Staged
    );
    assert_eq!(
        value.accept_heartbeat(heartbeat(2, 40, 0, 203), MonoRawNs(203)),
        ReducerEffect::MayRequestEnable(new)
    );
    assert_eq!(
        value.observe_enable_committed(old, CompletionOutcome::Committed, MonoRawNs(204)),
        ReducerEffect::Rejected(IngressReject::WrongRebindTicket)
    );
    let _ = value.on_control_lost(MonoRawNs(205));
    assert_eq!(
        value.accept_command(command(2, 30, 1, 206, &VALUES), MonoRawNs(206)),
        ReducerEffect::Accepted
    );
    assert_eq!(
        value.accept_heartbeat(heartbeat(2, 40, 1, 207), MonoRawNs(207)),
        ReducerEffect::MayRequestEnable(new)
    );
    assert_eq!(value.snapshot().phase, IngressPhase::ReadyToEnable);
    assert_eq!(
        value.gated_command(MonoRawNs(207)),
        Err(IngressReject::GateClosed)
    );
}

#[test]
fn accepted_counter_exhaustion_keeps_last_command_and_cursor_uncommitted() {
    let (mut value, _) = active();
    value.accepted_counter = u64::MAX - 1;
    assert_eq!(
        value.accept_command(command(1, 10, 1, 105, &VALUES), MonoRawNs(105)),
        ReducerEffect::Accepted
    );
    assert_eq!(value.gated_command(MonoRawNs(105)).unwrap().seq, u64::MAX);
    let before = value;
    assert_eq!(
        value.accept_command(command(1, 10, 2, 106, &[4.0, 5.0, 6.0]), MonoRawNs(106)),
        ReducerEffect::EnterSafe(IngressReject::CounterExhausted)
    );
    assert_eq!(value.command, before.command);
    assert_eq!(value.current, before.current);
    assert_eq!(value.accepted_counter, u64::MAX);
    assert_eq!(value.snapshot().phase, IngressPhase::Exhausted);
    assert_eq!(
        value.gated_command(MonoRawNs(106)),
        Err(IngressReject::GateClosed)
    );
    assert_eq!(
        value.observe_disabled(),
        ReducerEffect::Rejected(IngressReject::CounterExhausted)
    );
}

#[test]
fn pending_counter_overflow_cannot_commit_partial_pair() {
    let mut value = reducer();
    let _ticket = pending(&mut value, 100);
    value.accepted_counter = u64::MAX;
    assert_eq!(
        value.accept_command(command(1, 10, 0, 102, &VALUES), MonoRawNs(102)),
        ReducerEffect::Staged
    );
    let before = value;
    assert_eq!(
        value.accept_heartbeat(heartbeat(1, 20, 0, 103), MonoRawNs(103)),
        ReducerEffect::EnterSafe(IngressReject::CounterExhausted)
    );
    assert_eq!(value.current, before.current);
    assert_eq!(value.staged_command, before.staged_command);
    assert_eq!(value.staged_heartbeat, before.staged_heartbeat);
    assert_eq!(value.command, None);
    assert_eq!(value.heartbeat, None);
}

#[test]
fn ticket_overflow_is_sticky_and_never_resets_counter() {
    let mut value = reducer();
    value.ticket_counter = u64::MAX;
    value.accepted_counter = 123;
    assert_eq!(
        value.begin_explicit_enable(CLOSED, CONNECTION, MonoRawNs(100)),
        Err(IngressReject::TicketExhausted)
    );
    assert_eq!(value.snapshot().phase, IngressPhase::Exhausted);
    assert_eq!(value.ticket_counter, u64::MAX);
    assert_eq!(value.snapshot().accepted_counter.unwrap().get(), 123);
    assert_eq!(
        value.begin_explicit_enable(CLOSED, CONNECTION, MonoRawNs(101)),
        Err(IngressReject::TicketExhausted)
    );
}

#[test]
fn exhaustion_reason_survives_lifecycle_and_ingress_replay_without_wrap() {
    for reason in [ExhaustionReason::Counter, ExhaustionReason::Ticket] {
        let (mut value, ticket) = active();
        assert_eq!(value.snapshot().exhaustion_reason, None);
        let rejection = reason.rejection();
        match reason {
            ExhaustionReason::Counter => {
                value.accepted_counter = u64::MAX;
                assert_eq!(
                    value.accept_command(command(1, 10, 1, 105, &VALUES), MonoRawNs(105)),
                    ReducerEffect::EnterSafe(rejection)
                );
            }
            ExhaustionReason::Ticket => {
                assert_eq!(
                    value.observe_disabled(),
                    ReducerEffect::EnterSafe(IngressReject::GateClosed)
                );
                value.ticket_counter = u64::MAX;
                value.accepted_counter = 123;
                assert_eq!(
                    value.begin_explicit_enable(CLOSED, CONNECTION, MonoRawNs(105)),
                    Err(rejection)
                );
            }
        }
        let before = value;
        let snapshot = value.snapshot();
        assert_eq!(snapshot.phase, IngressPhase::Exhausted);
        assert_eq!(snapshot.exhaustion_reason, Some(reason));
        assert!(!snapshot.enable_committed);
        for now in [MonoRawNs(106), MonoRawNs(0), MonoRawNs(u64::MAX)] {
            assert_eq!(
                value.begin_explicit_enable(CLOSED, CONNECTION, now),
                Err(rejection)
            );
            assert_eq!(value.observe_disabled(), ReducerEffect::Rejected(rejection));
            assert_eq!(value.on_tick(now), ReducerEffect::Rejected(rejection));
            assert_eq!(
                value.on_control_lost(now),
                ReducerEffect::Rejected(rejection)
            );
            assert_eq!(
                value.observe_internal_zero_ack(
                    ticket,
                    ConnectionEpoch::new(8),
                    CompletionOutcome::Committed,
                    now
                ),
                ReducerEffect::Rejected(rejection)
            );
            assert_eq!(
                value.observe_enable_committed(ticket, CompletionOutcome::Committed, now),
                ReducerEffect::Rejected(rejection)
            );
            // 已耗尽时新身份、旧重放以及无效样本都不得替换原分类或写状态。
            assert_eq!(
                value.accept_command(command(2, 30, u64::MAX, 107, &VALUES), now),
                ReducerEffect::Rejected(rejection)
            );
            let invalid_targets = [f64::NAN; 3];
            let mut invalid_command = command(1, 10, 0, 103, &invalid_targets);
            invalid_command.origin = None;
            assert_eq!(
                value.accept_command(invalid_command, now),
                ReducerEffect::Rejected(rejection)
            );
            assert_eq!(
                value.accept_heartbeat(heartbeat(2, 40, u64::MAX, 107), now),
                ReducerEffect::Rejected(rejection)
            );
            let mut invalid_heartbeat = heartbeat(1, 20, 0, 102);
            invalid_heartbeat.tick_rate_hz = f64::NAN;
            invalid_heartbeat.origin = None;
            assert_eq!(
                value.accept_heartbeat(invalid_heartbeat, now),
                ReducerEffect::Rejected(rejection)
            );
            assert_eq!(value.gated_command(now), Err(IngressReject::GateClosed));
            assert_eq!(value, before);
            assert_eq!(value.snapshot(), snapshot);
        }
        assert_eq!(
            value.require_rebind(IngressReject::IdentityChanged),
            ReducerEffect::Rejected(rejection)
        );
        let opposite = match reason {
            ExhaustionReason::Counter => ExhaustionReason::Ticket,
            ExhaustionReason::Ticket => ExhaustionReason::Counter,
        };
        value.exhaust(opposite);
        assert_eq!(value, before);
        assert_eq!(value.accepted_counter, before.accepted_counter);
        assert_eq!(value.ticket_counter, before.ticket_counter);
    }
}

#[test]
fn retired_slot_is_bounded_to_one_complete_pair() {
    let (mut value, _) = active();
    for node in 2..6 {
        let _ = value.observe_disabled();
        let previous = value.retired.unwrap().identity().unwrap();
        let stamp = 100 * node;
        let ticket = pending(&mut value, stamp);
        assert_eq!(
            value.accept_command(
                command(u128::from(node), 10, 0, stamp + 2, &VALUES),
                MonoRawNs(stamp + 2)
            ),
            ReducerEffect::Staged
        );
        assert_eq!(
            value.accept_heartbeat(
                heartbeat(u128::from(node), 20, 0, stamp + 3),
                MonoRawNs(stamp + 3)
            ),
            ReducerEffect::MayRequestEnable(ticket)
        );
        assert_eq!(value.retired.unwrap().identity(), Some(previous));
        assert_eq!(
            value.observe_enable_committed(
                ticket,
                CompletionOutcome::Committed,
                MonoRawNs(stamp + 4)
            ),
            ReducerEffect::EnteredActive
        );
    }
    assert_eq!(value.retired.unwrap().node.value(), 4);
    assert_eq!(value.current.unwrap().node.value(), 5);
    assert_eq!(value.snapshot().accepted_counter.unwrap().get(), 5);
}

#[test]
fn burst_fold_and_snapshot_pod_are_consistent_fixed_values() {
    let (mut value, _) = active();
    let mut copied = value;
    for seq in 1..=128 {
        let stamp = 104 + seq;
        let input = command(1, 10, seq, stamp, &[0.0, -0.0, 10.0]);
        assert_eq!(
            value.accept_command(input, MonoRawNs(stamp)),
            ReducerEffect::Accepted
        );
        assert_eq!(
            copied.accept_command(input, MonoRawNs(stamp)),
            ReducerEffect::Accepted
        );
        assert_eq!(value, copied);
    }
    let buffer = value.gated_command(MonoRawNs(232)).unwrap();
    assert_eq!(buffer.seq, 129);
    assert_eq!(buffer.joint_count, 3);
    assert_eq!(buffer.mode, ControlModeRepr::Velocity);
    assert_eq!(
        [
            buffer.velocities[0],
            buffer.velocities[1],
            buffer.velocities[2]
        ]
        .map(f32::to_bits),
        [0.0_f32.to_bits(), (-0.0_f32).to_bits(), 10.0_f32.to_bits()]
    );
    assert!(
        buffer.velocities[3..]
            .iter()
            .all(|value| value.to_bits() == 0)
    );
    assert!(
        buffer
            .positions
            .iter()
            .chain(buffer.torques.iter())
            .all(|value| value.to_bits() == 0)
    );
    assert_eq!(
        value.snapshot().targets_rad_s.unwrap().map(f32::to_bits),
        [0.0_f32, -0.0_f32, 10.0_f32].map(f32::to_bits)
    );
}

#[test]
fn public_diagnostics_and_reducer_are_copy_without_drop() {
    fn copy_type<T: Copy>() {}
    const {
        assert!(!std::mem::needs_drop::<BaseIngress>());
    }
    const {
        assert!(!std::mem::needs_drop::<BaseIngressSnapshot>());
    }
    const {
        assert!(!std::mem::needs_drop::<ReducerEffect>());
    }
    copy_type::<BaseIngress>();
    copy_type::<BaseIngressSnapshot>();
    copy_type::<SessionIdentity>();
    assert!(std::mem::size_of::<BaseIngress>() < 2048);
}

#[cfg(feature = "rt_guard")]
#[test]
fn all_pure_hot_paths_run_under_real_allocation_disabler() {
    assert_no_alloc::assert_no_alloc(|| {
        // 夹具和所有状态/负向分支均在guard中，复用lib唯一全局allocator。
        explicit_zero_source_and_full_origin_values_are_preserved();
        both_pair_orders_require_actual_enable_committed();
        pre_ack_staging_is_discarded_with_partial_identity();
        duplicate_zero_ack_cannot_move_barrier_or_erase_new_stage();
        lifecycle_unknown_failed_wrong_ticket_and_connection_are_rejected();
        unknown_and_wrong_source_do_not_invalidate_identity();
        heartbeat_body_and_rate_validation_does_not_refresh_cursor();
        command_shape_layout_mode_and_timeout_are_strict();
        target_nan_infinity_and_profile_limit_are_rejected_atomically();
        f32_actual_conversion_cannot_round_above_profile();
        f32_max_and_f64_out_of_range_are_distinguished();
        ttl_zero_over_budget_deadline_and_future_stamp_are_rejected();
        heartbeat_expiry_upper_bound_and_clock_overflow_are_rejected();
        producer_seq_is_per_port_and_task_tag_never_becomes_pod_seq();
        duplicate_wrap_and_stamp_regression_preserve_state();
        pending_staging_keeps_only_highest_valid_seq_without_counter_advance();
        port_alias_is_rejected_and_full_high_bits_cannot_alias();
        identity_change_does_not_adopt_new_sample_and_old_a_cannot_resume();
        retired_complete_pair_is_refused_but_one_new_port_can_rebind();
        retired_floor_survives_ack_and_uses_same_node_same_port_only();
        only_new_hb_and_new_command_together_restore_active_from_lost();
        ready_lost_recovers_only_ready_until_current_ticket_committed();
        expired_recovery_heartbeat_cannot_pair_with_later_command();
        queued_high_seq_at_or_before_loss_cutoff_does_not_refresh();
        current_expiry_requires_two_new_inputs_instead_of_silent_refresh();
        gate_rechecks_expiry_and_clock_regression();
        regressed_lifecycle_clock_cannot_lower_recovery_cutoff();
        explicit_zero_node_and_port_and_heartbeat_wrap_are_validated();
        disable_clears_committed_and_old_ticket_cannot_activate_new_binding();
        accepted_counter_exhaustion_keeps_last_command_and_cursor_uncommitted();
        pending_counter_overflow_cannot_commit_partial_pair();
        ticket_overflow_is_sticky_and_never_resets_counter();
        exhaustion_reason_survives_lifecycle_and_ingress_replay_without_wrap();
        retired_slot_is_bounded_to_one_complete_pair();
        burst_fold_and_snapshot_pod_are_consistent_fixed_values();
        public_diagnostics_and_reducer_are_copy_without_drop();
    });
}
