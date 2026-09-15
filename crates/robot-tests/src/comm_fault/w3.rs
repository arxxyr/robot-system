//! W3 真实 daemon：事务清理、运行期 `FileStep` 执法及有界文件失败下的活性。
//! 所有场景复用本轮 artifact 目录与原 Case；不连接硬件或主机 systemd unit。

use super::{
    Args,
    harness::{Case, stop, wait_exit, wait_log},
    report::Report,
};
use iceoryx2::prelude::{Node, NodeBuilder};
use iceoryx2::service::ipc::Service;
use iceoryx2::strict::StrictChannelStatus;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::markers;
use robot_s0::comm_fault_observation::supervision::CurrentHealthObservation;
use robot_transport::Publisher;
use robot_transport::iceoryx2::Iceoryx2Publisher;
use robot_transport::iceoryx2::request_response::{Iceoryx2PendingResponse, Iceoryx2RrClient};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::process::Child;

pub(super) async fn run(args: &Args, report: &mut Report) -> Result<(), String> {
    if args.case.as_deref().is_some_and(|case| case != "W3") {
        return Ok(());
    }
    for mode in [Mode::Clear, Mode::FileStep, Mode::BoundedIoFailure] {
        let started = SystemClock.now().0;
        report.record(mode.label(), "W3", started, run_case(args, mode).await);
    }
    Ok(())
}

pub(super) async fn run_health(args: &Args, report: &mut Report) -> Result<(), String> {
    if args.phase != super::Phase::Mock || args.case.as_deref() != Some("R4C-PERSIST-HEALTH") {
        return Err("R4C-PERSIST-HEALTH 只接受 mock phase 的精确单例筛选".into());
    }
    let started = SystemClock.now().0;
    report.record(
        Mode::BoundedIoFailure.label(),
        "R4C-PERSIST-HEALTH",
        started,
        run_case(args, Mode::BoundedIoFailure).await,
    );
    Ok(())
}
#[derive(Clone, Copy)]
enum Mode {
    Clear,
    FileStep,
    BoundedIoFailure,
}
impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Clear => "w3_clear_retired_then_clean_shutdown",
            Self::FileStep => "w3_real_file_step_runtime_exit",
            Self::BoundedIoFailure => "w3_bounded_io_failure_keeps_watchdog_and_recovers",
        }
    }
}

/// 原请求与注入端点由真实 `Child` 的外层持有，异常返回也不能提前析构。
#[derive(Default)]
struct Owned {
    pending: Option<Iceoryx2PendingResponse<markers::ClearEStopResponse>>,
    client: Option<Iceoryx2RrClient<markers::ClearEStopRequest, markers::ClearEStopResponse>>,
    publisher: Option<Iceoryx2Publisher<markers::EStopEvent>>,
    node: Option<Node<Service>>,
}

async fn run_case(args: &Args, mode: Mode) -> Result<Value, String> {
    struct SocketPath(std::path::PathBuf);
    impl Drop for SocketPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let case = Case::new(&args.evidence_dir, &args.bin_dir, mode.label(), 1)?;
    if matches!(mode, Mode::Clear | Mode::FileStep) {
        for path in [&case.latch, &case.directory.join("estop_latch.bak")] {
            std::fs::write(path, b"1\n").map_err(|e| e.to_string())?;
        }
    }
    // sockaddr_un 有路径长度上限；绑定成功后只清理由本场景实际创建的 socket。
    let socket_path = std::env::temp_dir().join(format!("w3-notify-{}.sock", case.robot_id));
    let notify = tokio::net::UnixDatagram::bind(&socket_path).map_err(|e| e.to_string())?;
    let _socket_path = SocketPath(socket_path.clone());
    let mut command = case.command();
    command
        .env("NOTIFY_SOCKET", &socket_path)
        .env("WATCHDOG_USEC", "1000000")
        .env("ROBOT_LOG_FORMAT", "json");
    if matches!(mode, Mode::FileStep) {
        command.env("ROBOT_TEST_WRITER_PAUSE", "SyncMarkerFile:4000");
    }
    let mut child = case.spawn("s0", command)?;
    let mut owned = Owned::default();
    let result = exercise(&case, &mut child, &notify, mode, &mut owned).await;
    // 包含所有失败出口：原 Child、RR 和注入原件均保持到真正回收。
    let (cleanup, cleanup_error) = reap_owned(&mut child).await;
    let saved = save_json(&case, "child_cleanup.json", &cleanup);
    let cleanup_error = match (cleanup_error, saved) {
        (Some(first), Err(next)) => Some(format!("{first}；保存收尾失败：{next}")),
        (Some(first), Ok(())) => Some(first),
        (None, Err(error)) => Some(error),
        (None, Ok(())) => None,
    };
    match (result, cleanup_error) {
        (Err(first), Some(cleanup)) => Err(format!("{first}；收尾：{cleanup}")),
        (Err(first), None) => Err(first),
        (Ok(_), Some(cleanup)) => Err(cleanup),
        (Ok(mut evidence), None) => {
            evidence["child_cleanup"] = cleanup;
            Ok(evidence)
        }
    }
}

