//! 本机gRPC状态、可选单关节控制与RGB镜像Web视图；源时钟判断新鲜度。
//!
//! 固定轮询任务拥有连接，只保留最新快照；浏览器连接不产生额外RPC。
//! 整机procfs资源由独立普通线程共享采样缓存，HTTP查询不触发文件IO。
//! 控制默认关闭；开启后复用固定轮询连接，不访问UART或调用systemd。

use std::collections::HashSet;
use std::future::{Future, IntoFuture};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::{Json, Router};
use futures_util::SinkExt;
use robot_core::time::{RobotClock, SystemClock};
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tonic::transport::{Channel, Endpoint};

use crate::client::layout::{HardwareLayout, load_hardware_layout, validate_state_layout};
use crate::proto::robot_control_client::RobotControlClient;
use crate::proto::{
    GetStateRequest, RobotState, S0ControlLink, S0Mode, S0OutputMode, S0SafetyState,
};

#[path = "status_command.rs"]
mod command;
#[path = "status_control.rs"]
mod control;

#[path = "status_camera.rs"]
mod camera;
#[path = "status_resources.rs"]
mod resources;

const MAX_SOURCES: usize = 2;
const MAX_STREAMS: usize = 8;
const HTTP_SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);
const WS_IO_BUDGET: Duration = Duration::from_millis(500);
const WS_READ_BUFFER_BYTES: usize = 4096;
const WS_MAX_INPUT_BYTES: usize = 1024;
const WS_MAX_SNAPSHOT_BYTES: usize = 65_536;
const WS_MAX_WRITE_BUFFER_BYTES: usize = 131_072;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    listen: SocketAddr,
    poll_interval_ms: u64,
    stale_after_ms: u64,
    rpc_timeout_ms: u64,
    #[serde(default)]
    preview: camera::Config,
    #[serde(default)]
    control: control::FileConfig,
    sources: Vec<SourceFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceFile {
    id: String,
    label: String,
    config: PathBuf,
    endpoint: String,
    #[serde(default)]
    control: Option<control::SourceConfig>,
}

#[derive(Deserialize)]
struct RobotConfig {
    robot: RobotIdentity,
    hardware: Vec<HardwareIdentity>,
    #[serde(default)]
    sensor: Vec<SensorIdentity>,
}

#[derive(Deserialize)]
struct SensorIdentity {
    hardware_id: String,
    id: String,
    adapter: String,
}

#[derive(Deserialize)]
struct RobotIdentity {
    id: String,
}

#[derive(Deserialize)]
struct HardwareIdentity {
    id: String,
    #[serde(default)]
    feetech_bus2_mixed_raw: bool,
    #[serde(default)]
    calibration_range_tolerances_raw: Option<Vec<u16>>,
}

#[derive(Clone)]
struct Source {
    id: String,
    label: String,
    endpoint: String,
    robot_id: String,
    hardware_id: String,
    mixed_raw: bool,
    calibration_range_tolerances_raw: Option<Vec<u16>>,
    layout: HardwareLayout,
    motion: Option<control::MotionConfig>,
    enable_alignment_tolerance_rad: Option<f64>,
}

/// 已校验的状态与可选控制配置；所有文件读取均在启动期完成。
pub struct StatusConfig {
    listen: SocketAddr,
    poll_interval_ms: u64,
    stale_after_ms: u64,
    rpc_timeout_ms: u64,
    sources: Vec<Source>,
    preview: camera::Config,
    cameras: Vec<camera::CameraSource>,
    control_enabled: bool,
    control_token: Option<String>,
}

