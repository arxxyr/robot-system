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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{connect_async, tungstenite};
use tonic::{Request as RpcRequest, Response as RpcResponse, Status};

type FakeStream<T> =
    Pin<Box<dyn tonic::codegen::tokio_stream::Stream<Item = Result<T, Status>> + Send>>;
type Browser =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[cfg(feature = "test-base-command")]
mod base_panel {
    use super::*;

    async fn fixture(hold: bool, timeout: u64) -> Fixture {
        let mut right = source(2);
        right.base_simulation = true;
        Fixture::start_with_sources(100, hold, timeout, false, vec![right]).await
    }

    fn message(snapshot: &Value, hold: &str, seq: u64, operation: &str, revision: &Value) -> Value {
        let bus = &snapshot["buses"][0];
        let mut value = json!({"type":"control", "request_id":format!("base-{seq}-{operation}"),
            "bus_id":bus["id"], "process_id":bus["control"]["process_id"], "revision":revision,
            "operation":operation,"hold_id":hold,"seq":seq.to_string()});
        if operation == "base_hold" {
            value["wheels_raw"] = json!({"front":0,"left":-500,"right":500});
        }
        value
    }

    #[tokio::test]
    async fn base_hold_renews_one_action_on_one_shared_connection_and_owner_stops() {
        let fixture = fixture(false, 100).await;
        let mut first = fixture.same_origin().await;
        let initial = next_matching(&mut first, |v| {
            v["buses"][0]["control"]["base"]["can_hold"] == true
        })
        .await;
        let hold = uuid::Uuid::now_v7().to_string();
        let mut revision = initial["buses"][0]["control"]["revision"].clone();
        for seq in 1..=3 {
            let request = message(&initial, &hold, seq, "base_hold", &revision);
            send(&mut first, &request).await;
            let ack = reply(&mut first, request["request_id"].as_str().unwrap()).await;
            assert_eq!(ack["status"], "accepted");
            assert_eq!(ack["action_id"], hold);
            revision = ack["revision"].clone();
        }
        let mut second = fixture.same_origin().await;
        next_matching(&mut second, |value| {
            value["buses"]
                .as_array()
                .is_some_and(|buses| buses.len() == 1 && buses[0]["data_status"] == "fresh")
        })
        .await;
        let steal = message(&initial, &hold, 4, "base_hold", &revision);
        send(&mut second, &steal).await;
        assert_eq!(
            reply(&mut second, steal["request_id"].as_str().unwrap()).await["status"],
            "rejected"
        );
        let stop = message(&initial, &hold, 4, "base_stop", &json!("0"));
        send(&mut first, &stop).await;
        assert_eq!(
            reply(&mut first, stop["request_id"].as_str().unwrap()).await["status"],
            "accepted"
        );
        next_matching(&mut first, |v| {
            v["buses"][0]["control"]["base"]["can_hold"] == true
                && v["buses"][0]["control"]["base"]["active"] == false
        })
        .await;
        let calls = &fixture.calls[0];
        let invocations = calls.base_invocations.lock().unwrap().clone();
        assert_eq!(invocations.len(), 3);
        let mut previous_sequence = 0;
        for (index, invocation) in invocations.iter().enumerate() {
            let meta = invocation.meta.as_ref().unwrap();
            assert_eq!(meta.request_id, hold);
            assert_eq!(
                meta.source,
                if index == 0 {
                    "web-base-start"
                } else {
                    "web-base-renew"
                }
            );
            let wire =
                flatbuffers::root::<robot_msg::msg::JointCommand<'_>>(&invocation.payload).unwrap();
            assert_eq!(
                wire.raw_velocities().unwrap().iter().collect::<Vec<_>>(),
                [0, 0, 0, 0, 0, 0, 500, 0, -500]
            );
            assert!(wire.target().unwrap().iter().all(|v| v == 0.0));
            assert!(wire.source_task_id() > previous_sequence);
            previous_sequence = wire.source_task_id();
        }
        assert_eq!(calls.cancellations.load(Ordering::Relaxed), 1);
        assert_eq!(calls.connections.load(Ordering::Relaxed), 1);
        first.close(None).await.unwrap();
        second.close(None).await.unwrap();
        fixture.finish().await;
    }

