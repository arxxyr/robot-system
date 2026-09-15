//! 场景分派与真实 marker CLI；每个成功标签必须对应实际运行结果。

use std::time::Duration;

use robot_core::time::{RobotClock, SystemClock};
use robot_s0::boot_supervisor::{RUNTIME_SHUTDOWN_BUDGET, SHUTDOWN_MARGIN};
use robot_s0::shutdown_mechanism::HandleKind;
use serde_json::{Value, json};
use tokio::process::Child;

use super::harness::{Case, isolated_command, stop, wait_exit, wait_log};
use super::report::Report;
use super::{Args, Phase, rpc, startup};

pub(super) const SOFTWARE_LOAD_ACCEPTANCE: &str = "S-46";

pub(super) async fn run(args: &Args, report: &mut Report) -> Result<(), String> {
    super::validate_w4_case(args.phase, args.case.as_deref())?;
    super::validate_r4_case(args.phase, args.case.as_deref())?;
    match args.case.as_deref() {
        Some("W4-FILE-FAULT-SMOKE") => return super::w4_latch_crash::run(args, report).await,
        Some("W4-FILE-SIGKILL-SMOKE") => return super::w4_latch_sigkill::run(args, report).await,
        Some("R4-T8-RESYNC") => return super::w4_latch_resync::run(args, report).await,
        Some("R4-T8-RESYNC-EXHAUSTED") => {
            return super::w4_latch_retry_exhaustion::run(args, report).await;
        }
        Some("R4-T8-WRITER-KILL-25") => {
            return super::w4_latch_sigkill::run_batch(args, report).await;
        }
        Some("R4-T8-RECOVERY-KILL") => {
            return super::w4_latch_sigkill::run_recovery(args, report).await;
        }
        Some("R4C-MANAGER" | "R4C-MANAGER-UNWIND") => {
            return super::manager::run(args, report).await;
        }
        Some("R4C-GATEWAY-ORPHAN") => return super::gateway_exhaustion::run(args, report).await,
        Some("R4C-PERSIST-HEALTH") => return super::w3::run_health(args, report).await,
        Some("R4C-STANDALONE" | "R4C-UNWIND") => {
            return super::runtime_supervision::run(args, report).await;
        }
        Some("R4-SUP-STANDALONE") => return super::supervision::run(args, report).await,
        _ => {}
    }
    match args.phase {
        Phase::Mock => mock(args, report).await,
        Phase::Startup => {
            startup::run(args, report).await?;
            super::watchdog::run(args, report).await?;
            super::w3::run(args, report).await
        }
        Phase::Rpc => rpc::run(args, report).await,
        Phase::All => {
            mock(args, report).await?;
            startup::run(args, report).await?;
            super::watchdog::run(args, report).await?;
            rpc::run(args, report).await?;
            super::w3::run(args, report).await
        }
        Phase::MockH2Load => {
            let started = SystemClock.now().0;
            report.record(
                "mock_h2_load",
                SOFTWARE_LOAD_ACCEPTANCE,
                started,
                super::sla::software_load(args).await,
            );
            Ok(())
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

#[allow(
    clippy::too_many_lines,
    reason = "同一场景顺序保存 marker CLI、一次性授权和该子进程的完整收尾结果"
)]
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
        let startup_deadline_ns = SystemClock.now().0.saturating_add(5_000_000_000);
        let result = async {
            wait_log(
                &case,
                &mut child,
                name,
                "boot_ready",
                Duration::from_secs(5),
            )
            .await?;
            if expect_fault {
                wait_runtime_armed(&case, &mut child, name, startup_deadline_ns).await?;
            }
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
        let cleanup = if expect_fault {
            stop_dirty_runtime(&case, &mut child, name).await
        } else {
            stop(&mut child).await.map(|()| json!({"exit_code": 0}))
        };
        match (result, cleanup) {
            (Ok(mut evidence), Ok(cleanup)) => {
                evidence["cleanup"] = cleanup;
                operations.push(evidence);
            }
            (result, cleanup) => return Err(format!("{name}场景={result:?}；收尾={cleanup:?}")),
        }
    }
    Ok(json!({"operations": operations, "log_directory": case.directory}))
}

