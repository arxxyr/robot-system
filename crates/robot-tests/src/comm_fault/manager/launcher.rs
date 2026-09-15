//! 私有定型 launcher：先发布本 Invocation 原件，再原 PID 直接 exec。

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use robot_core::time::{RobotClock, SystemClock};
use robot_s0::comm_fault_observation::ObservationPage;
use robot_s0::comm_fault_trace::TracePage;
use serde_json::{Value, json};

use super::{Mode, write_json};

pub(super) const PREFIX: &str = "--r4c-manager-launch";
pub(super) const ENVIRONMENT: [&str; 4] = [
    "INVOCATION_ID",
    "NOTIFY_SOCKET",
    "WATCHDOG_USEC",
    "WATCHDOG_PID",
];

pub(super) fn digest(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 16_384];
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub(super) fn identity(path: &Path) -> Result<Value, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() {
        return Err(format!("本批身份对象不得是symlink：{}", path.display()));
    }
    Ok(
        json!({"dev":metadata.dev(),"ino":metadata.ino(),"len":metadata.len(),
        "directory":metadata.is_dir(),"mtime":metadata.mtime(),"mtime_nsec":metadata.mtime_nsec()}),
    )
}

pub(super) fn valid_invocation(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && value.bytes().any(|byte| byte != b'0')
}

pub(super) struct Plan {
    pub(super) value: Value,
    pub(super) root: PathBuf,
    pub(super) mode: Mode,
    pub(super) daemon: PathBuf,
    pub(super) robot_id: String,
    pub(super) unit: String,
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("launcher plan缺字符串{key}"))
}

impl Plan {
    pub(super) fn read(path: &Path) -> Result<Self, String> {
        if !path.is_absolute() || std::fs::canonicalize(path).map_err(|e| e.to_string())? != path {
            return Err("launcher只接受本批规范绝对plan路径".into());
        }
        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        let keys = [
            "schema",
            "mode",
            "unit",
            "root",
            "robot_id",
            "s0",
            "launcher",
            "config",
            "trust_identity",
            "directory_identity",
        ];
        let object = value.as_object().ok_or("launcher plan必须是对象")?;
        if object.len() != keys.len()
            || keys.iter().any(|key| !object.contains_key(*key))
            || value["schema"] != 1
        {
            return Err("launcher plan字段集合或版本不符".into());
        }
        let root = PathBuf::from(text(&value, "root")?);
        if path != root.join("launch-plan.json")
            || std::fs::canonicalize(&root).map_err(|e| e.to_string())? != root
        {
            return Err("launcher plan不属于原目录".into());
        }
        let mode = Mode::parse(text(&value, "mode")?)?;
        let unit = text(&value, "unit")?.to_owned();
        if !unit.starts_with("r4c-")
            || !unit.ends_with(".service")
            || !unit
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        {
            return Err("launcher unit不属于本批有限命名".into());
        }
        let daemon = PathBuf::from(text(&value["s0"], "path")?);
        let launcher = PathBuf::from(text(&value["launcher"], "path")?);
        if daemon.file_name().is_none_or(|name| name != "s0_daemon")
            || launcher
                .file_name()
                .is_none_or(|name| name != "comm_fault_e2e")
            || daemon.parent() != launcher.parent()
            || std::env::current_exe().map_err(|e| e.to_string())? != launcher
        {
            return Err("launcher不是同批指定工具/S0实件".into());
        }
        for (artifact, actual) in [
            (&value["s0"], &daemon),
            (&value["launcher"], &launcher),
            (&value["config"], &root.join("robot.toml")),
        ] {
            if text(artifact, "path")? != actual.to_string_lossy()
                || !actual.is_absolute()
                || std::fs::canonicalize(actual).map_err(|e| e.to_string())? != *actual
                || artifact["identity"] != identity(actual)?
                || text(artifact, "blake3")? != digest(actual)?
                || text(artifact, "sha256")?.len() != 64
            {
                return Err(format!("launcher原实件字节/身份不符：{}", actual.display()));
            }
        }
        for (actual, expected) in [
            (&root, &value["directory_identity"]),
            (&root.join("trust"), &value["trust_identity"]),
        ] {
            let now = identity(actual)?;
            if now["dev"] != expected["dev"]
                || now["ino"] != expected["ino"]
                || now["directory"] != true
            {
                return Err("launcher原目录/trust身份不符".into());
            }
        }
        let robot_id = text(&value, "robot_id")?.to_owned();
        validate_configuration(
            &std::fs::read_to_string(root.join("robot.toml")).map_err(|e| e.to_string())?,
            &robot_id,
        )?;
        Ok(Self {
            value,
            root,
            mode,
            daemon,
            robot_id,
            unit,
        })
    }
}

