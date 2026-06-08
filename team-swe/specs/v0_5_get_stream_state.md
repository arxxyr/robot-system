# Spec · v0.5 候选 B — `GetState` / `StreamState` RPC（round 3）

> RIPER Spec round 3（round 2 方向 ACK + reviewer 3 阻断 + 2 同步修订已吸收）。
> 前置 = [`team-swe/codemap/v0_5_get_stream_state.md`](../codemap/v0_5_get_stream_state.md)
> （reviewer CodeMap 方向 ACK + 4 收紧点 + 8 问题默认方向 + proto 缺口
> additive 结论）。round 2 把 round 1 全部待拍板项落到拍定；round 3 补齐
> **gateway hardware 列表来源（D10）** + 清掉 StreamState / e2e 残留旧口径 +
> 写实 AC-3 Unavailable / AC-6 auth。待 reviewer round 3 复核 → Plan。

## Spec round 2 amend 记录（2026-06-03；reviewer round 1 复核后）

reviewer round 1 ACK 方向但要 5 项拍板/修订后再进 round 2。已全吸收：

| # | round 1 复核点 | 修订 |
|---|---|---|
| 1 | D3 选 (b) | GetState(GetStateRequest{hardware_id})，hardware_id **必填非空**；StreamState(StreamRequest)；CHANGELOG 登记 design-freeze diff（§5.3 Empty → 显式 hardware selector）。**D3 拍定**|
| 2 | D4a 选**常驻订阅池**（不临时+重试）| gateway 启动建 state subscription pool（per config hardware）+ latest-state cache；GetState / StreamState 读 cache，**不**为每 RPC 新建底层 iceoryx2 subscriber；没首帧 → **Unavailable**（服务未 ready，非请求语义错）。**D4 重写**|
| 3 | D5a ACK | min_interval_ms==0→默认 50ms；0<x<20→clamp 20ms（不拒）；keep-last + interval tick；不保证每 S0 sample 送达。**D5 拍定**|
| 4 | D1a 不含 heartbeat rate ACK | RobotState 最小字段；rate 后续 additive 加。**D1a 拍定（不含）**|
| 5 | D3b 错误码拆口径 | hardware_id 缺/空→**InvalidArgument**；unknown→**NotFound**；已知但 cache 无首帧→**Unavailable**；FailedPrecondition **不**做默认。+ **新 AC-9**：gateway 只订阅 config 声明 hardware，请求未声明 → NotFound，**禁默默 fallback**。**D3b 重写**|

## Spec round 3 amend 记录（2026-06-03；reviewer round 2 复核后）

reviewer round 2 认可 D3/D4a/D5a/D1a/D3b 方向，但 3 阻断 + 2 同步修订须回修：

| # | round 2 复核点 | 修订 |
|---|---|---|
| 阻断 1 | gateway hardware 列表来源未定义（`robot_rpc_gateway.rs` / `gateway.rs` 无 config/hardware 输入，AC-9 无法实施）| **新 D10**：gateway 加 `--config <PATH>`（**可选**；缺省 = 空 state 池，state RPC 全 NotFound，B4 既有调用不破）；present 时 minimal local 解析 `[[hardware]].id`（复用 robot-rpc 既有 serde+toml，**不** dep robot-s0、**不**抽共享 crate）。§影响范围 + AC-9 引 D10 |
| 阻断 2 | StreamState 残留"per-RPC spawn 订阅线程"口径（§影响范围 line 52），与 D4a"常驻池 + per-RPC tick"冲突 | §影响范围 StreamState bullet 重写：底层 iceoryx2 state subscriber **只**在常驻池建；每 StreamState RPC 只建**轻量 async stream/tick task**，**不**新建底层 subscriber |
| 阻断 3 | e2e 清单残留"多 hardware FailedPrecondition"（§影响范围 line 58），与 round 2 D9 冲突 | e2e bullet 重写：unknown hardware → **NotFound** / no first frame → **Unavailable**；首版无 export-disabled flag，**FP 不进 e2e 主路径** |
| 同步 4 | AC-3 Unavailable 场景要写实 | AC-3：config 声明 hardware_id 但**测试不启对应 FakeStatePublisher** → 常驻池 cache 无首帧 → GetState 返 `Unavailable` |
| 同步 5 | AC-6 auth 表述别写得像 token 真上 wire | AC-6 + D7 重写：request **无 auth 字段**，无 token / 任意 metadata token **都不影响结果**（服务端不读不校验）；**不得**宣称 `read_state` cap 已 enforce |

> **round 3 文案收窄（reviewer ACK-into-Plan 非阻断）**：空池口径精确化——无
> `--config` 时**非空 `hardware_id`** 请求返 `NotFound`，但**缺/空 `hardware_id`
> 仍先按 D9/AC-3 返 `InvalidArgument`**（校验早于池查找；D9 主口径本就正确，此处
> 只把 §影响范围 D10 两处概述对齐）。错误码顺序在 Plan §P.2.2 GetState handler
> 步骤里硬锁。

## Plan round 1 amend 记录（2026-06-03；reviewer Plan round 1 复核后）

reviewer Plan round 1 **不批准直接进 (1/4)**，给 1 阻断 + 3 拍板 + 1 非阻断；本
amend 全吸收，预期下轮 **Plan Approved**：