pub(super) async fn wait_runtime_armed(
    case: &Case,
    child: &mut Child,
    name: &str,
    startup_deadline_ns: u64,
) -> Result<(), String> {
    loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Err(format!(
                "等待真实 RuntimeArmed 时该子进程提前退出：{status}"
            ));
        }
        if SystemClock.now().0 >= startup_deadline_ns {
            return Err("原 5 秒启动等待包络内未发布 RuntimeArmed".to_owned());
        }
        let observation = case.observation(name)?;
        if observation.runtime_armed_ns != 0 && observation.ready_reached {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 测试中真实尚存的责任角色；不是可任选的残件掩码。
#[derive(Clone, Copy)]
pub(super) enum UnretiredRuntimeCase {
    DirtyWithoutRr,
    SetRrHeld,
}

pub(super) async fn stop_unretired_runtime(
    case: &Case,
    child: &mut Child,
    name: &str,
    responsibility: UnretiredRuntimeCase,
) -> Result<Value, String> {
    let before = case.observation(name)?;
    if before.runtime_armed_ns == 0 || !before.ready_reached {
        return Err(format!(
            "未确认真实 RuntimeArmed，不发送运行期停止信号：{before:?}"
        ));
    }
    if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
        return Err(format!("发送停止信号前该子进程已退出：{status}"));
    }
    let pid = child.id().ok_or("发送停止信号前该子进程已无 id")?;
    let signal_pid = i32::try_from(pid).map_err(|error| error.to_string())?;
    let signal_at_ns = SystemClock.now().0;
    // SAFETY：仅对仍由此 Child 持有、未回收且可表示为 pid_t 的当前 PID 请求 SIGTERM。
    let signal_result = unsafe { libc::kill(signal_pid, libc::SIGTERM) };
    let signal_error = (signal_result != 0).then(std::io::Error::last_os_error);
    let wait_result = wait_exit(
        child,
        Duration::from_nanos(RUNTIME_SHUTDOWN_BUDGET.0 + SHUTDOWN_MARGIN.0),
    )
    .await;
    let reaped_ns = SystemClock.now().0;
    if signal_result != 0 {
        return Err(format!(
            "未退责 SIGTERM 未成功：pid={pid} result={signal_result} error={signal_error:?}；原等待={wait_result:?}"
        ));
    }
    let code = wait_result.map_err(|error| {
        format!("未退责 SIGTERM 已发送：pid={pid} result={signal_result}；原等待失败={error}")
    })?;
    let observation = case.observation(name)?;
    let elapsed_ns = reaped_ns
        .checked_sub(signal_at_ns)
        .ok_or("真实回收早于信号起点")?;
    // 信号前到真 wait 的父观测仅是外包络，绝不冒充 daemon 原 shutdown ticket 的 D。
    let earliest_ns = RUNTIME_SHUTDOWN_BUDGET.0 - 250_000_000;
    let latest_ns = RUNTIME_SHUTDOWN_BUDGET.0 + 100_000_000;
    let protected = HandleKind::Executor.bit()
        | HandleKind::Ipc.bit()
        | HandleKind::Watchdog.bit()
        | HandleKind::LatchWriter.bit()
        | match responsibility {
            UnretiredRuntimeCase::DirtyWithoutRr => 0,
            UnretiredRuntimeCase::SetRrHeld => HandleKind::SetRr.bit(),
        };
    if code != 1
        || !(earliest_ns..=latest_ns).contains(&elapsed_ns)
        || observation.fired_ns != 0
        || observation.fired_reason != 0
        || observation.fired_checkpoint != 0
        || observation.supervisor_exit_ns != 0
        || observation.stop_bitmap != 0x66
        || observation.residual_bitmap != u64::from(protected)
    {
        return Err(format!(
            "运行期持责退出不符：pid={pid} signal_result={signal_result} code={code} elapsed_ns={elapsed_ns}；{observation:?}"
        ));
    }
    Ok(json!({
        "pid": pid, "signal": "SIGTERM", "signal_result": signal_result,
        "signal_at_ns": signal_at_ns, "reaped_ns": reaped_ns, "elapsed_ns": elapsed_ns,
        "exit_code": code, "observation": format!("{observation:?}"),
        "time_evidence": "父进程信号前至真回收的外包络，不是原 D 的直接观测"
    }))
}

async fn stop_dirty_runtime(case: &Case, child: &mut Child, name: &str) -> Result<Value, String> {
    let receipt =
        stop_unretired_runtime(case, child, name, UnretiredRuntimeCase::DirtyWithoutRr).await?;
    for marker in ["rx_trust_marker", "rx_trust_marker.bak"] {
        if std::fs::read(case.trust.join(marker)).map_err(|error| error.to_string())? != b"1\n" {
            return Err(format!("dirty 收尾改变原一次性 marker：{marker}"));
        }
    }
    let backup_latch = case.directory.join("estop_latch.bak");
    for latch in [&case.latch, &backup_latch] {
        if std::fs::read(latch).map_err(|error| error.to_string())? != b"0\n" {
            return Err(format!("dirty 收尾改变独立急停 latch：{}", latch.display()));
        }
    }
    Ok(receipt)
}
