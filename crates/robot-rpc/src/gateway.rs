//! `robot_rpc_gateway` 服务端核心 —— A1-3 (2/3-c) graceful shutdown
//! 完整语义落地（handler shutdown gate + tonic
//! `serve_with_incoming_shutdown` 让 in-flight handler 跑完 + rr_io_thread
//! 在 tonic 退出之后才 stop/join）。
//!
//! ## 角色
//!
//! reviewer A1-3 (2/3-a) 拍板的 binary 薄壳模型：
//!
//! - **bin (`src/bin/robot_rpc_gateway.rs`)**：`#[tokio::main]` + CLI parse +
//!   loopback bind 校验 + 加载 role table + 起 [`run_gateway_with_rr_io`]；
//!   不持业务逻辑。
//! - **lib (本模块)**：[`GatewayHandler`] + [`run_gateway_with_rr_io`]
//!   加 [`run_gateway`] + 公共 [`GatewayConfig`] / [`LoopbackBindError`]
//!   / [`GatewayReadyError`]。
//!
//! ## A1-3 (2/3-b2-2) scope（本轮）
//!
//! - [`GatewayHandler`] 真起 12 步 logic：validate → auth (token →
//!   role) → authz (role has `clear_estop` cap) → fingerprint →
//!   `IdempotencyCache::decide` (Replay / Conflict / Capacity / FreshInsert)
//!   → 计算 deadline_ns → 映射 wire `RpcRole` → build wire `ClearEStopRequest`
//!   bytes → forward via [`crate::rr_io_thread`]（`crossbeam_channel::try_send`
//!   Full → `ResourceExhausted`，Disconnected → `Unavailable`）→
//!   `tokio::time::timeout(timeout_ms + grace_ms)` 等 oneshot reply（Timeout
//!   → `DeadlineExceeded`，RecvError → `Unavailable`）→ decode wire
//!   `ClearEStopResponse` → [`crate::clear_estop::rpc_status_to_tonic_code`]
//!   显式 match → `Ok` 路径 commit cache + 返 `Ack`；非 `Ok` drop
//!   InsertHandle（**不**缓存失败状态：状态依赖型失败 retry 仍可重新走 fresh）。
//! - [`run_gateway_with_rr_io`]：高层入口。spawn rr_io_thread → 构造
//!   handler → 调 [`run_gateway_with_ready`] → tonic 退出后 stop / join
//!   rr_io_thread（reviewer A1-3 (2/3-b2-1) 关键边界：rr_io_thread 生命周期
//!   **必须**在 tonic server 退出之后才停）。
//!
//! ## A1-3 (2/3-c) graceful shutdown 语义（本轮新加）
//!
//! 三段式：
//!
//! 1. **新请求 fast-fail**：handler `clear_e_stop` 入口在 `cache.decide`
//!    之后、forward 之前 check `*shutdown_rx.borrow()`；shutdown=true 且
//!    走到 `Decision::FreshInsert` → drop `InsertHandle` + 返
//!    `Unavailable("gateway is shutting down; new requests rejected")`。
//!    `Replay` / `Conflict` / `Capacity` 路径**不**受影响（cached 成功
//!    的 retry 不该因为 gateway shutdown 拒绝；已经 cleared 的 estop
//!    retry 也不需要再走 daemon）。
//! 2. **已 forward 的请求继续跑完**：tonic
//!    `serve_with_incoming_shutdown` 自然让 in-flight handler 跑完（含
//!    `oneshot.await` + RR reply）；handler 按原 `timeout_ms + grace_ms`
//!    等 daemon reply，shutdown 不强制中断。
//! 3. **rr_io_thread 在 tonic 退出之后才停**：[`run_gateway_with_rr_io`]
//!    在 `run_gateway_with_ready` 返回**之后**才调
//!    `rr_handle.request_stop()` + `join`（reviewer A1-3 (2/3-b2-1) 关
//!    键边界：避免 handler 还在等 RR 时 I/O 被掐）。
//!
//! ## 不在 A1-3 (2/3-c) 范围
//!
//! - 端到端两进程集成测（A1-3 (3/3)）。
//!
//! ## 不变量
//!
//! - **wire `role` 字段仅 audit**：daemon 端**不**做授权决策；本 module
//!   完成 token → role → capability 查表，viewer / operator 直接返
//!   `PermissionDenied`，**不**让 wire request 出门（reviewer A1-1 拍板）。
//! - **dry_run 不在 gateway 短路**：仅 set wire `dry_run` flag 透传给
//!   daemon action layer（reviewer A1 拍板硬边界）。
//! - **`request_id` 不进 idempotency fingerprint**：每次 retry 客户端必须
//!   生成新 UUIDv7 `request_id` 但用稳定 `idempotency_key`；fingerprint 由
//!   [`crate::clear_estop::clear_estop_idempotency_fingerprint`] 排除
//!   `request_id` / `timeout_ms` / `idempotency_key` 自身。
//! - **失败不缓存**：cache 仅在 wire response `RpcStatus::Ok` 路径 commit
//!   `InsertHandle`；非 Ok（含 `FailedPrecondition` / `Internal` /
//!   `DeadlineExceeded` / `ResourceExhausted` / `Aborted` 等）drop handle，
//!   下次同 key retry 走 FreshInsert（避免把状态依赖失败永久缓存）。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use iceoryx2::prelude::NodeBuilder;
use iceoryx2::service::ipc::Service as IxService;
use robot_core::time::RobotClock;
use robot_msg::build_helpers::{
    build_clear_estop_request, build_estop_event, build_set_enabled_request,
};
use robot_msg::markers;
use robot_msg::msg::{
    ClearEStopResponse as FbResp, JointState, RpcRole, SetEnabledResponse as FbSetEnabledResp,
};
use robot_transport::iceoryx2::request_response::{Iceoryx2RrClient, rr_client};
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
use crate::idempotency::{CachedResponse, Decision, IdempotencyCache, Method};
use crate::proto::robot_control_server::{RobotControl, RobotControlServer};
use crate::proto::{
    Ack, CancelTaskCommand, ClearEStopCommand, ClearErrorCommand, EStopCommand, GetStateRequest,
    InvokeTaskCommand, RobotState, SetEnabledCommand, StreamRequest, TaskEvent, TaskHandle,
    TaskHandleRef, TaskStatus,
};
use crate::rr_io_thread::{
    RrForwardRequest, RrIoThreadConfig, RrIoThreadHandle, spawn_rr_io_thread,
};
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
    pub rr_forward_tx_clear_estop: Sender<RrForwardRequest>,
    /// `InvokeTask` 转发 wire request（B4 (2/3-b)）。
    pub rr_forward_tx_task_invoke: Sender<RrForwardRequest>,
    /// `CancelTask` 转发 wire request（B4 (2/3-b)）。
    pub rr_forward_tx_task_cancel: Sender<RrForwardRequest>,
    /// `GetTaskStatus` 转发 wire request（B4 (2/3-b)；只读 RPC，**不**进
    /// idempotency cache）。
    pub rr_forward_tx_task_status: Sender<RrForwardRequest>,
    /// `ClearError` 转发 wire request（B4 (2/3-b)）。
    pub rr_forward_tx_clear_error: Sender<RrForwardRequest>,
    /// `SetEnabled` 转发 wire request（v0.5 候选 C；第 6 条 rr_io_thread →
    /// S0 `/<robot_id>/control/set_enabled`）。
    pub rr_forward_tx_set_enabled: Sender<RrForwardRequest>,
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
    pub idempotency: Arc<Mutex<IdempotencyCache>>,
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
        let cache_capacity = self.idempotency.lock().map_or(0, |c| c.capacity());
        f.debug_struct("GatewayHandlerState")
            .field("rr_timeout_grace_ms", &self.rr_timeout_grace_ms)
            .field("idempotency_capacity", &cache_capacity)
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
        // 6 个 forward channel 各留 1 buf；测试 drop rx 让 handler 看见
        // Disconnected。**RX 端不存**（让 sender 在第一次 send 时立刻
        // Disconnected），用于"只测 server bind / shutdown，不真调
        // handler"的 placeholder 场景。
        let (forward_tx_clear_estop, _rx0) = bounded(1);
        let (forward_tx_task_invoke, _rx1) = bounded(1);
        let (forward_tx_task_cancel, _rx2) = bounded(1);
        let (forward_tx_task_status, _rx3) = bounded(1);
        let (forward_tx_clear_error, _rx4) = bounded(1);
        let (forward_tx_set_enabled, _rx5) = bounded(1);
        let (estop_pub_tx, _rx6) = bounded(1);
        // shutdown_rx 始终 false（测试不模拟 shutdown）；保留 sender leak 让
        // receiver borrow 永远拿到 false。
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Box::leak(Box::new(shutdown_tx));
        let state = GatewayHandlerState {
            rr_forward_tx_clear_estop: forward_tx_clear_estop,
            rr_forward_tx_task_invoke: forward_tx_task_invoke,
            rr_forward_tx_task_cancel: forward_tx_task_cancel,
            rr_forward_tx_task_status: forward_tx_task_status,
            rr_forward_tx_clear_error: forward_tx_clear_error,
            rr_forward_tx_set_enabled: forward_tx_set_enabled,
            estop_pub_tx,
            robot_id: "for_test".to_string(),
            role_table: Arc::new(RoleTable::default()),
            idempotency: Arc::new(Mutex::new(IdempotencyCache::with_capacity(
                Duration::from_mins(1),
                64,
            ))),
            clock: Arc::new(robot_core::time::SystemClock),
            rr_timeout_grace_ms: 500,
            shutdown_rx,
            state_cache,
        };
        Self::new(state)
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

        // ─── Step 5: cache.decide — Replay / Conflict / Capacity 立刻短路 ───
        let insert_handle = {
            let mut cache = self
                .state
                .idempotency
                .lock()
                .expect("idempotency cache mutex poisoned");
            match cache.decide(Method::ClearEStop, &validated.idempotency_key, fingerprint) {
                Decision::FreshInsert(h) => h,
                Decision::Replay(CachedResponse::ClearEStop {
                    cleared_at_ns,
                    dry_run,
                }) => {
                    drop(cache);
                    tracing::info!(
                        request_id = %validated.request_id,
                        idempotency_key = %validated.idempotency_key,
                        cleared_at_ns,
                        dry_run,
                        "ClearEStop idempotency replay"
                    );
                    return Ok(tonic::Response::new(Ack {
                        request_id: validated.request_id,
                        cleared_at_ns,
                        dry_run,
                    }));
                }
                Decision::Replay(other) => {
                    // method namespace 已确保 cache key 不跨 method；同一
                    // method 命中 wrong variant 是 BUG（cache 内部状态不一
                    // 致）。fail-fast Internal 让运维一眼看见。
                    drop(cache);
                    tracing::error!(
                        request_id = %validated.request_id,
                        idempotency_key = %validated.idempotency_key,
                        cached_variant = ?other,
                        "BUG: ClearEStop method namespace 命中非 ClearEStop variant 的 cached response"
                    );
                    return Err(tonic::Status::internal(
                        "idempotency cache namespace inconsistency",
                    ));
                }
                Decision::Conflict {
                    cached_fingerprint: _,
                    incoming_fingerprint: _,
                } => {
                    drop(cache);
                    tracing::warn!(
                        request_id = %validated.request_id,
                        idempotency_key = %validated.idempotency_key,
                        "ClearEStop idempotency conflict: same key, different fingerprint"
                    );
                    return Err(tonic::Status::already_exists(format!(
                        "idempotency_key `{}` already used with a different request fingerprint",
                        validated.idempotency_key
                    )));
                }
                Decision::Capacity {
                    current_entries,
                    max_entries,
                } => {
                    drop(cache);
                    tracing::warn!(
                        request_id = %validated.request_id,
                        idempotency_key = %validated.idempotency_key,
                        current_entries,
                        max_entries,
                        "ClearEStop idempotency cache full"
                    );
                    return Err(tonic::Status::resource_exhausted(format!(
                        "idempotency cache full ({current_entries}/{max_entries}); retry after TTL expiry"
                    )));
                }
            }
        };

        // ─── Step 5.5: shutdown gate（reviewer A1-3 (2/3-c) 拍板）。
        //     Replay / Conflict / Capacity 路径已经在 Step 5 短路返；走到
        //     这里说明拿到 FreshInsert handle，正要 forward 新请求。如果
        //     gateway 已经在 graceful shutdown，**不**再启动新的 RR roundtrip
        //     —— 已 forward 的请求由 tonic serve_with_incoming_shutdown
        //     等其 in-flight 完成，未 forward 的 fresh 请求 fast-fail
        //     `Unavailable("gateway is shutting down; ...")`。drop
        //     insert_handle 让 cache 不留记录，下次同 key（gateway 重启
        //     后）retry 重新走 fresh。 ───
        if *self.state.shutdown_rx.borrow() {
            tracing::warn!(
                request_id = %validated.request_id,
                idempotency_key = %validated.idempotency_key,
                "ClearEStop rejected during shutdown (cache.decide=FreshInsert; not forwarding)"
            );
            drop(insert_handle);
            return Err(tonic::Status::unavailable(
                "gateway is shutting down; new requests rejected",
            ));
        }

        // ─── Step 6: 算 deadline_ns + stamp_ns（CLOCK_MONOTONIC_RAW；
        //     gateway 跟 daemon 同机器共享 clock domain） ───
        let now_ns = self.state.clock.now().0;
        let timeout_ns = validated.timeout_ms.saturating_mul(1_000_000);
        let wire_deadline_ns = now_ns.saturating_add(timeout_ns);
        let stamp_ns = now_ns;

        // ─── Step 7: 映射 Role → wire RpcRole（仅 audit） ───
        let wire_role = role_to_wire(role);

        // ─── Step 8: build wire ClearEStopRequest bytes ───
        let wire_bytes = build_clear_estop_request(
            &validated.request_id,
            &validated.idempotency_key,
            &validated.source,
            wire_role,
            &validated.confirmation,
            &validated.audit_reason,
            validated.dry_run,
            wire_deadline_ns,
            stamp_ns,
        );

        // ─── Step 9: forward via rr_io_thread channel + 持 oneshot_rx ───
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let forward = RrForwardRequest {
            wire_bytes,
            oneshot_tx,
            request_id: validated.request_id.clone(),
        };
        match self.state.rr_forward_tx_clear_estop.try_send(forward) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!(
                    request_id = %validated.request_id,
                    "ClearEStop forward channel full; replying ResourceExhausted"
                );
                drop(insert_handle);
                return Err(tonic::Status::resource_exhausted(
                    "gateway RR I/O forward channel full; retry shortly",
                ));
            }
            Err(TrySendError::Disconnected(_)) => {
                tracing::error!(
                    request_id = %validated.request_id,
                    "ClearEStop forward channel disconnected; rr_io_thread already stopped"
                );
                drop(insert_handle);
                return Err(tonic::Status::unavailable("gateway RR I/O thread stopped"));
            }
        }

        // ─── Step 10: 等 oneshot reply with timeout (timeout_ms + grace_ms) ───
        let total_timeout_ms = validated
            .timeout_ms
            .saturating_add(self.state.rr_timeout_grace_ms);
        let total_timeout = Duration::from_millis(total_timeout_ms);
        let reply_bytes = match tokio::time::timeout(total_timeout, oneshot_rx).await {
            Err(_elapsed) => {
                tracing::warn!(
                    request_id = %validated.request_id,
                    timeout_ms = validated.timeout_ms,
                    grace_ms = self.state.rr_timeout_grace_ms,
                    "ClearEStop gateway-side timeout; replying DeadlineExceeded"
                );
                drop(insert_handle);
                return Err(tonic::Status::deadline_exceeded(format!(
                    "no RR response within {total_timeout_ms} ms (timeout_ms + grace_ms)"
                )));
            }
            Ok(Err(_recv_err)) => {
                // oneshot_tx dropped without send = rr_io_thread shutdown /
                // send_err / pre-pass cleanup. handler 看到的是 RR I/O 不可
                // 用，**不**说 daemon 失败（reviewer (2/3-b2-1) 边界 2 拍板）。
                tracing::error!(
                    request_id = %validated.request_id,
                    "ClearEStop oneshot RecvError; gateway RR I/O unavailable"
                );
                drop(insert_handle);
                return Err(tonic::Status::unavailable("gateway RR I/O unavailable"));
            }
            Ok(Ok(b)) => b,
        };

        // ─── Step 11: decode wire ClearEStopResponse ───
        let view = match flatbuffers::root::<FbResp>(&reply_bytes) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(
                    request_id = %validated.request_id,
                    error = ?e,
                    "ClearEStop wire response decode failed; replying Internal"
                );
                drop(insert_handle);
                return Err(tonic::Status::internal(format!(
                    "decode wire ClearEStopResponse: {e}"
                )));
            }
        };
        let wire_status = view.status();
        let cleared_at_ns = view.cleared_at_ns();
        let dry_run = view.dry_run();
        let error_msg = view.error_msg().unwrap_or("");
        let resp_request_id = view.request_id().unwrap_or("");

        // ─── Step 11.5: 校验 wire response request_id 跟当前 request 一致
        //     （reviewer (2/3-b2-2) round 2 阻断 2；
        //     `crates/robot-msg/schemas/clear_estop.fbs:177` 明文 RPC 端用
        //     response.request_id correlate pending response）。daemon 端 wire
        //     bug / mock 错配 / 跨 in-flight 路由错位都会让 mismatch 出现，
        //     若不校验直接 commit cache 会把别的响应当成本请求成功缓存。 ───
        if resp_request_id != validated.request_id {
            tracing::error!(
                request_id = %validated.request_id,
                resp_request_id,
                ?wire_status,
                "ClearEStop wire response request_id mismatch; replying Internal (no cache commit)"
            );
            drop(insert_handle);
            return Err(tonic::Status::internal(format!(
                "wire ClearEStopResponse.request_id `{resp_request_id}` does not match \
                 outgoing request `{}`",
                validated.request_id
            )));
        }

        // ─── Step 12: map RpcStatus → tonic::Code 显式 match；Ok 路径 commit
        //     cache + 返 Ack；非 Ok drop InsertHandle（**不**缓存失败状态） ───
        let code = rpc_status_to_tonic_code(wire_status);
        if code == tonic::Code::Ok {
            // commit cached ack（下次同 key + 同 fingerprint → Replay）
            let mut cache = self
                .state
                .idempotency
                .lock()
                .expect("idempotency cache mutex poisoned");
            insert_handle.commit(
                &mut cache,
                CachedResponse::ClearEStop {
                    cleared_at_ns,
                    dry_run,
                },
            );
            drop(cache);
            tracing::info!(
                request_id = %validated.request_id,
                idempotency_key = %validated.idempotency_key,
                cleared_at_ns,
                dry_run,
                role = role.as_str(),
                source = %validated.source,
                audit_reason = %validated.audit_reason,
                "ClearEStop ok"
            );
            return Ok(tonic::Response::new(Ack {
                request_id: validated.request_id,
                cleared_at_ns,
                dry_run,
            }));
        }

        // 非 Ok：drop handle 让下次同 key retry 重新走 FreshInsert
        // （状态依赖型失败如 NotTorqueOffConfirmed 不应永久缓存）。
        drop(insert_handle);
        tracing::warn!(
            request_id = %validated.request_id,
            idempotency_key = %validated.idempotency_key,
            wire_status = ?wire_status,
            tonic_code = ?code,
            error_msg,
            "ClearEStop non-Ok wire status; dropping insert handle (no cache commit)"
        );
        Err(tonic::Status::new(
            code,
            format!("daemon returned {wire_status:?}: {error_msg}"),
        ))
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

        // ─── Step 5: cache.decide — Replay / Conflict / Capacity 立刻短路
        let insert_handle = {
            let mut cache = self
                .state
                .idempotency
                .lock()
                .expect("idempotency cache mutex poisoned");
            match cache.decide(Method::SetEnabled, &validated.idempotency_key, fingerprint) {
                Decision::FreshInsert(h) => h,
                Decision::Replay(CachedResponse::SetEnabled {
                    completed_at_ns,
                    dry_run,
                }) => {
                    drop(cache);
                    tracing::info!(
                        request_id = %validated.request_id,
                        idempotency_key = %validated.idempotency_key,
                        completed_at_ns,
                        dry_run,
                        "SetEnabled idempotency replay"
                    );
                    return Ok(tonic::Response::new(Ack {
                        request_id: validated.request_id,
                        cleared_at_ns: completed_at_ns,
                        dry_run,
                    }));
                }
                Decision::Replay(other) => {
                    drop(cache);
                    tracing::error!(
                        request_id = %validated.request_id,
                        idempotency_key = %validated.idempotency_key,
                        cached_variant = ?other,
                        "BUG: SetEnabled method namespace inconsistency"
                    );
                    return Err(tonic::Status::internal(
                        "idempotency cache namespace inconsistency",
                    ));
                }
                Decision::Conflict { .. } => {
                    drop(cache);
                    tracing::warn!(
                        request_id = %validated.request_id,
                        idempotency_key = %validated.idempotency_key,
                        "SetEnabled idempotency conflict"
                    );
                    return Err(tonic::Status::already_exists(format!(
                        "idempotency_key `{}` already used with a different request fingerprint",
                        validated.idempotency_key
                    )));
                }
                Decision::Capacity {
                    current_entries,
                    max_entries,
                } => {
                    drop(cache);
                    tracing::warn!(
                        request_id = %validated.request_id,
                        idempotency_key = %validated.idempotency_key,
                        current_entries,
                        max_entries,
                        "SetEnabled idempotency cache full"
                    );
                    return Err(tonic::Status::resource_exhausted(format!(
                        "idempotency cache full ({current_entries}/{max_entries})"
                    )));
                }
            }
        };

        // ─── Step 5.5: shutdown gate
        if *self.state.shutdown_rx.borrow() {
            tracing::warn!(
                request_id = %validated.request_id,
                "SetEnabled rejected during shutdown"
            );
            drop(insert_handle);
            return Err(tonic::Status::unavailable(
                "gateway is shutting down; new requests rejected",
            ));
        }

        // ─── Step 6: 算 deadline_ns + stamp_ns
        let now_ns = self.state.clock.now().0;
        let timeout_ns = validated.timeout_ms.saturating_mul(1_000_000);
        let wire_deadline_ns = now_ns.saturating_add(timeout_ns);
        let stamp_ns = now_ns;

        // ─── Step 7: 映射 Role → wire RpcRole（仅 audit）
        let wire_role = role_to_wire(role);

        // ─── Step 8: build wire SetEnabledRequest bytes
        let wire_bytes = build_set_enabled_request(
            &validated.request_id,
            &validated.idempotency_key,
            &validated.source,
            wire_role,
            validated.enabled,
            validated.dry_run,
            wire_deadline_ns,
            &validated.audit_reason,
            stamp_ns,
        );

        // ─── Step 9: forward via rr_io_thread channel + 持 oneshot_rx
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let forward = RrForwardRequest {
            wire_bytes,
            oneshot_tx,
            request_id: validated.request_id.clone(),
        };
        match self.state.rr_forward_tx_set_enabled.try_send(forward) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!(
                    request_id = %validated.request_id,
                    "SetEnabled forward channel full"
                );
                drop(insert_handle);
                return Err(tonic::Status::resource_exhausted(
                    "gateway RR I/O forward channel full; retry shortly",
                ));
            }
            Err(TrySendError::Disconnected(_)) => {
                tracing::error!(
                    request_id = %validated.request_id,
                    "SetEnabled forward channel disconnected"
                );
                drop(insert_handle);
                return Err(tonic::Status::unavailable("gateway RR I/O thread stopped"));
            }
        }

        // ─── Step 10: 等 oneshot reply with timeout (timeout_ms + grace_ms)
        let total_timeout_ms = validated
            .timeout_ms
            .saturating_add(self.state.rr_timeout_grace_ms);
        let total_timeout = Duration::from_millis(total_timeout_ms);
        let reply_bytes = match tokio::time::timeout(total_timeout, oneshot_rx).await {
            Err(_elapsed) => {
                tracing::warn!(
                    request_id = %validated.request_id,
                    grace_ms = self.state.rr_timeout_grace_ms,
                    "SetEnabled gateway-side timeout"
                );
                drop(insert_handle);
                return Err(tonic::Status::deadline_exceeded(format!(
                    "no RR response within {total_timeout_ms} ms"
                )));
            }
            Ok(Err(_recv_err)) => {
                tracing::error!(
                    request_id = %validated.request_id,
                    "SetEnabled oneshot RecvError"
                );
                drop(insert_handle);
                return Err(tonic::Status::unavailable("gateway RR I/O unavailable"));
            }
            Ok(Ok(b)) => b,
        };

        // ─── Step 11: decode wire SetEnabledResponse
        let view = match flatbuffers::root::<FbSetEnabledResp>(&reply_bytes) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(
                    request_id = %validated.request_id,
                    error = ?e,
                    "SetEnabled wire response decode failed"
                );
                drop(insert_handle);
                return Err(tonic::Status::internal(format!(
                    "decode wire SetEnabledResponse: {e}"
                )));
            }
        };
        let wire_status = view.status();
        let wire_domain = view.domain();
        let completed_at_ns = view.completed_at_ns();
        let error_msg = view.error_msg().unwrap_or("");
        let resp_request_id = view.request_id().unwrap_or("");

        // ─── Step 11.5: **PB12 防线** — 校验 wire response request_id 跟当前一致；
        //     mismatch → Internal + drop handle 不 commit cache（沿 ClearEStop）
        if resp_request_id != validated.request_id {
            tracing::error!(
                request_id = %validated.request_id,
                resp_request_id,
                ?wire_status,
                "SetEnabled wire response request_id mismatch; replying Internal (no cache commit)"
            );
            drop(insert_handle);
            return Err(tonic::Status::internal(format!(
                "wire SetEnabledResponse.request_id `{resp_request_id}` does not match \
                 outgoing request `{}`",
                validated.request_id
            )));
        }

        // ─── Step 12: map RpcStatus → tonic::Code（**复用** ClearEStop mapper）；
        //     Ok commit cache，non-Ok drop handle 不缓存。dry_run 透传
        //     request.dry_run（SetEnabledResponse 无 dry_run 字段）。
        let code = rpc_status_to_tonic_code(wire_status);
        if code == tonic::Code::Ok {
            let mut cache = self
                .state
                .idempotency
                .lock()
                .expect("idempotency cache mutex poisoned");
            insert_handle.commit(
                &mut cache,
                CachedResponse::SetEnabled {
                    completed_at_ns,
                    dry_run: validated.dry_run,
                },
            );
            drop(cache);
            tracing::info!(
                request_id = %validated.request_id,
                idempotency_key = %validated.idempotency_key,
                enabled = validated.enabled,
                dry_run = validated.dry_run,
                completed_at_ns,
                role = role.as_str(),
                source = %validated.source,
                audit_reason = %validated.audit_reason,
                "SetEnabled ok"
            );
            return Ok(tonic::Response::new(Ack {
                request_id: validated.request_id,
                cleared_at_ns: completed_at_ns,
                dry_run: validated.dry_run,
            }));
        }

        // 非 Ok：drop handle 让下次同 key retry 重新走 FreshInsert（状态依赖型
        // 失败如 GuardRejected / LatchBlocked / AdapterError 不应永久缓存）。
        drop(insert_handle);
        tracing::warn!(
            request_id = %validated.request_id,
            idempotency_key = %validated.idempotency_key,
            wire_status = ?wire_status,
            wire_domain = ?wire_domain,
            tonic_code = ?code,
            error_msg,
            "SetEnabled non-Ok wire status; dropping insert handle (no cache commit)"
        );
        Err(tonic::Status::new(
            code,
            format!("daemon returned {wire_status:?}: {error_msg}"),
        ))
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

        // ─── Step 5: cache.decide — Replay / Conflict / Capacity 立刻短路
        let insert_handle = {
            let mut cache = self
                .state
                .idempotency
                .lock()
                .expect("idempotency cache mutex poisoned");
            match cache.decide(Method::EStop, &validated.idempotency_key, fingerprint) {
                Decision::FreshInsert(h) => h,
                Decision::Replay(CachedResponse::EStop { published_at_ns }) => {
                    drop(cache);
                    tracing::info!(
                        request_id = %validated.request_id,
                        idempotency_key = %validated.idempotency_key,
                        published_at_ns,
                        "EStop idempotency replay (no re-publish)"
                    );
                    return Ok(tonic::Response::new(Ack {
                        request_id: validated.request_id,
                        cleared_at_ns: published_at_ns,
                        dry_run: false,
                    }));
                }
                Decision::Replay(other) => {
                    drop(cache);
                    tracing::error!(
                        request_id = %validated.request_id,
                        idempotency_key = %validated.idempotency_key,
                        cached_variant = ?other,
                        "BUG: EStop method namespace inconsistency"
                    );
                    return Err(tonic::Status::internal(
                        "idempotency cache namespace inconsistency",
                    ));
                }
                Decision::Conflict { .. } => {
                    drop(cache);
                    tracing::warn!(
                        request_id = %validated.request_id,
                        idempotency_key = %validated.idempotency_key,
                        "EStop idempotency conflict"
                    );
                    return Err(tonic::Status::already_exists(format!(
                        "idempotency_key `{}` already used with a different request fingerprint",
                        validated.idempotency_key
                    )));
                }
                Decision::Capacity {
                    current_entries,
                    max_entries,
                } => {
                    drop(cache);
                    tracing::warn!(
                        request_id = %validated.request_id,
                        idempotency_key = %validated.idempotency_key,
                        current_entries,
                        max_entries,
                        "EStop idempotency cache full"
                    );
                    return Err(tonic::Status::resource_exhausted(format!(
                        "idempotency cache full ({current_entries}/{max_entries})"
                    )));
                }
            }
        };

        // ─── Step 5.5: shutdown gate
        if *self.state.shutdown_rx.borrow() {
            tracing::warn!(
                request_id = %validated.request_id,
                "EStop rejected during shutdown"
            );
            drop(insert_handle);
            return Err(tonic::Status::unavailable(
                "gateway is shutting down; new requests rejected",
            ));
        }

        // ─── Step 6: stamp + build wire EStopEvent body（D3/D6：source="rpc"
        //     固定 + reason 透传〔空串合法〕+ **latched=false**——publish 时刻
        //     fsync 不可知，不在 wire 上说尚未发生的事实）
        let stamp_ns = self.state.clock.now().0;
        let body_bytes = build_estop_event(&validated.estop_reason, stamp_ns, "rpc", false);

        // ─── Step 7: forward 到 estop_pub_thread + 持 oneshot_rx（PD2 映射）
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let forward = EStopPubForward {
            body_bytes,
            stamp_ns,
            oneshot_tx,
            request_id: validated.request_id.clone(),
        };
        match self.state.estop_pub_tx.try_send(forward) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!(
                    request_id = %validated.request_id,
                    "EStop publish forward channel full"
                );
                drop(insert_handle);
                return Err(tonic::Status::resource_exhausted(
                    "gateway EStop publish forward channel full; retry shortly",
                ));
            }
            Err(TrySendError::Disconnected(_)) => {
                tracing::error!(
                    request_id = %validated.request_id,
                    "EStop publish forward channel disconnected"
                );
                drop(insert_handle);
                return Err(tonic::Status::unavailable(
                    "gateway EStop publish thread stopped",
                ));
            }
        }

        // ─── Step 8: 等 publish 结果（timeout_ms + grace；超时 →
        //     DeadlineExceeded〔gateway 侧；publish 路径无 wire deadline〕）
        let total_timeout_ms = validated
            .timeout_ms
            .saturating_add(self.state.rr_timeout_grace_ms);
        let total_timeout = Duration::from_millis(total_timeout_ms);
        let publish_result = match tokio::time::timeout(total_timeout, oneshot_rx).await {
            Err(_elapsed) => {
                tracing::warn!(
                    request_id = %validated.request_id,
                    "EStop gateway-side publish timeout"
                );
                drop(insert_handle);
                return Err(tonic::Status::deadline_exceeded(format!(
                    "no publish confirmation within {total_timeout_ms} ms"
                )));
            }
            Ok(Err(_recv_err)) => {
                tracing::error!(
                    request_id = %validated.request_id,
                    "EStop publish oneshot RecvError"
                );
                drop(insert_handle);
                return Err(tonic::Status::unavailable(
                    "gateway EStop publish thread unavailable",
                ));
            }
            Ok(Ok(r)) => r,
        };

        // ─── Step 9: publish 成败 → commit / Internal
        match publish_result {
            Ok(published_at_ns) => {
                let mut cache = self
                    .state
                    .idempotency
                    .lock()
                    .expect("idempotency cache mutex poisoned");
                insert_handle.commit(&mut cache, CachedResponse::EStop { published_at_ns });
                drop(cache);
                tracing::info!(
                    request_id = %validated.request_id,
                    idempotency_key = %validated.idempotency_key,
                    published_at_ns,
                    role = role.as_str(),
                    source = %validated.source,
                    estop_reason = %validated.estop_reason,
                    audit_reason = %validated.audit_reason,
                    "EStop published (Ack = published 弱语义；不证 S0 消费 / latch fsync)"
                );
                Ok(tonic::Response::new(Ack {
                    request_id: validated.request_id,
                    cleared_at_ns: published_at_ns,
                    dry_run: false,
                }))
            }
            Err(pub_err) => {
                drop(insert_handle);
                tracing::error!(
                    request_id = %validated.request_id,
                    error = %pub_err,
                    "EStop publish failed; replying Internal (no cache commit)"
                );
                Err(tonic::Status::internal(format!(
                    "EStopEvent publish failed: {pub_err}"
                )))
            }
        }
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
#[allow(clippy::too_many_lines)] // 6 个 rr_io_thread spawn + cache + handler + tonic 起 + cleanup；拆 helper 反而让阅读跳函数。
pub async fn run_gateway_with_rr_io(
    cfg: GatewayConfig,
    role_table: Arc<RoleTable>,
    clock: Arc<dyn RobotClock>,
    shutdown_rx: watch::Receiver<bool>,
    ready_tx: Option<tokio::sync::oneshot::Sender<Result<SocketAddr, GatewayReadyError>>>,
) -> Result<(), GatewayError> {
    // ─── 0) cfg.validate() **优先**（reviewer (2/3-b2-2) round 2 阻断 1）：
    //     无效配置（空 robot_id / 非 loopback bind / 0 TTL / 0 max_entries）
    //     必须在 spawn rr_io_thread 之前 fail-fast；不能用 iceoryx2 RR client
    //     创建路径触碰共享 shm，也不能把"配置错"误报成 RrIoSpawn。
    if let Err(e) = cfg.validate() {
        let ready_err = GatewayReadyError::Config(e.clone());
        if let Some(tx) = ready_tx {
            // 防 ready_rx 端永久阻塞 await。
            let _ = tx.send(Err(ready_err));
        }
        return Err(GatewayError::Config(e));
    }

    // ─── 1) 6 个 rr_io_thread spawn（B4 (2/3-b) 5 条 + v0.5 候选 C
    //     set_enabled 第 6 条）。每个 RR topic 一条独立线程 + 独立 typed
    //     Iceoryx2RrClient + 独立 forward channel。任一 spawn 失败 → 把先前
    //     已 spawn 的 stop/join 后回滚 → 返 RrIoSpawn 错。tonic 还没起，
    //     不需要 graceful 等。 ───
    let topics = RrTopics::for_robot(&cfg.robot_id);
    let mut spawned = SpawnedRrIo::default();

    let rr_clear_estop = match spawn_rr_io_thread(
        &RrIoThreadConfig {
            topic: topics.clear_estop.clone(),
            poll_interval_ns: 500_000,
        },
        client_factory_for::<markers::ClearEStopRequest, markers::ClearEStopResponse>(
            topics.clear_estop.clone(),
        ),
        spawned.clear_estop_rx(),
    ) {
        Ok(h) => h,
        Err(e) => {
            let ready_err = GatewayReadyError::rr_io_spawn_from_io(&e);
            if let Some(tx) = ready_tx {
                let _ = tx.send(Err(ready_err));
            }
            spawned.stop_and_join_all();
            return Err(GatewayError::RrIoSpawn(e));
        }
    };
    spawned.handles.push(rr_clear_estop);

    let rr_task_invoke = match spawn_rr_io_thread(
        &RrIoThreadConfig {
            topic: topics.task_invoke.clone(),
            poll_interval_ns: 500_000,
        },
        client_factory_for::<markers::InvokeTaskRequest, markers::InvokeTaskResponse>(
            topics.task_invoke.clone(),
        ),
        spawned.task_invoke_rx(),
    ) {
        Ok(h) => h,
        Err(e) => {
            let ready_err = GatewayReadyError::rr_io_spawn_from_io(&e);
            if let Some(tx) = ready_tx {
                let _ = tx.send(Err(ready_err));
            }
            spawned.stop_and_join_all();
            return Err(GatewayError::RrIoSpawn(e));
        }
    };
    spawned.handles.push(rr_task_invoke);

    let rr_task_cancel = match spawn_rr_io_thread(
        &RrIoThreadConfig {
            topic: topics.task_cancel.clone(),
            poll_interval_ns: 500_000,
        },
        client_factory_for::<markers::CancelTaskRequest, markers::CancelTaskResponse>(
            topics.task_cancel.clone(),
        ),
        spawned.task_cancel_rx(),
    ) {
        Ok(h) => h,
        Err(e) => {
            let ready_err = GatewayReadyError::rr_io_spawn_from_io(&e);
            if let Some(tx) = ready_tx {
                let _ = tx.send(Err(ready_err));
            }
            spawned.stop_and_join_all();
            return Err(GatewayError::RrIoSpawn(e));
        }
    };
    spawned.handles.push(rr_task_cancel);

    let rr_task_status = match spawn_rr_io_thread(
        &RrIoThreadConfig {
            topic: topics.task_status.clone(),
            poll_interval_ns: 500_000,
        },
        client_factory_for::<markers::GetTaskStatusRequest, markers::GetTaskStatusResponse>(
            topics.task_status.clone(),
        ),
        spawned.task_status_rx(),
    ) {
        Ok(h) => h,
        Err(e) => {
            let ready_err = GatewayReadyError::rr_io_spawn_from_io(&e);
            if let Some(tx) = ready_tx {
                let _ = tx.send(Err(ready_err));
            }
            spawned.stop_and_join_all();
            return Err(GatewayError::RrIoSpawn(e));
        }
    };
    spawned.handles.push(rr_task_status);

    let rr_clear_error = match spawn_rr_io_thread(
        &RrIoThreadConfig {
            topic: topics.clear_error.clone(),
            poll_interval_ns: 500_000,
        },
        client_factory_for::<markers::ClearErrorRequest, markers::ClearErrorResponse>(
            topics.clear_error.clone(),
        ),
        spawned.clear_error_rx(),
    ) {
        Ok(h) => h,
        Err(e) => {
            let ready_err = GatewayReadyError::rr_io_spawn_from_io(&e);
            if let Some(tx) = ready_tx {
                let _ = tx.send(Err(ready_err));
            }
            spawned.stop_and_join_all();
            return Err(GatewayError::RrIoSpawn(e));
        }
    };
    spawned.handles.push(rr_clear_error);

    // **v0.5 候选 C**：第 6 条 rr_io_thread —— set_enabled（gateway → S0
    // `/<robot_id>/control/set_enabled` RR server）。
    let rr_set_enabled = match spawn_rr_io_thread(
        &RrIoThreadConfig {
            topic: topics.set_enabled.clone(),
            poll_interval_ns: 500_000,
        },
        client_factory_for::<markers::SetEnabledRequest, markers::SetEnabledResponse>(
            topics.set_enabled.clone(),
        ),
        spawned.set_enabled_rx(),
    ) {
        Ok(h) => h,
        Err(e) => {
            let ready_err = GatewayReadyError::rr_io_spawn_from_io(&e);
            if let Some(tx) = ready_tx {
                let _ = tx.send(Err(ready_err));
            }
            spawned.stop_and_join_all();
            return Err(GatewayError::RrIoSpawn(e));
        }
    };
    spawned.handles.push(rr_set_enabled);

    // **v0.5 候选 D（PB1）**：estop_pub_thread 在 "rr_io_threads started"
    // marker **之前** spawn——spawn 同步 ready 握手 → marker 打出时 estop
    // publisher 已 attach（e2e 等 marker 即可保证 publisher ready；AC-7 顺序
    // 判定依赖此序）。publisher open_or_create **不依赖** S0 subscriber 存在。
    let (estop_pub_tx, estop_pub_rx) = bounded::<EStopPubForward>(64);
    let estop_topic = format!("/{}/events/estop", cfg.robot_id);
    let estop_topic_for_factory = estop_topic.clone();
    let estop_factory = move || {
        let node = NodeBuilder::new()
            .create::<IxService>()
            .expect("iceoryx2 NodeBuilder::create (estop_pub)");
        let publisher = robot_transport::iceoryx2::publisher::<markers::EStopEvent>(
            &node,
            &estop_topic_for_factory,
            robot_transport::iceoryx2::TopicClass::SafetyEvent,
        )
        .expect("open estop publisher");
        Box::leak(Box::new(node));
        publisher
    };
    let estop_pub_handle: EStopPubThreadHandle = match spawn_estop_pub_thread(
        &EStopPubThreadConfig {
            topic: estop_topic.clone(),
            poll_interval_ns: 500_000,
        },
        Arc::clone(&clock),
        estop_factory,
        estop_pub_rx,
    ) {
        Ok(h) => h,
        Err(e) => {
            let ready_err = GatewayReadyError::rr_io_spawn_from_io(&e);
            if let Some(tx) = ready_tx {
                let _ = tx.send(Err(ready_err));
            }
            spawned.stop_and_join_all();
            return Err(GatewayError::RrIoSpawn(e));
        }
    };

    tracing::info!(
        topic_clear_estop = %topics.clear_estop,
        topic_task_invoke = %topics.task_invoke,
        topic_task_cancel = %topics.task_cancel,
        topic_task_status = %topics.task_status,
        topic_clear_error = %topics.clear_error,
        topic_set_enabled = %topics.set_enabled,
        topic_estop_pub = %estop_topic,
        "rr_io_threads started (6 RR I/O threads + estop_pub; B4 + v0.5 候选 C/D)"
    );

    // ─── 2) idempotency cache（6 个 method 共用一个 cache 实例 + namespace
    //     key；详 idempotency::Method）───
    let cache = Arc::new(Mutex::new(IdempotencyCache::with_capacity(
        Duration::from_secs(cfg.idempotency_ttl_secs),
        cfg.idempotency_max_entries,
    )));

    // ─── 2.5) **v0.5 候选 B (2/4)**：常驻 per-hardware S0 state 订阅池 + cache。
    //     在 6 个 rr_io_thread + estop_pub_thread 起好之后、tonic server 起之
    //     前建；spawn 失败回滚已起 rr_io_thread + estop_pub_thread →
    //     fail-fast（沿 RrIoSpawn 先例；候选 D reviewer (1/3) 阻断：回滚必须
    //     含 estop_pub，否则半失败路径遗留 publisher 线程）。空
    //     state_hardware_ids → 空池（D-P1，合法）。 ───
    let state_cache = Arc::new(StateCache::new(&cfg.state_hardware_ids));
    let state_pool = match StatePool::spawn(&cfg.robot_id, &cfg.state_hardware_ids, &state_cache) {
        Ok(p) => p,
        Err(e) => {
            let ready_err = GatewayReadyError::state_pool_spawn_from_io(&e);
            if let Some(tx) = ready_tx {
                let _ = tx.send(Err(ready_err));
            }
            spawned.stop_and_join_all();
            estop_pub_handle.request_stop();
            if let Err(panic) = estop_pub_handle.join() {
                tracing::error!(?panic, "estop_pub_thread panicked at rollback join");
            }
            return Err(GatewayError::StatePoolSpawn(e));
        }
    };

    // ─── 3) handler（**A1-3 (2/3-c)**：handler clone 一份 shutdown_rx，
    //     同 watch::channel 的 sender 也喂给 tonic shutdown_signal future
    //     （由 run_gateway_with_ready 内部消费）；两路独立 receiver borrow
    //     不冲突。 ───
    // v0.5 候选 E 段(8)：probe metrics（watchdog observer + freeze 槽位）与
    // test-only freeze control 用 clock 先于 handler 构造取 clone。
    let probe_metrics = Arc::new(TonicProbeMetrics::default());
    #[cfg(feature = "test-freeze-hook")]
    let clock_for_freeze: Arc<dyn RobotClock> = Arc::clone(&clock);

    let handler_shutdown_rx = shutdown_rx.clone();
    let handler = Arc::new(GatewayHandler::new(GatewayHandlerState {
        rr_forward_tx_clear_estop: spawned.forward_txs.clear_estop.clone(),
        rr_forward_tx_task_invoke: spawned.forward_txs.task_invoke.clone(),
        rr_forward_tx_task_cancel: spawned.forward_txs.task_cancel.clone(),
        rr_forward_tx_task_status: spawned.forward_txs.task_status.clone(),
        rr_forward_tx_clear_error: spawned.forward_txs.clear_error.clone(),
        rr_forward_tx_set_enabled: spawned.forward_txs.set_enabled.clone(),
        estop_pub_tx: estop_pub_tx.clone(),
        robot_id: cfg.robot_id.clone(),
        role_table,
        idempotency: cache,
        clock,
        rr_timeout_grace_ms: cfg.rr_timeout_grace_ms,
        shutdown_rx: handler_shutdown_rx,
        state_cache: Arc::clone(&state_cache),
    }));

    // ─── 4) v0.5 候选 E 段(8)（Spec D12 + R3-B2 + RB4 + R6-5）：liveness
    //     observer 集 = 6 rr_io + estop_pub + state pool **逐 hardware 线程各一**
    //     （禁聚合求和）+ tonic probe；test-only freeze control 在全部 worker
    //     存在后起（不进 observer）；tonic server 由 serve_with_liveness 编排
    //     （独立 task → probe 首成 → READY → watchdog）───
    debug_assert_eq!(spawned.handles.len(), RR_IO_OBSERVER_LABELS.len());
    let mut observers: Vec<LivenessObserver> =
        Vec::with_capacity(RR_IO_OBSERVER_LABELS.len() + 2 + state_pool.thread_metrics().len());
    for (h, label) in spawned.handles.iter().zip(RR_IO_OBSERVER_LABELS) {
        let m = h.metrics();
        observers.push(LivenessObserver::from_fn(label, move || {
            m.loop_tick_count.load(Ordering::Relaxed)
        }));
    }
    {
        let m = estop_pub_handle.metrics();
        observers.push(LivenessObserver::from_fn("estop_pub", move || {
            m.loop_tick_count.load(Ordering::Relaxed)
        }));
    }
    for m in state_pool.thread_metrics() {
        // label 带 hardware_id（R6-5）；LivenessObserver.label 为 &'static str，
        // 进程生命周期内一次性 leak（每 hardware 一条，开销可忽略）。
        let label: &'static str =
            Box::leak(format!("state_pool:{}", m.hardware_id).into_boxed_str());
        let m = Arc::clone(m);
        observers.push(LivenessObserver::from_fn(label, move || {
            m.loop_tick_count.load(Ordering::Relaxed)
        }));
    }
    {
        let m = Arc::clone(&probe_metrics);
        observers.push(LivenessObserver::from_fn("tonic_probe", move || {
            m.success_count.load(Ordering::Acquire)
        }));
    }

    #[cfg(feature = "test-freeze-hook")]
    let freeze_control = {
        let ctx = crate::freeze_control::FreezeControlContext {
            robot_id: cfg.robot_id.clone(),
            clock: clock_for_freeze,
            rr_io: spawned
                .handles
                .iter()
                .map(RrIoThreadHandle::metrics)
                .collect(),
            estop_pub: estop_pub_handle.metrics(),
            state_pool: state_pool.thread_metrics().to_vec(),
            tonic_probe: Arc::clone(&probe_metrics),
        };
        match crate::freeze_control::spawn_freeze_control_thread(ctx) {
            Ok(h) => {
                tracing::info!("gw freeze_control thread spawned (test-freeze-hook feature)");
                Some(h)
            }
            Err(e) => {
                tracing::error!(error = ?e, "gw freeze_control thread spawn failed; control inactive");
                None
            }
        }
    };

    let serve_result = serve_with_liveness(
        cfg,
        handler,
        shutdown_rx,
        ready_tx,
        observers,
        probe_metrics,
    )
    .await;

    #[cfg(feature = "test-freeze-hook")]
    if let Some(h) = freeze_control {
        h.request_stop();
        if let Err(panic) = h.join() {
            tracing::error!(?panic, "gw freeze_control thread panicked at join");
        }
    }

    // ─── 5) **关键边界**：tonic server 退出**之后**才停 rr_io_thread +
    //     estop_pub_thread + state_pool（沿 B4 shutdown 顺序：in-flight
    //     handler 跑完 → 停后台线程）。
    spawned.stop_and_join_all();
    estop_pub_handle.request_stop();
    if let Err(panic) = estop_pub_handle.join() {
        tracing::error!(?panic, "estop_pub_thread panicked at join");
    }
    state_pool.stop();

    serve_result
}

