# CodeMap · v0.5 候选 B — `GetState` / `StreamState` RPC（Pre-Research）

> **Pre-Research / 只摸图，不开实现。** 目标：摸清 design §5.3 两条只读
> RPC `GetState` / `StreamState` 落地的可复用面 + 新增面 + 待 Spec 拍板的
> 开放问题。**不是 Spec**（不下设计决策），不动任何代码。
>
> 边界（沿 v0.5_entry.md 8 条 hard boundaries）：不跳 reviewer / 不动 §6.7 /
> 不动 design.md / **不动 B4 frozen 6-RPC schema**（GetState/StreamState 是
> **additive** 新方法 + 新 message，不改既有 6 条）/ A→B→C→D→E 硬序。

## 0. 候选 B 是什么（design §5.3 权威）

`docs/00_design.md` §5.3 已 spec（**实现冻结设计，非改设计**）：

```proto
// 只读区
rpc GetState     (Empty)          returns (RobotState);
rpc StreamState  (StreamRequest)  returns (stream RobotState);
```

- 都是**只读** RPC（§5.4 `read_state` capability）。
- design §157 硬要求：`GetState` / state topic 必须报告 **`mode` /
  `control_link`（OK / Lost）/ `output_mode`（Active / Hold / Zero / Stop）**。
- **`RobotState` / `StreamRequest` message body 当前在 design.md / proto /
  任何代码里都未定义**（只有 §5.3 service 签名一行）—— 候选 B 的 Spec 要
  定义这两个 message（新 proto message，待拍板字段集）。

### 0.1 proto 现状 + 缺口（reviewer 复核点：补 message vs additive method）

`crates/robot-rpc/proto/robot_control.proto`：

- `service RobotControl` block = **lines 94-104**，当前 **= 6 条 B4 method**：
  - `rpc ClearEStop (ClearEStopCommand) returns (Ack);`（line 96）
  - `rpc InvokeTask (InvokeTaskCommand) returns (TaskHandle);`（line 99）
  - `rpc CancelTask (CancelTaskCommand) returns (Ack);`（line 100）
  - `rpc GetTaskStatus (TaskHandleRef) returns (TaskStatus);`（line 101）
  - `rpc StreamTaskEvents (TaskHandleRef) returns (stream TaskEvent);`（line 102）
  - `rpc ClearError (ClearErrorCommand) returns (Ack);`（line 103）
- **缺口**：service 里**没有** `GetState` / `StreamState` rpc 声明（proto
  comment **line 14-16** 明示"… GetState / StreamState … 留 v0.5+ Phase B
  后续 outline 拍板，**不在本 proto 出现**"）；**没有** `RobotState` /
  `StreamRequest` / `GetStateRequest` message。
- **结论：候选 B = additive method + message**（`service RobotControl`
  **append** 2 rpc + 新增 RobotState / 请求 message + enum mirror），**不是
  "补 message"**。proto 头注（line ~6 "当前 scope … 6 条 RPC" + §method 数
  = 6）需同步改 8。**additive** = 既有 6 method 字段 / 字段号 0 改（B4 frozen
  schema hard boundary 不破）。

## 1. reviewer 点名 5 块勘探结果

### ① robot-msg/schema 当前 state sample 与 Get/StreamState 可复用面

- S0 state 数据载体 = `JointState` fbs（`crates/robot-msg/schemas/joint.fbs`，
  `SCHEMA_JOINT_STATE = 0x0002`），高频 500Hz+ 发到
  `/<robot_id>/s0/<hardware_id>/state`。字段（**RobotState 直接复用面**）：
  - `layout_id: ulong` / `position_rad: [double]` / `stamp_ns: ulong`
  - `s0_mode: S0Mode`（enum）+ `s0_control_link` + `s0_output_mode` +
    `s0_safety_state` + `s0_fault_code`（即 design §157 要求的
    mode/control_link/output_mode + safety/fault）—— 见 robotctl
    `print_snapshot`（robotctl.rs:384-426）读这些 accessor。
- `Heartbeat` fbs（`SCHEMA_HEARTBEAT = 0x0301`，`/<robot_id>/s0/heartbeat`）：
  source_id / seq / tick_rate_hz / stamp_ns。
- **可复用结论**：`RobotState` proto 的字段几乎 = `JointState` 的 s0 字段子集
  + 可能 heartbeat 的 rate。需要 **`JointState`-fbs → `RobotState`-proto
  转换器**（新；gateway 端，类比 B4 `events_subscribe.rs` 的
  `action_outcome_fb_to_proto` 模式）。
