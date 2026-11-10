//! `robot_rpc_gateway` —— A1-3 (2/3-c) ClearEStop control-plane bin（含
//! 完整 graceful shutdown 语义）。
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
//! - SIGINT / SIGTERM 触发 `tokio::sync::watch` shutdown 信号；A1-3 (2/3-c)
//!   三段式 graceful：(a) handler 在 cache.decide 之后、forward 之前 check
//!   shutdown，true 直接返 `Unavailable("gateway is shutting down; ...")`；
//!   (b) 已 forward 的 in-flight 请求由 tonic
//!   `serve_with_incoming_shutdown` 自然让其跑完（含 oneshot await + RR
//!   reply）；(c) tonic 退出**之后**才 `rr_io_thread` stop / join（reviewer
//!   A1-3 (2/3-b2-1) 关键边界，避免 handler 还在等 RR 时 I/O 被掐）。
//!
//! ### CLI
//!
//! ```text
//! robot_rpc_gateway --robot-id <ID> \
//!                   --rpc-roles-toml <PATH> \
//!                   [--config <PATH>] \           # v0.5 候选 B：state 订阅池来源
//!                   --listen-addr 127.0.0.1:50051 \
//!                   --idempotency-ttl-secs 300 \
//!                   --idempotency-max-entries 4096 \
//!                   --rr-timeout-grace-ms 500
//! ```
//!
//! ### 退出码
//!
//! - `0`：graceful shutdown（SIGINT/SIGTERM 触发后 tonic server 干净退出）。
//! - `1`：tonic server 启动失败 / `rr_io_thread` spawn 失败 / `state_pool`
//!   spawn 失败 / 运行期错。
//! - `2`：CLI 参数解析失败 / `--listen-addr` 非 loopback / `--rpc-roles-toml`
//!   加载失败 / `--config` 加载解析失败 / `[robot].id` != `--robot-id`。

#![cfg(target_os = "linux")]
// Bin doc 块中文 identifier-like 字串（ClearEStop / `RpcRole` /
// `IdempotencyCache` / `RobotControl` / `Iceoryx2RrClient` 等）跟 lib
// `robot-rpc` 一致走宽松 doc-markdown。
#![allow(clippy::doc_markdown)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use robot_core::time::SystemClock;
use robot_rpc::auth::load_role_table;
use robot_rpc::gateway::{
    ConfigError, GatewayConfig, GatewayError, GatewayRun, LoopbackBindError, finish_gateway_owned,
    run_gateway_with_rr_io,
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
    /// **v0.5 候选 B (2/4)**：`--config <PATH>` robot config TOML（可选）。
    /// present → minimal local 解析 `[[hardware]].id` 喂进 state 订阅池（D10）；
    /// 缺省 → 空 state 池（GetState/StreamState **非空** hardware_id 请求返
    /// `NotFound`，缺/空 hardware_id 仍先 `InvalidArgument`；B4 既有调用 0 破）。
    config: Option<PathBuf>,
}

/// **v0.5 候选 B (2/4)**：`--config` 的**宽松** minimal 视图——只取 `[robot].id`
/// 与 `[[hardware]].id` 两项。serde 默认忽略未知字段，故同一份 robot config TOML
/// （s1_daemon 已吃的那份）可直接喂入，其余字段（adapter / joints / s1.profile
/// 等）全忽略。**不** dep robot-s0、**不**抽共享 crate（D10 / 守 S0/S1 0 改）。
#[derive(Debug, serde::Deserialize)]
struct GatewayRobotConfig {
    robot: RobotIdSection,
    #[serde(default)]
    hardware: Vec<HardwareIdSection>,
}

#[derive(Debug, serde::Deserialize)]
struct RobotIdSection {
    id: String,
}

#[derive(Debug, serde::Deserialize)]
struct HardwareIdSection {
    id: String,
}

/// 读 `--config` → 解析 → 校验 `[robot].id == --robot-id` → 返
/// `[[hardware]].id` 列表（state 订阅池来源，D10）。
///
/// 全部错误（读失败 / TOML 语法错 / 缺 `[robot].id` / id 错配）→ `Err(String)`
/// → main 退出码 **2**（config 类，沿 `--rpc-roles-toml` load fail 同路径）。
/// `[[hardware]]` 空 → 返空列表（合法，D-P1）。
fn load_state_hardware_ids(config_path: &Path, robot_id: &str) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(config_path)
        .map_err(|e| format!("--config read failed ({}): {e}", config_path.display()))?;
    parse_state_hardware_ids(&text, robot_id)
        .map_err(|e| format!("--config ({}): {e}", config_path.display()))
}

