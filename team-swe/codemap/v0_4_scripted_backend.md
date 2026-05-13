# v0.4 候选 1 · Scripted backend Pre-Research CodeMap

> **范围限定**（reviewer v0.4_entry §2.2 + §3.2 拍板）：第二个真 action
> backend = Scripted；joint-space 线性插值 + waypoint dwell；不选直接
> 跳点 / 不选 cubic。append `ActionKindProto::Scripted = 3` + 新 fbs
> body schema_id 0x040B 续号；复用候选 3 smoke harness + AC-C1 reaper
> 行为。**不**动 B4 frozen schema / shutdown 顺序契约 /
> `Box::leak(node)` 模式 / B3 §3.7 source_task_id 单调契约。
>
> 沿 [`v0_4_pubsub_cleanup.md`](v0_4_pubsub_cleanup.md) 同款 CodeMap
> 模式：file:line 锚点 + 关键 API + 关键约束 + 末尾"风险与不确定项"。
> 本 CodeMap 是 RIPER **Pre-Research** 产物，下一步进 Spec / Plan，不
> 改代码。
>
> 写于 2026-05-14，候选 3 完结锚点 commit `28fd9af` push 之后。

---

## 1. B3 HoldPoseBackend 模式（候选 1 第一参照）

### 1.1 模块入口

**文件** `crates/robot-s1/src/action_backend/hold_pose.rs`（489 L）

整文件 `#[cfg(feature = "backend-hold-pose")]` gate（默认 on）；feature off
时 mod 不 link → `BackendRegistry::from_inventory` 不收集 → `[s1.profile.backends
= ["hold_pose"]]` 在 `Coordinator::validate_profile_section` 报
`UnknownBackend`（沿 mock_dwell 同款 feature gate 模式）。

### 1.2 struct + ActionBackend impl

```rust
// hold_pose.rs:67-68
#[derive(Debug, Clone, Copy)]
pub struct HoldPoseBackend;  // 零字段（hold-until-cancel 无 duration）
```

```rust
// hold_pose.rs:74-109
#[allow(clippy::unnecessary_literal_bound)]
impl ActionBackend for HoldPoseBackend {
    fn name(&self) -> &str { "hold_pose" }                    // line 76
    fn supported_kinds(&self) -> &[ActionKind] {              // line 80
        const KINDS: &[ActionKind] = &[ActionKind::HoldPose];
        KINDS
    }
    fn supported_schemas(&self) -> &[u32] {                   // line 86
        // reviewer 阻断 1 锁定：**显式哨兵**，不空切片
        const SCHEMAS: &[u32] = &[NO_PAYLOAD_SCHEMA_ID];
        SCHEMAS
    }
    fn start(&self, req: &ActionRequest, _ctx: &ActionContext<'_>)
        -> Result<Box<dyn ActionInstance>, ActionStartError>  // line 92
    { ... }                                                   // 防御性 payload empty check
}
```

### 1.3 ActionInstance + tick / cancel

```rust
// hold_pose.rs:122-126
struct HoldPoseInstance {
    cancelled: bool,  // cancel() 设 flag；下次 tick 自查 + 收敛 PartialSuccess
}

// hold_pose.rs:128-162
impl ActionInstance for HoldPoseInstance {
    fn tick(&mut self, ctx: &mut ActionContext<'_>)
        -> Result<ActionTickOutput, ActionFailure>            // line 129
    {
        if self.cancelled {
            return Ok(ActionTickOutput::Completed(
                ActionOutcome::PartialSuccess {
                    warning: "cancelled".to_string()          // line 134
                }));
        }
        // **核心** B3 round 2 阻断 1 锁定：用 ctx.command_seq 作 source_task_id
        let body = build_hold_command(
            ctx.config, ctx.layout_id, ctx.anchor_bias,
            /* source_task_id */ ctx.command_seq,             // line 145
            /* now_ns */ ctx.clock.now().0,
        );
        Ok(ActionTickOutput::Continue {
            progress: 0.0,                                    // reviewer #10 不语义化
            command: Some(JointCommandBody::new(body)),
        })
    }
    fn cancel(&mut self, _ctx: &mut ActionContext<'_>)
        -> Result<(), ActionCancelError>                      // line 157
    { self.cancelled = true; Ok(()) }
}
```

### 1.4 inventory 注册

```rust
// hold_pose.rs:172-176
static HOLD_POSE_BACKEND: HoldPoseBackend = HoldPoseBackend;
inventory::submit! { &HOLD_POSE_BACKEND as &'static dyn ActionBackend }
```

### 1.5 lib unit 模板（8 测）

`hold_pose.rs::tests`（line 182-488）：
- `name_is_hold_pose`
- `supported_kinds_is_hold_pose_only`
- `supported_schemas_is_explicit_no_payload_sentinel`（reviewer 阻断 1 锁测）
- `start_rejects_non_empty_payload`
- **`tick_returns_continue_with_some_hold_body_bytes_bit_equivalent_to_build_hold_command`**
  （reviewer round 2 阻断 1 修法**最强证据**：backend tick 返的 body bytes
  跟 build_hold_command(cfg, layout_id, anchor_bias, ctx.command_seq, now_ns)
  字节级相等 + fbs 字段 decode 双断言）
- `tick_after_cancel_returns_completed_partial_success`
- `tick_progress_field_pegged_to_zero`
- `inventory_default_instance_present`

