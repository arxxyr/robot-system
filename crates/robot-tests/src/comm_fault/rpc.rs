//! 真 S0、真 gateway 与真实 RPC：只用隔离 mock 总线提供设备输入。

use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use robot_core::time::{RobotClock, SystemClock};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{
    AuthContext, CommandMeta, GetStateRequest, RobotState, S0Mode, SetEnabledCommand,
};
use serde_json::{Value, json};
use tonic::Code;
use tonic::transport::{Channel, Endpoint};

use robot_core::serial_services::BusOpClass;
use robot_s0::comm_fault_hook::{HookCommandKind, RESULT_HARDWARE_FAULT, TestCommFaultCommand};
use robot_s0::comm_fault_trace::SampleMode;
use robot_s0::worker_stop::WorkerKind;

use super::Args;
use super::control::{ControlPort, wait_applied};
use super::harness::{Case, isolated_command, stop, wait_log};
use super::heartbeat::HeartbeatFeed;
use super::report::Report;
use super::sla::send_sample_request;

struct RequestTask<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for RequestTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub(super) async fn run(args: &Args, report: &mut Report) -> Result<(), String> {
    if args
        .case
        .as_deref()
        .is_none_or(|filter| matches!(filter, "AC-15" | "AC-17"))
    {
        let started = SystemClock.now().0;
        report.record(
            "rpc_dirty_boot_comm_fault",
            "AC-15/17",
            started,
            dirty_boot(args).await,
        );
    }
    if args.case.as_deref().is_none_or(|filter| filter == "AC-17") {
        for (name, code, serial_error) in [
            ("adapter_error", 1, RESULT_HARDWARE_FAULT),
            ("comm_fault", 7, 0),
            ("safety_stop_failed", 6, RESULT_HARDWARE_FAULT),
        ] {
            let started = SystemClock.now().0;
            report.record(
                &format!("rpc_inflight_{name}"),
                "AC-17",
                started,
                inflight_fault(args, name, code, serial_error).await,
            );
        }
    }
    Ok(())
}

fn serial_command(
    sequence: u64,
    class: BusOpClass,
    error: u8,
    duration_ms: u32,
    count: u32,
) -> TestCommFaultCommand {
    TestCommFaultCommand {
        sequence,
        duration_ms,
        count,
        kind: HookCommandKind::SerialResult as u8,
        target: class as u8,
        error,
        reserved: [0; 5],
    }
}

