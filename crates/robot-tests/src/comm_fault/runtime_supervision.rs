//! R4c S1–S7：原writer/helper责任与监督的独立真实进程门。
mod oracle;

use super::{Args, control::ControlPort, harness::Case, report::Report};
use iceoryx2::prelude::{Node, NodeBuilder};
use iceoryx2::service::ipc::Service;
use robot_core::time::{RobotClock, SystemClock};
use robot_msg::markers;
use robot_s0::comm_fault_hook::{HookCommandKind, TestCommFaultCommand};
use robot_s0::comm_fault_observation::ObservationSnapshot;
use robot_transport::iceoryx2::request_response::{Iceoryx2PendingResponse, Iceoryx2RrClient};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::process::Child;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Mode {
    Command = 1,
    PrePickup = 2,
    PreStep = 3,
    PanicBefore = 4,
    PanicAfter = 5,
    MainHeld = 6,
    Health = 7,
}
impl Mode {
    fn setting(self) -> &'static str {
        match self {
            Self::Command => "command-pickup",
            Self::PrePickup => "pre-ready-pickup",
            Self::PreStep => "pre-ready-file-step",
            Self::PanicBefore => "panic-before-primary",
            Self::PanicAfter => "panic-after-backup",
            Self::MainHeld => "panic-with-main-held",
            Self::Health => "health-monitor-panic",
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Command => "original_received_clear_pickup_exits_at_announce_deadline",
            Self::PrePickup => "history_obligation_pending_before_ready_uses_original_pickup",
            Self::PreStep => "history_obligation_active_before_ready_uses_original_file_step",
            Self::PanicBefore => {
                "writer_panic_before_primary_preserves_obligation_until_original_fallback"
            }
            Self::PanicAfter => {
                "writer_panic_after_backup_success_preserves_original_responsibility"
            }
            Self::MainHeld => "writer_exited_with_main_held_is_independently_supervised_without_d",
            Self::Health => "original_health_monitor_panics_and_joins_without_residual",
        }
    }
    fn pre_ready(self) -> bool {
        matches!(self, Self::PrePickup | Self::PreStep)
    }
    fn expected_code(self) -> i32 {
        if matches!(
            self,
            Self::Command | Self::PrePickup | Self::PreStep | Self::MainHeld
        ) {
            90
        } else {
            1
        }
    }
}

pub(super) async fn run(args: &Args, report: &mut Report) -> Result<(), String> {
    let modes: &[Mode] = match args.case.as_deref() {
        Some("R4C-STANDALONE") => &[Mode::Command, Mode::PrePickup, Mode::PreStep],
        Some("R4C-UNWIND") => &[
            Mode::PanicBefore,
            Mode::PanicAfter,
            Mode::MainHeld,
            Mode::Health,
        ],
        _ => return Err("R4c监督只接受封闭筛选".into()),
    };
    for &mode in modes {
        report.record(
            mode.name(),
            args.case.as_deref().unwrap(),
            SystemClock.now().0,
            run_case(args, mode).await,
        );
    }
    Ok(())
}

#[derive(Default)]
struct Owned {
    child: Option<Child>,
    control: Option<ControlPort>,
    pending: Option<Iceoryx2PendingResponse<markers::ClearEStopResponse>>,
    client: Option<Iceoryx2RrClient<markers::ClearEStopRequest, markers::ClearEStopResponse>>,
    node: Option<Node<Service>>,
    exit: Option<i32>,
    reaped_ns: u64,
}

async fn run_case(args: &Args, mode: Mode) -> Result<Value, String> {
    let case = Case::new(&args.evidence_dir, &args.bin_dir, mode.name(), 1)?;
    let mut owned = Owned::default();
    let result = exercise(&case, mode, &mut owned).await;
    let cleanup = reap(&mut owned).await;
    let control = owned.control.as_mut().map(ControlPort::finish).transpose();
    let control_joined_ns = SystemClock.now().0;
    let final_page = case.observation("s0").map(|page| oracle::snapshot(&page));
    let evidence = json!({"case":mode.name(),"directory":case.directory,"result":result,"cleanup":cleanup,"control_cleanup":control,"control_joined_ns":control_joined_ns,"exit":owned.exit,"reaped_ns":owned.reaped_ns,"observation":final_page});
    save(&case, "result.json", &evidence)?;
    match (result, cleanup, control) {
        (Ok(_), Ok(_), Ok(_)) => Ok(evidence),
        (r, c, h) => Err(format!(
            "原场景={r:?}；Child={c:?}；ControlPort={h:?}；证据={}",
            case.directory.display()
        )),
    }
}

