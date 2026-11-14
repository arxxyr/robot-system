//! 假gRPC与真实HTTP/WebSocket的共享资源及控制边界，不创建S0、S1或硬件连接。

use super::*;
use crate::proto::robot_control_server::{RobotControl, RobotControlServer};
use crate::proto::{
    Ack, ActionOutcomeProto, CancelTaskCommand, ClearEStopCommand, ClearErrorCommand, EStopCommand,
    InvokeTaskCommand, SetEnabledCommand, StreamRequest, TaskEvent, TaskHandle, TaskHandleRef,
    TaskState, TaskStatus,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{connect_async, tungstenite};
use tonic::{Request as RpcRequest, Response as RpcResponse, Status};

type FakeStream<T> =
    Pin<Box<dyn tonic::codegen::tokio_stream::Stream<Item = Result<T, Status>> + Send>>;
type Browser =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Default)]
struct Calls {
    connections: AtomicUsize,
    reads: AtomicUsize,
    invokes: AtomicUsize,
    task_reads: AtomicUsize,
    task_streams: AtomicUsize,
    cancellations: AtomicUsize,
    enables: AtomicUsize,
    disables: AtomicUsize,
    estops: AtomicUsize,
    clears: AtomicUsize,
    cancelled: AtomicBool,
    completed: AtomicBool,
    status_unknown: AtomicBool,
}

#[derive(Clone)]
struct FakeControl {
    state: RobotState,
    calls: Arc<Calls>,
    invoke_gate: Option<Arc<Semaphore>>,
    invoked: Arc<tokio::sync::Notify>,
}

fn ack(meta: Option<crate::proto::CommandMeta>) -> RpcResponse<Ack> {
    let meta = meta.unwrap();
    assert_eq!(meta.auth.unwrap().token, "software-only");
    RpcResponse::new(Ack {
        request_id: meta.request_id,
        cleared_at_ns: 1,
        dry_run: false,
    })
}

#[tonic::async_trait]
impl RobotControl for FakeControl {
    async fn get_state(
        &self,
        request: RpcRequest<GetStateRequest>,
    ) -> Result<RpcResponse<RobotState>, Status> {
        self.calls.reads.fetch_add(1, Ordering::Relaxed);
        assert_eq!(request.into_inner().hardware_id, self.state.hardware_id);
        let mut state = self.state.clone();
        state.stamp_ns = SystemClock::new().now().0;
        Ok(RpcResponse::new(state))
    }

    async fn invoke_task(
        &self,
        request: RpcRequest<InvokeTaskCommand>,
    ) -> Result<RpcResponse<TaskHandle>, Status> {
        self.calls.invokes.fetch_add(1, Ordering::Relaxed);
        self.invoked.notify_one();
        let command = request.into_inner();
        assert_eq!(
            command.payload_schema_id,
            robot_msg::schema_id::SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD
        );
        let meta = command.meta.unwrap();
        assert_eq!(meta.auth.unwrap().token, "software-only");
        if let Some(gate) = &self.invoke_gate {
            gate.acquire().await.unwrap().forget();
        }
        Ok(RpcResponse::new(TaskHandle {
            request_id: meta.request_id.clone(),
            action_id: meta.request_id,
        }))
    }

    async fn get_task_status(
        &self,
        request: RpcRequest<TaskHandleRef>,
    ) -> Result<RpcResponse<TaskStatus>, Status> {
        self.calls.task_reads.fetch_add(1, Ordering::Relaxed);
        if self.calls.status_unknown.load(Ordering::Relaxed) {
            return Err(Status::unavailable("假任务状态暂不可确认"));
        }
        let request = request.into_inner();
        let completed = self.calls.completed.load(Ordering::Relaxed);
        Ok(RpcResponse::new(TaskStatus {
            request_id: request.request_id,
            action_id: request.action_id,
            state: if self.calls.cancelled.load(Ordering::Relaxed) {
                TaskState::Cancelled as i32
            } else if completed {
                TaskState::Completed as i32
            } else {
                TaskState::Executing as i32
            },
            progress: if completed { 1.0 } else { 0.5 },
            outcome: completed.then(|| ActionOutcomeProto {
                kind: 1,
                ..Default::default()
            }),
            failure: None,
        }))
    }

