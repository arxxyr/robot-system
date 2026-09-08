//! 真实启动监督与锁验收；暂停业务阶段，不暂停整个进程。

use std::path::Path;
use std::time::Duration;

use robot_core::time::{RobotClock, SystemClock};
use serde_json::{Value, json};
use tokio::net::UnixDatagram;

use super::Args;
use super::control::{ControlPort, wait_applied};
use super::harness::{Case, isolated_command, stop, wait_exit, wait_log};
use super::report::Report;
use robot_s0::comm_fault_hook::{HookCommandKind, TestCommFaultCommand};
use robot_s0::worker_stop::WorkerKind;

pub(super) async fn run(args: &Args, report: &mut Report) -> Result<(), String> {
    if selected(args, "AC-22") {
        for stage in [
            "before_logging",
            "before_parse",
            "before_resolve",
            "adapter_open",
            "marker_write",
            "ipc_ready",
        ] {
            let started = SystemClock.now().0;
            report.record(
                &format!("cp1_{stage}"),
                "AC-22",
                started,
                blocked_stage(args, stage, 5_000).await,
            );
        }
        for stage in ["after_spawn", "before_ready"] {
            let started = SystemClock.now().0;
            // N=6 的 T_abs=10.7s，CP2=T_abs+1s。
            report.record(
                &format!("cp2_{stage}"),
                "AC-22",
                started,
                blocked_stage(args, stage, 11_700).await,
            );
        }
        for worker in [
            "ipc", "clear_rr", "set_rr", "watchdog", "executor", "pub", "io",
        ] {
            let started = SystemClock.now().0;
            report.record(
                &format!("spawn_failure_{worker}"),
                "AC-22",
                started,
                failed_spawn(args, worker).await,
            );
        }
        for joints in [6, 13, 32] {
            let started = SystemClock.now().0;
            report.record(
                &format!("normal_boot_{joints}"),
                "AC-22",
                started,
                normal_boot(args, joints).await,
            );
        }
        for worker in [WorkerKind::Ipc, WorkerKind::Executor] {
            let started = SystemClock.now().0;
            report.record(
                &format!("boot_frozen_{}", worker.name()),
                "AC-22",
                started,
                frozen_worker(args, worker).await,
            );
        }
        for worker in WorkerKind::ALL {
            let started = SystemClock.now().0;
            report.record(
                &format!("boot_refuse_stop_{}", worker.name()),
                "AC-22",
                started,
                refused_stop(args, worker).await,
            );
        }
        for cli_only in [false, true] {
            let started = SystemClock.now().0;
            report.record(
                &format!("joint_limit_{cli_only}"),
                "AC-22",
                started,
                too_many_joints(args, cli_only).await,
            );
        }
        for mode in ["auto", "required", "standalone"] {
            for socket in ["none", "empty", "missing", "abstract", "path"] {
                for invocation in [false, true] {
                    let started = SystemClock.now().0;
                    report.record(
                        &format!("notify_{mode}_{socket}_{invocation}"),
                        "AC-22",
                        started,
                        notify_case(args, mode, socket, invocation).await,
                    );
                }
            }
        }
    }
    if selected(args, "AC-24") {
        let started = SystemClock.now().0;
        report.record(
            "trust_lock_process_and_sigkill",
            "AC-24",
            started,
            lock_process(args).await,
        );
    }
    Ok(())
}

fn selected(args: &Args, ac: &str) -> bool {
    args.case.as_deref().is_none_or(|filter| filter == ac)
}

fn marker_pair(directory: &Path) -> Result<(Vec<u8>, Vec<u8>), String> {
    let primary =
        std::fs::read(directory.join("rx_trust_marker")).map_err(|error| error.to_string())?;
    let backup =
        std::fs::read(directory.join("rx_trust_marker.bak")).map_err(|error| error.to_string())?;
    Ok((primary, backup))
}

fn seed_marker(case: &Case) -> Result<(), String> {
    for name in ["rx_trust_marker", "rx_trust_marker.bak"] {
        std::fs::write(case.trust.join(name), b"0\n").map_err(|error| error.to_string())?;
    }
    Ok(())
}

