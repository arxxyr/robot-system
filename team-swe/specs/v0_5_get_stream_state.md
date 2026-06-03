# Spec · v0.5 候选 B — `GetState` / `StreamState` RPC（round 1）

> RIPER Spec round 1。前置 = [`team-swe/codemap/v0_5_get_stream_state.md`](../codemap/v0_5_get_stream_state.md)
> （reviewer CodeMap 方向 ACK + 4 收紧点 + 8 问题默认方向 + proto 缺口
> additive 结论）。本 round 把方向落成可拍板设计 + AC + hard boundary +
> **1 条 design-freeze diff（Q3 GetState 签名）**。待 reviewer round 复核。

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
- `crates/robot-rpc/src/gateway.rs`：`GetState` / `StreamState` 两个 trait
  method impl。
- `crates/robot-rpc/src/`（新模块或扩既有）：
  - gateway 端 **S0 state topic 订阅**（`open_sub::<JointState>` +
    `TopicClass::S0State`）。
  - **`JointState`-fbs → `RobotState`-proto 转换器**（类比 B4
    `events_subscribe.rs::action_*_fb_to_proto`）。
  - `StreamState` 的 **state_subscribe sync↔async 桥**（复用
    `events_subscribe.rs` 范式：per-RPC spawn 订阅线程 + `mpsc::Receiver` +
    `Box::pin` Stream + drop→stop）+ **节流**（keep-last + `min_interval_ms`
    tick，**不**逐 S0 sample 转发）。
- `docs/CHANGELOG.md`：登记 **design-freeze diff**（Q3 GetState 签名，见 §
  设计决策 D3）+ C1 延续（read RPC 非安全保证）。
- `crates/robot-rpc/tests/`：cross-process e2e（GetState happy + StreamState
  节流 + 多 hardware FailedPrecondition / hardware_id 选择）。

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
    // heartbeat rate：D1a 待拍（含 vs 不含）
}
```

- **字段全部来自 `JointState` fbs 的 s0_* accessor**（robotctl `print_snapshot`
  读的同一批：`s0_mode` / `s0_control_link` / `s0_output_mode` /
  `s0_safety_state` / `s0_fault_code` + `position_rad` / `layout_id` /
  `stamp_ns`）。`stamp_ns` 透传 S0 published 值（**非 gateway 自生 clock**，
  避免引入 self-clock）。
- **D1a 待拍板**：是否含 heartbeat rate（`/<robot_id>/s0/heartbeat` 的
  `tick_rate_hz`）。倾向**含**（operator 关心 S0 是否在跳），但要 gateway 多订
  一个 heartbeat topic。round 1 默认 **不含**（最小集；rate 可后续加，additive
  字段号兼容）。

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
message GetStateRequest { string hardware_id = 1; }
rpc GetState (GetStateRequest) returns (RobotState);
```

- **这偏离 frozen design §5.3 的 `Empty` 签名** → **必须 CHANGELOG 登记**
  （`docs/00_design.md` 不改；走"要改走 CHANGELOG"路径）。
- **round 1 拍板项 D3**（2 选 1）：
  - **(b) 推荐（reviewer 倾向）**：`GetState(GetStateRequest{hardware_id})` —
    显式选 hardware，多 hardware 无歧义。**登记 design-freeze diff**。
  - (a) fallback：保留 `GetState(Empty)`，gateway 端**仅当配置唯一 hardware**
    时返该 hardware；多 hardware → `FailedPrecondition`（"specify hardware via
    StreamRequest / 用 StreamState"）。不偏离 §5.3 但 GetState 多 hardware 不可用。
- **StreamState 同理**：`StreamRequest.hardware_id` 显式选（D2 已含）。

### D4 gateway 直订 S0 state topic（reviewer 收紧 1）= 权威源 + 生命周期

- **topic 形式**：`/<robot_id>/s0/<hardware_id>/state`（`TopicClass::S0State`，
  跟 robotctl status / s1 anchor 同一 topic）。
- **hardware_id 选择**：来自 `GetStateRequest` / `StreamRequest`（D2/D3）。
- **订阅生命周期**：
  - **GetState（unary）**：每次调用临时订阅 + 读 latest + drop（或 gateway
    持长生命周期 per-hardware 订阅池，round 1 倾向**临时订阅**简单 + 沿 robotctl
    status 单发模式；**注意** iceoryx2 late-subscriber 单发竞态〔候选 A E1〕——
    GetState 要么 gateway 持**常驻** per-hardware 订阅〔避免单发竞态〕，要么
    临时订阅 + 有界重试读到 sample。**D4a 待拍板**：常驻订阅池 vs 临时+重试）。
  - **StreamState（streaming）**：per-RPC spawn 一条 state_subscribe_thread
    （持久订阅，循环读，可靠拿 sample —— 沿候选 A "watch persistent subscriber
    reliable" 同理），`EventsSubscribeStream::drop → request_stop` 清理。

### D5 `StreamState` 节流（reviewer 收紧 4 + Q4）—— **硬决策，不留实现**

- S0 state 可能高频（design 注 JointState 500Hz+）。**StreamState 严禁逐 S0
  sample 裸转发**。
- **gateway keep-last + interval tick**：state_subscribe_thread 持最新 sample；
  按 `min_interval_ms` tick 输出最新值（keep-last，丢中间帧）。
- **默认下限**：`min_interval_ms = 0` 时用 gateway 默认 **50ms（20Hz）**；硬
  floor **20ms（50Hz）**（client 传 < 20 → clamp 到 20）。**D5a 待拍板**：默认
  50ms / floor 20ms 这两个数（reviewer 倾向 20ms 或 50ms 默认）。

