# v0.5 候选 A — operator CLI（`robotctl`）Pre-Research / CodeMap

> **Pre-Research / CodeMap**：现状摸底 + 已落地能力清单 + 待拍板项；
> **不**是 Spec / Plan / 实施计划。**不**直接进实现 / **不**预生成
> Spec 文档。
>
> 写于 2026-05-28，v0.5_entry handoff `9e327f3` 之后；reviewer 拍板
> "明确下一步只到 Pre-Research / CodeMap，不直接实现"。
>
> 下一步：reviewer 复审本 CodeMap → 拍板 Q1..Q15（详 §8）→ 写
> `team-swe/specs/v0_5_operator_cli.md` Spec round 1。
>
> 沿 [`v0_4_robot_kinematics_round2.md`](v0_4_robot_kinematics_round2.md)
> + [`v0_4_pubsub_cleanup.md`](v0_4_pubsub_cleanup.md) 同款"摸底事实 +
> 不在范围 + 已落地能力 + 待拍板项"模板。

---

## 0. 与 v0.5_entry 描述的关键修订

**v0.5_entry §2.1 / §3.2 / §4 / §6 / 开工前必读多处写"首次 land
`crates/robot-cli/` workspace member"** —— 摸底实测**不成立**：

- `crates/robot-cli/` 已存在：`Cargo.toml`（23 L）+ `src/lib.rs`（15 L）
  + `src/bin/robotctl.rs`（426 L）
- workspace 已注册：`Cargo.toml:12 "crates/robot-cli"` + `Cargo.toml:153
  robot-cli = { path = "crates/robot-cli" }`
- 当前 `robotctl` binary **已 land 3 个子命令**：`status` / `watch` /
  `estop`（v0.2 Part 2 起步版；任务追溯 `task-30 v9 / task #15` +
  `2026-04-25_1` handoff）
- 3 个子命令全走 **iceoryx2 直连 S0** 路径（`status` / `watch` 订阅
  `JointState` + `Heartbeat`；`estop` 直接 publish 一帧 `EStopEvent`
  到 `/<robot_id>/events/estop` 让 `s0_daemon::ipc_thread::poll_estop`
  drain → broadcast `Torque_Enable=0`），**完全不走** robot-rpc
  gateway / tonic gRPC

**候选 A 实际范围 = 给已有 `robotctl` binary 扩 tonic gRPC 子命令**
（沿 v0.5_entry 拍板的 7 个候选子命令复用 6 RPC），**不是**首次 land
crate。具体边界（含跟既有 3 个 iceoryx2 子命令的关系）走 §8 Q1 拍板。

---

## 1. `crates/robot-cli/` 现状

### 1.1 文件清单

| 文件 | LoC | 角色 |
|---|---|---|
| `Cargo.toml` | 23 | workspace 继承 + `[dependencies]` 6 条（robot-core / robot-logging / robot-msg / robot-transport / iceoryx2 / libc）+ `tracing` + `[lints] workspace = true`；**未启用** `robot-rpc/client` feature |
| `src/lib.rs` | 15 | 空 lib（仅文档 + `#![forbid(unused_must_use)]`；说明未来抽核心逻辑到 lib 方便单测）|
| `src/bin/robotctl.rs` | 426 | binary main + 3 子命令 |

### 1.2 现有 3 子命令

| 子命令 | 走通道 | 调用面 | 任务追溯 |
|---|---|---|---|
| `status` | iceoryx2 `LatestSubscriber<JointState>` + `LatestSubscriber<Heartbeat>` 一次性取 → stdout 打印摘要 → 退出 | `open_sub::<markers::JointState>` / `markers::Heartbeat` | v0.2 Part 2 |
| `watch` | 同 `status` 但每 `--interval-ms`（默认 100ms）刷新；Ctrl+C 退出（`SHUTDOWN: AtomicBool` + `shutdown_signal_handler` 直接 `libc::_exit(0)` 兜底 `iceoryx2 0.8.1` shutdown SIGSEGV）| 同上 + `signal::install_signal_handlers` | v0.2 Part 2 |
| `estop` | 直接 publish 一帧 `EStopEvent` fbs 到 `/<robot_id>/events/estop` → `s0_daemon::ipc_thread::poll_estop` drain → emergency `Torque_Enable=0` | `build_estop_event` + `open_pub::<markers::EStopEvent>` | task-30 v9 / task #15 |

### 1.3 现有 CLI 参数风格

```text
robotctl <subcommand> [OPTIONS]
options:
  --robot-id <ID>            必填（沿 robot-system 既有约定）
  --hardware-id <ID>         必填（watch / status）
  --interval-ms <MS>         默认 100（watch）
  -h | --help                打印帮助
```

`subcommand` parse 在 main 内手写（`match args[1].as_str()`；**未**用
`clap` / `structopt`），错误 fallback 打 `print_help` + exit 2。

### 1.4 不在 crate 范围的依赖（候选 A 期间需新增）

- **`robot-rpc` workspace dep + `client` feature**：当前 `Cargo.toml`
  未列；候选 A 必加（让 `RobotControlClient` 跟 `proto::*` request
  类型可见）
- **`tonic` 客户端 transport**：workspace 已有 `tonic = { workspace
  = true }`；client 端只需要 `tonic::transport::Endpoint` /
  `Channel`，不需 server 路径
- **`tokio` runtime**：当前 `robotctl` 是 sync binary（`fn main()` 无
  `#[tokio::main]`）；tonic client 要求 async runtime；候选 A 引入
  `tokio = { workspace = true, features = ["macros", "rt-multi-
  thread"] }` 是绕不开的（沿 `scripted_backend_smoke` /
  `ik_backend_smoke` 同款 `#[tokio::main(flavor = "multi_thread",
  worker_threads = 4)]`）—— **跟既有 3 个 iceoryx2 子命令的 sync
  路径冲突需 §8 Q2 拍板**