**ScriptedBackend 设计直接抄此 8 测模板**（增加 waypoint 序列 / progress
计算 / multi-segment 终态 / payload schema_id 验证 / source_task_id
跨 segment 单调 等）。

---

## 2. B2 ActionBackend trait + BackendRegistry

### 2.1 ActionBackend trait（4 method）

**文件** `crates/robot-s1/src/action.rs:426-459`（详 Agent 摘要）

```rust
pub trait ActionBackend: Send + Sync {
    fn name(&self) -> &str;
    fn supported_kinds(&self) -> &[ActionKind];
    fn supported_schemas(&self) -> &[u32];  // 不空切片；NO_PAYLOAD 用 &[NO_PAYLOAD_SCHEMA_ID] 哨兵
    fn start(&self, req: &ActionRequest, ctx: &ActionContext<'_>)
        -> Result<Box<dyn ActionInstance>, ActionStartError>;
}

pub trait ActionInstance: Send {
    fn tick(&mut self, ctx: &mut ActionContext<'_>)
        -> Result<ActionTickOutput, ActionFailure>;           // action.rs:468
    fn cancel(&mut self, ctx: &mut ActionContext<'_>)
        -> Result<(), ActionCancelError>;                     // action.rs:472
}
```

### 2.2 BackendRegistry + dispatch

**文件** `crates/robot-s1/src/action_backend/mod.rs:53-191`

- `inventory::collect!(&'static dyn ActionBackend);`（line 53）链接器
  自动收集所有 `inventory::submit!`
- `from_inventory()` 启动期一次遍历建 `HashMap<&'static str, &'static dyn ActionBackend>`
- `dispatch(enabled: &[String], kind: ActionKind, schema_id: u32)
  -> Result<&'static dyn ActionBackend, SubmitError>`：路由二元组
  - SubmitError variant: `UnknownBackend` / `NoBackendForKind(kind)` /
    `NoBackendForSchema(schema_id)` / `BackendNotEnabled(name)`
- Registry **不**存 enable list；enable 来自 `validate_profile_section`
  传入 dispatch（runtime 重载 profile 时可复用同一 registry）

### 2.3 Scripted 接入 BackendRegistry

新 `crates/robot-s1/src/action_backend/scripted.rs`（候选 1 land）：

```rust
#[cfg(feature = "backend-scripted")]  // 沿 mock_dwell / hold_pose 同款
mod scripted { ... }
```

- `ScriptedBackend` struct（含 waypoint 解码缓存？或 stateless）
- `inventory::submit! { &SCRIPTED_BACKEND as &'static dyn ActionBackend }`
- `supported_kinds() = &[ActionKind::Scripted]`
- `supported_schemas() = &[SCHEMA_SCRIPTED_PAYLOAD]`（0x040B，**不**用
  `NO_PAYLOAD_SCHEMA_ID`）

---

## 3. B2 ActionContext + B3 cmd_seq 透传契约

### 3.1 ActionContext 字段

**文件** `crates/robot-s1/src/action.rs:346-353`

```rust
pub struct ActionContext<'a> {
    pub clock: &'a dyn RobotClock,         // SystemClock / MockClock
    pub layout_id: u64,                    // FbJointCommand.layout_id
    pub hardware: &'a HardwareSection,     // joints / id
    pub config: &'a CoordinatorConfig,     // deadline_offset / priority / etc.
    pub anchor_bias: &'a [f64],            // 关节起点（用于 hold 路径）
    pub command_seq: u64,                  // B3 加：source_task_id 单一源（详 §3.3）
}
```

### 3.2 ActionTickOutput

**文件** `crates/robot-s1/src/action.rs:359-384`

```rust
pub enum ActionTickOutput {
    Continue {
        progress: f32,                     // [0, 1]；HoldPose 永 0.0；Scripted 用 waypoint_idx / total
        command: Option<JointCommandBody>, // Some = publish；None = 沿用上一帧
    },
    Completed(ActionOutcome),              // ActionOutcome::Success / PartialSuccess { warning }
}
```

### 3.3 source_task_id 单调（B3 §3.7 reviewer round 2 阻断 1 锁定）

候选 1 必保：

- `source_task_id` 来自 `ctx.command_seq`（`run_action_tick_loop` 透传
  `TickState.cmd_seq`）
- backend **不**得私有 counter（HoldPose 同款）
- S0 `ipc_thread::join_command_to_buf` 把 `fb.source_task_id()` 写到
  `JointCommandBuf.seq`，`adapter.rs` 要求单调递增；非单调 → S0 判
  `CONTROL_LOST` → `ACTIVE` 恢复路径破坏

ScriptedBackend 测试模板抄 hold_pose
`tick_returns_continue_with_some_hold_body_bytes_bit_equivalent_to_build_hold_command`：
- 候选 1 改为 `tick_segment_N_returns_command_with_source_task_id_eq_ctx_command_seq`
- 跨 waypoint segment 切换时 cmd_seq 不重置（同 hold 路径同款）

---

## 4. B4 RPC payload 路径 + ActionKindProto append

### 4.1 proto::ActionKindProto enum

**文件** `crates/robot-rpc/proto/robot_control.proto:119-123`

```proto
enum ActionKindProto {
    ACTION_KIND_UNSPECIFIED = 0;
    ACTION_KIND_DWELL_MS    = 1;  // B2 mock
    ACTION_KIND_HOLD_POSE   = 2;  // B3 land
    // 候选 1 append：
    // ACTION_KIND_SCRIPTED = 3;  // joint-space linear + waypoint dwell
}
```

