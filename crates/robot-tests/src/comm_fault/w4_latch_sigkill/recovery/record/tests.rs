//! 固定记录的纯解析与窗口反例；诊断字节不作为磁盘凭证或进程证据。
use super::*;
use std::fmt::Write as _;

fn hex_text(bytes: &[u8]) -> String {
    let mut value = String::new();
    for byte in bytes {
        write!(&mut value, "{byte:02x}").unwrap();
    }
    value
}

fn fixture() -> String {
    let txn = (1_u128 << 112) | 7;
    let mut bytes = [0_u8; 112];
    bytes[..8].copy_from_slice(b"RBTCLRTX");
    bytes[8] = 1;
    bytes[16..48].fill(19);
    bytes[48..64].copy_from_slice(&txn.to_le_bytes());
    bytes[64..72].copy_from_slice(&29_u64.to_le_bytes());
    bytes[72] = 1;
    bytes[80..112].fill(17);
    format!(
        "{TAG} pid=31 base=100 reached=200 pause_until=250000200 dir_dev=41 dir_ino=73 marker_dev=41 marker_ino=79 txn={txn:032x} epoch=29 latched=1 record={} checksum={} prefix={PREFIX}\n",
        hex_text(&bytes),
        hex_text(&bytes[80..])
    )
}

#[test]
fn complete_record_has_exact_bytes_and_ignores_unrelated_complete_logs() {
    let line = fixture();
    let record = extract(line.as_bytes()).unwrap().unwrap();
    assert_eq!(record.raw, line);
    assert_eq!(record.pid, 31);
    assert_eq!(record.txn, (1_u128 << 112) | 7);
    assert_eq!(record.bytes.len(), 112);
    assert_eq!(record.bytes[80..], [17; 32]);
    assert_eq!(
        extract(format!("{{\"message\":\"unrelated\"}}\n{line}ordinary\n").as_bytes()).unwrap(),
        Some(record)
    );
}

#[test]
fn every_truncation_waits_only_without_a_previous_committed_record() {
    let line = fixture();
    for at in 0..line.len() {
        assert!(parse_line(&line[..at]).is_err(), "cut={at}");
        assert!(
            extract(&line.as_bytes()[..at]).unwrap().is_none(),
            "cut={at}"
        );
    }
    for tail in [
        "R4_RECOVERY_",
        "R4_RECOVERY_REACHED_V1 pid=",
        "R4_RECOVERY_REACHED_V\n",
    ] {
        assert!(extract(format!("{line}{tail}").as_bytes()).is_err());
    }
}

#[test]
fn duplicate_interleaved_oversized_or_noncanonical_record_is_rejected() {
    let line = fixture();
    for bad in [
        format!("{line}{line}"),
        format!("other{line}"),
        line.replace(" pid=31", " pid=031"),
        line.replace(" epoch=29", " epoch=+29"),
        line.replace(" pid=31", " pid=0"),
        line.replace(" latched=1", " latched=true"),
        line.replace(" dir_ino=73", " dir_ino=0"),
        line.replace(" marker_ino=79", " marker_ino=0"),
        line.replace(" pid=31 base=100", " base=100 pid=31"),
        line.replace(" epoch=29", " epoch=29 duplicate=1"),
        line.replace("RenamePrimary\n", "RenamePrimary extra\n"),
        line.replace('\n', "\r\n"),
        format!("{TAG} {}\n", "x".repeat(MAX_RECORD_BYTES)),
        format!("{line}R4_RECOVERY_DIAGNOSTIC_V1 failure\n"),
    ] {
        assert!(extract(bad.as_bytes()).is_err(), "{bad}");
    }
}

#[test]
fn exact_prefix_and_all_identity_fields_remain_coupled_to_original_record_bytes() {
    let line = fixture();
    for (from, to) in [
        (" epoch=29", " epoch=30"),
        (" latched=1", " latched=0"),
        (
            " txn=00010000000000000000000000000007",
            " txn=00010000000000000000000000000008",
        ),
        ("ReadMarker,ValidateMarker,", "ValidateMarker,ReadMarker,"),
        ("SyncMarkerFile,", ""),
        ("checksum=1111", "checksum=2211"),
        ("record=524254434c525458", "record=524254434C525458"),
    ] {
        let bad = line.replace(from, to);
        assert_ne!(bad, line, "反例必须实际改变字段：{from}");
        assert!(extract(bad.as_bytes()).is_err(), "{from}");
    }
}

#[test]
fn original_cp1_and_pause_windows_reject_equal_late_reversed_and_overflowed_times() {
    let record = extract(fixture().as_bytes()).unwrap().unwrap();
    assert!(record.window(200, 201).is_ok());
    assert!(
        record
            .window(record.pause_until - 1, record.pause_until - 1)
            .is_ok()
    );
    for (before, after) in [
        (199, 200),
        (201, 200),
        (200, record.pause_until),
        (record.pause_until, record.pause_until),
    ] {
        assert!(record.window(before, after).is_err());
    }
    for changed in [
        Record {
            pause_until: record.reached,
            ..record.clone()
        },
        Record {
            pause_until: record.reached + 250_000_001,
            ..record.clone()
        },
        Record {
            reached: record.cp1().unwrap() - 100,
            pause_until: record.cp1().unwrap(),
            ..record.clone()
        },
        Record {
            base: u64::MAX,
            ..record.clone()
        },
        Record {
            base: 201,
            ..record
        },
    ] {
        assert!(changed.window(changed.reached, changed.reached).is_err());
    }
}