- **`flatbuffers` + `robot-msg::msg::{ScriptedPayload, IkPayload}`**：
  `invoke scripted` / `invoke ik` 子命令需构 fbs body bytes；workspace
  已有

---

## 2. `robot-rpc/client` feature 下 tonic client 可复用面

### 2.1 `client` feature 真实状态

`crates/robot-rpc/Cargo.toml`（v0.3 A1-3 (3/3-a) 拍板）：

```toml
# **A1-3 (3/3-a)** 拍板：默认**不**生成 tonic gRPC client stub（生产
# `robot_rpc_gateway` binary 是 server，dimos / Web UI 是 client，client
# 不在 robot-system workspace 内）。开启 `client` feature 让 build.rs 跑
# `tonic_build::configure().build_client(true)`，让 robot-tests 端的两进程
# e2e smoke 用 tonic client 调 server。
[features]
client = []
```

`build.rs` 按 feature 决定是否生成 client stub（`tonic_build::build_
client(true)`）。

### 2.2 启用后可用 import

```rust
use robot_rpc::proto::robot_control_client::RobotControlClient;
use robot_rpc::proto::{
    ActionKindProto, AuthContext, CommandMeta,
    ClearEStopCommand, Ack,
    InvokeTaskCommand, TaskHandle,
    CancelTaskCommand,
    TaskHandleRef, TaskStatus, TaskEvent, TaskEventKind, TaskState,
    ClearErrorCommand,
    ActionOutcomeProto, ActionFailureProto,
};
```

### 2.3 client 端连接 + 调用模板（沿 `scripted_backend_smoke.rs:932-946`）

```rust
let endpoint = Endpoint::try_from(format!("http://{addr}"))?
    .connect_timeout(timeout)
    .timeout(timeout);
let channel = endpoint.connect().await?;
let mut client = RobotControlClient::new(channel);

let req = tonic::Request::new(InvokeTaskCommand { ... });
let resp = client.invoke_task(req).await?.into_inner();  // resp = TaskHandle
```

server-streaming 路径（`StreamTaskEvents`）：

```rust
let stream_resp = client.stream_task_events(req).await?;
let mut stream = stream_resp.into_inner();
while let Some(event) = stream.message().await? { ... }
```

### 2.4 robot-rpc lib 端可复用 helper

- **`robot_rpc::auth::validate_request_id(s: &str) -> Result<Uuid,
  RequestIdError>`** 严格 UUIDv7 校验（拒空 / 非 v7 / 非 UUID）—— 客户
  端可直接复用做本地早拒，避免发到 gateway 才被拒
- **`robot_rpc::auth::Role` / `Capability` enum**（不需要导出给 CLI
  作权限校验 —— 服务器端会查 `rpc_roles.toml` 拒绝；但**展示**用可
  借助；详 §8 Q3）

---

## 3. B4 task RPC 6 method + 请求 / 响应字段

### 3.1 6 RPC 表

| RPC | Request | Response | 流类型 | 当前 role 限制 |
|---|---|---|---|---|
| `ClearEStop` | `ClearEStopCommand` | `Ack` | unary | maintainer+ |
| `InvokeTask` | `InvokeTaskCommand` | `TaskHandle` | unary | operator+ |
| `CancelTask` | `CancelTaskCommand` | `Ack` | unary | operator+ |
| `GetTaskStatus` | `TaskHandleRef` | `TaskStatus` | unary | viewer+（only `read_state`）|
| `StreamTaskEvents` | `TaskHandleRef` | `TaskEvent` stream | server-streaming | viewer+ |
| `ClearError` | `ClearErrorCommand` | `Ack` | unary | maintainer+ |

### 3.2 `CommandMeta`（5 个写 RPC 必带；2 个读 RPC `TaskHandleRef` 不带）

```protobuf
message CommandMeta {
    string      request_id      = 1;  // UUIDv7（gateway 强校验；非 v7 拒）
    string      source          = 2;  // "cli.loosqk" / "web.ui" 等；audit only
    AuthContext auth            = 3;  // { token, mtls_fingerprint }
    uint64      timeout_ms      = 4;  // client monotonic 相对超时；gateway 转 daemon deadline_ns
    string      idempotency_key = 5;  // robot-rpc 进程内 TTL cache key；retry 同 key
    bool        dry_run         = 6;  // daemon 端 action layer 真识别（不在 gateway 短路）
    string      audit_reason    = 7;  // human "谁/为什么调"
}

message AuthContext {
    string token            = 1;  // fake token；gateway 查 rpc_roles.toml [tokens]
    string mtls_fingerprint = 2;  // v0.5+ mTLS / OIDC 用；A1-1 仅占位
}
```

**关键契约**（沿 B4 + A1 共有）：

- `request_id` ≠ `idempotency_key`：前者每次 retry 改（audit），后者跨 retry
  稳定（dedup）；CLI 默认行为需拍板（详 §8 Q5）
- `fingerprint 排除字段`：`request_id` / `timeout_ms` / `idempotency_key`
  **不**进 idempotency cache 的 fingerprint（避免合法 retry 误走
  `Conflict`），其它 payload 字段必须严格匹配；CLI 同 `idempotency_key`
  + 不同 payload bytes → 直接 `Conflict`
- `auth.token`：**字符串字段在 body 内**，**不**走 tonic HTTP/2
  `Authorization` metadata header（沿 smoke binary 用法 +
  `scripted_backend_smoke.rs:1052 auth_token.to_string()` 实证）

### 3.3 各 RPC request 字段

