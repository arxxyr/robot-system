# Spec · v0.4 候选 1 · Scripted backend (joint-space linear interp + waypoint dwell)

> RIPER **Spec 阶段**产物。沿 [`v0_4_pubsub_cleanup.md`](v0_4_pubsub_cleanup.md)
> + [`b4_3_3_cross_process_e2e.md`](b4_3_3_cross_process_e2e.md) 同款
> 模板。只定义 What / 影响范围 / AC / 非范围 / 风险 / 回滚 / Reviewer
> 待拍板项；**不**写 Plan（待 reviewer round N 拍板 AC + Q 后单独走）。
>
> 范围限定（v0.4_entry §2.2 + §3.2 reviewer 拍板）：
>
> - 第二个真 action backend = **Scripted**；joint-space 线性插值 +
>   waypoint dwell
> - 不选直接跳点（语义弱）/ 不选 cubic（边界速度 / 加速度 / jerk 契约
>   未定，留 v0.4 后期 / v0.5+）
> - append `ActionKindProto::ActionKindScripted = 3` + 新 fbs body schema_id
>   `0x040B` 续号（不动 B4 frozen schema 现有 id / 字段编号）
> - 复用候选 3 smoke harness **模式**（不能 import bin 内私有 fn；走
>   inline 复制路径 A，详 CodeMap §7）
> - **不**触发任何条件性 AC 路径（候选 3 完结时 AC-C1 已 land；候选 1
>   不重新评估）
>
> 写于 2026-05-14，候选 1 Pre-Research CodeMap commit `9d91260` push
> 之后。

---

## What

为 v0.4 第二个真 action backend `ScriptedBackend` 落地：

1. **lib 层**：`crates/robot-s1/src/action_backend/scripted.rs` 实现
   `ActionBackend` + `ActionInstance` trait，复用 B3 `HoldPoseBackend`
   模式但加 waypoint 序列 + 线性插值 + dwell state machine
2. **schema 层**：新 `ScriptedPayload` fbs body（schema_id `0x040B`）
   + `ActionKindProto::ActionKindScripted = 3` append（prost 生成名：
   enum 前缀 + CamelCase variant；proto 端定义 `ACTION_KIND_SCRIPTED = 3`，
   沿现有 `ActionKindDwellMs` / `ActionKindHoldPose` 同款）+
   `ActionKindFb::Scripted`
   append + s1 handler ActionKindFb → ActionKind 映射扩展
3. **schema 注册链 5 必改注册点 + 1 可选 builder**（CodeMap §5.2）全
   部落地：
   * 5 必改：新 `scripted_payload.fbs` / `build.rs current_schemas`
     12→13 / `schema_id.rs` const + REGISTRY / `markers.rs` BodyMarker
     impl 手写 / `size_tier.rs default_tier_of` arm + test
   * 1 可选：`build_helpers.rs::build_scripted_payload(...)` builder
     （Q5 拍板是否抽公共 builder vs smoke / backend 各自 inline 用
     `FlatBufferBuilder`）
4. **测试**：lib unit（沿 hold_pose 8 测模板 + Scripted-specific 多 segment
   / cancel / source_task_id 跨 segment 单调 / bit-equivalent test）+
   cross-process smoke binary（沿候选 3 模式 + `scripted-backend-smoke`
   feature gate）

具体 backend struct / Instance / tick state machine / schema 字段集
细节见 CodeMap §8 设计草图；本 Spec 不预钉死字段集（待 Reviewer §Q1
拍板）。

---

## 影响范围

### 必改文件（候选 1 land 必须）

| 模块 | 改动 |
|---|---|
| `crates/robot-msg/schemas/scripted_payload.fbs`（新） | ScriptedPayload + WaypointTarget table 定义 |
| `crates/robot-msg/schemas/action_request.fbs` | `ActionKindFb` enum append `Scripted = 3`（不动现有 variant 编号） |
| `crates/robot-msg/build.rs:57` | `current_schemas: [&str; 12]` → `[&str; 13]` + append `"scripted_payload.fbs"` |
| `crates/robot-msg/src/schema_id.rs` | 加 `pub const SCHEMA_SCRIPTED_PAYLOAD: u32 = 0x0000_040B;` + `SCHEMA_REGISTRY` 排序数组 append |
| `crates/robot-msg/src/markers.rs` | 加 `#[derive(...)] pub struct ScriptedPayload;` + 手写 `impl BodyMarker`（沿现有 marker 同款，含 `BLOB_EMBEDDED = false`，default `read_blob_desc`） |
| `crates/robot-msg/src/size_tier.rs:62-95` | `default_tier_of` match arm 加 `SCHEMA_SCRIPTED_PAYLOAD` + `tests::default_tier_of` 穷举测试加 arm |
| `crates/robot-msg/src/build_helpers.rs` | **可选**：加 `build_scripted_payload(waypoints) -> Vec<u8>` helper（Spec §Q5 拍板是否抽公共 builder vs binary inline） |
| `crates/robot-rpc/proto/robot_control.proto:119-123` | `ActionKindProto` enum append `ACTION_KIND_SCRIPTED = 3` |
| `crates/robot-rpc/src/task_handlers.rs` | `ActionKindProto::ActionKindScripted` → `ActionKindFb::Scripted` 映射扩展（gateway 透传 payload bytes 不变）+ **Q2(a) 两道 byte gate**：(1) `cmd.payload.len() > MAX_SCRIPTED_PAYLOAD_BYTES` precheck（kind=Scripted only）+ (2) `build_invoke_task_request(...)` 后 / `forward_to_rr_io(...)` 前 `wire_bytes.len() > ICEORYX2_CONTROL_TIER_MAX_BYTES` final guard，两道均返 tonic `Code::InvalidArgument` 不进 RR/s1 |
| `crates/robot-s1/src/action.rs` | `ActionKind` enum append `Scripted` variant（同 `DwellMs` / `HoldPose`） |
| `crates/robot-s1/src/rr_server/handler.rs:174-193` | `ActionKindFb::Scripted` → `ActionKind::Scripted` 映射扩展 + lib unit `variant count match` 锁测 |
| `crates/robot-s1/src/action_backend/scripted.rs`（新） | `ScriptedBackend` + `ScriptedInstance` impl + inventory submit + lib unit |
| `crates/robot-s1/Cargo.toml` | 加 `backend-scripted` feature（沿 `backend-hold-pose` / `backend-mock-dwell` 同款 gate） |
| `crates/robot-tests/src/bin/scripted_backend_smoke.rs`（新） | sm smoke binary 沿 `pubsub_cleanup_smoke` 模式 + inline 复制 harness helper（CodeMap §7 路径 A） |
| `crates/robot-tests/Cargo.toml` | 加 `[[bin]] scripted_backend_smoke` + `required-features = ["scripted-backend-smoke"]` feature gate（沿 `pubsub-cleanup-smoke` 同款）|

### 严禁改动（hard boundary）

- `crates/robot-rpc/proto/robot_control.proto` 现有 message / field
  编号（仅 append `ActionKindProto::ActionKindScripted = 3`）
- `crates/robot-msg/schemas/*.fbs` 现有 schema 字段编号 / table
  layout（仅 `action_request.fbs` `ActionKindFb` enum append）
- B3 §3.7 `source_task_id` 单调契约：`ScriptedBackend` 必须用
  `ctx.command_seq`，不得私有 counter
- B4 frozen wire schema：`InvokeTaskRequest` / `TaskHandle` / `TaskStatus`
  / `TaskEvent` 字段编号不动
- `EventsDrainRegistry` API（含 `reap_finished` from 候选 3）签名不动
- shutdown 顺序契约（5 步含 candidate 3 reaper.stop+join）不动
- `Box::leak(node)` 模式不动（候选 3 hard boundary 延续）
- `docs/00_design.md` 正文：任何 §1.5 / §5 / §6 语义偏离走
  `docs/CHANGELOG.md` 偏离登记
- §6.7 不变量任一条降级 / 升级：5 条全保

---

## 风险等级

**中**（vs 候选 3 "低-中"）。理由：

