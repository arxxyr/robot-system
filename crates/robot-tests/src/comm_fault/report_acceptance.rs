//! AC-Q 独立证据入口验收；清单反例不能通过场景 PASS 绕过构建身份核验。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use super::super::{Args, Phase};
use super::{Report, file_hash, read_build_manifest, verify_build_manifest};

const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "robot-report-acceptance-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn manifest(name: &str, hash: &str) -> Value {
    let features = match name {
        "s0_daemon" => vec!["mock", "test-comm-fault-hook"],
        "comm_fault_e2e" => vec!["comm-fault-e2e"],
        "robot_rpc_gateway" => vec!["default"],
        _ => panic!("独立夹具不认识的角色"),
    };
    json!({"sha256": hash, "features": features, "profile": "release",
        "command": ["cargo", "+nightly", "build", "--release", "--bin", name],
        "exit_code": 0, "base_commit": "独立夹具附加字段", "deployable": false})
}

fn reject_field(field: &str, values: &[Value]) {
    for value in values {
        let mut record = manifest("s0_daemon", HASH);
        record[field] = value.clone();
        assert!(
            verify_build_manifest("s0_daemon", &record, HASH).is_err(),
            "非法清单不能通过：{field}={value}"
        );
    }
}

#[test]
fn all_roles_accept_explicit_successful_manifests_and_preserve_extra_metadata() {
    for name in ["s0_daemon", "comm_fault_e2e", "robot_rpc_gateway"] {
        for profile in ["dev", "release", "test"] {
            let mut record = manifest(name, HASH);
            record["profile"] = json!(profile);
            verify_build_manifest(name, &record, HASH).unwrap();
            assert_eq!(record["deployable"], false);
        }
    }
    // gateway 的普通构建有效；不能凭角色擅自要求它携带注入 hook。
    let gateway = manifest("robot_rpc_gateway", HASH);
    assert_eq!(gateway["features"], json!(["default"]));
    verify_build_manifest("robot_rpc_gateway", &gateway, HASH).unwrap();
}

#[test]
fn missing_required_fields_and_nonobject_records_are_rejected() {
    for field in ["sha256", "features", "profile", "command", "exit_code"] {
        let mut record = manifest("s0_daemon", HASH);
        assert!(record.as_object_mut().unwrap().remove(field).is_some());
        assert!(
            verify_build_manifest("s0_daemon", &record, HASH).is_err(),
            "不能省略必填字段 {field}"
        );
    }
    for record in [Value::Null, json!([]), json!("UNKNOWN"), json!(true)] {
        assert!(verify_build_manifest("s0_daemon", &record, HASH).is_err());
    }
}

#[test]
fn unknown_empty_mixed_and_duplicate_features_cannot_establish_build_identity() {
    reject_field(
        "features",
        &[
            Value::Null,
            json!([]),
            json!("test-comm-fault-hook"),
            json!([""]),
            json!([" "]),
            json!(["UNKNOWN"]),
            json!(["unknown"]),
            json!([false]),
            json!(["test-comm-fault-hook", 7]),
            json!(["test-comm-fault-hook", null]),
            json!(["test-comm-fault-hook", " "]),
            json!(["test-comm-fault-hook", "UNKNOWN"]),
            json!(["test-comm-fault-hook", "test-comm-fault-hook"]),
            json!([" test-comm-fault-hook"]),
            json!(["test-comm-fault-hook\n"]),
        ],
    );
    for (name, features) in [
        ("s0_daemon", json!(["comm-fault-e2e"])),
        ("comm_fault_e2e", json!(["test-comm-fault-hook"])),
    ] {
        let mut record = manifest(name, HASH);
        record["features"] = features;
        assert!(
            verify_build_manifest(name, &record, HASH).is_err(),
            "其它产物的特性不能替代 {name} 的必要特性"
        );
    }
}

#[test]
fn profile_command_and_success_exit_code_must_be_explicit() {
    reject_field(
        "profile",
        &[
            Value::Null,
            json!(""),
            json!(" "),
            json!("UNKNOWN"),
            json!("debug"),
            json!("release "),
            json!("custom"),
            json!(0),
        ],
    );
    reject_field(
        "command",
        &[
            Value::Null,
            json!([]),
            json!("cargo build"),
            json!([""]),
            json!([" "]),
            json!(["UNKNOWN"]),
            json!(["cargo", null]),
            json!(["cargo", 7]),
            json!(["cargo", "\t\n"]),
        ],
    );
    reject_field(
        "exit_code",
        &[
            Value::Null,
            json!(1),
            json!(-1),
            json!("0"),
            json!(false),
            json!(0.0),
        ],
    );
}