/// **v0.5 候选 E 段(8)** 编排（Spec R3-B2 并发拓扑 + RB4 + D12 第 3/4 条）：
///
/// 1. `run_gateway_with_ready` 以独立 `tokio::spawn` task 跑（`serve_with_
///    incoming_shutdown(...).await` 阻塞到停机，不能线性 "serve → probe"）；
///    bind-ready 经内部 oneshot 接收后**原样转发**外部 `ready_tx`（既有测试
///    bind-ready 语义 0 改；systemd-ready 为独立通路）——[`await_bind_ready`]。
/// 2. probe task：500 ms loopback unary，首成上限 10 s——超时 = 触发内部
///    serve-shutdown + join server → [`GatewayError::ReadyProbeTimeout`]；
///    server 在 READY 前自行结束 → 其错误按既有码退出——[`ready_gate`]。
/// 3. 首成 → `READY=1` → `spawn_watchdog(observers, "gw_systemd_wd")`。
/// 4. 运行期：外部 `shutdown_rx` 触发 → `STOPPING=1` → 停 watchdog / probe →
///    内部 serve-shutdown → join server；server READY 后自行结束（错误）→
///    同序收尾后按其错误返回。
///
/// 所有权：本 fn 持 server `JoinHandle` + probe handle + watchdog handle +
/// 内部 `serve_shutdown_tx`；外部 `shutdown_rx` 只读转发。`JoinHandle` 完成后
/// 不得再 poll（tokio panic）——各 helper 以 `server_done` 显式区分。
async fn serve_with_liveness(
    cfg: GatewayConfig,
    handler: Arc<GatewayHandler>,
    mut shutdown_rx: watch::Receiver<bool>,
    ready_tx: Option<tokio::sync::oneshot::Sender<Result<SocketAddr, GatewayReadyError>>>,
    observers: Vec<LivenessObserver>,
    probe_metrics: Arc<TonicProbeMetrics>,
) -> Result<(), GatewayError> {
    let (serve_shutdown_tx, serve_shutdown_rx) = watch::channel(false);
    let (bind_tx, bind_rx) = tokio::sync::oneshot::channel();
    let mut server = tokio::spawn(run_gateway_with_ready(
        cfg,
        handler,
        serve_shutdown_rx.clone(),
        Some(bind_tx),
    ));

    // 1) bind-ready（失败路径已在 helper 内 join server）。
    let actual_addr = await_bind_ready(bind_rx, &mut server, ready_tx).await?;

    // 2) probe 首成 ≤ FIRST_SUCCESS_TIMEOUT（失败路径 helper 内收 probe / server）。
    let (first_tx, first_rx) = tokio::sync::oneshot::channel::<()>();
    let probe = spawn_tonic_probe(
        actual_addr,
        Arc::clone(&probe_metrics),
        serve_shutdown_rx,
        first_tx,
    );
    let probe = ready_gate(
        first_rx,
        &mut server,
        probe,
        &serve_shutdown_tx,
        &probe_metrics,
        actual_addr,
    )
    .await?;

    // 3) READY=1 → watchdog（enforce 自 grace 周期后开始）。
    let observer_count = observers.len();
    notify_ready();
    let watchdog = spawn_watchdog(observers, WatchdogConfig::new("gw_systemd_wd"));
    tracing::info!(
        %actual_addr,
        watchdog_active = watchdog.is_some(),
        observers = observer_count,
        "robot_rpc_gateway systemd READY=1 sent (no-op when NOTIFY_SOCKET unset)"
    );

    // 4) 运行期：外部 shutdown 或 server 自行结束。
    let outcome = tokio::select! {
        r = &mut server => {
            tracing::error!("robot_rpc_gateway: tonic server task ended without external shutdown");
            ServeOutcome::ServerEnded(r)
        }
        () = wait_shutdown(&mut shutdown_rx) => ServeOutcome::Shutdown,
    };

    // D12 第 4 条：先 STOPPING=1，再停 watchdog ping / probe，再 server graceful。
    notify_stopping();
    if let Some(wd) = watchdog {
        // WatchdogHandle::stop 会 join std 线程（分片 sleep ≤ 50 ms 响应）；放
        // spawn_blocking 不占 runtime worker。
        if let Err(e) = tokio::task::spawn_blocking(move || wd.stop()).await {
            tracing::error!(error = ?e, "robot_rpc_gateway: watchdog stop join error");
        }
    }
    let _ = serve_shutdown_tx.send(true);
    probe.join().await;
    match outcome {
        ServeOutcome::Shutdown => join_server(&mut server).await,
        ServeOutcome::ServerEnded(r) => Err(server_result_to_error(
            r,
            "server task returned Ok without external shutdown",
        )),
    }
}

