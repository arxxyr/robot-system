//! 只用配置、内存状态和本机假gRPC验证页面，不创建机器人进程或IPC。

use super::*;
use crate::proto::robot_control_server::{RobotControl, RobotControlServer};
use crate::proto::{
    Ack, CancelTaskCommand, ClearEStopCommand, ClearErrorCommand, EStopCommand, InvokeTaskCommand,
    SetEnabledCommand, StreamRequest, TaskEvent, TaskHandle, TaskHandleRef, TaskStatus,
};
use axum::body::{Body, to_bytes};
use axum::http::Request;
use futures_util::StreamExt;
use robot_core::time::{MockClock, MonoRawNs};
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::{connect_async, tungstenite};
use tonic::{Request as RpcRequest, Response as RpcResponse, Status};
use tower::ServiceExt;

fn source(bus: u8, endpoint: String) -> Source {
    let config_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs");
    let path = config_dir.join(format!("xlerobot-bus{bus}.toml"));
    let robot: RobotConfig = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let hardware = &robot.hardware[0];
    Source {
        id: format!("bus{bus}"),
        label: format!("测试总线{bus}"),
        endpoint,
        robot_id: robot.robot.id,
        hardware_id: hardware.id.clone(),
        mixed_raw: hardware.feetech_bus2_mixed_raw,
        calibration_range_tolerances_raw: hardware.calibration_range_tolerances_raw.clone(),
        layout: load_hardware_layout(&path, &hardware.id).unwrap(),
        motion: None,
        enable_alignment_tolerance_rad: None,
        base_simulation: false,
    }
}

fn sample(source: &Source, stamp_ns: u64) -> RobotState {
    RobotState {
        robot_id: source.robot_id.clone(),
        hardware_id: source.hardware_id.clone(),
        layout_id: source.layout.layout_id,
        stamp_ns,
        position_rad: vec![0.25; source.layout.joints.len()],
        mode: 4,
        control_link: 2,
        output_mode: 4,
        safety_state: 5,
        fault_code: 1,
    }
}

#[test]
fn real_bus_layouts_and_wheel_identity_come_from_config() {
    let config = StatusConfig::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs/xlerobot-status.toml"),
    )
    .unwrap();
    assert_eq!(config.sources.len(), 2);
    assert!(config.preview.enabled);
    assert_eq!(config.cameras.len(), 3);
    assert_eq!(config.cameras[0].id, "bus1:head_color");
    assert_eq!(
        config.cameras[0].topic,
        "/xlerobot_bus1_full/sensors/head_color/color"
    );
    assert_eq!(config.cameras[0].frame_id, "head_color");
    assert_eq!(config.cameras[1].id, "bus1:left_wrist_color");
    assert_eq!(config.cameras[2].id, "bus2:right_wrist_color");
    let left = source(1, "http://127.0.0.1:50061".into());
    let right = source(2, "http://127.0.0.1:50062".into());
    assert_eq!(left.layout.joints.len(), 8);
    assert_eq!(right.layout.joints.len(), 9);
    let right_view = SourceState::new(right).view(1, 2000);
    let find = |name: &str| {
        right_view
            .joints
            .iter()
            .find(|joint| joint.name == name)
            .unwrap()
    };
    assert_eq!(find("base_left_wheel").motor_id, 9);
    assert_eq!(find("base_front_wheel").motor_id, 8);
    assert_eq!(find("base_right_wheel").motor_id, 7);
    assert_eq!(find("base_left_wheel").mode, "velocity_raw");
    assert_eq!(right_view.data_status, "unavailable");
    assert!(right_view.safety_state.is_none());
    assert!(
        right_view
            .joints
            .iter()
            .all(|joint| joint.position_rad.is_none())
    );
}

