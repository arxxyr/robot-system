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
use std::io::Write;
use std::time::Duration;

use robot_msg::schema_id::{SCHEMA_IK_PAYLOAD, SCHEMA_SCRIPTED_PAYLOAD};
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{
    Ack, ActionKindProto, CancelTaskCommand, ClearEStopCommand, ClearErrorCommand, CommandMeta,
    InvokeTaskCommand, TaskEvent, TaskHandleRef,
};
use tonic::Streaming;
use tonic::transport::Channel;

use crate::cli_args::{
    ArgError, GatewayArgs, expect_one_positional, parse_gateway_args, reject_confirmation,
    reject_dry_run, reject_invoke_flags_except, reject_positional,
};
use crate::exit_code::{code_label, map_tonic_code_to_exit};
use crate::gateway_client::{connect_gateway, resolve_gateway_addr};
use crate::json_out::{
    AckOutput, CancelAckOutput, InvokeOutput, TaskEventOutput, TaskStatusOutput, print_output,
};
use crate::meta::{
    MetaInputs, TOKEN_REQUIRED_MSG, build_command_meta, default_cli_source, resolve_token,
};
use crate::payload::{
    build_ik_payload, build_scripted_payload, parse_ik_target, parse_waypoints_json,
};
use crate::uuid_v7::new_request_id;
use crate::validate::{check_clear_estop_gates, check_write_rpc_gates, validate_action_id};

/// `invoke scripted` 允许的 invoke-family flag 白名单（其余 ik flag 拒）。
const INVOKE_SCRIPTED_ALLOWED: [&str; 2] = ["--waypoints-file", "--follow"];
/// `invoke ik` 允许的 invoke-family flag 白名单（`--waypoints-file` 拒）。
const INVOKE_IK_ALLOWED: [&str; 10] = [
    "--target-x",
    "--target-y",
    "--target-z",
    "--target-qx",
    "--target-qy",
    "--target-qz",
    "--target-qw",
    "--seed-joints",
    "--frame",
    "--follow",
];

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
/// 候选 A (3/5) = `clear-estop` / `clear-error` / `get-task-status` /
/// `cancel-task` / `stream-events`。
///
/// `main()` 只把这些名字分流进 tokio runtime；未落地的 async-lane 名
/// （`invoke`，待 (4/5)）继续走 `bin/robotctl.rs` 既有手写 parser →
/// "unknown subcommand" exit 2（不可达 / 不 panic，沿 (1/5) P1-3 同款保证）。
#[must_use]
pub fn is_dispatchable_async(name: &str) -> bool {
    matches!(
        name,
        "clear-estop"
            | "clear-error"
            | "get-task-status"
            | "cancel-task"
            | "stream-events"
            | "invoke"
    )
}