async fn reap_owned(child: &mut Child) -> (Value, Option<String>) {
    let first_error = match child.try_wait() {
        Ok(Some(status)) => {
            return (
                json!({"reaped":true,"forced":false,"status":status.to_string(),"observed_ns":SystemClock.now().0}),
                None,
            );
        }
        Ok(None) => None,
        Err(error) => Some(format!("原 Child try_wait 失败：{error}")),
    };
    let kill_started_ns = SystemClock.now().0;
    let deadline_ns = kill_started_ns + 2_000_000_000;
    let kill_result = child.start_kill().map_err(|error| error.to_string());
    let kill_return_ns = SystemClock.now().0;
    let remaining = Duration::from_nanos(deadline_ns.saturating_sub(kill_return_ns));
    let waited = tokio::time::timeout(remaining, child.wait()).await;
    let observed_ns = SystemClock.now().0;
    // 原请求仍在外层 Owned 中；未真 reap 不能沿错误返回提前析构它们。
    let Ok(Ok(status)) = waited else {
        eprintln!("爸爸，W3 原 Child 无法在 kill 后真实回收：{waited:?}");
        std::process::abort();
    };
    let evidence = json!({"reaped":true,"forced":true,"status":status.to_string(),
        "kill_started_ns":kill_started_ns,"kill_return_ns":kill_return_ns,
        "kill_result":format!("{kill_result:?}"),"deadline_ns":deadline_ns,"observed_ns":observed_ns});
    let error = match (first_error, kill_result, observed_ns < deadline_ns) {
        (Some(first), _, _) => first,
        (None, Err(error), _) => format!("原 Child kill 失败但已回收：{error}"),
        (None, Ok(()), false) => "原 Child 已真回收，但观察成功晚于原 2 秒收尾界".into(),
        (None, Ok(()), true) => "场景未自行回收原 Child，已强制终止并真 reap".into(),
    };
    (evidence, Some(error))
}