#[test]
fn source_age_is_not_renewed_by_repeated_delivery() {
    let mut state = SourceState::new(source(1, String::new()));
    let initial = sample(&state.source, 1_000_000_000);
    state.accept(Ok(initial.clone()), 1_010_000_000);
    assert_eq!(state.view(1_010_000_000, 2000).data_status, "fresh");
    state.accept(Ok(initial), 4_000_000_000);
    let view = state.view(4_000_000_000, 2000);
    assert_eq!(view.data_status, "stale");
    assert_eq!(view.age_ms, Some(3000));
    assert_eq!(view.safety_state.as_deref(), Some("AdapterUncertain"));
    state.accept(Err("连接已断开".into()), 4_000_000_000);
    assert_eq!(state.view(4_000_000_000, 2000).data_status, "unavailable");
    assert_eq!(
        state.view(4_000_000_000, 2000).stamp_ns.as_deref(),
        Some("1000000000")
    );
}

#[test]
fn invalid_identity_layout_clock_and_values_never_replace_valid_state() {
    let mut state = SourceState::new(source(1, String::new()));
    let valid = sample(&state.source, 2_000_000_000);
    state.accept(Ok(valid.clone()), 2_001_000_000);
    let mut variants = vec![valid.clone(); 7];
    variants[0].robot_id = "another".into();
    variants[1].hardware_id = "another".into();
    variants[2].layout_id ^= 1;
    variants[3].position_rad.pop();
    variants[4].position_rad[0] = f64::NAN;
    variants[5].stamp_ns = 0;
    variants[6].stamp_ns = 3_000_000_000;
    variants.push(RobotState {
        stamp_ns: 1_000_000_000,
        ..valid.clone()
    });
    for invalid in variants {
        state.accept(Ok(invalid), 2_001_000_000);
        let view = state.view(2_001_000_000, 2000);
        assert_eq!(view.data_status, "invalid");
        assert_eq!(view.stamp_ns.as_deref(), Some("2000000000"));
        assert!(view.error.is_some());
    }
    state.accept(
        Ok(RobotState {
            stamp_ns: 2_002_000_000,
            ..valid
        }),
        2_002_000_000,
    );
    assert_eq!(state.view(2_002_000_000, 2000).data_status, "fresh");
}

#[test]
fn protected_boot_reports_fault_without_claiming_position_feedback() {
    for bus in 1..=2 {
        let mut state = SourceState::new(source(bus, String::new()));
        let mut fault = sample(&state.source, 0);
        fault.position_rad.clear();
        fault.fault_code = 7;
        for now in [1_000_000_000, 5_000_000_000] {
            state.accept(Ok(fault.clone()), now);
            let view = state.view(now, 2000);
            assert_eq!(view.data_status, "invalid");
            assert_eq!(view.mode.as_deref(), Some("Fault"));
            assert_eq!(view.safety_state.as_deref(), Some("AdapterUncertain"));
            assert_eq!(view.fault_code, Some(7));
            assert!(view.error.as_deref().unwrap().contains("尚无位置采样"));
            assert!(view.stamp_ns.is_none() && view.age_ms.is_none());
            assert!(view.joints.iter().all(|joint| joint.position_rad.is_none()));
        }
    }
}

#[test]
fn invalid_positions_report_new_fault_but_keep_last_valid_positions() {
    let mut state = SourceState::new(source(1, String::new()));
    let mut valid = sample(&state.source, 1_000_000_000);
    valid.mode = S0Mode::Ready as i32;
    valid.safety_state = S0SafetyState::TorqueOffConfirmed as i32;
    valid.fault_code = 0;
    state.accept(Ok(valid.clone()), 1_000_000_000);

    let mut fault = valid;
    fault.mode = S0Mode::Fault as i32;
    fault.safety_state = S0SafetyState::AdapterUncertain as i32;
    fault.fault_code = 7;
    fault.position_rad[0] = f64::NAN;
    fault.stamp_ns = 1_500_000_000;
    state.accept(Ok(fault), 1_500_000_000);
    let view = state.view(1_500_000_000, 2000);
    assert_eq!(view.data_status, "invalid");
    assert_eq!(view.mode.as_deref(), Some("Fault"));
    assert_eq!(view.safety_state.as_deref(), Some("AdapterUncertain"));
    assert_eq!(view.fault_code, Some(7));
    assert_eq!(view.stamp_ns.as_deref(), Some("1000000000"));
    assert_eq!(view.age_ms, Some(500));
    assert!(
        view.joints
            .iter()
            .all(|joint| joint.position_rad == Some(0.25))
    );
}

