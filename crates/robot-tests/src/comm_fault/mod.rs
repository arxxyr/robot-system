//! post-E 独立验收：软件场景与现场测量入口共用明确的参数和证据格式。

mod control;
mod hardware;
mod harness;
mod heartbeat;
mod report;
mod rpc;
mod scenarios;
mod sla;
#[cfg(test)]
mod sla_acceptance;
mod startup;
mod watchdog;

use robot_core::time::{RobotClock, SystemClock};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Mock,
    Startup,
    Rpc,
    All,
    OrinCharacterize,
    OrinSla,
    OrinBroadcast,
}

impl Phase {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "mock" => Ok(Self::Mock),
            "startup" => Ok(Self::Startup),
            "rpc" => Ok(Self::Rpc),
            "all" => Ok(Self::All),
            "orin-characterize" => Ok(Self::OrinCharacterize),
            "orin-sla" => Ok(Self::OrinSla),
            "orin-broadcast" => Ok(Self::OrinBroadcast),
            _ => Err(format!("未知 phase：{value}")),
        }
    }
    const fn hardware(self) -> bool {
        matches!(
            self,
            Self::OrinCharacterize | Self::OrinSla | Self::OrinBroadcast
        )
    }
}

pub(super) struct Args {
    pub(super) phase: Phase,
    pub(super) bin_dir: PathBuf,
    pub(super) evidence_dir: PathBuf,
    pub(super) case: Option<String>,
    pub(super) config: Option<PathBuf>,
    pub(super) hardware_id: Option<String>,
}

impl Args {
    fn parse() -> Result<Option<Self>, String> {
        let mut phase = Phase::Mock;
        let mut bin_dir = None;
        let mut evidence_dir = None;
        let mut case = None;
        let mut config = None;
        let mut hardware_id = None;
        let mut supervised = false;
        let mut raw = std::env::args().skip(1);
        while let Some(key) = raw.next() {
            match key.as_str() {
                "-h" | "--help" => return Ok(None),
                "--ack-supervised-hardware" => supervised = true,
                "--phase" => phase = Phase::parse(&raw.next().ok_or("--phase 缺少值")?)?,
                "--bin-dir" => bin_dir = Some(PathBuf::from(raw.next().ok_or("--bin-dir 缺少值")?)),
                "--evidence-dir" => {
                    evidence_dir = Some(PathBuf::from(raw.next().ok_or("--evidence-dir 缺少值")?));
                }
                "--case" => case = Some(raw.next().ok_or("--case 缺少值")?),
                "--config" => config = Some(PathBuf::from(raw.next().ok_or("--config 缺少值")?)),
                "--hardware-id" => hardware_id = Some(raw.next().ok_or("--hardware-id 缺少值")?),
                _ => return Err(format!("未知参数：{key}")),
            }
        }
        let bin_dir = bin_dir.ok_or("必须显式指定 --bin-dir")?;
        let evidence_dir = evidence_dir.ok_or("必须显式指定 --evidence-dir")?;
        if !bin_dir.is_absolute() || !evidence_dir.is_absolute() {
            return Err("二进制及证据目录必须是绝对路径".to_owned());
        }
        if phase.hardware() {
            if !cfg!(feature = "real-hw")
                || !supervised
                || config.is_none()
                || hardware_id.as_deref().is_none_or(str::is_empty)
            {
                return Err("现场 phase 需要 real-hw 构建、--config、--hardware-id 和 --ack-supervised-hardware".to_owned());
            }
            if !config
                .as_ref()
                .is_some_and(|path| path.is_absolute() && path.is_file())
            {
                return Err("现场配置必须是存在的绝对路径".to_owned());
            }
        } else if supervised || config.is_some() || hardware_id.is_some() {
            return Err("软件 phase 不接受现场参数".to_owned());
        }
        Ok(Some(Self {
            phase,
            bin_dir,
            evidence_dir,
            case,
            config,
            hardware_id,
        }))
    }
}

pub(super) async fn main_entry() -> ExitCode {
    let args = match Args::parse() {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!(
                "comm_fault_e2e --phase mock|startup|rpc|all|orin-characterize|orin-sla|orin-broadcast --bin-dir <绝对路径> --evidence-dir <绝对路径> [--case <AC编号>]\n现场另需 --config <绝对路径> --hardware-id <id> --ack-supervised-hardware；退出码：0通过，1验收失败，2参数错误。"
            );
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("参数错误：{error}");
            return ExitCode::from(2);
        }
    };
    let mut report = match report::Report::new(&args) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("验收前置失败：{error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = scenarios::run(&args, &mut report).await {
        eprintln!("验收调度失败：{error}");
        report.record("fatal_dispatch", "AC-Q", SystemClock.now().0, Err(error));
        let _ = report.finish();
        return ExitCode::FAILURE;
    }
    match report.finish() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("证据写入失败：{error}");
            ExitCode::FAILURE
        }
    }
}