impl StatusConfig {
    /// 路径以状态配置所在目录为基准，身份及布局只从原机器人TOML派生。
    #[allow(
        clippy::too_many_lines,
        reason = "保持原配置的启动期单次读取与关联校验顺序"
    )]
    pub fn load(path: &Path) -> Result<Self, String> {
        let body = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
        let file: ConfigFile = toml::from_str(&body).map_err(|error| error.to_string())?;
        validate_file(&file)?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        let mut sources = Vec::with_capacity(file.sources.len());
        let mut cameras = Vec::new();
        let mut camera_ids = HashSet::new();
        let mut robot_ids = HashSet::new();
        for entry in file.sources {
            let path = base.join(&entry.config);
            let text = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
            let robot: RobotConfig = toml::from_str(&text).map_err(|error| error.to_string())?;
            if robot.robot.id.trim().is_empty() || !robot_ids.insert(robot.robot.id.clone()) {
                return Err("机器人身份为空或重复".into());
            }
            let [hardware] = robot.hardware.as_slice() else {
                return Err("每个状态源须对应原配置中唯一的hardware".into());
            };
            if hardware.id.trim().is_empty() {
                return Err("hardware身份不能为空".into());
            }
            let layout =
                load_hardware_layout(&path, &hardware.id).map_err(|error| error.to_string())?;
            if hardware
                .calibration_range_tolerances_raw
                .as_ref()
                .is_some_and(|values| {
                    values.len() != layout.joints.len() || values.iter().any(|&value| value > 4095)
                })
            {
                return Err("校准边界容差必须与关节等长且在0..4095内".into());
            }
            if hardware.feetech_bus2_mixed_raw
                && (layout.motor_ids.len() != 9
                    || !(1..=9).all(|id| layout.motor_ids.contains(&id)))
            {
                return Err("混合模式须使用现有ID1..9布局".into());
            }
            for sensor in &robot.sensor {
                if sensor.adapter != "opencv_v4l2" {
                    continue;
                }
                if sensor.hardware_id != hardware.id || sensor.id.trim().is_empty() {
                    return Err("相机所属hardware不匹配或身份为空".into());
                }
                let id = format!("{}:{}", entry.id, sensor.id);
                let topic = format!("/{}/sensors/{}/color", robot.robot.id, sensor.id);
                robot_transport::iceoryx2::observer_topic(&topic)
                    .map_err(|error| format!("相机topic无效：{error}"))?;
                if !camera_ids.insert(id.clone()) || cameras.len() >= 3 {
                    return Err("相机身份重复或超过当前三路预览上限".into());
                }
                let label = match sensor.id.as_str() {
                    "head_color" => "头部相机".to_owned(),
                    "left_wrist_color" => "左腕相机".to_owned(),
                    "right_wrist_color" => "右腕相机".to_owned(),
                    _ => format!("{} · {}", entry.label, sensor.id),
                };
                cameras.push(camera::CameraSource {
                    id,
                    label,
                    topic,
                    frame_id: sensor.id.clone(),
                });
            }
            let mut source = Source {
                id: entry.id,
                label: entry.label,
                endpoint: entry.endpoint,
                robot_id: robot.robot.id,
                hardware_id: hardware.id.clone(),
                mixed_raw: hardware.feetech_bus2_mixed_raw,
                calibration_range_tolerances_raw: hardware.calibration_range_tolerances_raw.clone(),
                layout,
                motion: None,
                enable_alignment_tolerance_rad: None,
            };
            if let Some(control) = entry.control {
                control.validate()?;
                source.enable_alignment_tolerance_rad = control.enable_alignment_tolerance_rad;
                source.motion = Some(control::MotionConfig::load(
                    &base.join(control.motion_config),
                    &source,
                )?);
            }
            sources.push(source);
        }
        let control_token = if file.control.enabled {
            if !sources.iter().any(|source| source.motion.is_some()) {
                return Err("开启Web控制必须引用至少一个总线运动配置".into());
            }
            Some(
                std::env::var("ROBOT_TOKEN")
                    .ok()
                    .filter(|token| !token.trim().is_empty())
                    .ok_or("开启Web控制需要服务端ROBOT_TOKEN；浏览器不传令牌")?,
            )
        } else {
            None
        };
        Ok(Self {
            listen: file.listen,
            poll_interval_ms: file.poll_interval_ms,
            stale_after_ms: file.stale_after_ms,
            rpc_timeout_ms: file.rpc_timeout_ms,
            sources,
            preview: file.preview,
            cameras,
            control_enabled: file.control.enabled,
            control_token,
        })
    }

    #[must_use]
    pub const fn control_enabled(&self) -> bool {
        self.control_enabled
    }

    #[must_use]
    pub const fn listen_addr(&self) -> SocketAddr {
        self.listen
    }
}

fn validate_http_listen(address: SocketAddr) -> Result<(), String> {
    // Web可由同网段Nginx接入；不放开通配、公网或gRPC监听。
    let allowed = match address.ip() {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
        IpAddr::V6(ip) => ip.is_loopback(),
    };
    if !allowed || address.port() == 0 {
        return Err("HTTP只允许固定端口的loopback或显式IPv4 RFC1918私网地址".into());
    }
    Ok(())
}