    async fn cancel_task(
        &self,
        request: RpcRequest<CancelTaskCommand>,
    ) -> Result<RpcResponse<Ack>, Status> {
        self.calls.cancellations.fetch_add(1, Ordering::Relaxed);
        self.calls.cancelled.store(true, Ordering::Relaxed);
        Ok(ack(request.into_inner().meta))
    }

    async fn set_enabled(
        &self,
        request: RpcRequest<SetEnabledCommand>,
    ) -> Result<RpcResponse<Ack>, Status> {
        let command = request.into_inner();
        if command.enabled {
            self.calls.enables.fetch_add(1, Ordering::Relaxed);
        } else {
            self.calls.disables.fetch_add(1, Ordering::Relaxed);
        }
        Ok(ack(command.meta))
    }

    async fn e_stop(&self, request: RpcRequest<EStopCommand>) -> Result<RpcResponse<Ack>, Status> {
        self.calls.estops.fetch_add(1, Ordering::Relaxed);
        Ok(ack(request.into_inner().meta))
    }

    async fn clear_e_stop(
        &self,
        request: RpcRequest<ClearEStopCommand>,
    ) -> Result<RpcResponse<Ack>, Status> {
        self.calls.clears.fetch_add(1, Ordering::Relaxed);
        let command = request.into_inner();
        assert_eq!(command.confirmation, "CONFIRM_CLEAR_ESTOP");
        Ok(ack(command.meta))
    }

    async fn clear_error(
        &self,
        _: RpcRequest<ClearErrorCommand>,
    ) -> Result<RpcResponse<Ack>, Status> {
        Err(Status::unimplemented("Web不清故障"))
    }

    type StreamStateStream = FakeStream<RobotState>;
    async fn stream_state(
        &self,
        _: RpcRequest<StreamRequest>,
    ) -> Result<RpcResponse<Self::StreamStateStream>, Status> {
        Err(Status::unimplemented("Web复用GetState轮询"))
    }

    type StreamTaskEventsStream = FakeStream<TaskEvent>;
    async fn stream_task_events(
        &self,
        _: RpcRequest<TaskHandleRef>,
    ) -> Result<RpcResponse<Self::StreamTaskEventsStream>, Status> {
        self.calls.task_streams.fetch_add(1, Ordering::Relaxed);
        Err(Status::unimplemented("不按网页创建任务事件订阅"))
    }
}

fn source(bus: u8) -> Source {
    let config_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs");
    let path = config_dir.join(format!("xlerobot-bus{bus}.toml"));
    let robot: RobotConfig = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let hardware = &robot.hardware[0];
    let mut source = Source {
        id: format!("bus{bus}"),
        label: format!("测试总线{bus}"),
        endpoint: String::new(),
        robot_id: robot.robot.id,
        hardware_id: hardware.id.clone(),
        mixed_raw: hardware.feetech_bus2_mixed_raw,
        layout: load_hardware_layout(&path, &hardware.id).unwrap(),
        motion: None,
    };
    source.motion = Some(
        control::MotionConfig::load(
            &config_dir.join(format!("xlerobot-bus{bus}-motion.toml")),
            &source,
        )
        .unwrap(),
    );
    source
}

struct Fixture {
    address: SocketAddr,
    calls: Vec<Arc<Calls>>,
    invoked: Arc<tokio::sync::Notify>,
    invoke_gate: Option<Arc<Semaphore>>,
    web_stop: oneshot::Sender<()>,
    web: tokio::task::JoinHandle<Result<(), String>>,
    gateways: Vec<(oneshot::Sender<()>, tokio::task::JoinHandle<()>)>,
}

