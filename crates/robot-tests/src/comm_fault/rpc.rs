//! 真 S0、真 gateway 与真实 RPC：只用隔离 mock 总线提供设备输入。

use std::net::TcpListener;
use std::time::Duration;

use robot_core::time::{RobotClock, SystemClock};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{GetStateRequest, RobotState, S0Mode, SetEnabledCommand};
use serde_json::{Value, json};
use tonic::Code;
use tonic::transport::Channel;

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

pub(super) use robot_tests::comm_fault_rpc::{command_meta, connect};

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
        let remaining = observation_deadline
            .checked_sub(t0)
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                format!("原Fault观察deadline前未完成，禁止继续GetState；尝试={samples:?}")
            })?;
        let response = tokio::time::timeout(
            Duration::from_nanos(remaining),
            client.get_state(GetStateRequest {
                hardware_id: "arm".into(),
            }),
        )
        .await;
        let t1 = SystemClock.now().0;
        let response = match response {
            Ok(response) => response.map(tonic::Response::into_inner),
            Err(error) => {
                samples.push(json!({"request_ns":t0,"response_ns":t1,"rtt_ns":t1-t0,"error":{"source":"原Fault观察deadline","message":error.to_string()}}));
                return Err(format!(
                    "原GetState await耗尽同一观察预算；尝试={samples:?}"
                ));
            }
        };
        if record_fault_poll(
            &mut samples,
            response,
            fault_code,
            t0,
            t1,
            observation_deadline,
        )? {
            return Ok(samples);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn record_fault_poll(
    samples: &mut Vec<Value>,
    response: Result<RobotState, tonic::Status>,
    fault_code: u32,
    started: u64,
    observed: u64,
    deadline: u64,
) -> Result<bool, String> {
    let mut attempt = json!({"request_ns":started,"response_ns":observed,"rtt_ns":observed.saturating_sub(started)});
    match &response {
        Ok(snapshot) => {
            attempt["mode"] = json!(snapshot.mode);
            attempt["fault_code"] = json!(snapshot.fault_code);
            attempt["state_stamp_ns"] = json!(snapshot.stamp_ns);
        }
        Err(error) => {
            attempt["error"] = json!({"source":"原GetState RPC","code":format!("{:?}",error.code()),"message":error.message()});
        }
    }
    samples.push(attempt);
    if started >= deadline || observed >= deadline || observed < started {
        return Err(format!(
            "原GetState成功或错误观察越过同一deadline；尝试={samples:?}"
        ));
    }
    match response {
        Ok(snapshot) => {
            Ok(snapshot.mode == S0Mode::Fault as i32 && snapshot.fault_code == fault_code)
        }
        Err(error)
            if error.code() == Code::Unavailable
                && error.message() == "hardware_id 'arm' has no S0 state sample yet" =>
        {
            Ok(false)
        }
        Err(error) => Err(format!(
            "原GetState非初始NoFrame错误：{error}；尝试={samples:?}"
        )),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "同一真实请求的注入、首故障、采样停止、客户端结果和原件退出按序闭合"
)]
async fn inflight_fault(
    args: &Args,
    name: &str,
    fault_code: u32,
    serial_error: u8,
) -> Result<Value, String> {
    let case = Case::new(&args.evidence_dir, &args.bin_dir, name, 6)?;
    let port = ControlPort::new(&case.robot_id)?;
    let startup_deadline = SystemClock.now().0 + 5_000_000_000;
    let mut s0 = case.spawn("s0", case.command())?;
    let (command, address) = gateway_command(&case)?;
    let mut gateway = case.spawn("gateway", command)?;
    let mut heartbeat = None;
    let mut persistence = None;
    let unretired = matches!(fault_code, 6 | 7);
    let result = async {
        wait_log(&case, &mut s0, "s0", "boot_ready", Duration::from_secs(5)).await?;
        wait_log(&case, &mut gateway, "gateway", "rr_io_threads started", Duration::from_secs(5)).await?;
        super::scenarios::wait_runtime_armed(&case, &mut s0, "s0", startup_deadline).await?;
        persistence = Some(persistence_snapshot(&case)?);
        heartbeat = Some(HeartbeatFeed::start(&case.robot_id)?);
        let mut client = connect(&address).await?;
        wait_enable_ready(&mut client).await?;
        let before = case.observation("s0")?;
        let trace = case.trace("s0")?;
        if !trace.installed() {
            return Err("S0未安装真实运行时事件环".to_owned());
        }
        send_sample_request(&port, &case, &mut s0, &trace, SampleMode::ArmInflight).await?;
        if fault_code == 6 {
            port.send(serial_command(1, BusOpClass::Safety, 0, 0, 3))?;
            wait_applied(&case, &mut s0, 1).await?;
        }
        // 延迟只制造原在途窗口，不改变实际 adapter 返回和收敛流程。
        port.send(serial_command(2, BusOpClass::NormalEffectful, serial_error, 100, 1))?;
        wait_applied(&case, &mut s0, 2).await?;
        let original = SetEnabledCommand {
            meta: Some(command_meta(false, 2000)),
            enabled: true,
        };
        let first_command = original.clone();
        let started = SystemClock.now().0;
        let mut request_client = client.clone();
        let mut request = RequestTask(tokio::spawn(async move {
            let response = request_client.set_enabled(first_command).await;
            (SystemClock.now().0, response)
        }));
        let deadline = started + 1_000_000_000;
        loop {
            let observed = case.observation("s0")?;
            if observed.serial_calls[BusOpClass::NormalEffectful as usize]
                > before.serial_calls[BusOpClass::NormalEffectful as usize] {
                break;
            }
            if request.0.is_finished() || SystemClock.now().0 >= deadline {
                let capture = capture_fault_evidence(&case, &mut s0, &port, &trace, fault_code, started + 1_100_000_000).await;
                return Err(format!("RPC未进入真实normal adapter调用；采样收尾={capture:?}"));
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        if fault_code == 6 {
            // 保原暂停交错：三次真实 Safety 失败先于 IPC 消费故障。
            port.send(TestCommFaultCommand {
                sequence: 3, duration_ms: 300, count: 0,
                kind: HookCommandKind::PauseWorker as u8, target: WorkerKind::Ipc as u8,
                error: 0, reserved: [0; 5],
            })?;
            wait_applied(&case, &mut s0, 3).await?;
        }
        let samples = poll_fault(&mut client, fault_code, started + 1_100_000_000).await;
        // 必须在客户端期限/L 等待之前停止原采样；不让错误分支跳过原事件保存。
        let capture = capture_fault_evidence(&case, &mut s0, &port, &trace, fault_code, started + 1_100_000_000).await;
        let response = tokio::time::timeout(Duration::from_secs(2), &mut request.0).await;
        let (completed, response) = response
            .map_err(|error| format!("原RPC等待={error}；采样={capture:?}；Fault={samples:?}"))?
            .map_err(|error| format!("原RPC任务={error}；采样={capture:?}；Fault={samples:?}"))?;
        let (trace_events, trace_path) = capture
            .map_err(|error| format!("采样={error}；原RPC结果={response:?}；Fault={samples:?}"))?;
        let samples = samples.map_err(|error| format!("Fault={error}；原RPC结果={response:?}"))?;
        let trace_boundary = verify_inflight_trace(&trace_events, fault_code)?;
        let retirement = match fault_code {
            1 => {
                match response {
                    Err(error) if error.code() == Code::Internal
                        && error.message().contains("adapter enable/disable ack failed") => {}
                    other => return Err(format!("在途RPC未映射AdapterError：{other:?}")),
                }
                verify_rpc_retirement(started, completed)?
            }
            6 | 7 => {
                let timeout = match response {
                    Err(error) if error.code() == Code::Cancelled && error.message() == "Timeout expired" => {
                        json!({"code": format!("{:?}", error.code()), "message": error.message(),
                            "scope": "客户端原2秒等待结束，不代表S0退休或信用归还"})
                    }
                    other => return Err(format!("未退休原RPC等待结果不符：{other:?}")),
                };
                let retry = verify_inflight_retry(&case, &mut client, &original).await?;
                json!({"client_wait": timeout, "same_request_retry": retry,
                    "scope": "原物理义务不可退休，待真实L收尾；不套用成功退休期限"})
            }
            other => return Err(format!("未批准的Fault场景：{other}")),
        };
        heartbeat.as_ref().ok_or("缺原heartbeat拥有者")?.verify_running()?;
        let after = case.observation("s0")?;
        if after.serial_calls[BusOpClass::NormalEffectful as usize]
            != before.serial_calls[BusOpClass::NormalEffectful as usize] + 1 {
            return Err(format!("同一请求发生重复normal调用：{before:?} → {after:?}"));
        }
        if fault_code == 6 && after.serial_calls[BusOpClass::Safety as usize]
            != before.serial_calls[BusOpClass::Safety as usize] + 3 {
            return Err("SafetyStopFailed的真实安全调用数不是严格三次".to_owned());
        }
        verify_latch_zero(&case)?;
        let sla = match fault_code {
            7 => verify_fault_visibility(&trace_events, samples.last().ok_or("缺GetState样本")?)?,
            _ => Value::Null,
        };
        Ok(json!({"expected_fault_code": fault_code, "rpc_started_ns": started,
            "rpc_completed_ns": completed, "rpc_retirement_budget": retirement,
            "get_state": samples, "before": format!("{before:?}"), "after": format!("{after:?}"),
            "fault_visibility_sla": sla, "trace_boundary": trace_boundary,
            "trace_csv": trace_path, "trace_dropped": trace.dropped(),
            "scope": "真实Fault、原请求与分层持责证据；InFlight不等于直接观测WirePermit或IPC私有locked",
            "log_directory": case.directory}))
    }.await;
    let result = finish_rpc_case(
        &case,
        &mut s0,
        &mut gateway,
        unretired,
        persistence.as_deref(),
        result,
    )
    .await;
    drop(heartbeat);
    result
}

async fn capture_fault_evidence(
    case: &Case,
    s0: &mut tokio::process::Child,
    port: &ControlPort,
    trace: &robot_s0::comm_fault_trace::TracePage,
    fault_code: u32,
    observation_deadline_ns: u64,
) -> Result<
    (
        Vec<robot_s0::comm_fault_trace::TraceEvent>,
        std::path::PathBuf,
    ),
    String,
> {
    use std::io::Write;
    let expected_sample = trace.armed_seq();
    let mut events = Vec::new();
    // Fault 可见不保证同线程随后的 Confirm 已完成；仅等本次真实目标事件，不预测调度时刻。
    let collected = loop {
        events.extend(trace.drain(8192));
        if expected_sample == 0
            || events
                .iter()
                .any(|event| event.sample_seq != expected_sample)
        {
            break Err("采样未实际启动或混入其他 sample 的原事件".to_owned());
        }
        if trace.dropped() != 0 {
            break Err(format!("目标事件收集期间发生丢弃：{}", trace.dropped()));
        }
        if SystemClock.now().0 > observation_deadline_ns {
            break Err(format!(
                "原 started+1.1s 内目标事件未齐：{:?}",
                verify_inflight_trace(&events, fault_code).err()
            ));
        }
        if verify_inflight_trace(&events, fault_code).is_ok() {
            break Ok(());
        }
        // 交还异步执行权同时继续排空固定容量环，不插入新的固定等待或延长观察期限。
        tokio::task::yield_now().await;
    };
    let collected_ns = SystemClock.now().0;
    let disarmed = send_sample_request(port, case, s0, trace, SampleMode::Disarm).await;
    events.extend(trace.drain(8192));
    let path = case.directory.join("fault_trace.csv");
    let mut output = std::fs::File::create(&path).map_err(|error| error.to_string())?;
    writeln!(
        output,
        "sample_seq,stamp_ns,kind,source,normal_seq,generation,a,b"
    )
    .map_err(|error| error.to_string())?;
    for event in &events {
        writeln!(
            output,
            "{},{},{:?},{},{},{},{},{}",
            event.sample_seq,
            event.stamp_ns,
            event.kind,
            event.source,
            event.normal_seq,
            event.generation,
            event.a,
            event.b
        )
        .map_err(|error| error.to_string())?;
    }
    std::fs::write(
        case.directory.join("fault_capture.json"),
        serde_json::to_vec_pretty(&json!({"disarm": format!("{disarmed:?}"),
            "dropped": trace.dropped(), "events": events.len(), "trace_csv": path,
            "expected_sample": expected_sample, "collected": format!("{collected:?}"),
            "collected_ns": collected_ns, "observation_deadline_ns": observation_deadline_ns}))
        .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    collected.map_err(|error| format!("原采样收集={error}；Disarm={disarmed:?}"))?;
    let (_, boundary) = disarmed?;
    if boundary.failure.is_some() || trace.dropped() != 0 {
        return Err(format!(
            "采样边界失败或丢事件：{boundary:?} dropped={}",
            trace.dropped()
        ));
    }
    if events
        .iter()
        .any(|event| event.sample_seq != expected_sample)
    {
        return Err("Disarm 末次排空混入其他 sample 的原事件".to_owned());
    }
    let _ = verify_inflight_trace(&events, fault_code)?;
    Ok((events, path))
}

fn verify_inflight_trace(
    events: &[robot_s0::comm_fault_trace::TraceEvent],
    code: u32,
) -> Result<Value, String> {
    use robot_s0::comm_fault_trace::TraceKind as K;
    let acquired = events
        .iter()
        .find(|event| event.kind == K::NormalAcquire && event.normal_seq != 0)
        .ok_or("缺原normal seq实际acquire")?;
    let released = events
        .iter()
        .find(|event| {
            event.kind == K::NormalRelease
                && event.normal_seq == acquired.normal_seq
                && event.sample_seq == acquired.sample_seq
                && event.stamp_ns >= acquired.stamp_ns
        })
        .ok_or("缺同原normal seq实际release")?;
    let commit = events
        .iter()
        .find(|event| {
            event.kind == K::FaultIpcCommit
                && event.sample_seq == acquired.sample_seq
                && event.a == u64::from(code)
        })
        .ok_or("缺原目标Fault真实IPC提交")?;
    if released.stamp_ns > commit.stamp_ns {
        return Err("业务Fault提交早于本用例原normal release".to_owned());
    }
    match code {
        7 => {
            if !events.iter().any(|event| {
                event.kind == K::ConfirmReturn
                    && event.sample_seq == acquired.sample_seq
                    && event.generation == commit.generation
                    && event.a == 0
                    && event.b == 1
            }) || events.iter().any(|event| {
                event.kind == K::AllOffTrusted
                    && event.sample_seq == acquired.sample_seq
                    && event.generation == commit.generation
            }) {
                return Err("CommFault未真读到全关且成功，或出现可信Off发布".to_owned());
            }
        }
        6 => {
            if commit.b != 128 {
                return Err(format!("SafetyStopFailed原detail不符：{commit:?}"));
            }
            // a 是本轮尝试号、b 是实际调用是否成功；升级到 gen2 前的 gen1 失败同样归原采样。
            let returns: Vec<_> = events
                .iter()
                .filter(|event| {
                    event.kind == K::SafetyReturn
                        && event.sample_seq == acquired.sample_seq
                        && event.stamp_ns >= acquired.stamp_ns
                        && event.stamp_ns <= commit.stamp_ns
                })
                .collect();
            if returns.len() != 3 {
                return Err(format!("原在途窗口SafetyReturn不是严格三件：{returns:?}"));
            }
            for (event, attempt) in returns.iter().zip(1..=3) {
                if event.a != attempt || event.b != 0 {
                    return Err(format!("第{attempt}次原Safety失败不符或重复：{event:?}"));
                }
            }
        }
        1 => {}
        other => return Err(format!("未批准的Fault：{other}")),
    }
    Ok(
        json!({"normal_seq": acquired.normal_seq, "sample_seq": acquired.sample_seq,
        "acquired_ns": acquired.stamp_ns, "released_ns": released.stamp_ns,
        "fault_generation": commit.generation, "fault_committed_ns": commit.stamp_ns,
        "fault_code": commit.a, "fault_detail": commit.b}),
    )
}

async fn verify_inflight_retry(
    case: &Case,
    client: &mut RobotControlClient<Channel>,
    original: &SetEnabledCommand,
) -> Result<Value, String> {
    let before = case.observation("s0")?;
    let result = client.set_enabled(original.clone()).await;
    let after = case.observation("s0")?;
    let error = match result {
        Err(error)
            if error.code() == Code::Aborted
                && matches!(
                    error.message(),
                    "request remains in flight: Sent" | "request remains in flight: Orphan"
                ) =>
        {
            error
        }
        other => return Err(format!("全同请求未保原在途缓存：{other:?}")),
    };
    if before.serial_calls[BusOpClass::NormalEffectful as usize]
        != after.serial_calls[BusOpClass::NormalEffectful as usize]
    {
        return Err(format!(
            "全同请求重试产生新normal调用：{before:?} → {after:?}"
        ));
    }
    let meta = original.meta.as_ref().ok_or("原请求缺meta")?;
    Ok(
        json!({"request_id": meta.request_id, "idempotency_key": meta.idempotency_key,
        "dry_run": meta.dry_run, "timeout_ms": meta.timeout_ms, "enabled": original.enabled,
        "code": format!("{:?}", error.code()), "message": error.message(),
        "normal_calls_before": before.serial_calls[0], "normal_calls_after": after.serial_calls[0],
        "scope": "全同meta走真实InFlight拒绝；只直接证明原缓存未终结，不是WirePermit数量观测"}),
    )
}

fn persistence_snapshot(case: &Case) -> Result<Vec<Option<Vec<u8>>>, String> {
    [
        case.latch.clone(),
        case.directory.join("estop_latch.bak"),
        case.trust.join("rx_trust_marker"),
        case.trust.join("rx_trust_marker.bak"),
    ]
    .iter()
    .map(|path| match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("读取原持久化文件{}：{error}", path.display())),
    })
    .collect()
}

fn verify_latch_zero(case: &Case) -> Result<(), String> {
    for path in [&case.latch, &case.directory.join("estop_latch.bak")] {
        if std::fs::read(path).map_err(|error| error.to_string())? != b"0\n" {
            return Err(format!("非急停故障误改独立latch：{}", path.display()));
        }
    }
    Ok(())
}

async fn finish_rpc_case(
    case: &Case,
    s0: &mut tokio::process::Child,
    gateway: &mut tokio::process::Child,
    unretired: bool,
    original_files: Option<&[Option<Vec<u8>>]>,
    result: Result<Value, String>,
) -> Result<Value, String> {
    let (s0_stop, gateway_stop) = if unretired {
        let s0_stop = super::scenarios::stop_unretired_runtime(
            case,
            s0,
            "s0",
            super::scenarios::UnretiredRuntimeCase::SetRrHeld,
        )
        .await;
        let gateway_stop = stop(gateway).await;
        (s0_stop, gateway_stop)
    } else {
        let gateway_stop = stop(gateway).await;
        let s0_stop = stop(s0).await.map(|()| Value::Null);
        (s0_stop, gateway_stop)
    };
    let files = match original_files {
        Some(original) => persistence_snapshot(case).and_then(|after| {
            if after.as_slice() == original {
                Ok(())
            } else {
                Err("收尾改变原marker存在性/内容或独立latch".to_owned())
            }
        }),
        None => Err("场景未取得原持久化快照".to_owned()),
    };
    match (result, s0_stop, gateway_stop, files) {
        (Ok(mut evidence), Ok(shutdown), Ok(()), Ok(())) => {
            evidence["s0_shutdown"] = shutdown;
            evidence["gateway_shutdown"] = json!({"exit_code": 0,
                "scope": "真实回收和原outcome；不以exit0推定ledger已空，原退出聚合保至进程exit"});
            evidence["persistent_files_unchanged"] = json!(true);
            Ok(evidence)
        }
        (result, s0, gateway, files) => Err(format!(
            "场景={result:?}；S0收尾={s0:?}；gateway收尾={gateway:?}；文件={files:?}"
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

#[allow(
    clippy::too_many_lines,
    reason = "同一dirty会话的启用拒绝、原Disable持责、持续状态和原件收尾按序验证"
)]
async fn dirty_boot(args: &Args) -> Result<Value, String> {
    let case = Case::new(&args.evidence_dir, &args.bin_dir, "rpc_dirty", 6)?;
    let mut command = case.command();
    command.args(["--rx-trust-policy", "marker"]);
    let startup_deadline = SystemClock.now().0 + 5_000_000_000;
    let mut s0 = case.spawn("s0", command)?;
    let (command, address) = gateway_command(&case)?;
    let mut gateway = case.spawn("gateway", command)?;
    let mut persistence = None;
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
        super::scenarios::wait_runtime_armed(&case, &mut s0, "s0", startup_deadline).await?;
        persistence = Some(persistence_snapshot(&case)?);
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
        let original = SetEnabledCommand {
            meta: Some(command_meta(false, 1000)),
            enabled: false,
        };
        let started = SystemClock.now().0;
        let response = client.set_enabled(original.clone()).await;
        let completed = SystemClock.now().0;
        let wait_evidence = match response {
            Err(error)
                if error.code() == Code::DeadlineExceeded
                    && error.message()
                        == "no RR response within 1500 ms (timeout_ms + grace_ms)" =>
            {
                json!({"code": format!("{:?}", error.code()), "message": error.message(),
                    "started_ns": started, "completed_ns": completed,
                    "scope": "原handler1500ms结束，物理退休及信用归还尚未发生"})
            }
            other => return Err(format!("Dirty原Disable未退休等待结果不符：{other:?}")),
        };
        let retry = verify_inflight_retry(&case, &mut client, &original).await?;
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
        let after = case.observation("s0")?;
        if after.serial_calls[BusOpClass::NormalEffectful as usize] != 0 {
            return Err(format!(
                "dirty拒绝/未退休Disable发生normal物理调用：{after:?}"
            ));
        }
        verify_latch_zero(&case)?;
        Ok(
            json!({"fault_code": initial.fault_code, "mode": initial.mode,
            "state_stamps": stamps, "get_state_rtt_ns": rtt_ns, "poll_interval_ms": 10,
            "initial_disable_wait": wait_evidence, "same_request_retry": retry,
            "after": format!("{after:?}"), "log_directory": case.directory}),
        )
    }
    .await;
    finish_rpc_case(
        &case,
        &mut s0,
        &mut gateway,
        true,
        persistence.as_deref(),
        result,
    )
    .await
}

#[cfg(test)]
#[path = "rpc_acceptance.rs"]
mod acceptance;

#[cfg(test)]
mod r4c_poll_tests {
    use super::*;

    #[test]
    fn only_original_no_frame_error_retries_and_preserves_actual_attempt_fields() {
        let mut attempts = Vec::new();
        assert!(
            !record_fault_poll(
                &mut attempts,
                Err(tonic::Status::unavailable(
                    "hardware_id 'arm' has no S0 state sample yet"
                )),
                1,
                100,
                110,
                200
            )
            .unwrap()
        );
        assert_eq!(attempts[0]["error"]["code"], "Unavailable");
        assert_eq!(attempts[0]["request_ns"], 100);
        assert_eq!(attempts[0]["response_ns"], 110);
        assert_eq!(attempts[0]["rtt_ns"], 10);
        let state = RobotState {
            mode: S0Mode::Fault as i32,
            fault_code: 1,
            stamp_ns: 115,
            ..RobotState::default()
        };
        assert!(record_fault_poll(&mut attempts, Ok(state), 1, 120, 130, 200).unwrap());
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[1]["state_stamp_ns"], 115);
    }

    #[test]
    fn foreign_unavailable_and_other_rpc_errors_are_never_treated_as_initial_no_frame() {
        for error in [
            tonic::Status::unavailable("transport is closed"),
            tonic::Status::unavailable("hardware_id 'other' has no S0 state sample yet"),
            tonic::Status::internal("hardware_id 'arm' has no S0 state sample yet"),
        ] {
            let mut attempts = Vec::new();
            assert!(record_fault_poll(&mut attempts, Err(error), 1, 100, 110, 200).is_err());
            assert_eq!(attempts.len(), 1);
        }
    }

    #[test]
    fn matching_fault_cannot_pass_equal_late_or_reversed_original_deadline_observation() {
        for (started, observed) in [(100, 200), (100, 201), (200, 201), (110, 100)] {
            let mut attempts = Vec::new();
            let state = RobotState {
                mode: S0Mode::Fault as i32,
                fault_code: 1,
                ..RobotState::default()
            };
            assert!(
                record_fault_poll(&mut attempts, Ok(state), 1, started, observed, 200).is_err()
            );
            assert_eq!(attempts.len(), 1);
        }
    }
}