fn validate_file(file: &ConfigFile) -> Result<(), String> {
    validate_http_listen(file.listen)?;
    file.preview.validate()?;
    if !(100..=10_000).contains(&file.poll_interval_ms)
        || !(50..=5_000).contains(&file.rpc_timeout_ms)
        || !(file.poll_interval_ms..=60_000).contains(&file.stale_after_ms)
    {
        return Err("轮询、RPC超时或陈旧判定时限无效".into());
    }
    if file.sources.is_empty() || file.sources.len() > MAX_SOURCES {
        return Err("状态源数量须为1或2".into());
    }
    let mut ids = HashSet::new();
    let mut endpoints = HashSet::new();
    for source in &file.sources {
        if source.id.trim().is_empty() || source.label.trim().is_empty() || !ids.insert(&source.id)
        {
            return Err("状态源id/label不能为空，id不能重复".into());
        }
        let Some(address) = source.endpoint.strip_prefix("http://") else {
            return Err("gRPC须为http://加本机字面地址".into());
        };
        let address: SocketAddr = address.parse().map_err(|_| "gRPC须为本机IP与端口")?;
        if !address.ip().is_loopback() || address.port() == 0 || !endpoints.insert(address) {
            return Err("gRPC须为不重复的本机固定端口".into());
        }
    }
    Ok(())
}

#[derive(Clone, Serialize)]
struct Snapshot {
    stale_after_ms: u64,
    buses: Vec<BusView>,
}

impl Snapshot {
    // HTTP读取时也按源时间更新，轮询任务停顿不能把旧缓存永久显示为fresh。
    fn at_time(&self, now_ns: u64) -> Self {
        let mut snapshot = self.clone();
        for bus in &mut snapshot.buses {
            if let Some(stamp) = bus
                .stamp_ns
                .as_ref()
                .and_then(|stamp| stamp.parse::<u64>().ok())
            {
                if stamp > now_ns {
                    bus.age_ms = None;
                    if matches!(bus.data_status, "fresh" | "stale") {
                        bus.data_status = "invalid";
                        bus.error = Some("本机时钟早于源状态，无法确认新鲜度".into());
                    }
                } else {
                    let age_ms = (now_ns - stamp) / 1_000_000;
                    bus.age_ms = Some(age_ms);
                    if bus.data_status == "fresh" && age_ms > snapshot.stale_after_ms {
                        bus.data_status = "stale";
                    }
                }
            }
        }
        snapshot
    }
}

#[derive(Clone, Serialize)]
struct BusView {
    id: String,
    label: String,
    robot_id: String,
    hardware_id: String,
    layout_id: String,
    data_status: &'static str,
    error: Option<String>,
    age_ms: Option<u64>,
    stamp_ns: Option<String>,
    mode: Option<String>,
    control_link: Option<String>,
    output_mode: Option<String>,
    safety_state: Option<String>,
    fault_code: Option<u32>,
    joints: Vec<JointView>,
    control: control::View,
}

#[derive(Clone, Serialize)]
struct JointView {
    name: String,
    motor_id: u8,
    mode: &'static str,
    position_rad: Option<f64>,
}

struct ReportedStatus {
    mode: i32,
    control_link: i32,
    output_mode: i32,
    safety_state: i32,
    fault_code: u32,
}

impl From<&RobotState> for ReportedStatus {
    fn from(state: &RobotState) -> Self {
        Self {
            mode: state.mode,
            control_link: state.control_link,
            output_mode: state.output_mode,
            safety_state: state.safety_state,
            fault_code: state.fault_code,
        }
    }
}

struct SourceState {
    source: Source,
    // 保护待机可报告故障但没有位置采样；软件状态不能随位置校验一起丢失。
    reported: Option<ReportedStatus>,
    last: Option<RobotState>,
    invalid: bool,
    error: Option<String>,
    control: control::ControlState,
    commands: Option<command::Mirrors>,
}

impl SourceState {
    fn new(source: Source) -> Self {
        Self {
            source,
            reported: None,
            last: None,
            invalid: false,
            error: Some("等待首个有效状态".into()),
            control: control::ControlState::new(false, ""),
            commands: None,
        }
    }

