//! 纯oracle输入不冒实际manager证据；真实命令生命周期测试放command模块。

use super::*;
use robot_s0::shutdown_mechanism::HandleKind;
use std::fmt::Write as _;

fn properties() -> Properties {
    let mut result: Properties = oracle::PROPERTIES
        .split(',')
        .map(|key| (key.into(), "0".into()))
        .collect();
    for (key, value) in [
        ("Id", "r4c-test.service"),
        ("LoadState", "loaded"),
        ("Type", "notify"),
        ("NotifyAccess", "main"),
        ("Restart", "on-failure"),
        ("RestartUSec", "2s"),
        ("WatchdogUSec", "2s"),
        ("TimeoutStartUSec", "1min 30s"),
        ("TimeoutStopUSec", "1min 30s"),
        ("TimeoutAbortUSec", "1min 30s"),
        ("ActiveState", "activating"),
        ("SubState", "auto-restart"),
        ("ExecMainPID", "31"),
        ("InvocationID", "0123456789abcdef0123456789abcdef"),
        ("ExecMainCode", "1"),
        ("ExecMainStatus", "90"),
        ("Result", "exit-code"),
        ("ExecMainStartTimestampMonotonic", "10"),
        ("ExecMainExitTimestampMonotonic", "30"),
        ("ActiveEnterTimestampMonotonic", "20"),
    ] {
        result.insert(key.into(), value.into());
    }
    result
}

fn manifest() -> Value {
    json!({"pid":31,"invocation":"0123456789abcdef0123456789abcdef"})
}

fn page() -> ObservationSnapshot {
    let directory = tempfile::tempdir().unwrap();
    let actual = ObservationPage::create(&directory.path().join("page")).unwrap();
    let mut page = actual.snapshot();
    page.main_start_ns = 100;
    page.ready_ns = 200;
    page.runtime_armed_ns = 210;
    page.observation_ready = true;
    page.ready_reached = true;
    page.worker_bitmap = 0xff;
    page.shutdown_deadline_ns = 10_000_000_300;
    page.before_process_exit_ns = 600;
    page.writer_exited_at_ns = 500;
    page.writer_joined_observed_at_ns = 550;
    page
}

fn exit_record(panic: bool, freeze: bool) -> Value {
    let kinds: Vec<_> = HandleKind::ALL
        .into_iter()
        .filter(|kind| freeze || *kind != HandleKind::FreezeControl)
        .collect();
    let joined:Vec<_>=kinds.iter().map(|kind|json!({"kind":format!("{kind:?}"),"ok":!(panic && *kind==HandleKind::SystemdWatchdog)})).collect();
    let order = [
        "ClearRr",
        "SetRr",
        "Publisher",
        "Io",
        "FreezeControl",
        "HookControl",
        "HealthMonitor",
        "SystemdWatchdog",
        "Executor",
        "Ipc",
        "LatchWriter",
        "Watchdog",
    ];
    let stop: Vec<_> = order
        .into_iter()
        .filter(|kind| freeze || *kind != "FreezeControl")
        .enumerate()
        .map(|(index, kind)| json!({"kind":kind,"at_ns":300+index}))
        .collect();
    json!({"at_ns":580,"ack":true,"clear":true,"flushed":1,"residual":0,"writer_join":"stopped",
        "fallback_tag":0,"fallback_settle":0,"stop_trace":stop,"joined":joined,
        "failures":if panic {vec!["Panicked(SystemdWatchdog)"]} else {vec![]}})
}

fn stdout(record: &Value) -> String {
    format!("R4C_EXIT_V1 {record}\nR4C_NOTIFY_STOPPING_V1 mode=configured result=Ok(())\n")
}

