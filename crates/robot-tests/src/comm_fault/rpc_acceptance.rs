//! AC17 独立边界反例：IPC 与串口周期不能混用，RTT 必须来自一对真实时间戳。

use super::{verify_fault_visibility, verify_rpc_retirement};
use robot_s0::comm_fault_trace::{TraceEvent, TraceKind};
use serde_json::json;

fn chain(first_to_mailbox_ns: u64) -> Vec<TraceEvent> {
    let first = 1_000_000_000;
    let mailbox = first + first_to_mailbox_ns;
    [
        (TraceKind::FirstCommError, first),
        (TraceKind::MailboxPublishing, mailbox),
        (TraceKind::MailboxPublished, mailbox + 100),
        (TraceKind::FaultIpcCommit, mailbox + 500_000),
    ]
    .into_iter()
    .map(|(kind, stamp_ns)| TraceEvent {
        sample_seq: 1,
        stamp_ns,
        kind,
        source: 2,
        normal_seq: 19,
        generation: 3,
        a: 7,
        b: 0,
    })
    .collect()
}

fn check(
    first_to_mailbox_ns: u64,
    mailbox_to_response_ns: u64,
) -> Result<serde_json::Value, String> {
    let response = 1_000_000_000 + first_to_mailbox_ns + mailbox_to_response_ns;
    verify_fault_visibility(
        &chain(first_to_mailbox_ns),
        &json!({
            "request_ns": response - 200_000, "response_ns": response, "rtt_ns": 200_000,
        }),
    )
}

#[test]
fn rpc_retirement_uses_two_half_millisecond_ipc_ticks_without_old_slack() {
    assert!(verify_rpc_retirement(10, 1_001_000_010).is_ok());
    for elapsed in [1_001_000_001, 1_010_000_000, 1_020_000_000] {
        assert!(verify_rpc_retirement(10, 10 + elapsed).is_err());
    }
    assert!(verify_rpc_retirement(10, 9).is_err());
}

#[test]
fn mailbox_bound_rejects_values_inside_the_old_serial_tick_based_limit() {
    assert!(check(0, 34_200_000).is_ok());
    for elapsed in [34_200_001, 40_000_000, 43_200_000] {
        assert!(check(0, elapsed).is_err());
    }
}

#[test]
fn first_error_bound_separately_adds_one_serial_tick_and_the_inflight_io_bound() {
    let evidence = check(110_000_000, 34_200_000).unwrap();
    assert_eq!(evidence["configured_ipc_tick_ns"], 500_000);
    assert_eq!(evidence["configured_serial_tick_ns"], 5_000_000);
    assert_eq!(evidence["first_error_bound_ns"], 144_200_000);
    for first_to_mailbox in [110_000_001, 115_000_000, 119_000_000] {
        assert!(check(first_to_mailbox, 34_200_000).is_err());
    }
}

#[test]
fn invented_rtt_cannot_expand_the_fault_visibility_budget() {
    let sample = json!({"request_ns": 1_033_800_000u64, "response_ns": 1_034_000_000u64,
        "rtt_ns": 20_000_000});
    assert!(verify_fault_visibility(&chain(0), &sample).is_err());
}
