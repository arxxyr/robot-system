//! gRPC Gateway 的真实请求生命周期与拥有关闭资源的外层。
//!
//! [`GatewayRun`] 持有启动资源、原线程/任务句柄、阶段计时器与同一个
//! GatewayLedger；[`run_gateway_with_rr_io`] 和 [`finish_gateway_owned`]
//! 借用该聚合，取消等待后仍可继续等待同一原件。
//!
//! ## 请求与责任
//!
//! - handler 校验身份、role/capability、deadline 和 fingerprint 后，沿同一
//!   ledger/cache 决定重放或准入；原 Placeholder、信用和队列票据不复制。
//! - 原信封进入分类有界队列，RR worker 消费真实 wire 完成；typed 退休事实
//!   决定原缓存终态，随后才移交等待者。错误也按实际退休/副作用事实保存，
//!   不能仅凭 gRPC 错误或调用方取消就删除原在飞责任。
//! - 关闭位只拒绝新准入，不替已排队/在途信封伪造退休；真实传输对象析构后
//!   仍须保留适用的 NoHandle 状态、原信用和未退休缓存。
//!
//! ## 关闭
//!
//! tonic 停止服务后才收 RR worker。标准线程的原 join 聚合移入唯一
//! blocking task，其句柄先装外层再借用等待；Tokio 各阶段沿原计时器等待。
//! 原 join 错误与残余进入 GatewayExit，只有真实空聚合才允许释放；不以
//! timeout、abort 请求或 drop 句柄冒充实际完成。
//!
//! ## 授权边界
//!
//! wire role 仅用于审计，Gateway 执行 token→role→capability 校验；
//! dry_run 透传给 action 层，不能在 Gateway 伪造结果。每次 retry 的
//! request_id 不参与幂等 fingerprint，同 key 的原在飞/退休事实仍由同一
//! ledger/cache 维护。

use std::net::SocketAddr;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
use std::time::Duration;

use crossbeam_channel::{Sender, TrySendError, bounded};
use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::RobotClock;
#[cfg(test)]
use robot_core::time::SystemClock;
use robot_msg::build_helpers::{
    build_clear_estop_request, build_estop_event, build_set_enabled_request,
};
use robot_msg::markers;
use robot_msg::msg::{
    ClearEStopResponse as FbResp, JointState, RpcRole, SetEnabledResponse as FbSetEnabledResp,
};
use thiserror::Error;
use tokio::sync::watch;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;

use crate::auth::{Capability, Role, RoleTable};
use crate::clear_estop::{
    clear_estop_idempotency_fingerprint, rpc_status_to_tonic_code, validate_clear_estop_command,
};
use crate::estop::{estop_idempotency_fingerprint, validate_estop_command};
use crate::estop_pub_thread::{
    EStopPubForward, EStopPubThreadConfig, EStopPubThreadHandle, spawn_estop_pub_thread,
};
pub use crate::gateway_ledger::RrPort;
#[cfg(test)]
use crate::idempotency::Decision;
use crate::idempotency::Method;
use crate::proto::robot_control_server::{RobotControl, RobotControlServer};
use crate::proto::{
    Ack, CancelTaskCommand, ClearEStopCommand, ClearErrorCommand, EStopCommand, GetStateRequest,
    InvokeTaskCommand, RobotState, SetEnabledCommand, StreamRequest, TaskEvent, TaskHandle,
    TaskHandleRef, TaskStatus,
};
use crate::rr_io_thread::{RrIoThreadConfig, RrIoThreadHandle, spawn_rr_io_thread};
use crate::set_enabled::{set_enabled_idempotency_fingerprint, validate_set_enabled_command};
use crate::state_convert::joint_state_fb_to_robot_state;
use crate::state_pool::{StateCache, StateLookup, StatePool};
use std::sync::atomic::Ordering;

/// v0.5 候选 E 段(8)：6 条 rr_io 线程的 watchdog observer label（顺序 =
/// `run_gateway_with_rr_io` spawn 序 = `WorkerKind::RrIo(index)` 索引域）。
const RR_IO_OBSERVER_LABELS: [&str; 6] = [
    "rr_io_clear_estop",
    "rr_io_task_invoke",
    "rr_io_task_cancel",
    "rr_io_task_status",
    "rr_io_clear_error",
    "rr_io_set_enabled",
];

use crate::systemd_probe::{FIRST_SUCCESS_TIMEOUT, TonicProbeMetrics, spawn_tonic_probe};
use robot_health::systemd::{
    LivenessObserver, WatchdogConfig, notify_ready, notify_stopping, spawn_watchdog,
};

// ============================================================================
// GatewayConfig
// ============================================================================

/// `robot_rpc_gateway` 运行期配置。bin 端 CLI parse 后构造。
#[derive(Clone, Debug)]
pub struct GatewayConfig {
    /// gRPC server bind 地址。**必须**是 loopback（`127.0.0.0/8` IPv4 或
    /// `::1` IPv6）；reviewer A1-3 (2/3-a) 拍板硬边界。`--listen-addr`
    /// CLI flag 覆盖；非 loopback 直接拒启动（[`validate_loopback_bind`]）。
    pub listen_addr: SocketAddr,

    /// 机器人 ID。用于构造 iceoryx2 RR topic
    /// `/<robot_id>/control/clear_estop`（A1-3 (2/3-b) 起接 daemon 时用）。
    pub robot_id: String,

    /// idempotency cache TTL（秒）。reviewer 拍板默认 300s（5min）；CLI
    /// `--idempotency-ttl-secs` 覆盖。
    pub idempotency_ttl_secs: u64,

    /// idempotency cache 最大 entry 数（lazy GC 后仍满则新 key 返
    /// `tonic::Code::ResourceExhausted`）。reviewer 拍板默认 4096。
    pub idempotency_max_entries: usize,

    /// gateway 等 RR response 超过 `request.timeout_ms` 后再多等的 grace
    /// 时间（毫秒）。reviewer 拍板默认 500ms（不要 100ms 偏紧）：daemon
    /// 一旦 deadline 前开始 action，可能在 fsync/clear 后才回真实 outcome；
    /// gateway 过早 `DeadlineExceeded` 会制造"副作用已发生但客户端以为
    /// 失败"的歧义。CLI `--rr-timeout-grace-ms` 覆盖。
    pub rr_timeout_grace_ms: u64,

    /// **v0.5 候选 B (2/4)**：要建常驻 state 订阅池的 hardware id 列表（来源 =
    /// bin `--config <PATH>` 解析的 `[[hardware]].id`，D10）。每个 id 对应一条
    /// `/<robot_id>/s0/<id>/state` 常驻 subscriber。**空列表合法**（D-P1：无
    /// `--config` / 无 `[[hardware]]` → 空 state 池 → 所有非空 hardware_id 请求
    /// `NotFound`；B4 既有 clear-estop-only 调用 0 破）。
    pub state_hardware_ids: Vec<String>,
}

impl GatewayConfig {
    /// `127.0.0.1:50051` + 默认 idempotency TTL/max + grace；用于单测 /
    /// fmt 默认值。
    #[must_use]
    pub fn loopback_default() -> Self {
        Self {
            listen_addr: "127.0.0.1:50051".parse().expect("loopback default literal"),
            robot_id: "default".to_string(),
            idempotency_ttl_secs: 300,
            idempotency_max_entries: 4096,
            rr_timeout_grace_ms: 500,
            state_hardware_ids: Vec::new(),
        }
    }

    /// **A1-3 (2/3-b1)** reviewer 拍板：[`run_gateway`] 入口先调本方法
    /// fail-fast 任何无效配置。
    ///
    /// 校验项：
    /// - `listen_addr`：必须 loopback（[`validate_loopback_bind`]）
    /// - `idempotency_ttl_secs > 0`：TTL = 0 让 cache 立即过期，等价于
    ///   完全失效 idempotency；reviewer 拒
    /// - `idempotency_max_entries > 0`：cap = 0 让所有新 key 立即返
    ///   `ResourceExhausted`，gateway 不可用；reviewer 拒
    /// - `robot_id` 非空：用于构造 iceoryx2 RR topic
    ///   `/<robot_id>/control/clear_estop`，空字串会构出非法 topic
    /// - `rr_timeout_grace_ms` 不限（0 也合理，handler 仍按 `timeout_ms` 等）
    ///
    /// # Errors
    ///
    /// 见 [`ConfigError`]。
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_loopback_bind(&self.listen_addr)?;
        if self.idempotency_ttl_secs == 0 {
            return Err(ConfigError::ZeroIdempotencyTtl);
        }
        if self.idempotency_max_entries == 0 {
            return Err(ConfigError::ZeroIdempotencyMaxEntries);
        }
        let reserves = crate::class_cache::Reserves::DEFAULT;
        let total = reserves
            .emergency
            .checked_add(reserves.lower)
            .and_then(|n| n.checked_add(reserves.restore));
        if total.is_none_or(|total| self.idempotency_max_entries <= total) {
            return Err(ConfigError::InvalidCacheReserves);
        }
        if self.robot_id.is_empty() {
            return Err(ConfigError::EmptyRobotId);
        }
        // state_hardware_ids 整体空合法（D-P1）；但单个 id 不得为空串（会构出
        // 非法 topic `/<robot_id>/s0//state`）。
        if self.state_hardware_ids.iter().any(String::is_empty) {
            return Err(ConfigError::EmptyStateHardwareId);
        }
        Ok(())
    }
}

/// [`GatewayConfig::validate`] 失败时的错误。reviewer A1-3 (2/3-b1)
/// 拍板硬边界：bin 退出码 2（CLI 参数错）。
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ConfigError {
    #[error(transparent)]
    LoopbackBind(#[from] LoopbackBindError),

    #[error("idempotency_ttl_secs must be > 0; cache 立即过期等价于失效")]
    ZeroIdempotencyTtl,

    #[error(
        "idempotency_max_entries must be > 0; cap = 0 会让所有新 key 立即返 \
         ResourceExhausted，gateway 不可用"
    )]
    ZeroIdempotencyMaxEntries,

    #[error("idempotency capacity must exceed the checked emergency/lower/restore reserves sum")]
    InvalidCacheReserves,

    #[error("robot_id must be non-empty (用于构造 iceoryx2 RR topic)")]
    EmptyRobotId,

    #[error("state_hardware_ids 含空 id (会构出非法 topic /<robot_id>/s0//state)")]
    EmptyStateHardwareId,
}

// ============================================================================
// loopback bind 校验
// ============================================================================

/// `validate_loopback_bind` 错误。reviewer A1-3 (2/3-a) 拍板硬边界：plain
/// TCP + fake token 阶段非 loopback 必须拒启动。
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LoopbackBindError {
    /// IP 不是 IPv4 `127.0.0.0/8` / IPv6 `::1`（含 `0.0.0.0` / `[::]` 通配
    /// + 任何外网 / 私网地址）。
    #[error(
        "listen-addr {0} is not a loopback address; v0.3 fake-token + plain TCP \
         requires loopback bind (127.0.0.0/8 or ::1)"
    )]
    NonLoopback(SocketAddr),
}

/// 校验 gRPC bind 地址必须是 loopback。`IpAddr::is_loopback()` 覆盖 IPv4
/// `127.0.0.0/8` 整个 range（含 `127.0.0.1:0` 测试端口）+ IPv6 `::1`。
///
/// reviewer 拍板：v0.3 fake-token + plain TCP 阶段禁止 `0.0.0.0` / 外网；
/// TLS / mTLS 落地后再考虑放行非 loopback。
///
/// # Errors
///
/// 见 [`LoopbackBindError`]。
pub fn validate_loopback_bind(addr: &SocketAddr) -> Result<(), LoopbackBindError> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err(LoopbackBindError::NonLoopback(*addr))
    }
}

// ============================================================================
// GatewayHandler
// ============================================================================

/// `GatewayHandler` 持有的 gateway 运行期状态。bin / `run_gateway_with_rr_io`
/// 构造，handler 借用。`Arc` 包好 thread-safe；tonic clone handler 给每个
/// worker，state 通过 `Arc` 共享。
///
/// **B4 (2/3-b) 起持 RR forward channel（v0.5 候选 C 后共 6 个）**（每 RPC
/// 一条 iceoryx2 RR 通道，对应 6 个 rr_io_thread）：
/// - `rr_forward_tx_clear_estop`（A1 ClearEStop；改名加 method 后缀避免
///   跟 4 个 task forward 混淆）
/// - `rr_forward_tx_task_invoke` / `_task_cancel` / `_task_status` /
///   `_clear_error`（B4 task control plane）
/// - `rr_forward_tx_set_enabled`（v0.5 候选 C；→ S0 set_enabled RR server）
///
/// `StreamTaskEvents` 不经此 forward channel 走 pubsub，留 2/3-c 接。
pub struct GatewayHandlerState {
    /// ClearEStop 转发 wire request 到 [`crate::rr_io_thread`]（A1 落地）。
    /// `bounded(64)` 跟 daemon 端 `rr_server_thread::max_in_flight` 对齐。
    pub rr_forward_tx_clear_estop: RrPort,
    /// `InvokeTask` 转发 wire request（B4 (2/3-b)）。
    pub rr_forward_tx_task_invoke: RrPort,
    /// `CancelTask` 转发 wire request（B4 (2/3-b)）。
    pub rr_forward_tx_task_cancel: RrPort,
    /// `GetTaskStatus` 转发 wire request（B4 (2/3-b)；只读 RPC，**不**进
    /// idempotency cache）。
    pub rr_forward_tx_task_status: RrPort,
    /// `ClearError` 转发 wire request（B4 (2/3-b)）。
    pub rr_forward_tx_clear_error: RrPort,
    /// `SetEnabled` 转发 wire request（v0.5 候选 C；第 6 条 rr_io_thread →
    /// S0 `/<robot_id>/control/set_enabled`）。
    pub rr_forward_tx_set_enabled: RrPort,
    /// `EStop` publish 转发（v0.5 候选 D；estop_pub_thread →
    /// `/<robot_id>/events/estop` topic，**非** RR）。`bounded(64)`（PB2）。
    pub estop_pub_tx: Sender<EStopPubForward>,
    /// 机器人 ID，`StreamTaskEvents` handler 用以构造 per-ActionId pubsub
    /// topic `/<robot_id>/s1/action_events/<action_id>`（B4 (2/3-c)）。
    pub robot_id: String,
    /// fake token → role + role → capability 查表。`Arc` 让多 handler /
    /// 多 worker 共享。
    pub role_table: Arc<RoleTable>,
    /// in-process idempotency cache（TTL + cap）。`Mutex` 因为 `decide` /
    /// `commit` 都是短临界区（不在 `await` 期间持锁）。
    pub(crate) ledger: crate::gateway_ledger::SharedLedger,
    /// `RobotClock`：算 wire `deadline_ns` + `stamp_ns`。生产用
    /// `SystemClock`，测试可用 `MockClock`。CLAUDE.md 禁
    /// `SystemTime::now()` / `Instant::now()`，时间读取经 trait。
    pub clock: Arc<dyn RobotClock>,
    /// 等 RR response 超过 `request.timeout_ms` 后再多等的 grace（ms）。
    /// daemon 一旦 deadline 前开始 action，可能在 fsync 之后才回真实 outcome；
    /// gateway 过早 `DeadlineExceeded` 会制造"副作用已发生但客户端以为失败"
    /// 的歧义（reviewer A1-3 (2/3-b2-2) 拍板边界）。
    pub rr_timeout_grace_ms: u64,
    /// **A1-3 (2/3-c)** shutdown signal。`watch::Receiver<bool>` 是
    /// Clone + Send + Sync，handler 每次入口 `*shutdown_rx.borrow()` 同步
    /// 读一下；shutdown=true 时 cache.decide 之后、forward 之前 fast-fail
    /// 返 `Unavailable("gateway is shutting down; ...")`。已进 handler 且
    /// 已 forward 的请求**不**被强制中断：tonic `serve_with_incoming_shutdown`
    /// 自然等 in-flight handler 跑完（含 oneshot await + RR reply）；
    /// rr_io_thread 在 tonic 退出之后才 stop/join（reviewer (2/3-b2-1)
    /// 关键边界）。
    pub shutdown_rx: watch::Receiver<bool>,

    /// **v0.5 候选 B (2/4)**：常驻 per-hardware S0 state cache（[`StatePool`]
    /// 的 subscriber 线程写，`GetState` / `StreamState` handler 只读）。`Arc`
    /// 让 handler 与 pool 线程共享同一份 cache。**唯一**底层 subscriber 来源
    /// 在 [`StatePool`]；handler **不**新建 subscriber（reviewer Plan 锁点 2）。
    pub state_cache: Arc<StateCache>,
}

// `Sender` / `Mutex<IdempotencyCache>` / `dyn RobotClock` 不全 Debug 友好；
// 手动 impl 只暴露字面字段方便 trace。`#[allow(clippy::missing_fields_in_debug)]`
// 跟 daemon 端 `rr_server_thread::PendingRr` / `RrForwardRequest` 同模式。
#[allow(clippy::missing_fields_in_debug)]
impl core::fmt::Debug for GatewayHandlerState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let responsibilities_empty = self
            .ledger
            .lock()
            .is_ok_and(|ledger| ledger.responsibilities_empty());
        f.debug_struct("GatewayHandlerState")
            .field("rr_timeout_grace_ms", &self.rr_timeout_grace_ms)
            .field("responsibilities_empty", &responsibilities_empty)
            .finish()
    }
}

/// gRPC `RobotControl` service 实现。**A1-3 (2/3-b2-2)** 起持完整 state +
/// 12 步 logic（详 module doc）。
///
/// 单测可用 [`GatewayHandler::for_test`] 构造一份 placeholder state（仅
/// 用于不调 `clear_e_stop` 的 server-binding 测试）。
#[derive(Debug)]
pub struct GatewayHandler {
    state: GatewayHandlerState,
}

impl GatewayHandler {
    /// 用 caller 提供的 state 构造 handler。
    #[must_use]
    pub fn new(state: GatewayHandlerState) -> Self {
        Self { state }
    }

    /// 借出内部 state（诊断 / 测试用，**不**用于线程间转移所有权）。
    #[must_use]
    pub fn state(&self) -> &GatewayHandlerState {
        &self.state
    }

    /// 测试 placeholder：构造一份 minimal handler 用于"只测 server bind /
    /// shutdown，不调 `clear_e_stop`"的场景。
    ///
    /// **不**适合 handler logic 单测（forward channel 容量为 1 + 立即 drop
    /// rx，role table 空 → 任何 ClearEStop 调用都会 fail 在 auth 步）。handler
    /// logic 单测应用 `Self::new` 显式提供完整 state（mock daemon 在 test 主
    /// 线程读 forward channel + 回 oneshot）。
    #[cfg(test)]
    #[must_use]
    pub fn for_test() -> Self {
        Self::for_test_with_state_cache(Arc::new(StateCache::new(&[])))
    }

    /// 同 [`Self::for_test`] 但注入指定 state cache（v0.5 候选 B：测
    /// `GetState` / `StreamState` handler 4 分支用）。
    #[cfg(test)]
    #[must_use]
    pub fn for_test_with_state_cache(state_cache: Arc<StateCache>) -> Self {
        let ledger = crate::gateway_ledger::handler_fixture::ledger(64);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (estop_pub_tx, _estop_rx) = bounded(1);
        let state = Self::test_state(
            ledger,
            Arc::new(RoleTable::default()),
            shutdown_rx,
            estop_pub_tx,
            state_cache,
        );
        Self::new(state)
    }
    #[cfg(test)]
    pub(crate) fn test_state(
        ledger: crate::gateway_ledger::SharedLedger,
        role_table: Arc<RoleTable>,
        shutdown_rx: watch::Receiver<bool>,
        estop_pub_tx: Sender<EStopPubForward>,
        state_cache: Arc<StateCache>,
    ) -> GatewayHandlerState {
        use crate::gateway_ledger::{RrRoute, handler_fixture::port};
        GatewayHandlerState {
            rr_forward_tx_clear_estop: port(&ledger, RrRoute::Write(Method::ClearEStop)),
            rr_forward_tx_task_invoke: port(&ledger, RrRoute::Write(Method::InvokeTask)),
            rr_forward_tx_task_cancel: port(&ledger, RrRoute::Write(Method::CancelTask)),
            rr_forward_tx_task_status: port(&ledger, RrRoute::ReadStatus),
            rr_forward_tx_clear_error: port(&ledger, RrRoute::Write(Method::ClearError)),
            rr_forward_tx_set_enabled: port(&ledger, RrRoute::Write(Method::SetEnabled)),
            estop_pub_tx,
            ledger,
            robot_id: "for_test".into(),
            role_table,
            clock: Arc::new(SystemClock),
            rr_timeout_grace_ms: 500,
            shutdown_rx,
            state_cache,
        }
    }
}

/// 把 [`Role`] 映射到 wire [`RpcRole`]（仅 audit；daemon 端不依赖）。
fn role_to_wire(r: Role) -> RpcRole {
    match r {
        Role::Viewer => RpcRole::Viewer,
        Role::Operator => RpcRole::Operator,
        Role::Maintainer => RpcRole::Maintainer,
        Role::Admin => RpcRole::Admin,
    }
}

// ============================================================================
// v0.5 候选 B (3/4)：StreamState 节流（D5a / reviewer Plan PD2）
// ============================================================================

/// `StreamState` 节流默认下限（D5a）：`min_interval_ms == 0` 用此值。
const STREAM_STATE_DEFAULT_INTERVAL_MS: u64 = 50;
/// `StreamState` 节流 floor（D5a）：`0 < min_interval_ms < 20` clamp 到此值。
const STREAM_STATE_FLOOR_INTERVAL_MS: u64 = 20;
/// per-RPC tick → mpsc 缓冲。keep-last + 节流下小 buffer 足够；满则 backpressure
/// 让 tick task 等 client，自然降到 client 消费速率（不无界堆积）。
const STREAM_STATE_CHANNEL_BUFFER: usize = 8;

