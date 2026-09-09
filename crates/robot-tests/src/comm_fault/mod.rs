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
mod sla_background;
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
    MockH2Load,
}

impl Phase {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "mock" => Ok(Self::Mock),
            "startup" => Ok(Self::Startup),
            "rpc" => Ok(Self::Rpc),
            "all" => Ok(Self::All),
            "mock-h2-load" => Ok(Self::MockH2Load),
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
    pub(super) h2_software_cache_entries: Option<usize>,
}

impl Args {
    fn parse() -> Result<Option<Self>, String> {
        Self::parse_from(std::env::args().skip(1))
    }

    fn parse_from(mut raw: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let mut phase = Phase::Mock;
        let mut bin_dir = None;
        let mut evidence_dir = None;
        let mut case = None;
        let mut config = None;
        let mut hardware_id = None;
        let mut supervised = false;
        let mut h2_software_cache_entries = None;
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
                "--h2-software-cache-entries" => {
                    let value = raw.next().ok_or("--h2-software-cache-entries 缺少值")?;
                    h2_software_cache_entries = Some(match value.as_str() {
                        "4096" => 4096,
                        "8192" => 8192,
                        _ => return Err("软件缓存对照仅允许 4096 或 8192".to_owned()),
                    });
                }
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
        if h2_software_cache_entries.is_some() && phase != Phase::MockH2Load {
            return Err("缓存对照参数仅属于 mock-h2-load".to_owned());
        }
        if phase == Phase::MockH2Load && case.is_some() {
            return Err("mock-h2-load 是完整长负载，不接受 --case 截短".to_owned());
        }
        Ok(Some(Self {
            phase,
            bin_dir,
            evidence_dir,
            case,
            config,
            hardware_id,
            h2_software_cache_entries,
        }))
    }
}

pub(super) async fn main_entry() -> ExitCode {
    let args = match Args::parse() {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!(
                "comm_fault_e2e --phase mock|startup|rpc|all|mock-h2-load|orin-characterize|orin-sla|orin-broadcast --bin-dir <绝对路径> --evidence-dir <绝对路径> [--case <AC编号>]\nmock-h2-load 可选 --h2-software-cache-entries 4096|8192，默认8192；至少360s且成功唯一写>8192，最多600s。\n现场另需 --config <绝对路径> --hardware-id <id> --ack-supervised-hardware；退出码：0通过，1验收失败，2参数错误。"
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

#[cfg(test)]
mod argument_tests {
    use super::*;

    fn parse(extra: &[&str]) -> Result<Option<Args>, String> {
        Args::parse_from(
            [
                "--bin-dir",
                "/tmp/s46-bins",
                "--evidence-dir",
                "/tmp/s46-evidence",
            ]
            .into_iter()
            .chain(extra.iter().copied())
            .map(str::to_owned),
        )
    }

    #[test]
    fn long_load_cache_override_is_software_only_and_finite() {
        let args = parse(&["--phase", "mock-h2-load"]).unwrap().unwrap();
        assert_eq!(args.h2_software_cache_entries.unwrap_or(8192), 8192);
        for cap in ["4096", "8192"] {
            assert!(
                parse(&[
                    "--phase",
                    "mock-h2-load",
                    "--h2-software-cache-entries",
                    cap
                ])
                .is_ok()
            );
        }
        for extra in [
            vec![
                "--phase",
                "mock-h2-load",
                "--h2-software-cache-entries",
                "0",
            ],
            vec!["--phase", "all", "--h2-software-cache-entries", "8192"],
            vec!["--phase", "mock-h2-load", "--config", "/tmp/real.toml"],
            vec!["--phase", "mock-h2-load", "--hardware-id", "left_arm"],
            vec!["--phase", "mock-h2-load", "--ack-supervised-hardware"],
            vec!["--phase", "mock-h2-load", "--case", "AC-12"],
        ] {
            assert!(parse(&extra).is_err(), "必须拒绝 {extra:?}");
        }
    }

    #[test]
    fn h2_gateway_only_overrides_its_isolated_capacity() {
        use super::sla::{SlaRunKind, configure_h2_gateway};
        for (kind, expected) in [
            (SlaRunKind::Physical, Some(8192)),
            (SlaRunKind::SoftwareProbe, None),
            (
                SlaRunKind::SoftwareLoad {
                    cache_entries: 4096,
                },
                Some(4096),
            ),
            (
                SlaRunKind::SoftwareLoad {
                    cache_entries: 8192,
                },
                Some(8192),
            ),
        ] {
            let mut command = tokio::process::Command::new("gateway");
            assert_eq!(configure_h2_gateway(&mut command, kind).unwrap(), expected);
            let args: Vec<_> = command
                .as_std()
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect();
            match expected {
                Some(entries) => assert_eq!(
                    args,
                    [
                        "--idempotency-ttl-secs",
                        "300",
                        "--idempotency-max-entries",
                        &entries.to_string()
                    ]
                ),
                None => assert_eq!(args, [] as [&str; 0]),
            }
        }
    }
}
