//! R4b2：八次原接受重投耗尽，保留原 Cleanup、RR 与退出残件。

mod oracle;

use std::time::Duration;

use iceoryx2::prelude::{Node, NodeBuilder};
use iceoryx2::service::ipc::Service;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::{markers, msg};
use robot_s0::comm_fault_observation::ObservationSnapshot;
use robot_s0::comm_fault_trace::SampleMode;
use robot_s0::shutdown_mechanism::HandleKind;
use robot_s0::worker_stop::WorkerKind;
use robot_transport::iceoryx2::request_response::{Iceoryx2PendingResponse, Iceoryx2RrClient};
use serde_json::{Value, json};
use tokio::process::Child;

use super::Args;
use super::control::ControlPort;
use super::harness::{Case, wait_log};
use super::report::Report;
use super::sla::send_sample_request;
use super::w4_latch_crash::{TraceCapture, receive_with_closed_tail, require_running};

const CASE_FILTER: &str = "R4-T8-RESYNC-EXHAUSTED";
const CASE_NAME: &str = "ordinary_resync_eight_claimed_retries_stick_with_original_responsibility";
const OBSERVATION_NS: u64 = 15_000_000_000;
const REQUEST_NS: u64 = 5_000_000_000;
const EXIT_NS: u64 = 11_000_000_000;

pub(super) async fn run(args: &Args, report: &mut Report) -> Result<(), String> {
    if !cfg!(target_os = "linux") || args.case.as_deref() != Some(CASE_FILTER) {
        return Err("R4b2 仅接受 Linux mock 的精确入口".into());
    }
    let before = report.cases.len();
    report.record(
        CASE_NAME,
        CASE_FILTER,
        SystemClock.now().0,
        run_case(args).await,
    );
    if report.cases.len().checked_sub(before) != Some(1) {
        return Err("R4b2 未真实登记恰一个具名样例".into());
    }
    Ok(())
}

/// 任一 Result 出口均由外层先真 reap，再 join 控制线程，最后释放原端点。
struct Owned {
    child: Child,
    control: Option<ControlPort>,
    pending: Option<Iceoryx2PendingResponse<markers::ClearEStopResponse>>,
    client: Option<Iceoryx2RrClient<markers::ClearEStopRequest, markers::ClearEStopResponse>>,
    node: Option<Node<Service>>,
    replies: u32,
}

