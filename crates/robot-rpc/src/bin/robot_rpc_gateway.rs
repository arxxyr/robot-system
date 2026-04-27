//! `robot_rpc_gateway` —— A1-3 (2/3-a) skeleton bin。
//!
//! ### 角色
//!
//! 独立进程，跟 `s0_daemon` 跨进程 iceoryx2 RR 通信（A1-3 (2/3-b) 起接通）。
//! gRPC client 端通过 tonic transport 进 gateway，gateway 把请求 forward 到
//! daemon 端 `/<robot_id>/control/clear_estop` topic。
//!
//! ### A1-3 (2/3-a) skeleton 阶段
//!
//! - 起 tonic gRPC server bind 到 loopback `127.0.0.1:50051`（默认）/
//!   `--listen-addr` 覆盖（必须 loopback；`0.0.0.0` 等非 loopback 直接拒）
//! - `ClearEStop` handler 返 `tonic::Code::Unimplemented`（A1-3 (2/3-b) 接
//!   RR I/O thread + idempotency cache 之前的占位）
//! - SIGINT / SIGTERM 触发 `tokio::sync::watch` shutdown 信号 → tonic
//!   `serve_with_shutdown` graceful stop
//!
//! ### CLI
//!
//! ```text
//! robot_rpc_gateway --robot-id <ID> \
//!                   --listen-addr 127.0.0.1:50051 \
//!                   --idempotency-ttl-secs 300 \
//!                   --idempotency-max-entries 4096 \
//!                   --rr-timeout-grace-ms 500
//! ```
//!
//! ### 退出码
//!
//! - `0`：graceful shutdown（SIGINT/SIGTERM 触发后 tonic server 干净退出）。
//! - `1`：tonic server 启动失败 / 运行期错（端口占用 / 其它 IO）。
//! - `2`：CLI 参数解析失败 / `--listen-addr` 非 loopback 拒启动。

#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::sync::Arc;

use robot_rpc::gateway::{
    ClearEStopHandler, GatewayConfig, GatewayError, LoopbackBindError, run_gateway,
};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

// ============================================================================
// CLI
// ============================================================================

#[derive(Debug)]
struct Args {
    robot_id: String,
    listen_addr: SocketAddr,
    idempotency_ttl_secs: u64,
    idempotency_max_entries: usize,
    rr_timeout_grace_ms: u64,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut robot_id: Option<String> = None;
        let mut listen_addr: SocketAddr = "127.0.0.1:50051".parse().expect("loopback default");
        let mut idempotency_ttl_secs: u64 = 300;
        let mut idempotency_max_entries: usize = 4096;
        let mut rr_timeout_grace_ms: u64 = 500;

        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            match a.as_str() {
                "--robot-id" => {
                    i += 1;
                    robot_id = Some(raw.get(i).ok_or("--robot-id requires value")?.clone());
                }
                "--listen-addr" => {
                    i += 1;
                    let val = raw.get(i).ok_or("--listen-addr requires value")?;
                    listen_addr = val
                        .parse()
                        .map_err(|e| format!("--listen-addr parse failed: {e}"))?;
                }
                "--idempotency-ttl-secs" => {
                    i += 1;
                    let val = raw.get(i).ok_or("--idempotency-ttl-secs requires value")?;
                    idempotency_ttl_secs = val
                        .parse()
                        .map_err(|e| format!("--idempotency-ttl-secs parse failed: {e}"))?;
                }
                "--idempotency-max-entries" => {
                    i += 1;
                    let val = raw
                        .get(i)
                        .ok_or("--idempotency-max-entries requires value")?;
                    idempotency_max_entries = val
                        .parse()
                        .map_err(|e| format!("--idempotency-max-entries parse failed: {e}"))?;
                }
                "--rr-timeout-grace-ms" => {
                    i += 1;
                    let val = raw.get(i).ok_or("--rr-timeout-grace-ms requires value")?;
                    rr_timeout_grace_ms = val
                        .parse()
                        .map_err(|e| format!("--rr-timeout-grace-ms parse failed: {e}"))?;
                }
                "--help" | "-h" => return Err("HELP".to_string()),
                other => return Err(format!("unknown CLI arg: {other}")),
            }
            i += 1;
        }

        let robot_id = robot_id.ok_or("--robot-id is required")?;
        if robot_id.is_empty() {
            return Err("--robot-id must be non-empty".to_string());
        }

        Ok(Self {
            robot_id,
            listen_addr,
            idempotency_ttl_secs,
            idempotency_max_entries,
            rr_timeout_grace_ms,
        })
    }
}