async fn blocked_stage(args: &Args, stage: &str, deadline_ms: u64) -> Result<Value, String> {
    let case = Case::new(&args.evidence_dir, &args.bin_dir, stage, 6)?;
    seed_marker(&case)?;
    let mut command = case.command();
    command.env("ROBOT_TEST_BOOT_PAUSE_STAGE", stage);
    if stage == "marker_write" {
        command.args(["--rx-trust-policy", "marker"]);
    }
    let started = SystemClock.now().0;
    let mut child = case.spawn("s0", command)?;
    wait_log(
        &case,
        &mut child,
        "s0",
        &format!("comm_fault_hook stage={stage}"),
        Duration::from_secs(4),
    )
    .await?;
    let code = wait_exit(&mut child, Duration::from_millis(deadline_ms + 500)).await?;
    let reaped_ns = SystemClock.now().0;
    let parent_elapsed_ns = reaped_ns.saturating_sub(started);
    let observation = case.observation("s0")?;
    let elapsed_ns = observation
        .fired_ns
        .checked_sub(observation.main_start_ns)
        .ok_or("监督器 fired 时间早于 main 起点")?;
    let checkpoint_ns = deadline_ms * 1_000_000;
    let expected_checkpoint = if deadline_ms == 5_000 { 1 } else { 2 };
    if code != 88
        || observation.fired_checkpoint != expected_checkpoint
        || elapsed_ns < checkpoint_ns
        || elapsed_ns > checkpoint_ns + 100_000_000
    {
        return Err(format!(
            "监督退出不符：stage={stage} code={code} elapsed_ns={elapsed_ns} checkpoint_ns={checkpoint_ns} observation={observation:?}"
        ));
    }
    let actual_exit_upper_ns =
        verify_exit_deadline(&observation, reaped_ns, checkpoint_ns + 100_000_000)?;
    let ctor_envelope_ns = verify_initialization(&observation)?;
    let log = case.logs("s0")?;
    if log.contains("boot_ready") {
        return Err("阻塞场景不应提前READY".to_owned());
    }
    if matches!(
        stage,
        "before_logging" | "before_parse" | "before_resolve" | "adapter_open"
    ) && marker_pair(&case.trust)? != (b"0\n".to_vec(), b"0\n".to_vec())
    {
        return Err("marker消费前阻塞却改变了授权".to_owned());
    }
    Ok(
        json!({"exit_code": code, "supervisor_elapsed_ns": elapsed_ns,
            "parent_elapsed_ns": parent_elapsed_ns, "actual_exit_upper_ns": actual_exit_upper_ns, "initialization_envelope_ns": ctor_envelope_ns,
            "checkpoint_ns": checkpoint_ns, "stage": stage, "observation": format!("{observation:?}"),
            "log_directory": case.directory}),
    )
}

async fn failed_spawn(args: &Args, worker: &str) -> Result<Value, String> {
    let case = Case::new(
        &args.evidence_dir,
        &args.bin_dir,
        &format!("fail_{worker}"),
        6,
    )?;
    let mut command = case.command();
    command.env("ROBOT_TEST_BOOT_FAIL_WORKER", worker);
    let mut child = case.spawn("s0", command)?;
    let code = wait_exit(&mut child, Duration::from_millis(5_200)).await?;
    let reaped_ns = SystemClock.now().0;
    let observation = case.observation("s0")?;
    verify_initialization(&observation)?;
    if !case
        .logs("s0")?
        .contains(&format!("comm_fault_hook fail_worker={worker}"))
    {
        return Err(format!("指定worker={worker}的故障注入未执行"));
    }
    if !matches!(code, 1 | 88) {
        return Err(format!("worker={worker} spawn失败返回{code}"));
    }
    verify_exit_deadline(
        &observation,
        reaped_ns,
        if code == 88 {
            5_100_000_000
        } else {
            5_000_000_000
        },
    )?;
    if code == 1 && observation.disabled_ns == 0 {
        return Err(format!("spawn失败协作退出未实际Disabled：{observation:?}"));
    }
    if code == 88 {
        let elapsed_ns = observation
            .fired_ns
            .saturating_sub(observation.main_start_ns);
        if observation.fired_checkpoint != 1
            || !(5_000_000_000..=5_100_000_000).contains(&elapsed_ns)
        {
            return Err(format!("spawn失败的监督退88不在CP1窗口：{observation:?}"));
        }
    }
    if case.logs("s0")?.contains("boot_ready") {
        return Err("worker未全部spawn却READY".to_owned());
    }
    if observation.spawned_ns != 0 || observation.first_io_ns != 0 || observation.ready_reached {
        return Err(format!("七worker未齐却通过首IO/READY门：{observation:?}"));
    }
    if observation.stop_bitmap != observation.worker_bitmap {
        return Err(format!("未向已spawn worker逐一请求停止：{observation:?}"));
    }
    let index = [
        "ipc", "clear_rr", "set_rr", "watchdog", "executor", "pub", "io",
    ]
    .iter()
    .position(|name| *name == worker)
    .ok_or("未知worker")?;
    if observation.worker_bitmap != (1_u64 << index) - 1 {
        return Err(format!("spawn失败成员之前的成功前缀错误：{observation:?}"));
    }
    Ok(
        json!({"worker": worker, "exit_code": code, "observation": format!("{observation:?}"), "log_directory": case.directory}),
    )
}