/// 纯解析（无文件 I/O，可单测）：TOML text + robot_id → `[[hardware]].id` 列表。
///
/// 错误：TOML 语法错 / 缺 `[robot].id` / `[robot].id != robot_id`。`[[hardware]]`
/// 空 → 空列表（合法，D-P1）。
fn parse_state_hardware_ids(toml_text: &str, robot_id: &str) -> Result<Vec<String>, String> {
    let parsed: GatewayRobotConfig =
        toml::from_str(toml_text).map_err(|e| format!("TOML parse failed: {e}"))?;
    if parsed.robot.id != robot_id {
        return Err(format!(
            "[robot].id '{}' != --robot-id '{}' (config/flag mismatch; 会订阅错 topic)",
            parsed.robot.id, robot_id
        ));
    }
    Ok(parsed.hardware.into_iter().map(|h| h.id).collect())
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut robot_id: Option<String> = None;
        let mut listen_addr: SocketAddr = "127.0.0.1:50051".parse().expect("loopback default");
        let mut idempotency_ttl_secs: u64 = 300;
        let mut idempotency_max_entries: usize = 4096;
        let mut rr_timeout_grace_ms: u64 = 500;
        let mut rpc_roles_toml: Option<PathBuf> = None;
        let mut config: Option<PathBuf> = None;

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
                "--config" => {
                    i += 1;
                    let val = raw.get(i).ok_or("--config requires value")?;
                    config = Some(PathBuf::from(val));
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
            config,
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
  --config <PATH>                  robot config TOML（可选；v0.5 候选 B）。present
                                   → minimal 解析 [[hardware]].id 建常驻 S0 state
                                   订阅池（GetState/StreamState 读源）。[robot].id
                                   必须 == --robot-id（错配拒启动 exit 2）。缺省
                                   → 空 state 池（GetState/StreamState 非空
                                   hardware_id 请求返 NotFound；B4 既有调用 0 破）。
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

SYSTEMD (v0.5 候选 E 段(8), unit Type=notify):
  NOTIFY_SOCKET / WATCHDOG_USEC 由 systemd 注入：bind 后再经 loopback 探针
  （GetState(\"\") → InvalidArgument，500 ms 一次，首成上限 10 s）才 READY=1；
  liveness-gated WATCHDOG=1（6 rr_io / estop_pub / 每 hardware state_pool /
  tonic 探针任一停滞即停喂）；退出先 STOPPING=1。未设置 = no-op。

退出码:
  0  graceful shutdown
  1  tonic server 启动失败 / rr_io_thread spawn 失败 / state_pool spawn 失败 / 运行期错
     / loopback 探针 10 s 内未首成（READY 未发）/ server task 异常结束（段(8)）
  2  CLI 参数解析失败 / listen-addr 非 loopback / rpc-roles-toml 加载失败 / --config 加载解析失败 / [robot].id != --robot-id
"
    );
}

// ============================================================================
// main
// ============================================================================

#[allow(clippy::too_many_lines)] // main 含 8-variant GatewayError exit-code 大 match
#[tokio::main(flavor = "multi_thread")]
async fn main() {
    robot_logging::set_process_name("robot-gateway");
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

    // ─── v0.5 候选 B (2/4)：--config → state 订阅池 hardware id 列表（D10）。
    //     用 &args.robot_id（cfg 构造后 robot_id 被 move）。 ───
    let state_hardware_ids = if let Some(path) = &args.config {
        match load_state_hardware_ids(path, &args.robot_id) {
            Ok(ids) => {
                tracing::info!(
                    config = %path.display(),
                    hardware_count = ids.len(),
                    "loaded state subscription pool hardware ids from --config"
                );
                ids
            }
            Err(msg) => {
                tracing::error!(error = %msg, "fatal: --config load failed");
                std::process::exit(2);
            }
        }
    } else {
        tracing::info!(
            "no --config; state subscription pool empty (GetState/StreamState \
             非空 hardware_id 请求返 NotFound)"
        );
        Vec::new()
    };

    let cfg = GatewayConfig {
        listen_addr: args.listen_addr,
        robot_id: args.robot_id,
        idempotency_ttl_secs: args.idempotency_ttl_secs,
        idempotency_max_entries: args.idempotency_max_entries,
        rr_timeout_grace_ms: args.rr_timeout_grace_ms,
        state_hardware_ids,
    };

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // 注册 SIGINT / SIGTERM handler。tokio::signal::unix 是 Send-friendly
    // async API；触发后 set shutdown_tx → run_gateway_with_rr_io 通过
    // shutdown_rx 收到 graceful stop 信号。
    let signal = spawn_signal_handler(shutdown_tx);

    let clock = Arc::new(SystemClock);

    let mut run = GatewayRun::new(cfg, role_table, clock, shutdown_rx, None);
    if let Err(original) = run.install_signal_handler(signal) {
        // new 构造的空槽不可拒绝；若不变量被破坏，仍把两份原 owner 持至实际失败退出。
        let _held_run = std::mem::ManuallyDrop::new(run);
        let _held_signal = std::mem::ManuallyDrop::new(original);
        std::process::exit(1);
    }
    run_gateway_with_rr_io(&mut run).await;
    let exit = loop {
        match run.into_exit() {
            Ok(exit) => break exit,
            Err(original) => {
                run = original;
                finish_gateway_owned(&mut run).await;
            }
        }
    };
    match exit.outcome() {
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
        }
        Err(GatewayError::Config(e)) => {
            tracing::error!(error = ?e, "fatal: gateway config validation failed");
        }
        Err(GatewayError::Bind { addr, source }) => {
            tracing::error!(
                %addr,
                error = ?source,
                "fatal: TcpListener::bind failed (端口占用 / 权限不足等)"
            );
        }
        Err(GatewayError::Tonic(e)) => {
            tracing::error!(error = ?e, "fatal: tonic transport failed");
        }
        Err(GatewayError::RrIoSpawn(e)) => {
            tracing::error!(error = ?e, "fatal: rr_io_thread spawn failed");
        }
        Err(GatewayError::StatePoolSpawn(e)) => {
            tracing::error!(error = ?e, "fatal: state_pool spawn failed");
        }
        // v0.5 候选 E 段(8)：READY 门槛 probe 超时 / server task 异常 → 沿
        // transport 族 exit 1（systemd Restart=on-failure 可拉起）。
        Err(GatewayError::ReadyProbeTimeout { waited_ms }) => {
            tracing::error!(
                waited_ms,
                "fatal: tonic loopback probe never succeeded before READY deadline"
            );
        }
        Err(GatewayError::ServerTask(msg)) => {
            tracing::error!(error = %msg, "fatal: tonic server task ended abnormally");
        }
    }
    exit.exit()
}