#[test]
fn mismatched_identity_or_layout_cannot_replace_reported_status() {
    for field in 0..3 {
        for has_previous in [false, true] {
            let mut state = SourceState::new(source(1, String::new()));
            let original = sample(&state.source, 1_000_000_000);
            if has_previous {
                state.accept(Ok(original.clone()), 1_000_000_000);
            }
            let mut wrong = original;
            match field {
                0 => wrong.robot_id = "other_robot".into(),
                1 => wrong.hardware_id = "other_hardware".into(),
                _ => wrong.layout_id ^= 1,
            }
            wrong.mode = S0Mode::Enabled as i32;
            wrong.safety_state = S0SafetyState::TorqueOn as i32;
            wrong.fault_code = 0;
            state.accept(Ok(wrong), 1_000_000_000);
            let view = state.view(1_000_000_000, 2000);
            assert_eq!(view.data_status, "invalid");
            assert_eq!(view.mode.as_deref(), has_previous.then_some("Fault"));
            assert_eq!(
                view.safety_state.as_deref(),
                has_previous.then_some("AdapterUncertain")
            );
            assert_eq!(view.fault_code, has_previous.then_some(1));
        }
    }
}

#[test]
fn reported_fault_survives_disconnect_and_positions_recover_only_from_valid_sample() {
    let mut state = SourceState::new(source(1, String::new()));
    let mut fault = sample(&state.source, 0);
    fault.position_rad.clear();
    fault.fault_code = 7;
    state.accept(Ok(fault), 1_000_000_000);
    state.accept(Err("连接已断开".into()), 2_000_000_000);
    let lost = state.view(5_000_000_000, 2000);
    assert_eq!(lost.data_status, "unavailable");
    assert_eq!(lost.error.as_deref(), Some("连接已断开"));
    assert_eq!(lost.mode.as_deref(), Some("Fault"));
    assert_eq!(lost.fault_code, Some(7));
    assert!(lost.stamp_ns.is_none() && lost.age_ms.is_none());

    let mut recovered = sample(&state.source, 5_000_000_000);
    recovered.position_rad.fill(0.5);
    recovered.mode = S0Mode::Ready as i32;
    recovered.safety_state = S0SafetyState::TorqueOffConfirmed as i32;
    recovered.fault_code = 0;
    state.accept(Ok(recovered), 5_000_000_000);
    let fresh = state.view(5_000_000_000, 2000);
    assert_eq!(fresh.data_status, "fresh");
    assert_eq!(fresh.mode.as_deref(), Some("Ready"));
    assert_eq!(fresh.safety_state.as_deref(), Some("TorqueOffConfirmed"));
    assert_eq!(fresh.fault_code, Some(0));
    assert!(
        fresh
            .joints
            .iter()
            .all(|joint| joint.position_rad == Some(0.5))
    );
    let expired = Snapshot {
        stale_after_ms: 2000,
        buses: vec![fresh],
    }
    .at_time(8_000_000_000);
    assert_eq!(expired.buses[0].data_status, "stale");
    assert_eq!(expired.buses[0].age_ms, Some(3000));
}