| # | 类型 | reviewer 点 | 落点 |
|---|---|---|---|
| PB1 | **阻断** | proto 4 enum 与 fbs 4 enum **同名**（`S0Mode` 等），`state_convert.rs` 同时 import 两个 → 未锁别名易混乱/clippy 噪声 | §P.2.1.b 硬约束：`use robot_msg::msg::{S0Mode as S0ModeFb, ...}` + `use crate::proto::{S0Mode as S0ModeProto, ...}`；四 enum 全 `*Fb`/`*Proto`；转换 fn `s0_*_fb_to_proto`；**禁裸用同名** |
| PD1 | 拍板 | D-P1 ACK | §P.3：`[[hardware]]` 空 = 空 state 池；非空 hardware_id NotFound，缺/空仍先 InvalidArgument；兼容 B4 |
| PD2 | 拍板 | D-P2 拍定数值 | §P.3 + §P.2.4.b：`interval=50`→ window=600ms 帧 ≤13 + 相邻 ≥40ms；clamp(<20)→ window=240ms 帧 ≤13 + 相邻 ≥16ms（测试判据，不改协议）|
| PD3 | 拍板 | D-P3 ACK + 改进 | §P.3 + §P.2.4.a：robot_id=`state_e2e_<pid>_<case>`（避残留 topic 互扰）+ port 50061 + GLOBAL_LOCK + --test-threads=1 |
| PN1 | 非阻断 | AC-Q `schema_compat` 措辞（本轮不改 fbs，别误读成有独立命令）| AC-Q + §P.2.1.d + §P.2.4.d：写实 `cargo run -p robot-msg --bin schema_lint` + `cargo test -p robot-msg`（fbs 0 退化）；proto additive 兼容由 prost build + `task_rpc_e2e` 覆盖 |

## What

实现 design §5.3 两条**只读** RPC，让**远程 client**（dimos / web UI，gRPC
跨进程/跨机）读 robot state：

- `GetState`（unary）→ 单次 `RobotState` 快照。
- `StreamState`（server-streaming）→ 节流后的 `RobotState` 流。

**additive**：`crates/robot-rpc/proto/robot_control.proto` `service RobotControl`
（lines 94-104，当前 6 B4 method）**append** 2 rpc + 新 message + enum mirror；
**既有 6 method 字段/字段号 0 改**（B4 frozen schema hard boundary）。method
数 6 → 8。

权威数据源 = **S0 state topic**（S0 发布 `JointState`），**不是** S1 RR（S1 是
coordinator，不持权威硬件 state）。gateway **首次**订阅 S0 state topic。

## 影响范围

### 必改（候选 B land 必须）

- `crates/robot-rpc/proto/robot_control.proto`：append `GetState` /
  `StreamState` rpc + `RobotState` / `GetStateRequest` / `StreamRequest`
  message + `S0Mode` / `S0ControlLink` / `S0OutputMode` / `S0SafetyState`
  proto 镜像 enum（最小，仅 RobotState 实际用到的字段）。头注 method 数 6→8。
- `crates/robot-rpc/src/bin/robot_rpc_gateway.rs`（**阻断 1 / D10**）：加
  **可选** `--config <PATH>` flag（robot config TOML）→ minimal local 解析
  `[[hardware]].id` → 把 hardware id 列表喂进 `GatewayConfig`（新字段
  `state_hardware_ids: Vec<String>`）。缺省（无 `--config`）= 空列表 = state
  订阅池为空 = 所有**非空 `hardware_id`** 请求返 `NotFound`（**缺/空 `hardware_id`
  仍先按 D9/AC-3 返 `InvalidArgument`**——校验早于池查找）（**B4 既有 gateway
  调用 0 破**，clear-estop 等照常）。
- `crates/robot-rpc/src/gateway.rs`：`GetState` / `StreamState` 两个 trait
  method impl + `GatewayConfig.state_hardware_ids` 字段 + 常驻 state 订阅池
  启停（沿 B4 shutdown 顺序）。
- `crates/robot-rpc/src/`（新模块或扩既有）：
  - gateway 端 **常驻 per-hardware S0 state 订阅池**（每个 config
    `[[hardware]].id` 一条 `open_sub::<JointState>` on
    `/<robot_id>/s0/<hardware_id>/state` + `TopicClass::S0State`，循环读 →
    更新该 hardware 的 latest-state cache；详 D4）。
  - **`JointState`-fbs → `RobotState`-proto 转换器**（类比 B4
    `events_subscribe.rs::action_*_fb_to_proto`）。
  - `StreamState` 的 sync↔async 桥（**阻断 2 修正**）：底层 iceoryx2 state
    subscriber **只**在常驻池里建；**每个 StreamState RPC 只 spawn 一条轻量
    async stream/tick task**（按 `min_interval_ms` 读 cache + `mpsc` →
    `Box::pin` Stream + drop→stop request），**绝不**为单 RPC 新建底层
    iceoryx2 subscriber。**节流**（keep-last + interval tick，**不**逐 S0
    sample 转发；详 D5）。
- `docs/CHANGELOG.md`：登记 **design-freeze diff**（Q3 GetState 签名，见 §
  设计决策 D3）+ C1 延续（read RPC 非安全保证）+ D10 gateway `--config`
  加法（B4 既有调用兼容）。
- `crates/robot-rpc/tests/`：cross-process e2e（**阻断 3 修正**）：GetState
  happy + StreamState 节流 + 错误码 D9 三条（缺/空 `InvalidArgument` /
  unknown hardware → **`NotFound`** / 已知但无首帧 → **`Unavailable`**）+
  AC-9 多 hardware 选择。**首版无 export-disabled flag，`FailedPrecondition`
  不进 e2e 主路径**（仅 D9 表留预留语义）。

### 严禁改动（hard boundary）

- **B4 frozen 6-RPC schema**：既有 ClearEStop / InvokeTask / CancelTask /
  GetTaskStatus / StreamTaskEvents / ClearError 字段 / 字段号 / 语义 0 改。
- **§6.7 五条不变量**：GetState/StreamState 只读，0 改任何 safety state /
  latch / EStop / output mode / fault。