#[test]
fn malformed_hash_or_another_binary_hash_cannot_match_a_manifest() {
    reject_field(
        "sha256",
        &[
            Value::Null,
            json!(""),
            json!("UNKNOWN"),
            json!("0".repeat(64)),
            json!(HASH[..63].to_owned()),
            json!(format!("{HASH}0")),
        ],
    );
    for malformed in ["x".repeat(64), "0".repeat(63), "0".repeat(65)] {
        // 即使清单与输入字符串相等，也不能接受无效 hash 形状。
        let record = manifest("s0_daemon", &malformed);
        assert!(verify_build_manifest("s0_daemon", &record, &malformed).is_err());
    }
}

#[test]
fn actual_manifest_reads_reject_absence_invalid_json_and_changed_artifacts() {
    let directory = Directory::new();
    let name = "s0_daemon";
    let path = directory.0.join(format!("{name}.build.json"));
    assert!(read_build_manifest(&directory.0, name, HASH).is_err());
    std::fs::write(&path, b"{\"sha256\":").unwrap();
    assert!(read_build_manifest(&directory.0, name, HASH).is_err());

    let binary = directory.0.join(name);
    std::fs::write(&binary, "独立产物一".as_bytes()).unwrap();
    let hash = file_hash(&binary).unwrap();
    let record = manifest(name, &hash);
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    assert_eq!(
        read_build_manifest(&directory.0, name, &hash).unwrap(),
        record
    );
    std::fs::write(&binary, "独立产物二".as_bytes()).unwrap();
    assert!(read_build_manifest(&directory.0, name, &file_hash(&binary).unwrap()).is_err());
}

fn report_args(directory: &Directory) -> Args {
    let bin_dir = directory.0.join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    for name in ["s0_daemon", "robot_rpc_gateway", "comm_fault_e2e"] {
        let path = bin_dir.join(name);
        match name {
            "comm_fault_e2e" => {
                std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &path).unwrap();
            }
            _ => std::fs::write(&path, name.as_bytes()).unwrap(),
        }
        let record = manifest(name, &file_hash(&path).unwrap());
        std::fs::write(
            bin_dir.join(format!("{name}.build.json")),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
    }
    Args {
        phase: Phase::Mock,
        bin_dir,
        evidence_dir: directory.0.join("evidence"),
        case: None,
        config: None,
        hardware_id: None,
    }
}

#[test]
fn report_new_requires_every_manifest_before_any_case_can_pass() {
    let directory = Directory::new();
    let args = report_args(&directory);
    let report = Report::new(&args).unwrap();
    assert_eq!(report.artifacts.len(), 3);
    assert!(!args.evidence_dir.join("report.json").exists());
    std::fs::remove_file(args.bin_dir.join("robot_rpc_gateway.build.json")).unwrap();
    assert!(Report::new(&args).is_err());
    assert!(
        !args.evidence_dir.join("report.json").exists(),
        "构建身份不完整时不能写出 PASS 报告"
    );
}

#[test]
fn software_pass_keeps_unmeasured_hardware_pending_and_empty_or_failed_cases_fail() {
    let directory = Directory::new();
    let args = report_args(&directory);
    let mut report = Report::new(&args).unwrap();
    assert!(!report.finish().unwrap(), "零场景不能通过");
    report.record(
        "software_fixture",
        "AC-Q",
        0,
        Ok(json!({"scope": "独立报告测试"})),
    );
    assert!(report.finish().unwrap());
    let payload: Value =
        serde_json::from_slice(&std::fs::read(args.evidence_dir.join("report.json")).unwrap())
            .unwrap();
    assert_eq!(payload["status"], "PASS");
    for gate in ["H1", "H2", "H3"] {
        assert_eq!(payload["hardware"][gate], "NEEDS_HUMAN");
    }
    assert_eq!(payload["hardware"]["D15"], "OPEN");
    assert_eq!(payload["artifacts"], json!(report.artifacts));
    report.record("deliberate_failure", "AC-Q", 0, Err("独立负例".to_owned()));
    assert!(!report.finish().unwrap(), "已有成功场景不能掩盖后续失败");
}