    fn accept(&mut self, result: Result<RobotState, String>, now_ns: u64) {
        match result {
            Err(error) => {
                self.invalid = false;
                self.error = Some(error);
            }
            Ok(state) => {
                if let Err(error) = self.validate_identity(&state) {
                    self.invalid = true;
                    self.error = Some(error);
                    return;
                }
                self.reported = Some(ReportedStatus::from(&state));
                match self.validate_positions(&state, now_ns) {
                    Ok(()) => {
                        self.last = Some(state);
                        self.invalid = false;
                        self.error = None;
                    }
                    Err(error) => {
                        self.invalid = true;
                        self.error = Some(error);
                    }
                }
            }
        }
    }

    fn validate_identity(&self, state: &RobotState) -> Result<(), String> {
        if state.robot_id != self.source.robot_id || state.hardware_id != self.source.hardware_id {
            return Err("状态身份与配置不一致".into());
        }
        validate_state_layout(&self.source.layout, state).map_err(|error| error.to_string())
    }

    fn validate_positions(&self, state: &RobotState, now_ns: u64) -> Result<(), String> {
        if state.position_rad.is_empty() {
            return Err("尚无位置采样，不能确认位置与数据新鲜度".into());
        }
        if state.position_rad.len() != self.source.layout.joints.len() {
            return Err("关节状态长度与配置不一致".into());
        }
        if state.position_rad.iter().any(|value| !value.is_finite()) {
            return Err("关节位置含非有限值".into());
        }
        if state.stamp_ns == 0 || state.stamp_ns > now_ns {
            return Err("源时间戳为零或在未来，无法确认新鲜度".into());
        }
        if self
            .last
            .as_ref()
            .is_some_and(|last| state.stamp_ns < last.stamp_ns)
        {
            return Err("源时间戳回退，拒绝覆盖已确认状态".into());
        }
        Ok(())
    }

    fn view(&self, now_ns: u64, stale_after_ms: u64) -> BusView {
        let age_ms = self
            .last
            .as_ref()
            .map(|last| now_ns.saturating_sub(last.stamp_ns) / 1_000_000);
        let data_status = if self.invalid {
            "invalid"
        } else if self.error.is_some() || self.last.is_none() {
            "unavailable"
        } else if age_ms.is_some_and(|age| age > stale_after_ms) {
            "stale"
        } else {
            "fresh"
        };
        let joints = self
            .source
            .layout
            .joints
            .iter()
            .zip(&self.source.layout.motor_ids)
            .enumerate()
            .map(|(index, (name, &motor_id))| {
                let velocity = self.source.mixed_raw && (7..=9).contains(&motor_id);
                JointView {
                    name: name.clone(),
                    motor_id,
                    mode: if velocity { "velocity_raw" } else { "position" },
                    position_rad: if velocity {
                        None
                    } else {
                        self.last.as_ref().map(|last| last.position_rad[index])
                    },
                }
            })
            .collect();
        let mut control = self.control.view(self.source.motion.as_ref());
        if control.enabled && !control.busy {
            let enable = if self.source.enable_alignment_tolerance_rad.is_none() {
                Err("未配置使能原位对齐容差".to_owned())
            } else if data_status == "fresh" {
                self.commands
                    .as_ref()
                    .ok_or_else(|| "S1命令镜像未开启".to_owned())
                    .and_then(|commands| {
                        let state = self.last.as_ref().ok_or("尚无完整反馈")?;
                        commands.check(&self.source, state, now_ns)
                    })
            } else {
                Err("使能前需要新鲜、有效的完整反馈".to_owned())
            };
            control.can_enable = enable.is_ok();
            control.enable_reason = enable
                .err()
                .unwrap_or_else(|| "原位已对齐；使能时仍会重新核对两帧".into());
        }
        BusView {
            id: self.source.id.clone(),
            label: self.source.label.clone(),
            robot_id: self.source.robot_id.clone(),
            hardware_id: self.source.hardware_id.clone(),
            layout_id: self.source.layout.layout_id.to_string(),
            data_status,
            error: self.error.clone(),
            age_ms,
            stamp_ns: self.last.as_ref().map(|last| last.stamp_ns.to_string()),
            mode: self
                .reported
                .as_ref()
                .map(|last| enum_name::<S0Mode>(last.mode)),
            control_link: self
                .reported
                .as_ref()
                .map(|last| enum_name::<S0ControlLink>(last.control_link)),
            output_mode: self
                .reported
                .as_ref()
                .map(|last| enum_name::<S0OutputMode>(last.output_mode)),
            safety_state: self
                .reported
                .as_ref()
                .map(|last| enum_name::<S0SafetyState>(last.safety_state)),
            fault_code: self.reported.as_ref().map(|last| last.fault_code),
            joints,
            control,
        }
    }
}