```protobuf
message ClearEStopCommand {
    CommandMeta meta         = 1;
    string      confirmation = 2;  // 必须非空；具体格式 v0.3 实施时定，proto 不锁
}

message InvokeTaskCommand {
    CommandMeta     meta              = 1;
    ActionKindProto kind              = 2;  // typed enum；UNSPECIFIED / DWELL_MS / HOLD_POSE / SCRIPTED / IK
    uint64          deadline_ns       = 3;  // CLOCK_MONOTONIC_RAW；client 显式 或 由 timeout_ms 推
    uint32          priority          = 4;  // 单 action 序列化 B2 不参与决策
    uint32          payload_schema_id = 5;  // 0 = NO_PAYLOAD；其它对应 fbs body schema
    bytes           payload           = 6;  // FlatBuffers body bytes
}

message CancelTaskCommand {
    CommandMeta meta      = 1;
    string      action_id = 2;  // UUIDv7 hex string
}

message ClearErrorCommand {
    CommandMeta meta = 1;
}

message TaskHandleRef {  // GetTaskStatus / StreamTaskEvents 共用
    string request_id = 1;
    string action_id  = 2;
}
```

### 3.4 各 RPC response 字段

```protobuf
message Ack {
    string request_id    = 1;
    uint64 cleared_at_ns = 2;  // daemon 端 clock.now()；status != Ok 时 0
    bool   dry_run       = 3;  // echo request.dry_run
}

message TaskHandle {        // InvokeTask reply
    string request_id = 1;
    string action_id  = 2;
}

message TaskStatus {        // GetTaskStatus reply
    string             request_id = 1;
    string             action_id  = 2;
    TaskState          state      = 3;  // PENDING / EXECUTING / COMPLETED / FAILED / CANCELLED
    float              progress   = 4;  // [0.0, 1.0]；仅 EXECUTING 有意义
    ActionOutcomeProto outcome    = 5;  // 仅 COMPLETED 有意义；kind SUCCESS / PARTIAL_SUCCESS + warning
    ActionFailureProto failure    = 6;  // 仅 FAILED 有意义；kind 5 variant + reason
}

message TaskEvent {         // StreamTaskEvents 单帧
    string             request_id = 1;
    string             action_id  = 2;
    uint64             at_ns      = 3;
    TaskEventKind      event      = 4;  // UNSPECIFIED / STARTED / PROGRESS / COMPLETED / FAILED / CANCELLED
    float              progress   = 5;
    ActionOutcomeProto outcome    = 6;
    ActionFailureProto failure    = 7;
}
```

### 3.5 错误码契约

`ActionRpcStatus → tonic::Code` 显式 match（沿 wire.rs:340 + A1 + B4）：

| 来源 | tonic `Code` |
|---|---|
| Ok | n/a |
| `InvalidRequest` / `UnsupportedSchema` / `Backend Stop+gateway payload byte gate` | `InvalidArgument` |
| `NoBackendForKind` | `Unimplemented` |
| `BusyExecuting` / idempotency dup | `AlreadyExists` |
| `CoordinatorDegraded` / `BackendStartFailed::{Rejected,PayloadDecode,HardwareMismatch}` / EStop latched / 多种 precondition | `FailedPrecondition` |
| `ShuttingDown` | `Unavailable` |
| `BackendStartFailed` 兜底 / 其它内部 | `Internal` |
| 角色 / 权限不足 | `PermissionDenied` |
| `request_id` 非 UUIDv7 / `idempotency_key` 空 | `InvalidArgument`（gateway 入口拒）|

CLI 端必须做映射 → 人类可读 exit message + exit code（详 §8 Q6）。

---

## 4. smoke binary 里可复制的 tonic client helper（**只作模式参考，不直接复用文件**）

reviewer 候选 4 light Research 同款拍板"smoke 不抽公共 helper"沿用；
本节列**可借鉴的模式**，**不**承诺把 helper 抽到 `robot-cli` lib 或新
公共 crate（具体抽不抽走 §8 Q7 拍板）。

### 4.1 `uuidv7_compat()` 生成 UUIDv7 hex string

3 个 smoke binary 都有同款 ~30 行 inline 实现（`clear_estop_e2e.rs:
699-740` / `scripted_backend_smoke.rs:948-983` / `ik_backend_smoke.rs`
同款）。

**可借鉴模式**：纯函数；用 `std::time::SystemTime::now()` 拿 unix
ms + nanos；按 UUIDv7 layout 拼 16 字节 + format hex；不依赖 `uuid`
crate 或者 `now_v7()`（兼容旧 toolchain；本仓库 nightly 实际上是
有 `Uuid::now_v7()` 的，可直接用）

**风险**：候选 A 不应再 inline 第 4 处；建议复用 `uuid` crate
`Uuid::now_v7().hyphenated().to_string().to_lowercase()`（沿
`robot_s1::action::ActionId::new_v7` 同款用法；workspace 已有 `uuid =
{ workspace = true }`）—— 详 §8 Q7

### 4.2 `build_invoke_task_cmd(...)` 等 request builder

```rust
// scripted_backend_smoke.rs:1036-1067
#[allow(clippy::too_many_arguments)]
fn build_invoke_task_cmd(
    request_id: &str, idempotency_key: &str, source: &str, auth_token: &str,
    kind: ActionKindProto, timeout_ms: u64, audit_reason: &str,
    payload_schema_id: u32, payload: Vec<u8>,
) -> tonic::Request<InvokeTaskCommand> {
    let cmd = InvokeTaskCommand {
        meta: Some(CommandMeta {
            request_id: request_id.to_string(),
            source: source.to_string(),
            auth: Some(AuthContext { token: auth_token.to_string(),
                                     mtls_fingerprint: String::new() }),
            timeout_ms,
            idempotency_key: idempotency_key.to_string(),
            dry_run: false,
            audit_reason: audit_reason.to_string(),
        }),
        kind: kind.into(),
        deadline_ns: 0,  // 让 gateway 端按 timeout_ms 推
        priority: 0,
        payload_schema_id,
        payload,
    };
    tonic::Request::new(cmd)
}
```

**可借鉴模式**：纯函数 build；`#[allow(clippy::too_many_arguments)]`
（参数多但顺序固定）；`deadline_ns = 0` 让 gateway 推（client **不**
自己算 monotonic deadline，避免 client/gateway 时钟漂移）；`auth =
Some(AuthContext { token, mtls_fingerprint: "" })`。

类似还有 `build_cancel_task_cmd` / `build_task_handle_ref_req` /
`build_cmd` (clear_estop)；模式一致。