/// 段(8) 编排阶段 1：等 server task 的 bind-ready 回执并**原样转发**外部
/// `ready_tx`；失败 → join server（其结果为权威错误）→ `Err`。
async fn await_bind_ready(
    bind_rx: tokio::sync::oneshot::Receiver<Result<SocketAddr, GatewayReadyError>>,
    server: &mut tokio::task::JoinHandle<Result<(), GatewayError>>,
    ready_tx: Option<tokio::sync::oneshot::Sender<Result<SocketAddr, GatewayReadyError>>>,
) -> Result<SocketAddr, GatewayError> {
    match bind_rx.await {
        Ok(Ok(addr)) => {
            if let Some(tx) = ready_tx {
                let _ = tx.send(Ok(addr));
            }
            Ok(addr)
        }
        Ok(Err(ready_err)) => {
            if let Some(tx) = ready_tx {
                let _ = tx.send(Err(ready_err.clone()));
            }
            Err(join_server(server)
                .await
                .err()
                .unwrap_or_else(|| ready_err.into()))
        }
        Err(_dropped) => {
            let err = join_server(server).await.err().unwrap_or_else(|| {
                GatewayError::ServerTask("server task exited before bind-ready".into())
            });
            if let Some(tx) = ready_tx {
                let _ = tx.send(Err(GatewayReadyError::ServerTask {
                    message: err.to_string(),
                }));
            }
            Err(err)
        }
    }
}