#[test]
fn config_checks_http_rpc_addresses_duplicates_and_budgets() {
    let parse = |listen: &str, endpoint: &str, stale: u64| ConfigFile {
        listen: listen.parse().unwrap(),
        poll_interval_ms: 500,
        stale_after_ms: stale,
        rpc_timeout_ms: 500,
        preview: camera::Config::default(),
        control: control::FileConfig::default(),
        sources: vec![SourceFile {
            id: "bus1".into(),
            label: "左侧".into(),
            config: "bus1.toml".into(),
            endpoint: endpoint.into(),
            control: None,
        }],
    };
    for listen in [
        "127.0.0.1:8080",
        "[::1]:8080",
        "10.0.0.2:8080",
        "172.16.0.1:8080",
        "172.31.255.254:8080",
        "192.168.1.40:8080",
    ] {
        assert!(validate_file(&parse(listen, "http://127.0.0.1:50061", 2000)).is_ok());
    }
    for listen in [
        "0.0.0.0:8080",
        "8.8.8.8:8080",
        "172.15.255.254:8080",
        "172.32.0.1:8080",
        "192.169.1.40:8080",
        "100.64.0.4:8080",
        "169.254.1.2:8080",
        "224.0.0.1:8080",
        "255.255.255.255:8080",
        "[::]:8080",
        "[::ffff:10.0.0.2]:8080",
        "[fd00::1]:8080",
        "[fe80::1]:8080",
        "[2001:db8::1]:8080",
        "127.0.0.1:0",
        "10.0.0.2:0",
    ] {
        assert!(validate_file(&parse(listen, "http://127.0.0.1:50061", 2000)).is_err());
    }
    assert!(validate_file(&parse("127.0.0.1:8080", "http://10.0.0.2:50061", 2000)).is_err());
    assert!(validate_file(&parse("10.0.0.2:8080", "http://10.0.0.2:50061", 2000)).is_err());
    assert!(validate_file(&parse("127.0.0.1:8080", "http://localhost:50061", 2000)).is_err());
    assert!(validate_file(&parse("127.0.0.1:8080", "http://127.0.0.1:50061", 1)).is_err());
    let mut config = parse("127.0.0.1:8080", "http://127.0.0.1:50061", 2000);
    config.sources.push(SourceFile {
        id: "bus2".into(),
        label: "右侧".into(),
        config: "bus2.toml".into(),
        endpoint: "http://127.0.0.1:50061".into(),
        control: None,
    });
    assert!(validate_file(&config).is_err());
}

#[test]
fn config_without_preview_keeps_camera_subscription_disabled() {
    let config: ConfigFile = toml::from_str(
        r#"
listen = "127.0.0.1:8080"
poll_interval_ms = 500
stale_after_ms = 2000
rpc_timeout_ms = 500
[[sources]]
id = "bus1"
label = "左侧"
config = "xlerobot-bus1.toml"
endpoint = "http://127.0.0.1:50061"
"#,
    )
    .unwrap();
    assert!(!config.preview.enabled);
    assert!(validate_file(&config).is_ok());
}

#[test]
fn deployment_roles_load_without_control_credentials() {
    use crate::auth::{Capability, Role, load_role_table};

    let text = include_str!("../../../configs/rpc-roles-status.toml");
    let config: toml::Value = toml::from_str(text).unwrap();
    assert!(config["tokens"].as_table().unwrap().is_empty());
    let table = load_role_table(text).unwrap();
    assert!(table.role_has_capability(Role::Viewer, Capability::ReadState));
    for role in [Role::Viewer, Role::Operator, Role::Maintainer, Role::Admin] {
        for capability in [
            Capability::InvokeTask,
            Capability::SetEnabled,
            Capability::EStop,
            Capability::ClearEStop,
            Capability::ClearError,
            Capability::ReloadCalibration,
            Capability::SetPolicy,
            Capability::Shutdown,
        ] {
            assert!(!table.role_has_capability(role, capability));
        }
    }
}

pub(super) fn web_state() -> (
    WebState,
    watch::Sender<Snapshot>,
    watch::Sender<bool>,
    mpsc::Receiver<WsConnection>,
) {
    let snapshot = Snapshot {
        stale_after_ms: 2000,
        buses: vec![SourceState::new(source(1, String::new())).view(1, 2000)],
    };
    let (tx, rx) = watch::channel(snapshot);
    let (stop, stopped) = watch::channel(false);
    let (connections, incoming) = mpsc::channel(MAX_STREAMS);
    (
        WebState {
            latest: rx,
            streams: Arc::new(Semaphore::new(MAX_STREAMS)),
            connections,
            shutdown: stopped,
            clock: Arc::new(MockClock::new(MonoRawNs(1))),
            cameras: camera::CameraState::new(camera::Config::default(), Vec::new()),
            resources: resources::ResourcesState::new(Arc::new(MockClock::new(MonoRawNs(1)))),
            commands: mpsc::channel(control::MAILBOX_CAPACITY).0,
            control_enabled: false,
        },
        tx,
        stop,
        incoming,
    )
}