### 4.2 InvokeTaskCommand

**文件** `crates/robot-rpc/proto/robot_control.proto:195-202`

```proto
message InvokeTaskCommand {
    CommandMeta     meta              = 1;
    ActionKindProto kind              = 2;
    uint64          deadline_ns       = 3;  // gateway 端透传 0；用 timeout_ms
    uint32          priority          = 4;
    uint32          payload_schema_id = 5;  // 0 = NO_PAYLOAD；Scripted = 0x040B
    bytes           payload           = 6;  // fbs ScriptedPayload bytes，gateway 端透传
}
```

### 4.3 gateway 端透传（不解 payload）

**文件** `crates/robot-rpc/src/task_handlers.rs::validate_invoke_task_command`
+ `build_invoke_task_request`

- proto → fbs 透传 payload bytes（**gateway 不 decode payload**）
- 仅校验 ActionKindProto → ActionKindFb 映射（候选 1 加 Scripted variant）

### 4.4 s1 端 decode + dispatch

**文件** `crates/robot-s1/src/rr_server/handler.rs::handle_invoke` (line 146-281)

- fbs InvokeTaskRequest decode → ActionKindFb
- ActionKindFb → ActionKind 映射（line 174-193；候选 1 加
  `ActionKindFb::Scripted => ActionKind::Scripted`）
- 构造 `ActionRequest { kind, payload_schema_id, payload }`（line 210-221）
- `Coordinator::submit(req, clock)` → `BackendRegistry::dispatch(enabled,
  kind, payload_schema_id)` → ScriptedBackend 拿到 owned payload bytes
- ScriptedBackend `start()` 内 `flatbuffers::root::<ScriptedPayload>(payload)`
  解码 + 构造 `ScriptedInstance { waypoints, current_idx, ... }`

### 4.5 ActionKindFb（fbs schema 端）

**文件** `crates/robot-msg/schemas/action_request.fbs`

需要 append `Scripted = 3` enum variant（候选 1 land；不动现有 variant 编号）。

---

## 5. schema_id registry + fbs schema 续号

### 5.1 当前 schema_id 段（B4 land 后）

**文件** `crates/robot-msg/src/schema_id.rs`

```
0x0000_0000  SCHEMA_UNSPECIFIED
0x0000_0001  SCHEMA_JOINT_LAYOUT
0x0000_0002  SCHEMA_JOINT_STATE
0x0000_0003  SCHEMA_JOINT_COMMAND
0x0000_0100  SCHEMA_TWIST
0x0000_0200  SCHEMA_IMAGE_HEADER
0x0000_0201  SCHEMA_POINTCLOUD_HEADER
0x0000_0202  SCHEMA_TENSOR_HEADER
0x0000_0300  SCHEMA_ESTOP_EVENT
0x0000_0301  SCHEMA_HEARTBEAT
─── A1 + B4 control plane ──────────
0x0000_0400  SCHEMA_CLEAR_ESTOP_REQUEST          (A1)
0x0000_0401  SCHEMA_CLEAR_ESTOP_RESPONSE         (A1)
0x0000_0402  SCHEMA_INVOKE_TASK_REQUEST          (B4)
0x0000_0403  SCHEMA_INVOKE_TASK_RESPONSE         (B4)
0x0000_0404  SCHEMA_GET_TASK_STATUS_REQUEST      (B4)
0x0000_0405  SCHEMA_GET_TASK_STATUS_RESPONSE     (B4)
0x0000_0406  SCHEMA_CANCEL_TASK_REQUEST          (B4)
0x0000_0407  SCHEMA_CANCEL_TASK_RESPONSE         (B4)
0x0000_0408  SCHEMA_CLEAR_ERROR_REQUEST          (B4)
0x0000_0409  SCHEMA_CLEAR_ERROR_RESPONSE         (B4)
0x0000_040A  SCHEMA_ACTION_EVENT_ENVELOPE        (B4)
─── 候选 1 待 append ─────────────
0x0000_040B  SCHEMA_SCRIPTED_PAYLOAD             (候选 1)
```

### 5.2 注册步骤（候选 1 land）—— 完整 6 步链（reviewer round 1 阻断 1 修正）

**关键修正**：`build.rs` **不**自动扫目录；`markers.rs` 是**手写 trait
impl** 不是 build.rs 生成。完整 schema 注册链 6 步硬点，候选 1 land
必须**全部**改：

#### 步骤 1 · 新 `.fbs` file

`crates/robot-msg/schemas/scripted_payload.fbs`：

```flatbuffers
namespace robot.msg;

table WaypointTarget {
  positions: [double];                   // joint-space；长度 = hardware.joints.len()
  dwell_ms: uint32;                      // 到 waypoint 后 hold 时长
  segment_duration_ms: uint32;           // 上 waypoint 线性插值到本的总时长
}

table ScriptedPayload {
  waypoints: [WaypointTarget];           // 非空；上限 Spec 拍板（详 §9.1 #3）
}
```

具体字段集 + waypoint 数量上限 Spec 阶段 reviewer 拍板。

#### 步骤 2 · `crates/robot-msg/build.rs` `current_schemas` 硬编码扩

**文件** `crates/robot-msg/build.rs:57`：