### 4.3 fbs payload builder

```rust
// ik_backend_smoke.rs: build_ik_payload_bytes
let mut fbb = FlatBufferBuilder::with_capacity(256);
let xyz_v = fbb.create_vector::<f64>(target_xyz);
// ... 4 个 vector
let payload = FbIkPayload::create(&mut fbb, &IkPayloadArgs { ... });
fbb.finish(payload, None);
fbb.finished_data().to_vec()
```

```rust
// scripted_backend_smoke.rs: build_scripted_payload_bytes
let mut fbb = FlatBufferBuilder::with_capacity(256);
let wp_offsets: Vec<_> = waypoints.iter().map(|(positions, dwell_ms, segment_duration_ms)| {
    let positions_vec = fbb.create_vector::<f64>(positions);
    WaypointTarget::create(&mut fbb, &WaypointTargetArgs { ... })
}).collect();
let wps_vec = fbb.create_vector(&wp_offsets);
let payload = FbScriptedPayload::create(&mut fbb, &ScriptedPayloadArgs {
    waypoints: Some(wps_vec) });
fbb.finish(payload, None);
fbb.finished_data().to_vec()
```

**可借鉴模式**：每个 backend 的 payload 由该 backend 决定字段集；
candidate 1 + candidate 2 round 2 都已经验证 builder 方法稳定；CLI
端 `invoke scripted` / `invoke ik` 直接复用（含 `--waypoints-file`
JSON parser 自定义 → fbs build 这条路径）

### 4.4 client 端连接 / readiness gate

```rust
// scripted_backend_smoke.rs:932-946 + :876-892
async fn connect_gateway_client(...)
async fn wait_for_gateway_tcp_ready(port: u16, timeout: Duration) -> Result<(), String>
async fn poll_get_task_status_ready(client: &mut ..., timeout: Duration) -> Result<(), String>
```

**可借鉴模式**：CLI 默认不做 readiness gate（人类输入命令时 gateway
已起；如 gateway 没起直接连接报错 exit）；smoke 走 gate 是因为
spawning subprocesses，CLI 不需要。

---

## 5. `rpc_roles.toml` 能力边界（v0.3 A1-1 + B4 (2/3-b) 锁定）

### 5.1 4 role × 9 capability 矩阵

| role | read_state | invoke_task | set_enabled | clear_error | clear_estop | estop | reload_calibration | set_policy | shutdown |
|---|---|---|---|---|---|---|---|---|---|
| **viewer** | ✅ |  |  |  |  |  |  |  |  |
| **operator** | ✅ | ✅ | ✅ |  |  | ✅ |  |  |  |
| **maintainer** | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |  |  |
| **admin** | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |

来源：`configs/rpc_roles.toml`（B4 (2/3-b) 收紧后；operator 删
`clear_error`）。

### 5.2 RPC × capability 映射

| RPC | 需要 capability | 默认可调 role |
|---|---|---|
| `GetTaskStatus` / `StreamTaskEvents` | `read_state` | viewer+ |
| `InvokeTask` / `CancelTask` | `invoke_task` | operator+ |
| `ClearError` | `clear_error` | maintainer+ |
| `ClearEStop` | `clear_estop` | maintainer+（**safety-critical**；§6.7 #3）|
| 候选 C 待 land：`SetEnabled` | `set_enabled` | operator+ |
| 候选 D 待 land：`EStop` 触发 | `estop` | operator+ |

### 5.3 fake token 映射

```toml
[tokens]
"viewer-fake-token-001"     = "viewer"
"operator-fake-token-001"   = "operator"
"maintainer-fake-token-001" = "maintainer"
"admin-fake-token-001"      = "admin"
```

CLI 默认用哪个 token / 怎么传 token / 怎么显示当前 role / mTLS / OIDC
路径都属于候选 A Spec 期范围（详 §8 Q3）；本 CodeMap **不**预定。

---

## 6. 7 个子命令最小 CLI contract（**待 reviewer 拍板细化**）

下表是按 7 RPC 一一对应整理的"最小可用 CLI surface"，参数名 / 默认
值 / 错误码映射 / 输出格式都属于 Spec 期范围（§8 Q8-Q12）。

| 子命令 | RPC | 必填参数 | 可选参数 | 输出（stdout 默认）| exit code 设计 |
|---|---|---|---|---|---|
| `robotctl clear-estop` | `ClearEStop` | `--robot-id` `--gateway-addr` `--confirmation` `--audit-reason` | `--token` `--source` `--idempotency-key` `--timeout-ms` `--dry-run` `--json` | `cleared_at_ns=<ns>  dry_run=<bool>` | 0 OK / 2 args / 3 auth / 4 server FailedPrecondition (e.g. latch 未 set) / 5 conn |
| `robotctl clear-error` | `ClearError` | `--robot-id` `--gateway-addr` `--audit-reason` | 同上 | `cleared_at_ns=<ns>` | 同上；4 server (e.g. coordinator 非 Degraded) |
| `robotctl get-task-status <action-id>` | `GetTaskStatus` | `--robot-id` `--gateway-addr` `<action-id>` | `--token` `--json` | `state=EXECUTING progress=0.45 ...` 或 `state=COMPLETED outcome=Success` 等 | 0 OK / 2 args / 3 auth / 4 NotFound / 5 conn |
| `robotctl cancel-task <action-id>` | `CancelTask` | `--robot-id` `--gateway-addr` `<action-id>` `--audit-reason` | `--token` `--source` `--idempotency-key` `--timeout-ms` `--dry-run` `--json` | `cleared_at_ns=<ns>` | 0 OK / 4 server (e.g. action_id 已终态) |
| `robotctl stream-events <action-id>` | `StreamTaskEvents` | `--robot-id` `--gateway-addr` `<action-id>` | `--token` `--json` `--timeout-ms`（整流；不是单帧）| 每帧一行 `at_ns=... event=PROGRESS progress=0.45` 或 `event=COMPLETED outcome=Success`；终态后 stream 自动关闭，exit 0 | 0 OK / SIGINT 早退 0 / 5 conn / 终态 FAILED 仍 exit 0（终态是有效结果）|
| `robotctl invoke scripted` | `InvokeTask` kind=Scripted | `--robot-id` `--gateway-addr` `--waypoints-file <PATH>` `--audit-reason` | `--token` `--source` `--idempotency-key` `--timeout-ms` `--dry-run` `--follow` `--json` | `action_id=<uuid>` 或 `--follow` 时 + stream-events 行 | 0 / 2 args / 3 auth / 4 server / 5 conn |
| `robotctl invoke ik` | `InvokeTask` kind=Ik | `--robot-id` `--gateway-addr` `--target-x` `--target-y` `--target-z` `--audit-reason` | `--target-qx / qy / qz / qw`（默认 `0 0 0 1`）`--seed-joints '<5个 f64>'` `--frame '<str>'` `--token` 等同上 + `--follow` | 同 invoke scripted | 同 invoke scripted |