#[allow(
    clippy::too_many_lines,
    reason = "同一真实进程按三种场景执行文件、原截止和健康断言，保留收尾前的完整证据链"
)]
async fn exercise(
    case: &Case,
    child: &mut Child,
    notify: &tokio::net::UnixDatagram,
    mode: Mode,
    owned: &mut Owned,
) -> Result<Value, String> {
    wait_log(case, child, "s0", "boot_ready", Duration::from_secs(12)).await?;
    let until = SystemClock.now().0 + 1_000_000_000;
    loop {
        let page = case.observation("s0")?;
        if SystemClock.now().0 >= until {
            return Err("真实READY后未完成Runtime接力".into());
        }
        if page.runtime_armed_ns >= page.ready_ns
            && page.ready_ns != 0
            && page.supervisor_exit_ns == 0
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    owned.node = Some(
        NodeBuilder::new()
            .create::<Service>()
            .map_err(|e| e.to_string())?,
    );
    if matches!(mode, Mode::BoundedIoFailure) {
        return exercise_bounded(case, child, notify, owned).await;
    }
    let node = owned.node.as_ref().ok_or("原Node安装后丢失")?;
    let profile = robot_transport::iceoryx2::RrProfile::S0_CONTROL;
    owned.client = Some(
        robot_transport::iceoryx2::rr_client_with::<
            markers::ClearEStopRequest,
            markers::ClearEStopResponse,
        >(
            node,
            &format!("/{}/control/clear_estop", case.robot_id),
            &profile,
        )
        .map_err(|e| e.to_string())?,
    );
    let client = owned.client.as_ref().ok_or("原Clear client安装后丢失")?;
    let started = SystemClock.now().0;
    let body = robot_msg::build_helpers::build_clear_estop_request(
        "w3-clear",
        "w3-clear",
        "w3",
        robot_msg::msg::RpcRole::Maintainer,
        "确认已停止",
        "真实软件Clear退休",
        false,
        started + 5_000_000_000,
        started,
    );
    owned.pending = Some(client.send_typed(&body).map_err(|e| format!("{e:?}"))?);
    let pending = owned.pending.as_ref().ok_or("原Clear pending安装后丢失")?;
    // issue 属于已真实入队的原请求；先保原 Pending，再判本例失败。
    if let Some(issue) = pending.delivery_issue() {
        return Err(format!("原Clear已入队但附带投递问题：{issue:?}"));
    }
    if matches!(mode, Mode::FileStep) {
        wait_log(
            case,
            child,
            "s0",
            "writer_pause_stage=SyncMarkerFile",
            Duration::from_secs(1),
        )
        .await?;
        let code = wait_exit(child, Duration::from_secs(4)).await?;
        let page = case.observation("s0")?;
        let elapsed = page.fired_ns.saturating_sub(started);
        if code != 90
            || page.fired_reason != 1
            || !(2_000_000_000..=2_500_000_000).contains(&elapsed)
        {
            return Err(format!(
                "真实FileStep未按运行预算退出：code={code} elapsed={elapsed} page={page:?}"
            ));
        }
        if receive_with_closed_tail(pending)?.is_some() {
            return Err("卡在Prepare的原请求不应冒充退休回复".into());
        }
        return Ok(
            json!({"exit_code":code,"runtime_reason":page.fired_reason,"request_to_fire_ns":elapsed,"log_directory":case.directory}),
        );
    }
    let response = loop {
        if let Some(bytes) = receive_with_closed_tail(pending)? {
            break bytes;
        }
        if SystemClock.now().0 >= started + 3_000_000_000 {
            return Err("真实Clear退休回复超时".into());
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    let decoded = flatbuffers::root::<robot_msg::msg::ClearEStopResponse>(&response)
        .map_err(|e| e.to_string())?;
    if decoded.request_id() != Some("w3-clear")
        || decoded.status() != robot_msg::msg::RpcStatus::Ok
        || !decoded.retired()
        || decoded.effect() != robot_msg::msg::SideEffect::Committed
    {
        return Err(format!("真实typed退休响应不符：{decoded:?}"));
    }
    stop(child).await?;
    if pair(case)? != (b"0\n".to_vec(), b"0\n".to_vec()) {
        return Err("清洁关闭改写了已清除的双文件".into());
    }
    let page = case.observation("s0")?;
    if page.fired_checkpoint != 0 || page.residual_bitmap != 0 {
        return Err(format!("真实join关闭仍有监督/残余：{page:?}"));
    }
    Ok(
        json!({"exit_code":0,"typed_retired":true,"effect":"Committed","latch_pair":[0,0],"log_directory":case.directory}),
    )
}

const PERSIST_CLEAR_ID: &str = "w3-persist-pending";
const PERSIST_PENDING_BIT: u64 = 1;

struct FailureWindow {
    health: CurrentHealthObservation,
    watchdog_count: u64,
    rename_error: Value,
    response: Value,
    clear_sent_ns: u64,
    response_observed_ns: u64,
}

async fn exercise_bounded(
    case: &Case,
    child: &mut Child,
    notify: &tokio::net::UnixDatagram,
    owned: &mut Owned,
) -> Result<Value, String> {
    let started_ns = inject_bounded_failure(case, notify, owned)?;
    // 新请求必须在原存活窗中退休；不从健康降级或请求发送时重新起算窗口。
    let end_ns = started_ns + 3_200_000_000;
    let failure = observe_failure(case, child, notify, owned, started_ns, end_ns).await?;
    let before_removal = case.observation("s0")?;
    let health = before_removal.supervision.health;
    if !before_removal.obligation_overdue
        || !health.is_some_and(|value| pending_health(value, started_ns))
    {
        return Err(format!(
            "障碍仍在时丢失当前 PersistPending：{before_removal:?}"
        ));
    }
    ensure_alive(child)?;
    std::fs::remove_file(case.latch.join("obstacle")).map_err(|e| e.to_string())?;
    std::fs::remove_dir(&case.latch).map_err(|e| e.to_string())?;
    let removed_ns = SystemClock.now().0;
    let recovery_deadline_ns = removed_ns + 2_000_000_000;
    let recovered = loop {
        ensure_alive(child)?;
        let page = case.observation("s0")?;
        let settled_pair = pair(case) == Ok((b"1\n".to_vec(), b"1\n".to_vec()));
        let observed_ns = SystemClock.now().0;
        if observed_ns >= recovery_deadline_ns {
            return Err(format!(
                "撤障后未在原 2 秒窗内结清义务并清除当前健康：{page:?}"
            ));
        }
        if page.fired_checkpoint != 0 || !page.obligation_overdue {
            return Err(format!("恢复期间错误执法或重置历史沿：{page:?}"));
        }
        if let Some(current) = page.supervision.health
            && settled_pair
            && recovered_health(current, failure.health, removed_ns)
        {
            break json!({"health":health_json(current),"sticky_overdue":page.obligation_overdue,
                "pair":[1,1],"observed_ns":observed_ns});
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    save_json(case, "health_recovered.json", &recovered)?;
    // 恢复事实取自停止前的原 IPC report；不把停止期间的新健康帧拼入恢复证明。
    let stop_started_ns = SystemClock.now().0;
    stop(child).await?;
    let stop_observed_ns = SystemClock.now().0;
    if stop_observed_ns >= stop_started_ns + 5_000_000_000 {
        return Err("原 Child 正常关闭已完成，但成功观察晚于原 5 秒界".into());
    }
    let page = case.observation("s0")?;
    if !page.obligation_overdue || page.residual_bitmap != 0 || page.fired_checkpoint != 0 {
        return Err(format!("恢复关闭不得重置历史沿或留下监督/残余：{page:?}"));
    }
    if !case.logs("s0")?.contains("控制运行期健康降级") {
        return Err("未看到真实 IPC 健康 report 输出".into());
    }
    Ok(
        json!({"exit_code":0,"failure_started_ns":started_ns,"failure_window_end_ns":end_ns,
        "watchdog_messages_during_io_failure":failure.watchdog_count,"rename_error":failure.rename_error,
        "degraded_health":health_json(failure.health),"clear_sent_ns":failure.clear_sent_ns,
        "clear_response_observed_ns":failure.response_observed_ns,"clear_response":failure.response,
        "obstacle_removed_ns":removed_ns,"recovery_deadline_ns":recovery_deadline_ns,
        "recovered":recovered,"sticky_overdue":true,"stop_started_ns":stop_started_ns,
        "stop_observed_ns":stop_observed_ns,"log_directory":case.directory}),
    )
}

fn inject_bounded_failure(
    case: &Case,
    notify: &tokio::net::UnixDatagram,
    owned: &mut Owned,
) -> Result<u64, String> {
    let node = owned.node.as_ref().ok_or("原 Node 安装后丢失")?;
    owned.publisher = Some(
        robot_transport::iceoryx2::publisher::<markers::EStopEvent>(
            node,
            &format!("/{}/events/estop", case.robot_id),
            robot_transport::iceoryx2::TopicClass::SafetyEvent,
        )
        .map_err(|e| e.to_string())?,
    );
    owned.client = Some(
        robot_transport::iceoryx2::rr_client_with::<
            markers::ClearEStopRequest,
            markers::ClearEStopResponse,
        >(
            node,
            &format!("/{}/control/clear_estop", case.robot_id),
            &robot_transport::iceoryx2::RrProfile::S0_CONTROL,
        )
        .map_err(|e| e.to_string())?,
    );
    // Store 已在 daemon 持锁打开；只对原独立临时 latch 制造真实 rename 障碍。
    std::fs::remove_file(&case.latch).map_err(|e| e.to_string())?;
    std::fs::create_dir(&case.latch).map_err(|e| e.to_string())?;
    std::fs::write(case.latch.join("obstacle"), "真实文件失败".as_bytes())
        .map_err(|e| e.to_string())?;
    let _ = drain_watchdog(notify)?;
    let started_ns = SystemClock.now().0;
    let body = robot_msg::build_helpers::build_estop_event(
        "w3-file-failure",
        started_ns,
        "software-test",
        true,
    );
    let publisher = owned
        .publisher
        .as_mut()
        .ok_or("原注入 publisher 安装后丢失")?;
    let mut sample = publisher
        .loan(1024 + body.len())
        .map_err(|e| e.to_string())?;
    sample.set_seq(1);
    sample.set_source_id(99);
    sample.set_stamp_ns(started_ns);
    sample.set_body_bytes(&body);
    sample.publish().map_err(|e| e.to_string())?;
    Ok(started_ns)
}

async fn observe_failure(
    case: &Case,
    child: &mut Child,
    notify: &tokio::net::UnixDatagram,
    owned: &mut Owned,
    started_ns: u64,
    end_ns: u64,
) -> Result<FailureWindow, String> {
    let mut health = None;
    let mut rename_error = None;
    let mut clear_sent_ns = 0;
    let mut response_observed_ns = 0;
    let mut response = None;
    let mut watchdog_count = 0;
    while SystemClock.now().0 < end_ns {
        ensure_alive(child)?;
        watchdog_count += drain_watchdog(notify)?;
        let page = case.observation("s0")?;
        if page.fired_checkpoint != 0 || page.supervisor_exit_ns != 0 {
            return Err(format!("单步未卡住却错误执法或丢失监督：{page:?}"));
        }
        if health.is_none()
            && page.obligation_overdue
            && let Some(current) = page.supervision.health
            && pending_health(current, started_ns)
        {
            health = Some(current);
            save_json(
                case,
                "health_pending.json",
                &json!({"health":health_json(current),
                "sticky_overdue":page.obligation_overdue,"observed_ns":SystemClock.now().0}),
            )?;
            let error = find_rename_error(&case.logs("s0")?)?
                .ok_or("已降级但未见原 RenamePrimary/Operation 的真实 errno")?;
            save_json(case, "rename_stage_error.json", &error)?;
            rename_error = Some(error);
            if SystemClock.now().0 >= end_ns {
                return Err("新 Clear 尚未发送就已超过原 3.2 秒存活窗".into());
            }
            clear_sent_ns = send_pending_clear(case, owned)?;
        }
        if response.is_none()
            && let Some(pending) = owned.pending.as_ref()
            && let Some(bytes) = receive_with_closed_tail(pending)?
        {
            response_observed_ns = SystemClock.now().0;
            std::fs::write(case.directory.join("persist_pending.response.bin"), &bytes)
                .map_err(|e| e.to_string())?;
            if response_observed_ns >= end_ns {
                return Err("新 Clear 原退休回复成功观察已超过原 3.2 秒窗".into());
            }
            let decoded = validate_pending_response(&bytes)?;
            save_json(case, "persist_pending.response.json", &decoded)?;
            response = Some(decoded);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    ensure_alive(child)?;
    if watchdog_count < 4 {
        return Err(format!(
            "原 3.2 秒文件失败窗未持续收到看门狗：{watchdog_count}"
        ));
    }
    Ok(FailureWindow {
        health: health.ok_or("原 3.2 秒窗未观察完整当前 PersistPending 与独立 sticky")?,
        watchdog_count,
        rename_error: rename_error.ok_or("未记录原真实 rename errno")?,
        response: response.ok_or("新 Clear 未在原 3.2 秒窗内真实返回 PersistPending")?,
        clear_sent_ns,
        response_observed_ns,
    })
}

fn send_pending_clear(case: &Case, owned: &mut Owned) -> Result<u64, String> {
    let started_ns = SystemClock.now().0;
    let body = robot_msg::build_helpers::build_clear_estop_request(
        PERSIST_CLEAR_ID,
        PERSIST_CLEAR_ID,
        "w3",
        robot_msg::msg::RpcRole::Maintainer,
        "确认已停止",
        "真实有界文件失败中的持久化拒绝",
        false,
        started_ns + 5_000_000_000,
        started_ns,
    );
    let client = owned.client.as_ref().ok_or("原 Clear client 安装后丢失")?;
    owned.pending = Some(client.send_typed(&body).map_err(|e| format!("{e:?}"))?);
    // send 可能已入队；先安装原 Pending，任何保存或断言失败也保留到 Child 真 reap。
    std::fs::write(case.directory.join("persist_pending.request.bin"), body)
        .map_err(|e| e.to_string())?;
    if let Some(issue) = owned
        .pending
        .as_ref()
        .and_then(|pending| pending.delivery_issue())
    {
        return Err(format!(
            "原 PersistPending 请求已入队但附带投递问题：{issue:?}"
        ));
    }
    Ok(started_ns)
}

fn validate_pending_response(bytes: &[u8]) -> Result<Value, String> {
    use robot_msg::msg::{ClearEStopResponse, ClearEStopStatus, RpcStatus, SideEffect};
    let decoded = flatbuffers::root::<ClearEStopResponse>(bytes).map_err(|e| e.to_string())?;
    if decoded.request_id() != Some(PERSIST_CLEAR_ID)
        || decoded.status() != RpcStatus::ResourceExhausted
        || decoded.domain() != ClearEStopStatus::PersistPending
        || decoded.error_msg() != Some("PersistPending")
        || decoded.effect() != SideEffect::None
        || !decoded.retired()
        || decoded.superseded()
        || decoded.dry_run()
        || decoded.cleared_at_ns() != 0
    {
        return Err(format!(
            "新 Clear 原 typed 响应不是无副作用退休的 PersistPending：{decoded:?}"
        ));
    }
    Ok(
        json!({"request_id":decoded.request_id(),"status":format!("{:?}",decoded.status()),
        "domain":format!("{:?}",decoded.domain()),"error_msg":decoded.error_msg(),
        "effect":format!("{:?}",decoded.effect()),"retired":decoded.retired(),
        "superseded":decoded.superseded(),"dry_run":decoded.dry_run(),
        "cleared_at_ns":decoded.cleared_at_ns()}),
    )
}

fn pending_health(value: CurrentHealthObservation, started_ns: u64) -> bool {
    value.version != 0
        && value.version & 1 == 0
        && value.bits == PERSIST_PENDING_BIT
        && value.overdue
        && value.unsettled_since_ns >= started_ns
        && value.unsettled_since_ns != 0
        && value
            .sampled_ns
            .checked_sub(value.unsettled_since_ns)
            .is_some_and(|age| age >= 2_000_000_000)
}

fn recovered_health(
    current: CurrentHealthObservation,
    previous: CurrentHealthObservation,
    removed_ns: u64,
) -> bool {
    current.version & 1 == 0
        && current.version > previous.version
        && current.sampled_ns >= removed_ns
        && current.sampled_ns > previous.sampled_ns
        && current.bits == 0
        && current.unsettled_since_ns == 0
        && !current.overdue
}

fn health_json(value: CurrentHealthObservation) -> Value {
    json!({"version":value.version,"bits":value.bits,"sampled_ns":value.sampled_ns,
        "unsettled_since_ns":value.unsettled_since_ns,"overdue":value.overdue})
}

fn find_rename_error(log: &str) -> Result<Option<Value>, String> {
    for line in log
        .split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
    {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if record["message"] != "latch writer: EStop 义务写入失败，退避后重试"
            || record["stage"] != "RenamePrimary"
            || record["phase"] != "Operation"
        {
            continue;
        }
        let errno = record["errno"]
            .as_str()
            .and_then(|value| value.strip_prefix("Some("))
            .and_then(|value| value.strip_suffix(')'))
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|value| *value != 0)
            .ok_or_else(|| {
                format!("原 RenamePrimary StageError 缺少真实 raw_os_error：{record}")
            })?;
        // 诊断原样保留平台 errno；不将非空目录导致的 rename 错误改名为 EIO。
        return Ok(Some(json!({"raw_os_error":errno,"record":record})));
    }
    Ok(None)
}

fn ensure_alive(child: &mut Child) -> Result<(), String> {
    match child.try_wait().map_err(|e| e.to_string())? {
        None => Ok(()),
        Some(status) => Err(format!("有界文件失败/恢复期间原 daemon 提前退出：{status}")),
    }
}

fn drain_watchdog(notify: &tokio::net::UnixDatagram) -> Result<u64, String> {
    let mut count = 0;
    let mut bytes = [0u8; 512];
    loop {
        match notify.try_recv(&mut bytes) {
            Ok(size) => {
                if bytes[..size]
                    .split(|value| *value == b'\n')
                    .any(|value| value == b"WATCHDOG=1")
                {
                    count += 1;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(count),
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn save_json(case: &Case, name: &str, value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    std::fs::write(case.directory.join(name), bytes).map_err(|e| e.to_string())
}

fn receive_with_closed_tail(
    pending: &Iceoryx2PendingResponse<markers::ClearEStopResponse>,
) -> Result<Option<Vec<u8>>, String> {
    let received = pending.try_recv().map_err(|e| e.to_string())?;
    match (received, pending.connection_state()) {
        // 正常关闭之后再收一次，覆盖首次空收与对端发布之间的窗口。
        (None, StrictChannelStatus::Closed) => pending.try_recv().map_err(|e| e.to_string()),
        // Open/Unknown 仅是观察结果；原件仍由 Owned 持有至真实 Child reap。
        (received, _) => Ok(received),
    }
}

fn pair(case: &Case) -> Result<(Vec<u8>, Vec<u8>), String> {
    Ok((
        std::fs::read(&case.latch).map_err(|e| e.to_string())?,
        std::fs::read(case.directory.join("estop_latch.bak")).map_err(|e| e.to_string())?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use robot_msg::build_helpers::{ClearResponseMeta, build_clear_estop_response_typed};
    use robot_msg::msg::{ClearEStopStatus, RpcStatus, SideEffect};

    fn pending_meta() -> ClearResponseMeta {
        ClearResponseMeta {
            domain: ClearEStopStatus::PersistPending,
            superseded: false,
            effect: SideEffect::None,
            retired: true,
        }
    }

    #[test]
    fn pending_reply_requires_original_identity_and_status() {
        let meta = pending_meta();
        let reply = |id, status, message, at, dry, metadata| {
            build_clear_estop_response_typed(id, status, message, at, dry, metadata)
        };
        let correct = reply(
            PERSIST_CLEAR_ID,
            RpcStatus::ResourceExhausted,
            "PersistPending",
            0,
            false,
            meta,
        );
        assert!(validate_pending_response(&correct).is_ok());
        for wrong in [
            reply(
                "other",
                RpcStatus::ResourceExhausted,
                "PersistPending",
                0,
                false,
                meta,
            ),
            reply(
                PERSIST_CLEAR_ID,
                RpcStatus::FailedPrecondition,
                "PersistPending",
                0,
                false,
                meta,
            ),
            reply(
                PERSIST_CLEAR_ID,
                RpcStatus::ResourceExhausted,
                "BusyLowering",
                0,
                false,
                meta,
            ),
            reply(
                PERSIST_CLEAR_ID,
                RpcStatus::ResourceExhausted,
                "PersistPending",
                1,
                false,
                meta,
            ),
            reply(
                PERSIST_CLEAR_ID,
                RpcStatus::ResourceExhausted,
                "PersistPending",
                0,
                true,
                meta,
            ),
        ] {
            assert!(validate_pending_response(&wrong).is_err());
        }
        assert!(validate_pending_response(b"invalid").is_err());
    }

    #[test]
    fn pending_reply_requires_exact_domain_and_retired_no_effect() {
        let meta = pending_meta();
        for wrong in [
            ClearResponseMeta {
                domain: ClearEStopStatus::BusyLowering,
                ..meta
            },
            ClearResponseMeta {
                effect: SideEffect::Committed,
                ..meta
            },
            ClearResponseMeta {
                retired: false,
                ..meta
            },
            ClearResponseMeta {
                superseded: true,
                ..meta
            },
        ] {
            let bytes = build_clear_estop_response_typed(
                PERSIST_CLEAR_ID,
                RpcStatus::ResourceExhausted,
                "PersistPending",
                0,
                false,
                wrong,
            );
            assert!(validate_pending_response(&bytes).is_err());
        }
    }

    fn degraded() -> CurrentHealthObservation {
        CurrentHealthObservation {
            version: 4,
            bits: PERSIST_PENDING_BIT,
            sampled_ns: 2_000_000_100,
            unsettled_since_ns: 100,
            overdue: true,
        }
    }

    #[test]
    fn pending_health_requires_complete_current_frame_and_original_obligation_age() {
        let value = degraded();
        assert!(pending_health(value, 99));
        for wrong in [
            CurrentHealthObservation {
                version: 0,
                ..value
            },
            CurrentHealthObservation {
                version: 5,
                ..value
            },
            CurrentHealthObservation { bits: 0, ..value },
            CurrentHealthObservation { bits: 3, ..value },
            CurrentHealthObservation {
                overdue: false,
                ..value
            },
            CurrentHealthObservation {
                unsettled_since_ns: 0,
                ..value
            },
            CurrentHealthObservation {
                unsettled_since_ns: 98,
                ..value
            },
            CurrentHealthObservation {
                sampled_ns: 2_000_000_099,
                ..value
            },
        ] {
            assert!(!pending_health(wrong, 99), "{wrong:?}");
        }
        assert!(!pending_health(CurrentHealthObservation::default(), 99));
    }

    #[test]
    fn recovered_health_rejects_old_incomplete_and_still_unsettled_frames() {
        let previous = degraded();
        let current = CurrentHealthObservation {
            version: 6,
            sampled_ns: 3_200_000_110,
            ..CurrentHealthObservation::default()
        };
        let removed_ns = 3_200_000_100;
        assert!(recovered_health(current, previous, removed_ns));
        for wrong in [
            CurrentHealthObservation {
                version: 4,
                ..current
            },
            CurrentHealthObservation {
                version: 7,
                ..current
            },
            CurrentHealthObservation {
                sampled_ns: removed_ns - 1,
                ..current
            },
            CurrentHealthObservation {
                bits: PERSIST_PENDING_BIT,
                ..current
            },
            CurrentHealthObservation {
                unsettled_since_ns: 100,
                ..current
            },
            CurrentHealthObservation {
                overdue: true,
                ..current
            },
            previous,
        ] {
            assert!(!recovered_health(wrong, previous, removed_ns), "{wrong:?}");
        }
    }

    #[test]
    fn rename_diagnostic_keeps_actual_errno_and_rejects_missing_source() {
        let record = json!({"message":"latch writer: EStop 义务写入失败，退避后重试",
            "stage":"RenamePrimary","phase":"Operation","errno":"Some(21)"});
        let parsed = find_rename_error(&format!("{record}\n")).unwrap().unwrap();
        assert_eq!(parsed["raw_os_error"], 21);
        assert_eq!(parsed["record"], record);
        let mut another = record.clone();
        another["errno"] = json!("Some(39)");
        assert_eq!(
            find_rename_error(&format!("{another}\n")).unwrap().unwrap()["raw_os_error"],
            39
        );
        assert!(find_rename_error(&record.to_string()).unwrap().is_none());
        another["errno"] = json!("None");
        assert!(find_rename_error(&format!("{another}\n")).is_err());
        another["errno"] = json!("EIO");
        assert!(find_rename_error(&format!("{another}\n")).is_err());
        another["stage"] = json!("WritePrimary");
        assert!(
            find_rename_error(&format!("{another}\n"))
                .unwrap()
                .is_none()
        );
    }
}
