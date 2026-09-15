//! 原 manager 属性和原退出事实的拒绝判据，不生成生命周期事件。

use std::collections::{BTreeMap, BTreeSet};

use robot_s0::comm_fault_observation::ObservationSnapshot;
use robot_s0::shutdown_mechanism::HandleKind;
use serde_json::Value;

use super::{Mode, launcher};

pub(super) const PROPERTIES: &str = "Id,LoadState,Type,NotifyAccess,Restart,RestartUSec,WatchdogUSec,TimeoutStartUSec,TimeoutStopUSec,TimeoutAbortUSec,ActiveState,SubState,MainPID,ExecMainPID,InvocationID,ExecMainCode,ExecMainStatus,Result,NRestarts,ExecMainStartTimestampMonotonic,ExecMainExitTimestampMonotonic,ActiveEnterTimestampMonotonic,InactiveEnterTimestampMonotonic,WatchdogTimestampMonotonic,Job";
pub(super) type Properties = BTreeMap<String, String>;

pub(super) fn parse_properties(text: &str) -> Result<Properties, String> {
    let mut result = BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line.split_once('=').ok_or("manager属性行缺等号")?;
        if !PROPERTIES.split(',').any(|expected| key == expected)
            || result.insert(key.into(), value.into()).is_some()
        {
            return Err(format!("manager未知/重复属性：{key}"));
        }
    }
    for key in PROPERTIES.split(',') {
        if !result.contains_key(key) {
            return Err(format!("manager缺实际属性：{key}"));
        }
    }
    Ok(result)
}

pub(super) fn number(properties: &Properties, key: &str) -> Result<u64, String> {
    properties
        .get(key)
        .ok_or_else(|| format!("缺{key}"))?
        .parse()
        .map_err(|_| format!("manager {key}非整数"))
}

pub(super) fn configuration(properties: &Properties, unit: &str, mode: Mode) -> Result<(), String> {
    loaded_configuration(properties, unit, mode)?;
    if !matches!(
        properties.get("WatchdogUSec").map(String::as_str),
        Some("2s" | "2000000us")
    ) {
        return Err(format!(
            "本代manager有效WatchdogUSec必须原2s：{properties:?}"
        ));
    }
    Ok(())
}

/// systemd 255在真正start前的WatchdogUSec是尚未初始化的原运行值，不冒unit配置。
pub(super) fn loaded_configuration(
    properties: &Properties,
    unit: &str,
    mode: Mode,
) -> Result<(), String> {
    for (key, expected) in [
        ("Id", unit),
        ("LoadState", "loaded"),
        ("Type", "notify"),
        ("NotifyAccess", "main"),
        (
            "Restart",
            if mode == Mode::Restart {
                "on-failure"
            } else {
                "no"
            },
        ),
    ] {
        if properties.get(key).map(String::as_str) != Some(expected) {
            return Err(format!("manager有效{key}不符：{properties:?}"));
        }
    }
    if !matches!(
        properties.get("RestartUSec").map(String::as_str),
        Some("2s" | "2000000us")
    ) {
        return Err(format!("manager有效RestartUSec必须原2s：{properties:?}"));
    }
    for key in ["TimeoutStartUSec", "TimeoutStopUSec", "TimeoutAbortUSec"] {
        if properties.get(key).is_none_or(String::is_empty) {
            return Err(format!("缺有效{key}"));
        }
    }
    Ok(())
}

pub(super) fn invocation(properties: &Properties, manifest: &Value) -> Result<(), String> {
    let id = manifest["invocation"]
        .as_str()
        .ok_or("manifest缺Invocation")?;
    let pid = manifest["pid"]
        .as_u64()
        .filter(|pid| *pid > 0)
        .ok_or("manifest缺PID")?;
    if !launcher::valid_invocation(id)
        || properties.get("InvocationID").map(String::as_str) != Some(id)
        || number(properties, "ExecMainPID")? != pid
        || number(properties, "ExecMainStartTimestampMonotonic")? == 0
    {
        return Err(format!("manager属性不属于原Invocation/PID：{properties:?}"));
    }
    Ok(())
}