fn enum_name<T: TryFrom<i32> + std::fmt::Debug>(value: i32) -> String {
    T::try_from(value).map_or_else(
        |_| format!("Unknown({value})"),
        |state| format!("{state:?}"),
    )
}

#[derive(Clone)]
struct WebState {
    latest: watch::Receiver<Snapshot>,
    streams: Arc<Semaphore>,
    connections: mpsc::Sender<WsConnection>,
    shutdown: watch::Receiver<bool>,
    clock: Arc<dyn RobotClock>,
    cameras: camera::CameraState,
    resources: resources::ResourcesState,
    commands: mpsc::Sender<control::Envelope>,
    control_enabled: bool,
}

struct WsConnection {
    socket: WebSocket,
    permit: OwnedSemaphorePermit,
    same_origin: bool,
}

async fn state_json(State(state): State<WebState>) -> Response {
    Json(state.latest.borrow().at_time(state.clock.now().0)).into_response()
}

async fn state_socket(
    State(state): State<WebState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let same_origin = same_origin(&headers);
    if *state.shutdown.borrow() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Ok(permit) = Arc::clone(&state.streams).try_acquire_owned() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    upgrade
        .read_buffer_size(WS_READ_BUFFER_BYTES)
        .write_buffer_size(0)
        .max_write_buffer_size(WS_MAX_WRITE_BUFFER_BYTES)
        .max_message_size(WS_MAX_INPUT_BYTES)
        .max_frame_size(WS_MAX_INPUT_BYTES)
        .on_upgrade(move |socket| async move {
            // Axum负责握手；升级完成后立即转交，所有持续连接任务由管理器持有。
            let _ = state.connections.try_send(WsConnection {
                socket,
                permit,
                same_origin,
            });
        })
}

async fn websocket_loop(connection: WsConnection, state: WebState) {
    let WsConnection {
        mut socket,
        permit,
        same_origin,
    } = connection;
    let _permit = permit;
    let mut latest = state.latest;
    let mut shutdown = state.shutdown;
    let mut send_snapshot = true;
    let mut close_frame = None;
    let mut pending: Option<oneshot::Receiver<control::Reply>> = None;
    loop {
        if *shutdown.borrow() {
            break;
        }
        if send_snapshot {
            let snapshot = latest.borrow_and_update().at_time(state.clock.now().0);
            let Ok(json) = serde_json::to_string(&snapshot) else {
                break;
            };
            if json.len() > WS_MAX_SNAPSHOT_BYTES
                || !bounded_ws_io(socket.send(Message::Text(json.into())), &mut shutdown).await
            {
                break;
            }
        }
        send_snapshot = false;
        tokio::select! {
            _ = shutdown.changed() => break,
            result = latest.changed() => {
                if result.is_err() { break; }
                send_snapshot = true;
            },
            reply = async { match pending.as_mut() { Some(reply) => reply.await.ok(), None => std::future::pending().await } }, if pending.is_some() => {
                pending = None;
                if let Some(reply) = reply {
                    if !send_control_reply(&mut socket, reply, &mut shutdown).await { break; }
                } else { break; }
            },
            message = socket.recv() => match message {
                Some(Ok(Message::Text(text))) if state.control_enabled => {
                    let Ok(request) = serde_json::from_str::<control::Request>(&text) else {
                        if !send_control_reply(&mut socket, control::Reply::rejected(None, 0, "控制消息格式无效，未发送"), &mut shutdown).await { break; }
                        continue;
                    };
                    let message = if !same_origin { Some("控制请求须来自同源页面，未发送") }
                        else if pending.is_some() { Some("本连接已有待处理请求，未发送") } else { None };
                    if let Some(message) = message {
                        if !send_control_reply(&mut socket, control::Reply::rejected(Some(&request), 0, message), &mut shutdown).await { break; }
                        continue;
                    }
                    let (reply, received) = oneshot::channel();
                    match state.commands.try_send(control::Envelope { request, reply }) {
                        Ok(()) => pending = Some(received),
                        Err(error) => {
                            if !send_control_reply(&mut socket, control::Reply::rejected(Some(&error.into_inner().request), 0, "共享控制入口繁忙或已关闭，未发送"), &mut shutdown).await { break; }
                        }
                    }
                }
                Some(Ok(Message::Text(_) | Message::Binary(_))) => {
                    close_frame = Some(CloseFrame {
                        code: close_code::POLICY,
                        reason: "只读状态连接不接受应用消息".into(),
                    });
                    break;
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                    // 官方协议栈已生成Pong；及时flush，不等待下一次状态采样。
                    if !bounded_ws_io(socket.flush(), &mut shutdown).await { break; }
                    tokio::task::yield_now().await;
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
            }
        }
    }
    // 关闭不等对端无限确认；发完Close并flush后释放socket和连接许可。
    let _ = tokio::time::timeout(WS_IO_BUDGET, async {
        if let Some(frame) = close_frame {
            let _ = socket.send(Message::Close(Some(frame))).await;
        }
        let _ = socket.close().await;
    })
    .await;
}

async fn bounded_ws_io(
    operation: impl Future<Output = Result<(), axum::Error>>,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    if *shutdown.borrow() {
        return false;
    }
    tokio::select! {
        _ = shutdown.changed() => false,
        result = tokio::time::timeout(WS_IO_BUDGET, operation) => matches!(result, Ok(Ok(()))),
    }
}

async fn manage_connections(
    mut connections: mpsc::Receiver<WsConnection>,
    state: WebState,
) -> Result<(), String> {
    let mut tasks = JoinSet::new();
    let mut shutdown = state.shutdown.clone();
    while !*shutdown.borrow() {
        tokio::select! {
            _ = shutdown.changed() => break,
            connection = connections.recv(), if tasks.len() < MAX_STREAMS => match connection {
                Some(connection) => { tasks.spawn(websocket_loop(connection, state.clone())); }
                None => break,
            },
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = result {
                    tracing::warn!(%error, "只读WebSocket连接任务异常");
                }
            }
        }
    }
    // 先拒收后关闭已升级连接，队列内尚未运行的socket直接释放。
    connections.close();
    while connections.try_recv().is_ok() {}
    if tokio::time::timeout(HTTP_SHUTDOWN_BUDGET, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        return Err("WebSocket关闭超过2秒预算，连接任务已取消并回收".into());
    }
    Ok(())
}

