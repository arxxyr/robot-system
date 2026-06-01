//! async-lane 子命令 dispatch + 各 gateway RPC handler。
//!
//! `bin/robotctl.rs` 入口分流（Q-CLI2(b)）只把 [`is_dispatchable_async`]
//! 命中的名字交给 [`block_on_async`](crate::block_on_async) → [`run_async`]；
//! 其余（既有 sync 3 子命令 + 未落地 async 名 + 未知名）走既有手写 parser，
//! 行为 0 改动。
//!
//! 各 handler 的纯逻辑（gate / meta 构造 / 输出渲染）落在
//! [`crate::validate`] / [`crate::meta`] / [`crate::json_out`] 并已 lib unit；
//! 本模块的 handler 只做 I/O 编排（连接 + RPC + 错误码透传），由
//! `robotctl_smoke` cross-process e2e 覆盖（(5/5)）。

use std::future::Future;
use std::time::Duration;

use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{Ack, ClearEStopCommand, ClearErrorCommand, CommandMeta};
use tonic::transport::Channel;

use crate::cli_args::{
    ArgError, GatewayArgs, parse_gateway_args, reject_confirmation, reject_positional,
};
use crate::exit_code::{code_label, map_tonic_code_to_exit};
use crate::gateway_client::{connect_gateway, resolve_gateway_addr};
use crate::json_out::{AckOutput, print_output};
use crate::meta::{MetaInputs, build_command_meta, default_cli_source, resolve_token};
use crate::validate::{check_clear_estop_gates, check_write_rpc_gates};

/// CLI 参数错 / safety contract 早拒（AC-5）。
const EXIT_CLI_ARGS: u8 = 2;
/// gateway 连接失败（AC-5：connect refused / 未起 / 建链超时）。
const EXIT_CONNECT: u8 = 5;
/// client-side RPC deadline 到（AC-5：`DeadlineExceeded` / `--timeout-ms` 到）。
const EXIT_TIMEOUT: u8 = 6;
/// 建链上限（**仅** connect；单 RPC deadline 由 [`complete_ack_rpc`] 的
/// `tokio::time::timeout` 独占，超时稳定 exit 6）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// 已落地、可经 async lane dispatch 的 gateway 子命令（随 impl commit 增长）。
/// 候选 A (2/5) = `clear-estop` + `clear-error`。
///
/// `main()` 只把这些名字分流进 tokio runtime；未落地的 async-lane 名
/// （`get-task-status` / `cancel-task` / `stream-events` / `invoke`）继续走
/// `bin/robotctl.rs` 既有手写 parser → "unknown subcommand" exit 2（不可达
/// / 不 panic，沿 (1/5) Plan round 1 P1-3 同款保证）。
#[must_use]
pub fn is_dispatchable_async(name: &str) -> bool {
    matches!(name, "clear-estop" | "clear-error")
}

/// dispatch 一个已落地的 async-lane 子命令到对应 handler，返回 CLI exit code。
/// `subcommand` = 顶层子命令名；`raw` = 其后全部 arg。
pub async fn run_async(subcommand: &str, raw: &[String]) -> u8 {
    match subcommand {
        "clear-estop" => run_clear_estop(raw).await,
        "clear-error" => run_clear_error(raw).await,
        // is_dispatchable_async 已在 main 入口 gate；防御性兜底（不 panic）。
        other => {
            eprintln!("error: unknown subcommand: {other}");
            EXIT_CLI_ARGS
        }
    }
}

const CLEAR_ESTOP_USAGE: &str = "\
robotctl clear-estop —— 清除已持久化的 EStop latch（maintainer+；AC-3 安全合约）

USAGE:
  robotctl clear-estop --confirmation CONFIRM_CLEAR_ESTOP --audit-reason <STR> [OPTIONS]

REQUIRED:
  --confirmation CONFIRM_CLEAR_ESTOP   精确字面值（任何变体 / 前后空白拒）
  --audit-reason <STR>                 非空；写入 gateway audit log
  --token <TOKEN>                      maintainer+ token（或 env ROBOT_TOKEN）

OPTIONS:
  --gateway-addr <HOST:PORT>   默认 127.0.0.1:50051（或 env ROBOT_GATEWAY_ADDR）
  --source <STR>               默认 cli.<USER>
  --idempotency-key <STR>      默认 cli-<request_id>
  --timeout-ms <MS>            默认 2000
  --dry-run                    daemon 端识别但不真清 latch
  --json                       stdout 输出 JSON";

const CLEAR_ERROR_USAGE: &str = "\
robotctl clear-error —— 清 coordinator Degraded 状态（maintainer+；仅内存，不碰 EStop latch）

USAGE:
  robotctl clear-error --audit-reason <STR> [OPTIONS]

REQUIRED:
  --audit-reason <STR>         非空；写入 gateway audit log
  --token <TOKEN>              maintainer+ token（或 env ROBOT_TOKEN）