    #[tokio::test]
    async fn base_disconnect_cancels_once_and_reconnect_does_not_resume() {
        let fixture = fixture(false, 100).await;
        let mut browser = fixture.same_origin().await;
        let initial = next_matching(&mut browser, |v| {
            v["buses"][0]["control"]["base"]["can_hold"] == true
        })
        .await;
        let hold = uuid::Uuid::now_v7().to_string();
        let start = message(
            &initial,
            &hold,
            1,
            "base_hold",
            &initial["buses"][0]["control"]["revision"],
        );
        send(&mut browser, &start).await;
        let ack = reply(&mut browser, start["request_id"].as_str().unwrap()).await;
        assert_eq!(ack["status"], "accepted");
        browser.close(None).await.unwrap();
        let mut reconnect = fixture.same_origin().await;
        let stopped = next_matching(&mut reconnect, |v| {
            v["buses"][0]["control"]["task"]["state"] == "cancelled"
        })
        .await;
        let old = message(
            &stopped,
            &hold,
            2,
            "base_hold",
            &stopped["buses"][0]["control"]["revision"],
        );
        send(&mut reconnect, &old).await;
        assert_eq!(
            reply(&mut reconnect, old["request_id"].as_str().unwrap()).await["status"],
            "rejected"
        );
        assert_eq!(fixture.calls[0].invokes.load(Ordering::Relaxed), 1);
        assert_eq!(fixture.calls[0].cancellations.load(Ordering::Relaxed), 1);
        reconnect.close(None).await.unwrap();
        fixture.finish().await;
    }