```rust
let current_schemas: [&str; 12] = [        // ← 候选 1 必改 12 → 13
    "frame_header.fbs",
    "joint.fbs",
    ...
    "action_request.fbs",
    "action_event.fbs",
    // 候选 1 加：
    // "scripted_payload.fbs",
];
```

build.rs `for s in current_schemas` (line 88 + line 165) 两处循环
（schema_lint + flatc invoke）跟着新数组自动覆盖；无需手改 loop。

#### 步骤 3 · `crates/robot-msg/src/schema_id.rs` const + REGISTRY 入

```rust
// 接 0x040A 续号
pub const SCHEMA_SCRIPTED_PAYLOAD: u32 = 0x0000_040B;

// SCHEMA_REGISTRY 排序数组同步 append
```

`schema_lint` 静态校验唯一性 + 升序 + 未与 `DEPRECATED_SCHEMA_IDS`
冲突；CI gate。

#### 步骤 4 · `crates/robot-msg/src/markers.rs` **手写** marker + BodyMarker impl

**文件** `crates/robot-msg/src/markers.rs`（**不**是 build.rs 生成；
手写 trait impl）：

```rust
// trait 定义（markers.rs:50）
pub trait BodyMarker: 'static + Send + Sync {
    const SCHEMA_ID: u32;
    const SCHEMA_NAME: &'static str;
    /// 该 body table 是否在内部嵌入 BlobDesc（`ImageHeader` /
    /// `PointCloudHeader` / `TensorHeader` 为 true；其余 false）。
    const BLOB_EMBEDDED: bool;
    /// flatc 生成的 root table 类型；GAT 借用 parser 输入 slice。
    type Root<'a>: flatbuffers::Follow<'a, Inner = Self::Root<'a>>
        + flatbuffers::Verifiable + Copy;
    /// 默认返回 None；带 blob 的 marker 覆盖此方法。
    fn read_blob_desc(_root: &Self::Root<'_>) -> Option<BlobDescInfo> {
        None
    }
}

// 候选 1 加：跟现有 BodyMarker impl 同款手写
#[derive(Debug, Clone, Copy, Default)]
pub struct ScriptedPayload;

impl BodyMarker for ScriptedPayload {
    const SCHEMA_ID: u32 = schema_id::SCHEMA_SCRIPTED_PAYLOAD;
    const SCHEMA_NAME: &'static str = "robot.msg.ScriptedPayload";
    const BLOB_EMBEDDED: bool = false;
    type Root<'a> = msg::ScriptedPayload<'a>;
    // read_blob_desc 用默认 impl 返 None（BLOB_EMBEDDED = false 的 marker
    // 沿用默认）
}
```

参考现有 BodyMarker impl 同款手写（`JointLayout` / `JointState` /
`JointCommand` / `Twist` / `ImageHeader` / `PointCloudHeader` /
`TensorHeader` / `EStopEvent` / `Heartbeat` / `InvokeTaskRequest` /
... 等；具体清单见 `markers.rs` 全文）。带 `BlobDesc` 的 marker（如
`ImageHeader` / `PointCloudHeader` / `TensorHeader`）`BLOB_EMBEDDED =
true` 并覆盖 `read_blob_desc`；ScriptedPayload 不带 blob，沿用默认。

#### 步骤 5 · `crates/robot-msg/src/size_tier.rs` `default_tier_of` arm + 测试

**文件** `crates/robot-msg/src/size_tier.rs:62-95`（`default_tier_of`
match arm 硬编码）：

```rust
pub const fn default_tier_of(schema_id: u32) -> Option<SizeTier> {
    match schema_id {
        SCHEMA_INVOKE_TASK_REQUEST       // line 74 当前落 Control 8KiB
        | SCHEMA_INVOKE_TASK_RESPONSE
        | ...
        | SCHEMA_ACTION_EVENT_ENVELOPE   // line 82
            => Some(SizeTier::Control),
        // 候选 1 加 SCHEMA_SCRIPTED_PAYLOAD → 哪个 tier？详 §9.1 #3
        //（**注意**：仅给 SCHEMA_SCRIPTED_PAYLOAD 设 SensorSmall 不放
        //  大外层 SCHEMA_INVOKE_TASK_REQUEST 上限；Spec 必拍三选一）
        ...
    }
}
```

`tests::default_tier_of` 锁测每 schema_id 都有 arm（穷举测试）；候选 1
加 ScriptedPayload 必须同步加 test arm。

#### 步骤 6 · 可选 `crates/robot-msg/src/build_helpers.rs` builder

**文件** `crates/robot-msg/src/build_helpers.rs`（含
`build_invoke_task_request` line 346 / `build_clear_estop_request` line
245 / 等手写 builder）：

如 smoke binary / s1 backend 需要构造 ScriptedPayload bytes
（`FlatBuffersBuilder` 序列化），可加 helper：

```rust
pub fn build_scripted_payload(waypoints: &[(Vec<f64>, u32, u32)]) -> Vec<u8> {
    // FlatBuffersBuilder 同款模式
}
```

或者直接由 smoke binary / backend 内部用 `FlatBuffersBuilder` 构造
（参考 `build_invoke_task_request` 内部模式）；Spec 阶段拍板是否需要
公共 builder。

#### schema_lint 校验

`schema_lint` (cargo run -p robot-msg --bin schema_lint) CI gate 已
覆盖：唯一性 + 升序 + deprecated 拦截。候选 1 land 时自动 verify。

### 5.3 现有 fbs schema 文件（参考）

