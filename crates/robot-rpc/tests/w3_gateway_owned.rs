//! 真 S1 子进程 → typed wire → 同一 `GatewayRun` 缓存 → 重放与 owned 收尾。
//! OS spawn 拒绝走 release；factory panic 的 typed 补偿只在明确的 unwind 实件成立。
//! release factory panic 验真实进程终止与 Gateway 原不确定责任，不宣称进程幸存。

mod common;

use std::time::Duration;

use common::case_ctx::CaseCtx;
use common::case_ctx::CaseShutdownError;
use common::client::{MAINTAINER_TOKEN, build_invoke_task_command, build_task_handle_ref};
use common::ready::wait_gateway_connection;
use robot_msg::msg::{ActionRpcStatus, SideEffect};
use robot_rpc::class_cache::{CachedTerminal, DrainOutcome, TerminalState, TypedOutcome};
use robot_rpc::gateway::{GatewayRun, finish_gateway_owned, run_gateway_with_rr_io};
use robot_rpc::idempotency::Method;
use robot_rpc::proto::{ActionKindProto, TaskState, robot_control_client::RobotControlClient};
use tonic::transport::Channel;
use uuid::Uuid;

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const RPC_BUDGET: Duration = Duration::from_secs(5);

#[tokio::test]
async fn real_drain_ready_panic_keeps_typed_terminal_and_replay_after_run_cancellation() {
    let _serial = SERIAL.lock().await;
    let binary = std::env::var_os("ROBOT_W3_S1_UNWIND_BIN")
        .map(std::path::PathBuf::from)
        .expect("runner 必须显式绑定同源 dev/unwind S1 实件");
    run_case("w3_factory_panic_unwind", "factory-panic", Some(&binary)).await;
}

