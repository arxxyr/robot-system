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
    if !matches!(child.try_wait(), Ok(Some(_))) {
        // kill 失败也继续真实 wait；任何未回收出口终止父进程，不能先 Drop 原 Child。
        let _ = child.start_kill();
        if !matches!(
            tokio::time::timeout(Duration::from_secs(2), child.wait()).await,
            Ok(Ok(_))
        ) {
            std::process::abort();
        }
    }
    result
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
        if page.runtime_armed_ns >= page.ready_ns
            && page.ready_ns != 0
            && page.supervisor_exit_ns == 0
        {
            break;
        }
        if SystemClock.now().0 >= until {
            return Err("真实READY后未完成Runtime接力".into());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    owned.node = Some(
        NodeBuilder::new()
            .create::<Service>()
            .map_err(|e| e.to_string())?,
    );
    let node = owned.node.as_ref().ok_or("原Node安装后丢失")?;
    if matches!(mode, Mode::BoundedIoFailure) {
        owned.publisher = Some(
            robot_transport::iceoryx2::publisher::<markers::EStopEvent>(
                node,
                &format!("/{}/events/estop", case.robot_id),
                robot_transport::iceoryx2::TopicClass::SafetyEvent,
            )
            .map_err(|e| e.to_string())?,
        );
        let publisher = owned
            .publisher
            .as_mut()
            .ok_or("原注入publisher安装后丢失")?;
        // Store 已在daemon持锁打开；只对独立临时latch制造真实rename障碍。
        std::fs::remove_file(&case.latch).map_err(|e| e.to_string())?;
        std::fs::create_dir(&case.latch).map_err(|e| e.to_string())?;
        std::fs::write(case.latch.join("obstacle"), "真实文件失败".as_bytes())
            .map_err(|e| e.to_string())?;
        let now = SystemClock.now().0;
        let body = robot_msg::build_helpers::build_estop_event(
            "w3-file-failure",
            now,
            "software-test",
            true,
        );
        let mut sample = publisher
            .loan(1024 + body.len())
            .map_err(|e| e.to_string())?;
        sample.set_seq(1);
        sample.set_source_id(99);
        sample.set_stamp_ns(now);
        sample.set_body_bytes(&body);
        sample.publish().map_err(|e| e.to_string())?;
        let end = now + 3_200_000_000;
        let mut watchdog_count = 0;
        let mut bytes = [0u8; 512];
        let mut overdue = false;
        while SystemClock.now().0 < end {
            if child.try_wait().map_err(|e| e.to_string())?.is_some() {
                return Err("有界文件失败不应按义务总龄退出".into());
            }
            while let Ok(size) = notify.try_recv(&mut bytes) {
                if bytes[..size]
                    .windows(10)
                    .any(|value| value == b"WATCHDOG=1")
                {
                    watchdog_count += 1;
                }
            }
            let page = case.observation("s0")?;
            overdue |= page.obligation_overdue;
            if page.fired_checkpoint != 0 {
                return Err(format!("单步未卡住却错误执法：{page:?}"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if !overdue || watchdog_count < 4 {
            return Err(format!(
                "未看到真实超期健康和持续看门狗：overdue={overdue} watchdog={watchdog_count}"
            ));
        }
        std::fs::remove_file(case.latch.join("obstacle")).map_err(|e| e.to_string())?;
        std::fs::remove_dir(&case.latch).map_err(|e| e.to_string())?;
        let until = SystemClock.now().0 + 2_000_000_000;
        loop {
            if pair(case) == Ok((b"1\n".to_vec(), b"1\n".to_vec())) {
                break;
            }
            if SystemClock.now().0 >= until {
                return Err("撤除真实障碍后writer未结清原义务".into());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        stop(child).await?;
        let page = case.observation("s0")?;
        if !page.obligation_overdue || page.residual_bitmap != 0 {
            return Err(format!("恢复不得重置历史沿或丢失残余：{page:?}"));
        }
        let log = case.logs("s0")?;
        if !log.contains("控制运行期健康降级") {
            return Err("未看到真实IPC健康report输出".into());
        }
        return Ok(
            json!({"exit_code":0,"watchdog_messages_during_io_failure":watchdog_count,"sticky_overdue":true,"log_directory":case.directory}),
        );
    }
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
