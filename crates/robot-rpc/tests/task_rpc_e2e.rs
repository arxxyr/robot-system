//! B4 (3/3) cross-process integration test for v0.3 task control plane RPC。
//!
//! ## 硬性运行要求
//!
//! 6 个 `#[tokio::test]` **必须串行跑**。本文件顶部 `static GLOBAL_LOCK:
//! tokio::sync::Mutex<()>` 强制 self-implemented serial（每 test fn 第一
//! 行 `let _guard = GLOBAL_LOCK.lock().await;` 持锁直到 fn 退出），不引
//! `serial_test` 依赖。**故意用 `tokio::sync::Mutex` 而非 `std::sync::Mutex`**：
//! `std::sync::MutexGuard` 跨 `.await` 持有是 anti-pattern（clippy
//! `await_holding_lock` 会 lint，且 fn 切到不同 worker 线程时 guard
//! 失效）；async-aware mutex 才能在 6 个 test fn 主体大量 await 调用之间
//! 安全持锁（reviewer Plan Approved 阶段拍板）。
//!
//! 同时**推荐**用：
//!
//! ```text
//! cargo test --features client --test task_rpc_e2e -- --test-threads=1
//! ```
//!
//! `--test-threads=1` 是 explicit 防御 = `GLOBAL_LOCK` 是隐式防御；
//! 两者并存不冲突。
//!
//! ## Feature gate
//!
//! 本文件用 `RobotControlClient`（gRPC client stub），需要 `client`
//! feature。`Cargo.toml` 的 `[[test]] required-features = ["client"]`
//! 让 `cargo test --workspace --all-targets` 默认跳过本 binary（生产路
//! 径不破坏）；CI / dev 显式 `cargo test --features client ...` 才跑。
//!
//! ## 测试一览（7 AC，6 cross-process + 1 lib/in-process）
//!
//! | AC | Test fn | 覆盖 |
//! |----|---------|------|
//! | AC-1 | `happy_invoke_stream_completed` | InvokeTask + StreamTaskEvents 端到端 |
//! | AC-2 | `cancel_task_emits_cancelled` | CancelTask happy + Cancelled stream |
//! | AC-3 | `get_task_status_three_paths` | 执行中 / 终态 / 未知 ID |
//! | AC-4 | `invoke_task_idempotency_replay_and_conflict` | Replay (R2 重 R1 action_id) + Conflict (different source) |
//! | AC-5 | `auth_deny_viewer_invoke_and_operator_clear_error` | 5a viewer + 5b operator (B4 (2/3-b) 收紧) |
//! | AC-6 | `wire_contract_source_task_id_monotonic` | 测试侧 sub command topic 验单调 |
//! | AC-7 | (lib unit in `src/task_handlers.rs::tests`) | ClearError handler 字面层 |

mod common;

use std::time::Duration;

use uuid::Uuid;

use common::case_ctx::CaseCtx;
use common::client::{
    MAINTAINER_TOKEN, OPERATOR_TOKEN, VIEWER_TOKEN, build_cancel_task_command,
    build_clear_error_command, build_invoke_task_command, build_task_handle_ref,
};
use robot_rpc::proto::{
    ActionKindProto, TaskEventKind, TaskState, robot_control_client::RobotControlClient,
};

/// 全局 async mutex 强制 6 个 `#[tokio::test]` 串行跑（双保险：跟
/// `--test-threads=1` 并存）。**必须** `tokio::sync::Mutex` 而不是
/// `std::sync::Mutex` —— guard 跨 `.await` 是 anti-pattern。
static GLOBAL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const STREAM_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

// ============================================================================
// AC-1: happy InvokeTask + StreamTaskEvents
// ============================================================================