- **`crates/robot-cli`（robotctl）0 改动**（Q6：候选 B **不**加 get-state /
  stream-state 子命令；本地 operator 已有 status/watch iceoryx2-direct）。
- **`docs/00_design.md` 0 改**：候选 B 是**实现** §5.3 冻结接口；GetState 签名
  偏离走 CHANGELOG 登记（D3），不改 design.md §5.3。
- **S0 / S1 daemon 0 改**：gateway 单向订阅 S0 已发布的 state topic，不要求
  S0/S1 改发布行为。

## 关键设计决策（reviewer 4 收紧点 + 8 默认方向落地）

### D1 `RobotState` 字段（Q1）= mirror JointState 当前 S0 可得字段，per-hardware，不聚合

```proto
message RobotState {
    string robot_id      = 1;
    string hardware_id   = 2;   // 本快照对应的单个 hardware（per-hardware，不聚合）
    uint64 layout_id     = 3;
    uint64 stamp_ns      = 4;   // S0 published stamp（CLOCK_MONOTONIC_RAW；非 gateway 自生）
    repeated double position_rad = 5;
    S0Mode        mode         = 6;   // design §157 必报
    S0ControlLink control_link = 7;   // design §157 必报（OK / Lost）
    S0OutputMode  output_mode  = 8;   // design §157 必报（Active / Hold / Zero / Stop）
    S0SafetyState safety_state = 9;
    uint32        fault_code   = 10;
    // heartbeat rate：D1a 拍定**不含**（最小集；后续 additive 字段号兼容加）
}
```

- **字段全部来自 `JointState` fbs 的 s0_* accessor**（robotctl `print_snapshot`
  读的同一批：`s0_mode` / `s0_control_link` / `s0_output_mode` /
  `s0_safety_state` / `s0_fault_code` + `position_rad` / `layout_id` /
  `stamp_ns`）。`stamp_ns` 透传 S0 published 值（**非 gateway 自生 clock**，
  避免引入 self-clock）。
- **D1a 拍定（reviewer round 1）不含** heartbeat rate（`/<robot_id>/s0/heartbeat`
  的 `tick_rate_hz`）：最小字段先走；rate 是观测增强、非 state RPC 首版必要，
  后续 additive 字段号兼容加（不必多订 heartbeat topic）。

### D2 `StreamRequest`（Q2）= `{ hardware_id, min_interval_ms }`

```proto
message StreamRequest {
    string hardware_id    = 1;   // 选哪个 bus/arm（多 hardware 必填）
    uint32 min_interval_ms = 2;  // 节流下限（0 = 用 gateway 默认下限）
}
```

### D3 ⚠ **design-freeze diff**：`GetState(Empty)` → `GetState(GetStateRequest)`（Q3）

design §5.3 写 `rpc GetState (Empty) returns (RobotState);`，但**多 hardware 下
`Empty` 歧义**（返哪个 hardware？）。reviewer 倾向**改 request 加 hardware_id**：

```proto
message GetStateRequest { string hardware_id = 1; }   // 必填非空
rpc GetState (GetStateRequest) returns (RobotState);
```

- **拍定 (b)**（reviewer round 1）：`GetState(GetStateRequest{hardware_id})`，
  `hardware_id` **必填非空**（缺/空 → `InvalidArgument`，见 D3b）。多 hardware
  无歧义，不为贴 §5.3 字面保留歧义接口。
- **这偏离 frozen design §5.3 的 `Empty` 签名** → **append-safe 修正 + CHANGELOG
  登记**（`docs/00_design.md` **不改**；走"要改走 CHANGELOG"路径）：
  > design §5.3 的 `GetState(Empty)` 在实现期被多 hardware 语义修正为**显式
  > hardware selector** `GetState(GetStateRequest{hardware_id})`；§5.3 文字不变，
  > 实现以本 Spec + CHANGELOG 为准。
- **StreamState 同理**：`StreamRequest.hardware_id` 显式选（D2 已含）。

### D4 gateway state subscription pool（reviewer 收紧 1 + D4a 拍定**常驻池**）

- **topic 形式**：`/<robot_id>/s0/<hardware_id>/state`（`TopicClass::S0State`，
  跟 robotctl status / s1 anchor 同一 topic）。
- **常驻订阅池（D4a 拍定，不临时+重试）**：gateway **启动时**按 `--config`
  声明的每个 `[[hardware]].id`（来源见 **D10**）建一条**常驻** state
  subscription（持久 iceoryx2 subscriber 线程，循环读 → 更新该 hardware 的
  **latest-state cache**）。
  - **解决候选 A E1 竞态**：常驻订阅持续读，可靠拿 S0 sample（沿候选 A "watch
    persistent subscriber reliable" 同理）；GetState 作为 RPC **不**靠临时订阅
    碰运气。
- **GetState（unary）**：按 `GetStateRequest.hardware_id` **读 cache**（**不**新建
  底层 subscriber）→ 错误码 per D3b（缺/空 `InvalidArgument` / unknown
  `NotFound` / 已知但 cache 无首帧 `Unavailable`）→ 有 cache → fbs→proto 返。
- **StreamState（streaming）**：**复用同一 cache**（**不**为每 RPC 新建底层
  iceoryx2 subscriber）+ per-RPC 一条轻量 tick task（按 `min_interval_ms` 读
  cache 推 stream，D5）；`EventsSubscribeStream::drop → request_stop` 只清 per-RPC
  task（**底层 pool 订阅不随单 RPC 关**）。
- **池生命周期**：随 gateway 进程（启动建 / shutdown 停 + join，沿 B4 shutdown
  顺序契约 mark_shutting_down → ... → pool stop+join）。

