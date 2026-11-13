//! 本机gRPC的只读HTTP/SSE视图；配置派生布局，源时钟判断数据新鲜度。
//!
//! 固定轮询任务拥有连接，只保留最新快照；浏览器连接不产生额外RPC。
//! 本模块没有控制RPC、UART或systemd调用，不把服务可达当作电机状态。

use std::collections::HashSet;
use std::convert::Infallible;
use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::{Json, Router};
use robot_core::time::{RobotClock, SystemClock};
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, watch};
use tonic::transport::{Channel, Endpoint};

use crate::auth::{Capability, RoleTable, load_role_table};
use crate::client::layout::{HardwareLayout, load_hardware_layout, validate_state_layout};
use crate::proto::robot_control_client::RobotControlClient;
use crate::proto::{
    GetStateRequest, RobotState, S0ControlLink, S0Mode, S0OutputMode, S0SafetyState,
};

const MAX_SOURCES: usize = 2;
const MAX_STREAMS: usize = 8;
const HTTP_SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    listen: SocketAddr,
    rpc_roles_toml: PathBuf,
    poll_interval_ms: u64,
    stale_after_ms: u64,
    rpc_timeout_ms: u64,
    sources: Vec<SourceFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceFile {
    id: String,
    label: String,
    config: PathBuf,
    endpoint: String,
}

#[derive(Deserialize)]
struct RobotConfig {
    robot: RobotIdentity,
    hardware: Vec<HardwareIdentity>,
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
}

#[derive(Clone)]
struct Source {
    id: String,
    label: String,
    endpoint: String,
    robot_id: String,
    hardware_id: String,
    mixed_raw: bool,
    layout: HardwareLayout,
}

/// 已校验的只读状态配置；所有文件读取均在启动期完成。
pub struct StatusConfig {
    listen: SocketAddr,
    roles: Arc<RoleTable>,
    poll_interval_ms: u64,
    stale_after_ms: u64,
    rpc_timeout_ms: u64,
    sources: Vec<Source>,
}

impl StatusConfig {
    /// 路径以状态配置所在目录为基准，身份及布局只从原机器人TOML派生。
    pub fn load(path: &Path) -> Result<Self, String> {
        let body = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
        let file: ConfigFile = toml::from_str(&body).map_err(|error| error.to_string())?;
        validate_file(&file)?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        let roles_text = std::fs::read_to_string(base.join(&file.rpc_roles_toml))
            .map_err(|error| format!("角色文件读取失败：{error}"))?;
        let roles = Arc::new(load_role_table(&roles_text).map_err(|error| error.to_string())?);
        let mut sources = Vec::with_capacity(file.sources.len());
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
            if hardware.feetech_bus2_mixed_raw
                && (layout.motor_ids.len() != 9
                    || !(1..=9).all(|id| layout.motor_ids.contains(&id)))
            {
                return Err("混合模式须使用现有ID1..9布局".into());
            }
            sources.push(Source {
                id: entry.id,
                label: entry.label,
                endpoint: entry.endpoint,
                robot_id: robot.robot.id,
                hardware_id: hardware.id.clone(),
                mixed_raw: hardware.feetech_bus2_mixed_raw,
                layout,
            });
        }
        Ok(Self {
            listen: file.listen,
            roles,
            poll_interval_ms: file.poll_interval_ms,
            stale_after_ms: file.stale_after_ms,
            rpc_timeout_ms: file.rpc_timeout_ms,
            sources,
        })
    }

    #[must_use]
    pub const fn listen_addr(&self) -> SocketAddr {
        self.listen
    }
}

fn validate_file(file: &ConfigFile) -> Result<(), String> {
    if !file.listen.ip().is_loopback() || file.listen.port() == 0 {
        return Err("HTTP只允许固定端口的loopback地址".into());
    }
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
}

#[derive(Clone, Serialize)]
struct JointView {
    name: String,
    motor_id: u8,
    mode: &'static str,
    position_rad: Option<f64>,
}

struct SourceState {
    source: Source,
    last: Option<RobotState>,
    invalid: bool,
    error: Option<String>,
}

impl SourceState {
    fn new(source: Source) -> Self {
        Self {
            source,
            last: None,
            invalid: false,
            error: Some("等待首个有效状态".into()),
        }
    }

    fn accept(&mut self, result: Result<RobotState, String>, now_ns: u64) {
        match result {
            Err(error) => {
                self.invalid = false;
                self.error = Some(error);
            }
            Ok(state) => match self.validate(&state, now_ns) {
                Ok(()) => {
                    self.last = Some(state);
                    self.invalid = false;
                    self.error = None;
                }
                Err(error) => {
                    self.invalid = true;
                    self.error = Some(error);
                }
            },
        }
    }

