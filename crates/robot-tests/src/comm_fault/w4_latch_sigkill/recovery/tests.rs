//! 原页读取后只构造验收反例，不将合成快照当作实际 daemon 观测。
use super::*;
use robot_s0::comm_fault_observation::ObservationPage;

#[test]
fn preworker_oracle_rejects_every_later_lifecycle_boundary_and_wrong_generation() {
    let path = std::env::temp_dir().join(format!("robot-r4b4-oracle-{}.page", std::process::id()));
    let page = ObservationPage::create(&path).unwrap();
    let mut snapshot = page.snapshot();
    drop(page);
    std::fs::remove_file(path).unwrap();
    snapshot.observation_ready = true;
    snapshot.main_start_ns = 100;
    snapshot.preinit_ns = 90;
    assert!(before_workers(&snapshot, 100).is_ok());
    assert!(before_workers(&snapshot, 101).is_err());
    for changed in [
        ObservationSnapshot {
            observation_ready: false,
            ..snapshot
        },
        ObservationSnapshot {
            preinit_ns: 0,
            ..snapshot
        },
        ObservationSnapshot {
            spawned_ns: 110,
            ..snapshot
        },
        ObservationSnapshot {
            ready_ns: 110,
            ..snapshot
        },
        ObservationSnapshot {
            ready_reached: true,
            ..snapshot
        },
        ObservationSnapshot {
            runtime_armed_ns: 110,
            ..snapshot
        },
        ObservationSnapshot {
            worker_bitmap: 1,
            ..snapshot
        },
        ObservationSnapshot {
            first_io_ns: 110,
            ..snapshot
        },
        ObservationSnapshot {
            serial_calls: [1, 0, 0, 0, 0, 0],
            ..snapshot
        },
        ObservationSnapshot {
            hook_ready: true,
            ..snapshot
        },
        ObservationSnapshot {
            fired_ns: 110,
            ..snapshot
        },
        ObservationSnapshot {
            supervisor_exit_ns: 110,
            ..snapshot
        },
    ] {
        assert!(before_workers(&changed, 100).is_err());
    }
}

fn originals() -> (Record, Value, Value) {
    let bytes = [7; 112];
    let record = Record {
        pid: 31,
        base: 100,
        reached: 200,
        pause_until: 250_000_200,
        dir_dev: 41,
        dir_ino: 73,
        marker_dev: 41,
        marker_ino: 79,
        txn: (1_u128 << 112) | 7,
        epoch: 29,
        latched: true,
        bytes,
        raw: "纯诊断输入".into(),
    };
    let snapshot = json!({"directory":{"dev":41,"ino":73},"files":{"estop_latch.clear_pending":{"dev":41,"ino":79,"bytes":bytes.as_slice(),"len":112}}});
    let chain = json!({"txn_id":record.txn.to_string(),"expected_epoch":29});
    (record, snapshot, chain)
}

#[test]
fn recovery_record_must_match_original_pid_directory_marker_and_full_transaction() {
    let (record, snapshot, chain) = originals();
    assert!(identity(&record, &snapshot, &chain, 31).is_ok());
    assert!(identity(&record, &snapshot, &chain, 32).is_err());
    for changed in [
        Record {
            dir_dev: 42,
            ..record.clone()
        },
        Record {
            dir_ino: 74,
            ..record.clone()
        },
        Record {
            marker_dev: 42,
            ..record.clone()
        },
        Record {
            marker_ino: 80,
            ..record.clone()
        },
        Record {
            txn: record.txn + 1,
            ..record.clone()
        },
        Record {
            epoch: 30,
            ..record.clone()
        },
        Record {
            bytes: [8; 112],
            ..record.clone()
        },
        Record {
            latched: false,
            ..record
        },
    ] {
        assert!(identity(&changed, &snapshot, &chain, 31).is_err());
    }
}

fn intermediate_fixture(pid: u32) -> Value {
    let hash = format!("仅诊断{pid}");
    json!({"directory":{"present":true,"dev":41,"ino":73},"lock":{"present":true,"dev":41,"ino":77},
        "config":{"ino":78,"bytes":[1]},"trust":{"ino":80},
        "trust_files":{"rx_trust.lock":hash},"trust_lock":{"present":true,"dev":41,"ino":81,"directory":false,"nlink":1,
            "bytes":format!("{pid}\n").into_bytes(),"sha256":hash},
        "files":{"estop_latch":{"bytes":[48,10],"ino":83},"estop_latch.bak":{"bytes":[48,10],"ino":84},
            "estop_latch.clear_pending":{"bytes":[7],"ino":79}}})
}

#[test]
fn second_generation_requires_same_marker_unchanged_backup_and_no_extra_temporary() {
    let before = intermediate_fixture(31);
    let mut after = intermediate_fixture(32);
    after["files"]["estop_latch"] = json!({"bytes":[49,10],"ino":85});
    assert!(intermediate_files(&before, &after, (31, 32)).is_ok());
    for (object, key, value) in [
        (
            "files",
            "estop_latch.clear_pending",
            json!({"bytes":[8],"ino":79}),
        ),
        (
            "files",
            "estop_latch.bak",
            json!({"bytes":[49,10],"ino":84}),
        ),
        (
            "files",
            "estop_latch.bak",
            json!({"bytes":[48,10],"ino":85}),
        ),
        ("files", "estop_latch", json!({"bytes":[48,10],"ino":85})),
        ("files", "estop_latch.clear_tmp.32.1", json!({"bytes":[]})),
        ("directory", "ino", json!(74)),
        ("lock", "ino", json!(78)),
        ("config", "bytes", json!([2])),
    ] {
        let mut changed = after.clone();
        changed[object][key] = value;
        assert!(
            intermediate_files(&before, &changed, (31, 32)).is_err(),
            "{object}/{key}"
        );
    }
}