fn router(state: WebState) -> Router {
    let cameras = camera::router(state.cameras.clone());
    let resources = resources::router(state.resources.clone());
    Router::new()
        .route(
            "/",
            axum::routing::get(|| async { Html(include_str!("status_web.html")) }),
        )
        .route("/api/state", axum::routing::get(state_json))
        .route("/api/ws", axum::routing::get(state_socket))
        .with_state(state)
        .merge(cameras)
        .merge(resources)
        .layer(axum::middleware::from_fn(
            |request: axum::extract::Request, next: axum::middleware::Next| async {
                let mut response = next.run(request).await;
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                response.headers_mut().insert(
                    header::X_CONTENT_TYPE_OPTIONS,
                    HeaderValue::from_static("nosniff"),
                );
                response
            },
        ))
}

/// 在给定回环或IPv4私网监听器上提供只读页面；关闭时回收轮询及WebSocket任务。
/// HTTP协作收尾最多等待2秒，超时返回错误，由CLI退出结束残余连接；
/// 本函数不宣称强制销毁Axum内部已派生的连接任务。
pub async fn serve(
    listener: tokio::net::TcpListener,
    config: StatusConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    validate_http_listen(listener.local_addr().map_err(|error| error.to_string())?)?;
    let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
    let process_id = uuid::Uuid::now_v7().to_string();
    let initial = initial_snapshot(&config, clock.now().0, &process_id);
    let (latest_tx, latest_rx) = watch::channel(initial);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let http_shutdown = shutdown_rx.clone();
    let (connections_tx, connections_rx) = mpsc::channel(MAX_STREAMS);
    let (commands_tx, commands_rx) = mpsc::channel(control::MAILBOX_CAPACITY);
    let cameras = camera::CameraState::new(config.preview.clone(), config.cameras.clone());
    let camera_worker = if config.preview.enabled {
        Some(
            cameras
                .start(shutdown_rx.clone())
                .map_err(|error| error.to_string())?,
        )
    } else {
        None
    };
    let resources = resources::ResourcesState::new(Arc::clone(&clock));
    let resource_worker = match resources.start(shutdown_rx.clone()) {
        Ok(worker) => worker,
        Err(error) => {
            let _ = shutdown_tx.send(true);
            if let Some(worker) = camera_worker {
                tokio::task::spawn_blocking(move || {
                    worker
                        .join()
                        .map_err(|_| "相机预览线程异常退出".to_owned())?
                })
                .await
                .map_err(|error| format!("相机预览线程回收异常：{error}"))??;
            }
            return Err(format!("资源采样线程启动失败：{error}"));
        }
    };
    let state = WebState {
        latest: latest_rx,
        streams: Arc::new(Semaphore::new(MAX_STREAMS)),
        connections: connections_tx,
        shutdown: shutdown_rx.clone(),
        clock: Arc::clone(&clock),
        cameras,
        resources,
        commands: commands_tx,
        control_enabled: config.control_enabled,
    };
    let poll = tokio::spawn(poll_sources(
        config,
        latest_tx,
        clock,
        shutdown_rx,
        commands_rx,
        process_id,
    ));
    let connections = tokio::spawn(manage_connections(connections_rx, state.clone()));
    let signal = shutdown_tx.clone();
    let server = axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            shutdown.await;
            let _ = signal.send(true);
        })
        .into_future();
    let result = finish_http(server, http_shutdown).await;
    let _ = shutdown_tx.send(true);
    resource_worker.thread().unpark();
    // 先回收轮询、连接、相机和资源采样线程，再传播错误。
    let camera_join = async move {
        match camera_worker {
            Some(worker) => tokio::task::spawn_blocking(move || {
                worker
                    .join()
                    .map_err(|_| "相机预览线程异常退出".to_owned())?
            })
            .await
            .map_err(|error| format!("相机预览线程回收异常：{error}"))?,
            None => Ok(()),
        }
    };
    let resource_join = tokio::task::spawn_blocking(move || {
        resource_worker
            .join()
            .map_err(|_| "资源采样线程异常退出".to_owned())?
    });
    let (poll, connections, camera, resources) =
        tokio::join!(poll, connections, camera_join, resource_join);
    poll.map_err(|error| format!("状态轮询任务异常：{error}"))??;
    connections.map_err(|error| format!("状态连接管理任务异常：{error}"))??;
    camera?;
    resources.map_err(|error| format!("资源采样线程回收异常：{error}"))??;
    result
}