```
crates/robot-msg/schemas/
├─ action_event.fbs     (B4 0x040A pubsub envelope)
├─ action_request.fbs   (B4 0x0402; 候选 1 加 ActionKindFb::Scripted enum)
├─ action_status.fbs    (B4 0x0404/0x0405)
├─ blob.fbs
├─ clear_estop.fbs      (A1 0x0400/0x0401)
├─ event.fbs            (0x0300/0x0301)
├─ frame_header.fbs
├─ image.fbs            (0x0200)
├─ joint.fbs            (0x0001-0x0003)
├─ pointcloud.fbs       (0x0201)
├─ tensor.fbs           (0x0202)
├─ twist.fbs            (0x0100)
└─ scripted_payload.fbs (候选 1 新增)
```

---

## 6. Coordinator state machine + run_action_tick_loop

### 6.1 5 state variant（候选 1 不动）

**文件** `crates/robot-s1/src/coordinator.rs:1162-1193`

```rust
pub enum CoordinatorState {
    Booting,                            // 启动期；anchor 未完；拒 submit
    Idle,                               // hold loop 跑；接受 submit
    Executing { action_id, started_at_ns },  // 单 action（B2 序列化）
    Degraded { reason },                // 失败后；拒 submit；hold loop 继续
    ShuttingDown,                       // 拒 submit + break tick loop
}
```

ScriptedBackend 状态转移：

```
Idle → submit(kind=Scripted, payload=[waypoints]) → Executing
Executing 期间 50 Hz tick: backend 每帧返 Continue { progress, command: Some(...) }
  * waypoint_idx 0 → linear interp 到 waypoint 0 → dwell waypoint 0
  * waypoint_idx ++ → linear interp 到 waypoint 1 → dwell waypoint 1
  * ...
  * 最后 waypoint dwell 完 → Completed(Success)
Executing → Completed/Failed/Cancelled → Idle + hold loop 恢复
```

### 6.2 run_action_tick_loop cmd_seq 单一源

**文件** `crates/robot-s1/src/coordinator.rs:780-898`

```rust
// Idle → hold loop
let body = build_hold_command(cfg, layout_id, bias, state.cmd_seq, now_ns);
state.cmd_seq += 1;

// Executing → backend tick (拿 ActionContext.command_seq = state.cmd_seq)
let outcome = coordinator.tick_action(ctx, state.cmd_seq);
// outcome 内 body 已用 state.cmd_seq 填 source_task_id
state.cmd_seq += 1;
```

**关键**：state.cmd_seq **单一源**；hold loop / backend / Scripted backend
都拿同一计数器；切换 backend / Idle ↔ Executing 不重置。

---

## 7. 候选 3 smoke harness 复用边界（**模式复用** / 复制私有 helper）

### 7.1 现状：bin 内私有 helper，**不**跨 binary 直接调用

**关键事实**（reviewer round 1 非阻断建议修正）：候选 3
`pubsub_cleanup_smoke.rs` 的 helper 全是 **bin 内私有 fn**（`fn xxx` /
`async fn xxx`，非 `pub`）。`crates/robot-tests/src/` 仅有 `bin/` 子目录，
**没有** `common/` lib 模块；不同 binary 之间**不能直接** `use
crates::robot_tests::common::...`。

候选 1 smoke binary land 时**只能选**两条路径：

- **路径 A（推荐）：复用模式 + 复制私有 helper**
  * 在新 `scripted_backend_smoke.rs` inline 重写 / 复制等价 helper
    （`FakeStatePublisher`, `SubprocessHandle`, `bring_up_pair`,
    `tear_down_pair`, `invoke_and_wait_terminal`, 三段式 ready 等）
  * 沿 `clear_estop_e2e` / `crash_restart_smoke` 同款 bin-内闭包模式
  * 工程量：复制 ~500-700 行（候选 3 binary 1192 + (2/2-a/b) 增长 = ~2300 行，
    其中 ~700 行是 harness）；复制成本 << 抽取 common module 的设计成本
- **路径 B：先抽 `crates/robot-tests/src/common/` lib 模块再复用**
  * 改 `crates/robot-tests/Cargo.toml` 加 `[lib]` 段；migrate 私有 fn
    → `pub fn` ;每 binary `use robot_tests::common::...`
  * 工程量：~2-3 个 commit refactor + reviewer round；适合候选 1+2+4
    都要 smoke binary 时一次性抽（v0.5+ 时机）；现在抽是 over-engineering

### 7.2 候选 3 helper 列表（复制目标 / 模式参考）

仅作 **模式参考**（候选 1 land 时复制 / inline 等价实现），不能跨
binary import：

**文件** `crates/robot-tests/src/bin/pubsub_cleanup_smoke.rs`（bin 内
私有；行号大致 anchor）：

| Helper | 行号 | 复用方式 |
|---|---|---|
| `FakeStatePublisher` struct + spawn / shutdown | ~600-650 | inline 复制（候选 3 也是从 B4 (3/3) `tests/common/fake_state.rs` 复制） |
| `SubprocessHandle` struct + spawn / kill_on_drop / sigkill_and_wait | ~650-780 | inline 复制 |
| `bring_up_pair(ctx) -> BringUp` | ~1450-1550 | inline 复制 + 适配 |
| `tear_down_pair(bring_up)` | ~1555-1575 | inline 复制 |
| `build_invoke_task_cmd(request_id, ik, source, token, kind, timeout_ms, reason)` | ~1230-1265 | 复制 + **扩 payload 参数**（候选 1 需要塞 ScriptedPayload bytes） |
| `invoke_and_wait_terminal(client, token, rpc_timeout_ms, stream_timeout)` | ~1290-1380 | inline 复制 + 适配（kind 改 Scripted）|
| `count_iceoryx2_storage_files() -> (u64, &'static str)` | ~1100-1145 | 复制（候选 1 通用 informational metric） |
| `sample_metric` + `judge_triggered_c1/c2` | ~1610-1680 | 复制（候选 1 如触发条件性 AC 沿用同款机制） |