- **S0Mode / control_link / output_mode / safety_state enum → proto enum**：
  需三层同步（fbs enum ↔ proto enum），沿 B4 `ActionKind` / `TaskState`
  三层 enum append 同款纪律（lib/proto/fbs 严格同步）。

### ② B4 gateway sync↔async bridge + Task RPC handler 模式

- `crates/robot-rpc/src/events_subscribe.rs`：`StreamTaskEvents` 的 sync↔async
  桥范式 —— `spawn_events_subscribe_thread`（per-RPC 起一条订阅线程）+
  `tokio::sync::mpsc::Receiver` + `EventsSubscribeStream`（`Box::pin` 成 tonic
  `Stream`）+ `EventsSubscribeStream::drop → request_stop`（in-flight stream
  清理）。**`StreamState` 直接复用此范式**（订 S0 state topic → push tonic
  stream），相当于一条 "state_subscribe_thread"。
- `crates/robot-rpc/src/task_handlers.rs`：unary handler 模式（validate →
  RR → decode → proto）。但 **GetState 不走 RR-to-s1**（见 ③）。
- `crates/robot-rpc/src/gateway.rs`：trait impl 把 RPC 转发到 task_handlers /
  events_subscribe。GetState/StreamState 加两个 trait method impl。

### ③ S0/S1 state publish/read 链路

- **S0 发布** state：`s0_pub_thread` 读 rt_snapshot → publish `JointState` +
  `Heartbeat` 到 `/<robot_id>/s0/<hardware_id>/state` + `/heartbeat`。
- **现有读端**：robotctl status/watch（iceoryx2-direct sub）+ s1_daemon
  anchor（`read_anchor_from_s0_state`）。
- **gateway 当前 0 订阅 state**（grep `gateway.rs` = 无 open_sub / JointState /
  S0State）—— B4 gateway 只做 RR-to-s1 + per-ActionId pubsub。**候选 B 新增
  面**：gateway 要**订阅 S0 state topic**（`open_sub::<JointState>` +
  `TopicClass::S0State`）才能 serve GetState/StreamState。
- **关键分叉**：GetState 的数据来自 **S0 state topic**（S0 发布），**不是**
  RR-to-s1（s1 是 coordinator，不持权威 robot state）。所以 GetState/StreamState
  的链路 = gateway 直订 S0 state topic（≠ B4 task RPC 的 RR-to-s1）。
- **多 bus / 多 hardware 复杂度**（v0.4 多 bus 语义）：S0 state 是
  per-hardware topic（`/<robot_id>/s0/<hardware_id>/state`）。GetState 要么
  按 `StreamRequest.hardware_id` 选一个，要么聚合多个 → **待 Spec 拍板**。

### ④ operator CLI 是否要同步加 robotctl get-state / stream-state

- robotctl **已有** status/watch（iceoryx2-direct 读 S0 state，robotctl.rs:239-426）
  —— **本地 operator 看 state 已覆盖**。
- GetState/StreamState RPC 的价值 = **远程 client**（dimos / web UI，gRPC 跨进程/跨机）
  读 state，跟 robotctl 本地 iceoryx2 读是两条路。
- **倾向**（待 Spec 拍板）：候选 B **不**急着加 robotctl get-state/stream-state
  子命令（会跟 status/watch 功能重叠，仅传输层不同）。若要加，是为"operator
  通过 gateway 远程读"或对称性 —— 留 Spec 决策；不是候选 B 主线必需。

### ⑤ read RPC auth：C1 非安全保证在 B 里如何处理 / 标注

- **沿候选 A C1**（§P.4 C1）：`GetTaskStatus` / `StreamTaskEvents`（`TaskHandleRef`
  无 CommandMeta）= 服务端 v0.3 **unauthenticated** read RPC；CLI token 仅
  client-side gate、不上 wire。
- design §5.3：`GetState(Empty)` / `StreamState(StreamRequest)` —— **Empty 无
  CommandMeta**，`StreamRequest` body 未定义（待 Spec；若不含 CommandMeta 则
  同 TaskHandleRef 无 auth）。design §5.4 说 read RPC 要 `read_state` cap，但
  B4 read RPC 实际未 enforce（C1）。
- **候选 B 的 auth 决策（待 Spec 拍板，2 选 1）**：
  - (a) **延续 C1**：GetState/StreamState unauthenticated read，token（若 CLI 加）
    仅 client-side gate，C1 非安全保证**继续标注**（read auth 留 v0.5+ metadata）。
  - (b) **给 read RPC 上 auth**：StreamRequest 带 CommandMeta（或走 tonic
    metadata token），gateway enforce read_state —— 但这会**首次**给 read RPC
    真鉴权，需 reviewer 明确（且 design §5.3 Empty/StreamRequest 倾向 (a)）。
  - **CodeMap 倾向 (a)**（最小偏离 + 沿 B4/C1 既有语义）；最终 Spec 拍。