- backend 内部状态机（multi-segment Interp/Dwell phase）首次引入，
  非 hold-until-cancel 类
- payload bytes 路径首次走（B3 HoldPose 用 NO_PAYLOAD）；`InvokeTaskRequest`
  外层 RR sample 容量约束首次踩（CodeMap §9.1 #3 三选一）
- `tick_action` hot path 加 linear interp 计算 + 多 segment 状态推进，
  需评估 runtime alloc 风险（每 tick 50Hz）
- schema 注册链 5 必改注册点全踩（fbs / build.rs / schema_id /
  markers / size_tier），任一漏改 → schema_lint / build / lib unit
  fail-fast；`build_helpers.rs` 是可选第 6 步（Q5 拍板）

主要风险点（详 Reviewer 待拍板 §Q + CodeMap §9）：

1. **payload size_tier 外层 RR sample**（CodeMap §9.1 #3 三选一）：
   仅给 `SCHEMA_SCRIPTED_PAYLOAD` 单设 SensorSmall 不充分；必须三选一
2. **`source_task_id` 跨 segment 单调**（B3 §3.7）：waypoint 切换不
   重置 cmd_seq；必须 bit-equivalent test 锁住
3. **insert alloc 在 tick path**：lerp_per_joint 每帧 `Vec<f64>` 分配
   风险；可能需复用 buffer
4. **cancel 中间 waypoint 收敛语义**：Q4 已拍 (a) 沿 HoldPose
   PartialSuccess —— backend lib 返
   `ActionOutcome::PartialSuccess { warning: "cancelled_at_waypoint_N" }`，
   coordinator 在 `pending_cancel` 收敛后转 `ActionEvent::Cancelled` 写
   stream；新增 `ActionOutcome::Cancelled` variant **out-of-scope**（详
   §Q4 reviewer round 1 拍板理由）
5. **smoke binary 复制 ~500-700 行 harness helper**（CodeMap §7 路径
   A）：复制成本可控；不抽 `tests/common/` lib

---

## 回滚策略

候选 1 是 **append-only 改动**（新 backend / 新 schema variant / 新
binary）；回滚 = revert 实施 commit。

各模块独立 revertable：

- 任一 land commit 发现回归 → `git revert <hash>` 单独回滚
- `ActionKindProto::ActionKindScripted = 3` append revert 后下游 ActionKindFb /
  ActionKind 映射跟着 revert；gateway / s1 路径回到 B4 land 状态
- `ScriptedBackend` revert 后 `inventory` 收不到（feature off 也行）；
  `[s1.profile.backends = ["scripted"]]` 配置启动 fail-fast，跟现有
  hold_pose / mock_dwell 同款 `UnknownBackend` 路径
- smoke binary revert：纯 test-only 加项；revert 无影响

如某 lib unit / cross-process smoke flaky → 标 `#[ignore]` + 留
follow-up issue，不回滚整 commit（沿候选 3 同款）。

---

## 目标分支 / 交付形式

- 分支：`master`（直接 commit，沿候选 3 模式 + 每段 reviewer ACK）
- 交付：沿 B3 / 候选 3 同款 3 段节奏，**Spec 阶段不预钉死**；具体段
  数 / 拆分待 Plan 阶段定（粗估 ~3-4 commit）
  * 候选拆分参考（仅参考）:
    - (1/3) schema + 类型（proto + .fbs + schema_id + markers +
      size_tier + build.rs）
    - (2/3) `ScriptedBackend` lib impl + 8+ lib unit + s1 handler
      ActionKindFb 映射 + gateway 映射
    - (3/3) smoke binary `scripted_backend_smoke` + cross-process e2e
      + summary
- §6.7 不变量影响：5 条全保（**0** 降级 / **0** 升级）

---

## Acceptance Criteria

> AC 在代码之前定义。沿 b4_3_3 / 候选 3 同款 + 加 Scripted-specific
> 多 segment / cancel / 跨 segment 单调验证。AC 数量预估 ~10（基础
> AC + Quality Gate），具体 reviewer round 1 拍板。

### AC-1: schema 注册链完整 land + 单元测试穷举

- **前置条件**：候选 1 (1/3) schema commit 之后
- **验证**：
  * `crates/robot-msg/build.rs current_schemas` 含 `scripted_payload.fbs`
    且长度 = 13；flatc 生成 Rust marker code 无误
  * `crates/robot-msg/src/schema_id.rs::SCHEMA_SCRIPTED_PAYLOAD = 0x0000_040B`
    + `SCHEMA_REGISTRY` 排序数组含 entry；`schema_lint` CI 过（唯一性 +
    升序 + 未与 deprecated 冲突）
  * `crates/robot-msg/src/markers.rs::ScriptedPayload` 手写 BodyMarker
    impl（含 `BLOB_EMBEDDED = false` + `type Root<'a>` + 默认
    `read_blob_desc`）；`#[derive(Debug, Clone, Copy, Default)]`
  * `crates/robot-msg/src/size_tier.rs::default_tier_of`
    `SCHEMA_SCRIPTED_PAYLOAD` arm 存在；穷举测试通过
- **验证方式**：`cargo test -p robot-msg`；`cargo run -p robot-msg
  --bin schema_lint`

### AC-2: ActionKindProto / ActionKindFb / ActionKind append 三层一致

- **前置条件**：(1/3) 之后
- **验证**：
  * `proto::ActionKindProto::ActionKindScripted = 3`（prost 生成名：enum 前缀 +
  CamelCase variant；proto 端定义 `ACTION_KIND_SCRIPTED = 3`，沿现有
  `ActionKindDwellMs` / `ActionKindHoldPose` 同款） enum variant（数值唯一不
    冲突）
  * `fbs::ActionKindFb` append `Scripted = 3`（fbs schema 不动现有
    variant 编号）
  * `crates/robot-s1/src/action.rs::ActionKind` enum append `Scripted`
    variant
  * gateway 端 `task_handlers.rs` `ActionKindProto::ActionKindScripted` →
    `ActionKindFb::Scripted` 映射 + lib unit 锁测全 variant 穷举
  * s1 端 `rr_server/handler.rs` `ActionKindFb::Scripted` →
    `ActionKind::Scripted` 映射 + lib unit `variant count match
    explicit mapping` 锁测（沿 B4 (3/3) AC-C 同款 ActionRpcStatus
    variant count 锁测模式）
- **验证方式**：`cargo test --workspace`

### AC-3: `ScriptedBackend` lib unit（沿 hold_pose 8 测模板 + scripted-specific）

- **前置条件**：(2/3) backend impl land
- **验证 8+ lib unit**（沿 hold_pose 同款 + 扩 Scripted 特有）：
  1. `name_is_scripted`
  2. `supported_kinds_is_scripted_only`
  3. `supported_schemas_is_scripted_payload_only`（`&[SCHEMA_SCRIPTED_PAYLOAD]`，
     **不**用 NO_PAYLOAD）
  4. `start_rejects_empty_waypoints`（payload waypoints 空数组拒）
  5. `start_rejects_payload_schema_id_mismatch`（payload_schema_id ≠
     SCHEMA_SCRIPTED_PAYLOAD 拒）
  6. **`tick_segment_returns_command_with_source_task_id_eq_ctx_command_seq`**
     （**bit-equivalent test** —— 沿 hold_pose 同款；候选 1 加跨
     segment 单调验证：连续 N tick 跨 waypoint 切换，source_task_id
     严格单调递增 + ctx.command_seq 透传）
  7. `tick_interp_phase_lerps_between_waypoints`（α=0.5 时 target 介于
     `from` 和 `to` 之间）
  8. `tick_dwell_phase_holds_at_waypoint_target`（dwell 期 target 不变）
  9. `tick_completed_after_last_dwell`（最后 waypoint dwell 完返
     `Completed(Success)`）
  10. `tick_after_cancel_returns_completed_partial_success`（沿
      hold_pose 同款；**backend lib 层**：cancel 中间 waypoint 走
      `ActionOutcome::PartialSuccess { warning: "cancelled_at_waypoint_N" }`
      —— Q4 已拍 (a)；coordinator 转 `ActionEvent::Cancelled` 是
      coordinator 层职责，不在本 lib unit 验证）
  11. `inventory_default_instance_present`（names 含 `"scripted"`）