OPTIONS:
  --gateway-addr <HOST:PORT>   默认 127.0.0.1:50051（或 env ROBOT_GATEWAY_ADDR）
  --source <STR>               默认 cli.<USER>
  --idempotency-key <STR>      默认 cli-<request_id>
  --timeout-ms <MS>            默认 2000
  --dry-run                    回传 dry_run（语义同 clear-estop）
  --json                       stdout 输出 JSON";

/// `clear-estop`（AC-3 四道闸 + `ClearEStop` RPC）。
async fn run_clear_estop(raw: &[String]) -> u8 {
    let args = match parse_gateway_args(raw) {
        Ok(a) => a,
        Err(ArgError::Help) => {
            eprintln!("{CLEAR_ESTOP_USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_CLI_ARGS;
        }
    };
    if let Err(e) = reject_positional("clear-estop", &args) {
        eprintln!("error: {e}");
        return EXIT_CLI_ARGS;
    }

    // 四道闸（token → confirmation → audit-reason；AC-3）—— 早拒 + 不发 RPC。
    let env_token = std::env::var("ROBOT_TOKEN").ok();
    let resolved = resolve_token(args.token.as_deref(), env_token.as_deref());
    if let Err(e) = check_clear_estop_gates(
        resolved.as_deref().ok(),
        args.confirmation.as_deref(),
        args.audit_reason.as_deref(),
    ) {
        eprintln!("error: {e}");
        return EXIT_CLI_ARGS;
    }
    let token = resolved.expect("gate ensured token resolvable");

    let meta = build_meta(&args, &token);
    maybe_emit_idempotency_info(&args, &meta);

    let mut client = match connect(&args).await {
        Ok(c) => c,
        Err(code) => return code,
    };
    let cmd = ClearEStopCommand {
        meta: Some(meta),
        confirmation: args.confirmation.clone().unwrap_or_default(),
    };
    let rpc_timeout = Duration::from_millis(args.timeout_ms);
    complete_ack_rpc(client.clear_e_stop(cmd), rpc_timeout, args.json).await
}

/// `clear-error`（token + audit-reason gate + `ClearError` RPC）。
async fn run_clear_error(raw: &[String]) -> u8 {
    let args = match parse_gateway_args(raw) {
        Ok(a) => a,
        Err(ArgError::Help) => {
            eprintln!("{CLEAR_ERROR_USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_CLI_ARGS;
        }
    };
    // clear-error 不接受 positional，也不接受 clear-estop 专属的
    // --confirmation（阻断 1：不静默吞）。
    if let Err(e) = reject_positional("clear-error", &args)
        .and_then(|()| reject_confirmation("clear-error", &args))
    {
        eprintln!("error: {e}");
        return EXIT_CLI_ARGS;
    }

    let env_token = std::env::var("ROBOT_TOKEN").ok();
    let resolved = resolve_token(args.token.as_deref(), env_token.as_deref());
    if let Err(e) = check_write_rpc_gates(resolved.as_deref().ok(), args.audit_reason.as_deref()) {
        eprintln!("error: {e}");
        return EXIT_CLI_ARGS;
    }
    let token = resolved.expect("gate ensured token resolvable");

    let meta = build_meta(&args, &token);
    maybe_emit_idempotency_info(&args, &meta);

    let mut client = match connect(&args).await {
        Ok(c) => c,
        Err(code) => return code,
    };
    let cmd = ClearErrorCommand { meta: Some(meta) };
    let rpc_timeout = Duration::from_millis(args.timeout_ms);
    complete_ack_rpc(client.clear_error(cmd), rpc_timeout, args.json).await
}

// ============================================================================
// 共享编排 helper（clear-estop / clear-error 复用；后续写 RPC 继续复用）
// ============================================================================

/// 构造 `CommandMeta`（source 默认走 env `USER`/`USERNAME`；token 已 gate 过）。
fn build_meta(args: &GatewayArgs, token: &str) -> CommandMeta {
    let env_user = std::env::var("USER").ok();
    let env_username = std::env::var("USERNAME").ok();
    let source = args
        .source
        .clone()
        .unwrap_or_else(|| default_cli_source(env_user.as_deref(), env_username.as_deref()));
    build_command_meta(&MetaInputs {
        token,
        source: source.as_str(),
        idempotency_key: args.idempotency_key.as_deref(),
        timeout_ms: args.timeout_ms,
        dry_run: args.dry_run,
        audit_reason: args.audit_reason.as_deref().unwrap_or_default(),
    })
}

/// `idempotency_key` 自动生成时（用户未传 / 传空）打 AC-6 Q5(b) 提示行。
fn maybe_emit_idempotency_info(args: &GatewayArgs, meta: &CommandMeta) {
    if args.idempotency_key.as_deref().is_none_or(str::is_empty) {
        eprintln!(
            "info: auto-generated idempotency_key={}; for retry pass --idempotency-key {}",
            meta.idempotency_key, meta.idempotency_key
        );
    }
}

/// 连接 gateway（地址走 `--gateway-addr` > env > 默认；失败 → exit 5）。
async fn connect(args: &GatewayArgs) -> Result<RobotControlClient<Channel>, u8> {
    let env_addr = std::env::var("ROBOT_GATEWAY_ADDR").ok();
    let addr = resolve_gateway_addr(args.gateway_addr.as_deref(), env_addr.as_deref());
    connect_gateway(&addr, CONNECT_TIMEOUT).await.map_err(|e| {
        eprintln!("error: {e}");
        EXIT_CONNECT
    })
}

/// Ok 响应渲染（AC-7：human / `--json`）→ exit 0。
fn render_ack(ack: Ack, json: bool) -> u8 {
    let out = AckOutput {
        cleared_at_ns: ack.cleared_at_ns,
        dry_run: ack.dry_run,
        request_id: ack.request_id,
    };
    print_output(&out, json);
    0
}

/// server 端 `tonic::Status` 透传（AC-5：stderr `error: <CODE>: <message>`
/// + exit code 按映射表）。
fn render_status_err(status: &tonic::Status) -> u8 {
    eprintln!("error: {}: {}", code_label(status.code()), status.message());
    map_tonic_code_to_exit(status.code())
}

/// 在 client-side deadline 内 await 一个返 `Ack` 的 RPC future（阻断 2）。
///
/// `--timeout-ms` 经此**独占**请求 deadline 语义：超时 → 稳定
/// `error: DEADLINE_EXCEEDED: ...` + **exit 6**（不依赖 tonic endpoint
/// timeout 那个可能落 `Cancelled`(→7) 的不稳定 code）；server `Status` →
/// 错误码透传；Ok → 渲染 `Ack` exit 0。
async fn complete_ack_rpc<F>(rpc: F, timeout: Duration, json: bool) -> u8
where
    F: Future<Output = Result<tonic::Response<Ack>, tonic::Status>>,
{
    match tokio::time::timeout(timeout, rpc).await {
        Err(_elapsed) => {
            eprintln!(
                "error: {}: client timeout after {} ms",
                code_label(tonic::Code::DeadlineExceeded),
                timeout.as_millis()
            );
            EXIT_TIMEOUT
        }
        Ok(Ok(resp)) => render_ack(resp.into_inner(), json),
        Ok(Err(status)) => render_status_err(&status),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use robot_rpc::proto::Ack;

    use super::{EXIT_TIMEOUT, complete_ack_rpc, is_dispatchable_async};
    use crate::{Lane, subcommand_lane};

    #[test]
    fn dispatchable_set_is_clear_estop_and_clear_error() {
        assert!(is_dispatchable_async("clear-estop"));
        assert!(is_dispatchable_async("clear-error"));
    }

    /// 不变量：landed ⊆ async-lane（dispatch 的名字必在 async lane）。
    #[test]
    fn dispatchable_implies_async_lane() {
        for n in ["clear-estop", "clear-error"] {
            assert!(is_dispatchable_async(n), "{n}");
            assert_eq!(subcommand_lane(n), Some(Lane::Async), "{n}");
        }
    }

    /// (2/5) 边界：async lane 中尚未落地的名字**不**可 dispatch（main 落
    /// legacy → "unknown subcommand" exit 2，沿 P1-3）。
    #[test]
    fn not_yet_landed_async_names_not_dispatchable() {
        for n in ["get-task-status", "cancel-task", "stream-events", "invoke"] {
            assert!(!is_dispatchable_async(n), "{n}");
            assert_eq!(subcommand_lane(n), Some(Lane::Async), "{n}");
        }
    }

    #[test]
    fn sync_and_unknown_not_dispatchable() {
        for n in ["status", "watch", "estop", "bogus", "scripted", "ik", ""] {
            assert!(!is_dispatchable_async(n), "{n}");
        }
    }

    // ----- 阻断 2：client-side deadline → exit 6（complete_ack_rpc helper）-----

    fn ack() -> Ack {
        Ack {
            request_id: "r".to_owned(),
            cleared_at_ns: 1,
            dry_run: false,
        }
    }

    #[tokio::test]
    async fn complete_ack_rpc_ok_returns_zero() {
        let rpc = async { Ok(tonic::Response::new(ack())) };
        assert_eq!(complete_ack_rpc(rpc, Duration::from_secs(5), true).await, 0);
    }

    #[tokio::test]
    async fn complete_ack_rpc_status_err_maps_exit_code() {
        let rpc = async {
            Err::<tonic::Response<Ack>, _>(tonic::Status::new(
                tonic::Code::PermissionDenied,
                "no cap",
            ))
        };
        // PermissionDenied → exit 3（AC-5）。
        assert_eq!(
            complete_ack_rpc(rpc, Duration::from_secs(5), false).await,
            3
        );
    }

    /// pending future 永不完成 → 超时必赢 → 稳定 exit 6（确定性，不依赖
    /// tonic endpoint timeout 的不稳定 code）。
    #[tokio::test]
    async fn complete_ack_rpc_timeout_returns_six() {
        let rpc = std::future::pending::<Result<tonic::Response<Ack>, tonic::Status>>();
        assert_eq!(
            complete_ack_rpc(rpc, Duration::from_millis(10), false).await,
            EXIT_TIMEOUT
        );
    }
}