### 6.1 共享参数集（所有 7 子命令）

| 参数 | 类型 | 默认 | 注 |
|---|---|---|---|
| `--robot-id <ID>` | str | env `ROBOT_ID` / 必填 | 沿既有 status / watch / estop 子命令风格 |
| `--gateway-addr <ADDR>` | str | env `ROBOT_GATEWAY_ADDR` 或 `127.0.0.1:50051` | `host:port`；不是 URL |
| `--token <TOKEN>` | str | env `ROBOT_TOKEN` / 必填 | fake token 字面值 |
| `--source <STR>` | str | `"cli.<unix_user>"` | audit |
| `--idempotency-key <STR>` | str | 自动生成（详 §8 Q5）| 跨 retry 稳定 |
| `--timeout-ms <U64>` | u64 | 2000 | CommandMeta.timeout_ms |
| `--dry-run` | flag | false | daemon 端真识别 |
| `--audit-reason <STR>` | str | 必填 5 个写 RPC；不必填 2 个读 RPC | audit + design.md §5.2 拍板"必带" |
| `--json` | flag | false | 切 JSON 输出（pipe 友好 / audit）|
| `--follow` | flag | false | 仅 `invoke scripted` / `invoke ik`：land action + 立即开 stream-events |

### 6.2 跟既有 3 个 iceoryx2 子命令的并存边界

| 既有子命令 | 路径 | 新子命令冲突项 |
|---|---|---|
| `status` | iceoryx2 直连 S0 state + heartbeat | 无；功能正交（status 是 S0 直读，无对等 RPC；候选 B `GetState` land 后可考虑 `status --via gateway`，留 v0.5 后期）|
| `watch` | 同 `status` 周期版 | 同上 |
| `estop` | publish `EStopEvent` 到 iceoryx2 → S0 ipc drain | **语义冲突**：`estop` 触发；候选 D 才 land `EStop` 触发 RPC；A 期间是否：(i) 保留 iceoryx2 路径作 offline fallback；(ii) `estop` 也加 `--via gateway` flag；(iii) deprecate iceoryx2 `estop` 改纯 RPC（候选 D land 后）；详 §8 Q1 |

---

## 7. 已落地能力 + 缺口

### 7.1 已落地（可直接复用）

| 能力 | 来源 | 候选 A 复用 |
|---|---|---|
| `crates/robot-cli/` workspace member + `robotctl` binary + 3 个 iceoryx2 子命令 + signal handler 兜底 SIGSEGV pattern + CLI parser scaffolding | v0.2 Part 2 + task-30 v9 | 直接扩 subcommand match arm；不重新搭脚手架 |
| `RobotControlClient` tonic stub via `robot-rpc/client` feature | A1-3 (3/3-a) | `cargo add robot-rpc --features client`（workspace dep） |
| 6 RPC 真起（gateway + s1 真接通 + per-ActionId pubsub 兜底）| A1-3 + B4 (1/3) + (2/3) + (3/3) | client 直接调；不需要再 wire-up |
| `rpc_roles.toml` 4 role × 9 capability 映射 + 4 fake token | A1-1 (3/3) + B4 (2/3-b) | client 端透传 token；server 端查表拒绝 |
| `validate_request_id` UUIDv7 严格校验 | A1-1 (3/3) | 客户端可前置本地校验（fail-fast）|
| smoke binary tonic client helper 4 处可借鉴模式（详 §4）| candidate 1 / 候选 2 round 2 / clear_estop_e2e | inline copy 或抽 helper（§8 Q7）|
| `IkPayload` / `ScriptedPayload` fbs builder（已在 lib unit + smoke binary 验证）| 候选 1 + 候选 2 round 2 | client 端构 payload 直接复用 |

### 7.2 缺口（候选 A 期间需补）

| 缺口 | 估工程量 | 节点 |
|---|---|---|
| `robotctl` binary 改用 `#[tokio::main]` async runtime（与既有 sync 子命令并存）| 中（涉及 signal handler / 入口分流）| §8 Q2 拍板后定 |
| `crates/robot-cli/Cargo.toml` 加 `robot-rpc = { workspace = true, features = ["client"] }` + `tonic` + `tokio` + `flatbuffers` | 小 | Spec 期定 |
| 7 个新子命令 parse + dispatch | 中（每个子命令 ~30-80 LoC）| Spec / Plan 期 |
| 共享 client harness（endpoint connect / token / `--json` 输出 / error → exit code 映射）| 中（~150-200 LoC，沿 smoke binary 模式）| Spec / Plan 期 |
| Smoke 入口（CLI 端 e2e；可选 `cargo run -p robot-cli --bin robotctl <subcommand>` 跑现有 gateway 起的 instance）| 小-中（沿 smoke binary 同款；详 §8 Q9）| 实施期 |
| 跟既有 iceoryx2 `estop` 子命令的并存策略 | 中（取决于 Q1 路径）| §8 Q1 |

---