/// `StreamState` 节流 interval 计算（D5a，reviewer Plan PD2 拍定）：
/// `0 → 50ms`（默认）；`0 < x < 20 → 20ms`（floor，不拒）；`>= 20 → 原值`。
fn clamp_stream_interval_ms(min_interval_ms: u32) -> u64 {
    let v = u64::from(min_interval_ms);
    if v == 0 {
        STREAM_STATE_DEFAULT_INTERVAL_MS
    } else if v < STREAM_STATE_FLOOR_INTERVAL_MS {
        STREAM_STATE_FLOOR_INTERVAL_MS
    } else {
        v
    }
}

#[tonic::async_trait]
impl RobotControl for GatewayHandler {
    #[allow(clippy::too_many_lines)] // 12 步 logic 一条 fn 实现，明确流程
    async fn clear_e_stop(
        &self,
        request: tonic::Request<ClearEStopCommand>,
    ) -> Result<tonic::Response<Ack>, tonic::Status> {
        let cmd = request.into_inner();

        // ─── Step 1: validate（含 request_id UUIDv7 / confirmation 非空 /
        //     idempotency_key 非空 / source 非空 / auth.token 非空 / timeout > 0） ───
        let validated = validate_clear_estop_command(&cmd).map_err(|e| {
            tracing::warn!(error = %e, "ClearEStop validation failed");
            tonic::Status::new(e.to_tonic_code(), e.to_string())
        })?;

        // ─── Step 2: auth — fake token → Role 查表 ───
        let Some(role) = self.state.role_table.lookup_token(&validated.auth_token) else {
            tracing::warn!(
                request_id = %validated.request_id,
                "ClearEStop auth.token unknown; replying Unauthenticated"
            );
            return Err(tonic::Status::unauthenticated(
                "auth.token not found in role table",
            ));
        };

        // ─── Step 3: authz — role 必须含 `clear_estop` capability ───
        if !self
            .state
            .role_table
            .role_has_capability(role, Capability::ClearEStop)
        {
            tracing::warn!(
                request_id = %validated.request_id,
                role = role.as_str(),
                "ClearEStop denied: role lacks `clear_estop` capability"
            );
            return Err(tonic::Status::permission_denied(format!(
                "role `{}` lacks `clear_estop` capability (requires maintainer+)",
                role.as_str()
            )));
        }

        // ─── Step 4: 算 idempotency fingerprint（**排除** request_id /
        //     timeout_ms / idempotency_key；详 clear_estop module doc） ───
        let fingerprint = clear_estop_idempotency_fingerprint(&validated);

        let reply_bytes = crate::gateway_ledger::exchange_write(
            &self.state.rr_forward_tx_clear_estop,
            crate::gateway_ledger::WriteRequest {
                method: Method::ClearEStop,
                request_id: &validated.request_id,
                key: &validated.idempotency_key,
                fingerprint,
                class: crate::gateway_ledger::class_for_write(
                    Method::ClearEStop,
                    validated.dry_run,
                    false,
                ),
                timeout_ms: validated.timeout_ms,
            },
            self.state.clock.as_ref(),
            &self.state.shutdown_rx,
            self.state.rr_timeout_grace_ms,
            |deadline_ns, now_ns| {
                Ok(build_clear_estop_request(
                    &validated.request_id,
                    &validated.idempotency_key,
                    &validated.source,
                    role_to_wire(role),
                    &validated.confirmation,
                    &validated.audit_reason,
                    validated.dry_run,
                    deadline_ns,
                    now_ns,
                ))
            },
        )
        .await?;
        let view = flatbuffers::root::<FbResp>(&reply_bytes).map_err(|error| {
            tonic::Status::internal(format!("decode wire ClearEStopResponse: {error}"))
        })?;
        if view.request_id() != Some(validated.request_id.as_str()) {
            return Err(tonic::Status::internal("RR response request_id mismatch"));
        }
        match rpc_status_to_tonic_code(view.status()) {
            tonic::Code::Ok => Ok(tonic::Response::new(Ack {
                request_id: validated.request_id,
                cleared_at_ns: view.cleared_at_ns(),
                dry_run: view.dry_run(),
            })),
            code => Err(tonic::Status::new(
                code,
                format!(
                    "daemon returned {:?}: {}",
                    view.status(),
                    view.error_msg().unwrap_or_default()
                ),
            )),
        }
    }

    // ------------------------------------------------------------------------
    // v0.5 候选 C SetEnabled handler（沿 ClearEStop 12 步骨架；operator+）。
    // 第 6 条 rr_io_thread → S0 `/<robot_id>/control/set_enabled`。
    // ------------------------------------------------------------------------
    #[allow(clippy::too_many_lines)]
    async fn set_enabled(
        &self,
        request: tonic::Request<SetEnabledCommand>,
    ) -> Result<tonic::Response<Ack>, tonic::Status> {
        let cmd = request.into_inner();

        // ─── Step 1: validate
        let validated = validate_set_enabled_command(&cmd).map_err(|e| {
            tracing::warn!(error = %e, "SetEnabled validation failed");
            tonic::Status::new(e.to_tonic_code(), e.to_string())
        })?;

        // ─── Step 2: auth — fake token → Role 查表
        let Some(role) = self.state.role_table.lookup_token(&validated.auth_token) else {
            tracing::warn!(
                request_id = %validated.request_id,
                "SetEnabled auth.token unknown; replying Unauthenticated"
            );
            return Err(tonic::Status::unauthenticated(
                "auth.token not found in role table",
            ));
        };

        // ─── Step 3: authz — role 必须含 `set_enabled` capability（operator+；
        //     viewer 拒）
        if !self
            .state
            .role_table
            .role_has_capability(role, Capability::SetEnabled)
        {
            tracing::warn!(
                request_id = %validated.request_id,
                role = role.as_str(),
                "SetEnabled denied: role lacks `set_enabled` capability"
            );
            return Err(tonic::Status::permission_denied(format!(
                "role `{}` lacks `set_enabled` capability (requires operator+)",
                role.as_str()
            )));
        }

        // ─── Step 4: 算 idempotency fingerprint
        let fingerprint = set_enabled_idempotency_fingerprint(&validated);

        let reply_bytes = crate::gateway_ledger::exchange_write(
            &self.state.rr_forward_tx_set_enabled,
            crate::gateway_ledger::WriteRequest {
                method: Method::SetEnabled,
                request_id: &validated.request_id,
                key: &validated.idempotency_key,
                fingerprint,
                class: crate::gateway_ledger::class_for_write(
                    Method::SetEnabled,
                    validated.dry_run,
                    validated.enabled,
                ),
                timeout_ms: validated.timeout_ms,
            },
            self.state.clock.as_ref(),
            &self.state.shutdown_rx,
            self.state.rr_timeout_grace_ms,
            |deadline_ns, now_ns| {
                Ok(build_set_enabled_request(
                    &validated.request_id,
                    &validated.idempotency_key,
                    &validated.source,
                    role_to_wire(role),
                    validated.enabled,
                    validated.dry_run,
                    deadline_ns,
                    &validated.audit_reason,
                    now_ns,
                ))
            },
        )
        .await?;
        let view = flatbuffers::root::<FbSetEnabledResp>(&reply_bytes).map_err(|error| {
            tonic::Status::internal(format!("decode wire SetEnabledResponse: {error}"))
        })?;
        if view.request_id() != Some(validated.request_id.as_str()) {
            return Err(tonic::Status::internal("RR response request_id mismatch"));
        }
        match rpc_status_to_tonic_code(view.status()) {
            tonic::Code::Ok => Ok(tonic::Response::new(Ack {
                request_id: validated.request_id,
                cleared_at_ns: view.completed_at_ns(),
                dry_run: validated.dry_run,
            })),
            code => Err(tonic::Status::new(
                code,
                format!(
                    "daemon returned {:?}: {}",
                    view.status(),
                    view.error_msg().unwrap_or_default()
                ),
            )),
        }
    }

    // ------------------------------------------------------------------------
    // v0.5 候选 D EStop 触发 handler（D1 路由 (a)：publish estop topic，非 RR；
    // 12 步骨架裁剪——**无** Step 11/11.5：publish 无 wire response，无 decode
    // / request_id-mismatch 防线〔属 RR 回包路径〕）。operator+。
    // Ack 弱语义（D2）：cleared_at_ns = published_at_ns；不证 S0 消费 / latch
    // fsync。失败映射 = PD2 表（estop_pub_thread 模块 doc）。
    // ------------------------------------------------------------------------
    #[allow(clippy::too_many_lines)]
    async fn e_stop(
        &self,
        request: tonic::Request<EStopCommand>,
    ) -> Result<tonic::Response<Ack>, tonic::Status> {
        let cmd = request.into_inner();

        // ─── Step 1: validate（含 dry_run 拒，D4——先于 cache，不入 cache）
        let validated = validate_estop_command(&cmd).map_err(|e| {
            tracing::warn!(error = %e, "EStop validation failed");
            tonic::Status::new(e.to_tonic_code(), e.to_string())
        })?;

        // ─── Step 2: auth — fake token → Role 查表
        let Some(role) = self.state.role_table.lookup_token(&validated.auth_token) else {
            tracing::warn!(
                request_id = %validated.request_id,
                "EStop auth.token unknown; replying Unauthenticated"
            );
            return Err(tonic::Status::unauthenticated(
                "auth.token not found in role table",
            ));
        };

        // ─── Step 3: authz — `estop` capability（operator+；viewer 拒）
        if !self
            .state
            .role_table
            .role_has_capability(role, Capability::EStop)
        {
            tracing::warn!(
                request_id = %validated.request_id,
                role = role.as_str(),
                "EStop denied: role lacks `estop` capability"
            );
            return Err(tonic::Status::permission_denied(format!(
                "role `{}` lacks `estop` capability (requires operator+)",
                role.as_str()
            )));
        }

        // ─── Step 4: 算 idempotency fingerprint
        let fingerprint = estop_idempotency_fingerprint(&validated);

        let decision = self
            .state
            .ledger
            .lock()
            .expect("网关责任锁未中毒")
            .begin_write(
                Method::EStop,
                &validated.idempotency_key,
                fingerprint,
                crate::class_cache::Class::Emergency,
                &validated.request_id,
                self.state.clock.now().0,
            );
        let id = match decision {
            crate::gateway_ledger::BeginWrite::Fresh(id) => id,
            crate::gateway_ledger::BeginWrite::Replay(terminal) => {
                let crate::class_cache::TypedOutcome::EStop {
                    published_at_ns, ..
                } = terminal.outcome
                else {
                    return Err(tonic::Status::internal("EStop cache namespace mismatch"));
                };
                return Ok(tonic::Response::new(Ack {
                    request_id: validated.request_id,
                    cleared_at_ns: published_at_ns,
                    dry_run: false,
                }));
            }
            crate::gateway_ledger::BeginWrite::Refused(refused) => {
                return Err(crate::gateway_ledger::refusal_status(refused));
            }
        };
        let guard = crate::gateway_ledger::QueuedGuard::new(&self.state.ledger, id);
        if *self.state.shutdown_rx.borrow()
            || self
                .state
                .ledger
                .lock()
                .expect("网关责任锁未中毒")
                .admission_closed()
        {
            return Err(tonic::Status::unavailable(
                "gateway is shutting down; new requests rejected",
            ));
        }
        let stamp_ns = self.state.clock.now().0;
        let body_bytes = build_estop_event(&validated.estop_reason, stamp_ns, "rpc", false);
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let forward = EStopPubForward {
            body_bytes,
            stamp_ns,
            oneshot_tx,
            request_id: validated.request_id.clone(),
            entry_id: id,
            guard,
        };
        match self.state.estop_pub_tx.try_send(forward) {
            Ok(()) => {}
            Err(TrySendError::Full(original)) => {
                drop(original);
                return Err(tonic::Status::resource_exhausted(
                    "gateway EStop publish forward channel full; retry shortly",
                ));
            }
            Err(TrySendError::Disconnected(original)) => {
                drop(original);
                return Err(tonic::Status::unavailable(
                    "gateway EStop publish thread stopped",
                ));
            }
        }
        let total_timeout_ms = validated
            .timeout_ms
            .saturating_add(self.state.rr_timeout_grace_ms);
        let published_at_ns =
            match tokio::time::timeout(Duration::from_millis(total_timeout_ms), oneshot_rx).await {
                Ok(Ok(Ok(at))) => at,
                Ok(Ok(Err(error))) => {
                    return Err(tonic::Status::internal(format!(
                        "EStopEvent publish failed: {error}"
                    )));
                }
                Ok(Err(_)) => {
                    return Err(tonic::Status::unavailable(
                        "gateway EStop publish thread unavailable",
                    ));
                }
                Err(_) => {
                    return Err(tonic::Status::deadline_exceeded(format!(
                        "no publish confirmation within {total_timeout_ms} ms"
                    )));
                }
            };
        Ok(tonic::Response::new(Ack {
            request_id: validated.request_id,
            cleared_at_ns: published_at_ns,
            dry_run: false,
        }))
    }

    // ------------------------------------------------------------------------
    // v0.3 B4 (2/3-b) task control plane handlers
    //
    // 4 个 task RPC + ClearError 真 handler 实现走 [`crate::task_handlers`]
    // module（12 步骨架共享 helper + 每 method 独立 fingerprint / wire
    // codec）。trait 方法本身是薄壳，仅做 trait 完整性 +
    // `tonic::Request → 内部 helper` 路由。
    //
    // `StreamTaskEvents` 留 Unimplemented 占位 → B4 (2/3-c) 接 per-ActionId
    // pubsub adapter（events drain → tokio::sync::mpsc → tonic stream）。
    // ------------------------------------------------------------------------

    type StreamTaskEventsStream = std::pin::Pin<
        Box<
            dyn tonic::codegen::tokio_stream::Stream<Item = Result<TaskEvent, tonic::Status>>
                + Send
                + 'static,
        >,
    >;

    async fn invoke_task(
        &self,
        request: tonic::Request<InvokeTaskCommand>,
    ) -> Result<tonic::Response<TaskHandle>, tonic::Status> {
        crate::task_handlers::handle_invoke_task(&self.state, request).await
    }

    async fn cancel_task(
        &self,
        request: tonic::Request<CancelTaskCommand>,
    ) -> Result<tonic::Response<Ack>, tonic::Status> {
        crate::task_handlers::handle_cancel_task(&self.state, request).await
    }

    async fn get_task_status(
        &self,
        request: tonic::Request<TaskHandleRef>,
    ) -> Result<tonic::Response<TaskStatus>, tonic::Status> {
        crate::task_handlers::handle_get_task_status(&self.state, request).await
    }

    async fn stream_task_events(
        &self,
        request: tonic::Request<TaskHandleRef>,
    ) -> Result<tonic::Response<Self::StreamTaskEventsStream>, tonic::Status> {
        // B4 (2/3-c) 真 handler：spawn per-ActionId pubsub subscribe thread →
        // 拿 tokio::sync::mpsc::Receiver → 包成 EventsSubscribeStream → Box::pin
        // 进 trait associated type。详 task_handlers::handle_stream_task_events
        // + crate::events_subscribe 模块 doc。
        let resp = crate::task_handlers::handle_stream_task_events(&self.state, request)?;
        let (metadata, stream, extensions) = resp.into_parts();
        let pinned: Self::StreamTaskEventsStream = Box::pin(stream);
        Ok(tonic::Response::from_parts(metadata, pinned, extensions))
    }

    async fn clear_error(
        &self,
        request: tonic::Request<ClearErrorCommand>,
    ) -> Result<tonic::Response<Ack>, tonic::Status> {
        crate::task_handlers::handle_clear_error(&self.state, request).await
    }

    // ------------------------------------------------------------------------
    // v0.5 候选 B read-only state plane（**(2/4)+(3/4) 真 handler**）。
    //
    // - `get_state`    → 读常驻 [`StatePool`] cache + 错误码顺序硬锁 D9（(2/4)）。
    // - `stream_state` → 开流前同 D9 校验 + **per-RPC tick task 读同一 cache**
    //   （**不**新建底层 subscriber，锁点 2）+ 节流 D5a（(3/4)）。
    // ------------------------------------------------------------------------

    type StreamStateStream = std::pin::Pin<
        Box<
            dyn tonic::codegen::tokio_stream::Stream<Item = Result<RobotState, tonic::Status>>
                + Send
                + 'static,
        >,
    >;

    async fn get_state(
        &self,
        request: tonic::Request<GetStateRequest>,
    ) -> Result<tonic::Response<RobotState>, tonic::Status> {
        // 错误码顺序即口径（D9 / AC-3）：① 空 hardware_id → InvalidArgument
        // （校验**早于**池查找）② 不在池 → NotFound（禁 fallback，AC-9）③ 已知
        // 但无首帧 → Unavailable ④ 有首帧 → convert → Ok。auth：read RPC
        // **不读不校验** token（D7 / C1 非安全保证）。
        let hardware_id = request.into_inner().hardware_id;
        if hardware_id.is_empty() {
            return Err(tonic::Status::invalid_argument(
                "GetStateRequest.hardware_id must be non-empty",
            ));
        }
        match self.state.state_cache.snapshot(&hardware_id) {
            StateLookup::UnknownHardware => Err(tonic::Status::not_found(format!(
                "hardware_id '{hardware_id}' is not a config-declared hardware (gateway state pool)"
            ))),
            StateLookup::NoFrame => Err(tonic::Status::unavailable(format!(
                "hardware_id '{hardware_id}' has no S0 state sample yet"
            ))),
            StateLookup::Frame(bytes) => match flatbuffers::root::<JointState>(&bytes) {
                Ok(fb) => Ok(tonic::Response::new(joint_state_fb_to_robot_state(
                    fb,
                    &self.state.robot_id,
                    &hardware_id,
                ))),
                Err(e) => Err(tonic::Status::internal(format!(
                    "state body flatbuffers decode failed: {e}"
                ))),
            },
        }
    }

    async fn stream_state(
        &self,
        request: tonic::Request<StreamRequest>,
    ) -> Result<tonic::Response<Self::StreamStateStream>, tonic::Status> {
        let req = request.into_inner();
        let hardware_id = req.hardware_id;

        // 开流前校验**同 GetState 口径**（D9）：① 空 → InvalidArgument ② 不在池
        // → NotFound（禁 fallback）③ 已知但无首帧 → Unavailable。auth：read RPC
        // **不读不校验** token（D7）。
        if hardware_id.is_empty() {
            return Err(tonic::Status::invalid_argument(
                "StreamRequest.hardware_id must be non-empty",
            ));
        }
        match self.state.state_cache.snapshot(&hardware_id) {
            StateLookup::UnknownHardware => {
                return Err(tonic::Status::not_found(format!(
                    "hardware_id '{hardware_id}' is not a config-declared hardware (gateway state pool)"
                )));
            }
            StateLookup::NoFrame => {
                return Err(tonic::Status::unavailable(format!(
                    "hardware_id '{hardware_id}' has no S0 state sample yet"
                )));
            }
            // 有首帧 → 开流（keep-last 永不清，后续 tick 恒有帧）。
            StateLookup::Frame(_) => {}
        }

        // 节流 interval（D5a）。per-RPC **轻量 tokio tick task**：每 interval 读
        // **同一常驻 cache**（**绝不**新建底层 iceoryx2 subscriber，锁点 2）→
        // convert → mpsc。client drop response stream → Receiver drop →
        // `tx.send().await` Err → task break（drop→stop，沿 events_subscribe 精神，
        // 但更轻：无 std::thread / 无 iceoryx2 句柄）。
        let interval_ms = clamp_stream_interval_ms(req.min_interval_ms);
        let cache = Arc::clone(&self.state.state_cache);
        let robot_id = self.state.robot_id.clone();
        let hw = hardware_id;
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_STATE_CHANNEL_BUFFER);

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
            // 不 burst 补偿漏掉的 tick（节流是"最新观察流"，不保证每 tick 一帧）。
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let item = match cache.snapshot(&hw) {
                    StateLookup::Frame(bytes) => match flatbuffers::root::<JointState>(&bytes) {
                        Ok(fb) => Ok(joint_state_fb_to_robot_state(fb, &robot_id, &hw)),
                        // 解码错单 tick 跳过（不杀流；理论不发生——bytes 来自合法帧）。
                        Err(_) => continue,
                    },
                    // 开流后理论不再 NoFrame（keep-last）；UnknownHardware 不会发生。
                    StateLookup::NoFrame | StateLookup::UnknownHardware => continue,
                };
                if tx.send(item).await.is_err() {
                    // client 已 drop response stream → 停 tick task。
                    break;
                }
            }
        });

        let stream: Self::StreamStateStream = Box::pin(ReceiverStream::new(rx));
        Ok(tonic::Response::new(stream))
    }
}

// ============================================================================
// run_gateway: 起 tonic server
// ============================================================================