fn spawn_signal_handler(shutdown_tx: watch::Sender<bool>) -> tokio::task::JoinHandle<()> {
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
    })
}

// ============================================================================
// Tests（--config 纯解析；exit code 路径 + state_pool 由 (4/4) e2e 覆盖）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::parse_state_hardware_ids;

    const SAMPLE: &str = r#"
[robot]
id = "rid"
config_version = "0.3.0"

[[hardware]]
id = "arm"
adapter = "mock_hard_rt"
joints = ["j1", "j2"]
motor_ids = [1, 2]

[[hardware]]
id = "base"
adapter = "mock_omni3"
motor_ids = [7, 8, 9]

[s1.profile]
backends = ["mock_dwell"]
"#;

    #[test]
    fn parses_hardware_ids_in_order_ignoring_unknown_fields() {
        let ids = parse_state_hardware_ids(SAMPLE, "rid").unwrap();
        assert_eq!(ids, vec!["arm".to_string(), "base".to_string()]);
    }

    #[test]
    fn empty_hardware_is_ok_empty_list() {
        let toml = "[robot]\nid = \"rid\"\nconfig_version = \"0.3.0\"\n";
        let ids = parse_state_hardware_ids(toml, "rid").unwrap();
        assert!(ids.is_empty(), "expected no hardware ids, got {ids:?}");
    }

    #[test]
    fn robot_id_mismatch_is_err() {
        let err = parse_state_hardware_ids(SAMPLE, "other").unwrap_err();
        assert!(err.contains("!= --robot-id"), "got: {err}");
    }

    #[test]
    fn missing_robot_section_is_toml_err() {
        let toml = "[[hardware]]\nid = \"arm\"\nadapter = \"x\"\n";
        let err = parse_state_hardware_ids(toml, "rid").unwrap_err();
        assert!(err.contains("TOML parse failed"), "got: {err}");
    }

    #[test]
    fn invalid_toml_is_err() {
        let err = parse_state_hardware_ids("not = valid = toml", "rid").unwrap_err();
        assert!(err.contains("TOML parse failed"), "got: {err}");
    }
}