    fn validate(&self, state: &RobotState, now_ns: u64) -> Result<(), String> {
        if state.robot_id != self.source.robot_id || state.hardware_id != self.source.hardware_id {
            return Err("状态身份与配置不一致".into());
        }
        validate_state_layout(&self.source.layout, state).map_err(|error| error.to_string())?;
        if state.position_rad.len() != self.source.layout.joints.len()
            || state.position_rad.iter().any(|value| !value.is_finite())
        {
            return Err("关节状态长度错误或含非有限值".into());
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
                .last
                .as_ref()
                .map(|last| enum_name::<S0Mode>(last.mode)),
            control_link: self
                .last
                .as_ref()
                .map(|last| enum_name::<S0ControlLink>(last.control_link)),
            output_mode: self
                .last
                .as_ref()
                .map(|last| enum_name::<S0OutputMode>(last.output_mode)),
            safety_state: self
                .last
                .as_ref()
                .map(|last| enum_name::<S0SafetyState>(last.safety_state)),
            fault_code: self.last.as_ref().map(|last| last.fault_code),
            joints,
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
    roles: Arc<RoleTable>,
    streams: Arc<Semaphore>,
    shutdown: watch::Receiver<bool>,
    clock: Arc<dyn RobotClock>,
}

fn authorize(headers: &HeaderMap, roles: &RoleTable) -> Result<(), StatusCode> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let role = roles.lookup_token(token).ok_or(StatusCode::UNAUTHORIZED)?;
    if roles.role_has_capability(role, Capability::ReadState) {
        Ok(())
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

async fn state_json(State(state): State<WebState>, headers: HeaderMap) -> Response {
    if let Err(status) = authorize(&headers, &state.roles) {
        return status.into_response();
    }
    Json(state.latest.borrow().at_time(state.clock.now().0)).into_response()
}

async fn state_events(State(state): State<WebState>, headers: HeaderMap) -> Response {
    if let Err(status) = authorize(&headers, &state.roles) {
        return status.into_response();
    }
    let Ok(permit) = Arc::clone(&state.streams).try_acquire_owned() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    let mut latest = state.latest;
    let mut shutdown = state.shutdown;
    let stream = async_stream::stream! {
        let _permit = permit;
        loop {
            if *shutdown.borrow() { break; }
            let snapshot = latest.borrow_and_update().at_time(state.clock.now().0);
            match Event::default().event("state").json_data(&snapshot) {
                Ok(event) => yield Ok::<_, Infallible>(event),
                Err(_) => break,
            }
            tokio::select! {
                result = latest.changed() => if result.is_err() { break; },
                _ = shutdown.changed() => break,
            }
        }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn router(state: WebState) -> Router {
    Router::new()
        .route(
            "/",
            axum::routing::get(|| async { Html(include_str!("status_web.html")) }),
        )
        .route("/api/state", axum::routing::get(state_json))
        .route("/api/events", axum::routing::get(state_events))
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
        .with_state(state)
}

/// 在给定本机监听器上提供只读页面；关闭时终止并回收唯一轮询任务。
/// HTTP协作收尾最多等待2秒，超时返回错误，由CLI退出结束残余连接；
/// 本函数不宣称强制销毁Axum内部已派生的连接任务。
pub async fn serve(
    listener: tokio::net::TcpListener,
    config: StatusConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    if !listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .ip()
        .is_loopback()
    {
        return Err("HTTP监听器必须是loopback".into());
    }
    let clock: Arc<dyn RobotClock> = Arc::new(SystemClock::new());
    let initial = Snapshot {
        stale_after_ms: config.stale_after_ms,
        buses: config
            .sources
            .iter()
            .cloned()
            .map(SourceState::new)
            .map(|source| source.view(clock.now().0, config.stale_after_ms))
            .collect(),
    };
    let (latest_tx, latest_rx) = watch::channel(initial);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let http_shutdown = shutdown_rx.clone();
    let state = WebState {
        latest: latest_rx,
        roles: Arc::clone(&config.roles),
        streams: Arc::new(Semaphore::new(MAX_STREAMS)),
        shutdown: shutdown_rx.clone(),
        clock: Arc::clone(&clock),
    };
    let poll = tokio::spawn(poll_sources(config, latest_tx, clock, shutdown_rx));
    let signal = shutdown_tx.clone();
    let server = axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            shutdown.await;
            let _ = signal.send(true);
        })
        .into_future();
    let result = finish_http(server, http_shutdown).await;
    let _ = shutdown_tx.send(true);
    poll.await
        .map_err(|error| format!("状态轮询任务异常：{error}"))??;
    result
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
    mut shutdown: watch::Receiver<bool>,
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
            Ok((
                SourceState::new(source),
                RobotControlClient::new(endpoint.connect_lazy()),
            ))
        })
        .collect::<Result<_, String>>()?;
    let mut interval = tokio::time::interval(Duration::from_millis(config.poll_interval_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = interval.tick() => {},
        }
        for (source, client) in &mut sources {
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
        }
        let now_ns = clock.now().0;
        let snapshot = Snapshot {
            stale_after_ms: config.stale_after_ms,
            buses: sources
                .iter()
                .map(|(source, _)| source.view(now_ns, config.stale_after_ms))
                .collect(),
        };
        if latest.send(snapshot).is_err() {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "status_web_tests.rs"]
mod tests;