#[test]
fn original_manager_properties_reject_missing_duplicate_and_reconfigured_budget() {
    let properties = properties();
    let mut text = String::new();
    for (key, value) in &properties {
        writeln!(text, "{key}={value}").unwrap();
    }
    assert_eq!(oracle::parse_properties(&text).unwrap(), properties);
    assert!(oracle::parse_properties(&format!("{text}MainPID=0\n")).is_err());
    assert!(oracle::parse_properties("MainPID=0\n").is_err());
    assert!(oracle::configuration(&properties, "r4c-test.service", Mode::Restart).is_ok());
    let mut not_started = properties.clone();
    not_started.insert("WatchdogUSec".into(), "infinity".into());
    assert!(oracle::loaded_configuration(&not_started, "r4c-test.service", Mode::Restart).is_ok());
    assert!(oracle::configuration(&not_started, "r4c-test.service", Mode::Restart).is_err());
    for (key, value) in [
        ("WatchdogUSec", "0"),
        ("WatchdogUSec", "4s"),
        ("RestartUSec", "3s"),
        ("NotifyAccess", "all"),
        ("Type", "simple"),
        ("Restart", "no"),
    ] {
        let mut changed = properties.clone();
        changed.insert(key.into(), value.into());
        assert!(oracle::configuration(&changed, "r4c-test.service", Mode::Restart).is_err());
    }
}

#[test]
fn first_exit_requires_original_invocation_pid_and_real_code_90_not_signal_or_restart() {
    let properties = properties();
    let manifest = manifest();
    assert!(oracle::exited(&properties, &manifest, 90, 0).is_ok());
    for (key, value) in [
        ("InvocationID", "fedcba9876543210fedcba9876543210"),
        ("ExecMainPID", "32"),
        ("MainPID", "31"),
        ("ExecMainCode", "2"),
        ("ExecMainStatus", "9"),
        ("Result", "watchdog"),
        ("NRestarts", "1"),
        ("ExecMainExitTimestampMonotonic", "0"),
    ] {
        let mut changed = properties.clone();
        changed.insert(key.into(), value.into());
        assert!(oracle::exited(&changed, &manifest, 90, 0).is_err());
    }
}

#[test]
fn ready_binds_actual_notify_generation_and_rejects_page_only_claim() {
    let mut properties = properties();
    for (key, value) in [
        ("ActiveState", "active"),
        ("SubState", "running"),
        ("MainPID", "31"),
        ("ExecMainExitTimestampMonotonic", "0"),
    ] {
        properties.insert(key.into(), value.into());
    }
    let page = page();
    assert!(oracle::ready(&properties, &manifest(), &page).is_ok());
    let mut changed = properties.clone();
    changed.insert("ActiveEnterTimestampMonotonic".into(), "0".into());
    assert!(oracle::ready(&changed, &manifest(), &page).is_err());
    for mutate in [
        |page: &mut ObservationSnapshot| page.version = 3,
        |page: &mut ObservationSnapshot| page.runtime_armed_ns = 0,
        |page: &mut ObservationSnapshot| page.fired_ns = 1,
    ] {
        let mut changed = page;
        mutate(&mut changed);
        assert!(oracle::ready(&properties, &manifest(), &changed).is_err());
    }
}

#[test]
fn stopping_requires_no_main_pid_no_pending_job_and_no_auto_restart() {
    let mut properties = properties();
    assert!(oracle::stopped(&properties).is_err());
    properties.insert("ActiveState".into(), "inactive".into());
    properties.insert("SubState".into(), "dead".into());
    assert!(oracle::stopped(&properties).is_ok());
    for (key, value) in [
        ("MainPID", "31"),
        ("Job", "19"),
        ("SubState", "auto-restart"),
        ("ActiveState", "activating"),
    ] {
        let mut changed = properties.clone();
        changed.insert(key.into(), value.into());
        assert!(oracle::stopped(&changed).is_err());
    }
}

