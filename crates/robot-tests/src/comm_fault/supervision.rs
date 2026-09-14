//! R4a：原 FileStep、Io 拒停与进程出口屏障的 standalone 真实进程证明。

use std::path::Path;
use std::time::Duration;

use iceoryx2::prelude::{Node, NodeBuilder};
use iceoryx2::service::ipc::Service;
use iceoryx2::strict::StrictChannelStatus;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::markers;
use robot_s0::comm_fault_hook::{HookCommandKind, TestCommFaultCommand};
use robot_s0::comm_fault_observation::ObservationSnapshot;
use robot_s0::latch_writer::CandidateKind;
use robot_s0::shutdown_mechanism::HandleKind;
use robot_s0::worker_stop::WorkerKind;
use robot_transport::iceoryx2::request_response::{Iceoryx2PendingResponse, Iceoryx2RrClient};
use serde_json::{Value, json};
use tokio::process::Child;

use super::control::{ControlPort, wait_applied};
use super::harness::{Case, wait_exit, wait_log};
use super::report::{Report, file_hash};
use super::{Args, scenarios::wait_runtime_armed};

const RUNTIME_MARGIN_NS: u64 = 250_000_000;
const ENVIRONMENT_KEYS: [&str; 4] = [
    "NOTIFY_SOCKET",
    "INVOCATION_ID",
    "WATCHDOG_USEC",
    "WATCHDOG_PID",
];

#[derive(Clone, Copy)]
enum Mode {
    FileStep,
    RefuseIo,
    ExitBarrier,
}

impl Mode {
    const fn name(self) -> &'static str {
        match self {
            Self::FileStep => "standalone_file_step_exits_90_at_original_deadline",
            Self::RefuseIo => "standalone_refuse_io_stops_at_l_with_original_residual",
            Self::ExitBarrier => "standalone_exit_barrier_keeps_residual_until_pure_d",
        }
    }
}

pub(super) async fn run(args: &Args, report: &mut Report) -> Result<(), String> {
    for mode in [Mode::FileStep, Mode::RefuseIo, Mode::ExitBarrier] {
        let started = SystemClock.now().0;
        report.record(
            mode.name(),
            "R4-SUP-STANDALONE",
            started,
            run_case(args, mode).await,
        );
    }
    Ok(())
}

/// 所有端点与注入 worker 留在原 Child 的拥有域，任何 Result 出口先真 reap。
#[derive(Default)]
struct Owned {
    child: Option<Child>,
    control: Option<ControlPort>,
    pending: Option<Iceoryx2PendingResponse<markers::ClearEStopResponse>>,
    client: Option<Iceoryx2RrClient<markers::ClearEStopRequest, markers::ClearEStopResponse>>,
    node: Option<Node<Service>>,
}