### AC-4: cross-process smoke binary AC-S1 happy multi-waypoint

- **前置条件**：(3/3) smoke binary land；s1_daemon + gateway release
  build；smoke binary build with `--features scripted-backend-smoke`
- **操作**：构造 N=3 waypoint payload（joint-space target + dwell_ms
  + segment_duration_ms）→ `InvokeTask kind=Scripted payload=<bytes>`
  → `StreamTaskEvents` 等终态
- **预期**：
  * `InvokeTask` 返 `Ok(TaskHandle { action_id })`
  * Stream 收到 `Started` + N 个或更多 `Progress` event（progress 单
    调递增 ≤ 1.0）+ `Completed(outcome=Success)` 终态
  * 子进程 stderr / stdout 无 panic / BUG / `[ERROR]`
- **验证方式**：`./target/release/scripted_backend_smoke --phase
  happy_multi_waypoint`

### AC-5: cross-process smoke binary AC-S2 source_task_id 跨 segment 单调

- **前置条件**：同 AC-4
- **操作**：smoke binary 起 iceoryx2 subscriber 订阅
  `/<robot>/s1/<hw>/command` topic；invoke Scripted N=3 waypoint；
  全程收集所有 `JointCommand.source_task_id` 值
- **预期**：
  * 收集到的 source_task_id 序列**严格单调递增**（跨 Interp / Dwell
    phase + 跨 waypoint segment 切换都保单调；沿 B3 §3.7 / B4 (3/3)
    AC-6 同款契约）
- **验证方式**：`./target/release/scripted_backend_smoke --phase
  source_task_id_monotonic`

### AC-6: cross-process smoke binary AC-S3 cancel 中间 waypoint

- **前置条件**：同 AC-4
- **操作**：invoke Scripted 长 duration（N=5 waypoint × 1000ms
  segment）→ 50ms 后 `CancelTask` → 等 stream 终态
- **预期**：
  * `CancelTask` 返 `Ok(Ack { cancel_at_ns > 0 })`
  * Stream 终态 = **`Cancelled` only**（Q4 已拍 (a)：backend lib 返
    `PartialSuccess` 但 coordinator 在 `pending_cancel` 路径**统一**转
    `ActionEvent::Cancelled` 写 stream，**不**发 `Completed` 终态；详
    B2/B3 coordinator cancel 语义 + B4 (3/3) AC-6 cancel 同款契约）
  * cancel 落在某 waypoint segment 中间（非首段终态前）
- **验证方式**：`./target/release/scripted_backend_smoke --phase
  cancel_mid`

### AC-7: cross-process smoke binary AC-S4 payload 边界

- **前置条件**：同 AC-4
- **操作**（Q2 拍板的 payload size 约束 + Q9 错误码口径联动）：
  * 7a：waypoints = [] 空数组 → `ScriptedBackend::start()` 内
    `BackendStartFailed(ActionStartError::Rejected(_))` →
    `submit_err_to_rpc_status` → wire `ActionRpcStatus::FailedPrecondition`
    → gateway `tonic::Code::FailedPrecondition`（**不**是
    InvalidArgument；`InvalidRequest` / `UnsupportedSchema` 才映
    `InvalidArgument`，详 `crates/robot-s1/src/rr_server/wire.rs:320-340`
    + Q9）
  * 7b：`payload_schema_id = 0`（NO_PAYLOAD）但 `kind = Scripted` →
    dispatch 层 `SubmitError::UnsupportedSchema` → wire
    `ActionRpcStatus::InvalidArgument` → tonic
    `Code::InvalidArgument`（dispatch 失败走 InvalidArgument，跟
    backend semantic reject 区分）
  * 7c：Q2 拍板 (a) 限上限 + 三层 enforcement → 子 case 三选一覆盖：
    (i) payload bytes 超 `MAX_SCRIPTED_PAYLOAD_BYTES` → **gateway
    payload precheck** 直接返 `tonic::Code::InvalidArgument`（不进 s1）；
    (ii) payload bytes 通过 precheck 但 build wire 后 `wire_bytes.len()
    > CONTROL_TIER_MAX_BYTES`（外层 CommandMeta + FlatBuffers overhead
    意外膨胀场景）→ **gateway final wire-size guard** 返
    `tonic::Code::InvalidArgument`（不进 RR / s1）；
    (iii) payload bytes 通过两道 gateway gate 但 fbs decode fail /
    waypoint 数超 / positions 长度不匹配 → **backend fbs decode +
    semantic validate** `BackendStartFailed(ActionStartError::PayloadDecode(_))`
    或 `Rejected(_)` → `FailedPrecondition`。Q2 拍板 (b)/(c) → 此子
    case 改为对应容量边界验证
- **验证方式**：`./target/release/scripted_backend_smoke --phase
  payload_boundary`

### AC-Q: Quality Gate（不进 AC，但是 PASS 条件）

- `cargo fmt --all` clean
- `cargo clippy --workspace --all-targets -- -D warnings` 双路 clean
  （含 / 不含 `scripted-backend-smoke` feature）
- `cargo test --workspace --tests --features "robot-rpc/client" --
  --test-threads=1`：现有 940 PASS + AC-3 新加 ~8-11 lib unit + AC-2
  variant count 锁测全过
- 双路 `crash_restart_smoke` 5/5 + 5/5（沿候选 3 同款回归门槛）
- `pubsub_cleanup_smoke --phase all`（候选 3 e2e）**不退化**：候选 1
  改动**不**应触发候选 3 任一条件性 AC（AC-C1/C2/C3 全保 not
  triggered）
- `cargo test -p robot-tests --bin scripted_backend_smoke --features
  scripted-backend-smoke`：smoke binary compile + 内置 `#[test]` 模块
  （如有）全过（沿 pubsub_cleanup_smoke 同款 cargo test compile-gate
  模式；保 `cargo test --workspace` 默认不开 smoke feature 时不 build
  本 binary）
- `./target/release/scripted_backend_smoke --phase all`：AC-4/5/6/7
  全过；exit code 0
- schema_lint CI gate 过：候选 1 的 `SCHEMA_SCRIPTED_PAYLOAD` 唯一 +
  升序 + 未与 deprecated 冲突
- proto + fbs schema 字节级一致（无 frozen 改动；仅 append）
- §6.7 5 条不变量真宣称保持（**0** 降级 / **0** 升级 / **0** 偏移）

---

## Reviewer 待拍板项（5 核心 Q + 12 风险细节）

> reviewer round 1 倾向已表态 5 核心 Q 默认；具体 ACK 待 Spec commit
> reviewer round 2 复审。Q17 (`backend-scripted` default feature) 在
> round 1 阻断 5 修订时新增。

### round 1 倾向（reviewer 表态；Spec commit 时 round 2 确认 ACK）

| Q | reviewer round 1 倾向 |
|---|---|
| Q1 | **(a) 最小集 fbs 字段**（仅 `waypoints` + `positions` / `dwell_ms` / `segment_duration_ms`；不加 v/a / metadata） |
| Q2 | **(a) 限 ≤ 8 KiB + 三层 enforcement**（gateway payload precheck → `InvalidArgument`；gateway final wire-size guard `wire_bytes.len() ≤ CONTROL_TIER_MAX_BYTES` → `InvalidArgument`；backend fbs decode + semantic validate → `FailedPrecondition`；详 §Q2(a) round 2 阻断 1 修法） |
| Q3 | **(c) 组合 progress** `(idx + segment_progress) / total` |
| Q4 | **(a) 沿 HoldPose PartialSuccess**（(b) `ActionOutcome::Cancelled` 移 out-of-scope） |
| Q5 | **(a) 路径 A：inline 复制 helper**（不抽 `tests/common/` lib） |
| Q17 | **(a) `backend-scripted` 加进 default**（沿 hold_pose / mock_dwell 同款） |


