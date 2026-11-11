//! S10：B2原耗尽链经唯一真实gateway写请求，handler超时不是业务退休。
mod oracle;
use super::{
    Args,
    control::ControlPort,
    harness::Case,
    report::Report,
    w4_latch_crash::{TraceCapture, require_running},
};
use robot_core::time::{RobotClock, SystemClock};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{Ack, ClearEStopCommand};
use robot_s0::comm_fault_trace::SampleMode;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{process::Child, task::JoinHandle};
use tonic::{
    Code,
    transport::{Channel, Endpoint},
};

type RequestResult = (u64, Result<Ack, tonic::Status>);
#[derive(Default)]
struct Owned {
    s0: Option<Child>,
    gateway: Option<Child>,
    control: Option<ControlPort>,
    request: Option<JoinHandle<RequestResult>>,
    client: Option<RobotControlClient<Channel>>,
}

pub(super) async fn run(args: &Args, report: &mut Report) -> Result<(), String> {
    report.record(
        "gateway_timeout_orphan_duplicate_keeps_original_exhausted_clear",
        "R4C-GATEWAY-ORPHAN",
        SystemClock.now().0,
        run_case(args).await,
    );
    Ok(())
}
async fn run_case(args: &Args) -> Result<Value, String> {
    let case = Case::new(&args.evidence_dir, &args.bin_dir, "gateway_orphan", 1)?;
    for path in [&case.latch, &case.directory.join("estop_latch.bak")] {
        std::fs::write(path, b"1\n").map_err(|e| e.to_string())?;
    }
    let mut owned = Owned::default();
    let result = exercise(&case, &mut owned).await;
    let s0_cleanup = reap(&mut owned.s0, "s0").await;
    let gateway_cleanup = reap(&mut owned.gateway, "gateway").await;
    let request_cleanup = finish_request(&mut owned).await;
    let control_cleanup = owned.control.as_mut().map(ControlPort::finish).transpose();
    let cleanup_completed_ns = SystemClock.now().0;
    let evidence = json!({"result":result,"s0_cleanup":s0_cleanup,"gateway_cleanup":gateway_cleanup,"request_cleanup":request_cleanup,"control_cleanup":control_cleanup,"cleanup_completed_ns":cleanup_completed_ns,"directory":case.directory});
    save(&case, "result.json", &evidence)?;
    match (
        result,
        s0_cleanup,
        gateway_cleanup,
        request_cleanup,
        control_cleanup,
    ) {
        (Ok(_), Ok(_), Ok(_), Ok(_), Ok(_)) => Ok(evidence),
        (result, s0, gateway, request, control) => Err(format!(
            "原场景={result:?};S0={s0:?};gateway={gateway:?};request={request:?};control={control:?};证据={}",
            case.directory.display()
        )),
    }
}