async fn run_case(args: &Args) -> Result<Value, String> {
    let case = Case::new(&args.evidence_dir, &args.bin_dir, CASE_NAME, 1)?;
    for path in [&case.latch, &case.directory.join("estop_latch.bak")] {
        std::fs::write(path, b"1\n").map_err(|error| error.to_string())?;
    }
    let mut command = case.command();
    command
        .args(["--notify-mode", "standalone"])
        .env("ROBOT_TEST_W4_FILE_FAULT", "ordinary-resync-exhausted")
        .env("ROBOT_LOG_FORMAT", "json")
        .env(
            "RUST_LOG",
            "warn,s0_daemon=info,robot_s0::ipc_runtime=info,robot_s0::rr_server_thread=info",
        );
    let launch = json!({"program":command.as_std().get_program().to_string_lossy(),
        "argv":command.as_std().get_args().map(|v|v.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        "env_changes":command.as_std().get_envs().map(|(k,v)|(k.to_string_lossy().into_owned(),v.map(|v|v.to_string_lossy().into_owned()))).collect::<std::collections::BTreeMap<_,_>>()});
    save_json(&case, "launch.json", &launch)?;
    let mut owned = Owned {
        child: case.spawn("s0", command)?,
        control: None,
        pending: None,
        client: None,
        node: None,
        replies: 0,
    };
    let mut capture = TraceCapture::open(&case);
    let result = match &mut capture {
        Ok(capture) => exercise(&case, &mut owned, capture).await,
        Err(error) => Err(error.clone()),
    };
    let cleanup = reap_original(&case, &mut owned, capture.as_mut().ok()).await;
    let reaped_at_ns = SystemClock.now().0;
    let control_cleanup = owned.control.as_mut().map(ControlPort::finish).transpose();
    let control_joined_at_ns = SystemClock.now().0;
    let final_trace = match &mut capture {
        Ok(capture) => {
            capture.drain();
            validate_capture(capture)
        }
        Err(error) => Err(error.clone()),
    };
    let final_state = final_state(&case, &mut owned);
    let capture_evidence = match &capture {
        Ok(capture) => capture.evidence(),
        Err(error) => json!({"open_error":error}),
    };
    let evidence = json!({"case":CASE_NAME,"directory":case.directory,"launch":launch,
        "result":result,"cleanup":cleanup,"reaped_at_ns":reaped_at_ns,
        "control_cleanup":control_cleanup,"control_joined_at_ns":control_joined_at_ns,
        "capture":capture_evidence,"final_trace":final_trace,"final_state":final_state,
        "fault_input":"原同目录O_PATH别名十次实际EBADF，原Unsynced一直保留"});
    save_json(&case, "result.json", &evidence)?;
    match (result, cleanup, control_cleanup, final_trace, final_state) {
        (Ok(_), Ok(()), Ok(_), Ok(_), Ok(_)) => Ok(evidence),
        (result, cleanup, control, trace, state) => Err(format!(
            "原场景={result:?}；Child收尾={cleanup:?}；控制线程={control:?}；原trace={trace:?}；终态={state:?}；证据={}",
            case.directory.display()
        )),
    }
}

async fn exercise(
    case: &Case,
    owned: &mut Owned,
    capture: &mut TraceCapture,
) -> Result<Value, String> {
    owned.control = Some(ControlPort::spawn(&case.robot_id)?);
    let port = owned.control.as_mut().ok_or("原ControlPort安装后丢失")?;
    port.wait_ready()?;
    wait_log(
        case,
        &mut owned.child,
        "s0",
        "boot_ready",
        Duration::from_secs(12),
    )
    .await?;
    let (sample, _) = send_sample_request(
        port,
        case,
        &mut owned.child,
        capture.page(),
        SampleMode::ArmInflight,
    )
    .await?;
    capture.set_sample(sample);
    capture.drain();
    let (request_id, started, deadline) = send_clear(case, owned)?;
    let until = started.saturating_add(OBSERVATION_NS);
    let mut expired_pending = None;
    let mut last_boundary = "原writer尚未形成完整scope".to_owned();
    let (chain, facts, accepted_at_ns) = loop {
        require_running(&mut owned.child, until, "原接受八次后Stuck")
            .map_err(|error| format!("{error}；末次实际判据：{last_boundary}"))?;
        capture.drain();
        let receive_started_at = SystemClock.now().0;
        capture_reply(case, owned)?;
        let observed_at = SystemClock.now().0;
        if receive_started_at >= deadline && expired_pending.is_none() {
            let pending = owned.pending.as_ref().ok_or("原Pending丢失")?;
            expired_pending = Some(
                json!({"receive_started_at_ns":receive_started_at,"observed_at_ns":observed_at,"deadline_ns":deadline,
                "request_id":request_id,"receive":"None","status":format!("{:?}",pending.connection_state())}),
            );
        }
        let evidence = validate_capture(capture).and_then(|chain| {
            let facts = oracle::validate_facts(&case.logs("s0")?, &request_id, deadline, false)?;
            match_original_transaction(&chain, &facts)?;
            Ok((chain, facts))
        });
        match evidence {
            Ok((chain, facts)) => {
                let accepted_at = accept_observation(SystemClock.now().0, until)?;
                break (chain, facts, accepted_at);
            }
            Err(error) => last_boundary = error,
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    let expired_pending = expired_pending.ok_or("未在原请求deadline之后真实采样同一Pending")?;
    let shutdown = signal_and_wait(case, owned, capture).await?;
    let final_facts = oracle::validate_facts(&case.logs("s0")?, &request_id, deadline, true)?;
    match_original_transaction(&chain, &final_facts)?;
    Ok(
        json!({"sample_seq":sample,"request_id":request_id,"request_started_ns":started,
        "request_deadline_ns":deadline,"observation_deadline_ns":until,"accepted_at_ns":accepted_at_ns,
        "expired_pending":expired_pending,"chain":chain,"facts_before_shutdown":facts,
        "shutdown":shutdown,"facts_after_shutdown":final_facts}),
    )
}

fn match_original_transaction(chain: &Value, facts: &Value) -> Result<(), String> {
    for field in ["txn_id", "expected_epoch"] {
        if chain.get(field).is_none() || chain.get(field) != facts.get(field) {
            return Err(format!("原writer与原责任日志完整事务字段不符：{field}"));
        }
    }
    Ok(())
}

fn send_clear(case: &Case, owned: &mut Owned) -> Result<(String, u64, u64), String> {
    owned.node = Some(
        NodeBuilder::new()
            .create::<Service>()
            .map_err(|error| error.to_string())?,
    );
    owned.client = Some(
        robot_transport::iceoryx2::rr_client_with::<
            markers::ClearEStopRequest,
            markers::ClearEStopResponse,
        >(
            owned.node.as_ref().ok_or("原Node丢失")?,
            &format!("/{}/control/clear_estop", case.robot_id),
            &robot_transport::iceoryx2::RrProfile::S0_CONTROL,
        )
        .map_err(|error| error.to_string())?,
    );
    let request_id = format!("{}-clear", case.robot_id);
    let started = SystemClock.now().0;
    let deadline = started.saturating_add(REQUEST_NS);
    let body = robot_msg::build_helpers::build_clear_estop_request(
        &request_id,
        &request_id,
        "R4b2",
        msg::RpcRole::Maintainer,
        "确认真实停止",
        "核原重投耗尽责任",
        false,
        deadline,
        started,
    );
    std::fs::write(case.directory.join("clear_request.bin"), &body)
        .map_err(|error| error.to_string())?;
    owned.pending = Some(
        owned
            .client
            .as_ref()
            .ok_or("原client丢失")?
            .send_typed(&body)
            .map_err(|error| format!("{error:?}"))?,
    );
    if let Some(issue) = owned
        .pending
        .as_ref()
        .ok_or("原Pending丢失")?
        .delivery_issue()
    {
        return Err(format!("原请求已入队诊断，原Pending保到真reap：{issue:?}"));
    }
    Ok((request_id, started, deadline))
}

fn accept_observation(observed_at: u64, until: u64) -> Result<u64, String> {
    if observed_at >= until {
        return Err(format!("原观察超限：{observed_at}>={until}"));
    }
    Ok(observed_at)
}

fn capture_reply(case: &Case, owned: &mut Owned) -> Result<(), String> {
    if let Some(pending) = owned.pending.as_ref()
        && let Some(bytes) = receive_with_closed_tail(pending)?
    {
        std::fs::write(
            case.directory
                .join(format!("unexpected_response_{}.bin", owned.replies)),
            bytes,
        )
        .map_err(|error| error.to_string())?;
        owned.replies = owned.replies.checked_add(1).ok_or("原RR响应计数溢出")?;
    }
    if owned.replies != 0 {
        return Err(format!("原未退休RR出现{}条实际终态", owned.replies));
    }
    Ok(())
}

async fn signal_and_wait(
    case: &Case,
    owned: &mut Owned,
    capture: &mut TraceCapture,
) -> Result<Value, String> {
    let pid = owned.child.id().ok_or("原Child在SIGTERM前已结束")?;
    let signal_before = SystemClock.now().0;
    // SAFETY: pid 来自本例仍持有的原Child，仅发送授权的SIGTERM。
    let signal_result = unsafe {
        libc::kill(
            i32::try_from(pid).map_err(|error| error.to_string())?,
            libc::SIGTERM,
        )
    };
    let signal_after = SystemClock.now().0;
    if signal_result != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let until = signal_before.saturating_add(EXIT_NS);
    loop {
        capture.drain();
        capture_reply(case, owned)?;
        if let Some(status) = owned.child.try_wait().map_err(|error| error.to_string())? {
            let observed_at = accept_observation(SystemClock.now().0, until)?;
            if status.code() != Some(1) {
                return Err(format!("原L失败退出应为1：{status}"));
            }
            return Ok(
                json!({"pid":pid,"signal_before_ns":signal_before,"signal_after_ns":signal_after,
                "exit_observation_deadline_ns":until,"reaped_observed_at_ns":observed_at,"exit_code":status.code()}),
            );
        }
        accept_observation(SystemClock.now().0, until)?;
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// 失败仍保持全部原件到真reap；强杀只作清理，绝不转成通过。
async fn reap_original(
    case: &Case,
    owned: &mut Owned,
    mut capture: Option<&mut TraceCapture>,
) -> Result<(), String> {
    let already_reaped = match owned.child.try_wait() {
        Ok(status) => status.is_some(),
        Err(error) => {
            eprintln!("R4b2父硬失败：无法确认原Child状态：{error}");
            std::process::abort();
        }
    };
    let mut error = None;
    if !already_reaped {
        error = Some("场景未自然完成，实际强杀原Child收尾".to_owned());
        if let Err(kill) = owned.child.start_kill() {
            error = Some(format!("原Child强杀失败：{kill}"));
        }
        let until = SystemClock.now().0.saturating_add(2_000_000_000);
        loop {
            if let Some(capture) = capture.as_mut() {
                capture.drain();
            }
            if let Err(reply) = capture_reply(case, owned) {
                error.get_or_insert(reply);
            }
            match owned.child.try_wait() {
                Ok(Some(_)) => {
                    if let Err(late) = accept_observation(SystemClock.now().0, until) {
                        error = Some(format!("原Child已真reap但超过原2s清理边界：{late}"));
                    }
                    break;
                }
                Ok(None) if SystemClock.now().0 < until => {}
                other => {
                    eprintln!("R4b2父硬失败：无法在原2s真reap原Child：{other:?}");
                    std::process::abort();
                }
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    if let Some(capture) = capture.as_mut() {
        capture.drain();
    }
    if let Err(reply) = capture_reply(case, owned) {
        error.get_or_insert(reply);
    }
    error.map_or(Ok(()), Err)
}

fn validate_capture(capture: &TraceCapture) -> Result<Value, String> {
    oracle::validate_chain(&capture.commands()?)
}

fn final_state(case: &Case, owned: &mut Owned) -> Result<Value, String> {
    let code = owned
        .child
        .try_wait()
        .map_err(|error| error.to_string())?
        .ok_or("原Child尚未reap")?
        .code();
    let page = case.observation("s0")?;
    validate_exit(code, &page, owned.replies)?;
    let exit_facts = oracle::validate_exit_facts(&case.logs("s0")?)?;
    for path in [&case.latch, &case.directory.join("estop_latch.bak")] {
        if std::fs::read(path).map_err(|error| error.to_string())? != b"0\n" {
            return Err("原文件未保0/0".into());
        }
    }
    match std::fs::symlink_metadata(case.directory.join("estop_latch.clear_pending")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        other => return Err(format!("原marker缺失事实不成立：{other:?}")),
    }
    Ok(
        json!({"exit_code":code,"clear_reply_count":owned.replies,"exit_facts":exit_facts,
        "observation":snapshot(&page),"latch_pair":[0,0],"marker_present":false,
        "file_facts_only":true,"retirement_proven":false,"durability_proven":false}),
    )
}

fn validate_exit(
    code: Option<i32>,
    page: &ObservationSnapshot,
    replies: u32,
) -> Result<(), String> {
    let residual = [
        HandleKind::ClearRr,
        HandleKind::Executor,
        HandleKind::Ipc,
        HandleKind::Watchdog,
        HandleKind::LatchWriter,
    ]
    .iter()
    .fold(0_u64, |bits, kind| bits | u64::from(kind.bit()));
    let stops = [
        WorkerKind::ClearEStopRr,
        WorkerKind::SetEnabledRr,
        WorkerKind::Pub,
        WorkerKind::Io,
    ]
    .iter()
    .fold(0_u64, |bits, kind| bits | (1_u64 << *kind as u8));
    let deadline = page.shutdown_deadline_ns;
    if code != Some(1)
        || replies != 0
        || deadline < 10_000_000_000
        || !(deadline - 250_000_000..deadline).contains(&page.before_process_exit_ns)
        || page.runtime_armed_ns == 0
        || page.runtime_armed_ns >= page.before_process_exit_ns
        || page.residual_bitmap != residual
        || page.exit_residual_bitmap != residual
        || page.stop_bitmap != stops
        || page.refused_stop_bitmap != 0
        || page.writer_exited_at_ns != 0
        || page.writer_joined_observed_at_ns != 0
        || page.fired_ns != 0
        || page.fired_checkpoint != 0
        || page.fired_reason != 0
        || page.fired_deadline_ns != 0
        || page.fired_candidates_bitmap != 0
        || page.supervisor_exit_ns != 0
    {
        return Err(format!(
            "原L/五残件/未退休退出不符：code={code:?}，replies={replies}，{page:?}"
        ));
    }
    Ok(())
}

fn snapshot(page: &ObservationSnapshot) -> Value {
    json!({"version":page.version,"runtime_armed_ns":page.runtime_armed_ns,"shutdown_deadline_ns":page.shutdown_deadline_ns,
        "derived_l_ns":page.shutdown_deadline_ns.checked_sub(250_000_000),"before_process_exit_ns":page.before_process_exit_ns,
        "residual_bitmap":page.residual_bitmap,"exit_residual_bitmap":page.exit_residual_bitmap,"stop_bitmap":page.stop_bitmap,
        "refused_stop_bitmap":page.refused_stop_bitmap,"writer_exited_at_ns":page.writer_exited_at_ns,
        "writer_joined_observed_at_ns":page.writer_joined_observed_at_ns,"fired_ns":page.fired_ns,"fired_checkpoint":page.fired_checkpoint,
        "fired_reason":page.fired_reason,"fired_deadline_ns":page.fired_deadline_ns,"fired_candidates_bitmap":page.fired_candidates_bitmap,
        "supervisor_exit_ns":page.supervisor_exit_ns})
}

fn save_json(case: &Case, name: &str, value: &Value) -> Result<(), String> {
    std::fs::write(
        case.directory.join(name),
        serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhaustion_rejects_acceptance_at_or_after_original_deadline() {
        assert_eq!(accept_observation(99, 100), Ok(99));
        assert!(accept_observation(100, 100).is_err());
        assert!(accept_observation(101, 100).is_err());
    }

    #[test]
    fn exhaustion_rejects_unrelated_writer_and_coordinator_transactions() {
        let chain = json!({"txn_id":"123456789012345678901234567890","expected_epoch":7});
        assert!(match_original_transaction(&chain, &chain).is_ok());
        for facts in [
            json!({}),
            json!({"txn_id":"2","expected_epoch":7}),
            json!({"txn_id":"123456789012345678901234567890","expected_epoch":8}),
        ] {
            assert!(match_original_transaction(&chain, &facts).is_err());
        }
    }

    /// 仅供出口判据反例；合成页字段不作为进程退出事实。
    fn exit_input() -> ObservationSnapshot {
        static NEXT_PAGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT_PAGE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "r4b2-exit-oracle-{}-{sequence}.page",
            std::process::id()
        ));
        let page = robot_s0::comm_fault_observation::ObservationPage::create(&path).unwrap();
        let mut snapshot = page.snapshot();
        drop(page);
        std::fs::remove_file(path).unwrap();
        snapshot.runtime_armed_ns = 1;
        snapshot.shutdown_deadline_ns = 11_000_000_000;
        snapshot.before_process_exit_ns = 10_750_000_000;
        snapshot.residual_bitmap = [
            HandleKind::ClearRr,
            HandleKind::Executor,
            HandleKind::Ipc,
            HandleKind::Watchdog,
            HandleKind::LatchWriter,
        ]
        .iter()
        .fold(0_u64, |bits, kind| bits | u64::from(kind.bit()));
        snapshot.exit_residual_bitmap = snapshot.residual_bitmap;
        snapshot.stop_bitmap = [
            WorkerKind::ClearEStopRr,
            WorkerKind::SetEnabledRr,
            WorkerKind::Pub,
            WorkerKind::Io,
        ]
        .iter()
        .fold(0_u64, |bits, kind| bits | (1_u64 << *kind as u8));
        snapshot
    }

    #[test]
    fn exhaustion_exit_oracle_rejects_wrong_residual_stop_retirement_or_deadline() {
        let valid = exit_input();
        assert!(validate_exit(Some(1), &valid, 0).is_ok());
        for code in [None, Some(0), Some(90)] {
            assert!(validate_exit(code, &valid, 0).is_err());
        }
        assert!(validate_exit(Some(1), &valid, 1).is_err());
        let mut wrong = valid;
        wrong.residual_bitmap ^= u64::from(HandleKind::LatchWriter.bit());
        assert!(validate_exit(Some(1), &wrong, 0).is_err());
        wrong = valid;
        wrong.exit_residual_bitmap = 0;
        assert!(validate_exit(Some(1), &wrong, 0).is_err());
        wrong = valid;
        wrong.stop_bitmap = 0xff;
        assert!(validate_exit(Some(1), &wrong, 0).is_err());
        wrong = valid;
        wrong.writer_exited_at_ns = 1;
        assert!(validate_exit(Some(1), &wrong, 0).is_err());
        wrong = valid;
        wrong.writer_joined_observed_at_ns = 1;
        assert!(validate_exit(Some(1), &wrong, 0).is_err());
        wrong = valid;
        wrong.fired_checkpoint = 4;
        assert!(validate_exit(Some(1), &wrong, 0).is_err());
        for boundary in [valid.before_process_exit_ns - 1, valid.shutdown_deadline_ns] {
            wrong = valid;
            wrong.before_process_exit_ns = boundary;
            assert!(validate_exit(Some(1), &wrong, 0).is_err());
        }
    }
}