/// 段(8) 编排阶段 2：等 probe 首成（READY 门槛）。三路失败（probe task 早退 /
/// 超时 / server 早退）均在本 fn 内收尾：内部 serve-shutdown → join probe →
/// join server（server 已自行结束的分支不再 poll）。成功返还 `probe`。
async fn ready_gate(
    first_rx: tokio::sync::oneshot::Receiver<()>,
    server: &mut tokio::task::JoinHandle<Result<(), GatewayError>>,
    probe: crate::systemd_probe::ProbeHandle,
    serve_shutdown_tx: &watch::Sender<bool>,
    probe_metrics: &TonicProbeMetrics,
    actual_addr: SocketAddr,
) -> Result<crate::systemd_probe::ProbeHandle, GatewayError> {
    let ready_deadline = tokio::time::sleep(FIRST_SUCCESS_TIMEOUT);
    tokio::pin!(ready_deadline);
    tokio::pin!(first_rx);
    let (err, server_done) = tokio::select! {
        first = &mut first_rx => {
            if first.is_ok() {
                return Ok(probe);
            }
            (
                GatewayError::ServerTask("tonic probe task exited before first success".into()),
                false,
            )
        }
        () = &mut ready_deadline => {
            let waited_ms = u64::try_from(FIRST_SUCCESS_TIMEOUT.as_millis()).unwrap_or(u64::MAX);
            tracing::error!(
                %actual_addr,
                waited_ms,
                attempts = probe_metrics.attempts.load(Ordering::Relaxed),
                failures = probe_metrics.failure_count.load(Ordering::Relaxed),
                "robot_rpc_gateway: loopback probe never succeeded; READY withheld → startup failure"
            );
            (GatewayError::ReadyProbeTimeout { waited_ms }, false)
        }
        r = &mut *server => (server_result_to_error(r, "server task exited before READY"), true),
    };
    let _ = serve_shutdown_tx.send(true);
    probe.join().await;
    if !server_done {
        let _ = join_server(server).await;
    }
    Err(err)
}