    #[tokio::test]
    async fn base_stop_preempts_pending_reply_without_repeating_unknown_invoke() {
        let fixture = fixture(true, 150).await;
        let mut browser = fixture.same_origin().await;
        let initial = next_matching(&mut browser, |v| {
            v["buses"][0]["control"]["base"]["can_hold"] == true
        })
        .await;
        let hold = uuid::Uuid::now_v7().to_string();
        let start = message(
            &initial,
            &hold,
            1,
            "base_hold",
            &initial["buses"][0]["control"]["revision"],
        );
        send(&mut browser, &start).await;
        fixture.invoked.notified().await;
        let stop = message(&initial, &hold, 2, "base_stop", &json!("0"));
        send(&mut browser, &stop).await;
        assert_eq!(
            reply(&mut browser, stop["request_id"].as_str().unwrap()).await["status"],
            "accepted"
        );
        assert_eq!(fixture.calls[0].invokes.load(Ordering::Relaxed), 1);
        assert_eq!(fixture.calls[0].cancellations.load(Ordering::Relaxed), 1);
        browser.close(None).await.unwrap();
        fixture.finish().await;
    }
}

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
    torque_off: AtomicBool,
    enable_unknown: AtomicBool,
    mirror_mode: AtomicUsize,
    mirror_wheel_slot: AtomicUsize,
    restart_mirror_on_read: AtomicBool,
    frozen_stamp_ns: AtomicU64,
    mirror_sequence: AtomicU64,
    base_invocations: std::sync::Mutex<Vec<InvokeTaskCommand>>,
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
        if self
            .calls
            .restart_mirror_on_read
            .swap(false, Ordering::Relaxed)
        {
            self.calls.mirror_mode.store(4, Ordering::Release);
            tokio::time::timeout(Duration::from_millis(250), async {
                while self.calls.mirror_mode.load(Ordering::Acquire) == 4 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .map_err(|_| Status::unavailable("假镜像重建超时"))?;
        }
        assert_eq!(request.into_inner().hardware_id, self.state.hardware_id);
        let mut state = self.state.clone();
        let frozen_stamp_ns = self.calls.frozen_stamp_ns.load(Ordering::Acquire);
        state.stamp_ns = if frozen_stamp_ns == 0 {
            SystemClock::new().now().0
        } else {
            frozen_stamp_ns
        };
        if self.calls.torque_off.load(Ordering::Relaxed) {
            state.mode = S0Mode::Safe as i32;
            state.output_mode = S0OutputMode::Hold as i32;
            state.safety_state = S0SafetyState::TorqueOffConfirmed as i32;
        }
        Ok(RpcResponse::new(state))
    }

    async fn invoke_task(
        &self,
        request: RpcRequest<InvokeTaskCommand>,
    ) -> Result<RpcResponse<TaskHandle>, Status> {
        self.calls.invokes.fetch_add(1, Ordering::Relaxed);
        self.invoked.notify_one();
        let command = request.into_inner();
        if command.payload_schema_id == robot_msg::schema_id::SCHEMA_JOINT_COMMAND {
            crate::auth::validate_request_id(&command.meta.as_ref().unwrap().request_id)
                .map_err(|error| Status::invalid_argument(error.to_string()))?;
            if command.meta.as_ref().unwrap().source == "web-base-start" {
                self.calls.cancelled.store(false, Ordering::Relaxed);
            }
            self.calls
                .base_invocations
                .lock()
                .unwrap()
                .push(command.clone());
        } else if command.payload_schema_id == robot_msg::schema_id::SCHEMA_RIGHT_ARM_HOME_PAYLOAD {
            let home =
                flatbuffers::root::<robot_msg::msg::RightArmHomePayload<'_>>(&command.payload)
                    .unwrap();
            assert_eq!(home.hardware_id(), self.state.hardware_id);
            assert_eq!(
                command.kind,
                crate::proto::ActionKindProto::ActionKindScripted as i32
            );
            // 模拟配置的腕轴最远端约 3 rad，0.1 rad/s 的轨迹必须允许 20 秒以上及到位观察时间。
            assert!(command.meta.as_ref().unwrap().timeout_ms >= 35_000);
        } else {
            assert_eq!(
                command.payload_schema_id,
                robot_msg::schema_id::SCHEMA_SINGLE_JOINT_POSITION_PAYLOAD
            );
        }
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
            self.calls.torque_off.store(false, Ordering::Relaxed);
            if self.calls.enable_unknown.load(Ordering::Relaxed) {
                return Err(Status::unavailable("假使能已执行但回执丢失"));
            }
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
        feetech_position: hardware.adapter == "feetech_sts3215",
        mixed_raw: hardware.feetech_bus2_mixed_raw,
        calibration_range_tolerances_raw: hardware.calibration_range_tolerances_raw.clone(),
        layout: load_hardware_layout(&path, &hardware.id).unwrap(),
        motion: None,
        enable_alignment_tolerance_rad: None,
        base_simulation: false,
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
    #[cfg(target_os = "linux")]
    sources: Vec<Source>,
    address: SocketAddr,
    calls: Vec<Arc<Calls>>,
    invoked: Arc<tokio::sync::Notify>,
    invoke_gate: Option<Arc<Semaphore>>,
    web_stop: oneshot::Sender<()>,
    web: tokio::task::JoinHandle<Result<(), String>>,
    gateways: Vec<(oneshot::Sender<()>, tokio::task::JoinHandle<()>)>,
    mirrors: Option<(Arc<AtomicBool>, std::thread::JoinHandle<()>)>,
}

impl Fixture {
    async fn start(poll_ms: u64, hold_invoke: bool, rpc_timeout_ms: u64) -> Self {
        Self::start_with_enable(poll_ms, hold_invoke, rpc_timeout_ms, false).await
    }

    async fn start_with_enable(
        poll_ms: u64,
        hold_invoke: bool,
        rpc_timeout_ms: u64,
        enable: bool,
    ) -> Self {
        Self::start_with_sources(
            poll_ms,
            hold_invoke,
            rpc_timeout_ms,
            enable,
            vec![source(1), source(2)],
        )
        .await
    }

    async fn start_with_sources(
        poll_ms: u64,
        hold_invoke: bool,
        rpc_timeout_ms: u64,
        enable: bool,
        configured_sources: Vec<Source>,
    ) -> Self {
        let mut sources = Vec::new();
        let mut calls = Vec::new();
        let mut gateways = Vec::new();
        let invoke_gate = hold_invoke.then(|| Arc::new(Semaphore::new(0)));
        let invoked = Arc::new(tokio::sync::Notify::new());
        for mut source in configured_sources {
            if enable {
                source.robot_id = format!("web_enable_{}", uuid::Uuid::now_v7().simple());
                if source.motion.is_some() {
                    source.enable_alignment_tolerance_rad = Some(0.006_136_5);
                }
            }
            let count = Arc::new(Calls::default());
            count.torque_off.store(enable, Ordering::Relaxed);
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
        let mirrors = enable.then(|| spawn_mirrors(sources.clone(), calls.clone()));
        #[cfg(target_os = "linux")]
        let test_sources = sources.clone();
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
            #[cfg(target_os = "linux")]
            sources: test_sources,
            address,
            calls,
            invoked,
            invoke_gate,
            web_stop,
            web,
            gateways,
            mirrors,
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
        if let Some((stop, worker)) = self.mirrors {
            stop.store(true, Ordering::Release);
            tokio::task::spawn_blocking(move || worker.join().unwrap())
                .await
                .unwrap();
        }
    }
}

/// 只在测试独立命名空间发布软件命令，通过生产transport生成观测镜像。
fn spawn_mirrors(
    sources: Vec<Source>,
    calls: Vec<Arc<Calls>>,
) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    use iceoryx2::prelude::NodeBuilder;
    use iceoryx2::service::ipc::Service;
    use robot_msg::{markers, msg};
    use robot_transport::Publisher;
    use robot_transport::iceoryx2::{TopicClass, publisher};
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let worker = std::thread::spawn(move || {
        let mut config = iceoryx2::config::Config::global_config().clone();
        config.global.node.cleanup_dead_nodes_on_creation = false;
        config.global.node.cleanup_dead_nodes_on_destruction = false;
        let node = NodeBuilder::new()
            .config(&config)
            .create::<Service>()
            .unwrap();
        let open = |source: &Source| {
            publisher::<markers::JointCommand>(
                &node,
                &format!("/{}/s1/{}/command", source.robot_id, source.hardware_id),
                TopicClass::S0Command,
            )
            .unwrap()
        };
        let mut publishers: Vec<_> = sources.iter().map(|source| Some(open(source))).collect();
        let mut seq = 1;
        while !stopped.load(Ordering::Acquire) {
            for (index, source) in sources.iter().enumerate() {
                let mode = calls[index].mirror_mode.load(Ordering::Acquire);
                if mode == 4 {
                    drop(publishers[index].take());
                    publishers[index] = Some(open(source));
                    calls[index].mirror_mode.store(0, Ordering::Release);
                }
                let publisher = publishers[index].as_mut().unwrap();
                let stamp =
                    SystemClock
                        .now()
                        .0
                        .saturating_sub(if mode == 2 { 300_000_000 } else { 0 });
                let mut positions =
                    vec![if mode == 3 { 0.02 } else { 0.0 }; source.layout.joints.len()];
                let layout = source.layout.layout_id.wrapping_add(u64::from(mode == 1));
                let body = if source.mixed_raw {
                    let mut raw_velocities = vec![0; positions.len()];
                    // 仅在独立测试镜像中注入指定轮槽的非零命令。
                    if matches!(mode, 5 | 6) {
                        let slot = calls[index].mirror_wheel_slot.load(Ordering::Acquire);
                        assert!(matches!(source.layout.motor_ids[slot], 7..=9));
                        if mode == 5 {
                            raw_velocities[slot] = 50;
                        } else {
                            positions[slot] = 0.1;
                        }
                    }
                    robot_msg::build_helpers::build_mixed_joint_command(
                        layout,
                        &positions,
                        &raw_velocities,
                        stamp + 200_000_000,
                        0,
                        seq,
                    )
                } else {
                    robot_msg::build_helpers::build_joint_command(
                        layout,
                        msg::ControlMode::Position,
                        &positions,
                        stamp + 200_000_000,
                        0,
                        msg::StaleAction::Hold,
                        seq,
                    )
                };
                let mut writer = publisher.loan(8192).unwrap();
                writer.set_stamp_ns(stamp);
                writer.set_seq(seq);
                writer.set_source_id(2);
                writer.set_body_bytes(&body);
                writer.publish().unwrap();
                calls[index].mirror_sequence.store(seq, Ordering::Release);
            }
            seq += 1;
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    (stop, worker)
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn enable_uses_real_mirror_guard_and_two_pages_dispatch_once() {
    let fixture = Fixture::start_with_enable(20, false, 500, true).await;
    let mut first = fixture.same_origin().await;
    let initial = next_matching(&mut first, |snapshot| {
        snapshot["buses"][0]["control"]["can_enable"] == true
    })
    .await;
    let mut second = fixture.same_origin().await;
    fresh(&mut second).await;
    let request = command(&initial, "enable-once", "enable");
    send(&mut first, &request).await;
    send(&mut second, &request).await;
    let accepted = reply(&mut first, "enable-once").await;
    assert_eq!(accepted["status"], "accepted", "{accepted}");
    assert_eq!(reply(&mut second, "enable-once").await, accepted);
    assert_eq!(fixture.calls[0].enables.load(Ordering::Relaxed), 1);
    assert_eq!(fixture.calls[0].clears.load(Ordering::Relaxed), 0);
    assert_eq!(fixture.calls[0].connections.load(Ordering::Relaxed), 1);
    assert_eq!(fixture.calls[1].enables.load(Ordering::Relaxed), 0);
    drop((first, second));
    fixture.finish().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn right_mixed_enable_and_joint_move_dispatch_once_without_wheel_requests() {
    let fixture = Fixture::start_with_enable(20, false, 500, true).await;
    let mut first = fixture.same_origin().await;
    let initial = next_matching(&mut first, |snapshot| {
        snapshot["buses"][1]["control"]["can_enable"] == true
    })
    .await;
    let joints = initial["buses"][1]["control"]["joints"].as_array().unwrap();
    assert_eq!(joints.len(), 6);
    let wheels: Vec<_> = fixture.sources[1]
        .layout
        .joints
        .iter()
        .zip(&fixture.sources[1].layout.motor_ids)
        .filter(|(_, id)| (7..=9).contains(*id))
        .map(|(name, _)| name)
        .collect();
    assert_eq!(wheels.len(), 3);
    for wheel in &wheels {
        assert!(
            joints
                .iter()
                .all(|joint| joint["name"].as_str() != Some(wheel.as_str()))
        );
    }
    let mut second = fixture.same_origin().await;
    fresh(&mut second).await;
    let enable = right_command(&initial, "right-enable-once", "enable");
    send(&mut first, &enable).await;
    send(&mut second, &enable).await;
    let accepted = reply(&mut first, "right-enable-once").await;
    assert_eq!(accepted["status"], "accepted", "{accepted}");
    assert_eq!(reply(&mut second, "right-enable-once").await, accepted);
    let enabled = next_matching(&mut first, |snapshot| {
        snapshot["buses"][1]["mode"] == "Enabled"
            && snapshot["buses"][1]["safety_state"] == "TorqueOn"
            && snapshot["buses"][1]["control"]["busy"] == false
    })
    .await;
    for (slot, wheel) in wheels.iter().enumerate() {
        let id = format!("right-wheel-denied-{slot}");
        let mut request = right_command(&enabled, &id, "move");
        request["joint"] = (*wheel).clone().into();
        send(&mut first, &request).await;
        let rejected = reply(&mut first, &id).await;
        assert_eq!(rejected["status"], "rejected", "{rejected}");
        assert!(
            rejected["message"]
                .as_str()
                .unwrap()
                .contains("未知或非位置关节"),
            "{rejected}"
        );
    }
    assert_eq!(fixture.calls[1].invokes.load(Ordering::Relaxed), 0);
    let request = right_command(&enabled, "right-move-once", "move");
    send(&mut first, &request).await;
    send(&mut second, &request).await;
    let accepted = reply(&mut first, "right-move-once").await;
    assert_eq!(accepted["status"], "accepted", "{accepted}");
    assert_eq!(reply(&mut second, "right-move-once").await, accepted);
    assert_eq!(fixture.calls[1].enables.load(Ordering::Relaxed), 1);
    assert_eq!(fixture.calls[1].invokes.load(Ordering::Relaxed), 1);
    assert_eq!(fixture.calls[1].connections.load(Ordering::Relaxed), 1);
    assert_eq!(fixture.calls[0].enables.load(Ordering::Relaxed), 0);
    assert_eq!(fixture.calls[0].invokes.load(Ordering::Relaxed), 0);
    assert_eq!(fixture.calls[1].clears.load(Ordering::Relaxed), 0);
    drop((first, second));
    fixture.finish().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn right_mixed_nonzero_wheel_raw_or_position_never_sends_enable() {
    let fixture = Fixture::start_with_enable(20, false, 500, true).await;
    let mut browser = fixture.same_origin().await;
    next_matching(&mut browser, |snapshot| {
        snapshot["buses"][1]["control"]["can_enable"] == true
    })
    .await;
    let right = &fixture.calls[1];
    for (slot, _) in fixture.sources[1]
        .layout
        .motor_ids
        .iter()
        .enumerate()
        .filter(|(_, id)| (7..=9).contains(*id))
    {
        right.mirror_wheel_slot.store(slot, Ordering::Release);
        for mode in [5, 6] {
            right.mirror_mode.store(mode, Ordering::Release);
            let snapshot = next_matching(&mut browser, |snapshot| {
                snapshot["buses"][1]["data_status"] == "fresh"
                    && snapshot["buses"][1]["control"]["can_enable"] == false
                    && snapshot["buses"][1]["control"]["enable_reason"]
                        .as_str()
                        .is_some_and(|reason| reason.contains("三轮零目标"))
            })
            .await;
            let id = format!("right-wheel-command-denied-{slot}-{mode}");
            send(&mut browser, &right_command(&snapshot, &id, "enable")).await;
            let rejected = reply(&mut browser, &id).await;
            assert_eq!(rejected["status"], "rejected", "{rejected}");
            assert_eq!(right.enables.load(Ordering::Relaxed), 0);
            right.mirror_mode.store(0, Ordering::Release);
            next_matching(&mut browser, |snapshot| {
                snapshot["buses"][1]["control"]["can_enable"] == true
            })
            .await;
        }
    }
    assert_eq!(right.invokes.load(Ordering::Relaxed), 0);
    assert_eq!(fixture.calls[0].enables.load(Ordering::Relaxed), 0);
    drop(browser);
    fixture.finish().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn left_only_control_rejects_every_right_bus_write_without_rpc() {
    let mut right = source(2);
    right.motion = None;
    right.enable_alignment_tolerance_rad = None;
    let fixture = Fixture::start_with_sources(20, false, 500, true, vec![source(1), right]).await;
    let mut browser = fixture.same_origin().await;
    let initial = next_matching(&mut browser, |snapshot| {
        snapshot["buses"][0]["control"]["can_enable"] == true
            && snapshot["buses"][1]["data_status"] == "fresh"
    })
    .await;
    let right_control = &initial["buses"][1]["control"];
    assert_eq!(initial["buses"][0]["control"]["enabled"], true);
    assert_eq!(right_control["enabled"], false);
    assert_eq!(right_control["can_enable"], false);
    assert_eq!(right_control["joints"], json!([]));

    // 伪造合法形状和当前版本的右侧请求，证明拒绝来自总线配置边界。
    for operation in [
        "move",
        "right_arm_home",
        "enable",
        "disable",
        "cancel",
        "estop",
        "clear_estop",
    ] {
        let id = format!("right-denied-{operation}");
        let mut request = command(&initial, &id, operation);
        request["bus_id"] = "bus2".into();
        request["process_id"] = right_control["process_id"].clone();
        request["revision"] = right_control["revision"].clone();
        if operation == "move" {
            request["joint"] = "right_arm_wrist_roll".into();
        } else if operation == "clear_estop" {
            request["confirmation"] = "CONFIRM_CLEAR_ESTOP".into();
        }
        send(&mut browser, &request).await;
        let result = reply(&mut browser, &id).await;
        assert_eq!(result["status"], "rejected", "{result}");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("此总线未开放Web控制")
        );
        assert_eq!(result["revision"], "0");
    }

    // 全局控制确实开启，左侧仍能沿同一连接显式使能和提交一次定位。
    send(&mut browser, &command(&initial, "left-enable", "enable")).await;
    assert_eq!(
        reply(&mut browser, "left-enable").await["status"],
        "accepted"
    );
    let enabled = next_matching(&mut browser, |snapshot| {
        snapshot["buses"][0]["mode"] == "Enabled"
            && snapshot["buses"][0]["safety_state"] == "TorqueOn"
    })
    .await;
    send(&mut browser, &command(&enabled, "left-move", "move")).await;
    assert_eq!(reply(&mut browser, "left-move").await["status"], "accepted");
    assert_eq!(fixture.calls[0].enables.load(Ordering::Relaxed), 1);
    assert_eq!(fixture.calls[0].invokes.load(Ordering::Relaxed), 1);
    let right = &fixture.calls[1];
    for count in [
        &right.invokes,
        &right.enables,
        &right.disables,
        &right.cancellations,
        &right.estops,
        &right.clears,
    ] {
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }
    assert_eq!(right.connections.load(Ordering::Relaxed), 1);
    assert!(right.reads.load(Ordering::Relaxed) > 0);
    drop(browser);
    fixture.finish().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn unknown_enable_reply_is_not_replayed_to_gateway() {
    let fixture = Fixture::start_with_enable(20, false, 500, true).await;
    fixture.calls[0]
        .enable_unknown
        .store(true, Ordering::Relaxed);
    let mut browser = fixture.same_origin().await;
    let initial = next_matching(&mut browser, |snapshot| {
        snapshot["buses"][0]["control"]["can_enable"] == true
    })
    .await;
    let request = command(&initial, "enable-unknown", "enable");
    send(&mut browser, &request).await;
    let unknown = reply(&mut browser, "enable-unknown").await;
    assert_eq!(unknown["status"], "unknown", "{unknown}");
    send(&mut browser, &request).await;
    assert_eq!(reply(&mut browser, "enable-unknown").await, unknown);
    assert_eq!(fixture.calls[0].enables.load(Ordering::Relaxed), 1);
    drop(browser);
    fixture.finish().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn invalid_mirror_layout_stale_and_offset_never_send_enable() {
    let fixture = Fixture::start_with_enable(20, false, 500, true).await;
    let mut browser = fixture.same_origin().await;
    next_matching(&mut browser, |snapshot| {
        snapshot["buses"][0]["control"]["can_enable"] == true
    })
    .await;
    for mode in [1, 2, 3] {
        fixture.calls[0].mirror_mode.store(mode, Ordering::Release);
        let snapshot = next_matching(&mut browser, |snapshot| {
            snapshot["buses"][0]["data_status"] == "fresh"
                && snapshot["buses"][0]["control"]["can_enable"] == false
        })
        .await;
        let id = format!("mirror-reject-{mode}");
        send(&mut browser, &command(&snapshot, &id, "enable")).await;
        assert_eq!(reply(&mut browser, &id).await["status"], "rejected");
        assert_eq!(fixture.calls[0].enables.load(Ordering::Relaxed), 0);
        fixture.calls[0].mirror_mode.store(0, Ordering::Release);
        next_matching(&mut browser, |snapshot| {
            snapshot["buses"][0]["control"]["can_enable"] == true
        })
        .await;
    }
    drop(browser);
    fixture.finish().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn frozen_fresh_state_with_advancing_commands_never_enables() {
    // 准备预算100ms短于200ms新鲜窗，拒绝必须来自未推进而非状态陈旧。
    let fixture = Fixture::start_with_enable(20, false, 100, true).await;
    let mut browser = fixture.same_origin().await;
    let snapshot = next_matching(&mut browser, |snapshot| {
        snapshot["buses"][0]["control"]["can_enable"] == true
    })
    .await;
    let calls = &fixture.calls[0];
    let frozen_stamp_ns = SystemClock.now().0;
    calls
        .frozen_stamp_ns
        .store(frozen_stamp_ns, Ordering::Release);
    let reads_before = calls.reads.load(Ordering::Relaxed);
    let commands_before = calls.mirror_sequence.load(Ordering::Acquire);
    send(&mut browser, &command(&snapshot, "frozen-state", "enable")).await;
    let rejected = reply(&mut browser, "frozen-state").await;
    assert_eq!(rejected["status"], "rejected", "{rejected}");
    assert!(
        rejected["message"]
            .as_str()
            .unwrap()
            .contains("未取得两份推进"),
        "{rejected}"
    );
    assert!(
        SystemClock.now().0 - frozen_stamp_ns < 200_000_000,
        "测试结束时冻结帧仍须新鲜"
    );
    assert!(calls.reads.load(Ordering::Relaxed) >= reads_before + 2);
    assert!(calls.mirror_sequence.load(Ordering::Acquire) > commands_before);
    assert_eq!(calls.enables.load(Ordering::Relaxed), 0);
    drop(browser);
    fixture.finish().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn publisher_replacement_during_enable_preparation_is_rejected() {
    // 固定长轮询避免普通状态读取消费本次精确的一次故障注入。
    let fixture = Fixture::start_with_enable(30_000, false, 500, true).await;
    let mut browser = fixture.same_origin().await;
    fresh(&mut browser).await;
    let source = &fixture.sources[0];
    let mirrors = command::Mirrors::new(vec![source.clone()]);
    let (stop, mut stopped) = watch::channel(false);
    let worker = mirrors.start(stopped.clone()).unwrap();
    let mut client = RobotControlClient::new(
        Endpoint::from_shared(source.endpoint.clone())
            .unwrap()
            .connect_lazy(),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = client
                .get_state(GetStateRequest {
                    hardware_id: source.hardware_id.clone(),
                })
                .await
                .unwrap()
                .into_inner();
            if mirrors.check(source, &state, SystemClock.now().0).is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    fixture.calls[0]
        .restart_mirror_on_read
        .store(true, Ordering::Relaxed);
    let result = command::prepare_enable(
        source,
        &mirrors,
        &mut client,
        &SystemClock,
        Duration::from_millis(500),
        &mut stopped,
    )
    .await;
    assert!(result.is_err(), "更换发布实例不能通过当前使能准备");
    assert_eq!(fixture.calls[0].enables.load(Ordering::Relaxed), 0);
    stop.send(true).unwrap();
    tokio::task::spawn_blocking(move || worker.join().unwrap().unwrap())
        .await
        .unwrap();
    drop(browser);
    fixture.finish().await;
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

fn right_command(snapshot: &Value, id: &str, operation: &str) -> Value {
    let mut request = command(snapshot, id, operation);
    let control = &snapshot["buses"][1]["control"];
    request["bus_id"] = "bus2".into();
    request["process_id"] = control["process_id"].clone();
    request["revision"] = control["revision"].clone();
    if operation == "move" {
        request["joint"] = "right_arm_wrist_roll".into();
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
async fn right_arm_home_shares_the_task_identity_replay_and_cancel() {
    let fixture = Fixture::start(10, false, 500).await;
    let mut browser = fixture.same_origin().await;
    let initial = fresh(&mut browser).await;
    let home = right_command(&initial, "home-once", "right_arm_home");
    send(&mut browser, &home).await;
    let accepted = reply(&mut browser, "home-once").await;
    assert_eq!(accepted["status"], "accepted", "{accepted}");
    send(&mut browser, &home).await;
    assert_eq!(reply(&mut browser, "home-once").await, accepted);
    let executing = next_matching(&mut browser, |state| {
        state["buses"][1]["control"]["task"]["state"] == "executing"
    })
    .await;
    assert_eq!(
        executing["buses"][1]["control"]["task"]["action_id"],
        accepted["action_id"]
    );
    send(
        &mut browser,
        &right_command(&executing, "home-busy", "right_arm_home"),
    )
    .await;
    assert_eq!(reply(&mut browser, "home-busy").await["status"], "rejected");
    send(
        &mut browser,
        &right_command(&executing, "home-cancel", "cancel"),
    )
    .await;
    let cancel = reply(&mut browser, "home-cancel").await;
    assert_eq!(cancel["status"], "accepted");
    assert_eq!(cancel["action_id"], accepted["action_id"]);
    next_matching(&mut browser, |state| {
        state["buses"][1]["control"]["task"]["state"] == "cancelled"
    })
    .await;
    let right = &fixture.calls[1];
    assert_eq!(right.invokes.load(Ordering::Relaxed), 1);
    assert_eq!(right.cancellations.load(Ordering::Relaxed), 1);
    assert_eq!(right.enables.load(Ordering::Relaxed), 0);
    assert_eq!(right.clears.load(Ordering::Relaxed), 0);
    assert_eq!(fixture.calls[0].invokes.load(Ordering::Relaxed), 0);
    drop(browser);
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
