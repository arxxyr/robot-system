//! 只用配置、内存状态和本机假gRPC验证页面，不创建机器人进程或IPC。

use super::*;
use crate::proto::robot_control_server::{RobotControl, RobotControlServer};
use crate::proto::{
    Ack, CancelTaskCommand, ClearEStopCommand, ClearErrorCommand, EStopCommand, InvokeTaskCommand,
    SetEnabledCommand, StreamRequest, TaskEvent, TaskHandle, TaskHandleRef, TaskStatus,
};
use axum::body::{Body, to_bytes};
use axum::http::Request;
use robot_core::time::{MockClock, MonoRawNs};
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tonic::{Request as RpcRequest, Response as RpcResponse, Status};
use tower::ServiceExt;

fn test_roles() -> Arc<RoleTable> {
    Arc::new(
        load_role_table(
            r#"
[tokens]
"status-test-viewer" = "viewer"
"status-test-no-read" = "operator"
[roles.viewer]
capabilities = ["read_state"]
[roles.operator]
capabilities = []
[roles.maintainer]
capabilities = ["clear_estop"]
[roles.admin]
capabilities = ["clear_estop"]
"#,
        )
        .unwrap(),
    )
}

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
        layout: load_hardware_layout(&path, &hardware.id).unwrap(),
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
fn config_rejects_remote_duplicate_sources_and_bad_budgets() {
    let parse = |listen: &str, endpoint: &str, stale: u64| ConfigFile {
        listen: listen.parse().unwrap(),
        rpc_roles_toml: "roles.toml".into(),
        poll_interval_ms: 500,
        stale_after_ms: stale,
        rpc_timeout_ms: 500,
        sources: vec![SourceFile {
            id: "bus1".into(),
            label: "左侧".into(),
            config: "bus1.toml".into(),
            endpoint: endpoint.into(),
        }],
    };
    assert!(validate_file(&parse("0.0.0.0:8080", "http://127.0.0.1:50061", 2000)).is_err());
    assert!(validate_file(&parse("127.0.0.1:8080", "http://10.0.1.40:50061", 2000)).is_err());
    assert!(validate_file(&parse("127.0.0.1:8080", "http://localhost:50061", 2000)).is_err());
    assert!(validate_file(&parse("127.0.0.1:8080", "http://127.0.0.1:50061", 1)).is_err());
    let mut config = parse("127.0.0.1:8080", "http://127.0.0.1:50061", 2000);
    config.sources.push(SourceFile {
        id: "bus2".into(),
        label: "右侧".into(),
        config: "bus2.toml".into(),
        endpoint: "http://127.0.0.1:50061".into(),
    });
    assert!(validate_file(&config).is_err());
}

fn web_state() -> (WebState, watch::Sender<Snapshot>, watch::Sender<bool>) {
    let snapshot = Snapshot {
        stale_after_ms: 2000,
        buses: vec![SourceState::new(source(1, String::new())).view(1, 2000)],
    };
    let (tx, rx) = watch::channel(snapshot);
    let (stop, stopped) = watch::channel(false);
    (
        WebState {
            latest: rx,
            roles: test_roles(),
            streams: Arc::new(Semaphore::new(MAX_STREAMS)),
            shutdown: stopped,
            clock: Arc::new(MockClock::new(MonoRawNs(1))),
        },
        tx,
        stop,
    )
}

#[tokio::test]
async fn http_recomputes_stale_when_watch_has_not_changed() {
    let (mut state, latest, _stop) = web_state();
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
        .oneshot(request("GET", "/api/state", Some("status-test-viewer")))
        .await
        .unwrap();
    let body = to_bytes(response.into_body(), 16_384).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["buses"][0]["data_status"], "stale");
    assert_eq!(body["buses"][0]["age_ms"], 3000);
}

fn request(method: &str, path: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn http_auth_read_only_routes_and_stream_capacity() {
    let (state, _latest, stop) = web_state();
    let app = router(state.clone());
    for path in ["/api/state", "/api/events"] {
        assert_eq!(
            app.clone()
                .oneshot(request("GET", path, None))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.clone()
                .oneshot(request("GET", path, Some("bad")))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.clone()
                .oneshot(request("GET", path, Some("status-test-no-read")))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.clone()
                .oneshot(request("POST", path, Some("status-test-viewer")))
                .await
                .unwrap()
                .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
    let response = app
        .clone()
        .oneshot(request("GET", "/api/state", Some("status-test-viewer")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let body = to_bytes(response.into_body(), 16_384).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["buses"][0]["data_status"], "unavailable");
    for path in ["/api/enable", "/api/estop", "/api/move", "/api/clear-estop"] {
        assert_eq!(
            app.clone()
                .oneshot(request("POST", path, Some("status-test-viewer")))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    let mut responses = Vec::new();
    for _ in 0..MAX_STREAMS {
        let response = app
            .clone()
            .oneshot(request("GET", "/api/events", Some("status-test-viewer")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        responses.push(response);
    }
    assert_eq!(state.streams.available_permits(), 0);
    assert_eq!(
        app.clone()
            .oneshot(request("GET", "/api/events", Some("status-test-viewer")))
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    responses.pop();
    assert_eq!(state.streams.available_permits(), 1);
    stop.send(true).unwrap();
    drop(responses);
    assert_eq!(state.streams.available_permits(), MAX_STREAMS);
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
    let incoming = async_stream::stream! {
        loop { yield listener.accept().await.map(|(stream, _)| stream); }
    };
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
    socket.write_all(format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer status-test-viewer\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut output = String::new();
    socket.read_to_string(&mut output).await.unwrap();
    output
}

#[tokio::test]
async fn fake_grpc_to_http_and_sse_never_calls_control() {
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
        roles: test_roles(),
        poll_interval_ms: 100,
        stale_after_ms: 2000,
        rpc_timeout_ms: 500,
        sources: vec![source(1, endpoint)],
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
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    socket.write_all(format!("GET /api/events HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer status-test-viewer\r\n\r\n").as_bytes()).await.unwrap();
    let output = tokio::time::timeout(Duration::from_secs(5), async {
        let mut output = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let count = socket.read(&mut chunk).await.unwrap();
            assert!(count > 0, "SSE在首个事件之前断开");
            output.extend_from_slice(&chunk[..count]);
            assert!(output.len() <= 16_384);
            if String::from_utf8_lossy(&output).contains("event: state") {
                break String::from_utf8(output).unwrap();
            }
        }
    })
    .await
    .unwrap();
    assert!(output.contains("200 OK") && output.contains("event: state"));
    // 客户端保持SSE连接，服务端仍须在预算内关闭，而非依赖客户端先断开。
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), web)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut trailing = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), socket.read_to_end(&mut trailing))
        .await
        .unwrap()
        .unwrap();
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
    eprintln!("只读假状态预览：http://{address}，测试token=status-test-viewer，120秒自动退出");
    let config = StatusConfig {
        listen: address,
        roles: test_roles(),
        poll_interval_ms: 500,
        stale_after_ms: 2000,
        rpc_timeout_ms: 500,
        sources,
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