> 本节问题在 Plan 阶段之前必须有 reviewer 拍板；未拍板项**不**进
> Plan。沿候选 3 Spec Q1-Q10 同款模式。

### 核心 Q（影响 AC 形态 + Plan 拆分）

#### Q1: ScriptedPayload fbs 字段集

- **选项 (a) 最小集**：仅 `waypoints: [WaypointTarget]` +
  `WaypointTarget { positions: [double], dwell_ms: uint32,
  segment_duration_ms: uint32 }`
- **选项 (b) 加速度上限**：加 `max_velocity` / `max_acceleration`
  字段（v0.4 后期 / v0.5 trajectory backend 才用；现在 reserved 字段
  vs 不加）
- **选项 (c) 加 metadata**：加 `name: string` / `tags: [string]`
  audit 字段
- **推荐**：(a) 最小集（v0.4_entry §3.2 reviewer 拍板 "joint-space
  线性插值"语义弱不需要速度/加速度；audit 走 CommandMeta.audit_reason
  字段，不污染 backend payload）；reviewer 复核

#### Q2: payload size 三选一（CodeMap §9.1 #3）

- **选项 (a) 限 Scripted payload 让整 InvokeTaskRequest ≤ 8 KiB**：
  硬限 waypoint 数量上限（如 ≤ 100 / 1 joint 时 ≤ ~1.6 KiB；含 fbs
  overhead ≤ 4 KiB）。
  **三层 enforcement**（reviewer round 1 阻断 4 + round 2 阻断 1 拍板
  补充：仅 `payload.len()` precheck 不足以保证最终 wire sample ≤
  Control 8 KiB —— 外层还有 `request_id` / `idempotency_key` /
  `source` / `audit_reason` / FlatBuffers vtable + body overhead；必须
  加 **final wire-size guard** 在 build wire 后实测 bytes 长度）：
  1. **gateway payload precheck**（保守 payload bytes 上限，不解 fbs）：
     `task_handlers::handle_invoke_task` validate 阶段加
     `cmd.payload.len() > MAX_SCRIPTED_PAYLOAD_BYTES` 拒；返 tonic
     `Code::InvalidArgument`（**不**进 RR / s1）。快速拒明显超大
     payload，仍保 gateway 不解 payload 边界（B4 outline §6.1 双层
     schema 分工）
  2. **gateway final wire-size guard**（reviewer round 2 阻断 1 拍板
     补充）：`build_invoke_task_request(...)` 后、`forward_to_rr_io(...)`
     前检查 `wire_bytes.len() <= ICEORYX2_CONTROL_TIER_MAX_BYTES`
     （= 8 KiB；`size_tier::SizeTier::Control.max_bytes()`），超限直
     接返 `tonic::Code::InvalidArgument`，**不**进 RR / s1。这才**真正**
     避免 iceoryx2 `loan_slice_uninit` 撞 `SampleTooLarge`（任一外层
     CommandMeta 字段意外长 / FlatBuffers padding 异常都被兜底）
  3. **backend fbs decode + semantic validate**：
     `ScriptedBackend::start()` 内 `flatbuffers::root::<ScriptedPayload>(payload)`
     verify + 字段语义校验（waypoints 非空 / positions 长度 ==
     hardware.joints.len() / segment_duration_ms > 0 等）；失败走
     `BackendStartFailed(ActionStartError::Rejected(_))` →
     `submit_err_to_rpc_status` → `FailedPrecondition`（详 Q9 错误
     码口径）
  最小工程量；锁住 wire 不改。三层职责清晰：(1) 快速拒明显超限 +
  (2) 兜底实际 wire sample ≤ Control 8 KiB + (3) 语义校验 fbs 内容
- **选项 (b) 给 `task_invoke` RR / `SCHEMA_INVOKE_TASK_REQUEST` 走更
  大容量**：升 `SizeTier::SensorSmall` (1 MiB) 或 per-schema_id
  override；影响 B4 frozen 边界 — 需 reviewer round + iceoryx2 shm
  占用评估
- **选项 (c) streaming/chunking**：拆 ScriptedPayload 跨多 RR frame；
  推 v0.5+
- **推荐**：(a) 限上限 + 双层 enforcement；如不够走 (b)；(c) 长期方案

#### Q3: ScriptedBackend `progress` 计算

- **选项 (a) 时间百分比**：`elapsed_in_action / total_action_duration`
- **选项 (b) waypoint index**：`current_waypoint_idx / total_waypoints`
- **选项 (c) 组合**：`(idx + segment_progress) / total`（idx 是已完
  成 waypoint 数；segment_progress 是当前 segment 内的时间百分比）
- **推荐**：(c) 组合（最自然 + 平滑）；vs hold_pose 永 0.0 不语义化
  形成对比

#### Q4: cancel 中间 waypoint 收敛语义

- **选项 (a) 沿 HoldPose PartialSuccess**：返
  `ActionOutcome::PartialSuccess { warning: "cancelled_at_waypoint_N" }`
  + coordinator 转 `ActionEvent::Cancelled` 写事件流（B3 拍板模式）
- **推荐**：(a)

**reviewer round 1 拍板**（已 ACK Q4(a)，备选 (b) 移 out-of-scope）：
新增 `ActionOutcome::Cancelled` variant 在本轮**不**考虑；coordinator
cancel 语义已经把 backend PartialSuccess 收敛后转 `ActionEvent::Cancelled`
写事件流（B3 拍板模式 cover 全），新增 outcome variant 无增量价值。
如未来真需要 cancel 独立 outcome（区分 partial-success vs intent-cancel
语义）→ 走另立 outline / v0.5+。

#### Q5: smoke binary 是否抽 `crates/robot-tests/src/common/` lib

- **选项 (a) 路径 A：inline 复制 helper（CodeMap §7 推荐）**：
  `scripted_backend_smoke.rs` inline 复制候选 3 helper ~500-700 行；
  复制成本可控；不动 robot-tests crate 结构
- **选项 (b) 路径 B：先抽 common/ lib 再复用**：改 robot-tests
  Cargo.toml 加 `[lib]` + migrate 候选 3 私有 fn → pub fn + scripted
  binary `use robot_tests::common::...`；工程量 +2-3 commit + reviewer
  round；适合多 binary（候选 1/2/4 都需要时一次性抽，但现在只 2 个）
- **推荐**：(a) 路径 A；(b) 留 v0.5+ 多 binary 时机

### 风险细节 Q（12 条；reviewer round 内可批量拍）