fn initial_snapshot(config: &StatusConfig, now_ns: u64, process_id: &str) -> Snapshot {
    Snapshot {
        stale_after_ms: config.stale_after_ms,
        buses: config
            .sources
            .iter()
            .cloned()
            .map(|source| {
                let mut source = SourceState::new(source);
                source.control = control::ControlState::new(config.control_enabled, process_id);
                source.view(now_ns, config.stale_after_ms)
            })
            .collect(),
    }
}

async fn finish_http(
    server: impl Future<Output = std::io::Result<()>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result.map_err(|error| error.to_string()),
        _ = shutdown.changed() => {
            tokio::time::timeout(HTTP_SHUTDOWN_BUDGET, &mut server)
                .await
                .map_err(|_| "HTTP关闭超过2秒预算，剩余连接由进程退出收尾".to_owned())?
                .map_err(|error| error.to_string())
        }
    }
}

async fn poll_sources(
    config: StatusConfig,
    latest: watch::Sender<Snapshot>,
    clock: Arc<dyn RobotClock>,
    shutdown: watch::Receiver<bool>,
    commands: mpsc::Receiver<control::Envelope>,
    process_id: String,
) -> Result<(), String> {
    let mirrors = (config.control_enabled
        && config
            .sources
            .iter()
            .any(|source| source.enable_alignment_tolerance_rad.is_some()))
    .then(|| command::Mirrors::new(config.sources.clone()));
    let (stop_mirror, stopped) = watch::channel(false);
    let worker = mirrors
        .as_ref()
        .map(|mirror| mirror.start(stopped))
        .transpose()
        .map_err(|error| format!("命令观测线程启动失败：{error}"))?;
    let result = poll_sources_inner(
        config, latest, clock, shutdown, commands, process_id, mirrors,
    )
    .await;
    let _ = stop_mirror.send(true);
    let joined = match worker {
        Some(worker) => tokio::task::spawn_blocking(move || {
            worker
                .join()
                .map_err(|_| "命令观测线程异常退出".to_owned())?
        })
        .await
        .map_err(|error| format!("命令观测线程回收异常：{error}"))?,
        None => Ok(()),
    };
    result.and(joined)
}