pub(super) fn ready(
    properties: &Properties,
    manifest: &Value,
    page: &ObservationSnapshot,
) -> Result<(), String> {
    invocation(properties, manifest)?;
    if properties["ActiveState"] != "active"
        || properties["SubState"] != "running"
        || number(properties, "MainPID")? != manifest["pid"].as_u64().ok_or("缺PID")?
        || number(properties, "ExecMainExitTimestampMonotonic")? != 0
        || number(properties, "ActiveEnterTimestampMonotonic")?
            < number(properties, "ExecMainStartTimestampMonotonic")?
        || page.version != 4
        || !page.observation_ready
        || page.main_start_ns == 0
        || page.ready_ns < page.main_start_ns
        || page.runtime_armed_ns < page.ready_ns
        || !page.ready_reached
        || page.worker_bitmap != 0xff
        || page.fired_ns != 0
    {
        return Err(format!(
            "原notify READY/RuntimeArmed与同代manager active不符：{properties:?} {page:?}"
        ));
    }
    Ok(())
}

pub(super) fn exited(
    properties: &Properties,
    manifest: &Value,
    code: u64,
    restarts: u64,
) -> Result<(), String> {
    invocation(properties, manifest)?;
    if number(properties, "MainPID")? != 0
        || number(properties, "ExecMainCode")? != 1
        || number(properties, "ExecMainStatus")? != code
        || number(properties, "NRestarts")? != restarts
        || number(properties, "ExecMainExitTimestampMonotonic")?
            <= number(properties, "ExecMainStartTimestampMonotonic")?
        || properties["Result"] != if code == 0 { "success" } else { "exit-code" }
    {
        return Err(format!(
            "原manager出口不是同代CLD_EXITED/{code}：{properties:?}"
        ));
    }
    Ok(())
}

pub(super) fn stopped(properties: &Properties) -> Result<(), String> {
    if number(properties, "MainPID")? != 0
        || !matches!(properties["ActiveState"].as_str(), "inactive" | "failed")
        || !matches!(properties["SubState"].as_str(), "dead" | "failed")
        || !matches!(properties["Job"].as_str(), "" | "0")
    {
        return Err(format!(
            "隔离unit仍有PID/活动状态/待重启job：{properties:?}"
        ));
    }
    Ok(())
}

pub(super) fn unique_exit(stdout: &str) -> Result<Value, String> {
    let mut records = Vec::new();
    for line in stdout.split_inclusive('\n') {
        if let Some(payload) = line.strip_prefix("R4C_EXIT_V1 ") {
            if !line.ends_with('\n') {
                return Err("原退出记录截断".into());
            }
            records.push(serde_json::from_str::<Value>(payload).map_err(|e| e.to_string())?);
        } else if line.contains("R4C_EXIT_V1") {
            return Err("原退出记录前缀/行交错".into());
        }
    }
    match records.as_slice() {
        [record] => Ok(record.clone()),
        _ => Err(format!("原退出记录必须唯一，实际{}", records.len())),
    }
}