### D5 `StreamState` 节流（reviewer 收紧 4 + Q4 + D5a 拍定）—— **硬决策**

- S0 state 可能高频（design 注 JointState 500Hz+）。**StreamState 严禁逐 S0
  sample 裸转发**；只保证**最新状态观察流**（**不**保证每个 S0 sample 送达）。
- **gateway keep-last + interval tick**：从 D4 cache 持最新 sample；per-RPC
  tick task 按 `min_interval_ms` 输出 cache 最新值（keep-last，丢中间帧）。
- **D5a 拍定（reviewer round 1）**：
  - `min_interval_ms == 0` → 默认 **50ms（20Hz）**。
  - `0 < min_interval_ms < 20` → **clamp 到 20ms**（floor，**不拒**）。
  - `min_interval_ms >= 20` → 原值。

### D6 enum 镜像（Q7）= 最小，只镜像 RobotState 实际用字段

- proto `S0Mode` / `S0ControlLink` / `S0OutputMode` / `S0SafetyState` =
  镜像 fbs 对应 enum；**fbs↔proto 转换器**沿 B4 `ActionKind` 三层同步纪律
  （fbs enum ↔ proto enum 严格同步，exhaustive match 锁）。**不**引超过实际
  字段需要的复杂度（fault_code 用 uint32 透传，不 enum 化）。

### D7 auth（reviewer 收紧 2 + Q5）= 延续 C1，**显式标非安全保证**

- GetState/StreamState **首版延续 C1**：`GetStateRequest` / `StreamRequest`
  **不含 auth / token 字段**；服务端 **不读、不校验**任何 metadata token（沿
  TaskHandleRef read RPC 现状）。**无 token 与任意 token 结果完全一致**——
  token 根本不参与 read 路径（reviewer round 2 同步 5：别写得像 token 真上 wire）。
- **Spec / CHANGELOG 必须写清**（reviewer 收紧 2）：
  - CLI / client 任何 token gate **≠ 服务端鉴权**（read 路径无 token 概念）。
  - `read_state` cap **暂不 enforce**（design §5.4 写了 cap，但本版不实现该安全
    属性）。
  - 后续 metadata auth 是**独立候选 / round**（不在候选 B）。
- **不得**把 "read_state cap" 写成已实现安全属性（D7 hard）。

### D8 schema-id（Q8）= **不**需新 fbs / schema_id

- `RobotState` 是 **proto-over-tonic response**；gateway 从 fbs `JointState`
  转 proto `RobotState`（进程内转换）。**不**进 `robot-msg` schema registry，
  **不**加 fbs 平行表 / 新 `SCHEMA_*`。

### D9 错误码口径（D3b 重写 + reviewer 收紧 5）

GetState（+ StreamState 开流时同口径）的 tonic `Status` code：

| 情况 | code | 说明 |
|---|---|---|
| `hardware_id` 缺失 / 空 | `InvalidArgument` | 请求语义错（D3 必填非空）|
| `hardware_id` **未知**（不在 config 声明的 hardware 池）| `NotFound` | **禁默默 fallback 到第一个**（AC-9 hard）|
| `hardware_id` 已知但 cache **无首帧** | `Unavailable` | 服务当前未 ready，**非**请求语义错 |
| `hardware_id` 已知 + cache 有 sample | OK | 返 `RobotState` |
| 已知但配置明确禁止该 hardware export | `FailedPrecondition` | **仅此情况**用 FP；候选 B 首版无此配置开关，预留语义 |

- **FailedPrecondition 不做默认口径**（reviewer 收紧 5）—— 只在"请求语义满足
  但配置明确禁止该 hardware state export"时用。

### D10 gateway hardware 列表来源（reviewer round 2 阻断 1）= `--config <PATH>`

round 2 D4 写"按 config 每个 `[[hardware]].id` 建常驻订阅池"，但当前
`robot_rpc_gateway`（`bin/robot_rpc_gateway.rs` `Args` = robot-id / listen-addr /
idempotency-* / rr-timeout-grace / rpc-roles-toml）**没有 config/hardware 输入**
→ AC-9 无法实施。**拍定来源 = `--config <PATH>`（robot config TOML，即 s1_daemon
已吃的同一份）**，理由：

- **单一真相**：gateway 订阅的 hardware 集 = robot 实际 `[[hardware]]` 声明，
  AC-9"只订 config 声明的 hardware"直接对齐；不选 `--hardware-id` 多次传（要手动
  跟 config 同步，易漂移）。
- **零新依赖 / 零越层**：`robot-rpc` **已**有 `serde` + `toml`（rpc_roles.toml
  用）→ minimal local 解析一个**宽松** struct（`{ robot: {id}, hardware:
  [{id}] }`，未知字段忽略），只取 `[[hardware]].id`。**不** dep `robot-s0`
  （避免把 RT daemon crate 拖进 gateway 树）、**不**把 `config_loader` 抽成共享
  crate（避免动 `robot-s0` → 守"S0/S1 daemon 0 改"硬边界）。
- **`--config` 可选（B4 兼容）**：缺省（无 `--config`）→ `state_hardware_ids`
  空 → state 订阅池空 → 所有**非空 `hardware_id`** 请求返 `NotFound`（**缺/空
  `hardware_id` 仍先按 D9/AC-3 返 `InvalidArgument`**——校验早于池查找）；既有
  clear-estop-only gateway 调用（`clear_estop_e2e` 等不传 `--config`）**0 破**。
  present → 按 `[[hardware]].id` 建常驻池。
- **topic prefix 权威仍 `--robot-id`**：state topic =
  `/<robot_id>/s0/<hardware_id>/state`（实测 `robotctl.rs:239` 同款；`robot_id`
  取 `--robot-id`，`hardware_id` 取 config 列表）。**fail-fast**：`--config`
  present 且 `[robot].id != --robot-id` → 启动拒（exit 2），防 config / flag
  错配静默订错 topic。

