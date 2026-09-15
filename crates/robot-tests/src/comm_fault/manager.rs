//! R4c：隔离本地 user unit 的真实监督、StopJob 和自然重启证明。

mod command;
mod launcher;
mod oracle;
#[cfg(test)]
mod tests;
mod unit_reference;

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use iceoryx2::prelude::{Node, NodeBuilder};
use iceoryx2::service::ipc::Service;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::markers;
use robot_s0::clear_estop_transaction::{ClearTxnStage, ClearTxnStore};
use robot_s0::comm_fault_observation::{ObservationPage, ObservationSnapshot};
use robot_transport::iceoryx2::request_response::{Iceoryx2PendingResponse, Iceoryx2RrClient};
use serde_json::{Value, json};

use super::harness::Case;
use super::report::Report;
use super::supervision::{snapshot, validate_file_step};
use super::w4_latch_crash::receive_with_closed_tail;
use super::w4_latch_sigkill::initial_latch;
use super::{Args, Phase};
use command::Runner;
use oracle::Properties;

const START_NS: u64 = 12_000_000_000;
const STOP_NS: u64 = 15_000_000_000;

const EXEC_TEST_ENVIRONMENT: [&str; 3] = [
    "ROBOT_TEST_OBSERVATION_PATH",
    "ROBOT_TEST_TRACE_PATH",
    "ROBOT_TEST_WRITER_PAUSE",
];

fn exec_environment(bytes: &[u8]) -> Result<(BTreeMap<String, String>, Vec<String>), String> {
    let mut actual = BTreeMap::new();
    let mut unexpected = Vec::new();
    for entry in bytes.split(|byte| *byte == 0) {
        let Some(split) = entry.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let key = std::str::from_utf8(&entry[..split]).map_err(|e| e.to_string())?;
        if launcher::ENVIRONMENT.contains(&key) || EXEC_TEST_ENVIRONMENT.contains(&key) {
            actual.insert(
                key.to_owned(),
                String::from_utf8(entry[split + 1..].to_vec()).map_err(|e| e.to_string())?,
            );
        } else if key.starts_with("ROBOT_TEST_") {
            unexpected.push(key.to_owned());
        }
    }
    Ok((actual, unexpected))
}

fn validate_exec_environment(
    manifest: &Value,
    actual: &BTreeMap<String, String>,
    unexpected_test_keys: &[String],
) -> Result<(), String> {
    for key in launcher::ENVIRONMENT {
        if actual.get(key).map(String::as_str) != manifest["manager_env"][key].as_str() {
            return Err(format!("exec丢失/改变原{key}"));
        }
    }
    let actual_test: BTreeMap<_, _> = actual
        .iter()
        .filter(|(key, _)| key.starts_with("ROBOT_TEST_"))
        .collect();
    if !unexpected_test_keys.is_empty()
        || serde_json::to_value(actual_test).map_err(|e| e.to_string())? != manifest["test_env"]
    {
        return Err("本代实际test env超出闭合集合".into());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Panic,
    Stop,
    Restart,
}

impl Mode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "S8" => Ok(Self::Panic),
            "S11" => Ok(Self::Stop),
            "S12" => Ok(Self::Restart),
            _ => Err("manager只接受S8/S11/S12".into()),
        }
    }
    const fn tag(self) -> &'static str {
        match self {
            Self::Panic => "S8",
            Self::Stop => "S11",
            Self::Restart => "S12",
        }
    }
    const fn name(self) -> &'static str {
        match self {
            Self::Panic => "manager_watchdog_real_loop_panic_is_joined",
            Self::Stop => "manager_stop_job_preserves_original_order_and_clean_exit",
            Self::Restart => "manager_file_step_90_naturally_restarts_same_directory",
        }
    }
}

/// `raw` 是 argv 去掉程序名后的完整数组；只匹配本模块私有固定前缀。
pub(super) fn launcher_entry(raw: &[String]) -> Option<Result<(), String>> {
    launcher::entry(raw)
}