impl Fixture {
    async fn start(poll_ms: u64, hold_invoke: bool, rpc_timeout_ms: u64) -> Self {
        let mut sources = Vec::new();
        let mut calls = Vec::new();
        let mut gateways = Vec::new();
        let invoke_gate = hold_invoke.then(|| Arc::new(Semaphore::new(0)));
        let invoked = Arc::new(tokio::sync::Notify::new());
        for bus in 1..=2 {
            let mut source = source(bus);
            let count = Arc::new(Calls::default());
            let fake = FakeControl {
                state: RobotState {
                    robot_id: source.robot_id.clone(),
                    hardware_id: source.hardware_id.clone(),
                    layout_id: source.layout.layout_id,
                    stamp_ns: 1,
                    position_rad: vec![0.0; source.layout.joints.len()],
                    mode: S0Mode::Enabled as i32,
                    control_link: S0ControlLink::Ok as i32,
                    output_mode: S0OutputMode::Active as i32,
                    safety_state: S0SafetyState::TorqueOn as i32,
                    fault_code: 0,
                },
                calls: Arc::clone(&count),
                invoke_gate: invoke_gate.clone(),
                invoked: Arc::clone(&invoked),
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            source.endpoint = format!("http://{}", listener.local_addr().unwrap());
            let (stop, shutdown) = oneshot::channel();
            let connections = Arc::clone(&count);
            let incoming =
                futures_util::stream::unfold((listener, connections), |(listener, count)| async {
                    let accepted = listener.accept().await.map(|(stream, _)| stream);
                    if accepted.is_ok() {
                        count.connections.fetch_add(1, Ordering::Relaxed);
                    }
                    Some((accepted, (listener, count)))
                });
            let task = tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(RobotControlServer::new(fake))
                    .serve_with_incoming_shutdown(incoming, async {
                        let _ = shutdown.await;
                    })
                    .await
                    .unwrap();
            });
            sources.push(source);
            calls.push(count);
            gateways.push((stop, task));
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let config = StatusConfig {
            listen: address,
            poll_interval_ms: poll_ms,
            stale_after_ms: 60_000,
            rpc_timeout_ms,
            sources,
            preview: camera::Config::default(),
            cameras: Vec::new(),
            control_enabled: true,
            control_token: Some("software-only".into()),
        };
        let (web_stop, shutdown) = oneshot::channel();
        let web = tokio::spawn(serve(listener, config, async {
            let _ = shutdown.await;
        }));
        Self {
            address,
            calls,
            invoked,
            invoke_gate,
            web_stop,
            web,
            gateways,
        }
    }

    async fn browser(&self, origin: Option<&str>) -> Browser {
        let mut request = format!("ws://{}/api/ws", self.address)
            .into_client_request()
            .unwrap();
        if let Some(origin) = origin {
            request
                .headers_mut()
                .insert("origin", origin.parse().unwrap());
        }
        connect_async(request).await.unwrap().0
    }

    async fn same_origin(&self) -> Browser {
        self.browser(Some(&format!("http://{}", self.address)))
            .await
    }

    async fn finish(self) {
        if let Some(gate) = self.invoke_gate {
            gate.add_permits(2);
        }
        self.web_stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.web)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        for (stop, task) in self.gateways {
            stop.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
        }
    }
}

async fn next_matching(browser: &mut Browser, matches: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let tungstenite::Message::Text(text) = browser.next().await.unwrap().unwrap() {
                let value: Value = serde_json::from_str(&text).unwrap();
                if matches(&value) {
                    return value;
                }
            }
        }
    })
    .await
    .unwrap()
}

async fn fresh(browser: &mut Browser) -> Value {
    next_matching(browser, |value| {
        value["buses"].as_array().is_some_and(|buses| {
            buses.len() == 2 && buses.iter().all(|bus| bus["data_status"] == "fresh")
        })
    })
    .await
}

fn command(snapshot: &Value, id: &str, operation: &str) -> Value {
    let control = &snapshot["buses"][0]["control"];
    let mut request = json!({"type":"control", "request_id":id, "bus_id":"bus1",
        "process_id":control["process_id"], "revision":control["revision"], "operation":operation});
    if operation == "move" {
        request["joint"] = "left_arm_wrist_roll".into();
        request["target_position_rad"] = 0.1.into();
        request["duration_ms"] = 10_000.into();
    }
    request
}

async fn send(browser: &mut Browser, request: &Value) {
    browser
        .send(tungstenite::Message::Text(request.to_string().into()))
        .await
        .unwrap();
}

async fn reply(browser: &mut Browser, id: &str) -> Value {
    next_matching(browser, |value| {
        value["type"] == "control_result" && value["request_id"] == id
    })
    .await
}