| # | Q | 默认倾向（reviewer 复核） |
|---|---|---|
| Q6 | waypoint 上限 N | (Q2 (a) 路径) 100；具体 reviewer 拍 |
| Q7 | runtime alloc 在 tick path | `Vec<f64>` 用 `Vec::with_capacity(joints)` 复用 + `clear()`（沿 hold_pose 模式）；尝试避 per-tick 分配 |
| Q8 | 插值 cadence vs tick_loop 50Hz | tick_loop 内调 backend.tick；segment_duration_ms 非 20ms 整数倍时 lerp alpha 落点不均匀；本轮**接受**（v0.5+ 评估 100Hz 内部插值） |
| Q9 | gateway payload schema 强校验 + 错误码口径 | gateway **不**解 payload bytes（沿 B4 outline §6.1 双层 schema 分工）；Q2(a) 三层 enforcement 拍板下 gateway 仅做 **两道 byte 长度 gate**：(1) `cmd.payload.len() > MAX_SCRIPTED_PAYLOAD_BYTES` precheck + (2) build wire 后 `wire_bytes.len() > CONTROL_TIER_MAX_BYTES` final guard，超限均返 tonic `Code::InvalidArgument`（**不**解 payload bytes）。s1 backend `start()` 内 `flatbuffers::root::<ScriptedPayload>(payload)` decode + semantic validate fail → `BackendStartFailed(ActionStartError::Rejected/PayloadDecode)` → `submit_err_to_rpc_status` → wire `ActionRpcStatus::FailedPrecondition` → tonic `Code::FailedPrecondition`（**不**是 InvalidArgument；详 `crates/robot-s1/src/rr_server/wire.rs:320-340` 错误码映射；`InvalidRequest` / `UnsupportedSchema` 才映 `InvalidArgument`，那是 dispatch 层语义） |
| Q10 | feature flag 名 | `backend-scripted`（沿 `backend-hold-pose` / `backend-mock-dwell` 同款连字符）；smoke binary feature 名 `scripted-backend-smoke`（沿 `pubsub-cleanup-smoke` 同款） |
| **Q17** | **`backend-scripted` 是否进 robot-s1 `default` features** | `crates/robot-s1/Cargo.toml:19` 当前 `default = ["backend-mock-dwell", "backend-hold-pose"]`。候选 1 land 后选 (a) 加 `backend-scripted` 进 default → smoke binary / `cargo build --release --workspace` 默认 link Scripted backend；(b) 留 default 不动 → smoke binary 命令必须显式 `--features robot-s1/backend-scripted`（沿 freeze_systemd_smoke 同款 feature 传递）。**推荐 (a)** 进 default — 跟 hold_pose 一致策略，避免 smoke 命令 / 文档维护额外 feature flag |
| Q11 | `ScriptedInstance` 内存模型 | start() 期解码 payload 一次性 owned `Vec<DecodedWaypoint>`；tick 期不再解 fbs；避 per-tick parser overhead |
| Q12 | smoke binary 复制 ~700 行 helper 的精确范围 | 全复制（FakeStatePublisher + SubprocessHandle + bring_up_pair + tear_down_pair + invoke_and_wait_terminal + sample_metric + helper），跟 CodeMap §7.2 表对齐 |
| Q13 | `--phase` 列表 | `selftest / happy_multi_waypoint / source_task_id_monotonic / cancel_mid / payload_boundary / all`（5+1） |
| Q14 | s1 handler ActionKindFb → ActionKind 映射的 lib unit 命名 | `action_kind_fb_variant_count_matches_explicit_mapping`（沿 B4 (3/3) `rpc_status_variant_count_matches_explicit_mapping` 同款） |
| Q15 | `inventory_default_instance_present` 测覆盖 | 现有测看 names 含 `"hold_pose"`；候选 1 land 后 names 也要含 `"scripted"`；选 (a) 改现有测 vs (b) 加 ScriptedBackend 专属测；推荐 (a) 改现有测（names 集断言 + assert 含两个） |
| Q16 | 候选 1 实施期能否触发候选 3 任一条件性 AC | 应该**不**触发（候选 1 不动 events_drain / gateway subscribe / iceoryx2 wrapper）；Quality Gate 跑 `pubsub_cleanup_smoke --phase all` 锁住 not triggered |

---

## 不在范围（候选 1 边界）

按 v0.4_entry §3.1 优先级 + §5 hard boundaries + 候选 3 完结锚点：

- **候选 2 (robot-kinematics + IK)**：trait 草案 + IK 算法 / vendor URDF
  / per-机器人 feature flag 全留候选 2 reviewer round
- **候选 4 (dimos 切换)**：留 v0.4 后期 / v0.5 早期；候选 1 + 2 land
  后才开
- **cubic 插值 / 加速度 / jerk 契约**：本轮拍板 joint-space 线性；留
  v0.4 后期 / v0.5 trajectory backend
- **`Box::leak(node)` 模式**：候选 3 hard boundary 延续不动
- **AC-C1 reaper 行为 / shutdown step 4 顺序**：候选 3 已 land；不动
- **events_drain / events_subscribe 路径**：候选 1 不触
- **B4 frozen schema** 现有 message / field 编号 / TaskHandle /
  TaskStatus / TaskEvent 字段：仅 append `ActionKindProto::ActionKindScripted`
  + `ActionKindFb::Scripted` + 新 `SCHEMA_SCRIPTED_PAYLOAD`
- **`run_action_tick_loop` / coordinator tick path 改动**：candidate 1
  仅 backend 内 tick；coordinator dispatch 路径不动（沿 hold_pose
  同款 ActionInstance trait 注入）
- **`EventsDrainRegistry::reap_finished` API**：签名不动（候选 3 land
  锚定）
- **iceoryx2 wrapper / Node ownership 模型**：候选 3 hard boundary
  延续不动
- **§6.7 5 不变量任一条降级 / 升级**：5 条全保
- **mTLS / OIDC / 真 auth / rate limiting / circuit breaker**：v0.5+
- **真硬件回归**：candidate 1 默认 dev box 跑；真硬件回归推 candidate 1
  land 后单独安排（沿 v0.3 B 真硬件回归节奏）

---

## 跟其它 milestone handoff / spec 的链接

- [`docs/handoffs/v0.4_entry.md`](../../docs/handoffs/v0.4_entry.md)
  §2.2 候选 1 范围 + §3.2 插值策略拍板（joint-space 线性 + waypoint
  dwell） + §5 6 hard boundaries
- [`docs/handoffs/v0.4_pubsub_cleanup_summary.md`](../../docs/handoffs/v0.4_pubsub_cleanup_summary.md)
  §6 候选 1 接班 + §3 关键契约审计（AC-C1 reaper 行为 + shutdown 顺
  序契约 + B3 §3.7 source_task_id 单调）
- [`team-swe/codemap/v0_4_scripted_backend.md`](../codemap/v0_4_scripted_backend.md)
  完整 Pre-Research CodeMap（9 段 + 18 条风险与不确定项）
- [`docs/handoffs/v0.3_b3_summary.md`](../../docs/handoffs/v0.3_b3_summary.md)
  HoldPoseBackend land 模式 + reviewer round 2 阻断 1 修法
  （source_task_id 来自 ctx.command_seq）
- [`docs/handoffs/v0.3_b2_outline.md`](../../docs/handoffs/v0.3_b2_outline.md)
  ActionBackend trait / BackendRegistry / ActionContext /
  ActionTickOutput 原始拍板表
- [`docs/handoffs/v0.3_b4_outline.md`](../../docs/handoffs/v0.3_b4_outline.md)
  §6.1 proto schema vs fbs schema 双层模型（gateway 透传 payload
  bytes 不解码）
- [`team-swe/specs/v0_4_pubsub_cleanup.md`](v0_4_pubsub_cleanup.md)
  候选 3 Spec + Plan v3 + Plan-Execute diff 4 项归档（参考模板）
- [`team-swe/specs/b4_3_3_cross_process_e2e.md`](b4_3_3_cross_process_e2e.md)
  Spec 模板 + cross-process smoke harness 模式参考
- `docs/00_design.md` §4.1 模块地图 + §4.9 RT 铁律 + §6.7 5 条不变量
  全保

---

## 后续：reviewer round 2 + Plan

reviewer round 1 已表态 5 核心 Q + Q17 倾向（详 §Reviewer 待拍板表）；
round 2 reviewer ACK 之后进 Plan 阶段：commit 拆分 + 每段验收 +
Plan-Execute diff 登记位置（沿候选 3 Plan v3 同款模板）。

---

## Plan

> Plan outline draft v1（reviewer ACK pending）。基于 Spec §AC + Q
> 拍板硬输入；**不**重新决策 Q1-Q17 / AC 边界 / hard boundary。
> Spec round 2 commit `5efe987` push 后开 Plan round 1。

### P.0 工作基线

- 起点：`master` HEAD = `5efe987`（Spec round 1+2 amend land）。
- Q 拍板硬输入：Q1(a) 最小集 fbs / Q2(a) 三层 enforcement / Q3(c) 组合
  progress / Q4(a) PartialSuccess（Cancelled outcome out-of-scope）/
  Q5(a) inline 复制 helper / Q17(a) `backend-scripted` 进 default；
  Q6-Q16 reviewer 倾向默认（详 §风险细节 Q 表）。
- 不开新分支（沿候选 3 Plan v3 + B4 模式直接 commit 到 master + 每段
  reviewer ACK）。
- 全 Plan 严格基于 Spec §AC + §Reviewer 拍板；**不**重新决策任何
  Q1-Q17 / AC 边界 / hard boundary。

### P.1 commit 节奏总览

**Happy path**（AC-1 至 AC-7 全过 + Quality Gate 全过）：