### 7.3 候选 1 smoke binary 设计（独立 binary，路径 A）

新 `crates/robot-tests/src/bin/scripted_backend_smoke.rs`（沿
`pubsub_cleanup_smoke` 模式 + `scripted-backend-smoke` feature gate）：

- inline 等价 `FakeStatePublisher`（复制候选 3 / B4 (3/3) `tests/common/fake_state.rs`）
- inline 复制 `bring_up_pair` / `tear_down_pair`
- **新** `build_scripted_payload(waypoints: &[Waypoint]) -> Vec<u8>`：
  在 smoke 端用 `FlatBuffersBuilder` 构造 ScriptedPayload bytes（不依赖
  `crates/robot-msg/src/build_helpers.rs` 公共 builder；smoke 直接用
  `flatbuffers::FlatBufferBuilder` API；详 §5.2 步骤 6）
- **扩** `build_invoke_task_cmd(..., kind, payload_schema_id, payload_bytes)`
  签名加 payload 参数（候选 3 当前签名只有 kind，没 payload）
- AC：单 waypoint dwell / 多 waypoint linear / progress 单调递增 /
  source_task_id 跨 segment 单调 / cancel 中间 waypoint → PartialSuccess
- 沿候选 3 `--phase {selftest, A1, A2, A3, all}` 模式扩展或新设
  phase（如 `--phase happy_multi_waypoint` / `--phase cancel_mid` /
  `--phase source_task_id_monotonic`）

### 7.4 path B 反思：何时抽 `tests/common/` lib

不在候选 1 范围。如未来候选 2 (IK) / 候选 4 (dimos) / v0.5+ 等多个
backend smoke binary 都需要同款 harness（≥ 3 个 binary 复制了），抽
`tests/common/` 才有正收益；候选 1 时仅 candidate 3 + candidate 1 两
个 binary，复制成本 < 抽取设计成本。Spec 阶段如想推路径 B 走另立
outline。

### 7.5 cross-process e2e 模式

候选 3 是 feature-gated binary（`required-features =
["pubsub-cleanup-smoke"]`）；候选 1 同款 (`required-features =
["scripted-backend-smoke"]`)。

**不**复用候选 3 binary 加 phase（如 `--phase scripted`），让
`pubsub_cleanup_smoke` 变成"v0.4 通用 backend smoke"——范围漂移风险
（Plan-Execute diff）；推荐**独立 binary** 沿 b4_3_3 / clear_estop_e2e
/ crash_restart_smoke 同款 feature gate 模式。

---

## 8. ScriptedBackend 设计草图（**不**land；Spec 阶段拍板）

### 8.1 schema 设计

```flatbuffers
// crates/robot-msg/schemas/scripted_payload.fbs
namespace robot.msg;

table WaypointTarget {
  /// 关节空间目标位置；长度必须 = hardware.joints.len()
  positions: [double];
  /// 到 waypoint 后 dwell 时长（ms）；0 = 不 dwell 直接进下一段
  dwell_ms: uint32;
  /// 从上一 waypoint 线性插值到本 waypoint 的总时长（ms）；0 = 直跳（候选 1
  /// 拒；reviewer 拍板 joint-space 线性插值）
  segment_duration_ms: uint32;
}

table ScriptedPayload {
  /// waypoint 序列；空数组 = 拒（at least 1）；上限留 Spec 拍板
  waypoints: [WaypointTarget];
}
```

### 8.2 ScriptedBackend struct + Instance

```rust
// pseudo-code; Spec 阶段拍板
pub struct ScriptedBackend;  // 零字段；状态全在 instance

struct ScriptedInstance {
    waypoints: Vec<DecodedWaypoint>,  // start() 期解码完一次性 owned
    current_idx: usize,
    segment_started_at_ns: u64,
    phase: SegmentPhase,              // Interp { from, to } / Dwell { until_ns }
    cancelled: bool,
    initial_pose: Vec<f64>,           // ctx.anchor_bias 快照（start 期）
}

enum SegmentPhase {
    Interp { from: Vec<f64>, to: Vec<f64>, segment_duration_ms: u32 },
    Dwell { dwell_until_ns: u64 },
}
```

### 8.3 tick() 状态机草图

```
tick(ctx):
  if cancelled: return Completed(PartialSuccess { warning: "cancelled" })
  match phase:
    Interp { from, to, duration_ms }:
      let elapsed = ctx.clock.now().0 - segment_started_at_ns
      if elapsed >= duration_ms * 1e6:
        phase = Dwell { dwell_until_ns: ctx.clock.now().0 + waypoints[idx].dwell_ms * 1e6 }
        target = to.clone()  // 钉到 waypoint
      else:
        let alpha = elapsed as f64 / (duration_ms * 1e6) as f64
        target = lerp_per_joint(from, to, alpha)
    Dwell { dwell_until_ns }:
      if ctx.clock.now().0 >= dwell_until_ns:
        idx += 1
        if idx >= waypoints.len():
          return Completed(Success)
        phase = Interp { from: waypoints[idx-1].positions, to: waypoints[idx].positions, ... }
        segment_started_at_ns = ctx.clock.now().0
      target = waypoints[idx].positions  // hold 期保持
  let body = build_joint_command(cfg, layout_id, target, ctx.command_seq, now_ns)
  let progress = (idx as f32 + segment_progress) / waypoints.len() as f32
  Continue { progress, command: Some(body) }
```