#[tokio::test]
async fn multiple_pages_share_connections_polling_and_one_dispatch() {
    // 首次采样后留出足够长的固定周期，以严格核对开页面不会触发额外RPC。
    let fixture = Fixture::start(30_000, false, 200).await;
    let mut first = fixture.same_origin().await;
    let initial = fresh(&mut first).await;
    let mut second = fixture.same_origin().await;
    fresh(&mut second).await;
    let request = command(&initial, "shared-once", "move");
    send(&mut first, &request).await;
    send(&mut second, &request).await;
    let accepted = reply(&mut first, "shared-once").await;
    assert_eq!(accepted["status"], "accepted");
    assert_eq!(reply(&mut second, "shared-once").await, accepted);
    let mut extra = Vec::new();
    for _ in 0..3 {
        let mut browser = fixture.same_origin().await;
        let snapshot = fresh(&mut browser).await;
        assert_eq!(
            snapshot["buses"][0]["control"]["task"]["action_id"],
            accepted["action_id"]
        );
        extra.push(browser);
    }
    assert_eq!(fixture.calls[0].invokes.load(Ordering::Relaxed), 1);
    for calls in &fixture.calls {
        assert_eq!(calls.connections.load(Ordering::Relaxed), 1);
        assert_eq!(calls.reads.load(Ordering::Relaxed), 1);
        assert_eq!(calls.task_reads.load(Ordering::Relaxed), 0);
        assert_eq!(calls.task_streams.load(Ordering::Relaxed), 0);
    }
    drop((first, second, extra));
    fixture.finish().await;
}

#[tokio::test]
async fn absent_or_cross_origin_and_enable_send_no_control_rpc() {
    let fixture = Fixture::start(30_000, false, 200).await;
    let mut owner = fixture.same_origin().await;
    let initial = fresh(&mut owner).await;
    for (index, origin) in [None, Some("https://another.invalid")]
        .into_iter()
        .enumerate()
    {
        let mut browser = fixture.browser(origin).await;
        fresh(&mut browser).await;
        let id = format!("origin-{index}");
        send(&mut browser, &command(&initial, &id, "move")).await;
        assert_eq!(reply(&mut browser, &id).await["status"], "rejected");
    }
    send(&mut owner, &command(&initial, "no-web-enable", "enable")).await;
    assert_eq!(
        reply(&mut owner, "no-web-enable").await["status"],
        "rejected"
    );
    assert_eq!(fixture.calls[0].invokes.load(Ordering::Relaxed), 0);
    assert_eq!(fixture.calls[0].enables.load(Ordering::Relaxed), 0);
    drop(owner);
    fixture.finish().await;
}

#[tokio::test]
async fn disconnected_inflight_timeout_stays_unknown_and_is_not_replayed() {
    let fixture = Fixture::start(10, true, 200).await;
    fixture.calls[0]
        .status_unknown
        .store(true, Ordering::Relaxed);
    let mut first = fixture.same_origin().await;
    let initial = fresh(&mut first).await;
    let request = command(&initial, "unknown-once", "move");
    send(&mut first, &request).await;
    tokio::time::timeout(Duration::from_secs(3), fixture.invoked.notified())
        .await
        .unwrap();
    drop(first);
    let mut second = fixture.same_origin().await;
    let unknown = next_matching(&mut second, |value| {
        value["buses"][0]["control"]["last_result"]["status"] == "unknown"
    })
    .await;
    assert_eq!(unknown["buses"][0]["control"]["busy"], true);
    send(&mut second, &request).await;
    assert_eq!(
        reply(&mut second, "unknown-once").await["status"],
        "unknown"
    );
    send(&mut second, &command(&unknown, "cannot-overwrite", "move")).await;
    assert_eq!(
        reply(&mut second, "cannot-overwrite").await["status"],
        "rejected"
    );
    next_matching(&mut second, |value| {
        value["buses"][0]["control"]["task"]["error"]
            .as_str()
            .is_some_and(|text| text.contains("任务查询失败"))
    })
    .await;
    assert_eq!(fixture.calls[0].invokes.load(Ordering::Relaxed), 1);
    assert!(fixture.calls[0].task_reads.load(Ordering::Relaxed) > 0);
    assert_eq!(fixture.calls[0].task_streams.load(Ordering::Relaxed), 0);
    drop(second);
    fixture.finish().await;
}