pub(super) async fn run(args: &Args, report: &mut Report) -> Result<(), String> {
    let modes: &[Mode] = match (args.phase, args.case.as_deref()) {
        (Phase::Mock, Some("R4C-MANAGER")) => &[Mode::Stop, Mode::Restart],
        (Phase::Mock, Some("R4C-MANAGER-UNWIND")) => &[Mode::Panic],
        _ => return Err("manager场景必须显式mock筛选，不能由All隐式执行".into()),
    };
    for &mode in modes {
        let start = SystemClock.now().0;
        report.record(mode.name(), mode.tag(), start, run_case(args, mode).await);
    }
    Ok(())
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let temporary = path.with_file_name(format!(
        "{}.writing",
        path.file_name().ok_or("证据文件无名称")?.to_string_lossy()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| e.to_string())?;
    file.write_all(&serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    // hard_link 原子发布且拒绝覆盖；读者不会把写入中的manifest当完成。
    std::fs::hard_link(&temporary, path).map_err(|e| e.to_string())?;
    std::fs::remove_file(temporary).map_err(|e| e.to_string())
}

fn read_json(path: &Path) -> Result<Value, String> {
    serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

struct Request {
    pending: Option<Iceoryx2PendingResponse<markers::ClearEStopResponse>>,
    client: Option<Iceoryx2RrClient<markers::ClearEStopRequest, markers::ClearEStopResponse>>,
    node: Node<Service>,
}

struct Managed {
    case: Case,
    mode: Mode,
    unit: String,
    unit_path: PathBuf,
    runner: Runner,
    installed: bool,
    start_requested: bool,
    stopped: bool,
    request: Option<Request>,
    plan: Option<Value>,
    reference: Option<unit_reference::UnitReference>,
}

async fn run_case(args: &Args, mode: Mode) -> Result<Value, String> {
    let case = Case::new(&args.evidence_dir, &args.bin_dir, mode.name(), 1)?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or("缺XDG_RUNTIME_DIR，真实user manager门不可运行")?;
    if !runtime.is_absolute() || !runtime.is_dir() {
        return Err("XDG_RUNTIME_DIR不是存在的绝对目录".into());
    }
    let unit = format!(
        "r4c-{}-{}.service",
        case.robot_id,
        mode.tag().to_ascii_lowercase()
    );
    let unit_directory = runtime.join("systemd/user");
    std::fs::create_dir_all(&unit_directory).map_err(|e| e.to_string())?;
    let runner = Runner::new(&case.directory.join("commands"))?;
    let mut owner = Managed {
        case,
        mode,
        unit_path: unit_directory.join(&unit),
        unit,
        runner,
        installed: false,
        start_requested: false,
        stopped: false,
        request: None,
        plan: None,
        reference: None,
    };
    let result = owner.exercise().await;
    let cleanup = owner.cleanup().await;
    let evidence = json!({"mode":mode.tag(),"directory":owner.case.directory,"result":result,"cleanup":cleanup,
        "unit":owner.unit,"manager_owns_daemon":true,"daemon_child_wait_used":false,
        "external_node_cleanup_used":false,"no_live_unit_confirmed":owner.stopped});
    let saved = write_json(&owner.case.directory.join("result.json"), &evidence);
    if owner.start_requested && !owner.stopped {
        eprintln!(
            "manager未能确认daemon退出，禁止丢弃原unit/RR拥有域：{evidence}；保存结果={saved:?}"
        );
        std::process::abort();
    }
    saved?;
    match (result, cleanup) {
        (Ok(_), Ok(_)) => Ok(evidence),
        (first, cleanup) => Err(format!(
            "原场景={first:?}；真实manager收尾={cleanup:?}；证据={}",
            owner.case.directory.display()
        )),
    }
}

impl Managed {
    async fn systemctl(&mut self, options: &[&str], until: u64) -> Result<command::Output, String> {
        let mut argv = vec!["--user".to_owned(), "--no-pager".to_owned()];
        argv.extend(options.iter().map(|arg| (*arg).to_owned()));
        self.runner
            .run(Path::new("/usr/bin/systemctl"), &argv, until)
            .await
    }

    async fn show(&mut self, until: u64) -> Result<Properties, String> {
        let unit = self.unit.clone();
        let property = format!("--property={}", oracle::PROPERTIES);
        let output = self
            .systemctl(&["show", &unit, &property], until)
            .await?
            .require_success()?;
        oracle::parse_properties(&output.stdout)
    }

    async fn artifact(&mut self, path: &Path, build: bool) -> Result<Value, String> {
        let before = launcher::identity(path)?;
        let sha256 = self.runner.sha256(path).await?;
        let blake3 = launcher::digest(path)?;
        if before != launcher::identity(path)? {
            return Err("原实件在两种digest采集间变化".into());
        }
        let mut result = json!({"path":path,"sha256":sha256,"blake3":blake3,"identity":before});
        if build {
            let sidecar = path.with_file_name(format!(
                "{}.build.json",
                path.file_name().ok_or("产物无名称")?.to_string_lossy()
            ));
            let manifest = read_json(&sidecar)?;
            if manifest["sha256"] != result["sha256"] || manifest["exit_code"] != 0 {
                return Err("原旁车未绑定实际成功构建字节".into());
            }
            result["build"] = manifest;
        }
        Ok(result)
    }

    #[allow(clippy::too_many_lines)] // unit创建即登记于原owner，按序保存每项启动身份后才允许start。
    async fn prepare(&mut self) -> Result<(), String> {
        let version = self
            .runner
            .run(
                Path::new("/usr/bin/systemctl"),
                &["--version".into()],
                SystemClock.now().0 + 3_000_000_000,
            )
            .await?
            .require_success()?;
        let probe = self
            .systemctl(&["is-system-running"], SystemClock.now().0 + 3_000_000_000)
            .await?;
        if probe
            .status
            .code()
            .is_none_or(|code| !(0..=2).contains(&code))
            || !matches!(
                probe.stdout.trim(),
                "running" | "degraded" | "starting" | "initializing"
            )
        {
            return Err(format!("真实user manager不可用：{}", probe.evidence));
        }
        write_json(
            &self.case.directory.join("manager-preflight.json"),
            &json!({"version":version.evidence,"manager":probe.evidence}),
        )?;
        std::fs::create_dir(self.case.directory.join("invocations")).map_err(|e| e.to_string())?;
        std::fs::create_dir(self.case.directory.join("journals")).map_err(|e| e.to_string())?;
        if self.mode == Mode::Restart {
            for path in [
                &self.case.latch,
                &self.case.directory.join("estop_latch.bak"),
            ] {
                std::fs::write(path, b"1\n").map_err(|e| e.to_string())?;
            }
        }
        let daemon = std::fs::canonicalize(self.case.bin_dir.join("s0_daemon"))
            .map_err(|e| e.to_string())?;
        let tool = std::fs::canonicalize(self.case.bin_dir.join("comm_fault_e2e"))
            .map_err(|e| e.to_string())?;
        if std::env::current_exe().map_err(|e| e.to_string())? != tool {
            return Err("manager launcher与当前同批工具路径不符".into());
        }
        let s0 = self.artifact(&daemon, true).await?;
        let launcher = self.artifact(&tool, true).await?;
        let config_path = self.case.config.clone();
        let config = self.artifact(&config_path, false).await?;
        let features = s0["build"]["features"]
            .as_array()
            .ok_or("S0旁车缺features")?;
        if !features
            .iter()
            .any(|feature| feature == "test-comm-fault-hook")
        {
            return Err("S0未明确启用原hook".into());
        }
        if self.mode == Mode::Panic && (s0["build"]["profile"] != "dev" || !cfg!(panic = "unwind"))
        {
            return Err("S8只能接受实际dev/unwind组合，不将abort冒真join".into());
        }
        let plan = json!({"schema":1,"mode":self.mode.tag(),"unit":self.unit,"root":self.case.directory,
            "robot_id":self.case.robot_id,"s0":s0,"launcher":launcher,"config":config,
            "trust_identity":launcher::identity(&self.case.trust)?,"directory_identity":launcher::identity(&self.case.directory)?});
        let plan_path = self.case.directory.join("launch-plan.json");
        write_json(&plan_path, &plan)?;
        self.plan = Some(plan);
        let rendered = render_unit(&tool, &plan_path, &self.case.directory, self.mode)?;
        write_json(
            &self.case.directory.join("unit.json"),
            &json!({"path":self.unit_path,"bytes":rendered}),
        )?;
        let mut installed = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&self.unit_path)
            .map_err(|e| e.to_string())?;
        self.installed = true;
        installed
            .write_all(rendered.as_bytes())
            .map_err(|e| e.to_string())?;
        installed.sync_all().map_err(|e| e.to_string())?;
        self.systemctl(&["daemon-reload"], SystemClock.now().0 + 5_000_000_000)
            .await?
            .require_success()?;
        let properties = self.show(SystemClock.now().0 + 3_000_000_000).await?;
        oracle::loaded_configuration(&properties, &self.unit, self.mode)?;
        write_json(
            &self.case.directory.join("loaded-unit-before-start.json"),
            &json!(properties),
        )?;
        let reference_until = SystemClock.now().0 + 3_000_000_000;
        self.reference = Some(unit_reference::UnitReference::connect(
            &self.unit,
            reference_until,
        )?);
        let reference = self
            .reference
            .as_mut()
            .ok_or("原bus owner缺失")?
            .acquire(reference_until)?;
        write_json(
            &self.case.directory.join("unit-reference-acquired.json"),
            &reference,
        )?;
        let unit = self.unit.clone();
        self.start_requested = true;
        self.systemctl(
            &["start", "--no-block", &unit],
            SystemClock.now().0 + 3_000_000_000,
        )
        .await?
        .require_success()?;
        Ok(())
    }

    fn generations(&self, maximum: u64) -> Result<(), String> {
        if self
            .case
            .directory
            .join(format!("generation-{}.json", maximum + 1))
            .exists()
        {
            return Err("出现未允许的后继generation标识".into());
        }
        for entry in std::fs::read_dir(&self.case.directory).map_err(|e| e.to_string())? {
            if entry
                .map_err(|e| e.to_string())?
                .file_name()
                .to_string_lossy()
                .starts_with("unexpected-generation-")
            {
                return Err("出现第三代launcher原件".into());
            }
        }
        let entries = std::fs::read_dir(self.case.directory.join("invocations"))
            .map_err(|e| e.to_string())?;
        let mut count = 0_u64;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry.file_type().map_err(|e| e.to_string())?.is_dir()
                || !launcher::valid_invocation(&entry.file_name().to_string_lossy())
            {
                return Err("Invocation目录集合损坏".into());
            }
            count += 1;
        }
        if count > maximum {
            return Err("Invocation目录数超过原代次上限".into());
        }
        Ok(())
    }

    async fn manifest(&self, generation: u64, until: u64) -> Result<Value, String> {
        loop {
            self.generations(if self.mode == Mode::Restart { 2 } else { 1 })?;
            let index = self
                .case
                .directory
                .join(format!("generation-{generation}.json"));
            if index.exists() {
                // 代次文件只在新launcher开始时写一次；完成manifest原子发布。
                if let Ok(identity) = read_json(&index) {
                    let invocation = identity["invocation"].as_str().ok_or("代次缺Invocation")?;
                    if !launcher::valid_invocation(invocation) {
                        return Err("代次Invocation非法".into());
                    }
                    let path = self
                        .case
                        .directory
                        .join("invocations")
                        .join(invocation)
                        .join("manifest.json");
                    if path.exists() {
                        let manifest = read_json(&path)?;
                        let plan = self.plan.as_ref().ok_or("原plan缺失")?;
                        if manifest["generation"] != generation
                            || manifest["invocation"] != invocation
                            || manifest["pid"] != identity["pid"]
                            || manifest["unit"] != self.unit
                            || manifest["s0"] != plan["s0"]
                            || manifest["config"] != plan["config"]
                            || manifest["root"] != plan["root"]
                            || manifest["observation_version"] != 4
                        {
                            return Err("本代manifest不绑定原计划/代次/PID".into());
                        }
                        if SystemClock.now().0 >= until {
                            return Err("成功manifest接纳已越过原父窗".into());
                        }
                        return Ok(manifest);
                    }
                }
            }
            if SystemClock.now().0 >= until {
                return Err("本代launcher manifest未在原父窗内出现".into());
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    fn invocation_path(&self, manifest: &Value) -> Result<PathBuf, String> {
        Ok(self
            .case
            .directory
            .join("invocations")
            .join(manifest["invocation"].as_str().ok_or("缺Invocation")?))
    }

    fn page(&self, manifest: &Value) -> Result<ObservationSnapshot, String> {
        ObservationPage::open(&self.invocation_path(manifest)?.join("s0.observation"))
            .map(|page| page.snapshot())
            .map_err(|e| e.to_string())
    }

    fn verify_exec(&self, manifest: &Value, ready: &Properties, until: u64) -> Result<(), String> {
        let pid = manifest["pid"].as_u64().ok_or("缺原PID")?;
        let proc = PathBuf::from(format!("/proc/{pid}"));
        let started = SystemClock.now().0;
        let mut observation = json!({"pid":pid,"invocation":manifest["invocation"],
            "ready_properties":ready,"read_started_ns":started,"deadline_ns":until,
            "expected_exe":manifest["s0"]["path"],"expected_identity":manifest["s0"]["identity"],
            "expected_manager_env":manifest["manager_env"],"expected_test_env":manifest["test_env"]});
        // READY已由原manager和本代页证明；随后一次读取，不重试已观察到的不匹配。
        let result = (|| {
            if started >= until {
                return Err("原exec采样开始已超过父窗".into());
            }
            oracle::invocation(ready, manifest)?;
            if ready["ActiveState"] != "active" || oracle::number(ready, "MainPID")? != pid {
                return Err("原exec采样缺本代存活READY事实".into());
            }
            let actual = std::fs::read_link(proc.join("exe"))
                .map_err(|e| format!("无法在原PID存活时核exec：{e}"))?;
            observation["exe"] = json!(actual);
            let metadata = std::fs::metadata(proc.join("exe")).map_err(|e| e.to_string())?;
            observation["dev"] = json!(metadata.dev());
            observation["ino"] = json!(metadata.ino());
            observation["environment_read_started_ns"] = json!(SystemClock.now().0);
            let environment = std::fs::read(proc.join("environ"));
            observation["environment_read_finished_ns"] = json!(SystemClock.now().0);
            let environment = environment.map_err(|e| e.to_string())?;
            observation["environment_bytes"] = json!(environment.len());
            let (actual_env, unexpected_test_keys) = exec_environment(&environment)?;
            observation["env"] = json!(actual_env);
            observation["unexpected_test_keys"] = json!(unexpected_test_keys);
            if actual != Path::new(manifest["s0"]["path"].as_str().ok_or("缺原S0路径")?)
                || manifest["s0"]["identity"]["dev"] != metadata.dev()
                || manifest["s0"]["identity"]["ino"] != metadata.ino()
            {
                return Err("原PID exec到不同S0路径/inode".into());
            }
            validate_exec_environment(manifest, &actual_env, &unexpected_test_keys)
        })();
        let observed = SystemClock.now().0;
        let result = result.and_then(|()| {
            if observed >= until || observed < started {
                Err("原exec成功观察超过父窗".into())
            } else {
                Ok(())
            }
        });
        observation["at_ns"] = json!(observed);
        observation["result"] = json!(result);
        // 成败均先保存原白名单样本；不落完整environ或未知测试变量的值。
        write_json(
            &self
                .case
                .directory
                .join(format!("exec-{}.json", manifest["generation"])),
            &observation,
        )?;
        result
    }

    async fn ready(&mut self, manifest: &Value, until: u64) -> Result<Properties, String> {
        loop {
            let properties = self
                .show(until.min(SystemClock.now().0 + 1_000_000_000))
                .await?;
            let page = self.page(manifest)?;
            if properties["ActiveState"] == "active" && page.runtime_armed_ns != 0 {
                oracle::configuration(&properties, &self.unit, self.mode)?;
                oracle::ready(&properties, manifest, &page)?;
                if SystemClock.now().0 >= until {
                    return Err("manager READY接纳超过原父窗".into());
                }
                write_json(
                    &self
                        .case
                        .directory
                        .join(format!("ready-{}.json", manifest["generation"])),
                    &json!({"properties":properties,"page":snapshot(&page),"at_ns":SystemClock.now().0}),
                )?;
                return Ok(properties);
            }
            if oracle::number(&properties, "ExecMainExitTimestampMonotonic")? != 0
                || SystemClock.now().0 >= until
            {
                return Err(format!(
                    "原notify READY前退出/超时：{properties:?} {page:?}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    async fn exited(
        &mut self,
        manifest: &Value,
        code: u64,
        restarts: u64,
        until: u64,
    ) -> Result<Properties, String> {
        loop {
            let properties = self
                .show(until.min(SystemClock.now().0 + 1_000_000_000))
                .await?;
            if properties["InvocationID"]
                != manifest["invocation"].as_str().ok_or("缺Invocation")?
            {
                return Err("原Invocation退出属性被下一代覆盖，不能推断".into());
            }
            if oracle::number(&properties, "ExecMainExitTimestampMonotonic")? != 0 {
                oracle::configuration(&properties, &self.unit, self.mode)?;
                oracle::exited(&properties, manifest, code, restarts)?;
                if SystemClock.now().0 >= until {
                    return Err("原同代退出成功观察已越窗".into());
                }
                write_json(
                    &self
                        .case
                        .directory
                        .join(format!("exit-{}.json", manifest["generation"])),
                    &json!({"properties":properties,"page":snapshot(&self.page(manifest)?),"at_ns":SystemClock.now().0}),
                )?;
                return Ok(properties);
            }
            if SystemClock.now().0 >= until {
                return Err("原manager同代退出超时".into());
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    async fn journal(&mut self, manifest: &Value) -> Result<String, String> {
        let invocation = manifest["invocation"].as_str().ok_or("缺Invocation")?;
        let argv = vec![
            format!("_SYSTEMD_USER_UNIT={}", self.unit),
            format!("_SYSTEMD_INVOCATION_ID={invocation}"),
            "--no-pager".into(),
            "--output=json".into(),
            "--all".into(),
            "--lines=2000".into(),
        ];
        let output = self
            .runner
            .run(
                Path::new("/usr/bin/journalctl"),
                &argv,
                SystemClock.now().0 + 3_000_000_000,
            )
            .await?
            .require_success()?;
        let mut messages = String::new();
        for line in output.stdout.lines() {
            let item: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
            if item["_SYSTEMD_INVOCATION_ID"] != invocation
                || item["_SYSTEMD_USER_UNIT"] != self.unit
            {
                return Err("journal混入其它Invocation".into());
            }
            if let Some(message) = item["MESSAGE"].as_str() {
                messages.push_str(message);
                messages.push('\n');
            }
        }
        write_json(
            &self
                .case
                .directory
                .join("journals")
                .join(format!("{invocation}.json")),
            &json!({"invocation":invocation,"command":output.evidence,"messages":messages}),
        )?;
        if messages.lines().any(|line| {
            line.starts_with("R4C_DIAGNOSTIC_V1")
                || line.starts_with("R4C_NODE_CONFIG_WRITE_FAILED")
        }) {
            return Err("本代真实journal报告R4c观察写出失败".into());
        }
        Ok(messages)
    }

    fn freeze_feature(&self) -> Result<bool, String> {
        Ok(
            self.plan.as_ref().ok_or("缺plan")?["s0"]["build"]["features"]
                .as_array()
                .ok_or("缺features")?
                .iter()
                .any(|feature| feature == "test-freeze-hook"),
        )
    }

    fn validate_exit_facts(&self, manifest: &Value, panic: bool) -> Result<Value, String> {
        let stdout = std::fs::read_to_string(self.invocation_path(manifest)?.join("s0.stdout.log"))
            .map_err(|e| e.to_string())?;
        node_config(&stdout)?;
        oracle::exit_facts(
            &stdout,
            &self.page(manifest)?,
            panic,
            self.freeze_feature()?,
        )
    }

    async fn stop_job(&mut self, manifest: &Value, restarts: u64) -> Result<Value, String> {
        let started = SystemClock.now().0;
        let until = started + STOP_NS;
        let unit = self.unit.clone();
        let job = self
            .systemctl(&["stop", &unit], until)
            .await?
            .require_success()?;
        let properties = self.exited(manifest, 0, restarts, until).await?;
        oracle::stopped(&properties)?;
        self.stopped = true;
        let facts = self.validate_exit_facts(manifest, false)?;
        Ok(
            json!({"stop_job":job.evidence,"started_ns":started,"deadline_ns":until,"properties":properties,"original_exit":facts}),
        )
    }

    async fn exercise(&mut self) -> Result<Value, String> {
        self.prepare().await?;
        let first_until = SystemClock.now().0 + START_NS;
        let first = self.manifest(1, first_until).await?;
        match self.mode {
            Mode::Panic => {
                let exited = self.exited(&first, 1, 0, first_until).await?;
                // 快速panic可能错过active采样；原notify ActiveEnter仍必须属于本代启动。
                let page = self.page(&first)?;
                if oracle::number(&exited, "ActiveEnterTimestampMonotonic")?
                    < oracle::number(&exited, "ExecMainStartTimestampMonotonic")?
                    || page.ready_ns == 0
                    || page.runtime_armed_ns < page.ready_ns
                    || page.worker_bitmap != 0xff
                {
                    return Err("S8没有本代manager接受READY和真实RuntimeArmed事实".into());
                }
                let facts = self.validate_exit_facts(&first, true)?;
                oracle::exited_executable(&first, &facts)?;
                let journal = self.journal(&first).await?;
                self.generations(1)?;
                Ok(
                    json!({"manifest":first,"manager_exit":exited,"original_exit":facts,"journal":journal,
                    "ready_evidence":"同代Type=notify ActiveEnterTimestampMonotonic与原READY页"}),
                )
            }
            Mode::Stop => {
                let ready = self.ready(&first, first_until).await?;
                self.verify_exec(&first, &ready, first_until)?;
                let stop = self.stop_job(&first, 0).await?;
                let journal = self.journal(&first).await?;
                self.generations(1)?;
                Ok(json!({"manifest":first,"manager_ready":ready,"stop":stop,"journal":journal}))
            }
            Mode::Restart => self.restart(first, first_until).await,
        }
    }

    fn start_clear(&mut self) -> Result<Value, String> {
        self.request = Some(Request {
            pending: None,
            client: None,
            node: NodeBuilder::new()
                .create::<Service>()
                .map_err(|e| e.to_string())?,
        });
        let request = self.request.as_mut().ok_or("原RR拥有域缺失")?;
        let node_configuration = request.node.config();
        let config = json!({"source":"原父RR Node::config",
            "cleanup_dead_nodes_on_creation":node_configuration.global.node.cleanup_dead_nodes_on_creation,
            "cleanup_dead_nodes_on_destruction":node_configuration.global.node.cleanup_dead_nodes_on_destruction,
            "full_debug":format!("{node_configuration:?}")});
        request.client = Some(
            robot_transport::iceoryx2::rr_client_with::<
                markers::ClearEStopRequest,
                markers::ClearEStopResponse,
            >(
                &request.node,
                &format!("/{}/control/clear_estop", self.case.robot_id),
                &robot_transport::iceoryx2::RrProfile::S0_CONTROL,
            )
            .map_err(|e| e.to_string())?,
        );
        let client = request.client.as_ref().ok_or("原Clear client缺失")?;
        let at = SystemClock.now().0;
        let id = format!("r4c-s12-{}", self.case.robot_id);
        let deadline = at + 5_000_000_000;
        let body = robot_msg::build_helpers::build_clear_estop_request(
            &id,
            &id,
            "r4c-manager",
            robot_msg::msg::RpcRole::Maintainer,
            "确认已停止",
            "原manager监督文件步骤",
            false,
            deadline,
            at,
        );
        request.pending = Some(client.send_typed(&body).map_err(|e| format!("{e:?}"))?);
        if let Some(issue) = request
            .pending
            .as_ref()
            .ok_or("原Pending缺失")?
            .delivery_issue()
        {
            return Err(format!("原Clear已入队但报告issue：{issue:?}"));
        }
        let record = json!({"request_id":id,"request_bytes":body,"started_ns":at,"deadline_ns":deadline,
            "node_configuration":config,"requests_sent":1});
        write_json(&self.case.directory.join("first-clear.json"), &record)?;
        Ok(record)
    }

    #[allow(clippy::too_many_lines)] // 两代原件按验完前代再进入后代的单链持有，不拆散原Pending生命周期。
    async fn restart(&mut self, first: Value, first_until: u64) -> Result<Value, String> {
        let ready = self.ready(&first, first_until).await?;
        self.verify_exec(&first, &ready, first_until)?;
        self.generations(1)?;
        let first_live = files_snapshot(&self.case)?;
        pair(&first_live, true, false)?;
        write_json(
            &self.case.directory.join("persistent-before-clear.json"),
            &first_live,
        )?;
        let request = self.start_clear()?;
        let observed_exit = self
            .exited(&first, 90, 0, SystemClock.now().0 + 4_000_000_000)
            .await?;
        let exited_at_ns = SystemClock.now().0;
        let page = self.page(&first)?;
        validate_file_step(90, &page)?;
        let step = page.supervision.step_entry_ns;
        let expected = step
            .max(page.runtime_armed_ns)
            .checked_add(2_000_000_000)
            .ok_or("原FileStep截止溢出")?;
        if step == 0
            || page.supervision.step_stage != ClearTxnStage::SyncMarkerFile as u64
            || page.fired_deadline_ns != expected
            || page.supervision.pause_failure != 0
        {
            return Err(format!("原FileStep源/armed/max截止不符：{page:?}"));
        }
        // 同代CLD_EXITED已由manager确认；在任何I2开始标识出现之前读取原文件。
        self.generations(1)?;
        let capture_before_ns = SystemClock.now().0;
        let first_files = files_snapshot(&self.case)?;
        pair(&first_files, true, true)?;
        let marker = {
            let mut store = ClearTxnStore::open(&self.case.latch, &self.case.robot_id, "arm")
                .map_err(|e| e.to_string())?;
            let pending = store
                .inspect_pending()
                .map_err(|e| e.to_string())?
                .ok_or("首代合法PendingMarker缺失")?;
            if pending.record().txn_id() == 0 || !pending.record().original_latched() {
                return Err("首代原marker身份/原latch非法".into());
            }
            json!({"txn":pending.record().txn_id().to_string(),"epoch":pending.record().expected_epoch(),
                "latched":pending.record().original_latched(),"dev":pending.identity().dev(),"ino":pending.identity().ino(),
                "directory_dev":store.directory_identity().dev(),"directory_ino":store.directory_identity().ino(),
                "validation":"原inspect_pending完整记录/目标身份/checksum校验；不证明fsync已成功"})
        };
        let second_read = files_snapshot(&self.case)?;
        if first_files != second_read {
            return Err("首代文件在采集间变化".into());
        }
        let capture_after_ns = SystemClock.now().0;
        self.generations(1)?;
        let after_properties = self.show(SystemClock.now().0 + 500_000_000).await?;
        oracle::exited(&after_properties, &first, 90, 0)?;
        self.generations(1)?;
        let first_capture = json!({"before_ns":capture_before_ns,"after_ns":capture_after_ns,
            "manager_before":observed_exit,"manager_after":after_properties,"files":first_files,"marker":marker,
            "i2_absent_before_and_after":true});
        write_json(
            &self.case.directory.join("persistent-first-exit.json"),
            &first_capture,
        )?;
        let pending = self
            .request
            .as_ref()
            .and_then(|request| request.pending.as_ref())
            .ok_or("原Clear Pending丢失")?;
        let reply = receive_with_closed_tail(pending)?;
        if reply.is_some() {
            return Err("首代Clear出现伪退休/终态回复".into());
        }
        let rr_end = json!({"manager_confirmed_exit_at_ns":exited_at_ns,"checked_ns":SystemClock.now().0,
            "reply":reply,"connection":format!("{:?}",pending.connection_state()),"retired_reply":false});
        write_json(
            &self.case.directory.join("first-rr-after-manager-exit.json"),
            &rr_end,
        )?;
        // 原Pending/client/Node只在manager证实首代退出后正常析构；不运行外部清理。
        self.request.take();
        let first_frozen = invocation_digest(&self.invocation_path(&first)?)?;
        write_json(
            &self.case.directory.join("first-invocation-frozen.json"),
            &json!(first_frozen),
        )?;
        let second_until = exited_at_ns + 2_000_000_000 + START_NS;
        let second = self.manifest(2, second_until).await?;
        if second["invocation"] == first["invocation"]
            || second["pid"] == first["pid"]
            || second["test_env"].as_object().is_none_or(|values| {
                values.len() != 2
                    || !values.contains_key("ROBOT_TEST_OBSERVATION_PATH")
                    || !values.contains_key("ROBOT_TEST_TRACE_PATH")
            })
        {
            return Err("自然第二代重复身份/首代故障环境".into());
        }
        let second_ready = self.ready(&second, second_until).await?;
        self.verify_exec(&second, &second_ready, second_until)?;
        if oracle::number(&second_ready, "NRestarts")? != 1 {
            return Err("自然重启计数不是原一次".into());
        }
        let recovered = files_snapshot(&self.case)?;
        pair(&recovered, true, false)?;
        same_storage(&first_capture["files"], &recovered)?;
        if invocation_digest(&self.invocation_path(&first)?)? != first_frozen {
            return Err("I2启动覆盖首代原件".into());
        }
        write_json(
            &self.case.directory.join("persistent-second-ready.json"),
            &recovered,
        )?;
        let stopped = self.stop_job(&second, 1).await?;
        let journal_first = self.journal(&first).await?;
        let journal_second = self.journal(&second).await?;
        let first_stdout =
            std::fs::read_to_string(self.invocation_path(&first)?.join("s0.stdout.log"))
                .map_err(|e| e.to_string())?;
        let second_stdout =
            std::fs::read_to_string(self.invocation_path(&second)?.join("s0.stdout.log"))
                .map_err(|e| e.to_string())?;
        let first_config = node_config(&first_stdout)?;
        let second_config = node_config(&second_stdout)?;
        if first_config != second_config {
            return Err("两代S0实际Node默认清理配置不同".into());
        }
        if !journal_first.contains("writer_pause_stage=SyncMarkerFile duration_ns=4000000000") {
            return Err("首代journal缺原SyncMarkerFile:4000实际暂停记录".into());
        }
        if !initial_latch(&format!("{second_stdout}\n{journal_second}"))? {
            return Err("I2真实initial latch非保守true".into());
        }
        if invocation_digest(&self.invocation_path(&first)?)? != first_frozen {
            return Err("I2完成后首代原件变化".into());
        }
        self.generations(2)?;
        let final_files = files_snapshot(&self.case)?;
        pair(&final_files, true, false)?;
        same_storage(&recovered, &final_files)?;
        write_json(
            &self.case.directory.join("persistent-second-exit.json"),
            &final_files,
        )?;
        Ok(
            json!({"first":first,"first_ready":ready,"clear":request,"first_exit":first_capture,
            "first_rr":rr_end,"first_page":snapshot(&page),"first_frozen":first_frozen,
            "second":second,"second_ready":second_ready,"recovered":recovered,"stop":stopped,
            "first_node_configuration":first_config,"second_node_configuration":second_config,
            "journal_first":journal_first,"journal_second":journal_second,"generations":2,
            "previous_invocation_unchanged":true,"natural_restart_only":true}),
        )
    }

    #[allow(clippy::too_many_lines)] // 同一拥有域按真停止、原RR、bus引用、unit文件顺序收尾，保留每项原错误。
    async fn cleanup(&mut self) -> Result<Value, String> {
        if !self.installed {
            self.stopped = true;
            return Ok(json!({"unit_installed":false}));
        }
        let mut errors = Vec::new();
        let unit = self.unit.clone();
        if self.start_requested {
            let stop = self
                .systemctl(&["stop", &unit], SystemClock.now().0 + STOP_NS)
                .await
                .and_then(command::Output::require_success);
            if let Err(error) = stop {
                errors.push(error);
            }
            let initial = self.show(SystemClock.now().0 + 3_000_000_000).await;
            let quiet = initial
                .as_ref()
                .is_ok_and(|properties| oracle::stopped(properties).is_ok());
            if !quiet {
                let kill = self
                    .systemctl(
                        &["kill", "--signal=SIGKILL", "--kill-whom=all", &unit],
                        SystemClock.now().0 + 3_000_000_000,
                    )
                    .await
                    .and_then(command::Output::require_success);
                errors.push(format!("正常StopJob未证实清空，额外kill收尾={kill:?}"));
                let stopped = self
                    .systemctl(&["stop", &unit], SystemClock.now().0 + STOP_NS)
                    .await
                    .and_then(command::Output::require_success);
                if let Err(error) = stopped {
                    errors.push(error);
                }
            }
            match self
                .show(SystemClock.now().0 + 3_000_000_000)
                .await
                .and_then(|properties| {
                    oracle::stopped(&properties)?;
                    Ok(properties)
                }) {
                Ok(properties) => {
                    self.stopped = true;
                    write_json(
                        &self.case.directory.join("cleanup-stopped.json"),
                        &json!(properties),
                    )?;
                }
                Err(error) => errors.push(error),
            }
        } else {
            self.stopped = true;
        }
        if self.stopped {
            self.request.take();
            if self.case.directory.join("invocations").is_dir()
                && let Err(error) = self.generations(if self.mode == Mode::Restart { 2 } else { 1 })
            {
                errors.push(error);
            }
            let reset = self
                .systemctl(
                    &["reset-failed", &unit],
                    SystemClock.now().0 + 3_000_000_000,
                )
                .await
                .and_then(command::Output::require_success);
            if let Err(error) = reset {
                errors.push(error);
            }
            if let Some(reference) = self.reference.as_mut() {
                let released = reference.release(SystemClock.now().0 + 3_000_000_000);
                if let Err(error) = write_json(
                    &self.case.directory.join("unit-reference-released.json"),
                    &json!({"result":released,"no_pid_or_restart":self.stopped}),
                ) {
                    errors.push(error);
                }
                if let Err(error) = released {
                    errors.push(error);
                }
            }
            self.reference.take();
            if let Err(error) = std::fs::remove_file(&self.unit_path) {
                errors.push(error.to_string());
            }
            let reload = self
                .systemctl(&["daemon-reload"], SystemClock.now().0 + 5_000_000_000)
                .await
                .and_then(command::Output::require_success);
            if let Err(error) = reload {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            Ok(json!({"unit_removed":!self.unit_path.exists(),"no_pid_or_restart":self.stopped}))
        } else {
            Err(format!("原manager清理错误保留：{errors:?}"))
        }
    }
}

fn quote_unit(value: &str) -> Result<String, String> {
    if value.chars().any(char::is_control) {
        return Err("unit参数含控制字符".into());
    }
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    ))
}

fn render_unit(tool: &Path, plan: &Path, root: &Path, mode: Mode) -> Result<String, String> {
    // 此指令读取整行路径；双引号会成为路径本身，不能复用ExecStart的argv引用。
    let working = root.to_str().ok_or("WorkingDirectory不是UTF-8")?;
    if !root.is_absolute() || working.chars().any(char::is_control) || working.trim() != working {
        return Err("WorkingDirectory必须是无控制字符的绝对路径".into());
    }
    let restart = if mode == Mode::Restart {
        "on-failure"
    } else {
        "no"
    };
    Ok(format!(
        "[Unit]\nDescription=R4c 本地隔离 mock 验收\nStartLimitIntervalSec=60s\nStartLimitBurst=3\n\n[Service]\nType=notify\nNotifyAccess=main\nWorkingDirectory={}\nExecStart={} {} {}\nWatchdogSec=2s\nRestart={restart}\nRestartSec=2s\nStandardOutput=journal\nStandardError=journal\nNoNewPrivileges=yes\n",
        working.replace('%', "%%"),
        quote_unit(&tool.to_string_lossy())?.replace('$', "$$"),
        quote_unit(launcher::PREFIX)?,
        quote_unit(&plan.to_string_lossy())?.replace('$', "$$")
    ))
}

fn node_config(stdout: &str) -> Result<Value, String> {
    let records: Vec<_> = stdout
        .split_inclusive('\n')
        .filter(|line| line.starts_with("R4C_NODE_CONFIG_V1 "))
        .collect();
    let [record] = records.as_slice() else {
        return Err("缺唯一原S0 Node配置记录".into());
    };
    if !record.ends_with('\n') {
        return Err("原Node配置记录截断".into());
    }
    let words: Vec<_> = record.split_ascii_whitespace().collect();
    if words.len() != 5
        || words[0] != "R4C_NODE_CONFIG_V1"
        || words[1] != "role=clear_rr"
        || words[4] != "source=NodeBuilder_default_global_config"
    {
        return Err("原Node配置固定字段不符".into());
    }
    let creation = words[2]
        .strip_prefix("cleanup_creation=")
        .ok_or("缺creation")?
        .parse::<bool>()
        .map_err(|e| e.to_string())?;
    let destruction = words[3]
        .strip_prefix("cleanup_destruction=")
        .ok_or("缺destruction")?
        .parse::<bool>()
        .map_err(|e| e.to_string())?;
    Ok(
        json!({"role":"clear_rr","cleanup_creation":creation,"cleanup_destruction":destruction,"source":"NodeBuilder_default_global_config"}),
    )
}

fn file_snapshot(path: &Path) -> Result<Value, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| e.to_string())?;
    let before = file.metadata().map_err(|e| e.to_string())?;
    if !before.is_file() {
        return Err("原持久对象不是普通文件".into());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    let after = file.metadata().map_err(|e| e.to_string())?;
    let path_after = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    let stamp = |metadata: &std::fs::Metadata| {
        (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
        )
    };
    if stamp(&before) != stamp(&after)
        || stamp(&before) != stamp(&path_after)
        || path_after.file_type().is_symlink()
    {
        return Err("原文件读取期间身份/内容变化".into());
    }
    Ok(
        json!({"dev":before.dev(),"ino":before.ino(),"len":before.len(),"bytes":bytes,
        "blake3":blake3::hash(&bytes).to_hex().to_string()}),
    )
}

fn files_snapshot(case: &Case) -> Result<Value, String> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(&case.directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "原持久名称非UTF8")?;
        if name.starts_with("estop_latch") && name != "estop_latch.lock" {
            files.insert(name, file_snapshot(&entry.path())?);
        }
    }
    let mut trust = BTreeMap::new();
    for entry in std::fs::read_dir(&case.trust).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "原trust名称非UTF8")?;
        if name != robot_s0::rx_trust::LOCK_NAME {
            trust.insert(name, file_snapshot(&entry.path())?);
        }
    }
    Ok(
        json!({"directory":launcher::identity(&case.directory)?,"config":file_snapshot(&case.config)?,
        "trust":launcher::identity(&case.trust)?,"trust_files":trust,
        "lock":launcher::identity(&case.directory.join("estop_latch.lock"))?,
        "trust_lock":launcher::identity(&case.trust.join(robot_s0::rx_trust::LOCK_NAME))?,"files":files}),
    )
}

fn pair(snapshot: &Value, latched: bool, marker: bool) -> Result<(), String> {
    let files = snapshot["files"].as_object().ok_or("原文件集合缺失")?;
    let expected = if latched {
        json!([49, 10])
    } else {
        json!([48, 10])
    };
    if files.len() != if marker { 3 } else { 2 }
        || snapshot["files"]["estop_latch"]["bytes"] != expected
        || snapshot["files"]["estop_latch.bak"]["bytes"] != expected
        || marker && snapshot["files"]["estop_latch.clear_pending"]["len"] != 112
        || !marker && files.contains_key("estop_latch.clear_pending")
    {
        return Err(format!("原双文件/marker/临时文件集合不符：{snapshot}"));
    }
    Ok(())
}

fn same_storage(before: &Value, after: &Value) -> Result<(), String> {
    for key in ["directory", "trust", "lock", "trust_lock"] {
        for field in ["dev", "ino"] {
            if before[key][field].is_null() || before[key][field] != after[key][field] {
                return Err(format!("同目录恢复改变{key}.{field}"));
            }
        }
    }
    if before["config"] != after["config"] || before["trust_files"] != after["trust_files"] {
        return Err("恢复改变原config/trust字节身份".into());
    }
    Ok(())
}

fn invocation_digest(directory: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut result = BTreeMap::new();
    for entry in std::fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
            return Err("Invocation原件目录有非普通文件".into());
        }
        result.insert(
            entry
                .file_name()
                .into_string()
                .map_err(|_| "Invocation名称非UTF8")?,
            launcher::digest(&entry.path())?,
        );
    }
    let expected = [
        "manifest.json",
        "s0.observation",
        "s0.stdout.log",
        "s0.trace",
    ];
    if result.len() != expected.len() || expected.iter().any(|name| !result.contains_key(*name)) {
        return Err("Invocation原件文件集合不符".into());
    }
    Ok(result)
}