#[tokio::test]
async fn http_recomputes_stale_when_watch_has_not_changed() {
    let (mut state, latest, _stop, _incoming) = web_state();
    let clock = Arc::new(MockClock::new(MonoRawNs(1_000_000_000)));
    state.clock = clock.clone();
    let mut source = SourceState::new(source(1, String::new()));
    source.accept(Ok(sample(&source.source, clock.now().0)), clock.now().0);
    latest
        .send(Snapshot {
            stale_after_ms: 2000,
            buses: vec![source.view(clock.now().0, 2000)],
        })
        .unwrap();
    clock.set(MonoRawNs(4_000_000_000));
    let response = router(state)
        .oneshot(request("GET", "/api/state"))
        .await
        .unwrap();
    let body = to_bytes(response.into_body(), 16_384).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["buses"][0]["data_status"], "stale");
    assert_eq!(body["buses"][0]["age_ms"], 3000);
}

fn request(method: &str, path: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn http_without_token_remains_read_only_and_removes_sse() {
    let (state, _latest, _stop, _incoming) = web_state();
    let app = router(state.clone());
    for path in ["/api/state", "/api/ws"] {
        assert_eq!(
            app.clone()
                .oneshot(request("POST", path))
                .await
                .unwrap()
                .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
    let response = app
        .clone()
        .oneshot(request("GET", "/api/state"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        response.headers()[header::X_CONTENT_TYPE_OPTIONS],
        "nosniff"
    );
    let body = to_bytes(response.into_body(), 16_384).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["buses"][0]["data_status"], "unavailable");
    for path in ["/api/enable", "/api/estop", "/api/move", "/api/clear-estop"] {
        assert_eq!(
            app.clone()
                .oneshot(request("POST", path))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        app.oneshot(request("GET", "/api/events"))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[derive(Clone)]
struct FakeGateway {
    state: Arc<Mutex<Option<RobotState>>>,
    clock: Arc<dyn RobotClock>,
    refresh: bool,
    reads: Arc<AtomicUsize>,
    controls: Arc<AtomicUsize>,
}

impl FakeGateway {
    fn reject<T>(&self) -> Result<RpcResponse<T>, Status> {
        self.controls.fetch_add(1, Ordering::Relaxed);
        Err(Status::unimplemented("测试只允许GetState"))
    }
}

type FakeStream<T> =
    Pin<Box<dyn tonic::codegen::tokio_stream::Stream<Item = Result<T, Status>> + Send>>;

#[tonic::async_trait]
impl RobotControl for FakeGateway {
    async fn get_state(
        &self,
        request: RpcRequest<GetStateRequest>,
    ) -> Result<RpcResponse<RobotState>, Status> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let mut state = self
            .state
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| Status::unavailable("等待状态首帧"))?;
        if request.into_inner().hardware_id != state.hardware_id {
            return Err(Status::not_found("hardware不匹配"));
        }
        if self.refresh {
            state.stamp_ns = self.clock.now().0;
        }
        Ok(RpcResponse::new(state))
    }
    async fn clear_e_stop(
        &self,
        _: RpcRequest<ClearEStopCommand>,
    ) -> Result<RpcResponse<Ack>, Status> {
        self.reject()
    }
    async fn invoke_task(
        &self,
        _: RpcRequest<InvokeTaskCommand>,
    ) -> Result<RpcResponse<TaskHandle>, Status> {
        self.reject()
    }
    async fn cancel_task(
        &self,
        _: RpcRequest<CancelTaskCommand>,
    ) -> Result<RpcResponse<Ack>, Status> {
        self.reject()
    }
    async fn get_task_status(
        &self,
        _: RpcRequest<TaskHandleRef>,
    ) -> Result<RpcResponse<TaskStatus>, Status> {
        self.reject()
    }
    type StreamTaskEventsStream = FakeStream<TaskEvent>;
    async fn stream_task_events(
        &self,
        _: RpcRequest<TaskHandleRef>,
    ) -> Result<RpcResponse<Self::StreamTaskEventsStream>, Status> {
        self.reject()
    }
    async fn clear_error(
        &self,
        _: RpcRequest<ClearErrorCommand>,
    ) -> Result<RpcResponse<Ack>, Status> {
        self.reject()
    }
    type StreamStateStream = FakeStream<RobotState>;
    async fn stream_state(
        &self,
        _: RpcRequest<StreamRequest>,
    ) -> Result<RpcResponse<Self::StreamStateStream>, Status> {
        self.reject()
    }
    async fn set_enabled(
        &self,
        _: RpcRequest<SetEnabledCommand>,
    ) -> Result<RpcResponse<Ack>, Status> {
        self.reject()
    }
    async fn e_stop(&self, _: RpcRequest<EStopCommand>) -> Result<RpcResponse<Ack>, Status> {
        self.reject()
    }
}

async fn start_fake(
    fake: FakeGateway,
) -> (
    String,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, shutdown) = tokio::sync::oneshot::channel();
    let incoming = futures_util::stream::unfold(listener, |listener| async {
        Some((listener.accept().await.map(|(stream, _)| stream), listener))
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
    (format!("http://{address}"), stop, task)
}

async fn http_get(address: SocketAddr, path: &str) -> String {
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    socket
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut output = String::new();
    socket.read_to_string(&mut output).await.unwrap();
    output
}

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next_json(socket: &mut WsClient) -> serde_json::Value {
    let message = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let tungstenite::Message::Text(text) = message else {
        panic!("首帧必须是JSON文本")
    };
    serde_json::from_str(&text).unwrap()
}

async fn next_close(socket: &mut WsClient) -> Option<tungstenite::protocol::CloseFrame> {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match socket.next().await.unwrap().unwrap() {
                tungstenite::Message::Close(frame) => return frame,
                tungstenite::Message::Text(_) => {}
                message => panic!("预期关闭帧，实际为{message:?}"),
            }
        }
    })
    .await
    .unwrap()
}

async fn wait_permits(streams: &Semaphore, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while streams.available_permits() != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn start_router(
    state: WebState,
    incoming: mpsc::Receiver<WsConnection>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let connections = tokio::spawn(manage_connections(incoming, state.clone()));
        let mut shutdown = state.shutdown.clone();
        axum::serve(listener, router(state))
            .with_graceful_shutdown(async move {
                let _ = shutdown.changed().await;
            })
            .await
            .unwrap();
        connections.await.unwrap().unwrap();
    });
    (address, task)
}

#[tokio::test]
async fn websocket_snapshot_age_ping_and_read_only_policy() {
    let (mut state, latest, stop, incoming) = web_state();
    let clock = Arc::new(MockClock::new(MonoRawNs(1_000_000_000)));
    state.clock = clock.clone();
    let streams = Arc::clone(&state.streams);
    let (address, task) = start_router(state, incoming).await;
    let url = format!("ws://{address}/api/ws");
    let (mut socket, _) = connect_async(&url).await.unwrap();
    let first = next_json(&mut socket).await;
    assert_eq!(first["buses"][0]["data_status"], "unavailable");
    assert!(first["buses"][0]["stamp_ns"].is_null());
    socket
        .send(tungstenite::Message::Ping(b"check".as_slice().into()))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        tungstenite::Message::Pong(b"check".as_slice().into()),
    );
    let mut source = SourceState::new(source(1, String::new()));
    source.accept(Ok(sample(&source.source, clock.now().0)), clock.now().0);
    let snapshot = Snapshot {
        stale_after_ms: 2000,
        buses: vec![source.view(clock.now().0, 2000)],
    };
    latest.send(snapshot.clone()).unwrap();
    assert_eq!(
        next_json(&mut socket).await["buses"][0]["data_status"],
        "fresh"
    );
    clock.set(MonoRawNs(4_000_000_000));
    latest.send(snapshot).unwrap();
    let aged = next_json(&mut socket).await;
    assert_eq!(aged["buses"][0]["data_status"], "stale");
    assert_eq!(aged["buses"][0]["age_ms"], 3000);
    assert_eq!(aged["buses"][0]["stamp_ns"], "1000000000");
    socket
        .send(tungstenite::Message::Text(r#"{"enable":true}"#.into()))
        .await
        .unwrap();
    assert_eq!(
        next_close(&mut socket).await.unwrap().code,
        tungstenite::protocol::frame::coding::CloseCode::Policy
    );
    wait_permits(&streams, MAX_STREAMS).await;
    let (mut binary, _) = connect_async(&url).await.unwrap();
    next_json(&mut binary).await;
    binary
        .send(tungstenite::Message::Binary(vec![1, 2, 3].into()))
        .await
        .unwrap();
    assert_eq!(
        next_close(&mut binary).await.unwrap().code,
        tungstenite::protocol::frame::coding::CloseCode::Policy
    );
    wait_permits(&streams, MAX_STREAMS).await;
    let (mut oversized, _) = connect_async(&url).await.unwrap();
    next_json(&mut oversized).await;
    oversized
        .send(tungstenite::Message::Binary(
            vec![0; WS_MAX_INPUT_BYTES + 1].into(),
        ))
        .await
        .unwrap();
    wait_permits(&streams, MAX_STREAMS).await;
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn websocket_capacity_disconnect_and_shutdown_reclaim_connections() {
    let (state, _latest, stop, incoming) = web_state();
    let streams = Arc::clone(&state.streams);
    let (address, task) = start_router(state, incoming).await;
    let url = format!("ws://{address}/api/ws");
    let mut sockets = Vec::new();
    for _ in 0..MAX_STREAMS {
        let (mut socket, _) = connect_async(&url).await.unwrap();
        next_json(&mut socket).await;
        sockets.push(socket);
    }
    assert_eq!(streams.available_permits(), 0);
    let rejected = connect_async(&url).await.unwrap_err();
    let tungstenite::Error::Http(response) = rejected else {
        panic!("应返回HTTP连接上限")
    };
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let mut closing = sockets.pop().unwrap();
    closing.close(None).await.unwrap();
    next_close(&mut closing).await;
    wait_permits(&streams, 1).await;
    let (mut replacement, _) = connect_async(&url).await.unwrap();
    next_json(&mut replacement).await;
    sockets.push(replacement);
    stop.send(true).unwrap();
    // 客户端保持打开且不协助收尾；服务器必须自行退出并归还全部许可。
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(streams.available_permits(), MAX_STREAMS);
    for mut socket in sockets {
        assert!(next_close(&mut socket).await.is_none());
    }
}

#[tokio::test]
async fn blocked_websocket_io_is_bounded_and_shutdown_cancellable() {
    let (stop, mut shutdown) = watch::channel(false);
    assert!(
        !tokio::time::timeout(
            Duration::from_secs(2),
            bounded_ws_io(
                std::future::pending::<Result<(), axum::Error>>(),
                &mut shutdown
            ),
        )
        .await
        .unwrap()
    );
    stop.send(true).unwrap();
    assert!(
        !tokio::time::timeout(
            Duration::from_millis(100),
            bounded_ws_io(
                std::future::pending::<Result<(), axum::Error>>(),
                &mut shutdown
            ),
        )
        .await
        .unwrap()
    );
}

#[tokio::test]
async fn fake_grpc_to_http_and_websocket_never_calls_control() {
    let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
    let fake = FakeGateway {
        state: Arc::new(Mutex::new(Some(sample(
            &source(1, String::new()),
            clock.now().0,
        )))),
        clock,
        refresh: true,
        reads: Arc::new(AtomicUsize::new(0)),
        controls: Arc::new(AtomicUsize::new(0)),
    };
    let (endpoint, fake_stop, fake_task) = start_fake(fake.clone()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, shutdown) = tokio::sync::oneshot::channel();
    let config = StatusConfig {
        listen: address,
        poll_interval_ms: 10_000,
        stale_after_ms: 10_000,
        rpc_timeout_ms: 500,
        sources: vec![source(1, endpoint)],
        preview: camera::Config::default(),
        cameras: Vec::new(),
        control_enabled: false,
        control_token: None,
    };
    let web = tokio::spawn(serve(listener, config, async {
        let _ = shutdown.await;
    }));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let response = http_get(address, "/api/state").await;
            if response.contains("\"data_status\":\"fresh\"") {
                assert!(response.contains("AdapterUncertain"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let (mut socket, response) = connect_async(format!("ws://{address}/api/ws"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    let snapshot = next_json(&mut socket).await;
    assert_eq!(snapshot["buses"][0]["data_status"], "fresh");
    assert_eq!(
        snapshot["buses"][0]["robot_id"],
        source(1, String::new()).robot_id
    );
    let reads_before = fake.reads.load(Ordering::Relaxed);
    let mut additional = Vec::new();
    for _ in 1..MAX_STREAMS {
        let (mut client, _) = connect_async(format!("ws://{address}/api/ws"))
            .await
            .unwrap();
        next_json(&mut client).await;
        additional.push(client);
    }
    // 连接只读取同一个watch快照，不按客户端数量增加GetState调用。
    assert_eq!(fake.reads.load(Ordering::Relaxed), reads_before);
    for message in [
        tungstenite::Message::Text(r#"{"enable":true}"#.into()),
        tungstenite::Message::Binary(vec![1].into()),
    ] {
        let mut client = additional.pop().unwrap();
        client.send(message).await.unwrap();
        assert_eq!(
            next_close(&mut client).await.unwrap().code,
            tungstenite::protocol::frame::coding::CloseCode::Policy
        );
    }
    assert_eq!(fake.controls.load(Ordering::Relaxed), 0);
    // 保持连接且不读数据，关闭仍须回收连接和源轮询。
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), web)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(next_close(&mut socket).await.is_none());
    for mut client in additional {
        assert!(next_close(&mut client).await.is_none());
    }
    drop(socket);
    fake_stop.send(()).unwrap();
    fake_task.await.unwrap();
    assert!(fake.reads.load(Ordering::Relaxed) > 0);
    assert_eq!(fake.controls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn unfinished_http_future_returns_shutdown_budget_error() {
    let (stop, shutdown) = watch::channel(false);
    stop.send(true).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        finish_http(std::future::pending::<std::io::Result<()>>(), shutdown),
    )
    .await
    .expect("HTTP预算路径没有按时退出");
    assert!(result.unwrap_err().contains("HTTP关闭超过2秒预算"));
}

#[test]
fn deterministic_clock_preserves_nanosecond_identity_as_string() {
    let clock = MockClock::new(MonoRawNs(9_007_199_254_740_999));
    let mut state = SourceState::new(source(1, String::new()));
    state.accept(Ok(sample(&state.source, clock.now().0)), clock.now().0);
    let view = state.view(clock.now().0, 2000);
    let json = serde_json::to_value(&view).unwrap();
    assert_eq!(json["stamp_ns"], "9007199254740999");
    assert!(json["layout_id"].is_string());
}

/// 手动浏览器验收夹具：两台假gRPC，无S0/IPC，120秒后回收全部服务。
#[tokio::test]
#[ignore = "本地浏览器验收夹具，仅在明确需要预览时运行"]
async fn browser_preview_fake_sources() {
    let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
    let mut running = Vec::new();
    let mut sources = Vec::new();
    for bus in 1..=2 {
        let fake = FakeGateway {
            state: Arc::new(Mutex::new(Some(sample(
                &source(bus, String::new()),
                clock.now().0,
            )))),
            clock: Arc::clone(&clock),
            refresh: bus == 1,
            reads: Arc::new(AtomicUsize::new(0)),
            controls: Arc::new(AtomicUsize::new(0)),
        };
        let (endpoint, stop, task) = start_fake(fake).await;
        sources.push(source(bus, endpoint));
        running.push((stop, task));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    eprintln!("只读假状态预览：http://{address}，120秒自动退出");
    let config = StatusConfig {
        listen: address,
        poll_interval_ms: 500,
        stale_after_ms: 2000,
        rpc_timeout_ms: 500,
        sources,
        preview: camera::Config::default(),
        cameras: Vec::new(),
        control_enabled: false,
        control_token: None,
    };
    serve(
        listener,
        config,
        tokio::time::sleep(Duration::from_secs(120)),
    )
    .await
    .unwrap();
    for (stop, task) in running {
        let _ = stop.send(());
        task.await.unwrap();
    }
}