## Acceptance Criteria（round 3）

- **AC-1**：proto additive —— `service RobotControl` append GetState/StreamState
  2 rpc + RobotState/GetStateRequest/StreamRequest message + 4 镜像 enum；既有
  6 method diff = 0（字段/字段号/语义不变）；fbs 0 退化（`schema_lint` +
  `cargo test -p robot-msg`）+ 既有 6 RPC e2e（`task_rpc_e2e`）0 退化（PN1：
  **无**独立 `schema_compat` 命令）。
- **AC-2**：`GetState(GetStateRequest{hardware_id})` → gateway 读该 hardware 的
  **常驻 cache**（D4 pool，**非**临时订阅）→ `JointState`-fbs→`RobotState`-proto
  → 返。字段等价（mode/control_link/output_mode/safety/fault/positions/layout/
  stamp 跟 published JointState 一致）。
- **AC-3**：错误码口径（D9）—— 三条各一 e2e case：
  - `hardware_id` 缺/空 → `InvalidArgument`。
  - `hardware_id` unknown（不在 `--config` 声明池）→ `NotFound`。
  - **`Unavailable` 写实场景（reviewer round 2 同步 4）**：`--config` 声明某
    `hardware_id`（gateway 建了该 hardware 的常驻订阅），但 e2e **故意不启动**
    对应 `FakeStatePublisher` → 常驻池 cache **无首帧** → `GetState(该 id)` 返
    `Unavailable`（**非** `NotFound`：hardware 已知，只是还没数据）。
- **AC-4**：`StreamState(StreamRequest{hardware_id, min_interval_ms})` →
  server-streaming RobotState；**节流实测**：min_interval_ms=50 时输出间隔
  ≥ ~50ms（不是 S0 的 500Hz）；client cancel / gateway shutdown → 流清理
  （沿 events_subscribe drop→stop）。
- **AC-5**：`min_interval_ms < floor(20)` → clamp 到 20ms（D5a）。
- **AC-6**：auth（reviewer round 2 同步 5）—— **无 token** 与**任意 metadata
  token**（viewer/operator/maintainer，甚至垃圾串）调 GetState/StreamState
  **结果完全一致**：request 无 auth 字段、服务端不读不校验 token（C1
  unauthenticated read；D7）。e2e 至少跑"无 token"+"有 token"两路断言**同结果**；
  e2e 注释 + CHANGELOG 明标**非安全保证**，**不得**宣称 `read_state` cap 已
  enforce、**不得**写得像 token 真上 wire 影响鉴权。
- **AC-7**：stamp_ns 透传 S0 published 值（**非** gateway 自生 clock）—— 同一
  fixed published state 下 RobotState.stamp_ns 确定（沿候选 A E1 无 self-clock）。
- **AC-8**：robotctl **0 改动**（hard boundary；diff = 0）。
- **AC-9**：`--config`（D10）声明 N hardware 时，gateway **只订阅 config 声明的
  `hardware_id`**；请求未声明的 hardware → `NotFound`，**禁默默 fallback 到第一个**
  （hard；e2e：2 hardware `--config`，请求第 3 个不存在的 id → NotFound）。
- **AC-Q**（PN1 措辞具体化）：fmt + clippy(-D warnings) + `cargo test --workspace`
  + 4 既有 smoke 0 退化 + GetState/StreamState cross-process e2e + cargo deny
  baseline（未新增 violation）。**fbs schema 本轮不改** → 用
  `cargo run -p robot-msg --bin schema_lint` + `cargo test -p robot-msg` 确认
  fbs **0 退化**（**非**独立 `schema_compat` 命令）；**proto additive 兼容**由
  prost build 成功 + 既有 6 RPC e2e（`task_rpc_e2e` 0 退化）覆盖。

## round 1 拍板结果（reviewer round 1 拍定；2026-06-03）

| # | 拍板项 | 拍定 |
|---|---|---|
| **D3** | GetState 签名（design-freeze diff）| **(b)** `GetState(GetStateRequest{hardware_id})`，hardware_id 必填非空；CHANGELOG 登记（`docs/00_design.md` 不改）|
| D1a | RobotState 含 heartbeat rate? | **不含**（最小集；后续 additive 加）|
| D4a | GetState 订阅生命周期 | **常驻订阅池**（启动建 per-hardware 持久订阅 + cache；GetState/StreamState 读 cache，不临时订阅）|
| D5a | StreamState 节流 | 默认 **50ms** / floor **20ms**（< 20 clamp 不拒）|
| D3b | 错误码口径 | 缺/空 `InvalidArgument` / unknown `NotFound` / 无首帧 `Unavailable`；FP 不做默认（见 D9）|

**全部 round 1 拍定，无遗留待拍板项**；多 hardware 不再是"错误"（hardware_id
必填后，错误拆成 InvalidArgument / NotFound / Unavailable，D9）。

## hard boundaries（实施期不破）

B4 frozen 6-RPC schema 0 改（additive）· §6.7 0 触动（只读）· robotctl 0 改动
（D7 CLI hard）· design.md 0 改（GetState 签名走 CHANGELOG 登记）· **S0/S1
daemon 0 改**（D10 gateway minimal local 解析 config，**不**抽 `robot-s0`
`config_loader`、**不** dep robot-s0）· **gateway `--config` 加法兼容**（D10 可选；
缺省不破 B4 既有 clear-estop-only 调用）· 候选 A 三条固定前置（A4 / **C1 延续并
显式标非安全保证** / D6 不涉）· auth 不得宣称 read_state 已 enforce。

## 后续（RIPER）