/// dispatch 一个已落地的 async-lane 子命令到对应 handler，返回 CLI exit code。
/// `subcommand` = 顶层子命令名；`raw` = 其后全部 arg。
pub async fn run_async(subcommand: &str, raw: &[String]) -> u8 {
    match subcommand {
        "clear-estop" => run_clear_estop(raw).await,
        "clear-error" => run_clear_error(raw).await,
        "get-task-status" => run_get_task_status(raw).await,
        "cancel-task" => run_cancel_task(raw).await,
        "stream-events" => run_stream_events(raw).await,
        "invoke" => run_invoke(raw).await,
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
  --json                       stdout 输出 JSON";

const GET_TASK_STATUS_USAGE: &str = "\
robotctl get-task-status <action-id> [OPTIONS]

读取单个 task 当前状态（EXECUTING / COMPLETED / FAILED / ...）。

REQUIRED:
  <action-id>                  UUIDv7 hex（invoke 返回的 action_id）
  --token <TOKEN>              token（或 env ROBOT_TOKEN；client-side 要求）

OPTIONS:
  --gateway-addr <HOST:PORT>   默认 127.0.0.1:50051（或 env ROBOT_GATEWAY_ADDR）
  --timeout-ms <MS>            默认 2000
  --json                       stdout 输出 JSON";

const CANCEL_TASK_USAGE: &str = "\
robotctl cancel-task <action-id> --audit-reason <STR> [OPTIONS]

协作式取消 task（返 Ack 不保证已停；终态走 stream-events）。

REQUIRED:
  <action-id>                  UUIDv7 hex
  --audit-reason <STR>         非空
  --token <TOKEN>              token（或 env ROBOT_TOKEN）

OPTIONS:
  --gateway-addr <HOST:PORT>   默认 127.0.0.1:50051（或 env ROBOT_GATEWAY_ADDR）
  --source <STR>               默认 cli.<USER>
  --idempotency-key <STR>      默认 cli-<request_id>
  --timeout-ms <MS>            默认 2000
  --json                       stdout 输出 JSON";

const STREAM_EVENTS_USAGE: &str = "\
robotctl stream-events <action-id> [OPTIONS]

server-streaming task 事件，每帧一行 stdout；终态自动关闭 exit 0；Ctrl+C
（SIGINT）提前退出 exit 0 + warning。

REQUIRED:
  <action-id>                  UUIDv7 hex
  --token <TOKEN>              token（或 env ROBOT_TOKEN）

OPTIONS:
  --gateway-addr <HOST:PORT>   默认 127.0.0.1:50051（或 env ROBOT_GATEWAY_ADDR）
  --timeout-ms <MS>            整流总时长上限（默认 2000；到期 exit 6）
  --json                       stdout 每帧一行 JSON（NDJSON）";

const INVOKE_USAGE: &str = "\
robotctl invoke <KIND> [OPTIONS]

提交一个 action。KIND ∈ {scripted, ik}：
  robotctl invoke scripted --waypoints-file <PATH> --audit-reason <STR> ...
  robotctl invoke ik --target-x <F> --target-y <F> --target-z <F> --audit-reason <STR> ...

各 KIND 详见 robotctl invoke <KIND> --help。";

const INVOKE_SCRIPTED_USAGE: &str = "\
robotctl invoke scripted --waypoints-file <PATH> --audit-reason <STR> [OPTIONS]

joint-space 线性插值 + waypoint dwell（kind=Scripted）。

REQUIRED:
  --waypoints-file <PATH>      JSON {\"waypoints\":[{\"positions\":[..],\"dwell_ms\":N,\"segment_duration_ms\":N},..]}
  --audit-reason <STR>         非空
  --token <TOKEN>              operator+ token（或 env ROBOT_TOKEN）

OPTIONS:
  --gateway-addr / --source / --idempotency-key / --timeout-ms / --json
  --follow                     提交后接 stream-events 跟到终态（默认 off，Q-CLI11(b)）";

const INVOKE_IK_USAGE: &str = "\
robotctl invoke ik --target-x <F> --target-y <F> --target-z <F> --audit-reason <STR> [OPTIONS]

逆运动学求解到目标位姿（kind=Ik）。

REQUIRED:
  --target-x/-y/-z <F>         目标位置（m，f64）
  --audit-reason <STR>         非空
  --token <TOKEN>              operator+ token（或 env ROBOT_TOKEN）

OPTIONS:
  --target-qx/-qy/-qz/-qw <F>  目标四元数（默认 0 0 0 1；CLI 不校验 normalization）
  --seed-joints <j0,j1,..>     IK seed（逗号列表；默认无）
  --frame <STR>                frame（默认空）
  --gateway-addr / --source / --idempotency-key / --timeout-ms / --json
  --follow                     提交后接 stream-events 跟到终态（默认 off，Q-CLI11(b)）";

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
    if let Err(e) = reject_positional("clear-estop", &args)
        .and_then(|()| reject_invoke_flags_except("clear-estop", &args, &[]))
    {
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
        .and_then(|()| reject_invoke_flags_except("clear-error", &args, &[]))
        .and_then(|()| reject_dry_run("clear-error", &args))
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

/// `get-task-status <action-id>`（只读 RPC；`TaskHandleRef`，无 `CommandMeta`）。
async fn run_get_task_status(raw: &[String]) -> u8 {
    let args = match parse_gateway_args(raw) {
        Ok(a) => a,
        Err(ArgError::Help) => {
            eprintln!("{GET_TASK_STATUS_USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_CLI_ARGS;
        }
    };
    if let Err(e) = reject_confirmation("get-task-status", &args)
        .and_then(|()| reject_invoke_flags_except("get-task-status", &args, &[]))
        .and_then(|()| reject_dry_run("get-task-status", &args))
    {
        eprintln!("error: {e}");
        return EXIT_CLI_ARGS;
    }
    let action_id = match read_rpc_action_id("get-task-status", &args) {
        Ok(id) => id,
        Err(code) => return code,
    };

    let mut client = match connect(&args).await {
        Ok(c) => c,
        Err(code) => return code,
    };
    let req = TaskHandleRef {
        request_id: new_request_id(),
        action_id,
    };
    let rpc_timeout = Duration::from_millis(args.timeout_ms);
    match with_deadline(client.get_task_status(req), rpc_timeout).await {
        None => EXIT_TIMEOUT,
        Some(Ok(resp)) => {
            print_output(&TaskStatusOutput::from_proto(&resp.into_inner()), args.json);
            0
        }
        Some(Err(status)) => render_status_err(&status),
    }
}

/// `cancel-task <action-id>`（写 RPC；`CommandMeta` + token + audit-reason）。
async fn run_cancel_task(raw: &[String]) -> u8 {
    let args = match parse_gateway_args(raw) {
        Ok(a) => a,
        Err(ArgError::Help) => {
            eprintln!("{CANCEL_TASK_USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_CLI_ARGS;
        }
    };
    if let Err(e) = reject_confirmation("cancel-task", &args)
        .and_then(|()| reject_invoke_flags_except("cancel-task", &args, &[]))
        .and_then(|()| reject_dry_run("cancel-task", &args))
    {
        eprintln!("error: {e}");
        return EXIT_CLI_ARGS;
    }
    let action_id = match expect_one_positional("cancel-task", &args) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_CLI_ARGS;
        }
    };
    if let Err(e) = validate_action_id(&action_id) {
        eprintln!("error: {e}");
        return EXIT_CLI_ARGS;
    }

    // 写 RPC：token + audit-reason 必填（AC-4）。
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
    let cmd = CancelTaskCommand {
        meta: Some(meta),
        action_id,
    };
    let rpc_timeout = Duration::from_millis(args.timeout_ms);
    match with_deadline(client.cancel_task(cmd), rpc_timeout).await {
        None => EXIT_TIMEOUT,
        Some(Ok(resp)) => {
            let ack = resp.into_inner();
            print_output(
                &CancelAckOutput {
                    cleared_at_ns: ack.cleared_at_ns,
                    request_id: ack.request_id,
                },
                args.json,
            );
            0
        }
        Some(Err(status)) => render_status_err(&status),
    }
}

/// `stream-events <action-id>`（server-streaming；SIGINT exit 0 + 整流 timeout
/// exit 6，AC-10）。
async fn run_stream_events(raw: &[String]) -> u8 {
    let args = match parse_gateway_args(raw) {
        Ok(a) => a,
        Err(ArgError::Help) => {
            eprintln!("{STREAM_EVENTS_USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_CLI_ARGS;
        }
    };
    if let Err(e) = reject_confirmation("stream-events", &args)
        .and_then(|()| reject_invoke_flags_except("stream-events", &args, &[]))
        .and_then(|()| reject_dry_run("stream-events", &args))
    {
        eprintln!("error: {e}");
        return EXIT_CLI_ARGS;
    }
    let action_id = match read_rpc_action_id("stream-events", &args) {
        Ok(id) => id,
        Err(code) => return code,
    };
    let client = match connect(&args).await {
        Ok(c) => c,
        Err(code) => return code,
    };
    let total = Duration::from_millis(args.timeout_ms);
    stream_to_terminal(client, action_id, total, args.json).await
}

/// `invoke` —— 顶层 token，按位置子类型 `scripted` / `ik` 再分发（(4/5)）。
async fn run_invoke(raw: &[String]) -> u8 {
    match raw.first().map(String::as_str) {
        Some("scripted") => run_invoke_scripted(&raw[1..]).await,
        Some("ik") => run_invoke_ik(&raw[1..]).await,
        Some("-h" | "--help") => {
            eprintln!("{INVOKE_USAGE}");
            0
        }
        Some(other) => {
            eprintln!("error: unknown invoke kind: {other} (expected 'scripted' or 'ik')");
            EXIT_CLI_ARGS
        }
        None => {
            eprintln!("error: invoke requires a kind ('scripted' or 'ik')");
            eprintln!("{INVOKE_USAGE}");
            EXIT_CLI_ARGS
        }
    }
}

/// `invoke scripted --waypoints-file <PATH>`（AC-8；写 RPC kind=Scripted）。
async fn run_invoke_scripted(raw: &[String]) -> u8 {
    let args = match parse_gateway_args(raw) {
        Ok(a) => a,
        Err(ArgError::Help) => {
            eprintln!("{INVOKE_SCRIPTED_USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_CLI_ARGS;
        }
    };
    if let Err(e) = reject_confirmation("invoke scripted", &args)
        .and_then(|()| {
            reject_invoke_flags_except("invoke scripted", &args, &INVOKE_SCRIPTED_ALLOWED)
        })
        .and_then(|()| reject_dry_run("invoke scripted", &args))
    {
        eprintln!("error: {e}");
        return EXIT_CLI_ARGS;
    }
    if !args.positional.is_empty() {
        eprintln!("error: invoke scripted takes no positional arguments");
        return EXIT_CLI_ARGS;
    }
    let Some(path) = args.waypoints_file.as_deref() else {
        eprintln!("error: --waypoints-file <PATH> is required for invoke scripted");
        return EXIT_CLI_ARGS;
    };
    // async 边界：用 tokio::fs（内部 spawn_blocking）读文件，不阻塞 worker。
    let json_text = match tokio::fs::read_to_string(path).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: cannot read --waypoints-file {path}: {e}");
            return EXIT_CLI_ARGS;
        }
    };
    let waypoints = match parse_waypoints_json(&json_text) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_CLI_ARGS;
        }
    };

    let env_token = std::env::var("ROBOT_TOKEN").ok();
    let resolved = resolve_token(args.token.as_deref(), env_token.as_deref());
    if let Err(e) = check_write_rpc_gates(resolved.as_deref().ok(), args.audit_reason.as_deref()) {
        eprintln!("error: {e}");
        return EXIT_CLI_ARGS;
    }
    let token = resolved.expect("gate ensured token resolvable");
    let meta = build_meta(&args, &token);
    maybe_emit_idempotency_info(&args, &meta);

    let cmd = InvokeTaskCommand {
        meta: Some(meta),
        kind: ActionKindProto::ActionKindScripted as i32,
        deadline_ns: 0,
        priority: 0,
        payload_schema_id: SCHEMA_SCRIPTED_PAYLOAD,
        payload: build_scripted_payload(&waypoints),
    };
    run_invoke_rpc(&args, cmd).await
}

/// `invoke ik --target-x/y/z ...`（AC-9；写 RPC kind=Ik）。
async fn run_invoke_ik(raw: &[String]) -> u8 {
    let args = match parse_gateway_args(raw) {
        Ok(a) => a,
        Err(ArgError::Help) => {
            eprintln!("{INVOKE_IK_USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_CLI_ARGS;
        }
    };
    if let Err(e) = reject_confirmation("invoke ik", &args)
        .and_then(|()| reject_invoke_flags_except("invoke ik", &args, &INVOKE_IK_ALLOWED))
        .and_then(|()| reject_dry_run("invoke ik", &args))
    {
        eprintln!("error: {e}");
        return EXIT_CLI_ARGS;
    }
    if !args.positional.is_empty() {
        eprintln!("error: invoke ik takes no positional arguments");
        return EXIT_CLI_ARGS;
    }
    let target = match parse_ik_target(
        args.target_x.as_deref(),
        args.target_y.as_deref(),
        args.target_z.as_deref(),
        args.target_qx.as_deref(),
        args.target_qy.as_deref(),
        args.target_qz.as_deref(),
        args.target_qw.as_deref(),
        args.seed_joints.as_deref(),
        args.frame.as_deref(),
    ) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_CLI_ARGS;
        }
    };

    let env_token = std::env::var("ROBOT_TOKEN").ok();
    let resolved = resolve_token(args.token.as_deref(), env_token.as_deref());
    if let Err(e) = check_write_rpc_gates(resolved.as_deref().ok(), args.audit_reason.as_deref()) {
        eprintln!("error: {e}");
        return EXIT_CLI_ARGS;
    }
    let token = resolved.expect("gate ensured token resolvable");
    let meta = build_meta(&args, &token);
    maybe_emit_idempotency_info(&args, &meta);

    let cmd = InvokeTaskCommand {
        meta: Some(meta),
        kind: ActionKindProto::ActionKindIk as i32,
        deadline_ns: 0,
        priority: 0,
        payload_schema_id: SCHEMA_IK_PAYLOAD,
        payload: build_ik_payload(&target),
    };
    run_invoke_rpc(&args, cmd).await
}

/// invoke scripted / ik 公共 RPC 收尾：`InvokeTask` → 打 `action_id` →
/// （`--follow`）接 stream-events 到终态（AC-7 / Q-CLI11(b)）。
async fn run_invoke_rpc(args: &GatewayArgs, cmd: InvokeTaskCommand) -> u8 {
    let mut client = match connect(args).await {
        Ok(c) => c,
        Err(code) => return code,
    };
    let rpc_timeout = Duration::from_millis(args.timeout_ms);
    let handle = match with_deadline(client.invoke_task(cmd), rpc_timeout).await {
        None => return EXIT_TIMEOUT,
        Some(Ok(resp)) => resp.into_inner(),
        Some(Err(status)) => return render_status_err(&status),
    };
    let action_id = handle.action_id.clone();
    print_output(
        &InvokeOutput {
            action_id: handle.action_id,
            request_id: handle.request_id,
        },
        args.json,
    );
    let _ = std::io::stdout().flush();
    if args.follow {
        // --follow：用同 client 接 stream-events 到终态（独立整流 deadline）。
        let total = Duration::from_millis(args.timeout_ms);
        stream_to_terminal(client, action_id, total, args.json).await
    } else {
        0
    }
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

/// read RPC（get-task-status / stream-events）公共早拒：恰好一个 action-id +
/// `UUIDv7` 校验（AC-11）+ token 可解析（AC-4 client-side gate）。
///
/// **token 不上 wire**：`TaskHandleRef` 无 auth 字段，v0.3 read RPC 服务端不
/// 强制 auth（详 `robot_rpc` `task_handlers::validate_task_handle_ref` doc，
/// 留 v0.5+ metadata 路径）；此处仅作 client-side 策略 gate（缺 → exit 2）。
fn read_rpc_action_id(subcommand: &str, args: &GatewayArgs) -> Result<String, u8> {
    let action_id = expect_one_positional(subcommand, args).map_err(|e| {
        eprintln!("error: {e}");
        EXIT_CLI_ARGS
    })?;
    validate_action_id(&action_id).map_err(|e| {
        eprintln!("error: {e}");
        EXIT_CLI_ARGS
    })?;
    let env_token = std::env::var("ROBOT_TOKEN").ok();
    if resolve_token(args.token.as_deref(), env_token.as_deref()).is_err() {
        eprintln!("error: {TOKEN_REQUIRED_MSG}");
        return Err(EXIT_CLI_ARGS);
    }
    Ok(action_id)
}

/// 消费 server-streaming：每帧一行 stdout（human / NDJSON）+ flush；终态帧
/// （COMPLETED / FAILED / CANCELLED）→ exit 0；server 关流（无更多帧）→ exit 0；
/// stream error → 错误码透传（AC-10）。
async fn consume_stream(mut stream: Streaming<TaskEvent>, json: bool) -> u8 {
    loop {
        match stream.message().await {
            Ok(Some(ev)) => {
                let out = TaskEventOutput::from_proto(&ev);
                print_output(&out, json);
                // 流式实时性：每帧后冲刷 stdout（readiness gate / 实时帧；
                // 不依赖 LineWriter 内部行为）。
                let _ = std::io::stdout().flush();
                if out.is_terminal() {
                    return 0;
                }
            }
            Ok(None) => return 0,
            Err(status) => return render_status_err(&status),
        }
    }
}

/// 开 `StreamTaskEvents` 流并消费到终态（`run_stream_events` + invoke
/// `--follow` 复用）。SIGINT → `warning` + exit 0；整流总时长 `total` 到期 →
/// exit 6（AC-10）。
async fn stream_to_terminal(
    mut client: RobotControlClient<Channel>,
    action_id: String,
    total: Duration,
    json: bool,
) -> u8 {
    let req = TaskHandleRef {
        request_id: new_request_id(),
        action_id,
    };
    let stream_work = async move {
        match client.stream_task_events(req).await {
            Ok(resp) => consume_stream(resp.into_inner(), json).await,
            Err(status) => render_status_err(&status),
        }
    };
    // AC-10：SIGINT 提前退 exit 0 + warning；整流总时长到期 exit 6。
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            eprintln!("warning: stream interrupted before terminal event");
            0
        }
        r = tokio::time::timeout(total, stream_work) => match r {
            Ok(code) => code,
            Err(_elapsed) => {
                eprintln!("error: stream timed out after {}ms", total.as_millis());
                EXIT_TIMEOUT
            }
        }
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

/// 在 client-side deadline 内 await 一个 RPC future（阻断 2 的统一实现）。
///
/// `--timeout-ms` 经此**独占**请求 deadline 语义：超时 → 稳定
/// `error: DEADLINE_EXCEEDED: ...` + 返 `None`（caller 返 [`EXIT_TIMEOUT`]；
/// 不依赖 tonic endpoint timeout 那个可能落 `Cancelled`(→7) 的不稳定 code）；
/// 否则返 `Some(rpc 结果)`。
async fn with_deadline<T, F>(
    rpc: F,
    timeout: Duration,
) -> Option<Result<tonic::Response<T>, tonic::Status>>
where
    F: Future<Output = Result<tonic::Response<T>, tonic::Status>>,
{
    match tokio::time::timeout(timeout, rpc).await {
        Ok(r) => Some(r),
        Err(_elapsed) => {
            eprintln!(
                "error: {}: client timeout after {} ms",
                code_label(tonic::Code::DeadlineExceeded),
                timeout.as_millis()
            );
            None
        }
    }
}

/// `clear-estop` / `clear-error` 收尾：deadline 内拿 `Ack` → 渲染 exit 0 /
/// 错误码透传 / 超时 exit 6。
async fn complete_ack_rpc<F>(rpc: F, timeout: Duration, json: bool) -> u8
where
    F: Future<Output = Result<tonic::Response<Ack>, tonic::Status>>,
{
    match with_deadline(rpc, timeout).await {
        None => EXIT_TIMEOUT,
        Some(Ok(resp)) => render_ack(resp.into_inner(), json),
        Some(Err(status)) => render_status_err(&status),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use robot_rpc::proto::Ack;

    use super::{EXIT_TIMEOUT, complete_ack_rpc, is_dispatchable_async};
    use crate::{Lane, subcommand_lane};

    /// (4/5) landed set = 全部 6 个 async-lane gateway 子命令（invoke 落地）。
    #[test]
    fn dispatchable_set_is_all_six_gateway_subcommands() {
        for n in [
            "clear-estop",
            "clear-error",
            "get-task-status",
            "cancel-task",
            "stream-events",
            "invoke",
        ] {
            assert!(is_dispatchable_async(n), "{n}");
        }
    }

    /// 不变量：dispatchable set == async-lane set（(4/5) 起全等）。
    #[test]
    fn dispatchable_equals_async_lane_set() {
        for n in [
            "clear-estop",
            "clear-error",
            "get-task-status",
            "cancel-task",
            "stream-events",
            "invoke",
        ] {
            assert!(is_dispatchable_async(n), "{n}");
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