async fn poll_sources_inner(
    config: StatusConfig,
    latest: watch::Sender<Snapshot>,
    clock: Arc<dyn RobotClock>,
    mut shutdown: watch::Receiver<bool>,
    mut commands: mpsc::Receiver<control::Envelope>,
    process_id: String,
    mirrors: Option<command::Mirrors>,
) -> Result<(), String> {
    let timeout = Duration::from_millis(config.rpc_timeout_ms);
    let mut sources: Vec<(SourceState, RobotControlClient<Channel>)> = config
        .sources
        .into_iter()
        .map(|source| {
            let endpoint = Endpoint::from_shared(source.endpoint.clone())
                .map_err(|error| error.to_string())?
                .connect_timeout(timeout)
                .timeout(timeout);
            let mut source = SourceState::new(source);
            source.control = control::ControlState::new(config.control_enabled, &process_id);
            source.commands.clone_from(&mirrors);
            Ok((source, RobotControlClient::new(endpoint.connect_lazy())))
        })
        .collect::<Result<_, String>>()?;
    let mut interval = tokio::time::interval(Duration::from_millis(config.poll_interval_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = interval.tick() => {},
            command = commands.recv() => {
                let Some(command) = command else { break; };
                let reply = if let Some((source, client)) = sources.iter_mut().find(|(source, _)| source.source.id == command.request.bus_id) {
                    let state = source.view(clock.now().0, config.stale_after_ms);
                    source.control.dispatch(command.request, &source.source, &state, control::RpcContext { client,
                        token: config.control_token.as_deref().unwrap_or_default(), timeout, shutdown: &mut shutdown,
                        commands: mirrors.as_ref(), clock: clock.as_ref() }).await
                } else { control::Reply::rejected(Some(&command.request), 0, "未知总线，未发送") };
                let _ = command.reply.send(reply);
                publish_snapshot(&sources, &latest, clock.now().0, config.stale_after_ms);
                continue;
            },
        }
        for (source, client) in &mut sources {
            if *shutdown.borrow() {
                return Ok(());
            }
            let request = GetStateRequest {
                hardware_id: source.source.hardware_id.clone(),
            };
            let result = tokio::select! {
                _ = shutdown.changed() => return Ok(()),
                result = tokio::time::timeout(timeout, client.get_state(request)) => match result {
                    Ok(Ok(response)) => Ok(response.into_inner()),
                    Ok(Err(status)) => Err(format!("gRPC {:?}：{}", status.code(), status.message())),
                    Err(_) => Err("gRPC读取超时".into()),
                },
            };
            source.accept(result, clock.now().0);
            source
                .control
                .poll_task(client, timeout, &mut shutdown)
                .await;
        }
        if !publish_snapshot(&sources, &latest, clock.now().0, config.stale_after_ms) {
            break;
        }
    }
    Ok(())
}

fn publish_snapshot(
    sources: &[(SourceState, RobotControlClient<Channel>)],
    latest: &watch::Sender<Snapshot>,
    now_ns: u64,
    stale_after_ms: u64,
) -> bool {
    latest
        .send(Snapshot {
            stale_after_ms,
            buses: sources
                .iter()
                .map(|(source, _)| source.view(now_ns, stale_after_ms))
                .collect(),
        })
        .is_ok()
}

fn same_origin(headers: &HeaderMap) -> bool {
    let (Some(origin), Some(host)) = (
        headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()),
        headers.get(header::HOST).and_then(|v| v.to_str().ok()),
    ) else {
        return false;
    };
    let Ok(uri) = origin.parse::<axum::http::Uri>() else {
        return false;
    };
    matches!(uri.scheme_str(), Some("http" | "https"))
        && uri
            .authority()
            .is_some_and(|authority| authority.as_str().eq_ignore_ascii_case(host))
        && matches!(
            uri.path_and_query()
                .map(axum::http::uri::PathAndQuery::as_str),
            None | Some("/")
        )
}

async fn send_control_reply(
    socket: &mut WebSocket,
    reply: control::Reply,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    let Ok(text) = serde_json::to_string(&reply) else {
        return false;
    };
    bounded_ws_io(socket.send(Message::Text(text.into())), shutdown).await
}

#[cfg(test)]
#[path = "status_web_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "status_control_integration_tests.rs"]
mod control_integration_tests;