Spec round 1 → reviewer round 1（D3/D1a/D4a/D5a/D3b 5 项拍板）✅ →
Spec round 2 → reviewer round 2（3 阻断 + 2 同步修订）✅ →
Spec round 3 → reviewer round 3 **ACK 进 Plan**（+ 非阻断文案收窄已修，commit
`5df51ff`）✅ → Plan round 1 → reviewer Plan round 1（1 阻断 PB1 + 3 拍板
PD1-3 + 1 非阻断 PN1）✅ → **Plan round 1 amend（本文，全吸收）→ 待 reviewer
Plan Approved** → (1/4)..(4/4) 实施。**reviewer 明确"批准 Plan 后才进实现"**——
Plan Approved 前不开 (1/4)。

## Plan

> RIPER Plan round 1（+ Plan round 1 amend：reviewer 1 阻断 PB1 + 3 拍板
> PD1-3 + 1 非阻断 PN1 全吸收，详顶部「Plan round 1 amend 记录」）。4 段 commit
> 拆分。本节把 Spec 落成可执行 commit 序列。**待 reviewer Plan Approved**；批准
> 后才进 (1/4)。

### P.0 工作基线

- **HEAD = `5df51ff`**（Spec round 3 + 文案收窄 land）。candidate B docs 前缀：
  CodeMap `d19a92c`/`f282384` + Spec r1 `84edc5f` / r2 `d08bd09` / r3 `91bf6d8`
  / 文案 `5df51ff`。candidate A 完结 = `25c2feb`。
- 全程 nightly；**每 commit 前** `cargo fmt --all` + `cargo clippy --all
  --all-targets -- -D warnings` 零警告。
- **0 新依赖**：proto（tonic build）/ iceoryx2 / tokio / std::thread / serde +
  toml 全部已在 robot-rpc dep；候选 B 不扩 `[dependencies]`、不引新 crate
  → cargo deny **未新增 violation**。

### P.1 commit 节奏总览

4 段 feat + 1 summary（reviewer ACK 拆分；每段独立过 Quality Gate）：

| 段 | 内容 | 主要文件 | 交付 |
|---|---|---|---|
| (1/4) | proto additive + 4 enum mirror + fbs→proto 转换器 | `proto/robot_control.proto` + 新 `src/state_convert.rs` | feat 1 |
| (2/4) | `--config` + `GatewayConfig.state_hardware_ids` + 常驻 state 订阅池 + GetState handler | `bin/robot_rpc_gateway.rs` + `gateway.rs` + 新 `src/state_pool.rs` | feat 1 |
| (3/4) | StreamState bridge + 节流（per-RPC tick task，无底层 subscriber）| `gateway.rs` + `src/state_pool.rs`（stream 侧）| feat 1 |
| (4/4) | cross-process e2e（`tests/state_rpc_e2e.rs`）+ summary + CHANGELOG + CLAUDE.md | `tests/` + `docs/` | feat 1 + docs 1 |

commit chain（hash 实施期填）：(1/4) `TBD` / (2/4) `TBD` / (3/4) `TBD` /
(4/4) feat `TBD` + summary docs `TBD`。

#### P.1.1 reviewer Plan round 锁点对账（5 锁点）

| # | reviewer 锁点 | Plan 落点 |
|---|---|---|
| 1 | `--config` 解析精确到错误码/exit code（invalid TOML / 缺 `[robot].id` / `[robot].id != --robot-id` 都 exit 2）+ `[[hardware]]` 空是否允许"明确" | §P.2.2.a（全部 → **exit 2** config 类，沿既有 rpc-roles-toml load fail 同路径）+ **决策：`[[hardware]]` 空 = 允许**（`config_loader` 既有 `empty_hardware_and_sensor_arrays_default_ok` 已证空 hardware 合法）→ P.3 D-P1 |
| 2 | 常驻 pool = 底层 subscriber 唯一来源；StreamState per-RPC 只 tick/stream task | §P.2.2.c（pool 建唯一 subscriber）+ §P.2.3.a（per-RPC 只 spawn tick task，**0** iceoryx2 subscriber）|
| 3 | e2e 覆盖三错误码（缺/空 InvalidArgument / unknown NotFound / 无首帧 Unavailable）；首版不测 FP | §P.2.4.b AC-3 三 case + **无 FP case** |
| 4 | AC-6 按"无 token / 任意 metadata token 同结果"测，不写 read_state cap enforce | §P.2.4.b AC-6 两路断言同结果 + 注释/CHANGELOG 标非安全保证 |
| 5 | hard boundary：robotctl 0改 / S0/S1 daemon 0改 / design.md 0改 / 既有 6 RPC 0改 | 每段 §Quality Gate + §风险锚点逐项断言 diff=0 |

### P.2 工作流（按 commit 顺序）

#### P.2.1 [commit (1/4)] proto additive + enum mirror + fbs→proto 转换器

##### P.2.1.a `proto/robot_control.proto` append（AC-1）
- `service RobotControl`（L94-103 当前 6 method）**末尾 append** 2 rpc（**不改既有 6 行**）：
  `rpc GetState (GetStateRequest) returns (RobotState);` +
  `rpc StreamState (StreamRequest) returns (stream RobotState);`。
- append 3 message：`RobotState`（D1 字段）/ `GetStateRequest{string hardware_id=1}`（D3）/ `StreamRequest{string hardware_id=1; uint32 min_interval_ms=2}`（D2）。
- append 4 enum（**number 与 fbs 1:1**；proto 零值约定 `*_UNSPECIFIED=0` 对齐 fbs `Unknown=0` / `TorqueUnknown=0`）：`S0Mode`(UNSPECIFIED/SAFE/READY/ENABLED/FAULT/STOPPING) / `S0ControlLink`(UNSPECIFIED/OK/LOST) / `S0OutputMode`(UNSPECIFIED/ACTIVE/HOLD/ZERO/STOP) / `S0SafetyState`(TORQUE_UNKNOWN=0/TORQUE_ON/STOP_COMMANDED/TORQUE_OFF_CONFIRMED/STOP_FAILED/ADAPTER_UNCERTAIN)。
- **头注 method 数 6→8**。