/// `run_gateway` 错误。
///
/// `tonic::transport::Error` / `std::io::Error` 都不是 `Clone`，所以本
/// enum 也不是 `Clone`。`run_gateway_with_ready` 在 ready 阶段（pre-serve）
/// 失败时需要把"等价的"错误同时送 `ready_tx` 和 return —— 用 [`GatewayReadyError`]
/// 承载（A1-3 (2/3-b2-2a) reviewer 拍板：替换旧
/// `clone_gateway_error` / `clone_config_error` 临时 helper）。
#[derive(Debug, Error)]
pub enum GatewayError {
    /// `GatewayConfig::validate()` 校验失败（含 listen-addr 非 loopback /
    /// 0 TTL / 0 max_entries / empty robot_id）。
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// `TcpListener::bind` 失败（端口占用 / 权限不足等）。reviewer A1-3
    /// (2/3-b1) 拍板：bind 错误必须在 `ready_tx` 之前产生 → ready_tx
    /// 携带 [`Result<SocketAddr, GatewayReadyError>`]，测试能区分"已 ready"
    /// vs "bind 失败"。
    #[error("TcpListener::bind {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    /// tonic transport 启动失败 / 服务运行期出错。
    #[error("tonic transport: {0}")]
    Tonic(#[from] tonic::transport::Error),

    /// **A1-3 (2/3-b2-2)**：[`spawn_rr_io_thread`] 失败（client_factory
    /// panic 在 ready signal 之前 / `thread::Builder::spawn` 失败）。
    /// `run_gateway_with_rr_io` 在调 [`run_gateway_with_ready`] 之前必须
    /// 先起 rr_io_thread，spawn 失败直接 fail-fast。
    #[error("rr_io_thread spawn failed: {0}")]
    RrIoSpawn(#[source] std::io::Error),

    /// **v0.5 候选 B (2/4)**：[`StatePool::spawn`] 失败（某 config 声明的
    /// hardware subscriber open 失败 / 线程 spawn 失败）。在 6 个 rr_io_thread
    /// 起好之后、tonic server 起之前 fail-fast（沿 RrIoSpawn 回滚先例）。
    #[error("state_pool spawn failed: {0}")]
    StatePoolSpawn(#[source] std::io::Error),

    /// **v0.5 候选 E 段(8)**：tonic loopback probe 在
    /// [`crate::systemd_probe::FIRST_SUCCESS_TIMEOUT`] 内未首成（server task
    /// 已起但 unary 不通）→ 启动失败（READY=1 永不发）。
    #[error("tonic loopback probe never succeeded within {waited_ms} ms (READY gate)")]
    ReadyProbeTimeout { waited_ms: u64 },

    /// **v0.5 候选 E 段(8)**：server task 异常结束（join 错 / 在 bind-ready
    /// 前退出 / READY 后非 shutdown 自行返回）。
    #[error("tonic server task ended abnormally: {0}")]
    ServerTask(String),
}

// 兼容旧 callers (run_gateway_rejects_non_loopback_bind_before_serving 等)：
// `LoopbackBindError` → `GatewayError::Config(ConfigError::LoopbackBind(_))`
// 链。From 链由 ConfigError 提供 #[from] 完成。
impl From<LoopbackBindError> for GatewayError {
    fn from(e: LoopbackBindError) -> Self {
        Self::Config(ConfigError::LoopbackBind(e))
    }
}

/// `ready_tx` 通道送出的错误（pre-serve 阶段失败：config validation /
/// `TcpListener::bind` / `rr_io_thread` spawn）。
///
/// 跟 [`GatewayError`] 区别：
///
/// - **没**有 `Tonic(tonic::transport::Error)` 变体：tonic 错误只在 server
///   运行期产生，那时 `ready_tx` 已经 `take()` 进 owning closure，不可能
///   还能通过 `ready_tx` 投递。
/// - **是** `Clone + PartialEq`：`std::io::Error` 不是 `Clone`，所以 `Bind`
///   变体把 `source` 拆成 `kind: io::ErrorKind` + `message: String`（语
///   义不丢，但允许整个 enum 派生 `Clone`），让 `run_gateway_with_ready`
///   pre-serve 失败路径能直接 send 一份 + return 一份等价错误，不再需
///   要旧的 `clone_gateway_error` / `clone_config_error` 临时 helper。
///
/// `From<GatewayReadyError> for GatewayError` 让 caller / 测试可以把 ready
/// 阶段错误升级到 [`GatewayError`]（`Bind` 重新构造一个 `io::Error`，跟
/// 原始错误信息一致）。
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum GatewayReadyError {
    /// 同 [`GatewayError::Config`]。
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// 同 [`GatewayError::Bind`] 但 `source` 拆 kind + message 让整个 enum
    /// 可 Clone。
    #[error("TcpListener::bind {addr}: {message}")]
    Bind {
        addr: SocketAddr,
        kind: std::io::ErrorKind,
        message: String,
    },

    /// 同 [`GatewayError::RrIoSpawn`] 但 `source` 拆 kind + message 让整个
    /// enum 可 Clone。**A1-3 (2/3-b2-2)** 起加，仅
    /// [`run_gateway_with_rr_io`] 路径会触发；底层 [`run_gateway_with_ready`]
    /// 不起 rr_io_thread，不会 produce 此 variant。
    #[error("rr_io_thread spawn failed: {message}")]
    RrIoSpawn {
        kind: std::io::ErrorKind,
        message: String,
    },

    /// 同 [`GatewayError::StatePoolSpawn`] 但 `source` 拆 kind + message 让整个
    /// enum 可 Clone（v0.5 候选 B (2/4)）。
    #[error("state_pool spawn failed: {message}")]
    StatePoolSpawn {
        kind: std::io::ErrorKind,
        message: String,
    },

    /// **v0.5 候选 E 段(8)**：server task 在 bind-ready 之前异常结束（未回执
    /// bind 结果）。
    #[error("tonic server task ended before bind-ready: {message}")]
    ServerTask { message: String },
}

impl GatewayReadyError {
    /// 从 `io::Error` 构造 `Bind` variant（拆 kind + message 让结果 Clone）。
    fn bind_from_io(addr: SocketAddr, source: &std::io::Error) -> Self {
        Self::Bind {
            addr,
            kind: source.kind(),
            message: source.to_string(),
        }
    }

    /// 从 `io::Error` 构造 `RrIoSpawn` variant。
    fn rr_io_spawn_from_io(source: &std::io::Error) -> Self {
        Self::RrIoSpawn {
            kind: source.kind(),
            message: source.to_string(),
        }
    }

    /// 从 `io::Error` 构造 `StatePoolSpawn` variant（v0.5 候选 B (2/4)）。
    fn state_pool_spawn_from_io(source: &std::io::Error) -> Self {
        Self::StatePoolSpawn {
            kind: source.kind(),
            message: source.to_string(),
        }
    }
}

impl From<GatewayReadyError> for GatewayError {
    fn from(e: GatewayReadyError) -> Self {
        match e {
            GatewayReadyError::Config(c) => Self::Config(c),
            GatewayReadyError::Bind {
                addr,
                kind,
                message,
            } => Self::Bind {
                addr,
                source: std::io::Error::new(kind, message),
            },
            GatewayReadyError::RrIoSpawn { kind, message } => {
                Self::RrIoSpawn(std::io::Error::new(kind, message))
            }
            GatewayReadyError::StatePoolSpawn { kind, message } => {
                Self::StatePoolSpawn(std::io::Error::new(kind, message))
            }
            GatewayReadyError::ServerTask { message } => Self::ServerTask(message),
        }
    }
}

/// 起 tonic gRPC server，bind 到 [`GatewayConfig::listen_addr`]，挂
/// [`GatewayHandler`]，等 `shutdown_rx` 收到 `true` 后 graceful stop。
///
/// **配置校验**先行：[`GatewayConfig::validate`] 通过（含 loopback bind /
/// idempotency TTL/max > 0 / robot_id 非空）才往下走（reviewer 拍板硬边界）。
///
/// `shutdown_rx` 是 [`tokio::sync::watch::Receiver<bool>`]：bin main 注册
/// SIGINT/SIGTERM handler set true 触发 graceful shutdown。
///
/// **A1-3 (2/3-b1)** reviewer 拍板：复用 `run_gateway_with_ready` 的薄壳，
/// 让单测能精确等 server bind 完再发 client 请求（替代 sleep 50ms 等
/// readiness）。
///
/// # Errors
///
/// 见 [`GatewayError`]。
pub async fn run_gateway(
    cfg: GatewayConfig,
    handler: Arc<GatewayHandler>,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<(), GatewayError> {
    run_gateway_with_ready(cfg, handler, shutdown_rx, None).await
}

/// [`run_gateway`] + 可选 `ready_tx` ：tonic server 真 bind 后（拿到实际
/// 绑定的 `SocketAddr`）通过 `ready_tx` 把 `Ok(actual_addr)` 发出来。
///
/// reviewer A1-3 (2/3-b1) 边界 1 拍板：
///
/// - **`Ok(SocketAddr)` 仅在 `TcpListener::bind` + `local_addr()` 成功后**
///   send；让单测能精确分辨 "ready 可发 gRPC client" vs "bind 失败"。
/// - 配置校验失败 / bind 失败 → 把 `Err(GatewayError)` 发出 `ready_tx`
///   后再 return Err（让 ready_rx 端不会永久阻塞 `await`）。
/// - tonic `serve_with_incoming_shutdown` 走 [`TcpIncoming::from`]
///   （tonic 0.14 起 `from_listener` 改 `From<TcpListener>` + builder），
///   bind 时机由本函数控制（不是 tonic 内部）。
///
/// # Errors
///
/// 见 [`GatewayError`]。
pub async fn run_gateway_with_ready(
    cfg: GatewayConfig,
    handler: Arc<GatewayHandler>,
    mut shutdown_rx: watch::Receiver<bool>,
    ready_tx: Option<tokio::sync::oneshot::Sender<Result<SocketAddr, GatewayReadyError>>>,
) -> Result<(), GatewayError> {
    // 1) 配置校验 fail-fast
    if let Err(e) = cfg.validate() {
        let ready_err = GatewayReadyError::Config(e);
        if let Some(tx) = ready_tx {
            // 把等价错误送 ready 监听者（防 await 永久阻塞）
            let _ = tx.send(Err(ready_err.clone()));
        }
        return Err(ready_err.into());
    }

    tracing::info!(
        listen_addr = %cfg.listen_addr,
        robot_id = %cfg.robot_id,
        idempotency_ttl_secs = cfg.idempotency_ttl_secs,
        idempotency_max_entries = cfg.idempotency_max_entries,
        rr_timeout_grace_ms = cfg.rr_timeout_grace_ms,
        "robot_rpc_gateway: starting tonic gRPC server (A1-3 (2/3-c) ClearEStop handler with full 12-step logic + shutdown gate + rr_io_thread integration)"
    );

    // 2) TcpListener bind 主动控制（不依赖 tonic 内部 bind 时机）
    let listener = match tokio::net::TcpListener::bind(cfg.listen_addr).await {
        Ok(l) => l,
        Err(e) => {
            let ready_err = GatewayReadyError::bind_from_io(cfg.listen_addr, &e);
            if let Some(tx) = ready_tx {
                let _ = tx.send(Err(ready_err));
            }
            return Err(GatewayError::Bind {
                addr: cfg.listen_addr,
                source: e,
            });
        }
    };

    // 3) 拿到真实绑定的 addr（处理 :0 自动分配端口的场景），发 ready 信号
    let actual_addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(e) => {
            let ready_err = GatewayReadyError::bind_from_io(cfg.listen_addr, &e);
            if let Some(tx) = ready_tx {
                let _ = tx.send(Err(ready_err));
            }
            return Err(GatewayError::Bind {
                addr: cfg.listen_addr,
                source: e,
            });
        }
    };
    if let Some(tx) = ready_tx {
        // ready_rx 端可能已 drop（test cancel）；忽略 send err。
        let _ = tx.send(Ok(actual_addr));
    }

    // tonic 0.14：`from_listener(listener, nodelay, keepalive)` 改
    // `From<TcpListener>` + builder（infallible——原 from_listener 在已
    // bind 成功的 listener 上本就几乎不会失败）。语义保持：nodelay=true /
    // keepalive 默认。
    let incoming = tonic::transport::server::TcpIncoming::from(listener).with_nodelay(Some(true));

    let svc = RobotControlServer::from_arc(handler);

    let shutdown_signal = async move {
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            if shutdown_rx.changed().await.is_err() {
                // sender drop = 视同 shutdown
                break;
            }
        }
        tracing::info!("robot_rpc_gateway: shutdown signal received; tonic graceful stop");
    };

    tonic::transport::Server::builder()
        .add_service(svc)
        .serve_with_incoming_shutdown(incoming, shutdown_signal)
        .await?;

    tracing::info!(
        actual_addr = %actual_addr,
        "robot_rpc_gateway: tonic server stopped cleanly"
    );
    Ok(())
}

// ============================================================================
// run_gateway_with_rr_io: 起 rr_io_thread + 起 tonic server（高层入口）
// ============================================================================

/// **A1-3 (2/3-c)** 高层入口。bin 直接调本函数即可启动整套 ClearEStop
/// control plane gateway：
///
/// 0. **`cfg.validate()` 优先**（reviewer (2/3-b2-2) round 2 阻断 1）：
///    无效配置（空 robot_id / 非 loopback bind / 0 TTL / 0 max_entries）
///    必须在 spawn rr_io_thread 之前 fail-fast；不能让 RR client 创建
///    路径触碰共享 shm，也不能把"配置错"误报成 `RrIoSpawn`。
/// 1. spawn [`crate::rr_io_thread`]（factory closure 在新线程内构造
///    `Iceoryx2RrClient`；topic = `/<robot_id>/control/clear_estop`，跟
///    daemon 端 `rr_server_thread` 对接）；
/// 2. 构造 [`IdempotencyCache`]（TTL + cap 取自 `cfg`）；
/// 3. 构造 [`GatewayHandler`] 持完整 state（rr forward channel +
///    role table + cache + clock + grace + **`shutdown_rx` clone**）；
/// 4. 调 [`run_gateway_with_ready`] 起 tonic gRPC server（同一份
///    `shutdown_rx` 也喂给 tonic shutdown signal future，独立 receiver
///    borrow 不冲突）；
/// 5. tonic server 退出**之后**才 `rr_handle.request_stop()` + `join`
///    （reviewer A1-3 (2/3-b2-1) 关键边界：rr_io_thread 生命周期必须在
///    tonic server 退出之后才停，避免 handler 还在等 RR 时 I/O 被掐）。
///
/// shutdown 过程的三段语义详 module-level doc "A1-3 (2/3-c) graceful
/// shutdown 语义"段（新请求 fast-fail / 已 forward 的请求继续等 RR /
/// rr_io_thread 在 tonic 之后停）。
///
/// `forward channel buffer = 64`，跟 daemon 端 `rr_server_thread` 的
/// `max_in_flight = 64` 对齐（reviewer A1-3 (2/3-b2-1) 拍板）。
///
/// `ready_tx`（可选）：tonic server 真 bind 后送 `Ok(actual_addr)`；
/// pre-serve 任意阶段（config validate / rr_io_thread spawn / bind）失败送
/// 等价 [`GatewayReadyError`]。
///
/// # Errors
///
/// 见 [`GatewayError`]。
/// 外层始终持有全部已创建资源；运行 future 只借用它。
pub struct GatewayRun {
    cfg: GatewayConfig,
    roles: Arc<RoleTable>,
    clock: Arc<dyn RobotClock>,
    shutdown_rx: watch::Receiver<bool>,
    ready: Option<tokio::sync::oneshot::Sender<Result<SocketAddr, GatewayReadyError>>>,
    setup: GatewaySetup,
    phase: GatewayPhase,
    outcome: Option<Result<(), GatewayError>>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum GatewayPhase {
    New,
    Serving,
    Closing,
    Settled,
}
#[derive(Default)]
struct GatewaySetup {
    ledger: Option<crate::gateway_ledger::SharedLedger>,
    rr: SpawnedRrIo,
    estop: Option<EStopPubThreadHandle>,
    estop_tx: Option<Sender<EStopPubForward>>,
    state_pool: Option<StatePool>,
    serve: Option<ServeOwned>,
    signal: Option<tokio::task::JoinHandle<()>>,
    blocking_join: Option<crate::liveness::TaskSlot<JoinedWorkers>>,
    #[cfg(feature = "test-freeze-hook")]
    freeze: Option<crate::freeze_control::FreezeControlHandle>,
}
/// blocking 任务的原返回值；Tokio join 与每个标准线程的 join 是两层独立事实。
struct JoinedWorkers {
    rr: SpawnedRrIo,
    estop: Option<std::thread::Result<crate::estop_pub_thread::EStopExit>>,
    pool: Option<StatePool>,
    #[cfg(feature = "test-freeze-hook")]
    freeze: Option<std::thread::Result<()>>,
}
impl JoinedWorkers {
    fn join_failed(&self) -> bool {
        let failed = self.rr.joined.iter().any(Result::is_err)
            || self.estop.as_ref().is_some_and(Result::is_err)
            || self.pool.as_ref().is_some_and(StatePool::join_failed);
        #[cfg(feature = "test-freeze-hook")]
        let failed = failed || self.freeze.as_ref().is_some_and(Result::is_err);
        failed
    }
    fn empty_and_joined(&self) -> bool {
        let empty = self.rr.empty_and_joined()
            && self.estop.as_ref().is_none_or(|result| {
                result
                    .as_ref()
                    .is_ok_and(crate::estop_pub_thread::EStopExit::responsibilities_empty)
            })
            && self.pool.as_ref().is_none_or(StatePool::joined_ok);
        #[cfg(feature = "test-freeze-hook")]
        let empty = empty && self.freeze.as_ref().is_none_or(Result::is_ok);
        empty
    }
}

struct ServeOwned {
    server: crate::liveness::TaskSlot<Result<(), GatewayError>>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    bind_rx: Option<tokio::sync::oneshot::Receiver<Result<SocketAddr, GatewayReadyError>>>,
    first_rx: Option<tokio::sync::oneshot::Receiver<()>>,
    first_timer: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    probe: Option<crate::systemd_probe::ProbeHandle>,
    probe_metrics: Arc<TonicProbeMetrics>,
    observers: Option<Vec<LivenessObserver>>,
    watchdog: Option<robot_health::systemd::WatchdogHandle>,
    watchdog_joined: Option<std::thread::Result<()>>,
    ready: bool,
    stopping: bool,
}
impl GatewayRun {
    #[must_use]
    pub fn new(
        cfg: GatewayConfig,
        roles: Arc<RoleTable>,
        clock: Arc<dyn RobotClock>,
        shutdown: watch::Receiver<bool>,
        ready: Option<tokio::sync::oneshot::Sender<Result<SocketAddr, GatewayReadyError>>>,
    ) -> Self {
        Self {
            cfg,
            roles,
            clock,
            shutdown_rx: shutdown,
            ready,
            setup: GatewaySetup::default(),
            phase: GatewayPhase::New,
            outcome: None,
        }
    }
    /// 观测已有终态，不执行 decide、TTL 回收、状态迁移或许可归还。
    #[cfg(any(test, feature = "test-freeze-hook"))]
    #[must_use]
    pub fn cached_terminal(
        &self,
        method: Method,
        key: &str,
    ) -> Option<crate::class_cache::CachedTerminal> {
        self.setup
            .ledger
            .as_ref()?
            .lock()
            .expect("网关责任锁未中毒")
            .cached_terminal(method, key)
    }
    /// 信号任务按原不等待政策归外层持有；不得替换已有原句柄。
    pub fn install_signal_handler(
        &mut self,
        handle: tokio::task::JoinHandle<()>,
    ) -> Result<(), tokio::task::JoinHandle<()>> {
        if self.setup.signal.is_some() {
            return Err(handle);
        }
        self.setup.signal = Some(handle);
        Ok(())
    }
    #[allow(
        clippy::result_large_err,
        reason = "未收尾时完整返原网关拥有者，不拆分或装箱原句柄与残余责任"
    )]
    pub fn into_exit(self) -> Result<GatewayRunExit, Self> {
        if self.phase == GatewayPhase::Settled {
            Ok(GatewayRunExit { run: self })
        } else {
            Err(self)
        }
    }
    fn fail(&mut self, error: GatewayError) {
        if let Some(ready) = self.ready.take() {
            let observed = match &error {
                GatewayError::Config(config) => GatewayReadyError::Config(config.clone()),
                GatewayError::Bind { addr, source } => {
                    GatewayReadyError::bind_from_io(*addr, source)
                }
                GatewayError::RrIoSpawn(source) => GatewayReadyError::rr_io_spawn_from_io(source),
                GatewayError::StatePoolSpawn(source) => {
                    GatewayReadyError::state_pool_spawn_from_io(source)
                }
                GatewayError::Tonic(_)
                | GatewayError::ReadyProbeTimeout { .. }
                | GatewayError::ServerTask(_) => GatewayReadyError::ServerTask {
                    message: error.to_string(),
                },
            };
            let _ = ready.send(Err(observed));
        }
        if self.outcome.as_ref().is_none_or(Result::is_ok) {
            self.outcome = Some(Err(error));
        }
        self.phase = GatewayPhase::Closing;
    }
}
impl std::fmt::Debug for GatewayRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayRun")
            .field("settled", &(self.phase == GatewayPhase::Settled))
            .finish_non_exhaustive()
    }
}

#[must_use]
pub struct GatewayRunExit {
    run: GatewayRun,
}
impl GatewayRunExit {
    #[must_use]
    pub fn outcome(&self) -> &Result<(), GatewayError> {
        self.run.outcome.as_ref().expect("Settled 必有原结果")
    }
    /// 仅真实无活责任、所有必需 join 均成功时才允许普通析构；错误/残余完整返原。
    #[allow(
        clippy::result_large_err,
        reason = "拒绝析构时必须完整返原退出聚合，保留真实错误和残余责任"
    )]
    pub fn try_release_empty(self) -> Result<Result<(), GatewayError>, Self> {
        if !self.run.setup.release_is_empty() {
            return Err(self);
        }
        Ok(self.run.outcome.expect("Settled 必有原结果"))
    }
    pub fn exit(self) -> ! {
        let code = match self.outcome() {
            Ok(()) => 0,
            Err(GatewayError::Config(_)) => 2,
            Err(_) => 1,
        };
        let _held = std::mem::ManuallyDrop::new(self);
        std::process::exit(code)
    }
}
impl std::fmt::Debug for GatewayRunExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayRunExit")
            .field("outcome", self.outcome())
            .finish_non_exhaustive()
    }
}
impl GatewaySetup {
    fn release_is_empty(&self) -> bool {
        if self.signal.is_some() {
            return false;
        }
        if !self.blocking_join.as_ref().is_none_or(|slot| {
            slot.result()
                .is_some_and(|result| result.as_ref().is_ok_and(JoinedWorkers::empty_and_joined))
        }) {
            return false;
        }
        if !self.ledger.as_ref().is_none_or(|ledger| {
            ledger
                .lock()
                .expect("网关责任锁未中毒")
                .responsibilities_empty()
        }) {
            return false;
        }
        if !self.rr.empty_and_joined() || self.estop.is_some() {
            return false;
        }
        if !self.state_pool.as_ref().is_none_or(StatePool::joined_ok) {
            return false;
        }
        #[cfg(feature = "test-freeze-hook")]
        if self.freeze.is_some() {
            return false;
        }
        self.serve.as_ref().is_none_or(|serve| {
            !serve.server.timed_out()
                && matches!(serve.server.result(), Some(Ok(Ok(()))))
                && serve
                    .probe
                    .as_ref()
                    .is_none_or(crate::systemd_probe::ProbeHandle::joined_ok)
                && serve.watchdog.is_none()
                && serve.watchdog_joined.as_ref().is_none_or(Result::is_ok)
        })
    }
}