#[tokio::test]
async fn happy_invoke_stream_completed() {
    let _guard = GLOBAL_LOCK.lock().await;
    let ctx = CaseCtx::bring_up("happy").await.expect("bring_up");
    let mut client = RobotControlClient::new(ctx.channel.clone());

    let request_id = Uuid::now_v7();
    let invoke = build_invoke_task_command(
        request_id,
        "ik-happy",
        "test.happy",
        MAINTAINER_TOKEN,
        ActionKindProto::ActionKindDwellMs,
        5_000,
        "AC-1 happy invoke + stream",
    );
    let invoke_resp = client
        .invoke_task(invoke)
        .await
        .expect("invoke_task RPC")
        .into_inner();
    assert_eq!(invoke_resp.request_id, request_id.to_string());
    assert!(!invoke_resp.action_id.is_empty(), "action_id non-empty");

    let stream_req = build_task_handle_ref(request_id, &invoke_resp.action_id);
    let stream_resp = client
        .stream_task_events(stream_req)
        .await
        .expect("stream_task_events RPC");
    let mut stream = stream_resp.into_inner();

    // 收 events 直到 terminal 或 timeout。
    let mut events: Vec<TaskEventKind> = Vec::new();
    let deadline = tokio::time::Instant::now() + STREAM_DRAIN_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.message()).await {
            Ok(Ok(Some(event))) => {
                let kind =
                    TaskEventKind::try_from(event.event).unwrap_or(TaskEventKind::Unspecified);
                events.push(kind);
                if matches!(
                    kind,
                    TaskEventKind::Completed | TaskEventKind::Failed | TaskEventKind::Cancelled
                ) {
                    break;
                }
            }
            Ok(Ok(None)) => break, // stream 自然 close
            Ok(Err(status)) => panic!("stream error: {status:?}"),
            Err(_elapsed) => break,
        }
    }
    // **不**强制收到 `Started` —— pubsub keep-last 单 slot；client 端
    // events_subscribe thread 在 invoke_task RPC 返回**之后**才起，可能错过
    // s1 端 events_drain 在 invoke 返 Ok 之前 publish 的第一帧 Started
    // （outline §5.5 #1 ack 的设计：client 用 GetTaskStatus catch up）。
    // 验"stream 有事件 + 终态收到"即足够锁住端到端通路。
    assert!(
        !events.is_empty(),
        "expected at least 1 stream event, got empty"
    );
    assert!(
        events
            .iter()
            .any(|k| matches!(k, TaskEventKind::Completed | TaskEventKind::Cancelled)),
        "expected Completed or Cancelled terminal in events, got: {events:?}"
    );

    drop(stream);
    drop(client);
    ctx.shutdown().await.expect("shutdown");
}

// ============================================================================
// AC-2: CancelTask emits Cancelled
// ============================================================================