## 8. 待 reviewer 拍板（Q1..Q15）

> 本 CodeMap **不**做拍板；下面是 Spec round 1 入口前必须 reviewer 决
> 定的具体路径选项。每条 Q 都附 2-3 个候选选项 + 推荐侧（带"建议"
> 字样的是 CodeMap 作者推荐，不是拍板）。

### Q1 — 既有 iceoryx2 `estop` 子命令的并存策略

- **(a)** 保留：iceoryx2 路径作 offline fallback（gateway 不在或网络
  断时仍可 trigger emergency stop）；候选 D land 后再加 `estop --via
  gateway` flag
- **(b)** 改造：候选 A 期间直接加 `estop --via gateway` 但 gateway
  端 RPC 是 stub（**不**真实现；等候选 D land）—— 提前 land CLI surface
- **(c)** Deprecate：候选 A 期间打 warning `iceoryx2 estop 即将
  deprecate`；候选 D land 后 next major 版本删

**建议（a）**：iceoryx2 路径已经过 task #15 真测试，是 offline
fallback 的合理位置；`clear-estop` 子命令的 gateway 路径跟它正交不
冲突。

### Q2 — `robotctl` async runtime 模型

- **(a)** 整 binary 改 `#[tokio::main]`：既有 `status` / `watch` /
  `estop` 子命令在 async 入口里调用 sync 路径（`tokio::task::spawn_
  blocking`）—— 简单但既有路径增加 runtime 启动开销
- **(b)** 入口分流：`fn main()` 先 parse subcommand → 若是 7 个 gateway
  子命令则 `tokio::runtime::Builder::new_multi_thread()...block_on()`；
  既有 3 个走原 sync 路径不变 —— 性能最优但代码结构有分支
- **(c)** 拆成两个 binary：`robotctl` (sync iceoryx2) + `robotctl-rpc`
  (async gateway)—— 用户体验差，不推荐

**建议（b）**：避免给既有 status / watch 路径加 tokio runtime 启动
开销；分流路径在 main fn 顶部一次 if-else 即可，结构清晰。

### Q3 — auth token 输入方式

- **(a)** `--token <TOKEN>` 必填 / env `ROBOT_TOKEN` —— 简单
- **(b)** 加 `~/.robotctl/credentials.toml` 文件 + `--role` flag 切
  当前 role 查表 token —— 跟 rpc_roles.toml 同 spirit 但客户端独立；
  支持本地多 role 切换
- **(c)** OIDC / mTLS 真 token —— 跟 v0.5+ 真 auth 实施同步

**建议（a）**：候选 A 沿 fake token 体系；OIDC / mTLS 留 v0.5+ 独
立 outline；`--token` 必填 + env fallback 足够 dev/staging。

### Q4 — gateway 地址默认 / 发现

- **(a)** `--gateway-addr 127.0.0.1:50051` 默认 / env `ROBOT_GATEWAY_ADDR`
  —— 简单；本地 loopback
- **(b)** 同 (a) 但加 `--robot-id` 参数 + 配置文件查 `<id> →
  <addr>` —— 支持多 robot 切换
- **(c)** mDNS / service discovery —— 过 engineering

**建议（a）**：候选 A 是 maintainer 本地工具优先；mDNS / multi-
robot 切换留后期。

### Q5 — `--idempotency-key` 默认行为

- **(a)** 必填 —— 强迫用户思考 retry 语义
- **(b)** 自动生成 `cli-<unix_ms>-<random>` —— 简单；retry 时用户必
  须显式带同 key 才生效
- **(c)** 自动生成 + 提示 `consider --idempotency-key for retry` —— 中
  庸

**建议（b）**：跟 5 个 smoke binary 既有模式一致（`format!("ik-
{request_id}")`）；用户需要显式 retry 时手动带同 key。

### Q6 — `tonic::Code` → CLI exit code / stderr 映射

- **(a)** 详细映射表：每条 tonic Code 一个独立 exit code（沿 §6 各
  子命令 exit code 设计列）—— 脚本友好
- **(b)** 粗映射：`0=Ok / 1=fatal / 2=args / 3=server`—— 简单
- **(c)** 仅 `0=Ok / 1=非 Ok` —— Unix tool 极简

**建议（a）**：CLI 是 maintainer 工具，audit / 脚本 use case 多；
详细 exit code 让自动化容易区分（沿 v0.4 ik_backend_smoke / scripted
_backend_smoke `ExitCode::from(2)` vs `from(1)` 既有风格）。

### Q7 — `uuidv7_compat()` / `build_*_cmd` helper 抽到哪里

- **(a)** 不抽，candidate A 内部 inline 复制（沿 smoke binary 不抽
  helper reviewer 拍板）
- **(b)** 抽到 `crates/robot-cli/src/lib.rs`（lib 内复用；不对外暴
  露）—— 候选 A 自洽 + 单测友好
- **(c)** 抽到 `crates/robot-rpc/src/client.rs`（lib + feature
  gate）—— 长期方向；但跟 reviewer 候选 4 light Research 拍板"smoke
  不抽公共 helper" 同 spirit 是否一致需 reviewer 决定
- **(d)** 抽到新 crate `robot-rpc-client-helpers` —— 过 engineering

**建议（b）**：跟既有 robotctl `src/lib.rs` 空 lib 备注"未来 CLI 体
积增大后核心逻辑抽到 lib 里方便单测"一致；候选 A 自洽不污染其它
crate。**关键**：候选 A 实施期顺手把 smoke binary 4 处 `uuidv7_compat
()` 重复实现替换成 `uuid::Uuid::now_v7().hyphenated().to_string()`
（沿 `robot_s1::action::ActionId::new_v7` 用法）—— 沿候选 4 light
Research §6 "operator 面工具化"机会一起清理代码 redundancy；**但**这
是候选 A 范围扩展，需 §8 Q14 拍板。

### Q8 — `--json` 输出格式细化