需要 helper `build_joint_command`（候选 1 新；同 `build_hold_command` 一族
但 `target` 不绑 `anchor_bias`）；或扩 `build_hold_command` 加 `target`
参数兼容（Spec 阶段拍板）。

---

## 9. 风险与不确定项（Spec 阶段必拍板）

### 9.1 schema / payload 风险

1. **schema_id 0x040B 唯一性**：跟 deprecated 表无冲突（v0.2 无历史 / B4
   占 0x0402-0x040A），schema_lint 静态校验
2. **ScriptedPayload 字段集**：linear 插值还需要什么？速度限制？加速度？
   * 候选 1 拍板 joint-space 线性插值不考虑速度 / 加速度（v0.4 后期 / v0.5
     做 trajectory backend 时再加）
3. **payload size_tier — 外层 RR sample 容量约束**（reviewer round 1
   阻断 2 修正）：

   **关键事实**：Scripted payload bytes 嵌在 `InvokeTaskRequest.payload`
   字段里（B4 frozen wire format），通过 `task_invoke` RR topic 发到
   s1 端。**iceoryx2 sample 外层 marker 是 `SCHEMA_INVOKE_TASK_REQUEST`**
   (`size_tier.rs:74` 落 `SizeTier::Control` 8 KiB)，**不**是
   `SCHEMA_SCRIPTED_PAYLOAD`。仅给 `SCHEMA_SCRIPTED_PAYLOAD` 单独设
   `SensorSmall` 是 **不充分方案** —— 外层 `task_invoke` RR sample 仍
   按 Control 8 KiB 上限走，超限 → iceoryx2 `loan_slice_uninit`
   `TransportError::SampleTooLarge`。

   实测估算：1000 waypoint × N joints × 16 byte ≈ 16+ KiB；加 fbs
   header + InvokeTaskRequest 外层字段 → 整 wire sample >> 8 KiB，
   会直接溢出 Control tier。

   **Spec 必拍三选一**（reviewer round 1 阻断 2 拍板）：

   - **(a) 限 Scripted payload 让整个 InvokeTaskRequest ≤ 8 KiB**：硬
     限 waypoint 数量上限（如 ≤ 100 / N=1 joint；100 waypoint × 1
     joint × 16 byte = 1.6 KiB；含 fbs overhead 应 ≤ 4 KiB）；超限
     `ActionStartError::Rejected` fail-fast。最小工程量；锁住 wire
     不改；适合"先 land 基础形态"路径
   - **(b) 给 `task_invoke` RR / `SCHEMA_INVOKE_TASK_REQUEST` 走更大
     容量 / per-schema_id override**：升 `SCHEMA_INVOKE_TASK_REQUEST`
     SizeTier 到 `SensorSmall` (1 MiB) 或加 per-topic override 机制；
     **影响**: B4 frozen 边界 — 改 SizeTier 等价改外层 RR wire 容量，
     需 reviewer round + 评估 iceoryx2 shm 占用；下游所有 RPC handler
     跟着改
   - **(c) 做 streaming / chunking**：拆 ScriptedPayload 跨多 RR /
     pubsub frame；最大灵活；工程量最大；新 RPC 路径设计 + 多 frame
     reassembly 状态；推 v0.5+

   推荐先走 (a)，仅锁 hard limit；如 (a) 不够（实际用例需要 >100
   waypoint），走 (b) 评估；(c) 长期方案。

   `size_tier.rs::default_tier_of` 内 `SCHEMA_SCRIPTED_PAYLOAD` arm
   tier 的选择**独立于**上述三选一：因为 SCHEMA_SCRIPTED_PAYLOAD 仅
   作为 BodyMarker 的 schema id 注册，**不**直接走 iceoryx2 wire（一
   律嵌在 InvokeTaskRequest.payload 内）。落 `SizeTier::Control` 仅
   是文档级标记，不影响实际 sample 上限。
4. **gateway payload schema 强校验？**：gateway `task_handlers.rs` 当前**不**
   解 payload bytes；候选 1 是否要 gateway 端先 `flatbuffers::root::<ScriptedPayload>(payload)`
   verify（fail-fast Internal vs s1 端 fail backend.start()→ActionRpcStatus）？
   推荐 s1 端 fail（gateway 不引入 schema 解码依赖）

### 9.2 运行时风险

5. **runtime alloc 在 tick 路径**：ScriptedInstance.tick 每帧
   `lerp_per_joint` 计算 → `Vec<f64>` 分配？跟 hold_pose 同款用栈数组 +
   `Vec::with_capacity(joints)` 复用？**关键约束**：tick_action 在
   `run_action_tick_loop` 50Hz 跑，**不**是 RT thread（详 §6.2），但仍
   是 hot path
6. **插值 cadence vs tick_loop 50Hz**：waypoint segment_duration_ms 不是 20ms
   整数倍时（e.g., 30ms），lerp alpha 跨 tick 落点不均匀。Spec 阶段
   拍板：是否要在 backend 内部 100Hz 计算 / 还是只跟 tick_loop 同步？