#[tokio::test]
async fn injected_drain_spawn_rejection_keeps_typed_terminal_and_replay_after_run_cancellation() {
    let _serial = SERIAL.lock().await;
    std::env::var_os("B4_E2E_BIN_ROBOT_S1").expect("runner 必须显式绑定 release S1 实件");
    run_case("w3_spawn_error_release", "spawn-error", None).await;
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "同一夹具保持外部服务、自然退出原状态与全部句柄的完整断言及回收链"
)]
async fn release_s1_qos_mismatch_exits_one_without_replacing_existing_service() {
    use common::case_ctx::BringUpError;
    use common::ready::ReadyError;
    use common::subprocess::MarkerError;
    use iceoryx2::prelude::{MessagingPattern, NodeBuilder, Service};
    use iceoryx2::service::ipc::Service as IxService;
    use robot_msg::markers;
    use robot_transport::iceoryx2::request_response::rr_server_with;
    use robot_transport::iceoryx2::{RrProfile, RrStaticLimits, topic_to_service_name};

    let _serial = SERIAL.lock().await;
    let binary = std::env::var_os("B4_E2E_BIN_ROBOT_S1")
        .map(std::path::PathBuf::from)
        .expect("runner 必须绑定本轮 release S1 实件");
    let (mut ctx, (existing, node, topic, before)) =
        CaseCtx::prepare_with_s1_setup("w3_s1_qos_release", &binary, |robot_id| {
            let node = NodeBuilder::new()
                .create::<IxService>()
                .map_err(|error| BringUpError::Config(error.to_string()))?;
            // 第三个 topic 失败：实际 invoke/cancel 原线程已启动，必须一并回收。
            let topic = format!("/{robot_id}/control/task_status");
            let profile = RrProfile {
                l: 3,
                ..RrProfile::S1_TASK
            };
            let existing = rr_server_with::<
                markers::GetTaskStatusRequest,
                markers::GetTaskStatusResponse,
            >(&node, &topic, &profile)
            .map_err(|error| BringUpError::Config(error.to_string()))?;
            let name = topic_to_service_name(&topic)
                .map_err(|error| BringUpError::Config(error.to_string()))?;
            let before =
                IxService::details(&name, node.config(), MessagingPattern::RequestResponse)
                    .map_err(|error| BringUpError::Config(error.to_string()))?
                    .ok_or_else(|| BringUpError::Config("原 RR 服务实际详情缺失".to_owned()))?
                    .static_details;
            Ok((existing, node, topic, before))
        })
        .await
        .expect("原同名服务保活后才 spawn 本次 S1");
    let deadline = tokio::time::Instant::now() + RPC_BUDGET;
    let exercised = async {
        match ctx.s1_ready_result().await {
            Err(ReadyError::S1Marker(MarkerError::EarlyExit {
                status: Some(status),
                ..
            })) if status.code() == Some(1) => {}
            other => {
                return Err(format!(
                    "shutdown 前必须真实自然 EarlyExit(1)，实际 {other:?}"
                ));
            }
        }
        let expected =
            "fatal: S1 RR 配置不兼容 field=max_active_requests_per_client expected=4 actual=3";
        loop {
            let lines = ctx.daemon_output().await;
            if lines.iter().any(|line| {
                line.contains("robot-s1 rr_server_threads ready")
                    || line.contains("panicked at")
                    || line.contains("w3_real_drain_factory_panic")
            }) {
                return Err(format!("配置失败不能报 ready 或借 panic 退出: {lines:?}"));
            }
            let matches = lines
                .iter()
                .filter(|line| line.as_str() == expected)
                .count();
            if matches == 1 {
                break;
            }
            if matches > 1 || tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "原 field/expected/actual 日志缺失或重复: {lines:?}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let name = topic_to_service_name(&topic).map_err(|error| error.to_string())?;
        let after = IxService::details(&name, node.config(), MessagingPattern::RequestResponse)
            .map_err(|error| error.to_string())?
            .ok_or("原服务被删除")?
            .static_details;
        if before.unique_service_id() != after.unique_service_id()
            || before.service_hash() != after.service_hash()
        {
            return Err("配置失败后原外部服务 identity 改变".to_owned());
        }
        let actual = after.request_response();
        if existing.limits() != Some(RrStaticLimits { l: 3, c: 8 })
            || actual.max_active_requests_per_client() != 3
            || actual.max_clients() != 8
            || actual.max_servers() != 1
            || actual.has_safe_overflow_for_requests()
            || !actual.does_support_fire_and_forget_requests()
        {
            return Err("配置失败后原外部服务五项 limits 改变".to_owned());
        }
        Ok::<(), String>(())
    }
    .await;
    // 即使断言不符也走原回收；原服务和 Node 保活到 S1 真 wait 与 fake_state 停止后。
    let stopped = ctx.shutdown().await;
    drop(existing);
    drop(node);
    exercised.expect("真实 release QoS 拒绝、诊断与外部服务保留");
    match stopped {
        Err(CaseShutdownError::DaemonExitNonZero { status }) => assert_eq!(status.code(), Some(1)),
        other => panic!("必须保留原自然 exit1 并真实回收: {other:?}"),
    }
    println!(
        "w3_s1_qos_verified exit=1 field=max_active_requests_per_client expected=4 actual=3 original_service=true child_reaped=true"
    );
}

async fn run_case(test_name: &'static str, mode: &str, s1_binary: Option<&std::path::Path>) {
    let request_id = Uuid::now_v7();
    let id_text = request_id.to_string();
    let environment: [(&str, &str); 2] = [
        ("ROBOT_W3_DRAIN_FAILURE_REQUEST_ID", &id_text),
        ("ROBOT_W3_DRAIN_FAILURE_MODE", mode),
    ];
    let (ctx, mut run, shutdown) = match s1_binary {
        Some(path) => {
            CaseCtx::bring_up_with_owned_gateway_binary(test_name, &environment, path).await
        }
        None => CaseCtx::bring_up_with_owned_gateway(test_name, &environment).await,
    }
    .expect("真实 S1 就绪，GatewayRun 尚未被驱动");

    // 原 GatewayRun 已归本层持有；错误同样先收尾，不能在活动 owner 上直接 panic。
    let exercised = exercise_wire_and_cache(&ctx, &mut run, request_id, mode).await;
    let _ = shutdown.send(true);
    finish_gateway_owned(&mut run).await;
    let released = match run.into_exit() {
        Ok(exit) => match exit.try_release_empty() {
            Ok(result) => result.map_err(|error| error.to_string()),
            Err(exit) => {
                let error = format!("原 Gateway exit 仍有责任或 join 错误: {exit:?}");
                let _held_until_process_exit = std::mem::ManuallyDrop::new(exit);
                Err(error)
            }
        },
        Err(run) => {
            let _held_until_process_exit = std::mem::ManuallyDrop::new(run);
            Err("finish 后原 GatewayRun 未 settled".to_string())
        }
    };
    // 即使前面的验证失败，仍按原顺序回收 S1 → fake_state；gateway=None 不跳过它们。
    let stopped = ctx.shutdown().await;
    exercised.expect("同一原缓存真实 typed 结果、重放与未命中路径");
    released.expect("全部原 Gateway workers 真 join 且原责任为空");
    stopped.expect("S1 与 fake_state 已按原顺序回收");
}

#[cfg(unix)]
#[tokio::test]
async fn release_drain_factory_panic_keeps_uncertain_gateway_responsibility() {
    use std::os::unix::process::ExitStatusExt;

    let _serial = SERIAL.lock().await;
    let id = Uuid::now_v7();
    let id_text = id.to_string();
    // release 实件由 runner 显式绑定；不能无声回退到可能 unwind 的 debug 二进制。
    let binary = std::env::var_os("B4_E2E_BIN_ROBOT_S1")
        .map(std::path::PathBuf::from)
        .expect("runner 必须显式绑定 release/abort S1 实件");
    let (ctx, mut run, shutdown) = CaseCtx::bring_up_with_owned_gateway_binary(
        "w3_factory_panic_release",
        &[
            ("ROBOT_W3_DRAIN_FAILURE_REQUEST_ID", &id_text),
            ("ROBOT_W3_DRAIN_FAILURE_MODE", "factory-panic"),
        ],
        &binary,
    )
    .await
    .expect("真实 release S1 已就绪，原 GatewayRun 已交外层");
    let exercised = exercise_release_abort(&ctx, &mut run, id).await;
    let _ = shutdown.send(true);
    finish_gateway_owned(&mut run).await;
    let retained = match run.into_exit() {
        Ok(exit) => match exit.try_release_empty() {
            Err(exit) => {
                // 活跃信封、原信用及各层 join 结果保留到真正的测试进程退出。
                let _held_until_process_exit = std::mem::ManuallyDrop::new(exit);
                true
            }
            Ok(result) => {
                eprintln!("S1 无退休回复却释放了原 Gateway 聚合: {result:?}");
                false
            }
        },
        Err(run) => {
            let _held_until_process_exit = std::mem::ManuallyDrop::new(run);
            false
        }
    };
    // 原 helper 真 wait 回收 S1，并在其后关闭 fake_state；SIGABRT 是预期事实，非吞错。
    let stopped = ctx.shutdown().await;
    exercised.expect("本次真实 factory panic、无终态、原在飞 key 仍阻止第二次提交");
    assert!(retained, "原 Gateway 完成 join 后仍须返非空聚合");
    let signal = match stopped {
        Err(CaseShutdownError::DaemonExitNonZero { status }) => status.signal(),
        other => panic!("release S1 必须实际被 SIGABRT 终止并回收: {other:?}"),
    };
    assert_eq!(signal, Some(libc::SIGABRT));
    println!(
        "w3_s1_abort_owned_verified signal={} terminal=false replay_in_flight=true nonempty_exit=true child_reaped=true",
        libc::SIGABRT
    );
}

#[cfg(unix)]
async fn exercise_release_abort(
    ctx: &CaseCtx,
    run: &mut GatewayRun,
    id: Uuid,
) -> Result<(), String> {
    let flow = async {
        let channel = wait_gateway_connection(ctx.listen_addr)
            .await
            .map_err(|error| error.to_string())?;
        let mut client = RobotControlClient::new(channel);
        if client.invoke_task(command(id, "w3-abort")).await.is_ok() {
            return Err("release factory panic 后不得返回成功".to_string());
        }
        let Err(status) = client
            .invoke_task(command(Uuid::now_v7(), "w3-abort"))
            .await
        else {
            return Err("无退休回复时同 key 意外重新提交或重放成功".to_string());
        };
        if status.code() != tonic::Code::Aborted
            || !status.message().starts_with("request remains in flight:")
        {
            return Err(format!("原 key 未保持真实在飞责任: {status:?}"));
        }
        let lines = ctx.daemon_output().await;
        if lines
            .iter()
            .filter(|line| line.as_str() == "w3_real_drain_factory_panic")
            .count()
            != 1
        {
            return Err(format!("本次 S1 没有唯一真实 factory panic: {lines:?}"));
        }
        Ok(())
    };
    tokio::select! {
        () = run_gateway_with_rr_io(run) => return Err("Invoke 结果检查前 Gateway 已退出".to_string()),
        result = flow => result?,
    }
    if run
        .cached_terminal(Method::InvokeTask, "w3-abort")
        .is_some()
    {
        return Err("S1 真 abort 未返回退休回复，却产生了缓存终态".to_string());
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "同一 GatewayRun 连续验证原 wire 终态、取消借用、缓存与重放，保留统一收尾责任"
)]
async fn exercise_wire_and_cache(
    ctx: &CaseCtx,
    run: &mut GatewayRun,
    id: Uuid,
    mode: &str,
) -> Result<(), String> {
    let first = async {
        let channel = wait_gateway_connection(ctx.listen_addr)
            .await
            .map_err(|error| error.to_string())?;
        let mut client = RobotControlClient::new(channel.clone());
        let Err(status) = client.invoke_task(command(id, "w3-failure")).await else {
            return Err("精确夹具触发后意外返回成功".to_string());
        };
        Ok::<_, String>((channel, status))
    };
    let (channel, first_status) = tokio::select! {
        () = run_gateway_with_rr_io(run) => return Err("首个 Invoke 返回前 Gateway 已退出".into()),
        result = first => result?,
    };
    // select 已取消借用 run future；读取的仍是该次 RR 消费后提交的原缓存。
    let terminal = run
        .cached_terminal(Method::InvokeTask, "w3-failure")
        .ok_or("真实 wire 返回后没有原 terminal")?;
    let cancel_ok = check_failure_terminal(&terminal, id, mode)?;
    if first_status.code() != tonic::Code::Internal {
        return Err(format!("首个 gRPC 应 Internal，实际 {first_status:?}"));
    }
    let normal_id = Uuid::now_v7();
    let expected_error = first_status.message().to_string();
    let flow = async {
        let mut client = RobotControlClient::new(channel);
        let Err(replay_status) = client
            .invoke_task(command(Uuid::now_v7(), "w3-failure"))
            .await
        else {
            return Err("同 key 错误终态重放意外成功".to_string());
        };
        if replay_status.code() != tonic::Code::Internal
            || replay_status.message() != expected_error
        {
            return Err(format!("重放未保原错误: {replay_status:?}"));
        }
        wait_terminal(&mut client, id).await?;
        let normal = client
            .invoke_task(command(normal_id, "w3-unmatched"))
            .await
            .map_err(|error| format!("其他 request_id 不得触发夹具: {error}"))?
            .into_inner();
        if normal.action_id != normal_id.to_string() {
            return Err(format!("真实正常提交未保 action_id: {normal:?}"));
        }
        wait_terminal(&mut client, normal_id).await?;
        // 等实际日志被原 drain 捕获；没有额外触发或状态模拟。
        let deadline = tokio::time::Instant::now() + RPC_BUDGET;
        loop {
            let lines = ctx.daemon_output().await;
            let failures: Vec<serde_json::Value> = lines
                .iter()
                .filter_map(|line| serde_json::from_str(line).ok())
                .filter(|value: &serde_json::Value| {
                    value["message"].as_str()
                        == Some(
                            "rr_server::handle_invoke: drain 启动失败，已移交回收责任并请求取消",
                        )
                })
                .collect();
            if failures.len() == 1
                && failures[0]["action_id"].as_str() == Some(id.to_string().as_str())
                && failures[0]["cancel_ok"].as_bool() == Some(cancel_ok)
            {
                return Ok::<_, String>(normal);
            }
            if failures.len() > 1 || tokio::time::Instant::now() >= deadline {
                return Err(format!("补偿日志不是本次真实的一次结果: {failures:?}"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let normal = tokio::select! {
        () = run_gateway_with_rr_io(run) => return Err("续接期间 Gateway 已退出".into()),
        result = flow => result?,
    };
    if run.cached_terminal(Method::InvokeTask, "w3-failure") != Some(terminal) {
        return Err("重放改变了原 typed terminal".into());
    }
    match run.cached_terminal(Method::InvokeTask, "w3-unmatched") {
        Some(CachedTerminal {
            state: TerminalState::Committed,
            effect: SideEffect::Committed,
            outcome:
                TypedOutcome::InvokeTask {
                    status: ActionRpcStatus::Ok,
                    action_id,
                    accepted_at_ns,
                    error_msg,
                    drain: DrainOutcome::Registered,
                },
        }) if action_id == normal.action_id && accepted_at_ns > 0 && error_msg.is_empty() => {}
        actual => {
            return Err(format!(
                "正常提交的原缓存未保 Registered 全字段: {actual:?}"
            ));
        }
    }
    Ok(())
}

fn command(id: Uuid, key: &str) -> robot_rpc::proto::InvokeTaskCommand {
    build_invoke_task_command(
        id,
        key,
        "test.w3-owned",
        MAINTAINER_TOKEN,
        ActionKindProto::ActionKindDwellMs,
        5_000,
        "W3 真实 drain 失败与重放",
    )
}

fn check_failure_terminal(terminal: &CachedTerminal, id: Uuid, mode: &str) -> Result<bool, String> {
    if terminal.state != TerminalState::Committed || terminal.effect != SideEffect::Committed {
        return Err(format!("真实提交后的责任分类错误: {terminal:?}"));
    }
    match &terminal.outcome {
        TypedOutcome::InvokeTask {
            status,
            action_id,
            accepted_at_ns,
            error_msg,
            drain: DrainOutcome::DrainFailedCompensated { cancel_ok },
        } if *status == ActionRpcStatus::Internal
            && action_id == &id.to_string()
            && *accepted_at_ns > 0 =>
        {
            let expected = match mode {
                "factory-panic" => "ready",
                "spawn-error" => "w3 注入 OS spawn 拒绝",
                _ => return Err("未知测试模式".into()),
            };
            if !error_msg.contains(expected) {
                return Err(format!("原 wire 未保精确失败诊断: {error_msg}"));
            }
            Ok(*cancel_ok)
        }
        _ => Err(format!("完整 typed Invoke 终态不符: {terminal:?}")),
    }
}

async fn wait_terminal(client: &mut RobotControlClient<Channel>, id: Uuid) -> Result<(), String> {
    let wait = async {
        loop {
            let status = client
                .get_task_status(build_task_handle_ref(Uuid::now_v7(), &id.to_string()))
                .await
                .map_err(|error| error.to_string())?
                .into_inner();
            match TaskState::try_from(status.state).map_err(|error| error.to_string())? {
                TaskState::Completed | TaskState::Cancelled | TaskState::Failed => return Ok(()),
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    };
    tokio::time::timeout(RPC_BUDGET, wait)
        .await
        .map_err(|error| error.to_string())?
}