/// [`serve_with_liveness`] 运行期两种退出路径。
enum ServeOutcome {
    Shutdown,
    ServerEnded(Result<Result<(), GatewayError>, tokio::task::JoinError>),
}

/// 等外部 shutdown watch 置位（sender drop 视同 shutdown）。
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

/// server task 结果 → 错误（`Ok(Ok(()))` 在非 shutdown 语境下亦是异常）。
fn server_result_to_error(
    r: Result<Result<(), GatewayError>, tokio::task::JoinError>,
    ok_is_abnormal: &str,
) -> GatewayError {
    match r {
        Ok(Ok(())) => GatewayError::ServerTask(ok_is_abnormal.into()),
        Ok(Err(e)) => e,
        Err(join) => GatewayError::ServerTask(format!("server task join: {join}")),
    }
}

/// join server task（**只可在其尚未完成时调**）：`JoinError` →
/// [`GatewayError::ServerTask`]。
async fn join_server(
    server: &mut tokio::task::JoinHandle<Result<(), GatewayError>>,
) -> Result<(), GatewayError> {
    match server.await {
        Ok(r) => r,
        Err(join) => Err(GatewayError::ServerTask(format!(
            "server task join: {join}"
        ))),
    }
}

/// 6 个 RR topic 串汇总（per-robot 派生；v0.5 候选 C +set_enabled）。
struct RrTopics {
    clear_estop: String,
    task_invoke: String,
    task_cancel: String,
    task_status: String,
    clear_error: String,
    set_enabled: String,
}

