//! `robot_rpc_gateway` —— A1-3 (2/3-b2-2) ClearEStop control-plane bin。
//!
//! ### 角色
//!
//! 独立进程，跟 `s0_daemon` 跨进程 iceoryx2 RR 通信。gRPC client 端通过
//! tonic transport 进 gateway，gateway 把请求 forward 到 daemon 端
//! `/<robot_id>/control/clear_estop` topic（`robot_rpc::gateway::run_gateway_with_rr_io`
//! 高层入口内部 spawn `rr_io_thread` 起独立 `std::thread` 持
//! `Iceoryx2RrClient`）。
//!
//! ### A1-3 (2/3-b2-2) 已落地
//!
//! - 起 tonic gRPC server bind 到 loopback `127.0.0.1:50051`（默认）/
//!   `--listen-addr` 覆盖（必须 loopback；`0.0.0.0` 等非 loopback 直接拒）。
//! - `ClearEStop` handler 真起 12 步 logic：`validate` → `auth` → `authz`
//!   (`maintainer+`) → idempotency fingerprint → `IdempotencyCache::decide`
//!   (`Replay` / `Conflict` / `Capacity` / `FreshInsert`) → 算 wire
//!   `deadline_ns` 和 `stamp_ns` → 映射 wire `RpcRole` → build wire
//!   `ClearEStopRequest` bytes → forward via `rr_io_thread` →
//!   `tokio::time::timeout` 等 oneshot reply → decode wire
//!   `ClearEStopResponse` → 校验 response `request_id` 跟当前 request
//!   一致 → `rpc_status_to_tonic_code` 显式 `match` → `Ok` 路径 commit
//!   cache 加返 `Ack`；非 `Ok` drop `InsertHandle` 不缓存。
//! - SIGINT / SIGTERM 触发 `tokio::sync::watch` shutdown 信号 → tonic
//!   `serve_with_incoming_shutdown` graceful stop；tonic 退出后再
//!   `rr_io_thread` stop / join（reviewer A1-3 (2/3-b2-1) 关键边界）。
//!
//! ### CLI
//!
//! ```text
//! robot_rpc_gateway --robot-id <ID> \
//!                   --rpc-roles-toml <PATH> \
//!                   --listen-addr 127.0.0.1:50051 \
//!                   --idempotency-ttl-secs 300 \
//!                   --idempotency-max-entries 4096 \
//!                   --rr-timeout-grace-ms 500
//! ```
//!
//! ### 退出码
//!
//! - `0`：graceful shutdown（SIGINT/SIGTERM 触发后 tonic server 干净退出）。
//! - `1`：tonic server 启动失败 / `rr_io_thread` spawn 失败 / 运行期错。
//! - `2`：CLI 参数解析失败 / `--listen-addr` 非 loopback / `--rpc-roles-toml`
//!   加载失败。

#![cfg(target_os = "linux")]
// Bin doc 块中文 identifier-like 字串（ClearEStop / `RpcRole` /
// `IdempotencyCache` / `RobotControl` / `Iceoryx2RrClient` 等）跟 lib
// `robot-rpc` 一致走宽松 doc-markdown。
#![allow(clippy::doc_markdown)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use robot_core::time::SystemClock;
use robot_rpc::auth::load_role_table;
use robot_rpc::gateway::{
    ConfigError, GatewayConfig, GatewayError, LoopbackBindError, run_gateway_with_rr_io,
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
    rpc_roles_toml: PathBuf,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut robot_id: Option<String> = None;
        let mut listen_addr: SocketAddr = "127.0.0.1:50051".parse().expect("loopback default");
        let mut idempotency_ttl_secs: u64 = 300;
        let mut idempotency_max_entries: usize = 4096;
        let mut rr_timeout_grace_ms: u64 = 500;
        let mut rpc_roles_toml: Option<PathBuf> = None;

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
                "--rpc-roles-toml" => {
                    i += 1;
                    let val = raw.get(i).ok_or("--rpc-roles-toml requires value")?;
                    rpc_roles_toml = Some(PathBuf::from(val));
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
        let rpc_roles_toml = rpc_roles_toml.ok_or("--rpc-roles-toml is required")?;

        Ok(Self {
            robot_id,
            listen_addr,
            idempotency_ttl_secs,
            idempotency_max_entries,
            rr_timeout_grace_ms,
            rpc_roles_toml,
        })
    }
}

fn print_help() {
    eprintln!(
        "robot_rpc_gateway —— v0.3 A1-3 (2/3-b2-2) ClearEStop control-plane gateway

USAGE:
  robot_rpc_gateway --robot-id <ID> --rpc-roles-toml <PATH> [OPTIONS]

OPTIONS:
  --robot-id <ID>                  机器人标识符（必填；用于 iceoryx2 RR
                                   topic /<robot_id>/control/clear_estop）。
  --rpc-roles-toml <PATH>          rpc roles TOML 文件路径（必填；fake token
                                   → role 表 + role → capability 映射）。
                                   `clear_estop` capability **仅** maintainer
                                   / admin（reviewer A1-1 拍板硬边界；load
                                   时 check_clear_estop_invariant 守门）。
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
  1  tonic server 启动失败 / rr_io_thread spawn 失败 / 运行期错
  2  CLI 参数解析失败 / listen-addr 非 loopback / rpc-roles-toml 加载失败
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

    // ─── 加载 rpc roles TOML（fail-fast：load_role_table 内部已 check
    //     `clear_estop` capability 仅 maintainer / admin） ───
    let role_toml_text = match std::fs::read_to_string(&args.rpc_roles_toml) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(
                path = %args.rpc_roles_toml.display(),
                error = ?e,
                "fatal: --rpc-roles-toml read failed"
            );
            std::process::exit(2);
        }
    };
    let role_table = match load_role_table(&role_toml_text) {
        Ok(t) => Arc::new(t),
        Err(e) => {
            tracing::error!(
                path = %args.rpc_roles_toml.display(),
                error = ?e,
                "fatal: --rpc-roles-toml load failed (含 check_clear_estop_invariant)"
            );
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
    // async API；触发后 set shutdown_tx → run_gateway_with_rr_io 通过
    // shutdown_rx 收到 graceful stop 信号。
    spawn_signal_handler(shutdown_tx);

    let clock = Arc::new(SystemClock);

    match run_gateway_with_rr_io(cfg, role_table, clock, shutdown_rx, None).await {
        Ok(()) => {
            tracing::info!("robot_rpc_gateway: clean shutdown");
        }
        Err(GatewayError::Config(ConfigError::LoopbackBind(LoopbackBindError::NonLoopback(
            addr,
        )))) => {
            tracing::error!(
                %addr,
                "fatal: --listen-addr must be loopback (127.0.0.0/8 or ::1); v0.3 \
                 fake-token + plain TCP 阶段禁绑外网"
            );
            std::process::exit(2);
        }
        Err(GatewayError::Config(e)) => {
            tracing::error!(error = ?e, "fatal: gateway config validation failed");
            std::process::exit(2);
        }
        Err(GatewayError::Bind { addr, source }) => {
            tracing::error!(
                %addr,
                error = ?source,
                "fatal: TcpListener::bind failed (端口占用 / 权限不足等)"
            );
            std::process::exit(1);
        }
        Err(GatewayError::Tonic(e)) => {
            tracing::error!(error = ?e, "fatal: tonic transport failed");
            std::process::exit(1);
        }
        Err(GatewayError::RrIoSpawn(e)) => {
            tracing::error!(error = ?e, "fatal: rr_io_thread spawn failed");
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