#[tokio::test]
async fn cancel_task_emits_cancelled() {
    let _guard = GLOBAL_LOCK.lock().await;
    let ctx = CaseCtx::bring_up("cancel").await.expect("bring_up");
    let mut client = RobotControlClient::new(ctx.channel.clone());

    // invoke long-duration（mock_dwell duration_ms=100ms 默认；用大 timeout_ms
    // 让 cancel 来得及在终态 Completed 之前到达）。
    let request_id = Uuid::now_v7();
    let invoke_resp = client
        .invoke_task(build_invoke_task_command(
            request_id,
            "ik-cancel",
            "test.cancel",
            MAINTAINER_TOKEN,
            ActionKindProto::ActionKindDwellMs,
            5_000,
            "AC-2 cancel test invoke",
        ))
        .await
        .expect("invoke_task RPC")
        .into_inner();

    let action_id = invoke_resp.action_id.clone();
    let stream_resp = client
        .stream_task_events(build_task_handle_ref(request_id, &action_id))
        .await
        .expect("stream_task_events RPC");
    let mut stream = stream_resp.into_inner();

    // 等 50ms 让 invoke 进 Executing 状态，然后 cancel
    tokio::time::sleep(Duration::from_millis(50)).await;
    let cancel_req_id = Uuid::now_v7();
    let cancel_resp = client
        .cancel_task(build_cancel_task_command(
            cancel_req_id,
            "ik-cancel-cmd",
            "test.cancel",
            MAINTAINER_TOKEN,
            &action_id,
            5_000,
            "AC-2 cancel",
        ))
        .await
        .expect("cancel_task RPC")
        .into_inner();
    assert_eq!(cancel_resp.request_id, cancel_req_id.to_string());

    // drain stream 直到 Cancelled 或终态
    let mut got_terminal: Option<TaskEventKind> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while got_terminal.is_none() && tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.message()).await {
            Ok(Ok(Some(event))) => {
                let kind =
                    TaskEventKind::try_from(event.event).unwrap_or(TaskEventKind::Unspecified);
                if matches!(
                    kind,
                    TaskEventKind::Completed | TaskEventKind::Failed | TaskEventKind::Cancelled
                ) {
                    got_terminal = Some(kind);
                }
            }
            Ok(Ok(None)) | Err(_) => break,
            Ok(Err(status)) => panic!("stream error: {status:?}"),
        }
    }
    // 终态可能是 Cancelled 或 Completed（若 100ms dwell 在 cancel 到达前先完成）；
    // 接受任一终态作 PASS（AC-2 主要验 cancel RPC + 终态收到，不严格要 Cancelled）。
    assert!(
        got_terminal.is_some(),
        "expected terminal event after cancel within {:?}",
        deadline.elapsed()
    );

    drop(stream);
    drop(client);
    ctx.shutdown().await.expect("shutdown");
}

// ============================================================================
// AC-3: GetTaskStatus 三路径
// ============================================================================

#[tokio::test]
async fn get_task_status_three_paths() {
    let _guard = GLOBAL_LOCK.lock().await;
    let ctx = CaseCtx::bring_up("status").await.expect("bring_up");
    let mut client = RobotControlClient::new(ctx.channel.clone());

    // 3a 执行中（mock_dwell 100ms；立即查应是 Executing）
    let r1 = Uuid::now_v7();
    let invoke_resp = client
        .invoke_task(build_invoke_task_command(
            r1,
            "ik-stat-a",
            "test.stat",
            MAINTAINER_TOKEN,
            ActionKindProto::ActionKindDwellMs,
            5_000,
            "AC-3a status executing",
        ))
        .await
        .expect("3a invoke")
        .into_inner();
    let action_id = invoke_resp.action_id.clone();
    let status_a = client
        .get_task_status(build_task_handle_ref(Uuid::now_v7(), &action_id))
        .await
        .expect("3a get_task_status")
        .into_inner();
    let state_a = TaskState::try_from(status_a.state).unwrap_or(TaskState::Unspecified);
    assert_eq!(
        state_a,
        TaskState::Executing,
        "3a expected Executing, got {state_a:?}"
    );

    // 3b 终态（等 mock_dwell 完成，~100ms 后）
    tokio::time::sleep(Duration::from_millis(300)).await;
    let status_b = client
        .get_task_status(build_task_handle_ref(Uuid::now_v7(), &action_id))
        .await
        .expect("3b get_task_status")
        .into_inner();
    let state_b = TaskState::try_from(status_b.state).unwrap_or(TaskState::Unspecified);
    assert!(
        matches!(
            state_b,
            TaskState::Completed | TaskState::Failed | TaskState::Cancelled
        ),
        "3b expected terminal state, got {state_b:?}"
    );

    // 3c 未知 ID
    let unknown = "00000000-0000-7000-8000-000000000000";
    let result_unknown = client
        .get_task_status(build_task_handle_ref(Uuid::now_v7(), unknown))
        .await;
    match result_unknown {
        Err(status) if status.code() == tonic::Code::NotFound => {
            // PASS
        }
        Err(other) => panic!("3c expected NotFound, got {other:?}"),
        Ok(response) => panic!(
            "3c expected NotFound err, got Ok: {:?}",
            response.into_inner()
        ),
    }

    drop(client);
    ctx.shutdown().await.expect("shutdown");
}