#[allow(clippy::too_many_lines)] // 同一拥有域串联原请求、原账、原S0与网关收尾，避免拆分时提前释放。
async fn exercise(case: &Case, owned: &mut Owned) -> Result<Value, String> {
    let mut command = case.command();
    command
        .args(["--notify-mode", "standalone"])
        .env("ROBOT_TEST_W4_FILE_FAULT", "ordinary-resync-exhausted")
        .env("ROBOT_LOG_FORMAT", "json")
        .env(
            "RUST_LOG",
            "warn,robot_s0=info,robot_s0::ipc_runtime=info,robot_s0::rr_server_thread=info",
        );
    save(case, "s0_launch.json", &launch(&command))?;
    owned.s0 = Some(case.spawn("s0", command)?);
    owned.control = Some(ControlPort::spawn(&case.robot_id)?);
    owned.control.as_mut().unwrap().wait_ready()?;
    super::harness::wait_log(
        case,
        owned.s0.as_mut().unwrap(),
        "s0",
        "boot_ready",
        Duration::from_secs(12),
    )
    .await?;
    let mut capture = TraceCapture::open(case)?;
    let (sample, _) = super::sla::send_sample_request(
        owned.control.as_ref().unwrap(),
        case,
        owned.s0.as_mut().unwrap(),
        capture.page(),
        SampleMode::ArmInflight,
    )
    .await?;
    capture.set_sample(sample);
    capture.drain();
    let mut meta = super::rpc::command_meta(false, 5000);
    meta.auth.as_mut().ok_or("原auth缺失")?.token = "maintainer-fake-token-001".into();
    let request_id = meta.request_id.clone();
    let key = meta.idempotency_key.clone();
    let request = ClearEStopCommand {
        meta: Some(meta),
        confirmation: "确认已停止".into(),
    };
    let (mut command, address) = super::rpc::gateway_command(case)?;
    command
        .env("ROBOT_LOG_FORMAT", "json")
        .env("ROBOT_TEST_R4C_LEDGER_KEY", &key);
    save(case, "gateway_launch.json", &launch(&command))?;
    owned.gateway = Some(case.spawn("gateway", command)?);
    owned.client = Some(connect(&address).await?);
    let started = SystemClock.now().0;
    let until = started + 15_000_000_000;
    save(
        case,
        "request.json",
        &json!({"request_id":request_id,"key":key,"role":"maintainer","timeout_ms":5000,"client_timeout_ns":8_000_000_000_u64,"handler_grace_ms":500,"confirmation":request.confirmation,"started_ns":started,"until_ns":until}),
    )?;
    start_request(owned, request.clone())?;
    let first = await_request(owned, &mut capture, until).await?;
    let handler = oracle::handler(
        &first,
        Code::DeadlineExceeded,
        started + 5_500_000_000,
        until,
    )?;
    let mut last = "未形成原Stuck/Orphan".to_owned();
    let (deadline, chain, orphan, accepted_ns) = loop {
        require_running(owned.s0.as_mut().unwrap(), until, "原S0 Stuck")
            .map_err(|e| format!("{e}；末次判据：{last}"))?;
        require_running(owned.gateway.as_mut().unwrap(), until, "原gateway Orphan")?;
        capture.drain();
        let observed = (|| {
            let deadline = oracle::original_deadline(&case.logs("s0")?, &request_id)?;
            let chain = super::w4_latch_retry_exhaustion::validate_shared_chain(
                case,
                &capture,
                &request_id,
                deadline,
                false,
            )?;
            let orphan = oracle::ledger(&case.logs("gateway")?, &request_id, &key, false)?;
            Ok::<_, String>((deadline, chain, orphan))
        })();
        match observed {
            Ok((d, c, o)) => {
                let now = accept(until)?;
                break (d, c, o, now);
            }
            Err(error) => last = error,
        }
        if SystemClock.now().0 >= until {
            return Err(format!("原15s窗超限：{last}"));
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    if deadline < started + 5_000_000_000 || first.0 < deadline + 500_000_000 {
        return Err("原wire deadline与handler宽限顺序不符".into());
    }
    start_request(owned, request)?;
    let second = await_request(owned, &mut capture, until).await?;
    let duplicate = oracle::handler(&second, Code::Aborted, accepted_ns, until)?;
    let ledger = oracle::ledger(&case.logs("gateway")?, &request_id, &key, true)?;
    let duplicate_observed_ns = accept(until)?;
    let s0_shutdown = signal_wait(
        owned.s0.as_mut().unwrap(),
        11_000_000_000,
        Some(&mut capture),
    )
    .await?;
    capture.drain();
    let final_chain = super::w4_latch_retry_exhaustion::validate_shared_chain(
        case,
        &capture,
        &request_id,
        deadline,
        true,
    )?;
    let page = case.observation("s0")?;
    super::w4_latch_retry_exhaustion::validate_exit(Some(1), &page, 0)?;
    if s0_shutdown["code"] != 1 {
        return Err("原S0不是L退1".into());
    }
    oracle::files(case)?;
    let gateway_shutdown =
        signal_wait(owned.gateway.as_mut().unwrap(), 11_000_000_000, None).await?;
    if gateway_shutdown["code"] != 0 {
        return Err(format!("原gateway正常收尾失败：{gateway_shutdown}"));
    }
    let final_ledger = oracle::ledger(&case.logs("gateway")?, &request_id, &key, true)?;
    let evidence = json!({"sample_seq":sample,"request_id":request_id,"key":key,"started_ns":started,"original_wire_deadline_ns":deadline,"observation_deadline_ns":until,"accepted_ns":accepted_ns,"handler":handler,"original_chain":chain,"orphan":orphan,"duplicate":duplicate,"duplicate_observed_ns":duplicate_observed_ns,"ledger":ledger,"s0_shutdown":s0_shutdown,"final_chain":final_chain,"s0_final":super::runtime_supervision::public_snapshot(&page),"gateway_shutdown":gateway_shutdown,"final_ledger":final_ledger,"capture":capture.evidence(),"gateway_exit_does_not_prove_retirement":true});
    save(case, "chain.json", &evidence)?;
    Ok(evidence)
}

fn start_request(owned: &mut Owned, request: ClearEStopCommand) -> Result<(), String> {
    if owned.request.is_some() {
        return Err("原request task尚未join".into());
    }
    let mut client = owned.client.as_ref().ok_or("原tonic client缺失")?.clone();
    owned.request = Some(tokio::spawn(async move {
        let result = client
            .clear_e_stop(request)
            .await
            .map(tonic::Response::into_inner);
        (SystemClock.now().0, result)
    }));
    Ok(())
}
async fn await_request(
    owned: &mut Owned,
    capture: &mut TraceCapture,
    until: u64,
) -> Result<RequestResult, String> {
    loop {
        capture.drain();
        require_running(owned.s0.as_mut().unwrap(), until, "原handler等待期间S0")?;
        require_running(
            owned.gateway.as_mut().unwrap(),
            until,
            "原handler等待期间gateway",
        )?;
        if owned.request.as_ref().ok_or("原task缺失")?.is_finished() {
            let result = owned
                .request
                .take()
                .unwrap()
                .await
                .map_err(|e| e.to_string())?;
            accept(until)?;
            return Ok(result);
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}
async fn finish_request(owned: &mut Owned) -> Result<Value, String> {
    let Some(request) = owned.request.as_ref() else {
        return Ok(json!({"live_task":false}));
    };
    request.abort();
    let until = SystemClock.now().0 + 2_000_000_000;
    loop {
        if SystemClock.now().0 >= until {
            std::process::abort();
        }
        if owned.request.as_ref().unwrap().is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let result = owned.request.take().unwrap().await;
    Err(format!("失败收尾已abort且真join原请求task：{result:?}"))
}
async fn connect(address: &str) -> Result<RobotControlClient<Channel>, String> {
    let endpoint = Endpoint::from_shared(format!("http://{address}"))
        .map_err(|e| e.to_string())?
        .connect_timeout(Duration::from_millis(200))
        .timeout(Duration::from_secs(8));
    let until = SystemClock.now().0 + 4_000_000_000;
    loop {
        let result = endpoint.connect().await;
        accept(until)?;
        if let Ok(channel) = result {
            return Ok(RobotControlClient::new(channel));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
fn accept(until: u64) -> Result<u64, String> {
    let now = SystemClock.now().0;
    if now >= until {
        Err("成功接纳也必须在原绝对截止内".into())
    } else {
        Ok(now)
    }
}
async fn signal_wait(
    child: &mut Child,
    budget: u64,
    mut capture: Option<&mut TraceCapture>,
) -> Result<Value, String> {
    let pid = child.id().ok_or("原Child SIGTERM前已结束")?;
    let before = SystemClock.now().0;
    // SAFETY: PID来自本例仍持有的真实Child，只发送已授权的正常停止信号。
    let result = unsafe {
        libc::kill(
            i32::try_from(pid).map_err(|e| e.to_string())?,
            libc::SIGTERM,
        )
    };
    let after = SystemClock.now().0;
    if result != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let until = before + budget;
    loop {
        if let Some(c) = capture.as_mut() {
            c.drain();
        }
        let status = child.try_wait().map_err(|e| e.to_string())?;
        let now = accept(until)?;
        if let Some(status) = status {
            return Ok(
                json!({"pid":pid,"signal_before_ns":before,"signal_after_ns":after,"deadline_ns":until,"reaped_ns":now,"code":status.code()}),
            );
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}
async fn reap(child: &mut Option<Child>, role: &str) -> Result<Value, String> {
    let Some(child) = child.as_mut() else {
        return Ok(json!({"spawned":false}));
    };
    match child.try_wait() {
        Ok(Some(status)) => {
            return Ok(
                json!({"reaped":true,"status":status.to_string(),"at_ns":SystemClock.now().0}),
            );
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("原{role} Child状态未知：{error}");
            std::process::abort();
        }
    }
    let before = SystemClock.now().0;
    let killed = child.start_kill();
    let after = SystemClock.now().0;
    let until = before + 2_000_000_000;
    loop {
        let state = child.try_wait();
        let now = SystemClock.now().0;
        match state {
            Ok(Some(status)) => {
                return Err(format!(
                    "原{role}失败收尾真reap：kill={killed:?} before={before} after={after} reaped={now} within={} status={status}",
                    now < until
                ));
            }
            Ok(None) if now < until => tokio::time::sleep(Duration::from_millis(1)).await,
            other => {
                eprintln!("原{role}未回收禁止丢owner：{other:?}");
                std::process::abort();
            }
        }
    }
}
fn launch(c: &tokio::process::Command) -> Value {
    json!({"program":c.as_std().get_program().to_string_lossy(),"argv":c.as_std().get_args().map(|v|v.to_string_lossy().into_owned()).collect::<Vec<_>>(),"env":c.as_std().get_envs().map(|(k,v)|(k.to_string_lossy().into_owned(),v.map(|v|v.to_string_lossy().into_owned()))).collect::<std::collections::BTreeMap<_,_>>()})
}
fn save(case: &Case, name: &str, value: &Value) -> Result<(), String> {
    std::fs::write(
        case.directory.join(name),
        serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}
