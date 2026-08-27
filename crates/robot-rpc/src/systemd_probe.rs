//! tonic **loopback health probe**（v0.5 候选 E 段(8)，Spec D12 r3 T4 第 2 条
//! 与 R3-B2 / RB4 / R5-4 / QA4）：gateway 的 tonic serve task 无进度计数，
//! liveness 用"本地 unary 是否仍有响应"作 observer。
//!
//! ## 探针语义（QA4 改准）
//!
//! - 请求 = `GetState(GetStateRequest { hardware_id: "" })`，**期望
//!   `InvalidArgument`** 计成功：经 tonic transport + handler 入口校验
//!   （`gateway.rs` GetState ① 空 hardware_id → InvalidArgument，早于池查找），
//!   **不依赖 S0 / S1 / state cache 首帧**——原 `GetTaskStatus(unknown)` 会真
//!   forward S1 RR 等 ≤ 5 s，S1 重启即拖停 gateway watchdog，违反 `Wants=`
//!   语义，**撤回**。
//! - path 必须带 package 前缀：[`GET_STATE_PATH`] =
//!   `/robot.control.v1.RobotControl/GetState`（R5-4：漏前缀得
//!   `Unimplemented`，READY 永达不到，10 s 后启动失败）。
//! - 实现 = `tonic::client::Grpc<Channel>` + 既有 prost message 类型 +
//!   `tonic_prost::ProstCodec`（RB4：**不开 `client` feature**、不改生产
//!   client 边界——`RobotControlClient` 仅 `client` feature 生成）。
//!
//! ## 节奏
//!
//! [`PROBE_INTERVAL`] = 500 ms；首次成功经 `first_success_tx` 回执 gateway
//! （READY=1 门槛；上限 [`FIRST_SUCCESS_TIMEOUT`] = 10 s，超时 = 启动失败按
//! 既有错误路径退出）。`stop_rx`（gateway 内部 serve-shutdown watch）置位即退。
//!
//! ## 冻结（test-only）
//!
//! `freeze_until_ns` 非 0 → task 以 5 ms 分片 `tokio::time::sleep` 等到过期
//! （不 spin runtime worker），`success_count` 停滞 → watchdog withholding。

#![allow(clippy::doc_markdown)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tonic::client::Grpc;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request};
use tonic_prost::ProstCodec;

use crate::proto::{GetStateRequest, RobotState};

/// 探针周期。
pub const PROBE_INTERVAL: Duration = Duration::from_millis(500);
/// 首次成功上限（超时 = gateway 启动失败）。
pub const FIRST_SUCCESS_TIMEOUT: Duration = Duration::from_secs(10);
/// `RobotControl.GetState` 完整 gRPC path（package = `robot.control.v1`，
/// `robot_control.proto:35`）。
pub const GET_STATE_PATH: &str = "/robot.control.v1.RobotControl/GetState";
/// 失败 warn 限频：首条必报 + 每 N 条一报。
const FAILURE_LOG_EVERY: u64 = 20;

/// 探针 metrics（`Arc` 由 gateway 持有并借给 watchdog observer / freeze control）。
#[derive(Debug, Default)]
pub struct TonicProbeMetrics {
    /// 发起的 unary 次数（诊断）。
    pub attempts: AtomicU64,
    /// 收到 `InvalidArgument` 的次数——**watchdog observer 源**。
    pub success_count: AtomicU64,
    /// 非预期结果（transport 错 / 其它 status / `Ok`）次数。
    pub failure_count: AtomicU64,
    /// test-only：冻结截止（absolute ns，0 = 未冻结）。
    #[cfg(any(test, feature = "test-freeze-hook"))]
    pub freeze_until_ns: AtomicU64,
}

/// 探针 task 句柄：`stop_rx` 置位后 task 自行退出；[`Self::join`] 等待。
pub struct ProbeHandle {
    task: JoinHandle<()>,
}

impl ProbeHandle {
    /// 等 task 退出（task panic 仅 log，不传播）。
    pub async fn join(self) {
        if let Err(e) = self.task.await {
            tracing::error!(error = ?e, "tonic_probe: task join error");
        }
    }
}