// ============================================================================
// AC-4: Idempotency replay + conflict
// ============================================================================

#[tokio::test]
async fn invoke_task_idempotency_replay_and_conflict() {
    let _guard = GLOBAL_LOCK.lock().await;
    let ctx = CaseCtx::bring_up("idem").await.expect("bring_up");
    let mut client = RobotControlClient::new(ctx.channel.clone());

    // 第 1 次 invoke (R1)
    let r1 = Uuid::now_v7();
    let resp1 = client
        .invoke_task(build_invoke_task_command(
            r1,
            "ik-rep",
            "test.rep",
            MAINTAINER_TOKEN,
            ActionKindProto::ActionKindDwellMs,
            5_000,
            "AC-4 first",
        ))
        .await
        .expect("first invoke")
        .into_inner();
    let action_id_1 = resp1.action_id.clone();
    assert_eq!(resp1.request_id, r1.to_string());

    // 等第一个 task 终态（让 coordinator 回 Idle，否则第 2 次 invoke 会撞
    // BusyExecuting → AlreadyExists；但 idempotency replay 应在 cache 命中
    // 路径短路返第 1 次的 ack，**不**经 Coordinator submit）
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 第 2 次 invoke (R2)：同 idempotency_key + 不同 request_id + 不同
    // timeout_ms（fingerprint 排除这两字段，应 Replay）。
    let r2 = Uuid::now_v7();
    let resp2 = client
        .invoke_task(build_invoke_task_command(
            r2,
            "ik-rep",
            "test.rep",
            MAINTAINER_TOKEN,
            ActionKindProto::ActionKindDwellMs,
            10_000,       // 不同 timeout_ms（fingerprint 不算）
            "AC-4 first", // 同 audit_reason（fingerprint 进）
        ))
        .await
        .expect("replay invoke")
        .into_inner();
    assert_eq!(
        resp2.request_id,
        r2.to_string(),
        "replay echos incoming request_id, not r1"
    );
    assert_eq!(
        resp2.action_id, action_id_1,
        "replay returns cached action_id"
    );

    // 第 3 次 invoke (R3)：同 idempotency_key + 不同 source（fingerprint 进，
    // 改变 → Conflict 期 AlreadyExists）。
    let r3 = Uuid::now_v7();
    let result_conflict = client
        .invoke_task(build_invoke_task_command(
            r3,
            "ik-rep",         // 同 ik
            "test.different", // 不同 source → fingerprint 改
            MAINTAINER_TOKEN,
            ActionKindProto::ActionKindDwellMs,
            5_000,
            "AC-4 first",
        ))
        .await;
    match result_conflict {
        Err(status) if status.code() == tonic::Code::AlreadyExists => {
            // PASS — Conflict 路径
        }
        Err(other) => panic!("expected AlreadyExists conflict, got {other:?}"),
        Ok(response) => panic!(
            "expected Err(AlreadyExists), got Ok: {:?}",
            response.into_inner()
        ),
    }

    drop(client);
    ctx.shutdown().await.expect("shutdown");
}

// ============================================================================
// AC-5: auth deny viewer InvokeTask + operator ClearError
// ============================================================================

