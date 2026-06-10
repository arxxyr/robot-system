//! 三段式 ready signal：marker → TCP probe → RPC probe。
//!
//! 详 Spec § Q2 Research Findings + § Plan §3 各段 timeout 数字。
//!
//! ## 失败 fingerprint
//!
//! - **marker 卡住** = TOML parse / panic / iceoryx2 service 资源
//!   create 失败 →  EarlyExit / Timeout 都看 stdout/stderr buffer 的
//!   `[FORENSIC]` log
//! - **TCP probe 卡** 但 marker 通 = tonic bind race（gateway 起 marker
//!   后才 bind listen_addr；retry 50ms × 10 次内一定通）
//! - **RPC probe 卡** 但 TCP 通 = rr_io_thread / s1 RR server 之间 iceoryx2
//!   service create 慢 → retry 100ms × 50 次 (5s 总) 内通
//!
//! ## 三段独立 timeout
//!
//! 总 wall time ≤ 5s + 5s + 0.5s + 5s = 15.5s/test（实测应远低）。

use std::net::SocketAddr;
use std::time::Duration;

use thiserror::Error;
use tonic::transport::{Channel, Endpoint};

use crate::common::subprocess::{MarkerError, SubprocessHandle};

const S1_READY_MARKER: &str = "s1_daemon rr_server_threads ready";
/// **count-independent 前缀**（v0.5 候选 C 修）：gateway 该行带 rr_io_thread
/// 计数（B4 时 5 → 候选 C 6），全串硬编码会在每次 method append 后 timeout
/// 假死（marker 永不匹配）。readiness 只需"全部 rr_io_thread 已 spawn"信号
/// （任一 spawn 失败 gateway fail-fast 退出，不会打出此行），计数不增加安全性。
const GATEWAY_READY_MARKER: &str = "rr_io_threads started";

const MARKER_TIMEOUT: Duration = Duration::from_secs(5);
const TCP_TOTAL: Duration = Duration::from_millis(500);
const TCP_INTERVAL: Duration = Duration::from_millis(50);
const RPC_TOTAL: Duration = Duration::from_secs(5);
const RPC_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Error)]
pub enum ReadyError {
    #[error("s1 marker: {0}")]
    S1Marker(MarkerError),
    #[error("gateway marker: {0}")]
    GatewayMarker(MarkerError),
    #[error("TCP probe {addr} failed within {total_ms} ms (last error: {last_err})")]
    Tcp {
        addr: SocketAddr,
        total_ms: u128,
        last_err: String,
    },
    #[error("RPC probe failed within {total_ms} ms: {reason}")]
    Rpc { total_ms: u128, reason: String },
    #[error("tonic Endpoint construct: {0}")]
    Endpoint(String),
}

/// 等三段式 ready 完成。
///
/// 1. wait s1 marker `"s1_daemon rr_server_threads ready"` (5s timeout)
/// 2. wait gateway marker `"rr_io_threads started"`（count-independent 前缀，
///    5s timeout）
/// 3. TCP probe gateway listen_addr (500ms total / 50ms retry)
/// 4. RPC probe `GetTaskStatus { unknown action_id }` 期 `NotFound` (5s total / 100ms retry)
///
/// 返成功建立的 tonic `Channel`，caller 用以构造 `RobotControlClient`。
///
/// # Errors
///
/// 见 [`ReadyError`]。
pub async fn wait_ready(
    s1_handle: &mut SubprocessHandle,
    gateway_handle: &mut SubprocessHandle,
    listen_addr: SocketAddr,
) -> Result<Channel, ReadyError> {
    // Step 1: s1 marker
    s1_handle
        .wait_for_marker(S1_READY_MARKER, MARKER_TIMEOUT)
        .await
        .map_err(ReadyError::S1Marker)?;

    // Step 2: gateway marker
    gateway_handle
        .wait_for_marker(GATEWAY_READY_MARKER, MARKER_TIMEOUT)
        .await
        .map_err(ReadyError::GatewayMarker)?;

    // Step 3: TCP probe
    tcp_probe(listen_addr, TCP_TOTAL, TCP_INTERVAL).await?;

    // Step 4: RPC probe
    let endpoint = Endpoint::from_shared(format!("http://{listen_addr}"))
        .map_err(|e| ReadyError::Endpoint(e.to_string()))?;
    let channel = endpoint
        .connect()
        .await
        .map_err(|e| ReadyError::Endpoint(e.to_string()))?;
    grpc_probe(channel.clone(), RPC_TOTAL, RPC_INTERVAL).await?;
    Ok(channel)
}

async fn tcp_probe(
    addr: SocketAddr,
    total: Duration,
    interval: Duration,
) -> Result<(), ReadyError> {
    let deadline = tokio::time::Instant::now() + total;
    let mut last_err = String::new();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(interval, tokio::net::TcpStream::connect(addr)).await {
            Ok(Ok(_stream)) => return Ok(()),
            Ok(Err(e)) => last_err = e.to_string(),
            Err(_) => last_err = "connect attempt timed out".to_string(),
        }
        tokio::time::sleep(interval).await;
    }
    Err(ReadyError::Tcp {
        addr,
        total_ms: total.as_millis(),
        last_err,
    })
}

async fn grpc_probe(
    channel: Channel,
    total: Duration,
    interval: Duration,
) -> Result<(), ReadyError> {
    use robot_rpc::proto::TaskHandleRef;
    use robot_rpc::proto::robot_control_client::RobotControlClient;

    let deadline = tokio::time::Instant::now() + total;
    let mut last_reason = String::new();
    let mut client = RobotControlClient::new(channel);

    while tokio::time::Instant::now() < deadline {
        let req = TaskHandleRef {
            request_id: uuid::Uuid::now_v7().to_string(),
            // zero UUID v7（保 v7 标识 + 不撞真 ID）
            action_id: "00000000-0000-7000-8000-000000000000".to_string(),
        };
        match client.get_task_status(req).await {
            Ok(_) => {
                // 不期望 Ok（unknown action_id 应 NotFound）；但若返 Ok
                // 也算通了端到端（极端情况），算 ready。
                return Ok(());
            }
            Err(status) => {
                if status.code() == tonic::Code::NotFound {
                    return Ok(());
                }
                last_reason = format!("status={:?} msg={}", status.code(), status.message());
            }
        }
        tokio::time::sleep(interval).await;
    }
    Err(ReadyError::Rpc {
        total_ms: total.as_millis(),
        reason: last_reason,
    })
}
