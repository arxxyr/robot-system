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
//! ## 节奏与有界性（round 2 阻断 2）
//!
//! [`PROBE_INTERVAL`] = 500 ms；首次成功经 `first_success_tx` 回执 gateway
//! （READY=1 门槛；上限 [`FIRST_SUCCESS_TIMEOUT`] = 10 s，超时 = 启动失败按
//! 既有错误路径退出）。**每次 attempt 有界**：`ready()` + `unary()` 整体包
//! [`ATTEMPT_TIMEOUT`] = 400 ms（原先只有 connect 超时——服务端 accept 后不
//! 响应 h2 preface / 不回响应时 attempt 永挂，10 s 外层超时后 join 也永挂）；
//! attempt 期间与 `stop_rx` `select!`，stop 置位即中断。`stop_rx`（gateway 内部
//! serve-shutdown watch）置位即退；[`ProbeHandle::join`] **两级有界**——正常
//! join 上限 [`JOIN_TIMEOUT`]，超时后 `abort()` 再以 [`ABORT_JOIN_TIMEOUT`]
//! 有界等**取消确认**（tokio abort 是协作取消，任务只在下一个 await 点才真正
//! 结束；round 2 P1-1：原实现 abort 后直接无界 await，仍非硬上界）；确认再
//! 超时 → 严重日志 + 返回（task detach，**不**阻塞停机）。
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
/// 单次 attempt（`ready()` + `unary()`）上限；< [`PROBE_INTERVAL`]（round 2 阻断 2）。
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(400);
/// [`ProbeHandle::join`] **正常** join 上限：attempt ≤ 400 ms + 冻结分片 5 ms +
/// select 即时，正常 < 1 s；超过即 abort（不让停机被探针拖死）。
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(2);
/// `abort()` 后等**取消确认**的上限（tokio abort 协作取消，任务未 yield 时不会
/// 立刻完成）；再超时 = 放弃等待并 detach，返回不阻塞停机（round 2 P1-1）。
pub const ABORT_JOIN_TIMEOUT: Duration = Duration::from_millis(500);
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
    /// 等 task 退出，**两级有界**：正常 join ≤ [`JOIN_TIMEOUT`]；超时 →
    /// `abort()` + 取消确认 ≤ [`ABORT_JOIN_TIMEOUT`]；确认再超时 → 严重日志 +
    /// 返回（detach）。task panic / 超时仅 log，不传播。调用方须先置位
    /// `stop_rx`。**返回时延硬上界 ≈ `JOIN_TIMEOUT + ABORT_JOIN_TIMEOUT`**。
    pub async fn join(self) {
        self.join_with_timeouts(JOIN_TIMEOUT, ABORT_JOIN_TIMEOUT)
            .await;
    }

    /// [`Self::join`] 的可注入超时版（单测缩短窗口；生产走常量）。
    async fn join_with_timeouts(mut self, join_timeout: Duration, abort_timeout: Duration) {
        match tokio::time::timeout(join_timeout, &mut self.task).await {
            Ok(Ok(())) => return,
            Ok(Err(e)) => {
                tracing::error!(error = ?e, "tonic_probe: task join error");
                return;
            }
            Err(_elapsed) => tracing::error!(
                timeout = ?join_timeout,
                "tonic_probe: task did not stop in time; aborting"
            ),
        }
        self.task.abort();
        if tokio::time::timeout(abort_timeout, &mut self.task)
            .await
            .is_err()
        {
            tracing::error!(
                timeout = ?abort_timeout,
                "tonic_probe: abort not confirmed (task never yielded); detaching and continuing shutdown"
            );
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
            // attempt 有界 + 可被 stop 打断（round 2 阻断 2）。
            let outcome = tokio::select! {
                r = tokio::time::timeout(ATTEMPT_TIMEOUT, probe_once(&mut grpc)) => match r {
                    Ok(inner) => inner,
                    Err(_elapsed) => Err(format!("attempt exceeded {ATTEMPT_TIMEOUT:?}")),
                },
                changed = stop_rx.changed() => {
                    if changed.is_err() || *stop_rx.borrow() {
                        break;
                    }
                    continue;
                }
            };
            match outcome {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn join_returns_even_when_abort_is_not_confirmed() {
        // 任务体是**阻塞** sleep（永不 yield）→ `abort()` 不会立刻生效（tokio
        // 协作取消）。round 2 P1-1 回归锁：join 必须在 join_timeout +
        // abort_timeout 量级返回，而不是等任务自己跑完。
        let handle = ProbeHandle {
            task: tokio::spawn(async {
                std::thread::sleep(Duration::from_millis(400));
            }),
        };
        let t0 = std::time::Instant::now();
        handle
            .join_with_timeouts(Duration::from_millis(50), Duration::from_millis(50))
            .await;
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_millis(300),
            "join 必须两级有界返回（不等任务跑完），实际 {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn hung_server_attempts_time_out_and_stop_join_is_bounded() {
        // 服务端只 accept **不响应**（不回 h2 preface / SETTINGS）：无 attempt 上限
        // 时 `ready()` 永挂，stop 后 join 也永挂（round 2 阻断 2 回归锁）。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let held: Arc<std::sync::Mutex<Vec<tokio::net::TcpStream>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let held_in = Arc::clone(&held);
        let acceptor = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept");
                held_in.lock().unwrap().push(stream);
            }
        });
        let metrics = Arc::new(TonicProbeMetrics::default());
        let (stop_tx, stop_rx) = watch::channel(false);
        let (first_tx, first_rx) = oneshot::channel();
        let handle = spawn_tonic_probe(addr, Arc::clone(&metrics), stop_rx, first_tx);
        tokio::time::sleep(ATTEMPT_TIMEOUT + Duration::from_millis(400)).await;
        assert!(metrics.attempts.load(Ordering::Relaxed) >= 1);
        assert!(
            metrics.failure_count.load(Ordering::Relaxed) >= 1,
            "hung attempt 必须经 ATTEMPT_TIMEOUT 计 failure"
        );
        assert_eq!(metrics.success_count.load(Ordering::Relaxed), 0);
        let _ = stop_tx.send(true);
        let t0 = std::time::Instant::now();
        handle.join().await;
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "stop → join 必须有界（attempt 超时 + select stop），实际 {:?}",
            t0.elapsed()
        );
        assert!(first_rx.await.is_err(), "hung server 不得回执首成");
        acceptor.abort();
        assert!(!held.lock().unwrap().is_empty(), "探针至少建过一条连接");
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