async fn wait_enable_ready(client: &mut RobotControlClient<Channel>) -> Result<(), String> {
    let ready_deadline = SystemClock.now().0 + 2_000_000_000;
    loop {
        let readiness = client
            .set_enabled(SetEnabledCommand {
                meta: Some(command_meta(true, 1000)),
                enabled: true,
            })
            .await;
        match readiness {
            Ok(_) => break,
            Err(error) if SystemClock.now().0 >= ready_deadline => {
                return Err(format!("启用前置未满足：{error}"));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
    Ok(())
}

async fn poll_fault(
    client: &mut RobotControlClient<Channel>,
    fault_code: u32,
    observation_deadline: u64,
) -> Result<Vec<Value>, String> {
    let mut samples = Vec::new();
    loop {
        let t0 = SystemClock.now().0;
        let snapshot = state(client).await?;
        let t1 = SystemClock.now().0;
        samples.push(json!({"request_ns": t0, "response_ns": t1, "rtt_ns": t1-t0,
                "mode": snapshot.mode, "fault_code": snapshot.fault_code, "state_stamp_ns": snapshot.stamp_ns}));
        if snapshot.mode == S0Mode::Fault as i32 && snapshot.fault_code == fault_code {
            break;
        }
        if t1 >= observation_deadline {
            return Err(format!("目标Fault {fault_code}未可见：{snapshot:?}"));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(samples)
}

async fn inflight_fault(
    args: &Args,
    name: &str,
    fault_code: u32,
    serial_error: u8,
) -> Result<Value, String> {
    let case = Case::new(&args.evidence_dir, &args.bin_dir, name, 6)?;
    let port = ControlPort::new(&case.robot_id)?;
    let mut s0 = case.spawn("s0", case.command())?;
    let (command, address) = gateway_command(&case)?;
    let mut gateway = case.spawn("gateway", command)?;
    let result = async {
        wait_log(&case, &mut s0, "s0", "boot_ready", Duration::from_secs(5)).await?;
        wait_log(&case, &mut gateway, "gateway", "rr_io_threads started", Duration::from_secs(5)).await?;
        let heartbeat = HeartbeatFeed::start(&case.robot_id)?;
        let mut client = connect(&address).await?;
        wait_enable_ready(&mut client).await?;
        let before = case.observation("s0")?;
        let trace = case.trace("s0")?;
        if !trace.installed() { return Err("S0未安装真实运行时事件环".to_owned()); }
        send_sample_request(&port, &case, &mut s0, &trace, SampleMode::ArmInflight).await?;
        if fault_code == 6 {
            port.send(serial_command(1, BusOpClass::Safety, 0, 0, 3))?;
            wait_applied(&case, &mut s0, 1).await?;
        }
        // 延迟仅制造可观察在途窗口；真实正常 action 返回后仍走生产收敛状态机。
        port.send(serial_command(2, BusOpClass::NormalEffectful, serial_error, 100, 1))?;
        wait_applied(&case, &mut s0, 2).await?;
        let started = SystemClock.now().0;
        let mut request_client = client.clone();
        let mut request = RequestTask(tokio::spawn(async move {
            let response = request_client.set_enabled(SetEnabledCommand {
                meta: Some(command_meta(false, 2000)), enabled: true,
            }).await;
            (SystemClock.now().0, response)
        }));
        let deadline = started + 1_000_000_000;
        loop {
            let observed = case.observation("s0")?;
            if observed.serial_calls[BusOpClass::NormalEffectful as usize]
                > before.serial_calls[BusOpClass::NormalEffectful as usize] { break; }
            if request.0.is_finished() || SystemClock.now().0 >= deadline {
                return Err("RPC未进入真实normal adapter调用".to_owned());
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        if fault_code == 6 {
            // 先证明确在途，再暂停IPC，让三次真实Safety失败先于RR退场发生。
            port.send(TestCommFaultCommand { sequence: 3, duration_ms: 300, count: 0,
                kind: HookCommandKind::PauseWorker as u8, target: WorkerKind::Ipc as u8,
                error: 0, reserved: [0; 5] })?;
            wait_applied(&case, &mut s0, 3).await?;
        }
        let samples = poll_fault(&mut client, fault_code, started + 1_100_000_000).await?;
        let (completed, response) = tokio::time::timeout(Duration::from_secs(2), &mut request.0).await
            .map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
        match response {
            Err(error) if error.code() == Code::Internal && error.message().contains("adapter enable/disable ack failed") => {}
            other => return Err(format!("在途RPC未映射AdapterError：{other:?}")),
        }
        let retirement = verify_rpc_retirement(started, completed)?;
        heartbeat.verify_running()?;
        let after = case.observation("s0")?;
        if fault_code == 6 && after.serial_calls[BusOpClass::Safety as usize]
            < before.serial_calls[BusOpClass::Safety as usize] + 3 {
            return Err("SafetyStopFailed未经过三次真实安全调用".to_owned());
        }
        if std::fs::read(&case.latch).map_err(|e| e.to_string())? != b"0\n" {
            return Err("非急停故障误改latch".to_owned());
        }
        let disarmed = send_sample_request(&port, &case, &mut s0, &trace, SampleMode::Disarm).await;
        let trace_events = trace.drain(8192);
        if trace.dropped() != 0 { return Err("事件环丢失，SLA证据无效".to_owned()); }
        let trace_path = case.directory.join("fault_trace.csv");
        {
            use std::io::Write;
            let mut output = std::fs::File::create(&trace_path).map_err(|e| e.to_string())?;
            writeln!(output, "sample_seq,stamp_ns,kind,source,normal_seq,generation,a,b").map_err(|e| e.to_string())?;
            for event in &trace_events {
                writeln!(output, "{},{},{:?},{},{},{},{},{}", event.sample_seq, event.stamp_ns, event.kind,
                    event.source, event.normal_seq, event.generation, event.a, event.b).map_err(|e| e.to_string())?;
            }
        }
        let (_, boundary) = disarmed?;
        if boundary.failure.is_some() { return Err(format!("Fault采样边界失败：{boundary:?}")); }
        let sla = if fault_code == 7 {
            verify_fault_visibility(&trace_events, samples.last().ok_or("缺GetState样本")?)?
        } else { Value::Null };
        Ok(json!({"expected_fault_code": fault_code, "rpc_started_ns": started,
            "rpc_completed_ns": completed, "rpc_retirement_budget": retirement, "get_state": samples,
            "before": format!("{before:?}"), "after": format!("{after:?}"),
            "fault_visibility_sla": sla, "trace_csv": trace_path,
            "scope": "真实三种Fault与在途RR错误映射；CommFault同时核首typed错误和mailbox发布两套起算", "log_directory": case.directory}))
    }.await;
    let gateway_stop = stop(&mut gateway).await;
    let s0_stop = stop(&mut s0).await;
    match (result, gateway_stop, s0_stop) {
        (Ok(value), Ok(()), Ok(())) => Ok(value),
        (result, gateway, s0) => Err(format!(
            "场景={result:?}；gateway收尾={gateway:?}；S0收尾={s0:?}"
        )),
    }
}

fn verify_rpc_retirement(started: u64, completed: u64) -> Result<Value, String> {
    // daemon 未覆盖 IPC 默认轮询间隔；这里与首错传播共用真实配置，不能拿 serial 5ms 代替。
    let ipc_tick_ns = robot_s0::ipc_thread::IpcThreadConfig::default().poll_interval_ns;
    let bound_ns = 1_000_000_000 + 2 * ipc_tick_ns;
    let elapsed_ns = completed.checked_sub(started).ok_or("RPC完成早于开始")?;
    if elapsed_ns > bound_ns {
        return Err(format!(
            "在途RPC退场超过T_pending+2ipc tick：{elapsed_ns}/{bound_ns}ns"
        ));
    }
    Ok(
        json!({"configured_ipc_tick_ns": ipc_tick_ns, "pending_timeout_ns": 1_000_000_000,
        "bound_ns": bound_ns, "elapsed_ns": elapsed_ns}),
    )
}

fn verify_fault_visibility(
    events: &[robot_s0::comm_fault_trace::TraceEvent],
    sample: &Value,
) -> Result<Value, String> {
    use robot_s0::comm_fault_trace::TraceKind;
    let before = events
        .iter()
        .find(|event| event.kind == TraceKind::MailboxPublishing && event.a == 7)
        .ok_or("缺CommFault真实mailbox发布前界")?;
    let matching = |kind| {
        events.iter().find(|event| {
            event.kind == kind
                && event.generation == before.generation
                && event.sample_seq == before.sample_seq
        })
    };
    let after = matching(TraceKind::MailboxPublished).ok_or("缺同gen发布后界")?;
    let first = matching(TraceKind::FirstCommError).ok_or("缺同gen首typed通信错误")?;
    let commit = matching(TraceKind::FaultIpcCommit).ok_or("缺同gen IPC Fault提交")?;
    let response = sample["response_ns"].as_u64().ok_or("缺GetState返回时刻")?;
    let rtt = sample["rtt_ns"].as_u64().ok_or("缺GetState实际RTT")?;
    let request = sample["request_ns"].as_u64().ok_or("缺GetState请求时刻")?;
    if response.checked_sub(request) != Some(rtt) {
        return Err("GetState实际RTT与请求/返回时间戳不符".to_owned());
    }
    if first.stamp_ns > before.stamp_ns
        || before.stamp_ns > after.stamp_ns
        || commit.stamp_ns < before.stamp_ns
        || commit.stamp_ns > response
        || first.normal_seq == 0
        || first.normal_seq != before.normal_seq
    {
        return Err(format!(
            "Fault因果时序/normal归属不符：{first:?} {before:?} {after:?} {commit:?}"
        ));
    }
    // t_publish实际位于前后界；以较早的前界起算只会更严格，不缩短传播延迟。
    let mailbox_elapsed = response
        .checked_sub(before.stamp_ns)
        .ok_or("GetState早于mailbox")?;
    let first_elapsed = response
        .checked_sub(first.stamp_ns)
        .ok_or("GetState早于首错")?;
    let ipc_tick_ns = robot_s0::ipc_thread::IpcThreadConfig::default().poll_interval_ns;
    let serial_tick_ns = robot_s0::serial_io_thread::SerialIoThreadConfig::default().tick_period_ns;
    let mailbox_bound = (2 * ipc_tick_ns + 23_000_000 + 10_000_000)
        .checked_add(rtt)
        .ok_or("实际RTT导致验收界溢出")?;
    let first_bound = mailbox_bound
        .checked_add(serial_tick_ns + 105_000_000)
        .ok_or("首错验收界溢出")?;
    if mailbox_elapsed > mailbox_bound || first_elapsed > first_bound {
        return Err(format!(
            "Fault传播超限：mailbox={mailbox_elapsed}/{mailbox_bound} 首错={first_elapsed}/{first_bound}"
        ));
    }
    Ok(
        json!({"generation": before.generation, "normal_seq": first.normal_seq,
        "first_comm_error_ns": first.stamp_ns, "mailbox_before_ns": before.stamp_ns,
        "mailbox_after_ns": after.stamp_ns, "ipc_committed_ns": commit.stamp_ns,
        "visible_response_ns": response, "measured_rtt_ns": rtt,
        "configured_ipc_tick_ns": ipc_tick_ns, "configured_serial_tick_ns": serial_tick_ns,
        "timing_scope": "daemon未覆盖的同源码默认配置；仅作验收界，不宣称实际周期无抖动",
        "mailbox_elapsed_upper_ns": mailbox_elapsed, "mailbox_bound_ns": mailbox_bound,
        "first_error_elapsed_ns": first_elapsed, "first_error_bound_ns": first_bound}),
    )
}

pub(super) fn command_meta(dry_run: bool, timeout_ms: u64) -> CommandMeta {
    let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    // 测试 ID 保留 UUIDv7 version/variant，唯一性由进程号和单调序号保证。
    let request_id = format!("{:08x}-0000-7000-8000-{sequence:012x}", std::process::id());
    CommandMeta {
        idempotency_key: request_id.clone(),
        request_id,
        source: "comm_fault_e2e".into(),
        auth: Some(AuthContext {
            token: "operator-fake-token-001".into(),
            mtls_fingerprint: String::new(),
        }),
        timeout_ms,
        dry_run,
        audit_reason: "独立通信故障验收".into(),
    }
}

pub(super) async fn connect(address: &str) -> Result<RobotControlClient<Channel>, String> {
    let endpoint = Endpoint::from_shared(format!("http://{address}"))
        .map_err(|error| error.to_string())?
        .connect_timeout(Duration::from_millis(200))
        .timeout(Duration::from_secs(2));
    let deadline = SystemClock.now().0 + 4_000_000_000;
    loop {
        match endpoint.connect().await {
            Ok(channel) => return Ok(RobotControlClient::new(channel)),
            Err(error) if SystemClock.now().0 >= deadline => {
                return Err(format!("连接gateway超时：{error}"));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
}

pub(super) async fn state(client: &mut RobotControlClient<Channel>) -> Result<RobotState, String> {
    client
        .get_state(GetStateRequest {
            hardware_id: "arm".into(),
        })
        .await
        .map(tonic::Response::into_inner)
        .map_err(|error| error.to_string())
}

pub(super) fn gateway_command(case: &Case) -> Result<(tokio::process::Command, String), String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let address = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .to_string();
    let roles = case.directory.join("rpc_roles.toml");
    std::fs::write(&roles, include_str!("../../../../configs/rpc_roles.toml"))
        .map_err(|error| error.to_string())?;
    let mut command = isolated_command(&case.bin_dir.join("robot_rpc_gateway"));
    command
        .args([
            "--robot-id",
            &case.robot_id,
            "--listen-addr",
            &address,
            "--rpc-roles-toml",
        ])
        .arg(roles)
        .arg("--config")
        .arg(&case.config)
        .env("RUST_LOG", "info");
    drop(listener);
    Ok((command, address))
}

async fn dirty_boot(args: &Args) -> Result<Value, String> {
    let case = Case::new(&args.evidence_dir, &args.bin_dir, "rpc_dirty", 6)?;
    let mut command = case.command();
    command.args(["--rx-trust-policy", "marker"]);
    let mut s0 = case.spawn("s0", command)?;
    let (command, address) = gateway_command(&case)?;
    let mut gateway = case.spawn("gateway", command)?;
    let result = async {
        wait_log(&case, &mut s0, "s0", "boot_ready", Duration::from_secs(5)).await?;
        wait_log(
            &case,
            &mut gateway,
            "gateway",
            "rr_io_threads started",
            Duration::from_secs(5),
        )
        .await?;
        let mut client = connect(&address).await?;
        let deadline = SystemClock.now().0 + 3_000_000_000;
        let initial = loop {
            match state(&mut client).await {
                Ok(value) if value.mode == S0Mode::Fault as i32 && value.fault_code == 7 => {
                    break value;
                }
                value if SystemClock.now().0 >= deadline => {
                    return Err(format!("未观测到真实CommFault：{value:?}"));
                }
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        };
        for dry_run in [false, true] {
            let result = client
                .set_enabled(SetEnabledCommand {
                    meta: Some(command_meta(dry_run, 1000)),
                    enabled: true,
                })
                .await;
            match result {
                Err(error) if error.code() == Code::FailedPrecondition => {}
                other => return Err(format!("Dirty会话启用未被拒绝：{other:?}")),
            }
        }
        client
            .set_enabled(SetEnabledCommand {
                meta: Some(command_meta(false, 1000)),
                enabled: false,
            })
            .await
            .map_err(|error| format!("故障下幂等禁用失败：{error}"))?;
        let mut stamps = Vec::new();
        let mut rtt_ns = Vec::new();
        for _ in 0..20 {
            let started = SystemClock.now().0;
            let value = state(&mut client).await?;
            rtt_ns.push(SystemClock.now().0.saturating_sub(started));
            if value.mode != S0Mode::Fault as i32 || value.fault_code != 7 {
                return Err(format!("故障被RPC意外清除：{value:?}"));
            }
            stamps.push(value.stamp_ns);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if std::fs::read(&case.latch).map_err(|error| error.to_string())? != b"0\n" {
            return Err("通信故障修改了独立急停latch".to_owned());
        }
        Ok(
            json!({"fault_code": initial.fault_code, "mode": initial.mode,
            "state_stamps": stamps, "get_state_rtt_ns": rtt_ns, "poll_interval_ms": 10,
            "log_directory": case.directory}),
        )
    }
    .await;
    // 无论断言成功或失败都显式收回自己启动的两个进程，保留原始失败。
    let gateway_stop = stop(&mut gateway).await;
    let s0_stop = stop(&mut s0).await;
    match (result, gateway_stop, s0_stop) {
        (Ok(value), Ok(()), Ok(())) => Ok(value),
        (result, gateway, s0) => Err(format!(
            "场景={result:?}；gateway收尾={gateway:?}；S0收尾={s0:?}"
        )),
    }
}

#[cfg(test)]
#[path = "rpc_acceptance.rs"]
mod acceptance;