##### P.2.1.b 新 `src/state_convert.rs` — fbs→proto 纯转换器（D6）
- **别名硬约束（reviewer Plan round 1 阻断 PB1）**：proto 4 enum 与 fbs 4 enum
  **同名**（都叫 `S0Mode` 等，fbs 来自 `robot_msg::msg`，proto 来自
  `crate::proto`）。**禁裸用同名**；import 一律加后缀别名：
  ```rust
  use robot_msg::msg::{S0Mode as S0ModeFb, S0ControlLink as S0ControlLinkFb,
                       S0OutputMode as S0OutputModeFb, S0SafetyState as S0SafetyStateFb};
  use crate::proto::{S0Mode as S0ModeProto, S0ControlLink as S0ControlLinkProto,
                     S0OutputMode as S0OutputModeProto, S0SafetyState as S0SafetyStateProto};
  ```
  转换 fn 一律 `s0_mode_fb_to_proto` / `s0_control_link_fb_to_proto` /
  `s0_output_mode_fb_to_proto` / `s0_safety_state_fb_to_proto`（`*Fb -> *Proto`）。
- 4 enum 转换器（沿 `events_subscribe.rs:419/433/448` exhaustive-match 范式）：`*Fb` enum → `*Proto` enum，**exhaustive match**（fbs 加 variant 不更新 → 编译失败，编译期锁）。
- `joint_state_fb_to_robot_state(fb: &JointState, robot_id: &str, hardware_id: &str) -> RobotState`：position_rad（fbs vector→Vec<f64>）/ layout_id / **stamp_ns 透传 S0 published 值**（非自生 clock，AC-7）/ mode·control_link·output_mode·safety_state（经 4 转换器）/ **fault_code = `s0_fault_code()` ubyte `as u32` 透传**（**不** mirror `S0FaultCode` enum，D6）/ robot_id·hardware_id（调用方传入）。

##### P.2.1.c lib unit（本 commit land）
- 4 enum 转换器逐 variant（含 UNSPECIFIED 边界）+ `joint_state_fb_to_robot_state` 手搓 fbs（`build_helpers`）断言字段等价 + stamp 透传 + fault_code 数值透传。

##### P.2.1.d Quality Gate
- fmt + clippy(-D warnings) clean；`cargo test --workspace` 不退化；fbs 未改 → `cargo run -p robot-msg --bin schema_lint` + `cargo test -p robot-msg` 0 退化；proto additive 兼容由 prost build + 既有 RPC e2e 覆盖（PN1）。

##### P.2.1.e 风险锚点（锁点 5）
- 既有 6 method/message diff=0（只 append）；4 enum 全新（不改既有 proto enum）；robotctl/S0/S1/design.md 0 改。

#### P.2.2 [commit (2/4)] `--config` + GetState handler + 常驻订阅池

##### P.2.2.a `bin/robot_rpc_gateway.rs` 加 `--config <PATH>`（锁点 1 / D10）
- `Args` 加 `config: Option<PathBuf>`；help + 头注 CLI/退出码块同步。
- present 时 minimal local 解析（宽松 struct `{ robot:{id}, hardware:[{id}] }`，未知字段忽略；复用既有 serde+toml）。
- **错误码（全 exit 2，沿既有 rpc-roles-toml load fail 同路径）**：`--config` 读失败 / invalid TOML / 缺 `[robot].id`（serde 必填解析失败）/ **`[robot].id != --robot-id`（fail-fast）** 均 exit 2。
- **`[[hardware]]` 空 = 允许**（合法；`state_hardware_ids` 空 → 池空 → 非空 hardware_id 请求 NotFound；缺/空 hardware_id 仍先 InvalidArgument）→ **P.3 D-P1**。
- 提取 `state_hardware_ids: Vec<String>` 喂进 `GatewayConfig`。

##### P.2.2.b `GatewayConfig.state_hardware_ids` 字段 + validate
- `GatewayConfig` 加 `state_hardware_ids: Vec<String>`（`loopback_default` 给空）；`validate`：每 id 非空（空 id 拒，`ConfigError` 新 variant），允许整体空。

##### P.2.2.c 新 `src/state_pool.rs` — 常驻 per-hardware 订阅池（锁点 2 / D4）
- 启动：每个 id 一条 `open_sub::<JointState>` on `/<robot_id>/s0/<id>/state`（`TopicClass::S0State`）+ 一条 std::thread 循环读 → 更新该 hardware 的 **keep-last cache（owned JointState sample bytes）**。**唯一底层 subscriber 来源**。
- 读路径（GetState/tick）：从 cache 取 owned bytes → `flatbuffers::root::<JointState>` → `joint_state_fb_to_robot_state`（convert-on-read，省 500Hz 转换）。
- 生命周期：startup 建 / shutdown stop+join（沿 B4 shutdown 顺序 mark_shutting_down → ... → pool stop+join）。

##### P.2.2.d `gateway.rs` GetState handler（错误码顺序硬锁 / D9 / AC-3）
- **顺序即口径**：① `hardware_id` 空 → `InvalidArgument`（校验**早于**池查找）② 不在 `state_hardware_ids` → `NotFound`（**禁 fallback**，AC-9）③ 池中但 cache 无首帧 → `Unavailable` ④ 有首帧 → convert → `Ok(RobotState)`。
- auth：**不读不校验 token**（D7）。