7. **progress 计算**：`(idx + segment_progress) / total`？还是按时间总
   长？Spec 阶段拍板；reviewer 拍板"progress 不语义化"对 hold_pose 成立，
   但 Scripted 有自然语义（waypoint 进度）
8. **cancel 中间 waypoint 收敛**：cooperative cancel 到一半 → PartialSuccess
   { warning: "cancelled_at_waypoint_N" } 还是 Cancelled outcome？沿
   HoldPose PartialSuccess 同款 vs 候选 1 用独立 Cancelled 路径
9. **`source_task_id` 跨 segment 单调**：必保 ctx.command_seq 透传，
   waypoint 切换不重置（hold_pose 同款；候选 1 必扩 bit-equivalent test
   覆盖跨 segment）

### 9.3 工程 / 测试风险

10. **smoke binary 怎么塞 payload bytes**：smoke 端 FlatBuffersBuilder
    构造 ScriptedPayload bytes；非 RT 线程可分配；但增加 smoke binary
    跟 robot-msg 依赖 (`flatbuffers` crate)；候选 3 smoke 已 link
    `flatbuffers`，无新 dep
11. **复用候选 3 binary vs 独立 binary**：推荐独立
    `scripted_backend_smoke` binary + `scripted-backend-smoke` feature
    （沿 b4_3_3 / candidate 3 同款）；不漂移 pubsub_cleanup_smoke 范围
12. **lib unit 覆盖**：抄 hold_pose 8 测模板 + 加 scripted-specific 测：
    * `tick_interp_alpha_0_returns_from_waypoint`
    * `tick_interp_alpha_1_returns_to_waypoint`
    * `tick_dwell_phase_holds_position`
    * `tick_completed_after_last_dwell`
    * `tick_segment_N_returns_command_with_source_task_id_eq_ctx_command_seq`
      （bit-equivalent + cross-segment）
    * `cancel_at_interp_returns_partial_success_at_waypoint_N`
    * `start_rejects_empty_waypoints` / `start_rejects_payload_schema_id_mismatch`
13. **B3 BackendRegistry inventory test 不破**：HoldPoseBackend 测的
    `inventory_default_instance_present` 看 names 含 `"hold_pose"`；候选 1
    land 后 names 也要含 `"scripted"`，可能需更新此测或加 ScriptedBackend
    专属测
14. **gateway → s1 ActionKindFb 映射表完整性**：
    `crates/robot-s1/src/rr_server/handler.rs:174-193` 当前 explicit match
    DwellMs / HoldPose / Unknown / 其它（Internal err）；候选 1 加
    `ActionKindFb::Scripted` match arm + lib unit 锁测 variant count

### 9.4 跨 milestone 边界

15. **B3 §3.7 source_task_id 单调**：候选 1 必保（同 hold_pose）；
    `s0_adapter.rs::rt_thread` 判 `CONTROL_LOST` 用 seq 单调
16. **B4 frozen schema**：仅 append（ActionKindProto::Scripted / 0x040B
    /ScriptedPayload）；**不**改 InvokeTaskCommand 字段编号
17. **候选 3 AC-C1 reaper 行为**：每个 Scripted task 完成后 events_drain
    handle 仍走 reap_finished 1s 回收路径；候选 1 不影响
18. **§6.7 5 不变量**：候选 1 完全在 backend 实施层面，**不**触发任一
    不变量降级 / 升级

---

## 跟其它 milestone handoff / spec 的链接

- [`../../docs/handoffs/v0.4_entry.md`](../../docs/handoffs/v0.4_entry.md)
  §2.2 候选 1 范围 + §3.2 插值策略拍板（joint-space 线性 + waypoint dwell）
  + §5 6 hard boundaries
- [`../../docs/handoffs/v0.4_pubsub_cleanup_summary.md`](../../docs/handoffs/v0.4_pubsub_cleanup_summary.md)
  §6 候选 1 接班 + §3 关键契约审计（AC-C1 reaper 行为 + shutdown 顺序
  契约 + B3 §3.7 source_task_id 单调）
- [`../../docs/handoffs/v0.3_b3_summary.md`](../../docs/handoffs/v0.3_b3_summary.md)
  HoldPoseBackend land 模式 + reviewer round 2 阻断 1 修法（source_task_id
  来自 ctx.command_seq 不私有 counter）
- [`../../docs/handoffs/v0.3_b2_outline.md`](../../docs/handoffs/v0.3_b2_outline.md)
  ActionBackend trait / BackendRegistry / ActionContext / ActionTickOutput
  原始拍板表
- [`../../docs/handoffs/v0.3_b4_outline.md`](../../docs/handoffs/v0.3_b4_outline.md)
  §6.1 proto schema vs fbs schema 双层模型（gateway 透传 payload bytes
  不解码）
- `crates/robot-s1/src/action_backend/hold_pose.rs` 全文（候选 1 直接抄）
- `crates/robot-s1/src/action.rs:346-473`（ActionContext / ActionBackend
  trait / ActionInstance trait / ActionTickOutput）
- `crates/robot-s1/src/action_backend/mod.rs:53-191`（BackendRegistry
  + inventory collect/submit）
- `crates/robot-msg/src/schema_id.rs:60-94`（B4 control plane segment +
  候选 1 0x040B 续号位置）