pub async fn run_gateway_with_rr_io(run: &mut GatewayRun) {
    if run.phase == GatewayPhase::New {
        match prepare_gateway(run) {
            Ok(()) => run.phase = GatewayPhase::Serving,
            Err(error) => run.fail(error),
        }
    }
    if run.phase == GatewayPhase::Serving {
        drive_serve(run).await;
    }
    finish_gateway_owned(run).await;
}

#[allow(
    clippy::too_many_lines,
    reason = "所有初始化资源逐项装入同一外层拥有者，保持部分失败的接管顺序可见"
)]
fn prepare_gateway(run: &mut GatewayRun) -> Result<(), GatewayError> {
    use crate::gateway_ledger::RrRoute;
    use crate::wire_credit::TopicKind;

    if let Err(error) = run.cfg.validate() {
        if let Some(ready) = run.ready.take() {
            let _ = ready.send(Err(GatewayReadyError::Config(error.clone())));
        }
        return Err(GatewayError::Config(error));
    }
    let ttl_ns = run.cfg.idempotency_ttl_secs.saturating_mul(1_000_000_000);
    let ledger = crate::gateway_ledger::GatewayLedger::new(ttl_ns, run.cfg.idempotency_max_entries)
        .map_err(|error| GatewayError::ServerTask(format!("gateway cache 配置拒绝: {error:?}")))?;
    run.setup.ledger = Some(Arc::clone(&ledger));
    let topics = RrTopics::for_robot(&run.cfg.robot_id);
    macro_rules! spawn_port {
        ($topic:expr, $req:ty, $resp:ty, $route:expr, $kind:expr) => {{
            let cfg = RrIoThreadConfig {
                topic: $topic.clone(),
                poll_interval_ns: 500_000,
            };
            let result = spawn_rr_io_thread(
                &cfg,
                client_factory_for::<$req, $resp>($topic.clone(), $kind),
                Arc::clone(&ledger),
                $route,
                $kind,
                Arc::clone(&run.clock),
            );
            run.setup.rr.install(result)?;
        }};
    }
    spawn_port!(
        topics.clear_estop,
        markers::ClearEStopRequest,
        markers::ClearEStopResponse,
        RrRoute::Write(Method::ClearEStop),
        TopicKind::S0Control
    );
    spawn_port!(
        topics.task_invoke,
        markers::InvokeTaskRequest,
        markers::InvokeTaskResponse,
        RrRoute::Write(Method::InvokeTask),
        TopicKind::S1Task
    );
    spawn_port!(
        topics.task_cancel,
        markers::CancelTaskRequest,
        markers::CancelTaskResponse,
        RrRoute::Write(Method::CancelTask),
        TopicKind::S1Task
    );
    spawn_port!(
        topics.task_status,
        markers::GetTaskStatusRequest,
        markers::GetTaskStatusResponse,
        RrRoute::ReadStatus,
        TopicKind::S1Task
    );
    spawn_port!(
        topics.clear_error,
        markers::ClearErrorRequest,
        markers::ClearErrorResponse,
        RrRoute::Write(Method::ClearError),
        TopicKind::S1Task
    );
    spawn_port!(
        topics.set_enabled,
        markers::SetEnabledRequest,
        markers::SetEnabledResponse,
        RrRoute::Write(Method::SetEnabled),
        TopicKind::S0Control
    );
    let (estop_tx, estop_rx) = bounded(64);
    run.setup.estop_tx = Some(estop_tx.clone());
    let topic = format!("/{}/events/estop", run.cfg.robot_id);
    let factory_topic = topic.clone();
    let factory = move || {
        let node = NodeBuilder::new()
            .create::<IxService>()
            .map_err(|error| format!("EStop Node: {error:?}"))?;
        let publisher = robot_transport::iceoryx2::publisher::<markers::EStopEvent>(
            &node,
            &factory_topic,
            robot_transport::iceoryx2::TopicClass::SafetyEvent,
        )
        .map_err(|error| format!("EStop publisher: {error:?}"))?;
        Ok(crate::estop_pub_thread::OwnedEStopPublisher {
            publisher,
            _node: node,
        })
    };
    match spawn_estop_pub_thread(
        &EStopPubThreadConfig {
            topic,
            poll_interval_ns: 500_000,
        },
        Arc::clone(&run.clock),
        factory,
        estop_rx,
        Arc::clone(&ledger),
    ) {
        Ok(handle) => run.setup.estop = Some(handle),
        Err(crate::estop_pub_thread::EStopSpawnError::ReadyFailed { handle, error }) => {
            run.setup.estop = Some(handle);
            return Err(GatewayError::RrIoSpawn(std::io::Error::other(error)));
        }
        Err(crate::estop_pub_thread::EStopSpawnError::ThreadSpawn(error)) => {
            return Err(GatewayError::RrIoSpawn(error));
        }
    }
    let state_cache = Arc::new(StateCache::new(&run.cfg.state_hardware_ids));
    match StatePool::spawn(&run.cfg.robot_id, &run.cfg.state_hardware_ids, &state_cache) {
        Ok(pool) => run.setup.state_pool = Some(pool),
        Err(rejected) => {
            run.setup.state_pool = Some(rejected.partial);
            return Err(GatewayError::StatePoolSpawn(rejected.error));
        }
    }
    tracing::info!("rr_io_threads started (6 RR I/O threads + estop_pub; W3 owned ledger)");
    let ports = &run.setup.rr.ports;
    let handler = Arc::new(GatewayHandler::new(GatewayHandlerState {
        rr_forward_tx_clear_estop: ports[0].clone(),
        rr_forward_tx_task_invoke: ports[1].clone(),
        rr_forward_tx_task_cancel: ports[2].clone(),
        rr_forward_tx_task_status: ports[3].clone(),
        rr_forward_tx_clear_error: ports[4].clone(),
        rr_forward_tx_set_enabled: ports[5].clone(),
        estop_pub_tx: estop_tx,
        robot_id: run.cfg.robot_id.clone(),
        role_table: Arc::clone(&run.roles),
        ledger,
        clock: Arc::clone(&run.clock),
        rr_timeout_grace_ms: run.cfg.rr_timeout_grace_ms,
        shutdown_rx: run.shutdown_rx.clone(),
        state_cache,
    }));
    let metrics = Arc::new(TonicProbeMetrics::default());
    let mut observers = Vec::new();
    for (handle, label) in run
        .setup
        .rr
        .handles
        .iter()
        .flatten()
        .zip(RR_IO_OBSERVER_LABELS)
    {
        let metrics = handle.metrics();
        observers.push(LivenessObserver::from_fn(label, move || {
            metrics.loop_tick_count.load(Ordering::Relaxed)
        }));
    }
    let estop_metrics = run
        .setup
        .estop
        .as_ref()
        .expect("真实 EStop ready 后已存句柄")
        .metrics();
    let observed_estop = Arc::clone(&estop_metrics);
    observers.push(LivenessObserver::from_fn("estop_pub", move || {
        observed_estop.loop_tick_count.load(Ordering::Relaxed)
    }));
    let state_metrics = run
        .setup
        .state_pool
        .as_ref()
        .expect("真实 state pool 已安装")
        .thread_metrics();
    for metrics in state_metrics {
        let label: &'static str =
            Box::leak(format!("state_pool:{}", metrics.hardware_id).into_boxed_str());
        let metrics = Arc::clone(metrics);
        observers.push(LivenessObserver::from_fn(label, move || {
            metrics.loop_tick_count.load(Ordering::Relaxed)
        }));
    }
    let observed_probe = Arc::clone(&metrics);
    observers.push(LivenessObserver::from_fn("tonic_probe", move || {
        observed_probe.success_count.load(Ordering::Acquire)
    }));
    #[cfg(feature = "test-freeze-hook")]
    {
        let context = crate::freeze_control::FreezeControlContext {
            robot_id: run.cfg.robot_id.clone(),
            clock: Arc::clone(&run.clock),
            rr_io: run
                .setup
                .rr
                .handles
                .iter()
                .flatten()
                .map(RrIoThreadHandle::metrics)
                .collect(),
            estop_pub: estop_metrics,
            state_pool: state_metrics.to_vec(),
            tonic_probe: Arc::clone(&metrics),
        };
        match crate::freeze_control::spawn_freeze_control_thread(context) {
            Ok(handle) => run.setup.freeze = Some(handle),
            Err(error) => tracing::error!(
                ?error,
                "gw freeze_control thread spawn failed; control inactive"
            ),
        }
    }
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (bind_tx, bind_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(run_gateway_with_ready(
        run.cfg.clone(),
        handler,
        shutdown_rx.clone(),
        Some(bind_tx),
    ));
    run.setup.serve = Some(ServeOwned {
        server: crate::liveness::TaskSlot::new(server),
        shutdown_tx,
        shutdown_rx,
        bind_rx: Some(bind_rx),
        first_rx: None,
        first_timer: None,
        probe: None,
        probe_metrics: metrics,
        observers: Some(observers),
        watchdog: None,
        watchdog_joined: None,
        ready: false,
        stopping: false,
    });
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "借用外层原任务槽推进就绪、监督和停止，各取消点保持同一拥有者"
)]
async fn drive_serve(run: &mut GatewayRun) {
    let serve = run
        .setup
        .serve
        .as_mut()
        .expect("Serving 必持真实 ServeOwned");
    if serve.bind_rx.is_some() {
        let bind = tokio::select! {
            result = serve.bind_rx.as_mut().expect("原 bind receiver") => Some(result),
            result = serve.server.running_mut().expect("原 server 尚未 join") => { serve.server.record_join(result); None },
            () = wait_shutdown(&mut run.shutdown_rx) => None,
        };
        match bind {
            Some(Ok(Ok(addr))) => {
                serve.bind_rx = None;
                if let Some(ready) = run.ready.take() {
                    let _ = ready.send(Ok(addr));
                }
                let (first_tx, first_rx) = tokio::sync::oneshot::channel();
                serve.probe = Some(spawn_tonic_probe(
                    addr,
                    Arc::clone(&serve.probe_metrics),
                    serve.shutdown_rx.clone(),
                    first_tx,
                ));
                serve.first_rx = Some(first_rx);
                serve.first_timer = Some(Box::pin(tokio::time::sleep(FIRST_SUCCESS_TIMEOUT)));
            }
            Some(Ok(Err(error))) => {
                if let Some(ready) = run.ready.take() {
                    let _ = ready.send(Err(error.clone()));
                }
                run.fail(error.into());
                return;
            }
            Some(Err(error)) => {
                run.fail(GatewayError::ServerTask(format!(
                    "bind-ready 断链: {error}"
                )));
                return;
            }
            None => {
                if let Some(ready) = run.ready.take() {
                    let _ = ready.send(Err(GatewayReadyError::ServerTask {
                        message: "shutdown or server exit before bind-ready".into(),
                    }));
                }
                run.phase = GatewayPhase::Closing;
                run.outcome = Some(match serve.server.result() {
                    Some(result) => Err(server_result_error_ref(
                        result,
                        "server task exited before bind-ready",
                    )),
                    None => Ok(()),
                });
                return;
            }
        }
    }
    if !serve.ready {
        enum ReadyObserved {
            Ready,
            Shutdown,
            Failed,
            Timeout,
            Server,
        }
        let event = tokio::select! {
            first = serve.first_rx.as_mut().expect("原 first-success receiver") => match first { Ok(()) => ReadyObserved::Ready, Err(_) => ReadyObserved::Failed },
            () = serve.first_timer.as_mut().expect("原首成计时器").as_mut() => ReadyObserved::Timeout,
            result = serve.server.running_mut().expect("原 server 尚未 join") => { serve.server.record_join(result); ReadyObserved::Server },
            () = wait_shutdown(&mut run.shutdown_rx) => ReadyObserved::Shutdown,
        };
        match event {
            ReadyObserved::Ready => {
                serve.first_rx = None;
                serve.first_timer = None;
                notify_ready();
                serve.watchdog = spawn_watchdog(
                    serve.observers.take().expect("observer 只移交一次"),
                    WatchdogConfig::new("gw_systemd_wd"),
                );
                serve.ready = true;
            }
            ReadyObserved::Shutdown => {
                run.outcome = Some(Ok(()));
                run.phase = GatewayPhase::Closing;
                return;
            }
            ReadyObserved::Failed => {
                run.fail(GatewayError::ServerTask(
                    "tonic probe task exited before first success".into(),
                ));
                return;
            }
            ReadyObserved::Timeout => {
                run.fail(GatewayError::ReadyProbeTimeout {
                    waited_ms: u64::try_from(FIRST_SUCCESS_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
                });
                return;
            }
            ReadyObserved::Server => {
                let error = server_result_error_ref(
                    serve.server.result().expect("原结果已保存"),
                    "server task exited before READY",
                );
                run.fail(error);
                return;
            }
        }
    }
    let server_ended = tokio::select! {
        result = serve.server.running_mut().expect("原运行 server 尚未 join") => { serve.server.record_join(result); true },
        () = wait_shutdown(&mut run.shutdown_rx) => false,
    };
    run.outcome = Some(if server_ended {
        Err(server_result_error_ref(
            serve.server.result().expect("实际 join 结果已保存"),
            "server returned without external shutdown",
        ))
    } else {
        Ok(())
    });
    run.phase = GatewayPhase::Closing;
}

pub async fn finish_gateway_owned(run: &mut GatewayRun) {
    if run.phase == GatewayPhase::Settled {
        return;
    }
    run.phase = GatewayPhase::Closing;
    if let Some(ledger) = &run.setup.ledger {
        ledger.lock().expect("网关责任锁未中毒").stop_admission();
    }
    if let Some(serve) = run.setup.serve.as_mut() {
        if !serve.stopping {
            notify_stopping();
            if let Some(watchdog) = &serve.watchdog {
                watchdog.request_stop();
            }
            serve.stopping = true;
        }
        while let Some(watchdog) = serve.watchdog.as_mut() {
            match watchdog.try_join() {
                robot_health::JoinState::Pending => {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                robot_health::JoinState::Joined(result) => {
                    serve.watchdog_joined = Some(result);
                    serve.watchdog = None;
                }
            }
        }
        let _ = serve.shutdown_tx.send(true);
        if let Some(probe) = &mut serve.probe {
            probe.join().await;
        }
        serve
            .server
            .join(SERVER_JOIN_TIMEOUT, SERVER_ABORT_JOIN_TIMEOUT)
            .await;
        let server_ok =
            !serve.server.timed_out() && matches!(serve.server.result(), Some(Ok(Ok(()))));
        let probe_ok = serve
            .probe
            .as_ref()
            .is_none_or(crate::systemd_probe::ProbeHandle::joined_ok);
        let watchdog_ok = serve.watchdog_joined.as_ref().is_none_or(Result::is_ok);
        if !(server_ok && probe_ok && watchdog_ok) && run.outcome.as_ref().is_none_or(Result::is_ok)
        {
            run.outcome = Some(Err(GatewayError::ServerTask(
                "关闭含原 join 错误、超时或未回收句柄".into(),
            )));
        }
    }
    start_worker_join(&mut run.setup);
    if let Some(slot) = &mut run.setup.blocking_join {
        slot.join_unbounded().await;
        let worker_failed = match slot.result().expect("blocking 任务返回已存原结果") {
            Ok(joined) => joined.join_failed(),
            Err(_) => true,
        };
        if worker_failed && run.outcome.as_ref().is_none_or(Result::is_ok) {
            run.outcome = Some(Err(GatewayError::ServerTask(
                "worker 或 blocking 回收任务真 join 返回 panic".into(),
            )));
        }
    }
    if run.outcome.is_none() {
        run.outcome = Some(Ok(()));
    }
    run.phase = GatewayPhase::Settled;
}

/// 同步部分只完成移交与安装，不等待；之后所有 await 均借 setup 内原任务槽。
fn start_worker_join(setup: &mut GatewaySetup) {
    if setup.blocking_join.is_some() {
        return;
    }
    let has_workers = setup.rr.handles.iter().any(Option::is_some)
        || setup.estop.is_some()
        || setup.state_pool.is_some();
    #[cfg(feature = "test-freeze-hook")]
    let has_workers = has_workers || setup.freeze.is_some();
    if !has_workers {
        return;
    }
    let mut rr = std::mem::take(&mut setup.rr);
    let estop = setup.estop.take();
    let mut pool = setup.state_pool.take();
    #[cfg(feature = "test-freeze-hook")]
    let freeze = setup.freeze.take();
    let task = tokio::task::spawn_blocking(move || {
        #[cfg(feature = "test-freeze-hook")]
        let freeze = freeze.map(|handle| {
            handle.request_stop();
            handle.join()
        });
        rr.stop_and_join_all();
        let estop = estop.map(|handle| {
            handle.request_stop();
            handle.join()
        });
        if let Some(pool) = &mut pool {
            pool.stop();
        }
        JoinedWorkers {
            rr,
            estop,
            pool,
            #[cfg(feature = "test-freeze-hook")]
            freeze,
        }
    });
    setup.blocking_join = Some(crate::liveness::TaskSlot::new(task));
}

fn server_result_error_ref(
    result: &Result<Result<(), GatewayError>, tokio::task::JoinError>,
    context: &str,
) -> GatewayError {
    GatewayError::ServerTask(match result {
        Ok(Ok(())) => context.to_owned(),
        Ok(Err(error)) => error.to_string(),
        Err(error) => format!("server task join: {error}"),
    })
}

async fn wait_shutdown(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}
const SERVER_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_ABORT_JOIN_TIMEOUT: Duration = Duration::from_millis(500);

struct RrTopics {
    clear_estop: String,
    task_invoke: String,
    task_cancel: String,
    task_status: String,
    clear_error: String,
    set_enabled: String,
}
impl RrTopics {
    fn for_robot(robot: &str) -> Self {
        Self {
            clear_estop: format!("/{robot}/control/clear_estop"),
            task_invoke: format!("/{robot}/control/task_invoke"),
            task_cancel: format!("/{robot}/control/task_cancel"),
            task_status: format!("/{robot}/control/task_status"),
            clear_error: format!("/{robot}/control/clear_error"),
            set_enabled: format!("/{robot}/control/set_enabled"),
        }
    }
}
#[derive(Default)]
struct SpawnedRrIo {
    ports: Vec<RrPort>,
    handles: Vec<Option<RrIoThreadHandle>>,
    joined: Vec<std::thread::Result<crate::rr_io_thread::RrIoExit>>,
}
impl SpawnedRrIo {
    fn install(
        &mut self,
        result: Result<(RrIoThreadHandle, RrPort), crate::rr_io_thread::RrIoSpawnError>,
    ) -> Result<(), GatewayError> {
        match result {
            Ok((handle, port)) => {
                self.handles.push(Some(handle));
                self.ports.push(port);
                Ok(())
            }
            Err(crate::rr_io_thread::RrIoSpawnError::ReadyFailed { handle, error }) => {
                self.handles.push(Some(handle));
                Err(GatewayError::RrIoSpawn(std::io::Error::other(error)))
            }
            Err(crate::rr_io_thread::RrIoSpawnError::ThreadSpawn(error)) => {
                Err(GatewayError::RrIoSpawn(error))
            }
        }
    }
    fn stop_and_join_all(&mut self) {
        for port in &self.ports {
            port.stop_admission();
        }
        for handle in self.handles.iter().flatten() {
            handle.request_stop();
        }
        for slot in &mut self.handles {
            if let Some(handle) = slot.take() {
                self.joined.push(handle.join());
            }
        }
    }
    fn empty_and_joined(&self) -> bool {
        self.handles.iter().all(Option::is_none)
            && self.ports.iter().all(RrPort::responsibility_empty)
            && self.joined.iter().all(|result| {
                result
                    .as_ref()
                    .is_ok_and(crate::rr_io_thread::RrIoExit::responsibilities_empty)
            })
    }
}
fn client_factory_for<Req: robot_msg::markers::BodyMarker, Resp: robot_msg::markers::BodyMarker>(
    topic: String,
    kind: crate::wire_credit::TopicKind,
) -> impl FnOnce() -> Result<crate::rr_io_thread::OwnedRrClientPorts<Req, Resp>, String> + Send + 'static
{
    move || {
        let node = NodeBuilder::new()
            .create::<IxService>()
            .map_err(|error| format!("RR Node: {error:?}"))?;
        let profile = match kind {
            crate::wire_credit::TopicKind::S0Control => {
                robot_transport::iceoryx2::RrProfile::S0_CONTROL
            }
            crate::wire_credit::TopicKind::S1Task => robot_transport::iceoryx2::RrProfile::S1_TASK,
        };
        let client =
            robot_transport::iceoryx2::rr_client_with::<Req, Resp>(&node, &topic, &profile)
                .map_err(|error| format!("RR client: {error:?}"))?;
        Ok(crate::rr_io_thread::OwnedRrClientPorts {
            client,
            _node: node,
        })
    }
}

// ============================================================================
// Tests —— A1-3 (2/3-b2-2) handler logic + run_gateway_with_rr_io 锁
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use tokio::time::timeout;

    // ────────────────────────────────────────────────────────────────────────
    // v0.5 候选 B (2/4)：GatewayConfig.state_hardware_ids 校验
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn validate_allows_empty_state_hardware_list() {
        // loopback_default 的 state_hardware_ids 为空 → 合法（D-P1）。
        assert!(GatewayConfig::loopback_default().validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_state_hardware_id() {
        let mut cfg = GatewayConfig::loopback_default();
        cfg.state_hardware_ids = vec!["ok".to_string(), String::new()];
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::EmptyStateHardwareId)
        ));
    }

    // ────────────────────────────────────────────────────────────────────────
    // v0.5 候选 B (2/4)：GetState handler 错误码顺序（D9 / AC-3）+ Ok 转换
    // ────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    #[allow(clippy::float_cmp)] // position_rad 透传精确字面值，float_cmp 误报
    async fn get_state_error_code_ordering_and_ok() {
        // ① 空 hardware_id → InvalidArgument（校验**早于**池查找，即使空池）。
        let h_empty = GatewayHandler::for_test();
        let err = h_empty
            .get_state(tonic::Request::new(GetStateRequest {
                hardware_id: String::new(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // ② 未知 hardware（不在池）→ NotFound（禁 fallback，AC-9）。
        let err = h_empty
            .get_state(tonic::Request::new(GetStateRequest {
                hardware_id: "nope".to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);

        // ③ 已知 hardware 但无首帧 → Unavailable。
        let cache = Arc::new(StateCache::new(&["arm".to_string()]));
        let h = GatewayHandler::for_test_with_state_cache(Arc::clone(&cache));
        let err = h
            .get_state(tonic::Request::new(GetStateRequest {
                hardware_id: "arm".to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unavailable);

        // ④ 有首帧 → Ok + 字段转换正确（robot_id 来自 handler state）。
        let body = robot_msg::build_helpers::build_joint_state(7, 123, &[1.0, 2.0], &[], &[], "");
        cache.store("arm", body);
        let rs = h
            .get_state(tonic::Request::new(GetStateRequest {
                hardware_id: "arm".to_string(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(rs.robot_id, "for_test");
        assert_eq!(rs.hardware_id, "arm");
        assert_eq!(rs.layout_id, 7);
        assert_eq!(rs.stamp_ns, 123);
        assert_eq!(rs.position_rad, vec![1.0, 2.0]);
    }

    // ────────────────────────────────────────────────────────────────────────
    // v0.5 候选 B (3/4)：StreamState 节流 clamp（D5a）+ 开流校验 + 帧转换
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn clamp_stream_interval_d5a() {
        assert_eq!(clamp_stream_interval_ms(0), 50); // 默认
        assert_eq!(clamp_stream_interval_ms(1), 20); // floor
        assert_eq!(clamp_stream_interval_ms(19), 20); // floor
        assert_eq!(clamp_stream_interval_ms(20), 20); // 边界原值
        assert_eq!(clamp_stream_interval_ms(100), 100); // 原值
    }

    #[tokio::test]
    async fn stream_state_open_time_error_codes() {
        // ① 空 hardware_id → InvalidArgument。
        let h_empty = GatewayHandler::for_test();
        let err = h_empty
            .stream_state(tonic::Request::new(StreamRequest {
                hardware_id: String::new(),
                min_interval_ms: 0,
            }))
            .await
            .err()
            .unwrap();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // ② 未知 hardware → NotFound。
        let err = h_empty
            .stream_state(tonic::Request::new(StreamRequest {
                hardware_id: "nope".to_string(),
                min_interval_ms: 0,
            }))
            .await
            .err()
            .unwrap();
        assert_eq!(err.code(), tonic::Code::NotFound);

        // ③ 已知但无首帧 → Unavailable（开流时同 GetState 口径）。
        let cache = Arc::new(StateCache::new(&["arm".to_string()]));
        let h = GatewayHandler::for_test_with_state_cache(cache);
        let err = h
            .stream_state(tonic::Request::new(StreamRequest {
                hardware_id: "arm".to_string(),
                min_interval_ms: 0,
            }))
            .await
            .err()
            .unwrap();
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }

    #[tokio::test]
    #[allow(clippy::float_cmp)] // position_rad 透传精确字面值
    async fn stream_state_emits_converted_frame() {
        use tonic::codegen::tokio_stream::StreamExt;

        let cache = Arc::new(StateCache::new(&["arm".to_string()]));
        let body = robot_msg::build_helpers::build_joint_state(7, 123, &[1.0, 2.0], &[], &[], "");
        cache.store("arm", body);
        let h = GatewayHandler::for_test_with_state_cache(Arc::clone(&cache));

        let resp = h
            .stream_state(tonic::Request::new(StreamRequest {
                hardware_id: "arm".to_string(),
                min_interval_ms: 20,
            }))
            .await
            .unwrap();
        let mut stream = resp.into_inner();

        // interval 首 tick 立即 → 首帧很快到达；字段转换正确。
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.robot_id, "for_test");
        assert_eq!(first.hardware_id, "arm");
        assert_eq!(first.layout_id, 7);
        assert_eq!(first.stamp_ns, 123);
        assert_eq!(first.position_rad, vec![1.0, 2.0]);

        // drop response stream → tick task 下次 send 失败后退出（不 panic / 不挂）。
        drop(stream);
    }

    // ────────────────────────────────────────────────────────────────────────
    // loopback bind 校验：accept loopback / reject 非 loopback
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn validate_loopback_accepts_ipv4_localhost() {
        let addr: SocketAddr = "127.0.0.1:50051".parse().unwrap();
        assert!(validate_loopback_bind(&addr).is_ok());
    }

    #[test]
    fn validate_loopback_accepts_ipv4_localhost_zero_port() {
        // reviewer 拍板：测试端口 127.0.0.1:0 也必须 accept
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        assert!(validate_loopback_bind(&addr).is_ok());
    }

    #[test]
    fn validate_loopback_accepts_ipv4_loopback_range() {
        // 127.0.0.0/8 整个 range 都是 loopback（is_loopback() 真）
        let addr: SocketAddr = "127.42.42.42:8080".parse().unwrap();
        assert!(validate_loopback_bind(&addr).is_ok());
    }

    #[test]
    fn validate_loopback_accepts_ipv6_localhost() {
        let addr: SocketAddr = "[::1]:50051".parse().unwrap();
        assert!(validate_loopback_bind(&addr).is_ok());
    }

    #[test]
    fn validate_loopback_rejects_ipv4_wildcard() {
        let addr: SocketAddr = "0.0.0.0:50051".parse().unwrap();
        match validate_loopback_bind(&addr) {
            Err(LoopbackBindError::NonLoopback(got)) => assert_eq!(got, addr),
            other => panic!("expected NonLoopback, got {other:?}"),
        }
    }

    #[test]
    fn validate_loopback_rejects_ipv6_wildcard() {
        let addr: SocketAddr = "[::]:50051".parse().unwrap();
        match validate_loopback_bind(&addr) {
            Err(LoopbackBindError::NonLoopback(got)) => assert_eq!(got, addr),
            other => panic!("expected NonLoopback, got {other:?}"),
        }
    }

    #[test]
    fn validate_loopback_rejects_lan_address() {
        let addr: SocketAddr = "192.168.1.10:50051".parse().unwrap();
        assert!(matches!(
            validate_loopback_bind(&addr),
            Err(LoopbackBindError::NonLoopback(_))
        ));
    }

    #[test]
    fn validate_loopback_rejects_external_address() {
        let addr: SocketAddr = "8.8.8.8:50051".parse().unwrap();
        assert!(matches!(
            validate_loopback_bind(&addr),
            Err(LoopbackBindError::NonLoopback(_))
        ));
    }

    #[test]
    fn validate_loopback_rejects_link_local() {
        let addr: SocketAddr = "169.254.1.1:50051".parse().unwrap();
        assert!(matches!(
            validate_loopback_bind(&addr),
            Err(LoopbackBindError::NonLoopback(_))
        ));
    }

    // ────────────────────────────────────────────────────────────────────────
    // run_gateway: non-loopback 拒启动 (不实际起 server)
    // ────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_gateway_rejects_non_loopback_bind_before_serving() {
        let cfg = GatewayConfig {
            listen_addr: "0.0.0.0:50051".parse().unwrap(),
            robot_id: "test".to_string(),
            idempotency_ttl_secs: 300,
            idempotency_max_entries: 4096,
            rr_timeout_grace_ms: 500,
            state_hardware_ids: Vec::new(),
        };
        let (_tx, rx) = watch::channel(false);
        let result = run_gateway(cfg, Arc::new(GatewayHandler::for_test()), rx).await;
        match result {
            Err(GatewayError::Config(ConfigError::LoopbackBind(
                LoopbackBindError::NonLoopback(_),
            ))) => {}
            other => panic!("expected Config(LoopbackBind), got {other:?}"),
        }
    }

    // ────────────────────────────────────────────────────────────────────────
    // run_gateway: tonic server 真起 + shutdown signal 触发 graceful stop
    // ────────────────────────────────────────────────────────────────────────

    /// `127.0.0.1:0` (OS 自动分配端口) bind → tonic server 起来后通过
    /// shutdown_rx 触发 graceful stop；await 应在 1s 内返回 Ok(())。
    ///
    /// **A1-3 (2/3-b1) reviewer 残留 #2 修复**：把旧
    /// `tokio::time::sleep(50ms)` 等 readiness 改成 `ready_rx.await`
    /// 拿真实绑定的 SocketAddr，强 readiness 信号。
    #[tokio::test]
    async fn run_gateway_loopback_bind_starts_and_graceful_stops_on_shutdown_signal() {
        let cfg = GatewayConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            robot_id: "test".to_string(),
            idempotency_ttl_secs: 300,
            idempotency_max_entries: 4096,
            rr_timeout_grace_ms: 500,
            state_hardware_ids: Vec::new(),
        };
        let (tx, rx) = watch::channel(false);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        let handle = tokio::spawn(async move {
            run_gateway_with_ready(
                cfg,
                Arc::new(GatewayHandler::for_test()),
                rx,
                Some(ready_tx),
            )
            .await
        });

        // 强 readiness：等 server 真 bind 完成 + 拿到 actual_addr
        let actual_addr = timeout(Duration::from_secs(2), ready_rx)
            .await
            .expect("ready signal within 2s")
            .expect("ready_tx not dropped")
            .expect("bind succeeds for loopback");
        assert!(
            actual_addr.ip().is_loopback(),
            "actual_addr 应是 loopback；实际 = {actual_addr}"
        );
        assert_ne!(actual_addr.port(), 0, "OS 自动分配的 port 不应是 0");

        // 触发 shutdown
        tx.send(true).expect("send shutdown");

        // server 应在 1s 内 graceful stop（tonic 当前没活连接，立即返回）
        let res = timeout(Duration::from_secs(1), handle)
            .await
            .expect("run_gateway should stop within 1s after shutdown signal")
            .expect("tokio task did not panic");
        assert!(res.is_ok(), "run_gateway returned error: {res:?}");
    }

    // ────────────────────────────────────────────────────────────────────────
    // GatewayConfig::validate: A1-3 (2/3-b1) reviewer 残留 #3 拍板
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn config_validate_loopback_default_ok() {
        let cfg = GatewayConfig::loopback_default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn config_validate_rejects_zero_idempotency_ttl_secs() {
        let mut cfg = GatewayConfig::loopback_default();
        cfg.idempotency_ttl_secs = 0;
        assert_eq!(cfg.validate(), Err(ConfigError::ZeroIdempotencyTtl));
    }

    #[test]
    fn config_validate_rejects_zero_idempotency_max_entries() {
        let mut cfg = GatewayConfig::loopback_default();
        cfg.idempotency_max_entries = 0;
        assert_eq!(cfg.validate(), Err(ConfigError::ZeroIdempotencyMaxEntries));
    }

    #[test]
    fn config_validate_rejects_empty_robot_id() {
        let mut cfg = GatewayConfig::loopback_default();
        cfg.robot_id = String::new();
        assert_eq!(cfg.validate(), Err(ConfigError::EmptyRobotId));
    }

    #[test]
    fn config_validate_rejects_non_loopback_listen_addr() {
        let mut cfg = GatewayConfig::loopback_default();
        cfg.listen_addr = "0.0.0.0:50051".parse().unwrap();
        match cfg.validate() {
            Err(ConfigError::LoopbackBind(LoopbackBindError::NonLoopback(_))) => {}
            other => panic!("expected LoopbackBind, got {other:?}"),
        }
    }

    /// `rr_timeout_grace_ms = 0` 是合法的（reviewer 拍板：0 也合理；
    /// handler 仍按 timeout_ms 等）。
    #[test]
    fn config_validate_accepts_zero_rr_timeout_grace_ms() {
        let mut cfg = GatewayConfig::loopback_default();
        cfg.rr_timeout_grace_ms = 0;
        assert!(cfg.validate().is_ok());
    }

    // ────────────────────────────────────────────────────────────────────────
    // run_gateway_with_ready: ready_tx 边界 (reviewer 边界 1)
    // ────────────────────────────────────────────────────────────────────────

    /// **A1-3 (2/3-b1) reviewer 边界 1**：config validation 失败时
    /// ready_tx 也要 send `Err(GatewayError)`，让 ready_rx 端不会永久
    /// 阻塞 await。
    #[tokio::test]
    async fn ready_tx_receives_err_when_config_validation_fails() {
        let mut cfg = GatewayConfig::loopback_default();
        cfg.idempotency_ttl_secs = 0; // 触发 validation fail
        let (_tx, rx) = watch::channel(false);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        let _handle = tokio::spawn(async move {
            run_gateway_with_ready(
                cfg,
                Arc::new(GatewayHandler::for_test()),
                rx,
                Some(ready_tx),
            )
            .await
        });

        let result = timeout(Duration::from_secs(2), ready_rx)
            .await
            .expect("ready signal within 2s")
            .expect("ready_tx not dropped");
        match result {
            Err(GatewayReadyError::Config(ConfigError::ZeroIdempotencyTtl)) => {}
            other => panic!("expected ZeroIdempotencyTtl, got {other:?}"),
        }
    }

    /// **A1-3 (2/3-b1) reviewer 边界 1**：bind 失败时 ready_tx 也 send
    /// `Err(GatewayReadyError::Bind { .. })`。用 dual-bind 制造端口占用。
    #[tokio::test]
    async fn ready_tx_receives_err_when_bind_fails() {
        // 1) 先起一个真 server 占住一个 loopback 端口
        let cfg_a = GatewayConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            robot_id: "occupy".to_string(),
            idempotency_ttl_secs: 300,
            idempotency_max_entries: 4096,
            rr_timeout_grace_ms: 500,
            state_hardware_ids: Vec::new(),
        };
        let (shutdown_a_tx, shutdown_a_rx) = watch::channel(false);
        let (ready_a_tx, ready_a_rx) = tokio::sync::oneshot::channel();
        let handle_a = tokio::spawn(async move {
            run_gateway_with_ready(
                cfg_a,
                Arc::new(GatewayHandler::for_test()),
                shutdown_a_rx,
                Some(ready_a_tx),
            )
            .await
        });
        let occupied_addr = timeout(Duration::from_secs(2), ready_a_rx)
            .await
            .expect("server A ready within 2s")
            .expect("ready_tx not dropped")
            .expect("server A binds successfully");

        // 2) 第二个 server 试图 bind 同一端口 → 应失败
        let cfg_b = GatewayConfig {
            listen_addr: occupied_addr,
            robot_id: "conflict".to_string(),
            idempotency_ttl_secs: 300,
            idempotency_max_entries: 4096,
            rr_timeout_grace_ms: 500,
            state_hardware_ids: Vec::new(),
        };
        let (_shutdown_b_tx, shutdown_b_rx) = watch::channel(false);
        let (ready_b_tx, ready_b_rx) = tokio::sync::oneshot::channel();
        let handle_b = tokio::spawn(async move {
            run_gateway_with_ready(
                cfg_b,
                Arc::new(GatewayHandler::for_test()),
                shutdown_b_rx,
                Some(ready_b_tx),
            )
            .await
        });

        let result = timeout(Duration::from_secs(2), ready_b_rx)
            .await
            .expect("server B ready signal within 2s")
            .expect("ready_tx not dropped");
        match result {
            Err(GatewayReadyError::Bind {
                addr,
                kind,
                message: _,
            }) => {
                assert_eq!(addr, occupied_addr);
                assert_eq!(kind, std::io::ErrorKind::AddrInUse);
            }
            other => panic!("expected Bind err, got {other:?}"),
        }

        // server B run task 也应返 Bind err
        let res_b = timeout(Duration::from_secs(2), handle_b)
            .await
            .expect("server B task within 2s")
            .expect("tokio task no panic");
        assert!(matches!(res_b, Err(GatewayError::Bind { .. })));

        // 收尾 server A
        shutdown_a_tx.send(true).unwrap();
        let _ = timeout(Duration::from_secs(2), handle_a).await;
    }

    // ════════════════════════════════════════════════════════════════════════
    // GatewayHandler logic 12 步单测（A1-3 (2/3-b2-2)）
    //
    // 模式：用 crossbeam_channel 直接 read forward channel 充当 mock daemon；
    // 单线程（tokio test 主线程）跑 spawn_blocking 拿 wire request → 回 wire
    // response（绕开真起 rr_io_thread + iceoryx2 RR client）。
    // ════════════════════════════════════════════════════════════════════════

    use crossbeam_channel::{Receiver as CrossbeamRecv, bounded};
    use robot_core::time::SystemClock;

    use robot_msg::msg::{ClearEStopRequest as FbReq, RpcStatus, SetEnabledStatus};
    use uuid::Uuid;

    use crate::auth::{Capability, Role, load_role_table};
    use crate::idempotency::{CachedResponse, IdempotencyCache, Method};
    use crate::proto::{AuthContext, ClearEStopCommand, CommandMeta, SetEnabledCommand};

    const TEST_RPC_ROLES_TOML: &str = r#"
[tokens]
"viewer-fake-token-001" = "viewer"
"operator-fake-token-001" = "operator"
"maintainer-fake-token-001" = "maintainer"
"admin-fake-token-001" = "admin"

[roles.viewer]
description = "viewer"
capabilities = ["read_state"]

[roles.operator]
description = "operator"
capabilities = ["read_state", "set_enabled", "estop"]

[roles.maintainer]
description = "maintainer"
capabilities = ["read_state", "clear_estop", "set_enabled", "estop"]

[roles.admin]
description = "admin"
capabilities = ["read_state", "clear_estop", "set_enabled", "estop", "shutdown"]
"#;

    /// 构造完整 handler state；caller 拿走 forward_rx 充当 mock daemon。
    /// shutdown_rx 始终 false。
    fn build_test_set_enabled_response(
        request_id: &str,
        status: RpcStatus,
        domain: SetEnabledStatus,
        at: u64,
        error: &str,
    ) -> Vec<u8> {
        robot_msg::build_helpers::build_set_enabled_response_typed(
            request_id,
            status,
            domain,
            at,
            error,
            match status {
                RpcStatus::Ok => robot_msg::msg::SideEffect::Committed,
                _ => robot_msg::msg::SideEffect::None,
            },
            true,
        )
    }

    fn build_test_clear_estop_response(
        request_id: &str,
        status: RpcStatus,
        error: &str,
        at: u64,
        dry_run: bool,
    ) -> Vec<u8> {
        use robot_msg::build_helpers::{ClearResponseMeta, build_clear_estop_response_typed};
        use robot_msg::msg::{ClearEStopStatus, SideEffect};
        build_clear_estop_response_typed(
            request_id,
            status,
            error,
            at,
            dry_run,
            ClearResponseMeta {
                domain: ClearEStopStatus::Unknown,
                superseded: false,
                effect: match status {
                    RpcStatus::Ok => SideEffect::Committed,
                    _ => SideEffect::None,
                },
                retired: true,
            },
        )
    }

    fn make_handler_with_state(
        cache_max_entries: usize,
    ) -> (
        GatewayHandler,
        crate::gateway_ledger::handler_fixture::TestRrReceiver,
    ) {
        let (handler, _shutdown_tx, forward_rx) = make_handler_with_shutdown_tx(cache_max_entries);
        (handler, forward_rx)
    }

    /// 跟 [`make_handler_with_state`] 同效但额外返回 shutdown_tx，让 (2/3-c)
    /// shutdown gate 测试可主动 set true 触发 fast-fail 路径。`shutdown_tx`
    /// **不**能 drop（drop 后 receiver borrow 拿到的 last value 仍 false，
    /// 但 receiver.changed() 会 RecvError —— 测试只 borrow 不调 changed，所
    /// 以即使 drop 也安全；返回 sender 让 test 可以显式 send(true)）。
    fn make_handler_with_shutdown_tx(
        cache_max_entries: usize,
    ) -> (
        GatewayHandler,
        watch::Sender<bool>,
        crate::gateway_ledger::handler_fixture::TestRrReceiver,
    ) {
        let role_table = Arc::new(load_role_table(TEST_RPC_ROLES_TOML).unwrap());
        let ledger = crate::gateway_ledger::handler_fixture::ledger(cache_max_entries);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (estop_pub_tx, _estop_rx) = bounded(1);
        let mut state = GatewayHandler::test_state(
            ledger,
            role_table,
            shutdown_rx,
            estop_pub_tx,
            Arc::new(StateCache::new(&[])),
        );
        state.robot_id = "test_robot".into();
        let receiver =
            crate::gateway_ledger::handler_fixture::receiver(&state.rr_forward_tx_clear_estop);
        (GatewayHandler::new(state), shutdown_tx, receiver)
    }

    /// 构造一个合法 maintainer ClearEStopCommand。`idempotency_key` 由调用方指定。
    fn make_cmd(idempotency_key: &str) -> ClearEStopCommand {
        ClearEStopCommand {
            meta: Some(CommandMeta {
                request_id: Uuid::now_v7().to_string(),
                source: "test.unit".into(),
                auth: Some(AuthContext {
                    token: "maintainer-fake-token-001".into(),
                    mtls_fingerprint: String::new(),
                }),
                timeout_ms: 2_000,
                idempotency_key: idempotency_key.into(),
                dry_run: false,
                audit_reason: "unit test".into(),
            }),
            confirmation: "operator-loosqk-confirm-001".into(),
        }
    }

    /// mock daemon：阻塞等一条 forward request → 用给定 wire response bytes
    /// 回 oneshot。返回 forward request 的 wire request_id（test 断言用）。
    fn mock_daemon_reply_with_status(
        forward_rx: &crate::gateway_ledger::handler_fixture::TestRrReceiver,
        status: RpcStatus,
        cleared_at_ns: u64,
        dry_run: bool,
        error_msg: &str,
        max_wait: Duration,
    ) -> String {
        let req = forward_rx
            .recv_timeout(max_wait)
            .expect("mock daemon: forward request not received");
        let view = flatbuffers::root::<FbReq>(&req.wire_bytes).expect("decode wire request");
        let request_id = view.request_id().unwrap_or("").to_string();
        let resp_bytes =
            build_test_clear_estop_response(&request_id, status, error_msg, cleared_at_ns, dry_run);
        req.oneshot_tx
            .send(resp_bytes)
            .expect("mock daemon: oneshot recv side already dropped");
        request_id
    }

    // ──────────────────────────────────────────────────────────────────────
    // happy path：daemon 返 Ok → handler 返 Ack + commit cache
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_happy_path_returns_ack_and_caches() {
        let (handler, forward_rx) = make_handler_with_state(64);
        let cmd = make_cmd("idem-happy-1");
        let cache = Arc::clone(&handler.state().ledger);

        // mock daemon spawn（阻塞 thread 充当）
        let mock = tokio::task::spawn_blocking(move || {
            mock_daemon_reply_with_status(
                &forward_rx,
                RpcStatus::Ok,
                1_234_567,
                false,
                "",
                Duration::from_secs(2),
            )
        });

        let resp = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect("ok response");
        let ack = resp.into_inner();
        assert_eq!(ack.cleared_at_ns, 1_234_567);
        assert!(!ack.dry_run);

        let mocked_request_id = mock.await.unwrap();
        assert_eq!(ack.request_id, mocked_request_id);

        // cache 应已 commit（下次同 key + 同 fingerprint = Replay）
        let cache_lock = cache.lock().unwrap();
        assert_eq!(cache_lock.len(), 1);
    }

    // ──────────────────────────────────────────────────────────────────────
    // idempotency: 同 key + 同 fingerprint → Replay（不再 forward）
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_idempotency_replays_cached_ack_without_forwarding() {
        let (handler, forward_rx) = make_handler_with_state(64);
        let key = "idem-replay-1";

        // 第一次：跑完整 happy path 让 cache commit
        {
            let cmd = make_cmd(key);
            let forward_rx_clone = forward_rx.clone();
            let mock = tokio::task::spawn_blocking(move || {
                mock_daemon_reply_with_status(
                    &forward_rx_clone,
                    RpcStatus::Ok,
                    111_111,
                    false,
                    "",
                    Duration::from_secs(2),
                )
            });
            let _ = handler
                .clear_e_stop(tonic::Request::new(cmd))
                .await
                .unwrap();
            mock.await.unwrap();
        }

        // 第二次：同 idempotency_key + 等价语义字段（fingerprint 一致）→
        // 必须 Replay，**不**再 forward。如果 handler 错误地 forward，
        // forward_rx.recv 会拿到 request 而下面 try_recv 会成功 → 测试失败。
        let cmd2 = make_cmd(key);
        let resp2 = handler
            .clear_e_stop(tonic::Request::new(cmd2))
            .await
            .expect("replay should succeed");
        let ack2 = resp2.into_inner();
        assert_eq!(ack2.cleared_at_ns, 111_111);
        assert!(!ack2.dry_run);

        // 验证 forward_rx 没新东西（handler 走 Replay 路径未 forward）
        assert!(
            forward_rx.try_recv().is_err(),
            "Replay 路径不应 forward；forward_rx 应空"
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // idempotency: 同 key + 不同 fingerprint → AlreadyExists（不 forward）
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_idempotency_conflict_returns_already_exists() {
        let (handler, forward_rx) = make_handler_with_state(64);
        let key = "idem-conflict-1";

        // 第一次：commit cache with confirmation A
        let mut cmd_a = make_cmd(key);
        cmd_a.confirmation = "confirm-A".into();
        let forward_rx_a = forward_rx.clone();
        let mock_a = tokio::task::spawn_blocking(move || {
            mock_daemon_reply_with_status(
                &forward_rx_a,
                RpcStatus::Ok,
                222_222,
                false,
                "",
                Duration::from_secs(2),
            )
        });
        handler
            .clear_e_stop(tonic::Request::new(cmd_a))
            .await
            .unwrap();
        mock_a.await.unwrap();

        // 第二次：同 key 但 confirmation B（fingerprint 不同）→ Conflict
        let mut cmd_b = make_cmd(key);
        cmd_b.confirmation = "confirm-B".into();
        let err = handler
            .clear_e_stop(tonic::Request::new(cmd_b))
            .await
            .expect_err("conflict should fail");
        assert_eq!(err.code(), tonic::Code::AlreadyExists);
        assert!(forward_rx.try_recv().is_err(), "Conflict 不应 forward");
    }

    // ──────────────────────────────────────────────────────────────────────
    // idempotency: cache 满 → ResourceExhausted（不 forward）
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_idempotency_capacity_returns_resource_exhausted() {
        let (handler, forward_rx) = make_handler_with_state(1);
        // 第一次填 cache
        let cmd1 = make_cmd("idem-cap-1");
        let forward_rx_1 = forward_rx.clone();
        let mock = tokio::task::spawn_blocking(move || {
            mock_daemon_reply_with_status(
                &forward_rx_1,
                RpcStatus::Ok,
                333_333,
                false,
                "",
                Duration::from_secs(2),
            )
        });
        handler
            .clear_e_stop(tonic::Request::new(cmd1))
            .await
            .unwrap();
        mock.await.unwrap();

        // 第二次：新 key + cache 已满 → Capacity
        let cmd2 = make_cmd("idem-cap-2");
        let err = handler
            .clear_e_stop(tonic::Request::new(cmd2))
            .await
            .expect_err("capacity should fail");
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
        assert!(forward_rx.try_recv().is_err(), "Capacity 不应 forward");
    }

    // ──────────────────────────────────────────────────────────────────────
    // validation: missing meta → InvalidArgument
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn handler_validation_missing_meta_returns_invalid_argument() {
        let (handler, forward_rx) = make_handler_with_state(64);
        let cmd = ClearEStopCommand {
            meta: None,
            confirmation: "irrelevant".into(),
        };
        let err = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect_err("missing meta should fail");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            forward_rx.try_recv().is_err(),
            "validation fail 不应 forward"
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // auth: token 不在 role table → Unauthenticated
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn handler_auth_unknown_token_returns_unauthenticated() {
        let (handler, forward_rx) = make_handler_with_state(64);
        let mut cmd = make_cmd("idem-auth-1");
        cmd.meta.as_mut().unwrap().auth.as_mut().unwrap().token = "unknown-token".into();
        let err = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect_err("unknown token should fail");
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert!(forward_rx.try_recv().is_err(), "auth fail 不应 forward");
    }

    // ──────────────────────────────────────────────────────────────────────
    // authz: viewer / operator 不得调 ClearEStop → PermissionDenied
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn handler_authz_operator_role_returns_permission_denied() {
        let (handler, forward_rx) = make_handler_with_state(64);
        let mut cmd = make_cmd("idem-authz-1");
        cmd.meta.as_mut().unwrap().auth.as_mut().unwrap().token = "operator-fake-token-001".into();
        let err = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect_err("operator should be denied");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(forward_rx.try_recv().is_err(), "authz fail 不应 forward");
    }

    #[tokio::test]
    async fn handler_authz_viewer_role_returns_permission_denied() {
        let (handler, forward_rx) = make_handler_with_state(64);
        let mut cmd = make_cmd("idem-authz-2");
        cmd.meta.as_mut().unwrap().auth.as_mut().unwrap().token = "viewer-fake-token-001".into();
        let err = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect_err("viewer should be denied");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(forward_rx.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_authz_admin_role_succeeds() {
        let (handler, forward_rx) = make_handler_with_state(64);
        let mut cmd = make_cmd("idem-authz-admin");
        cmd.meta.as_mut().unwrap().auth.as_mut().unwrap().token = "admin-fake-token-001".into();
        let mock = tokio::task::spawn_blocking(move || {
            mock_daemon_reply_with_status(
                &forward_rx,
                RpcStatus::Ok,
                444_444,
                false,
                "",
                Duration::from_secs(2),
            )
        });
        let resp = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect("admin should succeed");
        assert_eq!(resp.into_inner().cleared_at_ns, 444_444);
        mock.await.unwrap();
    }

    // ──────────────────────────────────────────────────────────────────────
    // forward channel: rr_io_thread 已停（rx drop）→ Unavailable
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn handler_forward_channel_disconnected_returns_unavailable() {
        let (handler, forward_rx) = make_handler_with_state(64);
        // 立即 drop rx 模拟 rr_io_thread 已停
        drop(forward_rx);
        let cmd = make_cmd("idem-forward-disc");
        let err = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect_err("disconnected should fail");
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }

    // ──────────────────────────────────────────────────────────────────────
    // oneshot: rr_io_thread shutdown drops oneshot_tx → handler RecvError →
    // Unavailable（reviewer 边界 2）
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_oneshot_recv_error_returns_unavailable() {
        let (handler, forward_rx) = make_handler_with_state(64);
        let cmd = make_cmd("idem-oneshot-drop");
        // mock daemon：拿到 forward 后**不** reply，直接 drop oneshot_tx
        let mock = tokio::task::spawn_blocking(move || {
            let req = forward_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("forward request");
            drop(req.oneshot_tx);
        });
        let err = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect_err("oneshot drop should fail");
        assert_eq!(err.code(), tonic::Code::Unavailable);
        assert!(
            err.message().contains("gateway RR I/O unavailable"),
            "应明确说 gateway RR I/O 不可用，不暗示 daemon 失败；got: {}",
            err.message()
        );
        mock.await.unwrap();
    }

    // ──────────────────────────────────────────────────────────────────────
    // timeout: daemon 不 reply → handler tokio::time::timeout 命中 →
    // DeadlineExceeded
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_timeout_returns_deadline_exceeded() {
        let (handler, forward_rx) = make_handler_with_state(64);
        let mut cmd = make_cmd("idem-timeout-1");
        cmd.meta.as_mut().unwrap().timeout_ms = 50; // 50ms + grace 500ms = 550ms total
        // 改 grace 让 timeout 不要太长（默认 grace=500ms）
        let state = handler.state();
        let _ = state; // 单独路径调整 grace 不影响：mock daemon 直接 hold oneshot
        // mock daemon: hold oneshot_tx 永不 reply（超过 timeout_ms + grace）
        let _hold = tokio::task::spawn_blocking(move || {
            let req = forward_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            // 持有 oneshot_tx，让 handler tokio::time::timeout 命中
            std::thread::sleep(Duration::from_secs(2));
            drop(req); // 测试结束前才 drop
        });
        let err = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect_err("timeout should fire");
        assert_eq!(err.code(), tonic::Code::DeadlineExceeded);
    }

    // ──────────────────────────────────────────────────────────────────────
    // wire status non-Ok: 失败状态映射 + **不**缓存
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_wire_failed_precondition_maps_and_does_not_cache() {
        let (handler, forward_rx) = make_handler_with_state(64);
        let cmd = make_cmd("idem-fp-1");
        let cache = Arc::clone(&handler.state().ledger);

        let mock = tokio::task::spawn_blocking(move || {
            mock_daemon_reply_with_status(
                &forward_rx,
                RpcStatus::FailedPrecondition,
                0,
                false,
                "safety_state must be TorqueOffConfirmed",
                Duration::from_secs(2),
            )
        });

        let err = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect_err("failed precondition should fail");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(
            err.message().contains("FailedPrecondition")
                || err.message().contains("TorqueOffConfirmed"),
            "error_msg 应透传 daemon 的人类可读描述；got: {}",
            err.message()
        );
        mock.await.unwrap();

        // **关键不变量**：失败不缓存。下次同 key retry 应是 FreshInsert。
        let cache_lock = cache.lock().unwrap();
        assert_eq!(
            cache_lock.len(),
            0,
            "失败状态**不**应缓存；状态依赖型失败 retry 必须能重新走 fresh"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_wire_aborted_maps_to_aborted_and_does_not_cache() {
        let (handler, forward_rx) = make_handler_with_state(64);
        let cmd = make_cmd("idem-aborted-1");
        let cache = Arc::clone(&handler.state().ledger);
        let mock = tokio::task::spawn_blocking(move || {
            mock_daemon_reply_with_status(
                &forward_rx,
                RpcStatus::Aborted,
                0,
                false,
                "preempted by emergency",
                Duration::from_secs(2),
            )
        });
        let err = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect_err("aborted should fail");
        assert_eq!(err.code(), tonic::Code::Aborted);
        mock.await.unwrap();
        assert_eq!(cache.lock().unwrap().len(), 0);
    }

    // ──────────────────────────────────────────────────────────────────────
    // dry_run: gateway **不**短路；wire dry_run flag 透传给 daemon；
    // daemon 端评估返 Ok（dry_run=true），handler 把 dry_run flag 传回 Ack
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_dry_run_passes_through_to_daemon_and_returns_dry_run_ack() {
        let (handler, forward_rx) = make_handler_with_state(64);
        let mut cmd = make_cmd("idem-dryrun-1");
        cmd.meta.as_mut().unwrap().dry_run = true;

        // mock daemon：assert wire dry_run=true，回 Ok + dry_run=true
        let mock = tokio::task::spawn_blocking(move || {
            let req = forward_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("forward request");
            let view = flatbuffers::root::<FbReq>(&req.wire_bytes).expect("decode");
            assert!(view.dry_run(), "wire dry_run flag 必须透传到 daemon");
            let request_id = view.request_id().unwrap_or("").to_string();
            let resp = build_test_clear_estop_response(
                &request_id,
                RpcStatus::Ok,
                "",
                999_111,
                true, // dry_run echoed
            );
            req.oneshot_tx.send(resp).unwrap();
        });

        let resp = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect("dry_run ok");
        let ack = resp.into_inner();
        assert!(ack.dry_run, "Ack.dry_run 应跟 wire 一致");
        assert_eq!(ack.cleared_at_ns, 999_111);
        mock.await.unwrap();
    }

    // ──────────────────────────────────────────────────────────────────────
    // wire role: 显式 audit 字段映射检查
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn role_to_wire_maps_each_variant_explicitly() {
        assert_eq!(role_to_wire(Role::Viewer), RpcRole::Viewer);
        assert_eq!(role_to_wire(Role::Operator), RpcRole::Operator);
        assert_eq!(role_to_wire(Role::Maintainer), RpcRole::Maintainer);
        assert_eq!(role_to_wire(Role::Admin), RpcRole::Admin);
    }

    // ──────────────────────────────────────────────────────────────────────
    // wire request: deadline_ns / stamp_ns / wire role 写入正确
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_wire_request_carries_correct_deadline_role_and_dry_run_flag() {
        let (handler, forward_rx) = make_handler_with_state(64);
        let mut cmd = make_cmd("idem-wire-1");
        cmd.meta.as_mut().unwrap().timeout_ms = 1_000;

        let before_ns = SystemClock.now().0;
        let mock = tokio::task::spawn_blocking(move || {
            let req = forward_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("forward request");
            let view = flatbuffers::root::<FbReq>(&req.wire_bytes).expect("decode");
            let role = view.role();
            let deadline_ns = view.deadline_ns();
            let stamp_ns = view.stamp_ns();
            let dry_run = view.dry_run();
            // reply Ok 让 handler 干净退出
            let request_id = view.request_id().unwrap_or("").to_string();
            req.oneshot_tx
                .send(build_test_clear_estop_response(
                    &request_id,
                    RpcStatus::Ok,
                    "",
                    1,
                    false,
                ))
                .unwrap();
            (role, deadline_ns, stamp_ns, dry_run)
        });

        let _ = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .unwrap();
        let (role, deadline_ns, stamp_ns, dry_run) = mock.await.unwrap();

        assert_eq!(role, RpcRole::Maintainer);
        assert!(!dry_run);
        // deadline_ns 应在 [stamp_ns, stamp_ns + 1.5s] 内（timeout=1s + 余量）
        assert!(
            deadline_ns >= stamp_ns,
            "deadline_ns 应不小于 stamp_ns；got deadline={deadline_ns} stamp={stamp_ns}"
        );
        assert!(
            deadline_ns <= stamp_ns.saturating_add(1_500_000_000),
            "deadline_ns 应在 stamp+timeout_ms+余量 内"
        );
        assert!(
            stamp_ns >= before_ns,
            "stamp_ns 应不早于调用前快照；got stamp={stamp_ns} before={before_ns}"
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // edge: idempotency cache 的 ClearEStop variant 仅存
    // cleared_at_ns/dry_run；request_id 不入 cache（每次重试不同，重建时
    // 由 handler 用 incoming request 的值 echo）。
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn cached_clear_estop_response_carries_cleared_at_ns_and_dry_run_only() {
        let resp = CachedResponse::ClearEStop {
            cleared_at_ns: 7,
            dry_run: true,
        };
        match resp {
            CachedResponse::ClearEStop {
                cleared_at_ns,
                dry_run,
            } => {
                assert_eq!(cleared_at_ns, 7);
                assert!(dry_run);
            }
            other => panic!("expected ClearEStop variant, got {other:?}"),
        }
    }

    // ──────────────────────────────────────────────────────────────────────
    // GatewayHandler::for_test：构造合法且 ClearEStop 立刻 fail（auth 步）
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn for_test_handler_rejects_clear_estop_due_to_empty_role_table() {
        let handler = GatewayHandler::for_test();
        let cmd = make_cmd("idem-fortest-1");
        let err = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect_err("for_test handler 应在 auth 步直接拒（role table 空）");
        // for_test 用空 role_table，token 找不到 → Unauthenticated
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    // ──────────────────────────────────────────────────────────────────────
    // wire 不变量：Capability::ClearEStop 仅 maintainer / admin 拥有
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn role_table_clear_estop_invariant_holds_for_test_table() {
        let table = load_role_table(TEST_RPC_ROLES_TOML).unwrap();
        assert!(table.role_has_capability(Role::Maintainer, Capability::ClearEStop));
        assert!(table.role_has_capability(Role::Admin, Capability::ClearEStop));
        assert!(!table.role_has_capability(Role::Operator, Capability::ClearEStop));
        assert!(!table.role_has_capability(Role::Viewer, Capability::ClearEStop));
    }

    // ──────────────────────────────────────────────────────────────────────
    // **A1-3 (2/3-b2-2) round 2 阻断 2 锁测**：handler 必须验 wire response
    // request_id 跟当前 request 一致；mismatch → Internal + **不**缓存
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_wire_response_request_id_mismatch_returns_internal_and_does_not_cache() {
        let (handler, forward_rx) = make_handler_with_state(64);
        let cmd = make_cmd("idem-mismatch-1");
        let cache = Arc::clone(&handler.state().ledger);

        // mock daemon：拿到 forward 后**故意**用错的 request_id 回 wire
        // response（同时 wire status = Ok，让 mismatch 校验是唯一拦截点；
        // 否则 Ok 路径会 commit cache）
        let mock = tokio::task::spawn_blocking(move || {
            let req = forward_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("forward request");
            // assert mock 看到的 wire request_id 跟入参一致（防 test 自身 bug）
            let view = flatbuffers::root::<FbReq>(&req.wire_bytes).expect("decode wire request");
            let expected = view.request_id().unwrap_or("").to_string();
            // 回错的 request_id（"wrong"），status=Ok
            let resp = build_test_clear_estop_response("wrong", RpcStatus::Ok, "", 12_345, false);
            req.oneshot_tx
                .send(resp)
                .expect("oneshot recv side dropped");
            expected
        });

        let err = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect_err("mismatched response request_id should fail");
        assert_eq!(err.code(), tonic::Code::Unavailable);
        assert!(
            err.message().contains("unavailable"),
            "error message 应明确 mismatch；got: {}",
            err.message()
        );

        let real_request_id = mock.await.unwrap();
        assert_ne!(
            real_request_id, "wrong",
            "mock 看到的 wire request_id 不该是 'wrong'（防 test 自身 bug）"
        );

        // 关键不变量：mismatch 路径**不**缓存（drop insert_handle 不 commit）
        let cache_lock = cache.lock().unwrap();
        assert_eq!(
            cache_lock.len(),
            1,
            "request_id mismatch 必须保留非终态原责任；不匹配帧不能退休，原责任必须保留"
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // **A1-3 (2/3-b2-2) round 2 阻断 1 锁测**：run_gateway_with_rr_io 必须先
    // cfg.validate() 再 spawn rr_io_thread；invalid config 返 Config(_)
    // **不**触碰 iceoryx2 RR client 路径
    // ──────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_gateway_with_rr_io_invalid_config_returns_config_before_rr_spawn() {
        // (a) 空 robot_id：必须返 Config(EmptyRobotId)（**不**RrIoSpawn）
        {
            let cfg = GatewayConfig {
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                robot_id: String::new(),
                idempotency_ttl_secs: 300,
                idempotency_max_entries: 4096,
                rr_timeout_grace_ms: 500,
                state_hardware_ids: Vec::new(),
            };
            let role_table = Arc::new(load_role_table(TEST_RPC_ROLES_TOML).unwrap());
            let clock: Arc<dyn RobotClock> = Arc::new(SystemClock);
            let (_tx, rx) = watch::channel(false);
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let mut run = GatewayRun::new(cfg, role_table, clock, rx, Some(ready_tx));
            run_gateway_with_rr_io(&mut run).await;
            assert!(run.setup.rr.handles.is_empty() && run.setup.blocking_join.is_none());
            let result = run.into_exit().unwrap().try_release_empty().unwrap();
            match result {
                Err(GatewayError::Config(ConfigError::EmptyRobotId)) => {}
                other => panic!("expected Config(EmptyRobotId); got {other:?}"),
            }
            // ready_tx 也应送等价错误（防永久阻塞）
            let ready = timeout(Duration::from_secs(1), ready_rx)
                .await
                .expect("ready signal within 1s")
                .expect("ready_tx not dropped");
            assert!(matches!(
                ready,
                Err(GatewayReadyError::Config(ConfigError::EmptyRobotId))
            ));
        }

        // (b) 非 loopback bind：必须返
        // Config(LoopbackBind(NonLoopback(_)))（**不**走 spawn 路径）
        {
            let cfg = GatewayConfig {
                listen_addr: "0.0.0.0:50051".parse().unwrap(),
                robot_id: "test".to_string(),
                idempotency_ttl_secs: 300,
                idempotency_max_entries: 4096,
                rr_timeout_grace_ms: 500,
                state_hardware_ids: Vec::new(),
            };
            let role_table = Arc::new(load_role_table(TEST_RPC_ROLES_TOML).unwrap());
            let clock: Arc<dyn RobotClock> = Arc::new(SystemClock);
            let (_tx, rx) = watch::channel(false);
            let mut run = GatewayRun::new(cfg, role_table, clock, rx, None);
            run_gateway_with_rr_io(&mut run).await;
            assert!(run.setup.rr.handles.is_empty() && run.setup.blocking_join.is_none());
            let result = run.into_exit().unwrap().try_release_empty().unwrap();
            match result {
                Err(GatewayError::Config(ConfigError::LoopbackBind(
                    LoopbackBindError::NonLoopback(_),
                ))) => {}
                other => panic!("expected Config(LoopbackBind); got {other:?}"),
            }
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // **A1-3 (2/3-c)** graceful shutdown 语义锁测
    //
    // 设计：cache.decide 之后、forward 之前检查 shutdown_rx。
    //   - shutdown=false：handler 正常走 happy path
    //   - shutdown=true 且 cache 命中（Replay）：仍返 cached Ack（shutdown
    //     **不**影响已成功的请求 retry）
    //   - shutdown=true 且 FreshInsert：drop handle + 返
    //     `Unavailable("gateway is shutting down; ...")`
    //   - **已 forward**（in-flight）的请求：shutdown 不强制中断；handler
    //     按原 timeout_ms + grace 等 RR reply（tonic
    //     serve_with_incoming_shutdown 自然让 in-flight handler 跑完）
    // ════════════════════════════════════════════════════════════════════════

    /// shutdown=true → 新 fresh request → Unavailable + cache 不留记录。
    #[tokio::test]
    async fn handler_shutdown_rejects_fresh_request_with_unavailable_and_does_not_cache() {
        let (handler, shutdown_tx, forward_rx) = make_handler_with_shutdown_tx(64);
        let cache = Arc::clone(&handler.state().ledger);

        // 先 set shutdown=true
        shutdown_tx.send(true).expect("send shutdown");

        let cmd = make_cmd("idem-shutdown-1");
        let err = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect_err("shutdown should reject fresh request");
        assert_eq!(err.code(), tonic::Code::Unavailable);
        assert!(
            err.message().contains("shutting down"),
            "error message 应明确 shutdown 原因；got: {}",
            err.message()
        );

        // **不**应 forward
        assert!(
            forward_rx.try_recv().is_err(),
            "shutdown gate 不应 forward；forward_rx 应空"
        );
        // **不**应 commit cache（drop insert_handle，下次 retry 重新 fresh）
        assert_eq!(
            cache.lock().unwrap().len(),
            0,
            "shutdown 拒绝路径**不**应缓存"
        );
    }

    /// shutdown=true 但 cache 命中 → Replay 返 cached Ack（**不**受 shutdown
    /// 影响）。reviewer 拍板边界：cached 成功的 retry 不该因为 gateway
    /// shutdown 拒绝（已经 cleared 的 estop retry 不需要再走 daemon）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_shutdown_does_not_affect_replay_path() {
        let (handler, shutdown_tx, forward_rx) = make_handler_with_shutdown_tx(64);
        let key = "idem-shutdown-replay-1";

        // 第一次：跑完整 happy path 让 cache commit（shutdown 仍 false）
        {
            let cmd = make_cmd(key);
            let forward_rx_clone = forward_rx.clone();
            let mock = tokio::task::spawn_blocking(move || {
                mock_daemon_reply_with_status(
                    &forward_rx_clone,
                    RpcStatus::Ok,
                    777_777,
                    false,
                    "",
                    Duration::from_secs(2),
                )
            });
            handler
                .clear_e_stop(tonic::Request::new(cmd))
                .await
                .unwrap();
            mock.await.unwrap();
        }

        // 现在 set shutdown=true
        shutdown_tx.send(true).expect("send shutdown");

        // 第二次：同 key + 同 fingerprint → 应走 Replay 路径返 cached Ack
        let cmd2 = make_cmd(key);
        let resp = handler
            .clear_e_stop(tonic::Request::new(cmd2))
            .await
            .expect("Replay path should succeed even during shutdown");
        let ack = resp.into_inner();
        assert_eq!(ack.cleared_at_ns, 777_777);

        // 仍**不** forward（Replay 路径 Step 5 短路返）
        assert!(
            forward_rx.try_recv().is_err(),
            "Replay 不该 forward；shutdown 进 Step 5.5 之前已经短路"
        );
    }

    /// shutdown 在 forward **之后**触发：已进 handler 且已 forward 的请求
    /// 不被 shutdown 中断，按原 timeout 等 RR reply 返 Ack。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_shutdown_after_forward_does_not_interrupt_in_flight() {
        let (handler, shutdown_tx, forward_rx) = make_handler_with_shutdown_tx(64);
        let cmd = make_cmd("idem-shutdown-inflight-1");

        // mock daemon：拿到 forward 后**先** set shutdown=true，**后**回
        // wire response（模拟 in-flight 期间触发 shutdown）
        let shutdown_tx_for_mock = shutdown_tx.clone();
        let mock = tokio::task::spawn_blocking(move || {
            let req = forward_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("forward request");
            let view = flatbuffers::root::<FbReq>(&req.wire_bytes).expect("decode wire request");
            let request_id = view.request_id().unwrap_or("").to_string();

            // **关键**：handler 已经过 Step 5.5 + forward；现在 trigger shutdown
            shutdown_tx_for_mock.send(true).expect("send shutdown");
            std::thread::sleep(Duration::from_millis(50));

            // 然后回 Ok wire response
            let resp =
                build_test_clear_estop_response(&request_id, RpcStatus::Ok, "", 555_111, false);
            req.oneshot_tx
                .send(resp)
                .expect("oneshot recv side dropped");
            request_id
        });

        let resp = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect("in-flight request should complete despite shutdown signal");
        let ack = resp.into_inner();
        assert_eq!(
            ack.cleared_at_ns, 555_111,
            "已 forward 的请求应正常等到 RR reply 返 Ack"
        );
        let _request_id = mock.await.unwrap();
    }

    /// 进一步锁住边界：第一条请求 forward 后 shutdown 触发，第二条 fresh
    /// 请求**应**立即返 Unavailable，第一条仍跑完成功。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn handler_shutdown_blocks_new_request_while_in_flight_finishes() {
        let (handler, shutdown_tx, forward_rx) = make_handler_with_shutdown_tx(64);
        let handler = Arc::new(handler);

        // 起一条 in-flight 请求
        let handler_a = Arc::clone(&handler);
        let cmd_a = make_cmd("idem-inflight-A");
        let task_a =
            tokio::spawn(async move { handler_a.clear_e_stop(tonic::Request::new(cmd_a)).await });

        // mock daemon: 等 in-flight forward 到位 → trigger shutdown → 才回 Ok
        let shutdown_tx_for_mock = shutdown_tx.clone();
        let mock = tokio::task::spawn_blocking(move || {
            let req = forward_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("first forward (A)");
            let view = flatbuffers::root::<FbReq>(&req.wire_bytes).expect("decode");
            let request_id = view.request_id().unwrap_or("").to_string();

            // shutdown 触发
            shutdown_tx_for_mock.send(true).expect("send shutdown");
            // 给 handler B 时间撞到 shutdown gate
            std::thread::sleep(Duration::from_millis(50));

            // 回 A 的 Ok
            let resp =
                build_test_clear_estop_response(&request_id, RpcStatus::Ok, "", 888_111, false);
            req.oneshot_tx.send(resp).expect("oneshot");

            // 期望此时**不**再有第二条 forward 进来（B 在 shutdown gate 拒了）
            std::thread::sleep(Duration::from_millis(50));
            forward_rx.try_recv().is_err()
        });

        // 给 mock 一点点时间真触发 shutdown
        tokio::time::sleep(Duration::from_millis(30)).await;

        // 起第二条 fresh 请求（A 还 in-flight；shutdown 应已 set）
        let cmd_b = make_cmd("idem-inflight-B");
        let res_b = handler.clear_e_stop(tonic::Request::new(cmd_b)).await;
        assert!(
            matches!(&res_b, Err(s) if s.code() == tonic::Code::Unavailable),
            "B 应被 shutdown gate 拒；实际 {res_b:?}"
        );

        // A 应正常完成
        let res_a = task_a.await.unwrap();
        let ack_a = res_a.expect("A should complete despite shutdown");
        assert_eq!(ack_a.into_inner().cleared_at_ns, 888_111);

        let no_second_forward = mock.await.unwrap();
        assert!(no_second_forward, "B 不应 forward（被 shutdown gate 拦下）");
    }

    // ════════════════════════════════════════════════════════════════════════
    // v0.5 候选 C SetEnabled handler 单测
    // ════════════════════════════════════════════════════════════════════════

    /// 构造 handler，set_enabled = active forward channel（返 rx 充当 mock
    /// daemon）；其它 5 条 leak rx。
    fn make_set_enabled_handler(
        cache_max_entries: usize,
    ) -> (
        GatewayHandler,
        crate::gateway_ledger::handler_fixture::TestRrReceiver,
    ) {
        let (handler, _shutdown, unused_receiver) =
            make_handler_with_shutdown_tx(cache_max_entries);
        handler.state.rr_forward_tx_clear_estop.stop_admission();
        drop(unused_receiver);
        let receiver = crate::gateway_ledger::handler_fixture::receiver(
            &handler.state.rr_forward_tx_set_enabled,
        );
        (handler, receiver)
    }

    fn make_set_enabled_cmd(
        idempotency_key: &str,
        enabled: bool,
        token: &str,
    ) -> SetEnabledCommand {
        SetEnabledCommand {
            meta: Some(CommandMeta {
                request_id: Uuid::now_v7().to_string(),
                source: "test.unit".into(),
                auth: Some(AuthContext {
                    token: token.into(),
                    mtls_fingerprint: String::new(),
                }),
                timeout_ms: 2_000,
                idempotency_key: idempotency_key.into(),
                dry_run: false,
                audit_reason: "unit test".into(),
            }),
            enabled,
        }
    }

    /// 逐条经过真实 handler 与 forward/response 路径填缓存，不能直接插入缓存项。
    async fn fill_set_enabled_cache_with_dry_runs(
        handler: &GatewayHandler,
        forward_rx: crate::gateway_ledger::handler_fixture::TestRrReceiver,
        count: usize,
    ) -> crate::gateway_ledger::handler_fixture::TestRrReceiver {
        let responder = tokio::task::spawn_blocking(move || {
            for index in 0..count {
                let request = forward_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("干跑请求必须实际转发");
                let wire =
                    flatbuffers::root::<robot_msg::msg::SetEnabledRequest>(&request.wire_bytes)
                        .expect("解码干跑请求");
                assert_eq!(wire.request_id(), Some(request.request_id.as_str()));
                assert_eq!(
                    wire.idempotency_key(),
                    Some(format!("h2-dry-run-{index}").as_str())
                );
                assert!(wire.dry_run());
                assert!(!wire.enabled());
                let response = build_test_set_enabled_response(
                    &request.request_id,
                    RpcStatus::Ok,
                    SetEnabledStatus::Ok,
                    u64::try_from(index).unwrap() + 1,
                    "",
                );
                request.oneshot_tx.send(response).expect("干跑仍在等待结果");
            }
            forward_rx
        });

        for index in 0..count {
            let mut command = make_set_enabled_cmd(
                &format!("h2-dry-run-{index}"),
                false,
                "operator-fake-token-001",
            );
            let meta = command.meta.as_mut().unwrap();
            meta.dry_run = true;
            let request_id = meta.request_id.clone();
            let ack = handler
                .set_enabled(tonic::Request::new(command))
                .await
                .expect("容量内的成功干跑必须提交缓存")
                .into_inner();
            assert_eq!(ack.request_id, request_id);
            assert!(ack.dry_run);
            assert_eq!(ack.cleared_at_ns, u64::try_from(index).unwrap() + 1);
        }
        responder.await.expect("干跑回复线程完成")
    }

    /// 普通额度满保持只读/重放/冲突，Lower 与 Restore 使用真实生产预留。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(
        clippy::too_many_lines,
        reason = "同一真实容量夹具连续验证满载、重放和优先级预留，保持原资源生命周期"
    )]
    async fn h2_default_cache_full_preserves_reads_replay_and_priority_reserves() {
        use crate::gateway_ledger::{
            GatewayLedger, RrRoute,
            handler_fixture::{port, receiver},
        };
        let config = GatewayConfig::loopback_default();
        assert_eq!(config.idempotency_max_entries, 4096);
        let ledger = GatewayLedger::new(
            config.idempotency_ttl_secs * 1_000_000_000,
            config.idempotency_max_entries,
        )
        .unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (estop_tx, _estop_rx) = bounded(64);
        let mut state = GatewayHandler::test_state(
            Arc::clone(&ledger),
            Arc::new(load_role_table(TEST_RPC_ROLES_TOML).unwrap()),
            shutdown_rx,
            estop_tx,
            Arc::new(StateCache::new(&["arm".into()])),
        );
        state.robot_id = "test_robot".into();
        state.state_cache.store(
            "arm",
            robot_msg::build_helpers::build_joint_state(7, 123, &[0.0], &[], &[], ""),
        );
        state.rr_forward_tx_set_enabled = port(&ledger, RrRoute::Write(Method::SetEnabled));
        let set_rx = receiver(&state.rr_forward_tx_set_enabled);
        let clear_rx = receiver(&state.rr_forward_tx_clear_estop);
        let handler = GatewayHandler::new(state);
        let normal_capacity = 4096 - 64 - 64 - 128;
        let set_rx = fill_set_enabled_cache_with_dry_runs(&handler, set_rx, normal_capacity).await;
        assert_eq!(ledger.lock().unwrap().len(), normal_capacity);
        let mut normal = make_set_enabled_cmd("normal-full", false, "operator-fake-token-001");
        normal.meta.as_mut().unwrap().dry_run = true;
        let error = handler
            .set_enabled(tonic::Request::new(normal))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::ResourceExhausted);
        assert!(error.message().contains("3840/3840"));
        assert!(set_rx.try_recv().is_err());
        assert_eq!(
            handler
                .get_state(tonic::Request::new(GetStateRequest {
                    hardware_id: "arm".into()
                }))
                .await
                .unwrap()
                .into_inner()
                .stamp_ns,
            123
        );
        let lower_rx = set_rx.clone();
        let lower = tokio::task::spawn_blocking(move || {
            let request = lower_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let view = flatbuffers::root::<robot_msg::msg::SetEnabledRequest>(&request.wire_bytes)
                .unwrap();
            assert!(!view.dry_run() && !view.enabled());
            request
                .oneshot_tx
                .send(build_test_set_enabled_response(
                    &request.request_id,
                    RpcStatus::Ok,
                    SetEnabledStatus::Ok,
                    77,
                    "",
                ))
                .unwrap();
        });
        assert_eq!(
            handler
                .set_enabled(tonic::Request::new(make_set_enabled_cmd(
                    "priority-lower",
                    false,
                    "operator-fake-token-001"
                )))
                .await
                .unwrap()
                .into_inner()
                .cleared_at_ns,
            77
        );
        lower.await.unwrap();
        let restore_rx = clear_rx.clone();
        let restore = tokio::task::spawn_blocking(move || {
            mock_daemon_reply_with_status(
                &restore_rx,
                RpcStatus::Ok,
                78,
                false,
                "",
                Duration::from_secs(2),
            );
        });
        assert_eq!(
            handler
                .clear_e_stop(tonic::Request::new(make_cmd("priority-restore")))
                .await
                .unwrap()
                .into_inner()
                .cleared_at_ns,
            78
        );
        restore.await.unwrap();
        let mut retry = make_set_enabled_cmd("h2-dry-run-0", false, "operator-fake-token-001");
        retry.meta.as_mut().unwrap().dry_run = true;
        assert_eq!(
            handler
                .set_enabled(tonic::Request::new(retry.clone()))
                .await
                .unwrap()
                .into_inner()
                .cleared_at_ns,
            1
        );
        retry.meta.as_mut().unwrap().request_id = Uuid::now_v7().to_string();
        retry.meta.as_mut().unwrap().dry_run = false;
        assert_eq!(
            handler
                .set_enabled(tonic::Request::new(retry))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::AlreadyExists
        );
        assert_eq!(ledger.lock().unwrap().len(), normal_capacity + 2);
        assert!(set_rx.try_recv().is_err() && clear_rx.try_recv().is_err());
        drop(shutdown_tx);
    }

    /// 跑一次 SetEnabled，mock daemon 用**正确** request_id 回指定
    /// `(status, domain)`。返 handler 结果。
    // tonic handler 签名固定 `Result<Response<T>, Status>`（Status ≥176B 在框架
    // 侧），helper 必须同签名转发，Box 不可行。
    #[allow(clippy::result_large_err)]
    async fn run_set_enabled_with(
        status: RpcStatus,
        domain: SetEnabledStatus,
    ) -> Result<tonic::Response<Ack>, tonic::Status> {
        let (handler, rx) = make_set_enabled_handler(64);
        let cmd = make_set_enabled_cmd("idem-statusmap", true, "operator-fake-token-001");
        let mock = tokio::task::spawn_blocking(move || {
            let req = rx.recv_timeout(Duration::from_secs(2)).expect("forward");
            let resp = build_test_set_enabled_response(&req.request_id, status, domain, 7_777, "");
            req.oneshot_tx.send(resp).expect("oneshot recv dropped");
        });
        let res = handler.set_enabled(tonic::Request::new(cmd)).await;
        mock.await.unwrap();
        res
    }

    /// **AC-7**：viewer `SetEnabled` → PermissionDenied（auth 步拒，不 forward）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_enabled_viewer_denied() {
        let (handler, _rx) = make_set_enabled_handler(64);
        let cmd = make_set_enabled_cmd("idem-viewer", true, "viewer-fake-token-001");
        let err = handler
            .set_enabled(tonic::Request::new(cmd))
            .await
            .expect_err("viewer 应被拒");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    /// **复用 rpc_status mapper**：Ok → Ack；ResourceExhausted / Aborted /
    /// Internal / DeadlineExceeded / FailedPrecondition → 对应 tonic code。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_enabled_status_mapping() {
        // Ok → Ack（cleared_at_ns 携 completed_at_ns）。
        let ok = run_set_enabled_with(RpcStatus::Ok, SetEnabledStatus::Ok).await;
        assert_eq!(ok.expect("Ok 应 Ack").into_inner().cleared_at_ns, 7_777);

        for (status, domain, code) in [
            (
                RpcStatus::ResourceExhausted,
                SetEnabledStatus::AlreadyInProgress,
                tonic::Code::ResourceExhausted,
            ),
            (
                RpcStatus::Aborted,
                SetEnabledStatus::PreemptedByEStop,
                tonic::Code::Aborted,
            ),
            (
                RpcStatus::Internal,
                SetEnabledStatus::AdapterError,
                tonic::Code::Internal,
            ),
            (
                RpcStatus::DeadlineExceeded,
                SetEnabledStatus::DeadlineExceeded,
                tonic::Code::DeadlineExceeded,
            ),
            (
                RpcStatus::FailedPrecondition,
                SetEnabledStatus::LatchBlocked,
                tonic::Code::FailedPrecondition,
            ),
        ] {
            let err = run_set_enabled_with(status, domain)
                .await
                .expect_err("非 Ok 应 Err");
            assert_eq!(err.code(), code, "status {status:?} 应映射 {code:?}");
        }
    }

    /// `CachedResponse::SetEnabled` 经 cache commit → replay 无损 roundtrip。
    #[test]
    fn cached_response_set_enabled_roundtrip() {
        let mut cache = IdempotencyCache::with_capacity(Duration::from_mins(1), 64);
        let fp = [7u8; 32];
        let Decision::FreshInsert(h) = cache.decide(Method::SetEnabled, "k-se-1", fp) else {
            panic!("expected FreshInsert");
        };
        h.commit(
            &mut cache,
            CachedResponse::SetEnabled {
                completed_at_ns: 4_242,
                dry_run: true,
            },
        );
        match cache.decide(Method::SetEnabled, "k-se-1", fp) {
            Decision::Replay(CachedResponse::SetEnabled {
                completed_at_ns,
                dry_run,
            }) => {
                assert_eq!(completed_at_ns, 4_242);
                assert!(dry_run);
            }
            other => panic!("expected Replay(SetEnabled), got {other:?}"),
        }
    }

    /// **PB12**：wire response request_id 跟 outgoing 不一致 → Internal +
    /// **不**缓存（沿 ClearEStop Step 11.5）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_enabled_wire_response_request_id_mismatch_returns_internal_and_does_not_cache() {
        let (handler, rx) = make_set_enabled_handler(64);
        let cmd = make_set_enabled_cmd("idem-se-mismatch", true, "operator-fake-token-001");
        let cache = Arc::clone(&handler.state().ledger);

        // mock daemon：**故意**回错 request_id（"wrong"），status=Ok，让 mismatch
        // 校验是唯一拦截点（否则 Ok 会 commit cache）。
        let mock = tokio::task::spawn_blocking(move || {
            let req = rx.recv_timeout(Duration::from_secs(2)).expect("forward");
            let resp = build_test_set_enabled_response(
                "wrong",
                RpcStatus::Ok,
                SetEnabledStatus::Ok,
                12_345,
                "",
            );
            req.oneshot_tx.send(resp).expect("oneshot recv dropped");
            req.request_id
        });

        let err = handler
            .set_enabled(tonic::Request::new(cmd))
            .await
            .expect_err("mismatched response request_id should fail");
        assert_eq!(err.code(), tonic::Code::Unavailable);
        assert!(
            err.message().contains("unavailable"),
            "error message 应明确 mismatch；got: {}",
            err.message()
        );

        let real_request_id = mock.await.unwrap();
        assert_ne!(
            real_request_id, "wrong",
            "mock 看到的 wire request_id 防 test bug"
        );

        // 关键不变量：mismatch 路径**不**缓存。
        assert_eq!(
            cache.lock().unwrap().len(),
            1,
            "request_id mismatch 必须保留非终态原责任"
        );
    }

    // ════════════════════════════════════════════════════════════════════════
    // v0.5 候选 D EStop handler 单测
    // ════════════════════════════════════════════════════════════════════════

    /// 构造 handler，estop_pub = active forward channel（返 rx 充当 mock
    /// publish 线程）；其它 6 条 leak rx。`estop_buf` 控制容量（PB2 Full
    /// unit 用 1）。
    fn make_estop_handler(estop_buf: usize) -> (GatewayHandler, CrossbeamRecv<EStopPubForward>) {
        let role_table = Arc::new(load_role_table(TEST_RPC_ROLES_TOML).unwrap());
        let ledger = crate::gateway_ledger::handler_fixture::ledger(64);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (estop_pub_tx, estop_pub_rx) = bounded(estop_buf);
        let state = GatewayHandler::test_state(
            ledger,
            role_table,
            shutdown_rx,
            estop_pub_tx,
            Arc::new(StateCache::new(&[])),
        );
        (GatewayHandler::new(state), estop_pub_rx)
    }

    fn make_estop_cmd(idempotency_key: &str, token: &str) -> crate::proto::EStopCommand {
        crate::proto::EStopCommand {
            meta: Some(CommandMeta {
                request_id: Uuid::now_v7().to_string(),
                source: "test.unit".into(),
                auth: Some(AuthContext {
                    token: token.into(),
                    mtls_fingerprint: String::new(),
                }),
                timeout_ms: 2_000,
                idempotency_key: idempotency_key.into(),
                dry_run: false,
                audit_reason: "unit test".into(),
            }),
            estop_reason: "unit estop".into(),
        }
    }

    /// mock publish 线程：收一条 forward → oneshot 回 Ok(at_ns)。
    struct TestPublished {
        body_bytes: Vec<u8>,
    }
    fn mock_publish_ok(
        rx: CrossbeamRecv<EStopPubForward>,
        ledger: crate::gateway_ledger::SharedLedger,
        at_ns: u64,
    ) -> tokio::task::JoinHandle<TestPublished> {
        tokio::task::spawn_blocking(move || {
            let mut fwd = rx.recv_timeout(Duration::from_secs(2)).expect("forward");
            {
                let mut guard = ledger.lock().unwrap();
                assert!(fwd.guard.belongs_to(&ledger, fwd.entry_id));
                assert!(guard.transition(
                    fwd.entry_id,
                    crate::class_cache::EntryState::Publishing,
                    fwd.stamp_ns
                ));
                fwd.guard.disarm();
                assert!(guard.finish_estop(fwd.entry_id, at_ns, 0));
            }
            fwd.oneshot_tx
                .send(Ok(at_ns))
                .expect("oneshot recv dropped");
            TestPublished {
                body_bytes: fwd.body_bytes,
            }
        })
    }

    /// **AC-4（gateway 层）**：viewer → PermissionDenied（不 forward）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn estop_viewer_denied() {
        let (handler, _rx) = make_estop_handler(64);
        let err = handler
            .e_stop(tonic::Request::new(make_estop_cmd(
                "idem-es-viewer",
                "viewer-fake-token-001",
            )))
            .await
            .expect_err("viewer 应被拒");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    /// publish Ok → Ack（cleared_at_ns = published_at_ns）+ cache commit +
    /// wire body 字段（reason 透传 / source="rpc" / latched=false，D3/D6）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn estop_publish_ok_acks_and_commits_cache() {
        let (handler, rx) = make_estop_handler(64);
        let cache = Arc::clone(&handler.state().ledger);
        let mock = mock_publish_ok(rx, Arc::clone(&handler.state.ledger), 424_242);

        let ack = handler
            .e_stop(tonic::Request::new(make_estop_cmd(
                "idem-es-ok",
                "operator-fake-token-001",
            )))
            .await
            .expect("publish ok 应 Ack")
            .into_inner();
        assert_eq!(ack.cleared_at_ns, 424_242, "Ack 应携 published_at_ns");
        assert!(!ack.dry_run);

        // wire body 字段断言（mock 收到的 forward）。
        let fwd = mock.await.unwrap();
        let view =
            flatbuffers::root::<robot_msg::msg::EStopEvent>(&fwd.body_bytes).expect("decode");
        assert_eq!(view.reason().unwrap_or(""), "unit estop");
        assert_eq!(view.source().unwrap_or(""), "rpc");
        assert!(!view.latched(), "RPC 路径 latched 必须 false（D3）");

        // cache committed（同 key 二发走 Replay 的前提）。
        assert_eq!(cache.lock().unwrap().len(), 1, "Ok 路径应 commit cache");
    }

    /// **PD2**：forward channel Disconnected（publish 线程死）→ Unavailable。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn estop_forward_disconnected_returns_unavailable() {
        let (handler, rx) = make_estop_handler(64);
        drop(rx); // 模拟 estop_pub_thread 已死
        let err = handler
            .e_stop(tonic::Request::new(make_estop_cmd(
                "idem-es-disc",
                "operator-fake-token-001",
            )))
            .await
            .expect_err("Disconnected 应 Unavailable");
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }

    /// **PB2**：forward channel Full（bounded(1) 预塞满不消费）→
    /// ResourceExhausted。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn estop_forward_channel_full_returns_resource_exhausted() {
        let (handler, rx) = make_estop_handler(1);
        // 真实第一条 handler 创建占位并入队；测试不伪造裸 EntryId 或 guard。
        let first = handler.e_stop(tonic::Request::new(make_estop_cmd(
            "filler",
            "operator-fake-token-001",
        )));
        tokio::pin!(first);
        assert!(
            tokio::time::timeout(Duration::from_millis(5), first.as_mut())
                .await
                .is_err()
        );

        let err = handler
            .e_stop(tonic::Request::new(make_estop_cmd(
                "idem-es-full",
                "operator-fake-token-001",
            )))
            .await
            .expect_err("Full 应 ResourceExhausted");
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
        drop(rx);
    }

    /// **D5/AC-5（handler 层）**：同 key 二发 → Replay 返缓存 Ack（同
    /// published_at_ns），**不**二次 forward（mock 端 forward_rx 只收到 1 条）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn estop_idem_replay_returns_cached_ack_without_second_forward() {
        let (mut handler, rx) = make_estop_handler(64);
        // handler 与固定发布戳使用同一时钟域，避免终态被主机时间当作 TTL 已过期。
        handler.state.clock = Arc::new(robot_core::time::MockClock::new(
            robot_core::time::MonoRawNs(777),
        ));

        // 第一发：mock 回 Ok(777)。fingerprint 字段须与第二发一致 →
        // 固定 source/reason/audit/token，仅 request_id 变（不进 fp）。
        let mut cmd1 = make_estop_cmd("idem-es-replay", "operator-fake-token-001");
        cmd1.meta.as_mut().unwrap().source = "replay.test".into();
        let mut cmd2 = make_estop_cmd("idem-es-replay", "operator-fake-token-001");
        cmd2.meta.as_mut().unwrap().source = "replay.test".into();

        let mock = mock_publish_ok(rx, Arc::clone(&handler.state.ledger), 777);
        let ack1 = handler
            .e_stop(tonic::Request::new(cmd1))
            .await
            .expect("第一发应 Ack")
            .into_inner();
        let fwd = mock.await.unwrap();
        assert_eq!(ack1.cleared_at_ns, 777);

        // 第二发：mock 线程已退出（rx 已被 mock 消费后 drop）——若 handler 误
        // 走第二次 forward 会 Disconnected/超时而非 Replay；Replay 路径不碰
        // channel，直接返缓存。
        let ack2 = handler
            .e_stop(tonic::Request::new(cmd2))
            .await
            .expect("第二发应 Replay Ack")
            .into_inner();
        assert_eq!(ack2.cleared_at_ns, 777, "Replay 应返同 published_at_ns");
        drop(fwd);
    }
}

// ============================================================================
// v0.5 候选 E 段(8) round 2 阻断 2：READY 门槛有界性（hung probe / 提前 shutdown）
// ============================================================================

#[cfg(test)]
mod liveness_gate_tests {
    use super::*;
    use std::time::Instant;
    type HeldStreams = Arc<Mutex<Vec<tokio::net::TcpStream>>>;
    async fn spawn_hung_acceptor() -> (SocketAddr, tokio::task::JoinHandle<()>, HeldStreams) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let held: HeldStreams = Arc::new(Mutex::new(Vec::new()));
        let held_in = Arc::clone(&held);
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                held_in.lock().unwrap().push(stream);
            }
        });
        (addr, task, held)
    }
    fn waiting_run(shutdown: watch::Receiver<bool>) -> GatewayRun {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut server_rx = shutdown_rx.clone();
        let server = tokio::spawn(async move {
            wait_shutdown(&mut server_rx).await;
            Ok(())
        });
        let mut run = GatewayRun::new(
            GatewayConfig::loopback_default(),
            Arc::new(RoleTable::default()),
            Arc::new(SystemClock),
            shutdown,
            None,
        );
        run.phase = GatewayPhase::Serving;
        run.setup.serve = Some(ServeOwned {
            server: crate::liveness::TaskSlot::new(server),
            shutdown_tx,
            shutdown_rx,
            bind_rx: None,
            first_rx: None,
            first_timer: None,
            probe: None,
            probe_metrics: Arc::new(TonicProbeMetrics::default()),
            observers: Some(Vec::new()),
            watchdog: None,
            watchdog_joined: None,
            ready: false,
            stopping: false,
        });
        run
    }
    fn install_hung_probe(run: &mut GatewayRun, address: SocketAddr, timeout: Duration) {
        let serve = run.setup.serve.as_mut().unwrap();
        let (first_tx, first_rx) = tokio::sync::oneshot::channel();
        serve.probe = Some(spawn_tonic_probe(
            address,
            Arc::clone(&serve.probe_metrics),
            serve.shutdown_rx.clone(),
            first_tx,
        ));
        serve.first_rx = Some(first_rx);
        // 测试缩短已拥有计时器，驱动同一生产阶段；生产固定十秒常量不变。
        serve.first_timer = Some(Box::pin(tokio::time::sleep(timeout)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn join_server_returns_even_when_abort_is_not_confirmed() {
        let (entered, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            entered.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(400));
            Ok::<(), GatewayError>(())
        });
        let mut server = crate::liveness::TaskSlot::new(task);
        ready.await.unwrap();
        let started = Instant::now();
        assert!(
            !server
                .join(Duration::from_millis(50), Duration::from_millis(50))
                .await
        );
        assert!(started.elapsed() < Duration::from_millis(300));
        assert!(server.is_residual() && server.result().is_none());
        assert!(
            server.running_mut().is_some(),
            "未确认 abort 的原句柄不能丢弃"
        );
        server.join_unbounded().await;
        assert!(matches!(server.result(), Some(Ok(Ok(())))));
        assert!(server.timed_out(), "事后完成不能抹掉原截止事实");
    }

    #[tokio::test]
    async fn ready_gate_hung_probe_times_out_and_collects_bounded() {
        let (address, acceptor, _held) = spawn_hung_acceptor().await;
        let (_tx, rx) = watch::channel(false);
        let mut run = waiting_run(rx);
        install_hung_probe(&mut run, address, Duration::from_secs(1));
        let timer = run
            .setup
            .serve
            .as_ref()
            .unwrap()
            .first_timer
            .as_ref()
            .unwrap()
            .deadline();
        // 中断第一次等待后仍借原 timer；不得重新获得完整一秒。
        assert!(
            tokio::time::timeout(Duration::from_millis(20), drive_serve(&mut run))
                .await
                .is_err()
        );
        assert_eq!(
            run.setup
                .serve
                .as_ref()
                .unwrap()
                .first_timer
                .as_ref()
                .unwrap()
                .deadline(),
            timer
        );
        let started = Instant::now();
        drive_serve(&mut run).await;
        assert!(matches!(
            run.outcome,
            Some(Err(GatewayError::ReadyProbeTimeout { .. }))
        ));
        finish_gateway_owned(&mut run).await;
        assert!(started.elapsed() < Duration::from_secs(4));
        let serve = run.setup.serve.as_ref().unwrap();
        assert!(serve.server.is_joined() && !serve.ready && serve.watchdog.is_none());
        assert!(serve.probe_metrics.failure_count.load(Ordering::Relaxed) >= 1);
        assert!(serve.probe.as_ref().unwrap().is_joined());
        let _held_exit = std::mem::ManuallyDrop::new(run.into_exit().unwrap());
        acceptor.abort();
        assert!(acceptor.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn ready_gate_external_shutdown_before_ready_returns_none_bounded() {
        let (address, acceptor, _held) = spawn_hung_acceptor().await;
        let (tx, rx) = watch::channel(false);
        let mut run = waiting_run(rx);
        install_hung_probe(&mut run, address, Duration::from_secs(30));
        let trigger = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            tx.send(true).unwrap();
            tx
        });
        let started = Instant::now();
        drive_serve(&mut run).await;
        assert!(matches!(run.outcome, Some(Ok(()))));
        finish_gateway_owned(&mut run).await;
        assert!(started.elapsed() < Duration::from_secs(4));
        let serve = run.setup.serve.as_ref().unwrap();
        assert!(serve.server.is_joined() && !serve.ready && serve.watchdog.is_none());
        run.into_exit()
            .unwrap()
            .try_release_empty()
            .unwrap()
            .unwrap();
        let _keep = trigger.await.unwrap();
        acceptor.abort();
        assert!(acceptor.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn await_bind_ready_external_shutdown_first_returns_none_and_rejects_ready_tx() {
        let (tx, rx) = watch::channel(false);
        let mut run = waiting_run(rx);
        let (_bind_tx, bind_rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        run.ready = Some(ready_tx);
        run.setup.serve.as_mut().unwrap().bind_rx = Some(bind_rx);
        tx.send(true).unwrap();
        drive_serve(&mut run).await;
        assert!(matches!(run.outcome, Some(Ok(()))));
        finish_gateway_owned(&mut run).await;
        assert!(run.setup.serve.as_ref().unwrap().server.is_joined());
        assert!(matches!(
            ready_rx.await,
            Ok(Err(GatewayReadyError::ServerTask { .. }))
        ));
        run.into_exit()
            .unwrap()
            .try_release_empty()
            .unwrap()
            .unwrap();
    }
}

#[cfg(all(test, unix))]
mod owned_join_tests {
    use super::*;
    use std::io::Read;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CHILD: &str = "ROBOT_W3_GATEWAY_JOIN_CHILD";

    // 父进程提供真实时间上界；失败断言不能靠丢弃活 worker 或 Tokio runtime Drop 收尾。
    #[test]
    fn cancelled_finish_reuses_blocking_owner_and_preserves_both_join_errors() {
        if std::env::var_os(CHILD).is_some() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(run_owned_join_child());
            std::process::exit(0);
        }
        let stdout = tempfile::tempfile().unwrap();
        let stderr = tempfile::tempfile().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gateway::owned_join_tests::cancelled_finish_reuses_blocking_owner_and_preserves_both_join_errors", "--nocapture"])
            .env(CHILD, "1").stdout(stdout.try_clone().unwrap()).stderr(stderr.try_clone().unwrap()).spawn().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut timed_out = false;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                timed_out = true;
                child.kill().unwrap();
                break child.wait().unwrap();
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let read_all = |mut file: std::fs::File| {
            use std::io::{Seek, SeekFrom};
            file.seek(SeekFrom::Start(0)).unwrap();
            let mut text = String::new();
            file.read_to_string(&mut text).unwrap();
            text
        };
        let output = read_all(stdout);
        let errors = read_all(stderr);
        assert!(
            !timed_out && status.success(),
            "子进程 {status}: {output} {errors}"
        );
        assert_eq!(output.matches("gateway_owned_join_verified spawn=1 std_join=1 resumed=true tokio_progress=true std_panic=true task_panic=true").count(), 1, "{output}");
        println!(
            "gateway_owned_join_verified spawn=1 std_join=1 resumed=true tokio_progress=true std_panic=true task_panic=true child_reaped=true"
        );
    }

    fn empty_run() -> GatewayRun {
        let (_tx, rx) = watch::channel(false);
        GatewayRun::new(
            GatewayConfig::loopback_default(),
            Arc::new(RoleTable::default()),
            Arc::new(SystemClock),
            rx,
            None,
        )
    }

    #[allow(
        clippy::too_many_lines,
        reason = "单个受监督子进程完整验证取消、续接与两层真实线程回收结果"
    )]
    async fn run_owned_join_child() {
        let mut run = empty_run();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_in = Arc::clone(&calls);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        run.setup
            .rr
            .handles
            .push(Some(crate::rr_io_thread::spawn_join_fixture(move || {
                calls_in.fetch_add(1, Ordering::Relaxed);
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })));
        entered_rx.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), finish_gateway_owned(&mut run))
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let original_id = run
            .setup
            .blocking_join
            .as_mut()
            .unwrap()
            .running_mut()
            .unwrap()
            .id();
        assert!(run.setup.blocking_join.as_ref().unwrap().result().is_none());
        assert!(
            run.setup.rr.handles.is_empty(),
            "原标准句柄已完整移交 blocking owner"
        );
        // 单 worker runtime 中这条任务仍能推进，证明 std join 没在 Tokio worker 上同步等待。
        let progressed = tokio::spawn(async {
            tokio::task::yield_now().await;
            7
        })
        .await
        .unwrap();
        assert_eq!(progressed, 7);
        assert_eq!(
            run.setup
                .blocking_join
                .as_mut()
                .unwrap()
                .running_mut()
                .unwrap()
                .id(),
            original_id
        );
        release_tx.send(()).unwrap();
        finish_gateway_owned(&mut run).await;
        finish_gateway_owned(&mut run).await;
        let joined = run
            .setup
            .blocking_join
            .as_ref()
            .unwrap()
            .result()
            .unwrap()
            .as_ref()
            .unwrap();
        assert_eq!(joined.rr.joined.len(), 1);
        assert!(joined.rr.joined[0].is_ok());
        assert!(joined.rr.handles.iter().all(Option::is_none));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        run.into_exit()
            .unwrap()
            .try_release_empty()
            .unwrap()
            .unwrap();

        let mut std_panic = empty_run();
        std_panic
            .setup
            .rr
            .handles
            .push(Some(crate::rr_io_thread::spawn_join_fixture(|| {
                panic!("gateway_true_std_panic")
            })));
        finish_gateway_owned(&mut std_panic).await;
        let aggregate = std_panic
            .setup
            .blocking_join
            .as_ref()
            .unwrap()
            .result()
            .unwrap()
            .as_ref()
            .unwrap();
        let panic = aggregate.rr.joined[0].as_ref().unwrap_err();
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"gateway_true_std_panic")
        );
        assert!(std_panic.outcome.as_ref().unwrap().is_err());
        let std_panic = std_panic
            .into_exit()
            .unwrap()
            .try_release_empty()
            .unwrap_err();
        let _held_std_panic = std::mem::ManuallyDrop::new(std_panic);

        let mut task_panic = empty_run();
        task_panic.setup.blocking_join = Some(crate::liveness::TaskSlot::new(
            tokio::task::spawn_blocking(|| -> JoinedWorkers { panic!("gateway_true_task_panic") }),
        ));
        finish_gateway_owned(&mut task_panic).await;
        assert!(
            matches!(task_panic.setup.blocking_join.as_ref().unwrap().result(), Some(Err(error)) if error.is_panic())
        );
        assert!(task_panic.outcome.as_ref().unwrap().is_err());
        let task_panic = task_panic
            .into_exit()
            .unwrap()
            .try_release_empty()
            .unwrap_err();
        let _held_task_panic = std::mem::ManuallyDrop::new(task_panic);
        println!(
            "gateway_owned_join_verified spawn=1 std_join=1 resumed=true tokio_progress=true std_panic=true task_panic=true"
        );
    }
}