#[tokio::test]
async fn shared_cancel_disable_and_explicit_clear_keep_distinct_semantics() {
    let fixture = Fixture::start(10, false, 200).await;
    let mut first = fixture.same_origin().await;
    let initial = fresh(&mut first).await;
    let mut second = fixture.same_origin().await;
    fresh(&mut second).await;
    send(&mut first, &command(&initial, "start-task", "move")).await;
    assert_eq!(reply(&mut first, "start-task").await["status"], "accepted");
    let executing = next_matching(&mut second, |value| {
        value["buses"][0]["control"]["task"]["state"] == "executing"
    })
    .await;
    send(&mut second, &command(&executing, "cancel-task", "cancel")).await;
    assert_eq!(
        reply(&mut second, "cancel-task").await["status"],
        "accepted"
    );
    let mut latest = next_matching(&mut first, |value| {
        value["buses"][0]["control"]["task"]["state"] == "cancelled"
    })
    .await;
    assert_eq!(fixture.calls[0].disables.load(Ordering::Relaxed), 0);
    for operation in ["disable", "estop"] {
        let request = command(&latest, operation, operation);
        send(&mut first, &request).await;
        let result = reply(&mut first, operation).await;
        assert_eq!(result["status"], "accepted");
        latest = next_matching(&mut second, |value| {
            value["buses"][0]["control"]["revision"] == result["revision"]
        })
        .await;
    }
    let mut clear = command(&latest, "clear-separate", "clear_estop");
    send(&mut first, &clear).await;
    assert_eq!(
        reply(&mut first, "clear-separate").await["status"],
        "rejected"
    );
    assert_eq!(fixture.calls[0].clears.load(Ordering::Relaxed), 0);
    clear["confirmation"] = "CONFIRM_CLEAR_ESTOP".into();
    send(&mut first, &clear).await;
    assert_eq!(
        reply(&mut first, "clear-separate").await["status"],
        "accepted"
    );
    let calls = &fixture.calls[0];
    assert_eq!(calls.cancellations.load(Ordering::Relaxed), 1);
    assert_eq!(calls.disables.load(Ordering::Relaxed), 1);
    assert_eq!(calls.estops.load(Ordering::Relaxed), 1);
    assert_eq!(calls.clears.load(Ordering::Relaxed), 1);
    assert_eq!(calls.enables.load(Ordering::Relaxed), 0);
    assert_eq!(calls.task_streams.load(Ordering::Relaxed), 0);
    drop((first, second));
    fixture.finish().await;
}

#[tokio::test]
async fn pending_connection_rejects_second_commands_without_losing_first_result() {
    let fixture = Fixture::start(30_000, true, 2000).await;
    let mut browser = fixture.same_origin().await;
    let initial = fresh(&mut browser).await;
    send(&mut browser, &command(&initial, "pending-move", "move")).await;
    tokio::time::timeout(Duration::from_secs(3), fixture.invoked.notified())
        .await
        .unwrap();
    for operation in ["cancel", "disable", "estop"] {
        send(&mut browser, &command(&initial, operation, operation)).await;
        let result = reply(&mut browser, operation).await;
        assert_eq!(result["status"], "rejected");
        assert!(result["message"].as_str().unwrap().contains("待处理请求"));
    }
    fixture.invoke_gate.as_ref().unwrap().add_permits(1);
    assert_eq!(
        reply(&mut browser, "pending-move").await["status"],
        "accepted"
    );
    let calls = &fixture.calls[0];
    assert_eq!(calls.invokes.load(Ordering::Relaxed), 1);
    assert_eq!(calls.cancellations.load(Ordering::Relaxed), 0);
    assert_eq!(calls.disables.load(Ordering::Relaxed), 0);
    assert_eq!(calls.estops.load(Ordering::Relaxed), 0);
    drop(browser);
    fixture.finish().await;
}

#[tokio::test]
async fn task_completion_is_shared_without_implicit_disable() {
    let fixture = Fixture::start(10, false, 200).await;
    let mut first = fixture.same_origin().await;
    let initial = fresh(&mut first).await;
    let mut second = fixture.same_origin().await;
    fresh(&mut second).await;
    send(&mut first, &command(&initial, "complete-task", "move")).await;
    let accepted = reply(&mut first, "complete-task").await;
    assert_eq!(accepted["status"], "accepted");
    fixture.calls[0].completed.store(true, Ordering::Relaxed);
    for browser in [&mut first, &mut second] {
        let complete = next_matching(browser, |value| {
            value["buses"][0]["control"]["task"]["state"] == "completed"
        })
        .await;
        assert_eq!(
            complete["buses"][0]["control"]["task"]["action_id"],
            accepted["action_id"]
        );
        assert_eq!(complete["buses"][0]["control"]["busy"], false);
    }
    assert_eq!(fixture.calls[0].invokes.load(Ordering::Relaxed), 1);
    assert_eq!(fixture.calls[0].disables.load(Ordering::Relaxed), 0);
    assert_eq!(fixture.calls[0].task_streams.load(Ordering::Relaxed), 0);
    drop((first, second));
    fixture.finish().await;
}