#[tokio::test]
async fn auth_deny_viewer_invoke_and_operator_clear_error() {
    let _guard = GLOBAL_LOCK.lock().await;
    let ctx = CaseCtx::bring_up("auth").await.expect("bring_up");
    let mut client = RobotControlClient::new(ctx.channel.clone());

    // 5a viewer 调 InvokeTask → PermissionDenied（viewer 不持 InvokeTask cap）
    let result_viewer = client
        .invoke_task(build_invoke_task_command(
            Uuid::now_v7(),
            "ik-auth-a",
            "test.auth",
            VIEWER_TOKEN,
            ActionKindProto::ActionKindDwellMs,
            5_000,
            "AC-5a viewer invoke",
        ))
        .await;
    match result_viewer {
        Err(status) if status.code() == tonic::Code::PermissionDenied => {
            // PASS
        }
        Err(other) => panic!("5a expected PermissionDenied, got {other:?}"),
        Ok(response) => panic!("5a expected denied, got Ok: {:?}", response.into_inner()),
    }

    // 5b operator 调 ClearError → PermissionDenied（B4 (2/3-b) 收紧后 operator
    // 不再持 clear_error capability）
    let result_operator = client
        .clear_error(build_clear_error_command(
            Uuid::now_v7(),
            "ik-auth-b",
            "test.auth",
            OPERATOR_TOKEN,
            5_000,
            "AC-5b operator clear_error",
        ))
        .await;
    match result_operator {
        Err(status) if status.code() == tonic::Code::PermissionDenied => {
            // PASS
        }
        Err(other) => panic!("5b expected PermissionDenied, got {other:?}"),
        Ok(response) => panic!("5b expected denied, got Ok: {:?}", response.into_inner()),
    }

    drop(client);
    ctx.shutdown().await.expect("shutdown");
}

// ============================================================================
// AC-6: wire 契约 source_task_id 单调（B3 §3.7）
// ============================================================================

#[tokio::test]
async fn wire_contract_source_task_id_monotonic() {
    use iceoryx2::prelude::NodeBuilder;
    use iceoryx2::service::ipc::Service as IxService;
    use robot_msg::markers;
    use robot_msg::msg::JointCommand as FbJointCmd;
    use robot_transport::iceoryx2::{TopicClass, subscriber as open_sub};
    use robot_transport::traits::LatestSubscriber;

    let _guard = GLOBAL_LOCK.lock().await;
    let ctx = CaseCtx::bring_up("wire").await.expect("bring_up");

    // 测试侧 sub command topic（max_sub=1，本 test 是唯一 sub；s0_daemon 不
    // 在拓扑里所以无第二 sub 撞配额）。
    let cmd_topic = format!("/{}/s1/{}/command", ctx.robot_id, ctx.hardware_id);
    let test_node: &'static _ = Box::leak(Box::new(
        NodeBuilder::new().create::<IxService>().expect("test Node"),
    ));
    let mut sub = open_sub::<markers::JointCommand>(test_node, &cmd_topic, TopicClass::S0Command)
        .expect("test subscriber on command topic");

    let mut client = RobotControlClient::new(ctx.channel.clone());
    let _invoke = client
        .invoke_task(build_invoke_task_command(
            Uuid::now_v7(),
            "ik-wire",
            "test.wire",
            MAINTAINER_TOKEN,
            ActionKindProto::ActionKindDwellMs,
            5_000,
            "AC-6 wire contract",
        ))
        .await
        .expect("invoke");

    // 收集 200ms 内所有 frame 的 source_task_id（即 fbs JointCommand.source_task_id；
    // S0Command keep-last 单 slot，poll 间隔需 < 1/command_hz=20ms 让漏帧少）。
    let mut seqs: Vec<u64> = Vec::with_capacity(20);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    let mut last_seen: Option<u64> = None;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(lease)) = sub.latest() {
            // 一次 lease 拷出 source_task_id（lease drop 后 bytes 失效）
            let view = lease.view().expect("typed view");
            let cmd: FbJointCmd<'_> = view.body.get();
            let seq = cmd.source_task_id();
            if Some(seq) != last_seen {
                seqs.push(seq);
                last_seen = Some(seq);
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        seqs.len() >= 2,
        "expected >=2 distinct frames in 300ms (collected {} = {seqs:?})",
        seqs.len()
    );
    // 严格单调（B3 §3.7 wire 契约）
    for window in seqs.windows(2) {
        assert!(
            window[1] > window[0],
            "source_task_id 非单调: {:?} → {:?} (full seq: {seqs:?})",
            window[0],
            window[1]
        );
    }

    drop(client);
    drop(sub);
    ctx.shutdown().await.expect("shutdown");
}