#[test]
fn original_exit_rejects_duplicate_truncated_join_missing_and_stop_reordering() {
    for freeze in [false, true] {
        let valid = exit_record(false, freeze);
        assert!(oracle::exit_facts(&stdout(&valid), &page(), false, freeze).is_ok());
        assert!(oracle::unique_exit(&format!("R4C_EXIT_V1 {valid}")).is_err());
        assert!(
            oracle::unique_exit(&format!("R4C_EXIT_V1 {valid}\nR4C_EXIT_V1 {valid}\n")).is_err()
        );
        let mut missing = valid.clone();
        missing["joined"].as_array_mut().unwrap().pop();
        assert!(oracle::exit_facts(&stdout(&missing), &page(), false, freeze).is_err());
        let mut reordered = valid.clone();
        reordered["stop_trace"].as_array_mut().unwrap().swap(0, 1);
        assert!(oracle::exit_facts(&stdout(&reordered), &page(), false, freeze).is_err());
        let mut false_gate = valid.clone();
        false_gate["flushed"] = json!(0);
        assert!(oracle::exit_facts(&stdout(&false_gate), &page(), false, freeze).is_err());
    }
}

#[test]
fn watchdog_panic_requires_original_target_join_error_and_entered_fact() {
    let valid = exit_record(true, false);
    let mut page = page();
    page.supervision.helper_owned = 3;
    page.supervision.helper_entered = 2;
    page.supervision.systemd_entered_ns = 250;
    assert!(oracle::exit_facts(&stdout(&valid), &page, true, false).is_ok());
    let mut fake = valid.clone();
    for joined in fake["joined"].as_array_mut().unwrap() {
        joined["ok"] = json!(true);
    }
    assert!(oracle::exit_facts(&stdout(&fake), &page, true, false).is_err());
    page.supervision.helper_entered = 1;
    assert!(oracle::exit_facts(&stdout(&valid), &page, true, false).is_err());
}

#[test]
fn launcher_private_arguments_and_unit_escaping_are_closed() {
    assert!(launcher_entry(&["--phase".into(), "mock".into()]).is_none());
    assert!(launcher_entry(&[launcher::PREFIX.into()]).unwrap().is_err());
    assert!(
        launcher_entry(&[
            launcher::PREFIX.into(),
            "relative.json".into(),
            "extra".into()
        ])
        .unwrap()
        .is_err()
    );
    assert!(quote_unit("/tmp/new\nExecStart=/bin/false").is_err());
    let unit = render_unit(
        Path::new("/tmp/a $x%/comm_fault_e2e"),
        Path::new("/tmp/a $x%/launch-plan.json"),
        Path::new("/tmp/a $x%"),
        Mode::Restart,
    )
    .unwrap();
    assert!(unit.contains("WorkingDirectory=/tmp/a $x%%\n"));
    assert!(unit.contains("\"/tmp/a $$x%%/comm_fault_e2e\""));
    assert!(unit.contains("Restart=on-failure\nRestartSec=2s"));
    assert!(!unit.contains("[Install]"));
    for root in ["relative", "/tmp/new\nExecStart=/bin/false"] {
        assert!(
            render_unit(
                Path::new("/tmp/tool"),
                Path::new("/tmp/plan"),
                Path::new(root),
                Mode::Stop
            )
            .is_err()
        );
    }
}

#[test]
fn launcher_parses_complete_toml_document_and_rejects_missing_or_foreign_hardware() {
    let document = "[robot]\nid = 'r4c'\n[[hardware]]\nid = 'arm'\nadapter = 'mock_serial_rt'\n";
    assert!(launcher::validate_configuration(document, "r4c").is_ok());
    for invalid in [
        document.replace("mock_serial_rt", "real"),
        document.replace("id = 'r4c'", "id = 'other'"),
        document.replace("[robot]", "[other]"),
        format!("{document}[[hardware]]\nid = 'arm'\nadapter = 'mock_serial_rt'\n"),
        "[]".into(),
        String::new(),
    ] {
        assert!(launcher::validate_configuration(&invalid, "r4c").is_err());
    }
}

#[test]
fn node_configuration_is_real_fixed_record_not_default_assumption() {
    let actual = "R4C_NODE_CONFIG_V1 role=clear_rr cleanup_creation=true cleanup_destruction=false source=NodeBuilder_default_global_config\n";
    assert_eq!(node_config(actual).unwrap()["cleanup_destruction"], false);
    assert!(node_config("").is_err());
    assert!(node_config(&format!("{actual}{actual}")).is_err());
    assert!(node_config(&actual.replace("true", "unknown")).is_err());
}