- **(a)** 全部子命令都支持 `--json`；schema 跟 proto response 一一对
  应（每字段 lowercase + snake_case；timestamps 用 `<key>_ns` 整数；
  enums 用大写 `EXECUTING` / `COMPLETED` 等）
- **(b)** 只 7 个新子命令支持 `--json`，既有 status / watch / estop
  不支持 —— 避免 retrofit 风险
- **(c)** `--json` 暂不支持；候选 A 首版仅 human-readable

**建议（a）（仅新子命令）+ (c)（既有不动）**：候选 A 拍板 7 个新
子命令必须支持 `--json`（pipe / audit / automation 都需要）；既有
3 个子命令的输出格式保稳定（avoid breaking 既有用户脚本）。

### Q9 — Smoke / 集成测试

- **(a)** 新 `robotctl_smoke` binary 沿 candidate 1/2/3/4 同款 spawn
  s1_daemon + gateway + 跑 CLI 子命令 + 验输出 + 退出码
- **(b)** integration test `crates/robot-cli/tests/robotctl_e2e.rs`
  用 `tokio::process::Command` spawn `robotctl <subcommand>` 验输出
- **(c)** 仅 lib unit + 手动验证（gateway 已起后跑命令）

**建议（a）**：覆盖每个子命令真起 → 真调 RPC → 真退出码；feature-
gated（`robotctl-smoke` feature）沿候选 1 / 2 / 3 / 4 同款；既有 4
个 smoke binary 都已经验证此 pattern 稳定。

### Q10 — `stream-events` SIGINT / 早退语义

- **(a)** SIGINT exit 0；尚未到终态 stream 也 OK；不打 warning
- **(b)** SIGINT exit 130（标准 128 + SIGINT 2）；告诉用户中断而非
  自然终止
- **(c)** SIGINT exit 0 但打 stderr `warning: stream interrupted before
  terminal event`

**建议（c）**：保 exit 0 兼容 Unix tool（SIGINT 是用户主动行为，不
是 fatal）；同时 stderr 提示让 audit 能识别"中断"vs"终态"。

### Q11 — `invoke ik` / `invoke scripted` 默认是否 `--follow`

- **(a)** 默认 `--follow`：land action + 立即开 stream-events 等终态
  打印 → 退出（用户体验最自然；类似 `kubectl apply -f ... -w`）
- **(b)** 默认**不** follow：land action + 打印 `action_id=...` 立即
  退出；用户主动 `robotctl stream-events <id>` 跟（脚本友好；retry
  快）
- **(c)** 两 mode 都不默认；强制用户指定 `--follow` 或 `--no-follow`

**建议（b）**：脚本友好 / 跟 6 RPC 设计契约（InvokeTask 返
TaskHandle 即结束，事件流是单独 RPC）一致；用户加 `--follow` 是显
式选择。

### Q12 — `cancel-task` 跟 `stream-events` 的协作模式

- **(a)** 两个独立子命令；用户先开 stream-events 后用另一终端
  cancel-task（沿 unix 工具组合哲学）
- **(b)** `cancel-task --follow` 在 cancel 后跟 stream 等终态打印（沿
  invoke 同款 follow flag）
- **(c)** 加 `robotctl wait <action-id>` 子命令专门等终态

**建议（a）**：候选 A 首版不加 follow 跨子命令；用户用 shell
piping / multiple terminals 即可；候选 A 后期或 v0.5 后期再考虑 (b)
/ (c) 扩展。

### Q13 — `invoke scripted --waypoints-file` 格式

- **(a)** JSON：`{ "waypoints": [{ "positions": [...], "dwell_ms":
  100, "segment_duration_ms": 500 }, ...] }`
- **(b)** TOML 沿 configs/ 风格
- **(c)** CSV 简单平铺
- **(d)** 直接接受 `--waypoint` 多次参数（不要文件）

**建议（a）**：JSON 跟 ScriptedPayload fbs schema 一一对应；序列化
最直接；用户也方便手写或脚本生成。

### Q14 — 候选 A 是否顺手清理 smoke binary 4 处 `uuidv7_compat()` 重复

- **(a)** 是，作 candidate A 范围扩展（沿 §7 Q7 §6 light Research
  §6 operator 面工具化机会）：把 4 个 smoke binary `uuidv7_compat()`
  替换成 `uuid::Uuid::now_v7()`；候选 A 实施期一起 land
- **(b)** 否，候选 A 不扩范围；smoke binary 4 处重复留独立 follow-
  up PR

**建议（b）**：候选 A 首版聚焦 7 子命令 land + 4 处 smoke 改动跟
本候选语义无关；扩范围会违反 reviewer "不改候选 1/3 既有 smoke
binary 行为"原则（v0.5_entry §5 #8）。

### Q15 — Spec round 1 Q-CLI 拍板范围（边界声明）

- **(a)** 必须含 Q1..Q14 全部；其它（schema_id 取值 / fbs builder /
  exit code 详细表 / etc.）也属于 Spec round 1 范围
- **(b)** Q1..Q14 必拍板；schema_id / exit code 详细表是 Plan 期细
  化 / 实施期 ad-hoc

**建议（a）**：候选 A 是首次 land tonic-based CLI surface；Spec
round 1 应该把 user-facing contract 拍透（CLI 参数 / 输出 / 错误码
全部），避免实施期反复回头 reviewer。

---

## 9. 不在范围（候选 A 边界硬约束）

reviewer v0.5_entry §5 hard boundaries 8 条沿用 + 本 CodeMap 额外
锁定：

- **不**改 `crates/robot-rpc/` lib（lib 不动；client 端透传 + 错误
  映射全在 `crates/robot-cli/`）
- **不**改 `crates/robot-msg/` schemas（fbs builder 借既有；新 fbs
  schema 是候选 B/C/D 范围）
- **不**改 `rpc_roles.toml`（capability 矩阵不动；client 端只是
  consumer）
- **不**改 `docs/00_design.md` §5.3 14 frozen RPC 表（候选 A 实现 6
  已 land RPC 的 client；不动 frozen 定义）