async fn run_case(args: &Args, mode: Mode) -> Result<Value, String> {
    let case = Case::new(&args.evidence_dir, &args.bin_dir, mode.name(), 1)?;
    let mut owned = Owned::default();
    let mut command = case.command();
    command
        .args(["--notify-mode", "standalone"])
        .env("ROBOT_LOG_FORMAT", "json")
        .env("RUST_LOG", "warn,s0_daemon=info");
    match mode {
        Mode::FileStep => {
            for path in [&case.latch, &case.directory.join("estop_latch.bak")] {
                std::fs::write(path, b"1\n").map_err(|error| error.to_string())?;
            }
            command.env("ROBOT_TEST_WRITER_PAUSE", "SyncMarkerFile:4000");
        }
        Mode::RefuseIo => {}
        Mode::ExitBarrier => {
            command.env("ROBOT_TEST_BOOT_PAUSE_STAGE", "before_process_exit");
        }
    }
    let launch = json!({"program":command.as_std().get_program().to_string_lossy(),
        "argv":command.as_std().get_args().map(|v|v.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        "env_changes":command.as_std().get_envs().map(|(k,v)|(k.to_string_lossy().into_owned(),v.map(|v|v.to_string_lossy().into_owned()))).collect::<std::collections::BTreeMap<_,_>>()});
    save_json(&case.directory.join("launch.json"), &launch)?;
    owned.child = Some(case.spawn("s0", command)?);
    let result = exercise(&case, mode, &mut owned).await;
    let cleanup = reap(&mut owned).await;
    // 原 daemon 已真 reap 后才关闭原注入端点；线程错误也不能被场景 PASS 遮住。
    let control_spawned = owned.control.is_some();
    let control_cleanup = owned.control.as_mut().map(ControlPort::finish).transpose();
    let control_cleanup_at_ns = SystemClock.now().0;
    let observation = case.observation("s0").map(|page| snapshot(&page));
    let evidence = json!({"case":mode.name(),"result":result,"cleanup":cleanup,"control_spawned":control_spawned,
        "control_cleanup":control_cleanup,"control_cleanup_at_ns":control_cleanup_at_ns,
        "observation":observation,"launch":launch,"directory":case.directory});
    save_json(&case.directory.join("result.json"), &evidence)?;
    match (result, cleanup, control_cleanup) {
        (Ok(_), Ok(_), Ok(_)) => Ok(evidence),
        (result, cleanup, control_cleanup) => Err(format!(
            "原场景={result:?}；真实收尾={cleanup:?}；原注入线程={control_cleanup:?}；证据={}",
            case.directory.display()
        )),
    }
}

async fn reap(owned: &mut Owned) -> Result<Value, String> {
    let Some(child) = owned.child.as_mut() else {
        return Ok(json!({"spawned":false}));
    };
    let initial = child.try_wait();
    if let Ok(Some(status)) = initial {
        return Ok(
            json!({"reaped":true,"forced":false,"exit":status.to_string(),"at_ns":SystemClock.now().0}),
        );
    }
    let kill = child.start_kill();
    let waited = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    match waited {
        Ok(Ok(status)) => Err(format!(
            "失败出口已强制真回收：原poll={initial:?} kill={kill:?} exit={status}"
        )),
        result => {
            eprintln!(
                "原Child未能真回收，禁止释放RR/Node：poll={initial:?} kill={kill:?} wait={result:?}"
            );
            std::process::abort();
        }
    }
}

async fn exercise(case: &Case, mode: Mode, owned: &mut Owned) -> Result<Value, String> {
    let child = owned.child.as_mut().ok_or("原Child缺失")?;
    let pid = child.id().ok_or("原Child没有PID")?;
    wait_log(case, child, "s0", "boot_ready", Duration::from_secs(12)).await?;
    wait_runtime_armed(case, child, "s0", SystemClock.now().0 + 1_000_000_000).await?;
    let ready = case.observation("s0")?;
    if ready.version != 3 || ready.worker_bitmap != 0xff || ready.fired_checkpoint != 0 {
        return Err(format!("真实standalone启动不符：{ready:?}"));
    }
    let environment = observe_environment(pid)?;
    save_json(&case.directory.join("environment.json"), &environment)?;
    let details = match mode {
        Mode::FileStep => exercise_file_step(case, owned).await?,
        Mode::RefuseIo | Mode::ExitBarrier => exercise_shutdown(case, mode, owned).await?,
    };
    Ok(json!({"pid":pid,"environment":environment,"ready":snapshot(&ready),"details":details}))
}

fn observe_environment(pid: u32) -> Result<Value, String> {
    let bytes = std::fs::read(format!("/proc/{pid}/environ")).map_err(|error| error.to_string())?;
    let mut values = serde_json::Map::new();
    for key in ENVIRONMENT_KEYS {
        if bytes.split(|byte| *byte == 0).any(|entry| {
            entry
                .strip_prefix(key.as_bytes())
                .is_some_and(|tail| tail.starts_with(b"="))
        }) {
            return Err(format!("原daemon实际环境仍含{key}，不能称standalone"));
        }
        values.insert(key.to_owned(), Value::Null);
    }
    Ok(Value::Object(values))
}

async fn exercise_file_step(case: &Case, owned: &mut Owned) -> Result<Value, String> {
    owned.node = Some(
        NodeBuilder::new()
            .create::<Service>()
            .map_err(|error| error.to_string())?,
    );
    let node = owned.node.as_ref().ok_or("原Node缺失")?;
    owned.client = Some(
        robot_transport::iceoryx2::rr_client_with::<
            markers::ClearEStopRequest,
            markers::ClearEStopResponse,
        >(
            node,
            &format!("/{}/control/clear_estop", case.robot_id),
            &robot_transport::iceoryx2::RrProfile::S0_CONTROL,
        )
        .map_err(|error| error.to_string())?,
    );
    let client = owned.client.as_ref().ok_or("原client缺失")?;
    let started = SystemClock.now().0;
    let body = robot_msg::build_helpers::build_clear_estop_request(
        "r4-file-step",
        "r4-file-step",
        "r4",
        robot_msg::msg::RpcRole::Maintainer,
        "确认已停止",
        "真实standalone文件步骤监督",
        false,
        started + 5_000_000_000,
        started,
    );
    owned.pending = Some(
        client
            .send_typed(&body)
            .map_err(|error| format!("{error:?}"))?,
    );
    let pending = owned.pending.as_ref().ok_or("原Pending缺失")?;
    if let Some(issue) = pending.delivery_issue() {
        return Err(format!("原请求已入队但附带issue：{issue:?}"));
    }
    let child = owned.child.as_mut().ok_or("原Child缺失")?;
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
    validate_file_step(code, &page)?;
    let received = receive_with_closed_tail(pending)?;
    if received.is_some() {
        return Err("Prepare原请求出现伪退休回复".into());
    }
    let files = [case.latch.clone(),case.directory.join("estop_latch.bak"),case.directory.join("estop_latch.clear_pending")]
        .iter().map(|path| Ok(json!({"path":path,"sha256":file_hash(path)?,"bytes":std::fs::read(path).map_err(|error|error.to_string())?})))
        .collect::<Result<Vec<Value>,String>>()?;
    Ok(
        json!({"exit_code":code,"request_started_ns":started,"request_id":"r4-file-step","retired_reply":false,"files":files,"observation":snapshot(&page)}),
    )
}

fn receive_with_closed_tail(
    pending: &Iceoryx2PendingResponse<markers::ClearEStopResponse>,
) -> Result<Option<Vec<u8>>, String> {
    let received = pending.try_recv().map_err(|error| error.to_string())?;
    match (received, pending.connection_state()) {
        // Closed 后再收一次；Open/Unknown 不推断退休，原 Pending 仍留在 Owned 中。
        (None, StrictChannelStatus::Closed) => {
            pending.try_recv().map_err(|error| error.to_string())
        }
        (received, _) => Ok(received),
    }
}

async fn exercise_shutdown(case: &Case, mode: Mode, owned: &mut Owned) -> Result<Value, String> {
    owned.control = Some(ControlPort::spawn(&case.robot_id)?);
    owned.control.as_mut().ok_or("原注入器缺失")?.wait_ready()?;
    owned
        .control
        .as_ref()
        .ok_or("原注入器缺失")?
        .send(TestCommFaultCommand {
            sequence: 1,
            duration_ms: 20_000,
            count: 0,
            kind: HookCommandKind::RefuseStop as u8,
            target: WorkerKind::Io as u8,
            error: 0,
            reserved: [0; 5],
        })?;
    let child = owned.child.as_mut().ok_or("原Child缺失")?;
    wait_applied(case, child, 1).await?;
    let pid = child.id().ok_or("原Child缺少PID")?;
    let signal_pid = i32::try_from(pid).map_err(|error| error.to_string())?;
    let signal_at_ns = SystemClock.now().0;
    // SAFETY：此 PID 仍由未回收的原 Child 持有，仅发送该场景授权的 SIGTERM。
    let signal_result = unsafe { libc::kill(signal_pid, libc::SIGTERM) };
    if signal_result != 0 {
        return Err(format!(
            "原SIGTERM失败：{}",
            std::io::Error::last_os_error()
        ));
    }
    let mut barrier_observed = None;
    if matches!(mode, Mode::ExitBarrier) {
        barrier_observed =
            Some(wait_exit_barrier(case, child, signal_at_ns + 11_000_000_000).await?);
    }
    let remaining = (signal_at_ns + 11_000_000_000).saturating_sub(SystemClock.now().0);
    let code = wait_exit(child, Duration::from_nanos(remaining)).await?;
    let reaped_ns = SystemClock.now().0;
    let page = case.observation("s0")?;
    validate_shutdown_exit(mode, code, &page)?;
    let exit_facts = parse_exit_facts(&case.logs("s0")?)?;
    if matches!(mode, Mode::ExitBarrier)
        && !case
            .logs("s0")?
            .contains("comm_fault_hook stage=before_process_exit")
    {
        return Err("缺少原出口屏障到达日志".into());
    }
    Ok(
        json!({"pid":pid,"signal":"SIGTERM","signal_at_ns":signal_at_ns,"signal_result":signal_result,
        "exit_code":code,"reaped_ns":reaped_ns,"barrier_alive":barrier_observed,"exit_facts":exit_facts,"observation":snapshot(&page)}),
    )
}

async fn wait_exit_barrier(case: &Case, child: &mut Child, until: u64) -> Result<Value, String> {
    loop {
        let page = case.observation("s0")?;
        let exited = child
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some();
        if page.before_process_exit_ns != 0 {
            if exited {
                return Err("未在原进程存活时观察到出口屏障".into());
            }
            validate_shutdown_boundary(&page)?;
            return Ok(
                json!({"alive_observed_at_ns":SystemClock.now().0,"observation":snapshot(&page)}),
            );
        }
        if exited || SystemClock.now().0 >= until {
            return Err(format!("原出口屏障未到达：{page:?}"));
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn validate_shutdown_exit(mode: Mode, code: i32, page: &ObservationSnapshot) -> Result<(), String> {
    validate_shutdown_boundary(page)?;
    match mode {
        Mode::RefuseIo
            if code == 1
                && page.fired_checkpoint == 0
                && page.fired_ns == 0
                && page.fired_reason == 0
                && page.fired_deadline_ns == 0
                && page.fired_candidates_bitmap == 0 => {}
        Mode::ExitBarrier
            if code == 90
                && page.fired_checkpoint == 4
                && page.fired_reason == 3
                && page.fired_deadline_ns == page.shutdown_deadline_ns
                && page.fired_candidates_bitmap == 1 << CandidateKind::Shutdown as u8 =>
        {
            validate_fire_window(page)?;
        }
        _ => return Err(format!("原运行期退出来源不符：code={code} {page:?}")),
    }
    Ok(())
}

fn parse_exit_facts(logs: &str) -> Result<Value, String> {
    let records: Vec<Value> = logs
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| record["message"] == "R4运行期原退出结果")
        .collect();
    let [record] = records.as_slice() else {
        return Err(format!("原退出结果日志必须恰一条，实际{}", records.len()));
    };
    if !matches!(
        record["writer_join_result"].as_str(),
        Some("stopped" | "stopped_after_ipc_disconnect")
    ) || record["shutdown_failures"] != "[JoinTimeout(Io)]"
    {
        return Err(format!("原writer结果/真实失败集合不符：{record}"));
    }
    Ok(record.clone())
}

fn validate_fire_window(page: &ObservationSnapshot) -> Result<(), String> {
    if page.fired_deadline_ns == 0
        || !(page.fired_deadline_ns..=page.fired_deadline_ns.saturating_add(RUNTIME_MARGIN_NS))
            .contains(&page.fired_ns)
    {
        return Err(format!("监督未遵守原candidate截止：{page:?}"));
    }
    Ok(())
}

fn validate_file_step(code: i32, page: &ObservationSnapshot) -> Result<(), String> {
    validate_fire_window(page)?;
    if code != 90
        || page.fired_checkpoint != 3
        || page.fired_reason != 1
        || page.fired_candidates_bitmap != 1 << CandidateKind::FileStep as u8
        || page.shutdown_deadline_ns != 0
        || page.before_process_exit_ns != 0
        || page.fired_deadline_ns < page.runtime_armed_ns.saturating_add(2_000_000_000)
    {
        return Err(format!("原FileStep监督不符：code={code} {page:?}"));
    }
    Ok(())
}

fn validate_shutdown_boundary(page: &ObservationSnapshot) -> Result<(), String> {
    let deadline = page.shutdown_deadline_ns;
    let boundary = page.before_process_exit_ns;
    if deadline < 10_000_000_000
        || !(deadline - RUNTIME_MARGIN_NS..deadline).contains(&boundary)
        || page.writer_exited_at_ns == 0
        || page.writer_exited_at_ns > page.writer_joined_observed_at_ns
        || page.writer_joined_observed_at_ns > boundary
        || page.residual_bitmap != u64::from(HandleKind::Io.bit())
        || page.exit_residual_bitmap != page.residual_bitmap
        || page.refused_stop_bitmap != 1 << WorkerKind::Io as u8
        || page.stop_bitmap != 0xff
        || page.supervisor_exit_ns != 0
    {
        return Err(format!("L出口、真实join或原residual不符：{page:?}"));
    }
    Ok(())
}

fn snapshot(page: &ObservationSnapshot) -> Value {
    json!({"version":page.version,"runtime_armed_ns":page.runtime_armed_ns,"ready_ns":page.ready_ns,
        "shutdown_deadline_ns":page.shutdown_deadline_ns,"derived_l_ns":page.shutdown_deadline_ns.checked_sub(RUNTIME_MARGIN_NS),
        "writer_exited_at_ns":page.writer_exited_at_ns,"writer_joined_observed_at_ns":page.writer_joined_observed_at_ns,
        "before_process_exit_ns":page.before_process_exit_ns,"exit_residual_bitmap":page.exit_residual_bitmap,
        "fired_ns":page.fired_ns,"fired_checkpoint":page.fired_checkpoint,"fired_reason":page.fired_reason,
        "fired_deadline_ns":page.fired_deadline_ns,"fired_candidates_bitmap":page.fired_candidates_bitmap,
        "residual_bitmap":page.residual_bitmap,"refused_stop_bitmap":page.refused_stop_bitmap,"stop_bitmap":page.stop_bitmap,
        "full_debug":format!("{page:?}")})
}

fn save_json(path: &Path, value: &Value) -> Result<(), String> {
    std::fs::write(
        path,
        serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use robot_s0::comm_fault_observation::ObservationPage;

    use super::*;

    #[test]
    fn original_exit_result_rejects_writer_panic_missing_or_additional_failure() {
        let valid = json!({"message":"R4运行期原退出结果","writer_join_result":"stopped","shutdown_failures":"[JoinTimeout(Io)]"});
        assert!(parse_exit_facts(&valid.to_string()).is_ok());
        assert!(parse_exit_facts("").is_err());
        assert!(parse_exit_facts(&format!("{valid}\n{valid}")).is_err());
        for (field, value) in [
            ("writer_join_result", "panicked"),
            ("writer_join_result", "missing"),
            (
                "shutdown_failures",
                "[JoinTimeout(Io), Panicked(LatchWriter)]",
            ),
            ("shutdown_failures", "[]"),
        ] {
            let mut record = valid.clone();
            record[field] = json!(value);
            assert!(parse_exit_facts(&record.to_string()).is_err());
        }
    }

    static NEXT_PAGE: AtomicU64 = AtomicU64::new(0);

    /// 先读取真实零页，再构造判据输入；这些输入不作为 daemon 运行证据。
    fn empty_snapshot() -> ObservationSnapshot {
        let sequence = NEXT_PAGE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "robot-r4-supervision-oracle-{}-{sequence}.page",
            std::process::id()
        ));
        let page = ObservationPage::create(&path).unwrap();
        let snapshot = page.snapshot();
        drop(page);
        std::fs::remove_file(path).unwrap();
        assert_eq!(snapshot.shutdown_deadline_ns, 0);
        assert_eq!(snapshot.writer_joined_observed_at_ns, 0);
        assert_eq!(snapshot.before_process_exit_ns, 0);
        assert_eq!(snapshot.exit_residual_bitmap, 0);
        assert_eq!(snapshot.fired_deadline_ns, 0);
        assert_eq!(snapshot.fired_candidates_bitmap, 0);
        snapshot
    }

    fn file_step_input() -> ObservationSnapshot {
        let mut page = empty_snapshot();
        page.runtime_armed_ns = 10_000_000_000;
        page.fired_deadline_ns = 12_000_000_000;
        page.fired_ns = page.fired_deadline_ns + 10_000_000;
        page.fired_checkpoint = 3;
        page.fired_reason = 1;
        page.fired_candidates_bitmap = 1 << CandidateKind::FileStep as u8;
        page
    }

    fn shutdown_input(mode: Mode) -> ObservationSnapshot {
        let mut page = empty_snapshot();
        page.runtime_armed_ns = 1_000_000_000;
        page.shutdown_deadline_ns = 20_000_000_000;
        page.before_process_exit_ns = 19_800_000_000;
        page.writer_exited_at_ns = 15_000_000_000;
        // 真实 join 的观测允许在 L 后，不能把它误收紧成 L 前条件。
        page.writer_joined_observed_at_ns = 19_760_000_000;
        page.residual_bitmap = u64::from(HandleKind::Io.bit());
        page.exit_residual_bitmap = page.residual_bitmap;
        page.refused_stop_bitmap = 1 << WorkerKind::Io as u8;
        page.stop_bitmap = 0xff;
        if matches!(mode, Mode::ExitBarrier) {
            page.fired_deadline_ns = page.shutdown_deadline_ns;
            page.fired_ns = page.fired_deadline_ns + 10_000_000;
            page.fired_checkpoint = 4;
            page.fired_reason = 3;
            page.fired_candidates_bitmap = 1 << CandidateKind::Shutdown as u8;
        }
        page
    }

    #[test]
    fn zero_observation_cannot_prove_any_runtime_exit() {
        let page = empty_snapshot();
        assert!(validate_file_step(90, &page).is_err());
        assert!(validate_shutdown_exit(Mode::RefuseIo, 1, &page).is_err());
        assert!(validate_shutdown_exit(Mode::ExitBarrier, 90, &page).is_err());
    }

    #[test]
    fn fire_window_accepts_original_bounds_and_rejects_early_or_late_fire() {
        let mut page = file_step_input();
        for accepted in [
            page.fired_deadline_ns,
            page.fired_deadline_ns + RUNTIME_MARGIN_NS,
        ] {
            page.fired_ns = accepted;
            assert!(validate_file_step(90, &page).is_ok());
        }
        for rejected in [
            page.fired_deadline_ns - 1,
            page.fired_deadline_ns + RUNTIME_MARGIN_NS + 1,
        ] {
            page.fired_ns = rejected;
            assert!(validate_file_step(90, &page).is_err());
        }
    }

    #[test]
    fn file_step_rejects_wrong_reason_checkpoint_deadline_and_shutdown_origin() {
        let valid = file_step_input();
        assert!(validate_file_step(90, &valid).is_ok());
        assert!(validate_file_step(1, &valid).is_err());
        let mutations: [fn(&mut ObservationSnapshot); 5] = [
            |page| page.fired_reason = 3,
            |page| page.fired_checkpoint = 4,
            |page| page.fired_deadline_ns = page.runtime_armed_ns + 1,
            |page| page.shutdown_deadline_ns = 20_000_000_000,
            |page| page.fired_candidates_bitmap |= 1 << CandidateKind::Shutdown as u8,
        ];
        for mutate in mutations {
            let mut page = valid;
            mutate(&mut page);
            assert!(
                validate_file_step(90, &page).is_err(),
                "错误来源被接受：{page:?}"
            );
        }
    }

    #[test]
    fn pure_d_rejects_file_step_pickup_and_wrong_fire_source() {
        let valid = shutdown_input(Mode::ExitBarrier);
        assert!(validate_shutdown_exit(Mode::ExitBarrier, 90, &valid).is_ok());
        assert!(validate_shutdown_exit(Mode::ExitBarrier, 1, &valid).is_err());
        for candidate in [
            CandidateKind::FileStep,
            CandidateKind::Pickup,
            CandidateKind::WriterExited,
        ] {
            let mut page = valid;
            page.fired_candidates_bitmap |= 1 << candidate as u8;
            assert!(validate_shutdown_exit(Mode::ExitBarrier, 90, &page).is_err());
        }
        let mutations: [fn(&mut ObservationSnapshot); 5] = [
            |page| page.fired_reason = 1,
            |page| page.fired_checkpoint = 3,
            |page| page.fired_deadline_ns -= 1,
            |page| page.fired_ns = page.shutdown_deadline_ns - 1,
            |page| page.fired_candidates_bitmap = 0,
        ];
        for mutate in mutations {
            let mut page = valid;
            mutate(&mut page);
            assert!(
                validate_shutdown_exit(Mode::ExitBarrier, 90, &page).is_err(),
                "伪纯 D 被接受：{page:?}"
            );
        }
    }

    #[test]
    fn shutdown_boundary_rejects_fake_join_and_mismatched_worker_handle_bitmaps() {
        let valid = shutdown_input(Mode::RefuseIo);
        assert!(validate_shutdown_exit(Mode::RefuseIo, 1, &valid).is_ok());
        let mutations: [fn(&mut ObservationSnapshot); 9] = [
            |page| page.writer_exited_at_ns = 0,
            |page| page.writer_joined_observed_at_ns = 0,
            |page| page.writer_joined_observed_at_ns = page.writer_exited_at_ns - 1,
            |page| page.writer_joined_observed_at_ns = page.before_process_exit_ns + 1,
            |page| page.residual_bitmap = 1 << WorkerKind::Io as u8,
            |page| page.exit_residual_bitmap = 1 << WorkerKind::Io as u8,
            |page| page.refused_stop_bitmap = u64::from(HandleKind::Io.bit()),
            |page| page.stop_bitmap &= !(1 << WorkerKind::LatchWriter as u8),
            |page| page.supervisor_exit_ns = page.before_process_exit_ns,
        ];
        for mutate in mutations {
            let mut page = valid;
            mutate(&mut page);
            assert!(
                validate_shutdown_boundary(&page).is_err(),
                "伪 join 或错位图被接受：{page:?}"
            );
        }
    }

    #[test]
    fn refuse_stop_keeps_l_inclusive_d_exclusive_and_cannot_claim_supervisor_exit() {
        let mut page = shutdown_input(Mode::RefuseIo);
        page.writer_joined_observed_at_ns = page.shutdown_deadline_ns - RUNTIME_MARGIN_NS;
        for accepted in [
            page.shutdown_deadline_ns - RUNTIME_MARGIN_NS,
            page.shutdown_deadline_ns - 1,
        ] {
            page.before_process_exit_ns = accepted;
            assert!(validate_shutdown_exit(Mode::RefuseIo, 1, &page).is_ok());
        }
        for rejected in [
            page.shutdown_deadline_ns - RUNTIME_MARGIN_NS - 1,
            page.shutdown_deadline_ns,
        ] {
            page.before_process_exit_ns = rejected;
            assert!(validate_shutdown_exit(Mode::RefuseIo, 1, &page).is_err());
        }
        let fired = shutdown_input(Mode::ExitBarrier);
        assert!(validate_shutdown_exit(Mode::RefuseIo, 1, &fired).is_err());
        assert!(validate_shutdown_exit(Mode::RefuseIo, 90, &fired).is_err());
    }
}
