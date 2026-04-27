//! `robot_rpc_gateway` 服务端核心 —— A1-3 (2/3-a) skeleton。
//!
//! ## 角色
//!
//! reviewer A1-3 (2/3-a) 拍板的 binary 薄壳模型：
//!
//! - **bin (`src/bin/robot_rpc_gateway.rs`)**：`#[tokio::main]` + CLI parse +
//!   loopback bind 校验 + 起 [`run_gateway`]；不持业务逻辑。
//! - **lib (本模块)**：[`ClearEStopHandler`] + [`run_gateway`] + 公共
//!   [`GatewayConfig`] / [`LoopbackBindError`]。便于 lib 单测覆盖（不依赖
//!   bin entry）。
//!
//! ## A1-3 (2/3-a) scope
//!
//! - tonic gRPC server skeleton 起来（`bind` + `serve_with_shutdown`）。
//! - [`ClearEStopHandler::clear_e_stop`] 暂返 `tonic::Code::Unimplemented`；
//!   A1-3 (2/3-b) 接 RR I/O thread + `validate_clear_estop_command` +
//!   `clear_estop_idempotency_fingerprint` + `IdempotencyCache`；A1-3
//!   (2/3-c) 接 shutdown handling 完整链路。
//! - **loopback bind 校验**（reviewer 拍板硬边界）：默认
//!   `127.0.0.1:50051`；`--listen-addr` 可覆盖但**必须** loopback
//!   （`IpAddr::is_loopback()` 真）；`0.0.0.0` / `[::]` / 外网地址直接
//!   拒启动。fake token + plain TCP 阶段禁止暴露外网；TLS / mTLS 留
//!   v0.4+。
//!
//! ## 不在 A1-3 (2/3-a) 范围
//!
//! - RR I/O thread + `Iceoryx2RrClient` 跟 daemon 端真接（A1-3 (2/3-b)）
//! - idempotency cache 集成 + replay/conflict 单测（A1-3 (2/3-b)）
//! - shutdown handling 完整 graceful（A1-3 (2/3-c)）
//! - 端到端两进程集成测（A1-3 (3/3)）

use std::net::SocketAddr;
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::watch;

use crate::proto::robot_control_server::{RobotControl, RobotControlServer};
use crate::proto::{Ack, ClearEStopCommand};

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
// ClearEStopHandler
// ============================================================================

/// gRPC `RobotControl` service 实现。
///
/// **A1-3 (2/3-a) skeleton**：本 handler 仅打通 tonic server 链路；
/// `clear_e_stop` 返 `tonic::Code::Unimplemented`，让 A1-3 (2/3-b) 接 RR I/O
/// thread + idempotency cache + auth + 完整逻辑。
///
/// 设计为 `Arc` 持有：tonic clone handler 给每个 worker；handler 内部
/// state（A1-3 (2/3-b) 起加 RR forward channel + idempotency cache）走 `Arc`
/// 共享。
#[derive(Debug, Default)]
pub struct ClearEStopHandler {
    // A1-3 (2/3-b) 起加：
    //   - rr_forward_tx: crossbeam_channel::Sender<RrForwardRequest>
    //   - idempotency: Arc<IdempotencyCache>
    //   - auth_role_table: Arc<RoleTable>
    //   - clock: Arc<dyn RobotClock>
    //   - rr_timeout_grace_ms: u64
    _private: (),
}

impl ClearEStopHandler {
    #[must_use]
    pub fn new() -> Self {
        Self { _private: () }
    }
}

