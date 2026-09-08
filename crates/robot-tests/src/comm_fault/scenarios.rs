//! 场景分派与真实 marker CLI；每个成功标签必须对应实际运行结果。

use std::time::Duration;

use robot_core::time::{RobotClock, SystemClock};
use serde_json::{Value, json};

use super::harness::{Case, isolated_command, stop, wait_exit, wait_log};
use super::report::Report;
use super::{Args, Phase, rpc, startup};

pub(super) async fn run(args: &Args, report: &mut Report) -> Result<(), String> {
    match args.phase {
        Phase::Mock => mock(args, report).await,
        Phase::Startup => {
            startup::run(args, report).await?;
            super::watchdog::run(args, report).await
        }
        Phase::Rpc => rpc::run(args, report).await,
        Phase::All => {
            mock(args, report).await?;
            startup::run(args, report).await?;
            super::watchdog::run(args, report).await?;
            rpc::run(args, report).await
        }
        Phase::OrinSla => {
            let started = SystemClock.now().0;
            report.record("orin_sla", "AC-12/H2", started, super::sla::run(args).await);
            Ok(())
        }
        Phase::OrinCharacterize | Phase::OrinBroadcast => super::hardware::run(args, report),
    }
}

async fn mock(args: &Args, report: &mut Report) -> Result<(), String> {
    if args
        .case
        .as_deref()
        .is_none_or(|filter| matches!(filter, "AC-12" | "AC-13"))
    {
        let started = SystemClock.now().0;
        let result = match super::hardware::software_protocol_probe() {
            Ok(wire_check) => super::sla::software_probe(args).await.map(|mut evidence| {
                evidence["feetech_wire_check"] = wire_check;
                evidence
            }),
            Err(error) => Err(error),
        };
        report.record(
            "runtime_estop_sampling_and_recovery",
            "AC-12/13",
            started,
            result,
        );
    }
    if args.case.as_deref().is_none_or(|filter| filter == "AC-21") {
        let started = SystemClock.now().0;
        report.record(
            "marker_cli_truth_and_one_shot",
            "AC-21",
            started,
            marker_cli(args).await,
        );
    }
    Ok(())
}

async fn marker_cli(args: &Args) -> Result<Value, String> {
    let case = Case::new(&args.evidence_dir, &args.bin_dir, "marker_cli", 6)?;
    let mut operations = Vec::new();
    for (index, operation, expected, pair) in [
        (0, "authorize", 2, None),
        (1, "revoke", 0, Some(b"1\n")),
        (2, "status", 0, Some(b"1\n")),
        (3, "authorize", 0, Some(b"0\n")),
        (4, "authorize", 2, Some(b"0\n")),
        (5, "revoke", 0, Some(b"1\n")),
    ] {
        let mut command = isolated_command(&args.bin_dir.join("s0_daemon"));
        command
            .args([
                "rx-trust",
                operation,
                "--robot-id",
                &case.robot_id,
                "--rx-trust-dir",
            ])
            .arg(&case.trust)
            .arg("--latch-path")
            .arg(&case.latch);
        let mut child = case.spawn(&format!("{index}_{operation}"), command)?;
        let code = wait_exit(&mut child, Duration::from_secs(2)).await?;
        if code != expected {
            return Err(format!("{operation}预期{expected}实际{code}"));
        }
        if let Some(expected) = pair {
            for name in ["rx_trust_marker", "rx_trust_marker.bak"] {
                let actual =
                    std::fs::read(case.trust.join(name)).map_err(|error| error.to_string())?;
                if actual != expected {
                    return Err(format!("{operation}后{name}={actual:?}"));
                }
            }
        }
        operations.push(json!({"operation": operation, "exit_code": code}));
    }
    let mut authorize = isolated_command(&args.bin_dir.join("s0_daemon"));
    authorize
        .args([
            "rx-trust",
            "authorize",
            "--robot-id",
            &case.robot_id,
            "--rx-trust-dir",
        ])
        .arg(&case.trust)
        .arg("--latch-path")
        .arg(&case.latch);
    let mut child = case.spawn("authorize_for_consume", authorize)?;
    if wait_exit(&mut child, Duration::from_secs(2)).await? != 0 {
        return Err("为真实一次性消费准备授权失败".to_owned());
    }
    for (name, expect_fault) in [("consume_authorized", false), ("restart_dirty", true)] {
        let mut command = case.command();
        command.args(["--rx-trust-policy", "marker"]);
        let mut child = case.spawn(name, command)?;
        let result = async {
            wait_log(
                &case,
                &mut child,
                name,
                "boot_ready",
                Duration::from_secs(5),
            )
            .await?;
            let observation = case.observation(name)?;
            if !observation.observation_ready
                || !observation.ready_reached
                || observation.preflight_finished_ns == 0
                || (observation.first_exit_ns != 0) != expect_fault
            {
                return Err(format!("一次性授权启动出口不符：{name} {observation:?}"));
            }
            for marker in ["rx_trust_marker", "rx_trust_marker.bak"] {
                if std::fs::read(case.trust.join(marker)).map_err(|e| e.to_string())? != b"1\n" {
                    return Err(format!("{name}未持久消费{marker}"));
                }
            }
            if std::fs::read(&case.latch).map_err(|e| e.to_string())? != b"0\n" {
                return Err(format!("{name}意外修改独立急停latch"));
            }
            Ok(json!({"operation": name, "observation": format!("{observation:?}")}))
        }
        .await;
        let cleanup = stop(&mut child).await;
        match (result, cleanup) {
            (Ok(evidence), Ok(())) => operations.push(evidence),
            (result, cleanup) => return Err(format!("{name}场景={result:?}；收尾={cleanup:?}")),
        }
    }
    Ok(json!({"operations": operations, "log_directory": case.directory}))
}