fn print_help() {
    eprintln!(
        "robot_rpc_gateway —— v0.3 A1-3 (2/3-a) ClearEStop control-plane gateway

USAGE:
  robot_rpc_gateway --robot-id <ID> [OPTIONS]

OPTIONS:
  --robot-id <ID>                  机器人标识符（必填；用于 iceoryx2 RR
                                   topic /<robot_id>/control/clear_estop
                                   A1-3 (2/3-b) 起接通）。
  --listen-addr <ADDR>             gRPC server bind 地址。默认 127.0.0.1:50051。
                                   **必须 loopback**（127.0.0.0/8 或 ::1）；
                                   0.0.0.0 / 外网地址直接拒启动（v0.3
                                   fake-token + plain TCP；TLS 留 v0.4+）。
  --idempotency-ttl-secs <SECS>    idempotency cache TTL（默认 300）。
  --idempotency-max-entries <N>    idempotency cache 上限（默认 4096；
                                   lazy GC 后仍满返 ResourceExhausted）。
  --rr-timeout-grace-ms <MS>       gateway 等 RR response 超过 timeout_ms
                                   后再多等的 grace（默认 500ms；防 daemon
                                   deadline 边缘 action 已开始却被 gateway
                                   过早 DeadlineExceeded 制造副作用歧义）。
  -h, --help                       显示帮助。

退出码:
  0  graceful shutdown
  1  tonic server 启动失败 / 运行期错
  2  CLI 参数解析失败 / listen-addr 非 loopback
"
    );
}

// ============================================================================
// main
// ============================================================================

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let _ = robot_logging::init();

    let args = match Args::parse() {
        Ok(a) => a,
        Err(msg) => {
            if msg == "HELP" {
                print_help();
                std::process::exit(0);
            }
            tracing::error!(error = %msg, "CLI argument parse failed");
            print_help();
            std::process::exit(2);
        }
    };

    let cfg = GatewayConfig {
        listen_addr: args.listen_addr,
        robot_id: args.robot_id,
        idempotency_ttl_secs: args.idempotency_ttl_secs,
        idempotency_max_entries: args.idempotency_max_entries,
        rr_timeout_grace_ms: args.rr_timeout_grace_ms,
    };

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // 注册 SIGINT / SIGTERM handler。tokio::signal::unix 是 Send-friendly
    // async API；触发后 set shutdown_tx → run_gateway 通过 shutdown_rx
    // 收到 graceful stop 信号。
    spawn_signal_handler(shutdown_tx);

    let handler = Arc::new(ClearEStopHandler::new());

    match run_gateway(cfg, handler, shutdown_rx).await {
        Ok(()) => {
            tracing::info!("robot_rpc_gateway: clean shutdown");
        }
        Err(GatewayError::LoopbackBind(LoopbackBindError::NonLoopback(addr))) => {
            tracing::error!(
                %addr,
                "fatal: --listen-addr must be loopback (127.0.0.0/8 or ::1); v0.3 \
                 fake-token + plain TCP 阶段禁绑外网"
            );
            std::process::exit(2);
        }
        Err(GatewayError::Tonic(e)) => {
            tracing::error!(error = ?e, "fatal: tonic transport failed");
            std::process::exit(1);
        }
    }
}

fn spawn_signal_handler(shutdown_tx: watch::Sender<bool>) {
    tokio::spawn(async move {
        // SIGINT 用 ctrl_c 抽象（tokio::signal::ctrl_c）；SIGTERM 走
        // tokio::signal::unix::signal。等任一触发即 set shutdown=true。
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = ?e, "failed to install SIGTERM handler");
                return;
            }
        };
        tokio::select! {
            res = tokio::signal::ctrl_c() => {
                if let Err(e) = res {
                    tracing::error!(error = ?e, "ctrl_c handler failed");
                }
                tracing::info!("robot_rpc_gateway: SIGINT received; triggering shutdown");
            }
            _ = sigterm.recv() => {
                tracing::info!("robot_rpc_gateway: SIGTERM received; triggering shutdown");
            }
        }
        let _ = shutdown_tx.send(true);
    });
}