##### P.2.2.e lib unit（config 解析 4 错误码 + GetState 4 分支）
##### P.2.2.f Quality Gate（同 P.2.1.d 全套）
##### P.2.2.g 风险锚点：既有 clear-estop handler / RR I/O 链 0 改（锁点 5）。

#### P.2.3 [commit (3/4)] StreamState bridge + 节流

##### P.2.3.a `gateway.rs` StreamState handler（锁点 2 / D5）
- per-RPC **只** spawn 一条轻量 async tick task（tokio）：按 clamp 后 interval 读 pool cache → convert → mpsc → `Box::pin` Stream（沿 `events_subscribe.rs` Stream 范式）；**绝不**新建底层 iceoryx2 subscriber。
- 开流前校验同 GetState：`hardware_id` 空 → InvalidArgument；不在池 → NotFound（D9）。

##### P.2.3.b 节流（D5a）
- `min_interval_ms==0` → 50ms；`0<x<20` → clamp 20ms（不拒）；`>=20` → 原值。keep-last（丢中间帧，不逐 S0 sample）。

##### P.2.3.c 清理
- client cancel / gateway shutdown → tick task stop（drop→stop，沿 events_subscribe）；**底层 pool 订阅不随单 RPC 关**。

##### P.2.3.d unit + e Quality Gate + f 风险锚点（同上全套）

#### P.2.4 [commit (4/4)] cross-process e2e + summary

##### P.2.4.a harness（复用 `tests/common/`）
- 新 `tests/state_rpc_e2e.rs`（`[[test]] required-features=["client"]`，**A4 默认随 workspace 跑**）。
- 复用 `common/`：`subprocess`（spawn s0/s1/gateway——**扩 gateway spawn helper 传 `--config`**）+ `fake_state::FakeStatePublisher::spawn`（AC-2 happy；AC-3 Unavailable = **不** spawn）+ `ready` + `case_ctx`。
- **进程隔离（锁点 5 + 防 flake；D-P3 拍定）**：robot_id = **`state_e2e_<pid>_<case>`**（按 pid + case 唯一化，避残留 iceoryx2 topic 互扰）+ distinct gateway port `127.0.0.1:50061`（避 task_rpc_e2e 50051）+ GLOBAL_LOCK 串行 + `--test-threads=1`（沿 task_rpc_e2e）。

##### P.2.4.b 测例（AC-2..AC-9）
- AC-2 happy：FakeStatePublisher 喂固定 state → GetState → 字段等价（+ AC-7 stamp 透传）。
- AC-3 三错误码（锁点 3）：缺/空 `InvalidArgument` / unknown `NotFound` / **声明但不启 FakeStatePublisher → `Unavailable`**（写实，同步 4）。**无 FP case**。
- AC-4 节流 + AC-5 clamp（**D-P2 拍定数值**，测试判据不改协议）：
  - `min_interval_ms=50`：固定 **window=600ms** 内帧数 **≤ ⌈600/50⌉+1 = 13** 且相邻帧间隔 **≥ 40ms**。
  - clamp 路径 `min_interval_ms<20`（如 10→clamp 20）：固定 **window=240ms** 内帧数 **≤ 13** 且相邻帧间隔 **≥ 16ms**。
- AC-6 auth（锁点 4）：**无 token + 有 token 两路断言同结果**；注释 + CHANGELOG 标非安全保证；**不**宣称 read_state cap enforce。
- AC-9 多 hardware：2 hardware `--config`，请求第 3 个不存在 id → `NotFound`（禁 fallback）。

##### P.2.4.c summary + CHANGELOG + CLAUDE.md
- `docs/handoffs/v0.5_get_stream_state_summary.md` + CHANGELOG（design-freeze diff D3 + D10 `--config` 加法 + C1 延续）+ CLAUDE.md 状态 + handoffs README。

##### P.2.4.d Quality Gate（候选 B 完结硬 gate；AC-Q 全集）
- fmt + clippy + `cargo test --workspace`（含新 state_rpc_e2e 默认跑）+ 4 既有 smoke 0 退化 + cargo deny baseline（未新增 violation）+ fbs 0 退化（`schema_lint` + `cargo test -p robot-msg`）+ proto additive 兼容（prost build + `task_rpc_e2e` 0 退化）（PN1）。

##### P.2.4.e 风险锚点
- robotctl 0 改（AC-8）/ S0·S1·design.md 0 改 / 既有 6 RPC + task_rpc_e2e 0 退化。

### P.3 Plan 拍板项（reviewer Plan round 1 全部拍定）

| # | 拍板项 | 拍定 |
|---|---|---|
| D-P1 | config present 但 `[[hardware]]` 空是否允许 | **ACK 允许**（合法空配置 = 空 state 池；非空 hardware_id → NotFound，缺/空 hardware_id 仍先 InvalidArgument；兼容 B4 既有 gateway 调用）|
| D-P2 | StreamState 节流 e2e 判据 | **拍定**（测试判据，不改协议）：`interval=50`→ window=600ms 帧 ≤13 + 相邻 ≥40ms；clamp 路径（<20）→ window=240ms 帧 ≤13 + 相邻 ≥16ms |
| D-P3 | state_rpc_e2e 进程隔离 | **ACK + 改进**：robot_id=`state_e2e_<pid>_<case>`（避残留 topic 互扰）+ port `127.0.0.1:50061` + GLOBAL_LOCK + --test-threads=1 |

### P.4 Plan-Execute diff 登记（实施期填）

（占位：实施中任何偏离 Plan 的点按 `#N | 阻断/非阻断 | 落点 | 描述` 登记，沿候选 A §P.4 体例。）