impl RrTopics {
    fn for_robot(robot_id: &str) -> Self {
        Self {
            clear_estop: format!("/{robot_id}/control/clear_estop"),
            task_invoke: format!("/{robot_id}/control/task_invoke"),
            task_cancel: format!("/{robot_id}/control/task_cancel"),
            task_status: format!("/{robot_id}/control/task_status"),
            clear_error: format!("/{robot_id}/control/clear_error"),
            set_enabled: format!("/{robot_id}/control/set_enabled"),
        }
    }
}

/// 6 个 RR topic 各自的 forward channel sender pair。给 handler state 用。
struct ForwardTxs {
    clear_estop: Sender<RrForwardRequest>,
    task_invoke: Sender<RrForwardRequest>,
    task_cancel: Sender<RrForwardRequest>,
    task_status: Sender<RrForwardRequest>,
    clear_error: Sender<RrForwardRequest>,
    set_enabled: Sender<RrForwardRequest>,
}

/// rr_io_thread spawn 进度跟踪 + 错误回滚 + shutdown 时一键 stop+join。
///
/// 6 个 forward channel 一次性构造（forward_tx 给 handler，forward_rx
/// 给对应 spawn fn）；handles 增量 push（任一 spawn 失败时 stop_and_join_all
/// 把已 spawn 的回滚清掉）。
struct SpawnedRrIo {
    forward_txs: ForwardTxs,
    /// 6 个 forward_rx 分别 take 进 spawn fn 之前临时存这里。`take` 后
    /// `Option::None` 占位；spawn 失败的回滚路径不需要 forward_rx。
    forward_rxs: ForwardRxs,
    handles: Vec<RrIoThreadHandle>,
}