pub(super) fn exit_facts(
    stdout: &str,
    page: &ObservationSnapshot,
    panic: bool,
    freeze: bool,
) -> Result<Value, String> {
    let record = unique_exit(stdout)?;
    let expected: Vec<_> = HandleKind::ALL
        .into_iter()
        .filter(|kind| freeze || *kind != HandleKind::FreezeControl)
        .map(|kind| format!("{kind:?}"))
        .collect();
    let joined = record["joined"].as_array().ok_or("缺原joined集合")?;
    let mut found = BTreeSet::new();
    for item in joined {
        let kind = item["kind"].as_str().ok_or("joined缺kind")?;
        let expected_ok = !(panic && kind == "SystemdWatchdog");
        if !expected.iter().any(|entry| entry == kind)
            || !found.insert(kind)
            || item["ok"].as_bool() != Some(expected_ok)
        {
            return Err(format!("原joined身份/真结果不符：{item}"));
        }
    }
    if found.len() != expected.len()
        || record["ack"] != true
        || record["clear"] != true
        || record["flushed"] != 1
        || record["residual"] != 0
        || !matches!(
            record["writer_join"].as_str(),
            Some("stopped" | "stopped_after_ipc_disconnect")
        )
    {
        return Err(format!("原门/原槽回收集合不完整：{record}"));
    }
    let failures = record["failures"].as_array().ok_or("缺原failures")?;
    match (panic, failures.as_slice()) {
        (false, []) => {}
        (true, [failure]) if failure == "Panicked(SystemdWatchdog)" => {}
        _ => return Err(format!("原失败集合不符：{failures:?}")),
    }
    let expected_stop = [
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
    let expected_stop: Vec<_> = expected_stop
        .into_iter()
        .filter(|kind| freeze || *kind != "FreezeControl")
        .collect();
    let trace = record["stop_trace"].as_array().ok_or("缺原stop_trace")?;
    if trace.len() != expected_stop.len() {
        return Err("原stop_trace长度不符".into());
    }
    let mut previous = 0;
    for (event, kind) in trace.iter().zip(expected_stop) {
        let at = event["at_ns"]
            .as_u64()
            .filter(|at| *at > 0)
            .ok_or("stop时戳缺失")?;
        if event["kind"] != kind || at < previous {
            return Err("原stop调用顺序/时戳倒序".into());
        }
        previous = at;
    }
    if record["at_ns"]
        .as_u64()
        .is_none_or(|at| at < previous || at > page.before_process_exit_ns)
        || page.fired_ns != 0
        || page.residual_bitmap != 0
        || page.exit_residual_bitmap != 0
        || page.shutdown_deadline_ns == 0
        || page.before_process_exit_ns == 0
        || page.before_process_exit_ns >= page.shutdown_deadline_ns
        || page.writer_exited_at_ns == 0
        || page.writer_joined_observed_at_ns < page.writer_exited_at_ns
    {
        return Err(format!("原普通退出页/真writer回收不符：{page:?}"));
    }
    if panic
        && (page.supervision.helper_owned & 2 == 0
            || page.supervision.helper_entered != 2
            || page.supervision.systemd_entered_ns < page.ready_ns
            || page.supervision.systemd_entered_ns == 0
            || page.supervision.systemd_entered_ns > page.before_process_exit_ns
            || page.supervision.health_entered_ns != 0
            || page.residual_bitmap & u64::from(HandleKind::SystemdWatchdog.bit()) != 0)
    {
        return Err(format!("原watchdog真实Entered/目标槽回收不符：{page:?}"));
    }
    notify_stopping(stdout)?;
    Ok(record)
}

fn notify_stopping(stdout: &str) -> Result<(), String> {
    let stopping: Vec<_> = stdout
        .split_inclusive('\n')
        .filter(|line| line.starts_with("R4C_NOTIFY_STOPPING_V1 "))
        .collect();
    if stopping.len() != 1
        || stdout
            .lines()
            .filter(|line| line.contains("R4C_NOTIFY_STOPPING_V1"))
            .count()
            != 1
        || stopping[0] != "R4C_NOTIFY_STOPPING_V1 mode=configured result=Ok(())\n"
    {
        return Err(format!("原一次notify_stopping调用缺失/失败：{stopping:?}"));
    }
    Ok(())
}

/// 同代原S0在真实退出路径读出的身份，不能用计划exec或父进程字段替代。
pub(super) fn exited_executable(manifest: &Value, record: &Value) -> Result<(), String> {
    let pid = record["pid"]
        .as_u64()
        .filter(|value| *value > 0)
        .ok_or("原S0退出PID缺失")?;
    let dev = record["executable"]["dev"]
        .as_u64()
        .ok_or("原S0退出exe设备号缺失")?;
    let ino = record["executable"]["ino"]
        .as_u64()
        .filter(|value| *value > 0)
        .ok_or("原S0退出exe inode缺失")?;
    if manifest["pid"] != pid
        || manifest["s0"]["identity"]["dev"] != dev
        || manifest["s0"]["identity"]["ino"] != ino
    {
        return Err("同代S0退出PID/真实exe inode与已验实件不符".into());
    }
    Ok(())
}