```
✨ feat: v0.4 候选 1 (1/3) Scripted backend schema 注册链 land
        （proto + .fbs + schema_id + markers + size_tier + build.rs
        + ActionKindProto/Fb/ActionKind 三层 append）
✨ feat: v0.4 候选 1 (2/3) Scripted backend lib impl + 三层 payload
        enforcement land（ScriptedBackend + ScriptedInstance + 8+ lib
        unit + s1 handler ActionKindFb 映射 + gateway 两道 byte gate
        + backend fbs decode/semantic validate）
✨ feat: v0.4 候选 1 (3/3) scripted_backend_smoke binary + cross-process
        e2e（AC-4/5/6/7 全过 + scripted-backend-smoke feature gate）
📝 docs: v0.4 候选 1 summary 归档 + Phase 候选 1 完结
```

粗估 3 commit + summary = 4 commit chain；如 reviewer round 内出现
拆分诉求，沿候选 3 同款拆为 (X/3-a) + (X/3-b) 模式（Plan-Execute
diff 登记）。

### P.2 工作流（按阶段顺序）

#### P.2.1 [commit (1/3)] schema 注册链 + 三层 ActionKind append

##### P.2.1.a `crates/robot-msg` schema 注册链 6 步（5 必改 + 1 可选）

| # | 改动 | 文件 + 行号 |
|---|---|---|
| 1 | 新 `.fbs` schema 定义 | `crates/robot-msg/schemas/scripted_payload.fbs`（新） |
| 2 | `current_schemas` 数组扩容 + 入注册 | `crates/robot-msg/build.rs:57` `[&str; 12]` → `[&str; 13]` + append `"scripted_payload.fbs"` |
| 3 | `SCHEMA_SCRIPTED_PAYLOAD` const + REGISTRY entry | `crates/robot-msg/src/schema_id.rs` 加 `pub const SCHEMA_SCRIPTED_PAYLOAD: u32 = 0x0000_040B;` + `SCHEMA_REGISTRY` 升序数组 append |
| 4 | 手写 `BodyMarker` impl | `crates/robot-msg/src/markers.rs` 加 `#[derive(Debug, Clone, Copy, Default)] pub struct ScriptedPayload;` + `impl BodyMarker`（`SCHEMA_ID = SCHEMA_SCRIPTED_PAYLOAD` + `BLOB_EMBEDDED = false` + `type Root<'a> = scripted_payload_generated::ScriptedPayload<'a>` + 默认 `read_blob_desc`） |
| 5 | `default_tier_of` arm + 穷举测试 | `crates/robot-msg/src/size_tier.rs:62-95` match arm 加 `SCHEMA_SCRIPTED_PAYLOAD => SizeTier::Control`（payload 本身 ≤ ~4 KiB 落 Control 8 KiB；wire 外层走 `SCHEMA_INVOKE_TASK_REQUEST` size tier 不动）+ `tests::default_tier_of` 穷举测试加 arm |
| 6 | **可选** `build_scripted_payload` helper | `crates/robot-msg/src/build_helpers.rs`（**§Q5 拍板 (a) inline 复制 → 本步骤跳过**，smoke binary 内 inline 构 fbs；如 reviewer round 改诉求可补） |

**`scripted_payload.fbs` 字段集（Q1(a) 最小集拍板）**：

```fbs
namespace robot.msg.scripted_payload;

// joint-space 单 waypoint target + dwell + 段 duration（Q1(a) 最小集）
table WaypointTarget {
  positions: [double];           // 长度 == hardware.joints.len()
  dwell_ms: uint32;              // 到位后停留 ms（0 = 不 dwell 直接进下一段）
  segment_duration_ms: uint32;   // 当前段线性插值时长 ms（> 0 必须）
}

table ScriptedPayload {
  waypoints: [WaypointTarget];   // 非空数组（empty → backend Rejected）
}

root_type ScriptedPayload;
```

**严禁改动**：现有 `action_request.fbs` / `command.fbs` / `state.fbs`
等任一现有 schema 字段编号 / table layout / blob layout；仅 append
`ActionKindFb::Scripted = 3`。

##### P.2.1.b proto + ActionKindFb + ActionKind 三层 append

| 层 | 改动 |
|---|---|
| `crates/robot-rpc/proto/robot_control.proto:119-123` | `ActionKindProto` enum append `ACTION_KIND_SCRIPTED = 3`（prost 生成名 `ActionKindProto::ActionKindScripted`，沿 `ActionKindDwellMs` / `ActionKindHoldPose` 同款 CamelCase + enum prefix） |
| `crates/robot-msg/schemas/action_request.fbs` | `ActionKindFb` enum append `Scripted = 3`（不动现有 variant 编号） |
| `crates/robot-s1/src/action.rs` | `ActionKind` enum append `Scripted` variant（沿 `DwellMs` / `HoldPose` 同款） |