#[test]
fn persistent_oracle_requires_exact_pair_marker_bytes_and_no_temporary() {
    let mut value = json!({"files":{"estop_latch":{"bytes":[49,10]},"estop_latch.bak":{"bytes":[49,10]},
        "estop_latch.clear_pending":{"len":112}}});
    assert!(pair(&value, true, true).is_ok());
    value["files"]["estop_latch.bak"]["bytes"] = json!([48, 10]);
    assert!(pair(&value, true, true).is_err());
    value["files"]["estop_latch.bak"]["bytes"] = json!([49, 10]);
    value["files"]["estop_latch.tmp"] = json!({});
    assert!(pair(&value, true, true).is_err());
}

#[test]
fn exited_s0_identity_requires_actual_pid_and_frozen_executable_inode() {
    let manifest = json!({"pid":123,"s0":{"identity":{"dev":5,"ino":6}}});
    let original = json!({"pid":123,"executable":{"dev":5,"ino":6}});
    assert!(oracle::exited_executable(&manifest, &original).is_ok());
    for bad in [
        json!({}),
        json!({"pid":123,"executable":null}),
        json!({"pid":124,"executable":{"dev":5,"ino":6}}),
        json!({"pid":123,"executable":{"dev":5,"ino":7}}),
    ] {
        assert!(oracle::exited_executable(&manifest, &bad).is_err());
    }
}

#[test]
fn exec_environment_keeps_only_whitelisted_values_and_rejects_unknown_test_keys() {
    let raw = b"INVOCATION_ID=original\0ROBOT_TEST_TRACE_PATH=/trace\0ROBOT_TEST_UNKNOWN=private-test-value\0OTHER_SECRET=private-nontest-value\0";
    let (actual, unexpected) = exec_environment(raw).unwrap();
    assert_eq!(actual.len(), 2);
    assert_eq!(actual["INVOCATION_ID"], "original");
    assert_eq!(actual["ROBOT_TEST_TRACE_PATH"], "/trace");
    assert_eq!(unexpected, ["ROBOT_TEST_UNKNOWN"]);
    let record = serde_json::to_string(&(actual, unexpected)).unwrap();
    assert!(!record.contains("private-test-value"));
    assert!(!record.contains("private-nontest-value"));
}

#[test]
fn ready_exec_environment_still_rejects_missing_changed_or_extra_original_values() {
    let manifest = json!({"manager_env":{
        "INVOCATION_ID":"original","NOTIFY_SOCKET":"/notify",
        "WATCHDOG_PID":"31","WATCHDOG_USEC":"2000000"},
        "test_env":{"ROBOT_TEST_OBSERVATION_PATH":"/page","ROBOT_TEST_TRACE_PATH":"/trace"}});
    let raw = b"INVOCATION_ID=original\0NOTIFY_SOCKET=/notify\0WATCHDOG_PID=31\0WATCHDOG_USEC=2000000\0ROBOT_TEST_OBSERVATION_PATH=/page\0ROBOT_TEST_TRACE_PATH=/trace\0";
    let (actual, unexpected) = exec_environment(raw).unwrap();
    assert!(validate_exec_environment(&manifest, &actual, &unexpected).is_ok());
    for key in launcher::ENVIRONMENT {
        let mut changed = actual.clone();
        changed.remove(key);
        assert!(validate_exec_environment(&manifest, &changed, &[]).is_err());
        changed.insert(key.into(), "different".into());
        assert!(validate_exec_environment(&manifest, &changed, &[]).is_err());
    }
    assert!(validate_exec_environment(&manifest, &BTreeMap::new(), &[]).is_err());
    assert!(validate_exec_environment(&manifest, &actual, &["ROBOT_TEST_UNKNOWN".into()]).is_err());
    let mut extra = actual;
    extra.insert(
        "ROBOT_TEST_WRITER_PAUSE".into(),
        "SyncMarkerFile:4000".into(),
    );
    assert!(validate_exec_environment(&manifest, &extra, &[]).is_err());
}