### D6 enum 镜像（Q7）= 最小，只镜像 RobotState 实际用字段

- proto `S0Mode` / `S0ControlLink` / `S0OutputMode` / `S0SafetyState` =
  镜像 fbs 对应 enum；**fbs↔proto 转换器**沿 B4 `ActionKind` 三层同步纪律
  （fbs enum ↔ proto enum 严格同步，exhaustive match 锁）。**不**引超过实际
  字段需要的复杂度（fault_code 用 uint32 透传，不 enum 化）。

### D7 auth（reviewer 收紧 2 + Q5）= 延续 C1，**显式标非安全保证**

- GetState/StreamState **首版延续 C1**：服务端 **不**做 auth（沿 TaskHandleRef
  read RPC 现状），任何 token 都能读。
- **Spec / CHANGELOG 必须写清**（reviewer 收紧 2）：
  - CLI / client token gate **≠ 服务端鉴权**。
  - `read_state` cap **暂不 enforce**（design §5.4 写了 cap，但本版不实现该安全
    属性）。
  - 后续 metadata auth 是**独立候选 / round**（不在候选 B）。
- **不得**把 "read_state cap" 写成已实现安全属性（D7 hard）。

### D8 schema-id（Q8）= **不**需新 fbs / schema_id

- `RobotState` 是 **proto-over-tonic response**；gateway 从 fbs `JointState`
  转 proto `RobotState`（进程内转换）。**不**进 `robot-msg` schema registry，
  **不**加 fbs 平行表 / 新 `SCHEMA_*`。

## Acceptance Criteria（round 1 草案）

- **AC-1**：proto additive —— `service RobotControl` append GetState/StreamState
  2 rpc + RobotState/GetStateRequest/StreamRequest message + 4 镜像 enum；既有
  6 method diff = 0（字段/字段号/语义不变）；`schema_compat` / 既有 6 RPC e2e
  0 退化。
- **AC-2**：`GetState(GetStateRequest{hardware_id})`〔D3 拍 (b)〕→ gateway 订
  `/<robot_id>/s0/<hardware_id>/state` 读 latest → `JointState`-fbs→
  `RobotState`-proto → 返。字段等价（mode/control_link/output_mode/safety/
  fault/positions/layout/stamp 跟 published JointState 一致）。
- **AC-3**：多 hardware —— hardware_id 不存在于 config → `FailedPrecondition`
  （或 NotFound，待拍）。〔D3 拍 (a) 时：Empty + 多 hardware → FailedPrecondition〕。
- **AC-4**：`StreamState(StreamRequest{hardware_id, min_interval_ms})` →
  server-streaming RobotState；**节流实测**：min_interval_ms=50 时输出间隔
  ≥ ~50ms（不是 S0 的 500Hz）；client cancel / gateway shutdown → 流清理
  （沿 events_subscribe drop→stop）。
- **AC-5**：`min_interval_ms < floor(20)` → clamp 到 20ms（D5a）。
- **AC-6**：auth —— 任意 token（viewer/operator/maintainer）都能 GetState/
  StreamState（C1 unauthenticated read；D7）；e2e 注释 + CHANGELOG 标"非安全
  保证"。
- **AC-7**：stamp_ns 透传 S0 published 值（**非** gateway 自生 clock）—— 同一
  fixed published state 下 RobotState.stamp_ns 确定（沿候选 A E1 无 self-clock）。
- **AC-8**：robotctl **0 改动**（hard boundary；diff = 0）。
- **AC-Q**：fmt + clippy(-D warnings) + workspace test + 4 既有 smoke 0 退化 +
  GetState/StreamState cross-process e2e + cargo deny baseline + schema_compat。

## round 1 待拍板项（reviewer round 复核）

| # | 拍板项 | 候选 | 倾向 |
|---|---|---|---|
| **D3** | GetState 签名（**design-freeze diff**）| (a) Empty + 多 hardware FailedPrecondition / (b) GetStateRequest{hardware_id} | **(b)**（reviewer 倾向；登记 CHANGELOG）|
| D1a | RobotState 含 heartbeat rate? | (a) 不含〔最小〕 / (b) 含（多订 heartbeat topic）| **(a)**（additive 后续可加）|
| D4a | GetState 订阅生命周期 | (a) 临时订阅+有界重试 / (b) gateway 常驻 per-hardware 订阅池 | 待拍（(b) 避单发竞态但复杂；(a) 简单但需重试读）|
| D5a | StreamState 节流默认 | 默认 50ms（20Hz）+ floor 20ms（50Hz）| 待 reviewer 确认两个数 |
| D3b | 多 hardware 错误码 | FailedPrecondition / NotFound | **FailedPrecondition**（沿 invoke HardwareMismatch 同款）|

## hard boundaries（实施期不破）

B4 frozen 6-RPC schema 0 改（additive）· §6.7 0 触动（只读）· robotctl 0 改动
（D7 CLI hard）· design.md 0 改（GetState 签名走 CHANGELOG 登记）· S0/S1 daemon
0 改 · 候选 A 三条固定前置（A4 / **C1 延续并显式标非安全保证** / D6 不涉）·
auth 不得宣称 read_state 已 enforce。

## 后续（RIPER）

Spec round 1 → reviewer round（拍 D3 设计-freeze diff + D1a/D4a/D5a/D3b）→
Spec amend（如有阻断）→ Plan → 实施（倾向 commit 拆分：proto+enum+转换器 /
GetState handler+gateway 订阅 / StreamState bridge+节流 / e2e + summary）。
**本 round 不开实现。**