##### P.2.1.c Quality Gate（commit (1/3)）

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo build --workspace --release` 过（无 `--features scripted-backend-smoke`）
- `cargo test -p robot-msg` 全过（schema_lint + default_tier_of 穷举）
- `cargo run -p robot-msg --bin schema_lint` 过（SCHEMA_SCRIPTED_PAYLOAD
  唯一 + 升序 + 未与 deprecated 冲突）
- `cargo test --workspace --tests` 全过（注册链 land 但 ActionKind 映射
  unimplemented 状态：commit (2/3) 才补；本步骤如出现 unreachable 分支
  panic，拆 (1/3-a) (1/3-b) 处理）

##### P.2.1.d 风险锚点

- **`SCHEMA_SCRIPTED_PAYLOAD` size_tier 选 Control vs SensorSmall**：
  Q1(a) 最小集字段 + 100 waypoint 上限下 fbs payload ≤ 4 KiB，落
  `SizeTier::Control` 8 KiB 充裕；如 reviewer round 内决定提升 waypoint
  上限 / 加 metadata（§Q1 (b)/(c)）触发 Plan-Execute diff 改 size_tier
- **build.rs 数组长度漂移**：`current_schemas: [&str; 12 → 13]` 必须
  跟 `.fbs` 文件个数一致；漏改 build 直接 fail-fast

#### P.2.2 [commit (2/3)] ScriptedBackend lib + 三层 payload enforcement

##### P.2.2.a `ScriptedBackend` impl（沿 hold_pose 模板）

新文件：`crates/robot-s1/src/action_backend/scripted.rs`

| 模块 | 改动 |
|---|---|
| `ScriptedBackend` | 零字段 struct + `inventory::submit!`；`ActionBackend` impl: `name = "scripted"` + `supported_kinds = &[ActionKind::Scripted]` + `supported_schemas = &[SCHEMA_SCRIPTED_PAYLOAD]`（**不**含 NO_PAYLOAD） |
| `ScriptedBackend::start()` | `flatbuffers::root::<ScriptedPayload>(payload)` verify + 语义校验（`waypoints` 非空 + 每段 `positions.len() == hardware.joints.len()` + `segment_duration_ms > 0`）→ 失败返 `BackendStartFailed(ActionStartError::Rejected(_))` 或 `PayloadDecode(_)`；成功构 `ScriptedInstance { decoded_waypoints: Vec<DecodedWaypoint>, current_idx: 0, phase: Interp{start_ns}, total_action_duration_ns, ... }`（**start() 期一次性 owned decode**；tick 期不再解 fbs，Q11 拍板） |
| `ScriptedInstance` | 字段含 `cancelled: bool` + `decoded_waypoints: Vec<DecodedWaypoint>`（含 owned positions Vec + dwell_ms + segment_duration_ms）+ `phase: enum { Interp { start_ns }, Dwell { start_ns } }` + `current_idx: usize` + `total_action_duration_ns: u64` + `tick_buf: Vec<f64>`（with_capacity(joints) 复用避 per-tick alloc，Q7 拍板）|
| `ScriptedInstance::tick(ctx)` | (1) cancelled 短路返 `Completed(PartialSuccess { warning: "cancelled_at_waypoint_N" })`（Q4(a)）；(2) phase 状态机推进（Interp → Dwell → next waypoint）；(3) 当前 phase 算 target positions（Interp 期 `α = elapsed_in_segment / segment_duration_ms` lerp `from→to`；Dwell 期 target = waypoint.positions 不变）；(4) Q3(c) 组合 progress = `(current_idx + segment_progress) / total_waypoints`；(5) `build_hold_command(ctx.command_seq, target_positions, &hardware.joints)` 复用 B1/B3 builder（**source_task_id == ctx.command_seq 单调契约，B3 §3.7**）；(6) 末段 Dwell 完返 `Completed(Success)`；(7) 中间段返 `Continue { command: Some(...), progress: Some(...) }` |
| `ScriptedInstance::cancel()` | `self.cancelled = true` 不返终态（沿 hold_pose 同款；下次 tick 收敛） |

##### P.2.2.b ScriptedBackend lib unit（AC-3 8+ unit）

8+ unit 沿 hold_pose 8 测模板 + Scripted-specific：

| # | 测试名 | 验证 |
|---|---|---|
| 1 | `name_is_scripted` | `backend.name() == "scripted"` |
| 2 | `supported_kinds_is_scripted_only` | `[ActionKind::Scripted]` |
| 3 | `supported_schemas_is_scripted_payload_only` | `[SCHEMA_SCRIPTED_PAYLOAD]`，**不**含 NO_PAYLOAD |
| 4 | `start_rejects_empty_waypoints` | empty waypoints → `BackendStartFailed(Rejected)` |
| 5 | `start_rejects_payload_schema_id_mismatch` | `payload_schema_id != SCHEMA_SCRIPTED_PAYLOAD` → `BackendStartFailed(Rejected)` |
| 6 | `tick_segment_returns_command_with_source_task_id_eq_ctx_command_seq` | **bit-equivalent test** + 连续 N tick 跨 segment `source_task_id` 严格单调递增 |
| 7 | `tick_interp_phase_lerps_between_waypoints` | α=0.5 时 target 介于 `from`/`to` |
| 8 | `tick_dwell_phase_holds_at_waypoint_target` | dwell 期 target == waypoint.positions 不变 |
| 9 | `tick_completed_after_last_dwell` | 末 waypoint dwell 完返 `Completed(Success)` |
| 10 | `tick_after_cancel_returns_completed_partial_success` | cancel 后下次 tick 返 `Completed(PartialSuccess { warning: "cancelled_at_waypoint_N" })`（backend lib 层；coordinator 转 `ActionEvent::Cancelled` 不在 lib unit 验证） |
| 11 | `inventory_default_instance_present` | `BackendRegistry::names()` 集含 `"scripted"`（沿 Q15 拍板 (a) 改现有测） |

##### P.2.2.c s1 handler `ActionKindFb::Scripted` → `ActionKind::Scripted` 映射

`crates/robot-s1/src/rr_server/handler.rs:174-193`：现有 match 加
`ActionKindFb::Scripted => ActionKind::Scripted`；lib unit
`action_kind_fb_variant_count_matches_explicit_mapping`（Q14 拍板）
锁住 enum variant 数 == 显式映射 arm 数。

##### P.2.2.d gateway `ActionKindProto::ActionKindScripted` → `ActionKindFb::Scripted` 映射 + 三层 payload enforcement

`crates/robot-rpc/src/task_handlers.rs`：

| 步骤 | 改动 |
|---|---|
| 1 | `action_kind_proto_to_fb` match arm 加 `ActionKindProto::ActionKindScripted => ActionKindFb::Scripted` + lib unit variant count 锁测（沿 B4 (3/3) AC-C 同款 `rpc_status_variant_count_matches_explicit_mapping` 模式）|
| 2 | **gate 1（payload precheck）**：`handle_invoke_task` validate 阶段，当 `kind == Scripted` 时加 `cmd.payload.len() > MAX_SCRIPTED_PAYLOAD_BYTES` 拒 → 返 `tonic::Code::InvalidArgument`（`MAX_SCRIPTED_PAYLOAD_BYTES` const = 4 KiB；定义位置 task_handlers.rs 内；Q6 拍板上限 100 waypoint × 1 joint ≤ ~1.6 KiB + fbs overhead，4 KiB 充裕）|
| 3 | **gate 2（final wire-size guard）**：`build_invoke_task_request(...)` 后、`forward_to_rr_io(...)` 前检查 `wire_bytes.len() > CONTROL_TIER_MAX_BYTES` 拒 → 返 `tonic::Code::InvalidArgument`。**常量来源**（reviewer ACK 期注意点）：定义 `const CONTROL_TIER_MAX_BYTES: usize = SizeTier::Control.max_bytes();`（**单源**绑定 `robot_msg::size_tier::SizeTier::Control.max_bytes()`，**不**硬编码 8192，避免常量漂移；命名统一 `CONTROL_TIER_MAX_BYTES`，删除 Spec 内 `ICEORYX2_CONTROL_TIER_MAX_BYTES` 别名）|
| 4 | **不**解 payload fbs bytes（保 B4 outline §6.1 双层 schema 分工） |

##### P.2.2.e backend feature gate

`crates/robot-s1/Cargo.toml`：

```toml
[features]
default = ["backend-mock-dwell", "backend-hold-pose", "backend-scripted"]  # Q17(a)
backend-scripted = []  # Q10 拍板：沿 backend-hold-pose 同款连字符
```

`crates/robot-s1/src/action_backend/mod.rs`：

```rust
#[cfg(feature = "backend-scripted")]
pub mod scripted;
```

##### P.2.2.f Quality Gate（commit (2/3)）

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` 双路 clean
  （含 / 不含 `backend-scripted` feature）
- `cargo test --workspace --tests --features "robot-rpc/client" --
  --test-threads=1`：现有 940 PASS + 新加 AC-3 11 lib unit + AC-2
  variant count + gateway 2 byte gate lib unit 全过
- 双路 `crash_restart_smoke` 5/5 + 5/5（沿候选 3 同款回归门槛）
- §6.7 5 条不变量真宣称保持

##### P.2.2.g 风险锚点

- **`CONTROL_TIER_MAX_BYTES` 单源绑定**：reviewer ACK 期注意点（直接
  绑 `SizeTier::Control.max_bytes()` 不硬编码 8192）；如 `size_tier.rs`
  扩展加新 tier 或常量值漂移，本 const 自动跟随
- **`MAX_SCRIPTED_PAYLOAD_BYTES` 选值**：4 KiB 充裕兜底 100 waypoint
  × 1 joint × ~16 bytes/waypoint + fbs overhead；如 Q1 后续扩字段 / Q6
  上限改大需 reviewer round 改 const
- **`Vec<f64> tick_buf` 复用**：`with_capacity(joints)` + `clear()`
  避 per-tick alloc；如 hardware.joints 数 runtime 变化要重 grow
  （目前 hardware.joints 是 backend start 期 owned 不变；Q11 拍板）

#### P.2.3 [commit (3/3)] scripted_backend_smoke binary + cross-process e2e

##### P.2.3.a Cargo.toml 变更

`crates/robot-tests/Cargo.toml` 追加：

```toml
[[bin]]
name = "scripted_backend_smoke"
path = "src/bin/scripted_backend_smoke.rs"
required-features = ["scripted-backend-smoke"]

[features]
# v0.4 候选 1 Scripted backend smoke 入口 feature（沿 pubsub-cleanup-smoke
# / clear-estop-e2e 同款 feature 守门）。
scripted-backend-smoke = [
    "dep:robot-rpc",
    "dep:tonic",
    "dep:tokio",
]
```

##### P.2.3.b CLI args（沿 `pubsub_cleanup_smoke` 同款）

- `--bin-dir target/release`（默认）
- `--robot-id-prefix scripted_backend`（默认）
- `--hardware-id <id>`（必填）/ `--config <toml>`（必填）
- `--phase selftest | happy_multi_waypoint | source_task_id_monotonic |
  cancel_mid | payload_boundary | all`（默认 all；Q13 拍板）
