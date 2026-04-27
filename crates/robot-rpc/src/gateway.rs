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
}

// ============================================================================
// loopback bind 校验
// ============================================================================

/// `validate_loopback_bind` 错误。reviewer A1-3 (2/3-a) 拍板硬边界：plain
/// TCP + fake token 阶段非 loopback 必须拒启动。
#[derive(Debug, Error, PartialEq, Eq)]
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
#[derive(Debug, Error)]
pub enum GatewayError {
    /// listen-addr 校验失败（非 loopback）。
    #[error(transparent)]
    LoopbackBind(#[from] LoopbackBindError),

    /// tonic transport 启动失败 / 服务运行期出错（端口占用 / IO 错）。
    #[error("tonic transport: {0}")]
    Tonic(#[from] tonic::transport::Error),
}

/// 起 tonic gRPC server，bind 到 [`GatewayConfig::listen_addr`]，挂
/// [`ClearEStopHandler`]，等 `shutdown_rx` 收到 `true` 后 graceful stop。
///
/// **loopback 校验**先行：非 loopback 立刻返 [`GatewayError::LoopbackBind`]，
/// 不起 server（reviewer 拍板硬边界）。
///
/// `shutdown_rx` 是 [`tokio::sync::watch::Receiver<bool>`]：bin main 注册
/// SIGINT/SIGTERM handler set true 触发 graceful shutdown。
///
/// # Errors
///
/// 见 [`GatewayError`]。
pub async fn run_gateway(
    cfg: GatewayConfig,
    handler: Arc<ClearEStopHandler>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), GatewayError> {
    validate_loopback_bind(&cfg.listen_addr)?;

    tracing::info!(
        listen_addr = %cfg.listen_addr,
        robot_id = %cfg.robot_id,
        idempotency_ttl_secs = cfg.idempotency_ttl_secs,
        idempotency_max_entries = cfg.idempotency_max_entries,
        rr_timeout_grace_ms = cfg.rr_timeout_grace_ms,
        "robot_rpc_gateway: starting tonic gRPC server (A1-3 (2/3-a) skeleton; ClearEStop returns Unimplemented)"
    );

    let svc = RobotControlServer::from_arc(handler);

    let shutdown_signal = async move {
        // 等 shutdown_rx 翻 true（main 端 signal handler 触发）。watch
        // channel 在 sender drop 后也会 changed → recv → returns Err，视同
        // shutdown 信号。
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
        .serve_with_shutdown(cfg.listen_addr, shutdown_signal)
        .await?;

    tracing::info!("robot_rpc_gateway: tonic server stopped cleanly");
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
            Err(GatewayError::LoopbackBind(LoopbackBindError::NonLoopback(_))) => {}
            other => panic!("expected LoopbackBind error, got {other:?}"),
        }
    }

    // ────────────────────────────────────────────────────────────────────────
    // run_gateway: tonic server 真起 + shutdown signal 触发 graceful stop
    // ────────────────────────────────────────────────────────────────────────

    /// `127.0.0.1:0` (OS 自动分配端口) bind → tonic server 起来后通过
    /// shutdown_rx 触发 graceful stop；await 应在 1s 内返回 Ok(())。
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

        let handle =
            tokio::spawn(
                async move { run_gateway(cfg, Arc::new(ClearEStopHandler::new()), rx).await },
            );

        // 等 server bind 完成（tonic 起来后 spawn task 已经 await
        // serve_with_shutdown）。50ms 通常足够；如需更稳可改成 retry-poll
        // listen-addr。
        tokio::time::sleep(Duration::from_millis(50)).await;

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