- **不**改候选 1/2/3/4 已 land smoke binary 行为
- **不**改既有 `robotctl status` / `watch` / `estop` 子命令行为
  （Q1 拍板若选 (a) 或 (c)，对 `estop` 子命令的改动方式必须明确不
  破既有 iceoryx2 路径）
- **不**自动连云端 / 不发遥测 / 不存 token 到 keychain（候选 A 是
  local maintainer tool；安全敏感行为 v0.5+ 单独 outline）
- **不**实现 candidate B/C/D 任何 RPC：候选 A land 6 已 land RPC 的
  client；`SetEnabled` / `GetState` / `StreamState` / `EStop` 触发
  4 个 RPC 是候选 B/C/D scope
- **不**做 Python SDK / dimos-deploy wrapper（候选 4 light Research
  defer 边界沿用）

---

## 10. 工程量估计（粗估；Plan 期再细化）

| 段 | LoC | 估 commit 数 | 范围 |
|---|---|---|---|
| `Cargo.toml` 改 + lib 扩 helper | ~100 | 1 | 加 `robot-rpc/client` + `tonic` + `tokio` 依赖；lib 加 `client_helpers` 或同款 module |
| `main()` 入口改 async/sync 分流 | ~50 | 同上 | Q2 (b) 路径 |
| 7 个新子命令 + 共享 harness | ~600-900 | 2-3 | 沿 §6 contract；JSON 输出 |
| `crates/robot-tests/src/bin/robotctl_smoke.rs` + feature gate | ~600-900 | 1-2 | 沿 candidate 1 / 2 / 3 / 4 smoke 同款 |
| docs（README / `--help` 文案 / handoff）| ~100 | 0（commit msg 内）| Spec 期定 |

**估总工程量**：~5-8 commit（沿 v0.5_entry §2.1 估的 ~5-8 commit）。

---

## 11. 跟其它 milestone handoff / spec 的链接

- [`docs/handoffs/v0.5_entry.md`](../../docs/handoffs/v0.5_entry.md)
  §2.1（候选 A 范围）+ §3.2（A 排第一位的理由）+ §4（切入点 +
  RIPER 5 步流程）+ §5（hard boundaries 8 条）— **本 CodeMap §0 锁
  定的关键修订点之一是 §3.2 / §4 / §6 / 开工前必读多处"首次 land
  `crates/robot-cli/` workspace member"描述不准**，需 Spec round 1
  reviewer 拍板修订或在 Spec 内明示
- [`docs/handoffs/v0.4_dimos_research.md`](../../docs/handoffs/v0.4_dimos_research.md)
  §6 7 个候选 `robotctl` CLI 工具表 — 候选 A 7 子命令的源头
- [`docs/handoffs/v0.4_robot_kinematics_round2_summary.md`](../../docs/handoffs/v0.4_robot_kinematics_round2_summary.md)
  关键契约审计 8 条 — 候选 A `invoke ik` 子命令复用 IkBackend / 6
  RPC client wire format 直接参考
- [`docs/handoffs/v0.4_scripted_backend_summary.md`](../../docs/handoffs/v0.4_scripted_backend_summary.md)
  ScriptedPayload fbs builder + cross-process e2e — 候选 A `invoke
  scripted` 子命令复用
- [`docs/handoffs/v0.3_b4_summary.md`](../../docs/handoffs/v0.3_b4_summary.md)
  §3 7 条契约 + §4 cross-process e2e harness 工程经验 — 候选 A
  smoke binary 沿用全部 7 契约
- [`docs/handoffs/v0.3_a1_4_clear_estop_smoke.md`](../../docs/handoffs/v0.3_a1_4_clear_estop_smoke.md)
  ClearEStop control plane 真闭环 — 候选 A `clear-estop` 子命令
  复用 6-12 step server side; client 端就 build + send 一次
- `crates/robot-rpc/proto/robot_control.proto` — 6 RPC + CommandMeta
  + AuthContext + 5 个 enum 字面权威
- `configs/rpc_roles.toml` — capability 矩阵权威
- `crates/robot-tests/src/bin/scripted_backend_smoke.rs` /
  `ik_backend_smoke.rs` / `clear_estop_e2e.rs` / `pubsub_cleanup_
  smoke.rs` — tonic client + helper pattern 4 份现成参考
- `crates/robot-cli/src/bin/robotctl.rs` — 既有 3 子命令 + signal
  handler pattern + CLI parser scaffolding
- `docs/00_design.md` §5.2（CommandMeta + AuthContext）+ §5.3 14
  frozen RPC + §5.4（4 roles + 9 capabilities）+ §6.7 5 不变量
  （候选 A 0 触）

---

## 12. 下一步（**不**直接进实现）

1. **reviewer 复审本 CodeMap** —— 确认 §0 修订点 + §8 Q1..Q15 路径
   选项是否完整 / 是否漏拍板项
2. **拍板 §8 Q1..Q15 全部**（reviewer round 1）
3. **写 Spec round 1**：`team-swe/specs/v0_5_operator_cli.md`
   * 包含 What / 影响范围 / 回滚策略 / AC-1..AC-N + Q-CLI（拍板
     Q1..Q15 转 AC 形式）+ §"严禁改动"hard boundary + reviewer 审
     查重点
4. **Plan outline draft v1 + reviewer round 2-N**
5. **commit 拆分**（Spec/Plan 期定；粗估 5-8 commit）
6. **summary**：`docs/handoffs/v0.5_operator_cli_summary.md`

**严禁**：

- 本 CodeMap 之后**不**直接开实现（reviewer Spec / Plan / 拍板未完
  成前任何 PR 都不进 master）
- 不擅自把 §8 Q1..Q15 拍板项往 v0.5_entry / v0.4_entry / design.md
  / CHANGELOG.md 倒灌
- 不擅自改既有 `robotctl status` / `watch` / `estop` 子命令行为
- 不擅自抽 helper crate（§8 Q7 reviewer 拍板前不动 crate 拓扑）