#[allow(clippy::struct_field_names)]
struct ForwardRxs {
    clear_estop: Option<Receiver<RrForwardRequest>>,
    task_invoke: Option<Receiver<RrForwardRequest>>,
    task_cancel: Option<Receiver<RrForwardRequest>>,
    task_status: Option<Receiver<RrForwardRequest>>,
    clear_error: Option<Receiver<RrForwardRequest>>,
    set_enabled: Option<Receiver<RrForwardRequest>>,
}

impl Default for SpawnedRrIo {
    fn default() -> Self {
        let (tx0, rx0) = bounded::<RrForwardRequest>(64);
        let (tx1, rx1) = bounded::<RrForwardRequest>(64);
        let (tx2, rx2) = bounded::<RrForwardRequest>(64);
        let (tx3, rx3) = bounded::<RrForwardRequest>(64);
        let (tx4, rx4) = bounded::<RrForwardRequest>(64);
        let (tx5, rx5) = bounded::<RrForwardRequest>(64);
        Self {
            forward_txs: ForwardTxs {
                clear_estop: tx0,
                task_invoke: tx1,
                task_cancel: tx2,
                task_status: tx3,
                clear_error: tx4,
                set_enabled: tx5,
            },
            forward_rxs: ForwardRxs {
                clear_estop: Some(rx0),
                task_invoke: Some(rx1),
                task_cancel: Some(rx2),
                task_status: Some(rx3),
                clear_error: Some(rx4),
                set_enabled: Some(rx5),
            },
            handles: Vec::new(),
        }
    }
}

impl SpawnedRrIo {
    fn clear_estop_rx(&mut self) -> Receiver<RrForwardRequest> {
        self.forward_rxs
            .clear_estop
            .take()
            .expect("clear_estop forward_rx already taken")
    }
    fn task_invoke_rx(&mut self) -> Receiver<RrForwardRequest> {
        self.forward_rxs
            .task_invoke
            .take()
            .expect("task_invoke forward_rx already taken")
    }
    fn task_cancel_rx(&mut self) -> Receiver<RrForwardRequest> {
        self.forward_rxs
            .task_cancel
            .take()
            .expect("task_cancel forward_rx already taken")
    }
    fn task_status_rx(&mut self) -> Receiver<RrForwardRequest> {
        self.forward_rxs
            .task_status
            .take()
            .expect("task_status forward_rx already taken")
    }
    fn clear_error_rx(&mut self) -> Receiver<RrForwardRequest> {
        self.forward_rxs
            .clear_error
            .take()
            .expect("clear_error forward_rx already taken")
    }
    fn set_enabled_rx(&mut self) -> Receiver<RrForwardRequest> {
        self.forward_rxs
            .set_enabled
            .take()
            .expect("set_enabled forward_rx already taken")
    }

    /// 给所有已 spawn 的 rr_io_thread 发 stop + join。回滚 / shutdown 共用。
    /// 任一 join panic 仅 log，不中断剩余 join（避免 thread leak）。
    fn stop_and_join_all(self) {
        for h in &self.handles {
            h.request_stop();
        }
        for h in self.handles {
            if let Err(panic) = h.join() {
                tracing::error!(?panic, "rr_io_thread panicked at join");
            }
        }
    }
}