async fn normal_boot(args: &Args, joints: usize) -> Result<Value, String> {
    let case = Case::new(
        &args.evidence_dir,
        &args.bin_dir,
        &format!("normal_{joints}"),
        joints,
    )?;
    let mut child = case.spawn("s0", case.command())?;
    wait_log(
        &case,
        &mut child,
        "s0",
        "boot_ready",
        Duration::from_millis(10_500 + 200 * joints as u64),
    )
    .await?;
    // 真实 supervisor 退出是证据；仅存活超过CP1不能证明CP2已解除。
    let until = SystemClock.now().0 + 1_000_000_000;
    let observation = loop {
        let observation = case.observation("s0")?;
        if observation.supervisor_exit_ns != 0 {
            break observation;
        }
        if SystemClock.now().0 > until {
            return Err("READY后监督线程未退出".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let ctor_envelope_ns = verify_initialization(&observation)?;
    if observation.joint_count != joints as u64
        || observation.worker_bitmap != 0x7f
        || observation.spawned_ns == 0
        || observation.first_io_ns < observation.spawned_ns
        || observation.preflight_finished_ns < observation.first_io_ns
        || observation.ready_ns < observation.preflight_finished_ns
        || !observation.ready_reached
        || observation.supervisor_exit_ns < observation.ready_ns
        || observation.fired_checkpoint != 0
        || observation.first_exit_ns != 0
    {
        return Err(format!("正常启动时间线或数量不符：{observation:?}"));
    }
    if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
        return Err(format!("正常READY后退出：{status}"));
    }
    stop(&mut child).await?;
    Ok(
        json!({"joints": joints, "initialization_envelope_ns": ctor_envelope_ns,
        "observation": format!("{observation:?}"), "log_directory": case.directory}),
    )
}

async fn too_many_joints(args: &Args, cli_only: bool) -> Result<Value, String> {
    let case = Case::new(&args.evidence_dir, &args.bin_dir, "joint_limit", 33)?;
    let command = if cli_only {
        let mut command = isolated_command(&args.bin_dir.join("s0_daemon"));
        command
            .args(["--robot-id", &case.robot_id, "--joints", "33"])
            .arg("--latch-path")
            .arg(&case.latch)
            .kill_on_drop(true);
        command
    } else {
        case.command()
    };
    let mut child = case.spawn("s0", command)?;
    let code = wait_exit(&mut child, Duration::from_secs(2)).await?;
    if code != 2 {
        return Err(format!("33关节应配置拒绝2，实际{code}"));
    }
    let observation = case.observation("s0")?;
    verify_initialization(&observation)?;
    if observation.joint_count != 32 {
        return Err(format!("33关节错误解析提前覆盖N_ATOMIC：{observation:?}"));
    }
    if observation.first_io_ns != 0 || observation.worker_bitmap != 0 {
        return Err(format!("超上限未在spawn/首IO前拒绝：{observation:?}"));
    }
    Ok(json!({"cli_only": cli_only, "exit_code": code, "log_directory": case.directory}))
}

async fn notify_case(
    args: &Args,
    mode: &str,
    socket: &str,
    invocation: bool,
) -> Result<Value, String> {
    let case = Case::new(&args.evidence_dir, &args.bin_dir, "notify", 6)?;
    let mut command = case.command();
    command.args(["--notify-mode", mode]);
    if invocation {
        command.env("INVOCATION_ID", "comm-fault-test");
    }
    let socket_path = std::env::temp_dir().join(format!("{}_notify.sock", case.robot_id));
    let receiver = match socket {
        "none" => None,
        "empty" => {
            command.env("NOTIFY_SOCKET", "");
            None
        }
        "missing" => {
            command.env("NOTIFY_SOCKET", &socket_path);
            None
        }
        "abstract" => {
            command.env("NOTIFY_SOCKET", "@comm-fault-no-abstract");
            None
        }
        "path" => {
            command.env("NOTIFY_SOCKET", &socket_path);
            Some(UnixDatagram::bind(&socket_path).map_err(|error| error.to_string())?)
        }
        _ => return Err("未知socket测试参数".to_owned()),
    };
    let configured_error = matches!(socket, "missing" | "abstract");
    let required = mode == "required" || (mode == "auto" && invocation);
    let ready = !configured_error && (socket == "path" || !required);
    let mut child = case.spawn("s0", command)?;
    if ready {
        if let Some(receiver) = receiver {
            let mut buffer = [0_u8; 4096];
            let count = tokio::time::timeout(Duration::from_secs(4), receiver.recv(&mut buffer))
                .await
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())?;
            if !String::from_utf8_lossy(&buffer[..count])
                .split('\n')
                .any(|line| line == "READY=1")
            {
                return Err("path socket未收到READY=1".to_owned());
            }
        }
        wait_log(
            &case,
            &mut child,
            "s0",
            "boot_ready",
            Duration::from_secs(4),
        )
        .await?;
        if !case.observation("s0")?.ready_reached {
            return Err("READY日志出现但权威提交观测未发布".to_owned());
        }
        stop(&mut child).await?;
    } else {
        let code = wait_exit(&mut child, Duration::from_secs(5)).await?;
        if code != 1 || case.logs("s0")?.contains("boot_ready") {
            return Err(format!("READY失败应有界退1且未置位，实际{code}"));
        }
        let observation = case.observation("s0")?;
        verify_initialization(&observation)?;
        if observation.ready_reached {
            return Err("READY失败却发布ready_reached".to_owned());
        }
    }
    if socket == "path" {
        std::fs::remove_file(&socket_path).map_err(|error| error.to_string())?;
    }
    Ok(
        json!({"mode": mode, "socket": socket, "invocation": invocation, "expected_ready": ready, "log_directory": case.directory}),
    )
}

// 父进程wait返回必晚于真实退出，作为保守上界；调度延迟只会导致拒收，不能放宽截止。
fn verify_exit_deadline(
    observation: &robot_s0::comm_fault_observation::ObservationSnapshot,
    reaped_ns: u64,
    bound_ns: u64,
) -> Result<u64, String> {
    let elapsed = reaped_ns
        .checked_sub(observation.main_start_ns)
        .ok_or("退出时刻早于main")?;
    if observation.main_start_ns == 0 || elapsed > bound_ns {
        return Err(format!(
            "实际退出的保守上界超过截止：elapsed={elapsed} bound={bound_ns}；fired不能替代退出"
        ));
    }
    Ok(elapsed)
}

fn verify_initialization(
    observation: &robot_s0::comm_fault_observation::ObservationSnapshot,
) -> Result<u64, String> {
    if !observation.observation_ready
        || observation.preinit_ns == 0
        || observation.main_start_ns == 0
    {
        return Err(format!("初始化观测缺失：{observation:?}"));
    }
    let elapsed = observation
        .main_start_ns
        .checked_sub(observation.preinit_ns)
        .ok_or("ELF初始化边界晚于main")?;
    if elapsed > 10_000_000 {
        return Err(format!(
            "ELF初始化边界到main的保守包络超过10ms：{elapsed}ns"
        ));
    }
    Ok(elapsed)
}

fn worker_command(kind: HookCommandKind, worker: WorkerKind) -> TestCommFaultCommand {
    TestCommFaultCommand {
        sequence: 1,
        duration_ms: 20_000,
        count: 0,
        kind: kind as u8,
        target: worker as u8,
        error: 0,
        reserved: [0; 5],
    }
}

async fn frozen_worker(args: &Args, worker: WorkerKind) -> Result<Value, String> {
    let case = Case::new(
        &args.evidence_dir,
        &args.bin_dir,
        &format!("frozen_{}", worker.name()),
        6,
    )?;
    let port = ControlPort::new(&case.robot_id)?;
    port.send(worker_command(HookCommandKind::PauseWorker, worker))?;
    let mut command = case.command();
    // Dirty startup要求真实IPC提交Fault并ACK，不能用Passed绕过冻结IPC。
    if worker == WorkerKind::Ipc {
        command.args(["--rx-trust-policy", "marker"]);
    }
    command.env("ROBOT_TEST_INITIAL_COMMAND_SEQUENCE", "1");
    let mut child = case.spawn("s0", command)?;
    wait_applied(&case, &mut child, 1).await?;
    let code = wait_exit(&mut child, Duration::from_secs(12)).await?;
    let reaped_ns = SystemClock.now().0;
    let observation = case.observation("s0")?;
    verify_initialization(&observation)?;
    verify_exit_deadline(
        &observation,
        reaped_ns,
        if code == 88 {
            11_800_000_000
        } else {
            11_700_000_000
        },
    )?;
    if !matches!(code, 1 | 88)
        || observation.ready_reached
        || observation.worker_bitmap != 0x7f
        || observation.spawned_ns == 0
    {
        return Err(format!(
            "冻结worker启动退场不符：code={code} {observation:?}"
        ));
    }
    if code == 88 {
        let elapsed = observation
            .fired_ns
            .saturating_sub(observation.main_start_ns);
        if observation.fired_checkpoint != 2
            || !(11_700_000_000..=11_800_000_000).contains(&elapsed)
        {
            return Err(format!("冻结worker未在CP2窗执法：{observation:?}"));
        }
    } else if observation.stop_bitmap != 0x7f || observation.disabled_ns == 0 {
        return Err(format!(
            "协作退出未Disabled或未停止全部worker：{observation:?}"
        ));
    }
    Ok(
        json!({"worker": worker.name(), "exit_code": code, "observation": format!("{observation:?}"), "log_directory": case.directory}),
    )
}

async fn refused_stop(args: &Args, worker: WorkerKind) -> Result<Value, String> {
    let case = Case::new(
        &args.evidence_dir,
        &args.bin_dir,
        &format!("refuse_{}", worker.name()),
        6,
    )?;
    let port = ControlPort::new(&case.robot_id)?;
    port.send(worker_command(HookCommandKind::RefuseStop, worker))?;
    let mut command = case.command();
    // READY发送失败进入真实启动收尾，所选成员忽略stop，强制检验有界join。
    command.args(["--notify-mode", "required"]);
    command.env("ROBOT_TEST_INITIAL_COMMAND_SEQUENCE", "1");
    let mut child = case.spawn("s0", command)?;
    wait_applied(&case, &mut child, 1).await?;
    let code = wait_exit(&mut child, Duration::from_secs(12)).await?;
    let reaped_ns = SystemClock.now().0;
    let observation = case.observation("s0")?;
    verify_initialization(&observation)?;
    verify_exit_deadline(
        &observation,
        reaped_ns,
        if code == 88 {
            11_800_000_000
        } else {
            11_700_000_000
        },
    )?;
    if !matches!(code, 1 | 88)
        || observation.ready_reached
        || observation.stop_bitmap != 0x7f
        || observation.worker_bitmap != 0x7f
        || observation.disabled_ns == 0
        || observation.refused_stop_bitmap & (1 << worker as u8) == 0
    {
        return Err(format!(
            "拒停成员未被启动收尾正确覆盖：code={code} {observation:?}"
        ));
    }
    if code == 88 {
        let elapsed = observation
            .fired_ns
            .saturating_sub(observation.main_start_ns);
        if observation.fired_checkpoint != 2
            || !(11_700_000_000..=11_800_000_000).contains(&elapsed)
        {
            return Err(format!("拒停成员未在CP2窗执法：{observation:?}"));
        }
    }
    Ok(
        json!({"worker": worker.name(), "exit_code": code, "observation": format!("{observation:?}"), "log_directory": case.directory}),
    )
}

fn verify_lock_rejection(
    case: &Case,
    name: &str,
    before: &(Vec<u8>, Vec<u8>),
) -> Result<Value, String> {
    let observation = case.observation(name)?;
    if marker_pair(&case.trust)? != *before
        || observation.first_io_ns != 0
        || observation.serial_calls.iter().any(|count| *count != 0)
        || observation.worker_bitmap != 0
        || observation.ready_reached
    {
        return Err(format!(
            "争锁失败却改变marker或启动adapter：{name} {observation:?}"
        ));
    }
    Ok(
        json!({"process": name, "exit_code": 87, "observation": format!("{observation:?}"), "marker_unchanged": true}),
    )
}

fn rx_trust_command(case: &Case, operation: &str) -> tokio::process::Command {
    let mut command = isolated_command(&case.bin_dir.join("s0_daemon"));
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
    command
}

async fn lock_process(args: &Args) -> Result<Value, String> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::process::ExitStatusExt;
    let case = Case::new(&args.evidence_dir, &args.bin_dir, "lock", 6)?;
    for name in ["rx_trust_marker", "rx_trust_marker.bak"] {
        std::fs::write(case.trust.join(name), b"1\n").map_err(|e| e.to_string())?;
    }
    let before = marker_pair(&case.trust)?;
    let mut command = case.command();
    command.args(["--rx-trust-policy", "marker"]);
    let mut owner = case.spawn("owner", command)?;
    wait_log(
        &case,
        &mut owner,
        "owner",
        "boot_ready",
        Duration::from_secs(4),
    )
    .await?;
    let lock_path = case.trust.join("rx_trust.lock");
    let inode = std::fs::metadata(&lock_path)
        .map_err(|error| error.to_string())?
        .ino();
    let mut rejections = Vec::new();
    let mut command = case.command();
    command.args(["--rx-trust-policy", "marker"]);
    let mut contender = case.spawn("contender", command)?;
    if wait_exit(&mut contender, Duration::from_secs(2)).await? != 87 {
        return Err("并发daemon没有被锁拒绝87".to_owned());
    }
    rejections.push(verify_lock_rejection(&case, "contender", &before)?);
    for operation in ["status", "authorize", "revoke"] {
        let command = rx_trust_command(&case, operation);
        let mut child = case.spawn(operation, command)?;
        if wait_exit(&mut child, Duration::from_secs(2)).await? != 87 {
            return Err(format!("并发{operation}未被锁拒绝87"));
        }
        rejections.push(verify_lock_rejection(&case, operation, &before)?);
    }
    if owner
        .try_wait()
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Err("持锁进程已自行退出，不能作为SIGKILL释放锁证据".to_owned());
    }
    owner.start_kill().map_err(|error| error.to_string())?;
    let killed = tokio::time::timeout(Duration::from_secs(2), owner.wait())
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    if killed.signal() != Some(libc::SIGKILL) || marker_pair(&case.trust)? != before {
        return Err(format!("SIGKILL退出或marker保持失败：{killed}"));
    }
    let authorize = rx_trust_command(&case, "authorize");
    let mut child = case.spawn("authorize_after_kill", authorize)?;
    if wait_exit(&mut child, Duration::from_secs(2)).await? != 0
        || marker_pair(&case.trust)? != (b"0\n".to_vec(), b"0\n".to_vec())
    {
        return Err("SIGKILL释放锁后真实授权未产生0/0".to_owned());
    }
    let mut command = case.command();
    command.args(["--rx-trust-policy", "marker"]);
    let mut next = case.spawn("after_kill", command)?;
    let result = async {
        wait_log(&case, &mut next, "after_kill", "boot_ready", Duration::from_secs(4)).await?;
        let observation = case.observation("after_kill")?;
        if std::fs::metadata(&lock_path).map_err(|error| error.to_string())?.ino() != inode {
            return Err("锁文件inode被替换".to_owned());
        }
        if marker_pair(&case.trust)? != before || !observation.ready_reached
            || observation.preflight_finished_ns == 0 || observation.first_exit_ns != 0
            || observation.serial_calls[3] == 0 || observation.serial_calls[5] == 0 {
            return Err(format!("授权后真实startup未消费或未走Passed物理轮：{observation:?}"));
        }
        Ok(json!({"stable_lock_inode": inode, "sigkill_releases_lock": true,
            "rejections": rejections, "authorized_restart": format!("{observation:?}"), "log_directory": case.directory}))
    }.await;
    let cleanup = stop(&mut next).await;
    match (result, cleanup) {
        (Ok(evidence), Ok(())) => Ok(evidence),
        (result, cleanup) => Err(format!("锁验收={result:?}；收尾={cleanup:?}")),
    }
}