## 2. 可复用面 vs 新增面（总表）

| 维度 | 可复用 | 新增（候选 B）|
|---|---|---|
| state 数据 | `JointState` fbs s0 字段（mode/link/output/safety/fault + pos + layout + stamp）| `RobotState` proto message（字段集待 Spec）|
| enum | fbs `S0Mode` 等 | proto `S0Mode` 等镜像 enum（三层同步纪律）|
| 转换 | B4 `events_subscribe.rs` `*_fb_to_proto` 模式 | `JointState`-fbs → `RobotState`-proto 转换器 |
| 订阅 | `robot_transport open_sub::<JointState>(TopicClass::S0State)` | **gateway 首次**订 S0 state topic |
| 流式桥 | `events_subscribe.rs` spawn_thread + mpsc + Box::pin Stream + drop-stop | `StreamState` 的 state_subscribe_thread（同范式）|
| handler | `gateway.rs` trait impl + `task_handlers.rs` unary 模式 | GetState（unary）+ StreamState（server-streaming）2 impl |
| 限频 | — | StreamState 节流（S0 500Hz+ 不能裸流）—— `StreamRequest.min_interval_ms`？待 Spec |
| auth | C1 read RPC 无鉴权范式 | （倾向延续 C1；待 Spec）|
| CLI | robotctl status/watch（本地 state 已覆盖）| 倾向**不**加 get-state/stream-state（待 Spec）|

## 3. 待 Spec 拍板的开放问题（CodeMap 只列，不决）

1. **RobotState 字段集**：mirror JointState s0 全字段？含 positions / layout_id /
   stamp_ns？含 heartbeat rate？per-hardware 还是聚合？
2. **StreamRequest 字段**：hardware_id（选哪个 bus/arm）？`min_interval_ms` 节流？
   `include_positions` 之类裁剪？
3. **多 bus / 多 hardware**：GetState(Empty) 在多 hardware 下返哪个 / 全部？
   （v0.4 多 bus 语义）。
4. **StreamState 限频**：S0 500Hz+，stream 必须节流（gateway 端 throttle /
   keep-last + min_interval_ms）—— 防 tonic 流爆。
5. **auth**：延续 C1（unauthenticated read + client-side gate）还是首次给 read
   RPC 上鉴权（StreamRequest 带 CommandMeta / tonic metadata）。CodeMap 倾向延续 C1。
6. **CLI**：是否加 robotctl get-state / stream-state（CodeMap 倾向不加，
   status/watch 已覆盖本地；RPC 面向远程 client）。
7. **enum 三层同步**：S0Mode / control_link / output_mode / safety_state 的
   proto 镜像 + fbs↔proto 转换（沿 B4 ActionKind 三层纪律）。
8. **schema-id**：RobotState 若需 fbs 平行表（B4 双层 schema 模型），分配新
   schema_id；若 GetState 纯 proto-over-tonic（gateway 内部 fbs→proto 不落
   独立 fbs），则不需要。待 Spec。

## 4. hard boundaries（候选 B 实施期不得破）

- **B4 frozen 6-RPC schema 0 改**：GetState/StreamState 是 proto **additive**
  新方法 + 新 message（`RobotControl` service append），不动既有
  ClearEStop/InvokeTask/CancelTask/GetTaskStatus/StreamTaskEvents/ClearError
  字段 / 字段号。
- **§6.7 0 触动**：GetState/StreamState 只读，不改任何安全状态 / latch /
  EStop / output mode。
- **不动 design.md**：候选 B 是**实现** §5.3 已 spec 的 GetState/StreamState
  （定义 RobotState/StreamRequest body 属"实现冻结接口的细节"，走 Spec +
  CHANGELOG，不改 §5.3 签名）。
- **v0.5 operator 面三条固定前置**（候选 A 封存）：A4（task_rpc_e2e 默认运行）/
  C1（read RPC token 非安全保证）/ D6（--dry-run 仅 clear-estop）—— 候选 B 若
  延续 C1，需在 Spec 显式标注 GetState/StreamState 同属 C1 类。

## 5. 下一步（RIPER）

CodeMap → reviewer 复核 → **Spec round 1**（定 RobotState/StreamRequest 字段 +
auth 决策 + 多 bus 语义 + 限频 + CLI 取舍）→ reviewer round → Plan → 实施。
**本 CodeMap 不进 Spec / 不开实现**；等 reviewer 对 5 块勘探 + 8 开放问题的
方向 ACK 后再起 Spec。