fn estop() -> TestCommFaultCommand {
    TestCommFaultCommand {
        sequence: 1,
        duration_ms: 0,
        count: 0,
        kind: HookCommandKind::ExternalEStop as u8,
        target: 0,
        error: 0,
        reserved: [0; 5],
    }
}
fn start_control(case: &Case, owned: &mut Owned) -> Result<(), String> {
    owned.control = Some(ControlPort::spawn(&case.robot_id)?);
    owned
        .control
        .as_mut()
        .ok_or("原ControlPort缺失")?
        .wait_ready()
}

async fn exercise(case: &Case, mode: Mode, owned: &mut Owned) -> Result<Value, String> {
    if mode == Mode::Command {
        for path in [&case.latch, &case.directory.join("estop_latch.bak")] {
            std::fs::write(path, b"1\n").map_err(|e| e.to_string())?;
        }
    }
    if mode.pre_ready() {
        start_control(case, owned)?;
        owned.control.as_ref().unwrap().send(estop())?;
    }
    let mut command = case.command();
    command
        .args(["--notify-mode", "standalone"])
        .env("ROBOT_TEST_SUP_MODE", mode.setting())
        .env("ROBOT_LOG_FORMAT", "json")
        .env("RUST_LOG", "warn,robot_s0=info");
    if mode.pre_ready() {
        command.env("ROBOT_TEST_INITIAL_COMMAND_SEQUENCE", "1");
    }
    let launch = json!({"program":command.as_std().get_program().to_string_lossy(),"argv":command.as_std().get_args().map(|v|v.to_string_lossy().into_owned()).collect::<Vec<_>>(),"env":command.as_std().get_envs().map(|(k,v)|(k.to_string_lossy().into_owned(),v.map(|v|v.to_string_lossy().into_owned()))).collect::<std::collections::BTreeMap<_,_>>()});
    save(case, "launch.json", &launch)?;
    owned.child = Some(case.spawn("s0", command)?);
    let pid = owned
        .child
        .as_ref()
        .and_then(Child::id)
        .ok_or("原Child无PID")?;
    let environment = standalone_environment(pid)?;
    save(case, "environment.json", &environment)?;
    let ready = observe(case, owned, SystemClock.now().0 + 12_000_000_000, |p| {
        p.ready_reached && p.runtime_armed_ns != 0
    })
    .await?;
    oracle::ready(mode, &ready)?;
    let mut request = Value::Null;
    match mode {
        Mode::Command => request = send_clear(case, owned)?,
        Mode::PanicBefore | Mode::PanicAfter | Mode::MainHeld => {
            start_control(case, owned)?;
            if mode == Mode::MainHeld {
                observe(case, owned, SystemClock.now().0 + 2_000_000_000, |p| {
                    p.supervision.main_hold_ns != 0
                })
                .await?;
            }
            owned.control.as_ref().unwrap().send(estop())?;
        }
        _ => {}
    }
    let final_until = SystemClock.now().0 + 12_000_000_000;
    loop {
        let exited = poll_exit(owned)?;
        if SystemClock.now().0 >= final_until {
            return Err("原最终12s父观察门超限".into());
        }
        if exited {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let page = case.observation("s0")?;
    let logs = case.logs("s0")?;
    let facts = oracle::exit(mode, owned.exit.ok_or("缺实际exit")?, &page, &logs)?;
    if let Some(pending) = &owned.pending
        && super::w4_latch_crash::receive_with_closed_tail(pending)?.is_some()
    {
        return Err("原未退休Clear出现回复".into());
    }
    let files = oracle::files(case, mode)?;
    Ok(
        json!({"pid":pid,"environment":environment,"launch":launch,"ready":oracle::snapshot(&ready),"request":request,"facts":facts,"files":files,"observation":oracle::snapshot(&page),"final_until":final_until}),
    )
}

async fn observe(
    case: &Case,
    owned: &mut Owned,
    until: u64,
    condition: impl Fn(&ObservationSnapshot) -> bool,
) -> Result<ObservationSnapshot, String> {
    loop {
        let page = case.observation("s0")?;
        if SystemClock.now().0 >= until {
            return Err(format!("原同步观察门超限：{page:?}"));
        }
        if condition(&page) {
            return Ok(page);
        }
        if poll_exit(owned)? {
            return Err(format!(
                "真实条件出现前子进程已退：exit={:?} page={page:?}",
                owned.exit
            ));
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

fn poll_exit(owned: &mut Owned) -> Result<bool, String> {
    if owned.exit.is_some() {
        return Ok(true);
    }
    let child = owned.child.as_mut().ok_or("原Child缺失")?;
    if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
        owned.reaped_ns = SystemClock.now().0;
        owned.exit = Some(
            status
                .code()
                .ok_or_else(|| format!("原进程被信号终止：{status}"))?,
        );
        return Ok(true);
    }
    Ok(false)
}

async fn reap(owned: &mut Owned) -> Result<Value, String> {
    if owned.child.is_none() {
        return Ok(json!({"spawned":false}));
    }
    if owned.reaped_ns != 0 {
        return Ok(json!({"forced":false,"exit":owned.exit,"reaped_ns":owned.reaped_ns}));
    }
    match owned.child.as_mut().unwrap().try_wait() {
        Ok(Some(status)) => {
            owned.reaped_ns = SystemClock.now().0;
            owned.exit = status.code();
            return Ok(
                json!({"forced":false,"exit":owned.exit,"status":status.to_string(),"reaped_ns":owned.reaped_ns}),
            );
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("原Child状态未知，保owner硬失败：{error}");
            std::process::abort();
        }
    }
    let kill_before_ns = SystemClock.now().0;
    let killed = owned
        .child
        .as_mut()
        .unwrap()
        .start_kill()
        .map_err(|e| e.to_string());
    let kill_return_ns = SystemClock.now().0;
    let until = kill_before_ns + 2_000_000_000;
    loop {
        let status = owned.child.as_mut().unwrap().try_wait();
        let observed = SystemClock.now().0;
        match status {
            Ok(Some(status)) => {
                owned.reaped_ns = observed;
                owned.exit = status.code();
                return Err(format!(
                    "失败路径实际强制回收：kill={killed:?} before={kill_before_ns} return={kill_return_ns} reap={observed} within={};status={status}",
                    observed < until
                ));
            }
            Ok(None) if observed < until => tokio::time::sleep(Duration::from_millis(1)).await,
            other => {
                eprintln!("原Child未真回收，保owner硬失败：{other:?} kill={killed:?}");
                std::process::abort();
            }
        }
    }
}

fn send_clear(case: &Case, owned: &mut Owned) -> Result<Value, String> {
    owned.node = Some(
        NodeBuilder::new()
            .create::<Service>()
            .map_err(|e| e.to_string())?,
    );
    owned.client = Some(
        robot_transport::iceoryx2::rr_client_with::<
            markers::ClearEStopRequest,
            markers::ClearEStopResponse,
        >(
            owned.node.as_ref().unwrap(),
            &format!("/{}/control/clear_estop", case.robot_id),
            &robot_transport::iceoryx2::RrProfile::S0_CONTROL,
        )
        .map_err(|e| e.to_string())?,
    );
    let started = SystemClock.now().0;
    let deadline = started + 5_000_000_000;
    let body = robot_msg::build_helpers::build_clear_estop_request(
        "r4c-pickup",
        "r4c-pickup",
        "r4c",
        robot_msg::msg::RpcRole::Maintainer,
        "确认已停止",
        "原接收公告监督",
        false,
        deadline,
        started,
    );
    owned.pending = Some(
        owned
            .client
            .as_ref()
            .unwrap()
            .send_typed(&body)
            .map_err(|e| format!("{e:?}"))?,
    );
    if let Some(issue) = owned.pending.as_ref().unwrap().delivery_issue() {
        return Err(format!("已入队附带issue：{issue:?}"));
    }
    Ok(json!({"id":"r4c-pickup","started_ns":started,"deadline_ns":deadline}))
}

fn standalone_environment(pid: u32) -> Result<Value, String> {
    let bytes = std::fs::read(format!("/proc/{pid}/environ")).map_err(|e| e.to_string())?;
    for key in [
        "NOTIFY_SOCKET",
        "INVOCATION_ID",
        "WATCHDOG_USEC",
        "WATCHDOG_PID",
    ] {
        if bytes
            .split(|v| *v == 0)
            .any(|v| v.starts_with(format!("{key}=").as_bytes()))
        {
            return Err(format!("真实standalone环境仍有{key}"));
        }
    }
    Ok(json!({"pid":pid,"observed_ns":SystemClock.now().0,"manager_environment_absent":true}))
}
fn save(case: &Case, name: &str, value: &Value) -> Result<(), String> {
    std::fs::write(
        case.directory.join(name),
        serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

pub(super) fn public_snapshot(page: &ObservationSnapshot) -> Value {
    oracle::snapshot(page)
}