pub(super) fn validate_configuration(document: &str, robot_id: &str) -> Result<(), String> {
    let config: toml::Value = toml::from_str(document).map_err(|e| e.to_string())?;
    if config
        .get("robot")
        .and_then(|value| value.get("id"))
        .and_then(toml::Value::as_str)
        != Some(robot_id)
        || config
            .get("hardware")
            .and_then(toml::Value::as_array)
            .is_none_or(|hardware| {
                hardware.len() != 1
                    || hardware[0].get("id").and_then(toml::Value::as_str) != Some("arm")
                    || hardware[0].get("adapter").and_then(toml::Value::as_str)
                        != Some("mock_serial_rt")
            })
    {
        return Err("launcher只接受原单mock arm配置".into());
    }
    Ok(())
}

pub(super) fn entry(raw: &[String]) -> Option<Result<(), String>> {
    if raw.first().is_none_or(|first| first != PREFIX) {
        return None;
    }
    Some(match raw {
        [_, path] => launch(Path::new(path)),
        _ => Err("私有manager launcher严格只接受一个plan绝对路径".into()),
    })
}

fn generation(root: &Path, invocation: &str, mode: Mode) -> Result<u64, String> {
    for number in 1..=2 {
        let path = root.join(format!("generation-{number}.json"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let value = json!({"generation":number,"invocation":invocation,
                    "pid":std::process::id(),"created_ns":SystemClock.now().0});
                file.write_all(&serde_json::to_vec(&value).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
                file.sync_all().map_err(|e| e.to_string())?;
                if number == 2 && mode != Mode::Restart {
                    return Err("Restart=no场景出现第二代".into());
                }
                return Ok(number);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let prior: Value =
                    serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?;
                if prior["invocation"] == invocation {
                    return Err("同Invocation重复launcher".into());
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    write_json(
        &root.join(format!("unexpected-generation-{invocation}.json")),
        &json!({"invocation":invocation,"pid":std::process::id(),"at_ns":SystemClock.now().0}),
    )?;
    Err("出现未授权第三代，拒绝再次exec".into())
}

fn launch(path: &Path) -> Result<(), String> {
    let plan = Plan::read(path)?;
    let manager_env: BTreeMap<String, Option<String>> = ENVIRONMENT
        .into_iter()
        .map(|key| (key.to_owned(), std::env::var(key).ok()))
        .collect();
    let invocation = manager_env["INVOCATION_ID"]
        .as_deref()
        .ok_or("manager未给INVOCATION_ID")?;
    let pid = std::process::id();
    if !valid_invocation(invocation)
        || manager_env["WATCHDOG_USEC"].as_deref() != Some("2000000")
        || manager_env["NOTIFY_SOCKET"]
            .as_deref()
            .is_none_or(|socket| !socket.starts_with('/'))
        || manager_env["WATCHDOG_PID"]
            .as_deref()
            .is_some_and(|value| value.parse::<u32>().ok() != Some(pid))
    {
        return Err("manager原四环境与本PID/2s watchdog不符".into());
    }
    let number = generation(&plan.root, invocation, plan.mode)?;
    let directory = plan.root.join("invocations").join(invocation);
    std::fs::create_dir(&directory).map_err(|e| format!("Invocation必须create_new：{e}"))?;
    let observation = directory.join("s0.observation");
    let trace = directory.join("s0.trace");
    let page = ObservationPage::create(&observation).map_err(|e| e.to_string())?;
    let trace_page = TracePage::create(&trace).map_err(|e| e.to_string())?;
    let stdout = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("s0.stdout.log"))
        .map_err(|e| e.to_string())?;
    let argv = vec![
        "--config".into(),
        plan.root.join("robot.toml").to_string_lossy().into_owned(),
        "--hardware-id".into(),
        "arm".into(),
        "--latch-path".into(),
        plan.root.join("estop_latch").to_string_lossy().into_owned(),
        "--rx-trust-dir".into(),
        plan.root.join("trust").to_string_lossy().into_owned(),
        "--notify-mode".into(),
        "required".into(),
    ];
    let mut test_env = BTreeMap::from([
        (
            "ROBOT_TEST_OBSERVATION_PATH",
            observation.to_string_lossy().into_owned(),
        ),
        (
            "ROBOT_TEST_TRACE_PATH",
            trace.to_string_lossy().into_owned(),
        ),
    ]);
    match (plan.mode, number) {
        (Mode::Panic, 1) => {
            test_env.insert("ROBOT_TEST_SUP_MODE", "systemd-watchdog-panic".into());
        }
        (Mode::Restart, 1) => {
            test_env.insert("ROBOT_TEST_WRITER_PAUSE", "SyncMarkerFile:4000".into());
        }
        _ => {}
    }
    let inherited_test: Vec<_> = std::env::vars_os()
        .filter(|(key, _)| key.to_string_lossy().starts_with("ROBOT_TEST_"))
        .map(|(key, _)| key)
        .collect();
    let mut command = Command::new(&plan.daemon);
    command
        .args(&argv)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::inherit())
        .env("ROBOT_LOG_FORMAT", "json")
        .env("RUST_LOG", "warn,s0_daemon=info")
        .env("NO_COLOR", "1");
    for key in &inherited_test {
        command.env_remove(key);
    }
    command.envs(&test_env);
    write_json(
        &directory.join("manifest.json"),
        &json!({"schema":1,"generation":number,
        "invocation":invocation,"pid":pid,"unit":plan.unit,"created_ns":SystemClock.now().0,
        "root":plan.root,"robot_id":plan.robot_id,"s0":plan.value["s0"],"launcher":plan.value["launcher"],
        "config":plan.value["config"],"directory_identity":plan.value["directory_identity"],
        "trust_identity":plan.value["trust_identity"],"argv":argv,"manager_env":manager_env,
        "test_env":test_env,"removed_test_env":inherited_test.iter().map(|key|key.to_string_lossy()).collect::<Vec<_>>(),
        "observation_version":4,"observation":observation,"trace":trace,
        "stdout":directory.join("s0.stdout.log"),"stderr":"manager journal"}),
    )?;
    drop(page);
    drop(trace_page);
    Err(format!("原PID直接exec S0失败：{}", command.exec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_claims_never_overwrite_first_and_reject_repeated_or_third_invocation() {
        let root = tempfile::tempdir().unwrap();
        let first = "0123456789abcdef0123456789abcdef";
        let second = "1123456789abcdef0123456789abcdef";
        let third = "2123456789abcdef0123456789abcdef";
        assert_eq!(generation(root.path(), first, Mode::Restart).unwrap(), 1);
        let original = std::fs::read(root.path().join("generation-1.json")).unwrap();
        assert!(generation(root.path(), first, Mode::Restart).is_err());
        assert_eq!(generation(root.path(), second, Mode::Restart).unwrap(), 2);
        assert!(generation(root.path(), third, Mode::Restart).is_err());
        assert_eq!(
            std::fs::read(root.path().join("generation-1.json")).unwrap(),
            original
        );
        assert!(
            root.path()
                .join(format!("unexpected-generation-{third}.json"))
                .is_file()
        );
    }

    #[test]
    fn restart_disabled_mode_cannot_exec_a_second_generation() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            generation(root.path(), "0123456789abcdef0123456789abcdef", Mode::Stop).unwrap(),
            1
        );
        assert!(generation(root.path(), "1123456789abcdef0123456789abcdef", Mode::Stop).is_err());
        assert!(root.path().join("generation-2.json").is_file());
    }
}