#[tonic::async_trait]
impl RobotControl for ClearEStopHandler {
    async fn clear_e_stop(
        &self,
        request: tonic::Request<ClearEStopCommand>,
    ) -> Result<tonic::Response<Ack>, tonic::Status> {
        // A1-3 (2/3-a) skeleton：返 Unimplemented，让 A1-3 (2/3-b) 接 RR
        // I/O thread + idempotency 实施。
        let request_id = request
            .get_ref()
            .meta
            .as_ref()
            .map_or("<no-meta>", |m| m.request_id.as_str());
        tracing::warn!(
            %request_id,
            "robot_rpc_gateway: ClearEStop received but A1-3 (2/3-a) skeleton stage; replying Unimplemented (A1-3 (2/3-b) 接 RR I/O thread)"
        );
        Err(tonic::Status::unimplemented(
            "robot_rpc_gateway A1-3 (2/3-a) skeleton: ClearEStop handler not yet wired to s0 daemon RR (A1-3 (2/3-b) 范围)",
        ))
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
/// `TcpListener::bind`）。
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
        }
    }
}

/// 起 tonic gRPC server，bind 到 [`GatewayConfig::listen_addr`]，挂
/// [`ClearEStopHandler`]，等 `shutdown_rx` 收到 `true` 后 graceful stop。
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
    handler: Arc<ClearEStopHandler>,
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
/// - tonic `serve_with_incoming_shutdown` 走 [`TcpIncoming::from_listener`]，
///   bind 时机由本函数控制（不是 tonic 内部）。
///
/// # Errors
///
/// 见 [`GatewayError`]。
pub async fn run_gateway_with_ready(
    cfg: GatewayConfig,
    handler: Arc<ClearEStopHandler>,
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
        "robot_rpc_gateway: starting tonic gRPC server (A1-3 (2/3-a) skeleton; ClearEStop returns Unimplemented)"
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

    let incoming = tonic::transport::server::TcpIncoming::from_listener(listener, true, None)
        .map_err(|e| {
            // TcpIncoming::from_listener 在已经 bind 成功的 TcpListener 上
            // 几乎不会失败；万一失败包成 GatewayError::Bind（保留原错误
            // 字符串到 io::Error message）。
            GatewayError::Bind {
                addr: actual_addr,
                source: std::io::Error::other(format!("{e}")),
            }
        })?;

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
// Tests —— A1-3 (2/3-a) skeleton 锁
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use tokio::time::timeout;

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
        };
        let (_tx, rx) = watch::channel(false);
        let result = run_gateway(cfg, Arc::new(ClearEStopHandler::new()), rx).await;
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
        };
        let (tx, rx) = watch::channel(false);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        let handle = tokio::spawn(async move {
            run_gateway_with_ready(cfg, Arc::new(ClearEStopHandler::new()), rx, Some(ready_tx))
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
            run_gateway_with_ready(cfg, Arc::new(ClearEStopHandler::new()), rx, Some(ready_tx))
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
        };
        let (shutdown_a_tx, shutdown_a_rx) = watch::channel(false);
        let (ready_a_tx, ready_a_rx) = tokio::sync::oneshot::channel();
        let handle_a = tokio::spawn(async move {
            run_gateway_with_ready(
                cfg_a,
                Arc::new(ClearEStopHandler::new()),
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
        };
        let (_shutdown_b_tx, shutdown_b_rx) = watch::channel(false);
        let (ready_b_tx, ready_b_rx) = tokio::sync::oneshot::channel();
        let handle_b = tokio::spawn(async move {
            run_gateway_with_ready(
                cfg_b,
                Arc::new(ClearEStopHandler::new()),
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

    // ────────────────────────────────────────────────────────────────────────
    // ClearEStopHandler skeleton: returns Unimplemented (A1-3 (2/3-a))
    // ────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn clear_estop_handler_skeleton_returns_unimplemented() {
        let handler = ClearEStopHandler::new();
        let cmd = ClearEStopCommand {
            meta: None,
            confirmation: "irrelevant".to_string(),
        };
        let request = tonic::Request::new(cmd);
        let result = handler.clear_e_stop(request).await;
        match result {
            Err(status) => {
                assert_eq!(
                    status.code(),
                    tonic::Code::Unimplemented,
                    "A1-3 (2/3-a) skeleton 必须返 Unimplemented；A1-3 (2/3-b) 接 RR I/O 后改"
                );
            }
            Ok(resp) => panic!("expected Unimplemented Status, got Ok({resp:?})"),
        }
    }
}
