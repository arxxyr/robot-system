# Spec · v0.5 候选 B — `GetState` / `StreamState` RPC（round 2）

> RIPER Spec round 2（round 1 方向 ACK + reviewer 5 项拍板已吸收）。前置 =
> [`team-swe/codemap/v0_5_get_stream_state.md`](../codemap/v0_5_get_stream_state.md)
> （reviewer CodeMap 方向 ACK + 4 收紧点 + 8 问题默认方向 + proto 缺口
> additive 结论）。本 round 把 round 1 全部待拍板项落到拍定 + **1 条
> design-freeze diff（Q3 GetState 签名）**。待 reviewer round 2 复核 → Plan。

## Spec round 2 amend 记录（2026-06-03；reviewer round 1 复核后）

reviewer round 1 ACK 方向但要 5 项拍板/修订后再进 round 2。已全吸收：

| # | round 1 复核点 | 修订 |
|---|---|---|
| 1 | D3 选 (b) | GetState(GetStateRequest{hardware_id})，hardware_id **必填非空**；StreamState(StreamRequest)；CHANGELOG 登记 design-freeze diff（§5.3 Empty → 显式 hardware selector）。**D3 拍定**|
| 2 | D4a 选**常驻订阅池**（不临时+重试）| gateway 启动建 state subscription pool（per config hardware）+ latest-state cache；GetState / StreamState 读 cache，**不**为每 RPC 新建底层 iceoryx2 subscriber；没首帧 → **Unavailable**（服务未 ready，非请求语义错）。**D4 重写**|
| 3 | D5a ACK | min_interval_ms==0→默认 50ms；0<x<20→clamp 20ms（不拒）；keep-last + interval tick；不保证每 S0 sample 送达。**D5 拍定**|
| 4 | D1a 不含 heartbeat rate ACK | RobotState 最小字段；rate 后续 additive 加。**D1a 拍定（不含）**|
| 5 | D3b 错误码拆口径 | hardware_id 缺/空→**InvalidArgument**；unknown→**NotFound**；已知但 cache 无首帧→**Unavailable**；FailedPrecondition **不**做默认。+ **新 AC-9**：gateway 只订阅 config 声明 hardware，请求未声明 → NotFound，**禁默默 fallback**。**D3b 重写**|

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
- **常驻订阅池（D4a 拍定，不临时+重试）**：gateway **启动时**按 config 声明的
  每个 `[[hardware]].id` 建一条**常驻** state subscription（持久 iceoryx2
  subscriber 线程，循环读 → 更新该 hardware 的 **latest-state cache**）。
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

## Acceptance Criteria（round 2 拍定）

- **AC-1**：proto additive —— `service RobotControl` append GetState/StreamState
  2 rpc + RobotState/GetStateRequest/StreamRequest message + 4 镜像 enum；既有
  6 method diff = 0（字段/字段号/语义不变）；`schema_compat` / 既有 6 RPC e2e
  0 退化。
- **AC-2**：`GetState(GetStateRequest{hardware_id})` → gateway 读该 hardware 的
  **常驻 cache**（D4 pool，**非**临时订阅）→ `JointState`-fbs→`RobotState`-proto
  → 返。字段等价（mode/control_link/output_mode/safety/fault/positions/layout/
  stamp 跟 published JointState 一致）。
- **AC-3**：错误码口径（D9）—— `hardware_id` 缺/空 → `InvalidArgument`；unknown
  → `NotFound`；已知但 cache 无首帧 → `Unavailable`。三条各一 e2e case。
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
- **AC-9**：config 有 N hardware 时，gateway **只订阅 config 声明的
  `hardware_id`**；请求未声明的 hardware → `NotFound`，**禁默默 fallback 到第一个**
  （hard；e2e：2 hardware config，请求第 3 个不存在的 id → NotFound）。
- **AC-Q**：fmt + clippy(-D warnings) + workspace test + 4 既有 smoke 0 退化 +
  GetState/StreamState cross-process e2e + cargo deny baseline + schema_compat。

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
（D7 CLI hard）· design.md 0 改（GetState 签名走 CHANGELOG 登记）· S0/S1 daemon
0 改 · 候选 A 三条固定前置（A4 / **C1 延续并显式标非安全保证** / D6 不涉）·
auth 不得宣称 read_state 已 enforce。

## 后续（RIPER）

Spec round 1 → reviewer round 1（D3/D1a/D4a/D5a/D3b 5 项拍板）✅ →
**Spec round 2（本文，全吸收）→ 待 reviewer round 2 复核** → Plan → 实施
（倾向 commit 拆分：proto+enum+转换器 / GetState handler+gateway 常驻订阅池 /
StreamState bridge+节流 / e2e + summary）。**本 round 不开实现；reviewer 明确
"不批准直接进 Plan"，round 2 ACK 后才进 Plan。**