/// 起探针 task（tokio runtime 内调用）。
///
/// `addr` = tonic server **实际** bind 地址（`:0` 场景取 `local_addr`）。
#[must_use]
pub fn spawn_tonic_probe(
    addr: SocketAddr,
    metrics: Arc<TonicProbeMetrics>,
    mut stop_rx: watch::Receiver<bool>,
    first_success_tx: oneshot::Sender<()>,
) -> ProbeHandle {
    let task = tokio::spawn(async move {
        let endpoint = match Endpoint::from_shared(format!("http://{addr}")) {
            Ok(ep) => ep.connect_timeout(Duration::from_secs(1)),
            Err(e) => {
                tracing::error!(%addr, error = ?e, "tonic_probe: invalid endpoint; probe inactive");
                return;
            }
        };
        let mut grpc = Grpc::new(endpoint.connect_lazy());
        let mut first = Some(first_success_tx);
        #[cfg(any(test, feature = "test-freeze-hook"))]
        let clock = robot_core::time::SystemClock::new();

        loop {
            if *stop_rx.borrow() {
                break;
            }
            #[cfg(any(test, feature = "test-freeze-hook"))]
            {
                use robot_core::time::RobotClock;
                let until = metrics.freeze_until_ns.load(Ordering::Acquire);
                if until != 0 {
                    while clock.now().0 < until && !*stop_rx.borrow() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
            }
            metrics.attempts.fetch_add(1, Ordering::Relaxed);
            match probe_once(&mut grpc).await {
                Ok(()) => {
                    metrics.success_count.fetch_add(1, Ordering::Release);
                    if let Some(tx) = first.take() {
                        let _ = tx.send(());
                        tracing::info!(%addr, "tonic_probe: first loopback probe succeeded (InvalidArgument as expected)");
                    }
                }
                Err(e) => {
                    let n = metrics.failure_count.fetch_add(1, Ordering::Relaxed) + 1;
                    if n == 1 || n.is_multiple_of(FAILURE_LOG_EVERY) {
                        tracing::warn!(%addr, failures = n, error = %e, "tonic_probe: loopback probe failed");
                    }
                }
            }
            tokio::select! {
                () = tokio::time::sleep(PROBE_INTERVAL) => {}
                changed = stop_rx.changed() => {
                    if changed.is_err() || *stop_rx.borrow() {
                        break;
                    }
                }
            }
        }
        tracing::info!(%addr, "tonic_probe: stopped");
    });
    ProbeHandle { task }
}

/// 一次 unary：成功 ⟺ 收到 `InvalidArgument`（空 `hardware_id` 被 handler 拒）。
async fn probe_once(grpc: &mut Grpc<Channel>) -> Result<(), String> {
    grpc.ready()
        .await
        .map_err(|e| format!("channel not ready: {e}"))?;
    let req = Request::new(GetStateRequest {
        hardware_id: String::new(),
    });
    let codec = ProstCodec::<GetStateRequest, RobotState>::default();
    let path = PathAndQuery::from_static(GET_STATE_PATH);
    match grpc.unary(req, path, codec).await {
        Ok(_) => Err("unexpected Ok for empty hardware_id (handler contract drift)".into()),
        Err(status) if status.code() == Code::InvalidArgument => Ok(()),
        Err(status) => Err(format!(
            "unexpected status {:?}: {}",
            status.code(),
            status.message()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_has_package_prefix() {
        // R5-4：漏 package 前缀 → Unimplemented → READY 永达不到。
        assert!(GET_STATE_PATH.starts_with("/robot.control.v1.RobotControl/"));
        let _ = PathAndQuery::from_static(GET_STATE_PATH);
    }

    #[test]
    fn metrics_default_zero() {
        let m = TonicProbeMetrics::default();
        assert_eq!(m.attempts.load(Ordering::Relaxed), 0);
        assert_eq!(m.success_count.load(Ordering::Relaxed), 0);
        assert_eq!(m.failure_count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn probe_against_closed_port_counts_failures_and_stops() {
        // 无 server 的回环端口：探针必须计 failure、不 panic，stop 置位后退出。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let metrics = Arc::new(TonicProbeMetrics::default());
        let (stop_tx, stop_rx) = watch::channel(false);
        let (first_tx, first_rx) = oneshot::channel();
        let handle = spawn_tonic_probe(addr, Arc::clone(&metrics), stop_rx, first_tx);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(metrics.attempts.load(Ordering::Relaxed) >= 1);
        assert_eq!(metrics.success_count.load(Ordering::Relaxed), 0);
        let _ = stop_tx.send(true);
        tokio::time::timeout(Duration::from_secs(3), handle.join())
            .await
            .expect("probe task must stop after stop_rx");
        assert!(first_rx.await.is_err(), "无 server 不得回执首成");
    }
}