/// 构造一个 typed `client_factory` closure（move topic 进闭包；闭包内构
/// 造独立 iceoryx2 Node + RR client + leak Node 让 client 活到进程退出）。
fn client_factory_for<Req, Resp>(
    topic: String,
) -> impl FnOnce() -> Iceoryx2RrClient<Req, Resp> + Send + 'static
where
    Req: robot_msg::markers::BodyMarker,
    Resp: robot_msg::markers::BodyMarker,
{
    move || {
        let node = NodeBuilder::new()
            .create::<IxService>()
            .expect("iceoryx2 NodeBuilder::create");
        let client = rr_client::<Req, Resp>(&node, &topic).expect("rr_client create");
        // node 持 service shm fd；leak 让 client 活到 daemon 进程退出。
        Box::leak(Box::new(node));
        client
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
    use robot_msg::build_helpers::{build_clear_estop_response, build_set_enabled_response};
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
    fn make_handler_with_state(
        cache_max_entries: usize,
    ) -> (GatewayHandler, CrossbeamRecv<RrForwardRequest>) {
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
        CrossbeamRecv<RrForwardRequest>,
    ) {
        let role_table = Arc::new(load_role_table(TEST_RPC_ROLES_TOML).expect("role table parse"));
        let cache = Arc::new(Mutex::new(IdempotencyCache::with_capacity(
            Duration::from_mins(1),
            cache_max_entries,
        )));
        let (forward_tx, forward_rx) = bounded::<RrForwardRequest>(64);
        // 4 个 task forward channel 在 ClearEStop 单测里不参与；构造 (tx, rx)
        // pair 后立刻 leak rx 让 sender 永久"连着"（防 Disconnected 误触发
        // 其它路径），测试 ClearEStop 路径不被错误打扰。
        let (task_invoke_tx, task_invoke_rx) = bounded::<RrForwardRequest>(1);
        let (task_cancel_tx, task_cancel_rx) = bounded::<RrForwardRequest>(1);
        let (task_status_tx, task_status_rx) = bounded::<RrForwardRequest>(1);
        let (clear_error_tx, clear_error_rx) = bounded::<RrForwardRequest>(1);
        let (set_enabled_tx, set_enabled_rx) = bounded::<RrForwardRequest>(1);
        let (estop_pub_tx, estop_pub_rx) = bounded::<EStopPubForward>(1);
        Box::leak(Box::new(task_invoke_rx));
        Box::leak(Box::new(task_cancel_rx));
        Box::leak(Box::new(task_status_rx));
        Box::leak(Box::new(clear_error_rx));
        Box::leak(Box::new(set_enabled_rx));
        Box::leak(Box::new(estop_pub_rx));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let state = GatewayHandlerState {
            rr_forward_tx_clear_estop: forward_tx,
            rr_forward_tx_task_invoke: task_invoke_tx,
            rr_forward_tx_task_cancel: task_cancel_tx,
            rr_forward_tx_task_status: task_status_tx,
            rr_forward_tx_clear_error: clear_error_tx,
            rr_forward_tx_set_enabled: set_enabled_tx,
            estop_pub_tx,
            robot_id: "test_robot".to_string(),
            role_table,
            idempotency: cache,
            clock: Arc::new(SystemClock),
            rr_timeout_grace_ms: 500,
            shutdown_rx,
            state_cache: Arc::new(StateCache::new(&[])),
        };
        (GatewayHandler::new(state), shutdown_tx, forward_rx)
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
        forward_rx: &CrossbeamRecv<RrForwardRequest>,
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
            build_clear_estop_response(&request_id, status, error_msg, cleared_at_ns, dry_run);
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
        let cache = Arc::clone(&handler.state().idempotency);

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
        let cache = Arc::clone(&handler.state().idempotency);

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
        let cache = Arc::clone(&handler.state().idempotency);
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
            let resp = build_clear_estop_response(
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
                .send(build_clear_estop_response(
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
        let cache = Arc::clone(&handler.state().idempotency);

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
            let resp = build_clear_estop_response("wrong", RpcStatus::Ok, "", 12_345, false);
            req.oneshot_tx
                .send(resp)
                .expect("oneshot recv side dropped");
            expected
        });

        let err = handler
            .clear_e_stop(tonic::Request::new(cmd))
            .await
            .expect_err("mismatched response request_id should fail");
        assert_eq!(err.code(), tonic::Code::Internal);
        assert!(
            err.message().contains("does not match"),
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
            0,
            "request_id mismatch 路径**不**应缓存；下次 retry 必须能重新走 fresh"
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
            let result = run_gateway_with_rr_io(cfg, role_table, clock, rx, Some(ready_tx)).await;
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
            let result = run_gateway_with_rr_io(cfg, role_table, clock, rx, None).await;
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
        let cache = Arc::clone(&handler.state().idempotency);

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
            let resp = build_clear_estop_response(&request_id, RpcStatus::Ok, "", 555_111, false);
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
            let resp = build_clear_estop_response(&request_id, RpcStatus::Ok, "", 888_111, false);
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
    ) -> (GatewayHandler, CrossbeamRecv<RrForwardRequest>) {
        let role_table = Arc::new(load_role_table(TEST_RPC_ROLES_TOML).expect("role table parse"));
        let cache = Arc::new(Mutex::new(IdempotencyCache::with_capacity(
            Duration::from_mins(1),
            cache_max_entries,
        )));
        let (set_enabled_tx, set_enabled_rx) = bounded::<RrForwardRequest>(64);
        let (clear_estop_tx, ce_rx) = bounded::<RrForwardRequest>(1);
        let (task_invoke_tx, ti_rx) = bounded::<RrForwardRequest>(1);
        let (task_cancel_tx, tc_rx) = bounded::<RrForwardRequest>(1);
        let (task_status_tx, ts_rx) = bounded::<RrForwardRequest>(1);
        let (clear_error_tx, cer_rx) = bounded::<RrForwardRequest>(1);
        let (estop_pub_tx, ep_rx) = bounded::<EStopPubForward>(1);
        Box::leak(Box::new(ce_rx));
        Box::leak(Box::new(ti_rx));
        Box::leak(Box::new(tc_rx));
        Box::leak(Box::new(ts_rx));
        Box::leak(Box::new(cer_rx));
        Box::leak(Box::new(ep_rx));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Box::leak(Box::new(shutdown_tx));
        let state = GatewayHandlerState {
            rr_forward_tx_clear_estop: clear_estop_tx,
            rr_forward_tx_task_invoke: task_invoke_tx,
            rr_forward_tx_task_cancel: task_cancel_tx,
            rr_forward_tx_task_status: task_status_tx,
            rr_forward_tx_clear_error: clear_error_tx,
            rr_forward_tx_set_enabled: set_enabled_tx,
            estop_pub_tx,
            robot_id: "test_robot".to_string(),
            role_table,
            idempotency: cache,
            clock: Arc::new(SystemClock),
            rr_timeout_grace_ms: 500,
            shutdown_rx,
            state_cache: Arc::new(StateCache::new(&[])),
        };
        (GatewayHandler::new(state), set_enabled_rx)
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
            let resp = build_set_enabled_response(&req.request_id, status, domain, 7_777, "");
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
        let cache = Arc::clone(&handler.state().idempotency);

        // mock daemon：**故意**回错 request_id（"wrong"），status=Ok，让 mismatch
        // 校验是唯一拦截点（否则 Ok 会 commit cache）。
        let mock = tokio::task::spawn_blocking(move || {
            let req = rx.recv_timeout(Duration::from_secs(2)).expect("forward");
            let resp = build_set_enabled_response(
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
        assert_eq!(err.code(), tonic::Code::Internal);
        assert!(
            err.message().contains("does not match"),
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
            0,
            "request_id mismatch 路径**不**应缓存"
        );
    }

    // ════════════════════════════════════════════════════════════════════════
    // v0.5 候选 D EStop handler 单测
    // ════════════════════════════════════════════════════════════════════════

    /// 构造 handler，estop_pub = active forward channel（返 rx 充当 mock
    /// publish 线程）；其它 6 条 leak rx。`estop_buf` 控制容量（PB2 Full
    /// unit 用 1）。
    fn make_estop_handler(estop_buf: usize) -> (GatewayHandler, CrossbeamRecv<EStopPubForward>) {
        let role_table = Arc::new(load_role_table(TEST_RPC_ROLES_TOML).expect("role table parse"));
        let cache = Arc::new(Mutex::new(IdempotencyCache::with_capacity(
            Duration::from_mins(1),
            64,
        )));
        let (estop_pub_tx, estop_pub_rx) = bounded::<EStopPubForward>(estop_buf);
        let (clear_estop_tx, ce_rx) = bounded::<RrForwardRequest>(1);
        let (task_invoke_tx, ti_rx) = bounded::<RrForwardRequest>(1);
        let (task_cancel_tx, tc_rx) = bounded::<RrForwardRequest>(1);
        let (task_status_tx, ts_rx) = bounded::<RrForwardRequest>(1);
        let (clear_error_tx, cer_rx) = bounded::<RrForwardRequest>(1);
        let (set_enabled_tx, se_rx) = bounded::<RrForwardRequest>(1);
        Box::leak(Box::new(ce_rx));
        Box::leak(Box::new(ti_rx));
        Box::leak(Box::new(tc_rx));
        Box::leak(Box::new(ts_rx));
        Box::leak(Box::new(cer_rx));
        Box::leak(Box::new(se_rx));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Box::leak(Box::new(shutdown_tx));
        let state = GatewayHandlerState {
            rr_forward_tx_clear_estop: clear_estop_tx,
            rr_forward_tx_task_invoke: task_invoke_tx,
            rr_forward_tx_task_cancel: task_cancel_tx,
            rr_forward_tx_task_status: task_status_tx,
            rr_forward_tx_clear_error: clear_error_tx,
            rr_forward_tx_set_enabled: set_enabled_tx,
            estop_pub_tx,
            robot_id: "test_robot".to_string(),
            role_table,
            idempotency: cache,
            clock: Arc::new(SystemClock),
            rr_timeout_grace_ms: 500,
            shutdown_rx,
            state_cache: Arc::new(StateCache::new(&[])),
        };
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
    fn mock_publish_ok(
        rx: CrossbeamRecv<EStopPubForward>,
        at_ns: u64,
    ) -> tokio::task::JoinHandle<EStopPubForward> {
        tokio::task::spawn_blocking(move || {
            let fwd = rx.recv_timeout(Duration::from_secs(2)).expect("forward");
            let (reply_tx, fwd_back) = {
                // 拆 oneshot 出来回 Ok；fwd 其余字段回传给断言。
                let EStopPubForward {
                    body_bytes,
                    stamp_ns,
                    oneshot_tx,
                    request_id,
                } = fwd;
                (
                    oneshot_tx,
                    EStopPubForward {
                        body_bytes,
                        stamp_ns,
                        oneshot_tx: tokio::sync::oneshot::channel().0,
                        request_id,
                    },
                )
            };
            reply_tx.send(Ok(at_ns)).expect("oneshot recv dropped");
            fwd_back
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
        let cache = Arc::clone(&handler.state().idempotency);
        let mock = mock_publish_ok(rx, 424_242);

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
        // 预塞满容量 1 的 channel（rx 持着不消费）。
        let (dummy_tx, _dummy_rx) = tokio::sync::oneshot::channel();
        handler
            .state()
            .estop_pub_tx
            .try_send(EStopPubForward {
                body_bytes: vec![0u8; 4],
                stamp_ns: 0,
                oneshot_tx: dummy_tx,
                request_id: "filler".into(),
            })
            .expect("filler 应入队");

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
        let (handler, rx) = make_estop_handler(64);

        // 第一发：mock 回 Ok(777)。fingerprint 字段须与第二发一致 →
        // 固定 source/reason/audit/token，仅 request_id 变（不进 fp）。
        let mut cmd1 = make_estop_cmd("idem-es-replay", "operator-fake-token-001");
        cmd1.meta.as_mut().unwrap().source = "replay.test".into();
        let mut cmd2 = make_estop_cmd("idem-es-replay", "operator-fake-token-001");
        cmd2.meta.as_mut().unwrap().source = "replay.test".into();

        let mock = mock_publish_ok(rx, 777);
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