- `--waypoint-count N`（默认 3，AC-4 happy 用）
- `--segment-duration-ms <ms>`（默认 500，AC-4 用）
- `--cancel-delay-ms <ms>`（默认 50，AC-6 用）
- `--output-csv <path>`（可选）

##### P.2.3.c smoke binary 复制 ~500-700 行 harness（Q5(a) 路径 A）

inline 复制候选 3 `pubsub_cleanup_smoke.rs` helper（Q12 拍板全复制；
CodeMap §7.2 表对齐）：

| helper | 来源 |
|---|---|
| `FakeStatePublisher` | `crates/robot-rpc/tests/common/fake_state.rs:39-153` 等价 inline |
| `SubprocessHandle` + `bring_up_pair` + `tear_down_pair` | `pubsub_cleanup_smoke.rs` inline 等价 |
| `invoke_and_wait_terminal` | `pubsub_cleanup_smoke.rs` inline 等价 |
| `sample_metric` + CSV writer | `pubsub_cleanup_smoke.rs` inline 等价 |
| `build_scripted_payload(waypoints) -> Vec<u8>` | **新增** inline fbs builder（构 `ScriptedPayload` fbs bytes；Q5(a) 路径 A 拍板 → 不抽公共 helper） |

**复用边界**：smoke binary 直接 inline 等价实现（沿
`pubsub_cleanup_smoke` 不复用 `tests/common` 同款独立模式；**不**改
`fake_state.rs` 公共可见性）。

##### P.2.3.d AC 实施序列（5 phase）

| Phase | 验证 | Q 引用 |
|---|---|---|
| `selftest` | smoke binary 自检（gateway 通 + fake S0 publish + scripted_payload fbs build → root verify）| - |
| `happy_multi_waypoint` | AC-4：N=3 waypoint → `InvokeTask` + Stream Started + ≥N Progress（单调 ≤ 1.0）+ `Completed(Success)` 终态 | AC-4 |
| `source_task_id_monotonic` | AC-5：iceoryx2 subscriber 收集 `JointCommand.source_task_id` 全程严格单调递增 | AC-5 + B3 §3.7 |
| `cancel_mid` | AC-6：N=5 waypoint × 1000ms → 50ms 后 `CancelTask` → stream 终态 = `Cancelled` only（**不**是 `Completed{PartialSuccess}`；coordinator 路径转换契约）| AC-6 + Q4(a) |
| `payload_boundary` | AC-7：(7a) empty waypoints → `FailedPrecondition` ; (7b) `payload_schema_id=0` + `kind=Scripted` → `InvalidArgument`（dispatch 层） ; (7c-i) payload bytes 超 `MAX_SCRIPTED_PAYLOAD_BYTES` → gateway precheck `InvalidArgument` ; (7c-ii) payload bytes 通过 precheck 但 wire 超 `CONTROL_TIER_MAX_BYTES` → gateway final guard `InvalidArgument` ; (7c-iii) 通过两道 gate 但 fbs decode fail → backend `FailedPrecondition` | AC-7 + Q2(a) 三层 + Q9 |

##### P.2.3.e Quality Gate（commit (3/3)）

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` 双路 clean
  （含 / 不含 `scripted-backend-smoke` feature）
- `cargo test -p robot-tests --bin scripted_backend_smoke --features
  scripted-backend-smoke`：smoke binary compile + 内置 `#[test]` 模
  块（如有）全过
- `cargo build -p robot-tests --release --bin scripted_backend_smoke
  --features scripted-backend-smoke` 过；生产构建（无 feature）
  `cargo build -p robot-tests --release` 不 link 本 binary
- `./target/release/scripted_backend_smoke --phase all`：AC-4/5/6/7
  5 phase 全过；exit code 0
- 双路 `crash_restart_smoke` 5/5 + 5/5（沿候选 3 同款回归门槛）
- `pubsub_cleanup_smoke --phase all` 不退化（候选 1 改动**不**触发
  候选 3 任一 AC-C1/C2/C3）

##### P.2.3.f 风险锚点

- **gateway final wire-size guard 单测可观测性**：(7c-ii) 子 case 构
  造 payload bytes 通过 precheck 但 wire 撞 8 KiB 的边界 payload 难
  调（依赖 `idempotency_key` / `audit_reason` 等外层字段长度的精确膨
  胀）；smoke 期可用 dummy 长 `audit_reason` 拼到 wire 超限；如难复
  现，留 lib unit `build_wire_bytes_over_control_tier_returns_invalid_argument`
  在 task_handlers.rs 内构造 mock cmd → assert guard 拒，作为 AC-7
  (7c-ii) 子 case 补充
- **smoke binary harness 复制后维护漂移**：候选 3 `pubsub_cleanup_smoke`
  和候选 1 `scripted_backend_smoke` 两个 binary 同套 helper 后续如
  修需双 binary 同步；Q5(b) 路径 B 抽 lib 留 v0.5+ 多 binary 时机

#### P.2.4 [commit summary] summary 归档

新文件：`docs/handoffs/v0.4_scripted_backend_summary.md`（沿候选 3
`v0.4_pubsub_cleanup_summary.md` 同款模板）

| 段 | 内容 |
|---|---|
| §1 范围 | 候选 1 Scripted backend = joint-space 线性插值 + waypoint dwell |
| §2 commit chain | (1/3) → (2/3) → (3/3) → summary（含 hash + 文件统计）|
| §3 关键契约 | (a) B3 §3.7 `source_task_id == ctx.command_seq` ; (b) Q2(a) 三层 enforcement ; (c) Q4(a) PartialSuccess + coordinator 转 `Cancelled` ; (d) `CONTROL_TIER_MAX_BYTES` 单源绑 `SizeTier::Control.max_bytes()` ; (e) Q5(a) inline 复制边界 |
| §4 AC + Quality Gate 实测 | AC-1 至 AC-7 + AC-Q 全过记录 ; 双路 `crash_restart_smoke` + `pubsub_cleanup_smoke --phase all` 不退化 |
| §5 §6.7 5 不变量真宣称 | **0** 降级 / **0** 升级 / **0** 偏移 |
| §6 后续接班 | 候选 2 (robot-kinematics IK trait) Pre-Research / CodeMap 入口 |
| §7 Reviewer Plan round 归档 | Plan round 1/2/... ACK 节点 + Plan-Execute diff 列表 |

同步更新：

- `docs/handoffs/README.md` 加 anchor
- `docs/CHANGELOG.md` 加 2026-05-14 候选 1 land 行
- `CLAUDE.md` 状态段更新（v0.4 候选 1 ✅；候选 2 准备开）

### P.3 reviewer Plan round 拍板（pending）

| Round | 状态 | 内容 |
|---|---|---|
| Plan round 1 | pending | 本 outline + Q1-Q17 reviewer 倾向 ACK |
| Plan round 2 | pending | round 1 阻断 fix（如有）+ ACK |
| Plan round N | pending | 最终 Approved 进入实施 |

### P.4 Plan-Execute diff 登记位置

沿候选 3 Plan v3 §P.4 同款模板。实施期偏离 Plan 任何决策 → 登记本节
（不**回退** Plan，appendix-only），含：

- 触发条件（哪个 Plan 决策被偏离）
- 实际选择（实施期做了什么）
- 理由（reviewer ACK / 实施期发现）
- 验证回归（双路 crash_restart_smoke + AC-Q 全过）

预期可能 diff：

- diff #1：smoke binary harness 复制行数实际 vs 预估（~500-700 行）
- diff #2：`MAX_SCRIPTED_PAYLOAD_BYTES` 常量选值如调整
- diff #3：lib unit 数量实际 vs 预估（AC-3 11 unit）
- diff #4：AC-7 (7c-ii) wire 撞限子 case 难复现 → 改 lib unit 补 +
  smoke phase only run (7c-i/iii)

---

## 后续：reviewer Plan round + 实施

Plan outline draft v1 commit 后开 Plan round 1；ACK 后进 (1/3) 实施。
沿候选 3 Plan v3 同款 reviewer round 模式（outline → ACK → 实施 →
reviewer ACK push → 下一 commit）。
